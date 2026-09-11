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

// The frame builders and the state machines below are the portable half of the
// engine: on macOS the AX engine consumes every one of them (and the unit tests
// exercise them everywhere), but on a target whose engine is the stub nothing does.
// Scoped to non-macOS so a genuinely unused helper still fails the macOS gate.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

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
    out: Arc<Mutex<Sink>>,
}

/// Where emitted lines go. The buffer exists so the engine's own emit paths (a gap
/// that must carry its app, a debounce that must flush on detach) are asserted on the
/// FRAMES they produce, not on a builder called in isolation.
enum Sink {
    Stdout(io::Stdout),
    #[cfg(test)]
    Buffer(Vec<String>),
}

impl Emitter {
    pub fn new() -> Self {
        Emitter {
            out: Arc::new(Mutex::new(Sink::Stdout(io::stdout()))),
        }
    }

    /// An emitter that collects frames instead of writing them (tests only).
    #[cfg(test)]
    pub fn capturing() -> Self {
        Emitter {
            out: Arc::new(Mutex::new(Sink::Buffer(Vec::new()))),
        }
    }

    /// Every frame emitted so far, parsed back from the wire (tests only).
    #[cfg(test)]
    pub fn captured(&self) -> Vec<Value> {
        let out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        match &*out {
            Sink::Buffer(lines) => lines
                .iter()
                .map(|line| serde_json::from_str(line).expect("emitted a non-JSON line"))
                .collect(),
            Sink::Stdout(_) => panic!("captured() on a stdout emitter"),
        }
    }

    /// Write one already-serialized JSON line (no trailing newline needed). Returns
    /// `Err` only when the parent Port is gone — the caller (the main loop) treats
    /// that as end-of-session.
    pub fn emit_line(&self, line: &str) -> io::Result<()> {
        // A poisoned lock means a writer thread panicked mid-line; recover the guard
        // rather than propagate — a dropped frame is a gap, never a downed sidecar.
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *out {
            Sink::Stdout(stdout) => {
                writeln!(stdout, "{line}")?;
                stdout.flush()
            }
            #[cfg(test)]
            Sink::Buffer(lines) => {
                lines.push(line.to_string());
                Ok(())
            }
        }
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
/// capture-side reasons `secure_input`, `grant_revoked`, `private_unknown`,
/// `title_only`, and `ax_refused:<notifications>`; Fermix authors the transport-side
/// ones under its own boot_id. `from`/`to` bracket the covered interval (both
/// epoch-ms).
///
/// `app` says whose coverage the gap is about: a per-app COVERAGE gap (`title_only`,
/// `ax_refused:…`) carries the app object so the decoder attributes it through the
/// same `bundle_id` path every other event uses; a SYSTEM gap (`grant_revoked`,
/// `secure_input`, `private_unknown`) is app-less because it is not a property of one
/// app.
pub fn gap_frame(
    boot_id: &str,
    seq: u64,
    reason: GapReason,
    from_ms: i64,
    to_ms: i64,
    app: Option<&AppIdentity>,
) -> Value {
    let mut extra = Map::new();
    extra.insert("gap_reason".into(), json!(reason.as_str()));
    extra.insert("gap_from_ts".into(), json!(from_ms));
    extra.insert("gap_to_ts".into(), json!(to_ms));
    event_frame(boot_id, seq, "observer.gap", app, extra)
}

/// The capture-side `gap_reason` values this v1 sidecar authors (a subset of the
/// §8.4 enumeration; `sleep`/`ax_timeout` arrive with v1.1's sleep-notification and
/// read-timeout wiring). A typed enum keeps a stray string off the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GapReason {
    SecureInput,
    GrantRevoked,
    PrivateUnknown,
    /// The app's accessibility tree could not be switched on, so only window
    /// titles are observable for it — never a `field.value` (§8.2).
    TitleOnly,
    /// `AXObserverAddNotification` refused notifications the field-level rail depends
    /// on. WHICH ones is part of the reason, so a reader can tell a narrow refusal
    /// from a total one and the once-per-app ledger treats a wider refusal later as a
    /// new fact.
    AxRefused(RefusedSet),
}

/// The content notifications an attach was refused. Two flags rather than a list so
/// the whole reason — including its wire spelling — stays a `Copy` value that can key
/// a ledger.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RefusedSet {
    pub focused_ui_element: bool,
    pub value: bool,
}

impl RefusedSet {
    pub fn is_empty(self) -> bool {
        !self.focused_ui_element && !self.value
    }

    /// The wire spelling, e.g. `ax_refused:AXValueChanged`. The empty set never
    /// reaches the wire (nothing was refused, so no gap is authored).
    fn as_str(self) -> &'static str {
        match (self.value, self.focused_ui_element) {
            (true, true) => "ax_refused:AXValueChanged,AXFocusedUIElementChanged",
            (true, false) => "ax_refused:AXValueChanged",
            (false, true) => "ax_refused:AXFocusedUIElementChanged",
            (false, false) => "ax_refused",
        }
    }
}

impl GapReason {
    pub fn as_str(self) -> &'static str {
        match self {
            GapReason::SecureInput => "secure_input",
            GapReason::GrantRevoked => "grant_revoked",
            GapReason::PrivateUnknown => "private_unknown",
            GapReason::TitleOnly => "title_only",
            GapReason::AxRefused(refused) => refused.as_str(),
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

// --- coverage classification + debounce state (portable, unit-tested) --------

/// What an attached app can deliver, as an OBSERVATION rather than a verdict.
/// Chromium/Electron keep their accessibility tree OFF until a client asks for it
/// (`AXManualAccessibility`), so an app whose tree stays off delivers window titles
/// and nothing else — the live 48h incident: 38,590 `window.title_changed`, zero
/// `field.value`. `TitleOnly` is that state, NAMED. `Unknown` is the answer when the
/// app did not answer at all (mid-launch, busy, no AX connection yet): it announces
/// nothing and is re-probed, because a transient non-answer must never be published
/// as a standing coverage claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxTree {
    Enabled,
    TitleOnly,
    Unknown,
}

/// One AX call's answer, reduced to what the classification turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxAnswer {
    /// The call succeeded: a set that took, or a read that returned a value.
    Ok,
    /// The typed "this attribute is not a thing here" (`kAXErrorAttributeUnsupported`
    /// / `kAXErrorNotImplemented`).
    Unsupported,
    /// `kAXErrorNoValue` — the attribute exists and holds nothing right now. For a
    /// focused-element read that is a real tree with nothing focused.
    NoValue,
    /// Anything else: cannot-complete, API disabled, an app that did not reply. NOT
    /// evidence about coverage.
    Unanswered,
}

/// Classify an app's observable surface from the two AX answers the probe collects:
/// what setting `AXManualAccessibility` said, and — only when that was unsupported —
/// what reading `AXFocusedUIElement` said afterwards (`None` when no read was made).
///
/// * the switch took → `Enabled` (a Chromium/Electron tree is now on);
/// * switch unsupported, focused element readable (a value, or `NoValue` with nothing
///   focused) → `Enabled` (a native tree that never needed a switch);
/// * switch unsupported and the focused element unsupported too → `TitleOnly`, the
///   only definitive gap;
/// * anything unanswered, at either step → `Unknown`, which publishes nothing.
pub fn classify_ax_tree(manual_set: AxAnswer, focused_read: Option<AxAnswer>) -> AxTree {
    match (manual_set, focused_read) {
        (AxAnswer::Ok, _) => AxTree::Enabled,
        (AxAnswer::Unsupported, Some(AxAnswer::Ok | AxAnswer::NoValue)) => AxTree::Enabled,
        (AxAnswer::Unsupported, Some(AxAnswer::Unsupported)) => AxTree::TitleOnly,
        _ => AxTree::Unknown,
    }
}

/// Whether detach must switch an activation attribute back off: only when WE turned it
/// on. An app whose owner (a screen reader, another assistive client) already had it on
/// must be left exactly as it was, so a prior `true` records nothing.
pub fn should_record_activation(prior: Option<bool>) -> bool {
    prior != Some(true)
}

/// Whether a debounce burst that began at `started_ms` must settle NOW: a burst that
/// keeps being extended would otherwise never report at all — a continuously
/// re-titling window, or one long uninterrupted typing run.
pub fn burst_expired(started_ms: i64, now_ms: i64, max_wait_ms: i64) -> bool {
    now_ms.saturating_sub(started_ms) >= max_wait_ms
}

/// When the caller must settle a recorded title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettleWhen {
    /// The burst has outrun its ceiling: settle in this callback.
    Immediately,
    /// Normal case: (re)schedule the debounce timer.
    AfterDebounce,
}

/// Window-title debounce for ONE attached app. `AXTitleChanged` is unthrottled — VS
/// Code re-titles several times a second while a task spins (31,778 of the incident's
/// 38,590 title frames were one braille spinner glyph apart) — so the latest title is
/// recorded and only the SETTLED one is emitted, and only when it differs from the
/// last title already reported for that app. A burst that never goes quiet still
/// reports at the ceiling.
#[derive(Debug, Default)]
pub struct TitleDebounce {
    pending: Option<String>,
    first_observed_ms: Option<i64>,
    last_emitted: Option<String>,
}

impl TitleDebounce {
    /// Record the newest title seen, and say whether the caller must settle now
    /// (the burst outran `max_wait_ms`) or reschedule the debounce.
    pub fn observe(&mut self, title: String, now_ms: i64, max_wait_ms: i64) -> SettleWhen {
        let started = *self.first_observed_ms.get_or_insert(now_ms);
        self.pending = Some(title);
        if burst_expired(started, now_ms, max_wait_ms) {
            SettleWhen::Immediately
        } else {
            SettleWhen::AfterDebounce
        }
    }

    /// The settle timer fired (or a flush ran): the title to emit, if any. An identical
    /// settled title, or no pending title at all, emits nothing.
    pub fn settle(&mut self) -> Option<String> {
        self.first_observed_ms = None;
        let title = self.pending.take()?;
        if self.last_emitted.as_deref() == Some(title.as_str()) {
            return None;
        }
        self.last_emitted = Some(title.clone());
        Some(title)
    }

    /// Record a title this app has ALREADY reported through another event (the
    /// `window_title` of a `window.focused`), so the `AXTitleChanged` that follows an
    /// app switch does not report the same title a second time.
    pub fn seed_emitted(&mut self, title: Option<String>) {
        if title.is_some() {
            self.last_emitted = title;
        }
    }
}

/// A once-per-key ledger. Coverage gaps are announced once per app per reason (a gap
/// describes a standing condition, not an event), and the attach-failure logs use the
/// same discipline so a 0.3 s poll can never turn one broken app into a log flood.
#[derive(Debug)]
pub struct Ledger<K: Eq + std::hash::Hash> {
    seen: std::collections::HashSet<K>,
}

impl<K: Eq + std::hash::Hash> Default for Ledger<K> {
    fn default() -> Self {
        Ledger {
            seen: std::collections::HashSet::new(),
        }
    }
}

impl<K: Eq + std::hash::Hash> Ledger<K> {
    /// True the FIRST time this key is announced.
    pub fn announce(&mut self, key: K) -> bool {
        self.seen.insert(key)
    }
}

/// What a bounded retry is counting. A probe and an attach retry independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Attempt {
    Probe,
    Attach,
}

/// Bounded per-app attempt counters. The difference between "retry until the app
/// answers" and "retry forever" is this cap, so both retries state theirs.
#[derive(Debug, Default)]
pub struct AttemptLedger {
    counts: std::collections::HashMap<(String, Attempt), u32>,
}

impl AttemptLedger {
    /// Consume one attempt, returning false once `max` have been spent.
    pub fn take_attempt(&mut self, bundle_id: &str, attempt: Attempt, max: u32) -> bool {
        let spent = self
            .counts
            .entry((bundle_id.to_string(), attempt))
            .or_insert(0);
        if *spent >= max {
            return false;
        }
        *spent += 1;
        true
    }

    /// Whether every attempt of this kind has been spent (the point at which a
    /// degradation stops being transient and becomes a fact worth announcing).
    pub fn exhausted(&self, bundle_id: &str, attempt: Attempt, max: u32) -> bool {
        self.counts
            .get(&(bundle_id.to_string(), attempt))
            .copied()
            .unwrap_or(0)
            >= max
    }
}

/// The at-most-one element whose `AXValueChanged` we registered directly (the
/// app-level registration is not guaranteed to be delivered for descendants). Pure
/// bookkeeping: it decides WHICH element the caller must unregister and release, the
/// unsafe layer makes the AX calls. Identity is the caller's to define — two distinct
/// `AXUIElementRef`s can name the same element, so the engine passes `CFEqual`.
#[derive(Debug, Default)]
pub struct WatchSlot<T> {
    current: Option<T>,
}

impl<T: Copy> WatchSlot<T> {
    /// Already watching this element (a re-notification for the same focus)?
    pub fn is_current(&self, candidate: T, same: impl Fn(T, T) -> bool) -> bool {
        self.current
            .map(|held| same(held, candidate))
            .unwrap_or(false)
    }

    /// Hand back the watched element so the caller unregisters + releases it. Used by
    /// both the focus change and detach, so a registration is never orphaned.
    pub fn take(&mut self) -> Option<T> {
        self.current.take()
    }

    /// Remember the element the caller has just registered.
    pub fn set(&mut self, element: T) {
        self.current = Some(element);
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
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
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
    // The same treatment for window titles, which arrive unthrottled: a spinner in
    // a title bar re-titles several times a second, and every frame of it used to
    // become an event (31,778 of one 48h session's 38,590 title frames).
    const TITLE_DEBOUNCE_S: f64 = 1.0;
    // The debounce timers are rescheduled per edit, but they must NOT be created as
    // one-shots: CF invalidates a timer whose interval is 0 the moment it fires, and
    // `CFRunLoopTimerSetNextFireDate` on an invalid timer does nothing — a one-shot
    // debounce therefore settles once per session and is silent forever after. A long
    // repeat keeps the timer valid; between edits it re-arms this far out, which is
    // one no-op wake an hour (`a_rescheduled_debounce_timer_fires_more_than_once`).
    const DEBOUNCE_REARM_S: f64 = 3600.0;
    // Ceilings for both debounces (TITLE_MAX_WAIT_S / VALUE_MAX_WAIT_S = 5.0, held in
    // ms because the burst is measured against event timestamps): a window that
    // re-titles without pause, or one long uninterrupted typing run, still reports at
    // this interval instead of being deferred forever by its own next edit.
    const TITLE_MAX_WAIT_MS: i64 = 5_000;
    const VALUE_MAX_WAIT_MS: i64 = 5_000;
    // A generous run-loop wake to re-check the accessibility grant.
    const GRANT_CHECK_EVERY: u32 = 20; // × FRONTMOST_POLL_S ≈ 6s
                                       // Bounded retries, per app per session. An app that is mid-launch answers nothing
                                       // about its accessibility tree and refuses notifications; that is transient, so it
                                       // is retried — but a fixed number of times, and the failure is announced only when
                                       // the attempts are spent (never on the transient first look).
    const PROBE_ATTEMPTS: u32 = 3;
    const ATTACH_ATTEMPTS: u32 = 3;

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
            front_pid: None,
            pending_value: None,
            watched_value: WatchSlot::default(),
            gap_ledger: Ledger::default(),
            log_ledger: Ledger::default(),
            attempts: AttemptLedger::default(),
            debounce_timer: std::ptr::null_mut(),
            title_timer: std::ptr::null_mut(),
            poll_ticks: 0,
        });

        let ctx_ptr = ctx.as_mut() as *mut Ctx as *mut c_void;
        let run_loop = unsafe { CFRunLoopGetCurrent() };

        // Timer 1: frontmost-app poll (repeating). Timers 2 and 3: the value and
        // title debounces, rescheduled per edit — they start far in the future,
        // effectively idle, and repeat on a long interval so CF keeps them valid
        // across fires (see DEBOUNCE_REARM_S).
        let poll_timer =
            unsafe { make_timer(FRONTMOST_POLL_S, FRONTMOST_POLL_S, frontmost_tick, ctx_ptr) };
        let debounce_timer =
            unsafe { make_timer(f64::MAX, DEBOUNCE_REARM_S, value_settle, ctx_ptr) };
        let title_timer = unsafe { make_timer(f64::MAX, DEBOUNCE_REARM_S, title_settle, ctx_ptr) };
        ctx.debounce_timer = debounce_timer;
        ctx.title_timer = title_timer;

        unsafe {
            CFRunLoopAddTimer(run_loop, poll_timer, kCFRunLoopDefaultMode);
            CFRunLoopAddTimer(run_loop, debounce_timer, kCFRunLoopDefaultMode);
            CFRunLoopAddTimer(run_loop, title_timer, kCFRunLoopDefaultMode);
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
            CFRelease(title_timer as CFTypeRef);
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
        // The frontmost pid we have already reconciled to, attached or not: an app we
        // announced but could not attach to must be retried without re-announcing.
        front_pid: Option<i32>,
        // The element with an un-settled value edit + the timestamp it started.
        pending_value: Option<PendingValue>,
        // The focused element whose AXValueChanged we registered directly.
        watched_value: WatchSlot<CFTypeRef>,
        // Which apps have already had a named coverage gap announced.
        gap_ledger: Ledger<(String, GapReason)>,
        // Which apps have already logged a given attach failure (never per poll tick).
        log_ledger: Ledger<(String, &'static str)>,
        // Bounded per-app retries for the tree probe and the attach itself.
        attempts: AttemptLedger,
        debounce_timer: CFRunLoopTimerRef,
        title_timer: CFRunLoopTimerRef,
        poll_ticks: u32,
    }

    struct Attached {
        identity: AppIdentity,
        app_element: CFTypeRef,
        observer: AXObserverRef,
        // What this app can deliver. `Unknown` is re-probed (bounded) rather than
        // published, so a mid-launch app is never recorded as title-only.
        ax_tree: AxTree,
        // Per-app window-title debounce (an app switch starts a fresh one).
        title: TitleDebounce,
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

        // `app` is Some only for a gap that is a property of ONE app (its coverage);
        // a system gap — the grant went away, a secure field, a browser's unknown
        // privacy state — is app-less.
        fn emit_gap(&self, reason: GapReason, from_ms: i64, to_ms: i64, app: Option<&AppIdentity>) {
            let frame = gap_frame(&self.boot_id, self.next_seq(), reason, from_ms, to_ms, app);
            self.emitter.emit_frame(&frame);
        }
    }

    // The observer thread's only diagnostic channel. stdout is the NDJSON frame
    // wire, so a log line goes to stderr — every AX call this engine cannot make
    // lands here, so a degraded session is explainable after the fact. Anything that
    // can repeat on the 0.3 s poll goes through `log_once`.
    fn log(msg: &str) {
        eprintln!("compux: capture: {msg}");
    }

    // One line per app per topic, for failures a poll tick would otherwise repeat
    // forever.
    fn log_once(ctx: &mut Ctx, app: &AppIdentity, topic: &'static str, msg: &str) {
        let key = (app_label(app), topic);
        if ctx.log_ledger.announce(key) {
            log(msg);
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
            // Everything still in a debounce belongs to the covered interval, so it is
            // emitted BEFORE the gap: no event may follow the frame that says coverage
            // ended.
            unsafe { flush_pending(ctx) };
            let now = now_ms();
            ctx.emit_gap(GapReason::GrantRevoked, now, now, None);
            ctx.stop.store(true, Ordering::SeqCst);
            unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
            return;
        }

        unsafe { reconcile_frontmost(ctx) };
    }

    // Attach observers to the frontmost app iff it is allowlisted; detach on a
    // switch. Emits `app.activated` (+ `prev_bundle_id`) on any change. An app we
    // announced but could not attach to is retried on later ticks WITHOUT a second
    // `app.activated` — the switch happened once.
    unsafe fn reconcile_frontmost(ctx: &mut Ctx) {
        let front = match frontmost_app() {
            Some(app) => app,
            None => return,
        };

        if ctx.attached.as_ref().map(|a| a.identity.pid) == Some(front.pid) {
            return;
        }

        let switched = ctx.front_pid != Some(front.pid);
        if switched {
            let prev_bundle = ctx
                .attached
                .as_ref()
                .and_then(|a| a.identity.bundle_id.clone());
            detach(ctx);
            ctx.front_pid = Some(front.pid);

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
        } else if !app_allowed(&front.bundle_id, &ctx.config.apps) {
            return;
        }

        attach(ctx, front);
    }

    // Attach an AXObserver to `app`'s AX element for the notifications we translate.
    // Either the whole attach succeeds or nothing is left behind: a half-registered
    // observer (one that will never deliver content) is torn down so the next tick can
    // retry, bounded by ATTACH_ATTEMPTS per app per session.
    unsafe fn attach(ctx: &mut Ctx, app: AppIdentity) {
        let bundle = match app.bundle_id.clone() {
            Some(bundle) => bundle,
            // Unreachable via the allowlist (which requires a bundle id); belt so a
            // future caller cannot attach an unidentifiable app.
            None => return,
        };
        // Only FAILED attaches spend the budget — an app the owner switches to twenty
        // times attaches twenty times.
        if ctx
            .attempts
            .exhausted(&bundle, Attempt::Attach, ATTACH_ATTEMPTS)
        {
            return;
        }

        let app_element = AXUIElementCreateApplication(app.pid);
        if app_element.is_null() {
            spend_attach_attempt(ctx, &bundle);
            log_once(
                ctx,
                &app,
                "no_ax_connection",
                &format!(
                    "{bundle}: no AX connection to pid {} — not attached",
                    app.pid
                ),
            );
            return;
        }

        let mut observer: AXObserverRef = std::ptr::null_mut();
        let rc = AXObserverCreate(app.pid, ax_callback, &mut observer);
        if rc != 0 || observer.is_null() {
            spend_attach_attempt(ctx, &bundle);
            log_once(
                ctx,
                &app,
                "observer_create_refused",
                &format!("{bundle}: AXObserverCreate refused (AXError {rc}) — not attached"),
            );
            CFRelease(app_element);
            return;
        }

        // Ask for the accessibility tree BEFORE registering: a Chromium/Electron app
        // whose tree is off delivers window titles only, and the registrations below
        // are what would silently do nothing for its content.
        let ax_tree = probe_ax_tree(ctx, &app, app_element);

        let ctx_ptr = ctx as *mut Ctx as *mut c_void;
        let refused = add_notifications(observer, app_element, ctx_ptr, &app);
        if !refused.is_empty() {
            spend_attach_attempt(ctx, &bundle);
            abandon_attach(ctx, &app, app_element, observer, refused);
            return;
        }

        let source = AXObserverGetRunLoopSource(observer);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);

        ctx.attached = Some(Attached {
            identity: app,
            app_element,
            observer,
            ax_tree,
            title: TitleDebounce::default(),
        });

        announce_title_only(ctx);
    }

    fn spend_attach_attempt(ctx: &mut Ctx, bundle: &str) {
        ctx.attempts
            .take_attempt(bundle, Attempt::Attach, ATTACH_ATTEMPTS);
    }

    // A content notification was refused: release everything this attach created (the
    // observer was never added to the run loop, so there is no source to remove) and
    // leave nothing attached. The refusal becomes a named gap only once the retries are
    // spent — a mid-launch app refuses and then works.
    unsafe fn abandon_attach(
        ctx: &mut Ctx,
        app: &AppIdentity,
        app_element: CFTypeRef,
        observer: AXObserverRef,
        refused: RefusedSet,
    ) {
        // Releasing the observer drops every registration it holds, so the partial set
        // needs no per-notification removal here.
        for failure in crate::ax::clear_activation(app.pid) {
            log(&format!("{}: {failure}", app_label(app)));
        }
        CFRelease(observer as CFTypeRef);
        CFRelease(app_element);

        announce_refusal(ctx, app, refused);
    }

    // A refusal becomes a NAMED gap only once the bounded retries are spent: an app
    // that refuses while it is still launching is transient, and a transient refusal
    // published as a coverage gap would be a lie about the session. The ledger key
    // carries WHICH notifications were refused, so a wider refusal later is a new fact
    // and gets its own announcement.
    fn announce_refusal(ctx: &mut Ctx, app: &AppIdentity, refused: RefusedSet) {
        let Some(bundle) = app.bundle_id.clone() else {
            return;
        };
        if refused.is_empty()
            || !ctx
                .attempts
                .exhausted(&bundle, Attempt::Attach, ATTACH_ATTEMPTS)
        {
            return;
        }

        let reason = GapReason::AxRefused(refused);
        if ctx.gap_ledger.announce((bundle.clone(), reason)) {
            log(&format!(
                "{bundle}: {} after {ATTACH_ATTEMPTS} attempts — no content is observable",
                reason.as_str()
            ));
            let now = now_ms();
            ctx.emit_gap(reason, now, now, Some(app));
        }
    }

    // Register every notification we translate, returning the CONTENT ones that were
    // refused. Window-level refusals are best-effort and only logged.
    unsafe fn add_notifications(
        observer: AXObserverRef,
        app_element: CFTypeRef,
        ctx_ptr: *mut c_void,
        app: &AppIdentity,
    ) -> RefusedSet {
        let mut refused = RefusedSet::default();
        for name in OBSERVED_NOTIFICATIONS {
            let cf = CFString::new(name);
            let rc =
                AXObserverAddNotification(observer, app_element, cf.as_concrete_TypeRef(), ctx_ptr);
            if rc == 0 {
                continue;
            }
            log(&format!(
                "{}: {name} refused (AXError {rc})",
                app_label(app)
            ));
            match *name {
                FOCUS_CHANGED => refused.focused_ui_element = true,
                VALUE_CHANGED => refused.value = true,
                _ => {}
            }
        }
        refused
    }

    // Name a definitive title-only app, once per app per session (a standing condition,
    // not an event), so the store and `/history status` can say why it only ever
    // produced titles. `Unknown` publishes nothing — it is re-probed instead.
    fn announce_title_only(ctx: &mut Ctx) {
        let Some((app, tree)) = ctx
            .attached
            .as_ref()
            .map(|a| (a.identity.clone(), a.ax_tree))
        else {
            return;
        };
        let Some(bundle) = app.bundle_id.clone() else {
            return;
        };
        if tree != AxTree::TitleOnly {
            return;
        }
        if ctx
            .gap_ledger
            .announce((bundle.clone(), GapReason::TitleOnly))
        {
            log(&format!(
                "{bundle}: title-only coverage — no editable content is observable"
            ));
            let now = now_ms();
            ctx.emit_gap(GapReason::TitleOnly, now, now, Some(&app));
        }
    }

    // Detach the current observer (flush both debounces, drop the focused-element
    // registration, remove notifications, clear our activation, drop the run-loop
    // source, release the AX refs). Idempotent.
    unsafe fn detach(ctx: &mut Ctx) {
        // Everything still pending belongs to the interval this app was attached for,
        // so it is emitted while the app is still attached.
        flush_pending(ctx);

        let Some(attached) = ctx.attached.take() else {
            return;
        };

        if let Some(element) = ctx.watched_value.take() {
            remove_notification(attached.observer, element, VALUE_CHANGED);
            CFRelease(element);
        }

        // The AX refs are non-null by construction (attach stores nothing else); the
        // guard is what lets the teardown path be exercised by a test that owns no AX
        // objects, and CFRelease(NULL) would abort the process.
        if !attached.observer.is_null() {
            let source = AXObserverGetRunLoopSource(attached.observer);
            CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            for name in OBSERVED_NOTIFICATIONS {
                remove_notification(attached.observer, attached.app_element, name);
            }
            CFRelease(attached.observer as CFTypeRef);
        }

        for failure in crate::ax::clear_activation(attached.identity.pid) {
            log(&format!("{}: {failure}", app_label(&attached.identity)));
        }

        if !attached.app_element.is_null() {
            CFRelease(attached.app_element);
        }
    }

    unsafe fn remove_notification(observer: AXObserverRef, element: CFTypeRef, name: &str) {
        let cf = CFString::new(name);
        let rc = AXObserverRemoveNotification(observer, element, cf.as_concrete_TypeRef());
        // A refused removal on teardown is benign (the app may have exited), but it
        // is never silent: the observer is released next, which drops the registration.
        if rc != 0 {
            log(&format!("{name} removal refused (AXError {rc})"));
        }
    }

    // The AX notifications we translate to §8.4 event kinds.
    const OBSERVED_NOTIFICATIONS: &[&str] =
        &[FOCUS_CHANGED, VALUE_CHANGED, WINDOW_CHANGED, TITLE_CHANGED];

    // The two the field-level rail depends on: without them no editable content can
    // ever be observed, so a refusal of either is a named gap and the attach is
    // abandoned rather than left half-registered.
    const FOCUS_CHANGED: &str = "AXFocusedUIElementChanged";
    const VALUE_CHANGED: &str = "AXValueChanged";
    // Window-level, best-effort: a refusal is logged, never a gap.
    const WINDOW_CHANGED: &str = "AXFocusedWindowChanged";
    const TITLE_CHANGED: &str = "AXTitleChanged";

    fn app_label(app: &AppIdentity) -> String {
        match (&app.bundle_id, &app.name) {
            (Some(bundle), _) => bundle.clone(),
            (None, Some(name)) => name.clone(),
            (None, None) => format!("pid {}", app.pid),
        }
    }

    // --- accessibility-tree activation (§8.2) --------------------------------

    /// The attribute names live in the `ax` module, which owns the activation policy
    /// and the shared teardown ledger — capture drives the sequence, not a second copy
    /// of the rules.
    use crate::ax::{ENHANCED_UI, MANUAL_ACCESSIBILITY};

    /// `kAXErrorNoValue` — the attribute exists and currently holds nothing. For
    /// `AXFocusedUIElement` that is a real tree with nothing focused, which is coverage,
    /// not a gap.
    const NO_VALUE: i32 = -25212;
    const FOCUSED_UI_ELEMENT: &str = "AXFocusedUIElement";

    // Ask this app for its accessibility tree and OBSERVE what it can deliver. Bounded:
    // an app that answers nothing (mid-launch, busy) is `Unknown` and re-probed later,
    // at most PROBE_ATTEMPTS times per app per session, so a transient non-answer is
    // never published as a coverage claim.
    unsafe fn probe_ax_tree(ctx: &mut Ctx, app: &AppIdentity, app_element: CFTypeRef) -> AxTree {
        let Some(bundle) = app.bundle_id.clone() else {
            return AxTree::Unknown;
        };
        if ctx
            .attempts
            .exhausted(&bundle, Attempt::Probe, PROBE_ATTEMPTS)
        {
            log_once(
                ctx,
                app,
                "probe_exhausted",
                &format!(
                    "{bundle}: accessibility tree still unknown after {PROBE_ATTEMPTS} probes"
                ),
            );
            return AxTree::Unknown;
        }

        let tree = observe_ax_tree(app, app_element, &bundle);
        // Only a NON-answer spends the budget: an app that answers definitively can be
        // re-probed on every future attach without exhausting anything.
        if tree == AxTree::Unknown {
            ctx.attempts
                .take_attempt(&bundle, Attempt::Probe, PROBE_ATTEMPTS);
        }
        tree
    }

    // The AX sequence itself: switch, then — only if the app has no such switch — ask
    // whether it hands out a focused element at all.
    unsafe fn observe_ax_tree(app: &AppIdentity, app_element: CFTypeRef, bundle: &str) -> AxTree {
        // 1. The Chromium/Electron switch. A native app answers the typed "not a thing
        //    here"; anything else means the app did not answer, which decides nothing.
        let manual = switch_on(app, app_element, MANUAL_ACCESSIBILITY);
        if manual == AxAnswer::Ok {
            log(&format!(
                "{bundle}: accessibility tree enabled via {MANUAL_ACCESSIBILITY}"
            ));
            return AxTree::Enabled;
        }
        if manual != AxAnswer::Unsupported {
            log(&format!(
                "{bundle}: {MANUAL_ACCESSIBILITY} unanswered — coverage unknown, will re-probe"
            ));
            return AxTree::Unknown;
        }

        // 2. No switch: does the app hand out a focused element at all? A value or an
        //    empty-but-supported answer is a native tree.
        let focused = read_answer(app_element, FOCUSED_UI_ELEMENT);
        if focused == AxAnswer::Unsupported {
            // 3. Only here — an app that exposes no focused element AND no manual
            //    switch — is the older enhanced-UI switch worth its side effects.
            let enhanced = switch_on(app, app_element, ENHANCED_UI);
            let after = read_answer(app_element, FOCUSED_UI_ELEMENT);
            log(&format!(
                "{bundle}: {FOCUSED_UI_ELEMENT} unsupported, {ENHANCED_UI} {enhanced:?}, re-read {after:?}"
            ));
            return classify_ax_tree(manual, Some(after));
        }

        let tree = classify_ax_tree(manual, Some(focused));
        log(&format!(
            "{bundle}: {MANUAL_ACCESSIBILITY} unsupported, {FOCUSED_UI_ELEMENT} {focused:?} — {tree:?}"
        ));
        tree
    }

    // Switch one activation attribute ON, recording it for teardown ONLY when we are the
    // ones who turned it on: an app whose owner already had it set (a screen reader is
    // running) must be left exactly as it was. The record lives in the `ax` module's one
    // ledger, so process exit undoes it even if this engine never reaches detach.
    unsafe fn switch_on(
        app: &AppIdentity,
        app_element: CFTypeRef,
        attribute: &'static str,
    ) -> AxAnswer {
        let prior = read_bool_attr(app_element, attribute);
        let code = set_bool_attr(app_element, attribute, true);
        if code == 0 && should_record_activation(prior) {
            crate::ax::record_activation(app.pid, attribute);
        }
        answer_for(code)
    }

    // Read an attribute purely to learn whether it is supported.
    unsafe fn read_answer(element: CFTypeRef, attribute: &str) -> AxAnswer {
        let cf = CFString::new(attribute);
        let mut value: CFTypeRef = std::ptr::null_mut();
        let code = AXUIElementCopyAttributeValue(element, cf.as_concrete_TypeRef(), &mut value);
        if !value.is_null() {
            CFRelease(value);
        }
        answer_for(code)
    }

    // One place turns an AXError into the answer the classification reads, so the two
    // typed spellings of "not a thing here" (`attribute_rejected`) cannot drift.
    fn answer_for(code: i32) -> AxAnswer {
        match code {
            0 => AxAnswer::Ok,
            NO_VALUE => AxAnswer::NoValue,
            code if crate::ax::attribute_rejected(code) => AxAnswer::Unsupported,
            _ => AxAnswer::Unanswered,
        }
    }

    // The CURRENT value of a boolean attribute, so an activation we did not perform is
    // never recorded (and so never cleared). `None` = absent, unreadable, or not a
    // boolean at all — none of which is "the app already had it on".
    unsafe fn read_bool_attr(element: CFTypeRef, attribute: &str) -> Option<bool> {
        let cf = CFString::new(attribute);
        let mut value: CFTypeRef = std::ptr::null_mut();
        if AXUIElementCopyAttributeValue(element, cf.as_concrete_TypeRef(), &mut value) != 0
            || value.is_null()
        {
            return None;
        }
        // Copy rule: we own the +1, and the wrapper releases it on drop.
        CFType::wrap_under_create_rule(value)
            .downcast::<CFBoolean>()
            .map(bool::from)
    }

    unsafe fn set_bool_attr(element: CFTypeRef, attribute: &str, value: bool) -> i32 {
        let attr = CFString::new(attribute);
        let flag = if value {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };
        AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), flag.as_CFTypeRef())
    }

    // Re-probe an app whose coverage is still unknown. Called when its focused window
    // changes — the moment a mid-launch app has become a real one — and on every attach.
    unsafe fn reprobe_if_unknown(ctx: &mut Ctx) {
        let Some((app, element, tree)) = ctx
            .attached
            .as_ref()
            .map(|a| (a.identity.clone(), a.app_element, a.ax_tree))
        else {
            return;
        };
        if tree != AxTree::Unknown {
            return;
        }

        let observed = probe_ax_tree(ctx, &app, element);
        if let Some(attached) = ctx.attached.as_mut() {
            attached.ax_tree = observed;
        }
        announce_title_only(ctx);
    }

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
            FOCUS_CHANGED => unsafe { on_focus_changed(ctx, element) },
            VALUE_CHANGED => unsafe { on_value_changed(ctx, element) },
            WINDOW_CHANGED => unsafe { on_window_focused(ctx) },
            TITLE_CHANGED => unsafe { on_title_changed(ctx) },
            _ => {}
        }
    }

    unsafe fn on_focus_changed(ctx: &mut Ctx, element: CFTypeRef) {
        // Blur is a boundary, not a cancellation: the edit the user just made and then
        // left (typed a message, hit send) is READ AND EMITTED here. Cancelling it is
        // how a quick edit inside the debounce window went missing.
        flush_pending_value(ctx);

        let app = match &ctx.attached {
            Some(a) => a.identity.clone(),
            None => return,
        };

        let role = read_attr(element, "AXRole");
        // The app-level AXValueChanged is not guaranteed to be delivered for
        // descendants, so an editable focused element is observed directly too.
        track_value_source(ctx, element, role.as_deref());

        let mut extra = Map::new();
        insert_str(&mut extra, "role", role);
        insert_str(
            &mut extra,
            "role_desc",
            read_attr(element, "AXRoleDescription"),
        );
        insert_str(&mut extra, "field_label", field_label(element));
        insert_str(&mut extra, "window_title", focused_window_title(&app));
        ctx.emit("focus.changed", Some(&app), extra);
    }

    // Register `AXValueChanged` on the focused element itself, keeping at most one
    // such registration alive: a focus change (and detach) unregisters and releases
    // the previous element, so the pairing is one-to-one. A duplicate delivery with
    // the app-level registration is harmless — the debounce settles the same element
    // once.
    unsafe fn track_value_source(ctx: &mut Ctx, element: CFTypeRef, role: Option<&str>) {
        // Two different AXUIElementRefs can name the same element, so identity is
        // CFEqual — a pointer comparison would churn a registration on every
        // re-notification of the same focus.
        if ctx.watched_value.is_current(element, same_element) {
            return;
        }

        let Some(observer) = ctx.attached.as_ref().map(|a| a.observer) else {
            return;
        };

        if let Some(previous) = ctx.watched_value.take() {
            remove_notification(observer, previous, VALUE_CHANGED);
            CFRelease(previous);
        }

        // Only an editable element can carry typed content (§22.5); a label or a
        // button needs no registration.
        if !is_editable_role(role) {
            return;
        }

        let ctx_ptr = ctx as *mut Ctx as *mut c_void;
        let cf = CFString::new(VALUE_CHANGED);
        CFRetain(element);
        let rc = AXObserverAddNotification(observer, element, cf.as_concrete_TypeRef(), ctx_ptr);
        if rc != 0 {
            CFRelease(element);
            log(&format!(
                "{VALUE_CHANGED} on the focused element refused (AXError {rc}) — content \
                 depends on the app-level registration"
            ));
            return;
        }
        ctx.watched_value.set(element);
    }

    // Element identity as AX defines it.
    fn same_element(a: CFTypeRef, b: CFTypeRef) -> bool {
        if a.is_null() || b.is_null() {
            return a == b;
        }
        unsafe { CFEqual(a, b) != 0 }
    }

    unsafe fn on_value_changed(ctx: &mut Ctx, element: CFTypeRef) {
        // Debounce: (re)start the settle timer and remember the element. The actual
        // read + emit happens in the settle when edits go quiet.
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

        // A burst that never goes quiet (one long uninterrupted typing run) still
        // reports at the ceiling instead of being deferred by its own next keystroke.
        if burst_expired(started, now, VALUE_MAX_WAIT_MS) {
            flush_pending_value(ctx);
            return;
        }

        reschedule(ctx.debounce_timer, VALUE_DEBOUNCE_S);
    }

    // A window focus change is a discrete event: emitted immediately, as before. Its
    // `window_title` counts as a report of that title, so the AXTitleChanged that
    // usually follows an app switch does not report the same text twice.
    unsafe fn on_window_focused(ctx: &mut Ctx) {
        let app = match &ctx.attached {
            Some(a) => a.identity.clone(),
            None => return,
        };
        let title = focused_window_title(&app);

        let mut extra = Map::new();
        insert_str(&mut extra, "window_title", title.clone());
        ctx.emit("window.focused", Some(&app), extra);

        if let Some(attached) = ctx.attached.as_mut() {
            attached.title.seed_emitted(title);
        }

        // A window appearing is the moment a mid-launch app becomes a real one, so it
        // is when an unknown accessibility tree is worth another look.
        reprobe_if_unknown(ctx);
    }

    // A title change is a stream, not an event: record the latest title and let the
    // debounce emit the settled one. An unreadable title is recorded as empty, which
    // the emit renders as a frame without `window_title` — the shape this path always
    // produced when the read failed.
    unsafe fn on_title_changed(ctx: &mut Ctx) {
        let now = now_ms();
        let Some(attached) = ctx.attached.as_mut() else {
            return;
        };
        let title = focused_window_title(&attached.identity).unwrap_or_default();
        match attached.title.observe(title, now, TITLE_MAX_WAIT_MS) {
            SettleWhen::Immediately => flush_pending_title(ctx),
            SettleWhen::AfterDebounce => reschedule(ctx.title_timer, TITLE_DEBOUNCE_S),
        }
    }

    extern "C" fn title_settle(_timer: CFRunLoopTimerRef, info: *mut c_void) {
        let ctx = unsafe { &mut *(info as *mut Ctx) };
        unsafe { flush_pending_title(ctx) };
    }

    extern "C" fn value_settle(_timer: CFRunLoopTimerRef, info: *mut c_void) {
        let ctx = unsafe { &mut *(info as *mut Ctx) };
        unsafe { flush_pending_value(ctx) };
    }

    // Both debounces, emptied. Called before the frame that says coverage ended (no
    // event may follow it) and by detach.
    unsafe fn flush_pending(ctx: &mut Ctx) {
        flush_pending_value(ctx);
        flush_pending_title(ctx);
    }

    // Emit the settled title, if the debounce has one that differs from the last title
    // already reported for this app.
    unsafe fn flush_pending_title(ctx: &mut Ctx) {
        let Some(attached) = ctx.attached.as_mut() else {
            return;
        };
        let Some(title) = attached.title.settle() else {
            return;
        };
        let app = attached.identity.clone();
        emit_title_changed(ctx, &app, &title);
    }

    fn emit_title_changed(ctx: &Ctx, app: &AppIdentity, title: &str) {
        let mut extra = Map::new();
        if !title.is_empty() {
            extra.insert("window_title".into(), json!(title));
        }
        ctx.emit("window.title_changed", Some(app), extra);
    }

    // Read the settled value of the pending element and emit field.value. THE only way
    // a value leaves the debounce — the timer, the ceiling, a blur, and detach all come
    // through here — so an edit is never dropped by one path and kept by another. In a
    // browser the CONTENT is withheld (metadata only) until v1.1 correlates
    // tabs/privacy; an unreadable element emits nothing and is simply released.
    unsafe fn flush_pending_value(ctx: &mut Ctx) {
        let Some(pending) = ctx.pending_value.take() else {
            return;
        };

        let app = match &ctx.attached {
            Some(a) => a.identity.clone(),
            None => {
                CFRelease(pending.element);
                return;
            }
        };

        emit_field_value(ctx, &app, pending.element, pending.started_ms);
        CFRelease(pending.element);
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
            ctx.emit_gap(GapReason::SecureInput, started_ms, now_ms(), None);
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
                ctx.emit_gap(GapReason::PrivateUnknown, started_ms, now_ms(), None);
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

    // Build a run-loop timer. `interval_s` MUST be > 0 for anything that is later
    // rescheduled: CF invalidates a zero-interval timer as it fires, and an invalid
    // timer ignores `CFRunLoopTimerSetNextFireDate`. `first_delay_s == f64::MAX`
    // parks the timer in the distant future (idle until the first reschedule).
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
        // §8.2 activation: switch a Chromium/Electron app's accessibility tree on
        // (and off again on detach). Same ABI the `ax` module declares.
        fn AXUIElementSetAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: CFTypeRef,
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
        // Element identity: two AXUIElementRefs can name the same element.
        fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> u8;
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
        // Bounded run-loop turn + timer removal: the timer-contract test drives the
        // real run loop rather than asserting a comment about CF's behavior.
        #[cfg(test)]
        fn CFRunLoopRemoveTimer(rl: CFRunLoopRef, timer: CFRunLoopTimerRef, mode: CFStringRef);
        #[cfg(test)]
        fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: bool) -> i32;
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

    // --- engine tests (real CFRunLoop, no AX grant needed) -------------------

    #[cfg(test)]
    mod tests {
        use super::*;

        static FIRES: AtomicU64 = AtomicU64::new(0);

        extern "C" fn count_fire(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
            FIRES.fetch_add(1, Ordering::SeqCst);
            // Hand control back to the test, the way the engine stops its own loop.
            unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
        }

        // The contract both debounces rest on: a rescheduled timer fires EVERY time it
        // is re-armed. CF invalidates a zero-interval timer as it fires and then
        // silently ignores `CFRunLoopTimerSetNextFireDate`, so a one-shot debounce
        // would emit the first settled value of a session and nothing ever after —
        // exactly the shape of "field.value has never been observed live".
        #[test]
        fn a_rescheduled_debounce_timer_fires_more_than_once() {
            unsafe {
                let timer =
                    make_timer(f64::MAX, DEBOUNCE_REARM_S, count_fire, std::ptr::null_mut());
                assert!(!timer.is_null(), "CFRunLoopTimerCreate returned null");
                CFRunLoopAddTimer(CFRunLoopGetCurrent(), timer, kCFRunLoopDefaultMode);

                for expected in 1..=3 {
                    reschedule(timer, 0.01);
                    // Bounded: returns on the fire (the callout stops the loop), or on
                    // the timeout if the timer went invalid.
                    CFRunLoopRunInMode(kCFRunLoopDefaultMode, 2.0, false);
                    assert_eq!(
                        FIRES.load(Ordering::SeqCst),
                        expected,
                        "re-arm {expected} never fired: the timer is no longer valid"
                    );
                }

                CFRunLoopRemoveTimer(CFRunLoopGetCurrent(), timer, kCFRunLoopDefaultMode);
                CFRelease(timer as CFTypeRef);
            }
        }

        // --- engine paths, driven with a capturing emitter --------------------
        //
        // These exercise the REAL functions (detach, the gap announcements) against a
        // Ctx whose AX handles are null and whose pid has nothing in the activation
        // ledger, so no AX call is made: what is asserted is the frames that leave.

        // A pid range no live process uses, so the shared activation ledger and any AX
        // call this could reach stay untouched.
        const TEST_PID: i32 = 0x7f00_0101;

        fn test_identity() -> AppIdentity {
            AppIdentity {
                bundle_id: Some("com.microsoft.VSCode".into()),
                name: Some("Code".into()),
                pid: TEST_PID,
            }
        }

        fn attached_with(tree: AxTree, title: TitleDebounce) -> Attached {
            Attached {
                identity: test_identity(),
                app_element: std::ptr::null_mut(),
                observer: std::ptr::null_mut(),
                ax_tree: tree,
                title,
            }
        }

        fn test_ctx(seq: &AtomicU64, attached: Option<Attached>) -> Ctx<'_> {
            Ctx {
                config: ObserveConfig::default(),
                emitter: Emitter::capturing(),
                stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                boot_id: "boot-test".to_string(),
                seq,
                attached,
                front_pid: None,
                pending_value: None,
                watched_value: WatchSlot::default(),
                gap_ledger: Ledger::default(),
                log_ledger: Ledger::default(),
                attempts: AttemptLedger::default(),
                debounce_timer: std::ptr::null_mut(),
                title_timer: std::ptr::null_mut(),
                poll_ticks: 0,
            }
        }

        // An app switch inside the debounce window must not lose the app's last title:
        // detach settles it, on the way out, while the app is still attached.
        #[test]
        fn detach_flushes_the_pending_title() {
            let mut title = TitleDebounce::default();
            title.observe("capture.rs — fermix".into(), now_ms(), TITLE_MAX_WAIT_MS);

            let seq = AtomicU64::new(0);
            let mut ctx = test_ctx(&seq, Some(attached_with(AxTree::Enabled, title)));
            unsafe { detach(&mut ctx) };

            let frames = ctx.emitter.captured();
            assert_eq!(
                frames.len(),
                1,
                "expected exactly the flushed title: {frames:?}"
            );
            assert_eq!(frames[0]["kind"], json!("window.title_changed"));
            assert_eq!(frames[0]["window_title"], json!("capture.rs — fermix"));
            assert_eq!(frames[0]["app"]["bundle_id"], json!("com.microsoft.VSCode"));
            assert!(ctx.attached.is_none(), "detach must leave nothing attached");

            // A second detach has nothing to flush and nothing to tear down.
            unsafe { detach(&mut ctx) };
            assert_eq!(ctx.emitter.captured().len(), 1);
        }

        // A title-only app is announced once, with the app object, and never again on
        // the next app switch.
        #[test]
        fn a_title_only_gap_carries_the_app_and_is_announced_once() {
            let seq = AtomicU64::new(0);
            let mut ctx = test_ctx(
                &seq,
                Some(attached_with(AxTree::TitleOnly, TitleDebounce::default())),
            );

            announce_title_only(&mut ctx);
            announce_title_only(&mut ctx);

            let frames = ctx.emitter.captured();
            assert_eq!(frames.len(), 1, "announced more than once: {frames:?}");
            assert_eq!(frames[0]["kind"], json!("observer.gap"));
            assert_eq!(frames[0]["gap_reason"], json!("title_only"));
            assert_eq!(frames[0]["app"]["bundle_id"], json!("com.microsoft.VSCode"));
        }

        // An `Unknown` tree is the answer for an app that did not answer: it publishes
        // NOTHING, because a transient non-answer must never become a coverage claim.
        #[test]
        fn an_unknown_tree_announces_nothing() {
            let seq = AtomicU64::new(0);
            let mut ctx = test_ctx(
                &seq,
                Some(attached_with(AxTree::Unknown, TitleDebounce::default())),
            );

            announce_title_only(&mut ctx);
            assert!(ctx.emitter.captured().is_empty());
        }

        // A refusal is a coverage gap only once the bounded retries are spent: an app
        // that refuses while launching is transient. A wider refusal later is a new fact.
        #[test]
        fn a_refusal_is_announced_only_when_the_attach_attempts_are_spent() {
            let seq = AtomicU64::new(0);
            let mut ctx = test_ctx(&seq, None);
            let app = test_identity();
            let bundle = app.bundle_id.clone().unwrap();
            let value_only = RefusedSet {
                value: true,
                ..RefusedSet::default()
            };

            for _ in 1..ATTACH_ATTEMPTS {
                ctx.attempts
                    .take_attempt(&bundle, Attempt::Attach, ATTACH_ATTEMPTS);
                announce_refusal(&mut ctx, &app, value_only);
                assert!(
                    ctx.emitter.captured().is_empty(),
                    "announced a refusal that could still be transient"
                );
            }

            ctx.attempts
                .take_attempt(&bundle, Attempt::Attach, ATTACH_ATTEMPTS);
            announce_refusal(&mut ctx, &app, value_only);
            announce_refusal(&mut ctx, &app, value_only);

            let frames = ctx.emitter.captured();
            assert_eq!(frames.len(), 1, "announced more than once: {frames:?}");
            assert_eq!(frames[0]["gap_reason"], json!("ax_refused:AXValueChanged"));
            assert_eq!(frames[0]["app"]["pid"], json!(TEST_PID));

            // The refusal widening is a different fact and is announced on its own.
            announce_refusal(
                &mut ctx,
                &app,
                RefusedSet {
                    focused_ui_element: true,
                    value: true,
                },
            );
            let frames = ctx.emitter.captured();
            assert_eq!(frames.len(), 2);
            assert_eq!(
                frames[1]["gap_reason"],
                json!("ax_refused:AXValueChanged,AXFocusedUIElementChanged")
            );

            // Nothing was refused: nothing is announced.
            announce_refusal(&mut ctx, &app, RefusedSet::default());
            assert_eq!(ctx.emitter.captured().len(), 2);
        }

        // The grant going away ends coverage, so nothing may be emitted after that
        // frame: a pending title is flushed BEFORE it.
        #[test]
        fn a_pending_title_is_flushed_before_the_grant_revoked_gap() {
            let mut title = TitleDebounce::default();
            title.observe("fermix — fermix".into(), now_ms(), TITLE_MAX_WAIT_MS);

            let seq = AtomicU64::new(0);
            let mut ctx = test_ctx(&seq, Some(attached_with(AxTree::Enabled, title)));

            unsafe { flush_pending(&mut ctx) };
            let now = now_ms();
            ctx.emit_gap(GapReason::GrantRevoked, now, now, None);

            let frames = ctx.emitter.captured();
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[0]["kind"], json!("window.title_changed"));
            assert_eq!(frames[1]["gap_reason"], json!("grant_revoked"));
            assert!(frames[1].get("app").is_none());
        }
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
    fn system_gaps_have_no_app_object() {
        // The grant going away, a secure field, a browser's unknown privacy state: none
        // of these is a property of one app, so none carries an app object.
        for reason in [
            GapReason::SecureInput,
            GapReason::GrantRevoked,
            GapReason::PrivateUnknown,
        ] {
            let frame = gap_frame("boot-x", 1, reason, 100, 200, None);
            assert!(frame.get("app").is_none(), "{reason:?} must be app-less");
            assert_eq!(frame["kind"], json!("observer.gap"));
            assert_eq!(frame["gap_from_ts"], json!(100));
            assert_eq!(frame["gap_to_ts"], json!(200));
        }
    }

    #[test]
    fn per_app_coverage_gaps_carry_the_app_object() {
        // A coverage gap IS a fact about one app, so it rides the same `app` object as
        // every other event and needs no side channel to be attributed.
        let frame = gap_frame(
            "boot-x",
            2,
            GapReason::TitleOnly,
            100,
            100,
            Some(&identity()),
        );
        assert_eq!(frame["gap_reason"], json!("title_only"));
        assert_eq!(frame["app"]["bundle_id"], json!("com.apple.TextEdit"));
        assert_eq!(frame["app"]["pid"], json!(42));
        // `detail` left the wire: the reason string carries what was refused.
        assert!(frame.get("detail").is_none());

        let refused = RefusedSet {
            focused_ui_element: true,
            value: true,
        };
        let frame = gap_frame(
            "boot-x",
            3,
            GapReason::AxRefused(refused),
            100,
            100,
            Some(&identity()),
        );
        assert_eq!(
            frame["gap_reason"],
            json!("ax_refused:AXValueChanged,AXFocusedUIElementChanged")
        );
        assert_eq!(frame["app"]["bundle_id"], json!("com.apple.TextEdit"));
    }

    #[test]
    fn gap_reasons_match_the_taxonomy() {
        assert_eq!(GapReason::SecureInput.as_str(), "secure_input");
        assert_eq!(GapReason::GrantRevoked.as_str(), "grant_revoked");
        assert_eq!(GapReason::PrivateUnknown.as_str(), "private_unknown");
        assert_eq!(GapReason::TitleOnly.as_str(), "title_only");

        // Which notifications were refused is part of the reason, so a reader can tell
        // a narrow refusal from a total one.
        let value_only = RefusedSet {
            value: true,
            ..RefusedSet::default()
        };
        let focus_only = RefusedSet {
            focused_ui_element: true,
            ..RefusedSet::default()
        };
        let both = RefusedSet {
            focused_ui_element: true,
            value: true,
        };
        assert_eq!(
            GapReason::AxRefused(value_only).as_str(),
            "ax_refused:AXValueChanged"
        );
        assert_eq!(
            GapReason::AxRefused(focus_only).as_str(),
            "ax_refused:AXFocusedUIElementChanged"
        );
        assert_eq!(
            GapReason::AxRefused(both).as_str(),
            "ax_refused:AXValueChanged,AXFocusedUIElementChanged"
        );
        assert!(RefusedSet::default().is_empty());
        assert!(!value_only.is_empty());
    }

    #[test]
    fn a_named_coverage_gap_is_announced_once_per_app_and_reason() {
        let mut ledger: Ledger<(String, GapReason)> = Ledger::default();
        let vscode = "com.microsoft.VSCode".to_string();
        let value_only = RefusedSet {
            value: true,
            ..RefusedSet::default()
        };
        let both = RefusedSet {
            focused_ui_element: true,
            value: true,
        };

        assert!(ledger.announce((vscode.clone(), GapReason::TitleOnly)));
        // Re-attaching the same app on every app switch must not re-announce.
        assert!(!ledger.announce((vscode.clone(), GapReason::TitleOnly)));
        // A different reason for the same app is its own announcement …
        assert!(ledger.announce((vscode.clone(), GapReason::AxRefused(value_only))));
        assert!(!ledger.announce((vscode.clone(), GapReason::AxRefused(value_only))));
        // … and a WIDER refusal later is a new fact, not a repeat of the narrow one.
        assert!(ledger.announce((vscode, GapReason::AxRefused(both))));
        // The same reason for a different app is also its own announcement.
        let claude = "com.anthropic.claudefordesktop".to_string();
        assert!(ledger.announce((claude.clone(), GapReason::TitleOnly)));
        assert!(!ledger.announce((claude, GapReason::TitleOnly)));
    }

    #[test]
    fn ax_tree_classification_covers_every_probe_answer() {
        use AxAnswer::{NoValue, Ok, Unanswered, Unsupported};

        // The switch took: a Chromium/Electron tree is on, whatever a later read says.
        for read in [None, Some(Ok), Some(Unsupported), Some(Unanswered)] {
            assert_eq!(classify_ax_tree(Ok, read), AxTree::Enabled, "read {read:?}");
        }

        // No such switch, but the app hands out a focused element (or supports the
        // attribute with nothing focused): a native tree, which is coverage, not a gap.
        assert_eq!(classify_ax_tree(Unsupported, Some(Ok)), AxTree::Enabled);
        assert_eq!(
            classify_ax_tree(Unsupported, Some(NoValue)),
            AxTree::Enabled
        );

        // No switch AND no focused-element attribute: the definitive title-only app —
        // the only cell that publishes a gap.
        assert_eq!(
            classify_ax_tree(Unsupported, Some(Unsupported)),
            AxTree::TitleOnly
        );

        // Anything unanswered decides NOTHING and must be re-probed, never published.
        assert_eq!(
            classify_ax_tree(Unsupported, Some(Unanswered)),
            AxTree::Unknown
        );
        assert_eq!(classify_ax_tree(Unsupported, None), AxTree::Unknown);
        for read in [None, Some(Ok), Some(NoValue), Some(Unsupported)] {
            assert_eq!(
                classify_ax_tree(Unanswered, read),
                AxTree::Unknown,
                "read {read:?}"
            );
            assert_eq!(
                classify_ax_tree(NoValue, read),
                AxTree::Unknown,
                "a set cannot answer NoValue; read {read:?}"
            );
        }
    }

    #[test]
    fn an_activation_is_recorded_only_when_we_switched_it_on() {
        // An app whose owner already had the switch on (a screen reader is running) must
        // be left exactly as it was, so nothing is recorded and detach clears nothing.
        assert!(!should_record_activation(Some(true)));
        // Off, or no such attribute to read: whatever a successful set changed is ours.
        assert!(should_record_activation(Some(false)));
        assert!(should_record_activation(None));
    }

    #[test]
    fn attempt_ledger_caps_retries_per_app() {
        let mut attempts = AttemptLedger::default();
        let vscode = "com.microsoft.VSCode";

        assert!(!attempts.exhausted(vscode, Attempt::Attach, 3));
        for spent in 1..=3 {
            assert!(
                attempts.take_attempt(vscode, Attempt::Attach, 3),
                "attempt {spent} must be allowed"
            );
        }
        assert!(!attempts.take_attempt(vscode, Attempt::Attach, 3));
        assert!(attempts.exhausted(vscode, Attempt::Attach, 3));

        // The probe budget is separate from the attach budget …
        assert!(attempts.take_attempt(vscode, Attempt::Probe, 3));
        // … and so is another app's.
        assert!(attempts.take_attempt("com.apple.Notes", Attempt::Attach, 3));
    }

    #[test]
    fn title_debounce_emits_one_settled_title_per_burst() {
        let mut debounce = TitleDebounce::default();
        // A spinner burst: one braille glyph apart, several times a second.
        let start = 1_000_000;
        for (tick, title) in [
            "⠙ fermix — fermix",
            "⠹ fermix — fermix",
            "⠸ fermix — fermix",
        ]
        .iter()
        .enumerate()
        {
            let now = start + tick as i64 * 200;
            assert_eq!(
                debounce.observe((*title).into(), now, TITLE_MAX_WAIT_MS_TEST),
                SettleWhen::AfterDebounce
            );
        }
        assert_eq!(debounce.settle().as_deref(), Some("⠸ fermix — fermix"));
        // The timer firing again with nothing pending emits nothing.
        assert_eq!(debounce.settle(), None);
    }

    #[test]
    fn title_debounce_drops_an_identical_settled_title() {
        let mut debounce = TitleDebounce::default();
        debounce.observe("fermix — fermix".into(), 10, TITLE_MAX_WAIT_MS_TEST);
        assert_eq!(debounce.settle().as_deref(), Some("fermix — fermix"));
        // A burst that settles back to the title already emitted is not an event.
        debounce.observe("⠙ fermix — fermix".into(), 20, TITLE_MAX_WAIT_MS_TEST);
        debounce.observe("fermix — fermix".into(), 30, TITLE_MAX_WAIT_MS_TEST);
        assert_eq!(debounce.settle(), None);
        // A genuinely new title still emits.
        debounce.observe("capture.rs — fermix".into(), 40, TITLE_MAX_WAIT_MS_TEST);
        assert_eq!(debounce.settle().as_deref(), Some("capture.rs — fermix"));
    }

    #[test]
    fn title_debounce_forces_a_settle_when_the_burst_outruns_the_ceiling() {
        let mut debounce = TitleDebounce::default();
        let start = 5_000_000;
        // A window that re-titles without pause would defer its settle forever.
        assert_eq!(
            debounce.observe("⠙ build".into(), start, TITLE_MAX_WAIT_MS_TEST),
            SettleWhen::AfterDebounce
        );
        assert_eq!(
            debounce.observe("⠹ build".into(), start + 4_999, TITLE_MAX_WAIT_MS_TEST),
            SettleWhen::AfterDebounce
        );
        assert_eq!(
            debounce.observe("⠸ build".into(), start + 5_000, TITLE_MAX_WAIT_MS_TEST),
            SettleWhen::Immediately,
            "the burst has outrun the ceiling and must report now"
        );
        assert_eq!(debounce.settle().as_deref(), Some("⠸ build"));

        // The next title starts a fresh burst, measured from its own first sighting.
        assert_eq!(
            debounce.observe("done".into(), start + 5_100, TITLE_MAX_WAIT_MS_TEST),
            SettleWhen::AfterDebounce
        );
    }

    #[test]
    fn title_debounce_suppresses_a_title_already_reported_by_window_focused() {
        let mut debounce = TitleDebounce::default();
        // window.focused carried this title, so the AXTitleChanged that follows an app
        // switch must not report the same text a second time.
        debounce.seed_emitted(Some("fermix — fermix".into()));
        debounce.observe("fermix — fermix".into(), 10, TITLE_MAX_WAIT_MS_TEST);
        assert_eq!(debounce.settle(), None);

        // An unreadable title reports nothing, so it seeds nothing.
        debounce.seed_emitted(None);
        debounce.observe("capture.rs — fermix".into(), 20, TITLE_MAX_WAIT_MS_TEST);
        assert_eq!(debounce.settle().as_deref(), Some("capture.rs — fermix"));
    }

    #[test]
    fn a_burst_expires_exactly_at_the_ceiling() {
        // The value debounce measures its own burst with this, so a long uninterrupted
        // typing run still reports at the ceiling.
        assert!(!burst_expired(1_000, 1_000, 5_000));
        assert!(!burst_expired(1_000, 5_999, 5_000));
        assert!(burst_expired(1_000, 6_000, 5_000));
        // A clock that goes backwards must not read as an expired burst.
        assert!(!burst_expired(9_000, 1_000, 5_000));
    }

    // The debounce ceiling the engine uses, restated so a pure test does not reach into
    // the macOS-only module for it.
    const TITLE_MAX_WAIT_MS_TEST: i64 = 5_000;

    #[test]
    fn watch_slot_pairs_a_registration_with_exactly_one_unregister() {
        // Stand-ins for the retained AX element refs the engine registers on: `a` and
        // `a_again` are two refs naming the SAME element, which is what CFEqual answers
        // for AX and what a pointer comparison would get wrong.
        let (field_a, field_a_again, field_b) = (0xa1_u64, 0x1a1_u64, 0xb2_u64);
        let same = |x: u64, y: u64| x % 0x100 == y % 0x100;

        let mut slot: WatchSlot<u64> = WatchSlot::default();
        assert_eq!(slot.take(), None);

        slot.set(field_a);
        assert!(slot.is_current(field_a, same));
        // A re-notification for the same focused element must not re-register …
        assert!(slot.is_current(field_a_again, same));
        // … while a different element must.
        assert!(!slot.is_current(field_b, same));

        // Focus moves: the previous element is handed back exactly once, to be
        // unregistered and released.
        assert_eq!(slot.take(), Some(field_a));
        assert_eq!(slot.take(), None);

        // Detach with a live registration hands it back; a second detach does not.
        slot.set(field_b);
        assert_eq!(slot.take(), Some(field_b));
        assert_eq!(slot.take(), None);
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
