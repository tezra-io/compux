//! compux CAPTURE mode (MILESTONE_32 §8.4a) — the observe rail.
//!
//! Unlike the request/response core (`main.rs`), capture is an unsolicited event
//! PUSH: after `observe_start` is acked, the sidecar streams NDJSON event frames
//! on stdout until `observe_stop` (or teardown). The wire is defined by the Fermix
//! `Capturer` + `Wire` decoder (the authority); this module produces exactly the
//! frames that decoder reads.
//!
//! Architecture (the two halves never share a thread with the request loop):
//!   * a serialized [`Emitter`] (an `Arc<Mutex<Stdout>>`) that both the main loop
//!     and this engine write through, so a multi-write screenshot response and an
//!     event frame can never split each other's line;
//!   * on macOS, a dedicated observer thread that owns a `CFRunLoop`, attaches an
//!     `AXObserver` to the frontmost ALLOWLISTED app (the battery SLO, §8.5), and
//!     a poll timer for app switches. `observe_stop` (and the two `main.rs` exit
//!     paths) raise a cooperative stop flag the poll timer honors — stopping the
//!     run loop from INSIDE it (a cross-thread `CFRunLoopStop` is lost if it races
//!     the loop's startup) — then join.
//!
//! Everything above the `#[cfg(target_os = "macos")]` engine is portable and unit
//! tested; the engine itself is unsafe AX/CF FFI whose live behavior needs a real
//! Mac with the Accessibility grant (the same caveat the `ax` module carries).

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

// --- Emitter: the one serialized writer both halves share --------------------

/// A cloneable handle to the process stdout, guarded so the request loop and the
/// observer thread never interleave a partial line. Each `emit` holds the lock
/// across the whole `writeln!` + flush, making line atomicity structural.
#[derive(Clone)]
pub struct Emitter {
    out: Arc<Mutex<io::Stdout>>,
}

impl Emitter {
    pub fn new() -> Self {
        Emitter {
            out: Arc::new(Mutex::new(io::stdout())),
        }
    }

    /// Write one already-serialized JSON line (no trailing newline needed). Returns
    /// `Err` only when the parent Port is gone — the caller (the main loop) treats
    /// that as end-of-session.
    pub fn emit_line(&self, line: &str) -> io::Result<()> {
        // A poisoned lock means a writer thread panicked mid-line; recover the guard
        // rather than propagate — a dropped frame is a gap, never a downed sidecar.
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(out, "{line}")?;
        out.flush()
    }

    /// Serialize and emit a frame. A frame that fails to serialize is dropped (it
    /// can never happen for the maps we build, but we never panic the engine).
    pub fn emit_frame(&self, frame: &Value) {
        if let Ok(line) = serde_json::to_string(frame) {
            let _ = self.emit_line(&line);
        }
    }
}

impl Default for Emitter {
    fn default() -> Self {
        Self::new()
    }
}

// --- wire contract: frame builders (portable, unit-tested) -------------------

/// The event-frame envelope version (§8.4a `"v":1`). The Fermix decoder ignores it;
/// we still stamp it so the wire is self-describing. The wire `protocol_version`
/// (the handshake gate) lives in `main.rs` (`PROTOCOL_VERSION`), which builds the
/// `observe_start`/`observe_stop` acks — one source of truth for the version.
const ENVELOPE_V: u32 = 1;

/// Per-field / per-title / per-value byte cap. AX hands back whole field values
/// (§8.3), so the sidecar owns truncation; a truncated field still reports its full
/// `char_len` and rides a `gap{truncated}` so volume is never silently lost. Kept
/// well under the Capturer's 1 MiB frame ceiling.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// The identity of the app an event is scoped to. `bundle_id` is the allowlist key.
#[derive(Clone, Debug, PartialEq)]
pub struct AppIdentity {
    pub bundle_id: Option<String>,
    pub name: Option<String>,
    pub pid: i32,
}

/// Milliseconds since the Unix epoch — the `ts` field (an integer; the decoder
/// rejects a float).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build an app-scoped event frame. `extra` carries the kind-specific fields
/// (`role`, `text`, `window_title`, …) exactly as §8.4 names them; `seq` is the
/// per-boot monotonic counter and `boot_id` the per-process generation, together
/// idempotent under the spool's `UNIQUE(boot_id, source_seq)`.
pub fn event_frame(
    boot_id: &str,
    seq: u64,
    kind: &str,
    app: Option<&AppIdentity>,
    extra: Map<String, Value>,
) -> Value {
    let mut frame = Map::new();
    frame.insert("type".into(), json!("event"));
    frame.insert("v".into(), json!(ENVELOPE_V));
    frame.insert("ts".into(), json!(now_ms()));
    frame.insert("seq".into(), json!(seq));
    frame.insert("boot_id".into(), json!(boot_id));
    frame.insert("kind".into(), json!(kind));

    if let Some(app) = app {
        // `name`/`pid` are informational (the decoder reads only `bundle_id`); we
        // send the full object so the wire is complete and future-proof.
        frame.insert(
            "app".into(),
            json!({ "bundle_id": app.bundle_id, "name": app.name, "pid": app.pid }),
        );
    }

    for (k, v) in extra {
        frame.insert(k, v);
    }

    Value::Object(frame)
}

/// A self-authored `observer.gap` (§8.4 taxonomy). This v1 sidecar authors the
/// capture-side reasons `secure_input`, `grant_revoked`, and `private_unknown`;
/// Fermix authors the transport-side ones under its own boot_id. `from`/`to`
/// bracket the covered interval (both epoch-ms).
pub fn gap_frame(boot_id: &str, seq: u64, reason: GapReason, from_ms: i64, to_ms: i64) -> Value {
    let mut extra = Map::new();
    extra.insert("gap_reason".into(), json!(reason.as_str()));
    extra.insert("gap_from_ts".into(), json!(from_ms));
    extra.insert("gap_to_ts".into(), json!(to_ms));
    event_frame(boot_id, seq, "observer.gap", None, extra)
}

/// The capture-side `gap_reason` values this v1 sidecar authors (a subset of the
/// §8.4 enumeration; `sleep`/`ax_timeout` arrive with v1.1's sleep-notification and
/// read-timeout wiring). A typed enum keeps a stray string off the wire.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GapReason {
    SecureInput,
    GrantRevoked,
    PrivateUnknown,
}

impl GapReason {
    pub fn as_str(self) -> &'static str {
        match self {
            GapReason::SecureInput => "secure_input",
            GapReason::GrantRevoked => "grant_revoked",
            GapReason::PrivateUnknown => "private_unknown",
        }
    }
}

/// Truncate `text` to at most `MAX_TEXT_BYTES` on a char boundary. Returns the
/// (possibly shortened) text and whether it was truncated — the caller emits a
/// `gap{truncated}`-equivalent by reporting the FULL `char_len` regardless.
pub fn truncate_text(text: &str) -> (String, bool) {
    if text.len() <= MAX_TEXT_BYTES {
        return (text.to_string(), false);
    }
    let mut end = MAX_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// The bundle-id set treated as a web browser. A browser's field/selection content
/// cannot be site-correlated in v1 (no tab/window ref, no private-window signal),
/// so its content is WITHHELD before it crosses the Port (§13.2) — never sent for
/// Fermix to filter later. `browser.navigated` + full correlation is v1.1.
pub fn is_browser(bundle_id: &str) -> bool {
    matches!(
        bundle_id,
        "com.apple.Safari"
            | "com.apple.SafariTechnologyPreview"
            | "com.google.Chrome"
            | "com.google.Chrome.canary"
            | "com.microsoft.edgemac"
            | "com.brave.Browser"
            | "org.mozilla.firefox"
            | "com.operasoftware.Opera"
            | "company.thebrowser.Browser"
            | "com.vivaldi.Vivaldi"
            | "com.arc.Arc"
    )
}

/// Roles whose value changes are actual user-entered content (§22.5). A value
/// change on any other element — notably `AXStaticText` (a label/title/breadcrumb)
/// — is NOT typed input and must not ride `field.value`; it rides
/// `window.title_changed`/`focus.changed` instead. Secure fields are editable and
/// pass here so they still get their `gap{secure_input}` suppression.
pub fn is_editable_role(role: Option<&str>) -> bool {
    matches!(
        role,
        Some("AXTextField")
            | Some("AXTextArea")
            | Some("AXComboBox")
            | Some("AXSearchField")
            | Some("AXSecureTextField")
    )
}

/// Membership test for the app allowlist (§8.5 / inv. 11). An empty bundle id is
/// never allowlisted (a system/agent process is not the owner's activity).
pub fn app_allowed(bundle_id: &Option<String>, allow: &[String]) -> bool {
    match bundle_id {
        Some(id) if !id.is_empty() => allow.iter().any(|a| a == id),
        _ => false,
    }
}

// --- session control (portable facade) ---------------------------------------

/// The resolved allowlists an `observe_start` carries.
#[derive(Clone, Debug, Default)]
pub struct ObserveConfig {
    pub apps: Vec<String>,
    // The site allowlist is parsed (the full observe_start contract) but not yet
    // consulted: v1 withholds browser content entirely rather than site-filter it,
    // so per-site enforcement lands with v1.1's browser.navigated correlation.
    #[allow(dead_code)]
    pub sites: Vec<String>,
}

impl ObserveConfig {
    /// Parse `{"action":"observe_start","params":{"apps":[…],"sites":[…]}}`. Missing
    /// or malformed params default to empty allowlists (default-deny; Ingest
    /// re-enforces at the write boundary, so a permissive parse can never leak).
    pub fn from_request(req: &Value) -> ObserveConfig {
        let params = req.get("params");
        ObserveConfig {
            apps: string_list(params, "apps"),
            sites: string_list(params, "sites"),
        }
    }
}

fn string_list(params: Option<&Value>, key: &str) -> Vec<String> {
    params
        .and_then(|p| p.get(key))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Start capture. Returns `Ok(())` when the observer session is running (the
/// `observe_start` ack then reports `ok:true`), or `Err` with a reason (acked
/// `ok:false`). Idempotent-safe: a second start with a session already running is
/// an error rather than a leaked second observer.
pub fn start(req: &Value, emitter: Emitter) -> Result<(), String> {
    let config = ObserveConfig::from_request(req);
    imp::start(config, emitter)
}

/// Stop capture and join the observer thread. A no-op when nothing is running, so
/// it is safe to call from every teardown path.
pub fn stop() {
    imp::stop();
}

// --- macOS engine ------------------------------------------------------------

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use core_foundation::base::{CFTypeRef, TCFType};
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    // The live session: the cooperative stop flag + the thread to join. A module
    // static mirrors `ACTIVATED`/`CAPTURE_WEDGED` in main.rs. Guarded so
    // observe_start / observe_stop / teardown never race the handle.
    static SESSION: Mutex<Option<Session>> = Mutex::new(None);

    struct Session {
        thread: JoinHandle<()>,
        // The single shutdown signal. The observer's own poll timer reads it and
        // stops its run loop FROM INSIDE the loop — the only race-free way, since a
        // cross-thread `CFRunLoopStop` is silently lost if it lands before the loop
        // is running (observed: an `observe_stop` right after `observe_start` hung
        // the join forever). The flag persists regardless of run-loop state.
        stop: Arc<std::sync::atomic::AtomicBool>,
    }

    pub fn start(config: ObserveConfig, emitter: Emitter) -> Result<(), String> {
        if !ax_trusted() {
            return Err("accessibility permission not granted".to_string());
        }

        let mut guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Err("capture already running".to_string());
        }

        // A one-shot readiness signal so `start` can confirm the observer booted
        // (and report a refusal if it didn't) — `()` carries no non-Send ref.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = stop.clone();

        let thread = std::thread::Builder::new()
            .name("compux-observe".into())
            .spawn(move || observe_loop(config, emitter, stop_thread, tx))
            .map_err(|e| format!("failed to spawn observer thread: {e}"))?;

        // Wait (bounded) for the observer to finish setup and signal ready. If it
        // never does, the thread failed to boot — stop it and refuse.
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(()) => {
                *guard = Some(Session { thread, stop });
                Ok(())
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                let _ = thread.join();
                Err("observer thread failed to start".to_string())
            }
        }
    }

    pub fn stop() {
        let session = {
            let mut guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };

        if let Some(session) = session {
            // Raise the cooperative stop flag and wait. The observer's poll timer
            // (≤ FRONTMOST_POLL_S) sees it and stops its run loop from inside; the
            // thread then detaches its observer and exits, and this join completes.
            // This is race-free where a cross-thread CFRunLoopStop is not.
            session.stop.store(true, Ordering::SeqCst);
            let _ = session.thread.join();
        }
    }

    // --- the observer thread -------------------------------------------------

    // Poll cadence for frontmost-app changes. Fine-grained WITHIN an app is
    // AXObserver-driven (real time); switching apps is coarse and cheap to poll.
    const FRONTMOST_POLL_S: f64 = 0.3;
    // Debounce: emit a field.value only after edits settle, so a burst of
    // keystrokes yields one content event, not one per character.
    const VALUE_DEBOUNCE_S: f64 = 0.6;
    // A generous run-loop wake to re-check the accessibility grant.
    const GRANT_CHECK_EVERY: u32 = 20; // × FRONTMOST_POLL_S ≈ 6s

    fn observe_loop(
        config: ObserveConfig,
        emitter: Emitter,
        stop: Arc<std::sync::atomic::AtomicBool>,
        ready: std::sync::mpsc::Sender<()>,
    ) {
        let boot_id = boot_id();
        let seq = AtomicU64::new(0);

        // Boxed so the callbacks' raw refcon has a single stable address and there
        // is one access path (the raw pointer) once the run loop is live — the
        // callbacks only fire inside `CFRunLoopRun`, after all setup below.
        let mut ctx = Box::new(Ctx {
            config,
            emitter,
            stop,
            boot_id,
            seq: &seq,
            attached: None,
            pending_value: None,
            debounce_timer: std::ptr::null_mut(),
            poll_ticks: 0,
        });

        let ctx_ptr = ctx.as_mut() as *mut Ctx as *mut c_void;
        let run_loop = unsafe { CFRunLoopGetCurrent() };

        // Timer 1: frontmost-app poll (repeating). Timer 2: value debounce (one-shot,
        // rescheduled per edit — starts far in the future, effectively idle).
        let poll_timer =
            unsafe { make_timer(FRONTMOST_POLL_S, FRONTMOST_POLL_S, frontmost_tick, ctx_ptr) };
        let debounce_timer = unsafe { make_timer(f64::MAX, 0.0, value_settle, ctx_ptr) };
        ctx.debounce_timer = debounce_timer;

        unsafe {
            CFRunLoopAddTimer(run_loop, poll_timer, kCFRunLoopDefaultMode);
            CFRunLoopAddTimer(run_loop, debounce_timer, kCFRunLoopDefaultMode);
            // Attach to whatever is frontmost now (setup done).
            reconcile_frontmost(&mut ctx);
        }

        // Signal ready AFTER setup so `start`'s ack reflects a live observer. From
        // here the poll timer honors the stop flag, so an observe_stop that already
        // set it is picked up on the first tick — no lost-stop race.
        if ready.send(()).is_err() {
            unsafe { detach(&mut ctx) };
            return;
        }

        unsafe { CFRunLoopRun() };

        // Torn down (observe_stop / grant revocation / EOF): detach cleanly.
        unsafe {
            detach(&mut ctx);
            CFRelease(poll_timer as CFTypeRef);
            CFRelease(debounce_timer as CFTypeRef);
        }
    }

    // Per-thread engine state. Every field is touched ONLY on the observer thread
    // (the timers and the AX callback all run on this one run loop), so a raw
    // `*mut Ctx` refcon needs no lock; the cross-thread `Emitter` carries its own.
    struct Ctx<'a> {
        config: ObserveConfig,
        emitter: Emitter,
        stop: Arc<std::sync::atomic::AtomicBool>,
        boot_id: String,
        seq: &'a AtomicU64,
        // The app we currently observe: (pid, identity, AX app element, AXObserver).
        attached: Option<Attached>,
        // The element with an un-settled value edit + the timestamp it started.
        pending_value: Option<PendingValue>,
        debounce_timer: CFRunLoopTimerRef,
        poll_ticks: u32,
    }

    struct Attached {
        identity: AppIdentity,
        app_element: CFTypeRef,
        observer: AXObserverRef,
    }

    struct PendingValue {
        element: CFTypeRef,
        started_ms: i64,
    }

    impl Ctx<'_> {
        fn next_seq(&self) -> u64 {
            self.seq.fetch_add(1, Ordering::SeqCst)
        }

        fn emit(&self, kind: &str, app: Option<&AppIdentity>, extra: Map<String, Value>) {
            let frame = event_frame(&self.boot_id, self.next_seq(), kind, app, extra);
            self.emitter.emit_frame(&frame);
        }

        fn emit_gap(&self, reason: GapReason, from_ms: i64, to_ms: i64) {
            let frame = gap_frame(&self.boot_id, self.next_seq(), reason, from_ms, to_ms);
            self.emitter.emit_frame(&frame);
        }
    }

    // --- frontmost tracking --------------------------------------------------

    extern "C" fn frontmost_tick(_timer: CFRunLoopTimerRef, info: *mut c_void) {
        let ctx = unsafe { &mut *(info as *mut Ctx) };

        // The cooperative stop: observe_stop / teardown set the flag; we stop the
        // run loop FROM INSIDE the callback, which always takes effect (a
        // cross-thread stop can be lost). This is the single shutdown path.
        if ctx.stop.load(Ordering::SeqCst) {
            unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
            return;
        }

        // Fail closed on a grant revoked mid-session: mark a gap and stop, so a
        // silently-dead observer never masquerades as "capturing".
        ctx.poll_ticks = ctx.poll_ticks.wrapping_add(1);
        if ctx.poll_ticks % GRANT_CHECK_EVERY == 0 && !ax_trusted() {
            let now = now_ms();
            ctx.emit_gap(GapReason::GrantRevoked, now, now);
            ctx.stop.store(true, Ordering::SeqCst);
            unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
            return;
        }

        unsafe { reconcile_frontmost(ctx) };
    }

    // Attach observers to the frontmost app iff it is allowlisted; detach on a
    // switch. Emits `app.activated` (+ `prev_bundle_id`) on any change.
    unsafe fn reconcile_frontmost(ctx: &mut Ctx) {
        let front = match frontmost_app() {
            Some(app) => app,
            None => return,
        };

        let same = ctx
            .attached
            .as_ref()
            .map(|a| a.identity.pid == front.pid)
            .unwrap_or(false);
        if same {
            return;
        }

        let prev_bundle = ctx
            .attached
            .as_ref()
            .and_then(|a| a.identity.bundle_id.clone());

        detach(ctx);

        // app.activated is emitted regardless of allowlist membership? No — only
        // allowlisted apps are the owner's tracked activity (§8.5); a switch to a
        // non-allowlisted app is simply "we stopped observing", not an event.
        if !app_allowed(&front.bundle_id, &ctx.config.apps) {
            return;
        }

        let mut extra = Map::new();
        if let Some(prev) = prev_bundle {
            extra.insert("prev_bundle_id".into(), json!(prev));
        }
        ctx.emit("app.activated", Some(&front), extra);

        attach(ctx, front);
    }

    // Attach an AXObserver to `app`'s AX element for the notifications we translate.
    unsafe fn attach(ctx: &mut Ctx, app: AppIdentity) {
        let app_element = AXUIElementCreateApplication(app.pid);
        if app_element.is_null() {
            return;
        }

        let mut observer: AXObserverRef = std::ptr::null_mut();
        if AXObserverCreate(app.pid, ax_callback, &mut observer) != 0 || observer.is_null() {
            CFRelease(app_element);
            return;
        }

        let ctx_ptr = ctx as *mut Ctx as *mut c_void;
        for name in OBSERVED_NOTIFICATIONS {
            let cf = CFString::new(name);
            // A refused notification (e.g. an app that exposes no AX tree) is not
            // fatal — we observe what we can; the rest is a coverage gap, not a crash.
            let _ =
                AXObserverAddNotification(observer, app_element, cf.as_concrete_TypeRef(), ctx_ptr);
        }

        let source = AXObserverGetRunLoopSource(observer);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);

        ctx.attached = Some(Attached {
            identity: app,
            app_element,
            observer,
        });
    }

    // Detach the current observer (remove notifications, drop the run-loop source,
    // release the AX refs). Idempotent.
    unsafe fn detach(ctx: &mut Ctx) {
        cancel_pending_value(ctx);

        if let Some(attached) = ctx.attached.take() {
            let source = AXObserverGetRunLoopSource(attached.observer);
            CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            for name in OBSERVED_NOTIFICATIONS {
                let cf = CFString::new(name);
                let _ = AXObserverRemoveNotification(
                    attached.observer,
                    attached.app_element,
                    cf.as_concrete_TypeRef(),
                );
            }
            CFRelease(attached.observer as CFTypeRef);
            CFRelease(attached.app_element);
        }
    }

    // The AX notifications we translate to §8.4 event kinds.
    const OBSERVED_NOTIFICATIONS: &[&str] = &[
        "AXFocusedUIElementChanged",
        "AXValueChanged",
        "AXFocusedWindowChanged",
        "AXTitleChanged",
    ];

    // --- the AX callback (runs on the observer run loop) ---------------------

    extern "C" fn ax_callback(
        _observer: AXObserverRef,
        element: CFTypeRef,
        notification: CFStringRef,
        info: *mut c_void,
    ) {
        let ctx = unsafe { &mut *(info as *mut Ctx) };
        let note = unsafe { cfstring_to_string(notification) }.unwrap_or_default();

        match note.as_str() {
            "AXFocusedUIElementChanged" => unsafe { on_focus_changed(ctx, element) },
            "AXValueChanged" => unsafe { on_value_changed(ctx, element) },
            "AXFocusedWindowChanged" => unsafe { on_window(ctx, "window.focused") },
            "AXTitleChanged" => unsafe { on_window(ctx, "window.title_changed") },
            _ => {}
        }
    }

    unsafe fn on_focus_changed(ctx: &mut Ctx, element: CFTypeRef) {
        // A focus change abandons any un-settled edit in the previous field.
        cancel_pending_value(ctx);

        let app = match &ctx.attached {
            Some(a) => a.identity.clone(),
            None => return,
        };

        let mut extra = Map::new();
        insert_str(&mut extra, "role", read_attr(element, "AXRole"));
        insert_str(
            &mut extra,
            "role_desc",
            read_attr(element, "AXRoleDescription"),
        );
        insert_str(&mut extra, "field_label", field_label(element));
        insert_str(&mut extra, "window_title", focused_window_title(&app));
        ctx.emit("focus.changed", Some(&app), extra);
    }

    unsafe fn on_value_changed(ctx: &mut Ctx, element: CFTypeRef) {
        // Debounce: (re)start the settle timer and remember the element. The actual
        // read + emit happens in value_settle when edits go quiet.
        let now = now_ms();
        let started = ctx
            .pending_value
            .as_ref()
            .map(|p| p.started_ms)
            .unwrap_or(now);

        // Retain the element for the lifetime of the pending window (CopyAttribute
        // gave us borrowed refs elsewhere; here we hold a raw element the callback
        // owns for the duration of the run loop, so retain to be safe).
        CFRetain(element);
        if let Some(prev) = ctx.pending_value.take() {
            CFRelease(prev.element);
        }
        ctx.pending_value = Some(PendingValue {
            element,
            started_ms: started,
        });

        reschedule(ctx.debounce_timer, VALUE_DEBOUNCE_S);
    }

    unsafe fn on_window(ctx: &mut Ctx, kind: &str) {
        let app = match &ctx.attached {
            Some(a) => a.identity.clone(),
            None => return,
        };
        let mut extra = Map::new();
        insert_str(&mut extra, "window_title", focused_window_title(&app));
        ctx.emit(kind, Some(&app), extra);
    }

    // The debounce fired: read the settled value and emit field.value. In a browser
    // the CONTENT is withheld (metadata only) until v1.1 correlates tabs/privacy.
    extern "C" fn value_settle(_timer: CFRunLoopTimerRef, info: *mut c_void) {
        let ctx = unsafe { &mut *(info as *mut Ctx) };
        let pending = match ctx.pending_value.take() {
            Some(p) => p,
            None => return,
        };

        let app = match &ctx.attached {
            Some(a) => a.identity.clone(),
            None => {
                unsafe { CFRelease(pending.element) };
                return;
            }
        };

        unsafe { emit_field_value(ctx, &app, pending.element, pending.started_ms) };
        unsafe { CFRelease(pending.element) };
    }

    unsafe fn emit_field_value(ctx: &Ctx, app: &AppIdentity, element: CFTypeRef, started_ms: i64) {
        let role = read_attr(element, "AXRole");

        // Only editable fields carry typed content; a value change on a label/title
        // (AXStaticText, …) is not user input and emits nothing here (§22.5).
        if !is_editable_role(role.as_deref()) {
            return;
        }

        let mut extra = Map::new();
        insert_str(&mut extra, "role", role.clone());
        insert_str(&mut extra, "field_label", field_label(element));

        // Secure fields (password entry) are NEVER read for content — mark a
        // gap{secure_input} and emit no text (§7.1).
        if role.as_deref() == Some("AXSecureTextField") {
            ctx.emit_gap(GapReason::SecureInput, started_ms, now_ms());
            extra.insert("content_withheld".into(), json!(true));
            ctx.emit("field.value", Some(app), extra);
            return;
        }

        let is_browser_app = app.bundle_id.as_deref().map(is_browser).unwrap_or(false);
        let value = read_attr(element, "AXValue");

        match value {
            Some(text) if is_browser_app => {
                // Can't site-correlate or detect a private window in v1 → withhold
                // the content before it crosses the Port (§13.2), keep the volume.
                extra.insert("char_len".into(), json!(char_len(&text)));
                extra.insert("content_withheld".into(), json!(true));
                extra.insert("private_state".into(), json!("unknown"));
                ctx.emit("field.value", Some(app), extra);
                ctx.emit_gap(GapReason::PrivateUnknown, started_ms, now_ms());
            }
            Some(text) => {
                let full_len = char_len(&text);
                let (sent, truncated) = truncate_text(&text);
                extra.insert("text".into(), json!(sent));
                extra.insert("char_len".into(), json!(full_len));
                extra.insert("content_withheld".into(), json!(false));
                ctx.emit("field.value", Some(app), extra);
                if truncated {
                    // A truncated field still reported its full char_len; a gap marks
                    // that the bytes were clipped (Fermix authors "truncated" too, but
                    // ours is boot-scoped to this event's timeline).
                    ctx.emit_gap_reason_truncated(started_ms);
                }
            }
            None => {
                // No readable value (AX refused) — record volume-less metadata.
                extra.insert("content_withheld".into(), json!(true));
                ctx.emit("field.value", Some(app), extra);
            }
        }
    }

    impl Ctx<'_> {
        // A "truncated" gap is a transport reason Fermix also authors; the sidecar
        // uses the shared spelling so a reader sees one vocabulary. Kept here (not in
        // the portable GapReason enum) because it is not a capture-only reason.
        fn emit_gap_reason_truncated(&self, from_ms: i64) {
            let mut extra = Map::new();
            extra.insert("gap_reason".into(), json!("truncated"));
            extra.insert("gap_from_ts".into(), json!(from_ms));
            extra.insert("gap_to_ts".into(), json!(now_ms()));
            let frame = event_frame(&self.boot_id, self.next_seq(), "observer.gap", None, extra);
            self.emitter.emit_frame(&frame);
        }
    }

    unsafe fn cancel_pending_value(ctx: &mut Ctx) {
        if let Some(prev) = ctx.pending_value.take() {
            CFRelease(prev.element);
        }
    }

    // --- AX reads (single-shot, on the observer thread) ----------------------

    // Read a string attribute of an element; None if absent or non-string.
    unsafe fn read_attr(element: CFTypeRef, attr: &str) -> Option<String> {
        let cf = CFString::new(attr);
        let mut out: CFTypeRef = std::ptr::null_mut();
        if AXUIElementCopyAttributeValue(element, cf.as_concrete_TypeRef(), &mut out) != 0
            || out.is_null()
        {
            return None;
        }
        let s = cfstring_to_string(out as CFStringRef);
        CFRelease(out);
        s
    }

    // The human label of a field: AXTitle, else AXDescription.
    unsafe fn field_label(element: CFTypeRef) -> Option<String> {
        read_attr(element, "AXTitle").or_else(|| read_attr(element, "AXDescription"))
    }

    // Title of the app's focused window (best-effort).
    unsafe fn focused_window_title(app: &AppIdentity) -> Option<String> {
        let app_element = AXUIElementCreateApplication(app.pid);
        if app_element.is_null() {
            return None;
        }
        let cf = CFString::new("AXFocusedWindow");
        let mut window: CFTypeRef = std::ptr::null_mut();
        let rc = AXUIElementCopyAttributeValue(app_element, cf.as_concrete_TypeRef(), &mut window);
        let title = if rc == 0 && !window.is_null() {
            let t = read_attr(window, "AXTitle");
            CFRelease(window);
            t
        } else {
            None
        };
        CFRelease(app_element);
        title
    }

    // --- frontmost app + identity (CGWindowList + CFBundle, no objc2) ---------

    // The frontmost normal (layer-0) window's owner is the frontmost app. Reads
    // pid + owner name from CGWindowList (no Screen-Recording grant needed for
    // pid/owner-name), then resolves the bundle id from the executable path.
    unsafe fn frontmost_app() -> Option<AppIdentity> {
        let options = K_CG_ON_SCREEN_ONLY | K_CG_EXCLUDE_DESKTOP;
        let list = CGWindowListCopyWindowInfo(options, K_CG_NULL_WINDOW_ID);
        if list.is_null() {
            return None;
        }

        let mut found: Option<(i32, Option<String>)> = None;
        let count = CFArrayGetCount(list);
        let mut i = 0;
        while i < count {
            let dict = CFArrayGetValueAtIndex(list, i);
            i += 1;
            if dict.is_null() {
                continue;
            }
            // Only real app windows (layer 0); skip menu bar / overlays / cursor.
            if dict_i64(dict, "kCGWindowLayer") != Some(0) {
                continue;
            }
            let pid = match dict_i64(dict, "kCGWindowOwnerPID") {
                Some(p) => p as i32,
                None => continue,
            };
            let name = dict_string(dict, "kCGWindowOwnerName");
            found = Some((pid, name));
            break;
        }

        CFRelease(list);

        let (pid, name) = found?;
        Some(AppIdentity {
            bundle_id: bundle_id_for_pid(pid),
            name,
            pid,
        })
    }

    // Resolve a bundle id from a pid: executable path → enclosing `.app` → CFBundle.
    unsafe fn bundle_id_for_pid(pid: i32) -> Option<String> {
        let mut buf = [0u8; 4096];
        let len = proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32);
        if len <= 0 {
            return None;
        }
        let path = std::str::from_utf8(&buf[..len as usize]).ok()?;
        let app_dir = enclosing_app_bundle(path)?;

        let cf_path = CFString::new(app_dir);
        let url = CFURLCreateWithFileSystemPath(
            std::ptr::null(),
            cf_path.as_concrete_TypeRef(),
            K_CF_URL_POSIX,
            true,
        );
        if url.is_null() {
            return None;
        }
        let bundle = CFBundleCreate(std::ptr::null(), url);
        CFRelease(url as CFTypeRef);
        if bundle.is_null() {
            return None;
        }
        // CFBundleGetIdentifier follows the GET rule (do not release the result).
        let id_ref = CFBundleGetIdentifier(bundle);
        let id = if id_ref.is_null() {
            None
        } else {
            cfstring_to_string(id_ref)
        };
        CFRelease(bundle as CFTypeRef);
        id
    }

    // Walk up `/Applications/Safari.app/Contents/MacOS/Safari` to the `.app` dir.
    fn enclosing_app_bundle(exe_path: &str) -> Option<&str> {
        let idx = exe_path.find(".app/")?;
        Some(&exe_path[..idx + 4])
    }

    // --- CFString helpers ----------------------------------------------------

    unsafe fn cfstring_to_string(s: CFStringRef) -> Option<String> {
        if s.is_null() {
            return None;
        }
        // Reuse core-foundation's tested conversion (handles encoding + memory).
        let cf = CFString::wrap_under_get_rule(s);
        Some(cf.to_string())
    }

    fn char_len(s: &str) -> i64 {
        s.chars().count() as i64
    }

    fn insert_str(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
        if let Some(v) = value {
            map.insert(key.to_string(), json!(v));
        }
    }

    // --- CFDictionary readers (CGWindow info) --------------------------------

    unsafe fn dict_i64(dict: CFTypeRef, key: &str) -> Option<i64> {
        let cf_key = CFString::new(key);
        let value = CFDictionaryGetValue(dict, cf_key.as_concrete_TypeRef() as CFTypeRef);
        if value.is_null() {
            return None;
        }
        let mut out: i64 = 0;
        // kCFNumberSInt64Type = 4.
        if CFNumberGetValue(value, 4, &mut out as *mut i64 as *mut c_void) {
            Some(out)
        } else {
            None
        }
    }

    unsafe fn dict_string(dict: CFTypeRef, key: &str) -> Option<String> {
        let cf_key = CFString::new(key);
        let value = CFDictionaryGetValue(dict, cf_key.as_concrete_TypeRef() as CFTypeRef);
        if value.is_null() {
            return None;
        }
        cfstring_to_string(value as CFStringRef)
    }

    // --- accessibility trust (reused gate) -----------------------------------

    fn ax_trusted() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    // --- CFRunLoopTimer construction + reschedule ----------------------------

    unsafe fn make_timer(
        first_delay_s: f64,
        interval_s: f64,
        callout: extern "C" fn(CFRunLoopTimerRef, *mut c_void),
        info: *mut c_void,
    ) -> CFRunLoopTimerRef {
        let fire_at = if first_delay_s == f64::MAX {
            f64::MAX
        } else {
            CFAbsoluteTimeGetCurrent() + first_delay_s
        };
        let mut context = CFRunLoopTimerContext {
            version: 0,
            info,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };
        CFRunLoopTimerCreate(
            std::ptr::null(),
            fire_at,
            interval_s,
            0,
            0,
            callout,
            &mut context,
        )
    }

    unsafe fn reschedule(timer: CFRunLoopTimerRef, delay_s: f64) {
        CFRunLoopTimerSetNextFireDate(timer, CFAbsoluteTimeGetCurrent() + delay_s);
    }

    // --- per-process boot id -------------------------------------------------

    fn boot_id() -> String {
        // Unique per sidecar process: pid + start nanos. (boot_id, seq) is what the
        // spool dedupes on, so uniqueness — not RFC-4122 shape — is what matters.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("compux-{}-{}", std::process::id(), nanos)
    }

    // --- FFI: types + externs ------------------------------------------------

    type AXUIElementRef = CFTypeRef;
    type AXObserverRef = *mut c_void;
    type CFRunLoopRef = *mut c_void;
    type CFRunLoopSourceRef = *mut c_void;
    type CFRunLoopTimerRef = *mut c_void;
    type CFAllocatorRef = *const c_void;
    type CFURLRef = *mut c_void;
    type CFBundleRef = *mut c_void;

    type AXObserverCallback =
        extern "C" fn(AXObserverRef, AXUIElementRef, CFStringRef, *mut c_void);
    type CFRunLoopTimerCallBack = extern "C" fn(CFRunLoopTimerRef, *mut c_void);

    #[repr(C)]
    struct CFRunLoopTimerContext {
        version: isize,
        info: *mut c_void,
        retain: *const c_void,
        release: *const c_void,
        copy_description: *const c_void,
    }

    const K_CG_ON_SCREEN_ONLY: u32 = 1 << 0;
    const K_CG_EXCLUDE_DESKTOP: u32 = 1 << 4;
    const K_CG_NULL_WINDOW_ID: u32 = 0;
    const K_CF_URL_POSIX: u32 = 0; // kCFURLPOSIXPathStyle

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
        fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXObserverCreate(
            application: i32,
            callback: AXObserverCallback,
            out_observer: *mut AXObserverRef,
        ) -> i32;
        fn AXObserverAddNotification(
            observer: AXObserverRef,
            element: AXUIElementRef,
            notification: CFStringRef,
            refcon: *mut c_void,
        ) -> i32;
        fn AXObserverRemoveNotification(
            observer: AXObserverRef,
            element: AXUIElementRef,
            notification: CFStringRef,
        ) -> i32;
        fn AXObserverGetRunLoopSource(observer: AXObserverRef) -> CFRunLoopSourceRef;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        // Signatures mirror the ones main.rs already declares (CFTypeRef throughout)
        // so the shared symbols never redeclare with a divergent ABI.
        fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFTypeRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFRunLoopDefaultMode: CFStringRef;

        fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
        fn CFRelease(cf: CFTypeRef);
        fn CFArrayGetCount(array: CFTypeRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
        fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        fn CFNumberGetValue(number: CFTypeRef, the_type: isize, out: *mut c_void) -> bool;

        fn CFAbsoluteTimeGetCurrent() -> f64;
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopRun();
        fn CFRunLoopStop(rl: CFRunLoopRef);
        fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
        fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
        fn CFRunLoopAddTimer(rl: CFRunLoopRef, timer: CFRunLoopTimerRef, mode: CFStringRef);
        fn CFRunLoopTimerCreate(
            allocator: CFAllocatorRef,
            fire_date: f64,
            interval: f64,
            flags: u32,
            order: isize,
            callout: CFRunLoopTimerCallBack,
            context: *mut CFRunLoopTimerContext,
        ) -> CFRunLoopTimerRef;
        fn CFRunLoopTimerSetNextFireDate(timer: CFRunLoopTimerRef, fire_date: f64);

        fn CFURLCreateWithFileSystemPath(
            allocator: CFAllocatorRef,
            file_path: CFStringRef,
            path_style: u32,
            is_directory: bool,
        ) -> CFURLRef;
        fn CFBundleCreate(allocator: CFAllocatorRef, bundle_url: CFURLRef) -> CFBundleRef;
        fn CFBundleGetIdentifier(bundle: CFBundleRef) -> CFStringRef;
    }

    extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
    }
}

// --- non-macOS stub ----------------------------------------------------------

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub fn start(_config: ObserveConfig, _emitter: Emitter) -> Result<(), String> {
        Err("computer-history capture is only supported on macOS".to_string())
    }

    pub fn stop() {}
}

// --- tests (portable frame/contract logic) -----------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> AppIdentity {
        AppIdentity {
            bundle_id: Some("com.apple.TextEdit".into()),
            name: Some("TextEdit".into()),
            pid: 42,
        }
    }

    #[test]
    fn event_frame_carries_the_required_envelope() {
        let frame = event_frame("boot-x", 7, "focus.changed", Some(&identity()), Map::new());
        assert_eq!(frame["type"], json!("event"));
        assert_eq!(frame["v"], json!(1));
        assert_eq!(frame["seq"], json!(7));
        assert_eq!(frame["boot_id"], json!("boot-x"));
        assert_eq!(frame["kind"], json!("focus.changed"));
        assert!(frame["ts"].is_i64(), "ts must be an integer, not a float");
        assert_eq!(frame["app"]["bundle_id"], json!("com.apple.TextEdit"));
        // scan_flag is NEVER on the wire (Ingest computes it).
        assert!(frame.get("scan_flag").is_none());
    }

    #[test]
    fn system_events_have_no_app_object() {
        let frame = gap_frame("boot-x", 1, GapReason::SecureInput, 100, 200);
        assert!(frame.get("app").is_none());
        assert_eq!(frame["kind"], json!("observer.gap"));
        assert_eq!(frame["gap_reason"], json!("secure_input"));
        assert_eq!(frame["gap_from_ts"], json!(100));
        assert_eq!(frame["gap_to_ts"], json!(200));
    }

    #[test]
    fn gap_reasons_match_the_taxonomy() {
        assert_eq!(GapReason::SecureInput.as_str(), "secure_input");
        assert_eq!(GapReason::GrantRevoked.as_str(), "grant_revoked");
        assert_eq!(GapReason::PrivateUnknown.as_str(), "private_unknown");
    }

    #[test]
    fn truncate_respects_the_cap_and_char_boundaries() {
        let short = "hello";
        assert_eq!(truncate_text(short), ("hello".into(), false));

        let long = "a".repeat(MAX_TEXT_BYTES + 100);
        let (sent, truncated) = truncate_text(&long);
        assert!(truncated);
        assert!(sent.len() <= MAX_TEXT_BYTES);

        // A multibyte char straddling the cap must not split.
        let multibyte = "é".repeat(MAX_TEXT_BYTES); // 2 bytes each
        let (sent, truncated) = truncate_text(&multibyte);
        assert!(truncated);
        assert!(sent.is_char_boundary(sent.len()));
    }

    #[test]
    fn allowlist_is_default_deny() {
        let allow = vec!["com.apple.Safari".to_string()];
        assert!(app_allowed(&Some("com.apple.Safari".into()), &allow));
        assert!(!app_allowed(&Some("com.evil.Keylogger".into()), &allow));
        assert!(!app_allowed(&None, &allow));
        assert!(!app_allowed(&Some("".into()), &allow));
        assert!(!app_allowed(&Some("com.apple.Safari".into()), &[]));
    }

    #[test]
    fn browser_detection_covers_the_majors() {
        assert!(is_browser("com.apple.Safari"));
        assert!(is_browser("com.google.Chrome"));
        assert!(!is_browser("com.apple.TextEdit"));
    }

    #[test]
    fn field_value_only_for_editable_roles() {
        assert!(is_editable_role(Some("AXTextField")));
        assert!(is_editable_role(Some("AXTextArea")));
        assert!(is_editable_role(Some("AXComboBox")));
        assert!(is_editable_role(Some("AXSearchField")));
        // Secure fields pass so they still get gap{secure_input} suppression.
        assert!(is_editable_role(Some("AXSecureTextField")));
        // A label/title/breadcrumb is NOT field.value content (the live VSCode case).
        assert!(!is_editable_role(Some("AXStaticText")));
        assert!(!is_editable_role(Some("AXButton")));
        assert!(!is_editable_role(None));
    }

    #[test]
    fn observe_config_parses_apps_and_sites() {
        let req = json!({
            "action": "observe_start",
            "params": {"apps": ["com.apple.Safari", "com.apple.mail"], "sites": ["github.com"]}
        });
        let cfg = ObserveConfig::from_request(&req);
        assert_eq!(cfg.apps, vec!["com.apple.Safari", "com.apple.mail"]);
        assert_eq!(cfg.sites, vec!["github.com"]);
    }

    #[test]
    fn observe_config_defaults_to_deny_on_missing_params() {
        let cfg = ObserveConfig::from_request(&json!({"action": "observe_start"}));
        assert!(cfg.apps.is_empty());
        assert!(cfg.sites.is_empty());
    }
}
