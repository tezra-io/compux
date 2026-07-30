//! compux — the native computer-use (screen-capture + input-injection) sidecar.
//!
//! Reads one JSON request line from stdin, performs the GUI action, writes one
//! JSON response line to stdout. The wire contract is `Compux.Protocol`
//! (lib/compux/protocol.ex). The Elixir `Compux.PortDriver` owns this process
//! as a Port.
//!
//! Coordinate model (the #1 "clicks land offset" risk — read carefully):
//!   * A screenshot is the target display captured at PHYSICAL pixels, then
//!     downscaled so its long edge is <= `MAX_EDGE`. The model sees that
//!     downscaled image and sends click coordinates in ITS pixel space.
//!   * Synthetic input (enigo) uses the display's LOGICAL points. So a click at
//!     `(x, y)` in the sent image maps to logical `origin + (x, y) / k` where
//!     `k = sent_dim / logical_dim`. `logical = physical / scale_factor`.
//!   * v1 drives ONE display (the configured index, default primary). Multi-
//!     display origins are passed through but need on-device verification.
//!
//! Runtime behavior must be verified on a real machine with the macOS TCC grants
//! (Screen Recording and Accessibility). It never panics the request loop — every
//! action answers with `{"ok": true, ...}` or `{"ok": false, "error": "..."}`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use image::ImageEncoder as _;
use serde::Deserialize;
use serde_json::{json, Value};
use xcap::Monitor;

/// Long-edge cap for a sent screenshot (design §5: oversized captures 400 on
/// Anthropic and ground worse).
const MAX_EDGE: u32 = 1366;

/// Pixel-area budget for a sent screenshot (M28 B5). The long-edge cap alone
/// punishes extreme aspect ratios: a 3840x1080 super-ultrawide got the same
/// budget as 16:9 but only 384px tall — unreadable, which forced the model to
/// live in magnified crops (and grid-switch errors). A sent image may use
/// whichever budget grants MORE pixels, never upscaled: 16:9 keeps exactly
/// 1366x768, 32:9 recovers ~1931x543, and any capture that fits the long edge
/// today still ships at native resolution.
const MAX_AREA: u32 = MAX_EDGE * 768;

/// The sent-image downscale `kz <= 1` fitting `w x h` physical pixels into the
/// budgets: the looser of the long-edge and area rules, capped at native. One
/// formula for full captures and crops, so the two can never diverge.
fn budget_scale(w: f32, h: f32) -> f32 {
    let long = w.max(h).max(1.0);
    let edge = (MAX_EDGE as f32 / long).min(1.0);
    let area = (MAX_AREA as f32 / (w * h).max(1.0)).sqrt();
    edge.max(area).min(1.0)
}

/// Wire-compatibility version. MUST match `Compux.Protocol.protocol_version/0`.
/// Bumped ONLY on a wire-incompatible change; reported in the `hello` handshake so
/// a consumer can refuse a mismatched sidecar (the two-pin drift guard).
///
/// v3: added the operational idle-detection actions `idle_ms` + `wait_for_idle`
/// (coexistence — let a policy layer yield the seat to a present human).
///
/// v5: `screenshot` gained the optional grounding-integrity fields `rulers`,
/// `annotate_point`, and `marks` (M28) — additive, but a consumer advertising
/// them against an older sidecar would get silently un-annotated images, so the
/// version bumps and the handshake refuses the pairing loudly.
const PROTOCOL_VERSION: u32 = 5;

// --- macOS TCC responsibility disclaim ---------------------------------------

/// Make this process its OWN TCC "responsible process".
///
/// `Compux.PortDriver` spawns us via `posix_spawn` (the Elixir Port). A child that
/// does not disclaim inherits its PARENT's TCC identity, so macOS would attribute our
/// Screen-Recording (`kTCCServiceScreenCapture`) and Accessibility
/// (`kTCCServiceAccessibility`) requests to the ad-hoc, version-keyed BEAM/daemon
/// ancestor — an identity whose grant never persists (re-prompt every action, a new
/// System-Settings row per version). Disclaiming attaches the grant to compux's OWN
/// stable Developer-ID bundle identity instead.
///
/// The disclaim flag must be set on the spawn attributes BEFORE the spawn — which the
/// Port cannot do, and a running process cannot do to itself — so we re-exec ONCE via
/// `POSIX_SPAWN_SETEXEC` (replaces this image in place; the pid and the stdin/stdout
/// Port fds are preserved, so the wire protocol is untouched). A `COMPUX_DISCLAIMED`
/// sentinel bounds it to a single re-exec. The API is private/header-less (resolved
/// via `dlsym`), so we FAIL LOUD (non-zero exit) if it is absent or errors, rather
/// than silently running un-disclaimed and resurrecting the mis-attribution bug.
#[cfg(target_os = "macos")]
mod disclaim {
    use std::env;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::ptr;

    type SetDisclaimFn =
        unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;

    pub fn become_responsible() {
        if env::var_os("COMPUX_DISCLAIMED").is_some() {
            return; // already the re-exec'd, disclaimed image
        }

        let set_disclaim = resolve_set_disclaim();
        let exe = env::current_exe().unwrap_or_else(|e| fatal(73, &format!("current_exe: {e}")));
        let exe_c = cstr(exe.as_os_str().as_bytes());

        let argv = CArray::new(env::args_os().map(|a| cstr(a.as_bytes())).collect());
        let mut env_strings: Vec<CString> = env::vars_os()
            .map(|(k, v)| {
                let mut kv = k.as_bytes().to_vec();
                kv.push(b'=');
                kv.extend_from_slice(v.as_bytes());
                cstr(&kv)
            })
            .collect();
        env_strings.push(cstr(b"COMPUX_DISCLAIMED=1"));
        let envp = CArray::new(env_strings);

        unsafe {
            let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
            if libc::posix_spawnattr_init(&mut attr) != 0 {
                fatal(74, "posix_spawnattr_init failed");
            }
            if set_disclaim(&mut attr, 1) != 0 {
                fatal(71, "responsibility_spawnattrs_setdisclaim returned nonzero");
            }
            if libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_SETEXEC as libc::c_short)
                != 0
            {
                fatal(75, "posix_spawnattr_setflags failed");
            }
            // SETEXEC replaces this image; posix_spawn returns ONLY on failure.
            libc::posix_spawn(
                ptr::null_mut(),
                exe_c.as_ptr(),
                ptr::null(),
                &attr,
                argv.ptrs.as_ptr(),
                envp.ptrs.as_ptr(),
            );
            fatal(72, "POSIX_SPAWN_SETEXEC re-exec failed");
        }
    }

    fn resolve_set_disclaim() -> SetDisclaimFn {
        // dlsym's RTLD_DEFAULT pseudo-handle on macOS is (void *)-2.
        let rtld_default = (-2isize) as *mut libc::c_void;
        let name = cstr(b"responsibility_spawnattrs_setdisclaim");
        let sym = unsafe { libc::dlsym(rtld_default, name.as_ptr()) };
        if sym.is_null() {
            fatal(70, "responsibility_spawnattrs_setdisclaim unavailable");
        }
        unsafe { std::mem::transmute::<*mut libc::c_void, SetDisclaimFn>(sym) }
    }

    fn cstr(bytes: &[u8]) -> CString {
        CString::new(bytes).unwrap_or_else(|_| fatal(76, "unexpected NUL in argv/env"))
    }

    // Owns the CString backing store so the null-terminated pointer vector stays valid.
    struct CArray {
        ptrs: Vec<*mut libc::c_char>,
        _owned: Vec<CString>,
    }

    impl CArray {
        fn new(owned: Vec<CString>) -> CArray {
            let mut ptrs: Vec<*mut libc::c_char> =
                owned.iter().map(|s| s.as_ptr() as *mut _).collect();
            ptrs.push(ptr::null_mut());
            CArray {
                ptrs,
                _owned: owned,
            }
        }
    }

    fn fatal(code: i32, msg: &str) -> ! {
        eprintln!("compux: FATAL disclaim: {msg}");
        std::process::exit(code);
    }
}

fn main() {
    // macOS: become our OWN TCC responsible process before any capture/input call,
    // so Screen-Recording + Accessibility grants attribute to compux's stable code
    // identity rather than the ad-hoc BEAM ancestor that Port-spawned us. One-shot
    // self-re-exec (see the `disclaim` module); a no-op on the second entry.
    #[cfg(target_os = "macos")]
    disclaim::become_responsible();

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Value>(&line) {
            Ok(req) => handle(&req),
            Err(e) => err(format!("invalid request JSON: {e}")),
        };

        // One JSON line per response. A write failure means the parent is gone.
        if writeln!(stdout, "{response}").is_err() {
            break;
        }
        let _ = stdout.flush();

        // A capture wedged the OS backend and leaked a stuck worker we can't
        // reclaim — the reply is now flushed, so exit and let the parent respawn
        // a clean sidecar (EX_TEMPFAIL). Do this AFTER flush so the caller got
        // its `capture_stalled` answer first.
        if CAPTURE_WEDGED.load(Ordering::SeqCst) {
            #[cfg(target_os = "macos")]
            ax::clear_activations();
            std::process::exit(75);
        }
    }

    // stdin EOF: the owning Port closed — the session is over. Switch OFF any
    // accessibility attribute this process switched on (B4), so one enumeration
    // never leaves the user's browser in an altered AX mode. Best-effort: a
    // SIGKILLed sidecar skips this, and the next activation is idempotent.
    #[cfg(target_os = "macos")]
    ax::clear_activations();
}

fn handle(req: &Value) -> Value {
    let action = req.get("action").and_then(Value::as_str).unwrap_or("");
    let result = match action {
        "hello" => hello(),
        "probe" => probe(),
        "idle_ms" => idle_ms(),
        "wait_for_idle" => wait_for_idle(req),
        "request_permissions" => request_permissions(req),
        "screenshot" => screenshot(req),
        "mouse_move" => mouse_move(req),
        "left_click" => click(req, Button::Left, 1),
        "right_click" => click(req, Button::Right, 1),
        "double_click" => click(req, Button::Left, 2),
        "left_click_drag" => drag(req),
        "scroll" => scroll(req),
        "type" => type_text(req),
        "key" => key_chord(req),
        "wait" => wait(req),
        "inspect" => inspect(req),
        "wait_for_change" => wait_for_change(req),
        "paste" => paste(req),
        "elements" => elements(req),
        "windows" => windows(req),
        other => Err(format!("unknown action: {other}")),
    };

    match result {
        Ok(value) => value,
        Err(message) => err(message),
    }
}

fn err(message: String) -> Value {
    json!({ "ok": false, "error": message })
}

// --- hello (version handshake, NOT a model action) --------------------------

/// Identity + wire-version handshake performed once by `Compux.start/1`. Lets the
/// consumer refuse a sidecar whose `protocol_version` its compiled-in encoder does
/// not speak (the two-pin drift guard). `compux_version` is diagnostic; `actions`
/// is the model-facing verb set (probe/hello are operational, excluded).
fn hello() -> Result<Value, String> {
    Ok(json!({
        "ok": true,
        "protocol_version": PROTOCOL_VERSION,
        "compux_version": env!("CARGO_PKG_VERSION"),
        "actions": [
            "screenshot", "left_click", "right_click", "double_click", "mouse_move",
            "left_click_drag", "scroll", "type", "key", "wait", "inspect",
            "wait_for_change", "paste", "elements", "windows"
        ],
    }))
}

// --- probe (operational permission check, NOT a model action) ---------------

/// Report whether screen capture and input control are actually available, plus
/// the platform and display server. NON-PROMPTING: on macOS this queries TCC grant
/// state (Accessibility + Screen Recording) WITHOUT raising a permission dialog or
/// posting an event — the only reliable way to detect the silent-drop state where
/// capture works but synthetic input is discarded. Surfaced by the consumer's
/// diagnostics (a doctor/setup surface); the model never calls this.
fn probe() -> Result<Value, String> {
    Ok(json!({
        "ok": true,
        "platform": std::env::consts::OS,
        "display_server": display_server(),
        "screen_capture": screen_capture_ok(),
        "input_control": input_control_ok(),
    }))
}

// --- request_permissions (operational grant PROMPT, NOT a model action) -------

/// Actively PROMPT for the macOS grants and report the resulting state. Operational,
/// like `probe` — excluded from `hello`'s model-facing verbs. The consumer's setup /
/// doctor flow calls this at enable time so the OS dialogs (Screen Recording +
/// Accessibility) appear up front instead of on the first screenshot, and the app
/// registers in System Settings. Unlike `probe`'s non-prompting preflight, these
/// variants RAISE the system dialog. The prompts are async, so the returned booleans
/// are the pre-response snapshot (typically `false` on first call) — the consumer
/// re-runs `probe` after the user approves. No-op on Linux (no TCC).
fn request_permissions(_req: &Value) -> Result<Value, String> {
    Ok(json!({
        "ok": true,
        "platform": std::env::consts::OS,
        "screen_capture": request_screen_capture_ok(),
        "input_control": request_input_control_ok(),
    }))
}

#[cfg(target_os = "macos")]
fn request_screen_capture_ok() -> bool {
    permissions::request_screen_capture()
}

#[cfg(target_os = "macos")]
fn request_input_control_ok() -> bool {
    permissions::request_input_control()
}

// No TCC off macOS: report the same reality `probe` does (there is nothing to prompt).
#[cfg(not(target_os = "macos"))]
fn request_screen_capture_ok() -> bool {
    screen_capture_ok()
}

#[cfg(not(target_os = "macos"))]
fn request_input_control_ok() -> bool {
    input_control_ok()
}

#[cfg(target_os = "macos")]
mod permissions {
    //! macOS TCC grant state, queried without prompting.
    //!
    //! `AXIsProcessTrusted` (ApplicationServices): is this process trusted for the
    //! Accessibility API — the gate macOS silently drops `CGEventPost` without (so a
    //! click returns ok yet nothing moves). `CGPreflightScreenCaptureAccess`
    //! (CoreGraphics, 10.15+): is screen capture permitted — without it capture
    //! returns wallpaper-only. Both are preflight checks; neither prompts.
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::CFString;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
        // Same trust check, but with `kAXTrustedCheckOptionPrompt` it RAISES the
        // Accessibility prompt (directs the user to System Settings). Returns the
        // current (pre-grant) trust state.
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        // Prompts if the grant is undetermined and registers the app in System
        // Settings ▸ Screen Recording. Returns whether access is already granted.
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    pub fn input_control() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    pub fn screen_capture() -> bool {
        unsafe { CGPreflightScreenCaptureAccess() }
    }

    // --- prompting variants (used by `request_permissions`) ---

    pub fn request_screen_capture() -> bool {
        unsafe { CGRequestScreenCaptureAccess() }
    }

    pub fn request_input_control() -> bool {
        // The exported `kAXTrustedCheckOptionPrompt` constant does not link as a
        // symbol (same as the AX attribute-name constants elsewhere here), so we
        // build the CFString from its documented value.
        let key = CFString::new("AXTrustedCheckOptionPrompt");
        let options = CFDictionary::from_CFType_pairs(&[(
            key.as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )]);
        unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) != 0 }
    }
}

/// Make the pointer BE at the requested point before any button/scroll event is
/// posted, so the event's own destination is the point the caller asked for.
///
/// Why this exists (observed live 2026-07-26: 4 of 7 clicks landed on the PREVIOUS
/// click's point, every one reporting success): enigo's macOS `button()` opens with
/// `let (current_x, current_y) = self.location()?` — `NSEvent::mouseLocation()` — and
/// builds `dest` for BOTH the mouse-down and the mouse-up from that live read. Our
/// preceding `move_mouse` only POSTS a CGEvent, which the window server applies
/// asynchronously, so `mouseLocation()` can still report the old position when
/// `button()` reads it. The click then goes to — and warps the cursor to — wherever
/// the pointer still was. `scroll` has the same exposure: a CGEvent scroll carries no
/// destination and lands wherever the pointer actually is.
///
/// `CGWarpMouseCursorPosition` is SYNCHRONOUS: it moves the cursor in the window
/// server before returning, so the subsequent `location()` read is the truth. It is
/// deliberately called AFTER `move_mouse`, not instead of it — `move_mouse` is what
/// emits the MouseMoved/Dragged event (hover states, drag tracking) with real deltas,
/// and warping first would zero those deltas.
///
/// Takes the same top-left global point space `move_mouse(_, _, Coordinate::Abs)`
/// does, which is what `to_logical` already produces. Note macOS suppresses local
/// HID mouse events for a short interval after a warp; compux already yields the
/// cursor to a present human through the caller's courtesy arbiter, so a brief
/// suppression during an agent action is the intended trade.
#[cfg(target_os = "macos")]
mod pointer {
    use std::ffi::c_void;
    use std::ptr;

    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    /// `CGEventType::kCGEventLeftMouseDragged` / `kCGHIDEventTap` /
    /// `kCGMouseButtonLeft` — numeric values from CGEventTypes.h, stable ABI.
    const LEFT_MOUSE_DRAGGED: u32 = 6;
    const HID_EVENT_TAP: u32 = 0;
    const MOUSE_BUTTON_LEFT: u32 = 0;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWarpMouseCursorPosition(newCursorPosition: CGPoint) -> i32;
        fn CGEventCreateMouseEvent(
            source: *const c_void,
            event_type: u32,
            point: CGPoint,
            button: u32,
        ) -> *mut c_void;
        fn CGEventPost(tap: u32, event: *mut c_void);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    /// `Err` on a non-zero CGError: a pointer we could not place is a click we must
    /// not post, since it would land somewhere the caller never asked for.
    pub fn settle(x: i32, y: i32) -> Result<(), String> {
        let point = CGPoint {
            x: f64::from(x),
            y: f64::from(y),
        };

        match unsafe { CGWarpMouseCursorPosition(point) } {
            0 => Ok(()),
            error => Err(format!("warp pointer to ({x},{y}): CGError {error}")),
        }
    }

    /// One intermediate point of a drag, posted as an EXPLICIT `LeftMouseDragged`
    /// event. enigo's `move_mouse` picks its event type from a live
    /// `pressedMouseButtons()` read, which races the just-posted mouse-down — the
    /// intermediate move then goes out as `MouseMoved` (a hover, not a drag) and
    /// no drag handler ever arms. Building the event by hand removes the state
    /// read entirely. The warp afterwards keeps the visible cursor and the next
    /// `location()` read (the release's destination) at the same point.
    pub fn drag_step(x: i32, y: i32) -> Result<(), String> {
        let point = CGPoint {
            x: f64::from(x),
            y: f64::from(y),
        };

        let event = unsafe {
            CGEventCreateMouseEvent(ptr::null(), LEFT_MOUSE_DRAGGED, point, MOUSE_BUTTON_LEFT)
        };

        if event.is_null() {
            return Err(format!("create drag event at ({x},{y})"));
        }

        unsafe {
            CGEventPost(HID_EVENT_TAP, event);
            CFRelease(event);
        }

        settle(x, y)
    }
}

/// X11 injects motion synchronously (`XTestFakeMotionEvent` + flush) and
/// `XTestFakeButtonEvent` carries no coordinate, so there is nothing to settle.
#[cfg(not(target_os = "macos"))]
mod pointer {
    pub fn settle(_x: i32, _y: i32) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn display_server() -> &'static str {
    "quartz"
}

#[cfg(target_os = "macos")]
fn screen_capture_ok() -> bool {
    permissions::screen_capture()
}

#[cfg(target_os = "macos")]
fn input_control_ok() -> bool {
    permissions::input_control()
}

// Linux: X11 is permissive (no TCC — any local client may capture/inject); Wayland
// deliberately blocks global capture + injection (no uniform API). Capability tracks
// the display server; a real capture/input still fails loud per action.
#[cfg(target_os = "linux")]
fn display_server() -> &'static str {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "wayland"
    } else if std::env::var_os("DISPLAY").is_some() {
        "x11"
    } else {
        "none"
    }
}

#[cfg(target_os = "linux")]
fn screen_capture_ok() -> bool {
    display_server() == "x11"
}

#[cfg(target_os = "linux")]
fn input_control_ok() -> bool {
    display_server() == "x11"
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn display_server() -> &'static str {
    "unknown"
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn screen_capture_ok() -> bool {
    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn input_control_ok() -> bool {
    false
}

// --- display geometry -------------------------------------------------------

/// Display geometry, separated from the OS `Monitor` handle so the coordinate math
/// is pure and unit-testable (a `Monitor` cannot be constructed off a real screen).
struct Geometry {
    /// physical capture pixels
    phys_w: u32,
    phys_h: u32,
    /// logical points (physical / scale_factor)
    logical_w: f32,
    logical_h: f32,
    /// logical top-left origin in the global desktop space
    origin_x: f32,
    origin_y: f32,
    scale_factor: f32,
}

struct Display {
    geom: Geometry,
    /// The monitor's stable id (CGDirectDisplayID on macOS). Capture and the
    /// asleep check both re-resolve the monitor by THIS (never by list index),
    /// so a mid-action display change fails typed instead of rebinding to a
    /// different physical monitor. The xcap `Monitor` handle isn't `Send` and
    /// isn't held past geometry read — the id is all a later capture needs.
    id: u32,
}

/// A zoom rectangle in full-display SENT-image pixel space — the coordinates the
/// model reads off a normal screenshot. Absent on a request → the whole display.
#[derive(Clone, Copy)]
struct Region {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Region {
    /// The region spanning the entire full-display sent image.
    fn full(geom: &Geometry) -> Region {
        let k = sent_scale(geom);
        Region {
            x: 0.0,
            y: 0.0,
            w: (geom.logical_w * k) as f64,
            h: (geom.logical_h * k) as f64,
        }
    }
}

/// A physical crop of the display plus the scale used to send it. Built once from a
/// region and shared by capture and the inverse coordinate map, so the two can never
/// disagree — the #1 "clicks land offset" bug class.
struct CropRect {
    left_phys: f32,
    top_phys: f32,
    w_phys: f32,
    h_phys: f32,
}

impl CropRect {
    /// Downscale to fit the sent budgets (`budget_scale`); never upscale
    /// (`kz <= 1`). A small crop is therefore sent at native physical
    /// resolution — that is the zoom.
    fn sent_scale(&self) -> f32 {
        budget_scale(self.w_phys, self.h_phys)
    }

    fn sent_dims(&self) -> (u32, u32) {
        let kz = self.sent_scale();
        (
            (self.w_phys * kz).round().max(1.0) as u32,
            (self.h_phys * kz).round().max(1.0) as u32,
        )
    }
}

/// Pick the requested monitor, distinguishing "no display is capturable at all"
/// from "that index doesn't exist on a multi-monitor host".
///
/// `xcap`'s active-monitor list is EMPTY when nothing can be captured — on macOS
/// that is the screen-locked, display-asleep, or no-GUI-session state, none of
/// which a different `display` index can fix. Reporting that as `display 0 not
/// found` reads like a bad index and sends the caller hunting for another monitor;
/// the typed `no_active_display` lets the Elixir layer say what is actually wrong.
fn select_monitor(monitors: Vec<Monitor>, index: usize) -> Result<Monitor, String> {
    if monitors.is_empty() {
        return Err("no_active_display".to_string());
    }

    monitors
        .into_iter()
        .nth(index)
        .ok_or_else(|| format!("display {index} not found"))
}

fn target_display(req: &Value) -> Result<Display, String> {
    let index = req.get("display").and_then(Value::as_u64).unwrap_or(0) as usize;
    let monitors = Monitor::all().map_err(|e| format!("enumerate displays: {e}"))?;
    let monitor = select_monitor(monitors, index)?;

    // xcap 0.4 returns the monitor geometry as `Result`s — unwrap each loudly so a
    // capture-backend hiccup surfaces as a clean action error, never a wrong click.
    let scale_factor = monitor
        .scale_factor()
        .map_err(|e| format!("scale_factor: {e}"))?
        .max(1.0);
    let phys_w = monitor.width().map_err(|e| format!("display width: {e}"))?;
    let phys_h = monitor
        .height()
        .map_err(|e| format!("display height: {e}"))?;
    let origin_x = monitor.x().map_err(|e| format!("display origin x: {e}"))?;
    let origin_y = monitor.y().map_err(|e| format!("display origin y: {e}"))?;
    let id = monitor.id().map_err(|e| format!("display id: {e}"))?;

    let geom = Geometry {
        logical_w: phys_w as f32 / scale_factor,
        logical_h: phys_h as f32 / scale_factor,
        origin_x: origin_x as f32 / scale_factor,
        origin_y: origin_y as f32 / scale_factor,
        scale_factor,
        phys_w,
        phys_h,
    };

    Ok(Display { geom, id })
}

/// The full-display "sent scale" `k`: sent pixels per LOGICAL point for a full
/// screenshot. The full image is the PHYSICAL display downscaled to fit the sent
/// budgets (`budget_scale` → `kz_full`), so `k = sent_dim / logical_dim = kz_full *
/// scale_factor`. Region coordinates are read off that sent image, so `crop_rect` /
/// `Region::full` MUST use this physical-derived `k`. A logical-derived `k` diverges
/// whenever the logical long edge already fits the budget but the physical one does
/// not (e.g. a 13" Retina at 2560x1600@2x → 1280x800 logical) and mislocates region
/// zooms — the #1 offset bug.
fn sent_scale(geom: &Geometry) -> f32 {
    budget_scale(geom.phys_w as f32, geom.phys_h as f32) * geom.scale_factor
}

fn parse_region(req: &Value) -> Result<Option<Region>, String> {
    match req.get("region") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let x = region_field(value, "x")?;
            let y = region_field(value, "y")?;
            let w = region_field(value, "w")?;
            let h = region_field(value, "h")?;
            if w <= 0.0 || h <= 0.0 {
                return Err("region.w and region.h must be > 0".to_string());
            }
            Ok(Some(Region { x, y, w, h }))
        }
    }
}

fn region_field(value: &Value, key: &str) -> Result<f64, String> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("region.{key} is missing or not a number"))
}

fn region_or_full(geom: &Geometry, requested: Option<Region>) -> Region {
    requested.unwrap_or_else(|| Region::full(geom))
}

/// The physical crop for a region (or the whole display when the region spans it).
/// `region` is in full-display SENT-image pixels; convert through the full-display
/// sent scale `k` to logical, then to physical, clamped to the display bounds.
fn crop_rect(geom: &Geometry, region: &Region) -> CropRect {
    let k = sent_scale(geom);
    let sf = geom.scale_factor;
    // Clamp left/top in-bounds (an out-of-range region can't produce a degenerate or
    // out-of-image crop); width/height then fill the remaining space, min 1px.
    let max_left = (geom.phys_w as f32 - 1.0).max(0.0);
    let max_top = (geom.phys_h as f32 - 1.0).max(0.0);
    let left = (region.x as f32 / k * sf).clamp(0.0, max_left);
    let top = (region.y as f32 / k * sf).clamp(0.0, max_top);
    let w = (region.w as f32 / k * sf)
        .min(geom.phys_w as f32 - left)
        .max(1.0);
    let h = (region.h as f32 / k * sf)
        .min(geom.phys_h as f32 - top)
        .max(1.0);
    CropRect {
        left_phys: left,
        top_phys: top,
        w_phys: w,
        h_phys: h,
    }
}

/// Map a coordinate from the last sent image to a global LOGICAL point for enigo.
///
/// One convention for full and zoomed views: a full screenshot is a region spanning
/// the whole sent image, so this reduces to `origin + (x,y)/k` there. With a region
/// the image is a physical crop downscaled by `kz`; the inverse adds the crop's
/// logical offset. Capture and this share `crop_rect`, so they cannot disagree.
fn to_logical(geom: &Geometry, region: &Region, x: f64, y: f64) -> (i32, i32) {
    let crop = crop_rect(geom, region);
    let kz = crop.sent_scale();
    let lx = geom.origin_x + (crop.left_phys + (x as f32) / kz) / geom.scale_factor;
    let ly = geom.origin_y + (crop.top_phys + (y as f32) / kz) / geom.scale_factor;
    (lx.round() as i32, ly.round() as i32)
}

/// Inverse of `to_logical`: a global LOGICAL point → the sent-image coordinate for
/// `region`, or None when it falls outside the sent image. Used by `elements` to
/// place accessibility frames back onto the coordinates the model reads.
fn to_sent(geom: &Geometry, region: &Region, lx: f64, ly: f64) -> Option<(i64, i64)> {
    let crop = crop_rect(geom, region);
    let kz = crop.sent_scale();
    let sf = geom.scale_factor;
    let sx = ((lx as f32 - geom.origin_x) * sf - crop.left_phys) * kz;
    let sy = ((ly as f32 - geom.origin_y) * sf - crop.top_phys) * kz;
    let (sw, sh) = crop.sent_dims();
    if sx < 0.0 || sy < 0.0 || sx > sw as f32 || sy > sh as f32 {
        None
    } else {
        Some((sx.round() as i64, sy.round() as i64))
    }
}

// --- overlay drawing (M28 B1/B2/B3) ------------------------------------------

/// Marker / ruler / badge drawing on the SENT image, hand-rolled on the raw
/// buffer. Deliberately no imageproc/font dependency: the sidecar ships
/// size-optimized (`opt-level="z"`, lto, strip) and the only text needed is
/// digits plus three symbols, covered by a 5x7 bitmap atlas.
///
/// Everything draws in SENT-image pixel space — the space the model reads and
/// answers in — and every write is bounds-checked, so a marker near an edge
/// clips instead of panicking.
mod overlay {
    use image::RgbaImage;

    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const RED: [u8; 4] = [230, 40, 40, 255];

    /// 5x7 glyphs, one row per byte (5 low bits, MSB = leftmost pixel).
    const GLYPH_W: i32 = 5;
    const GLYPH_H: i32 = 7;

    fn glyph(c: char) -> Option<[u8; 7]> {
        match c {
            '0' => Some([0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E]),
            '1' => Some([0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E]),
            '2' => Some([0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F]),
            '3' => Some([0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E]),
            '4' => Some([0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02]),
            '5' => Some([0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E]),
            '6' => Some([0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E]),
            '7' => Some([0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08]),
            '8' => Some([0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E]),
            '9' => Some([0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C]),
            '(' => Some([0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02]),
            ')' => Some([0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08]),
            ',' => Some([0x00, 0x00, 0x00, 0x00, 0x0C, 0x04, 0x08]),
            _ => None,
        }
    }

    /// Rendered width of a label in pixels (glyphs + 1px spacing).
    fn text_width(text: &str) -> i32 {
        let n = text.chars().count() as i32;
        if n == 0 {
            0
        } else {
            n * (GLYPH_W + 1) - 1
        }
    }

    fn put(img: &mut RgbaImage, x: i32, y: i32, color: [u8; 4]) {
        if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
            img.put_pixel(x as u32, y as u32, image::Rgba(color));
        }
    }

    fn fill_rect(img: &mut RgbaImage, x0: i32, y0: i32, w: i32, h: i32, color: [u8; 4]) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                put(img, x, y, color);
            }
        }
    }

    /// Black text on a white plate (1px padding) — readable on any background.
    fn draw_label(img: &mut RgbaImage, x0: i32, y0: i32, text: &str) {
        fill_rect(
            img,
            x0 - 1,
            y0 - 1,
            text_width(text) + 2,
            GLYPH_H + 2,
            WHITE,
        );
        let mut x = x0;
        for c in text.chars() {
            if let Some(rows) = glyph(c) {
                for (dy, row) in rows.iter().enumerate() {
                    for dx in 0..GLYPH_W {
                        if row & (0x10 >> dx) != 0 {
                            put(img, x + dx, y0 + dy as i32, BLACK);
                        }
                    }
                }
            }
            x += GLYPH_W + 1;
        }
    }

    /// Keep a `w`-wide element fully inside `0..limit` (labels/plates near edges).
    fn clamp_span(start: i32, w: i32, limit: i32) -> i32 {
        start.min(limit - w).max(0)
    }

    fn ring(img: &mut RgbaImage, cx: i32, cy: i32, r: i32, thickness: i32, color: [u8; 4]) {
        let (r_out, r_in) = (r + thickness, r);
        for dy in -r_out..=r_out {
            for dx in -r_out..=r_out {
                let d2 = dx * dx + dy * dy;
                if d2 <= r_out * r_out && d2 > r_in * r_in {
                    put(img, cx + dx, cy + dy, color);
                }
            }
        }
    }

    /// B1: mark the EXECUTED point in the check image — ring + cross + the
    /// coordinate as text — so the model SEES where its click landed relative to
    /// the target instead of only reading its own number echoed back.
    pub fn executed_point(img: &mut RgbaImage, x: i32, y: i32) {
        ring(img, x, y, 8, 2, WHITE);
        ring(img, x, y, 6, 2, RED);
        for d in 3..=14 {
            for (px, py) in [(x + d, y), (x - d, y), (x, y + d), (x, y - d)] {
                put(img, px, py, WHITE);
            }
        }
        for d in 3..=13 {
            for (px, py) in [(x + d, y), (x - d, y), (x, y + d), (x, y - d)] {
                if d % 2 == 0 {
                    put(img, px, py, BLACK);
                }
            }
        }

        let label = format!("({x},{y})");
        let lx = clamp_span(x + 12, text_width(&label), img.width() as i32);
        let ly = clamp_span(y + 10, GLYPH_H, img.height() as i32);
        draw_label(img, lx, ly, &label);
    }

    /// B2: edge rulers in THIS image's own pixel space — ticks every 100px,
    /// labels every 200px — so the answer grid is visible in the image itself
    /// and a wrong-grid answer stops being label-compatible with what the model
    /// is looking at.
    const TICK_EVERY: i32 = 100;
    const LABEL_EVERY: i32 = 200;
    const TICK_LEN: i32 = 7;

    pub fn rulers(img: &mut RgbaImage) {
        let (w, h) = (img.width() as i32, img.height() as i32);

        let mut x = TICK_EVERY;
        while x < w {
            for y in 0..TICK_LEN {
                put(img, x, y, BLACK);
                put(img, x + 1, y, WHITE);
            }
            if x % LABEL_EVERY == 0 {
                let text = x.to_string();
                draw_label(img, clamp_span(x + 3, text_width(&text), w), 9, &text);
            }
            x += TICK_EVERY;
        }

        let mut y = TICK_EVERY;
        while y < h {
            for x in 0..TICK_LEN {
                put(img, x, y, BLACK);
                put(img, x, y + 1, WHITE);
            }
            if y % LABEL_EVERY == 0 {
                let text = y.to_string();
                draw_label(img, 9, clamp_span(y + 3, GLYPH_H, h), &text);
            }
            y += TICK_EVERY;
        }
    }

    /// B3: a numbered set-of-marks badge at an element's click point. The model
    /// answers with the NUMBER; the caller resolves it to the exact point — no
    /// pixel estimation at all. Badges are only ever drawn from the macOS AX
    /// mark collection, so the fn is scoped with it.
    #[cfg(target_os = "macos")]
    pub fn badge(img: &mut RgbaImage, x: i32, y: i32, n: usize) {
        let text = n.to_string();
        let half_w = (text_width(&text) / 2 + 4).max(8);

        fill_rect(img, x - half_w - 1, y - 7, 2 * half_w + 2, 14, BLACK);
        fill_rect(img, x - half_w, y - 6, 2 * half_w, 12, RED);
        draw_label(img, x - text_width(&text) / 2, y - 3, &text);
    }
}

// --- screenshot -------------------------------------------------------------

fn screenshot(req: &Value) -> Result<Value, String> {
    let display = target_display(req)?;
    let region = parse_region(req)?;
    capture_payload_encoded(
        &display,
        region,
        parse_jpeg_quality(req)?,
        parse_overlays(req)?,
    )
}

/// Grounding-integrity overlays for one capture (M28), all drawn on the SENT
/// image in its own pixel space: `rulers` (B2) makes the answer grid visible,
/// `annotate_point` (B1) marks an executed click in a check image, `marks` (B3)
/// badges accessibility click points and returns their id table.
#[derive(Default, Clone, Copy)]
struct Overlays {
    rulers: bool,
    marks: bool,
    annotate: Option<(i32, i32)>,
}

fn parse_overlays(req: &Value) -> Result<Overlays, String> {
    let rulers = req.get("rulers").and_then(Value::as_bool).unwrap_or(false);
    let marks = req.get("marks").and_then(Value::as_bool).unwrap_or(false);

    let annotate = match req.get("annotate_point") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let x = value
                .get("x")
                .and_then(Value::as_f64)
                .ok_or("annotate_point.x is missing or not a number")?;
            let y = value
                .get("y")
                .and_then(Value::as_f64)
                .ok_or("annotate_point.y is missing or not a number")?;
            Some((x.round() as i32, y.round() as i32))
        }
    };

    Ok(Overlays {
        rulers,
        marks,
        annotate,
    })
}

/// Opt into JPEG for this capture (1-100). Absent = PNG, the lossless default a
/// caller reading fine UI text wants.
fn parse_jpeg_quality(req: &Value) -> Result<Option<u8>, String> {
    match req.get("jpeg_quality") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(q) if (1..=100).contains(&q) => Ok(Some(q as u8)),
            _ => Err("jpeg_quality must be an integer 1-100".to_string()),
        },
    }
}

/// Fail FAST when the target display is asleep instead of engaging a capture.
/// ScreenCaptureKit delivers no frame from a sleeping display until an internal
/// ~30s give-up — long enough to bust a caller's action deadline, and a client
/// stuck in that wait wedges SCK for every later capture system-wide (observed
/// live, 2026-07-01). The typed error lets the caller say what is actually wrong.
#[cfg(target_os = "macos")]
fn ensure_display_awake(display: &Display) -> Result<(), String> {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        // boolean_t CGDisplayIsAsleep(CGDirectDisplayID display)
        fn CGDisplayIsAsleep(display: u32) -> u32;
    }

    if unsafe { CGDisplayIsAsleep(display.id) } != 0 {
        return Err("display_asleep".to_string());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn ensure_display_awake(_display: &Display) -> Result<(), String> {
    Ok(())
}

// --- bounded capture (the anti-stall watchdog) -------------------------------

/// Hard budget for one physical frame grab. A real capture takes ~0.2–0.5s; one
/// that exceeds this is stalled inside the OS capture service (the 2026-07-01
/// wedge: CGWindowListCreateImage's ScreenCaptureKit proxy waited out a ~30s XPC
/// semaphore PER CALL), and waiting would burn the caller's whole action
/// deadline. Fail fast with the typed `capture_stalled` instead.
const CAPTURE_STALL_MS: u64 = 5_000;

/// Set when a capture blew its budget: the OS capture backend is wedged and a
/// leaked worker thread is still stuck inside the syscall. There is no in-process
/// recovery (the stuck thread can't be cancelled), so after replying we EXIT and
/// let the owning parent (`Compux.PortDriver`) respawn a fresh sidecar with a
/// clean capture-service connection. Checked in `main` AFTER the response flush,
/// so the `capture_stalled` reply always reaches the caller first.
static CAPTURE_WEDGED: AtomicBool = AtomicBool::new(false);

/// Grab a display's physical frame on a worker thread, bounded by
/// `CAPTURE_STALL_MS`. Bounding (not moving to a different capture API) is the
/// fix: the newest xcap still captures via `CGWindowListCreateImage`, so ANY
/// backend can wedge — only a hard deadline is robust.
///
/// The worker re-resolves the monitor by its stable id (an xcap `Monitor` is not
/// `Send`; the id is a plain `u32`). Re-resolving by ID — not by list index —
/// means a display unplugged mid-action fails typed (`display_disconnected`)
/// instead of silently rebinding to whatever now occupies that index. On a hard
/// stall we set `CAPTURE_WEDGED` (→ process exit + respawn) so a leaked worker
/// can never pile up or wedge the process forever.
fn capture_display_image(monitor_id: u32) -> Result<image::RgbaImage, String> {
    let (tx, rx) = mpsc::channel();

    let spawned = thread::Builder::new()
        .name("compux-capture".to_string())
        .spawn(move || {
            let _ = tx.send(capture_by_id(monitor_id));
        });

    if spawned.is_err() {
        return Err("capture failed: could not spawn capture worker".to_string());
    }

    match rx.recv_timeout(Duration::from_millis(CAPTURE_STALL_MS)) {
        Ok(result) => result,

        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The worker is still stuck in the OS call; it holds a capture-service
            // resource this process can't reclaim. Flag for exit-after-reply.
            CAPTURE_WEDGED.store(true, Ordering::SeqCst);
            Err("capture_stalled".to_string())
        }

        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // Worker died without sending (a panic inside xcap). It leaked no stuck
            // resource, so keep serving — a retry may well succeed.
            Err("capture failed: capture worker exited without a result".to_string())
        }
    }
}

fn capture_by_id(monitor_id: u32) -> Result<image::RgbaImage, String> {
    let monitors = Monitor::all().map_err(|e| format!("enumerate displays: {e}"))?;

    let monitor = monitors
        .into_iter()
        .find(|m| m.id().map(|id| id == monitor_id).unwrap_or(false))
        .ok_or_else(|| "display_disconnected".to_string())?;

    monitor.capture_image().map_err(|e| format!("capture: {e}"))
}

/// Encode the sent image. PNG by default — lossless, which is what a caller reading
/// fine UI text wants. `jpeg_quality` opts into JPEG for a BULK, periodic caller
/// (a continuous screen feed): a full-desktop PNG runs to hundreds of KB, and at a
/// frame every couple of seconds that saturates the consumer's uplink; the same
/// frame as JPEG is roughly an order of magnitude smaller.
///
/// Deliberately NOT paired with a dimension cap. The sent width/height feed
/// `sent_scale`, which is the inverse used to map a click back to the desktop, so
/// shrinking them here would silently move every coordinate. Compression is the one
/// axis that shrinks the payload while leaving the coordinate space identical.
fn encode_image(
    image: &image::RgbaImage,
    w: u32,
    h: u32,
    jpeg_quality: Option<u8>,
) -> Result<(Vec<u8>, &'static str), String> {
    let mut out: Vec<u8> = Vec::new();

    match jpeg_quality {
        Some(quality) => {
            // The JPEG encoder takes no alpha channel; the capture is opaque, so
            // dropping it costs nothing.
            let rgb = image::DynamicImage::ImageRgba8(image.clone()).into_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
                .write_image(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| format!("encode jpeg: {e}"))?;
            Ok((out, "image/jpeg"))
        }
        None => {
            image::codecs::png::PngEncoder::new(&mut out)
                .write_image(image.as_raw(), w, h, image::ExtendedColorType::Rgba8)
                .map_err(|e| format!("encode png: {e}"))?;
            Ok((out, "image/png"))
        }
    }
}

fn capture_payload_encoded(
    display: &Display,
    requested: Option<Region>,
    jpeg_quality: Option<u8>,
    overlays: Overlays,
) -> Result<Value, String> {
    ensure_display_awake(display)?;
    let geom = &display.geom;
    let region = region_or_full(geom, requested);
    let crop = crop_rect(geom, &region);
    let (sent_w, sent_h) = crop.sent_dims();

    let image = capture_display_image(display.id)?;

    // Crop to the region's physical rect, then downscale to the sent size. The
    // model's coordinates live in this (sent) space; `to_logical` inverts it.
    let cropped = image::imageops::crop_imm(
        &image,
        crop.left_phys.round() as u32,
        crop.top_phys.round() as u32,
        crop.w_phys.round().max(1.0) as u32,
        crop.h_phys.round().max(1.0) as u32,
    )
    .to_image();

    let mut resized = image::imageops::resize(
        &cropped,
        sent_w,
        sent_h,
        image::imageops::FilterType::Triangle,
    );

    // Overlays draw on the sent image, bottom to top: grid, badges, then the
    // executed-point marker so it is never covered.
    if overlays.rulers {
        overlay::rulers(&mut resized);
    }
    let marks = if overlays.marks {
        Some(collect_marks(geom, &region, &mut resized))
    } else {
        None
    };
    if let Some((ax, ay)) = overlays.annotate {
        overlay::executed_point(&mut resized, ax, ay);
    }

    let (encoded, mime) = encode_image(&resized, sent_w, sent_h, jpeg_quality)?;
    let data = base64::engine::general_purpose::STANDARD.encode(&encoded);

    let mut payload = json!({
        "ok": true,
        "mime": mime,
        "width": sent_w,
        "height": sent_h,
        "scale": geom.scale_factor,
        "origin": { "x": geom.origin_x.round() as i32, "y": geom.origin_y.round() as i32 },
        "physical": { "width": geom.phys_w, "height": geom.phys_h },
        "region": {
            "x": region.x.round() as i64,
            "y": region.y.round() as i64,
            "w": region.w.round() as i64,
            "h": region.h.round() as i64
        },
        "data": data
    });

    // The cursor's position in this image's coordinates, when it falls inside the
    // captured region — useful for drag/hover reasoning. Absent if off-region.
    if let (Some((cursor_x, cursor_y)), Some(object)) =
        (cursor_point(geom, &region), payload.as_object_mut())
    {
        object.insert(
            "cursor".to_string(),
            json!({ "x": cursor_x, "y": cursor_y }),
        );
    }

    // B3: the mark table, present (possibly empty) whenever marks were requested,
    // so the caller can tell "zero accessibility marks" from "none asked for".
    if let (Some(info), Some(object)) = (marks, payload.as_object_mut()) {
        object.insert("marks".to_string(), Value::Array(info.entries));
        if let Some(note) = info.ax_activation {
            object.insert("ax_activation".to_string(), json!(note));
        }
        if info.truncated > 0 {
            object.insert("marks_truncated".to_string(), json!(info.truncated));
        }
    }

    Ok(payload)
}

/// The badge cap keeps a marked image readable — a dense tree can expose
/// hundreds of interactive nodes, and a badge soup grounds worse than pixels.
/// Tree-walk order is roughly top-down, so the cap drops the least prominent.
#[cfg(target_os = "macos")]
const MAX_MARKS: usize = 60;

struct MarksInfo {
    entries: Vec<Value>,
    ax_activation: Option<String>,
    truncated: usize,
}

#[cfg(target_os = "macos")]
fn collect_marks(geom: &Geometry, region: &Region, img: &mut image::RgbaImage) -> MarksInfo {
    let (nodes, ax_activation) = interactive_in_view(geom, region);
    let truncated = nodes.len().saturating_sub(MAX_MARKS);

    let mut entries = Vec::new();
    for (index, (node, (sx, sy))) in nodes.into_iter().take(MAX_MARKS).enumerate() {
        let id = index + 1;
        overlay::badge(img, sx as i32, sy as i32, id);
        entries.push(json!({ "id": id, "role": node.role, "title": node.title, "x": sx, "y": sy }));
    }

    MarksInfo {
        entries,
        ax_activation,
        truncated,
    }
}

#[cfg(not(target_os = "macos"))]
fn collect_marks(_geom: &Geometry, _region: &Region, _img: &mut image::RgbaImage) -> MarksInfo {
    MarksInfo {
        entries: Vec::new(),
        ax_activation: Some("marks are only supported on macOS".to_string()),
        truncated: 0,
    }
}

// --- input ------------------------------------------------------------------

#[derive(Deserialize)]
struct Point {
    x: f64,
    y: f64,
}

fn enigo() -> Result<Enigo, String> {
    // Never let library INIT raise the macOS Accessibility dialog mid-action:
    // prompting is an operator flow that belongs exclusively to the
    // `request_permissions` action (the setup card's button). Without the grant,
    // actions fail/no-op and the consumer's probe reports the state loudly.
    let settings = Settings {
        open_prompt_to_get_permissions: false,
        ..Settings::default()
    };

    Enigo::new(&settings).map_err(|e| format!("init input: {e}"))
}

// Best-effort cursor position in sent-image coords (None if input can't be read or
// the cursor lies outside the region) — a screenshot never fails on the cursor read.
fn cursor_point(geom: &Geometry, region: &Region) -> Option<(i64, i64)> {
    let input = enigo().ok()?;
    let (lx, ly) = input.location().ok()?;
    to_sent(geom, region, lx as f64, ly as f64)
}

fn coords(req: &Value) -> Result<(f64, f64), String> {
    let x = req.get("x").and_then(Value::as_f64).ok_or("missing x")?;
    let y = req.get("y").and_then(Value::as_f64).ok_or("missing y")?;
    Ok((x, y))
}

fn modifiers(req: &Value) -> Vec<Key> {
    req.get("modifiers")
        .and_then(Value::as_array)
        .map(|m| {
            m.iter()
                .filter_map(|v| v.as_str().and_then(modifier_key))
                .collect()
        })
        .unwrap_or_default()
}

fn mouse_move(req: &Value) -> Result<Value, String> {
    let display = target_display(req)?;
    let region = region_or_full(&display.geom, parse_region(req)?);
    let (x, y) = coords(req)?;
    let (lx, ly) = to_logical(&display.geom, &region, x, y);
    let mut e = enigo()?;
    e.move_mouse(lx, ly, Coordinate::Abs)
        .map_err(|e| format!("move: {e}"))?;
    // Same settle as the acting verbs: this action's entire promise is "the pointer
    // is now here", and a posted move alone leaves that pending (hover would land on
    // whatever the pointer had not left yet).
    pointer::settle(lx, ly)?;
    // read-only: no post-action screenshot
    Ok(json!({ "ok": true }))
}

fn click(req: &Value, button: Button, count: u32) -> Result<Value, String> {
    let display = target_display(req)?;
    let region = region_or_full(&display.geom, parse_region(req)?);
    let (x, y) = coords(req)?;
    let (lx, ly) = to_logical(&display.geom, &region, x, y);
    let mods = modifiers(req);

    let mut e = enigo()?;
    e.move_mouse(lx, ly, Coordinate::Abs)
        .map_err(|e| format!("move: {e}"))?;
    pointer::settle(lx, ly)?;
    hold(&mut e, &mods, Direction::Press)?;
    for _ in 0..count {
        e.button(button, Direction::Click)
            .map_err(|e| format!("click: {e}"))?;
    }
    hold(&mut e, &mods, Direction::Release)?;

    post(req, &display)
}

/// Drag pacing. A zero-dwell teleport drag lands inside one render frame, which
/// rAF-gated drag handlers (chessground boards, HTML5 drag-and-drop, sliders,
/// maps) never observe as a drag at all — they demote it to a click. Every
/// mature driver interpolates with dwell (Playwright `mouse.move(steps)`,
/// pyautogui `dragTo(duration)`); these are that, as internal constants.
const DRAG_STEPS: u32 = 10;
const DRAG_STEP_MS: u64 = 20;
const DRAG_GRAB_MS: u64 = 60;
const DRAG_DROP_MS: u64 = 50;

fn drag(req: &Value) -> Result<Value, String> {
    let display = target_display(req)?;
    let region = region_or_full(&display.geom, parse_region(req)?);
    let from: Point = parse_point(req, "from")?;
    let to: Point = parse_point(req, "to")?;
    let (fx, fy) = to_logical(&display.geom, &region, from.x, from.y);
    let (tx, ty) = to_logical(&display.geom, &region, to.x, to.y);

    let mut e = enigo()?;
    e.move_mouse(fx, fy, Coordinate::Abs)
        .map_err(|e| format!("move: {e}"))?;
    pointer::settle(fx, fy)?;
    e.button(Button::Left, Direction::Press)
        .map_err(|e| format!("press: {e}"))?;
    // Let the press register (and the target arm its drag) before moving.
    thread::sleep(Duration::from_millis(DRAG_GRAB_MS));
    drag_through(&mut e, &drag_path(fx, fy, tx, ty, DRAG_STEPS))?;
    pointer::settle(tx, ty)?;
    // Dwell at the destination so the drop is observed where it happens.
    thread::sleep(Duration::from_millis(DRAG_DROP_MS));
    e.button(Button::Left, Direction::Release)
        .map_err(|e| format!("release: {e}"))?;

    post(req, &display)
}

/// The interpolated pointer path from start to end: `steps` evenly spaced
/// points, endpoints exact (the last point IS the destination), each axis
/// monotonic. Pure, so the geometry is unit-testable without posting events.
fn drag_path(fx: i32, fy: i32, tx: i32, ty: i32, steps: u32) -> Vec<(i32, i32)> {
    (1..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            (
                (fx as f32 + (tx - fx) as f32 * t).round() as i32,
                (fy as f32 + (ty - fy) as f32 * t).round() as i32,
            )
        })
        .collect()
}

/// macOS posts each step as an EXPLICIT `LeftMouseDragged` (enigo's `move_mouse`
/// derives its event type from a live `pressedMouseButtons()` read that races
/// the just-posted mouse-down and then emits `MouseMoved` — a hover mid-press).
#[cfg(target_os = "macos")]
fn drag_through(_e: &mut Enigo, path: &[(i32, i32)]) -> Result<(), String> {
    for &(x, y) in path {
        pointer::drag_step(x, y)?;
        thread::sleep(Duration::from_millis(DRAG_STEP_MS));
    }

    Ok(())
}

/// X11 motion while the button is pressed IS the drag — no distinct event type —
/// so enigo's own motion injection is correct here.
#[cfg(not(target_os = "macos"))]
fn drag_through(e: &mut Enigo, path: &[(i32, i32)]) -> Result<(), String> {
    for &(x, y) in path {
        e.move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| format!("drag: {e}"))?;
        thread::sleep(Duration::from_millis(DRAG_STEP_MS));
    }

    Ok(())
}

fn scroll(req: &Value) -> Result<Value, String> {
    let display = target_display(req)?;
    let region = region_or_full(&display.geom, parse_region(req)?);
    let (x, y) = coords(req)?;
    let (lx, ly) = to_logical(&display.geom, &region, x, y);
    let amount = req.get("amount").and_then(Value::as_i64).unwrap_or(3) as i32;
    let (axis, length) = match req.get("direction").and_then(Value::as_str) {
        Some("up") => (Axis::Vertical, -amount),
        Some("down") => (Axis::Vertical, amount),
        Some("left") => (Axis::Horizontal, -amount),
        Some("right") => (Axis::Horizontal, amount),
        other => return Err(format!("bad scroll direction: {other:?}")),
    };

    let mut e = enigo()?;
    e.move_mouse(lx, ly, Coordinate::Abs)
        .map_err(|e| format!("move: {e}"))?;
    pointer::settle(lx, ly)?;
    e.scroll(length, axis).map_err(|e| format!("scroll: {e}"))?;

    post(req, &display)
}

fn type_text(req: &Value) -> Result<Value, String> {
    let text = req
        .get("text")
        .and_then(Value::as_str)
        .ok_or("missing text")?;
    let mut e = enigo()?;
    e.text(text).map_err(|e| format!("type: {e}"))?;
    post(req, &target_display(req)?)
}

fn key_chord(req: &Value) -> Result<Value, String> {
    let chord = req
        .get("chord")
        .and_then(Value::as_str)
        .ok_or("missing chord")?;
    let parts: Vec<&str> = chord.split('+').map(str::trim).collect();
    let (mod_parts, key_part) = parts.split_at(parts.len().saturating_sub(1));
    let key_name = key_part.first().copied().ok_or("empty chord")?;

    let mods: Vec<Key> = mod_parts.iter().filter_map(|m| modifier_key(m)).collect();
    let main = named_key(key_name).ok_or_else(|| format!("unknown key: {key_name}"))?;

    let mut e = enigo()?;
    hold(&mut e, &mods, Direction::Press)?;
    let res = e
        .key(main, Direction::Click)
        .map_err(|e| format!("key: {e}"));
    hold(&mut e, &mods, Direction::Release)?;
    res?;

    post(req, &target_display(req)?)
}

fn wait(req: &Value) -> Result<Value, String> {
    // Clamped like every other blocking verb: the wire is one-request-one-response,
    // so an unbounded sleep would hold the whole session hostage to one argument.
    let ms = req
        .get("ms")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_WAIT_FOR_IDLE_MS);
    thread::sleep(Duration::from_millis(ms));
    Ok(json!({ "ok": true }))
}

// --- v2 actions: wait_for_change / paste / elements -------------------------

/// Block until the region's pixels change (or `timeout_ms` elapses), then return
/// the resulting screenshot plus a `changed` flag. Each poll captures the frame
/// (xcap has no sub-region capture) and diffs an AVERAGED thumbnail hash of the
/// region; the poll budget is bounded by the caller's protocol.
fn wait_for_change(req: &Value) -> Result<Value, String> {
    let display = target_display(req)?;
    let region = region_or_full(&display.geom, parse_region(req)?);
    let timeout_ms = req
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(10_000)
        .min(MAX_WAIT_FOR_IDLE_MS);
    let poll_ms = req
        .get("poll_ms")
        .and_then(Value::as_u64)
        .unwrap_or(250)
        .max(1);

    let baseline = region_hash(&display, &region)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        thread::sleep(Duration::from_millis(poll_ms));
        let changed = region_hash(&display, &region)? != baseline;
        if changed || Instant::now() >= deadline {
            // The returned frame becomes the caller's coordinate view, so it
            // carries the same `rulers` grid an explicit screenshot would.
            let overlays = Overlays {
                rulers: req.get("rulers").and_then(Value::as_bool).unwrap_or(false),
                marks: false,
                annotate: None,
            };
            let mut payload = capture_payload_encoded(&display, Some(region), None, overlays)?;
            if let Some(object) = payload.as_object_mut() {
                object.insert("changed".to_string(), json!(changed));
            }
            return Ok(payload);
        }
    }
}

/// A change-detector: capture the region and hash an AVERAGED thumbnail of it.
/// Triangle (not Nearest) folds every source pixel into a cell, so a small change
/// still perturbs the hash instead of landing between sample points and being
/// missed — a miss would block `wait_for_change` to its full timeout.
fn region_hash(display: &Display, region: &Region) -> Result<u64, String> {
    ensure_display_awake(display)?;
    let geom = &display.geom;
    let crop = crop_rect(geom, region);
    let image = capture_display_image(display.id)?;
    let cropped = image::imageops::crop_imm(
        &image,
        crop.left_phys.round() as u32,
        crop.top_phys.round() as u32,
        crop.w_phys.round().max(1.0) as u32,
        crop.h_phys.round().max(1.0) as u32,
    )
    .to_image();
    let thumb = image::imageops::resize(&cropped, 256, 256, image::imageops::FilterType::Triangle);
    let mut hasher = DefaultHasher::new();
    thumb.as_raw().hash(&mut hasher);
    Ok(hasher.finish())
}

// --- idle detection (coexistence: yield the seat to a present human) --------

/// Upper bound on ANY blocking verb (`wait`, `wait_for_change`, `wait_for_idle`),
/// so one argument can never hang the sidecar past its per-action deadline (30s
/// in Fermix). Cross-platform since the `wait`/`wait_for_change` clamps adopted
/// it — no longer macOS-gated.
const MAX_WAIT_FOR_IDLE_MS: u64 = 25_000;

/// Milliseconds since the last input event the OS saw, via CoreGraphics' session
/// idle clock. NOTE: this counts ANY input, INCLUDING this sidecar's own synthetic
/// events (`enigo` posts through the HID tap) — so a caller that also drives input
/// must disambiguate "the human vs my own last action" itself (that is policy, and
/// lives in the consumer; compux only reports the raw number). macOS only; mirrors
/// the `CGDisplayIsAsleep` FFI shape.
#[cfg(target_os = "macos")]
fn human_idle_ms() -> Result<u64, String> {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        // CFTimeInterval CGEventSourceSecondsSinceLastEventType(
        //     CGEventSourceStateID stateID, CGEventType eventType)
        fn CGEventSourceSecondsSinceLastEventType(state_id: u32, event_type: u32) -> f64;
    }

    // kCGEventSourceStateHIDSystemState = 1; kCGAnyInputEventType = 0xFFFF_FFFF.
    const HID_SYSTEM_STATE: u32 = 1;
    const ANY_INPUT_EVENT: u32 = 0xFFFF_FFFF;

    let seconds =
        unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, ANY_INPUT_EVENT) };

    if seconds.is_finite() && seconds >= 0.0 {
        Ok((seconds * 1000.0).round() as u64)
    } else {
        Err("idle query returned an invalid interval".to_string())
    }
}

/// Report ms since the last input event. Operational (a policy-support probe), NOT a
/// model action — excluded from `hello`'s advertised verbs like `probe`.
#[cfg(target_os = "macos")]
fn idle_ms() -> Result<Value, String> {
    Ok(json!({ "ok": true, "idle_ms": human_idle_ms()? }))
}

#[cfg(not(target_os = "macos"))]
fn idle_ms() -> Result<Value, String> {
    Err("idle detection is only supported on macOS".to_string())
}

/// Block until the human has been idle for `idle_ms` (default 1000), bounded by
/// `timeout_ms` (default 3000, capped at `MAX_WAIT_FOR_IDLE_MS`). Returns
/// `idle: true` if the quiet window was reached, `idle: false` if it timed out with
/// the human still active. Reuses the `wait_for_change` bounded-poll idiom so a
/// consumer can schedule input into a human-idle gap.
#[cfg(target_os = "macos")]
fn wait_for_idle(req: &Value) -> Result<Value, String> {
    let idle_target = req.get("idle_ms").and_then(Value::as_u64).unwrap_or(1_000);
    let timeout_ms = req
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(3_000)
        .min(MAX_WAIT_FOR_IDLE_MS);
    let poll_ms = req
        .get("poll_ms")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .max(1);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let idle = human_idle_ms()?;
        if idle >= idle_target {
            return Ok(json!({ "ok": true, "idle": true, "idle_ms": idle }));
        }
        if Instant::now() >= deadline {
            return Ok(json!({ "ok": true, "idle": false, "idle_ms": idle }));
        }
        thread::sleep(Duration::from_millis(poll_ms));
    }
}

#[cfg(not(target_os = "macos"))]
fn wait_for_idle(_req: &Value) -> Result<Value, String> {
    Err("idle detection is only supported on macOS".to_string())
}

/// Paste `text` via the clipboard + the platform paste chord — fast and
/// unicode-safe for long strings that char-by-char typing would stall on.
fn paste(req: &Value) -> Result<Value, String> {
    let text = req
        .get("text")
        .and_then(Value::as_str)
        .ok_or("missing text")?;
    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("clipboard: {e}"))?;
    // Best-effort save of the user's clipboard TEXT so paste doesn't silently destroy
    // it (a non-text clipboard — image/files — can't be preserved here).
    let prior = clipboard.get_text().ok();
    clipboard
        .set_text(text)
        .map_err(|e| format!("clipboard set: {e}"))?;
    // Let the pasteboard write settle before the paste keystroke.
    thread::sleep(Duration::from_millis(50));

    let mut e = enigo()?;
    let modifier = paste_modifier();
    e.key(modifier, Direction::Press)
        .map_err(|e| format!("paste modifier: {e}"))?;
    e.key(Key::Unicode('v'), Direction::Click)
        .map_err(|e| format!("paste key: {e}"))?;
    e.key(modifier, Direction::Release)
        .map_err(|e| format!("paste modifier: {e}"))?;

    // Restore the prior clipboard once the target has consumed the paste (a small
    // delay avoids racing the paste read).
    if let Some(previous) = prior {
        thread::sleep(Duration::from_millis(80));
        let _ = clipboard.set_text(previous);
    }

    post(req, &target_display(req)?)
}

#[cfg(target_os = "macos")]
fn paste_modifier() -> Key {
    Key::Meta
}

#[cfg(not(target_os = "macos"))]
fn paste_modifier() -> Key {
    Key::Control
}

// --- windows ----------------------------------------------------------------

/// Hard cap on the reported window list. A desktop can carry a hundred windows
/// (helpers, panels, offscreen shells); an unbounded list would bloat the reply and
/// bury the two or three that matter. Front-most first, so the cap drops the least
/// relevant.
const MAX_WINDOWS: usize = 40;

/// Enumerate the on-screen windows of a display, each with its bounds ALREADY
/// EXPRESSED AS A `region` in that display's screenshot space.
///
/// This is the precision lever on a large or ultrawide display. A full-screen
/// capture is downscaled to fit the sent budget, so the app the caller cares about
/// arrives at a fraction of its real size; a `region` crop is rescaled to that same
/// budget on its own, so cropping to one window recovers the lost resolution
/// (up to native 1:1 — on a 3840x1080 display, ~1.7x over the full view for a
/// typical browser window).
///
/// Returning a ready-made `region` — rather than raw window geometry — is the whole
/// point: the caller pastes it straight into `screenshot`/click and rides the
/// EXISTING, proven region transform. No second coordinate system is introduced, so
/// this cannot reintroduce the region-offset class of bug.
///
/// READ-ONLY: pure metadata — no capture, no input.
fn windows(req: &Value) -> Result<Value, String> {
    let display = target_display(req)?;
    let full = Region::full(&display.geom);
    let mut listed = window_entries(&display.geom, &full)?;
    listed.truncate(MAX_WINDOWS);
    Ok(json!({ "ok": true, "windows": listed }))
}

fn window_entries(geom: &Geometry, full: &Region) -> Result<Vec<Value>, String> {
    let mut windows = xcap::Window::all().map_err(|e| format!("enumerate windows: {e}"))?;

    // Front-most first: xcap's `z` grows toward the front, so the caller reads the
    // window it most likely means at the top of the list.
    windows.sort_by_cached_key(|w| std::cmp::Reverse(w.z().unwrap_or(0)));

    // Shell-layer windows (the Dock, the menu bar, floating overlays) are not
    // windows a caller can work IN — listing them invites zooming into a
    // phantom region (observed live: "Dock — region {0,0,1931,543}").
    let normal = shell_window_filter()?;

    let mut entries = Vec::new();
    for window in windows {
        if window.is_minimized().unwrap_or(false) {
            continue;
        }
        if let (Some(normal), Ok(id)) = (&normal, window.id()) {
            if !normal.contains(&id) {
                continue;
            }
        }
        if let Some(region) = window_region(geom, full, &window) {
            entries.push(json!({
                "id": window.id().unwrap_or_default(),
                "app": window.app_name().unwrap_or_default(),
                "title": window.title().unwrap_or_default(),
                "focused": window.is_focused().unwrap_or(false),
                "region": {
                    "x": region.x.round() as i64,
                    "y": region.y.round() as i64,
                    "w": region.w.round() as i64,
                    "h": region.h.round() as i64
                }
            }));
        }
    }
    Ok(entries)
}

/// A window's LOGICAL bounds (`kCGWindowBounds` on macOS — the same unit `Geometry`
/// uses) converted into this display's sent-image space. Thin effectful shell; the
/// arithmetic is `logical_bounds_to_region` so it can be tested without a desktop.
fn window_region(geom: &Geometry, full: &Region, window: &xcap::Window) -> Option<Region> {
    let (x, y) = (window.x().ok()? as f64, window.y().ok()? as f64);
    let (w, h) = (window.width().ok()? as f64, window.height().ok()? as f64);
    logical_bounds_to_region(geom, full, x, y, w, h)
}

/// Convert logical bounds to a sent-space `region`, clipped to the display's own
/// sent image. `None` for a degenerate window, or one that does not overlap this
/// display, so the listing never offers a region that cannot be captured.
fn logical_bounds_to_region(
    geom: &Geometry,
    full: &Region,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Option<Region> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }

    let k = sent_scale(geom) as f64;
    let left = (x - geom.origin_x as f64) * k;
    let top = (y - geom.origin_y as f64) * k;

    let x0 = left.max(full.x);
    let y0 = top.max(full.y);
    let x1 = (left + w * k).min(full.x + full.w);
    let y1 = (top + h * k).min(full.y + full.h);

    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        return None;
    }

    Some(Region {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Enumerate interactive accessibility elements (role + label + a click point in
/// screenshot coordinates) so the model can target by element, not raw pixels.
fn elements(req: &Value) -> Result<Value, String> {
    let display = target_display(req)?;
    let region = region_or_full(&display.geom, parse_region(req)?);
    elements_for(&display.geom, &region)
}

#[cfg(target_os = "macos")]
fn elements_for(geom: &Geometry, region: &Region) -> Result<Value, String> {
    let (nodes, ax_activation) = interactive_in_view(geom, region);

    let items: Vec<Value> = nodes
        .iter()
        .map(|(node, (x, y))| json!({ "role": node.role, "title": node.title, "x": x, "y": y }))
        .collect();

    let mut payload = json!({ "ok": true, "elements": items });
    if let (Some(note), Some(object)) = (ax_activation, payload.as_object_mut()) {
        object.insert("ax_activation".to_string(), json!(note));
    }
    Ok(payload)
}

/// An interactive AX node paired with its click point in sent-image space.
#[cfg(target_os = "macos")]
type ViewNode = (ax::Node, (i64, i64));

/// Interactive AX elements of ONE application: the ones whose centers fall
/// inside this view (as sent-space points) plus the TOTAL interactive count the
/// walk saw before the view filter. The total is what distinguishes "the app's
/// tree is gated/empty" (activation territory) from "the app has elements, just
/// none inside this region" (nothing to activate). Shared by `elements` and the
/// `marks` overlay so the two can never disagree about what is clickable.
#[cfg(target_os = "macos")]
fn in_view_nodes(pid: i32, geom: &Geometry, region: &Region) -> (Vec<ViewNode>, usize) {
    let nodes = ax::interactive_elements_of(pid);
    let total = nodes.len();

    let in_view = nodes
        .into_iter()
        .filter_map(|node| {
            let center_x = node.x + node.w / 2.0;
            let center_y = node.y + node.h / 2.0;
            to_sent(geom, region, center_x, center_y).map(|point| (node, point))
        })
        .collect();

    (in_view, total)
}

/// The application to read accessibility from: the one OWNING the window the
/// caller is looking at, resolved from the window list. Deliberately NOT
/// `AXFocusedApplication`: that query proved flaky from this spawned process
/// (observed live 2026-07-29: "no focused application" three seconds after
/// `windows` listed Chrome focused), and during a voice call it can resolve to
/// the floating voice companion instead of the app on screen — which is how an
/// activation attempt hit a non-Chromium app and died with AXError -25208
/// while Chrome sat frontmost. Known limitation, accepted: an app with no
/// named on-screen window (a menu-bar app with an open popover) cannot be
/// resolved this way — the old query never reached those reliably either, and
/// the typed no-window note says what happened.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq)]
struct TargetApp {
    pid: i32,
    app: String,
}

/// The CGWindowIDs of NORMAL-layer windows (kCGWindowLayer == 0) on screen.
/// The Dock, menu bar, Control Center, and floating shell panels live on
/// non-zero layers while reporting large bounds at high z — observed live
/// 2026-07-30: the Dock's full-screen window (layer 20) won target selection
/// for BOTH a full-screen and a browser-window region, so `marks`/`elements`
/// activated accessibility on the Dock and walked an empty tree. Layer 0 is
/// the OS's own definition of "an app window a user works in", so filtering by
/// it kills the class instead of chasing shell apps by name. (Measured here:
/// Dock 20, menu bar 24, Control Center 25, floating companion panels 3.)
#[cfg(target_os = "macos")]
mod window_layer {
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::string::CFString;
    use std::collections::HashSet;
    use std::ffi::c_void;

    /// kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements.
    const ON_SCREEN_ONLY: u32 = 1 << 0;
    const EXCLUDE_DESKTOP: u32 = 1 << 4;
    /// kCFNumberSInt64Type.
    const SINT64: isize = 4;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFTypeRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: CFTypeRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
        fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        fn CFNumberGetValue(number: CFTypeRef, the_type: isize, out: *mut c_void) -> bool;
    }

    /// `Err` when the OS refuses the listing — the same underlying call the
    /// window enumeration rides, so the two fail together and loudly.
    pub fn normal_window_ids() -> Result<HashSet<u32>, String> {
        unsafe {
            let list_ref = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
            if list_ref.is_null() {
                return Err("window layer listing failed".to_string());
            }
            let list = CFType::wrap_under_create_rule(list_ref);
            let layer_key = CFString::new("kCGWindowLayer");
            let number_key = CFString::new("kCGWindowNumber");

            let mut ids = HashSet::new();
            for index in 0..CFArrayGetCount(list.as_CFTypeRef()) {
                let dict = CFArrayGetValueAtIndex(list.as_CFTypeRef(), index);
                if dict.is_null() {
                    continue;
                }
                if let (Some(0), Some(id)) =
                    (read_i64(dict, &layer_key), read_i64(dict, &number_key))
                {
                    ids.insert(id as u32);
                }
            }
            Ok(ids)
        }
    }

    // CFDictionaryGetValue returns a borrowed (Get-rule) ref — no release here.
    unsafe fn read_i64(dict: CFTypeRef, key: &CFString) -> Option<i64> {
        let value = CFDictionaryGetValue(dict, key.as_CFTypeRef());
        if value.is_null() {
            return None;
        }
        let mut out: i64 = 0;
        if CFNumberGetValue(value, SINT64, &mut out as *mut _ as *mut c_void) {
            Some(out)
        } else {
            None
        }
    }
}

/// The normal-layer window-id set on macOS; `None` where layers do not exist
/// (X11 has no shell-overlay class to exclude).
#[cfg(target_os = "macos")]
fn shell_window_filter() -> Result<Option<std::collections::HashSet<u32>>, String> {
    window_layer::normal_window_ids().map(Some)
}

#[cfg(not(target_os = "macos"))]
fn shell_window_filter() -> Result<Option<std::collections::HashSet<u32>>, String> {
    Ok(None)
}

/// The window list as selection candidates, FRONT TO BACK: minimized windows,
/// shell-layer windows (`window_layer` — the Dock/menu-bar/overlay class that
/// reports huge bounds at high z and shadowed real targets), off-display
/// windows, and windows with no readable pid are out. An enumeration failure
/// is a typed error, not an empty list — the two mean different things to the
/// caller.
#[cfg(target_os = "macos")]
fn window_candidates(geom: &Geometry) -> Result<Vec<(Region, TargetApp)>, String> {
    let full = Region::full(geom);
    let mut windows = xcap::Window::all().map_err(|e| format!("window enumeration failed: {e}"))?;
    windows.sort_by_cached_key(|w| std::cmp::Reverse(w.z().unwrap_or(0)));

    let normal = shell_window_filter()?;

    let mut candidates = Vec::new();
    for window in windows {
        if window.is_minimized().unwrap_or(false) {
            continue;
        }
        if let (Some(normal), Ok(id)) = (&normal, window.id()) {
            if !normal.contains(&id) {
                continue;
            }
        }
        let app = window.app_name().unwrap_or_default();
        let Some(bounds) = window_region(geom, &full, &window) else {
            continue;
        };
        let Ok(pid) = window.pid() else {
            continue;
        };
        candidates.push((
            bounds,
            TargetApp {
                pid: pid as i32,
                app,
            },
        ));
    }
    Ok(candidates)
}

/// Substantial-overlap floor: the fraction of the REQUEST region a window must
/// cover to win on stacking order alone. High enough that a small always-on-top
/// panel (the voice companion, ~1-3% of a window region) can never shadow the
/// window being read; low enough that a normal app window over a larger
/// background one clears it easily.
#[cfg(target_os = "macos")]
const AX_TARGET_MIN_OVERLAP: f64 = 0.10;

/// Pick the app the view is ABOUT from front-to-back candidates: the frontmost
/// window with SUBSTANTIAL overlap of the request region wins — stacking order
/// is the tiebreak the screen actually shows, so a maximized background window
/// can never beat the smaller window in front of it. Only when nothing is
/// substantial (a sparse desktop of small windows) does raw maximum overlap
/// decide. Pure, so the selection semantics are unit-tested.
#[cfg(target_os = "macos")]
fn select_target(candidates: Vec<(Region, TargetApp)>, region: &Region) -> Option<TargetApp> {
    let region_area = (region.w * region.h).max(1.0);

    let mut best: Option<(f64, TargetApp)> = None;
    for (bounds, target) in candidates {
        let area = overlap_area(&bounds, region);
        if area <= 0.0 {
            continue;
        }
        if area >= AX_TARGET_MIN_OVERLAP * region_area {
            return Some(target);
        }
        let better = best.as_ref().map(|(b, _)| area > *b).unwrap_or(true);
        if better {
            best = Some((area, target));
        }
    }
    best.map(|(_, target)| target)
}

/// Overlap area of two sent-space rectangles; 0 when disjoint.
#[cfg(target_os = "macos")]
fn overlap_area(a: &Region, b: &Region) -> f64 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if w <= 0.0 || h <= 0.0 {
        0.0
    } else {
        w * h
    }
}

/// Bounded settle for a lazily-built accessibility tree: Chromium switches web
/// accessibility on when an AX client starts querying it, then needs a moment
/// to build the tree — so an empty first walk re-queries on a short cadence
/// instead of concluding emptiness from one look.
#[cfg(target_os = "macos")]
const AX_SETTLE_POLL_MS: u64 = 300;
#[cfg(target_os = "macos")]
const AX_SETTLE_POLLS: u32 = 5;

/// B4, revised on live evidence: enumerate the TARGET app's tree rooted at its
/// own application element. Activation (one typed attempt —
/// `AXManualAccessibility`, then `AXEnhancedUserInterface` on an
/// attribute-unsupported / not-implemented answer — current Chrome refuses
/// BOTH yet serves its full tree to a querying client anyway) plus the bounded
/// settle poll fire ONLY when the app's tree walked to zero interactive nodes
/// overall: an app whose elements merely fall outside the view has nothing
/// gated, and flipping enhanced-UI mode on it would be a pure side effect.
/// Every outcome lands in the note — which app was read, what activation did,
/// how long the tree took — so no result is silent about its cause.
#[cfg(target_os = "macos")]
fn interactive_in_view(geom: &Geometry, region: &Region) -> (Vec<ViewNode>, Option<String>) {
    let candidates = match window_candidates(geom) {
        Ok(candidates) => candidates,
        Err(reason) => return (Vec::new(), Some(reason)),
    };
    let Some(target) = select_target(candidates, region) else {
        return (
            Vec::new(),
            Some("no application window to target for accessibility".to_string()),
        );
    };

    let (found, total) = in_view_nodes(target.pid, geom, region);
    if !found.is_empty() {
        return (found, Some(format!("read {}", target.app)));
    }
    if total > 0 {
        let note = format!(
            "{}: {total} interactive element(s) in the app, none inside this view",
            target.app
        );
        return (Vec::new(), Some(note));
    }

    let attempt = ax::activate_accessibility(target.pid);
    for poll in 1..=AX_SETTLE_POLLS {
        thread::sleep(Duration::from_millis(AX_SETTLE_POLL_MS));
        let (again, again_total) = in_view_nodes(target.pid, geom, region);
        let waited = u64::from(poll) * AX_SETTLE_POLL_MS;
        if !again.is_empty() {
            let note = format!(
                "{}: tree appeared after {waited}ms ({})",
                target.app,
                attempt_note(&attempt)
            );
            return (again, Some(note));
        }
        if again_total > 0 {
            // The tree came alive; this view just contains none of it — more
            // polling cannot change that.
            let note = format!(
                "{}: tree appeared after {waited}ms ({}), but its {again_total} element(s) \
                 are outside this view",
                target.app,
                attempt_note(&attempt)
            );
            return (Vec::new(), Some(note));
        }
    }

    let waited = u64::from(AX_SETTLE_POLLS) * AX_SETTLE_POLL_MS;
    let note = format!(
        "{}: no accessibility elements — {}; tree still empty after {waited}ms",
        target.app,
        attempt_note(&attempt)
    );
    (Vec::new(), Some(note))
}

#[cfg(target_os = "macos")]
fn attempt_note(attempt: &Result<&'static str, String>) -> String {
    match attempt {
        Ok(attribute) => format!("{attribute} activated"),
        Err(reason) => format!("activation refused: {reason}"),
    }
}

#[cfg(not(target_os = "macos"))]
fn elements_for(_geom: &Geometry, _region: &Region) -> Result<Value, String> {
    Err("element enumeration is only supported on macOS".to_string())
}

// --- accessibility (inspect) ------------------------------------------------

/// Report the accessibility element under a (screenshot-space) point: its role and
/// label. READ-ONLY — a grounding/judgment aid (confirm what control is there before
/// a consequential click), not a gate. Coordinates map through the same region
/// transform as input, so the model can inspect a zoomed point too.
fn inspect(req: &Value) -> Result<Value, String> {
    let display = target_display(req)?;
    let region = region_or_full(&display.geom, parse_region(req)?);
    let (x, y) = coords(req)?;
    let (lx, ly) = to_logical(&display.geom, &region, x, y);
    inspect_at(lx as f32, ly as f32)
}

#[cfg(target_os = "macos")]
fn inspect_at(x: f32, y: f32) -> Result<Value, String> {
    match ax::element_at(x, y) {
        Some(el) => Ok(json!({
            "ok": true,
            "found": true,
            "role": el.role,
            "title": el.title,
            "description": el.description,
            "value": el.value,
        })),
        None => Ok(json!({ "ok": true, "found": false })),
    }
}

#[cfg(not(target_os = "macos"))]
fn inspect_at(_x: f32, _y: f32) -> Result<Value, String> {
    Err("element inspection is only supported on macOS".to_string())
}

/// macOS Accessibility FFI for `inspect`. Reads the element under a global LOGICAL
/// point via the system-wide AX element; core-foundation owns CFType memory (drop =
/// release), and every AX call's error code is checked before the out-param is read.
/// NON-PROMPTING and read-only. Like the rest of the native driver, the runtime
/// behavior needs a real Mac with the Accessibility grant to verify.
#[cfg(target_os = "macos")]
mod ax {
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;
    use std::sync::Mutex;

    type AXUIElementRef = CFTypeRef;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateSystemWide() -> AXUIElementRef;
        // AXError; 0 == success. On success `element` is set to a +1 (Copy-rule) ref.
        fn AXUIElementCopyElementAtPosition(
            application: AXUIElementRef,
            x: f32,
            y: f32,
            element: *mut AXUIElementRef,
        ) -> i32;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        // Extract the concrete value (CGPoint/CGSize) an AXValue wraps; false if the
        // requested type doesn't match.
        fn AXValueGetValue(value: CFTypeRef, the_type: u32, out: *mut c_void) -> bool;
        // B4 activation: set an attribute on an application element, created
        // from the pid the caller resolved off the window list.
        fn AXUIElementSetAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: CFTypeRef,
        ) -> i32;
        fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: CFTypeRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFArrayGetTypeID() -> usize;
    }

    // The AX attribute-name constants (`kAXRoleAttribute`, …) are header `extern
    // const`s that don't link as symbols; their string VALUES are stable + documented,
    // so we build the CFStrings from those instead.
    const ROLE: &str = "AXRole";
    const TITLE: &str = "AXTitle";
    const DESCRIPTION: &str = "AXDescription";
    const VALUE: &str = "AXValue";
    const CHILDREN: &str = "AXChildren";
    const POSITION: &str = "AXPosition";
    const SIZE: &str = "AXSize";

    // AXValueType tags for AXValueGetValue.
    const AXVALUE_CGPOINT: u32 = 1;
    const AXVALUE_CGSIZE: u32 = 2;

    // Bound the tree walk so a deep/huge hierarchy can't stall the request:
    // MAX_NODES caps elements COLLECTED, MAX_VISITED caps nodes TRAVERSED (a large
    // sparse subtree has few interactive nodes but many to walk), MAX_DEPTH the depth.
    const MAX_DEPTH: usize = 14;
    const MAX_NODES: usize = 250;
    const MAX_VISITED: usize = 3000;

    // Roles worth surfacing as clickable targets (set-of-marks).
    const INTERACTIVE: &[&str] = &[
        "AXButton",
        "AXMenuItem",
        "AXMenuButton",
        "AXPopUpButton",
        "AXCheckBox",
        "AXRadioButton",
        "AXTextField",
        "AXTextArea",
        "AXComboBox",
        "AXLink",
        "AXTabButton",
        "AXSlider",
        "AXDisclosureTriangle",
        "AXCell",
    ];

    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    pub struct Element {
        pub role: Option<String>,
        pub title: Option<String>,
        pub description: Option<String>,
        pub value: Option<String>,
    }

    pub fn element_at(x: f32, y: f32) -> Option<Element> {
        unsafe {
            let system_ref = AXUIElementCreateSystemWide();
            if system_ref.is_null() {
                return None;
            }
            let system = CFType::wrap_under_create_rule(system_ref);

            let mut element_ref: AXUIElementRef = std::ptr::null();
            let err =
                AXUIElementCopyElementAtPosition(system.as_CFTypeRef(), x, y, &mut element_ref);
            if err != 0 || element_ref.is_null() {
                return None;
            }
            let element = CFType::wrap_under_create_rule(element_ref);

            Some(Element {
                role: copy_string_attr(&element, ROLE),
                title: copy_string_attr(&element, TITLE),
                description: copy_string_attr(&element, DESCRIPTION),
                value: copy_string_attr(&element, VALUE),
            })
        }
    }

    // Read any CFType-valued AX attribute (a +1 Copy-rule ref, released on drop).
    unsafe fn copy_element_attr(element: &CFType, attribute: &str) -> Option<CFType> {
        let attr = CFString::new(attribute);
        let mut value_ref: CFTypeRef = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(
            element.as_CFTypeRef(),
            attr.as_concrete_TypeRef(),
            &mut value_ref,
        );
        if err != 0 || value_ref.is_null() {
            return None;
        }
        Some(CFType::wrap_under_create_rule(value_ref))
    }

    // Read a string-valued AX attribute. Non-string values (e.g. a slider's number)
    // downcast to None — we only surface text labels.
    unsafe fn copy_string_attr(element: &CFType, attribute: &str) -> Option<String> {
        copy_element_attr(element, attribute)?
            .downcast::<CFString>()
            .map(|s| s.to_string())
    }

    /// An interactive element with its global-logical frame.
    pub struct Node {
        pub role: Option<String>,
        pub title: Option<String>,
        pub x: f64,
        pub y: f64,
        pub w: f64,
        pub h: f64,
    }

    // --- accessibility activation (M28 B4) -----------------------------------

    /// Chromium/Electron's opt-in switch: the family builds its AX tree only for
    /// detected assistive clients, and this per-app attribute is the documented
    /// way to request it manually. Current Chrome refuses it (and serves its
    /// tree to a querying client regardless); Electron-family builds honor it.
    const MANUAL_ACCESSIBILITY: &str = "AXManualAccessibility";
    /// Second choice ONLY on a typed rejection: it also flips apps into an
    /// enhanced-UI mode window managers react to (layout side effects), which
    /// is why it is never tried first.
    const ENHANCED_UI: &str = "AXEnhancedUserInterface";
    /// AXError `kAXErrorAttributeUnsupported`.
    const ATTRIBUTE_UNSUPPORTED: i32 = -25205;
    /// AXError `kAXErrorNotImplemented` — what a process that does not
    /// implement the attribute's setter answers (observed live from Chrome and
    /// from non-Chromium apps).
    const NOT_IMPLEMENTED: i32 = -25208;

    /// The typed rejections that mean "this attribute is not a thing here" —
    /// the deterministic criterion for trying the second attribute.
    pub fn attribute_rejected(code: i32) -> bool {
        code == ATTRIBUTE_UNSUPPORTED || code == NOT_IMPLEMENTED
    }

    /// `(pid, attribute)` pairs this process switched ON — cleared on the exit
    /// paths so one enumeration never leaves an app in an altered AX mode.
    static ACTIVATED: Mutex<Vec<(i32, &'static str)>> = Mutex::new(Vec::new());

    /// Ask ONE application — the pid the caller resolved off the window list —
    /// to expose its accessibility tree. Returns the attribute that activated,
    /// or a typed reason. Setting the attribute on an app that does not gate
    /// its tree is a harmless typed error — callers attempt unconditionally on
    /// an empty enumeration, no app-family sniffing.
    pub fn activate_accessibility(pid: i32) -> Result<&'static str, String> {
        unsafe {
            let app_ref = AXUIElementCreateApplication(pid);
            if app_ref.is_null() {
                return Err(format!("no accessibility connection to pid {pid}"));
            }
            let app = CFType::wrap_under_create_rule(app_ref);

            match set_bool_attr(&app, MANUAL_ACCESSIBILITY, true) {
                0 => {
                    record(pid, MANUAL_ACCESSIBILITY);
                    Ok(MANUAL_ACCESSIBILITY)
                }
                code if attribute_rejected(code) => match set_bool_attr(&app, ENHANCED_UI, true) {
                    0 => {
                        record(pid, ENHANCED_UI);
                        Ok(ENHANCED_UI)
                    }
                    code => Err(format!("AXError {code}")),
                },
                code => Err(format!("AXError {code}")),
            }
        }
    }

    /// Best-effort teardown: switch OFF every attribute this process switched on.
    pub fn clear_activations() {
        let entries: Vec<(i32, &'static str)> = match ACTIVATED.lock() {
            Ok(mut list) => list.drain(..).collect(),
            Err(_poisoned) => return,
        };

        for (pid, attribute) in entries {
            unsafe {
                let app_ref = AXUIElementCreateApplication(pid);
                if app_ref.is_null() {
                    continue;
                }
                let app = CFType::wrap_under_create_rule(app_ref);
                let _ = set_bool_attr(&app, attribute, false);
            }
        }
    }

    fn record(pid: i32, attribute: &'static str) {
        if let Ok(mut list) = ACTIVATED.lock() {
            if !list.contains(&(pid, attribute)) {
                list.push((pid, attribute));
            }
        }
    }

    unsafe fn set_bool_attr(element: &CFType, attribute: &str, value: bool) -> i32 {
        let attr = CFString::new(attribute);
        let flag = if value {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };
        AXUIElementSetAttributeValue(
            element.as_CFTypeRef(),
            attr.as_concrete_TypeRef(),
            flag.as_CFTypeRef(),
        )
    }

    /// Walk ONE application's accessibility tree — rooted at its own
    /// application element, never at `AXFocusedApplication` (a query that
    /// proved flaky from this spawned process and, during a voice call, can
    /// name the floating companion instead of the app on screen) — and collect
    /// interactive elements (bounded depth + count) with global-logical frames.
    /// NON-PROMPTING, read-only; runtime behavior needs a real Mac with the
    /// Accessibility grant.
    pub fn interactive_elements_of(pid: i32) -> Vec<Node> {
        unsafe {
            let app_ref = AXUIElementCreateApplication(pid);
            if app_ref.is_null() {
                return Vec::new();
            }
            let root = CFType::wrap_under_create_rule(app_ref);
            let mut out = Vec::new();
            let mut visited = 0;
            walk(&root, 0, &mut visited, &mut out);
            out
        }
    }

    unsafe fn walk(element: &CFType, depth: usize, visited: &mut usize, out: &mut Vec<Node>) {
        if depth > MAX_DEPTH || out.len() >= MAX_NODES || *visited >= MAX_VISITED {
            return;
        }
        *visited += 1;
        if let Some(node) = interactive_node(element) {
            out.push(node);
        }
        for child in copy_children(element) {
            walk(&child, depth + 1, visited, out);
        }
    }

    unsafe fn interactive_node(element: &CFType) -> Option<Node> {
        let role = copy_string_attr(element, ROLE)?;
        if !INTERACTIVE.contains(&role.as_str()) {
            return None;
        }
        let (x, y, w, h) = element_frame(element)?;
        Some(Node {
            title: copy_string_attr(element, TITLE)
                .or_else(|| copy_string_attr(element, DESCRIPTION))
                .or_else(|| copy_string_attr(element, VALUE)),
            role: Some(role),
            x,
            y,
            w,
            h,
        })
    }

    unsafe fn copy_children(element: &CFType) -> Vec<CFType> {
        let Some(children) = copy_element_attr(element, CHILDREN) else {
            return Vec::new();
        };
        let array = children.as_CFTypeRef();
        // AXChildren SHOULD be a CFArray, but an app with a custom/broken AX impl can
        // return another CFType; the CFArray getters would then type-confuse and read
        // garbage. Verify the concrete type before treating it as an array.
        if CFGetTypeID(array) != CFArrayGetTypeID() {
            return Vec::new();
        }
        let count = CFArrayGetCount(array);
        let mut out = Vec::new();
        let mut index = 0;
        while index < count && out.len() < MAX_NODES {
            let child_ref = CFArrayGetValueAtIndex(array, index);
            if !child_ref.is_null() {
                out.push(CFType::wrap_under_get_rule(child_ref));
            }
            index += 1;
        }
        out
    }

    unsafe fn element_frame(element: &CFType) -> Option<(f64, f64, f64, f64)> {
        let position = copy_element_attr(element, POSITION)?;
        let size = copy_element_attr(element, SIZE)?;
        let mut point = CGPoint { x: 0.0, y: 0.0 };
        let mut dims = CGSize {
            width: 0.0,
            height: 0.0,
        };
        let got_point = AXValueGetValue(
            position.as_CFTypeRef(),
            AXVALUE_CGPOINT,
            &mut point as *mut _ as *mut c_void,
        );
        let got_size = AXValueGetValue(
            size.as_CFTypeRef(),
            AXVALUE_CGSIZE,
            &mut dims as *mut _ as *mut c_void,
        );
        if got_point && got_size && dims.width > 0.0 && dims.height > 0.0 {
            Some((point.x, point.y, dims.width, dims.height))
        } else {
            None
        }
    }
}

// --- helpers ----------------------------------------------------------------

/// After a mutating action, include the post-action screen state when the request
/// asked for it (`screenshot_after`). Always the FULL display (region: None) so the
/// model sees the broader result of a zoomed action; it can re-zoom with an explicit
/// `screenshot` if it needs detail.
///
/// B1: the check draws the EXECUTED point (the action's own coordinates — the drag
/// destination for drags) into the image, so the caller SEES where the click landed
/// relative to its target instead of only reading its own number echoed back. An
/// unregioned action's coordinates are full-screen sent space — the same space this
/// full-display check captures in. `rulers` is honored from the action request.
fn post(req: &Value, display: &Display) -> Result<Value, String> {
    if req
        .get("screenshot_after")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let overlays = Overlays {
            rulers: req.get("rulers").and_then(Value::as_bool).unwrap_or(false),
            marks: false,
            annotate: executed_point_of(req),
        };
        capture_payload_encoded(display, None, None, overlays)
    } else {
        Ok(json!({ "ok": true }))
    }
}

/// The point a pointer action executed at, from its own request: `to` for drags,
/// top-level x/y otherwise; None for keyboard/uncoordinated actions.
fn executed_point_of(req: &Value) -> Option<(i32, i32)> {
    let (x, y) = if let Some(to) = req.get("to") {
        (to.get("x")?.as_f64()?, to.get("y")?.as_f64()?)
    } else {
        (req.get("x")?.as_f64()?, req.get("y")?.as_f64()?)
    };
    Some((x.round() as i32, y.round() as i32))
}

fn hold(e: &mut Enigo, mods: &[Key], dir: Direction) -> Result<(), String> {
    for key in mods {
        e.key(*key, dir).map_err(|e| format!("modifier: {e}"))?;
    }
    Ok(())
}

fn parse_point(req: &Value, field: &str) -> Result<Point, String> {
    serde_json::from_value(req.get(field).cloned().unwrap_or(Value::Null))
        .map_err(|_| format!("bad {field} point"))
}

fn modifier_key(name: &str) -> Option<Key> {
    match name {
        "cmd" | "meta" | "super" => Some(Key::Meta),
        "ctrl" | "control" => Some(Key::Control),
        "alt" | "option" => Some(Key::Alt),
        "shift" => Some(Key::Shift),
        _ => None,
    }
}

/// Map a chord key token to an enigo Key. Single printable chars become a
/// Unicode key; common named keys are mapped explicitly. Extend as needed.
fn named_key(name: &str) -> Option<Key> {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "enter" | "return" => Some(Key::Return),
        "tab" => Some(Key::Tab),
        "esc" | "escape" => Some(Key::Escape),
        "space" => Some(Key::Space),
        "backspace" => Some(Key::Backspace),
        "delete" | "del" => Some(Key::Delete),
        "up" => Some(Key::UpArrow),
        "down" => Some(Key::DownArrow),
        "left" => Some(Key::LeftArrow),
        "right" => Some(Key::RightArrow),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "f1" => Some(Key::F1),
        "f2" => Some(Key::F2),
        "f3" => Some(Key::F3),
        "f4" => Some(Key::F4),
        "f5" => Some(Key::F5),
        "f6" => Some(Key::F6),
        "f7" => Some(Key::F7),
        "f8" => Some(Key::F8),
        "f9" => Some(Key::F9),
        "f10" => Some(Key::F10),
        "f11" => Some(Key::F11),
        "f12" => Some(Key::F12),
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(Key::Unicode(c)),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The drag path's geometry is what makes an interpolated drag land exactly:
    // the LAST point must BE the destination (a rounded near-miss would drop the
    // piece a pixel off), the step count fixed, and each axis monotonic so the
    // pointer never doubles back mid-drag.
    #[test]
    fn drag_path_ends_exactly_at_the_destination() {
        let path = drag_path(10, 20, 313, 207, DRAG_STEPS);
        assert_eq!(path.len(), DRAG_STEPS as usize);
        assert_eq!(*path.last().unwrap(), (313, 207));
    }

    #[test]
    fn drag_path_is_monotonic_on_both_axes() {
        let path = drag_path(300, 400, 20, 40, DRAG_STEPS);
        let mut prev = (300, 400);
        for &(x, y) in &path {
            assert!(x <= prev.0, "x doubled back: {x} after {}", prev.0);
            assert!(y <= prev.1, "y doubled back: {y} after {}", prev.1);
            prev = (x, y);
        }
        assert_eq!(prev, (20, 40));
    }

    #[test]
    fn drag_path_handles_a_zero_length_drag() {
        let path = drag_path(50, 60, 50, 60, DRAG_STEPS);
        assert!(path.iter().all(|&p| p == (50, 60)));
    }

    // An empty monitor list (locked / asleep / no GUI session) maps to the typed
    // `no_active_display` for ANY requested index — never the bad-index message,
    // which would wrongly suggest another monitor could work. The non-empty path
    // needs a real `Monitor` (an OS handle) and is covered by on-device runs.
    #[test]
    fn empty_monitor_list_is_no_active_display_for_any_index() {
        assert_eq!(
            select_monitor(Vec::new(), 0).err(),
            Some("no_active_display".to_string())
        );
        assert_eq!(
            select_monitor(Vec::new(), 4).err(),
            Some("no_active_display".to_string())
        );
    }

    // Retina reference display: 2880x1800 physical, 2x scale -> 1440x900 logical,
    // origin (0,0). Geometry is constructible without a real Monitor, so the
    // coordinate math (the #1 offset-bug class) is unit-tested here, not on-device.
    fn retina_geom() -> Geometry {
        Geometry {
            phys_w: 2880,
            phys_h: 1800,
            logical_w: 1440.0,
            logical_h: 900.0,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_factor: 2.0,
        }
    }

    #[test]
    fn full_screenshot_mapping_matches_the_simple_formula() {
        // A full screenshot is a region spanning the whole sent image, so the unified
        // map must reduce to the original `origin + (x,y)/k`.
        let g = retina_geom();
        let region = Region::full(&g);
        let k = sent_scale(&g);
        let (lx, ly) = to_logical(&g, &region, 683.0, 450.0);
        assert_eq!(lx, (683.0_f32 / k).round() as i32);
        assert_eq!(ly, (450.0_f32 / k).round() as i32);
    }

    #[test]
    fn region_zoom_corners_map_back_into_the_region() {
        let g = retina_geom();
        let k = sent_scale(&g);
        // The lower-right quadrant, expressed in full-display sent pixels.
        let region = Region {
            x: (720.0 * k) as f64,
            y: (450.0 * k) as f64,
            w: (720.0 * k) as f64,
            h: (450.0 * k) as f64,
        };

        // Top-left of the zoomed image is the region origin in logical points.
        assert_eq!(to_logical(&g, &region, 0.0, 0.0), (720, 450));

        // Bottom-right of the zoomed image is the region's far corner.
        let crop = crop_rect(&g, &region);
        let (sw, sh) = crop.sent_dims();
        let (lx, ly) = to_logical(&g, &region, sw as f64, sh as f64);
        assert!((lx - 1440).abs() <= 2, "lx={lx}");
        assert!((ly - 900).abs() <= 2, "ly={ly}");
    }

    // `elements` maps AX frames back through `to_sent`, so a click point is only as
    // accurate as `to_sent` inverting `to_logical`. Round-trip a grid of sent points:
    // sent -> logical -> sent must return the origin (within double-rounding slack).
    #[test]
    fn to_sent_inverts_to_logical_full_screen() {
        let g = retina_geom();
        let region = Region::full(&g);
        for (sx, sy) in [(0.0_f64, 0.0_f64), (683.0, 450.0), (1200.0, 700.0)] {
            let (lx, ly) = to_logical(&g, &region, sx, sy);
            let (rx, ry) = to_sent(&g, &region, lx as f64, ly as f64).expect("in bounds");
            assert!((rx - sx as i64).abs() <= 2, "x: sent={sx} back={rx}");
            assert!((ry - sy as i64).abs() <= 2, "y: sent={sy} back={ry}");
        }
    }

    #[test]
    fn to_sent_inverts_to_logical_in_a_zoomed_region() {
        let g = retina_geom();
        let k = sent_scale(&g);
        let region = Region {
            x: (720.0 * k) as f64,
            y: (450.0 * k) as f64,
            w: (720.0 * k) as f64,
            h: (450.0 * k) as f64,
        };
        let crop = crop_rect(&g, &region);
        let (sw, sh) = crop.sent_dims();
        for (sx, sy) in [(0.0_f64, 0.0_f64), ((sw / 2) as f64, (sh / 2) as f64)] {
            let (lx, ly) = to_logical(&g, &region, sx, sy);
            let (rx, ry) = to_sent(&g, &region, lx as f64, ly as f64).expect("in region");
            // Zoom magnifies, so a logical i32 rounding is worth >1 sent px — allow 3.
            assert!((rx - sx as i64).abs() <= 3, "x: sent={sx} back={rx}");
            assert!((ry - sy as i64).abs() <= 3, "y: sent={sy} back={ry}");
        }
    }

    #[test]
    fn to_sent_round_trips_with_nonzero_origin_and_physical_mismatch() {
        let mut g = macbook_air_geom();
        g.origin_x = 1440.0;
        g.origin_y = 100.0;
        let region = Region::full(&g);
        let crop = crop_rect(&g, &region);
        let (sw, sh) = crop.sent_dims();
        let (sx, sy) = ((sw as f64) / 3.0, (sh as f64) / 3.0);
        let (lx, ly) = to_logical(&g, &region, sx, sy);
        let (rx, ry) = to_sent(&g, &region, lx as f64, ly as f64).expect("in bounds");
        assert!((rx - sx as i64).abs() <= 2, "x: sent={sx} back={rx}");
        assert!((ry - sy as i64).abs() <= 2, "y: sent={sy} back={ry}");
    }

    #[test]
    fn to_sent_is_none_outside_the_sent_image() {
        let g = retina_geom();
        let region = Region::full(&g);
        // Left of / above the display origin, and past the far edge.
        assert_eq!(to_sent(&g, &region, -100.0, 10.0), None);
        assert_eq!(
            to_sent(
                &g,
                &region,
                g.logical_w as f64 + 100.0,
                g.logical_h as f64 + 100.0
            ),
            None
        );
    }

    // 13" Retina: 2560x1600 physical, 2x -> 1280x800 logical. logical_long (1280) <=
    // MAX_EDGE < phys_long (2560) — the regime a logical-derived sent scale got wrong.
    fn macbook_air_geom() -> Geometry {
        Geometry {
            phys_w: 2560,
            phys_h: 1600,
            logical_w: 1280.0,
            logical_h: 800.0,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_factor: 2.0,
        }
    }

    /// The display the window listing exists for: 3840x1080 at 1x, where a full
    /// capture is squeezed to 1366x384 and a window crop wins most of it back.
    fn ultrawide_geom() -> Geometry {
        Geometry {
            phys_w: 3840,
            phys_h: 1080,
            logical_w: 3840.0,
            logical_h: 1080.0,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_factor: 1.0,
        }
    }

    #[test]
    fn window_bounds_become_a_region_in_sent_space() {
        let g = ultrawide_geom();
        let full = Region::full(&g);
        // A 1600x1000 window at logical (400,60): k = 1366/3840 = 0.3557.
        let r =
            logical_bounds_to_region(&g, &full, 400.0, 60.0, 1600.0, 1000.0).expect("on screen");

        let k = sent_scale(&g) as f64;
        assert!((r.x - 400.0 * k).abs() < 1.0, "x: {}", r.x);
        assert!((r.y - 60.0 * k).abs() < 1.0, "y: {}", r.y);
        assert!((r.w - 1600.0 * k).abs() < 1.0, "w: {}", r.w);
        assert!((r.h - 1000.0 * k).abs() < 1.0, "h: {}", r.h);
    }

    /// The whole point of the action: cropping to the window recovers resolution a
    /// full capture spends on empty desktop. The margin is 1.5x (not the old 2x):
    /// the area budget already recovered part of the gap for the FULL view on this
    /// display (0.356 → 0.503), which narrows the crop's relative win.
    #[test]
    fn a_window_crop_is_sharper_than_the_full_screen() {
        let g = ultrawide_geom();
        let full = Region::full(&g);
        let r =
            logical_bounds_to_region(&g, &full, 400.0, 60.0, 1600.0, 1000.0).expect("on screen");

        let full_k = sent_scale(&g) as f64;
        let crop = crop_rect(&g, &r);
        let (sent_w, _) = crop.sent_dims();
        let window_k = sent_w as f64 / 1600.0;

        assert!(
            window_k > full_k * 1.5,
            "a window crop must be clearly sharper: full={full_k} window={window_k}"
        );
    }

    // --- M28 B4 rev: AX target selection + activation codes ------------------

    #[cfg(target_os = "macos")]
    fn candidate(x: f64, y: f64, w: f64, h: f64, pid: i32, app: &str) -> (Region, TargetApp) {
        (
            Region { x, y, w, h },
            TargetApp {
                pid,
                app: app.to_string(),
            },
        )
    }

    /// The frontmost SUBSTANTIAL window wins on stacking order — a maximized
    /// background window must never beat the smaller window in front of it
    /// (occlusion-blind max-overlap was the reviewed-out bug), and a small
    /// always-on-top panel is never substantial, so it can never shadow the
    /// window being read.
    #[cfg(target_os = "macos")]
    #[test]
    fn select_target_prefers_the_frontmost_substantial_window() {
        let full = Region {
            x: 0.0,
            y: 0.0,
            w: 1931.0,
            h: 543.0,
        };
        // Front to back: tiny voice panel, focused Chrome, maximized Slack.
        let candidates = vec![
            candidate(1700.0, 400.0, 120.0, 90.0, 10, "FermixPet"),
            candidate(0.0, 16.0, 823.0, 481.0, 20, "Google Chrome"),
            candidate(0.0, 0.0, 1931.0, 543.0, 30, "Slack"),
        ];

        let chosen = select_target(candidates, &full).expect("a target");
        assert_eq!(chosen.app, "Google Chrome");
    }

    /// A region call scoped to a window picks that window even when the panel
    /// floats ABOVE it inside the region.
    #[cfg(target_os = "macos")]
    #[test]
    fn select_target_ignores_a_small_panel_over_the_region() {
        let request = Region {
            x: 0.0,
            y: 16.0,
            w: 823.0,
            h: 481.0,
        };
        let candidates = vec![
            candidate(600.0, 300.0, 120.0, 90.0, 10, "FermixPet"),
            candidate(0.0, 16.0, 823.0, 481.0, 20, "Google Chrome"),
        ];

        let chosen = select_target(candidates, &request).expect("a target");
        assert_eq!(chosen.app, "Google Chrome");
    }

    /// With nothing substantial, raw maximum overlap decides — and a LATER,
    /// larger-overlap candidate beats an earlier smaller one (kills a
    /// first-hit-wins regression), while an exact tie keeps the frontmost.
    #[cfg(target_os = "macos")]
    #[test]
    fn select_target_falls_to_max_overlap_below_the_floor() {
        let full = Region {
            x: 0.0,
            y: 0.0,
            w: 1931.0,
            h: 543.0,
        };
        let candidates = vec![
            candidate(100.0, 100.0, 80.0, 60.0, 10, "Tiny"),
            candidate(400.0, 200.0, 300.0, 150.0, 20, "MiniPlayer"),
        ];
        let chosen = select_target(candidates, &full).expect("a target");
        assert_eq!(chosen.app, "MiniPlayer");

        let tied = vec![
            candidate(0.0, 0.0, 100.0, 100.0, 1, "Front"),
            candidate(500.0, 0.0, 100.0, 100.0, 2, "Back"),
        ];
        let chosen = select_target(tied, &full).expect("a target");
        assert_eq!(chosen.app, "Front", "an exact tie keeps the frontmost");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn select_target_is_none_without_overlap() {
        let request = Region {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 300.0,
        };
        let candidates = vec![candidate(1500.0, 400.0, 200.0, 100.0, 10, "Elsewhere")];
        assert_eq!(select_target(candidates, &request), None);
        assert_eq!(select_target(Vec::new(), &request), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn overlap_area_geometry() {
        let a = Region {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        };
        let inside = Region {
            x: 2.0,
            y: 2.0,
            w: 4.0,
            h: 4.0,
        };
        let disjoint = Region {
            x: 20.0,
            y: 0.0,
            w: 5.0,
            h: 5.0,
        };
        let partial = Region {
            x: 5.0,
            y: 5.0,
            w: 10.0,
            h: 10.0,
        };

        assert_eq!(overlap_area(&a, &inside), 16.0);
        assert_eq!(overlap_area(&a, &disjoint), 0.0);
        assert_eq!(overlap_area(&a, &partial), 25.0);
    }

    /// The two typed rejections fall through to the second attribute; every
    /// other AXError is terminal.
    #[cfg(target_os = "macos")]
    #[test]
    fn attribute_rejection_codes() {
        assert!(ax::attribute_rejected(-25205));
        assert!(ax::attribute_rejected(-25208));
        assert!(!ax::attribute_rejected(-25204));
        assert!(!ax::attribute_rejected(0));
    }

    // --- M28 B5: the area budget ---------------------------------------------

    /// The incident display: a pure long-edge cap sent 1366x384 (unreadable);
    /// the area budget recovers ~1931x543 — same pixel count as 1366x768.
    #[test]
    fn ultrawide_full_view_uses_the_area_budget() {
        let g = ultrawide_geom();
        let region = Region::full(&g);
        let crop = crop_rect(&g, &region);
        let (sw, sh) = crop.sent_dims();

        assert_eq!((sw, sh), (1931, 543), "sent dims");
        assert!(
            sw > MAX_EDGE,
            "the long edge may exceed MAX_EDGE under the area rule"
        );
        assert!(
            sw * sh <= MAX_AREA + sw,
            "within the area budget (rounding slack)"
        );

        // The center of the sent image still maps to the display center.
        let (lx, ly) = to_logical(&g, &region, (sw as f64) / 2.0, (sh as f64) / 2.0);
        assert!((lx - 1920).abs() <= 2, "lx={lx}");
        assert!((ly - 540).abs() <= 2, "ly={ly}");
    }

    /// A 16:9 display is the budget's fixed point: 1366x768 exactly, as before.
    #[test]
    fn sixteen_nine_full_view_is_unchanged_by_the_area_budget() {
        let g = Geometry {
            phys_w: 1920,
            phys_h: 1080,
            logical_w: 1920.0,
            logical_h: 1080.0,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_factor: 1.0,
        };
        let crop = crop_rect(&g, &Region::full(&g));
        assert_eq!(crop.sent_dims(), (1366, 768));
    }

    /// A crop that fits the long edge ships native — the incident's 1355x959
    /// region crop (1.30MP) must NOT be shrunk by the area rule; the looser
    /// budget wins. This is the regression a pure-area budget would introduce.
    #[test]
    fn a_crop_that_fits_the_long_edge_stays_native() {
        let crop = CropRect {
            left_phys: 64.7,
            top_phys: 30.9,
            w_phys: 1355.0,
            h_phys: 959.0,
        };
        assert!((crop.sent_scale() - 1.0).abs() < f32::EPSILON);
        assert_eq!(crop.sent_dims(), (1355, 959));
    }

    /// Retina 16:10 (2880x1800): the edge rule (0.474) beats the area rule
    /// (0.450) — behavior identical to the pure long-edge cap.
    #[test]
    fn retina_full_view_keeps_the_long_edge_budget() {
        let g = retina_geom();
        let crop = crop_rect(&g, &Region::full(&g));
        let (sw, _sh) = crop.sent_dims();
        assert_eq!(sw, MAX_EDGE);
    }

    #[test]
    fn a_window_is_clipped_to_the_display_it_overlaps() {
        let g = ultrawide_geom();
        let full = Region::full(&g);
        // Straddles the left edge: half of it lies off this display.
        let r = logical_bounds_to_region(&g, &full, -500.0, 0.0, 1000.0, 400.0).expect("overlaps");

        assert!(r.x >= full.x, "clipped left edge: {}", r.x);
        assert!(r.x + r.w <= full.x + full.w + 1.0, "within the sent image");
        assert!(
            (r.w - 500.0 * sent_scale(&g) as f64).abs() < 1.0,
            "w: {}",
            r.w
        );
    }

    #[test]
    fn an_offscreen_or_degenerate_window_is_not_listed() {
        let g = ultrawide_geom();
        let full = Region::full(&g);

        assert!(
            logical_bounds_to_region(&g, &full, 0.0, 0.0, 0.0, 500.0).is_none(),
            "zero-width window"
        );
        assert!(
            logical_bounds_to_region(&g, &full, 9000.0, 0.0, 800.0, 600.0).is_none(),
            "entirely to the right of this display"
        );
    }

    /// Regression guard for the region-offset class of bug: a window's region must
    /// survive the round trip through the very transform clicks use.
    #[test]
    fn a_window_region_round_trips_through_the_click_transform() {
        let g = ultrawide_geom();
        let full = Region::full(&g);
        let r =
            logical_bounds_to_region(&g, &full, 400.0, 60.0, 1600.0, 1000.0).expect("on screen");

        // The window's own top-left, read as the origin of the magnified crop.
        let (lx, ly) = to_logical(&g, &r, 0.0, 0.0);
        assert!((lx - 400).abs() <= 2, "logical x: {lx}");
        assert!((ly - 60).abs() <= 2, "logical y: {ly}");
    }

    // The DISPATCH is what this pins; whether a display is attached is the host's
    // business (CI has none, and a locked Mac reports `no_active_display`). So a
    // window list OR a typed display error both pass — "unknown action" never does.
    #[test]
    fn handle_dispatches_windows() {
        let response = handle(&json!({ "action": "windows" }));

        let unknown = response["error"]
            .as_str()
            .map(|e| e.contains("unknown action"))
            .unwrap_or(false);

        assert!(!unknown, "windows must reach its handler: {response}");
        assert!(
            response["windows"].is_array() || response["ok"] == json!(false),
            "expected a window list or a typed error: {response}"
        );
    }

    #[test]
    fn full_mapping_is_correct_when_logical_fits_but_physical_does_not() {
        // The full sent image is the PHYSICAL display downscaled (1366 wide), NOT the
        // logical one left at 1.0. A click read off it must map to the logical center,
        // and Region::full's coordinate space must equal the real sent dims.
        let g = macbook_air_geom();
        let region = Region::full(&g);

        let crop = crop_rect(&g, &region);
        let (sw, sh) = crop.sent_dims();

        // sent_scale is sent_dim/logical_dim — not a clamped 1.0.
        let k = sent_scale(&g);
        assert!((k - sw as f32 / g.logical_w).abs() < 0.01, "k={k} sw={sw}");

        // Region::full's reported width equals the actual sent width (the canary that
        // a logical-derived scale would break: it would report 1280, not ~1366).
        assert_eq!(region.w.round() as u32, sw);
        assert!(sw > 1300 && sw <= MAX_EDGE, "sw={sw}");

        // The center of the sent image maps to the logical center (640, 400).
        let (lx, ly) = to_logical(&g, &region, (sw as f64) / 2.0, (sh as f64) / 2.0);
        assert!((lx - 640).abs() <= 2, "lx={lx}");
        assert!((ly - 400).abs() <= 2, "ly={ly}");
    }

    #[test]
    fn mapping_respects_a_nonzero_display_origin() {
        // A secondary display offset to the right: sent (0,0) is that display's origin.
        let mut g = retina_geom();
        g.origin_x = 1440.0;
        let region = Region::full(&g);
        assert_eq!(to_logical(&g, &region, 0.0, 0.0).0, 1440);
    }

    // --- M28 B1/B2/B3: overlay placement ------------------------------------

    fn blank(w: u32, h: u32) -> image::RgbaImage {
        image::RgbaImage::from_pixel(w, h, image::Rgba([20, 20, 20, 255]))
    }

    fn changed_pixels(img: &image::RgbaImage) -> usize {
        img.pixels().filter(|p| p.0 != [20, 20, 20, 255]).count()
    }

    /// The executed-point marker must draw AT the point (ring pixels near it),
    /// clip safely at edges, and leave a blank image otherwise untouched.
    #[test]
    fn executed_point_marker_draws_at_the_point_and_clips_at_edges() {
        let mut img = blank(200, 120);
        overlay::executed_point(&mut img, 60, 40);
        assert!(changed_pixels(&img) > 50, "marker must be visible");
        // A ring pixel at radius ~7 on the horizontal axis.
        assert_ne!(img.get_pixel(67, 40).0, [20, 20, 20, 255]);

        // Clipping: a marker at the corner must not panic and still draws.
        let mut corner = blank(40, 30);
        overlay::executed_point(&mut corner, 0, 0);
        assert!(changed_pixels(&corner) > 0);
    }

    /// Rulers tick every 100px on the top and left edges; a small image gets no
    /// ticks at all (nothing at or past its size).
    #[test]
    fn rulers_tick_every_100_pixels() {
        let mut img = blank(350, 250);
        overlay::rulers(&mut img);
        // Vertical ticks at x=100/200/300 on the top edge; horizontal at y=100/200.
        for x in [100u32, 200, 300] {
            assert_eq!(img.get_pixel(x, 0).0, [0, 0, 0, 255], "tick at x={x}");
        }
        for y in [100u32, 200] {
            assert_eq!(img.get_pixel(0, y).0, [0, 0, 0, 255], "tick at y={y}");
        }

        let mut small = blank(90, 60);
        overlay::rulers(&mut small);
        assert_eq!(
            changed_pixels(&small),
            0,
            "no tick fits an image under 100px"
        );
    }

    /// A badge paints its red disc/pill centered on the mark point.
    #[cfg(target_os = "macos")]
    #[test]
    fn badge_is_centered_on_the_mark_point() {
        let mut img = blank(120, 80);
        overlay::badge(&mut img, 60, 40, 7);
        assert_eq!(img.get_pixel(52, 40).0, [230, 40, 40, 255], "red pill body");
        assert!(changed_pixels(&img) > 80, "badge must be prominent");
    }

    #[test]
    fn parse_region_rejects_nonpositive_dimensions() {
        let req = json!({"region": {"x": 0, "y": 0, "w": 0, "h": 10}});
        assert!(parse_region(&req).is_err());
    }

    #[test]
    fn hello_reports_the_protocol_version_and_verbs() {
        let v = hello().unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["protocol_version"], json!(PROTOCOL_VERSION));
        assert!(v["compux_version"].is_string());
        assert!(v["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "screenshot"));
    }

    // idle_ms / wait_for_idle are OPERATIONAL (policy-support), never advertised as
    // model verbs — same posture as `probe`. Lock that so they aren't offered to a model.
    #[test]
    fn idle_verbs_are_not_advertised_model_actions() {
        let actions = hello().unwrap()["actions"].as_array().unwrap().clone();
        assert!(!actions.iter().any(|a| a == "idle_ms"));
        assert!(!actions.iter().any(|a| a == "wait_for_idle"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn idle_ms_reports_a_nonnegative_value() {
        let v = idle_ms().unwrap();
        assert_eq!(v["ok"], json!(true));
        assert!(v["idle_ms"].as_u64().is_some());
    }

    // idle_ms:0 means "idle for >= 0ms", which is always true, so the poll returns
    // immediately with idle:true — a deterministic check of the loop's success path.
    #[cfg(target_os = "macos")]
    #[test]
    fn wait_for_idle_zero_target_returns_immediately_idle() {
        let v = wait_for_idle(&json!({"idle_ms": 0, "timeout_ms": 500})).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["idle"], json!(true));
    }

    // A target longer than any machine uptime is unreachable, so the poll must time
    // out cleanly as idle:false rather than block — the step-aside path. The target
    // must exceed real idle even on a CI runner that has been idle for hours (an
    // hours-scale target is NOT safe — it can be satisfied immediately there).
    #[cfg(target_os = "macos")]
    #[test]
    fn wait_for_idle_times_out_when_target_unreachable() {
        let v = wait_for_idle(
            &json!({"idle_ms": 9_000_000_000_000u64, "timeout_ms": 50, "poll_ms": 10}),
        )
        .unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["idle"], json!(false));
    }

    // The dispatch table wires the operational verbs through `handle`.
    #[cfg(target_os = "macos")]
    #[test]
    fn handle_dispatches_idle_ms() {
        let v = handle(&json!({"action": "idle_ms"}));
        assert_eq!(v["ok"], json!(true));
        assert!(v["idle_ms"].as_u64().is_some());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn idle_detection_is_macos_only_off_macos() {
        assert!(idle_ms().is_err());
        assert!(wait_for_idle(&json!({})).is_err());
    }
}
