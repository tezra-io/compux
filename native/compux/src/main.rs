//! compux — the native computer-use (screen-capture + input-injection) sidecar.
//!
//! Reads one JSON request line from stdin, performs the GUI action, writes one
//! JSON response line to stdout. The wire contract is `Compux.Protocol`
//! (lib/compux/protocol.ex). The Elixir `Compux.PortDriver` owns this process
//! as a Port.
//!
//! Coordinate model (the #1 "clicks land offset" risk — read carefully):
//!   * A screenshot is the target display captured at PHYSICAL pixels, then
//!     downscaled to fit the sent budgets. The model sees that downscaled image
//!     and sends click coordinates in ITS pixel space.
//!   * Every reply that hands out coordinates therefore names the image they are
//!     in (`observation_id`), and every action that sends coordinates back names
//!     one. The transform is stored with the image and used as stored; nothing
//!     re-derives it from a rectangle the caller repeated. `mod geometry` owns
//!     the arithmetic and `mod observation` owns the table.
//!   * v1 drives ONE display (the configured index, default primary).
//!
//! Runtime behavior must be verified on a real machine with the macOS TCC grants
//! (Screen Recording and Accessibility). It never panics the request loop — every
//! action answers with `{"ok": true, ...}` or `{"ok": false, "error": "..."}`.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::io;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
#[cfg(target_os = "macos")]
use enigo::{Enigo, Mouse, Settings};
use held::{Axis, Button, Key};
use image::ImageEncoder as _;
use serde::Deserialize;
use serde_json::{json, Value};
#[cfg(target_os = "macos")]
use xcap::Monitor;

use gate::{Gate, Gated, Phase, SystemClock};
use geometry::{
    crop_rect, sent_scale, to_logical, to_sent, Geometry, Host, Measured, MonitorFacts, Region,
};
use held::Platform as _;
use listing::{in_view, insert_states, listed_nodes, published, ViewNode, Viewport};
use observation::{Kind, Observation, Observations, ViewHash};
use wire::Failure;

/// Capture mode (MILESTONE_32 §8.4a): the AXObserver/CFRunLoop event-push engine +
/// the serialized `Emitter`. Isolated from the request/response core here.
mod capture;

/// Accessibility: the AX FFI behind a per-node trait narrow enough that
/// everything above it — the element list, the references it hands out, the
/// revalidation before an action — is tested with no OS call.
mod ax;

/// The tree walk above the accessibility seam: its budgets, which nodes it lists,
/// children by page and the ancestor path, tested against scripted trees.
mod tree;

/// Names (protocol 12): the normalised roles, the surface kinds and which verbs a
/// control lists, as pure tables.
mod roles;

/// The verbs on a reference, the wait on the tree and the surfaces (protocol 12).
mod semantic;

/// A walk as the wire publishes it: entries, click points, references and what
/// each listed control sits inside.
mod listing;

/// Linux accessibility: the AT-SPI reader behind `ax::Ax`, and the session and bus
/// it reads through (M86 L1).
#[cfg(target_os = "linux")]
mod linux;

/// Held synthetic input: the press registry and the `Platform` seam every input
/// sequence posts through, so nothing this process presses can outlive the action.
mod held;

/// The protocol-10 frames as types: line parsing, the outbound builders, receipts.
mod wire;

/// Display geometry and the one coordinate transform, pure and platform-injected.
mod geometry;

/// The images and coordinate lists handed out, and the transform each was made
/// with — so a click is mapped by the geometry its picture was taken with.
mod observation;

/// The admission gate: generations, the `mutation_seq` high-water mark, the pause
/// barrier, and the cancellation checkpoints every paced sequence reads.
mod gate;

/// The control reader thread, which owns stdin so the sidecar listens while it acts.
mod control;

/// The window server: what windows exist, where they are, and what is in front of
/// what — behind a trait, so every rule a bound target lives by is a pure function.
mod window_server;

/// One window's frames: the `WindowFrames` seam, the after-action fence, the memory
/// budget, and the ScreenCaptureKit stream that really produces them.
mod window_frames;

/// The bound window: what it is, whether it is still there, and what may be done
/// inside it.
mod target;

/// The ownership indicator — the badge that shows the person their window is being
/// worked in, and the Pause and Stop on it that reach the gate directly.
mod indicator;

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
///
/// v6: CAPTURE MODE (MILESTONE_32 §8.4a) — the control actions `observe_start` /
/// `observe_stop` (excluded from `hello`'s model verbs) and an unsolicited
/// `{"type":"event"|"ack",…}` push wire. The push channel is wire-incompatible
/// with the strictly-positional request/response core, so the version bumps and
/// the handshake refuses an older pairing. See `mod capture`.
///
/// 0.9.0 stays on v6: the `browser.navigated` observation kind and the browser
/// context on `field.value` are ADDITIVE fields on that same push wire, and the
/// retired `sites` key of `observe_start` is accepted and ignored — no control
/// action changed, so neither side needs a new minimum.
///
/// v7 (M42 slice 2): the action wire becomes TAGGED and CORRELATED. Every line
/// carries a `type`; every request a `request_id` its response echoes, and, after
/// the handshake, the generations it belongs to. Requests and responses no longer
/// pair by ORDER, which retires the desync class where one late frame answered
/// every later question. A mutating request carries an increasing `mutation_seq`
/// and its response a `receipt` saying whether input was dispatched; `control` and
/// `control_ack` make Pause a confirmed barrier rather than a hope. The caller's
/// remaining budget rides as `deadline_ms`, NOT `timeout_ms`, which two actions
/// have used as an argument of their own since v2. Computer history keeps its
/// `ack` and `event` families byte for byte; only this integer inside the ack
/// moves. See `mod wire`.
///
/// v8 (M42 slice 3): every reply that hands out coordinates names the image they
/// are in, and every action that sends coordinates back names one. A pointer action
/// or `inspect` carries `observation_id` and no `region`; `screenshot`, `elements`
/// and `wait_for_change` keep `region` and may name the image it was read in. The
/// transform is stored with the image and used as stored, so nothing re-derives it
/// from a rectangle a caller repeated — and it is built from a ratio MEASURED on
/// the frame that was really captured rather than assumed from the display mode.
/// See `mod observation` and `mod geometry`.
///
/// v9 (M42 slice 4): controls the caller can NAME. Every element an `elements`
/// reply lists, and every mark a `marks: true` screenshot badges, carries an
/// `element_ref` scoped to its observation, and the observation RETAINS the native
/// reference behind it (with the owning pid and that process's start time, so pid
/// reuse cannot be mistaken for the same application). Two actions address a
/// control rather than a point — `press` and `set_value`, offered only where the
/// control itself advertises support — and a pointer action may carry an
/// `element_ref` instead of `x`/`y`, in which case the bounds are read again at
/// the moment it acts. Both addressing forms on one request is
/// `addressing_conflict`. Receipts gained `input_method: "ax"`, the `verified`
/// effect a read-back earns, and `foreground_changed`. `element_ref` was a
/// reserved field this sidecar refused outright until now. See `mod ax`.
///
/// v10 (M42 slice 6): an action that dispatches input says what evidence it is to
/// bring back, and `check` REPLACES `screenshot_after` — deleted, not kept beside
/// it, so a caller that still sends the old field is refused rather than run with
/// no check at all. `image` answers the view the action was aimed in (the crop of
/// the observation it named, or the display when it named none), waited on until
/// two consecutive samples agree and encoded from the sample that proved it;
/// `semantic` re-reads the control an `element_ref` named and answers
/// `element_after` with no capture; `none` is the receipt alone. The receipt says
/// which evidence it carries (`check: {kind, settle, changed}`) and what each
/// phase cost (`timings_ms` gained `encode`). `wait_for_change` returns the frame
/// whose hash differed rather than a third one taken after the fact.
/// v11 (M42 slice 5): ONE WINDOW, bound. `select_target` takes a `window_id` from a
/// `windows` listing and binds that window — its pid, that process's start time, its
/// `CGWindowID`, and, where exactly one of the application's `AXWindow`s matches it,
/// that window too — answering a `target_id` and a first observation of the window
/// alone. An action carrying `target_id` is answered from that window's own stream
/// of frames and in that window's own coordinates, so a window another one covers is
/// still seen; a pixel action on it is refused `target_obstructed` rather than
/// landing on whatever is in front, and nothing is ever raised or activated to make
/// room. While a target is held the helper shows an ownership badge, and its Pause
/// and Stop reach the gate directly and are reported as `session_event`s.
/// `target_id` was a reserved field this sidecar refused outright until now. An
/// action WITHOUT one behaves byte for byte as it did at protocol 10: the
/// display-level path is untouched, and it is also how the whole desktop stays
/// reachable — there is no "desktop" target, there is the absence of one.
///
/// v12 (M86 P2): controls before pixels. An `elements` listing names each control
/// by a normalised `role` beside its `native_role`, lists the `verbs` it has a
/// route for in place of `actions` and `settable`, and reports its states; it can
/// be scoped (`window`, `app`, `focused`, `menu`, `dialog`), narrowed to a find
/// (`query`), cut to a `skeleton`, or drilled into (`root`). New verbs act on a
/// reference through the accessibility API alone — `focus`, `select`, `toggle`,
/// `expand`, `collapse`, `scroll_to`, and `type` with an `element_ref` — each
/// reading its state first and refusing rather than clicking where it has no route.
/// `wait_for` and a check's `until` wait on the tree and never send input; every
/// receipt of input that went out names the dialogs, alerts, menus and popups open
/// after it. See `PROTOCOL.md`, `mod roles` and `mod semantic`.
const PROTOCOL_VERSION: u32 = 12;

/// The capture-stall self-reap (EX_TEMPFAIL), and NOTHING else.
///
/// Fermix reads this exact status as a clean, retryable capture wedge: it feeds
/// `CaptureHealth`, `Session.note_capture_wedge/1` and the realtime feed's
/// `wedge?/1`. Any other failure that exits 75 is therefore reported to the
/// operator as a wedged capture backend, which is why `disclaim` owns its own
/// codes and none of them is this one.
const EXIT_CAPTURE_STALLED: i32 = 75;

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
///
/// EXIT CODES: this module owns 70-74, 76 and 77, one per distinguishable failure.
/// It must never claim 75 — that is the capture-stall self-reap in `main`, which
/// Fermix reads as a specific, clean, retryable condition (it feeds `CaptureHealth`
/// and the realtime feed's wedge check). `posix_spawnattr_setflags` used to exit 75
/// and was therefore reported to the operator as a wedged capture backend.
#[cfg(target_os = "macos")]
mod disclaim {
    use std::env;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::ptr;

    type SetDisclaimFn =
        unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;

    /// One code per distinguishable failure, so an operator's exit status names
    /// which step broke. Named rather than inline so the whole set can be gated:
    /// see `no_disclaim_exit_code_collides_with_the_capture_stall`.
    const NO_SETDISCLAIM_SYMBOL: i32 = 70;
    const SETDISCLAIM_REFUSED: i32 = 71;
    const REEXEC_FAILED: i32 = 72;
    const NO_CURRENT_EXE: i32 = 73;
    const ATTR_INIT_FAILED: i32 = 74;
    const NUL_IN_ARGV: i32 = 76;
    /// 77 because 70 to 76 were all taken and 75 belongs to the capture stall.
    const SETFLAGS_FAILED: i32 = 77;

    /// Every code this module can exit with. The gate loops over this, so a code
    /// added later either joins it or fails the test.
    #[cfg(test)]
    pub const EXIT_CODES: [i32; 7] = [
        NO_SETDISCLAIM_SYMBOL,
        SETDISCLAIM_REFUSED,
        REEXEC_FAILED,
        NO_CURRENT_EXE,
        ATTR_INIT_FAILED,
        NUL_IN_ARGV,
        SETFLAGS_FAILED,
    ];

    pub fn become_responsible() {
        if env::var_os("COMPUX_DISCLAIMED").is_some() {
            return; // already the re-exec'd, disclaimed image
        }

        let set_disclaim = resolve_set_disclaim();
        let exe = env::current_exe()
            .unwrap_or_else(|e| fatal(NO_CURRENT_EXE, &format!("current_exe: {e}")));
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
                fatal(ATTR_INIT_FAILED, "posix_spawnattr_init failed");
            }
            if set_disclaim(&mut attr, 1) != 0 {
                fatal(
                    SETDISCLAIM_REFUSED,
                    "responsibility_spawnattrs_setdisclaim returned nonzero",
                );
            }
            if libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_SETEXEC as libc::c_short)
                != 0
            {
                fatal(SETFLAGS_FAILED, "posix_spawnattr_setflags failed");
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
            fatal(REEXEC_FAILED, "POSIX_SPAWN_SETEXEC re-exec failed");
        }
    }

    fn resolve_set_disclaim() -> SetDisclaimFn {
        // dlsym's RTLD_DEFAULT pseudo-handle on macOS is (void *)-2.
        let rtld_default = (-2isize) as *mut libc::c_void;
        let name = cstr(b"responsibility_spawnattrs_setdisclaim");
        let sym = unsafe { libc::dlsym(rtld_default, name.as_ptr()) };
        if sym.is_null() {
            fatal(
                NO_SETDISCLAIM_SYMBOL,
                "responsibility_spawnattrs_setdisclaim unavailable",
            );
        }
        unsafe { std::mem::transmute::<*mut libc::c_void, SetDisclaimFn>(sym) }
    }

    fn cstr(bytes: &[u8]) -> CString {
        CString::new(bytes).unwrap_or_else(|_| fatal(NUL_IN_ARGV, "unexpected NUL in argv/env"))
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

    // Linux: where this process's display is, decided once and carried as a value,
    // never written into the environment: its own, or for a helper started with
    // none, the session's from the user manager, with its X authority.
    #[cfg(target_os = "linux")]
    let endpoint = Endpoint::of_session();
    #[cfg(not(target_os = "linux"))]
    let endpoint = Endpoint {};

    // One serialized writer shared by the control reader, the action worker and
    // the capture observer thread (§8.4a): every response, acknowledgement, ack
    // and event frame goes through `emitter`, so two writers can never split each
    // other's line.
    let emitter = capture::Emitter::new();

    // One gate for the process. The reader flips it; the worker is admitted
    // through it. Both hold the same handle, which is what makes a pause a
    // barrier rather than a request to stop soon.
    let gate = Gate::new(boot_generation(), Arc::new(SystemClock::new()));

    // Capacity one: a second request while one is in flight is refused `busy` by
    // the reader, never queued behind work whose screen has moved on.
    let (to_worker, jobs) = mpsc::sync_channel::<control::Job>(1);

    // The reader owns stdin on its own thread, so a control is heard and answered
    // while the worker is inside a wait, a poll or an accessibility settle.
    {
        let gate = gate.clone();
        let emitter = emitter.clone();
        thread::spawn(move || control::run(io::stdin().lock(), &gate, &emitter, to_worker));
    }

    // The action worker runs on the MAIN thread: input stays where it runs today,
    // and AppKit will want this thread in a later slice.
    run_worker(jobs, &gate, &emitter, endpoint);

    // Every sender is gone, so stdin reached EOF: the owning Port closed. Tear down any live
    // capture observer, then switch OFF any accessibility attribute this process
    // switched on (B4), so one enumeration never leaves the user's browser in an
    // altered AX mode. Best-effort: a SIGKILLed sidecar skips this, and the next
    // activation is idempotent.
    capture::stop();
    #[cfg(target_os = "macos")]
    ax::clear_activations();
}

/// A fresh identity per boot, so a frame minted by an earlier sidecar can never be
/// taken for one of ours. The pid alone would not do it — pids are reused.
fn boot_generation() -> String {
    format!("boot-{}-{}", std::process::id(), capture::now_ms())
}

/// Where this process's display is. Linux decides it once, at start, and carries
/// it as a value (`linux::display::Endpoint`); a macOS helper's is always the
/// session's own, with nothing to carry.
#[cfg(target_os = "linux")]
use linux::display::Endpoint;

#[cfg(not(target_os = "linux"))]
#[derive(Clone, Debug, Default)]
struct Endpoint {}

/// Which way pixels reach this session. Linux decides it once, at start
/// (`linux::pixels`): X11 through x11rb, GNOME Wayland through the portal, or
/// refused with the reason. macOS has one way, enigo and xcap, always.
#[cfg(target_os = "linux")]
use linux::pixels::Pixels;

#[cfg(not(target_os = "linux"))]
#[derive(Default)]
struct Pixels;

#[cfg(not(target_os = "linux"))]
impl Pixels {
    fn serves(&self, _action: &str) -> bool {
        true
    }

    fn refusal(&self, action: &str) -> String {
        format!("{action} has no pixel route here")
    }

    fn release(&mut self) {}

    fn portal(&self) -> bool {
        false
    }
}

/// Everything the action worker owns that is neither on the wire nor in the gate:
/// the images handed out and the transform each was made with, the pixels per
/// point measured on each display, and the accessibility platform every element
/// reference is retained and released through. One thread reads and writes them,
/// one request at a time, so none is locked — and nothing else may take a
/// reference to them, which is the invariant that keeps it that way.
struct Worker {
    observations: Observations,
    measured: Measured,
    ax: Rc<dyn ax::Ax>,
    frames: Rc<dyn Frames>,
    /// The window server, shared with the indicator's watch thread — which is why
    /// this one is an `Arc` where the others are `Rc`.
    windows: Arc<dyn window_server::Windows>,
    /// What starts the ownership badge. Read for `hello` (is it in the bundle at
    /// all) and used at every selection.
    launcher: Arc<dyn indicator::Launcher>,
    /// What the OS says about the displays. A seam so no unit test ever reaches a
    /// real screen; see [`Displays`].
    displays: Rc<dyn Displays>,
    /// The ONE bound window, when there is one. Owned here, like the observation
    /// table, so its stream and its badge are stopped by dropping it — a release, a
    /// reselect and the worker ending all go through the same `Drop`.
    target: Option<target::Target>,
    /// How many targets this boot has bound, which is what `target_generation`
    /// counts.
    target_generation: u64,
    /// The one serialized writer, for the `session_event`s the ownership badge
    /// produces. Held here because a badge is restarted from inside an action, and
    /// a restart needs everything the first start needed.
    emitter: capture::Emitter,
    /// Whether `ax` is an accessibility API at all. A build for a system with none
    /// answers `accessibility_unavailable` to every action that reads or acts on
    /// controls, rather than a refusal about the control.
    accessibility: bool,
    /// An action's semantic `until`, read BEFORE its input and taken by its check:
    /// whether the condition already held.
    until_held: Option<bool>,
    /// When the request in hand must have answered, on the gate's clock, where it
    /// named a `deadline_ms`: a wait never outlives it.
    cutoff_ms: Option<u64>,
    /// Where this process's display is: what the handshake and the probe report.
    endpoint: Endpoint,
    /// How pixels reach it: what the handshake offers, what a pixel action is
    /// refused by, and on Linux the input platform kept for the session.
    pixels: Pixels,
}

/// The accessibility reader of this process, on its display.
#[cfg(target_os = "linux")]
fn reader(endpoint: &Endpoint) -> ax::Real {
    ax::Real::on(endpoint.clone())
}

#[cfg(not(target_os = "linux"))]
fn reader(_endpoint: &Endpoint) -> ax::Real {
    ax::Real::new()
}

/// How pixels reach this session, decided once at start.
#[cfg(target_os = "linux")]
fn pixels_of(endpoint: &Endpoint) -> Pixels {
    Pixels::of(endpoint)
}

#[cfg(not(target_os = "linux"))]
fn pixels_of(_endpoint: &Endpoint) -> Pixels {
    Pixels
}

/// The serial action worker. One request at a time, to completion, on this thread.
fn run_worker(
    jobs: mpsc::Receiver<control::Job>,
    gate: &Gate,
    emitter: &capture::Emitter,
    endpoint: Endpoint,
) {
    let pixels = pixels_of(&endpoint);
    let mut worker = Worker {
        observations: Observations::new(&gate.envelope().sidecar_generation, gate.clock()),
        measured: Measured::new(),
        ax: Rc::new(reader(&endpoint)),
        frames: Rc::new(Screen::of(&pixels)),
        windows: Arc::new(window_server::Real::new()),
        launcher: Arc::new(indicator::Bundled::new()),
        displays: Rc::new(RealDisplays::of(&pixels)),
        target: None,
        target_generation: 0,
        emitter: emitter.clone(),
        accessibility: accessibility_supported(),
        until_held: None,
        cutoff_ms: None,
        endpoint,
        pixels,
    };

    for job in jobs {
        let control::Job::Action(request) = job;

        // Letting go of the seat lets go of what the caller could still address
        // with it. The table — and the native element references it retains — is
        // this thread's alone to touch, so the control flips a flag and the answer
        // is read here, which is the first moment anything may act on it.
        if gate.take_release() {
            worker.observations.clear();
            // And of the window it was working in. Dropping the target is what
            // stops its stream and its badge, so letting go of the seat lets go of
            // the screen the person can see it on.
            worker.target = None;
            // And of the portal's remote-desktop session, which the shell shows.
            worker.pixels.release();
        }

        let frame = serve(&request, gate, emitter, &mut worker);

        // One JSON line per reply. A write failure means the parent is gone.
        if emitter.emit_line(&frame.to_string()).is_err() {
            break;
        }

        // A capture wedged the OS backend and leaked a stuck worker we can't
        // reclaim — the reply is now flushed, so exit and let the parent respawn
        // a clean sidecar (EX_TEMPFAIL). Do this AFTER flush so the caller got
        // its `capture_stalled` answer first.
        if CAPTURE_WEDGED.load(Ordering::SeqCst) {
            capture::stop();
            #[cfg(target_os = "macos")]
            ax::clear_activations();
            std::process::exit(EXIT_CAPTURE_STALLED);
        }
    }
}

/// Admit, run, and answer one request.
fn serve(
    request: &wire::Request,
    gate: &Gate,
    emitter: &capture::Emitter,
    worker: &mut Worker,
) -> Value {
    if let Err(refusal) = gate.admit(request) {
        return wire::error_response(
            gate.envelope(),
            &request.request_id,
            refusal.code(),
            Some(refusal.detail().to_string()),
            receipt(request, &gate::Dispatched::default(), None),
        );
    }

    let outcome = handle(request, gate, emitter, worker);
    let done = gate.finish();
    let surfaces = surfaces_after(request, &done, worker);
    reply(request, gate, outcome, done, surfaces)
}

/// Turn what the action did into the frame that reports it.
fn reply(
    request: &wire::Request,
    gate: &Gate,
    outcome: Result<Value, Failure>,
    done: gate::Dispatched,
    surfaces: Option<Vec<wire::Surface>>,
) -> Value {
    // The capture verbs answer in the computer-history `ack` family, byte for byte
    // as they did at protocol 6. Their client reads that shape and nothing else.
    if wire::answers_with_ack(&request.action) {
        return match outcome {
            Ok(ack) => ack,
            Err(failure) => observe_ack(&request.action, false, Some(failure.code)),
        };
    }

    match outcome {
        Ok(payload) => {
            // The image this action handed back, when it handed one back — read
            // off the payload rather than off the request: what was produced, not
            // what was asked for.
            let after = payload
                .get("observation_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let receipt = receipt(request, &done, after)
                .map(|receipt| receipt.reported(done.state, surfaces));
            wire::response(gate.envelope(), &request.request_id, payload, receipt)
        }

        Err(failure) => {
            // `input_complete`, not `false`: the check is taken INSIDE the action,
            // so a click whose every event landed and whose own check then failed
            // dispatched `sent`. Calling that `partial` would have Fermix report
            // `unknown` where the truth is performed-unverified. The gate recorded
            // no check either, so the receipt claims no evidence: input that went
            // out and evidence that did not are two facts, both reported.
            let receipt =
                receipt(request, &done, None).map(|receipt| receipt.reported(done.state, surfaces));
            // Every refusal that reaches here was cancelled (a pause sets both
            // flags), so a refusal code only ever lands in `detail` and never
            // stands in for the action's own error. A detail that merely repeats
            // the code is dropped: it would tell an operator nothing.
            let (code, detail) = if done.cancelled {
                (
                    "cancelled".to_string(),
                    Some(failure.code).filter(|code| code != "cancelled"),
                )
            } else {
                (failure.code, failure.detail)
            };
            wire::error_response(gate.envelope(), &request.request_id, &code, detail, receipt)
        }
    }
}

/// A receipt rides every action that is not read-only, refusals included: "no
/// input was sent" is exactly the fact a caller needs before it retries.
///
/// Everything it reports comes from the gate, which is the only path from an
/// action to the screen: whether input was posted, how long each phase took,
/// which method carried it, what its check brought back, whether the action could
/// verify its own effect, and whether the foreground moved. None of it is inferred
/// from the action's name.
fn receipt(
    request: &wire::Request,
    done: &gate::Dispatched,
    after: Option<String>,
) -> Option<wire::Receipt> {
    if wire::carries_mutation_seq(&request.action) {
        Some(
            wire::Receipt::derive(done.posted, done.input_complete, done.check, done.timings)
                .addressing(request.observation_id.clone(), after)
                .noted(done.input_method, done.effect, done.foreground_changed),
        )
    } else {
        None
    }
}

fn handle(
    request: &wire::Request,
    gate: &Gate,
    emitter: &capture::Emitter,
    worker: &mut Worker,
) -> Result<Value, Failure> {
    before_dispatch(request, gate, worker)?;
    let req = &request.body;

    match request.action.as_str() {
        "hello" => hello(&worker.launcher, &worker.pixels),
        "probe" => probe(&worker.endpoint, &worker.pixels),
        "idle_ms" => idle_ms(worker),
        "wait_for_idle" => wait_for_idle(req, gate),
        "request_permissions" => request_permissions(req, &worker.pixels),
        // Capture control verbs (MILESTONE_32 §8.4a) — NOT model actions, excluded
        // from `hello`. They return a type-discriminated `ack` frame (even on
        // refusal), never the generic `{ok:false,error}` shape, so the consumer's
        // handshake reads `ok`/`protocol_version` rather than degrading on a
        // missing frame type.
        "observe_start" => Ok(observe_start(req, emitter)),
        "observe_stop" => Ok(observe_stop()),
        "screenshot" => screenshot(request, gate, worker),
        "mouse_move" => mouse_move(request, gate, worker),
        "left_click" => click(request, gate, worker, Button::Left, 1),
        "right_click" => click(request, gate, worker, Button::Right, 1),
        "double_click" => click(request, gate, worker, Button::Left, 2),
        "left_click_drag" => drag(request, gate, worker),
        "scroll" => scroll(request, gate, worker),
        "type" if request.element_ref.is_some() => {
            let verb = semantic::Verb::from_request("type", req)?;
            verb_on(request, gate, worker, verb)
        }
        "type" => type_text(request, gate, worker),
        "key" => key_chord(request, gate, worker),
        "wait" => wait(req, gate),
        "inspect" => inspect(request, gate, worker),
        "wait_for_change" => wait_for_change(request, gate, worker),
        "paste" => paste(request, gate, worker),
        "elements" => elements(request, gate, worker),
        "windows" => windows(req, gate, worker),
        "press" => press(request, gate, worker),
        "set_value" => set_value(request, gate, worker),
        "focus" | "select" | "toggle" | "expand" | "collapse" | "scroll_to" => {
            let verb = semantic::Verb::from_request(&request.action, req)?;
            verb_on(request, gate, worker, verb)
        }
        "wait_for" => wait_for(request, gate, worker),
        "select_target" => select_target(request, gate, emitter, worker),
        "release_target" => release_target(worker),
        other => Err(Failure::new(
            "unknown_action",
            format!("{other} is not an action this build has"),
        )),
    }
}

/// What every request settles before its action runs: a build with no
/// accessibility API refuses one that reads controls, the request's deadline
/// becomes the cutoff its waits are held to, and an action that waits on the tree
/// reads its condition once before any input (§5.2).
fn before_dispatch(
    request: &wire::Request,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<(), Failure> {
    if !worker.accessibility && reads_controls(request) {
        return Err(Failure::new(
            "accessibility_unavailable",
            "this build has no accessibility API, so it cannot read or act on controls by name; \
             work in the image instead"
                .to_string(),
        ));
    }
    if let Some(action) = pixel_need(request) {
        if !worker.pixels.serves(action) {
            return Err(Failure::new(
                "pixels_unavailable",
                worker.pixels.refusal(action),
            ));
        }
    }
    if POINT_ACTIONS.contains(&request.action.as_str()) && !points_readable() {
        return Err(Failure::new(
            "unknown_action",
            "this build cannot read the control under a point: this session does not place \
             every control on the screen; list them with `elements` instead"
                .to_string(),
        ));
    }
    let deadline_ms = request.body.get("deadline_ms").and_then(Value::as_u64);
    worker.cutoff_ms = deadline_ms.map(|left| {
        gate.now_ms()
            .saturating_add(left.saturating_sub(DEADLINE_MARGIN_MS))
    });
    worker.until_held = None;
    if let (Some(until), false) = (&request.until, request.action == "wait_for") {
        worker.until_held = Some(held_before(request, until, gate, worker)?);
    }
    Ok(())
}

/// The pixel action this request needs, if it captures the screen or posts input
/// to it: a pixel verb itself, `type` at the focus (by reference it acts through
/// the accessibility API), and a `screenshot` for any action that asks for an
/// image check.
fn pixel_need(request: &wire::Request) -> Option<&'static str> {
    let action = request.action.as_str();
    if let Some(pixel) = PIXEL_ACTIONS.iter().find(|pixel| **pixel == action) {
        return Some(pixel);
    }
    if action == "type" && request.element_ref.is_none() {
        return Some("type");
    }
    (request.check == wire::Check::Image).then_some("screenshot")
}

/// What a reply needs to reach the caller before its deadline: a wait gives this
/// much back rather than answer late.
const DEADLINE_MARGIN_MS: u64 = 250;

/// Does this request read or act on controls through the accessibility API? A
/// check that waits on the tree does, whatever the action.
fn reads_controls(request: &wire::Request) -> bool {
    ACCESSIBILITY_ACTIONS.contains(&request.action.as_str())
        || (request.action == "type" && request.element_ref.is_some())
        || request.until.is_some()
}

// --- addressing: which image a request's coordinates are in ------------------

/// The image an action's coordinates were read in, taken out of the table.
///
/// The parse layer has already guaranteed there is an id, so what is left is the
/// two ways a stored transform can go wrong, and they are checked in this order on
/// purpose: what is pure first, what needs the OS second.
///
/// Copied rather than borrowed, so the action that follows can go on to mint its
/// own check image into the same table.
fn observed(request: &wire::Request, worker: &Worker) -> Result<Observation, Failure> {
    let id = request
        .observation_id
        .as_deref()
        .ok_or_else(|| Failure::from("observation_required"))?;

    let observation = worker.observations.resolve(id).map_err(refused)?.clone();

    Ok(observation)
}

/// A point the model read off that image, mapped to the global logical point enigo
/// takes. A point outside the image is refused and never clamped onto an edge: it
/// was read wrong, probably off a different image, and clamping turns that into a
/// click on whatever happens to sit at the boundary.
fn point_in(observation: &Observation, x: f64, y: f64) -> Result<(i32, i32), Failure> {
    observation.contains(x, y).map_err(refused)?;

    Ok(to_logical(&observation.geometry, &observation.region, x, y))
}

/// The display that image was made on, proven to still be the display in front of
/// us. Reads what the OS says — bounds, origin, mode scale, id — which needs no
/// capture, and refuses on any difference: a click computed from a geometry that
/// has since moved lands somewhere nobody chose.
fn same_display(
    observation: &Observation,
    req: &Value,
    worker: &Worker,
) -> Result<Display, Failure> {
    let display = target_display(req, worker)?;

    if observation.matches_display(display.id, &display.facts) {
        Ok(display)
    } else {
        Err(refused(observation::Refusal::Stale))
    }
}

/// Every one of these is `dispatch: not_sent` — nothing was done — and each says
/// what to do next in its own words.
fn refused(refusal: observation::Refusal) -> Failure {
    Failure::new(refusal.code(), refusal.detail())
}

// --- addressing: which CONTROL a request names (protocol 9) ------------------

/// What an action is allowed to do to a control, and therefore what has to still
/// be true of it. Named rather than a boolean because the two refusals a caller
/// acts on differ: one says press it another way, the other says it is not a
/// field you can set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Capability {
    Press,
    SetValue,
}

impl Capability {
    fn detail(self) -> &'static str {
        match self {
            Capability::Press => {
                "this control does not list press among its own actions, so it \
                                  cannot be pressed by name; click it by element_ref or by point \
                                  instead"
            }
            Capability::SetValue => {
                "this control does not report its value as settable, so it \
                                     cannot be set by name; type or paste into it instead"
            }
        }
    }
}

/// The control a request named, out of the observation that listed it — the
/// native reference included, which is why this is the one path to an
/// accessibility action.
///
/// Copied rather than borrowed for the same reason `observed` copies: the action
/// that follows goes on to mint its own check image into the same table. The
/// references ride an `Arc`, so a copy shares them and releases nothing.
fn element_in<'a>(
    observation: &'a Observation,
    request: &wire::Request,
) -> Result<&'a ax::Entry, Failure> {
    let reference = request
        .element_ref
        .as_deref()
        .ok_or_else(|| Failure::from("element_required"))?;

    let Some(elements) = observation.elements.as_deref() else {
        return Err(Failure::new(
            "stale_element",
            "that reply listed no controls, so it holds no references; take `elements` (or a \
             screenshot with marks) and use a reference from it"
                .to_string(),
        ));
    };

    elements.get(reference).ok_or_else(|| {
        Failure::new(
            "stale_element",
            format!(
                "{reference} is not one of the {} controls that reply listed; take `elements` \
                 again and use a reference from the new list",
                elements.len()
            ),
        )
    })
}

/// Is this still the control the caller was shown, and will it act at all?
///
/// In this order on purpose, because each answer sends the caller somewhere
/// different: the application first (a reference into a process that has quit —
/// or whose pid another process now has — names nothing), then what the control
/// says it is, then whether it will take input. Every refusal here is
/// `dispatch: not_sent`, checked before anything is dispatched.
///
/// One read of the control answers all of it, and the facts it read are handed
/// back, so the action that follows asks the application nothing twice.
fn present(
    ax: &Rc<dyn ax::Ax>,
    observation: &Observation,
    entry: &ax::Entry,
) -> Result<ax::Facts, Failure> {
    let facts = answering(ax, observation, entry)?;
    if facts.enabled {
        Ok(facts)
    } else {
        Err(Failure::new(
            "element_disabled",
            "that control is disabled, so it will not act; do not retry it — find what enables \
             it first"
                .to_string(),
        ))
    }
}

/// Is this still the control the caller was shown: the same process, and the
/// same role? What [`present`] checks before it asks whether the control will act,
/// and all a drill asks of the control it reads inside.
fn answering(
    ax: &Rc<dyn ax::Ax>,
    observation: &Observation,
    entry: &ax::Entry,
) -> Result<ax::Facts, Failure> {
    let elements = observation
        .elements
        .as_deref()
        .ok_or_else(|| Failure::from("stale_element"))?;

    if ax.process_started_at(elements.pid) != Some(elements.started_at) {
        return Err(Failure::new(
            "stale_element",
            "the application that listed this control is not the one running now; take \
             `elements` again and use a reference from the new list"
                .to_string(),
        ));
    }

    let facts = ax.identify(entry.element.handle()).map_err(unidentified)?;
    match facts.role.clone() {
        Some(role) if role != entry.role => Err(Failure::new(
            "stale_element",
            format!(
                "that reference now answers a {role} where it listed a {}, so it is not the \
                 control you were shown; take `elements` again",
                entry.role
            ),
        )),
        Some(_) => Ok(facts),
        None => Err(no_longer_there()),
    }
}

/// Why a reference could not be found again, in the caller's words. Each says
/// nothing was sent, because nothing was.
fn unidentified(identity: ax::Identity) -> Failure {
    match identity {
        ax::Identity::Unread(ax::Unread::NoAnswer) => Failure::new(
            "stale_element",
            "that control did not answer within the accessibility timeout, so it could not \
             be checked and nothing was sent; take `elements` again"
                .to_string(),
        ),
        ax::Identity::Unread(_gone_or_refused) => no_longer_there(),
        ax::Identity::Ambiguous => Failure::new(
            "ambiguous_element",
            "the list changed under that reference: the control moved and another of its \
             role took its place, so either could be the one you were shown and nothing was \
             sent; take `elements` again"
                .to_string(),
        ),
        ax::Identity::Unknown => Failure::new(
            "identity_unknown",
            "that control stopped answering while it was being checked, three reads in a row, \
             so nothing was sent; take `elements` again"
                .to_string(),
        ),
    }
}

/// A reference that answers nothing it could be checked by.
fn no_longer_there() -> Failure {
    Failure::new(
        "stale_element",
        "that control is no longer there; take `elements` again and use a reference from the \
         new list"
            .to_string(),
    )
}

/// Everything [`present`] checks, plus the one capability this action needs — read
/// from the control ITSELF, never inferred from what its role suggests it ought to
/// support. A control that cannot do it is refused and never quietly clicked
/// instead: which of the two to send is the caller's decision.
fn revalidate(
    ax: &Rc<dyn ax::Ax>,
    observation: &Observation,
    entry: &ax::Entry,
    want: Capability,
) -> Result<ax::Facts, Failure> {
    let facts = present(ax, observation, entry)?;

    let capable = match want {
        Capability::Press => facts.actions.iter().any(|name| name == ax::PRESS),
        Capability::SetValue => ax.settable(entry.element.handle()),
    };

    if capable {
        Ok(facts)
    } else {
        Err(Failure::new(
            "ax_action_unsupported",
            want.detail().to_string(),
        ))
    }
}

/// Did this accessibility message leave the process?
///
/// A success did. A refusal did only when the platform says so — a timeout is the
/// one refusal that arrives AFTER the message went out, and every other one (the
/// accessibility API disabled, the element gone, a platform with no such API at
/// all) means nothing was sent. A receipt that read `sent` for those would tell a
/// caller its press may have landed when it provably did not.
fn went_out(outcome: &Result<(), ax::Refusal>) -> bool {
    match outcome {
        Ok(()) => true,
        Err(refusal) => refusal.sent(),
    }
}

/// A gate refusal as the action's own failure: nothing was dispatched and the
/// code says which barrier stopped it.
fn barred(refusal: gate::Refusal) -> Failure {
    Failure::new(refusal.code(), refusal.detail().to_string())
}

/// An accessibility call that failed after it went out.
///
/// Two codes, and the split is the whole point. A timeout means the message
/// landed somewhere and the answer never came, so nothing may repeat it — its
/// receipt says `dispatch: sent, effect: unknown`. Anything else is the
/// application refusing a message it did receive. Neither is `stale_element`,
/// which promises nothing was sent and is only ever minted by the revalidation
/// before the call. A reader that stopped before sending, for a reason the
/// contract names, keeps that reason's code.
fn ax_failure(refusal: ax::Refusal) -> Failure {
    let code = match (refusal.code(), refusal.sent()) {
        (Some(code), _) => code,
        (None, true) => "ax_timed_out",
        (None, false) => "ax_action_failed",
    };
    Failure::new(code, refusal.detail())
}

// --- capture control (MILESTONE_32 §8.4a, NOT model actions) -----------------

/// Start the capture observer and reply with the `observe_start` ack. On a refusal
/// (no Accessibility grant, already running, non-macOS) the ack carries `ok:false`
/// plus a diagnostic `error` (which the consumer ignores). The consumer degrades to
/// observe_start_refused on the `ok:false`, so it MUST be an `ack`, never `err()`.
fn observe_start(req: &Value, emitter: &capture::Emitter) -> Value {
    match capture::start(req, emitter.clone()) {
        Ok(()) => observe_ack("observe_start", true, None),
        Err(reason) => observe_ack("observe_start", false, Some(reason)),
    }
}

/// Tear down the observer and ack. `observe_stop` is best-effort and always `ok`.
fn observe_stop() -> Value {
    capture::stop();
    observe_ack("observe_stop", true, None)
}

fn observe_ack(action: &str, ok: bool, error: Option<String>) -> Value {
    let mut ack = json!({
        "type": "ack",
        "action": action,
        "ok": ok,
        "protocol_version": PROTOCOL_VERSION,
    });
    if let Some(error) = error {
        ack["error"] = json!(error);
    }
    ack
}

// --- hello (version handshake, NOT a model action) --------------------------

/// Identity + wire-version handshake performed once by `Compux.start/1`. Lets the
/// consumer refuse a sidecar whose `protocol_version` its compiled-in encoder does
/// not speak (the two-pin drift guard). `compux_version` is diagnostic; `actions`
/// is the model-facing verb set (probe/hello are operational, excluded), and it
/// lists only what THIS build can do (F1): the engine builds the model's verbs
/// from it, and a verb whose every use is a refusal is a turn wasted on it.
fn hello(launcher: &Arc<dyn indicator::Launcher>, pixels: &Pixels) -> Result<Value, Failure> {
    Ok(json!({
        "ok": true,
        "protocol_version": PROTOCOL_VERSION,
        "compux_version": env!("CARGO_PKG_VERSION"),
        "actions": advertised_actions(pixels),
        // Listed only because this build really has them: the input methods an
        // action can carry, the three controls the gate implements, and the
        // bounds of the observation table — a caller mirrors those numbers to know
        // which ids it may still address before it asks.
        "capabilities": {
            "input_methods": input_methods(pixels),
            "controls": ["pause", "resume", "release"],
            "observations": {
                "max": observation::MAX_OBSERVATIONS,
                "ttl_ms": observation::TTL_MS,
            },
            // What a caller needs to decide whether to offer bound windows at all,
            // BEFORE it selects anything. `indicator` is whether the badge's
            // executable is beside this one: a target cannot be worked in without
            // it, and finding that out by selecting a window and being refused
            // would be finding it out too late.
            "targets": targets_supported(),
            "capture_methods": capture_methods(pixels),
            "indicator": indicator_presence(launcher),
        },
    }))
}

/// Every model-facing verb, in the order the handshake has always listed them.
const ACTIONS: [&str; 26] = [
    "screenshot",
    "left_click",
    "right_click",
    "double_click",
    "mouse_move",
    "left_click_drag",
    "scroll",
    "type",
    "key",
    "wait",
    "inspect",
    "wait_for_change",
    "paste",
    "elements",
    "windows",
    "press",
    "set_value",
    "focus",
    "select",
    "toggle",
    "expand",
    "collapse",
    "scroll_to",
    "wait_for",
    "select_target",
    "release_target",
];

/// The verbs that read or act through the accessibility API.
const ACCESSIBILITY_ACTIONS: [&str; 11] = [
    "inspect",
    "elements",
    "press",
    "set_value",
    "focus",
    "select",
    "toggle",
    "expand",
    "collapse",
    "scroll_to",
    "wait_for",
];

/// Of those, the one that reads the control under a point, which only macOS
/// answers: a Linux session places no GTK 4 control, and nothing on Wayland.
const POINT_ACTIONS: [&str; 1] = ["inspect"];

/// The verbs that bind a window.
const TARGET_ACTIONS: [&str; 2] = ["select_target", "release_target"];

/// The verbs that capture the screen or post input to it: through xcap and enigo
/// on macOS, and on Linux through the session's pixel route, which may serve some
/// of them or none.
const PIXEL_ACTIONS: [&str; 11] = [
    "screenshot",
    "left_click",
    "right_click",
    "double_click",
    "mouse_move",
    "left_click_drag",
    "scroll",
    "key",
    "wait_for_change",
    "paste",
    "windows",
];

/// The verbs this build can really serve: the accessibility ones only where there
/// is an accessibility API, `inspect` only where it can read a point, the window
/// ones only where a window can be bound, and the pixel ones only where this
/// session's pixel route serves them.
fn advertised_actions(pixels: &Pixels) -> Vec<&'static str> {
    ACTIONS
        .into_iter()
        .filter(|action| accessibility_served(action))
        .filter(|action| targets_supported() || !TARGET_ACTIONS.contains(action))
        .filter(|action| !PIXEL_ACTIONS.contains(action) || pixels.serves(action))
        .collect()
}

/// Whether this build answers an action that reads or acts on controls. An action
/// that does neither is always served.
fn accessibility_served(action: &str) -> bool {
    if !ACCESSIBILITY_ACTIONS.contains(&action) {
        return true;
    }
    accessibility_supported() && (points_readable() || !POINT_ACTIONS.contains(&action))
}

/// How an action can reach the machine in this build: `foreground_hid` only where
/// the session's pixel route posts input, `ax` only where there is an
/// accessibility API to carry it.
fn input_methods(pixels: &Pixels) -> Vec<&'static str> {
    let mut methods = Vec::new();
    if pixels.serves("left_click") {
        methods.push("foreground_hid");
    }
    if accessibility_supported() {
        methods.push("ax");
    }
    methods
}

/// Does this build read and act on controls through an accessibility API? macOS,
/// and Linux through AT-SPI (M86 L1). Elsewhere every verb that reads or acts on a
/// control answers a typed refusal.
fn accessibility_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

/// Can this build read the control under a point?
fn points_readable() -> bool {
    cfg!(target_os = "macos")
}

/// Can this build bind a window at all?
///
/// False where every selection would fail: there is no window server this build
/// reads off macOS and no stream behind it, so `targets: true` there would be the
/// handshake advertising a feature whose first use is a refusal. [M42.1] owns the
/// Linux half and flips this with it.
fn targets_supported() -> bool {
    cfg!(target_os = "macos")
}

/// Which capture this build really has. `window` is a bound target's stream, and it
/// is listed only where it exists: a caller that reads it and gets
/// `capture_unavailable` on every selection has been lied to.
fn capture_methods(pixels: &Pixels) -> Vec<&'static str> {
    match (pixels.serves("screenshot"), targets_supported()) {
        (false, _) => Vec::new(),
        (true, true) => vec!["display", "window"],
        (true, false) => vec!["display"],
    }
}

/// Read-only, and deliberately so: it looks for the file and never spawns it, so a
/// handshake cannot put a panel on somebody's screen.
fn indicator_presence(launcher: &Arc<dyn indicator::Launcher>) -> &'static str {
    if launcher.present() {
        "present"
    } else {
        "missing"
    }
}

// --- probe (operational permission check, NOT a model action) ---------------

/// Report whether screen capture and input control are actually available, plus
/// the platform and display server. NON-PROMPTING: on macOS this queries TCC grant
/// state (Accessibility + Screen Recording) WITHOUT raising a permission dialog or
/// posting an event — the only reliable way to detect the silent-drop state where
/// capture works but synthetic input is discarded. On Linux it also says whether
/// the accessibility bus answers, which packages it rests on, and the toolkit
/// notes known without reading an application (`linux::probe`), read-only as
/// well. Surfaced by the consumer's diagnostics (a doctor/setup surface); the
/// model never calls this.
fn probe(endpoint: &Endpoint, pixels: &Pixels) -> Result<Value, Failure> {
    probe_with(endpoint, pixels, || platform_facts(endpoint))
}

/// `probe` with the facts `platform` reads: on Linux the accessibility bus, its
/// packages and the toolkit notes, and both pixel routes' state; none on macOS,
/// whose grants are the two booleans.
fn probe_with(
    endpoint: &Endpoint,
    pixels: &Pixels,
    platform: impl FnOnce() -> serde_json::Map<String, Value>,
) -> Result<Value, Failure> {
    let mut reply = with_x11_endpoint(
        json!({
            "ok": true,
            "platform": std::env::consts::OS,
            "display_server": display_server(endpoint),
            "screen_capture": screen_capture_ok(pixels),
            "input_control": input_control_ok(pixels),
        }),
        endpoint,
    );
    if let Some(object) = reply.as_object_mut() {
        object.extend(platform());
    }
    Ok(reply)
}

/// Linux: whether an X endpoint answers, beside Wayland (XWayland) or as the
/// session itself, since that is all an X client of this helper can reach; and
/// whether the display was taken from the user manager, which serves controls
/// only.
#[cfg(target_os = "linux")]
fn with_x11_endpoint(mut reply: Value, endpoint: &Endpoint) -> Value {
    if let Some(object) = reply.as_object_mut() {
        object.insert(
            "x11_endpoint".to_string(),
            json!(endpoint.server().x11_endpoint()),
        );
        object.insert("display_adopted".to_string(), json!(endpoint.adopted));
    }
    reply
}

#[cfg(not(target_os = "linux"))]
fn with_x11_endpoint(reply: Value, _endpoint: &Endpoint) -> Value {
    reply
}

/// What `probe` reports of the accessibility bus, its packages and the toolkits,
/// and of the X11 and portal pixel routes.
#[cfg(target_os = "linux")]
fn platform_facts(endpoint: &Endpoint) -> serde_json::Map<String, Value> {
    let mut facts = linux::probe::report(Duration::from_millis(ax::READ_TIMEOUT_MS));
    facts.extend(linux::pixels::probe(endpoint));
    facts
}

#[cfg(not(target_os = "linux"))]
fn platform_facts(_endpoint: &Endpoint) -> serde_json::Map<String, Value> {
    serde_json::Map::new()
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
fn request_permissions(_req: &Value, pixels: &Pixels) -> Result<Value, Failure> {
    Ok(json!({
        "ok": true,
        "platform": std::env::consts::OS,
        "screen_capture": request_screen_capture_ok(pixels),
        "input_control": request_input_control_ok(pixels),
    }))
}

#[cfg(target_os = "macos")]
fn request_screen_capture_ok(_pixels: &Pixels) -> bool {
    permissions::request_screen_capture()
}

#[cfg(target_os = "macos")]
fn request_input_control_ok(_pixels: &Pixels) -> bool {
    permissions::request_input_control()
}

// No TCC off macOS: report the same reality `probe` does (there is nothing to
// prompt, and a portal dialog is raised by first use, never by asking).
#[cfg(not(target_os = "macos"))]
fn request_screen_capture_ok(pixels: &Pixels) -> bool {
    screen_capture_ok(pixels)
}

#[cfg(not(target_os = "macos"))]
fn request_input_control_ok(pixels: &Pixels) -> bool {
    input_control_ok(pixels)
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

#[cfg(target_os = "macos")]
fn display_server(_endpoint: &Endpoint) -> &'static str {
    "quartz"
}

#[cfg(target_os = "macos")]
fn screen_capture_ok(_pixels: &Pixels) -> bool {
    permissions::screen_capture()
}

#[cfg(target_os = "macos")]
fn input_control_ok(_pixels: &Pixels) -> bool {
    permissions::input_control()
}

// Linux: the display server is decided by which socket answers, and capture and
// input by the session's pixel route (X11 through x11rb, GNOME Wayland through the
// portal); a real capture or input still fails loud per action.
#[cfg(target_os = "linux")]
fn display_server(endpoint: &Endpoint) -> &'static str {
    endpoint.server().name()
}

#[cfg(target_os = "linux")]
fn screen_capture_ok(pixels: &Pixels) -> bool {
    pixels.serves("screenshot")
}

#[cfg(target_os = "linux")]
fn input_control_ok(pixels: &Pixels) -> bool {
    pixels.serves("left_click")
}

// --- display selection ------------------------------------------------------

/// A display, as the OS describes it. The geometry is NOT here: it depends on the
/// pixels-per-point ratio, which is measured from a capture (see `mod geometry`),
/// so it is built per action from these facts and a measurement.
#[derive(Clone, Copy, Debug)]
struct Display {
    facts: MonitorFacts,
    /// The monitor's stable id (CGDirectDisplayID on macOS). Capture and the
    /// asleep check both re-resolve the monitor by THIS (never by list index),
    /// so a mid-action display change fails typed instead of rebinding to a
    /// different physical monitor. The xcap `Monitor` handle isn't `Send` and
    /// isn't held past geometry read — the id is all a later capture needs.
    id: u32,
}

/// What the OS says about the displays, behind a trait.
///
/// It exists for one reason: enumerating the screens is an OS call, and a unit
/// test that makes it is a test that passes on a developer's Mac and fails on a
/// headless runner. Every test worker is built with [`ScriptedDisplays`], and the
/// real implementation REFUSES TO RUN under `cfg(test)` — so a path that reaches
/// the screen fails here too, rather than only where there is no screen to reach.
trait Displays {
    /// The display at this index, as the OS describes it right now.
    fn at(&self, index: usize) -> Result<Display, String>;
}

/// The real one: `xcap`'s monitor list on macOS; on Linux the pixel route's
/// (RandR on X11, the portal session's monitor on Wayland).
struct RealDisplays {
    #[cfg(target_os = "linux")]
    #[cfg_attr(test, allow(dead_code))]
    screen: linux::pixels::Screen,
}

impl RealDisplays {
    #[cfg(target_os = "linux")]
    fn of(pixels: &Pixels) -> RealDisplays {
        RealDisplays {
            screen: pixels.screen(),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn of(_pixels: &Pixels) -> RealDisplays {
        RealDisplays {}
    }
}

impl Displays for RealDisplays {
    #[cfg(all(not(test), target_os = "macos"))]
    fn at(&self, index: usize) -> Result<Display, String> {
        enumerate_display(index)
    }

    #[cfg(all(not(test), target_os = "linux"))]
    fn at(&self, index: usize) -> Result<Display, String> {
        let (facts, id) = self.screen.monitor(index)?;
        Ok(Display { facts, id })
    }

    /// Under test there is no screen this process may look at, whatever this
    /// machine happens to have attached. A test that lands here has been given the
    /// real displays by mistake; the panic names the fix, and it is the whole
    /// proof that the suite is headless-safe.
    #[cfg(test)]
    fn at(&self, _index: usize) -> Result<Display, String> {
        panic!(
            "a unit test asked the OS to enumerate displays: build the worker with \
             ScriptedDisplays, or make the path under test resolve its display lazily"
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
#[cfg(target_os = "macos")]
fn select_monitor(monitors: Vec<Monitor>, index: usize) -> Result<Monitor, String> {
    if monitors.is_empty() {
        return Err("no_active_display".to_string());
    }

    monitors
        .into_iter()
        .nth(index)
        .ok_or_else(|| format!("display {index} not found"))
}

/// Which display the request named. Parsing only — it asks nobody anything, which
/// is what lets a view carry it around unresolved.
fn display_index(req: &Value) -> usize {
    req.get("display").and_then(Value::as_u64).unwrap_or(0) as usize
}

/// What the OS says about the requested display, right now. Cheap — it enumerates
/// monitors and reads their numbers, and takes no capture — which is what lets the
/// staleness check run before every addressed action.
///
/// Through the worker's [`Displays`], never `Monitor::all` directly: that is the
/// one seam that keeps a unit test off the real screen.
fn target_display(req: &Value, worker: &Worker) -> Result<Display, String> {
    worker.displays.at(display_index(req))
}

/// The enumeration itself. Only [`RealDisplays`] calls it, and only on a build
/// that is not the test binary.
#[cfg(all(not(test), target_os = "macos"))]
fn enumerate_display(index: usize) -> Result<Display, String> {
    let monitors = Monitor::all().map_err(|e| format!("enumerate displays: {e}"))?;
    let monitor = select_monitor(monitors, index)?;

    // xcap 0.4 returns the monitor geometry as `Result`s — unwrap each loudly so a
    // capture-backend hiccup surfaces as a clean action error, never a wrong click.
    let facts = MonitorFacts {
        x: monitor.x().map_err(|e| format!("display origin x: {e}"))?,
        y: monitor.y().map_err(|e| format!("display origin y: {e}"))?,
        width: monitor.width().map_err(|e| format!("display width: {e}"))?,
        height: monitor
            .height()
            .map_err(|e| format!("display height: {e}"))?,
        scale_factor: monitor
            .scale_factor()
            .map_err(|e| format!("scale_factor: {e}"))?
            .max(1.0),
    };

    Ok(Display {
        facts,
        id: monitor.id().map_err(|e| format!("display id: {e}"))?,
    })
}

/// The transform for a display that is about to hand out coordinates WITHOUT
/// capturing an image (`windows`, `elements`).
///
/// It needs a real measurement and the only honest source of one is a real capture,
/// so a display whose CURRENT configuration nothing has captured is measured here
/// with one frame that is thrown away. Guessing it from the display mode is exactly
/// the assumption this slice removed — and remembering it against the display's id
/// alone would be the same assumption one mode change later, so the memo is keyed on
/// the facts it was measured under.
fn measured_geometry(
    display: &Display,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<Geometry, Failure> {
    if let Some(measured) = worker.measured.get(display.id, &display.facts) {
        return Ok(Geometry::from_facts(&display.facts, measured, host(worker)));
    }

    let image = take_frame(display, gate, &worker.frames)?;
    measure_geometry(display, &image, worker)
}

/// Build the display's transform from a frame that was really captured, and
/// remember it for the replies that hand out coordinates without one.
fn measure_geometry(
    display: &Display,
    image: &image::RgbaImage,
    worker: &mut Worker,
) -> Result<Geometry, Failure> {
    let measured = geometry::measure(&display.facts, image.width(), image.height(), host(worker))
        .map_err(|detail| Failure::new("capture_geometry_mismatch", detail))?;

    worker
        .measured
        .remember(display.id, &display.facts, measured);
    Ok(Geometry::from_facts(&display.facts, measured, host(worker)))
}

/// Which reading of the display's numbers applies: the platform's, or on GNOME
/// Wayland the portal's logical points with the scale measured off each frame.
fn host(worker: &Worker) -> Host {
    if worker.pixels.portal() {
        Host::Portal
    } else {
        Host::HERE
    }
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

/// The rectangle a VIEWING action is asking for, in the full-display sent pixels of
/// the geometry in force now.
///
/// Three cases, and none of them is a recovery path for another. With no `region`
/// it is the whole display, and an image named beside no rectangle has nothing to
/// qualify — it is accepted and unused rather than refused, because a caller that
/// names its image on every request is doing the right thing. With a `region` and
/// no image named, the rectangle is read in a full-display image, which is the
/// space `windows` answers in. With both, it is read in THAT image and mapped
/// through the transform that image was made with — so a caller can zoom into a
/// crop of a crop without ever doing arithmetic of its own.
fn viewing_region(
    request: &wire::Request,
    geom: &Geometry,
    worker: &Worker,
) -> Result<Region, Failure> {
    let Some(rect) = parse_region(&request.body)? else {
        return Ok(Region::full(geom));
    };

    let Some(id) = request.observation_id.as_deref() else {
        return Ok(rect);
    };

    let observation = worker.observations.resolve(id).map_err(refused)?;

    Ok(geometry::rect_through(
        &observation.geometry,
        &observation.region,
        &rect,
        geom,
    ))
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
    /// pixel estimation at all.
    pub fn badge(img: &mut RgbaImage, x: i32, y: i32, n: usize) {
        let text = n.to_string();
        let half_w = (text_width(&text) / 2 + 4).max(8);

        fill_rect(img, x - half_w - 1, y - 7, 2 * half_w + 2, 14, BLACK);
        fill_rect(img, x - half_w, y - 6, 2 * half_w, 12, RED);
        draw_label(img, x - text_width(&text) / 2, y - 3, &text);
    }
}

// --- binding one window ------------------------------------------------------

/// Bind one window and answer what was bound, with a first observation OF IT.
///
/// One target per helper. Selecting again REPLACES the old one, and dropping it is
/// what stops its stream and its badge — so there is never a moment with two
/// streams, two badges, or a badge over a window nobody is working in.
fn select_target(
    request: &wire::Request,
    gate: &Gate,
    emitter: &capture::Emitter,
    worker: &mut Worker,
) -> Result<Value, Failure> {
    let window_id = request
        .body
        .get("window_id")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            Failure::new(
                "invalid_argument",
                "select_target needs window_id: the id a `windows` listing gave the window"
                    .to_string(),
            )
        })? as u32;

    // The display the window is on, so a target observation records the same facts
    // every other observation does and a display that changes mode is still caught.
    let display = target_display(&request.body, worker)?;

    // The old binding goes FIRST and completely: its stream, its badge and its
    // watch stop with it, and only then is anything new started.
    worker.target = None;
    worker.target_generation += 1;
    let generation = worker.target_generation;

    let selected = target::select(
        window_id,
        generation,
        format!("t{generation}"),
        gate,
        target::Selecting {
            windows: &worker.windows,
            ax: &worker.ax,
            launcher: &worker.launcher,
            emitter,
            stream: new_window_stream(gate),
            display_id: display.id,
            facts: display.facts,
        },
    )?;

    let mut payload = target::selected_payload(&selected.target);
    let geometry = selected.geometry;
    let image = selected.first.to_rgba().map_err(target::as_failure)?;
    worker.target = Some(selected.target);

    // The first observation is a picture of the WINDOW, so the caller's next
    // coordinates are pixels of it rather than of the display behind it.
    let captured = capture_payload_encoded(
        &display,
        geometry,
        &image,
        Rendering {
            requested: Requested::Exactly(Region::full(&geometry)),
            jpeg_quality: None,
            overlays: Overlays::default(),
        },
        gate,
        worker,
    )?;
    merge_into(&mut payload, captured);
    note_target(&mut payload, worker, true)?;

    Ok(payload)
}

/// Let the bound window go. Idempotent, and a no-op when there was none: releasing
/// nothing is what the caller asked for either way, and refusing it would make a
/// tidy-up path fail for doing its job.
fn release_target(worker: &mut Worker) -> Result<Value, Failure> {
    let released = worker.target.take().is_some();

    Ok(json!({ "ok": true, "released": released }))
}

/// A fresh stream for one binding.
#[cfg(target_os = "macos")]
fn new_window_stream(gate: &Gate) -> Rc<dyn window_frames::WindowFrames> {
    Rc::new(window_frames::sck::Stream::new(gate.clock()))
}

/// Linux has no window capture this build speaks, so a selection fails typed rather
/// than binding a window nothing can look at. [M42.1] owns the Linux half.
#[cfg(not(target_os = "macos"))]
fn new_window_stream(_gate: &Gate) -> Rc<dyn window_frames::WindowFrames> {
    Rc::new(window_frames::Unsupported)
}

/// Fold one payload into another, which is how `select_target` answers both what it
/// bound and the first picture of it in one reply.
fn merge_into(payload: &mut Value, extra: Value) {
    let (Some(object), Value::Object(extra)) = (payload.as_object_mut(), extra) else {
        return;
    };

    for (key, value) in extra {
        // `ok` is already there and the two agree; anything else the capture named
        // is the caller's to read.
        object.entry(key).or_insert(value);
    }
}

// --- what a request is answered from ----------------------------------------

/// Where an action's pixels come from and what they mean.
///
/// Two ways to fill it and ONE shape after that: the display, or the one bound
/// window. Everything downstream — the transform, the frames a settle samples
/// through, the rectangle a check covers, the observation that is minted — is the
/// same code either way, which is the whole reason a window's frames are just
/// another [`Frames`].
struct View {
    /// The display this view is on, WHEN IT IS ALREADY KNOWN. A bound window
    /// brings its own with the binding; the display-level path leaves this `None`
    /// and carries the index instead.
    ///
    /// Lazy on purpose, and the reason is a bug this shape had: asking the OS
    /// which screens exist is an OS call, and an action that presses a control by
    /// name has no business making it. Resolving it when the view was built meant
    /// a machine with no display session refused `press` with a display error
    /// before it ever revalidated the control — so `stale_element`,
    /// `element_disabled` and `ax_action_unsupported`, every one of which sent
    /// nothing, were masked by the state of a screen they do not touch.
    on: Option<Display>,
    /// Which display the request named, for the resolve that may never happen.
    index: usize,
    /// The transform, when it is already known. A bound window's is built from the
    /// frame it was read from, so it is known before anything is captured; the
    /// display's is measured from a real capture, and measuring it eagerly would
    /// cost an action that never looks a capture it does not need.
    geometry: Option<Geometry>,
    frames: Rc<dyn Frames>,
    /// The window this view is OF, when it is one.
    bound: Option<target::Live>,
}

impl View {
    /// The display this view is on, asking the OS only if nothing has yet.
    ///
    /// Every caller of this is about to want pixels. An action whose check is
    /// `semantic` or `none` never reaches it, which is the point.
    fn display(&self, worker: &Worker) -> Result<Display, Failure> {
        match self.on {
            Some(display) => Ok(display),
            None => Ok(worker.displays.at(self.index)?),
        }
    }

    /// The transform in force, measuring the display's if nothing has yet.
    fn geometry(&self, gate: &Gate, worker: &mut Worker) -> Result<Geometry, Failure> {
        match self.geometry {
            Some(geometry) => Ok(geometry),
            None => {
                let display = self.display(worker)?;
                measured_geometry(&display, gate, worker)
            }
        }
    }
}

/// The bound window this request names, checked and ready — or `None`, which is the
/// display-level path exactly as it was.
///
/// The badge check is here rather than in each action, because "no ready indicator,
/// no target mutation" is a property of the REQUEST (does it name a window, does it
/// dispatch input) and not of what any one verb does.
fn bound(
    request: &wire::Request,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<Option<target::Live>, Failure> {
    let Some(named) = request.target_id.as_deref() else {
        return Ok(None);
    };

    let live = target::live(worker.target.as_ref(), named, &worker.windows, &worker.ax)?;

    if target::needs_control_surface(&request.action) {
        let (launcher, windows, emitter) = (
            worker.launcher.clone(),
            worker.windows.clone(),
            worker.emitter.clone(),
        );
        if let Some(target) = worker.target.as_mut() {
            target::control_surface(
                target,
                target::Restarting {
                    launcher: &launcher,
                    windows: &windows,
                    gate,
                    emitter: &emitter,
                },
            )?;
        }
    }

    Ok(Some(live))
}

/// What this request looks at: the bound window it names, or the display.
fn view(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<View, Failure> {
    let bound = bound(request, gate, worker)?;

    match bound {
        Some(live) => Ok(View {
            on: Some(Display {
                facts: live.facts,
                id: live.display_id,
            }),
            index: display_index(&request.body),
            geometry: Some(live.geometry),
            frames: live.frames.clone(),
            bound: Some(live),
        }),

        // Nothing is asked of the OS here. A `press` with no check builds this
        // view, dispatches, and never resolves a display at all.
        None => Ok(View {
            on: None,
            index: display_index(&request.body),
            geometry: None,
            frames: worker.frames.clone(),
            bound: None,
        }),
    }
}

// --- screenshot -------------------------------------------------------------

fn screenshot(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let req = &request.body;
    let view = view(request, gate, worker)?;
    let display = view.display(worker)?;
    let frame = take_frame(&display, gate, &view.frames)?;
    // A display's transform is measured from the frame that was really captured; a
    // bound window's came with the frame. Both are the picture speaking for itself.
    let geom = match view.geometry {
        Some(geometry) => geometry,
        None => measure_geometry(&display, &frame, worker)?,
    };

    let mut payload = capture_payload_encoded(
        &display,
        geom,
        &frame,
        Rendering {
            requested: Requested::Region(request),
            jpeg_quality: parse_jpeg_quality(req)?,
            overlays: parse_overlays(req)?,
        },
        gate,
        worker,
    )?;
    note_target(&mut payload, worker, view.bound.is_some())?;

    Ok(payload)
}

/// Say which window a reply is about, and which of that application's other windows
/// have appeared since it was bound.
///
/// On every reply that looks at THAT target, because a sheet that opened is the one
/// thing a caller cannot see in a picture of the window it was covering — and on no
/// other reply, however much a target happens to be bound. A display-level
/// screenshot is a picture of the whole screen, and stamping a `target_id` on it
/// would tell the caller its coordinates are in a window's space when they are not.
fn note_target(payload: &mut Value, worker: &Worker, about_target: bool) -> Result<(), Failure> {
    if !about_target {
        return Ok(());
    }

    let (Some(target), Some(object)) = (worker.target.as_ref(), payload.as_object_mut()) else {
        return Ok(());
    };

    // A listing that could not be taken is NOT an empty one: the caller would read
    // "no new windows" for "nobody knows", which is the sheet it cannot see.
    let children = target::children(target, &worker.windows)?;

    object.insert("target_id".to_string(), json!(target.id));
    object.insert("children".to_string(), Value::Array(children));
    Ok(())
}

/// What a capture is being asked to cover. A `screenshot` carries the request whose
/// `region` says which rectangle of the display, and whose `observation_id` says
/// which image that rectangle was read in. A `wait_for_change` and an action's
/// check have each resolved their rectangle already — the wait because every poll
/// must watch the same patch, the check because it is the view the action was
/// aimed in.
enum Requested<'a> {
    Region(&'a wire::Request),
    Exactly(Region),
}

/// Grounding-integrity overlays for one capture (M28), all drawn on the SENT
/// image in its own pixel space: `rulers` (B2) makes the answer grid visible,
/// `annotate_point` (B1) marks an executed click in a check image, `marks` (B3)
/// badges accessibility click points and returns their id table.
#[derive(Default, Clone, Copy)]
struct Overlays {
    rulers: bool,
    marks: bool,
    annotate: Option<Annotate>,
}

/// Where to draw the executed-point marker, in the one space its sender knows it
/// in. A `screenshot` names a point in the image it is asking for. An action's
/// check image names the point the action really executed at, which it knows as a
/// LOGICAL point — the image the model read the coordinate in may be a crop, and
/// the check is always the whole display, so the marker would otherwise be drawn
/// at the crop's numbers on the full screen.
#[derive(Clone, Copy)]
enum Annotate {
    Sent(i32, i32),
    Logical(f32, f32),
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
            Some(Annotate::Sent(x.round() as i32, y.round() as i32))
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
/// Compiled out of the test binary: its only caller is [`Screen::capture`]'s real
/// body, and a unit test that reached the real screen would be a test that cannot
/// run headless.
#[cfg(all(target_os = "macos", not(test)))]
fn ensure_display_awake(display_id: u32) -> Result<(), String> {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        // boolean_t CGDisplayIsAsleep(CGDirectDisplayID display)
        fn CGDisplayIsAsleep(display: u32) -> u32;
    }

    if unsafe { CGDisplayIsAsleep(display_id) } != 0 {
        return Err("display_asleep".to_string());
    }
    Ok(())
}

// --- bounded capture (the anti-stall watchdog) -------------------------------

/// Hard budget for one physical frame grab. A real capture takes ~0.2–0.5s; one
/// that exceeds this is stalled inside the OS capture service (the 2026-07-01
/// wedge: CGWindowListCreateImage's ScreenCaptureKit proxy waited out a ~30s XPC
/// semaphore PER CALL), and waiting would burn the caller's whole action
/// deadline. Fail fast with the typed `capture_stalled` instead.
#[cfg(not(test))]
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
/// Compiled out of the test binary: its only caller is [`Screen::capture`]'s real
/// body, and a unit test that reached the real screen would be a test that cannot
/// run headless.
#[cfg(not(test))]
fn capture_display_image(
    grab: impl FnOnce() -> Result<image::RgbaImage, String> + Send + 'static,
) -> Result<image::RgbaImage, String> {
    let (tx, rx) = mpsc::channel();

    let spawned = thread::Builder::new()
        .name("compux-capture".to_string())
        .spawn(move || {
            // The receiver is gone only once the budget ran out, and then the
            // stall is already what was answered.
            let _ = tx.send(grab());
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

#[cfg(all(not(test), target_os = "macos"))]
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

/// One display frame, and the seam a test feeds them through.
///
/// Narrow on purpose. What a caller of this module decides is WHICH frame to keep
/// — the one a settle stopped on, the one that ended a wait — and that decision is
/// what has to be provable without a screen. How a frame is grabbed (a worker
/// thread, a stall budget, a re-resolve by display id) is not this trait's
/// business and stays exactly where it was.
trait Frames {
    fn capture(&self, display_id: u32) -> Result<image::RgbaImage, Failure>;
}

/// The real screen: xcap on macOS; on Linux the pixel route's (`GetImage` on
/// X11 under the same stall budget, the portal's screenshot on Wayland under its
/// consent budget).
struct Screen {
    #[cfg(target_os = "linux")]
    #[cfg_attr(test, allow(dead_code))]
    screen: linux::pixels::Screen,
}

impl Screen {
    #[cfg(target_os = "linux")]
    fn of(pixels: &Pixels) -> Screen {
        Screen {
            screen: pixels.screen(),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn of(_pixels: &Pixels) -> Screen {
        Screen {}
    }
}

impl Frames for Screen {
    #[cfg(all(not(test), target_os = "macos"))]
    fn capture(&self, display_id: u32) -> Result<image::RgbaImage, Failure> {
        ensure_display_awake(display_id)?;
        Ok(capture_display_image(move || capture_by_id(display_id))?)
    }

    #[cfg(all(not(test), target_os = "linux"))]
    fn capture(&self, display_id: u32) -> Result<image::RgbaImage, Failure> {
        match &self.screen {
            linux::pixels::Screen::X11(env) => {
                let env = env.clone();
                Ok(capture_display_image(move || {
                    linux::x11::capture(&env, display_id)
                })?)
            }
            linux::pixels::Screen::Portal(portal) => linux::pixels::portal_frame(portal)
                .map_err(|refusal| Failure::new(refusal.code(), refusal.detail().to_string())),
            linux::pixels::Screen::Refused(why) => {
                Err(Failure::new("pixels_unavailable", why.clone()))
            }
        }
    }

    /// Under test there is no screen this process may capture, whatever this
    /// machine has attached. Every test worker is given [`tests::BlankScreen`];
    /// landing here means one was not, and a capture of the developer's desktop is
    /// both a test that cannot run headless and a picture nobody asked for.
    #[cfg(test)]
    fn capture(&self, _display_id: u32) -> Result<image::RgbaImage, Failure> {
        panic!(
            "a unit test asked to capture the real screen: build the worker with a scripted \
             `Frames`"
        )
    }
}

/// One frame, as an action reads it, timed the way the receipt reports it. Every
/// capture in this process goes through here, so what a frame costs is counted once
/// and in one place — a display's, and a bound window's alike.
fn take_frame(
    display: &Display,
    gate: &Gate,
    frames: &Rc<dyn Frames>,
) -> Result<image::RgbaImage, Failure> {
    let started = gate.now_ms();
    let image = frames.capture(display.id);
    gate.record(Phase::Capture, gate.now_ms().saturating_sub(started));
    image
}

/// What one captured frame is turned into: which rectangle of it, how it is
/// encoded, and what is drawn on it. A struct rather than three arguments, because
/// the three always travel together and a call site reads better naming them.
struct Rendering<'a> {
    requested: Requested<'a>,
    jpeg_quality: Option<u8>,
    overlays: Overlays,
}

fn capture_payload_encoded(
    display: &Display,
    geom: Geometry,
    image: &image::RgbaImage,
    rendering: Rendering<'_>,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<Value, Failure> {
    let Rendering {
        requested,
        jpeg_quality,
        overlays,
    } = rendering;
    // The transform is the caller's, because it is built from the FRAME this image
    // came out of — a display's measured from the capture, a bound window's from the
    // window's own content rectangle — and how many pixels there are per point is a
    // fact about the picture rather than about the display mode.
    let region = match requested {
        Requested::Exactly(region) => region,
        Requested::Region(request) => viewing_region(request, &geom, worker)?,
    };
    let crop = crop_rect(&geom, &region);
    let (sent_w, sent_h) = crop.sent_dims();

    // Crop to the region's physical rect, then downscale to the sent size. The
    // model's coordinates live in this (sent) space; `to_logical` inverts it.
    let cropped = crop_out(image, &crop);

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
        Some(collect_marks(
            &geom,
            &region,
            &mut resized,
            gate,
            &worker.ax,
        ))
    } else {
        None
    };
    let annotate = match overlays.annotate {
        None => None,
        Some(Annotate::Sent(x, y)) => Some((x, y)),
        // Placed through THIS image's own transform, so the marker lands where the
        // action landed even when the coordinate was read on a crop.
        Some(Annotate::Logical(lx, ly)) => {
            to_sent(&geom, &region, lx as f64, ly as f64).map(|(x, y)| (x as i32, y as i32))
        }
    };
    if let Some((ax, ay)) = annotate {
        overlay::executed_point(&mut resized, ax, ay);
    }

    let started = gate.now_ms();
    let (encoded, mime) = encode_image(&resized, sent_w, sent_h, jpeg_quality)?;
    let data = base64::engine::general_purpose::STANDARD.encode(&encoded);
    gate.record(Phase::Encode, gate.now_ms().saturating_sub(started));

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
    if let (Some((cursor_x, cursor_y)), Some(object)) = (
        cursor_point(worker, &geom, &region),
        payload.as_object_mut(),
    ) {
        object.insert(
            "cursor".to_string(),
            json!({ "x": cursor_x, "y": cursor_y }),
        );
    }

    // B3: the mark table, present (possibly empty) whenever marks were requested,
    // so the caller can tell "zero accessibility marks" from "none asked for".
    let mut listed = None;
    if let (Some(info), Some(object)) = (marks, payload.as_object_mut()) {
        object.insert("marks".to_string(), Value::Array(info.entries));
        if let Some(note) = info.ax_activation {
            object.insert("ax_activation".to_string(), json!(note));
        }
        if info.truncated > 0 {
            object.insert("marks_truncated".to_string(), json!(info.truncated));
        }
        // The walk's own bound, distinct from the badge cap above it: one says a
        // marked image shows fewer controls than exist, the other that the tree
        // was not read to its end.
        if let Some(truncation) = info.truncated_walk {
            object.insert("truncated".to_string(), json!(truncation));
        }
        listed = info.elements;
    }

    // This image is now the space the caller's next coordinates are in, marks
    // included, so it is minted and named before it leaves — with the references
    // behind those marks, which live and die with it.
    let observation = worker.observations.mint(observation::Minting {
        kind: Kind::Image,
        display_id: display.id,
        facts: display.facts,
        geometry: geom,
        region,
        sent: (sent_w, sent_h),
        elements: listed,
        // Kept with the image so a later check can say whether this view changed,
        // without the caller holding anything or a second capture proving it.
        // Hashed off the FRAME, not off `cropped`: the same pixels either way, and
        // the rectangle that rides with it is then the one in the frame's own space.
        view_hash: Some(view_hash(image, &crop)),
    });
    name_observation(&mut payload, &observation);

    Ok(payload)
}

/// Say which image a reply's coordinates are in, the same three fields on every
/// reply that hands any out (plus the frame counter, for the ones that are a
/// picture). `width`, `height`, `region`, `scale`, `origin` and `physical` keep
/// their own meanings beside these.
fn name_observation(payload: &mut Value, observation: &Observation) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };

    object.insert("observation_id".to_string(), json!(observation.id));
    object.insert(
        "observation_kind".to_string(),
        json!(observation.kind.as_str()),
    );
    object.insert(
        "captured_at_monotonic_ns".to_string(),
        json!(observation.captured_at_monotonic_ns),
    );
    if let Some(frame_seq) = observation.frame_seq {
        object.insert("frame_seq".to_string(), json!(frame_seq));
    }
}

/// The badge cap keeps a marked image readable — a dense tree can expose
/// hundreds of interactive nodes, and a badge soup grounds worse than pixels.
/// Tree-walk order is roughly top-down, so the cap drops the least prominent.
const MAX_MARKS: usize = 60;

struct MarksInfo {
    entries: Vec<Value>,
    ax_activation: Option<String>,
    /// How many interactive nodes were found beyond the badge cap.
    truncated: usize,
    /// Why the WALK stopped early, when it did — a different fact from the cap
    /// above, and one a caller needs before concluding a control is not there.
    truncated_walk: Option<&'static str>,
    /// The references behind these badges, to live with the image they are drawn
    /// on: a mark can be pressed by name, like any other listed control.
    elements: Option<Rc<ax::Elements>>,
}

/// Badge the interactive elements in view and answer their table.
///
/// The read itself is the macOS half (`interactive_in_view`); everything from the
/// nodes onwards is the same code on every platform, which is what keeps a
/// reference, a badge and an element-list entry spelling the same fact one way.
fn collect_marks(
    geom: &Geometry,
    region: &Region,
    img: &mut image::RgbaImage,
    gate: &Gate,
    ax: &Rc<dyn ax::Ax>,
) -> MarksInfo {
    let viewed = match interactive_in_view(ax, geom, region, gate) {
        Ok(viewed) => viewed,
        Err(unsupported) => {
            return MarksInfo {
                entries: Vec::new(),
                ax_activation: Some(unsupported.code),
                truncated: 0,
                truncated_walk: None,
                elements: None,
            }
        }
    };

    // A badge needs a place: a control the platform places nowhere gets none.
    let placed: Vec<(tree::Node, (i64, i64))> = viewed
        .nodes
        .into_iter()
        .filter_map(|(node, point)| point.map(|point| (node, point)))
        .collect();
    let truncated = placed.len().saturating_sub(MAX_MARKS);
    let names = viewed.owner.is_some();
    let mut entries = Vec::new();
    let mut listed = Vec::new();

    for (index, (node, (sx, sy))) in placed.into_iter().take(MAX_MARKS).enumerate() {
        let id = index + 1;
        let reference = ax::reference_for(index);
        overlay::badge(img, sx as i32, sy as i32, id);
        let named = names.then_some(reference.as_str());
        entries.push(mark_entry(id, &node, (sx, sy), named));

        listed.push(ax::Entry {
            reference,
            role: node.role,
            secure: node.secure,
            under: None,
            element: node.element,
        });
    }

    MarksInfo {
        entries,
        ax_activation: viewed.note,
        truncated,
        truncated_walk: viewed.truncated,
        elements: retained(viewed.owner, listed),
    }
}

/// One badge's entry: its number, the control's role and click point, and the
/// reference it is named by where the reply hands those out.
///
/// `label` is spelled exactly as an `elements` entry spells it: it is the same fact
/// read from the same attributes, and two names for one fact is how a consumer
/// ends up with two ways to read a control's name. (`inspect` keeps its own
/// `title` and `description`, which are genuinely different AX attributes rather
/// than this one under another name.)
fn mark_entry(id: usize, node: &tree::Node, point: (i64, i64), reference: Option<&str>) -> Value {
    let (x, y) = point;
    let mut entry = json!({ "id": id, "role": node.role, "x": x, "y": y });
    if let Some(object) = entry.as_object_mut() {
        if let Some(reference) = reference {
            object.insert("element_ref".to_string(), json!(reference));
        }
        if let Some(label) = &node.label {
            object.insert("label".to_string(), json!(label));
        }
    }
    entry
}

/// The references one reply hands out, or `None` when it listed none — an empty
/// table would be a promise of controls that are not there.
fn retained(owner: Option<(i32, u64)>, entries: Vec<ax::Entry>) -> Option<Rc<ax::Elements>> {
    let (pid, started_at) = owner?;
    let elements = ax::Elements::new(pid, started_at, entries);

    if elements.is_empty() {
        None
    } else {
        Some(Rc::new(elements))
    }
}

// --- input ------------------------------------------------------------------

#[derive(Deserialize)]
struct Point {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
fn cursor_point(_worker: &mut Worker, geom: &Geometry, region: &Region) -> Option<(i64, i64)> {
    let input = enigo().ok()?;
    let (lx, ly) = input.location().ok()?;
    to_sent(geom, region, lx as f64, ly as f64)
}

/// On Linux the pixel route says where the pointer is: X11 does, the portal never.
#[cfg(target_os = "linux")]
fn cursor_point(worker: &mut Worker, geom: &Geometry, region: &Region) -> Option<(i64, i64)> {
    let (lx, ly) = worker.pixels.pointer()?;
    to_sent(geom, region, lx as f64, ly as f64)
}

/// The desktop an input sequence posts through. macOS builds one per action (its
/// enigo paces the events it posted when it drops); Linux keeps the session's,
/// whose clipboard owner and journal must outlive the action.
#[cfg(target_os = "macos")]
fn desktop(_worker: &mut Worker) -> Result<held::Real, Failure> {
    Ok(held::Real::default())
}

#[cfg(target_os = "linux")]
fn desktop(worker: &mut Worker) -> Result<&mut dyn held::Platform, Failure> {
    worker
        .pixels
        .input()
        .map_err(|why| Failure::new("pixels_unavailable", why))
}

/// Keys go only into a window that reads ACTIVE on the accessibility bus, and
/// text only where the layout types every character and the keyboard is neutral.
/// Each refusal is made before anything is sent. A window is never activated to
/// make room, and `_NET_ACTIVE_WINDOW` is not asked: under another application's
/// grab it names a window whose frame is not ACTIVE (L28-20).
#[cfg(target_os = "linux")]
fn ready_for_keys(worker: &mut Worker, text: Option<&str>) -> Result<(), Failure> {
    if worker.ax.frontmost_pid().is_none() {
        return Err(Failure::new(
            "no_active_window",
            "no window reads ACTIVE on the accessibility bus, so keys would go nowhere anyone \
             chose; compux never activates a window to make room: nothing was sent"
                .to_string(),
        ));
    }
    let Some(text) = text else {
        return Ok(());
    };
    match worker.pixels.ready_to_type(text)? {
        Ok(()) => Ok(()),
        Err(linux::x11::Refusal::Outside(outside)) => {
            Err(Failure::new("text_outside_layout", outside.describe()))
        }
        Err(linux::x11::Refusal::Keyboard(why)) => Err(Failure::new(
            "keyboard_not_neutral",
            format!("{why}; nothing was typed"),
        )),
    }
}

/// macOS types into the key window of the application in front, which always has
/// one.
#[cfg(target_os = "macos")]
fn ready_for_keys(_worker: &mut Worker, _text: Option<&str>) -> Result<(), Failure> {
    Ok(())
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

/// Where a pointer action is aiming, in the global logical points the input path
/// takes.
///
/// Two ways in, and neither is a recovery path for the other. A POINT is read in
/// the image that named it, through the transform that image was made with. A
/// CONTROL is re-read where it is NOW — its bounds at this moment, not where it
/// was listed — so one that moved is hit where it moved to, and one that is gone
/// is refused rather than clicked at the place it used to be.
fn aim(
    request: &wire::Request,
    observation: &Observation,
    element: Option<&ax::Entry>,
    worker: &Worker,
) -> Result<(i32, i32), Failure> {
    let Some(entry) = element else {
        let (x, y) = coords(&request.body)?;
        return point_in(observation, x, y);
    };

    let facts = present(&worker.ax, observation, entry)?;
    let bounds = facts.frame.ok_or_else(|| {
        Failure::new(
            "stale_element",
            "that control no longer reports where it is, so there is nowhere to click; take \
             `elements` again"
                .to_string(),
        )
    })?;

    let (x, y) = bounds.centre();
    Ok((x.round() as i32, y.round() as i32))
}

/// The control this request names, when it names one. `None` is "it named a
/// point", which the parse layer has already proved is the only other answer.
fn addressed_element<'a>(
    request: &wire::Request,
    observation: &'a Observation,
) -> Result<Option<&'a ax::Entry>, Failure> {
    match request.element_ref {
        None => Ok(None),
        Some(_) => element_in(observation, request).map(Some),
    }
}

/// The view an ADDRESSED action acts in, with its staleness proved.
///
/// The two paths prove the same thing about two different things. On the display,
/// the check is that the display has not moved or changed mode since the image was
/// made. On a bound window it is that the WINDOW has not: its bounds, its display
/// and its scale are part of every target observation, and a difference in any of
/// them means a coordinate read in that image maps somewhere nobody chose.
///
/// A control needs neither: its bounds are read live in global points at the moment
/// it acts, and not needing this is the point of addressing one.
fn acting_view(
    request: &wire::Request,
    observation: &Observation,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<View, Failure> {
    let view = view(request, gate, worker)?;

    match (&view.bound, request.element_ref.is_some()) {
        (_, true) => Ok(view),
        (Some(live), false) => target::unmoved(observation, live).map(|()| view),
        (None, false) => same_display(observation, &request.body, worker).map(|_display| view),
    }
}

/// A pixel action on a bound window needs that window to be topmost where it is
/// aiming. Nothing else does: typing goes to the focus, and a control is reached by
/// name rather than through whatever is drawn over it.
fn reachable(view: &View, worker: &Worker, lx: i32, ly: i32) -> Result<(), Failure> {
    match view.bound.as_ref() {
        None => Ok(()),
        Some(live) => target::unobstructed(live, &worker.windows, lx, ly),
    }
}

fn mouse_move(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let observation = observed(request, worker)?;
    let element = addressed_element(request, &observation)?;
    let (lx, ly) = aim(request, &observation, element, worker)?;
    let view = acting_view(request, &observation, gate, worker)?;
    reachable(&view, worker, lx, ly)?;
    // Read-only on the wire and still the human's pointer, so it goes through the
    // gate like every other input and a pause stops it.
    let mut platform = Gated::new(gate, desktop(worker)?);
    platform.move_mouse(lx, ly)?;
    // Same settle as the acting verbs: this action's entire promise is "the pointer
    // is now here", and a posted move alone leaves that pending (hover would land on
    // whatever the pointer had not left yet).
    platform.settle(lx, ly)?;
    gate.input_complete();
    // read-only: no post-action screenshot
    Ok(json!({ "ok": true }))
}

fn click(
    request: &wire::Request,
    gate: &Gate,
    worker: &mut Worker,
    button: Button,
    count: u32,
) -> Result<Value, Failure> {
    let req = &request.body;
    let observation = observed(request, worker)?;
    let element = addressed_element(request, &observation)?;
    let (lx, ly) = aim(request, &observation, element, worker)?;
    let view = acting_view(request, &observation, gate, worker)?;
    reachable(&view, worker, lx, ly)?;
    let mods = modifiers(req);

    // A local, not a temporary: enigo's own `Drop` paces the events it posted, and
    // it ran after the check image before. Inlining this into the call below would
    // move that pacing sleep in front of the screenshot.
    let mut platform = Gated::new(gate, desktop(worker)?);
    click_seq(&mut platform, lx, ly, button, count, &mods)?;
    gate.input_complete();

    let acted = Acted {
        named: Some(&observation),
        executed: Some((lx, ly)),
        element,
    };
    post(request, Some(view), gate, worker, acted)
}

/// The click itself, over the injected platform: warp, settle, hold the modifiers,
/// post the button. The guard releases the modifiers after the last click AND on
/// every failure in between — a `?` out of the repeat loop used to return with them
/// still down.
fn click_seq<P: held::Platform>(
    platform: &mut P,
    lx: i32,
    ly: i32,
    button: Button,
    count: u32,
    mods: &[Key],
) -> Result<(), String> {
    held::guarded(platform, |input| {
        input.move_mouse(lx, ly)?;
        input.settle(lx, ly)?;
        input.press_keys(mods)?;
        for _ in 0..count {
            input.click_button(button)?;
        }
        Ok(())
    })
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

fn drag(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let req = &request.body;
    let from: Point = parse_point(req, "from")?;
    let to: Point = parse_point(req, "to")?;
    // One image for both points: a drag whose ends were read off different
    // screenshots is a drag across two coordinate spaces.
    let observation = observed(request, worker)?;
    let (fx, fy) = point_in(&observation, from.x, from.y)?;
    let (tx, ty) = point_in(&observation, to.x, to.y)?;
    let view = acting_view(request, &observation, gate, worker)?;
    // BOTH ends: a drag that starts on the target and finishes under something else
    // would drop where nobody chose.
    reachable(&view, worker, fx, fy)?;
    reachable(&view, worker, tx, ty)?;

    // A local for the same reason as `click`: enigo's pacing runs on its drop.
    let mut platform = Gated::new(gate, desktop(worker)?);
    drag_seq(&mut platform, fx, fy, tx, ty)?;
    gate.input_complete();

    // The drag DESTINATION is what the check image marks: that is where the action
    // ended and the place the model has to judge.
    let acted = Acted {
        named: Some(&observation),
        executed: Some((tx, ty)),
        element: None,
    };
    post(request, Some(view), gate, worker, acted)
}

/// The drag itself, over the injected platform. Everything from the press to the
/// drop runs under the guard, so a failed step, a failed settle or a panic ends
/// with the left button UP — it used to end with the desktop still dragging.
fn drag_seq<P: held::Platform>(
    platform: &mut P,
    fx: i32,
    fy: i32,
    tx: i32,
    ty: i32,
) -> Result<(), String> {
    held::guarded(platform, |input| {
        input.move_mouse(fx, fy)?;
        input.settle(fx, fy)?;
        input.press_button(Button::Left)?;
        // Let the press register (and the target arm its drag) before moving.
        input.sleep(DRAG_GRAB_MS)?;
        drag_through(input, &drag_path(fx, fy, tx, ty, DRAG_STEPS))?;
        input.settle(tx, ty)?;
        // Dwell at the destination so the drop is observed where it happens.
        input.sleep(DRAG_DROP_MS)?;
        Ok(())
    })
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

/// Walk the interpolated path with a dwell at each point. What ONE step is differs
/// per platform (`held::Real::drag_step` holds that split); the pacing does not.
fn drag_through<P: held::Platform>(
    input: &mut held::Guard<'_, P>,
    path: &[(i32, i32)],
) -> Result<(), String> {
    for &(x, y) in path {
        input.drag_step(x, y)?;
        input.sleep(DRAG_STEP_MS)?;
    }

    Ok(())
}

fn scroll(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let req = &request.body;
    let observation = observed(request, worker)?;
    let element = addressed_element(request, &observation)?;
    let (lx, ly) = aim(request, &observation, element, worker)?;
    let view = acting_view(request, &observation, gate, worker)?;
    reachable(&view, worker, lx, ly)?;
    let amount = req.get("amount").and_then(Value::as_i64).unwrap_or(3) as i32;
    let (axis, length) = match req.get("direction").and_then(Value::as_str) {
        Some("up") => (Axis::Vertical, -amount),
        Some("down") => (Axis::Vertical, amount),
        Some("left") => (Axis::Horizontal, -amount),
        Some("right") => (Axis::Horizontal, amount),
        other => return Err(format!("bad scroll direction: {other:?}").into()),
    };

    // One call with the repeat count inside it, so there is no loop of ours to
    // check: it is admitted at the gate and is not interruptible after that.
    let mut platform = Gated::new(gate, desktop(worker)?);
    platform.move_mouse(lx, ly)?;
    platform.settle(lx, ly)?;
    platform.scroll(length, axis)?;
    gate.input_complete();

    let acted = Acted {
        named: Some(&observation),
        executed: Some((lx, ly)),
        element,
    };
    post(request, Some(view), gate, worker, acted)
}

fn type_text(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let req = &request.body;
    let text = req
        .get("text")
        .and_then(Value::as_str)
        .ok_or("missing text")?;
    // A single `text` call with no loop of ours, so it cannot honour a 25 ms
    // checkpoint and this slice does not pretend it can: it is checked at the gate
    // before dispatch and not inside. Chunking the string would change typing
    // timing in ways only a live check could qualify.
    // A window may still be named: typing goes to the focus wherever it is, and the
    // check then shows the window the caller is working in rather than the screen.
    let view = view(request, gate, worker)?;
    ready_for_keys(worker, Some(text))?;
    let mut platform = Gated::new(gate, desktop(worker)?);
    platform.text(text)?;
    gate.input_complete();
    // Typing executes at the focus, not at a coordinate: it names no image and
    // there is nothing to mark, so its check is the whole of the view.
    post(request, Some(view), gate, worker, nothing_named())
}

fn key_chord(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let req = &request.body;
    let chord = req
        .get("chord")
        .and_then(Value::as_str)
        .ok_or("missing chord")?;
    let parts: Vec<&str> = chord.split('+').map(str::trim).collect();
    let (mod_parts, key_part) = parts.split_at(parts.len().saturating_sub(1));
    let key_name = key_part.first().copied().ok_or("empty chord")?;

    let mods: Vec<Key> = mod_parts.iter().filter_map(|m| modifier_key(m)).collect();
    let main = named_key(key_name).ok_or_else(|| format!("unknown key: {key_name}"))?;

    let view = view(request, gate, worker)?;
    ready_for_keys(worker, None)?;
    // A local for the same reason as `click`: enigo's pacing runs on its drop.
    let mut platform = Gated::new(gate, desktop(worker)?);
    key_chord_seq(&mut platform, &mods, main)?;
    gate.input_complete();

    post(request, Some(view), gate, worker, nothing_named())
}

/// The chord itself, over the injected platform. This was the one sequence that
/// already released its modifiers after a failed key — but `hold`'s own loop could
/// still fail part way through PRESSING them and strand the earlier ones, which the
/// guard's record-before-post closes.
fn key_chord_seq<P: held::Platform>(
    platform: &mut P,
    mods: &[Key],
    main: Key,
) -> Result<(), String> {
    held::guarded(platform, |input| {
        input.press_keys(mods)?;
        input.click_key(main)
    })
}

fn wait(req: &Value, gate: &Gate) -> Result<Value, Failure> {
    // Clamped like every other blocking verb: the wire is one-request-one-response,
    // so an unbounded sleep would hold the whole session hostage to one argument.
    let ms = req
        .get("ms")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_WAIT_FOR_IDLE_MS);
    // Sliced at the checkpoint cadence, so a pause during a long wait returns at
    // once instead of running the argument out.
    gate.sleep(ms)
        .map_err(|refusal| refusal.code().to_string())?;
    Ok(json!({ "ok": true }))
}

// --- v2 actions: wait_for_change / paste / elements -------------------------

/// Block until the region's pixels change (or `timeout_ms` elapses), then return
/// the resulting screenshot plus a `changed` flag. Each poll captures the frame
/// (xcap has no sub-region capture) and diffs an AVERAGED thumbnail hash of the
/// region; the poll budget is bounded by the caller's protocol.
fn wait_for_change(
    request: &wire::Request,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<Value, Failure> {
    let display = target_display(&request.body, worker)?;
    wait_for_change_on(&display, request, gate, worker)
}

/// The wait itself, on a display already resolved — which is the whole of it, and
/// the half that is provable without a screen.
fn wait_for_change_on(
    display: &Display,
    request: &wire::Request,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<Value, Failure> {
    let req = &request.body;
    // The image the caller named, resolved before anything is captured: an id the
    // table no longer holds is refused here rather than after a wait.
    let named = named_observation(request, worker)?;
    // The rectangle is resolved once, before the first baseline, so every poll and
    // the frame that ends the wait all watch the same patch of screen.
    let geom = measured_geometry(display, gate, worker)?;
    let region = viewing_region(request, &geom, worker)?;
    let crop = crop_rect(&geom, &region);
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

    // What "changed" is measured against: the hash kept with the image the caller
    // named, when the rectangle being watched IS that image's own view, and a
    // sample taken now otherwise. Naming an image and watching a different
    // rectangle of it is a different question, and answering it from that image's
    // hash would report a change the instant the wait began.
    let baseline = match stored_baseline(named.as_ref(), &region) {
        Some(hash) => hash,
        None => sample_hash(&take_frame(display, gate, &worker.frames)?, &crop, gate),
    };
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        gate.sleep(poll_ms)
            .map_err(|refusal| refusal.code().to_string())?;

        // Each poll KEEPS its frame, and the one whose hash differs is the one
        // encoded: capturing a third time would hand back a frame that may show
        // something else again, which is not what the wait was satisfied by. A
        // timeout returns its last sample for the same reason.
        let frame = take_frame(display, gate, &worker.frames)?;
        // A pair that is not comparable at all — the frame changed shape under the
        // wait — IS a change: whatever the caller was watching, it is not what it is
        // looking at now, and the fresh frame is the answer to that.
        let changed = sample_hash(&frame, &crop, gate)
            .changed_from(&baseline)
            .unwrap_or(true);

        if changed || Instant::now() >= deadline {
            // The returned frame becomes the caller's coordinate view, so it
            // carries the same `rulers` grid an explicit screenshot would.
            let overlays = Overlays {
                rulers: req.get("rulers").and_then(Value::as_bool).unwrap_or(false),
                marks: false,
                annotate: None,
            };
            let mut payload = capture_payload_encoded(
                display,
                geom,
                &frame,
                Rendering {
                    requested: Requested::Exactly(region),
                    jpeg_quality: None,
                    overlays,
                },
                gate,
                worker,
            )?;
            if let Some(object) = payload.as_object_mut() {
                object.insert("changed".to_string(), json!(changed));
            }
            return Ok(payload);
        }
    }
}

/// The hash kept with the image a request named, when that image's own view is
/// what is being watched. `None` is "there is nothing remembered to compare
/// against", and the caller takes a sample instead.
fn stored_baseline(named: Option<&Observation>, region: &Region) -> Option<ViewHash> {
    let observation = named?;

    if same_rect(&observation.region, region) {
        observation.view_hash
    } else {
        None
    }
}

/// The observation a request names, when it names one. Separate from `observed`,
/// which is the ADDRESSED form and refuses a request that names none.
fn named_observation(
    request: &wire::Request,
    worker: &Worker,
) -> Result<Option<Observation>, Failure> {
    match request.observation_id.as_deref() {
        None => Ok(None),
        Some(id) => Ok(Some(
            worker.observations.resolve(id).map_err(refused)?.clone(),
        )),
    }
}

/// An action that names no image and marks no point: its check is the display.
fn nothing_named<'a>() -> Acted<'a> {
    Acted {
        named: None,
        executed: None,
        element: None,
    }
}

/// A crop's whole pixels inside a frame: rounded, and clamped to the frame so a
/// rectangle that runs past an edge indexes nothing that is not there. One reader,
/// so the rectangle that is HASHED and the rectangle that is ENCODED are the same
/// rectangle by construction rather than by two roundings that happen to agree.
fn crop_bounds(image: &image::RgbaImage, crop: &geometry::CropRect) -> (u32, u32, u32, u32) {
    let left = (crop.left_phys.round().max(0.0) as u32).min(image.width().saturating_sub(1));
    let top = (crop.top_phys.round().max(0.0) as u32).min(image.height().saturating_sub(1));
    let w = (crop.w_phys.round().max(1.0) as u32).min(image.width() - left);
    let h = (crop.h_phys.round().max(1.0) as u32).min(image.height() - top);
    (left, top, w, h)
}

/// The physical rectangle of one frame, at full resolution.
fn crop_out(image: &image::RgbaImage, crop: &geometry::CropRect) -> image::RgbaImage {
    let (left, top, w, h) = crop_bounds(image, crop);
    image::imageops::crop_imm(image, left, top, w, h).to_image()
}

/// The change-detector: hash every pixel of a rectangle, read IN PLACE out of the
/// frame it lives in — one contiguous run per row, no copy and no resize.
///
/// The first version hashed a 256x256 downscale of the crop, and that bought
/// nothing. A thumbnail is not more tolerant than the pixels for an EQUALITY test:
/// a blinking caret perturbs an averaged cell exactly as it perturbs a pixel, so
/// either hash differs, and the resize only added time — measured at 258 ms a
/// sample against the 195 ms capture beside it on a 3840x1080 panel, which made the
/// settle cost more than the thing it was waiting for.
///
/// Every pixel participates. A stride would be the way to miss a small change; the
/// rows are read whole, so nothing falls between sample points.
///
/// One function for every comparison in this process: the hash kept with an image
/// when it is minted, the samples a settle compares, and the polls of a wait. What
/// makes two of them comparable rides with them (see [`ViewHash`]).
fn view_hash(image: &image::RgbaImage, crop: &geometry::CropRect) -> ViewHash {
    let (left, top, w, h) = crop_bounds(image, crop);
    let stride = image.width() as usize * 4;
    let raw = image.as_raw();
    let mut hasher = DefaultHasher::new();

    for row in top..top + h {
        let start = row as usize * stride + left as usize * 4;
        hasher.write(&raw[start..start + w as usize * 4]);
    }

    ViewHash {
        pixels: hasher.finish(),
        rect: (left, top, w, h),
        frame: (image.width(), image.height()),
    }
}

/// One sample of a rectangle, counted as settling time because deciding whether the
/// view has stopped moving is what it is for.
fn sample_hash(image: &image::RgbaImage, crop: &geometry::CropRect, gate: &Gate) -> ViewHash {
    let started = gate.now_ms();
    let hash = view_hash(image, crop);
    gate.record(Phase::Settle, gate.now_ms().saturating_sub(started));
    hash
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
fn idle_ms(worker: &Worker) -> Result<Value, Failure> {
    let mut payload = json!({ "ok": true, "idle_ms": human_idle_ms()? });
    note_front_is_target(&mut payload, worker);
    Ok(payload)
}

#[cfg(not(target_os = "macos"))]
fn idle_ms(_worker: &Worker) -> Result<Value, Failure> {
    Err("idle detection is only supported on macOS".into())
}

/// Whether the window the person is working in is the TARGET's.
///
/// The difference between a person busy in this application and a person busy
/// somewhere else, which is what decides whether an action that takes no pointer has
/// to wait for them at all. Absent when nothing is bound, and absent when the window
/// server would not say — never invented as `false`, which is a claim.
///
/// Gated with its one caller: `idle_ms` reads a macOS-only idle counter, so on a
/// build without one there is no reply for this to ride on.
#[cfg(target_os = "macos")]
fn note_front_is_target(payload: &mut Value, worker: &Worker) {
    let (Some(target), Some(object)) = (worker.target.as_ref(), payload.as_object_mut()) else {
        return;
    };

    if let Some(front) = target::front_is_target(target, &worker.windows) {
        object.insert("front_is_target".to_string(), json!(front));
    }
}

/// Block until the human has been idle for `idle_ms` (default 1000), bounded by
/// `timeout_ms` (default 3000, capped at `MAX_WAIT_FOR_IDLE_MS`). Returns
/// `idle: true` if the quiet window was reached, `idle: false` if it timed out with
/// the human still active. Reuses the `wait_for_change` bounded-poll idiom so a
/// consumer can schedule input into a human-idle gap.
#[cfg(target_os = "macos")]
fn wait_for_idle(req: &Value, gate: &Gate) -> Result<Value, Failure> {
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
        gate.sleep(poll_ms)
            .map_err(|refusal| refusal.code().to_string())?;
    }
}

#[cfg(not(target_os = "macos"))]
fn wait_for_idle(_req: &Value, _gate: &Gate) -> Result<Value, Failure> {
    Err("idle detection is only supported on macOS".into())
}

/// Paste `text` via the clipboard + the platform paste chord — fast and
/// unicode-safe for long strings that char-by-char typing would stall on.
fn paste(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let req = &request.body;
    let text = req
        .get("text")
        .and_then(Value::as_str)
        .ok_or("missing text")?;

    let view = view(request, gate, worker)?;
    ready_for_keys(worker, None)?;
    // A local for the same reason as `click`: enigo's pacing runs on its drop, and
    // the clipboard handle lived this long too.
    let mut platform = Gated::new(gate, desktop(worker)?);
    paste_seq(&mut platform, text)?;
    gate.input_complete();

    post(request, Some(view), gate, worker, nothing_named())
}

/// Let the pasteboard write settle before the paste keystroke.
const PASTE_SETTLE_MS: u64 = 50;

/// The paste itself, over the injected platform. `take_clipboard` saves the user's
/// text and arms its restore in one call, and the guard runs that restore — and
/// lifts the modifier — on every exit. A failure around the keystroke used to
/// return with the modifier down and the user's clipboard still overwritten.
fn paste_seq<P: held::Platform>(platform: &mut P, text: &str) -> Result<(), String> {
    held::guarded(platform, |input| {
        input.take_clipboard(text)?;
        input.sleep(PASTE_SETTLE_MS)?;
        input.press_keys(&[paste_modifier()])?;
        input.click_key(Key::Unicode('v'))
    })
}

#[cfg(target_os = "macos")]
fn paste_modifier() -> Key {
    Key::Meta
}

#[cfg(not(target_os = "macos"))]
fn paste_modifier() -> Key {
    Key::Control
}

// --- accessibility actions (protocol 9) --------------------------------------

/// Press the control the caller named, through the accessibility API: no pointer
/// moves, nothing can miss, and whatever is in front stays in front.
///
/// Through the gate like every other action, so pause, generations and
/// `mutation_seq` hold with no second path to the machine — but through
/// `dispatch_message`, not `dispatch`: one accessibility message can take the
/// whole one-second messaging timeout, and holding the gate's mutex across it
/// would leave a pause landing there unacknowledged until the wedged application
/// answered. That is slice 2's lesson, and it applies to a press exactly as it
/// applied to a long `type`.
fn press(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let observation = observed(request, worker)?;
    // Resolved BEFORE the control, so a request naming a window that is gone is
    // refused for that rather than for a reference that went with it — and so the
    // badge check runs before anything is dispatched.
    let view = view(request, gate, worker)?;
    let entry = element_in(&observation, request)?;
    let since = gate.now_ms();
    let facts = revalidate(&worker.ax, &observation, entry, Capability::Press)?;

    let handle = entry.element.handle();
    let aimed = aimed_point(request, &facts);
    let before = worker.ax.frontmost_pid();
    let timeout_ms = write_timeout(gate, since);

    let outcome = gate
        .dispatch_message(
            || worker.ax.perform(handle, ax::PRESS, timeout_ms),
            went_out,
        )
        .map_err(barred)?;
    gate.input_complete();
    note_ax(gate, worker, before);
    outcome.map_err(ax_failure)?;

    // A successful accessibility return is a DISPATCH result, not an effect: the
    // control took the message, and what it did with it is for the caller to look
    // at. Saying `verified` here would be the one lie this receipt exists to
    // prevent.
    gate.observed_effect(wire::Effect::NotObserved);

    let acted = Acted {
        named: Some(&observation),
        executed: aimed,
        element: Some(entry),
    };
    post(request, Some(view), gate, worker, acted)
}

/// Set the control's value, then read it back — which is the only thing in this
/// build that earns `effect: verified`.
///
/// Through `dispatch_message` for the same reason `press` is: one message, outside
/// the gate's mutex, so a pause landing while a slow application is thinking is
/// still answered — and marked as dispatched only if it really went out.
fn set_value(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let value = request
        .body
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Failure::new(
                "invalid_argument",
                "set_value needs a string value: the text to put in the control".to_string(),
            )
        })?
        .to_string();

    let observation = observed(request, worker)?;
    let view = view(request, gate, worker)?;
    let entry = element_in(&observation, request)?;
    let since = gate.now_ms();
    let facts = revalidate(&worker.ax, &observation, entry, Capability::SetValue)?;

    // The revalidation is the fresher read: a field can turn secure after the walk
    // that listed it.
    let (handle, secure) = (entry.element.handle(), entry.secure || facts.secure);
    let aimed = aimed_point(request, &facts);
    let before = worker.ax.frontmost_pid();
    let timeout_ms = write_timeout(gate, since);

    let outcome = gate
        .dispatch_message(|| worker.ax.set_value(handle, &value, timeout_ms), went_out)
        .map_err(barred)?;
    gate.input_complete();
    note_ax(gate, worker, before);
    outcome.map_err(ax_failure)?;

    // Read it back. Equal is the one thing that earns `verified`; a secure field
    // is not read at all — it reads back masked, so it could never verify, and
    // not asking is a stronger guarantee than asking and discarding.
    let read_back = if secure {
        None
    } else {
        read_back(gate, &worker.ax, handle, &value)
    };
    let verified = !secure && read_back.as_deref() == Some(value.as_str());
    gate.observed_effect(if verified {
        wire::Effect::Verified
    } else {
        wire::Effect::NotObserved
    });

    let acted = Acted {
        named: Some(&observation),
        executed: aimed,
        element: Some(entry),
    };
    let mut payload = post(request, Some(view), gate, worker, acted)?;

    if let Some(object) = payload.as_object_mut() {
        object.insert("verified".to_string(), json!(verified));
        // What the field holds NOW, so a caller that did not verify can see what
        // it got instead. Withheld for a secure field, whose value is never read.
        if let (false, Some(read_back)) = (secure, read_back) {
            object.insert("value".to_string(), json!(ax::bounded_value(read_back)));
        }
    }

    Ok(payload)
}

/// A verb on a reference (protocol 12): `focus`, `select`, `toggle`, `expand`,
/// `collapse`, `scroll_to`, and `type` by reference.
///
/// The control is re-validated, then its route for this verb read again and its
/// state read first (`semantic::plan`); a control with no route is refused and
/// never clicked instead, and one already in the state asked for is sent
/// nothing and says so. Otherwise one message, through `dispatch_message` as
/// `press` does, and the state or value read back.
fn verb_on(
    request: &wire::Request,
    gate: &Gate,
    worker: &mut Worker,
    verb: semantic::Verb,
) -> Result<Value, Failure> {
    let observation = observed(request, worker)?;
    let view = view(request, gate, worker)?;
    let entry = element_in(&observation, request)?;
    let since = gate.now_ms();
    let facts = present(&worker.ax, &observation, entry)?;
    let listed_by = observation
        .elements
        .as_deref()
        .map(|elements| elements.pid)
        .ok_or_else(|| Failure::from("stale_element"))?;

    let handle = entry.element.handle();
    // The revalidation is the fresher read: a field can turn secure after the walk
    // that listed it.
    let secure = entry.secure || facts.secure;
    let plan = semantic::plan(&worker.ax, handle, listed_by, &facts, secure, &verb)?;
    let acted = Acted {
        named: Some(&observation),
        executed: aimed_point(request, &facts),
        element: Some(entry),
    };

    if plan.already() {
        gate.used_input_method(wire::InputMethod::Ax);
        noted(gate, &semantic::held(&plan), plan.state.is_some());
        return post(request, Some(view), gate, worker, acted);
    }

    let before = worker.ax.frontmost_pid();
    let timeout_ms = write_timeout(gate, since);
    let outcome = gate
        .dispatch_message(
            || semantic::send(&worker.ax, handle, &plan, timeout_ms),
            went_out,
        )
        .map_err(barred)?;
    gate.input_complete();
    note_ax(gate, worker, before);
    outcome.map_err(ax_failure)?;

    // Infallible, like every step between the latch and the check: a pause ends
    // its polling and the last read is the answer.
    let judged = semantic::judge(gate, &worker.ax, handle, &plan);
    noted(gate, &judged, plan.state.is_some());
    let mut payload = post(request, Some(view), gate, worker, acted)?;

    // What a field holds after `type`, never for a secure one.
    if let (Some(object), Some(value)) = (payload.as_object_mut(), judged.value) {
        object.insert("value".to_string(), json!(value));
    }
    Ok(payload)
}

/// What a verb's read-back proved, for its receipt: the effect, and for a verb
/// that reads a state, the state before and after.
fn noted(gate: &Gate, judged: &semantic::Judged, reads_a_state: bool) {
    gate.observed_effect(judged.effect);
    if reads_a_state {
        gate.observed_state(wire::StateChange {
            already: judged.already,
            before: judged.state_before,
            after: judged.state_after,
        });
    }
}

/// Where the control is, for the check image's executed-point marker.
///
/// Wanted only when there is going to be an image to draw it on: an accessibility
/// action aims at a name, not a place. Taken from the read that revalidated the
/// control a moment ago, so it costs no message. Best effort — a control that will
/// not say where it is still gets its action.
fn aimed_point(request: &wire::Request, facts: &ax::Facts) -> Option<(i32, i32)> {
    if request.check != wire::Check::Image {
        return None;
    }

    facts.frame.map(|bounds| {
        let (x, y) = bounds.centre();
        (x.round() as i32, y.round() as i32)
    })
}

/// The messaging timeout for this action's one write (M2): half of what is left of
/// its accessibility budget, counted from the read that revalidated the control.
fn write_timeout(gate: &Gate, since: u64) -> u64 {
    let spent = gate.now_ms().saturating_sub(since);
    ax::write_timeout_ms(ax::WRITE_BUDGET_MS.saturating_sub(spent))
}

/// How often a written value is read back, and for how long (M6). Some fields show
/// a write a moment after it — a formatter, a binding that updates on the next turn
/// of the run loop — so one read straight after it called a write that worked
/// unobserved. At most `READ_BACK_CAP_MS / READ_BACK_POLL_MS` reads after the
/// first; past the cap the receipt reports what the field holds instead.
const READ_BACK_POLL_MS: u64 = 50;
const READ_BACK_CAP_MS: u64 = 400;

/// What a written field holds now: read until it shows `wanted` or the cap runs
/// out. It never writes again.
///
/// Infallible on purpose, because it runs between the input latch and the check,
/// where nothing may fail (the check is the last thing an action does). The waits
/// go through the gate, so a pause during one ends the read-back at once: what the
/// field held at the last read is the answer — `not_observed` unless it already
/// matched — and the cancellation stays with the gate for the check that follows.
fn read_back(gate: &Gate, ax: &Rc<dyn ax::Ax>, handle: ax::Handle, wanted: &str) -> Option<String> {
    // The value alone: one message a look, not a node's whole facts.
    let mut held = ax.value(handle);
    let mut waited = 0;

    while held.as_deref() != Some(wanted) && waited < READ_BACK_CAP_MS {
        let started = gate.now_ms();
        let paused = gate.sleep(READ_BACK_POLL_MS).is_err();
        gate.record(Phase::Settle, gate.now_ms().saturating_sub(started));
        if paused {
            break;
        }
        waited += READ_BACK_POLL_MS;
        held = ax.value(handle);
    }

    held
}

/// What only an accessibility action knows about its own receipt: the method that
/// carried it, and whether the application took the foreground while it ran.
///
/// The foreground is read before and after. A change is reported only when BOTH
/// readings answered — a platform that will not say must never be made to look
/// like a change, and a `false` here is a claim the live check is meant to test.
fn note_ax(gate: &Gate, worker: &Worker, before: Option<i32>) {
    gate.used_input_method(wire::InputMethod::Ax);

    // Only when BOTH readings answered. A platform that would not say leaves the
    // field off the wire entirely: publishing `false` there would be this build
    // asserting the background contract held, on evidence it does not have — and
    // the support matrix is filled in from exactly that field.
    if let (Some(before), Some(after)) = (before, worker.ax.frontmost_pid()) {
        gate.note_foreground(before != after);
    }
}

// --- the check: what an action shows for itself (protocol 10) ----------------

/// How long a settle may go on, and how many looks it may take to get there. BOTH
/// bind: the wall clock is the promise a caller's own deadline is built on, and the
/// sample count is the promise that a machine whose captures are instant does not
/// spin.
const SETTLE_CAP_MS: u64 = 1_500;
const MAX_SETTLE_SAMPLES: u32 = 30;

/// The FLOOR between two looks, not a delay added to each one. A capture that took
/// 200 ms has already provided the gap, so only the shortfall is slept; a machine
/// whose captures are instant is paced to this.
const SETTLE_POLL_MS: u64 = 50;

/// The shortest sleep between two looks. A look is never immediately followed by
/// another, whatever the arithmetic says.
const SETTLE_MIN_POLL_MS: u64 = 5;

/// How long after the input a view has to be quiet before "it has not moved" is
/// believed.
///
/// Two equal samples alone are not enough, and this is the hole: the first sample
/// is taken as soon as the input is complete, so an application that only starts
/// repainting a poll or two later hands back two identical samples of the view as
/// it was BEFORE the action — `stable`, `changed: false`, and the misleading check
/// this slice exists to remove. So equality settles the view only once a change has
/// been seen, or once the view has been this quiet since the input went out. An
/// action that really changes nothing therefore costs this window, once.
const SETTLE_QUIET_MS: u64 = 300;

/// The check image's encoding, for the crop of a view. The full display stays PNG:
/// these are the encodings a caller already got for each of the two, and nothing
/// about image quality changes in this slice.
const CHECK_JPEG_QUALITY: u8 = 85;

/// What the action did, as far as its check is concerned: the view it was aimed
/// in, where it landed, and the control it named when it named one.
struct Acted<'a> {
    /// The image or listing the action's target was read from. `None` for `type`,
    /// `key` and `paste`, which act at the focus and name nothing.
    named: Option<&'a Observation>,
    /// The point the action really executed at, in global logical points.
    executed: Option<(i32, i32)>,
    /// The control it acted on, when it named one — the only thing a semantic
    /// check has to read again.
    element: Option<&'a ax::Entry>,
}

/// The frame a settle stopped on, and what it proved.
struct Settled {
    frame: image::RgbaImage,
    hash: ViewHash,
    settle: wire::Settle,
}

/// The evidence this action brings back, taken AFTER the input and reported on the
/// receipt.
///
/// The display is a `None` for an accessibility action and resolved here only if an
/// image is really wanted: a press needs no display, so refusing one because the
/// screen is asleep would fail an action that would have worked — and a press
/// through the AX API is exactly the action that still works when a capture does
/// not.
///
/// Completion is latched before any of this (`input_complete`), so evidence that
/// could not be obtained never turns `sent` into anything else.
fn post(
    request: &wire::Request,
    view: Option<View>,
    gate: &Gate,
    worker: &mut Worker,
    acted: Acted<'_>,
) -> Result<Value, Failure> {
    // The fence is armed the instant the input is complete and disarmed when the
    // action ends, so every sample a check takes is held to it and nothing later is.
    // A view with no window has no fence: a display capture is taken now, by
    // definition after the input, and has no sequence to be beyond.
    let _fenced = view
        .as_ref()
        .and_then(|view| view.bound.as_ref())
        .map(|live| {
            // On the MACH base, because that is the base the frame's display
            // time arrives on and nothing converts between the two.
            live.frames.arm(gate.clock().now_mach(), live.seq);
            Fenced(live.frames.clone())
        });

    match request.check {
        wire::Check::None => {
            gate.observed_check(wire::Checked {
                kind: wire::Check::None,
                settle: None,
                changed: None,
                until: None,
            });
            Ok(json!({ "ok": true }))
        }

        wire::Check::Semantic => match &request.until {
            None => element_after(gate, worker, acted.element),
            Some(until) => waited_after(request, until, gate, worker, acted.element),
        },

        wire::Check::Image => check_image(request, view, gate, worker, acted),
    }
}

/// The armed fence, disarmed on EVERY path out of the check — the settle finishing,
/// a capture failing, a pause cancelling it, an early `?`. A fence left armed would
/// hold the next action to the last one's dispatch.
struct Fenced(Rc<target::TargetFrames>);

impl Drop for Fenced {
    fn drop(&mut self) {
        self.0.disarm();
    }
}

/// The view the model acted in, settled and drawn on.
///
/// The rectangle is the one the action's own observation covers — its crop, or the
/// rectangle a listing was read over — mapped back through the transform that
/// observation was made with, so a check of a zoomed action is that zoom rather
/// than a whole screen the model has to find its target in again. An action that
/// named nothing gets the display.
fn check_image(
    request: &wire::Request,
    view: Option<View>,
    gate: &Gate,
    worker: &mut Worker,
    acted: Acted<'_>,
) -> Result<Value, Failure> {
    let view = view.unwrap_or_else(|| named_display(request, worker));

    // The transform before the first sample, because the rectangle the settle
    // watches has to be the rectangle that is then encoded — and a display's
    // transform is measured from a frame, so this is the one place a check may take
    // a capture it does not keep (only on a display this process has never
    // measured). A bound window's came with the frame it was read from.
    let geom = view.geometry(gate, worker)?;
    let display = &view.display(worker)?;
    let region = check_view(&geom, acted.named);
    let before = acted.named.and_then(|observation| observation.view_hash);
    let settled = settle_view(display, &geom, &region, before, gate, &view.frames, worker)?;

    // Evidence about the VIEW, not about the action: did what the model looked at
    // change since the image it acted on. A listing carries no hash, and saying
    // `false` there would answer a question nobody has evidence for.
    let changed = before.and_then(|before| settled.hash.changed_from(&before));

    let overlays = Overlays {
        rulers: true,
        marks: false,
        annotate: acted
            .executed
            .map(|(lx, ly)| Annotate::Logical(lx as f32, ly as f32)),
    };

    let mut payload = capture_payload_encoded(
        display,
        geom,
        &settled.frame,
        Rendering {
            requested: Requested::Exactly(region),
            jpeg_quality: check_quality(&region, &geom),
            overlays,
        },
        gate,
        worker,
    )?;
    note_target(&mut payload, worker, view.bound.is_some())?;

    gate.observed_check(wire::Checked {
        kind: wire::Check::Image,
        settle: Some(settled.settle),
        changed,
        until: None,
    });

    Ok(payload)
}

/// The display a request names, as the view of a check whose action named none.
fn named_display(request: &wire::Request, worker: &Worker) -> View {
    View {
        on: None,
        index: display_index(&request.body),
        geometry: None,
        frames: worker.frames.clone(),
        bound: None,
    }
}

/// The rectangle a check image covers: the whole of the observation the action was
/// aimed in, mapped back through the transform THAT image was made with.
///
/// Through `rect_through`, whose corners are edges and not pixel centres: the same
/// rectangle mapped again and again must stay the same rectangle, and the centre
/// convention made the view creep inwards over a run of checks.
fn check_view(geom: &Geometry, named: Option<&Observation>) -> Region {
    let Some(observation) = named else {
        return Region::full(geom);
    };

    let whole = Region {
        x: 0.0,
        y: 0.0,
        w: observation.sent_w as f64,
        h: observation.sent_h as f64,
    };

    geometry::rect_through(&observation.geometry, &observation.region, &whole, geom)
}

/// PNG for the whole display, JPEG for a crop — decided by the rectangle itself, so
/// an action aimed in a full-display image and one that named no image at all are
/// encoded the same way because they show the same thing.
fn check_quality(region: &Region, geom: &Geometry) -> Option<u8> {
    if same_rect(region, &Region::full(geom)) {
        None
    } else {
        Some(CHECK_JPEG_QUALITY)
    }
}

/// Two rectangles of sent pixels, compared as the whole pixels they are drawn in.
fn same_rect(a: &Region, b: &Region) -> bool {
    let round = |region: &Region| {
        (
            region.x.round() as i64,
            region.y.round() as i64,
            region.w.round() as i64,
            region.h.round() as i64,
        )
    };

    round(a) == round(b)
}

/// Poll the crop until two consecutive samples are identical, then hand back the
/// sample that proved it.
///
/// The frame that proved the view stable is the one encoded: capturing again after
/// it would return a frame nobody has looked at, which is the whole defect this
/// replaces. The cap is reported, never hidden — a caret, a spinner or a playing
/// video reaches it every time, and `timeout` is an observation state rather than a
/// reason to send the input again.
///
/// Sleeps go through the gate, so a pause during a settle returns at once: the
/// action then answers `cancelled` with the input it dispatched reported truthfully
/// and no evidence claimed.
fn settle_view(
    display: &Display,
    geom: &Geometry,
    region: &Region,
    before: Option<ViewHash>,
    gate: &Gate,
    frames: &Rc<dyn Frames>,
    _worker: &Worker,
) -> Result<Settled, Failure> {
    let crop = crop_rect(geom, region);
    let started = gate.now_ms();
    // Counted from the INPUT and not from the first look, because a display has to
    // be resolved and may have to be measured in between. An action that never
    // latched its completion dispatched nothing to wait on, so its window starts
    // here.
    let quiet_until = gate.input_completed_at().unwrap_or(started) + SETTLE_QUIET_MS;

    let mut baseline: Option<ViewHash> = None;
    let mut previous: Option<ViewHash> = None;
    let mut seen_change = false;
    let mut samples: u32 = 0;
    let mut looked_at = started;

    loop {
        samples += 1;
        if samples > 1 {
            pace(gate, looked_at)?;
        }

        looked_at = gate.now_ms();
        let frame = take_frame(display, gate, frames)?;
        let hash = sample_hash(&frame, &crop, gate);

        // What a change is measured FROM: the image the caller acted on when this
        // sample is comparable with it — which is what catches a repaint that had
        // already begun before the first look — and this first look otherwise,
        // which is all an action naming no image has. Decided once.
        let base = *baseline.get_or_insert(match before {
            Some(before) if hash.changed_from(&before).is_some() => before,
            _ => hash,
        });
        seen_change |= hash.changed_from(&base) == Some(true);

        let settled = previous == Some(hash) && (seen_change || gate.now_ms() >= quiet_until);
        let spent = gate.now_ms().saturating_sub(started);

        if settled || spent >= SETTLE_CAP_MS || samples >= MAX_SETTLE_SAMPLES {
            return Ok(Settled {
                frame,
                hash,
                settle: if settled {
                    wire::Settle::Stable
                } else {
                    wire::Settle::Timeout
                },
            });
        }

        previous = Some(hash);
    }
}

/// Wait out the floor between two looks, and no longer: a capture that took 200 ms
/// has already provided the gap, and sleeping a further 50 would be this process
/// adding delay to a check it is meant to be shortening.
fn pace(gate: &Gate, looked_at: u64) -> Result<(), Failure> {
    let since = gate.now_ms().saturating_sub(looked_at);
    let waited = gate.now_ms();

    gate.sleep(SETTLE_POLL_MS.saturating_sub(since).max(SETTLE_MIN_POLL_MS))
        .map_err(|refusal| refusal.code().to_string())?;
    gate.record(Phase::Settle, gate.now_ms().saturating_sub(waited));

    Ok(())
}

/// The control the action named, read again: what it is, what it calls itself,
/// whether it will take input, and what it holds.
///
/// Never described as visual evidence and never a capture. A secure field's value
/// is not read here any more than it is read anywhere else — it reads back masked,
/// and not asking is a stronger guarantee than asking and discarding. Its LABEL is
/// read like any other control's: what a password field calls itself is not what it
/// holds.
///
/// `label` is here so a consumer can say WHICH control it re-read without keeping
/// its own copy of a listing, and it is bounded and collapsed to one line at the
/// source, exactly as a listing publishes it.
///
/// `present: false` is a control that no longer answers at all, which is itself
/// worth knowing: a dialog that closed, a row that went away.
fn element_after(
    gate: &Gate,
    worker: &Worker,
    element: Option<&ax::Entry>,
) -> Result<Value, Failure> {
    let entry = element.ok_or_else(|| {
        Failure::new(
            "check_unsupported",
            "a semantic check re-reads the control the action named, and this action named none"
                .to_string(),
        )
    })?;

    let after = reread(&worker.ax, entry);
    gate.observed_check(wire::Checked {
        kind: wire::Check::Semantic,
        settle: None,
        changed: None,
        until: None,
    });

    Ok(json!({ "ok": true, "element_after": after }))
}

/// The control an action named, read again as `element_after` publishes it.
fn reread(ax: &Rc<dyn ax::Ax>, entry: &ax::Entry) -> Value {
    // A control that does not answer its role does not answer anything else either,
    // and is reported as not there.
    let handle = entry.element.handle();
    match ax.facts(handle) {
        Ok(facts) if facts.role.is_some() => {
            // What it says now decides as well as what the listing said: a field
            // can turn secure after the walk that listed it.
            let secure = entry.secure || facts.secure;
            described_after(ax, handle, secure, facts)
        }
        _not_there => json!({ "present": false }),
    }
}

/// A semantic check that waits on the tree (protocol 12): the condition was read
/// before the input, so one that already held says so and is not waited on;
/// otherwise it is looked at until it holds or its timeout passes. An action that
/// named a control also gets that control read again.
fn waited_after(
    request: &wire::Request,
    until: &semantic::Until,
    gate: &Gate,
    worker: &mut Worker,
    element: Option<&ax::Entry>,
) -> Result<Value, Failure> {
    let seen = match worker.until_held.take() {
        Some(true) => wire::UntilSeen {
            outcome: wire::UntilOutcome::HeldBefore,
            looks: 0,
            waited_ms: 0,
            deadline_cut: false,
        },
        _ => {
            let (scene, named) = scene_for(request, worker)?;
            let (waited, cut) = waited_on(gate, worker, &scene, named.as_deref(), until)?;
            wire::UntilSeen {
                outcome: if waited.met {
                    wire::UntilOutcome::Met
                } else {
                    wire::UntilOutcome::NotMet
                },
                looks: waited.looks,
                waited_ms: waited.waited_ms,
                deadline_cut: cut,
            }
        }
    };

    let mut payload = json!({ "ok": true });
    if let (Some(object), Some(entry)) = (payload.as_object_mut(), element) {
        object.insert("element_after".to_string(), reread(&worker.ax, entry));
    }
    gate.observed_check(wire::Checked {
        kind: wire::Check::Semantic,
        settle: None,
        changed: None,
        until: Some(seen),
    });
    Ok(payload)
}

/// Whether an action's condition already holds, read BEFORE its input (§5.2), in
/// the application and listing the request names.
fn held_before(
    request: &wire::Request,
    until: &semantic::Until,
    gate: &Gate,
    worker: &Worker,
) -> Result<bool, Failure> {
    let (scene, named) = scene_for(request, worker)?;
    let clock = gate.clock();
    let at = semantic::Looking {
        ax: &worker.ax,
        scene,
        named: named.as_deref(),
        clock: clock.as_ref(),
    };
    semantic::held_before(&at, until, gate.now_ms(), worker.cutoff_ms)
}

/// The wait on a condition (§5), in the application and listing the request
/// names, held to the request's own deadline.
fn waited_on(
    gate: &Gate,
    worker: &Worker,
    scene: &semantic::Scene,
    named: Option<&ax::Elements>,
    until: &semantic::Until,
) -> Result<(semantic::Waited, bool), Failure> {
    let clock = gate.clock();
    let at = semantic::Looking {
        ax: &worker.ax,
        scene: *scene,
        named,
        clock: clock.as_ref(),
    };
    semantic::wait_until(gate, &at, until, worker.cutoff_ms)
}

/// The application a condition reads, and the listing its references name: the
/// bound window's when the request names one, else the listing's it names, else
/// the one in front.
fn scene_for(
    request: &wire::Request,
    worker: &Worker,
) -> Result<(semantic::Scene, Option<Rc<ax::Elements>>), Failure> {
    let named = match request.observation_id.as_deref() {
        Some(id) => worker
            .observations
            .resolve(id)
            .map_err(refused)?
            .elements
            .clone(),
        None => None,
    };
    if request.target_id.is_some() {
        let target = worker
            .target
            .as_ref()
            .ok_or_else(|| Failure::from("target_unavailable"))?;
        let scene = semantic::Scene {
            pid: target.pid,
            window: Some(target::ax_root(target)?),
        };
        return Ok((scene, named));
    }
    let pid = match named.as_deref() {
        Some(elements) => Some(elements.pid),
        None => worker.ax.frontmost_pid(),
    };
    let pid = pid.ok_or_else(|| {
        Failure::new(
            "scope_not_found",
            "no application is in front to read, and the request names no listing".to_string(),
        )
    })?;
    Ok((semantic::Scene { pid, window: None }, named))
}

/// Wait on the tree (protocol 12). Read-only: it sends nothing however long it
/// looks. Not met by the deadline is an answer, not a failure; met on a condition
/// that names what matched lists it, with references, in a new observation.
fn wait_for(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let until = request.until.as_ref().ok_or_else(|| {
        Failure::new(
            "invalid_argument",
            "wait_for needs until: the condition to wait for".to_string(),
        )
    })?;
    let (scene, named) = scene_for(request, worker)?;
    let (waited, cut) = waited_on(gate, worker, &scene, named.as_deref(), until)?;

    let mut payload = json!({
        "ok": true,
        "met": waited.met,
        "looks": waited.looks,
        "waited_ms": waited.waited_ms,
    });
    if let (Some(object), true) = (payload.as_object_mut(), cut) {
        object.insert("deadline_cut".to_string(), json!(true));
    }
    let Some(found) = waited.found else {
        return Ok(payload);
    };

    let (display, geom, region) = listing_frame(request, gate, worker)?;
    let owner = worker
        .ax
        .process_started_at(scene.pid)
        .map(|started_at| (scene.pid, started_at));
    let view = Viewport {
        geom: &geom,
        region: &region,
    };
    let (placed, _hidden) = in_view(found.nodes, view.geom, view.region);
    let (items, entries) = listed_nodes(placed, owner.is_some(), 0, None, &view);
    merge_into(&mut payload, json!({ "elements": items }));
    let observation = mint_semantic(worker, &display, geom, region, retained(owner, entries));
    name_observation(&mut payload, &observation);
    Ok(payload)
}

/// A control that is still there, as `element_after` publishes it: its facts, and
/// its value from a read of its own — never asked of a secure field.
fn described_after(
    ax: &Rc<dyn ax::Ax>,
    handle: ax::Handle,
    secure: bool,
    facts: ax::Facts,
) -> Value {
    let native = facts.role.unwrap_or_default();
    let kind = roles::normalise(&native, facts.subrole.as_deref());
    let mut after = json!({
        "present": true,
        "role": kind,
        "native_role": native,
        "enabled": facts.enabled,
    });
    let value = if secure { None } else { ax.value(handle) };
    let on = roles::has_check_state(kind) || kind == "disclosure";
    // States that will not read are left out, as a listing leaves them out.
    let states = ax.states(handle, on).unwrap_or_default();

    if let Some(object) = after.as_object_mut() {
        if let Some(label) = facts.label {
            object.insert("label".to_string(), json!(label));
        }
        if let Some(value) = value {
            object.insert("value".to_string(), json!(ax::bounded_value(value)));
        }
        if secure {
            object.insert("secure".to_string(), json!(true));
        }
        insert_states(object, kind, &states);
    }
    after
}

// --- protocol 12: listings by scope, and what is open --------------------------

/// One listing of a scope (protocol 12), as the wire publishes it: the scope it
/// read, its controls and the references behind them, and for a skeleton what its
/// cuts hide that could not be listed.
fn scoped_listing(
    ax: &Rc<dyn ax::Ax>,
    scene: &semantic::Scene,
    owner: Option<(i32, u64)>,
    listing: &semantic::Listing,
    view: &Viewport<'_>,
    gate: &Gate,
) -> Result<(Value, Option<Rc<ax::Elements>>), Failure> {
    let scope = listing
        .scope
        .unwrap_or_else(|| semantic::Scope::default_for(scene));
    let clock = gate.clock();
    let before = ax.messages();
    let deadline = gate.now_ms().saturating_add(tree::WALK_BUDGET_MS);
    let found = semantic::walk_scope(ax, scene, scope, &listing.shape, clock.as_ref(), deadline)?;
    let messages = ax.messages() - before;

    let names = owner.is_some();
    let (mut payload, entries) = published(found, messages, &listing.shape, names, 0, None, view);
    merge_into(&mut payload, json!({ "scope": scope.as_str() }));
    Ok((payload, retained(owner, entries)))
}

/// The application a scoped listing reads: the bound window's, else the one that
/// owns the window in view, as protocol 11 read it.
fn listing_scene(
    view: &View,
    geom: &Geometry,
    region: &Region,
    worker: &Worker,
) -> Result<semantic::Scene, Failure> {
    if view.bound.is_some() {
        let target = worker
            .target
            .as_ref()
            .ok_or_else(|| Failure::from("target_unavailable"))?;
        return Ok(semantic::Scene {
            pid: target.pid,
            window: Some(target::ax_root(target)?),
        });
    }
    match view_target(&worker.ax, geom, region)? {
        Ok(app) => Ok(semantic::Scene {
            pid: app.pid,
            window: None,
        }),
        Err(note) => Err(Failure::new("scope_not_found", note)),
    }
}

/// The dialogs, alerts, menus and popups open once an action that sent input has
/// run, for its receipt. `None` is "not looked": nothing was sent, the action earns
/// no receipt, this build has no accessibility API, or there is no application to
/// look in.
fn surfaces_after(
    request: &wire::Request,
    done: &gate::Dispatched,
    worker: &Worker,
) -> Option<Vec<wire::Surface>> {
    if !done.posted || !worker.accessibility || !wire::carries_mutation_seq(&request.action) {
        return None;
    }
    let (scene, _named) = scene_for(request, worker).ok()?;
    let open = semantic::surfaces(&worker.ax, scene.pid);
    Some(
        open.into_iter()
            .map(|surface| wire::Surface {
                kind: surface.kind,
                native_kind: surface.native_kind,
                label: surface.label,
            })
            .collect(),
    )
}

/// Where a listing that answers a request is drawn: the named listing's own view
/// when the request names one and no region, so the new click points are in the
/// same pixels; otherwise the view an `elements` request reads.
fn listing_frame(
    request: &wire::Request,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<(Display, Geometry, Region), Failure> {
    if let (Some(id), None) = (
        request.observation_id.as_deref(),
        request.body.get("region"),
    ) {
        let named = worker.observations.resolve(id).map_err(refused)?;
        let display = Display {
            facts: named.facts,
            id: named.display_id,
        };
        return Ok((display, named.geometry, named.region));
    }
    let view = view(request, gate, worker)?;
    drawn_in(&view, request, gate, worker)
}

/// The frame a listing's click points are drawn in: the view's display, or no
/// frame at all on a session that places nothing, where a listing needs no
/// display and no capture. A bound window always has its frame.
fn drawn_in(
    view: &View,
    request: &wire::Request,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<(Display, Geometry, Region), Failure> {
    if view.bound.is_some() || worker.ax.places() {
        let geom = view.geometry(gate, worker)?;
        let display = view.display(worker)?;
        let region = viewing_region(request, &geom, worker)?;
        return Ok((display, geom, region));
    }
    if request
        .body
        .get("region")
        .is_some_and(|region| !region.is_null())
    {
        return Err(Failure::new(
            "invalid_argument",
            "this session gives no screen positions, so there is no image a region could \
             be read in; list without one"
                .to_string(),
        ));
    }
    Ok(no_frame())
}

/// The frame of a session that places nothing: no display, no pixels, and a
/// region with nothing in it, so every point named against it is outside.
fn no_frame() -> (Display, Geometry, Region) {
    let facts = MonitorFacts {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        scale_factor: 1.0,
    };
    let geometry = Geometry {
        phys_w: 0,
        phys_h: 0,
        logical_w: 0.0,
        logical_h: 0.0,
        origin_x: 0.0,
        origin_y: 0.0,
        scale_factor: 1.0,
    };
    let region = Region {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };
    (Display { facts, id: 0 }, geometry, region)
}

/// A listing's observation: no picture, so nothing for a later check to compare
/// its view against — `changed` is absent on such a check, never false.
fn mint_semantic(
    worker: &mut Worker,
    display: &Display,
    geometry: Geometry,
    region: Region,
    elements: Option<Rc<ax::Elements>>,
) -> Observation {
    // A frame with no pixels sends none, so no point can be read off it.
    let (sent_w, sent_h) = if geometry.phys_w == 0 {
        (0, 0)
    } else {
        crop_rect(&geometry, &region).sent_dims()
    };
    worker.observations.mint(observation::Minting {
        kind: Kind::Semantic,
        display_id: display.id,
        facts: display.facts,
        geometry,
        region,
        sent: (sent_w, sent_h),
        elements,
        view_hash: None,
    })
}

/// Drill into a control of a listing (protocol 12): its subtree, listed as `shape`
/// asks (a find, or a skeleton cut three levels below the root), with references
/// numbered after the listing's last, in a new observation that keeps every
/// reference of the listing outside that subtree. The root is re-validated first
/// (its process and its role): one that names something else now is
/// `stale_element`.
fn drill(
    request: &wire::Request,
    root: &str,
    shape: &tree::Shape,
    gate: &Gate,
    worker: &mut Worker,
) -> Result<Value, Failure> {
    let observation = observed(request, worker)?;
    let named = observation.elements.clone().ok_or_else(no_longer_there)?;
    let entry = named.get(root).ok_or_else(|| {
        Failure::new(
            "stale_element",
            format!(
                "{root} is not one of the {} controls that reply listed; take `elements` again",
                named.len()
            ),
        )
    })?;
    answering(&worker.ax, &observation, entry)?;

    let (display, geom, region) = listing_frame(request, gate, worker)?;
    let clock = gate.clock();
    let before = worker.ax.messages();
    let deadline = gate.now_ms().saturating_add(tree::WALK_BUDGET_MS);
    let handle = entry.element.handle();
    let found = tree::walk_handle(&worker.ax, handle, shape, clock.as_ref(), deadline);
    let messages = worker.ax.messages() - before;

    let view = Viewport {
        geom: &geom,
        region: &region,
    };
    let first = named.next_index();
    let (mut payload, entries) = published(found, messages, shape, true, first, Some(root), &view);
    merge_into(&mut payload, json!({ "root": root }));

    let mut held: Vec<Rc<ax::Entry>> = named
        .entries()
        .iter()
        .filter(|kept| !listing::inside(&named, kept, root))
        .cloned()
        .collect();
    held.extend(entries.into_iter().map(Rc::new));
    let elements = ax::Elements::shared(named.pid, named.started_at, held);
    let minted = mint_semantic(worker, &display, geom, region, Some(Rc::new(elements)));
    name_observation(&mut payload, &minted);
    Ok(payload)
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
/// point: the caller pastes it straight into `screenshot` and rides the EXISTING,
/// proven region transform. No second coordinate system is introduced, so this
/// cannot reintroduce the region-offset class of bug.
///
/// READ-ONLY: pure metadata and no input — but it hands out coordinates, so it
/// mints an observation, and on a display nothing has captured yet that costs one
/// frame to measure the transform those coordinates are in.
fn windows(req: &Value, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let display = target_display(req, worker)?;
    let geom = measured_geometry(&display, gate, worker)?;
    let full = Region::full(&geom);
    let mut listed = window_entries(&geom, &full, listed_windows(worker)?)?;
    listed.truncate(MAX_WINDOWS);

    let mut payload = json!({ "ok": true, "windows": listed });
    let observation = worker.observations.mint(observation::Minting {
        kind: Kind::Semantic,
        display_id: display.id,
        facts: display.facts,
        geometry: geom,
        region: full,
        sent: crop_rect(&geom, &full).sent_dims(),
        // A listing has no picture, so there is nothing for a later check to
        // compare its view against.
        view_hash: None,
        // A window is not a control: it has no accessibility reference to hand
        // out, and `elements` is what answers with those.
        elements: None,
    });
    name_observation(&mut payload, &observation);

    Ok(payload)
}

/// One window as the platform lists it, front-most first: its LOGICAL bounds
/// (`kCGWindowBounds` on macOS, root pixels on X11 — the unit `Geometry` uses).
struct Listed {
    id: u32,
    app: String,
    title: String,
    focused: bool,
    minimized: bool,
    bounds: (f64, f64, f64, f64),
}

/// The windows, front-most first: xcap's `z` grows toward the front.
#[cfg(target_os = "macos")]
fn listed_windows(_worker: &Worker) -> Result<Vec<Listed>, String> {
    let mut windows = xcap::Window::all().map_err(|e| format!("enumerate windows: {e}"))?;
    windows.sort_by_cached_key(|w| std::cmp::Reverse(w.z().unwrap_or(0)));
    Ok(windows
        .into_iter()
        .filter_map(|window| {
            Some(Listed {
                id: window.id().unwrap_or_default(),
                app: window.app_name().unwrap_or_default(),
                title: window.title().unwrap_or_default(),
                focused: window.is_focused().unwrap_or(false),
                minimized: window.is_minimized().unwrap_or(false),
                bounds: (
                    window.x().ok()? as f64,
                    window.y().ok()? as f64,
                    window.width().ok()? as f64,
                    window.height().ok()? as f64,
                ),
            })
        })
        .collect())
}

/// The windows the X11 window manager lists, front-most first.
#[cfg(target_os = "linux")]
fn listed_windows(worker: &Worker) -> Result<Vec<Listed>, String> {
    Ok(worker
        .pixels
        .windows()?
        .into_iter()
        .map(|window| Listed {
            id: window.id,
            app: window.app,
            title: window.title,
            focused: window.focused,
            minimized: window.minimized,
            bounds: (
                f64::from(window.x),
                f64::from(window.y),
                f64::from(window.width),
                f64::from(window.height),
            ),
        })
        .collect())
}

fn window_entries(
    geom: &Geometry,
    full: &Region,
    windows: Vec<Listed>,
) -> Result<Vec<Value>, String> {
    // Shell-layer windows (the Dock, the menu bar, floating overlays) are not
    // windows a caller can work IN — listing them invites zooming into a
    // phantom region (observed live: "Dock — region {0,0,1931,543}").
    let normal = shell_window_filter()?;

    let mut entries = Vec::new();
    for window in windows {
        if window.minimized {
            continue;
        }
        if let Some(normal) = &normal {
            if !normal.contains(&window.id) {
                continue;
            }
        }
        let (x, y, w, h) = window.bounds;
        if let Some(region) = logical_bounds_to_region(geom, full, x, y, w, h) {
            entries.push(json!({
                "id": window.id,
                "app": window.app,
                "title": window.title,
                "focused": window.focused,
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

/// A window's LOGICAL bounds (`kCGWindowBounds`, the unit `Geometry` uses)
/// converted into this display's sent-image space. Thin effectful shell; the
/// arithmetic is `logical_bounds_to_region` so it can be tested without a desktop.
#[cfg(target_os = "macos")]
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

/// Enumerate the interactive accessibility elements so the caller can target by
/// CONTROL rather than by raw pixels: what each one is, what it holds, what it can
/// do, where it is, and a reference that outlives the reply.
///
/// The click points are pixels in an image that is never sent, so this mints a
/// `semantic` observation and names it: the coordinates are in the same space a
/// `screenshot` of that region would be, and the model addresses them the same way
/// — and the references ride in the same observation, so they die with it.
fn elements(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let listing = semantic::listing(&request.body)?;
    if let Some(root) = listing.root.as_deref() {
        return drill(request, root, &listing.shape, gate, worker);
    }

    let view = view(request, gate, worker)?;
    let (display, geom, region) = drawn_in(&view, request, gate, worker)?;
    let display = &display;

    // A request with none of protocol 12's parameters reads exactly what protocol
    // 11 read, its settling for an application whose tree is still empty included.
    if listing != semantic::Listing::default() {
        let scene = listing_scene(&view, &geom, &region, worker)?;
        let owner = worker
            .ax
            .process_started_at(scene.pid)
            .map(|started_at| (scene.pid, started_at));
        let viewport = Viewport {
            geom: &geom,
            region: &region,
        };
        let (mut payload, listed) =
            scoped_listing(&worker.ax, &scene, owner, &listing, &viewport, gate)?;
        note_target(&mut payload, worker, view.bound.is_some())?;
        let observation = mint_semantic(worker, display, geom, region, listed);
        name_observation(&mut payload, &observation);
        return Ok(payload);
    }

    // On a bound window the walk starts at THAT window's accessibility element, so
    // a listing is the window's controls and not the application's. Without one
    // there is nothing to walk from, and walking the application instead would name
    // controls of windows the caller never bound.
    let root = match view.bound.as_ref() {
        None => None,
        Some(_live) => Some(target::ax_root(
            worker
                .target
                .as_ref()
                .ok_or_else(|| Failure::from("target_unavailable"))?,
        )?),
    };

    let (mut payload, listed) = match root {
        None => elements_for(&worker.ax, &geom, &region, gate)?,
        Some(root) => elements_rooted(&worker.ax, root, &geom, &region, gate, worker)?,
    };
    note_target(&mut payload, worker, view.bound.is_some())?;
    let observation = mint_semantic(worker, display, geom, region, listed);
    name_observation(&mut payload, &observation);

    Ok(payload)
}

/// The element list and the references behind it. Portable on purpose: only the
/// READ is per platform, so the shape a caller sees is written once.
fn elements_for(
    ax: &Rc<dyn ax::Ax>,
    geom: &Geometry,
    region: &Region,
    gate: &Gate,
) -> Result<(Value, Option<Rc<ax::Elements>>), Failure> {
    let viewed = interactive_in_view(ax, geom, region, gate)?;
    Ok(publish_elements(viewed, geom, region))
}

/// One read, as the wire publishes it. Shared by the application listing and the
/// bound window's, so a reference, a click point and a bounds rectangle mean the
/// same thing however the walk was rooted.
fn publish_elements(
    viewed: Viewed,
    geom: &Geometry,
    region: &Region,
) -> (Value, Option<Rc<ax::Elements>>) {
    // Whether this reply can hand out references at all is decided BEFORE any is
    // published: `retained` needs the owning process's start time, and without it
    // there is no table to resolve them against.
    let names = viewed.owner.is_some();
    let view = Viewport { geom, region };
    let (items, listed) = listed_nodes(viewed.nodes, names, 0, None, &view);

    // What the read cost in accessibility messages (M7), so the batching the
    // macOS reader does is a number the owner can read on a real application.
    let mut payload = json!({ "ok": true, "elements": items, "ax_messages": viewed.messages });
    if let Some(object) = payload.as_object_mut() {
        if let Some(note) = viewed.note {
            object.insert("ax_activation".to_string(), json!(note));
        }
        // A walk that stopped at one of its bounds says which. Without it a caller
        // reads a partial tree as the whole one and concludes a control is not
        // there, which is the one wrong answer this list can give.
        if let Some(truncation) = viewed.truncated {
            object.insert("truncated".to_string(), json!(truncation));
        }
    }

    (payload, retained(viewed.owner, listed))
}

/// The controls of ONE window, from the accessibility element a target is bound to.
///
/// Everything below the read is the same code an application listing uses — the
/// same click points, the same references, the same bounds — because a control is a
/// control wherever the walk started. Only the root differs, and only the root has
/// to.
fn elements_rooted(
    ax: &Rc<dyn ax::Ax>,
    root: ax::Handle,
    geom: &Geometry,
    region: &Region,
    gate: &Gate,
    worker: &Worker,
) -> Result<(Value, Option<Rc<ax::Elements>>), Failure> {
    let target = worker
        .target
        .as_ref()
        .ok_or_else(|| Failure::from("target_unavailable"))?;

    let clock = gate.clock();
    let before = ax.messages();
    let deadline = gate.now_ms().saturating_add(tree::WALK_BUDGET_MS);
    let found = tree::walk_from(ax, root, clock.as_ref(), deadline);
    let messages = ax.messages() - before;
    let (placed, _outside) = in_view(found.nodes, geom, region);

    let viewed = Viewed {
        nodes: placed,
        owner: Some((target.pid, target.started_at)),
        truncated: found.truncated,
        note: Some(format!("read {} — {}", target.app, target.title)),
        messages,
    };

    Ok(publish_elements(viewed, geom, region))
}

/// What one accessibility read found: the controls inside the view with their
/// click points, the application they came from (and when that process started,
/// which is what a reference is checked against), why the walk stopped early if it
/// did, and the note that explains any of it.
struct Viewed {
    nodes: Vec<ViewNode>,
    owner: Option<(i32, u64)>,
    truncated: Option<&'static str>,
    note: Option<String>,
    /// What the walk whose list this is cost in accessibility messages (M7), and
    /// that walk alone: an empty tree is walked again and again while it settles,
    /// and only the last walk's list is the one published.
    messages: u64,
}

impl Viewed {
    /// A read that found no tree to walk. It still says which application it
    /// looked at where it knows, because "nothing here" and "I could not tell you
    /// where I looked" are different facts.
    fn nothing(note: String) -> Viewed {
        Viewed {
            nodes: Vec::new(),
            owner: None,
            truncated: None,
            note: Some(note),
            messages: 0,
        }
    }
}

/// One walk of ONE application, cut to this view.
struct InView {
    /// The controls whose centres fall inside this view, with their sent-space
    /// points.
    nodes: Vec<ViewNode>,
    /// How many controls the walk saw before the view filter: what distinguishes
    /// "the app's tree is gated or empty" (activation territory) from "the app has
    /// controls, just none inside this region" (nothing to activate).
    total: usize,
    truncated: Option<&'static str>,
    /// What this one walk cost in accessibility messages.
    messages: u64,
}

/// One walk of ONE application, cut to this view. Shared by `elements` and the
/// `marks` overlay so the two can never disagree about what is there.
///
/// Every node the view filter drops releases the reference the walk retained for
/// it, because dropping a node IS releasing it.
fn in_view_nodes(
    ax: &Rc<dyn ax::Ax>,
    pid: i32,
    geom: &Geometry,
    region: &Region,
    gate: &Gate,
) -> InView {
    let clock = gate.clock();
    let before = ax.messages();
    let deadline = gate.now_ms().saturating_add(tree::WALK_BUDGET_MS);
    let found = tree::walk(ax, pid, clock.as_ref(), deadline);
    let messages = ax.messages() - before;
    let total = found.nodes.len();
    let (nodes, _outside) = in_view(found.nodes, geom, region);

    InView {
        nodes,
        total,
        truncated: found.truncated,
        messages,
    }
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

/// Pick the APPLICATION the view is about from front-to-back candidates: the frontmost
/// window with SUBSTANTIAL overlap of the request region wins — stacking order
/// is the tiebreak the screen actually shows, so a maximized background window
/// can never beat the smaller window in front of it. Only when nothing is
/// substantial (a sparse desktop of small windows) does raw maximum overlap
/// decide. Pure, so the selection semantics are unit-tested.
#[cfg(target_os = "macos")]
fn choose_target_app(candidates: Vec<(Region, TargetApp)>, region: &Region) -> Option<TargetApp> {
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
/// instead of concluding emptiness from one look. 2.4 s in all, because Chromium
/// asks for its complete mode through a two-second debounce after the switch is
/// written (M5), and a window shorter than that reports a tree that was about to
/// appear as absent.
const AX_SETTLE_POLL_MS: u64 = 300;
const AX_SETTLE_POLLS: u32 = 8;

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
fn interactive_in_view(
    ax_platform: &Rc<dyn ax::Ax>,
    geom: &Geometry,
    region: &Region,
    gate: &Gate,
) -> Result<Viewed, Failure> {
    let target = match view_target(ax_platform, geom, region)? {
        Ok(target) => target,
        Err(note) => return Ok(Viewed::nothing(note)),
    };
    read_application(
        ax_platform,
        &target,
        geom,
        region,
        gate,
        activate_accessibility,
    )
}

/// The portable half of [`interactive_in_view`], which only has to say WHICH
/// application: walk it, and only if its tree walked to nothing at all, ask it to
/// expose one (`activate`) and poll until it appears.
fn read_application(
    ax_platform: &Rc<dyn ax::Ax>,
    target: &TargetApp,
    geom: &Geometry,
    region: &Region,
    gate: &Gate,
    activate: fn(i32) -> Result<String, String>,
) -> Result<Viewed, Failure> {
    // The reference that outlives this reply is only usable while the process it
    // came from is still THAT process, so the start time is read once, here,
    // against the same pid the walk is about to use.
    let started_at = ax_platform.process_started_at(target.pid);
    let owner = started_at.map(|started_at| (target.pid, started_at));

    let first = in_view_nodes(ax_platform, target.pid, geom, region, gate);
    if !first.nodes.is_empty() {
        return Ok(Viewed {
            nodes: first.nodes,
            owner,
            truncated: first.truncated,
            note: Some(format!("read {}", target.app)),
            messages: first.messages,
        });
    }
    if first.total > 0 {
        let note = format!(
            "{}: {} interactive element(s) in the app, none inside this view",
            target.app, first.total
        );
        return Ok(Viewed {
            messages: first.messages,
            ..Viewed::nothing(note)
        });
    }

    let attempt = attempt_note(&activate(target.pid));
    match settle(ax_platform, target.pid, geom, region, gate, first) {
        Ok((walked, waited)) => {
            tree_published(ax_platform, target, &walked)?;
            Ok(settled_view(target, owner, &attempt, walked, waited))
        }
        Err(last) => Ok(Viewed {
            messages: last.messages,
            ..Viewed::nothing(format!("{}: cancelled while the tree settled", target.app))
        }),
    }
}

/// A tree that stayed empty in an application whose toolkit publishes none until
/// it is asked (Qt, Chromium on Linux) is that, said by name with the switch
/// that would make it publish one — not an application with nothing in it.
fn tree_published(
    ax_platform: &Rc<dyn ax::Ax>,
    target: &TargetApp,
    walked: &InView,
) -> Result<(), Failure> {
    if walked.total > 0 {
        return Ok(());
    }
    let Some(switch) = ax_platform.tree_switch(target.pid) else {
        return Ok(());
    };
    Err(Failure::new(
        "toolkit_accessibility_off",
        format!(
            "{}: its toolkit publishes no accessibility tree until it is asked to: {switch}, \
             then list again; or work in the image",
            target.app
        ),
    ))
}

/// Walk an empty tree again on a short cadence until it has controls in it or
/// the poll runs out: the last walk, and how long it waited for it.
///
/// Up to 2.4 seconds, reached from `elements` AND from any `marks: true`
/// screenshot, so it is a checkpoint site: a pause during a check image stops
/// waiting for a tree the caller no longer wants, and answers the last walk it
/// took (`Err`). Read-only, so the caller answers with an empty set and a note
/// rather than a cancelled action.
fn settle(
    ax_platform: &Rc<dyn ax::Ax>,
    pid: i32,
    geom: &Geometry,
    region: &Region,
    gate: &Gate,
    first: InView,
) -> Result<(InView, u64), InView> {
    let mut last = first;
    for poll in 1..=AX_SETTLE_POLLS {
        if gate.sleep(AX_SETTLE_POLL_MS).is_err() {
            return Err(last);
        }
        last = in_view_nodes(ax_platform, pid, geom, region, gate);
        if last.total > 0 {
            return Ok((last, u64::from(poll) * AX_SETTLE_POLL_MS));
        }
    }
    Ok((last, u64::from(AX_SETTLE_POLLS) * AX_SETTLE_POLL_MS))
}

/// What a settled tree came to, with the note that says how it got there.
fn settled_view(
    target: &TargetApp,
    owner: Option<(i32, u64)>,
    attempt: &str,
    walked: InView,
    waited: u64,
) -> Viewed {
    if !walked.nodes.is_empty() {
        return Viewed {
            nodes: walked.nodes,
            owner,
            truncated: walked.truncated,
            note: Some(format!(
                "{}: tree appeared after {waited}ms ({attempt})",
                target.app
            )),
            messages: walked.messages,
        };
    }
    let note = if walked.total > 0 {
        // The tree came alive; this view just contains none of it — more polling
        // cannot change that.
        format!(
            "{}: tree appeared after {waited}ms ({attempt}), but its {} element(s) are \
             outside this view",
            target.app, walked.total
        )
    } else {
        format!(
            "{}: no accessibility elements — {attempt}; tree still empty after {waited}ms",
            target.app
        )
    };
    Viewed {
        messages: walked.messages,
        ..Viewed::nothing(note)
    }
}

/// Which application this view is about, when there is one.
///
/// Three answers, and they are different facts: a target, a note saying why there
/// is none (a desktop with nothing in front, a window listing the OS refused), or
/// a typed failure because this platform has no accessibility API at all.
#[cfg(target_os = "macos")]
fn view_target(
    _ax: &Rc<dyn ax::Ax>,
    geom: &Geometry,
    region: &Region,
) -> Result<Result<TargetApp, String>, Failure> {
    let candidates = match window_candidates(geom) {
        Ok(candidates) => candidates,
        Err(reason) => return Ok(Err(reason)),
    };

    Ok(choose_target_app(candidates, region)
        .ok_or_else(|| "no application window to target for accessibility".to_string()))
}

/// On Linux, the application whose window reads ACTIVE on the accessibility bus.
/// Not the window under the view, as on macOS: telling which X window a frame is
/// takes the window list, which arrives with bound windows (M86 L3). The view still
/// decides which of its controls are listed.
#[cfg(target_os = "linux")]
fn view_target(
    ax: &Rc<dyn ax::Ax>,
    _geom: &Geometry,
    _region: &Region,
) -> Result<Result<TargetApp, String>, Failure> {
    Ok(linux::atspi::frontmost_application(ax).map(|(pid, app)| TargetApp { pid, app }))
}

/// No accessibility API this build speaks. A typed failure, not an empty list: a
/// caller that cannot tell "nothing there" from "not supported here" writes the
/// wrong sentence about both.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn view_target(
    _ax: &Rc<dyn ax::Ax>,
    _geom: &Geometry,
    _region: &Region,
) -> Result<Result<TargetApp, String>, Failure> {
    Err("element enumeration is only supported on macOS and Linux".into())
}

/// Ask one application to expose its accessibility tree, where that is a thing
/// that can be asked.
#[cfg(target_os = "macos")]
fn activate_accessibility(pid: i32) -> Result<String, String> {
    ax::activate_accessibility(pid)
}

/// Linux has no switch per application, and compux changes no accessibility setting
/// for the person: a toolkit publishes its tree, or the refusal names what would
/// make it. The empty tree is still walked again while it settles — Chromium
/// builds its tree after the first client reads it.
#[cfg(target_os = "linux")]
fn activate_accessibility(_pid: i32) -> Result<String, String> {
    Err("nothing to switch on: a Linux toolkit publishes its own tree or none".to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn activate_accessibility(_pid: i32) -> Result<String, String> {
    Err("no accessibility API on this platform".to_string())
}

/// What an activation did, in the note's words: the attribute and whether this
/// process switched it on or found it on already, or why it would not.
fn attempt_note(attempt: &Result<String, String>) -> String {
    match attempt {
        Ok(done) => done.clone(),
        Err(reason) => format!("activation refused: {reason}"),
    }
}

// --- accessibility (inspect) ------------------------------------------------

/// Report the accessibility element under a point of the image it names: its role
/// and label. READ-ONLY — a grounding/judgment aid (confirm what control is there
/// before a consequential click), not a gate. It addresses a point exactly as a
/// click does, through the transform its image was made with, so what it reports
/// is what a click at that coordinate would reach.
fn inspect(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Result<Value, Failure> {
    let (x, y) = coords(&request.body)?;
    let observation = observed(request, worker)?;
    let (lx, ly) = point_in(&observation, x, y)?;
    acting_view(request, &observation, gate, worker)?;
    Ok(inspect_at(lx as f32, ly as f32)?)
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

// --- helpers ----------------------------------------------------------------

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

    use crate::held::{
        sweep_injected_failures, Call, Direction, Recorder, CLIPBOARD_RESTORE_DWELL_MS,
    };
    use crate::listing::element_value;

    /// A launcher a test can ask about without a bundle: the badge is "there", and
    /// nothing is ever spawned unless a test spawns it.
    fn test_launcher() -> Arc<dyn indicator::Launcher> {
        indicator::Scripted::new()
    }

    /// A gate with nothing in flight, for a test that drives one function directly.
    fn idle_gate() -> Gate {
        Gate::new("boot-test".to_string(), Arc::new(SystemClock::new()))
    }

    /// A failed action as its payload, so a dispatch test can assert on one shape
    /// whether the host had a display or not.
    fn err_payload(failure: Failure) -> Value {
        json!({ "ok": false, "error": failure.code })
    }

    /// A worker with an empty table, for a test that drives one action directly.
    /// The displays a test is given: fixed facts, no OS call, and the SAME shape
    /// every other seam in this suite has.
    ///
    /// Every worker a test builds gets this. `RealDisplays` panics under
    /// `cfg(test)`, so a path that resolves a display the eager way fails here on
    /// a Mac with three screens attached exactly as it would on a headless runner
    /// — which is the only way a suite can prove it is headless-safe from a
    /// machine that has a display.
    struct ScriptedDisplays {
        display: Display,
        /// Answers for an index nobody scripted. A test that asks for display 3 of
        /// a one-display desktop gets the refusal a real host would give it.
        listed: usize,
    }

    impl ScriptedDisplays {
        fn one() -> Rc<ScriptedDisplays> {
            Rc::new(ScriptedDisplays {
                display: check_display(),
                listed: 1,
            })
        }

        /// A host with no display session at all — a locked Mac, a headless
        /// runner, a CI container. Every resolve refuses.
        fn none() -> Rc<ScriptedDisplays> {
            Rc::new(ScriptedDisplays {
                display: check_display(),
                listed: 0,
            })
        }
    }

    impl Displays for ScriptedDisplays {
        fn at(&self, index: usize) -> Result<Display, String> {
            if index < self.listed {
                Ok(self.display)
            } else {
                Err(format!("display {index} not found"))
            }
        }
    }

    /// A worker over this build's own accessibility platform: none at all off
    /// macOS, exactly as production has it.
    fn idle_worker(gate: &Gate) -> Worker {
        Worker {
            accessibility: accessibility_supported(),
            ..worker_with(gate, Rc::new(ax::Real::new()))
        }
    }

    /// A worker over a given accessibility platform, so an action that acts on a
    /// control is driven against the recording one.
    fn worker_with(gate: &Gate, ax: Rc<dyn ax::Ax>) -> Worker {
        frames_worker(gate, ax, Rc::new(BlankScreen))
    }

    /// The frames a test worker is given when the test is not about pixels: one
    /// flat image the size of [`check_display`], so a measured transform comes out
    /// at one pixel a point and nothing asks the OS for anything.
    struct BlankScreen;

    impl Frames for BlankScreen {
        fn capture(&self, _display_id: u32) -> Result<image::RgbaImage, Failure> {
            Ok(image::RgbaImage::from_pixel(
                CHECK_DISPLAY_W,
                CHECK_DISPLAY_H,
                image::Rgba([20, 20, 20, 255]),
            ))
        }
    }

    /// A worker over both injected platforms: the accessibility tree it reads, and
    /// the screen it takes frames from.
    fn frames_worker(gate: &Gate, ax: Rc<dyn ax::Ax>, frames: Rc<dyn Frames>) -> Worker {
        Worker {
            observations: Observations::new(&gate.envelope().sidecar_generation, gate.clock()),
            measured: Measured::new(),
            ax,
            frames,
            windows: window_server::Recorder::new(Vec::new()),
            launcher: indicator::Scripted::new(),
            displays: ScriptedDisplays::one(),
            target: None,
            target_generation: 0,
            emitter: capture::Emitter::capturing(),
            accessibility: true,
            until_held: None,
            cutoff_ms: None,
            endpoint: Endpoint::default(),
            pixels: test_pixels(),
        }
    }

    /// Pixels a test worker can post through and never reach a screen with: on
    /// Linux a recording platform that serves every pixel action.
    #[cfg(target_os = "linux")]
    fn test_pixels() -> Pixels {
        Pixels::Recorded(crate::held::Recorder::new(None))
    }

    #[cfg(not(target_os = "linux"))]
    fn test_pixels() -> Pixels {
        Pixels
    }

    /// Run one action through the real dispatch table, as the worker does.
    fn dispatch(body: Value) -> Result<Value, Failure> {
        let gate = Gate::new("boot-test".to_string(), Arc::new(SystemClock::new()));
        let mut worker = idle_worker(&gate);
        let request = request_for(&body);

        handle(&request, &gate, &capture::Emitter::new(), &mut worker)
    }

    /// The frame the reader would have parsed for this body, addressing included —
    /// so a dispatch test exercises the same shape a real request arrives in.
    fn request_for(body: &Value) -> wire::Request {
        wire::Request {
            request_id: "r1".to_string(),
            action: body["action"].as_str().unwrap_or_default().to_string(),
            body: body.clone(),
            sidecar_generation: None,
            session_generation: None,
            authorization_generation: None,
            mutation_seq: None,
            observation_id: body["observation_id"].as_str().map(str::to_string),
            element_ref: body["element_ref"].as_str().map(str::to_string),
            target_id: body["target_id"].as_str().map(str::to_string),
            until: None,
            check: match body["check"].as_str() {
                Some("image") => wire::Check::Image,
                Some("semantic") => wire::Check::Semantic,
                _ => wire::Check::None,
            },
        }
    }

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
    #[cfg(target_os = "macos")]
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

        let chosen = choose_target_app(candidates, &full).expect("a target");
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

        let chosen = choose_target_app(candidates, &request).expect("a target");
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
        let chosen = choose_target_app(candidates, &full).expect("a target");
        assert_eq!(chosen.app, "MiniPlayer");

        let tied = vec![
            candidate(0.0, 0.0, 100.0, 100.0, 1, "Front"),
            candidate(500.0, 0.0, 100.0, 100.0, 2, "Back"),
        ];
        let chosen = choose_target_app(tied, &full).expect("a target");
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
        assert_eq!(choose_target_app(candidates, &request), None);
        assert_eq!(choose_target_app(Vec::new(), &request), None);
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

    /// A target bound to window 20, with one frame in its stream — enough for the
    /// replies that name it and the ones that do not.
    fn bound_worker(gate: &Gate) -> Worker {
        use window_frames::WindowFrames as _;

        let stream = window_frames::Recorder::new();
        stream
            .start(window_frames::StreamWindow {
                id: 20,
                width: 1600,
                height: 1200,
            })
            .unwrap();
        stream.deliver_sized(1_000, window_frames::Health::Live, 1600, 1200, 1600 * 4);

        let mut worker = idle_worker(gate);
        worker.windows = window_server::Recorder::new(vec![window_server::window(
            20,
            200,
            window_server::Bounds {
                x: 0.0,
                y: 0.0,
                w: 800.0,
                h: 600.0,
            },
        )]);
        worker.target = Some(target::Target {
            id: "t1".to_string(),
            generation: 1,
            window_id: 20,
            pid: 200,
            started_at: 1,
            app: "App200".to_string(),
            title: "Window 20".to_string(),
            ax_binding: target::AxBinding::Unavailable,
            ax_window: None,
            siblings: vec![20],
            display_id: 7,
            facts: MonitorFacts {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale_factor: 2.0,
            },
            frames: Rc::new(target::TargetFrames::new(stream, gate.clock())),
            indicator: None,
            restarts: 0,
        });
        worker
    }

    // A picture of the whole screen is not a picture of the bound window, whatever
    // is bound at the time. Stamping a `target_id` on it would tell the caller its
    // coordinates are in a window's space when they are the display's.
    #[test]
    fn a_display_level_reply_is_never_stamped_with_the_bound_window() {
        let gate = Gate::new("boot-test".to_string(), Arc::new(SystemClock::new()));
        let worker = bound_worker(&gate);

        let mut display_level = json!({ "ok": true });
        note_target(&mut display_level, &worker, false).expect("nothing to do");
        assert_eq!(display_level.get("target_id"), None);
        assert_eq!(display_level.get("children"), None);

        let mut about_the_window = json!({ "ok": true });
        note_target(&mut about_the_window, &worker, true).expect("the window server answered");
        assert_eq!(about_the_window["target_id"], json!("t1"));
        assert_eq!(about_the_window["children"], json!([]));
    }

    // A window server that would not answer is not an empty list of children: the
    // caller would read "no new windows" for "nobody knows".
    #[test]
    fn a_reply_about_a_window_fails_rather_than_claiming_no_children() {
        let gate = Gate::new("boot-test".to_string(), Arc::new(SystemClock::new()));
        let refused = window_server::Recorder::new(Vec::new());
        refused.refuses("the window server refused");

        let mut worker = bound_worker(&gate);
        worker.windows = refused;

        let mut payload = json!({ "ok": true });
        let refusal = note_target(&mut payload, &worker, true).expect_err("nothing is known");
        assert_eq!(refusal.code, "target_unavailable");
    }

    // The handshake may not offer a feature whose every use is a refusal.
    #[test]
    fn the_handshake_offers_targets_exactly_where_a_window_can_be_captured() {
        assert_eq!(
            targets_supported(),
            capture_methods(&test_pixels()).contains(&"window"),
            "`targets` and the window capture method are one fact"
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
        let response = dispatch(json!({ "action": "windows" })).unwrap_or_else(err_payload);

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
        let v = hello(&test_launcher(), &test_pixels()).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["protocol_version"], json!(PROTOCOL_VERSION));
        assert!(v["compux_version"].is_string());
        assert!(v["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "screenshot"));

        // The bounds of the observation table are capabilities, so a caller mirrors
        // the same numbers instead of hard-coding a guess at them.
        assert_eq!(
            v["capabilities"]["observations"],
            json!({ "max": observation::MAX_OBSERVATIONS, "ttl_ms": observation::TTL_MS })
        );
    }

    fn advertised(hello: &Value) -> Vec<&str> {
        hello["actions"]
            .as_array()
            .expect("a list of actions")
            .iter()
            .map(|action| action.as_str().expect("an action name"))
            .collect()
    }

    // F1: the handshake lists only what THIS build can do. The engine builds the
    // model's verbs from it, and a verb that always refuses is a verb the model is
    // invited to waste a turn on. The Linux build lists and acts on controls through
    // AT-SPI (M86 L1), answers no `inspect`, and binds no window.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_the_handshake_offers_the_controls_and_their_verbs_and_no_inspect() {
        let v = hello(&test_launcher(), &test_pixels()).unwrap();

        assert_eq!(
            advertised(&v),
            vec![
                "screenshot",
                "left_click",
                "right_click",
                "double_click",
                "mouse_move",
                "left_click_drag",
                "scroll",
                "type",
                "key",
                "wait",
                "wait_for_change",
                "paste",
                "elements",
                "windows",
                "press",
                "set_value",
                "focus",
                "select",
                "toggle",
                "expand",
                "collapse",
                "scroll_to",
                "wait_for",
            ]
        );
        assert_eq!(
            v["capabilities"]["input_methods"],
            json!(["foreground_hid", "ax"]),
            "`ax` is how an action reaches a control, on both systems"
        );
    }

    // The probe says which display server answered, and whether an X endpoint
    // answers, as or beside it. Capture and input are the pixel route's.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_the_probe_says_which_display_answered() {
        let (_x11, env) = linux::display::answering_x11();
        let own = Endpoint {
            env,
            adopted: false,
        };
        let shown = |endpoint: &Endpoint, pixels: &Pixels| {
            let v = probe_with(endpoint, pixels, serde_json::Map::new).unwrap();
            let keys = [
                "display_server",
                "x11_endpoint",
                "screen_capture",
                "input_control",
            ];
            keys.map(|key| v[key].clone())
        };

        assert_eq!(
            shown(&own, &test_pixels()),
            [json!("x11"), json!(true), json!(true), json!(true)]
        );
        let refused = Pixels::Refused("no display answers".to_string());
        assert_eq!(
            shown(&Endpoint::default(), &refused),
            [json!("none"), json!(false), json!(false), json!(false)]
        );
    }

    // Beside the display, the Linux probe says what the engine's doctor shows of
    // accessibility: the bus, its packages and the toolkit notes, as read.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_the_probe_adds_the_accessibility_bus_its_packages_and_the_notes() {
        let facts = linux::probe::fields(
            &linux::probe::Bus::Empty,
            None,
            &linux::probe::Packages::default(),
        );

        let v = probe_with(&Endpoint::default(), &test_pixels(), || facts.clone()).unwrap();

        for key in [
            "accessibility_bus",
            "accessibility_packages",
            "toolkit_notes",
        ] {
            assert_eq!(v[key], facts[key], "{key}: {v}");
        }
        assert!(v["display_server"].is_string(), "{v}");
    }

    // A session whose pixel route is refused (a compositor compux has not
    // qualified, a portal with no Registry, no display at all) serves controls only.
    // The handshake offers no pixel verb and no foreground input, the probe says it
    // captures and posts nothing, and a pixel request is refused by name, with the
    // route's reason, before anything is sent.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_a_session_whose_pixel_route_is_refused_serves_controls_only() {
        let refused = || Pixels::Refused("KDE Plasma on Wayland has no pixel route".to_string());
        let adopted = Endpoint {
            adopted: true,
            ..Endpoint::default()
        };

        let v = hello(&test_launcher(), &refused()).unwrap();
        let probed = probe_with(&adopted, &refused(), serde_json::Map::new).unwrap();

        let actions = advertised(&v);
        for pixel in PIXEL_ACTIONS {
            assert!(!actions.contains(&pixel), "{pixel} offered: {v}");
        }
        for control in ["elements", "press", "set_value", "wait_for", "type", "wait"] {
            assert!(actions.contains(&control), "{control} not offered: {v}");
        }
        assert_eq!(v["capabilities"]["input_methods"], json!(["ax"]), "{v}");
        assert_eq!(v["capabilities"]["capture_methods"], json!([]), "{v}");
        assert_eq!(probed["display_adopted"], json!(true), "{probed}");
        assert_eq!(probed["screen_capture"], json!(false), "{probed}");
        assert_eq!(probed["input_control"], json!(false), "{probed}");

        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let mut worker = Worker {
            endpoint: adopted.clone(),
            pixels: refused(),
            ..idle_worker(&gate)
        };
        let mut typed = running("type", Some(2));
        typed.body = json!({"action": "type", "text": "hi"});
        // A verb by reference that asks for an image check would capture after it
        // acted: it is refused before anything is sent.
        let mut pictured = running("press", Some(3));
        pictured.check = wire::Check::Image;
        let pixel_requests = [
            running("screenshot", None),
            running("left_click", Some(1)),
            typed,
            pictured,
        ];
        for request in pixel_requests {
            let frame = served(&request, &gate, &mut worker);
            assert_eq!(frame["error"], json!("pixels_unavailable"), "{frame}");
            let detail = frame["detail"].as_str().unwrap_or_default();
            assert!(detail.contains("KDE Plasma"), "the route's reason: {frame}");
        }
    }

    // Keys go only into a window that reads ACTIVE, as macOS types into the key
    // window of the application in front. With none, typing, a chord and a paste
    // are refused before a key is posted; compux never activates a window to make
    // room.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_keys_go_only_into_a_window_that_reads_active() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        app.set_frontmost(None);
        let mut worker = worker_with(&gate, app.clone());
        let bodies = [
            json!({"action": "type", "text": "hi"}),
            json!({"action": "key", "chord": "ctrl+a"}),
            json!({"action": "paste", "text": "hi"}),
        ];

        for (seq, body) in (1..).zip(bodies) {
            let mut request = running(body["action"].as_str().unwrap_or_default(), Some(seq));
            request.body = body;
            let frame = served(&request, &gate, &mut worker);
            assert_eq!(frame["error"], json!("no_active_window"), "{frame}");
        }
        let Pixels::Recorded(recorder) = &worker.pixels else {
            panic!("a test worker records its input");
        };
        assert!(recorder.calls.is_empty(), "posted: {:?}", recorder.calls);

        app.set_frontmost(Some(FIXTURE_PID));
        let mut typed = running("type", Some(4));
        typed.body = json!({"action": "type", "text": "hi"});
        let frame = served(&typed, &gate, &mut worker);

        assert!(frame["error"].is_null(), "{frame}");
        let Pixels::Recorded(recorder) = &worker.pixels else {
            panic!("a test worker records its input");
        };
        assert_eq!(recorder.calls, [held::Call::Text("hi".to_string())]);
    }

    // On GNOME Wayland the portal serves the pointer, the keys and the screen,
    // and `hello` lists exactly that: no `paste` (no portal clipboard compux can
    // own) and no `windows` (no portal lists them). Nothing is asked of the portal
    // to say so.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_the_portal_route_lists_what_it_serves_and_asks_nothing_to_say_so() {
        let portal = linux::portal::Portal::with(
            Box::new(|| panic!("hello asked the portal")),
            None,
            Duration::from_millis(1),
        );
        let pixels = Pixels::on_portal(portal);

        let v = hello(&test_launcher(), &pixels).unwrap();

        let actions = advertised(&v);
        for served in [
            "screenshot",
            "left_click",
            "left_click_drag",
            "scroll",
            "key",
            "type",
        ] {
            assert!(actions.contains(&served), "{served} not offered: {v}");
        }
        for unserved in ["paste", "windows"] {
            assert!(!actions.contains(&unserved), "{unserved} offered: {v}");
        }
        assert_eq!(
            v["capabilities"]["input_methods"],
            json!(["foreground_hid", "ax"]),
            "{v}"
        );
        assert_eq!(
            v["capabilities"]["capture_methods"],
            json!(["display"]),
            "{v}"
        );
    }

    // handed it and its authority as values. Only the route decides, never where
    // the display came from.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_a_display_taken_from_the_user_manager_serves_pixels() {
        let (_x11, env) = linux::display::answering_x11();
        let adopted = Endpoint { env, adopted: true };

        let v = hello(&test_launcher(), &test_pixels()).unwrap();
        let probed = probe_with(&adopted, &test_pixels(), serde_json::Map::new).unwrap();

        assert!(advertised(&v).contains(&"screenshot"), "{v}");
        assert_eq!(probed["display_adopted"], json!(true), "{probed}");
        assert_eq!(probed["screen_capture"], json!(true), "{probed}");
        assert_eq!(
            linux::pixels::decide(adopted.server(), None),
            linux::pixels::Decision::X11
        );
    }

    // And the macOS build offers every verb it offered before, in the same order.
    #[cfg(target_os = "macos")]
    #[test]
    fn on_macos_the_handshake_offers_every_verb_and_both_input_methods() {
        let v = hello(&test_launcher(), &test_pixels()).unwrap();

        assert_eq!(
            advertised(&v),
            vec![
                "screenshot",
                "left_click",
                "right_click",
                "double_click",
                "mouse_move",
                "left_click_drag",
                "scroll",
                "type",
                "key",
                "wait",
                "inspect",
                "wait_for_change",
                "paste",
                "elements",
                "windows",
                "press",
                "set_value",
                "focus",
                "select",
                "toggle",
                "expand",
                "collapse",
                "scroll_to",
                "wait_for",
                "select_target",
                "release_target",
            ]
        );
        assert_eq!(
            v["capabilities"]["input_methods"],
            json!(["foreground_hid", "ax"])
        );
    }

    // --- M42 slice 3: a coordinate names the image it was read in ------------

    /// A worker holding one image of a display that is not this machine's, so the
    /// refusals that come BEFORE any display read are testable on a host with no
    /// screen at all.
    fn worker_holding_an_image(gate: &Gate) -> (Worker, Observation) {
        let mut worker = idle_worker(gate);
        let facts = MonitorFacts {
            x: 0,
            y: 0,
            width: 1512,
            height: 982,
            scale_factor: 2.0,
        };
        let measured = geometry::Measurement {
            frame_w: 3024,
            frame_h: 1964,
            pixels_per_point: 2.0,
        };
        let geom = Geometry::from_facts(&facts, measured, Host::MacOs);
        let region = Region::full(&geom);
        let sent = crop_rect(&geom, &region).sent_dims();
        let observation = worker.observations.mint(observation::Minting {
            kind: Kind::Image,
            display_id: 424_242,
            facts,
            geometry: geom,
            region,
            sent,
            elements: None,
            view_hash: None,
        });

        (worker, observation)
    }

    // Every one of these is the same promise: nothing was sent. The model is told
    // which of them happened because the next move differs — look again, or read
    // the coordinate again.
    #[test]
    fn a_click_that_names_no_image_this_process_holds_is_refused_and_sends_nothing() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let (mut worker, observation) = worker_holding_an_image(&gate);

        let mut request = running("left_click", Some(1));
        request.body = json!({"action": "left_click", "x": 4, "y": 9});
        request.observation_id = Some("nobody-1".to_string());

        let frame = serve(&request, &gate, &capture::Emitter::new(), &mut worker);
        assert_eq!(frame["error"], json!("unknown_observation"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert_eq!(frame["receipt"]["observation_id_before"], json!("nobody-1"));
        assert!(frame["detail"]
            .as_str()
            .unwrap()
            .contains("fresh screenshot"));

        // A point past the edge of the image it DOES hold: refused, never clamped
        // onto the edge, and the sentence says how big that image was.
        let outside = point_in(&observation, observation.sent_w as f64, 10.0)
            .expect_err("one pixel past the right edge");
        assert_eq!(outside.code, "point_outside_observation");
        assert!(outside
            .detail
            .unwrap()
            .contains(&format!("{}x{}", observation.sent_w, observation.sent_h)));

        // And the last pixel inside it maps, so the bound is exclusive at the top
        // and inclusive at the bottom, once.
        assert!(point_in(&observation, (observation.sent_w - 1) as f64, 0.0).is_ok());
    }

    // A frame that cannot be explained fails the action rather than being clicked
    // through, and it carries both sizes: an operator reading the trace has to be
    // able to see which two numbers disagreed.
    #[test]
    fn a_capture_that_does_not_match_its_display_fails_with_both_sizes() {
        let facts = MonitorFacts {
            x: 0,
            y: 0,
            width: 1512,
            height: 982,
            scale_factor: 2.0,
        };

        let detail = geometry::measure(&facts, 2268, 1473, Host::MacOs)
            .expect_err("1.5 pixels per point is neither the mode's 2 nor 1");
        let failure = Failure::new("capture_geometry_mismatch", detail);

        assert_eq!(failure.code, "capture_geometry_mismatch");
        let detail = failure.detail.unwrap();
        for number in [
            "frame 2268x1473",
            "display 1512x982 points",
            "mode scale 2.0000",
            "1.5000 across",
            "1.5000 down",
        ] {
            assert!(detail.contains(number), "{number} missing from: {detail}");
        }
    }

    // idle_ms / wait_for_idle are OPERATIONAL (policy-support), never advertised as
    // model verbs — same posture as `probe`. Lock that so they aren't offered to a model.
    #[test]
    fn idle_verbs_are_not_advertised_model_actions() {
        let actions = hello(&test_launcher(), &test_pixels()).unwrap()["actions"]
            .as_array()
            .unwrap()
            .clone();
        assert!(!actions.iter().any(|a| a == "idle_ms"));
        assert!(!actions.iter().any(|a| a == "wait_for_idle"));
    }

    // The capture control verbs are operational (policy-driven), NOT model verbs —
    // no model tool call must reach observe_start/observe_stop (MILESTONE_32 §8.4a).
    #[test]
    fn observe_verbs_are_not_advertised_model_actions() {
        let actions = hello(&test_launcher(), &test_pixels()).unwrap()["actions"]
            .as_array()
            .unwrap()
            .clone();
        assert!(!actions.iter().any(|a| a == "observe_start"));
        assert!(!actions.iter().any(|a| a == "observe_stop"));
    }

    // observe_start/observe_stop reply with a type-discriminated ack carrying
    // protocol_version — the frame the consumer's handshake reads (never err()).
    #[test]
    fn observe_stop_returns_an_ack_frame() {
        let ack = dispatch(json!({"action": "observe_stop"})).expect("an ack is always ok");
        assert_eq!(ack["type"], json!("ack"));
        assert_eq!(ack["action"], json!("observe_stop"));
        assert_eq!(ack["ok"], json!(true));
        assert_eq!(ack["protocol_version"], json!(PROTOCOL_VERSION));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn idle_ms_reports_a_nonnegative_value() {
        let gate = idle_gate();
        let v = idle_ms(&idle_worker(&gate)).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert!(v["idle_ms"].as_u64().is_some());
    }

    // idle_ms:0 means "idle for >= 0ms", which is always true, so the poll returns
    // immediately with idle:true — a deterministic check of the loop's success path.
    #[cfg(target_os = "macos")]
    #[test]
    fn wait_for_idle_zero_target_returns_immediately_idle() {
        let v = wait_for_idle(&json!({"idle_ms": 0, "timeout_ms": 500}), &idle_gate()).unwrap();
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
            &idle_gate(),
        )
        .unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["idle"], json!(false));
    }

    // The dispatch table wires the operational verbs through `handle`.
    #[cfg(target_os = "macos")]
    #[test]
    fn handle_dispatches_idle_ms() {
        let v = dispatch(json!({"action": "idle_ms"})).expect("idle_ms answers");
        assert_eq!(v["ok"], json!(true));
        assert!(v["idle_ms"].as_u64().is_some());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn idle_detection_is_macos_only_off_macos() {
        let gate = idle_gate();
        assert!(idle_ms(&idle_worker(&gate)).is_err());
        assert!(wait_for_idle(&json!({}), &gate).is_err());
    }

    // 75 is a MEANING, not a number: Fermix reads it as a clean capture-stall
    // self-reap. `posix_spawnattr_setflags` used to exit 75 too, so a disclaim
    // failure reached the operator as a wedged capture backend. Stated as "NO
    // disclaim code is 75" over the whole set, so a code added later either joins
    // the list or fails here — asserting only the one code that moved would pass
    // for the next collision as happily as this one.
    #[cfg(target_os = "macos")]
    #[test]
    fn no_disclaim_exit_code_collides_with_the_capture_stall() {
        let codes = disclaim::EXIT_CODES;
        assert!(
            !codes.contains(&EXIT_CAPTURE_STALLED),
            "a disclaim failure exits {EXIT_CAPTURE_STALLED}, which Fermix reads as a capture stall"
        );

        for (i, code) in codes.iter().enumerate() {
            assert!(
                !codes[i + 1..].contains(code),
                "two disclaim failures share exit code {code}, so neither can be diagnosed"
            );
        }
    }

    // --- held input: the four sequences that can leave a press behind ---------
    //
    // Two assertions per sequence, and both matter. The CLEAN one pins the exact
    // event list the desktop used to see, so the guard cannot have quietly changed
    // what a click or a drag does. The SWEEP refuses every one of those calls in
    // turn and demands that nothing is left held and the clipboard reads as the
    // user left it — the defect class itself, asserted at every step rather than at
    // the one step someone thought of.

    #[test]
    fn click_posts_the_event_sequence_it_always_has() {
        let mut platform = Recorder::new(None);
        click_seq(
            &mut platform,
            120,
            240,
            Button::Left,
            2,
            &[Key::Meta, Key::Shift],
        )
        .unwrap();

        assert_eq!(
            platform.calls,
            vec![
                Call::MoveMouse(120, 240),
                Call::Settle(120, 240),
                Call::Key(Key::Meta, Direction::Press),
                Call::Key(Key::Shift, Direction::Press),
                Call::Button(Button::Left, Direction::Click),
                Call::Button(Button::Left, Direction::Click),
                // Reverse of the press order (it used to release Meta first); the
                // modifier flag state a target reads is identical either way.
                Call::Key(Key::Shift, Direction::Release),
                Call::Key(Key::Meta, Direction::Release),
            ]
        );
    }

    // The leak: a `?` out of the repeat loop returned with the modifiers down.
    #[test]
    fn click_releases_its_modifiers_however_it_fails() {
        sweep_injected_failures(None, |platform| {
            click_seq(platform, 12, 34, Button::Right, 2, &[Key::Meta, Key::Shift])
        });
    }

    #[test]
    fn drag_posts_the_event_sequence_it_always_has() {
        let mut platform = Recorder::new(None);
        drag_seq(&mut platform, 10, 20, 50, 60).unwrap();

        let mut expected = vec![
            Call::MoveMouse(10, 20),
            Call::Settle(10, 20),
            Call::Button(Button::Left, Direction::Press),
            Call::Sleep(DRAG_GRAB_MS),
        ];
        for &(x, y) in &drag_path(10, 20, 50, 60, DRAG_STEPS) {
            expected.push(Call::DragStep(x, y));
            expected.push(Call::Sleep(DRAG_STEP_MS));
        }
        expected.push(Call::Settle(50, 60));
        expected.push(Call::Sleep(DRAG_DROP_MS));
        expected.push(Call::Button(Button::Left, Direction::Release));

        assert_eq!(platform.calls, expected);
    }

    // The leak: anything between the press and the release left the desktop
    // dragging — a held left button, which takes a physical click to clear.
    #[test]
    fn drag_releases_the_button_however_it_fails() {
        sweep_injected_failures(None, |platform| drag_seq(platform, 10, 20, 50, 60));
    }

    #[test]
    fn paste_posts_the_event_sequence_it_always_has() {
        let mut platform = Recorder::new(Some("the user's own text"));
        paste_seq(&mut platform, "pasted").unwrap();

        assert_eq!(
            platform.calls,
            vec![
                Call::ClipboardRead,
                Call::ClipboardWrite("pasted".to_string()),
                Call::Sleep(PASTE_SETTLE_MS),
                Call::Key(paste_modifier(), Direction::Press),
                Call::Key(Key::Unicode('v'), Direction::Click),
                Call::Key(paste_modifier(), Direction::Release),
                Call::Sleep(CLIPBOARD_RESTORE_DWELL_MS),
                Call::ClipboardWrite("the user's own text".to_string()),
            ]
        );
        assert_eq!(platform.clipboard.as_deref(), Some("the user's own text"));
    }

    // The leak: a failure around the keystroke returned with the modifier down AND
    // the user's clipboard still holding our text.
    #[test]
    fn paste_releases_and_restores_the_clipboard_however_it_fails() {
        sweep_injected_failures(Some("the user's own text"), |platform| {
            paste_seq(platform, "pasted")
        });
    }

    #[test]
    fn key_chord_posts_the_event_sequence_it_always_has() {
        let mut platform = Recorder::new(None);
        key_chord_seq(&mut platform, &[Key::Meta], Key::Unicode('c')).unwrap();

        assert_eq!(
            platform.calls,
            vec![
                Call::Key(Key::Meta, Direction::Press),
                Call::Key(Key::Unicode('c'), Direction::Click),
                Call::Key(Key::Meta, Direction::Release),
            ]
        );
    }

    // This one already released after a failed key; what it could not survive was a
    // failure part way through PRESSING several modifiers.
    #[test]
    fn key_chord_releases_its_modifiers_however_it_fails() {
        sweep_injected_failures(None, |platform| {
            key_chord_seq(platform, &[Key::Meta, Key::Shift, Key::Alt], Key::Tab)
        });
    }

    // --- cancellation: a pause that lands INSIDE a running sequence -----------
    //
    // The gate's own tests prove the barrier; these prove what the barrier does to
    // a sequence that is already part way through one — which is the half a caller
    // feels, because it is the half that decides whether the left button is still
    // down when the drag stops.

    /// A clock that installs a pause on the Nth sleep, so a cancellation lands at a
    /// known point inside a sequence rather than at a hopeful wall-clock moment.
    struct PauseOnSleep {
        at: u64,
        sleeps: std::sync::atomic::AtomicU64,
        gate: std::sync::Mutex<Option<Gate>>,
    }

    impl PauseOnSleep {
        fn new(at: u64) -> Arc<PauseOnSleep> {
            Arc::new(PauseOnSleep {
                at,
                sleeps: std::sync::atomic::AtomicU64::new(0),
                gate: std::sync::Mutex::new(None),
            })
        }

        fn arm(self: &Arc<Self>, gate: &Gate) {
            *self.gate.lock().unwrap() = Some(gate.clone());
        }

        /// Breaks the clock <-> gate cycle the arming makes, so the test leaks nothing.
        fn disarm(self: &Arc<Self>) {
            *self.gate.lock().unwrap() = None;
        }
    }

    impl gate::Clock for PauseOnSleep {
        fn now_ms(&self) -> u64 {
            0
        }

        fn now_ns(&self) -> u128 {
            0
        }

        fn now_mach(&self) -> u64 {
            0
        }

        fn sleep(&self, _ms: u64) {
            let nth = self
                .sleeps
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if nth == self.at {
                if let Some(gate) = self.gate.lock().unwrap().as_ref() {
                    gate.control(wire::ControlAction::Pause, None);
                }
            }
        }
    }

    fn running(action: &str, mutation_seq: Option<u64>) -> wire::Request {
        wire::Request {
            request_id: "r1".to_string(),
            action: action.to_string(),
            body: json!({}),
            sidecar_generation: Some("boot-1".to_string()),
            session_generation: Some(1),
            authorization_generation: Some(1),
            mutation_seq,
            observation_id: wire::addresses_an_image(action).then(|| "7c1e-1".to_string()),
            element_ref: wire::addresses_an_element(action).then(|| "e1".to_string()),
            check: wire::Check::None,
            target_id: None,
            until: None,
        }
    }

    // The live check the owner cannot be asked to trust to luck: a pause during a
    // long drag stops it PART WAY and the left button comes back up. Before the
    // gate there was no way to stop it at all; the danger of stopping it badly is
    // a desktop left dragging.
    #[test]
    fn a_pause_part_way_through_a_drag_stops_it_and_releases_the_button() {
        // Sleeps: three slices of the 60 ms grab, then one per path step. The
        // fifth lands inside the second step, so the drag is genuinely mid-path.
        let clock = PauseOnSleep::new(5);
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        clock.arm(&gate);
        gate.admit(&running("left_click_drag", Some(1))).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(None));
        let outcome = drag_seq(&mut platform, 10, 20, 50, 60);
        let done = gate.finish();

        assert_eq!(outcome, Err("cancelled".to_string()));
        assert!(done.posted, "input did reach the screen before the pause");
        assert!(done.cancelled);

        let calls = &platform.inner().calls;
        let presses = calls
            .iter()
            .filter(|call| matches!(call, Call::Button(_, Direction::Press)))
            .count();
        let releases = calls
            .iter()
            .filter(|call| matches!(call, Call::Button(_, Direction::Release)))
            .count();
        let steps = calls
            .iter()
            .filter(|call| matches!(call, Call::DragStep(..)))
            .count();

        assert_eq!(presses, 1);
        assert_eq!(releases, 1, "the left button must not be left down");
        assert!(
            steps > 0 && steps < DRAG_STEPS as usize,
            "the drag must stop part way, not before it started or after it ended: {steps}"
        );

        // What the caller is told: input was dispatched and the action did not finish.
        let receipt = wire::Receipt::derive(done.posted, false, None, done.timings);
        assert_eq!(receipt.dispatch, wire::Dispatch::Partial);

        clock.disarm();
    }

    // A cancelled paste has two things to put back, and the clipboard is the one
    // the operator notices: their own text must survive being interrupted.
    #[test]
    fn a_pause_during_a_paste_releases_the_modifier_and_restores_the_clipboard() {
        let clock = PauseOnSleep::new(1); // the pasteboard settle, before the keystroke
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        clock.arm(&gate);
        gate.admit(&running("paste", Some(1))).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(Some("the owner clipboard")));
        let outcome = paste_seq(&mut platform, "pasted by compux");
        let done = gate.finish();

        assert_eq!(outcome, Err("cancelled".to_string()));
        assert!(done.cancelled);
        assert_eq!(
            platform.inner().clipboard.as_deref(),
            Some("the owner clipboard"),
            "a cancelled paste must not keep the user's clipboard"
        );
        assert!(platform.inner().down.is_empty(), "nothing may be left held");

        clock.disarm();
    }

    // Every wait this sidecar owns returns at its next checkpoint rather than
    // running its argument out, and says which one it was.
    #[test]
    fn a_pause_during_a_wait_returns_at_once() {
        let clock = PauseOnSleep::new(1);
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        clock.arm(&gate);
        gate.admit(&running("wait", None)).unwrap();

        let outcome = wait(&json!({"ms": 20_000}), &gate);

        assert_eq!(outcome, Err(Failure::from("cancelled")));
        clock.disarm();
    }

    // The worker's own answer, end to end: admit, run, refuse, report. A paused
    // click never reaches the screen and says so in one word the caller can act on.
    #[test]
    fn a_paused_click_is_refused_with_a_receipt_that_says_nothing_was_sent() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let paused = gate.control(wire::ControlAction::Pause, None);

        let mut request = running("left_click", Some(1));
        request.authorization_generation = Some(paused.authorization_generation);

        let frame = serve(
            &request,
            &gate,
            &capture::Emitter::new(),
            &mut idle_worker(&gate),
        );

        assert_eq!(frame["type"], json!("response"));
        assert_eq!(frame["request_id"], json!("r1"));
        assert_eq!(frame["ok"], json!(false));
        assert_eq!(frame["error"], json!("paused"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert_eq!(frame["sidecar_generation"], json!("boot-1"));
        assert_eq!(frame["session_generation"], json!(1));
    }

    // The handshake, through the worker: the one response that names the version,
    // carries the boot identity, and never carries an authorization generation.
    #[test]
    fn the_handshake_answers_with_this_boot_and_this_version() {
        let gate = Gate::new("boot-xyz".to_string(), Arc::new(SystemClock::new()));
        let mut request = running("hello", None);
        request.sidecar_generation = None;
        request.session_generation = None;
        request.authorization_generation = None;

        let frame = serve(
            &request,
            &gate,
            &capture::Emitter::new(),
            &mut idle_worker(&gate),
        );

        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["protocol_version"], json!(PROTOCOL_VERSION));
        assert_eq!(frame["sidecar_generation"], json!("boot-xyz"));
        assert_eq!(frame["session_generation"], json!(1));
        assert_eq!(frame["compux_version"], json!(env!("CARGO_PKG_VERSION")));
        assert!(frame["actions"].is_array());
        assert_eq!(
            frame["capabilities"]["input_methods"],
            if cfg!(any(target_os = "macos", target_os = "linux")) {
                json!(["foreground_hid", "ax"])
            } else {
                json!(["foreground_hid"])
            },
            "a caller reads which methods this build really has"
        );
        assert_eq!(
            frame["capabilities"]["controls"],
            json!(["pause", "resume", "release"])
        );
        assert!(
            frame.get("authorization_generation").is_none(),
            "the gate publishes that in an acknowledgement, never here"
        );
        assert!(
            frame.get("receipt").is_none(),
            "hello is read-only and earns no receipt"
        );
    }

    // F2: the post-action check image is taken INSIDE the action, so an action can
    // land every one of its events and then fail on its own screenshot. Reporting
    // that as `partial` would have Fermix call it `unknown`, when the truth is that
    // the click happened and only the verification did not.
    #[test]
    fn a_click_whose_own_check_image_failed_still_reports_the_input_as_sent() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let request = running("left_click", Some(1));
        gate.admit(&request).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(None));
        click_seq(&mut platform, 10, 20, Button::Left, 1, &[]).unwrap();
        gate.input_complete(); // exactly where `click` latches it, before `post`
        let done = gate.finish();

        // ... and then `post` failed on its own capture.
        let frame = reply(
            &request,
            &gate,
            Err(Failure::from("capture_stalled")),
            done,
            None,
        );

        assert_eq!(frame["ok"], json!(false));
        assert_eq!(frame["error"], json!("capture_stalled"));
        assert_eq!(
            frame["receipt"]["dispatch"],
            json!("sent"),
            "the input landed; only the check did not"
        );
    }

    // The same shape for the one refusal that can only arise AFTER the click: the
    // check image is captured inside the action, after `gate.input_complete()`, so
    // a frame that cannot be explained fails an action whose input has already
    // landed. `dispatch: sent` with `error: capture_geometry_mismatch` is therefore
    // a real combination, and a caller's sentence for that code may not claim
    // nothing was sent. The numbers ride in `detail`, because that refusal has to
    // be diagnosable from a bug report with nothing else in it.
    #[test]
    fn a_geometry_mismatch_on_the_check_image_still_reports_the_click_as_sent() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let request = running("left_click", Some(1));
        gate.admit(&request).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(None));
        click_seq(&mut platform, 10, 20, Button::Left, 1, &[]).unwrap();
        gate.input_complete();
        let done = gate.finish();

        let facts = MonitorFacts {
            x: 0,
            y: 0,
            width: 1512,
            height: 982,
            scale_factor: 2.0,
        };
        let detail = geometry::measure(&facts, 2268, 1473, Host::MacOs)
            .expect_err("1.5 pixels per point is neither the mode's 2 nor 1");
        let frame = reply(
            &request,
            &gate,
            Err(Failure::new("capture_geometry_mismatch", detail)),
            done,
            None,
        );

        assert_eq!(frame["ok"], json!(false));
        assert_eq!(frame["error"], json!("capture_geometry_mismatch"));
        assert_eq!(
            frame["receipt"]["dispatch"],
            json!("sent"),
            "the click landed before the check image was taken"
        );

        let detail = frame["detail"].as_str().expect("the numbers");
        assert!(detail.contains("frame 2268x1473"), "{detail}");
        assert!(detail.contains("display 1512x982 points"), "{detail}");
        assert!(detail.contains("mode scale 2.0000"), "{detail}");
        assert!(detail.contains("1.5000 across"), "{detail}");
    }

    // The complement, so the latch cannot simply be "always sent": a sequence that
    // stopped part way is still `partial`.
    #[test]
    fn a_click_that_stopped_part_way_still_reports_partial() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let request = running("left_click", Some(1));
        gate.admit(&request).unwrap();

        let mut platform = Gated::new(&gate, Recorder::new(None));
        platform.move_mouse(10, 20).unwrap();
        // the sequence failed here, so nothing latched
        let done = gate.finish();

        let frame = reply(
            &request,
            &gate,
            Err(Failure::from("click: refused")),
            done,
            None,
        );

        assert_eq!(frame["receipt"]["dispatch"], json!("partial"));
    }

    // The capture rail answers in its own family, with no envelope and no id, so
    // the one client that reads it keeps working unchanged.
    #[test]
    fn a_capture_verb_answers_in_the_ack_family_at_this_version() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let mut request = running("observe_stop", None);
        request.sidecar_generation = None;
        request.session_generation = None;
        request.authorization_generation = None;

        let frame = serve(
            &request,
            &gate,
            &capture::Emitter::new(),
            &mut idle_worker(&gate),
        );

        assert_eq!(frame["type"], json!("ack"));
        assert_eq!(frame["action"], json!("observe_stop"));
        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["protocol_version"], json!(PROTOCOL_VERSION));
        assert!(frame.get("request_id").is_none());
        assert!(frame.get("sidecar_generation").is_none());
        assert!(frame.get("receipt").is_none());
    }

    // --- M42 slice 4: a control has a name -----------------------------------
    //
    // Every one of these runs through `serve`, so it exercises the same path a
    // real request takes — admit, act, reply — against a scripted application.
    // Nothing here makes an accessibility call, so they hold on a host with no
    // grant, which is every host these were written on.

    /// The scripted application every test below reads.
    const FIXTURE_PID: i32 = 4711;

    fn frame_at(x: f64, y: f64) -> ax::Frame {
        ax::Frame {
            x,
            y,
            w: 80.0,
            h: 24.0,
        }
    }

    /// Two controls: a pressable button and a settable field, the two capabilities
    /// the slice adds.
    fn fixture_app() -> Rc<ax::Recorder> {
        ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted::button("Save", frame_at(100.0, 200.0)),
                ax::Scripted::field("Name", "ada", frame_at(100.0, 260.0)),
            ],
        )
    }

    /// A worker holding one `elements` reply over a scripted application, and the
    /// observation that names it.
    fn worker_listing(gate: &Gate, recorder: &Rc<ax::Recorder>) -> (Worker, Observation) {
        let platform: Rc<dyn ax::Ax> = recorder.clone();
        let mut worker = worker_with(gate, platform.clone());

        let found = tree::walk(&platform, FIXTURE_PID, gate.clock().as_ref(), u64::MAX);
        let entries: Vec<ax::Entry> = found
            .nodes
            .into_iter()
            .enumerate()
            .map(|(index, node)| ax::Entry {
                reference: ax::reference_for(index),
                role: node.role,
                secure: node.secure,
                under: node.under.map(ax::reference_for),
                element: node.element,
            })
            .collect();

        let facts = MonitorFacts {
            x: 0,
            y: 0,
            width: 1512,
            height: 982,
            scale_factor: 2.0,
        };
        let measured = geometry::Measurement {
            frame_w: 3024,
            frame_h: 1964,
            pixels_per_point: 2.0,
        };
        let geom = Geometry::from_facts(&facts, measured, Host::MacOs);
        let region = Region::full(&geom);
        let sent = crop_rect(&geom, &region).sent_dims();

        let observation = worker.observations.mint(observation::Minting {
            kind: Kind::Semantic,
            display_id: 424_242,
            facts,
            geometry: geom,
            region,
            sent,
            view_hash: None,
            elements: Some(Rc::new(ax::Elements::new(
                FIXTURE_PID,
                recorder.started_at(),
                entries,
            ))),
        });

        (worker, observation)
    }

    /// One request addressed at a control of that observation.
    ///
    /// Each gets its own sequence number, increasing across the whole test module:
    /// the gate refuses a repeat as `stale_mutation`, so a test that served twice
    /// would otherwise be reading that instead of what it is about.
    fn at_element(action: &str, observation: &Observation, reference: &str) -> wire::Request {
        static NEXT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = NEXT_SEQ.fetch_add(1, Ordering::SeqCst) + 1;

        let mut request = running(action, Some(seq));
        request.observation_id = Some(observation.id.clone());
        request.element_ref = Some(reference.to_string());
        request.body = json!({
            "action": action,
            "observation_id": observation.id,
            "element_ref": reference,
        });
        request
    }

    /// A `set_value` of that text at a control of that observation.
    fn set_to(observation: &Observation, reference: &str, value: &str) -> wire::Request {
        let mut request = at_element("set_value", observation, reference);
        request.body["value"] = json!(value);
        request
    }

    fn served(request: &wire::Request, gate: &Gate, worker: &mut Worker) -> Value {
        serve(request, gate, &capture::Emitter::new(), worker)
    }

    // The happy path, and every fact the receipt is for: it went through the
    // accessibility API, one press reached the control the caller named, and the
    // foreground did not move — which is the promise the whole action exists for.
    #[test]
    fn a_press_reaches_the_named_control_without_taking_the_foreground() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);

        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["receipt"]["dispatch"], json!("sent"));
        assert_eq!(frame["receipt"]["input_method"], json!("ax"));
        assert_eq!(
            frame["receipt"]["effect"],
            json!("not_observed"),
            "a clean accessibility return is a dispatch result, not an effect"
        );
        assert_eq!(frame["receipt"]["foreground_changed"], json!(false));
        assert_eq!(
            frame["receipt"]["observation_id_before"],
            json!(observation.id)
        );

        let performed = app.performed();
        assert_eq!(performed.len(), 1, "exactly one press: {performed:?}");
        assert_eq!(performed[0].1, ax::PRESS);
    }

    // The foreground moving is a background-contract violation, and this slice
    // reports it truthfully rather than hiding it or failing the action.
    #[test]
    fn a_press_that_moved_the_foreground_says_so() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        // Another application takes the front WHILE the press runs.
        app.takes_foreground(Some(FIXTURE_PID + 1));
        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);

        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["receipt"]["foreground_changed"], json!(true));

        // A platform that will not say leaves the field OFF THE WIRE. Publishing
        // `false` there would assert the background contract held on evidence this
        // build does not have — and the support matrix is filled in from this
        // field, so a guess in it is a qualification nobody earned.
        app.set_frontmost(None);
        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        assert!(
            frame["receipt"].get("foreground_changed").is_none(),
            "an unanswered question is not a false: {}",
            frame["receipt"]
        );
    }

    // The receipt must not say a message went out when the platform says it never
    // left. Every host without the Accessibility grant answers exactly this, and
    // the Linux stub answers it for every call, so `dispatch: sent` there would be
    // the ordinary case rather than the rare one.
    #[test]
    fn a_message_the_platform_never_sent_reports_nothing_sent() {
        for (label, refusal) in [
            ("the accessibility API is disabled", "AXError -25211"),
            ("the element is gone", "AXError -25202"),
            (
                "no accessibility API at all",
                "not supported on this platform",
            ),
        ] {
            let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
            let app = fixture_app();
            let (mut worker, observation) = worker_listing(&gate, &app);
            app.fail_next(ax::Refusal::new(false, refusal));

            let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);

            assert_eq!(frame["error"], json!("ax_action_failed"), "{label}");
            assert_eq!(
                frame["receipt"]["dispatch"],
                json!("not_sent"),
                "{label}: the platform says nothing left this process"
            );
            assert_eq!(frame["receipt"]["effect"], json!("unknown"), "{label}");
        }
    }

    // The complement, so the rule cannot simply be "never sent": the one refusal
    // that arrives AFTER the message went out still reports `sent`, and a clean
    // return does too.
    #[test]
    fn a_message_that_did_go_out_reports_sent_however_it_ended() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        app.fail_next(ax::Refusal::new(true, "AXError -25204"));
        let timed_out = served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        assert_eq!(timed_out["receipt"]["dispatch"], json!("sent"));

        let landed = served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        assert_eq!(landed["receipt"]["dispatch"], json!("sent"));
        assert_eq!(landed["ok"], json!(true));
    }

    // A control that does not list `AXPress` is REFUSED, and the helper never
    // clicks it instead. Which of the two to send is the caller's decision; a
    // silent substitution is the fallback this build refuses to have.
    #[test]
    fn a_control_that_cannot_be_pressed_is_refused_and_never_clicked_instead() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        // e2 is the text field: settable, and it lists no press action.
        let frame = served(&at_element("press", &observation, "e2"), &gate, &mut worker);

        assert_eq!(frame["error"], json!("ax_action_unsupported"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert!(
            frame["detail"].as_str().unwrap().contains("click it"),
            "the sentence must name the next move: {}",
            frame["detail"]
        );
        assert!(app.performed().is_empty(), "nothing may have been sent");
    }

    // A disabled control says so, and says not to retry: the model that cannot
    // see the greyed-out button invents a reason it is missing.
    #[test]
    fn a_disabled_control_is_refused_by_name() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.change(0, |element| element.enabled = false);

        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);

        assert_eq!(frame["error"], json!("element_disabled"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert!(app.performed().is_empty());
    }

    // An accessibility action reaches a control BY NAME. It moves no pointer, it
    // maps no coordinate, and it has no business asking the OS which screens
    // exist — so on a host with no display session at all it must still work.
    //
    // This is the regression guard for a real defect: the view was resolved
    // eagerly, so `press` asked for a display before it had even revalidated the
    // control, and a headless host refused every accessibility action with a
    // display error.
    #[test]
    fn an_accessibility_action_needs_no_display_at_all() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        worker.displays = ScriptedDisplays::none();

        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);

        assert_eq!(frame["ok"], json!(true), "{frame}");
        assert_eq!(frame["receipt"]["dispatch"], json!("sent"));
        assert_eq!(frame["receipt"]["input_method"], json!("ax"));
    }

    // And the refusals keep their own names. A control that is gone, disabled, or
    // cannot take the message sent NOTHING — so what the caller is told must not
    // depend on the state of a screen none of them touch.
    #[test]
    fn a_refusal_that_sent_nothing_is_never_masked_by_a_display_error() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));

        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        worker.displays = ScriptedDisplays::none();
        app.set_started_at(None);

        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        assert_eq!(frame["error"], json!("stale_element"), "{frame}");
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));

        // A semantic check re-reads the control and nothing else, so it needs no
        // display either.
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        worker.displays = ScriptedDisplays::none();

        let mut checked = at_element("press", &observation, "e1");
        checked.check = wire::Check::Semantic;
        checked.body["check"] = json!("semantic");

        let frame = served(&checked, &gate, &mut worker);
        assert_eq!(frame["ok"], json!(true), "{frame}");
        assert_eq!(frame["element_after"]["present"], json!(true));
    }

    // Three ways a reference stops naming what it named, and one answer, because
    // the next move is the same for all three: take the list again.
    #[test]
    fn a_reference_that_no_longer_names_what_it_named_is_stale() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));

        // The application quit. A pid alone would not catch this: the next process
        // to get 4711 would answer for it.
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.set_started_at(None);
        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        assert_eq!(frame["error"], json!("stale_element"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));

        // The pid was REUSED by another process: alive, and a different start time.
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.set_started_at(Some(9_999_000_000));
        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        assert_eq!(frame["error"], json!("stale_element"));

        // The reference now answers a different role, so it is not the control the
        // caller was shown.
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.change(0, |element| element.role = "AXTextField".to_string());
        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        assert_eq!(frame["error"], json!("stale_element"));
        assert!(frame["detail"].as_str().unwrap().contains("AXTextField"));

        assert!(app.performed().is_empty(), "none of these sent anything");
    }

    // On Linux a reference is also a place in the tree. Two candidates for it, or
    // one that stays unreadable, are refusals of their own, and nothing is sent.
    #[test]
    fn a_reference_with_two_candidates_or_no_readable_one_is_refused_by_name() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        for (identity, code) in [
            (ax::Identity::Ambiguous, "ambiguous_element"),
            (ax::Identity::Unknown, "identity_unknown"),
        ] {
            let app = fixture_app();
            let (mut worker, observation) = worker_listing(&gate, &app);
            app.identifies_as(0, identity);

            let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);

            assert_eq!(frame["error"], json!(code), "{frame}");
            assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
            assert!(app.performed().is_empty());
        }
    }

    // A reference nobody minted, and one against an image that listed no controls
    // at all. Both are `stale_element`: the next move is to take a list.
    #[test]
    fn a_reference_this_observation_never_listed_is_refused() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(&at_element("press", &observation, "e9"), &gate, &mut worker);
        assert_eq!(frame["error"], json!("stale_element"));
        assert!(frame["detail"].as_str().unwrap().contains("2 controls"));

        // A plain screenshot lists no controls, so it holds no references — minted
        // into the SAME table, so it is a second observation and not the first one
        // under another name.
        let image = worker.observations.mint(observation::Minting {
            kind: Kind::Image,
            display_id: observation.display_id,
            facts: observation.facts,
            geometry: observation.geometry,
            region: observation.region,
            sent: (observation.sent_w, observation.sent_h),
            elements: None,
            view_hash: None,
        });
        assert_ne!(image.id, observation.id);

        let frame = served(&at_element("press", &image, "e1"), &gate, &mut worker);
        assert_eq!(frame["error"], json!("stale_element"));
        assert!(frame["detail"]
            .as_str()
            .unwrap()
            .contains("listed no controls"));
    }

    // The read-back is the whole of `verified`, and it is the only thing in this
    // build that earns it.
    #[test]
    fn a_set_value_verifies_only_when_the_read_back_matches() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(&set_to(&observation, "e2", "grace"), &gate, &mut worker);

        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["receipt"]["effect"], json!("verified"));
        assert_eq!(frame["receipt"]["input_method"], json!("ax"));
        assert_eq!(frame["verified"], json!(true));
        assert_eq!(frame["value"], json!("grace"), "what the field holds now");
    }

    // A field that stores something else — a formatter, a mask, a control that
    // rejected part of it — is `not_observed`, never `verified`.
    #[test]
    fn a_set_value_the_field_changed_is_not_verified() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.writes_instead("GRACE");

        let frame = served(&set_to(&observation, "e2", "grace"), &gate, &mut worker);

        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["receipt"]["effect"], json!("not_observed"));
        assert_eq!(frame["verified"], json!(false));
        assert_eq!(frame["value"], json!("GRACE"));
    }

    // A secure field reads back masked, so it can never verify — and its value is
    // withheld rather than published as a row of bullets.
    #[test]
    fn a_secure_field_never_verifies_and_never_reports_its_value() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        // A password field keeps the ORDINARY text-field role and is told apart by
        // its SUBROLE. Setting the role here instead is the mistake that published
        // every password field's value, so the fixture models the real shape.
        app.change(1, |element| {
            element.subrole = Some(ax::SECURE_TEXT_FIELD.to_string());
        });
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(&set_to(&observation, "e2", "hunter2"), &gate, &mut worker);

        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["receipt"]["effect"], json!("not_observed"));
        assert_eq!(frame["verified"], json!(false));
        assert!(
            frame.get("value").is_none(),
            "a secure field's value never reaches the wire"
        );
    }

    // A field can turn secure after the walk that listed it as plain: the write's
    // own revalidation is the fresher read, and it decides. Nothing is read back,
    // and nothing it holds reaches the wire.
    #[test]
    fn a_field_that_turned_secure_since_the_listing_is_never_read_back() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.change(1, |element| {
            element.subrole = Some(ax::SECURE_TEXT_FIELD.to_string());
        });
        // The listing read it while it was plain.
        let listed = app.values_read();

        let frame = served(&set_to(&observation, "e2", "hunter2"), &gate, &mut worker);

        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["receipt"]["effect"], json!("not_observed"));
        assert_eq!(frame["verified"], json!(false));
        assert!(frame.get("value").is_none(), "{frame}");
        assert_eq!(app.values_read(), listed, "the field was read back");
    }

    #[test]
    fn a_control_whose_value_is_not_settable_is_refused() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        // e1 is the button: pressable, and its value is not settable.
        let frame = served(&set_to(&observation, "e1", "nope"), &gate, &mut worker);

        assert_eq!(frame["error"], json!("ax_action_unsupported"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert!(frame["detail"].as_str().unwrap().contains("type or paste"));
    }

    // M6: some fields show a written value a moment after the write — a formatter,
    // a binding that updates on the next turn of the run loop. One read straight
    // after it called a write that worked unobserved, so the read-back is taken
    // again every 50 ms until it matches.
    #[test]
    fn a_set_value_reads_back_until_a_slow_field_shows_the_value() {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.commits_after(3);

        let frame = served(&set_to(&observation, "e2", "grace"), &gate, &mut worker);

        assert_eq!(frame["verified"], json!(true));
        assert_eq!(frame["receipt"]["effect"], json!("verified"));
        assert_eq!(frame["value"], json!("grace"));
        assert_eq!(
            frame["receipt"]["timings_ms"]["settle"],
            json!(150),
            "three waits of 50 ms, and not one more once it matched"
        );
    }

    // A field that never shows it is read for 400 ms and no longer, and the reply
    // says what it holds instead.
    #[test]
    fn a_set_value_stops_reading_back_after_four_hundred_milliseconds() {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.commits_after(1_000);

        let frame = served(&set_to(&observation, "e2", "grace"), &gate, &mut worker);

        assert_eq!(frame["ok"], json!(true));
        assert_eq!(frame["verified"], json!(false));
        assert_eq!(frame["receipt"]["effect"], json!("not_observed"));
        assert_eq!(frame["value"], json!("ada"), "what it still holds");
        assert_eq!(frame["receipt"]["timings_ms"]["settle"], json!(400));
    }

    // The wait goes through the gate, so a pause during it stops it at once. The
    // write had already gone out, and the receipt says so and claims no effect.
    #[test]
    fn a_pause_while_reading_back_stops_the_wait_and_reports_the_write_sent() {
        let clock = PauseOnSleep::new(1);
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        clock.arm(&gate);
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.commits_after(1_000);
        let platform: Rc<dyn ax::Ax> = app.clone();
        let before = platform.messages();

        let frame = served(&set_to(&observation, "e2", "grace"), &gate, &mut worker);
        clock.disarm();

        assert_eq!(frame["receipt"]["dispatch"], json!("sent"));
        assert_eq!(frame["receipt"]["effect"], json!("not_observed"));
        assert_eq!(frame["verified"], json!(false));
        assert_eq!(app.performed().len(), 1, "one write, never repeated");
        // The revalidation (its facts, and whether the value may be written), the
        // foreground before and after, the write, and ONE read back — not the eight
        // more a 400 ms wait would have taken. Then the look for what the write
        // opened: the application's two children by count and page, and each one's
        // facts and child count.
        assert_eq!(platform.messages() - before, 6 + 2 + 2 * 2);
    }

    // M2, through the actions rather than the arithmetic: a write is sent with half
    // of what is left of its budget once the control has been revalidated, and
    // never with less than a read gets, however slow the revalidation was.
    #[test]
    fn an_accessibility_write_gets_half_of_what_is_left_and_never_less_than_a_read() {
        let press = |observation: &Observation| at_element("press", observation, "e1");
        let set = |observation: &Observation| set_to(observation, "e2", "grace");

        assert_eq!(sent_timeout(press, 0), 1_000, "revalidated at once");
        assert_eq!(
            sent_timeout(press, 600),
            700,
            "after 600 ms of revalidation"
        );
        assert_eq!(sent_timeout(set, 600), 700);
        assert_eq!(
            sent_timeout(set, 1_800),
            ax::READ_TIMEOUT_MS,
            "never less than a read"
        );
    }

    /// The messaging timeout the one write a request made was sent with, when every
    /// read of a control costs `read_ms`.
    fn sent_timeout(request: impl Fn(&Observation) -> wire::Request, read_ms: u64) -> u64 {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.costs_per_node(read_ms, clock.clone());

        let frame = served(&request(&observation), &gate, &mut worker);

        assert_eq!(frame["ok"], json!(true), "{frame}");
        let performed = app.performed();
        assert_eq!(performed.len(), 1, "{performed:?}");
        performed[0].2
    }

    // The case the receipt exists for: the message went out and the answer never
    // came. Nothing may repeat it, and the receipt says so in the only two words
    // that mean it — sent, and unknown.
    #[test]
    fn an_accessibility_call_that_timed_out_reports_sent_and_unknown() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.fail_next(ax::Refusal::new(true, "AXError -25204"));

        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);

        assert_eq!(frame["error"], json!("ax_timed_out"));
        assert_eq!(frame["receipt"]["dispatch"], json!("sent"));
        assert_eq!(frame["receipt"]["effect"], json!("unknown"));
        assert_eq!(frame["receipt"]["input_method"], json!("ax"));
    }

    // A refusal the application made after receiving the message is NOT
    // `stale_element`, which promises nothing was sent.
    #[test]
    fn an_application_that_refused_the_message_is_not_reported_as_stale() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.fail_next(ax::Refusal::new(false, "AXError -25200"));

        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);

        assert_eq!(frame["error"], json!("ax_action_failed"));
        assert!(frame["detail"].as_str().unwrap().contains("-25200"));
    }

    // A reader that stopped before sending, for a reason the contract names (a
    // range it cannot move in, a value no control takes), keeps that code, and
    // the receipt says nothing went out.
    #[test]
    fn a_refusal_the_reader_names_keeps_its_code_and_nothing_was_sent() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.fail_next(ax::Refusal::unsent(
            "state_unreadable",
            "its scroll bar's range is no range",
        ));

        let frame = served(&at_element("press", &observation, "e1"), &gate, &mut worker);

        assert_eq!(frame["error"], json!("state_unreadable"), "{frame}");
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"), "{frame}");
        assert!(frame["detail"].as_str().unwrap().contains("no range"));
    }

    // Through the gate exactly like a click: a pause stops it, and the receipt
    // says nothing was sent.
    #[test]
    fn a_paused_press_is_refused_with_nothing_sent() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let paused = gate.control(wire::ControlAction::Pause, None);
        let mut request = at_element("press", &observation, "e1");
        request.authorization_generation = Some(paused.authorization_generation);

        let frame = served(&request, &gate, &mut worker);

        assert_eq!(frame["error"], json!("paused"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert!(app.performed().is_empty());
    }

    // The point of addressing a pointer action by control: the bounds are read
    // AGAIN at the moment it acts, so a control that moved is clicked where it is
    // now rather than where it was listed.
    #[test]
    fn a_click_by_reference_reads_the_bounds_again_at_the_moment_it_acts() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (worker, observation) = worker_listing(&gate, &app);

        let request = at_element("left_click", &observation, "e1");
        let element = addressed_element(&request, &observation).expect("a listed control");
        let listed = aim(&request, &observation, element, &worker).expect("a control to aim at");
        assert_eq!(listed, (140, 212), "the centre of the listed frame");

        app.change(0, |element| element.frame = Some(frame_at(500.0, 600.0)));
        let moved = aim(&request, &observation, element, &worker).expect("still a control to aim");
        assert_eq!(moved, (540, 612), "the centre of where it is NOW");
    }

    // A control that is gone is refused rather than clicked at the place it used
    // to be, which is the one thing a stale coordinate would do.
    #[test]
    fn a_click_by_reference_on_a_gone_control_is_refused_rather_than_aimed() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.set_started_at(None);

        let frame = served(
            &at_element("left_click", &observation, "e1"),
            &gate,
            &mut worker,
        );

        assert_eq!(frame["error"], json!("stale_element"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert_eq!(
            frame["receipt"]["input_method"],
            json!("foreground_hid"),
            "a click by reference is still the pointer"
        );
    }

    // Every refusal above leaves the references exactly as it found them: a
    // refused action must not leak a retain, and must not release one the
    // observation still holds.
    #[test]
    fn a_refused_action_changes_nothing_about_what_is_retained() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let app = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        // The walk held the application to read its children, and gave it back.
        assert_eq!(app.counts(), (3, 1, 2));

        app.change(0, |element| element.enabled = false);
        served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        served(&at_element("press", &observation, "e9"), &gate, &mut worker);
        served(&set_to(&observation, "e1", "nope"), &gate, &mut worker);

        assert_eq!(app.counts(), (3, 1, 2), "a refusal is not a release");

        drop(observation);
        drop(worker);
        assert_eq!(app.counts(), (3, 3, 0), "and everything goes in the end");
    }

    // The shape of one element on the wire. Written out in full because it is the
    // contract the caller's summary is built from, and because every field here
    // was read from the control itself rather than guessed from its role.
    #[test]
    fn an_element_is_published_with_everything_needed_to_name_and_judge_it() {
        // The Save button inside a named toolbar inside a named document; the field
        // beside the document, with no ancestry of its own.
        let app = ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted::group("Document", &[1]),
                ax::Scripted::group("Toolbar", &[2]),
                ax::Scripted::button("Save", frame_at(100.0, 200.0)),
                ax::Scripted::field("Name", "ada", frame_at(100.0, 260.0)),
            ],
        );
        let platform: Rc<dyn ax::Ax> = app.clone();
        let clock = SystemClock::new();

        // A 1x display, so the bounds in this reply's pixels are the same numbers
        // as the frame the walk read; the mapping itself is `geometry`'s to prove.
        let facts = MonitorFacts {
            x: 0,
            y: 0,
            width: 1366,
            height: 768,
            scale_factor: 1.0,
        };
        let geom = Geometry::from_facts(
            &facts,
            geometry::Measurement {
                frame_w: 1366,
                frame_h: 768,
                pixels_per_point: 1.0,
            },
            Host::MacOs,
        );
        let region = Region::full(&geom);

        let found = tree::walk(&platform, FIXTURE_PID, &clock, u64::MAX);
        let button = element_value(
            &found.nodes[0],
            Some("e1"),
            Some((140, 212)),
            &geom,
            &region,
        );
        let field = element_value(
            &found.nodes[1],
            Some("e2"),
            Some((140, 272)),
            &geom,
            &region,
        );

        assert_eq!(
            button,
            json!({
                "element_ref": "e1",
                "role": "button",
                "native_role": "AXButton",
                "label": "Save",
                "enabled": true,
                "verbs": ["press"],
                "bounds": { "x": 100, "y": 200, "w": 80, "h": 24 },
                "path": ["Document", "Toolbar"],
                "x": 140,
                "y": 212
            })
        );

        assert_eq!(
            field["verbs"],
            json!(["set_value"]),
            "no press of its own, and a value that may be written"
        );
        assert_eq!(field["value"], json!("ada"));
        assert!(
            field.get("path").is_none(),
            "an element with no ancestry carries no path"
        );

        // A reply that cannot resolve references offers none. Listing `e1` beside
        // a table that would answer `stale_element` the moment it was used is a
        // promise this build cannot keep.
        let unnamed = element_value(&found.nodes[0], None, Some((140, 212)), &geom, &region);
        assert!(unnamed.get("element_ref").is_none());
        assert!(
            unnamed.get("verbs").is_none(),
            "nothing may be offered on a control that cannot be named"
        );
        assert_eq!(unnamed["role"], json!("button"), "it is still listed");
        assert_eq!(
            unnamed["x"],
            json!(140),
            "with the point it can be clicked at"
        );
    }

    /// A desktop whose one application, this test process (so `/proc` can vouch
    /// for it and the listing can hand out references), has an ACTIVE window with
    /// a Save button in it.
    #[cfg(target_os = "linux")]
    fn linux_desktop_with_a_save_button() -> linux::fake::Desktop {
        use crate::linux::fake::{Desktop, Node, ACTIVE, ENABLED, SENSITIVE, SHOWING};

        const APP: &str = ":1.10";
        let me = std::process::id();
        let mut desktop = Desktop::with_applications(vec![(
            APP,
            me,
            Node::of(75).named("fixture").holding(APP, &["/w"]),
        )]);
        desktop.put(
            APP,
            "/w",
            Node {
                states: vec![ACTIVE, ENABLED, SENSITIVE, SHOWING],
                ..Node::of(23)
                    .named("Fixture")
                    .at(0, 0, 800, 600)
                    .holding(APP, &["/w/save"])
            },
        );
        desktop.put(
            APP,
            "/w/save",
            Node::of(43)
                .named("Save")
                .at(100, 200, 80, 24)
                .acting(&[Ok("click")]),
        );
        desktop
    }

    // M86 L1: on Linux `elements` lists the controls of the application whose
    // window reads ACTIVE, read through AT-SPI by the shared walk, with references,
    // verbs, click points and what the walk cost — from a fake desktop, never the
    // one this test runs on.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_elements_lists_the_controls_of_the_application_in_front() {
        use crate::linux::fake::Fake;

        let (fake, bus) = Fake::start(
            linux_desktop_with_a_save_button(),
            Duration::from_millis(200),
        );
        let platform: Rc<dyn ax::Ax> = Rc::new(linux::atspi::Real::over(bus));
        let gate = Gate::new("boot-test".to_string(), Arc::new(SystemClock::new()));
        let facts = MonitorFacts {
            x: 0,
            y: 0,
            width: 1366,
            height: 768,
            scale_factor: 1.0,
        };
        let geom = Geometry::from_facts(
            &facts,
            geometry::Measurement {
                frame_w: 1366,
                frame_h: 768,
                pixels_per_point: 1.0,
            },
            Host::Linux,
        );

        let (payload, listed) =
            elements_for(&platform, &geom, &Region::full(&geom), &gate).expect("a listing");

        assert_eq!(
            payload["elements"],
            json!([{
                "element_ref": "e1",
                "role": "button",
                "native_role": "push button",
                "label": "Save",
                "enabled": true,
                "verbs": ["press"],
                "bounds": { "x": 100, "y": 200, "w": 80, "h": 24 },
                "path": ["fixture", "Fixture"],
                "x": 140,
                "y": 212
            }])
        );
        assert_eq!(listed.map(|listed| listed.len()), Some(1));
        let walked = payload["ax_messages"].as_u64().expect("a count");
        assert!(
            walked > 0 && walked < fake.calls().len() as u64,
            "the walk alone is counted, not the lookup of the application in front"
        );
        assert!(payload["ax_activation"]
            .as_str()
            .is_some_and(|note| note.starts_with("read ")));
    }

    /// A desktop whose one application, made with this toolkit, has an ACTIVE
    /// window (or one that is not, where `active` is false) with nothing in it.
    #[cfg(target_os = "linux")]
    fn linux_desktop_with_an_empty_window(
        toolkit: &'static str,
        version: &'static str,
        active: bool,
    ) -> linux::fake::Desktop {
        use crate::linux::fake::{Desktop, Node, ACTIVE, ENABLED, SENSITIVE, SHOWING};

        const APP: &str = ":1.10";
        let mut desktop = Desktop::with_applications(vec![(
            APP,
            std::process::id(),
            Node::of(75)
                .named("app")
                .holding(APP, &["/w"])
                .made_with(toolkit, version),
        )]);
        let mut states = vec![ENABLED, SENSITIVE, SHOWING];
        if active {
            states.push(ACTIVE);
        }
        desktop.put(
            APP,
            "/w",
            Node {
                states,
                ..Node::of(23).named("Window").at(0, 0, 800, 600)
            },
        );
        desktop
    }

    /// `elements` with no parameters, through the worker.
    #[cfg(target_os = "linux")]
    fn list_elements(worker: &mut Worker, gate: &Gate) -> Value {
        let mut list = running("elements", None);
        list.body = json!({"action": "elements"});
        served(&list, gate, worker)
    }

    // An application in front whose toolkit publishes no tree until it is asked
    // (Qt, Chromium), and whose tree stays empty, is refused by name with the
    // switch that would make it publish one. A GTK 3 window with nothing in it is
    // only empty.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_an_application_whose_toolkit_publishes_no_tree_names_its_switch() {
        use crate::linux::fake::Fake;

        for (toolkit, version, switch) in [
            ("Qt", "5.15.3", Some("QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1")),
            ("Chromium", "120.0", Some("ACCESSIBILITY_ENABLED=1")),
            ("GTK", "3.24.41", None),
        ] {
            let desktop = linux_desktop_with_an_empty_window(toolkit, version, true);
            let (_fake, bus) = Fake::start(desktop, Duration::from_millis(200));
            let platform: Rc<dyn ax::Ax> = Rc::new(linux::atspi::Real::over(bus));
            let gate = Gate::new("boot-1".to_string(), StepClock::new());
            let mut worker = worker_with(&gate, platform);

            let listed = list_elements(&mut worker, &gate);

            let Some(switch) = switch else {
                assert_eq!(listed["elements"], json!([]), "{toolkit}: {listed}");
                continue;
            };
            assert_eq!(
                listed["error"],
                json!("toolkit_accessibility_off"),
                "{toolkit}: {listed}"
            );
            let detail = listed["detail"].as_str().unwrap_or_default();
            assert!(detail.contains(switch), "{toolkit}: {detail}");
        }
    }

    // When no window on the accessibility bus reads ACTIVE, the application in
    // front may be one whose toolkit is not on it, and the note says which
    // switches would put it there.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_a_front_window_not_on_the_bus_is_explained_by_the_switches() {
        use crate::linux::fake::Fake;

        let desktop = linux_desktop_with_an_empty_window("GTK", "3.24.41", false);
        let (_fake, bus) = Fake::start(desktop, Duration::from_millis(200));
        let platform: Rc<dyn ax::Ax> = Rc::new(linux::atspi::Real::over(bus));
        let gate = Gate::new("boot-1".to_string(), StepClock::new());
        let mut worker = worker_with(&gate, platform);

        let listed = list_elements(&mut worker, &gate);

        assert_eq!(listed["ok"], json!(true), "{listed}");
        let note = listed["ax_activation"].as_str().unwrap_or_default();
        for switch in [
            "QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1",
            "--force-renderer-accessibility",
            "ACCESSIBILITY_ENABLED=1",
            "GTK 4",
        ] {
            assert!(note.contains(switch), "{switch}: {note}");
        }
    }

    /// A screen no test may take a frame of.
    #[cfg(target_os = "linux")]
    struct NoFrames;

    #[cfg(target_os = "linux")]
    impl Frames for NoFrames {
        fn capture(&self, display_id: u32) -> Result<image::RgbaImage, Failure> {
            panic!("display {display_id} was captured on a session that places nothing")
        }
    }

    // A session that places nothing (Wayland, or no display in reach) still lists
    // its controls: without bounds or click points, without asking for a display
    // or a frame, and in an observation no point can be read off. Its references
    // still act.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_a_session_that_places_nothing_lists_its_controls_without_a_display() {
        use crate::linux::fake::Fake;

        let (fake, bus) = Fake::start(
            linux_desktop_with_a_save_button(),
            Duration::from_millis(200),
        );
        let platform: Rc<dyn ax::Ax> = Rc::new(linux::atspi::Real::over(bus).without_positions());
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let mut worker = Worker {
            displays: ScriptedDisplays::none(),
            ..frames_worker(&gate, platform, Rc::new(NoFrames))
        };

        let mut list = running("elements", None);
        list.body = json!({"action": "elements"});
        let listed = served(&list, &gate, &mut worker);

        assert_eq!(listed["ok"], json!(true), "{listed}");
        let save = &listed["elements"][0];
        assert_eq!(save["label"], json!("Save"), "{listed}");
        assert_eq!(save["verbs"], json!(["press"]));
        for placed in ["bounds", "x", "y"] {
            assert!(
                save.get(placed).is_none(),
                "{placed} on a placeless control: {save}"
            );
        }
        let id = listed["observation_id"].as_str().expect("an observation");
        let observation = worker.observations.resolve(id).expect("held").clone();

        let mut click = at_element("left_click", &observation, "e1");
        click.element_ref = None;
        click.body = json!({"action": "left_click", "x": 0, "y": 0, "observation_id": id});
        let clicked = served(&click, &gate, &mut worker);
        assert_eq!(
            clicked["error"],
            json!("point_outside_observation"),
            "{clicked}"
        );

        let pressed = served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        assert_eq!(pressed["ok"], json!(true), "{pressed}");
        assert_eq!(fake.sent("DoAction"), vec!["/w/save 0".to_string()]);
    }

    // A walk that stopped early SAYS it stopped early. Without that a caller reads
    // a partial tree as the whole one and concludes a control is not there, which
    // is the one wrong answer an element list can give.
    #[test]
    fn a_truncated_walk_is_named_on_the_reply() {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let app = fixture_app();
        let platform: Rc<dyn ax::Ax> = app.clone();

        let viewed = Viewed {
            nodes: Vec::new(),
            owner: None,
            truncated: Some("time"),
            note: Some("read Fixture".to_string()),
            messages: 0,
        };
        let mut payload = json!({ "ok": true, "elements": [] });
        if let (Some(object), Some(truncation)) = (payload.as_object_mut(), viewed.truncated) {
            object.insert("truncated".to_string(), json!(truncation));
        }
        assert_eq!(payload["truncated"], json!("time"));

        // And the budget that produces it is the walk's own, not the caller's.
        app.costs_per_node(WALK_COST_MS, clock.clone());
        let deadline = gate.now_ms().saturating_add(tree::WALK_BUDGET_MS);
        let found = tree::walk(&platform, FIXTURE_PID, gate.clock().as_ref(), deadline);
        assert_eq!(found.truncated, Some("time"));
        assert_eq!(app.walks(), 1);
    }

    /// The whole walk budget for one node, so the second one cannot be reached: the
    /// budget is checked before each node is read.
    const WALK_COST_MS: u64 = tree::WALK_BUDGET_MS;

    // M7: a listing says how many accessibility messages its read sent, beside the
    // fields it always had, so the batching the macOS reader exists for is a number
    // the owner can read on a real application.
    #[test]
    fn a_listing_says_how_many_accessibility_messages_its_read_sent() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let worker = bound_worker(&gate);
        let app = ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted {
                    label: Some("Window 20".to_string()),
                    children: vec![1, 2],
                    ..ax::Scripted::of("AXWindow")
                },
                ax::Scripted::button("Save", frame_at(100.0, 200.0)),
                ax::Scripted::field("Name", "ada", frame_at(100.0, 260.0)),
            ],
        );
        app.windows_are(&[0]);
        let platform: Rc<dyn ax::Ax> = app.clone();
        let windows = ax::application_windows(&platform, FIXTURE_PID);
        let (geom, region) = one_to_one();

        let (payload, listed) = elements_rooted(
            &platform,
            windows[0].handle(),
            &geom,
            &region,
            &gate,
            &worker,
        )
        .expect("a listing of the bound window");

        assert_eq!(payload["elements"].as_array().map(Vec::len), Some(2));
        assert!(listed.is_some());
        // The window: its facts, its child count, one page. Each control: its facts,
        // its value, its write routes, its states and its child count. Their window
        // is asked once whether it selects its children.
        assert_eq!(payload["ax_messages"], json!(3 + 2 * 5 + 1));
    }

    // A read that found nothing still says what it cost.
    #[test]
    fn an_empty_listing_still_says_what_its_read_cost() {
        let (geom, region) = one_to_one();
        let viewed = Viewed {
            nodes: Vec::new(),
            owner: None,
            truncated: None,
            note: Some("Fixture: no accessibility elements".to_string()),
            messages: 12,
        };

        let (payload, listed) = publish_elements(viewed, &geom, &region);

        assert_eq!(payload["ax_messages"], json!(12));
        assert!(listed.is_none());
    }

    // A tree still being built is walked again until it appears, and the reply's
    // cost is the walk whose list it publishes. The empty walks before it are not
    // the cost of that list, and a count that included them made the per-control
    // cost of a Chromium listing look like the batching had not worked.
    #[test]
    fn a_settled_listing_counts_only_the_walk_it_publishes() {
        let gate = Gate::new("boot-1".to_string(), StepClock::new());
        let app = fixture_app();
        app.empty_for(2);
        let platform: Rc<dyn ax::Ax> = app.clone();
        let (geom, region) = one_to_one();
        let target = TargetApp {
            pid: FIXTURE_PID,
            app: "Fixture".to_string(),
        };

        let viewed = read_application(&platform, &target, &geom, &region, &gate, switched_on)
            .expect("a tree that appeared");

        assert_eq!(viewed.nodes.len(), 2);
        assert_eq!(
            viewed.note.as_deref(),
            Some("Fixture: tree appeared after 600ms (AXManualAccessibility activated)")
        );
        assert_eq!(app.walks(), 3, "two empty walks, then the tree");
        // The application: its facts, its child count, one page. Each control: its
        // facts, its value, its write routes, its states and its child count. The
        // application is asked once whether it selects its children.
        assert_eq!(viewed.messages, 3 + 2 * 5 + 1);
    }

    /// An activation that worked, for a test with no application to ask.
    fn switched_on(_pid: i32) -> Result<String, String> {
        Ok("AXManualAccessibility activated".to_string())
    }

    // A password field's value is never asked for, so it cannot reach a reply by
    // any route: the listing names the field and carries no value for it, and the
    // platform was never asked for one.
    #[test]
    fn a_password_fields_value_is_never_read_and_never_published() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let worker = bound_worker(&gate);
        let app = ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted {
                    label: Some("Window 20".to_string()),
                    children: vec![1, 2],
                    ..ax::Scripted::of("AXWindow")
                },
                ax::Scripted::field("Name", "ada", frame_at(100.0, 200.0)),
                ax::Scripted::secure_field("Password", "hunter2", frame_at(100.0, 260.0)),
            ],
        );
        app.windows_are(&[0]);
        let platform: Rc<dyn ax::Ax> = app.clone();
        let windows = ax::application_windows(&platform, FIXTURE_PID);
        let (geom, region) = one_to_one();

        let (payload, _listed) = elements_rooted(
            &platform,
            windows[0].handle(),
            &geom,
            &region,
            &gate,
            &worker,
        )
        .expect("a listing of the bound window");

        let elements = payload["elements"].as_array().expect("a list");
        assert_eq!(elements.len(), 2, "the password field is listed");
        assert_eq!(elements[0]["value"], json!("ada"));
        assert!(elements[1].get("value").is_none(), "{payload}");
        assert!(!payload.to_string().contains("hunter2"), "{payload}");
        assert_eq!(app.values_read(), vec![1], "only the plain field was asked");
    }

    /// A 1x display, so a reply's pixels are the same numbers as the points read.
    fn one_to_one() -> (Geometry, Region) {
        let facts = MonitorFacts {
            x: 0,
            y: 0,
            width: 1366,
            height: 768,
            scale_factor: 1.0,
        };
        let geom = Geometry::from_facts(
            &facts,
            geometry::Measurement {
                frame_w: 1366,
                frame_h: 768,
                pixels_per_point: 1.0,
            },
            Host::MacOs,
        );
        let region = Region::full(&geom);
        (geom, region)
    }

    // M5: Chromium asks for its complete accessibility mode through a two-second
    // debounce after the switch is written, so the empty-tree poll must outlast it
    // or the tree that was about to appear is reported as absent.
    #[test]
    fn the_empty_tree_poll_outlasts_chromiums_two_second_debounce() {
        const CHROMIUM_DEBOUNCE_MS: u64 = 2_000;
        let window = u64::from(AX_SETTLE_POLLS) * AX_SETTLE_POLL_MS;
        assert!(window > CHROMIUM_DEBOUNCE_MS, "{window} ms of polling");
    }

    // --- M42 slice 6: one action with its check, settled, and timed -----------
    //
    // The SCREEN is injected here, the way the accessibility tree is above: a test
    // writes down what each look at the display answers, so which frame a settle
    // keeps, how many looks it takes and what it then encodes are asserted rather
    // than hoped for. Nothing captures, nothing sleeps and nothing is drawn on a
    // real display, which is what lets these run on a host with no grant at all.

    /// A clock a test moves. It advances on every sleep, and a scripted screen
    /// advances it again for what each look costs, so both settle caps — the wall
    /// clock and the sample count — are reachable without waiting for either.
    struct StepClock {
        ms: std::sync::atomic::AtomicU64,
    }

    impl StepClock {
        fn new() -> Arc<StepClock> {
            Arc::new(StepClock {
                ms: std::sync::atomic::AtomicU64::new(0),
            })
        }

        fn advance(&self, ms: u64) {
            self.ms.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl gate::Clock for StepClock {
        fn now_ms(&self) -> u64 {
            self.ms.load(Ordering::SeqCst)
        }

        fn now_ns(&self) -> u128 {
            self.now_ms() as u128 * 1_000_000
        }

        fn now_mach(&self) -> u64 {
            self.now_ms() * 1_000_000
        }

        fn sleep(&self, ms: u64) {
            self.advance(ms);
        }
    }

    /// A screen a test writes down, frame by frame: each look answers the next one
    /// and the last one repeats, and each costs the clock what a real grab would.
    struct ScriptedScreen {
        frames: Vec<image::RgbaImage>,
        taken: std::cell::Cell<usize>,
        cost_ms: u64,
        clock: Arc<StepClock>,
    }

    impl ScriptedScreen {
        fn new(clock: &Arc<StepClock>, shades: &[u8], cost_ms: u64) -> Rc<ScriptedScreen> {
            Rc::new(ScriptedScreen {
                frames: shades.iter().map(|shade| shaded(*shade)).collect(),
                taken: std::cell::Cell::new(0),
                cost_ms,
                clock: clock.clone(),
            })
        }

        fn taken(&self) -> usize {
            self.taken.get()
        }
    }

    impl Frames for ScriptedScreen {
        fn capture(&self, _display_id: u32) -> Result<image::RgbaImage, Failure> {
            let taken = self.taken.get();
            self.taken.set(taken + 1);
            self.clock.advance(self.cost_ms);
            Ok(self.frames[taken.min(self.frames.len() - 1)].clone())
        }
    }

    /// The whole display, in one shade — so which frame was kept is readable off
    /// the pixels the reply carries.
    fn shaded(shade: u8) -> image::RgbaImage {
        image::RgbaImage::from_pixel(
            CHECK_DISPLAY_W,
            CHECK_DISPLAY_H,
            image::Rgba([shade, shade, shade, 255]),
        )
    }

    const CHECK_DISPLAY_W: u32 = 400;
    const CHECK_DISPLAY_H: u32 = 300;

    /// A one-to-one display, so the transform is the identity on every host and a
    /// test can read a rectangle in the reply as the rectangle it asked for.
    fn check_display() -> Display {
        Display {
            facts: MonitorFacts {
                x: 0,
                y: 0,
                width: CHECK_DISPLAY_W,
                height: CHECK_DISPLAY_H,
                scale_factor: 1.0,
            },
            id: 424_242,
        }
    }

    fn check_geometry() -> Geometry {
        let display = check_display();
        let measured =
            geometry::measure(&display.facts, CHECK_DISPLAY_W, CHECK_DISPLAY_H, Host::HERE)
                .expect("a one-to-one frame");

        Geometry::from_facts(&display.facts, measured, Host::HERE)
    }

    /// A worker over a scripted screen, with the display's transform already
    /// measured — so nothing below takes a frame it did not ask for.
    fn screen_worker(gate: &Gate, screen: &Rc<ScriptedScreen>) -> Worker {
        let display = check_display();
        let measured =
            geometry::measure(&display.facts, CHECK_DISPLAY_W, CHECK_DISPLAY_H, Host::HERE)
                .expect("a one-to-one frame");

        let mut worker = frames_worker(gate, Rc::new(ax::Real::new()), screen.clone());
        worker
            .measured
            .remember(display.id, &display.facts, measured);
        worker
    }

    /// An action that has dispatched its input and is about to bring evidence back.
    fn checking(action: &str, check: wire::Check) -> wire::Request {
        let mut request = running(action, Some(1));
        request.check = check;
        request.observation_id = None;
        request.element_ref = None;
        request.body = json!({ "action": action });
        request
    }

    /// One observation over a rectangle of the check display, minted the way a real
    /// reply mints one: from a frame, with the hash of the crop it covers.
    fn viewed(
        worker: &mut Worker,
        gate: &Gate,
        shade: u8,
        region: Region,
    ) -> Result<Observation, Failure> {
        let display = check_display();
        let frame = shaded(shade);
        let geom = measure_geometry(&display, &frame, worker)?;
        let payload = capture_payload_encoded(
            &display,
            geom,
            &frame,
            Rendering {
                requested: Requested::Exactly(region),
                jpeg_quality: None,
                overlays: Overlays::default(),
            },
            gate,
            worker,
        )?;

        let id = payload["observation_id"]
            .as_str()
            .expect("named")
            .to_string();
        Ok(worker
            .observations
            .resolve(&id)
            .expect("just minted")
            .clone())
    }

    /// The display-level view a check test drives `post` with, which is exactly what
    /// `view` builds for a request that names no window.
    fn display_view(worker: &Worker) -> View {
        View {
            on: Some(check_display()),
            index: 0,
            geometry: None,
            frames: worker.frames.clone(),
            bound: None,
        }
    }

    fn crop_region(x: f64, y: f64, w: f64, h: f64) -> Region {
        Region { x, y, w, h }
    }

    /// The shade of the image a reply carries, decoded from the bytes it really
    /// sent — the only way to tell WHICH frame was encoded.
    fn shade_of(payload: &Value) -> u8 {
        let data = payload["data"].as_str().expect("an image");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .expect("base64");
        let decoded = image::load_from_memory(&bytes).expect("an image this build encoded");
        decoded.to_rgba8().get_pixel(1, 1).0[0]
    }

    fn idle_check_gate(clock: &Arc<StepClock>) -> Gate {
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        gate.admit(&checking("left_click", wire::Check::Image))
            .expect("an idle gate admits");
        gate
    }

    // The defect this slice exists for: the frame an action hands back used to be
    // taken the instant the input left, before the application had repainted. The
    // sample that proved the view stable is the one encoded, and nothing is
    // captured after it.
    #[test]
    fn a_settle_keeps_the_frame_that_proved_the_view_stable() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        // Moving, moving, then two looks that agree.
        let screen = ScriptedScreen::new(&clock, &[10, 20, 30, 30, 40], 0);
        let worker = screen_worker(&gate, &screen);
        let geom = check_geometry();

        let settled = settle_view(
            &check_display(),
            &geom,
            &Region::full(&geom),
            None,
            &gate,
            &worker.frames.clone(),
            &worker,
        )
        .expect("a settle with nothing in its way");

        assert_eq!(settled.settle, wire::Settle::Stable);
        assert_eq!(screen.taken(), 4, "it stopped on the look that agreed");
        assert_eq!(
            settled.frame.get_pixel(1, 1).0[0],
            30,
            "the frame kept is the one that proved it, not a fresh one"
        );
    }

    // A caret, a spinner or a playing video never agrees with itself. That is an
    // observation state and not a reason to send the input again, so the cap is
    // reported and the last sample is what comes back.
    #[test]
    fn a_view_that_never_settles_ends_at_the_sample_cap() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        let shades: Vec<u8> = (0..60).collect();
        let screen = ScriptedScreen::new(&clock, &shades, 0);
        let worker = screen_worker(&gate, &screen);
        let geom = check_geometry();

        let settled = settle_view(
            &check_display(),
            &geom,
            &Region::full(&geom),
            None,
            &gate,
            &worker.frames.clone(),
            &worker,
        )
        .expect("a settle that runs out of looks");

        assert_eq!(settled.settle, wire::Settle::Timeout);
        assert_eq!(screen.taken(), MAX_SETTLE_SAMPLES as usize);
    }

    // Both caps bind. On a machine whose captures are slow the wall clock is what
    // runs out first, and a caller's own deadline is built on that promise.
    #[test]
    fn a_settle_whose_looks_are_slow_ends_at_the_wall_clock_cap() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        let shades: Vec<u8> = (0..60).collect();
        let screen = ScriptedScreen::new(&clock, &shades, 400);
        let worker = screen_worker(&gate, &screen);
        let geom = check_geometry();

        let settled = settle_view(
            &check_display(),
            &geom,
            &Region::full(&geom),
            None,
            &gate,
            &worker.frames.clone(),
            &worker,
        )
        .expect("a settle that runs out of time");

        assert_eq!(settled.settle, wire::Settle::Timeout);
        assert!(
            screen.taken() < MAX_SETTLE_SAMPLES as usize,
            "the clock ran out first, after {} looks",
            screen.taken()
        );
        assert!(gate.now_ms() >= SETTLE_CAP_MS);
    }

    // A pause during a settle returns at once — and the truth it leaves behind is
    // the one the whole receipt design is for: the input DID go out, and the
    // evidence did not. Neither fact is allowed to stand in for the other.
    #[test]
    fn a_pause_during_a_settle_returns_at_once_with_the_input_still_sent() {
        let clock = PauseOnSleep::new(1);
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        clock.arm(&gate);
        let request = checking("left_click", wire::Check::Image);
        gate.admit(&request).unwrap();

        // The input landed and was latched before any of this.
        gate.dispatch(Phase::Input, || ()).expect("input posted");
        gate.input_complete();

        let screen = ScriptedScreen::new(&StepClock::new(), &[10, 20, 30], 0);
        let worker = screen_worker(&gate, &screen);

        let refused = post(
            &request,
            Some(display_view(&worker)),
            &gate,
            &mut { worker },
            nothing_named(),
        )
        .expect_err("a pause during the settle");
        assert_eq!(refused.code, "cancelled");

        let done = gate.finish();
        let receipt = receipt(&request, &done, None).expect("a mutation earns one");
        assert_eq!(receipt.dispatch, wire::Dispatch::Sent, "the input went out");
        assert_eq!(receipt.effect, wire::Effect::Unknown);
        assert!(
            receipt.check.is_none(),
            "evidence that could not be obtained is claimed by nobody"
        );

        clock.disarm();
    }

    // What an image check answers: the view the action was aimed in, the whole
    // display when it named none, drawn on and named as an observation of its own.
    #[test]
    fn an_image_check_answers_the_whole_display_when_the_action_named_none() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        let screen = ScriptedScreen::new(&clock, &[10, 10], 0);
        let mut worker = screen_worker(&gate, &screen);
        let request = checking("type", wire::Check::Image);

        let payload = post(
            &request,
            Some(display_view(&worker)),
            &gate,
            &mut worker,
            nothing_named(),
        )
        .expect("a check image");

        assert_eq!(
            payload["mime"],
            json!("image/png"),
            "the display is lossless"
        );
        assert_eq!(payload["width"], json!(CHECK_DISPLAY_W));
        assert_eq!(payload["height"], json!(CHECK_DISPLAY_H));
        assert_eq!(payload["observation_kind"], json!("image"));
        assert!(payload["observation_id"].is_string(), "the check is a view");

        let done = gate.finish();
        let checked = done.check.expect("a check that ran says so");
        assert_eq!(checked.kind, wire::Check::Image);
        assert_eq!(checked.settle, Some(wire::Settle::Stable));
        assert_eq!(
            checked.changed, None,
            "it named no image to compare against"
        );
    }

    // A zoomed action gets its zoom back, not a whole screen it has to find its
    // target in again — and that is what the second round trip used to buy.
    #[test]
    fn an_image_check_of_a_crop_is_that_crop() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        let screen = ScriptedScreen::new(&clock, &[10, 10, 10], 0);
        let mut worker = screen_worker(&gate, &screen);

        let zoomed = viewed(
            &mut worker,
            &gate,
            10,
            crop_region(100.0, 60.0, 120.0, 90.0),
        )
        .expect("a crop to act in");
        let request = checking("left_click", wire::Check::Image);

        let payload = post(
            &request,
            Some(display_view(&worker)),
            &gate,
            &mut worker,
            Acted {
                named: Some(&zoomed),
                executed: Some((150, 90)),
                element: None,
            },
        )
        .expect("a check image");

        assert_eq!(payload["mime"], json!("image/jpeg"), "a crop is compressed");
        assert_eq!(
            payload["region"],
            json!({ "x": 100, "y": 60, "w": 120, "h": 90 }),
            "the rectangle the action was aimed in, mapped through its own transform"
        );
        assert_eq!(payload["width"], json!(120));
    }

    // `changed` is evidence about the VIEW: did what the model looked at move on
    // since the image it acted on. Never a verdict on the action.
    #[test]
    fn a_check_says_whether_the_view_changed_since_the_image_acted_on() {
        for (after, changed) in [(10, false), (200, true)] {
            let clock = StepClock::new();
            let gate = idle_check_gate(&clock);
            let screen = ScriptedScreen::new(&clock, &[after, after], 0);
            let mut worker = screen_worker(&gate, &screen);

            let geom = check_geometry();
            let acted_in =
                viewed(&mut worker, &gate, 10, Region::full(&geom)).expect("an image to act in");
            let request = checking("left_click", wire::Check::Image);

            let payload = post(
                &request,
                Some(display_view(&worker)),
                &gate,
                &mut worker,
                Acted {
                    named: Some(&acted_in),
                    executed: Some((10, 10)),
                    element: None,
                },
            )
            .expect("a check image");

            // The whole of a full-display image, mapped back through its own
            // transform, IS that display — so the check of an unzoomed action is
            // the display, losslessly. A rectangle that crept even a pixel here
            // would compress it as a crop and drift over a run of checks.
            assert_eq!(payload["mime"], json!("image/png"));
            assert_eq!(
                payload["region"],
                json!({ "x": 0, "y": 0, "w": CHECK_DISPLAY_W, "h": CHECK_DISPLAY_H })
            );

            let checked = gate.finish().check.expect("a check that ran");
            assert_eq!(checked.changed, Some(changed), "shade {after}");
        }
    }

    // A listing has no picture. Answering `false` there would claim the view had
    // not moved on evidence nobody has, so the field is absent instead.
    #[test]
    fn a_view_read_from_a_listing_claims_nothing_about_changing() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        let screen = ScriptedScreen::new(&clock, &[10, 10], 0);
        let app = fixture_app();
        let (mut worker, listing) = worker_listing(&gate, &app);
        worker.frames = screen.clone();
        let display = check_display();
        let measured =
            geometry::measure(&display.facts, CHECK_DISPLAY_W, CHECK_DISPLAY_H, Host::HERE)
                .expect("a one-to-one frame");
        worker
            .measured
            .remember(display.id, &display.facts, measured);

        assert_eq!(listing.view_hash, None, "a listing keeps no hash");

        let request = checking("press", wire::Check::Image);
        let view = display_view(&worker);
        post(
            &request,
            Some(view),
            &gate,
            &mut worker,
            Acted {
                named: Some(&listing),
                executed: None,
                element: None,
            },
        )
        .expect("a check image over the rectangle the listing was read in");

        let checked = gate.finish().check.expect("a check that ran");
        assert_eq!(checked.changed, None);
    }

    // The receipt alone. Still said, because "no evidence was asked for" and "the
    // evidence could not be obtained" are different answers.
    #[test]
    fn a_check_of_none_is_the_receipt_alone_and_still_says_so() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        let screen = ScriptedScreen::new(&clock, &[10], 0);
        let mut worker = screen_worker(&gate, &screen);
        let request = checking("type", wire::Check::None);

        let payload = post(
            &request,
            Some(display_view(&worker)),
            &gate,
            &mut worker,
            nothing_named(),
        )
        .expect("a receipt-only action");

        assert_eq!(payload, json!({ "ok": true }));
        assert_eq!(screen.taken(), 0, "nothing was captured");

        let done = gate.finish();
        let checked = done.check.expect("a mutating success always says which");
        assert_eq!(checked.kind, wire::Check::None);
        assert_eq!(checked.settle, None);

        let receipt = receipt(&request, &done, None).expect("a mutation earns one");
        assert_eq!(
            receipt.effect,
            wire::Effect::Unknown,
            "nothing was observed"
        );
    }

    // A semantic check is the control read again — never a capture, and never a
    // secure field's value, which is not read back anywhere in this build.
    #[test]
    fn a_semantic_check_reads_the_control_again_and_never_a_secure_value() {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let app = ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted::field("Name", "ada", frame_at(100.0, 200.0)),
                ax::Scripted::secure_field("Password", "hunter2", frame_at(100.0, 260.0)),
            ],
        );
        let (mut worker, listing) = worker_listing(&gate, &app);
        let screen = ScriptedScreen::new(&clock, &[10], 0);
        worker.frames = screen.clone();

        for (reference, expected) in [("e1", Some("ada")), ("e2", None)] {
            let mut request = at_element("set_value", &listing, reference);
            request.check = wire::Check::Semantic;
            gate.admit(&request).expect("a fresh sequence");

            let entry = element_in(&listing, &request).expect("a listed control");
            let payload = post(
                &request,
                None,
                &gate,
                &mut worker,
                Acted {
                    named: Some(&listing),
                    executed: None,
                    element: Some(entry),
                },
            )
            .expect("a semantic check");

            let after = &payload["element_after"];
            assert_eq!(after["present"], json!(true));
            assert_eq!(after["role"], json!("textfield"));
            assert_eq!(after["native_role"], json!("AXTextField"));
            assert_eq!(after["enabled"], json!(true));
            // Which control was re-read, in the control's own words — so a consumer
            // can say so without keeping a copy of the listing. A secure field has
            // a label like any other: what it CALLS itself is not what it holds.
            assert_eq!(
                after["label"],
                json!(if reference == "e1" {
                    "Name"
                } else {
                    "Password"
                })
            );
            assert_eq!(
                after.get("value").and_then(Value::as_str),
                expected,
                "{reference}: a secure field's value is never read back"
            );

            let checked = gate.finish().check.expect("a check that ran");
            assert_eq!(checked.kind, wire::Check::Semantic);
            assert_eq!(checked.settle, None, "nothing was watched");
        }

        assert_eq!(screen.taken(), 0, "a semantic check captures nothing");
    }

    // The re-read decides from what the control says now: a field listed as plain
    // that turned secure since carries no value after the action.
    #[test]
    fn a_semantic_check_never_reads_a_field_that_turned_secure_since_the_listing() {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let app = fixture_app();
        let (mut worker, listing) = worker_listing(&gate, &app);
        app.change(1, |element| {
            element.subrole = Some(ax::SECURE_TEXT_FIELD.to_string());
        });
        // The listing read it while it was plain.
        let listed = app.values_read();

        let mut request = at_element("set_value", &listing, "e2");
        request.check = wire::Check::Semantic;
        gate.admit(&request).expect("a fresh sequence");
        let entry = element_in(&listing, &request).expect("a listed control");
        let payload = post(
            &request,
            None,
            &gate,
            &mut worker,
            Acted {
                named: Some(&listing),
                executed: None,
                element: Some(entry),
            },
        )
        .expect("a semantic check");

        let after = &payload["element_after"];
        assert_eq!(after["present"], json!(true));
        assert!(after.get("value").is_none(), "{after}");
        assert_eq!(app.values_read(), listed, "the field was read again");
    }

    // A control that no longer answers at all is worth knowing about — a dialog
    // that closed, a row that went away — and it is not a missing reply.
    #[test]
    fn a_semantic_check_of_a_control_that_went_away_says_so() {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let app = fixture_app();
        let (mut worker, listing) = worker_listing(&gate, &app);
        let mut request = at_element("press", &listing, "e1");
        request.check = wire::Check::Semantic;
        gate.admit(&request).expect("a fresh sequence");
        let entry = element_in(&listing, &request).expect("a listed control");

        app.vanish(0);

        let payload = post(
            &request,
            None,
            &gate,
            &mut worker,
            Acted {
                named: Some(&listing),
                executed: None,
                element: Some(entry),
            },
        )
        .expect("a semantic check of a control that is gone");

        // Nothing but `present`: a control that does not answer its role does not
        // answer its label either, and inventing one from a stale listing would be
        // this build saying a control is there when it is not.
        assert_eq!(payload["element_after"], json!({ "present": false }));
    }

    // The wait returns what satisfied it. It used to hash twice and then capture a
    // THIRD frame, which could show something else again.
    #[test]
    fn a_wait_returns_the_frame_whose_hash_differed_and_takes_no_third_look() {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let screen = ScriptedScreen::new(&clock, &[10, 10, 200, 90], 0);
        let mut worker = screen_worker(&gate, &screen);

        let mut request = running("wait_for_change", None);
        request.observation_id = None;
        request.body = json!({ "action": "wait_for_change", "poll_ms": 50 });
        gate.admit(&request).expect("read-only, always admitted");

        let payload = wait_for_change_on(&check_display(), &request, &gate, &mut worker)
            .expect("a view that changed");

        assert_eq!(payload["changed"], json!(true));
        assert_eq!(
            screen.taken(),
            3,
            "the baseline, one look, and the one that differed"
        );
        assert_eq!(shade_of(&payload), 200, "the frame that satisfied the wait");
    }

    // A wait that times out returns its LAST sample, for the same reason: it is the
    // frame the answer is about.
    #[test]
    fn a_wait_that_times_out_returns_its_last_sample() {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let screen = ScriptedScreen::new(&clock, &[10], 0);
        let mut worker = screen_worker(&gate, &screen);

        let mut request = running("wait_for_change", None);
        request.observation_id = None;
        request.body = json!({ "action": "wait_for_change", "poll_ms": 50, "timeout_ms": 1 });
        gate.admit(&request).expect("read-only, always admitted");

        let payload = wait_for_change_on(&check_display(), &request, &gate, &mut worker)
            .expect("a view that never changed");

        assert_eq!(payload["changed"], json!(false));
        assert_eq!(shade_of(&payload), 10);
    }

    // The baseline is the hash kept with the image the caller named, so the wait is
    // about the view that caller saw rather than about one taken after the fact.
    #[test]
    fn a_wait_compares_against_the_image_the_caller_named() {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let screen = ScriptedScreen::new(&clock, &[200], 0);
        let mut worker = screen_worker(&gate, &screen);

        let geom = check_geometry();
        let seen = viewed(&mut worker, &gate, 10, Region::full(&geom)).expect("an image");
        let took = screen.taken();

        let mut request = running("wait_for_change", None);
        request.observation_id = Some(seen.id.clone());
        request.body = json!({
            "action": "wait_for_change",
            "observation_id": seen.id,
            "region": { "x": 0, "y": 0, "w": CHECK_DISPLAY_W, "h": CHECK_DISPLAY_H },
            "poll_ms": 50
        });
        gate.admit(&request).expect("read-only, always admitted");

        let payload = wait_for_change_on(&check_display(), &request, &gate, &mut worker)
            .expect("a view that changed");

        assert_eq!(payload["changed"], json!(true));
        assert_eq!(
            screen.taken() - took,
            1,
            "the remembered hash is the baseline: one look answered the wait"
        );
    }

    // The equality test the whole settle rests on. A hash that missed a one-pixel
    // change would tell a settle the view had stopped moving while it had not, and
    // would leave `wait_for_change` polling to its full timeout.
    #[test]
    fn one_pixel_flips_the_hash_and_an_identical_frame_does_not() {
        let geom = check_geometry();
        let crop = crop_rect(&geom, &Region::full(&geom));

        let frame = shaded(10);
        assert_eq!(
            view_hash(&frame, &crop),
            view_hash(&shaded(10), &crop),
            "the same pixels are the same view"
        );

        let mut nudged = frame.clone();
        nudged.put_pixel(
            CHECK_DISPLAY_W - 1,
            CHECK_DISPLAY_H - 1,
            image::Rgba([10, 10, 11, 255]),
        );
        assert_ne!(
            view_hash(&frame, &crop).pixels,
            view_hash(&nudged, &crop).pixels,
            "one pixel, one channel, at the far corner"
        );
    }

    // Only the rows of the rectangle are read, so what is outside it cannot move
    // the hash — which is what makes a crop's hash about that crop.
    #[test]
    fn a_hash_reads_its_own_rectangle_and_nothing_around_it() {
        let geom = check_geometry();
        let crop = crop_rect(&geom, &crop_region(100.0, 60.0, 120.0, 90.0));

        let frame = shaded(10);
        let mut outside = frame.clone();
        outside.put_pixel(5, 5, image::Rgba([200, 200, 200, 255]));
        assert_eq!(
            view_hash(&frame, &crop),
            view_hash(&outside, &crop),
            "a pixel outside the rectangle is not part of this view"
        );

        let mut inside = frame.clone();
        inside.put_pixel(150, 90, image::Rgba([200, 200, 200, 255]));
        assert_ne!(
            view_hash(&frame, &crop).pixels,
            view_hash(&inside, &crop).pixels
        );
    }

    // Two hashes of DIFFERENT rectangles are not two readings of one view. Calling
    // them different would publish `changed: true` to a caller whose view nobody
    // looked at twice, so the answer is that there is no answer.
    #[test]
    fn hashes_of_different_rectangles_are_not_comparable() {
        let geom = check_geometry();
        let frame = shaded(10);

        let whole = view_hash(&frame, &crop_rect(&geom, &Region::full(&geom)));
        let part = view_hash(
            &frame,
            &crop_rect(&geom, &crop_region(0.0, 0.0, 120.0, 90.0)),
        );

        assert_eq!(part.changed_from(&whole), None, "different rectangles");
        assert_eq!(
            whole.changed_from(&whole),
            Some(false),
            "the same view, twice"
        );

        // The same rectangle read out of a frame of another SIZE is not comparable
        // either: the numbers mean something different in each.
        let larger = image::RgbaImage::from_pixel(
            CHECK_DISPLAY_W * 2,
            CHECK_DISPLAY_H * 2,
            image::Rgba([10, 10, 10, 255]),
        );
        let rescaled = view_hash(&larger, &crop_rect(&geom, &Region::full(&geom)));
        assert_eq!(rescaled.changed_from(&whole), None, "another frame size");
    }

    // The measurement that decided the shape of `view_hash`, kept runnable so
    // anyone can re-take it: `cargo test the_hash_costs -- --nocapture`.
    //
    // The first version hashed a 256x256 Triangle downscale of the crop, on the
    // theory that an averaged thumbnail was more tolerant. It is not — a hash is an
    // equality test, and a one-pixel change perturbs an averaged cell exactly as it
    // perturbs a pixel — so the resize only added time, and on a real panel it
    // added more than the capture it was waiting on.
    #[test]
    fn the_hash_costs_less_than_the_resize_it_replaced() {
        // Noisy, not one flat colour: a real screen is, and a flat frame flatters
        // both sides for the wrong reason (a resize of it is unnaturally
        // cache-friendly). A cheap deterministic pattern, so the number is stable.
        let mut panel = image::RgbaImage::new(3840, 1080);
        let mut seed: u32 = 0x1234_5678;
        for pixel in panel.pixels_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [r, g, b, _] = seed.to_le_bytes();
            *pixel = image::Rgba([r, g, b, 255]);
        }

        let crop = geometry::CropRect {
            left_phys: 0.0,
            top_phys: 0.0,
            w_phys: 3840.0,
            h_phys: 1080.0,
        };

        let started = std::time::Instant::now();
        let hash = view_hash(&panel, &crop);
        let rows_ms = started.elapsed().as_secs_f64() * 1000.0;

        // The old path, whole: it COPIED the crop out of the frame and then resized
        // that copy, so the copy of a full-display crop — 16 MB on this panel — is
        // part of what went away.
        let started = std::time::Instant::now();
        let copied = crop_out(&panel, &crop);
        let thumb =
            image::imageops::resize(&copied, 256, 256, image::imageops::FilterType::Triangle);
        let mut old = DefaultHasher::new();
        old.write(thumb.as_raw());
        let thumbnail_ms = started.elapsed().as_secs_f64() * 1000.0;

        println!(
            "3840x1080: raw rows in place {rows_ms:.1} ms, the copy-and-thumbnail it replaced \
             {thumbnail_ms:.1} ms (hash {})",
            old.finish()
        );

        // The measurement is the point, but the test still has to test something:
        // the hash it produces is the one a settle will compare.
        assert_eq!(hash.frame, (3840, 1080));
        assert_eq!(hash.rect, (0, 0, 3840, 1080));
    }

    // What the owner's live run reads: four phases, measured on the one clock, with
    // every frame grabbed counted as what it is.
    #[test]
    fn every_phase_of_a_check_is_measured() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        let screen = ScriptedScreen::new(&clock, &[10, 20, 20], 7);
        let mut worker = screen_worker(&gate, &screen);
        let request = checking("type", wire::Check::Image);

        gate.dispatch(Phase::Input, || clock.advance(3))
            .expect("input posted");
        gate.input_complete();

        post(
            &request,
            Some(display_view(&worker)),
            &gate,
            &mut worker,
            nothing_named(),
        )
        .expect("a check image");

        let timings = gate.finish().timings;
        assert_eq!(timings.input_ms, 3, "the dispatch itself");
        assert_eq!(
            timings.capture_ms,
            3 * 7,
            "every look, the settle's included"
        );
        assert_eq!(
            timings.settle_ms,
            2 * (SETTLE_POLL_MS - 7),
            "two gaps, each the floor MINUS what the capture before it already took"
        );
    }

    // The floor is between LOOKS. A capture that took longer than it has already
    // provided the gap, and sleeping the full poll on top would be this process
    // adding delay to the check it exists to shorten.
    #[test]
    fn a_slow_look_is_not_slept_on_top_of() {
        for (cost, expected) in [(0, SETTLE_POLL_MS), (400, SETTLE_MIN_POLL_MS)] {
            let clock = StepClock::new();
            let gate = idle_check_gate(&clock);
            gate.input_complete();
            let screen = ScriptedScreen::new(&clock, &[10, 20, 20], cost);
            let mut worker = screen_worker(&gate, &screen);

            post(
                &checking("type", wire::Check::Image),
                Some(display_view(&worker)),
                &gate,
                &mut worker,
                nothing_named(),
            )
            .expect("a check image");

            assert_eq!(screen.taken(), 3, "cost {cost}");
            assert_eq!(
                gate.finish().timings.settle_ms,
                2 * expected,
                "cost {cost}: two gaps"
            );
        }
    }

    // THE hole two equal samples alone leave: the first look happens the instant the
    // input is complete, so an application that starts repainting a poll later would
    // hand back two identical samples of the view as it was BEFORE the action —
    // `stable`, `changed: false`, and the misleading check this slice removes.
    #[test]
    fn a_repaint_that_starts_late_is_still_the_frame_that_comes_back() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        gate.input_complete();
        // Two looks at the view as it was, then the repaint.
        let screen = ScriptedScreen::new(&clock, &[10, 10, 200, 200], 0);
        let mut worker = screen_worker(&gate, &screen);

        let geom = check_geometry();
        let acted_in = viewed(&mut worker, &gate, 10, Region::full(&geom)).expect("an image");

        let payload = post(
            &checking("left_click", wire::Check::Image),
            Some(display_view(&worker)),
            &gate,
            &mut worker,
            Acted {
                named: Some(&acted_in),
                executed: Some((10, 10)),
                element: None,
            },
        )
        .expect("a check image");

        assert_eq!(
            shade_of(&payload),
            200,
            "the repainted view, not the old one"
        );
        assert_eq!(screen.taken(), 4);

        let checked = gate.finish().check.expect("a check that ran");
        assert_eq!(checked.settle, Some(wire::Settle::Stable));
        assert_eq!(checked.changed, Some(true));
    }

    // Once a change HAS been seen, two equal looks settle it at once: an action
    // whose effect is immediate pays nothing for the window above.
    #[test]
    fn a_change_already_seen_settles_without_waiting_out_the_quiet_window() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        gate.input_complete();
        let screen = ScriptedScreen::new(&clock, &[200, 200], 0);
        let mut worker = screen_worker(&gate, &screen);

        let geom = check_geometry();
        let acted_in = viewed(&mut worker, &gate, 10, Region::full(&geom)).expect("an image");

        post(
            &checking("left_click", wire::Check::Image),
            Some(display_view(&worker)),
            &gate,
            &mut worker,
            Acted {
                named: Some(&acted_in),
                executed: Some((10, 10)),
                element: None,
            },
        )
        .expect("a check image");

        assert_eq!(screen.taken(), 2, "the first look already differed");
        assert!(
            gate.now_ms() < SETTLE_QUIET_MS,
            "it did not wait out the window: {} ms",
            gate.now_ms()
        );
        assert_eq!(
            gate.finish().check.expect("a check").settle,
            Some(wire::Settle::Stable)
        );
    }

    // And an action that really changes nothing pays the window, once — which is
    // the price of the case above being trustworthy.
    #[test]
    fn a_view_that_never_changes_is_quiet_before_it_is_called_stable() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);
        gate.input_complete();
        let screen = ScriptedScreen::new(&clock, &[10, 10, 10, 10, 10, 10, 10, 10], 0);
        let mut worker = screen_worker(&gate, &screen);

        let geom = check_geometry();
        let acted_in = viewed(&mut worker, &gate, 10, Region::full(&geom)).expect("an image");

        post(
            &checking("left_click", wire::Check::Image),
            Some(display_view(&worker)),
            &gate,
            &mut worker,
            Acted {
                named: Some(&acted_in),
                executed: Some((10, 10)),
                element: None,
            },
        )
        .expect("a check image");

        assert!(
            gate.now_ms() >= SETTLE_QUIET_MS,
            "stable was not claimed before the window: {} ms",
            gate.now_ms()
        );

        let checked = gate.finish().check.expect("a check that ran");
        assert_eq!(checked.settle, Some(wire::Settle::Stable));
        assert_eq!(
            checked.changed,
            Some(false),
            "nothing moved, and it says so"
        );
    }

    // Fermix reads "an image check was asked for, `dispatch` says `sent`, and the
    // receipt carries no check" as "the evidence could not be obtained". That holds
    // only while the check is the LAST thing an action does: a fallible step added
    // between the latch and it would produce those same three facts for another
    // reason. This reads the source, so such a step fails here rather than in a
    // consumer's sentence.
    #[test]
    fn nothing_fallible_comes_between_the_latch_and_the_check() {
        // The production half only: this test's own text names the latch too.
        let source = include_str!("main.rs")
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("a production half");

        // The accessibility actions report their own outcome, with their own error
        // code, before the check. They are the only `?` allowed after the latch.
        const ALLOWED: [&str; 1] = ["outcome.map_err(ax_failure)?;"];

        let latch = concat!("gate.input_", "complete();");
        let tails: Vec<&str> = source.split(latch).skip(1).collect();
        assert_eq!(
            tails.len(),
            10,
            "every action that dispatches input latches exactly once"
        );

        let mut allowed_seen = 0;
        for tail in tails {
            let body = tail
                .split("\n}\n")
                .next()
                .expect("the rest of the function");
            assert!(
                body.contains("post(") || body.contains("read-only"),
                "the check is what follows the latch: {body}"
            );

            // The check itself is fallible, and is what all of this is about.
            for line in body
                .lines()
                .map(str::trim)
                .filter(|line| line.contains('?') && !line.contains("post("))
            {
                assert!(
                    ALLOWED.contains(&line),
                    "a fallible step between the latch and the check: {line}"
                );
                allowed_seen += 1;
            }
        }

        assert_eq!(
            allowed_seen,
            ALLOWED.len() * 3,
            "press, set_value and the verbs on a reference, and nothing else, report an \
             accessibility outcome"
        );
    }

    // A clock that jumped must not put an absurd number on the wire.
    #[test]
    fn a_phase_is_clamped_to_a_ceiling_no_honest_action_reaches() {
        let clock = StepClock::new();
        let gate = idle_check_gate(&clock);

        gate.record(Phase::Capture, u64::MAX);
        gate.record(Phase::Capture, 5);

        assert_eq!(gate.finish().timings.capture_ms, wire::MAX_TIMING_MS);
    }

    // --- protocol 12: the verbs on a reference, waits, surfaces --------------------

    /// A gate of the boot the test requests name, so `serve` admits them.
    fn boot_gate() -> Gate {
        Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()))
    }

    /// A checkbox (e1), a field that takes focus and text (e2), a folder row that
    /// expands, selects and scrolls into view (e3) and a button (e4).
    fn controls_app() -> Rc<ax::Recorder> {
        ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted::checkbox("Bold", false, frame_at(100.0, 200.0)),
                ax::Scripted {
                    routes: ax::Routes {
                        focus: true,
                        text: true,
                        ..ax::Routes::default()
                    },
                    states: ax::States {
                        focused: Some(false),
                        ..ax::States::default()
                    },
                    ..ax::Scripted::field("Name", "ada", frame_at(100.0, 260.0))
                },
                ax::Scripted {
                    label: Some("Folder".to_string()),
                    actions: vec!["AXConfirm".to_string(), ax::SCROLL_TO_VISIBLE.to_string()],
                    frame: Some(frame_at(100.0, 320.0)),
                    routes: ax::Routes {
                        select: true,
                        expand: Some(ax::Expander::Expanded),
                        ..ax::Routes::default()
                    },
                    states: ax::States {
                        expanded: Some(false),
                        selected: Some(false),
                        ..ax::States::default()
                    },
                    ..ax::Scripted::of("AXRow")
                },
                ax::Scripted::button("Save", frame_at(100.0, 380.0)),
            ],
        )
    }

    fn verb_at(
        action: &str,
        observation: &Observation,
        reference: &str,
        extra: Value,
    ) -> wire::Request {
        let mut request = at_element(action, observation, reference);
        if let (Some(body), Some(extra)) = (request.body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        request
    }

    fn performed_names(app: &ax::Recorder) -> Vec<String> {
        app.performed()
            .into_iter()
            .map(|(_, what, _)| what)
            .collect()
    }

    // The whole receipt of a verb that changed what it was asked to: it went
    // through the accessibility API, its read-back shows the state it was asked
    // for, and the state before and after are named.
    #[test]
    fn a_toggle_reads_the_state_first_presses_once_and_reads_it_back() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(
            &verb_at("toggle", &observation, "e1", json!({"on": true})),
            &gate,
            &mut worker,
        );

        assert_eq!(frame["ok"], json!(true), "{frame}");
        assert_eq!(frame["receipt"]["dispatch"], json!("sent"));
        assert_eq!(frame["receipt"]["input_method"], json!("ax"));
        assert_eq!(frame["receipt"]["effect"], json!("changed"));
        assert_eq!(frame["receipt"]["state_before"], json!("off"));
        assert_eq!(frame["receipt"]["state_after"], json!("on"));
        assert!(frame["receipt"].get("already").is_none());
        assert_eq!(performed_names(&app), vec![ax::PRESS]);
    }

    // A state that already holds is not touched: nothing is sent, and the receipt
    // says so rather than claiming a press.
    #[test]
    fn a_toggle_already_in_its_state_sends_nothing_and_says_so() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(
            &verb_at("toggle", &observation, "e1", json!({"on": false})),
            &gate,
            &mut worker,
        );

        assert_eq!(frame["ok"], json!(true), "{frame}");
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert_eq!(frame["receipt"]["already"], json!(true));
        assert_eq!(frame["receipt"]["state_before"], json!("off"));
        assert_eq!(frame["receipt"]["state_after"], json!("off"));
        assert_eq!(frame["receipt"]["effect"], json!("not_observed"));
        assert!(
            frame["receipt"].get("surfaces").is_none(),
            "nothing sent, nothing looked at"
        );
        assert!(app.performed().is_empty());
    }

    // A control with no route for the verb is refused, and never clicked.
    #[test]
    fn a_verb_with_no_route_is_refused_and_never_becomes_a_click() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        for (action, reference, extra) in [
            ("toggle", "e4", json!({"on": true})),
            ("focus", "e4", json!({})),
            ("select", "e1", json!({})),
            ("expand", "e4", json!({})),
            ("scroll_to", "e1", json!({})),
            ("type", "e4", json!({"text": "x"})),
        ] {
            let frame = served(
                &verb_at(action, &observation, reference, extra),
                &gate,
                &mut worker,
            );
            assert_eq!(
                frame["error"],
                json!("ax_action_unsupported"),
                "{action}: {frame}"
            );
            assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"), "{action}");
        }
        assert!(app.performed().is_empty(), "{:?}", app.performed());
    }

    // A write the application took and did nothing with is reported as such: sent,
    // and not observed.
    #[test]
    fn a_verb_whose_read_back_is_unchanged_is_not_observed() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.ignores_writes();

        let frame = served(
            &verb_at("toggle", &observation, "e1", json!({"on": true})),
            &gate,
            &mut worker,
        );

        assert_eq!(frame["receipt"]["dispatch"], json!("sent"));
        assert_eq!(frame["receipt"]["effect"], json!("not_observed"));
        assert_eq!(frame["receipt"]["state_after"], json!("off"));
    }

    // A state that will not read before the write refuses the verb; one that will
    // not read after it is an effect nobody can claim.
    #[test]
    fn a_state_that_will_not_read_refuses_before_and_is_unknown_after() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.change(0, |bold| bold.states.on = None);

        let frame = served(
            &verb_at("toggle", &observation, "e1", json!({"on": true})),
            &gate,
            &mut worker,
        );
        assert_eq!(frame["error"], json!("state_unreadable"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert!(app.performed().is_empty());
    }

    #[test]
    fn focus_moves_the_keyboard_and_reads_it_back() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(
            &verb_at("focus", &observation, "e2", json!({})),
            &gate,
            &mut worker,
        );

        assert_eq!(frame["receipt"]["effect"], json!("changed"), "{frame}");
        assert_eq!(frame["receipt"]["state_before"], json!("unfocused"));
        assert_eq!(frame["receipt"]["state_after"], json!("focused"));
        assert_eq!(frame["receipt"]["foreground_changed"], json!(false));
        assert_eq!(performed_names(&app), vec!["write:Focused"]);
    }

    // Focus never brings a window forward, so a control whose window is not
    // the active one is refused and nothing is sent.
    #[test]
    fn focus_in_a_window_that_is_not_active_is_refused() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.change(1, |name| name.in_focused_window = Some(false));

        let frame = served(
            &verb_at("focus", &observation, "e2", json!({})),
            &gate,
            &mut worker,
        );

        assert_eq!(frame["error"], json!("focus_refused"));
        assert_eq!(frame["receipt"]["dispatch"], json!("not_sent"));
        assert!(app.performed().is_empty());
    }

    #[test]
    fn expand_and_collapse_act_only_when_the_state_differs() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let collapsed = served(
            &verb_at("collapse", &observation, "e3", json!({})),
            &gate,
            &mut worker,
        );
        assert_eq!(collapsed["receipt"]["already"], json!(true), "{collapsed}");
        assert_eq!(collapsed["receipt"]["state_before"], json!("collapsed"));

        let expanded = served(
            &verb_at("expand", &observation, "e3", json!({})),
            &gate,
            &mut worker,
        );
        assert_eq!(
            expanded["receipt"]["effect"],
            json!("changed"),
            "{expanded}"
        );
        assert_eq!(expanded["receipt"]["state_after"], json!("expanded"));
        assert_eq!(performed_names(&app), vec!["write:Expand(Expanded, true)"]);
    }

    #[test]
    fn select_writes_the_item_and_reads_it_back() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(
            &verb_at("select", &observation, "e3", json!({})),
            &gate,
            &mut worker,
        );

        assert_eq!(frame["receipt"]["effect"], json!("changed"), "{frame}");
        assert_eq!(frame["receipt"]["state_after"], json!("selected"));
        assert_eq!(performed_names(&app), vec!["write:Selected"]);
    }

    // An item that cannot select itself is selected through its container.
    #[test]
    fn select_goes_through_the_container_when_the_item_has_no_route() {
        let gate = boot_gate();
        let app = ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted {
                    selects_children: true,
                    label: Some("Files".to_string()),
                    children: vec![1],
                    ..ax::Scripted::of("AXList")
                },
                ax::Scripted {
                    label: Some("notes.txt".to_string()),
                    actions: vec!["AXConfirm".to_string()],
                    frame: Some(frame_at(100.0, 200.0)),
                    states: ax::States {
                        selected: Some(false),
                        ..ax::States::default()
                    },
                    ..ax::Scripted::of("AXRow")
                },
            ],
        );
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(
            &verb_at("select", &observation, "e1", json!({})),
            &gate,
            &mut worker,
        );

        assert_eq!(frame["receipt"]["effect"], json!("changed"), "{frame}");
        let performed = performed_names(&app);
        assert_eq!(performed.len(), 1);
        assert!(
            performed[0].starts_with("write:SelectedChildren"),
            "{performed:?}"
        );
        assert_eq!(
            app.counts().2,
            1,
            "the container was given back; the listing holds its row"
        );
    }

    #[test]
    fn scroll_to_reads_where_the_control_is_afterwards() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let still = served(
            &verb_at("scroll_to", &observation, "e3", json!({})),
            &gate,
            &mut worker,
        );
        assert_eq!(
            still["receipt"]["effect"],
            json!("not_observed"),
            "already in view: {still}"
        );
        assert!(
            still["receipt"].get("state_before").is_none(),
            "scroll_to has no state"
        );

        app.scrolls_to(2, frame_at(100.0, 40.0));
        let moved = served(
            &verb_at("scroll_to", &observation, "e3", json!({})),
            &gate,
            &mut worker,
        );
        assert_eq!(moved["receipt"]["effect"], json!("changed"));
        assert_eq!(
            performed_names(&app),
            vec![ax::SCROLL_TO_VISIBLE, ax::SCROLL_TO_VISIBLE]
        );
    }

    // Typing by reference uses no keyboard, and reports what the field holds after.
    #[test]
    fn type_by_reference_writes_the_selection_and_reports_the_value() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(
            &verb_at("type", &observation, "e2", json!({"text": "!"})),
            &gate,
            &mut worker,
        );

        assert_eq!(frame["ok"], json!(true), "{frame}");
        assert_eq!(frame["receipt"]["input_method"], json!("ax"));
        assert_eq!(frame["receipt"]["effect"], json!("changed"));
        assert_eq!(frame["value"], json!("ada!"));
        assert_eq!(performed_names(&app), vec![r#"write:SelectedText("!")"#]);
    }

    // A password field is typed into and never read, before or after.
    #[test]
    fn type_by_reference_into_a_secure_field_never_reads_it() {
        let gate = boot_gate();
        let app = ax::Recorder::new(
            FIXTURE_PID,
            vec![ax::Scripted {
                routes: ax::Routes {
                    text: true,
                    ..ax::Routes::default()
                },
                ..ax::Scripted::secure_field("Password", "hunter2", frame_at(100.0, 200.0))
            }],
        );
        let (mut worker, observation) = worker_listing(&gate, &app);

        let frame = served(
            &verb_at("type", &observation, "e1", json!({"text": "x"})),
            &gate,
            &mut worker,
        );

        assert_eq!(frame["ok"], json!(true), "{frame}");
        assert_eq!(frame["receipt"]["input_method"], json!("ax"));
        assert_eq!(frame["receipt"]["effect"], json!("unknown"), "{frame}");
        assert!(frame.get("value").is_none());
        assert!(app.values_read().is_empty(), "{:?}", app.values_read());
        assert_eq!(performed_names(&app), vec![r#"write:SelectedText("x")"#]);
    }

    // What a receipt's check says when the action also waited on the tree: the
    // condition is read BEFORE the input, so one that already held says so rather
    // than claiming the action made it hold.
    #[test]
    fn a_semantic_until_is_read_before_the_input_and_waited_on_after() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        let waiting = |value: Value| {
            let mut request = verb_at("type", &observation, "e2", json!({"text": "!"}));
            request.check = wire::Check::Semantic;
            request.until = Some(Box::new(
                semantic::parse_until(&value, true).expect("a condition"),
            ));
            request
        };

        let met = served(
            &waiting(json!({"kind": "value", "element_ref": "e2", "value": "ada!"})),
            &gate,
            &mut worker,
        );
        assert_eq!(met["receipt"]["check"]["kind"], json!("semantic"), "{met}");
        assert_eq!(met["receipt"]["check"]["until"], json!("met"));
        assert_eq!(met["receipt"]["check"]["looks"], json!(1));
        assert_eq!(met["element_after"]["role"], json!("textfield"));

        let before = served(
            &waiting(json!({"kind": "value", "element_ref": "e2", "value": "ada!"})),
            &gate,
            &mut worker,
        );
        assert_eq!(
            before["receipt"]["check"]["until"],
            json!("held_before"),
            "it held before this input, so it shows nothing about it: {before}"
        );

        let never = served(
            &waiting(json!({"kind": "appears", "name": "Never", "timeout_ms": 200})),
            &gate,
            &mut worker,
        );
        assert_eq!(
            never["receipt"]["check"]["until"],
            json!("not_met"),
            "{never}"
        );
        assert_eq!(never["receipt"]["check"]["looks"], json!(3));
        assert_eq!(never["receipt"]["dispatch"], json!("sent"));
        assert_eq!(
            performed_names(&app).len(),
            3,
            "one write per request, and the waits sent nothing"
        );
    }

    #[test]
    fn wait_for_answers_met_and_not_met_and_never_sends_input() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        let waiting = |value: Value| {
            let mut request = running("wait_for", None);
            request.observation_id = Some(observation.id.clone());
            request.until = Some(Box::new(
                semantic::parse_until(&value, true).expect("a condition"),
            ));
            request
        };

        let met = served(
            &waiting(json!({"kind": "enabled", "element_ref": "e4"})),
            &gate,
            &mut worker,
        );
        assert_eq!(met["ok"], json!(true), "{met}");
        assert_eq!(met["met"], json!(true));
        assert_eq!(met["looks"], json!(1));
        assert!(met.get("receipt").is_none(), "read-only");

        let never = served(
            &waiting(json!({"kind": "appears", "name": "Never", "timeout_ms": 300})),
            &gate,
            &mut worker,
        );
        assert_eq!(never["ok"], json!(true), "not met is an answer: {never}");
        assert_eq!(never["met"], json!(false));
        assert_eq!(never["looks"], json!(4));
        assert!(app.performed().is_empty());
    }

    // A met wait for something to appear lists what matched, with references in a
    // new observation, exactly as `elements` lists controls.
    #[test]
    fn a_met_wait_for_lists_what_appeared_with_references() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        let mut request = running("wait_for", None);
        request.observation_id = Some(observation.id.clone());
        request.until = Some(Box::new(
            semantic::parse_until(
                &json!({"kind": "appears", "role": "button", "name": "save"}),
                true,
            )
            .expect("a condition"),
        ));

        let met = served(&request, &gate, &mut worker);

        assert_eq!(met["met"], json!(true), "{met}");
        assert_eq!(met["observation_kind"], json!("semantic"));
        assert_ne!(met["observation_id"], json!(observation.id));
        let listed = met["elements"].as_array().expect("a listing");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["label"], json!("Save"));
        assert_eq!(listed[0]["element_ref"], json!("e1"));

        let mut press = at_element("press", &observation, "e1");
        press.observation_id = met["observation_id"].as_str().map(str::to_string);
        let pressed = served(&press, &gate, &mut worker);
        assert_eq!(
            pressed["ok"],
            json!(true),
            "the new reference presses: {pressed}"
        );
    }

    // What is open once an action has run rides its receipt: a sheet the press
    // opened, nothing when nothing is, and nothing at all when nothing was sent.
    #[test]
    fn surfaces_ride_the_receipt_of_input_that_went_out() {
        let gate = boot_gate();
        let app = ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted::button("Clear", frame_at(100.0, 200.0)),
                ax::Scripted {
                    label: Some("Document".to_string()),
                    children: vec![2],
                    ..ax::Scripted::of("AXWindow")
                },
                ax::Scripted {
                    label: Some("Clear all items?".to_string()),
                    ..ax::Scripted::of("AXSheet")
                },
            ],
        );
        let (mut worker, observation) = worker_listing(&gate, &app);

        let pressed = served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        assert_eq!(
            pressed["receipt"]["surfaces"],
            json!([{"kind": "dialog", "native_kind": "AXSheet", "label": "Clear all items?"}]),
            "{pressed}"
        );

        let refused = served(&at_element("press", &observation, "e9"), &gate, &mut worker);
        assert!(
            refused["receipt"].get("surfaces").is_none(),
            "nothing sent: {refused}"
        );

        let none = fixture_app();
        let (mut worker, observation) = worker_listing(&gate, &none);
        let pressed = served(&at_element("press", &observation, "e1"), &gate, &mut worker);
        assert_eq!(
            pressed["receipt"]["surfaces"],
            json!([]),
            "looked, nothing open"
        );
    }

    // A build with no accessibility API never claims to have looked.
    #[test]
    fn a_build_with_no_accessibility_reports_no_surfaces() {
        let gate = boot_gate();
        let app = fixture_app();
        let (mut worker, _observation) = worker_listing(&gate, &app);
        worker.accessibility = false;
        let done = gate::Dispatched {
            posted: true,
            input_complete: true,
            ..gate::Dispatched::default()
        };

        assert_eq!(
            surfaces_after(&running("key", Some(1)), &done, &worker),
            None
        );
        worker.accessibility = true;
        assert_eq!(
            surfaces_after(&running("key", Some(1)), &done, &worker),
            Some(Vec::new())
        );
        assert_eq!(
            surfaces_after(
                &running("key", Some(1)),
                &gate::Dispatched::default(),
                &worker
            ),
            None,
            "nothing sent"
        );
    }

    // The controls as protocol 12 names them: a normalised role beside the native
    // one, the verbs each has a route for in place of `actions` and `settable`, and
    // the states that read.
    #[test]
    fn an_element_is_published_with_its_role_its_verbs_and_its_states() {
        let app = controls_app();
        let platform: Rc<dyn ax::Ax> = app.clone();
        let (geom, region) = one_to_one();
        let found = tree::walk(&platform, FIXTURE_PID, &SystemClock::new(), u64::MAX);

        let bold = element_value(
            &found.nodes[0],
            Some("e1"),
            Some((140, 212)),
            &geom,
            &region,
        );
        assert_eq!(bold["role"], json!("checkbox"));
        assert_eq!(bold["native_role"], json!("AXCheckBox"));
        assert_eq!(bold["verbs"], json!(["press", "toggle"]));
        assert_eq!(bold["checked"], json!(false));
        assert!(bold.get("actions").is_none() && bold.get("settable").is_none());
        assert_eq!(
            bold["bounds"],
            json!({"x": 100, "y": 200, "w": 80, "h": 24})
        );

        let name = element_value(
            &found.nodes[1],
            Some("e2"),
            Some((140, 272)),
            &geom,
            &region,
        );
        assert_eq!(name["role"], json!("textfield"));
        assert_eq!(name["verbs"], json!(["set_value", "type", "focus"]));
        assert!(name.get("focused").is_none(), "only said when true");
        assert!(name.get("checked").is_none(), "a field has no check state");

        let folder = element_value(
            &found.nodes[2],
            Some("e3"),
            Some((140, 332)),
            &geom,
            &region,
        );
        assert_eq!(folder["role"], json!("row"));
        assert_eq!(
            folder["verbs"],
            json!(["expand", "collapse", "select", "scroll_to"])
        );
        assert_eq!(folder["expanded"], json!(false));

        let unnamed = element_value(&found.nodes[3], None, Some((140, 392)), &geom, &region);
        assert!(
            unnamed.get("verbs").is_none(),
            "nothing offered on what cannot be named"
        );

        // A control the platform places nowhere has no bounds and no click point,
        // and nothing synthesises one.
        let mut nowhere = tree::walk(&platform, FIXTURE_PID, &SystemClock::new(), u64::MAX);
        nowhere.nodes[3].frame = None;
        let placeless = element_value(&nowhere.nodes[3], Some("e4"), None, &geom, &region);
        for absent in ["bounds", "x", "y"] {
            assert!(placeless.get(absent).is_none(), "{absent}: {placeless}");
        }
        assert_eq!(placeless["verbs"], json!(["press"]));
    }

    // The control a semantic check reads again is named as a listing names it.
    #[test]
    fn element_after_carries_the_normalised_role_and_the_states() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        let mut request = verb_at("toggle", &observation, "e1", json!({"on": true}));
        request.check = wire::Check::Semantic;

        let frame = served(&request, &gate, &mut worker);

        assert_eq!(frame["element_after"]["role"], json!("checkbox"), "{frame}");
        assert_eq!(frame["element_after"]["native_role"], json!("AXCheckBox"));
        assert_eq!(frame["element_after"]["checked"], json!(true));
    }

    // A secure field says so wherever its value is withheld — in a listing and in
    // the control read again — so a caller knows why there is none.
    #[test]
    fn a_secure_field_says_it_is_secure_where_its_value_is_withheld() {
        let gate = boot_gate();
        let app = ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted::secure_field("Password", "hunter2", frame_at(100.0, 200.0)),
                ax::Scripted::field("Name", "ada", frame_at(100.0, 260.0)),
            ],
        );
        let platform: Rc<dyn ax::Ax> = app.clone();
        let (geom, region) = one_to_one();
        let found = tree::walk(&platform, FIXTURE_PID, &SystemClock::new(), u64::MAX);

        let password = element_value(
            &found.nodes[0],
            Some("e1"),
            Some((140, 212)),
            &geom,
            &region,
        );
        assert_eq!(password["secure"], json!(true), "{password}");
        assert!(password.get("value").is_none());
        let name = element_value(
            &found.nodes[1],
            Some("e2"),
            Some((140, 272)),
            &geom,
            &region,
        );
        assert!(
            name.get("secure").is_none(),
            "said only of a secure field: {name}"
        );

        let (mut worker, observation) = worker_listing(&gate, &app);
        let mut request = set_to(&observation, "e1", "x");
        request.check = wire::Check::Semantic;
        let frame = served(&request, &gate, &mut worker);
        assert_eq!(frame["element_after"]["secure"], json!(true), "{frame}");
        assert!(frame["element_after"].get("value").is_none());
    }

    // A wait never outlives the request: what is left of its deadline bounds the
    // wait however long the condition's own timeout, and the reply says the
    // deadline is what ended it.
    #[test]
    fn a_wait_cut_short_by_the_request_deadline_says_so() {
        let gate = boot_gate();
        let app = controls_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        let never = || {
            Box::new(
                semantic::parse_until(&json!({"kind": "appears", "name": "Never"}), true)
                    .expect("a condition"),
            )
        };

        let mut waiting = running("wait_for", None);
        waiting.observation_id = Some(observation.id.clone());
        waiting.body = json!({"action": "wait_for", "deadline_ms": DEADLINE_MARGIN_MS + 100});
        waiting.until = Some(never());
        let cut = served(&waiting, &gate, &mut worker);
        assert_eq!(cut["met"], json!(false), "{cut}");
        assert_eq!(cut["deadline_cut"], json!(true));
        assert!(cut["waited_ms"].as_u64().expect("waited") < 1_000, "{cut}");

        let mut typing = verb_at("type", &observation, "e2", json!({"text": "!"}));
        typing.body["deadline_ms"] = json!(DEADLINE_MARGIN_MS + 100);
        typing.check = wire::Check::Semantic;
        typing.until = Some(never());
        let typed = served(&typing, &gate, &mut worker);
        assert_eq!(
            typed["receipt"]["check"]["until"],
            json!("not_met"),
            "{typed}"
        );
        assert_eq!(typed["receipt"]["check"]["deadline_cut"], json!(true));

        let mut whole = running("wait_for", None);
        whole.observation_id = Some(observation.id.clone());
        whole.until = Some(Box::new(
            semantic::parse_until(
                &json!({"kind": "appears", "name": "Never", "timeout_ms": 100}),
                true,
            )
            .expect("a condition"),
        ));
        let ran = served(&whole, &gate, &mut worker);
        assert!(
            ran.get("deadline_cut").is_none(),
            "its own timeout ended it: {ran}"
        );
    }

    // The look before the input is held to the request's deadline, as the wait
    // after it is: a request with little left does not spend a whole walk on it.
    #[test]
    fn the_look_before_the_input_stops_at_the_request_deadline() {
        let clock = StepClock::new();
        let gate = Gate::new("boot-1".to_string(), clock.clone());
        let rows = (0..40)
            .map(|row| ax::Scripted::button(&format!("Row {row}"), frame_at(100.0, row as f64)))
            .collect();
        let app = ax::Recorder::new(FIXTURE_PID, rows);
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.costs_per_node(100, clock.clone());
        worker.cutoff_ms = Some(gate.now_ms() + 300);
        let until = semantic::parse_until(&json!({"kind": "appears", "name": "Never"}), true)
            .expect("a condition");
        let mut request = at_element("press", &observation, "e1");
        request.until = Some(Box::new(until.clone()));

        let started = gate.now_ms();
        let held = held_before(&request, &until, &gate, &worker).expect("a look");

        assert!(!held);
        let spent = gate.now_ms() - started;
        assert!(spent < 1_000, "the look before the input took {spent} ms");
    }

    /// A card that presses (e1) holding two buttons (e2, e3), and a button beside
    /// it (e4).
    fn card_app() -> Rc<ax::Recorder> {
        ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted {
                    label: Some("Card".to_string()),
                    actions: vec![ax::PRESS.to_string()],
                    frame: Some(frame_at(100.0, 100.0)),
                    children: vec![1, 2],
                    ..ax::Scripted::of("AXGroup")
                },
                ax::Scripted::button("One", frame_at(100.0, 200.0)),
                ax::Scripted::button("Two", frame_at(100.0, 260.0)),
                ax::Scripted::button("Elsewhere", frame_at(100.0, 320.0)),
            ],
        )
    }

    fn drill_into(observation: &Observation, root: &str) -> wire::Request {
        let mut request = running("elements", None);
        request.observation_id = Some(observation.id.clone());
        request.body =
            json!({"action": "elements", "observation_id": observation.id, "root": root});
        request
    }

    // A drill replaces the drilled subtree's references and keeps every other one:
    // the root and what is outside it stay valid in the new observation, the new
    // ones are numbered after the listing's last, and the listing it named stays
    // valid too.
    #[test]
    fn a_drill_replaces_only_the_drilled_subtrees_references() {
        let gate = boot_gate();
        let app = card_app();
        let (mut worker, observation) = worker_listing(&gate, &app);

        let drilled = served(&drill_into(&observation, "e1"), &gate, &mut worker);

        assert_eq!(drilled["ok"], json!(true), "{drilled}");
        assert_eq!(drilled["root"], json!("e1"));
        let refs: Vec<&str> = drilled["elements"]
            .as_array()
            .expect("a listing")
            .iter()
            .filter_map(|item| item["element_ref"].as_str())
            .collect();
        assert_eq!(refs, vec!["e5", "e6"]);

        let new_id = drilled["observation_id"]
            .as_str()
            .expect("a new observation")
            .to_string();
        let press_in = |id: &str, reference: &str, worker: &mut Worker| {
            let mut press = at_element("press", &observation, reference);
            press.observation_id = Some(id.to_string());
            served(&press, &gate, worker)
        };
        for kept in ["e1", "e4", "e5", "e6"] {
            assert_eq!(
                press_in(&new_id, kept, &mut worker)["ok"],
                json!(true),
                "{kept}"
            );
        }
        assert_eq!(
            press_in(&new_id, "e2", &mut worker)["error"],
            json!("stale_element"),
            "replaced by the drill"
        );
        assert_eq!(
            press_in(&observation.id, "e2", &mut worker)["ok"],
            json!(true),
            "the listing it named is untouched"
        );
    }

    /// A panel (e1) holding a button (e2) and, three groups down, another (e3).
    fn nested_app() -> Rc<ax::Recorder> {
        let group = |label: &str, child: usize, y: f64| ax::Scripted {
            label: Some(label.to_string()),
            frame: Some(frame_at(100.0, y)),
            children: vec![child],
            ..ax::Scripted::of("AXGroup")
        };
        ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted {
                    label: Some("Panel".to_string()),
                    actions: vec![ax::PRESS.to_string()],
                    frame: Some(frame_at(100.0, 100.0)),
                    children: vec![1, 2],
                    ..ax::Scripted::of("AXGroup")
                },
                ax::Scripted::button("Near", frame_at(100.0, 150.0)),
                group("Outer", 3, 200.0),
                group("Middle", 4, 220.0),
                group("Inner", 5, 240.0),
                ax::Scripted::button("Deep", frame_at(100.0, 260.0)),
            ],
        )
    }

    fn labels_of(payload: &Value) -> Vec<&str> {
        payload["elements"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| item["label"].as_str().unwrap_or("-"))
                    .collect()
            })
            .unwrap_or_default()
    }

    // A drill honours a skeleton: it cuts three levels below its root, lists the
    // cut with what it hides, and says it is a skeleton.
    #[test]
    fn a_drilled_skeleton_cuts_three_levels_below_its_root() {
        let gate = boot_gate();
        let app = nested_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        let mut request = drill_into(&observation, "e1");
        request.body["skeleton"] = json!(true);

        let drilled = served(&request, &gate, &mut worker);

        assert_eq!(drilled["ok"], json!(true), "{drilled}");
        assert_eq!(labels_of(&drilled), vec!["Near", "Inner"]);
        assert_eq!(drilled["elements"][1]["hidden"], json!(1));
        assert_eq!(drilled["skeleton"], json!({"depth": 3, "hidden": 0}));
    }

    // A drill honours a find: only what matches inside its root is listed.
    #[test]
    fn a_drilled_find_lists_only_what_matches() {
        let gate = boot_gate();
        let app = nested_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        let mut request = drill_into(&observation, "e1");
        request.body["query"] = json!({"name": "deep"});

        let drilled = served(&request, &gate, &mut worker);

        assert_eq!(drilled["ok"], json!(true), "{drilled}");
        assert_eq!(labels_of(&drilled), vec!["Deep"]);
        assert!(drilled.get("skeleton").is_none(), "{drilled}");
    }

    #[test]
    fn a_drill_into_a_control_that_is_gone_is_stale() {
        let gate = boot_gate();
        let app = card_app();
        let (mut worker, observation) = worker_listing(&gate, &app);
        app.vanish(0);

        let refused = served(&drill_into(&observation, "e1"), &gate, &mut worker);
        assert_eq!(refused["error"], json!("stale_element"), "{refused}");

        let unknown = served(&drill_into(&observation, "e9"), &gate, &mut worker);
        assert_eq!(unknown["error"], json!("stale_element"));
    }

    // Every scope, as a listing publishes it: the scope it read, the references
    // it hands out, and for a skeleton what it cut.
    #[test]
    fn a_scoped_listing_says_which_scope_it_read() {
        let gate = boot_gate();
        let app = ax::Recorder::new(
            FIXTURE_PID,
            vec![
                ax::Scripted {
                    label: Some("Document".to_string()),
                    frame: Some(frame_at(0.0, 0.0)),
                    children: vec![1, 2],
                    ..ax::Scripted::of("AXWindow")
                },
                ax::Scripted::button("Save", frame_at(100.0, 200.0)),
                ax::Scripted {
                    label: Some("Clear all items?".to_string()),
                    frame: Some(frame_at(0.0, 0.0)),
                    children: vec![3],
                    ..ax::Scripted::of("AXSheet")
                },
                ax::Scripted::button("Clear", frame_at(100.0, 260.0)),
            ],
        );
        app.focuses(Some(0), Some(1));
        let platform: Rc<dyn ax::Ax> = app.clone();
        let (geom, region) = one_to_one();
        let view = Viewport {
            geom: &geom,
            region: &region,
        };
        let scene = semantic::Scene {
            pid: FIXTURE_PID,
            window: None,
        };
        let owner = Some((FIXTURE_PID, app.started_at()));
        let listed = |scope: semantic::Scope, shape: tree::Shape| {
            let listing = semantic::Listing {
                scope: Some(scope),
                shape,
                root: None,
            };
            scoped_listing(&platform, &scene, owner, &listing, &view, &gate)
                .unwrap_or_else(|failure| panic!("{}: {failure:?}", scope.as_str()))
        };
        let labels = |payload: &Value| -> Vec<String> {
            payload["elements"]
                .as_array()
                .expect("a listing")
                .iter()
                .map(|item| item["label"].as_str().unwrap_or("-").to_string())
                .collect()
        };

        let (dialog, references) = listed(semantic::Scope::Dialog, tree::Shape::default());
        assert_eq!(dialog["scope"], json!("dialog"));
        assert_eq!(labels(&dialog), vec!["Clear"]);
        assert_eq!(references.map(|elements| elements.len()), Some(1));

        let (window, _) = listed(semantic::Scope::Window, tree::Shape::default());
        assert_eq!(labels(&window), vec!["Save", "Clear"]);

        let (focused, _) = listed(semantic::Scope::Focused, tree::Shape::default());
        assert_eq!(labels(&focused), vec!["Save"]);

        let skeleton = tree::Shape {
            skeleton: true,
            ..tree::Shape::default()
        };
        let (cut, _) = listed(semantic::Scope::App, skeleton);
        assert_eq!(cut["skeleton"], json!({"depth": 3, "hidden": 0}));

        let find = tree::Shape {
            query: Some(tree::Query {
                role: Some("dialog".to_string()),
                name: None,
            }),
            ..tree::Shape::default()
        };
        let (found, _) = listed(semantic::Scope::App, find);
        assert_eq!(labels(&found), vec!["Clear all items?"]);
        assert_eq!(found["elements"][0]["role"], json!("dialog"));
        assert_eq!(found["elements"][0]["native_role"], json!("AXSheet"));

        app.focuses(None, None);
        let missing = semantic::Listing {
            scope: Some(semantic::Scope::Focused),
            shape: tree::Shape::default(),
            root: None,
        };
        let refused = scoped_listing(&platform, &scene, owner, &missing, &view, &gate)
            .expect_err("nothing is focused");
        assert_eq!(refused.code, "scope_not_found");
    }

    #[test]
    fn an_action_this_build_does_not_have_is_named_as_such() {
        let refused = dispatch(json!({"action": "teleport"})).expect_err("no such action");
        assert_eq!(refused.code, "unknown_action");
    }

    // The Linux handshake does not offer `inspect`, and a caller that sends it
    // anyway is told so before anything is read or captured.
    #[cfg(target_os = "linux")]
    #[test]
    fn on_linux_inspect_is_an_action_this_build_does_not_have() {
        let refused =
            dispatch(json!({"action": "inspect", "x": 1, "y": 1, "observation_id": "o-1"}))
                .expect_err("not on Linux");

        assert_eq!(refused.code, "unknown_action");
        assert!(refused
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("elements")));
    }

    // Off macOS and Linux every action that reads or acts on controls says the
    // build has no accessibility API, rather than refusing the control.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn off_macos_every_control_action_says_there_is_no_accessibility() {
        for body in [
            json!({"action": "inspect", "x": 1, "y": 1, "observation_id": "o-1"}),
            json!({"action": "press", "element_ref": "e1", "observation_id": "o-1"}),
            json!({"action": "focus", "element_ref": "e1", "observation_id": "o-1"}),
            json!({"action": "toggle", "element_ref": "e1", "observation_id": "o-1", "on": true}),
            json!({"action": "wait_for", "until": {"kind": "menu_open"}}),
            json!({"action": "type", "text": "x", "element_ref": "e1", "observation_id": "o-1"}),
        ] {
            let refused = dispatch(body.clone()).expect_err(&body.to_string());
            assert_eq!(refused.code, "accessibility_unavailable", "{body}");
        }
    }

    // The handshake offers the new verbs exactly where controls can be read.
    #[test]
    fn the_new_verbs_are_offered_only_where_controls_can_be_read() {
        for verb in [
            "focus",
            "select",
            "toggle",
            "expand",
            "collapse",
            "scroll_to",
            "wait_for",
        ] {
            assert!(ACTIONS.contains(&verb), "{verb} is a verb");
            assert!(
                ACCESSIBILITY_ACTIONS.contains(&verb),
                "{verb} needs an accessibility API"
            );
        }
    }
}
