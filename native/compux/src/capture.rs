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
/// `ax_refused:…`, `private_unknown`) carries the app object so the decoder attributes
/// it through the same `bundle_id` path every other event uses; a SYSTEM gap
/// (`grant_revoked`, `secure_input`) is app-less because it is not a property of one
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
    /// A browser family with NO pinned private-window marker, so the posture of its
    /// windows can never be read and its typed text is always withheld (inv. 26). A
    /// standing property of ONE app, so it carries the app and is announced once per app
    /// per session, like `title_only`. A PINNED family never owes this gap: a title its
    /// window did not answer withholds that one value and says nothing about the app.
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

/// Number of CHARACTERS in a value — the volume a `field.value` reports even when its
/// content is withheld or clipped, so a gap is never a silent loss.
pub fn char_len(s: &str) -> i64 {
    s.chars().count() as i64
}

/// Insert a string field only when there is one: an absent value is an ABSENT key on
/// the wire, never a `null` the decoder would have to special-case.
pub fn insert_str(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(v) = value {
        map.insert(key.to_string(), json!(v));
    }
}

/// The bundle-id set treated as a web browser. A browser's content events are
/// site-correlated (`browser_id`/`window_ref`/`tab_ref`/`host`) and gated on the
/// window's private-browsing posture: typed text leaves the sidecar only for a window
/// KNOWN not to be private, and a private window's URL never leaves it at all
/// (inv. 26, §13.2 — withheld before it crosses the Port, never sent for Fermix to
/// filter later).
pub fn is_browser(bundle_id: &str) -> bool {
    matches!(
        bundle_id,
        "com.apple.Safari"
            | "com.apple.SafariTechnologyPreview"
            | "com.google.Chrome"
            | "com.google.Chrome.canary"
            | "com.google.Chrome.beta"
            | "com.google.Chrome.dev"
            | "org.chromium.Chromium"
            | "com.microsoft.edgemac"
            | "com.microsoft.edgemac.Beta"
            | "com.microsoft.edgemac.Dev"
            | "com.microsoft.edgemac.Canary"
            | "com.brave.Browser"
            | "org.mozilla.firefox"
            | "org.mozilla.nightly"
            | "org.mozilla.firefoxdeveloperedition"
            | "com.operasoftware.Opera"
            | "company.thebrowser.Browser"
            | "com.vivaldi.Vivaldi"
            | "com.arc.Arc"
    )
}

// --- browser privacy, URLs and navigation (portable, unit-tested) ------------

/// A browser window's private-browsing posture — the ONLY gate left on browser data,
/// so it is a four-valued answer rather than a boolean guess (§2.2 of the v1.1
/// design). Three of the four withhold typed text; what separates the last two is
/// whether the sidecar learned anything worth SAYING about the app:
///
/// * `Unknown` — this browser family has no pinned marker, so its posture can never be
///   read. A standing property of the app: it owes one `private_unknown` gap so
///   `/history status` can name it.
/// * `Unreadable` — a pinned family whose window title could not be read on THIS
///   value. One failed read is not a standing condition, so it owes no gap; and since
///   the absence of a marker is only evidence when the title was readable, its URL is
///   withheld too (an incognito window whose marker we merely failed to see must not
///   report where it went).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateState {
    Private,
    NotPrivate,
    Unknown,
    Unreadable,
}

impl PrivateState {
    /// The wire spelling (the store's `private_state` column, which has three values —
    /// an unreadable read is reported as `unknown`).
    pub fn as_str(self) -> &'static str {
        match self {
            PrivateState::Private => "private",
            PrivateState::NotPrivate => "not_private",
            PrivateState::Unknown | PrivateState::Unreadable => "unknown",
        }
    }
}

/// The AX window-title substring that marks a private/incognito window, per browser
/// family — the single source for the classification below.
///
/// Only what `LIVE_CHECK.md` actually exercises is pinned. The Chromium family shares
/// Chrome's code and its `Incognito` window-title string, and stable Chrome is verified
/// in LIVE_CHECK §5b, so those channels are pinned together. Every other browser is
/// deliberately UNPINNED and answers `Unknown` — its URLs still flow, its typed text
/// does not, and the gap names it — because a marker that has not been checked against
/// a live window of that browser must never be pinned: a wrong `not_private` leaks
/// typed text out of a private window (inv. 26), while an `unknown` only loses capture
/// and says so.
///
/// Candidate markers, recorded but NOT pinned (no live check covers them; LIVE_CHECK
/// "Pinning another browser family" is the procedure): Edge `InPrivate`, Firefox
/// `Private Browsing`, Safari — reported to expose a `Private Browsing` toolbar element
/// rather than a title marker at all (§2.2).
const PRIVATE_WINDOW_MARKERS: &[(&str, &str)] = &[
    ("com.google.Chrome", "Incognito"),
    ("com.google.Chrome.canary", "Incognito"),
    ("com.google.Chrome.beta", "Incognito"),
    ("com.google.Chrome.dev", "Incognito"),
    ("org.chromium.Chromium", "Incognito"),
];

/// Classify one browser window from its AX title. Fail-closed at every step: a
/// non-browser has no posture to report, an unpinned family stays `Unknown`, and a
/// pinned family with no readable title is `Unreadable` rather than `NotPrivate` by
/// absence of evidence. Marker matching is a case-sensitive substring test — the
/// markers are fixed UI strings.
pub fn private_state(bundle_id: &str, window_title: &str) -> PrivateState {
    if !is_browser(bundle_id) {
        return PrivateState::Unknown;
    }
    let Some((_, marker)) = PRIVATE_WINDOW_MARKERS
        .iter()
        .find(|(family, _)| *family == bundle_id)
    else {
        return PrivateState::Unknown;
    };
    if window_title.is_empty() {
        return PrivateState::Unreadable;
    }
    if window_title.contains(marker) {
        return PrivateState::Private;
    }
    PrivateState::NotPrivate
}

/// Reduce a browser URL to what the store is allowed to keep: scheme + host + path.
/// The query string, the fragment, the userinfo and the port are dropped HERE, in the
/// sidecar (inv. 27, §13.2 — never send what the store must not keep), because a query
/// string is where session ids, tokens and one-time codes live.
///
/// `None` means there is no navigation to report: a non-http(s) scheme (`file:` paths
/// are local secrets; `about:`/`chrome:`/`data:` pages are nothing to recall), a URL
/// with no host, or a normalized URL past [`MAX_URL_BYTES`]. Returns `(url, host)`; the
/// host is lowercased (an IDN host passes through as-is) and the path is kept verbatim.
pub fn normalize_url(raw: &str) -> Option<(String, String)> {
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let host = host_of(authority)?;
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let url = format!("{scheme}://{host}{}", &tail[..path_end]);
    if url.len() > MAX_URL_BYTES {
        // Dropped, never truncated: a cut URL names a page that does not exist and the
        // store cannot tell it was cut.
        return None;
    }
    Some((url, host))
}

/// Cap on a normalized URL. Past this the navigation is DROPPED — the length is already
/// pathological (every real page is far under it) and a clipped URL is worse than none.
pub const MAX_URL_BYTES: usize = 2048;

/// The host of an authority: userinfo dropped (everything up to the last `@`), port
/// dropped, lowercased. An IPv6 literal keeps its brackets — the colons inside them
/// are not a port separator.
fn host_of(authority: &str) -> Option<String> {
    let after_userinfo = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let host = match after_userinfo.rfind(']') {
        Some(end) => &after_userinfo[..=end],
        None => after_userinfo.split(':').next().unwrap_or(""),
    };
    if host.is_empty() {
        return None;
    }
    Some(host.to_lowercase())
}

/// What a bounded web-area walk found, and what the caller must release.
pub struct WebAreaWalk<T> {
    /// The page web area, when the walk reached one. The caller owns it.
    pub found: Option<T>,
    /// Every OTHER element the walk was handed, root included: the caller releases each
    /// one. Accounting for them here is what makes the walk leak-free by construction
    /// rather than by inspection of an early return.
    pub discarded: Vec<T>,
}

/// Breadth-first search for a browser window's PAGE web area, factored over the two
/// reads it makes so its bounds are testable against a synthetic tree.
///
/// `children` and `is_page` are the AX reads (`AXChildren`, and `AXRole` + `AXURL`
/// reduced by [`normalize_url`]); the walk owns only the ORDER and the BOUNDS:
///
/// * `max_nodes` bounds the elements TESTED — checked before each read, not merely
///   before expanding, because a wide tree would otherwise turn a 400-node budget into
///   one read per enqueued element (400 expansions × 400 children ≈ 160k IPC calls);
/// * expansion stops once that many elements have been enqueued, so at most `max_nodes`
///   plus one node's children are ever held at once;
/// * `max_depth` bounds how deep it descends.
///
/// Breadth-first because the content area sits shallow under the window, so a deep
/// toolbar subtree must never be descended before it.
pub fn find_page_web_area<T: Copy>(
    root: T,
    max_depth: usize,
    max_nodes: usize,
    children: &mut impl FnMut(T) -> Vec<T>,
    is_page: &mut impl FnMut(T) -> bool,
) -> WebAreaWalk<T> {
    let mut queue: std::collections::VecDeque<(T, usize)> = std::collections::VecDeque::new();
    queue.push_back((root, 0));
    let mut queued = 1usize;
    let mut discarded: Vec<T> = Vec::new();
    let mut visited = 0usize;

    while let Some((node, depth)) = queue.pop_front() {
        // The cap is spent: this element is never READ, only released.
        if visited >= max_nodes {
            discarded.push(node);
            discarded.extend(queue.into_iter().map(|(node, _)| node));
            return WebAreaWalk {
                found: None,
                discarded,
            };
        }
        visited += 1;
        if is_page(node) {
            discarded.extend(queue.into_iter().map(|(node, _)| node));
            return WebAreaWalk {
                found: Some(node),
                discarded,
            };
        }
        if depth < max_depth && queued < max_nodes {
            let kids = children(node);
            queued += kids.len();
            queue.extend(kids.into_iter().map(|kid| (kid, depth + 1)));
        }
        discarded.push(node);
    }

    WebAreaWalk {
        found: None,
        discarded,
    }
}

/// One settled browser navigation, as read off the AX tree. Plain data, so both the
/// decision it feeds and the frame it becomes are pure and testable without AX.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Navigation {
    pub window_ref: String,
    /// The AX window title as read — the text the classifier judged, on the wire so
    /// the owner can pin a family's marker from the store itself.
    pub window_title: String,
    pub tab_ref: String,
    /// Already stripped by [`normalize_url`].
    pub url: String,
    pub host: String,
    pub page_title: Option<String>,
    pub private_state: PrivateState,
}

/// Whether a freshly read `(tab_ref, url)` pair is a navigation worth a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavigationDecision {
    Emit,
    Skip,
}

/// A navigation is a CHANGE of the `(tab, url)` pair last recorded for this window: a
/// spinner retitling the same page is not one, and refocusing a window whose page has
/// not changed is not one either. A private window never navigates as far as the wire
/// is concerned — its URL must not cross the Port (inv. 26) — and neither does an
/// `Unreadable` one, which may be a private window whose marker we merely failed to
/// see. An `Unknown` (unpinned) browser DOES report where it went: consent is per
/// browser, so only typed text waits for a positive signal.
pub fn navigation_decision(
    last: Option<(&str, &str)>,
    tab_ref: &str,
    url: &str,
    state: PrivateState,
) -> NavigationDecision {
    if matches!(state, PrivateState::Private | PrivateState::Unreadable) {
        return NavigationDecision::Skip;
    }
    if last == Some((tab_ref, url)) {
        return NavigationDecision::Skip;
    }
    NavigationDecision::Emit
}

/// The `browser.navigated` extras (§8.4). `browser_id` is the bundle id; everything
/// else comes off the read.
pub fn navigation_extras(browser_id: &str, nav: &Navigation) -> Map<String, Value> {
    let mut extra = Map::new();
    extra.insert("browser_id".into(), json!(browser_id));
    extra.insert("url".into(), json!(nav.url));
    extra.insert("host".into(), json!(nav.host));
    extra.insert("window_ref".into(), json!(nav.window_ref));
    extra.insert("tab_ref".into(), json!(nav.tab_ref));
    extra.insert("private_state".into(), json!(nav.private_state.as_str()));
    insert_str(&mut extra, "page_title", nav.page_title.clone());
    if !nav.window_title.is_empty() {
        extra.insert("window_title".into(), json!(nav.window_title));
    }
    extra
}

/// The browser context a `field.value` frame is stamped with (§8.4): which window it
/// was typed in, and the site the last navigation bound that window to.
pub struct BrowserFieldContext<'a> {
    pub browser_id: &'a str,
    pub window_ref: Option<&'a str>,
    pub tab_ref: Option<&'a str>,
    pub host: Option<&'a str>,
}

/// What the caller still owes after a browser `field.value` frame is built.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BrowserFieldOwed {
    /// The text was clipped at [`MAX_TEXT_BYTES`]: a `gap{truncated}` is owed.
    pub truncated: bool,
    /// The window's posture is unreadable: a `gap{private_unknown}` is owed, once per
    /// app per session.
    pub private_unknown: bool,
}

/// Build the browser half of a `field.value` frame, with the private gate deciding what
/// may leave (inv. 26). `not_private` sends the text (truncated like any other field)
/// plus the full site context; every other posture sends the volume and the refs and
/// withholds everything else. Only `Unknown` — a browser family with no pinned marker —
/// owes the standing `private_unknown` gap: a private window is working as designed, and
/// a single `Unreadable` read is not a property of the app.
pub fn browser_field_extras(
    text: &str,
    state: PrivateState,
    context: &BrowserFieldContext,
    extra: &mut Map<String, Value>,
) -> BrowserFieldOwed {
    extra.insert("char_len".into(), json!(char_len(text)));
    extra.insert("private_state".into(), json!(state.as_str()));
    extra.insert("browser_id".into(), json!(context.browser_id));
    insert_str(extra, "window_ref", context.window_ref.map(str::to_string));

    if state != PrivateState::NotPrivate {
        extra.insert("content_withheld".into(), json!(true));
        return BrowserFieldOwed {
            truncated: false,
            private_unknown: state == PrivateState::Unknown,
        };
    }

    let (sent, truncated) = truncate_text(text);
    extra.insert("text".into(), json!(sent));
    extra.insert("content_withheld".into(), json!(false));
    insert_str(extra, "tab_ref", context.tab_ref.map(str::to_string));
    insert_str(extra, "host", context.host.map(str::to_string));
    BrowserFieldOwed {
        truncated,
        private_unknown: false,
    }
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

/// The resolved app allowlist an `observe_start` carries. There is no site allowlist:
/// consent is per BROWSER (allowlisting Chrome means "record where I go in Chrome"), so
/// the retired `sites` key is simply ignored when an older consumer still sends it.
#[derive(Clone, Debug, Default)]
pub struct ObserveConfig {
    pub apps: Vec<String>,
}

impl ObserveConfig {
    /// Parse `{"action":"observe_start","params":{"apps":[…]}}`. Missing or malformed
    /// params default to an empty allowlist (default-deny; Ingest re-enforces at the
    /// write boundary, so a permissive parse can never leak).
    pub fn from_request(req: &Value) -> ObserveConfig {
        ObserveConfig {
            apps: string_list(req.get("params"), "apps"),
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
    // Every AX read this observer makes is bounded (inv. 28): an app that stops
    // answering must not wedge the observer thread. Set once per attach on the app
    // element, which scopes it to every element created from it. A timed-out read
    // simply answers `None` and no frame is built — the named `observer.gap{ax_timeout}`
    // is the later v1.1 slice.
    const AX_MESSAGING_TIMEOUT_S: f32 = 2.0;
    // The web-area search under one browser window (see `find_page_web_area`):
    // breadth-first, so the shallow content area is found without descending a deep
    // toolbar subtree first. `MAX_NODES` bounds the elements actually READ — one AX
    // round-trip each — and, through the same counter, how many are ever retained at
    // once; `MAX_DEPTH` bounds the descent. So a browser that exposes no page web area
    // costs at most 400 reads, not a stall and not a tree pulled into memory.
    const WEB_AREA_MAX_DEPTH: usize = 12;
    const WEB_AREA_MAX_NODES: usize = 400;
    // Per-app browser windows tracked at once. The map is keyed by `window_ref` and
    // evicts the oldest entry, so a long session with many windows cannot grow without
    // limit; an evicted window simply re-reads its navigation on the next settle.
    const BROWSER_WINDOWS_MAX: usize = 32;

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
        // Populated only for a browser: the per-window navigation context every
        // browser content event is correlated through (§8.4). Bounded and released on
        // detach.
        browser_windows: Vec<BrowserWindow>,
    }

    // One browser window's correlation state. `web_area` is a RETAINED cache of the
    // element the URL is read from, validated on every hit against the window title
    // (see `web_area_for`).
    struct BrowserWindow {
        window_ref: String,
        web_area: CFTypeRef,
        // The `(tab_ref, url)` pair of the last navigation EMITTED for this window,
        // plus its host — what a `field.value` typed in this window is stamped with.
        tab_ref: Option<String>,
        last_url: Option<String>,
        host: Option<String>,
    }

    impl BrowserWindow {
        fn new(window_ref: &str) -> BrowserWindow {
            BrowserWindow {
                window_ref: window_ref.to_string(),
                web_area: std::ptr::null_mut(),
                tab_ref: None,
                last_url: None,
                host: None,
            }
        }
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

        // `app` is Some only for a gap that is a property of ONE app (its coverage —
        // title-only, refused notifications, an unreadable privacy posture); a system
        // gap — the grant went away, a secure field — is app-less.
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

        // Bound every read this attach will make, before any of them is made.
        let rc = AXUIElementSetMessagingTimeout(app_element, AX_MESSAGING_TIMEOUT_S);
        if rc != 0 {
            log_once(
                ctx,
                &app,
                "messaging_timeout_refused",
                &format!("{bundle}: AXUIElementSetMessagingTimeout refused (AXError {rc})"),
            );
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
            browser_windows: Vec::new(),
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

        // Every cached web area is a +1 this engine owns, so an app switch releases
        // them all — the browser context does not outlive the attach it describes.
        for window in attached.browser_windows {
            release_web_area(window.web_area);
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
        insert_str(&mut extra, "window_title", focused_window_title(ctx));
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
        let title = focused_window_title(ctx);

        let mut extra = Map::new();
        insert_str(&mut extra, "window_title", title.clone());
        ctx.emit("window.focused", Some(&app), extra);

        if let Some(attached) = ctx.attached.as_mut() {
            attached.title.seed_emitted(title);
        }

        // A window appearing is the moment a mid-launch app becomes a real one, so it
        // is when an unknown accessibility tree is worth another look.
        reprobe_if_unknown(ctx);

        // The newly focused window is the page the owner is on now. Checked AFTER the
        // re-probe: switching a browser's accessibility tree on is what makes its web
        // area — and therefore its URL — readable at all.
        //
        // This copies `AXFocusedWindow` a second time (the title read above was the
        // first), and deliberately: the navigation check runs only for an attached browser
        // whose tree is on, while this event fires for every app, so threading one
        // retained window through the emit / seed / re-probe sequence would put a
        // release-on-every-path obligation on every app's path to save one read on a
        // browser's.
        maybe_emit_navigation(ctx);
    }

    // A title change is a stream, not an event: record the latest title and let the
    // debounce emit the settled one. An unreadable title is recorded as empty, which
    // the emit renders as a frame without `window_title` — the shape this path always
    // produced when the read failed.
    unsafe fn on_title_changed(ctx: &mut Ctx) {
        let now = now_ms();
        if ctx.attached.is_none() {
            return;
        }
        let title = focused_window_title(ctx).unwrap_or_default();
        let Some(attached) = ctx.attached.as_mut() else {
            return;
        };
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
        // A settled title is the moment a page has finished becoming a different page,
        // so it is the navigation check for a browser (a no-op for anything else).
        maybe_emit_navigation(ctx);
    }

    fn emit_title_changed(ctx: &Ctx, app: &AppIdentity, title: &str) {
        let mut extra = Map::new();
        if !title.is_empty() {
            extra.insert("window_title".into(), json!(title));
        }
        ctx.emit("window.title_changed", Some(app), extra);
    }

    // --- browser navigation (§8.4 `browser.navigated`) -----------------------

    // One navigation check for an attached browser: read the focused window, classify
    // its privacy from the title, and — only when it is not private — read the web
    // area's URL and emit `browser.navigated` if the `(tab, url)` pair changed. A
    // no-op for a non-browser. Bounded: one window read and at most one web-area walk
    // per call, and the title debounce bounds how often it is called.
    unsafe fn maybe_emit_navigation(ctx: &mut Ctx) {
        let Some(app) = attached_browser(ctx) else {
            return;
        };
        let Some(bundle) = app.bundle_id.clone() else {
            return;
        };
        let Some(window) = focused_window(ctx) else {
            return;
        };
        let read = read_navigation(ctx, &bundle, window);
        CFRelease(window);

        let Some(nav) = read else {
            return;
        };
        emit_navigation(ctx, &app, &bundle, nav);
    }

    // The attached app's identity, when it is a browser whose accessibility tree is ON.
    // The tree is part of the gate, not an optimization: a `TitleOnly` app exposes no web
    // area to find, and an `Unknown` one is still being re-probed (window focus is when it
    // is looked at again). Walking either would spend up to WEB_AREA_MAX_NODES AX reads
    // per settled title on a guaranteed nothing.
    fn attached_browser(ctx: &Ctx) -> Option<AppIdentity> {
        let attached = ctx.attached.as_ref()?;
        if attached.ax_tree != AxTree::Enabled {
            return None;
        }
        let app = attached.identity.clone();
        if !is_browser(app.bundle_id.as_deref()?) {
            return None;
        }
        Some(app)
    }

    // Everything one frame needs, read off the focused window. `None` when there is
    // nothing to report: a window whose URL must not cross the Port (private, or a pinned
    // family whose title could not be read — whose URL is never even looked for), no web
    // area, an unreadable URL, or a scheme that is not http(s).
    unsafe fn read_navigation(
        ctx: &mut Ctx,
        bundle: &str,
        window: CFTypeRef,
    ) -> Option<Navigation> {
        let window_title = read_attr(window, "AXTitle").unwrap_or_default();
        let window_ref = hash_ref(window)?;
        let state = private_state(bundle, &window_title);
        // Inv. 26 at the earliest point it can be enforced: `navigation_decision` skips
        // both of these too, but stopping here also saves the web-area walk.
        if matches!(state, PrivateState::Private | PrivateState::Unreadable) {
            return None;
        }

        let web_area = web_area_for(ctx, &window_ref, window, &window_title)?;
        let Some((url, host)) = page_url_of(web_area.element) else {
            // The element no longer answers, or it is no longer on a page: drop it so the
            // next check walks again. One walk per call stays one walk per call.
            forget_web_area(ctx, &window_ref);
            return None;
        };

        Some(Navigation {
            window_ref,
            window_title,
            tab_ref: web_area.tab_ref,
            url,
            host,
            page_title: web_area.page_title,
            private_state: state,
        })
    }

    // Emit the frame when this is a change, and record what was emitted so the next
    // check can tell. Recording happens only on an emit, so the stored host always
    // names a site a frame already reported.
    unsafe fn emit_navigation(ctx: &mut Ctx, app: &AppIdentity, bundle: &str, nav: Navigation) {
        let last = recorded_navigation(ctx, &nav.window_ref);
        let decision = navigation_decision(
            last.as_ref().map(|(tab, url)| (tab.as_str(), url.as_str())),
            &nav.tab_ref,
            &nav.url,
            nav.private_state,
        );
        if decision == NavigationDecision::Skip {
            return;
        }
        record_navigation(ctx, &nav);
        ctx.emit(
            "browser.navigated",
            Some(app),
            navigation_extras(bundle, &nav),
        );
    }

    // --- per-window browser context -----------------------------------------

    // This window's entry, created on first sight. Bounded at BROWSER_WINDOWS_MAX, and
    // LEAST-RECENTLY-TOUCHED first out: the map is ordered by use (a touch moves the entry
    // to the back), so the entry that goes is the window the owner stopped using. Evicting
    // a live window instead would drop its `last_url`, and the next check would re-report
    // the page it is already on as a fresh navigation.
    unsafe fn browser_entry<'c>(
        ctx: &'c mut Ctx,
        window_ref: &str,
    ) -> Option<&'c mut BrowserWindow> {
        let windows = &mut ctx.attached.as_mut()?.browser_windows;
        if let Some(index) = windows.iter().position(|w| w.window_ref == window_ref) {
            let touched = windows.remove(index);
            windows.push(touched);
            return windows.last_mut();
        }
        if windows.len() >= BROWSER_WINDOWS_MAX {
            let evicted = windows.remove(0);
            release_web_area(evicted.web_area);
        }
        windows.push(BrowserWindow::new(window_ref));
        windows.last_mut()
    }

    // The `(tab_ref, url)` pair of the last navigation emitted for this window.
    unsafe fn recorded_navigation(ctx: &mut Ctx, window_ref: &str) -> Option<(String, String)> {
        let entry = browser_entry(ctx, window_ref)?;
        Some((entry.tab_ref.clone()?, entry.last_url.clone()?))
    }

    unsafe fn record_navigation(ctx: &mut Ctx, nav: &Navigation) {
        let Some(entry) = browser_entry(ctx, &nav.window_ref) else {
            return;
        };
        entry.tab_ref = Some(nav.tab_ref.clone());
        entry.last_url = Some(nav.url.clone());
        entry.host = Some(nav.host.clone());
    }

    // The tab and host a `field.value` typed in this window is stamped with: `(None,
    // None)` until that window has reported a navigation.
    unsafe fn recorded_site(ctx: &mut Ctx, window_ref: &str) -> (Option<String>, Option<String>) {
        match browser_entry(ctx, window_ref) {
            Some(entry) => (entry.tab_ref.clone(), entry.host.clone()),
            None => (None, None),
        }
    }

    // The cached-or-walked page web area of one window.
    struct WebArea {
        // Owned by the CACHE — the caller must not release it.
        element: CFTypeRef,
        tab_ref: String,
        // The web area's own `AXTitle` when non-empty: the frame's `page_title`, and on a
        // cache hit the value that validated the cache.
        page_title: Option<String>,
    }

    // The window's page web area, from the cache or by ONE bounded walk.
    //
    // A cache hit is VALIDATED with a single read: the web area's own `AXTitle` must be
    // non-empty and appear inside the current window title. A same-tab retitle (a
    // countdown, an unread-count badge) moves both titles in step, so that one read
    // confirms the cached element is still the visible page; a tab switch replaces the
    // window title with the other tab's, which the cached web area's title no longer
    // matches. An empty web-area title is always a miss — never a hit by vacuous
    // containment. Validation cannot be left to a failing read instead: a Chromium
    // background tab's web area stays alive and keeps answering with ITS url, so a stale
    // hit would be silent rather than an error.
    unsafe fn web_area_for(
        ctx: &mut Ctx,
        window_ref: &str,
        window: CFTypeRef,
        window_title: &str,
    ) -> Option<WebArea> {
        if let Some(valid) = valid_cached_web_area(ctx, window_ref, window_title) {
            return Some(valid);
        }
        forget_web_area(ctx, window_ref);

        let web_area = web_area_of(window)?;
        // The walk's +1 belongs to the cache, so an entry that vanished under us (the app
        // detached) releases it here rather than leaking it.
        let Some(entry) = browser_entry(ctx, window_ref) else {
            release_web_area(web_area);
            return None;
        };
        entry.web_area = web_area;
        Some(WebArea {
            element: web_area,
            tab_ref: hash_ref(web_area)?,
            page_title: read_attr(web_area, "AXTitle").filter(|title| !title.is_empty()),
        })
    }

    unsafe fn valid_cached_web_area(
        ctx: &mut Ctx,
        window_ref: &str,
        window_title: &str,
    ) -> Option<WebArea> {
        let cached = browser_entry(ctx, window_ref)?.web_area;
        if cached.is_null() {
            return None;
        }
        let page_title = read_attr(cached, "AXTitle").filter(|title| !title.is_empty())?;
        if !window_title.contains(page_title.as_str()) {
            return None;
        }
        Some(WebArea {
            element: cached,
            tab_ref: hash_ref(cached)?,
            page_title: Some(page_title),
        })
    }

    unsafe fn forget_web_area(ctx: &mut Ctx, window_ref: &str) {
        let Some(entry) = browser_entry(ctx, window_ref) else {
            return;
        };
        release_web_area(entry.web_area);
        entry.web_area = std::ptr::null_mut();
    }

    unsafe fn release_web_area(web_area: CFTypeRef) {
        if !web_area.is_null() {
            CFRelease(web_area);
        }
    }

    // A browser whose privacy posture cannot be read is a STANDING condition of that
    // app (no marker is pinned for its family), so it is announced once per app per
    // session and carries the app — the same discipline as `title_only`.
    fn announce_private_unknown(ctx: &mut Ctx, app: &AppIdentity, from_ms: i64) {
        let Some(bundle) = app.bundle_id.clone() else {
            return;
        };
        if !ctx
            .gap_ledger
            .announce((bundle.clone(), GapReason::PrivateUnknown))
        {
            return;
        }
        log(&format!(
            "{bundle}: private-browsing state unreadable — typed text is withheld for it"
        ));
        ctx.emit_gap(GapReason::PrivateUnknown, from_ms, now_ms(), Some(app));
    }

    // Read the settled value of the pending element and emit field.value. THE only way
    // a value leaves the debounce — the timer, the ceiling, a blur, and detach all come
    // through here — so an edit is never dropped by one path and kept by another. In a
    // browser the content rides the private gate (inv. 26) and carries the site context
    // of the window it was typed in; an unreadable element emits nothing and is simply
    // released.
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

    unsafe fn emit_field_value(
        ctx: &mut Ctx,
        app: &AppIdentity,
        element: CFTypeRef,
        started_ms: i64,
    ) {
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
                emit_browser_field_value(ctx, app, element, &text, extra, started_ms);
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

    // A browser field's content is gated on the window's private-browsing posture
    // (inv. 26) and stamped with the site the last navigation bound that window to
    // (§8.4). The posture is re-read here rather than trusted from the last navigation:
    // a tab switch changes the window title, and the marker rides the title.
    unsafe fn emit_browser_field_value(
        ctx: &mut Ctx,
        app: &AppIdentity,
        element: CFTypeRef,
        text: &str,
        mut extra: Map<String, Value>,
        started_ms: i64,
    ) {
        let Some(bundle) = app.bundle_id.clone() else {
            return;
        };
        let site = browser_field_site(ctx, &bundle, element);
        let context = BrowserFieldContext {
            browser_id: &bundle,
            window_ref: site.window_ref.as_deref(),
            tab_ref: site.tab_ref.as_deref(),
            host: site.host.as_deref(),
        };
        let owed = browser_field_extras(text, site.private_state, &context, &mut extra);
        ctx.emit("field.value", Some(app), extra);

        if owed.truncated {
            ctx.emit_gap_reason_truncated(started_ms);
        }
        if owed.private_unknown {
            announce_private_unknown(ctx, app, started_ms);
        }
    }

    // What the private gate and the site stamp need for one settled browser value.
    struct BrowserFieldSite {
        private_state: PrivateState,
        window_ref: Option<String>,
        tab_ref: Option<String>,
        host: Option<String>,
    }

    // Classify the window this value was typed in and look up its site. The window comes
    // from the ELEMENT (`AXWindow`), not from the app's focused window — a value can
    // settle after focus has moved on.
    //
    // A window that cannot be read is classified from an EMPTY title, the same answer a
    // window that answered with no title gets: `Unreadable` for a pinned family (withhold
    // this one value, claim nothing about the app) and `Unknown` for an unpinned one
    // (a standing fact that owes its gap). Never `NotPrivate` by absence of evidence.
    unsafe fn browser_field_site(
        ctx: &mut Ctx,
        bundle: &str,
        element: CFTypeRef,
    ) -> BrowserFieldSite {
        let Some(window) = copy_element_attr(element, "AXWindow") else {
            return BrowserFieldSite {
                private_state: private_state(bundle, ""),
                window_ref: None,
                tab_ref: None,
                host: None,
            };
        };
        let window_title = read_attr(window, "AXTitle").unwrap_or_default();
        let window_ref = hash_ref(window);
        CFRelease(window);

        let state = private_state(bundle, &window_title);
        let Some(window_ref) = window_ref else {
            return BrowserFieldSite {
                private_state: state,
                window_ref: None,
                tab_ref: None,
                host: None,
            };
        };
        let (tab_ref, host) = recorded_site(ctx, &window_ref);
        BrowserFieldSite {
            private_state: state,
            window_ref: Some(window_ref),
            tab_ref,
            host,
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

    // Copy a CFType-valued attribute of an element. The result follows the Copy rule
    // (+1), so the CALLER releases it.
    unsafe fn copy_element_attr(element: CFTypeRef, attr: &str) -> Option<CFTypeRef> {
        if element.is_null() {
            return None;
        }
        let cf = CFString::new(attr);
        let mut out: CFTypeRef = std::ptr::null_mut();
        if AXUIElementCopyAttributeValue(element, cf.as_concrete_TypeRef(), &mut out) != 0
            || out.is_null()
        {
            return None;
        }
        Some(out)
    }

    // Read a string attribute of an element; None if absent or non-string.
    unsafe fn read_attr(element: CFTypeRef, attr: &str) -> Option<String> {
        let out = copy_element_attr(element, attr)?;
        let text = cfstring_to_string(out as CFStringRef);
        CFRelease(out);
        text
    }

    // The human label of a field: AXTitle, else AXDescription.
    unsafe fn field_label(element: CFTypeRef) -> Option<String> {
        read_attr(element, "AXTitle").or_else(|| read_attr(element, "AXDescription"))
    }

    // The attached app's focused window, RETAINED — the caller releases it. Read off
    // the app element the attach created, so it inherits that app's messaging timeout.
    unsafe fn focused_window(ctx: &Ctx) -> Option<CFTypeRef> {
        let app_element = ctx.attached.as_ref()?.app_element;
        copy_element_attr(app_element, "AXFocusedWindow")
    }

    // Title of the app's focused window (best-effort).
    unsafe fn focused_window_title(ctx: &Ctx) -> Option<String> {
        let window = focused_window(ctx)?;
        let title = read_attr(window, "AXTitle");
        CFRelease(window);
        title
    }

    // A stable per-element identity for the wire: `CFHash` of the AX element, decimal so
    // the store can group by it. `window_ref` and `tab_ref` are these.
    unsafe fn hash_ref(element: CFTypeRef) -> Option<String> {
        if element.is_null() {
            return None;
        }
        Some(CFHash(element).to_string())
    }

    // The window's page web area, RETAINED — the caller (the per-window cache) owns it.
    // The traversal, its order and its bounds live in `find_page_web_area`; this is only
    // the AX half: the two reads, and releasing every element the walk hands back.
    unsafe fn web_area_of(window: CFTypeRef) -> Option<CFTypeRef> {
        if window.is_null() {
            return None;
        }
        // The root is retained so it is released through the same accounting as every
        // element the walk copies.
        CFRetain(window);
        let walk = find_page_web_area(
            window,
            WEB_AREA_MAX_DEPTH,
            WEB_AREA_MAX_NODES,
            &mut |element| copy_children(element),
            &mut |element| page_url_of(element).is_some(),
        );
        for element in walk.discarded {
            CFRelease(element);
        }
        walk.found
    }

    // The walk's page test, and the authoritative URL read for a navigation: an
    // `AXWebArea` whose `AXURL` reduces to an http(s) page. A browser window holds more
    // than one web area — a side panel, a `chrome://` new-tab page — so the ROLE alone
    // is not the page, and the walk keeps going past the ones that are not.
    unsafe fn page_url_of(element: CFTypeRef) -> Option<(String, String)> {
        if read_attr(element, "AXRole").as_deref() != Some(WEB_AREA_ROLE) {
            return None;
        }
        normalize_url(&read_url(element)?)
    }

    const WEB_AREA_ROLE: &str = "AXWebArea";

    // One element's children, each RETAINED (the caller releases them).
    unsafe fn copy_children(element: CFTypeRef) -> Vec<CFTypeRef> {
        let Some(array) = copy_element_attr(element, "AXChildren") else {
            return Vec::new();
        };
        // AXChildren SHOULD be a CFArray, but an app with a custom or broken AX
        // implementation can return another CFType, and the array getters would then
        // type-confuse and read garbage. Verify before treating it as an array.
        if CFGetTypeID(array) != CFArrayGetTypeID() {
            CFRelease(array);
            return Vec::new();
        }
        let count = CFArrayGetCount(array);
        let mut children = Vec::new();
        let mut index = 0;
        while index < count && children.len() < WEB_AREA_MAX_NODES {
            let child = CFArrayGetValueAtIndex(array, index);
            if !child.is_null() {
                CFRetain(child);
                children.push(child);
            }
            index += 1;
        }
        CFRelease(array);
        children
    }

    // The web area's `AXURL`, as the browser reports it (still unstripped — the caller
    // runs it through `normalize_url` before anything reaches the wire). The attribute
    // is a CFURL; any other type is refused rather than reinterpreted.
    unsafe fn read_url(web_area: CFTypeRef) -> Option<String> {
        let value = copy_element_attr(web_area, "AXURL")?;
        if CFGetTypeID(value) != CFURLGetTypeID() {
            CFRelease(value);
            return None;
        }
        // CFURLGetString follows the GET rule (do not release the result).
        let url = cfstring_to_string(CFURLGetString(value as CFURLRef));
        CFRelease(value);
        url
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
        // Inv. 28: bound every read made through an app's elements, so an app that
        // stops answering cannot wedge the observer thread. Set once per attach.
        fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout_s: f32) -> i32;
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
        // The wire's `window_ref` / `tab_ref`: a stable identity for one AX element.
        fn CFHash(cf: CFTypeRef) -> usize;
        fn CFRelease(cf: CFTypeRef);
        fn CFArrayGetCount(array: CFTypeRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
        // AXChildren and AXURL are type-checked before use: an app with a broken AX
        // implementation can answer with a different CFType entirely.
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFArrayGetTypeID() -> usize;
        fn CFURLGetTypeID() -> usize;
        fn CFURLGetString(url: CFURLRef) -> CFStringRef;
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
            attached_as(test_identity(), tree, title)
        }

        fn attached_as(identity: AppIdentity, tree: AxTree, title: TitleDebounce) -> Attached {
            Attached {
                identity,
                app_element: std::ptr::null_mut(),
                observer: std::ptr::null_mut(),
                ax_tree: tree,
                title,
                browser_windows: Vec::new(),
            }
        }

        fn browser_identity() -> AppIdentity {
            AppIdentity {
                bundle_id: Some("com.google.Chrome".into()),
                name: Some("Google Chrome".into()),
                pid: TEST_PID,
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

        // The navigation read is gated on the app's accessibility tree, not just on it
        // being a browser: a `TitleOnly` app has no web area to walk at all, and an
        // `Unknown` tree is still being re-probed. Walking either is pure cost — up to
        // WEB_AREA_MAX_NODES AX reads per settled title — for a guaranteed nothing.
        #[test]
        fn a_browser_without_an_enabled_tree_is_never_walked() {
            let seq = AtomicU64::new(0);

            for tree in [AxTree::TitleOnly, AxTree::Unknown] {
                let mut ctx = test_ctx(
                    &seq,
                    Some(attached_as(
                        browser_identity(),
                        tree,
                        TitleDebounce::default(),
                    )),
                );
                assert!(
                    attached_browser(&ctx).is_none(),
                    "{tree:?} must not be walked"
                );
                unsafe { maybe_emit_navigation(&mut ctx) };
                assert!(ctx.emitter.captured().is_empty());
            }

            // An enabled tree IS the one that gets walked (the AX reads then answer
            // nothing here, since this Ctx owns no real app element).
            let mut ctx = test_ctx(
                &seq,
                Some(attached_as(
                    browser_identity(),
                    AxTree::Enabled,
                    TitleDebounce::default(),
                )),
            );
            assert!(attached_browser(&ctx).is_some());
            unsafe { maybe_emit_navigation(&mut ctx) };
            assert!(ctx.emitter.captured().is_empty());

            // A non-browser with an enabled tree is not walked either.
            let ctx = test_ctx(
                &seq,
                Some(attached_with(AxTree::Enabled, TitleDebounce::default())),
            );
            assert!(attached_browser(&ctx).is_none());
        }

        // The per-window map is bounded, and what it drops is the window the owner has
        // not looked at in longest — not the one they are typing in. Dropping a live
        // window's entry would lose its `last_url`, so the next check would re-report the
        // page it is already on as a fresh navigation.
        #[test]
        fn the_browser_window_map_evicts_the_least_recently_touched_entry() {
            let seq = AtomicU64::new(0);
            let mut ctx = test_ctx(
                &seq,
                Some(attached_as(
                    browser_identity(),
                    AxTree::Enabled,
                    TitleDebounce::default(),
                )),
            );

            for window in 0..BROWSER_WINDOWS_MAX {
                let entry = unsafe { browser_entry(&mut ctx, &window.to_string()) }
                    .expect("an attached app always has a map");
                entry.last_url = Some(format!("https://example.com/{window}"));
            }
            assert_eq!(
                ctx.attached.as_ref().unwrap().browser_windows.len(),
                BROWSER_WINDOWS_MAX
            );

            // Touch the oldest so it is no longer the oldest …
            unsafe { browser_entry(&mut ctx, "0") };
            // … then overflow the bound by one.
            unsafe { browser_entry(&mut ctx, "fresh") };

            let windows = &ctx.attached.as_ref().unwrap().browser_windows;
            assert_eq!(windows.len(), BROWSER_WINDOWS_MAX, "the bound holds");
            let refs: Vec<&str> = windows.iter().map(|w| w.window_ref.as_str()).collect();
            assert!(refs.contains(&"fresh"), "the new window is tracked");
            assert!(
                refs.contains(&"0"),
                "re-touching kept the recently used window alive"
            );
            assert!(!refs.contains(&"1"), "the least recently touched one went");
            // The surviving entry kept what it had recorded.
            let zero = windows.iter().find(|w| w.window_ref == "0").unwrap();
            assert_eq!(zero.last_url.as_deref(), Some("https://example.com/0"));
        }

        // A window read that fails is a failed READ, wherever in the value path it fails.
        // An unreadable `AXWindow` used to classify every family as `Unknown`, so one
        // failed read on a PINNED family named it private-unknown for the rest of the
        // session — the thing the Unreadable split exists to prevent, on the other branch
        // of the same function. A null element takes that branch without an AX call.
        #[test]
        fn an_unreadable_value_window_owes_no_gap_for_a_pinned_family() {
            let seq = AtomicU64::new(0);
            let chrome = browser_identity();
            let mut ctx = test_ctx(
                &seq,
                Some(attached_as(
                    chrome.clone(),
                    AxTree::Enabled,
                    TitleDebounce::default(),
                )),
            );

            unsafe {
                emit_browser_field_value(
                    &mut ctx,
                    &chrome,
                    std::ptr::null_mut(),
                    "ship it",
                    Map::new(),
                    100,
                )
            };

            let frames = ctx.emitter.captured();
            assert_eq!(frames.len(), 1, "a pinned family owes no gap: {frames:?}");
            assert_eq!(frames[0]["kind"], json!("field.value"));
            assert_eq!(frames[0]["private_state"], json!("unknown"));
            assert_eq!(frames[0]["content_withheld"], json!(true));
            assert_eq!(frames[0]["char_len"], json!(7));
            assert!(frames[0].get("text").is_none(), "the text is withheld");

            // An UNPINNED family is a standing fact and still owes its one gap, whether or
            // not this particular window read worked.
            let safari = AppIdentity {
                bundle_id: Some("com.apple.Safari".into()),
                name: Some("Safari".into()),
                pid: TEST_PID,
            };
            let mut ctx = test_ctx(
                &seq,
                Some(attached_as(
                    safari.clone(),
                    AxTree::Enabled,
                    TitleDebounce::default(),
                )),
            );
            unsafe {
                emit_browser_field_value(
                    &mut ctx,
                    &safari,
                    std::ptr::null_mut(),
                    "ship it",
                    Map::new(),
                    100,
                )
            };

            let frames = ctx.emitter.captured();
            assert_eq!(
                frames.len(),
                2,
                "expected the value and one gap: {frames:?}"
            );
            assert_eq!(frames[0]["private_state"], json!("unknown"));
            assert_eq!(frames[1]["gap_reason"], json!("private_unknown"));
            assert_eq!(frames[1]["app"]["bundle_id"], json!("com.apple.Safari"));
        }

        // A browser whose private-browsing posture cannot be read is a standing fact
        // about THAT app, so the gap carries the app object and is announced once per
        // app per session — not once per value, which is how v1 emitted it.
        #[test]
        fn a_private_unknown_gap_carries_the_app_and_is_announced_once() {
            let seq = AtomicU64::new(0);
            let safari = AppIdentity {
                bundle_id: Some("com.apple.Safari".into()),
                name: Some("Safari".into()),
                pid: TEST_PID,
            };
            let mut ctx = test_ctx(&seq, None);

            announce_private_unknown(&mut ctx, &safari, 100);
            announce_private_unknown(&mut ctx, &safari, 200);

            let frames = ctx.emitter.captured();
            assert_eq!(frames.len(), 1, "announced more than once: {frames:?}");
            assert_eq!(frames[0]["kind"], json!("observer.gap"));
            assert_eq!(frames[0]["gap_reason"], json!("private_unknown"));
            assert_eq!(frames[0]["app"]["bundle_id"], json!("com.apple.Safari"));
            assert_eq!(frames[0]["gap_from_ts"], json!(100));

            // Another browser with the same unreadable posture is its own fact.
            let chrome = AppIdentity {
                bundle_id: Some("com.brave.Browser".into()),
                name: Some("Brave".into()),
                pid: TEST_PID,
            };
            announce_private_unknown(&mut ctx, &chrome, 300);
            let frames = ctx.emitter.captured();
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[1]["app"]["bundle_id"], json!("com.brave.Browser"));
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
        // The grant going away and a secure field are not properties of one app, so
        // neither carries an app object.
        for reason in [GapReason::SecureInput, GapReason::GrantRevoked] {
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

        // An unreadable private-browsing posture is a fact about ONE browser (no marker
        // is pinned for its family), so it moved into this column in 0.9.0.
        let frame = gap_frame(
            "boot-x",
            4,
            GapReason::PrivateUnknown,
            100,
            100,
            Some(&identity()),
        );
        assert_eq!(frame["gap_reason"], json!("private_unknown"));
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
    fn observe_config_parses_apps_and_ignores_a_retired_sites_key() {
        // `sites` is retired (consent is per browser, §2.1). An older consumer that
        // still sends it must be accepted with the key simply ignored — never refused,
        // and never a second allowlist to enforce.
        let req = json!({
            "action": "observe_start",
            "params": {"apps": ["com.apple.Safari", "com.apple.mail"], "sites": ["github.com"]}
        });
        let cfg = ObserveConfig::from_request(&req);
        assert_eq!(cfg.apps, vec!["com.apple.Safari", "com.apple.mail"]);
    }

    #[test]
    fn observe_config_defaults_to_deny_on_missing_params() {
        let cfg = ObserveConfig::from_request(&json!({"action": "observe_start"}));
        assert!(cfg.apps.is_empty());
    }

    // --- browser privacy, URLs, navigation -----------------------------------

    #[test]
    fn private_state_reads_each_pinned_family_from_its_window_title() {
        use PrivateState::{NotPrivate, Private};

        // The whole Chromium family shares Chrome's code and its "Incognito" string, and
        // stable Chrome is the one the live check exercises, so they are pinned together.
        for family in [
            "com.google.Chrome",
            "com.google.Chrome.canary",
            "com.google.Chrome.beta",
            "com.google.Chrome.dev",
            "org.chromium.Chromium",
        ] {
            assert_eq!(
                private_state(family, "Inbox - Gmail - Google Chrome (Incognito)"),
                Private,
                "{family} must read its pinned marker"
            );
            assert_eq!(
                private_state(family, "Inbox - Gmail - Google Chrome"),
                NotPrivate,
                "{family} without the marker is a normal window"
            );
        }

        // Case-sensitive: the markers are fixed UI strings, and a lowercase lookalike in
        // a page title must not be read as the marker.
        assert_eq!(
            private_state(
                "com.google.Chrome",
                "how incognito mode works - Google Chrome"
            ),
            NotPrivate
        );
    }

    #[test]
    fn every_pinned_family_is_a_browser() {
        // The table is only ever consulted for a browser, so an entry `is_browser` does
        // not know about would classify nothing and silently answer Unknown forever.
        for (family, marker) in PRIVATE_WINDOW_MARKERS {
            assert!(is_browser(family), "{family} is pinned but not a browser");
            assert!(!marker.is_empty(), "{family} has an empty marker");
        }
    }

    #[test]
    fn private_state_is_unknown_without_pinned_evidence() {
        // An UNPINNED browser is always Unknown: its private marker has never been checked
        // against a live window of that browser, and a false "not_private" would leak typed
        // text out of a private window (inv. 26) where an Unknown only loses capture.
        // Edge and Firefox are here deliberately — their candidate markers are recorded
        // beside the table but no live check covers them.
        for browser in [
            "com.apple.Safari",
            "com.apple.SafariTechnologyPreview",
            "com.brave.Browser",
            "com.operasoftware.Opera",
            "com.vivaldi.Vivaldi",
            "com.arc.Arc",
            "company.thebrowser.Browser",
            "com.microsoft.edgemac",
            "com.microsoft.edgemac.Beta",
            "org.mozilla.firefox",
            "org.mozilla.nightly",
        ] {
            assert_eq!(
                private_state(browser, "Anything At All"),
                PrivateState::Unknown,
                "{browser} has no pinned marker and must stay unknown"
            );
            // Including when its title is unreadable: an unpinned family could not have
            // been classified from a title anyway.
            assert_eq!(private_state(browser, ""), PrivateState::Unknown);
        }

        // The classifier is only ever asked about browsers; anything else answers with
        // the fail-closed value rather than a guess.
        assert_eq!(
            private_state("com.apple.TextEdit", "Untitled"),
            PrivateState::Unknown
        );
    }

    #[test]
    fn private_state_is_unreadable_for_a_pinned_family_without_a_title() {
        // A PINNED family whose title could not be read is a failed READ, not an unpinned
        // browser. Both withhold the text, but only the unpinned case is a standing fact
        // about the app: conflating them lets one timed-out read name Chrome
        // private-unknown for the rest of the session (and, before the split, let an
        // incognito URL cross as "unknown").
        assert_eq!(
            private_state("com.google.Chrome", ""),
            PrivateState::Unreadable
        );
        assert_eq!(
            private_state("org.chromium.Chromium", ""),
            PrivateState::Unreadable
        );
    }

    #[test]
    fn private_state_wire_spellings_match_the_store_column() {
        assert_eq!(PrivateState::Private.as_str(), "private");
        assert_eq!(PrivateState::NotPrivate.as_str(), "not_private");
        assert_eq!(PrivateState::Unknown.as_str(), "unknown");
        // The store has three values, so an unreadable read is reported as `unknown`; the
        // distinction lives in what the sidecar DOES, not in a fourth wire spelling.
        assert_eq!(PrivateState::Unreadable.as_str(), "unknown");
    }

    #[test]
    fn normalize_url_strips_everything_the_store_must_not_keep() {
        // The brief's worked example: userinfo, port, query and fragment all gone.
        assert_eq!(
            normalize_url("https://user:pw@host:8443/a?b#c"),
            Some(("https://host/a".into(), "host".into()))
        );
        // A query string is where session ids and one-time codes live (inv. 27).
        assert_eq!(
            normalize_url("https://mail.google.com/mail/u/0?token=abc123"),
            Some((
                "https://mail.google.com/mail/u/0".into(),
                "mail.google.com".into()
            ))
        );
        assert_eq!(
            normalize_url("http://example.com/docs#section-4"),
            Some(("http://example.com/docs".into(), "example.com".into()))
        );
        // No path at all is still a navigation.
        assert_eq!(
            normalize_url("https://example.com"),
            Some(("https://example.com".into(), "example.com".into()))
        );
        assert_eq!(
            normalize_url("https://example.com/"),
            Some(("https://example.com/".into(), "example.com".into()))
        );
        // A query with no path must not swallow the host.
        assert_eq!(
            normalize_url("https://example.com?q=secret"),
            Some(("https://example.com".into(), "example.com".into()))
        );
    }

    #[test]
    fn normalize_url_normalizes_the_host_and_keeps_the_path_verbatim() {
        // Scheme and host are case-insensitive; the path is not.
        assert_eq!(
            normalize_url("HTTPS://Example.COM/Path/To/Page"),
            Some((
                "https://example.com/Path/To/Page".into(),
                "example.com".into()
            ))
        );
        // An IDN host passes through as-is (lowercased), punycode included.
        assert_eq!(
            normalize_url("https://Bücher.example/seite"),
            Some((
                "https://bücher.example/seite".into(),
                "bücher.example".into()
            ))
        );
        assert_eq!(
            normalize_url("https://xn--bcher-kva.example/seite"),
            Some((
                "https://xn--bcher-kva.example/seite".into(),
                "xn--bcher-kva.example".into()
            ))
        );
        // An IPv6 literal keeps its brackets: the colons inside are not a port.
        assert_eq!(
            normalize_url("http://[::1]:8080/health"),
            Some(("http://[::1]/health".into(), "[::1]".into()))
        );
    }

    #[test]
    fn normalize_url_drops_a_url_over_the_cap_rather_than_cutting_it() {
        // A truncated URL is worse than no URL: it names a page that does not exist and
        // the store cannot tell it was cut. So the whole navigation is dropped.
        let host = "example.com";
        let at_cap = "/".to_string() + &"a".repeat(MAX_URL_BYTES - "https://example.com/".len());
        let url = format!("https://{host}{at_cap}");
        assert_eq!(url.len(), MAX_URL_BYTES);
        assert_eq!(
            normalize_url(&url),
            Some((url.clone(), host.to_string())),
            "exactly at the cap still reports"
        );

        let over = format!("{url}b");
        assert_eq!(over.len(), MAX_URL_BYTES + 1);
        assert_eq!(normalize_url(&over), None);

        // The cap applies to the NORMALIZED url, so a monstrous query string on a short
        // path is still a perfectly good navigation.
        let long_query = format!("https://{host}/search?q={}", "x".repeat(20_000));
        assert_eq!(
            normalize_url(&long_query),
            Some((
                "https://example.com/search".into(),
                "example.com".to_string()
            ))
        );
    }

    #[test]
    fn normalize_url_refuses_what_is_not_a_page_to_recall() {
        // A `file:` path is a local secret; the internal pages are nothing to recall.
        for raw in [
            "file:///Users/owner/Documents/tax-return.pdf",
            "about:blank",
            "chrome://settings/passwords",
            "safari-resource:///ErrorPage.html",
            "data:text/html,<h1>hi</h1>",
            "ftp://files.example.com/pub",
            "javascript:alert(1)",
        ] {
            assert_eq!(normalize_url(raw), None, "{raw} must emit no navigation");
        }
        // An http(s) URL with no host is not a site.
        assert_eq!(normalize_url("https:///just/a/path"), None);
        assert_eq!(normalize_url("http://@/x"), None);
        // Not a URL at all.
        assert_eq!(normalize_url(""), None);
        assert_eq!(normalize_url("example.com/page"), None);
    }

    #[test]
    fn browser_navigation_is_emitted_once_per_url_change() {
        use NavigationDecision::{Emit, Skip};
        let tab = "4815162342";

        // The first reading for a window is always a navigation …
        assert_eq!(
            navigation_decision(None, tab, "https://a.example/one", PrivateState::NotPrivate),
            Emit
        );
        // … the same pair again is not …
        assert_eq!(
            navigation_decision(
                Some((tab, "https://a.example/one")),
                tab,
                "https://a.example/one",
                PrivateState::NotPrivate
            ),
            Skip
        );
        // … a new URL in the same tab is …
        assert_eq!(
            navigation_decision(
                Some((tab, "https://a.example/one")),
                tab,
                "https://a.example/two",
                PrivateState::NotPrivate
            ),
            Emit
        );
        // … and the same URL in a DIFFERENT tab is, too (a second tab on one page is a
        // place the owner went).
        assert_eq!(
            navigation_decision(
                Some((tab, "https://a.example/one")),
                "99",
                "https://a.example/one",
                PrivateState::NotPrivate
            ),
            Emit
        );
        // An unpinned browser still reports where the owner went: only TEXT is gated on
        // a positive signal, URLs are gated on the absence of a private one.
        assert_eq!(
            navigation_decision(None, tab, "https://a.example/one", PrivateState::Unknown),
            Emit
        );
    }

    #[test]
    fn a_retitle_without_a_url_change_is_not_a_navigation() {
        // The spinner case: a window that re-titles several times a second settles a new
        // title each time, and every settle asks this. The page did not change.
        let tab = "7";
        let last = Some((tab, "https://build.example/job/1742"));
        for _ in 0..5 {
            assert_eq!(
                navigation_decision(
                    last,
                    tab,
                    "https://build.example/job/1742",
                    PrivateState::NotPrivate
                ),
                NavigationDecision::Skip
            );
        }
    }

    #[test]
    fn an_unreadable_window_never_emits_a_navigation() {
        // A pinned family whose title could not be read might be an incognito window whose
        // marker we simply failed to see, so its URL must not cross either (inv. 26). The
        // absence of a marker is only evidence when the title was actually readable.
        assert_eq!(
            navigation_decision(
                None,
                "7",
                "https://bank.example/login",
                PrivateState::Unreadable
            ),
            NavigationDecision::Skip
        );
        assert_eq!(
            navigation_decision(
                Some(("7", "https://bank.example/login")),
                "7",
                "https://bank.example/transfer",
                PrivateState::Unreadable
            ),
            NavigationDecision::Skip
        );
    }

    #[test]
    fn a_private_window_never_emits_a_navigation() {
        // Inv. 26: the URL of a private window must not cross the Port at all — not on
        // the first sighting, and not when it changes.
        assert_eq!(
            navigation_decision(
                None,
                "7",
                "https://bank.example/login",
                PrivateState::Private
            ),
            NavigationDecision::Skip
        );
        assert_eq!(
            navigation_decision(
                Some(("7", "https://bank.example/login")),
                "7",
                "https://bank.example/transfer",
                PrivateState::Private
            ),
            NavigationDecision::Skip
        );
    }

    #[test]
    fn browser_navigated_frame_carries_the_stripped_url_and_both_refs() {
        let nav = Navigation {
            window_ref: "111".into(),
            window_title: "Pull requests - Google Chrome".into(),
            tab_ref: "222".into(),
            url: "https://github.com/tezra-io/compux/pulls".into(),
            host: "github.com".into(),
            page_title: Some("Pull requests".into()),
            private_state: PrivateState::NotPrivate,
        };
        let frame = event_frame(
            "boot-x",
            9,
            "browser.navigated",
            Some(&identity()),
            navigation_extras("com.google.Chrome", &nav),
        );

        assert_eq!(frame["kind"], json!("browser.navigated"));
        assert_eq!(frame["browser_id"], json!("com.google.Chrome"));
        assert_eq!(
            frame["url"],
            json!("https://github.com/tezra-io/compux/pulls")
        );
        assert_eq!(frame["host"], json!("github.com"));
        assert_eq!(frame["page_title"], json!("Pull requests"));
        assert_eq!(
            frame["window_title"],
            json!("Pull requests - Google Chrome")
        );
        assert_eq!(frame["window_ref"], json!("111"));
        assert_eq!(frame["tab_ref"], json!("222"));
        assert_eq!(frame["private_state"], json!("not_private"));
        // Navigation carries no content field of its own.
        assert!(frame.get("text").is_none());

        // An empty web-area title is an ABSENT page_title, never the window title
        // substituted for it; an unreadable window title is absent too.
        let bare = Navigation {
            page_title: None,
            window_title: String::new(),
            ..nav
        };
        let extra = navigation_extras("com.google.Chrome", &bare);
        assert!(extra.get("page_title").is_none());
        assert!(extra.get("window_title").is_none());
    }

    // --- the web-area walk, over a synthetic tree -----------------------------
    //
    // The real walk's two reads are AX IPC calls, so what matters is HOW MANY it makes
    // and that every element it obtains comes back to be released. Both are properties
    // of the traversal, not of AX, so the traversal takes its reads as closures and is
    // exercised here against a tree built in memory.
    struct FakeTree {
        // `kids[i]` are node i's children.
        kids: Vec<Vec<usize>>,
        // The nodes that answer "this is the page".
        pages: Vec<usize>,
        // One entry per `is_page` call — the count the cap must bound.
        reads: Vec<usize>,
        // Every node the walk was handed, root included: each must come back exactly once.
        handed: Vec<usize>,
    }

    impl FakeTree {
        // A root with `width` children, each with `width` children of its own.
        fn wide(width: usize) -> FakeTree {
            let mut kids = vec![(1..=width).collect::<Vec<usize>>()];
            for parent in 1..=width {
                let first = 1 + width + (parent - 1) * width;
                kids.push((first..first + width).collect());
            }
            while kids.len() < 1 + width + width * width {
                kids.push(Vec::new());
            }
            FakeTree {
                kids,
                pages: Vec::new(),
                reads: Vec::new(),
                handed: vec![0],
            }
        }

        fn walk(&mut self, max_depth: usize, max_nodes: usize) -> WebAreaWalk<usize> {
            let (kids, pages) = (self.kids.clone(), self.pages.clone());
            let (reads, handed) = (&mut self.reads, &mut self.handed);
            find_page_web_area(
                0,
                max_depth,
                max_nodes,
                &mut |node| {
                    let children = kids.get(node).cloned().unwrap_or_default();
                    handed.extend(children.iter().copied());
                    children
                },
                &mut |node| {
                    reads.push(node);
                    pages.contains(&node)
                },
            )
        }
    }

    fn assert_every_node_came_back(tree: &FakeTree, walk: &WebAreaWalk<usize>) {
        let mut returned = walk.discarded.clone();
        returned.extend(walk.found);
        returned.sort_unstable();
        let mut handed = tree.handed.clone();
        handed.sort_unstable();
        assert_eq!(
            returned, handed,
            "every element the walk obtained must come back exactly once to be released"
        );
    }

    #[test]
    fn the_web_area_walk_reads_no_more_nodes_than_its_cap() {
        // A wide tree is the hazard: gating only EXPANSION lets 10 expansions × 8 children
        // enqueue 80 elements and read all of them. The cap is checked before each read.
        let mut tree = FakeTree::wide(8);
        let walk = tree.walk(12, 10);

        assert_eq!(walk.found, None, "there is no page in this tree");
        assert_eq!(
            tree.reads.len(),
            10,
            "the cap bounds the READS, not just the expansions"
        );
        // Retention is bounded too: at most the cap plus one node's children are ever
        // held, so a 160k-node tree cannot be pulled into memory.
        assert!(
            tree.handed.len() <= 10 + 8,
            "held {} elements at once",
            tree.handed.len()
        );
        assert_every_node_came_back(&tree, &walk);
    }

    #[test]
    fn the_web_area_walk_keeps_going_past_a_web_area_that_is_not_a_page() {
        // A browser window holds more than one web area: a side panel, a `chrome://`
        // new-tab page. The predicate is "web area whose URL is a page", so the walk must
        // keep going past the ones that are not rather than caching the first hit.
        let mut tree = FakeTree::wide(3);
        // Node 1 is the chrome:// web area (not a page); node 3 is the real one.
        tree.pages = vec![3];
        let walk = tree.walk(12, 100);

        assert_eq!(walk.found, Some(3));
        assert!(
            tree.reads.contains(&1),
            "the non-page web area was tested and rejected"
        );
        assert!(
            walk.discarded.contains(&1),
            "a rejected web area must be released, not cached"
        );
        assert_every_node_came_back(&tree, &walk);
    }

    #[test]
    fn the_web_area_walk_is_breadth_first_and_stops_at_its_depth_cap() {
        // Breadth-first: the content area sits shallow under the window, so a deep
        // toolbar subtree must never be descended before it.
        let mut tree = FakeTree::wide(3);
        tree.pages = vec![2, 7];
        let walk = tree.walk(12, 100);
        assert_eq!(walk.found, Some(2), "the shallow page wins");

        // The depth cap: a page below it is simply not found, and nothing leaks.
        let mut tree = FakeTree::wide(3);
        tree.pages = vec![7]; // a grandchild, at depth 2
        let walk = tree.walk(1, 100);
        assert_eq!(walk.found, None);
        assert_every_node_came_back(&tree, &walk);
    }

    fn field_context<'a>() -> BrowserFieldContext<'a> {
        BrowserFieldContext {
            browser_id: "com.google.Chrome",
            window_ref: Some("111"),
            tab_ref: Some("222"),
            host: Some("github.com"),
        }
    }

    #[test]
    fn a_not_private_browser_field_sends_its_text_with_the_site_context() {
        let mut extra = Map::new();
        let owed = browser_field_extras(
            "ship it",
            PrivateState::NotPrivate,
            &field_context(),
            &mut extra,
        );

        assert_eq!(extra["text"], json!("ship it"));
        assert_eq!(extra["char_len"], json!(7));
        assert_eq!(extra["content_withheld"], json!(false));
        assert_eq!(extra["private_state"], json!("not_private"));
        assert_eq!(extra["browser_id"], json!("com.google.Chrome"));
        assert_eq!(extra["window_ref"], json!("111"));
        assert_eq!(extra["tab_ref"], json!("222"));
        assert_eq!(extra["host"], json!("github.com"));
        assert_eq!(owed, BrowserFieldOwed::default());

        // A window that has not reported a navigation yet has no site to stamp: the keys
        // are absent, never null.
        let mut extra = Map::new();
        let context = BrowserFieldContext {
            tab_ref: None,
            host: None,
            ..field_context()
        };
        browser_field_extras("ship it", PrivateState::NotPrivate, &context, &mut extra);
        assert!(extra.get("tab_ref").is_none());
        assert!(extra.get("host").is_none());

        // Over the cap the text is clipped, the FULL length is still reported, and a
        // truncated gap is owed.
        let long = "a".repeat(MAX_TEXT_BYTES + 10);
        let mut extra = Map::new();
        let owed = browser_field_extras(
            &long,
            PrivateState::NotPrivate,
            &field_context(),
            &mut extra,
        );
        assert_eq!(extra["char_len"], json!((MAX_TEXT_BYTES + 10) as i64));
        assert!(extra["text"].as_str().unwrap().len() <= MAX_TEXT_BYTES);
        assert!(owed.truncated);
        assert!(!owed.private_unknown);
    }

    #[test]
    fn a_private_browser_field_withholds_its_text_and_owes_no_gap() {
        let mut extra = Map::new();
        let owed = browser_field_extras(
            "my card number",
            PrivateState::Private,
            &field_context(),
            &mut extra,
        );

        // Inv. 26: no text, and no site either — a private window's host never reached
        // the wire, so nothing may imply it did.
        assert!(extra.get("text").is_none());
        assert!(extra.get("host").is_none());
        assert!(extra.get("tab_ref").is_none());
        assert_eq!(extra["content_withheld"], json!(true));
        assert_eq!(extra["private_state"], json!("private"));
        assert_eq!(extra["char_len"], json!(14));
        assert_eq!(extra["browser_id"], json!("com.google.Chrome"));
        assert_eq!(extra["window_ref"], json!("111"));
        // A private window is working as designed, not a coverage gap.
        assert_eq!(owed, BrowserFieldOwed::default());
    }

    #[test]
    fn an_unknown_browser_field_withholds_its_text_and_owes_the_gap() {
        let mut extra = Map::new();
        let owed = browser_field_extras(
            "my card number",
            PrivateState::Unknown,
            &field_context(),
            &mut extra,
        );

        assert!(extra.get("text").is_none());
        assert!(extra.get("host").is_none());
        assert_eq!(extra["content_withheld"], json!(true));
        assert_eq!(extra["private_state"], json!("unknown"));
        assert_eq!(extra["char_len"], json!(14));
        assert_eq!(extra["window_ref"], json!("111"));
        assert!(
            owed.private_unknown,
            "an unreadable posture is a coverage fact the session must name"
        );
        assert!(!owed.truncated);

        // No window to classify at all: the refs are absent, the posture is still
        // unknown, and the text is still withheld.
        let mut extra = Map::new();
        let context = BrowserFieldContext {
            window_ref: None,
            tab_ref: None,
            host: None,
            ..field_context()
        };
        let owed = browser_field_extras("x", PrivateState::Unknown, &context, &mut extra);
        assert!(extra.get("window_ref").is_none());
        assert_eq!(extra["content_withheld"], json!(true));
        assert!(owed.private_unknown);
    }

    #[test]
    fn an_unreadable_browser_field_withholds_without_a_standing_gap() {
        let mut extra = Map::new();
        let owed = browser_field_extras(
            "my card number",
            PrivateState::Unreadable,
            &field_context(),
            &mut extra,
        );

        // The text is withheld exactly as for an unknown window …
        assert!(extra.get("text").is_none());
        assert!(extra.get("host").is_none());
        assert_eq!(extra["content_withheld"], json!(true));
        assert_eq!(extra["private_state"], json!("unknown"));
        assert_eq!(extra["char_len"], json!(14));
        // … but ONE failed read is not a standing property of the app, so it owes no gap:
        // a pinned family must never be named private-unknown for the whole session.
        assert_eq!(owed, BrowserFieldOwed::default());
    }
}
