//! Computer history's recorder on the accessibility bus (M97 stage 4): the I/O
//! behind `recorder::Recorder`, which decides every frame.
//!
//! `observe_start` opens a connection of its own to the session's accessibility
//! bus, says which events it listens for (`RegisterEvent`, sent as `(sas)` like the
//! reader's), and asks the bus for two things from every application: that a window
//! became or stopped being active, a state change that carries no text, and that an
//! application left the bus. Everything else (focus, titles and typed text) is
//! asked of the one active allowlisted application alone, by match rules that name
//! its connection, added when it comes in front and removed when it leaves. A
//! browser's rules leave out its text changes, whose signals carry the text itself.
//! So nothing an application the owner did not list types or shows reaches this
//! process.
//!
//! Two threads, so that a read the recorder waits on never stops the bus being read:
//! the pump takes every signal off the connection into a bounded queue (one that
//! is full drops the signal, and the drops are said as `gap{overflow}`), and the
//! observer takes them off the queue into the recorder, whose reads are each held
//! to OBSERVER_READ_TIMEOUT. `observe_stop` raises a flag the observer sees within
//! STOP_TICK: it ends what is pending, closes the connection, which ends the pump,
//! and both threads are joined.
//!
//! A bus that goes away (a logout, at-spi2-core restarting) is said as
//! `gap{accessibility_bus_lost}`. The observer then asks for the bus again every
//! RECONNECT_EVERY, at most RECONNECT_TRIES times, and stops observing once they
//! are spent.
//!
//! The rules M86 carries hold here too: no `Action.GetActions`, no
//! `Properties.GetAll` (every property is one `Get`, an optional interface's only
//! after `GetInterfaces` lists it), and nothing changes a setting of the person's
//! session: an application whose toolkit publishes no tree is not switched on, and
//! records nothing.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde::Serialize;
use zbus::message::Type;
use zbus::zvariant::{DynamicDeserialize, DynamicType, OwnedObjectPath, OwnedValue};
use zbus::{Connection, Message, MessageStream};

use super::vocabulary::{self, ACTIVE, EDITABLE, PASSWORD_TEXT, TERMINAL};

/// `ATSPI_ROLE_DOCUMENT_WEB`: a web page, in a browser or in any application's web
/// view.
const DOCUMENT_WEB: u32 = 95;
/// How far up a focused field's ancestry is read for a web document; past it, the
/// field is taken as inside one.
const MAX_ANCESTRY: usize = 64;
use crate::capture::{now_ms, Emitter};
use crate::recorder::{Application, Bus, Event, Field, Focus, Recorder, Text, MAX_TEXT_CHARS};

const EVENT_OBJECT: &str = "org.a11y.atspi.Event.Object";
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const TEXT_INTERFACE: &str = "org.a11y.atspi.Text";
const APPLICATION_INTERFACE: &str = "org.a11y.atspi.Application";
const NULL_PATH: &str = "/org/a11y/atspi/null";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const BUS_DAEMON: &str = "org.freedesktop.DBus";
const BUS_DAEMON_PATH: &str = "/org/freedesktop/DBus";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const REGISTRY_PATH: &str = "/org/a11y/atspi/registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";

/// The events the recorder says it listens for. A toolkit's bridge sends only the
/// events some client registered, and Chromium reports its ACTIVE window only once
/// `window:activate` is registered.
pub const REGISTERED: [&str; 5] = [
    "object:state-changed:active",
    "object:state-changed:focused",
    "object:text-changed",
    "object:property-change:accessible-name",
    "window:activate",
];

/// What is asked of every application: a window's activation, which carries no
/// text, and a connection leaving the bus.
pub const EVERY_APPLICATION: [&str; 2] = [
    "type='signal',interface='org.a11y.atspi.Event.Object',member='StateChanged',arg0='active'",
    "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged'",
];

/// Signals the pump holds for the observer at most; past it they are dropped and
/// counted.
const QUEUE: usize = 1024;
/// How often the observer looks at the stop flag while nothing is due.
const STOP_TICK: Duration = Duration::from_millis(250);
/// How long `observe_start` waits for the observer to say it is running.
const READY_WAIT: Duration = Duration::from_secs(5);
/// What each of the recorder's reads, and each subscription, is held to.
#[cfg(not(test))]
const OBSERVER_READ_TIMEOUT: Duration = Duration::from_secs(1);
/// After the bus is lost: one try every RECONNECT_EVERY, at most RECONNECT_TRIES
/// (an hour), then the observer stops.
const RECONNECT_EVERY: Duration = Duration::from_secs(5);
const RECONNECT_TRIES: u32 = 720;
/// Bounds of the look for the active window at start.
const MAX_APPLICATIONS: usize = 256;
const MAX_WINDOWS: usize = 64;

// --- the session ------------------------------------------------------------

/// The running observation: its stop flag and its thread.
struct Session {
    thread: JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// Start observing `apps` (the allowlist, by name on the bus), each frame through
/// `emitter`. `Ok` once the observer runs; `Err` with why it could not.
pub fn start(apps: Vec<String>, emitter: Emitter) -> Result<(), String> {
    let mut guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return Err("capture already running".to_string());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (ready, started) = mpsc::channel::<Result<(), String>>();
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("compux-observe".into())
            .spawn(move || observe(apps, emitter, &stop, ready))
            .map_err(|e| format!("failed to spawn observer thread: {e}"))?
    };
    match started.recv_timeout(READY_WAIT) {
        Ok(Ok(())) => {
            *guard = Some(Session { thread, stop });
            Ok(())
        }
        Ok(Err(why)) => {
            let _ = thread.join();
            Err(why)
        }
        Err(_) => {
            stop.store(true, Ordering::SeqCst);
            let _ = thread.join();
            Err("the observer did not start".to_string())
        }
    }
}

/// Stop observing and join the observer. A no-op when nothing runs.
pub fn stop() {
    let session = SESSION.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(session) = session {
        session.stop.store(true, Ordering::SeqCst);
        let _ = session.thread.join();
    }
}

fn observe(
    apps: Vec<String>,
    emitter: Emitter,
    stop: &AtomicBool,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let (link, active) = match Link::open() {
        Ok(opened) => opened,
        Err(why) => {
            let _ = ready.send(Err(why));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    let bus = LiveBus::over(link.connection.clone());
    let mut recorder = Recorder::new(bus, emitter, apps, boot_id());
    let mut link = link;
    begin(&mut recorder, active);
    loop {
        match run(&mut recorder, &link, stop) {
            Ended::Stopped => {
                recorder.end(now_ms());
                link.close();
                return;
            }
            Ended::Lost => {
                recorder.lost(now_ms());
                link.close();
            }
        }
        let Some((reopened, active)) = reconnect(stop) else {
            return;
        };
        recorder.bus().replace(reopened.connection.clone());
        link = reopened;
        begin(&mut recorder, active);
    }
}

/// Start from the window that is active now, as if it had just become so.
fn begin(recorder: &mut Recorder<LiveBus>, active: Option<(String, String)>) {
    if let Some((sender, path)) = active {
        let event = Event::Active {
            sender,
            path,
            on: true,
        };
        recorder.handle(event, now_ms());
    }
}

/// Ask for the bus again until it answers, the tries are spent, or observation stops.
fn reconnect(stop: &AtomicBool) -> Option<(Link, Option<(String, String)>)> {
    for _ in 0..RECONNECT_TRIES {
        if !sleep_unless_stopped(RECONNECT_EVERY, stop) {
            return None;
        }
        match Link::open() {
            Ok(opened) => return Some(opened),
            Err(why) => eprintln!("compux: capture: the accessibility bus is still away: {why}"),
        }
    }
    eprintln!("compux: capture: the accessibility bus did not come back; observation stopped");
    None
}

/// Sleep `total`, looking at the flag every STOP_TICK. False when it was raised.
fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) -> bool {
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(STOP_TICK);
        slept += STOP_TICK;
    }
    !stop.load(Ordering::SeqCst)
}

/// Why an observation's loop ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    Stopped,
    Lost,
}

/// Feed the recorder until observation stops or the bus goes away.
fn run(recorder: &mut Recorder<LiveBus>, link: &Link, stop: &AtomicBool) -> Ended {
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ended::Stopped;
        }
        match link.events.recv_timeout(wait_for(recorder.due())) {
            Ok(event) => recorder.handle(event, now_ms()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ended::Lost,
        }
        let dropped = link.dropped.swap(0, Ordering::SeqCst);
        if dropped > 0 {
            eprintln!("compux: capture: {dropped} accessibility events dropped");
            recorder.overflowed(now_ms(), now_ms());
        }
        recorder.tick(now_ms());
    }
}

/// How long to wait for the next event: until the next timer, at most STOP_TICK.
fn wait_for(due_ms: Option<i64>) -> Duration {
    let Some(due) = due_ms else {
        return STOP_TICK;
    };
    let left = u64::try_from(due.saturating_sub(now_ms())).unwrap_or(0);
    Duration::from_millis(left).min(STOP_TICK)
}

fn boot_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("compux-{}-{}", std::process::id(), nanos)
}

// --- the link: the connection, its subscriptions and its pump --------------

/// One connection to the accessibility bus, subscribed to what every application
/// is asked, and the pump that reads it.
struct Link {
    connection: Connection,
    events: Receiver<Event>,
    dropped: Arc<AtomicU64>,
    pump: Option<JoinHandle<()>>,
}

impl Link {
    /// The session's accessibility bus, set up, with the window active on it now.
    #[cfg(not(test))]
    fn open() -> Result<(Link, Option<(String, String)>), String> {
        let connection = async_io::block_on(async {
            let session = super::bus::session().await?;
            let address = super::bus::address(&session)
                .await
                .map_err(|why| why.detail())?;
            drop(session);
            super::bus::open(&address, OBSERVER_READ_TIMEOUT).await
        })?;
        Link::over(connection)
    }

    /// Under test there is no session this process may read.
    #[cfg(test)]
    fn open() -> Result<(Link, Option<(String, String)>), String> {
        Err("a unit test builds its link over a fake's connection".to_string())
    }

    /// Set up an open connection: the pump first, so no signal sent after a
    /// subscription is missed, then the registrations and the subscriptions, then
    /// the registry read that proves the bus answers.
    fn over(connection: Connection) -> Result<(Link, Option<(String, String)>), String> {
        let stream = MessageStream::from(&connection);
        let (queue, events) = mpsc::sync_channel::<Event>(QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        let pump = {
            let dropped = dropped.clone();
            std::thread::Builder::new()
                .name("compux-observe-pump".into())
                .spawn(move || pump(stream, &queue, &dropped))
                .map_err(|e| format!("failed to spawn the observer's pump: {e}"))?
        };
        let link = Link {
            connection,
            events,
            dropped,
            pump: Some(pump),
        };
        match async_io::block_on(subscribe(&link.connection)) {
            Ok(()) => {
                let active = async_io::block_on(active_window(&link.connection));
                Ok((link, active))
            }
            Err(why) => {
                link.close();
                Err(why)
            }
        }
    }

    /// Close the connection, which ends the pump, and join it.
    fn close(mut self) {
        if let Err(error) = async_io::block_on(self.connection.clone().close()) {
            eprintln!("compux: capture: closing the accessibility bus failed: {error}");
        }
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

/// Register the events, subscribe to what every application is asked, and prove
/// the registry answers. A registration the registry refuses is said and the rest
/// go on: a toolkit that never sends that event is an event that never arrives.
async fn subscribe(connection: &Connection) -> Result<(), String> {
    for event in REGISTERED {
        let registered: Result<Message, zbus::Error> = connection
            .call_method(
                Some(REGISTRY),
                REGISTRY_PATH,
                Some(REGISTRY),
                "RegisterEvent",
                &(event, Vec::<String>::new()),
            )
            .await;
        if let Err(error) = registered {
            eprintln!("compux: capture: the registry refused {event}: {error}");
        }
    }
    for rule in EVERY_APPLICATION {
        add_match(connection, rule).await?;
    }
    super::bus::ready(connection).await.map(|_| ())
}

async fn add_match(connection: &Connection, rule: &str) -> Result<(), String> {
    bus_daemon(connection, "AddMatch", rule)
        .await
        .map_err(|error| format!("the bus would not deliver {rule}: {error}"))
}

async fn remove_match(connection: &Connection, rule: &str) -> Result<(), String> {
    bus_daemon(connection, "RemoveMatch", rule)
        .await
        .map_err(|error| format!("the bus would not stop delivering {rule}: {error}"))
}

async fn bus_daemon(connection: &Connection, member: &str, rule: &str) -> zbus::Result<()> {
    connection
        .call_method(
            Some(BUS_DAEMON),
            BUS_DAEMON_PATH,
            Some(BUS_DAEMON),
            member,
            &(rule,),
        )
        .await
        .map(|_| ())
}

/// Take every signal off the connection into the queue, until it closes.
fn pump(mut stream: MessageStream, queue: &SyncSender<Event>, dropped: &AtomicU64) {
    async_io::block_on(async {
        while let Some(message) = stream.next().await {
            let Some(event) = message.ok().as_ref().and_then(event_of) else {
                continue;
            };
            match queue.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    dropped.fetch_add(1, Ordering::SeqCst);
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    });
}

/// The event one signal is, if it is one the recorder takes. A text change's body,
/// which is the text itself, is never read.
pub fn event_of(message: &Message) -> Option<Event> {
    let header = message.header();
    if header.message_type() != Type::Signal {
        return None;
    }
    let interface = header.interface()?.to_string();
    let member = header.member()?.to_string();
    if interface == BUS_DAEMON && member == "NameOwnerChanged" {
        return gone_of(message);
    }
    if interface != EVENT_OBJECT {
        return None;
    }
    let sender = header.sender()?.to_string();
    let path = header.path()?.to_string();
    if member == "TextChanged" {
        return Some(Event::TextChanged { sender, path });
    }
    let (detail, detail1, any) = fields(message)?;
    match (member.as_str(), detail.as_str()) {
        ("StateChanged", "active") => Some(Event::Active {
            sender,
            path,
            on: detail1 != 0,
        }),
        ("StateChanged", "focused") => Some(Event::Focused {
            sender,
            path,
            on: detail1 != 0,
        }),
        ("PropertyChange", "accessible-name") => Some(Event::NameChanged {
            sender,
            path,
            name: String::try_from(any).ok(),
        }),
        _ => None,
    }
}

/// An event's detail, its first number and its data, in either of the two forms
/// AT-SPI events are sent in: `(siiva{sv})` since at-spi2-core 2.x, and the older
/// `(siiv(so))` some bridges still send.
fn fields(message: &Message) -> Option<(String, i32, OwnedValue)> {
    type Current = (String, i32, i32, OwnedValue, HashMap<String, OwnedValue>);
    type Older = (String, i32, i32, OwnedValue, (String, OwnedObjectPath));
    let body = message.body();
    if let Ok((detail, detail1, _, any, _)) = body.deserialize::<Current>() {
        return Some((detail, detail1, any));
    }
    body.deserialize::<Older>()
        .ok()
        .map(|(detail, detail1, _, any, _)| (detail, detail1, any))
}

/// A unique connection that left the bus.
fn gone_of(message: &Message) -> Option<Event> {
    let (name, _old, new): (String, String, String) = message.body().deserialize().ok()?;
    (name.starts_with(':') && new.is_empty()).then_some(Event::Gone { sender: name })
}

/// The window that reads ACTIVE on the bus now, when exactly one does.
async fn active_window(connection: &Connection) -> Option<(String, String)> {
    let applications = children(connection, REGISTRY, ROOT, MAX_APPLICATIONS).await;
    let mut active = Vec::new();
    for (sender, root) in applications {
        for (bus, window) in children(connection, &sender, &root, MAX_WINDOWS).await {
            let states: Vec<u32> = call(connection, &bus, &window, ACCESSIBLE, "GetState", &())
                .await
                .unwrap_or_default();
            if vocabulary::has(&states, ACTIVE) {
                active.push((bus, window));
            }
        }
    }
    match <[(String, String); 1]>::try_from(active) {
        Ok([one]) => Some(one),
        Err(_) => None,
    }
}

async fn children(
    connection: &Connection,
    bus: &str,
    path: &str,
    limit: usize,
) -> Vec<(String, String)> {
    let listed: Vec<(String, OwnedObjectPath)> =
        call(connection, bus, path, ACCESSIBLE, "GetChildren", &())
            .await
            .unwrap_or_default();
    listed
        .into_iter()
        .map(|(bus, path)| (bus, path.to_string()))
        .take(limit)
        .collect()
}

async fn call<B, R>(
    connection: &Connection,
    bus: &str,
    path: &str,
    interface: &str,
    member: &str,
    body: &B,
) -> Option<R>
where
    B: Serialize + DynamicType,
    R: for<'d> DynamicDeserialize<'d>,
{
    let reply = connection
        .call_method(Some(bus), path, Some(interface), member, body)
        .await
        .ok()?;
    reply.body().deserialize::<R>().ok()
}

async fn property<T>(
    connection: &Connection,
    bus: &str,
    path: &str,
    interface: &str,
    name: &str,
) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    let value: OwnedValue =
        call(connection, bus, path, PROPERTIES, "Get", &(interface, name)).await?;
    T::try_from(value).ok()
}

// --- the bus the recorder reads ----------------------------------------------

/// The recorder's reads and subscriptions, over the link's connection (replaced
/// when the bus is opened again).
pub struct LiveBus {
    connection: RefCell<Connection>,
}

impl LiveBus {
    pub fn over(connection: Connection) -> LiveBus {
        LiveBus {
            connection: RefCell::new(connection),
        }
    }

    fn replace(&self, connection: Connection) {
        // The connection it replaces is the one already closed when the bus was lost.
        drop(self.connection.replace(connection));
    }

    fn connection(&self) -> Connection {
        self.connection.borrow().clone()
    }

    fn lists(&self, sender: &str, path: &str, interface: &str) -> bool {
        let listed: Vec<String> = async_io::block_on(call(
            &self.connection(),
            sender,
            path,
            ACCESSIBLE,
            "GetInterfaces",
            &(),
        ))
        .unwrap_or_default();
        listed.iter().any(|name| name == interface)
    }

    /// The toolkit an application's root names, read only where the root lists the
    /// Application interface. `None` when it does not say.
    fn toolkit(&self, sender: &str) -> Option<String> {
        if !self.lists(sender, ROOT, APPLICATION_INTERFACE) {
            return None;
        }
        let name: Option<String> = async_io::block_on(property(
            &self.connection(),
            sender,
            ROOT,
            APPLICATION_INTERFACE,
            "ToolkitName",
        ));
        name.filter(|name| !name.trim().is_empty())
    }

    /// Whether a control sits inside a web document: its own role and each
    /// ancestor's, read up to its application's root. The answer is "no" only when
    /// the walk ends at this connection's own root, or at the null reference. An
    /// ancestry that will not read, that passes onto another connection (a web view
    /// embedded from another process, a plug in a socket), or that is deeper than
    /// MAX_ANCESTRY is taken as inside one, so a field is read only where it is
    /// known not to be a web page's.
    fn in_web_document(&self, sender: &str, path: &str, role: u32) -> bool {
        if role == DOCUMENT_WEB {
            return true;
        }
        let connection = self.connection();
        let mut at = path.to_string();
        for _ in 0..MAX_ANCESTRY {
            let parent: Option<(String, OwnedObjectPath)> =
                async_io::block_on(property(&connection, sender, &at, ACCESSIBLE, "Parent"));
            let Some((bus, parent)) = parent else {
                return true;
            };
            let parent = parent.to_string();
            if parent == NULL_PATH {
                return false;
            }
            if bus != sender {
                return true;
            }
            if parent == ROOT {
                return false;
            }
            let role: Option<u32> = async_io::block_on(call(
                &connection,
                sender,
                &parent,
                ACCESSIBLE,
                "GetRole",
                &(),
            ));
            match role {
                Some(DOCUMENT_WEB) | None => return true,
                Some(_) => at = parent,
            }
        }
        true
    }

    fn words(&self, sender: &str, path: &str, name: &str) -> Option<String> {
        let text: Option<String> =
            async_io::block_on(property(&self.connection(), sender, path, ACCESSIBLE, name));
        text.filter(|text| !text.trim().is_empty())
    }
}

/// The match rules that watch one application: its focus and its window titles,
/// and its text changes where `text` is true. A connection's unique name is all
/// they hold of it.
pub fn scoped_rules(sender: &str, text: bool) -> Vec<String> {
    let rule = |member: &str, arg0: Option<&str>| {
        let arg0 = arg0.map(|a| format!(",arg0='{a}'")).unwrap_or_default();
        format!(
            "type='signal',sender='{sender}',interface='{EVENT_OBJECT}',member='{member}'{arg0}"
        )
    };
    let mut rules = vec![
        rule("StateChanged", Some("focused")),
        rule("PropertyChange", Some("accessible-name")),
    ];
    if text {
        rules.push(rule("TextChanged", None));
    }
    rules
}

/// A unique connection name (`:1.42`): nothing else may be put into a rule.
fn unique_name(sender: &str) -> bool {
    sender.starts_with(':')
        && sender.len() <= 255
        && sender[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// What kind of field a control is, from its role and its states. A terminal is its
/// own kind, whose text is never read: what a prompt asks for there (a password to
/// `sudo` or `ssh`) is marked by no role.
pub fn field_of(role: u32, states: &[u32]) -> Field {
    if role == PASSWORD_TEXT {
        return Field::Secure;
    }
    if role == TERMINAL {
        return Field::Terminal;
    }
    if vocabulary::textual(role, vocabulary::has(states, EDITABLE)) {
        return Field::Editable;
    }
    Field::Other
}

/// The file name of a process's executable, without the mark of one replaced on
/// disk since it started.
fn program(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let target = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let name = target.file_name()?.to_string_lossy().to_string();
    Some(name.trim_end_matches(" (deleted)").to_string()).filter(|name| !name.is_empty())
}

impl Bus for LiveBus {
    fn application(&self, sender: &str) -> Option<Application> {
        let name = self.words(sender, ROOT, "Name")?;
        let pid: Option<u32> = async_io::block_on(call(
            &self.connection(),
            BUS_DAEMON,
            BUS_DAEMON_PATH,
            BUS_DAEMON,
            "GetConnectionUnixProcessID",
            &(sender,),
        ));
        let pid = pid.and_then(|pid| i32::try_from(pid).ok()).unwrap_or(0);
        Some(Application {
            name,
            pid,
            program: program(pid),
            toolkit: self.toolkit(sender),
        })
    }

    fn title(&self, sender: &str, path: &str) -> Option<String> {
        self.words(sender, path, "Name")
    }

    fn focus(&self, sender: &str, path: &str) -> Option<Focus> {
        let connection = self.connection();
        let role: u32 =
            async_io::block_on(call(&connection, sender, path, ACCESSIBLE, "GetRole", &()))?;
        let states: Vec<u32> =
            async_io::block_on(call(&connection, sender, path, ACCESSIBLE, "GetState", &()))
                .unwrap_or_default();
        let label = self
            .words(sender, path, "Name")
            .or_else(|| self.words(sender, path, "Description"));
        let field = field_of(role, &states);
        // Only a field's ancestry matters: nothing else's text is ever read.
        let web = field != Field::Other && self.in_web_document(sender, path, role);
        Some(Focus {
            role: vocabulary::role(role),
            field,
            label,
            web,
        })
    }

    fn text(&self, sender: &str, path: &str) -> Option<Text> {
        let chars = self.count(sender, path)?;
        let end = i32::try_from(chars.min(MAX_TEXT_CHARS)).ok()?;
        let text: String = async_io::block_on(call(
            &self.connection(),
            sender,
            path,
            TEXT_INTERFACE,
            "GetText",
            &(0i32, end),
        ))?;
        Some(Text { text, chars })
    }

    fn is_focused(&self, sender: &str, path: &str) -> bool {
        let states: Option<Vec<u32>> = async_io::block_on(call(
            &self.connection(),
            sender,
            path,
            ACCESSIBLE,
            "GetState",
            &(),
        ));
        states.is_some_and(|states| vocabulary::has(&states, vocabulary::FOCUSED))
    }

    fn count(&self, sender: &str, path: &str) -> Option<i64> {
        if !self.lists(sender, path, TEXT_INTERFACE) {
            return None;
        }
        let count: Option<i32> = async_io::block_on(property(
            &self.connection(),
            sender,
            path,
            TEXT_INTERFACE,
            "CharacterCount",
        ));
        count.map(i64::from).filter(|count| *count >= 0)
    }

    fn watch(&self, sender: &str, text: bool) -> Result<(), String> {
        if !unique_name(sender) {
            return Err(format!("{sender:?} is not a connection's name"));
        }
        let connection = self.connection();
        let rules = scoped_rules(sender, text);
        for (index, rule) in rules.iter().enumerate() {
            if let Err(why) = async_io::block_on(add_match(&connection, rule)) {
                for added in &rules[..index] {
                    let _ = async_io::block_on(remove_match(&connection, added));
                }
                return Err(why);
            }
        }
        Ok(())
    }

    fn unwatch(&self, sender: &str, text: bool) {
        if !unique_name(sender) {
            return;
        }
        let connection = self.connection();
        for rule in scoped_rules(sender, text) {
            if let Err(why) = async_io::block_on(remove_match(&connection, &rule)) {
                eprintln!("compux: capture: {why}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fake::{self, Desktop, Fake, Node, APP, PID};
    use serde_json::{json, Value};
    use zbus::zvariant::Value as ZValue;

    const TIMEOUT: Duration = Duration::from_millis(500);
    const ENTRY_ROLE: u32 = 79;
    const PASSWORD_ROLE: u32 = 40;
    const BUTTON_ROLE: u32 = 43;
    /// A second application on the bus, which is never on the list.
    const OTHER: &str = ":1.11";
    const OTHER_PID: u32 = 4712;

    fn desktop() -> Desktop {
        let mut desktop = Desktop::with_applications(vec![
            (
                APP,
                PID as u32,
                Node::of(75)
                    .named("Fixture Editor")
                    .made_with("GTK", "3.24.41")
                    .holding(APP, &["/w"]),
            ),
            (
                OTHER,
                OTHER_PID,
                Node::of(75).named("Mail").holding(OTHER, &["/w"]),
            ),
        ]);
        let window = Node {
            states: vec![fake::ACTIVE, fake::ENABLED, fake::SHOWING],
            ..Node::of(23)
                .named("notes.txt")
                .holding(APP, &["/w/f", "/w/p", "/w/doc"])
        };
        desktop.put(APP, "/w", window);
        desktop.put(OTHER, "/w", Node::of(23).named("Inbox"));
        let mut entry = Node::of(ENTRY_ROLE)
            .named("Search")
            .listing(&["Text"])
            .reading(&[fake::EDITABLE]);
        entry.text = "groceries".to_string();
        desktop.put(APP, "/w/f", entry);
        let mut secure = Node::of(PASSWORD_ROLE).named("Password").listing(&["Text"]);
        secure.text = "hunter2".to_string();
        desktop.put(APP, "/w/p", secure);
        desktop.put(APP, "/w/b", Node::of(BUTTON_ROLE).named("Save"));
        desktop.put(
            APP,
            "/w/doc",
            Node::of(DOCUMENT_WEB).holding(APP, &["/w/doc/f"]),
        );
        desktop.put(
            APP,
            "/w/doc/f",
            Node::of(ENTRY_ROLE)
                .named("Comment")
                .listing(&["Text"])
                .reading(&[fake::EDITABLE]),
        );
        // An entry no node lists: its ancestry does not read.
        desktop.put(
            APP,
            "/orphan",
            Node::of(ENTRY_ROLE).named("Loose").listing(&["Text"]),
        );
        desktop
    }

    fn signal(
        member: &str,
        sender: &str,
        path: &str,
        detail: &str,
        on: i32,
        any: ZValue,
    ) -> Message {
        let body = (detail, on, 0i32, any, HashMap::<String, ZValue>::new());
        Message::signal(path, EVENT_OBJECT, member)
            .expect("a signal")
            .sender(sender)
            .expect("a sender")
            .build(&body)
            .expect("a body")
    }

    // A window's activation, a focus, a name change and a text change decode to the
    // recorder's events, a text change without its body being read; anything else
    // is not an event.
    #[test]
    fn signals_decode_to_the_recorders_events() {
        let active = signal("StateChanged", APP, "/w", "active", 1, ZValue::from(0i32));
        let blurred = signal(
            "StateChanged",
            APP,
            "/w/f",
            "focused",
            0,
            ZValue::from(0i32),
        );
        let renamed = signal(
            "PropertyChange",
            APP,
            "/w",
            "accessible-name",
            0,
            ZValue::from("notes.txt *"),
        );
        let typed = signal(
            "TextChanged",
            APP,
            "/w/f",
            "insert",
            0,
            ZValue::from("secret"),
        );
        let other = signal("StateChanged", APP, "/w", "showing", 1, ZValue::from(0i32));

        assert_eq!(
            event_of(&active),
            Some(Event::Active {
                sender: APP.into(),
                path: "/w".into(),
                on: true
            })
        );
        assert_eq!(
            event_of(&blurred),
            Some(Event::Focused {
                sender: APP.into(),
                path: "/w/f".into(),
                on: false
            })
        );
        assert_eq!(
            event_of(&renamed),
            Some(Event::NameChanged {
                sender: APP.into(),
                path: "/w".into(),
                name: Some("notes.txt *".into())
            })
        );
        assert_eq!(
            event_of(&typed),
            Some(Event::TextChanged {
                sender: APP.into(),
                path: "/w/f".into()
            })
        );
        assert_eq!(event_of(&other), None);
    }

    // The older event form some bridges still send decodes the same.
    #[test]
    fn the_older_event_form_decodes_too() {
        let body = (
            "active",
            1i32,
            0i32,
            ZValue::from(0i32),
            (
                APP.to_string(),
                OwnedObjectPath::try_from(ROOT).expect("a path"),
            ),
        );
        let message = Message::signal("/w", EVENT_OBJECT, "StateChanged")
            .expect("a signal")
            .sender(APP)
            .expect("a sender")
            .build(&body)
            .expect("a body");

        assert_eq!(
            event_of(&message),
            Some(Event::Active {
                sender: APP.into(),
                path: "/w".into(),
                on: true
            })
        );
    }

    // A unique connection that leaves the bus is gone; a well-known name changing
    // hands, or a connection arriving, is not.
    #[test]
    fn a_connection_leaving_the_bus_is_gone() {
        let left = |name: &str, new: &str| {
            Message::signal(BUS_DAEMON_PATH, BUS_DAEMON, "NameOwnerChanged")
                .expect("a signal")
                .sender(BUS_DAEMON)
                .expect("a sender")
                .build(&(name, ":1.5", new))
                .expect("a body")
        };

        assert_eq!(
            event_of(&left(":1.10", "")),
            Some(Event::Gone {
                sender: ":1.10".into()
            })
        );
        assert_eq!(event_of(&left("org.example.App", "")), None);
        assert_eq!(event_of(&left(":1.10", ":1.10")), None);
    }

    // Every rule that watches content names the one application's connection; a
    // browser's leave out its text changes; nothing else can be put into a rule.
    #[test]
    fn content_rules_name_the_one_application_and_a_browser_has_no_text_rule() {
        let rules = scoped_rules(APP, true);
        assert_eq!(rules.len(), 3);
        assert!(
            rules.iter().all(|r| r.contains("sender=':1.10'")),
            "{rules:?}"
        );
        assert!(rules.iter().any(|r| r.contains("member='TextChanged'")));
        assert!(!scoped_rules(APP, false)
            .iter()
            .any(|r| r.contains("TextChanged")));
        assert!(EVERY_APPLICATION
            .iter()
            .all(|r| !r.contains("TextChanged") && !r.contains("PropertyChange")));
        assert!(unique_name(":1.10"));
        assert!(!unique_name(":1.10',type='signal"));
        assert!(!unique_name("org.example.App"));
    }

    #[test]
    fn fields_are_classified_by_role_and_state() {
        assert_eq!(field_of(PASSWORD_ROLE, &[]), Field::Secure);
        assert_eq!(field_of(ENTRY_ROLE, &[]), Field::Editable);
        assert_eq!(field_of(TERMINAL, &[]), Field::Terminal);
        assert_eq!(field_of(BUTTON_ROLE, &[]), Field::Other);
        // A text node counts only while it is editable.
        let editable = [1u32 << EDITABLE, 0];
        assert_eq!(field_of(vocabulary::TEXT, &editable), Field::Editable);
        assert_eq!(field_of(vocabulary::TEXT, &[0, 0]), Field::Other);
    }

    // The reads the recorder makes: who an application is (its root's name and its
    // process), a field's text through `Text` once its interfaces list it, and its
    // count; never GetAll or GetActions (the fake fails the test on either).
    #[test]
    fn the_reads_name_the_application_and_read_a_field() {
        let (fake, connection) = Fake::start(desktop(), TIMEOUT);
        let bus = LiveBus::over(connection);

        let app = bus.application(APP).expect("the application");
        assert_eq!(app.name, "Fixture Editor");
        assert_eq!(app.toolkit.as_deref(), Some("GTK"));
        let other = bus.application(OTHER).expect("the other application");
        assert_eq!(
            other.toolkit, None,
            "a root that lists no Application says no toolkit"
        );
        assert_eq!(app.pid, PID);
        let focus = bus.focus(APP, "/w/f").expect("the field");
        assert_eq!(focus.role, "entry");
        assert_eq!(focus.field, Field::Editable);
        assert_eq!(focus.label.as_deref(), Some("Search"));
        assert!(!focus.web, "read up to the application, no web document");
        assert_eq!(
            bus.text(APP, "/w/f"),
            Some(Text {
                text: "groceries".into(),
                chars: 9
            })
        );
        assert_eq!(bus.count(APP, "/w/b"), None, "a button lists no Text");
        assert_eq!(fake.sent("GetText").len(), 1);
    }

    // A field inside a web document is a web page's, and one whose ancestry does not
    // read as far as its application is taken as one: its text is never read.
    #[test]
    fn a_field_in_a_web_document_or_an_unreadable_ancestry_reads_as_web() {
        let (_fake, connection) = Fake::start(desktop(), TIMEOUT);
        let bus = LiveBus::over(connection);

        let inside = bus.focus(APP, "/w/doc/f").expect("the field");
        let orphan = bus.focus(APP, "/orphan").expect("the field");

        assert!(inside.web);
        assert!(orphan.web);
    }

    /// The desktop with a chain of `depth` panels under the window and an entry at
    /// its bottom, `/deep/<depth>`.
    fn deep(depth: usize) -> Desktop {
        let mut desktop = desktop();
        let window = desktop
            .nodes
            .get_mut(&(APP.to_string(), "/w".to_string()))
            .expect("the window");
        window
            .children
            .push((APP.to_string(), "/deep/0".to_string()));
        for level in 0..depth {
            let below = format!("/deep/{}", level + 1);
            desktop.put(
                APP,
                &format!("/deep/{level}"),
                Node::of(39).holding(APP, &[below.as_str()]),
            );
        }
        desktop.put(
            APP,
            &format!("/deep/{depth}"),
            Node::of(ENTRY_ROLE).named("Deep").listing(&["Text"]),
        );
        desktop
    }

    // A field whose ancestry reaches its own application's root, through nothing
    // that is a web document, is read: the one case that is.
    #[test]
    fn an_ancestry_that_reaches_its_own_root_reads_as_not_web() {
        let (_fake, connection) = Fake::start(deep(3), TIMEOUT);
        let bus = LiveBus::over(connection);

        assert!(!bus.focus(APP, "/w/f").expect("the field").web);
        assert!(!bus.focus(APP, "/deep/3").expect("the field").web);
    }

    // A parent on another connection (a web view embedded from another process, a
    // plug in a socket) is not known to be no web page's: withheld.
    #[test]
    fn a_parent_on_another_connection_reads_as_web() {
        let mut desktop = desktop();
        let mut embedded = Node::of(ENTRY_ROLE).named("Embedded").listing(&["Text"]);
        embedded.claims = Some(((OTHER.to_string(), "/plug".to_string()), 0));
        desktop.put(APP, "/embedded", embedded);
        let (_fake, connection) = Fake::start(desktop, TIMEOUT);
        let bus = LiveBus::over(connection);

        assert!(bus.focus(APP, "/embedded").expect("the field").web);
    }

    // An ancestry deeper than MAX_ANCESTRY is not read to its end, so it is not
    // known to be no web page's: withheld. One level shorter is read to its root.
    #[test]
    fn an_ancestry_past_the_bound_reads_as_web() {
        let past = MAX_ANCESTRY + 1;
        let (_fake, connection) = Fake::start(deep(past), TIMEOUT);
        let bus = LiveBus::over(connection);
        assert!(
            bus.focus(APP, &format!("/deep/{past}"))
                .expect("the field")
                .web
        );

        let within = MAX_ANCESTRY - 2;
        let (_fake, connection) = Fake::start(deep(within), TIMEOUT);
        let bus = LiveBus::over(connection);
        assert!(
            !bus.focus(APP, &format!("/deep/{within}"))
                .expect("the field")
                .web
        );
    }

    // Watching an application subscribes rules naming it, and unwatching removes
    // exactly those.
    #[test]
    fn watching_adds_and_unwatching_removes_the_same_rules() {
        let (fake, connection) = Fake::start(desktop(), TIMEOUT);
        let bus = LiveBus::over(connection);

        bus.watch(APP, true).expect("watched");
        bus.unwatch(APP, true);

        let added: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.member == "AddMatch")
            .map(|c| c.argument)
            .collect();
        let removed: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.member == "RemoveMatch")
            .map(|c| c.argument)
            .collect();
        assert_eq!(added, scoped_rules(APP, true));
        assert_eq!(removed, added);
    }

    /// An observation over the fake bus: the fake, the link and the recorder.
    struct Observing {
        fake: Fake,
        link: Link,
        recorder: Recorder<LiveBus>,
        emitter: Emitter,
        active: Option<(String, String)>,
    }

    fn observing(apps: &[&str]) -> Observing {
        let (fake, connection) = Fake::start(desktop(), TIMEOUT);
        let (link, active) = Link::over(connection.clone()).expect("a link");
        let emitter = Emitter::capturing();
        let apps = apps.iter().map(|a| a.to_string()).collect();
        let recorder = Recorder::new(
            LiveBus::over(connection),
            emitter.clone(),
            apps,
            "boot-t".into(),
        );
        Observing {
            fake,
            link,
            recorder,
            emitter,
            active,
        }
    }

    impl Observing {
        /// Take `count` events off the link into the recorder, each waited for at most
        /// two seconds, then fire every timer as if the debounces had passed.
        fn take(&mut self, count: usize) {
            for _ in 0..count {
                let event = self
                    .link
                    .events
                    .recv_timeout(Duration::from_secs(2))
                    .expect("an event the fake sent");
                self.recorder.handle(event, now_ms());
            }
            self.recorder
                .tick(now_ms() + crate::recorder::VALUE_DEBOUNCE_MS + 1);
        }
    }

    fn kinds(emitter: &Emitter) -> Vec<String> {
        emitter
            .captured()
            .iter()
            .map(|f: &Value| f["kind"].as_str().unwrap_or("").to_string())
            .collect()
    }

    // Setting up registers the events in the form every registry takes, subscribes
    // to the content-free activation of every application, and finds the window
    // active now.
    #[test]
    fn a_link_registers_subscribes_and_finds_the_active_window() {
        let observing = observing(&["Fixture Editor"]);
        let fake = &observing.fake;

        let registered: Vec<_> = fake
            .calls()
            .into_iter()
            .filter(|c| c.member == "RegisterEvent")
            .collect();
        assert_eq!(registered.len(), REGISTERED.len());
        assert!(registered.iter().all(|c| c.signature == "sas"));
        let added: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.member == "AddMatch")
            .map(|c| c.argument)
            .collect();
        assert_eq!(added, EVERY_APPLICATION.to_vec());
        assert_eq!(observing.active, Some((APP.to_string(), "/w".to_string())));
        observing.link.close();
    }

    // End to end over the fake bus: the active allowlisted application is announced
    // and watched, its typed value read once it settles, a password field never
    // read, and an application not on the list says nothing whatever it sends.
    #[test]
    fn an_observation_records_the_allowlisted_application_alone() {
        let mut observing = observing(&["Fixture Editor"]);
        let active = observing.active.take();
        begin(&mut observing.recorder, active);
        assert_eq!(
            kinds(&observing.emitter),
            vec!["app.activated", "window.focused"]
        );

        let fake = &observing.fake;
        fake.emit(signal(
            "StateChanged",
            APP,
            "/w/f",
            "focused",
            1,
            ZValue::from(0i32),
        ));
        fake.emit(signal(
            "TextChanged",
            APP,
            "/w/f",
            "insert",
            0,
            ZValue::from("x"),
        ));
        fake.emit(signal(
            "TextChanged",
            OTHER,
            "/w/f",
            "insert",
            0,
            ZValue::from("y"),
        ));
        observing.take(3);

        let frames = observing.emitter.captured();
        let value = frames
            .iter()
            .find(|f| f["kind"] == json!("field.value"))
            .expect("a value");
        assert_eq!(value["text"], json!("groceries"));
        assert_eq!(value["app"]["bundle_id"], json!("Fixture Editor"));
        assert!(frames
            .iter()
            .all(|f| f["app"]["bundle_id"] != json!("Mail")));

        let fake = &observing.fake;
        fake.emit(signal(
            "StateChanged",
            APP,
            "/w/p",
            "focused",
            1,
            ZValue::from(0i32),
        ));
        fake.emit(signal(
            "TextChanged",
            APP,
            "/w/p",
            "insert",
            0,
            ZValue::from("*"),
        ));
        observing.take(2);

        assert_eq!(
            observing.fake.sent("GetText").len(),
            1,
            "the password field was never read"
        );
        let last = observing.emitter.captured().pop().expect("a frame");
        assert_eq!(last["kind"], json!("field.value"));
        assert_eq!(last["content_withheld"], json!(true));
        observing.recorder.end(now_ms());
        observing.link.close();
    }
}
