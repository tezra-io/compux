//! Computer history on Linux (M97 stage 4): what the recorder emits for each event
//! the accessibility bus sends, decided here and tested on every system. The bus
//! itself (the connection, the subscriptions, the reads) is `linux::observe`'s,
//! behind [`Bus`]; this module makes no D-Bus call of its own.
//!
//! It emits the frames the macOS observer emits (`capture`), for the same reasons:
//!
//! - `app.activated` (with `prev_bundle_id`) when a window of an allowlisted
//!   application becomes the active one, and nothing for any other application;
//! - `window.focused` when one of its windows becomes active, and
//!   `window.title_changed` for that window's settled title;
//! - `focus.changed` when one of its controls takes the focus;
//! - `field.value` for the settled text of the focused field. A password field is
//!   never read (`gap{secure_input}`). In a browser no text is ever received or
//!   read: the field's character count alone, withheld, with `private_state`
//!   `unknown` and `gap{private_unknown}` once per browser per session, since no
//!   Linux browser has a private-window marker this recorder trusts. A field
//!   inside a web document in any application is a browser's field. A terminal's
//!   text is never read either (a prompt's password is marked by no role, and a
//!   command prints what it likes): its count alone, withheld.
//!
//! An application's identity, the allowlist key the frames carry as
//! `app.bundle_id`, is its name on the accessibility bus (the Name of its root),
//! matched exactly. Whether it is a browser fails closed: it is one by its name, by
//! its program, or by the web engine its toolkit names (Gecko, Chromium, WebKit),
//! and an application whose toolkit cannot be read is taken as one, so withholding
//! never rests on a list being complete.
//!
//! Default-deny is structural: only the one active allowlisted application is
//! watched (`Bus::watch`), so nothing another application does reaches the helper
//! beyond the content-free fact that one of its windows became active. A browser is
//! watched without its text changes, whose signals carry the text itself.

// The engine that drives this module is Linux's; on any other system only the tests
// do, so an unused helper here is a portability fact rather than dead code.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::capture::{
    app_allowed, burst_expired, char_len, event_frame, gap_frame, insert_str, truncate_text,
    AppIdentity, Emitter, GapReason, Ledger, SettleWhen, TitleDebounce, MAX_TEXT_BYTES,
};

/// Edits settle this long after the last one: a burst of keystrokes is one value.
pub const VALUE_DEBOUNCE_MS: i64 = 600;
/// A value that never goes quiet still reports this often.
pub const VALUE_MAX_WAIT_MS: i64 = 5_000;
/// A window title settles this long after its last change.
pub const TITLE_DEBOUNCE_MS: i64 = 1_000;
/// A title that never settles still reports this often.
pub const TITLE_MAX_WAIT_MS: i64 = 5_000;
/// A window that stops being active is given this long for another window of the
/// same application to become active, which is a window switch and not an
/// application switch.
pub const DEACTIVATION_GRACE_MS: i64 = 300;
/// The most characters a field's text is read with: enough for MAX_TEXT_BYTES of
/// any text, which `truncate_text` then cuts to bytes.
pub const MAX_TEXT_CHARS: i64 = MAX_TEXT_BYTES as i64;
/// The most applications whose identity is remembered; past it the memory starts
/// again, and an application is simply read again when it next acts.
const MAX_KNOWN: usize = 512;

/// Browsers by their name on the accessibility bus, compared without case: a name
/// here withholds text, and a wrong match only withholds more.
const BROWSER_NAMES: &[&str] = &[
    "Firefox",
    "Firefox Developer Edition",
    "Firefox Nightly",
    "Nightly",
    "LibreWolf",
    "Waterfox",
    "Floorp",
    "Zen",
    "Zen Browser",
    "Tor Browser",
    "Mullvad Browser",
    "Chromium",
    "Google Chrome",
    "Google Chrome Beta",
    "Google Chrome Dev",
    "Google Chrome Canary",
    "Brave",
    "Brave Browser",
    "Microsoft Edge",
    "Vivaldi",
    "Opera",
    "Yandex Browser",
    "Thorium",
    "Epiphany",
    "Web",
    "GNOME Web",
    "Falkon",
    "Konqueror",
    "Midori",
    "qutebrowser",
];

/// Browsers by their program (the executable's file name), exactly.
const BROWSER_PROGRAMS: &[&str] = &[
    "firefox",
    "firefox-bin",
    "firefox-esr",
    "firefox.real",
    "librewolf",
    "waterfox",
    "floorp",
    "zen",
    "zen-bin",
    "chromium",
    "chromium-browser",
    "chrome",
    "brave",
    "brave-browser",
    "msedge",
    "vivaldi-bin",
    "opera",
    "yandex_browser",
    "thorium",
    "epiphany",
    "epiphany-browser",
    "falkon",
    "konqueror",
    "midori",
    "qutebrowser",
];

/// The web engines a root's `ToolkitName` names, lower case, as a part of it.
const WEB_ENGINES: &[&str] = &["gecko", "chromium", "webkit"];

/// Whether an application's typed text is withheld as a browser's: by its name, by
/// its program, or by its toolkit, which fails closed (`web_engine`), so withholding
/// never rests on a hand list being complete.
pub fn is_browser(name: &str, program: Option<&str>, toolkit: Option<&str>) -> bool {
    BROWSER_NAMES
        .iter()
        .any(|browser| browser.eq_ignore_ascii_case(name))
        || program.is_some_and(|program| BROWSER_PROGRAMS.contains(&program))
        || web_engine(toolkit)
}

/// Whether a toolkit is a web engine, or could not be read to say it is not: an
/// application that does not say what it was made with is not known to be one that
/// keeps no web content, so it is taken as one. Gecko is Firefox's and Thunderbird's,
/// Chromium every Chromium browser's and every Electron application's.
fn web_engine(toolkit: Option<&str>) -> bool {
    let Some(toolkit) = toolkit.filter(|t| !t.trim().is_empty()) else {
        return true;
    };
    let toolkit = toolkit.to_ascii_lowercase();
    WEB_ENGINES.iter().any(|engine| toolkit.contains(engine))
}

/// One event of the accessibility bus, as the recorder reads it. `sender` is the
/// application's connection on the bus, `path` the accessible the event is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A window read ACTIVE (`on`) or stopped reading it.
    Active {
        sender: String,
        path: String,
        on: bool,
    },
    /// A control took the focus (`on`) or lost it.
    Focused {
        sender: String,
        path: String,
        on: bool,
    },
    /// A control's text changed. The recorder reads it again when it settles.
    TextChanged { sender: String, path: String },
    /// An accessible's name changed, to `name` when the event carried it.
    NameChanged {
        sender: String,
        path: String,
        name: Option<String>,
    },
    /// The application's connection left the bus.
    Gone { sender: String },
}

/// What kind of field a focused control is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// Its text is what a person typed there: a text field, a text area.
    Editable,
    /// A password field: never read.
    Secure,
    /// A terminal.
    Terminal,
    /// Anything else: its text is not recorded.
    Other,
}

/// A focused control, as one read of it found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Focus {
    /// AT-SPI's role word (`entry`, `password text`, …).
    pub role: String,
    pub field: Field,
    /// Its name, else its description.
    pub label: Option<String>,
    /// It sits inside a web document, or its ancestry could not be read as far as
    /// its application to say it does not.
    pub web: bool,
}

/// An application on the bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Application {
    /// Its root's Name: the allowlist key.
    pub name: String,
    pub pid: i32,
    /// The executable's file name, where it reads.
    pub program: Option<String>,
    /// The toolkit its root says it was made with (`ToolkitName`), where it says.
    pub toolkit: Option<String>,
}

/// A field's text, read bounded, and how many characters it holds in all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    pub text: String,
    pub chars: i64,
}

/// What the recorder asks of the bus. Every read answers `None` when it could not be
/// made; none of them changes anything in an application.
pub trait Bus {
    /// Who the application behind `sender` is.
    fn application(&self, sender: &str) -> Option<Application>;
    /// A window's title.
    fn title(&self, sender: &str, path: &str) -> Option<String>;
    /// A focused control: its role, the kind of field it is, its label.
    fn focus(&self, sender: &str, path: &str) -> Option<Focus>;
    /// A field's text, at most MAX_TEXT_CHARS of it. Never asked of a secure field
    /// or of a browser's.
    fn text(&self, sender: &str, path: &str) -> Option<Text>;
    /// How many characters a field holds, and nothing of what they are.
    fn count(&self, sender: &str, path: &str) -> Option<i64>;
    /// Whether a control reads FOCUSED now.
    fn is_focused(&self, sender: &str, path: &str) -> bool;
    /// Start receiving this application's focus and title changes, and its text
    /// changes where `text` is true.
    fn watch(&self, sender: &str, text: bool) -> Result<(), String>;
    /// Stop receiving what `watch` started.
    fn unwatch(&self, sender: &str, text: bool);
}

#[derive(Clone, Debug)]
struct Known {
    identity: AppIdentity,
    browser: bool,
}

#[derive(Clone, Debug)]
struct Focused {
    path: String,
    focus: Focus,
    /// A browser field's character count when it took the focus.
    baseline: Option<i64>,
}

#[derive(Clone, Debug)]
struct PendingValue {
    path: String,
    focus: Focus,
    started_ms: i64,
    due_ms: i64,
}

#[derive(Debug)]
struct Attached {
    sender: String,
    known: Known,
    frame: String,
    window_title: Option<String>,
    title: TitleDebounce,
    title_due_ms: Option<i64>,
    focused: Option<Focused>,
    value: Option<PendingValue>,
}

/// Gap reasons only this recorder authors, beside `capture`'s `GapReason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusGap {
    /// The accessibility bus went away (a logout, at-spi2-core restarting).
    BusLost,
    /// Events arrived faster than the recorder took them, and some were dropped.
    Overflow,
    /// A value was cut at MAX_TEXT_BYTES; its `char_len` still says how long it was.
    Truncated,
}

impl BusGap {
    pub fn as_str(self) -> &'static str {
        match self {
            BusGap::BusLost => "accessibility_bus_lost",
            BusGap::Overflow => "overflow",
            BusGap::Truncated => "truncated",
        }
    }
}

/// The recorder's state over one observation.
pub struct Recorder<B: Bus> {
    bus: B,
    emitter: Emitter,
    boot_id: String,
    seq: u64,
    apps: Vec<String>,
    known: HashMap<String, Option<Known>>,
    /// The application whose window is active, allowlisted or not.
    front: Option<String>,
    attached: Option<Attached>,
    /// The front application's window stopped being active; at the deadline, with no
    /// other of its windows active, it is no longer in front.
    deactivation: Option<(String, i64)>,
    gaps: Ledger<(String, &'static str)>,
    logged: Ledger<(String, &'static str)>,
    /// The last control a text change came from that did not read FOCUSED.
    unfocused: Option<String>,
}

impl<B: Bus> Recorder<B> {
    pub fn new(bus: B, emitter: Emitter, apps: Vec<String>, boot_id: String) -> Recorder<B> {
        Recorder {
            bus,
            emitter,
            boot_id,
            seq: 0,
            apps,
            known: HashMap::new(),
            front: None,
            attached: None,
            deactivation: None,
            gaps: Ledger::default(),
            logged: Ledger::default(),
            unfocused: None,
        }
    }

    /// The bus this recorder reads, for the engine that owns its connection.
    pub fn bus(&self) -> &B {
        &self.bus
    }

    /// Take one event.
    pub fn handle(&mut self, event: Event, now_ms: i64) {
        match event {
            Event::Active { sender, path, on } if on => self.activated(sender, path, now_ms),
            Event::Active { sender, path, .. } => self.deactivated(sender, path, now_ms),
            Event::Focused { sender, path, on } if on => self.focused(&sender, path, now_ms),
            Event::Focused { sender, path, .. } => self.unfocused(&sender, &path, now_ms),
            Event::TextChanged { sender, path } => self.text_changed(&sender, path, now_ms),
            Event::NameChanged { sender, path, name } => {
                self.name_changed(&sender, &path, name, now_ms)
            }
            Event::Gone { sender } => self.gone(&sender, now_ms),
        }
    }

    /// The earliest moment a timer is due, if one is set.
    pub fn due(&self) -> Option<i64> {
        let attached = self.attached.as_ref();
        [
            attached.and_then(|a| a.value.as_ref().map(|v| v.due_ms)),
            attached.and_then(|a| a.title_due_ms),
            self.deactivation.as_ref().map(|(_, due)| *due),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Fire every timer due by `now_ms`.
    pub fn tick(&mut self, now_ms: i64) {
        if self.value_due(now_ms) {
            self.flush_value(now_ms);
        }
        if self.title_due(now_ms) {
            self.flush_title();
        }
        if let Some((sender, due)) = self.deactivation.clone() {
            if due <= now_ms {
                self.deactivation = None;
                self.left_front(&sender, now_ms);
            }
        }
    }

    /// The observation ends (`observe_stop`): what is pending is said, and the
    /// application being watched is no longer watched.
    pub fn end(&mut self, now_ms: i64) {
        self.detach(now_ms);
        self.front = None;
        self.deactivation = None;
    }

    /// The bus went away: what is pending is said with what can still be read, the
    /// gap is announced, and the recorder starts again from nothing.
    pub fn lost(&mut self, now_ms: i64) {
        self.end(now_ms);
        self.known.clear();
        self.emit_bus_gap(BusGap::BusLost, now_ms, now_ms);
    }

    /// Events were dropped between `from_ms` and `now_ms`.
    pub fn overflowed(&mut self, from_ms: i64, now_ms: i64) {
        self.emit_bus_gap(BusGap::Overflow, from_ms, now_ms);
    }

    // --- activation ------------------------------------------------------------

    fn activated(&mut self, sender: String, path: String, now_ms: i64) {
        self.deactivation = None;
        if self.front.as_deref() == Some(sender.as_str()) {
            self.window_switched(&sender, path);
            return;
        }
        let previous = self
            .attached
            .as_ref()
            .and_then(|a| a.known.identity.bundle_id.clone());
        self.detach(now_ms);
        self.front = Some(sender.clone());
        let Some(known) = self.identify(&sender) else {
            return;
        };
        if !app_allowed(&known.identity.bundle_id, &self.apps) {
            return;
        }
        let mut extra = Map::new();
        insert_str(&mut extra, "prev_bundle_id", previous);
        self.emit("app.activated", Some(&known.identity), extra);
        self.attach(sender, path, known);
    }

    /// Another window of the application in front became active.
    fn window_switched(&mut self, sender: &str, path: String) {
        let same = self
            .attached
            .as_ref()
            .is_some_and(|a| a.sender == sender && a.frame != path);
        if !same {
            return;
        }
        self.flush_title();
        let title = self.bus.title(sender, &path);
        if let Some(attached) = self.attached.as_mut() {
            attached.frame = path;
            attached.window_title = title.clone().filter(|t| !t.is_empty());
        }
        self.emit_window_focused(title);
    }

    fn deactivated(&mut self, sender: String, path: String, now_ms: i64) {
        if self.front.as_deref() != Some(sender.as_str()) {
            return;
        }
        let other_window = self
            .attached
            .as_ref()
            .is_some_and(|a| a.sender == sender && a.frame != path);
        if other_window {
            return;
        }
        self.deactivation = Some((sender, now_ms + DEACTIVATION_GRACE_MS));
    }

    /// The application in front stopped being in front, and nothing on the bus took
    /// its place: the desktop, or an application that is not on the bus.
    fn left_front(&mut self, sender: &str, now_ms: i64) {
        if self.front.as_deref() != Some(sender) {
            return;
        }
        self.detach(now_ms);
        self.front = None;
    }

    fn attach(&mut self, sender: String, frame: String, known: Known) {
        if let Err(why) = self.bus.watch(&sender, !known.browser) {
            let name = known.identity.bundle_id.clone().unwrap_or_default();
            self.log_once(&name, "watch", &format!("{name}: not watched: {why}"));
            return;
        }
        let title = self.bus.title(&sender, &frame);
        self.attached = Some(Attached {
            sender,
            known,
            frame,
            window_title: title.clone().filter(|t| !t.is_empty()),
            title: TitleDebounce::default(),
            title_due_ms: None,
            focused: None,
            value: None,
        });
        self.emit_window_focused(title);
    }

    fn detach(&mut self, now_ms: i64) {
        if self.attached.is_none() {
            return;
        }
        self.flush_title();
        self.blur(now_ms);
        if let Some(attached) = self.attached.take() {
            self.bus.unwatch(&attached.sender, !attached.known.browser);
        }
    }

    fn gone(&mut self, sender: &str, now_ms: i64) {
        if self.attached.as_ref().is_some_and(|a| a.sender == sender) {
            self.detach(now_ms);
        }
        if self.front.as_deref() == Some(sender) {
            self.front = None;
        }
        if self.deactivation.as_ref().is_some_and(|(s, _)| s == sender) {
            self.deactivation = None;
        }
        self.known.remove(sender);
    }

    fn identify(&mut self, sender: &str) -> Option<Known> {
        if let Some(known) = self.known.get(sender) {
            return known.clone();
        }
        if self.known.len() >= MAX_KNOWN {
            self.known.clear();
        }
        let known = self.bus.application(sender).map(|app| Known {
            browser: is_browser(&app.name, app.program.as_deref(), app.toolkit.as_deref()),
            identity: AppIdentity {
                bundle_id: Some(app.name.clone()).filter(|name| !name.is_empty()),
                name: Some(app.name),
                pid: app.pid,
            },
        });
        self.known.insert(sender.to_string(), known.clone());
        known
    }

    // --- focus and values ------------------------------------------------------

    fn focused(&mut self, sender: &str, path: String, now_ms: i64) {
        if !self.watching(sender) {
            return;
        }
        // Blur is a boundary: the edit just left is said before the focus moves.
        self.blur(now_ms);
        self.unfocused = None;
        let focus = self.bus.focus(sender, &path);
        self.emit_focus_changed(focus.as_ref());
        let Some(focus) = focus.filter(|f| f.field != Field::Other) else {
            return;
        };
        let baseline = if self.counted_only(&focus) {
            self.bus.count(sender, &path)
        } else {
            None
        };
        if let Some(attached) = self.attached.as_mut() {
            attached.focused = Some(Focused {
                path,
                focus,
                baseline,
            });
        }
    }

    fn unfocused(&mut self, sender: &str, path: &str, now_ms: i64) {
        let focused_here = self.attached.as_ref().is_some_and(|a| {
            a.sender == sender && a.focused.as_ref().is_some_and(|f| f.path == path)
        });
        if focused_here {
            self.blur(now_ms);
        }
    }

    fn text_changed(&mut self, sender: &str, path: String, now_ms: i64) {
        self.adopt_focus(sender, &path, now_ms);
        // A browser's text is never taken, whatever arrives.
        let Some(focus) = self.focused_field(sender, &path) else {
            return;
        };
        let expired = self
            .attached
            .as_ref()
            .and_then(|a| a.value.as_ref())
            .filter(|pending| pending.path == path)
            .map(|pending| burst_expired(pending.started_ms, now_ms, VALUE_MAX_WAIT_MS));
        match expired {
            // A burst that never goes quiet still reports at the ceiling.
            Some(true) => self.flush_value(now_ms),
            Some(false) => {
                if let Some(pending) = self.attached.as_mut().and_then(|a| a.value.as_mut()) {
                    pending.due_ms = now_ms + VALUE_DEBOUNCE_MS;
                }
            }
            None => {
                self.flush_value(now_ms);
                if let Some(attached) = self.attached.as_mut() {
                    attached.value = Some(PendingValue {
                        path,
                        focus,
                        started_ms: now_ms,
                        due_ms: now_ms + VALUE_DEBOUNCE_MS,
                    });
                }
            }
        }
    }

    /// A field that had the focus before observation began sent no focus event:
    /// a text change from a control the recorder has not seen take the focus asks
    /// whether it reads FOCUSED, and one that does is taken as just focused. A
    /// control found not focused is not asked again until the focus moves.
    fn adopt_focus(&mut self, sender: &str, path: &str, now_ms: i64) {
        let Some(attached) = self.attached.as_ref() else {
            return;
        };
        let known = attached.focused.as_ref().is_some_and(|f| f.path == path);
        if attached.sender != sender || known || self.unfocused.as_deref() == Some(path) {
            return;
        }
        if self.bus.is_focused(sender, path) {
            self.focused(sender, path.to_string(), now_ms);
        } else {
            self.unfocused = Some(path.to_string());
        }
    }

    /// The focused field `path` is, in a watched application that is not a browser.
    fn focused_field(&self, sender: &str, path: &str) -> Option<Focus> {
        let attached = self.attached.as_ref()?;
        if attached.sender != sender {
            return None;
        }
        let focused = attached.focused.as_ref()?;
        (focused.path == path && !self.counted_only(&focused.focus)).then(|| focused.focus.clone())
    }

    /// Whether a field's text is never read, only its count kept: a browser's field
    /// (`is_browser`), a field inside a web document in any application, and a
    /// terminal, whose contents hold what no role marks as a password (a prompt's)
    /// and whatever a command printed.
    fn counted_only(&self, focus: &Focus) -> bool {
        let browser = self.attached.as_ref().is_some_and(|a| a.known.browser);
        browser || focus.web || focus.field == Field::Terminal
    }

    /// The focused field is left: its pending value is said, and a field whose
    /// text is never read says, if it changed, how many characters it holds.
    fn blur(&mut self, now_ms: i64) {
        self.flush_value(now_ms);
        let Some(attached) = self.attached.as_mut() else {
            return;
        };
        let Some(focused) = attached.focused.take() else {
            return;
        };
        let sender = attached.sender.clone();
        if self.counted_only(&focused.focus) {
            self.counted_field_left(&sender, focused, now_ms);
        }
    }

    fn counted_field_left(&mut self, sender: &str, focused: Focused, now_ms: i64) {
        let Some(count) = self
            .bus
            .count(sender, &focused.path)
            .filter(|count| Some(*count) != focused.baseline)
        else {
            return;
        };
        let Some(attached) = self.attached.as_ref() else {
            return;
        };
        let app = attached.known.identity.clone();
        let web = attached.known.browser || focused.focus.web;
        let mut extra = field_extras(&focused.focus);
        if focused.focus.field == Field::Secure {
            self.emit_secure(&app, extra, now_ms, now_ms);
            return;
        }
        extra.insert("char_len".into(), json!(count));
        extra.insert("content_withheld".into(), json!(true));
        if !web {
            self.emit("field.value", Some(&app), extra);
            return;
        }
        extra.insert("private_state".into(), json!("unknown"));
        insert_str(&mut extra, "browser_id", app.bundle_id.clone());
        extra.insert("window_ref".into(), json!(attached.frame));
        self.emit("field.value", Some(&app), extra);
        self.announce_private_unknown(&app, now_ms);
    }

    fn value_due(&self, now_ms: i64) -> bool {
        self.attached
            .as_ref()
            .and_then(|a| a.value.as_ref())
            .is_some_and(|v| v.due_ms <= now_ms)
    }

    /// Say the pending value, if there is one: the one way a value leaves.
    fn flush_value(&mut self, now_ms: i64) {
        let Some(attached) = self.attached.as_mut() else {
            return;
        };
        let Some(pending) = attached.value.take() else {
            return;
        };
        let app = attached.known.identity.clone();
        let sender = attached.sender.clone();
        let extra = field_extras(&pending.focus);
        if pending.focus.field == Field::Secure {
            self.emit_secure(&app, extra, pending.started_ms, now_ms);
            return;
        }
        let text = self.bus.text(&sender, &pending.path);
        self.emit_value(&app, extra, text, pending.started_ms, now_ms);
    }

    fn emit_value(
        &mut self,
        app: &AppIdentity,
        mut extra: Map<String, Value>,
        text: Option<Text>,
        started_ms: i64,
        now_ms: i64,
    ) {
        let Some(text) = text else {
            extra.insert("content_withheld".into(), json!(true));
            self.emit("field.value", Some(app), extra);
            return;
        };
        let (sent, cut) = truncate_text(&text.text);
        let clipped = cut || text.chars > char_len(&text.text);
        extra.insert("text".into(), json!(sent));
        extra.insert(
            "char_len".into(),
            json!(text.chars.max(char_len(&text.text))),
        );
        extra.insert("content_withheld".into(), json!(false));
        self.emit("field.value", Some(app), extra);
        if clipped {
            self.emit_bus_gap(BusGap::Truncated, started_ms, now_ms);
        }
    }

    /// A password field changed: the gap says so, and the value carries no text.
    fn emit_secure(
        &mut self,
        app: &AppIdentity,
        mut extra: Map<String, Value>,
        from_ms: i64,
        now_ms: i64,
    ) {
        let seq = self.next_seq();
        let frame = gap_frame(
            &self.boot_id,
            seq,
            GapReason::SecureInput,
            from_ms,
            now_ms,
            None,
        );
        self.emitter.emit_frame(&frame);
        extra.insert("content_withheld".into(), json!(true));
        self.emit("field.value", Some(app), extra);
    }

    // --- titles ----------------------------------------------------------------

    fn name_changed(&mut self, sender: &str, path: &str, name: Option<String>, now_ms: i64) {
        let on_frame = self
            .attached
            .as_ref()
            .is_some_and(|a| a.sender == sender && a.frame == path);
        if !on_frame {
            return;
        }
        let title = name
            .or_else(|| self.bus.title(sender, path))
            .unwrap_or_default();
        let Some(attached) = self.attached.as_mut() else {
            return;
        };
        attached.window_title = Some(title.clone()).filter(|t| !t.is_empty());
        match attached.title.observe(title, now_ms, TITLE_MAX_WAIT_MS) {
            SettleWhen::Immediately => self.flush_title(),
            SettleWhen::AfterDebounce => attached.title_due_ms = Some(now_ms + TITLE_DEBOUNCE_MS),
        }
    }

    fn title_due(&self, now_ms: i64) -> bool {
        self.attached
            .as_ref()
            .and_then(|a| a.title_due_ms)
            .is_some_and(|due| due <= now_ms)
    }

    fn flush_title(&mut self) {
        let Some(attached) = self.attached.as_mut() else {
            return;
        };
        attached.title_due_ms = None;
        let Some(title) = attached.title.settle() else {
            return;
        };
        let app = attached.known.identity.clone();
        let mut extra = Map::new();
        if !title.is_empty() {
            extra.insert("window_title".into(), json!(title));
        }
        self.emit("window.title_changed", Some(&app), extra);
    }

    // --- frames ----------------------------------------------------------------

    fn watching(&self, sender: &str) -> bool {
        self.attached.as_ref().is_some_and(|a| a.sender == sender)
    }

    fn emit_window_focused(&mut self, title: Option<String>) {
        let Some(attached) = self.attached.as_mut() else {
            return;
        };
        attached.title.seed_emitted(title.clone());
        let app = attached.known.identity.clone();
        let mut extra = Map::new();
        insert_str(&mut extra, "window_title", title.filter(|t| !t.is_empty()));
        self.emit("window.focused", Some(&app), extra);
    }

    fn emit_focus_changed(&mut self, focus: Option<&Focus>) {
        let Some(attached) = self.attached.as_ref() else {
            return;
        };
        let app = attached.known.identity.clone();
        let mut extra = focus.map(field_extras).unwrap_or_default();
        insert_str(&mut extra, "window_title", attached.window_title.clone());
        self.emit("focus.changed", Some(&app), extra);
    }

    /// The standing fact that a browser's posture cannot be read, once per browser
    /// per session, carrying the browser.
    fn announce_private_unknown(&mut self, app: &AppIdentity, now_ms: i64) {
        let name = app.bundle_id.clone().unwrap_or_default();
        if !self.gaps.announce((name, "private_unknown")) {
            return;
        }
        let seq = self.next_seq();
        let frame = gap_frame(
            &self.boot_id,
            seq,
            GapReason::PrivateUnknown,
            now_ms,
            now_ms,
            Some(app),
        );
        self.emitter.emit_frame(&frame);
    }

    fn emit_bus_gap(&mut self, reason: BusGap, from_ms: i64, to_ms: i64) {
        let mut extra = Map::new();
        extra.insert("gap_reason".into(), json!(reason.as_str()));
        extra.insert("gap_from_ts".into(), json!(from_ms));
        extra.insert("gap_to_ts".into(), json!(to_ms));
        self.emit("observer.gap", None, extra);
    }

    fn emit(&mut self, kind: &str, app: Option<&AppIdentity>, extra: Map<String, Value>) {
        let seq = self.next_seq();
        let frame = event_frame(&self.boot_id, seq, kind, app, extra);
        self.emitter.emit_frame(&frame);
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn log_once(&mut self, key: &str, topic: &'static str, message: &str) {
        if self.logged.announce((key.to_string(), topic)) {
            eprintln!("compux: capture: {message}");
        }
    }
}

/// The `role` and `field_label` every field frame carries.
fn field_extras(focus: &Focus) -> Map<String, Value> {
    let mut extra = Map::new();
    extra.insert("role".into(), json!(focus.role));
    insert_str(&mut extra, "field_label", focus.label.clone());
    extra
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const EDITOR: &str = ":1.20";
    const OTHER: &str = ":1.21";
    const BROWSER: &str = ":1.22";

    /// A scripted bus: what each read answers, and every watch it was asked for.
    #[derive(Default)]
    struct Script {
        apps: HashMap<String, Application>,
        titles: HashMap<(String, String), String>,
        focus: HashMap<(String, String), Focus>,
        text: RefCell<HashMap<(String, String), Text>>,
        counts: RefCell<HashMap<(String, String), i64>>,
        focused_now: RefCell<Vec<(String, String)>>,
        watched: RefCell<Vec<String>>,
        reads: RefCell<Vec<String>>,
    }

    impl Script {
        fn key(sender: &str, path: &str) -> (String, String) {
            (sender.to_string(), path.to_string())
        }
    }

    impl Bus for &Script {
        fn application(&self, sender: &str) -> Option<Application> {
            self.reads
                .borrow_mut()
                .push(format!("application {sender}"));
            self.apps.get(sender).cloned()
        }
        fn title(&self, sender: &str, path: &str) -> Option<String> {
            self.titles.get(&Script::key(sender, path)).cloned()
        }
        fn focus(&self, sender: &str, path: &str) -> Option<Focus> {
            self.focus.get(&Script::key(sender, path)).cloned()
        }
        fn text(&self, sender: &str, path: &str) -> Option<Text> {
            self.reads
                .borrow_mut()
                .push(format!("text {sender} {path}"));
            self.text.borrow().get(&Script::key(sender, path)).cloned()
        }
        fn count(&self, sender: &str, path: &str) -> Option<i64> {
            self.reads
                .borrow_mut()
                .push(format!("count {sender} {path}"));
            self.counts
                .borrow()
                .get(&Script::key(sender, path))
                .copied()
        }
        fn is_focused(&self, sender: &str, path: &str) -> bool {
            self.reads
                .borrow_mut()
                .push(format!("focused {sender} {path}"));
            self.focused_now
                .borrow()
                .contains(&Script::key(sender, path))
        }
        fn watch(&self, sender: &str, text: bool) -> Result<(), String> {
            self.watched
                .borrow_mut()
                .push(format!("watch {sender} text={text}"));
            Ok(())
        }
        fn unwatch(&self, sender: &str, text: bool) {
            self.watched
                .borrow_mut()
                .push(format!("unwatch {sender} text={text}"));
        }
    }

    fn app(name: &str, pid: i32, program: &str) -> Application {
        Application {
            name: name.to_string(),
            pid,
            program: Some(program.to_string()),
            toolkit: Some("GTK".to_string()),
        }
    }

    fn made_with(app: Application, toolkit: Option<&str>) -> Application {
        Application {
            toolkit: toolkit.map(str::to_string),
            ..app
        }
    }

    fn field(role: &str, field: Field, label: &str) -> Focus {
        Focus {
            role: role.to_string(),
            field,
            label: Some(label.to_string()),
            web: false,
        }
    }

    fn script() -> Script {
        let mut script = Script::default();
        script
            .apps
            .insert(EDITOR.into(), app("Text Editor", 41, "gnome-text-editor"));
        script.apps.insert(OTHER.into(), app("Mail", 42, "mail"));
        script.apps.insert(
            BROWSER.into(),
            made_with(app("Firefox", 43, "firefox"), Some("Gecko")),
        );
        for (sender, title) in [(EDITOR, "notes.txt"), (OTHER, "Inbox"), (BROWSER, "Docs")] {
            script
                .titles
                .insert(Script::key(sender, "/w1"), title.to_string());
        }
        script
            .titles
            .insert(Script::key(EDITOR, "/w2"), "todo.txt".to_string());
        for sender in [EDITOR, OTHER, BROWSER] {
            script.focus.insert(
                Script::key(sender, "/f"),
                field("entry", Field::Editable, "Search"),
            );
            script.focus.insert(
                Script::key(sender, "/p"),
                field("password text", Field::Secure, "Password"),
            );
            script.focus.insert(
                Script::key(sender, "/b"),
                field("push button", Field::Other, "Save"),
            );
        }
        script
    }

    fn recorder(script: &Script) -> (Recorder<&Script>, Emitter) {
        recorder_of(script, &["Text Editor", "Firefox"])
    }

    fn recorder_of<'s>(script: &'s Script, apps: &[&str]) -> (Recorder<&'s Script>, Emitter) {
        let emitter = Emitter::capturing();
        let apps = apps.iter().map(|app| app.to_string()).collect();
        let recorder = Recorder::new(script, emitter.clone(), apps, "boot-test".to_string());
        (recorder, emitter)
    }

    fn active(sender: &str, path: &str, on: bool) -> Event {
        Event::Active {
            sender: sender.into(),
            path: path.into(),
            on,
        }
    }

    fn focused(sender: &str, path: &str) -> Event {
        Event::Focused {
            sender: sender.into(),
            path: path.into(),
            on: true,
        }
    }

    fn typed(sender: &str, path: &str) -> Event {
        Event::TextChanged {
            sender: sender.into(),
            path: path.into(),
        }
    }

    fn kinds(emitter: &Emitter) -> Vec<String> {
        emitter
            .captured()
            .iter()
            .map(|frame| match frame["gap_reason"].as_str() {
                Some(reason) => format!("gap:{reason}"),
                None => frame["kind"].as_str().unwrap_or("").to_string(),
            })
            .collect()
    }

    fn last(emitter: &Emitter, kind: &str) -> Value {
        emitter
            .captured()
            .into_iter()
            .rfind(|frame| frame["kind"] == json!(kind))
            .unwrap_or_else(|| panic!("no {kind} frame"))
    }

    // An allowlisted application in front is announced and watched; its window is the
    // one it is focused in, and its identity is its name on the bus.
    #[test]
    fn an_allowlisted_application_in_front_is_announced_and_watched() {
        let script = script();
        let (mut recorder, emitter) = recorder(&script);

        recorder.handle(active(EDITOR, "/w1", true), 0);

        assert_eq!(kinds(&emitter), vec!["app.activated", "window.focused"]);
        let frame = last(&emitter, "window.focused");
        assert_eq!(frame["app"]["bundle_id"], json!("Text Editor"));
        assert_eq!(frame["app"]["pid"], json!(41));
        assert_eq!(frame["window_title"], json!("notes.txt"));
        assert_eq!(*script.watched.borrow(), vec!["watch :1.20 text=true"]);
    }

    // Default-deny: an application not on the list is never watched and nothing of
    // it is emitted, whatever its events say; switching back to an allowlisted one
    // has no previous application to name.
    #[test]
    fn an_application_not_on_the_list_is_never_watched_and_says_nothing() {
        let script = script();
        let (mut recorder, emitter) = recorder(&script);

        recorder.handle(active(OTHER, "/w1", true), 0);
        recorder.handle(focused(OTHER, "/f"), 10);
        recorder.handle(typed(OTHER, "/f"), 20);
        recorder.tick(10_000);

        assert!(emitter.captured().is_empty(), "{:?}", emitter.captured());
        assert!(script.watched.borrow().is_empty());
        assert!(!script.reads.borrow().iter().any(|r| r.starts_with("text")));
    }

    // Switching from one allowlisted application to another names the one left, and
    // the one left is no longer watched.
    #[test]
    fn a_switch_names_the_application_left_and_stops_watching_it() {
        let script = script();
        let (mut recorder, emitter) = recorder(&script);

        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(active(BROWSER, "/w1", true), 10);

        let frame = last(&emitter, "app.activated");
        assert_eq!(frame["app"]["bundle_id"], json!("Firefox"));
        assert_eq!(frame["prev_bundle_id"], json!("Text Editor"));
        assert_eq!(
            *script.watched.borrow(),
            vec![
                "watch :1.20 text=true",
                "unwatch :1.20 text=true",
                "watch :1.22 text=false"
            ]
        );
    }

    // A typed value is read once the edits settle, as one frame, with its length.
    #[test]
    fn a_typed_value_is_read_once_it_settles() {
        let script = script();
        script.text.borrow_mut().insert(
            Script::key(EDITOR, "/f"),
            Text {
                text: "groceries".into(),
                chars: 9,
            },
        );
        let (mut recorder, emitter) = recorder(&script);

        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(focused(EDITOR, "/f"), 10);
        recorder.handle(typed(EDITOR, "/f"), 20);
        recorder.handle(typed(EDITOR, "/f"), 300);
        assert_eq!(recorder.due(), Some(300 + VALUE_DEBOUNCE_MS));
        recorder.tick(500);
        assert!(!kinds(&emitter).contains(&"field.value".to_string()));
        recorder.tick(300 + VALUE_DEBOUNCE_MS);

        let frame = last(&emitter, "field.value");
        assert_eq!(frame["text"], json!("groceries"));
        assert_eq!(frame["char_len"], json!(9));
        assert_eq!(frame["content_withheld"], json!(false));
        assert_eq!(frame["role"], json!("entry"));
        assert_eq!(frame["field_label"], json!("Search"));
        let reads: Vec<String> = script.reads.borrow().clone();
        assert_eq!(reads.iter().filter(|r| r.starts_with("text")).count(), 1);
    }

    // A field that already had the focus when observation began sends no focus
    // event: its first text change finds it focused, and it is recorded as if it
    // had just taken the focus.
    #[test]
    fn a_field_focused_before_observation_is_recorded_from_its_first_change() {
        let script = script();
        scripted_text(&script, EDITOR, "/f", "groceries");
        script
            .focused_now
            .borrow_mut()
            .push(Script::key(EDITOR, "/f"));
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(typed(EDITOR, "/f"), 10);
        recorder.tick(10_000);

        assert_eq!(
            last(&emitter, "focus.changed")["field_label"],
            json!("Search")
        );
        assert_eq!(last(&emitter, "field.value")["text"], json!("groceries"));
    }

    // A value that never goes quiet still reports at the ceiling.
    #[test]
    fn a_value_that_never_settles_reports_at_the_ceiling() {
        let script = script();
        script.text.borrow_mut().insert(
            Script::key(EDITOR, "/f"),
            Text {
                text: "x".into(),
                chars: 1,
            },
        );
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(focused(EDITOR, "/f"), 0);

        let mut at = 0;
        while at <= VALUE_MAX_WAIT_MS {
            recorder.handle(typed(EDITOR, "/f"), at);
            at += 100;
        }

        assert!(kinds(&emitter).contains(&"field.value".to_string()));
    }

    // Leaving a field says its edit at once: blur is a boundary.
    #[test]
    fn leaving_a_field_says_its_edit_at_once() {
        let script = script();
        script.text.borrow_mut().insert(
            Script::key(EDITOR, "/f"),
            Text {
                text: "sent".into(),
                chars: 4,
            },
        );
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(focused(EDITOR, "/f"), 0);
        recorder.handle(typed(EDITOR, "/f"), 10);
        recorder.handle(focused(EDITOR, "/b"), 20);

        assert_eq!(
            kinds(&emitter),
            vec![
                "app.activated",
                "window.focused",
                "focus.changed",
                "field.value",
                "focus.changed"
            ]
        );
    }

    // A password field is never read: its change is a gap and a withheld value.
    #[test]
    fn a_password_field_is_never_read() {
        let script = script();
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(focused(EDITOR, "/p"), 0);
        recorder.handle(typed(EDITOR, "/p"), 10);
        recorder.tick(10_000);

        assert!(!script.reads.borrow().iter().any(|r| r.starts_with("text")));
        let frame = last(&emitter, "field.value");
        assert_eq!(frame["content_withheld"], json!(true));
        assert!(frame.get("text").is_none());
        assert!(kinds(&emitter).contains(&"gap:secure_input".to_string()));
        let gap = emitter
            .captured()
            .into_iter()
            .find(|f| f["gap_reason"] == json!("secure_input"))
            .expect("the gap");
        assert!(gap.get("app").is_none(), "a secure gap is app-less");
    }

    // In a browser no text is ever read, even if a text change arrives: a changed
    // field says only how many characters it holds, withheld, once it is left, and
    // the browser's posture gap is announced once per session.
    #[test]
    fn a_browser_field_says_only_its_count_and_its_gap_once() {
        let script = script();
        script
            .counts
            .borrow_mut()
            .insert(Script::key(BROWSER, "/f"), 0);
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(BROWSER, "/w1", true), 0);
        recorder.handle(focused(BROWSER, "/f"), 0);
        recorder.handle(typed(BROWSER, "/f"), 10);
        script
            .counts
            .borrow_mut()
            .insert(Script::key(BROWSER, "/f"), 12);
        recorder.handle(focused(BROWSER, "/b"), 20);
        recorder.handle(focused(BROWSER, "/f"), 30);
        script
            .counts
            .borrow_mut()
            .insert(Script::key(BROWSER, "/f"), 20);
        recorder.handle(focused(BROWSER, "/b"), 40);
        recorder.tick(10_000);

        assert!(!script.reads.borrow().iter().any(|r| r.starts_with("text")));
        let values: Vec<Value> = emitter
            .captured()
            .into_iter()
            .filter(|f| f["kind"] == json!("field.value"))
            .collect();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["char_len"], json!(12));
        assert_eq!(values[0]["content_withheld"], json!(true));
        assert_eq!(values[0]["private_state"], json!("unknown"));
        assert_eq!(values[0]["browser_id"], json!("Firefox"));
        assert!(values[0].get("text").is_none());
        let gaps = kinds(&emitter)
            .into_iter()
            .filter(|k| k == "gap:private_unknown")
            .count();
        assert_eq!(gaps, 1, "announced once per browser per session");
    }

    // A browser field left unchanged says nothing.
    #[test]
    fn an_unchanged_browser_field_says_nothing() {
        let script = script();
        script
            .counts
            .borrow_mut()
            .insert(Script::key(BROWSER, "/f"), 3);
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(BROWSER, "/w1", true), 0);
        recorder.handle(focused(BROWSER, "/f"), 0);
        recorder.handle(focused(BROWSER, "/b"), 20);

        assert!(!kinds(&emitter).contains(&"field.value".to_string()));
    }

    // A browser under a name the table does not know is still a browser by its
    // program, and by its web engine whatever it is called; an application whose
    // engine cannot be read is taken as one, so withholding never rests on a list.
    #[test]
    fn a_browser_is_known_by_its_name_its_program_or_its_engine() {
        let gtk = Some("GTK");
        assert!(is_browser("Firefox", None, gtk));
        assert!(is_browser("firefox", None, gtk));
        assert!(is_browser("Some Fork", Some("firefox-bin"), gtk));
        assert!(is_browser("Fennec Fork", Some("fennec"), Some("Gecko")));
        assert!(is_browser(
            "Brave Nightly Plus",
            Some("brave-plus"),
            Some("Chromium")
        ));
        assert!(is_browser("Code", Some("code"), Some("Chromium")));
        assert!(is_browser("Mystery", Some("mystery"), None));
        assert!(!is_browser("Terminal", Some("gnome-terminal-server"), gtk));
        assert!(!is_browser("Dolphin", Some("dolphin"), Some("Qt")));
    }

    /// The text a field held, for a test that must never see it read.
    fn scripted_text(script: &Script, sender: &str, path: &str, text: &str) {
        script.text.borrow_mut().insert(
            Script::key(sender, path),
            Text {
                text: text.into(),
                chars: char_len(text),
            },
        );
    }

    /// Focus `path`, type into it, change its count, and leave it.
    fn typed_and_left(recorder: &mut Recorder<&Script>, script: &Script, sender: &str, path: &str) {
        script
            .counts
            .borrow_mut()
            .insert(Script::key(sender, path), 0);
        recorder.handle(focused(sender, path), 0);
        recorder.handle(typed(sender, path), 10);
        recorder.tick(10_000);
        script
            .counts
            .borrow_mut()
            .insert(Script::key(sender, path), 7);
        recorder.handle(focused(sender, "/b"), 20_000);
        recorder.tick(40_000);
    }

    fn texts_read(script: &Script) -> usize {
        script
            .reads
            .borrow()
            .iter()
            .filter(|r| r.starts_with("text"))
            .count()
    }

    // A browser on no list, named anything, is a browser by its web engine: an
    // allowlisted Gecko or Chromium application (a renamed fork, an Electron
    // browser) never has its text read, and says only its count.
    #[test]
    fn an_application_on_a_web_engine_never_has_its_text_read() {
        for (name, engine) in [("Fennec Fork", "Gecko"), ("Chat", "Chromium")] {
            let mut script = script();
            script.apps.insert(
                ":1.30".into(),
                made_with(app(name, 50, "fork"), Some(engine)),
            );
            script
                .titles
                .insert(Script::key(":1.30", "/w1"), "Page".to_string());
            script.focus.insert(
                Script::key(":1.30", "/f"),
                field("entry", Field::Editable, "Search"),
            );
            scripted_text(&script, ":1.30", "/f", "my secret search");
            let (mut recorder, emitter) = recorder_of(&script, &[name]);
            recorder.handle(active(":1.30", "/w1", true), 0);
            typed_and_left(&mut recorder, &script, ":1.30", "/f");

            assert_eq!(texts_read(&script), 0, "{name}: text was read");
            let value = last(&emitter, "field.value");
            assert_eq!(value["content_withheld"], json!(true), "{name}");
            assert_eq!(value["browser_id"], json!(name));
            assert!(value.get("text").is_none());
            assert!(*script.watched.borrow() == vec![format!("watch :1.30 text=false")]);
        }
    }

    // An application whose toolkit could not be read is not known not to be a web
    // engine, so its text is withheld too.
    #[test]
    fn an_application_whose_engine_cannot_be_read_never_has_its_text_read() {
        let mut script = script();
        script.apps.insert(
            EDITOR.into(),
            made_with(app("Text Editor", 41, "gnome-text-editor"), None),
        );
        scripted_text(&script, EDITOR, "/f", "draft");
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        typed_and_left(&mut recorder, &script, EDITOR, "/f");

        assert_eq!(texts_read(&script), 0);
        assert_eq!(
            last(&emitter, "field.value")["content_withheld"],
            json!(true)
        );
    }

    // A field inside a web document in any application (a web view in a native
    // one) is a browser's field: never read, its count only.
    #[test]
    fn a_field_inside_a_web_document_never_has_its_text_read() {
        let mut script = script();
        let mut web = field("entry", Field::Editable, "Comment");
        web.web = true;
        script.focus.insert(Script::key(EDITOR, "/web"), web);
        scripted_text(&script, EDITOR, "/web", "a web comment");
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        typed_and_left(&mut recorder, &script, EDITOR, "/web");

        assert_eq!(texts_read(&script), 0);
        let value = last(&emitter, "field.value");
        assert_eq!(value["content_withheld"], json!(true));
        assert_eq!(value["char_len"], json!(7));
        assert!(value.get("text").is_none());
    }

    // A terminal's contents are what a prompt's password (which no role marks) and a
    // command's output are made of, so they are never read: a terminal left after
    // it changed says only how many characters it holds, and nothing about a
    // browser, since it is not one.
    #[test]
    fn a_terminal_never_has_its_text_read() {
        let mut script = script();
        script.focus.insert(
            Script::key(EDITOR, "/t"),
            field("terminal", Field::Terminal, "Terminal"),
        );
        scripted_text(&script, EDITOR, "/t", "$ sudo -k\n[sudo] password: hunter2");
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        typed_and_left(&mut recorder, &script, EDITOR, "/t");

        assert_eq!(texts_read(&script), 0);
        let value = last(&emitter, "field.value");
        assert_eq!(value["role"], json!("terminal"));
        assert_eq!(value["content_withheld"], json!(true));
        assert_eq!(value["char_len"], json!(7));
        assert!(value.get("text").is_none());
        assert!(value.get("browser_id").is_none());
        assert!(value.get("private_state").is_none());
        assert!(!kinds(&emitter).contains(&"gap:private_unknown".to_string()));
    }

    // A window title settles before it is said, and the title a window was focused
    // with is not said a second time.
    #[test]
    fn a_title_settles_and_is_never_said_twice() {
        let script = script();
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        let renamed = |name: &str| Event::NameChanged {
            sender: EDITOR.into(),
            path: "/w1".into(),
            name: Some(name.into()),
        };

        recorder.handle(renamed("notes.txt"), 10);
        recorder.tick(10 + TITLE_DEBOUNCE_MS);
        recorder.handle(renamed("notes.txt *"), 2_000);
        recorder.handle(renamed("notes.txt — saved"), 2_100);
        recorder.tick(2_100 + TITLE_DEBOUNCE_MS);

        let titles: Vec<Value> = emitter
            .captured()
            .into_iter()
            .filter(|f| f["kind"] == json!("window.title_changed"))
            .map(|f| f["window_title"].clone())
            .collect();
        assert_eq!(titles, vec![json!("notes.txt — saved")]);
    }

    // A name change on anything but the active window is not a title.
    #[test]
    fn a_name_change_off_the_window_is_not_a_title() {
        let script = script();
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(
            Event::NameChanged {
                sender: EDITOR.into(),
                path: "/f".into(),
                name: Some("renamed".into()),
            },
            10,
        );
        recorder.tick(10_000);

        assert!(!kinds(&emitter).contains(&"window.title_changed".to_string()));
    }

    // Moving between two windows of the same application is a window switch, not an
    // application switch, in whichever order the two events arrive.
    #[test]
    fn a_window_switch_inside_an_application_is_not_an_application_switch() {
        let script = script();
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(active(EDITOR, "/w1", false), 10);
        recorder.handle(active(EDITOR, "/w2", true), 20);
        recorder.tick(1_000);

        assert_eq!(
            kinds(&emitter),
            vec!["app.activated", "window.focused", "window.focused"]
        );
        assert_eq!(
            last(&emitter, "window.focused")["window_title"],
            json!("todo.txt")
        );
        assert_eq!(*script.watched.borrow(), vec!["watch :1.20 text=true"]);
    }

    // A window that stops being active with nothing on the bus taking its place
    // leaves the application unwatched once the grace has passed.
    #[test]
    fn an_application_left_for_nothing_on_the_bus_is_unwatched() {
        let script = script();
        let (mut recorder, _emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(active(EDITOR, "/w1", false), 10);
        recorder.tick(10 + DEACTIVATION_GRACE_MS);

        assert_eq!(
            *script.watched.borrow(),
            vec!["watch :1.20 text=true", "unwatch :1.20 text=true"]
        );
        assert_eq!(recorder.due(), None);
    }

    // An application that leaves the bus is unwatched and forgotten, and its pending
    // edit is said with what can still be read.
    #[test]
    fn an_application_that_leaves_the_bus_is_unwatched_and_forgotten() {
        let script = script();
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(focused(EDITOR, "/f"), 0);
        recorder.handle(typed(EDITOR, "/f"), 10);
        recorder.handle(
            Event::Gone {
                sender: EDITOR.into(),
            },
            20,
        );
        recorder.handle(active(EDITOR, "/w1", true), 30);

        let frame = last(&emitter, "field.value");
        assert_eq!(
            frame["content_withheld"],
            json!(true),
            "nothing could be read"
        );
        let asked = script
            .reads
            .borrow()
            .iter()
            .filter(|r| r.starts_with("application"))
            .count();
        assert_eq!(asked, 2, "a returning connection is read again");
    }

    // A text change from a control other than the focused field is not a value.
    #[test]
    fn a_text_change_off_the_focused_field_is_not_a_value() {
        let script = script();
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(focused(EDITOR, "/f"), 0);
        recorder.handle(typed(EDITOR, "/label"), 10);
        recorder.tick(10_000);

        assert!(!kinds(&emitter).contains(&"field.value".to_string()));
    }

    // A value read past what was taken says so with a gap, and keeps its full length.
    #[test]
    fn a_value_cut_short_says_so() {
        let script = script();
        script.text.borrow_mut().insert(
            Script::key(EDITOR, "/f"),
            Text {
                text: "a".repeat(10),
                chars: 90_000,
            },
        );
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(focused(EDITOR, "/f"), 0);
        recorder.handle(typed(EDITOR, "/f"), 10);
        recorder.tick(10_000);

        assert_eq!(last(&emitter, "field.value")["char_len"], json!(90_000));
        assert!(kinds(&emitter).contains(&"gap:truncated".to_string()));
    }

    // Losing the bus says what was pending, announces the gap, and forgets every
    // application.
    #[test]
    fn losing_the_bus_says_what_was_pending_then_the_gap() {
        let script = script();
        script.text.borrow_mut().insert(
            Script::key(EDITOR, "/f"),
            Text {
                text: "draft".into(),
                chars: 5,
            },
        );
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(focused(EDITOR, "/f"), 0);
        recorder.handle(typed(EDITOR, "/f"), 10);
        recorder.lost(20);

        let kinds = kinds(&emitter);
        assert_eq!(
            kinds[kinds.len() - 2..],
            [
                "field.value".to_string(),
                "gap:accessibility_bus_lost".to_string()
            ]
        );
        assert_eq!(recorder.due(), None);
    }

    // Every frame is one observation's: the same boot and increasing sequence numbers.
    #[test]
    fn every_frame_carries_the_boot_and_an_increasing_sequence() {
        let script = script();
        let (mut recorder, emitter) = recorder(&script);
        recorder.handle(active(EDITOR, "/w1", true), 0);
        recorder.handle(focused(EDITOR, "/b"), 0);

        let frames = emitter.captured();
        let seqs: Vec<u64> = frames.iter().map(|f| f["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert!(frames.iter().all(|f| f["boot_id"] == json!("boot-test")));
    }
}
