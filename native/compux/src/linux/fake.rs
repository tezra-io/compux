//! A fake AT-SPI desktop, for the Linux reader's unit tests.
//!
//! One peer-to-peer D-Bus connection, no bus daemon and no desktop: the reader's
//! end is an ordinary `zbus::Connection`, and this end answers every call from a
//! scripted table of nodes keyed by (bus name, object path) — the registry, the
//! applications on it, the bus daemon's process lookup and the session bus's
//! `org.a11y.Bus`. It answers the way the toolkits were measured to (agent-desktop
//! ledger rows L17-1..L17-12): a destroyed object answers `UnknownObject` to every
//! read, a live one answers `UnknownObject` for an interface it does not have, a
//! GTK3 object keeps answering after it left its parent's list, and an application
//! that quit is `ServiceUnknown`.
//!
//! It RECORDS every call, including the two the reader must never make
//! (`Action.GetActions` and `Properties.GetAll`), so a test asserts on what was
//! asked, not only on what came back.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::StreamExt;
use zbus::connection::Builder;
use zbus::message::{Flags, Header, Type};
use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::{Connection, Message, MessageStream};

/// Where the registry lives, and the path every application's root answers at.
pub const REGISTRY: &str = "org.a11y.atspi.Registry";
pub const ROOT: &str = "/org/a11y/atspi/accessible/root";

/// The error names the toolkits answer, as the ledger recorded them.
pub const UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
pub const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
pub const SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";
pub const FAILED: &str = "org.freedesktop.DBus.Error.Failed";

/// AT-SPI state bits a script sets.
pub const ACTIVE: u32 = 1;
pub const CHECKED: u32 = 4;
pub const EDITABLE: u32 = 7;
pub const ENABLED: u32 = 8;
pub const EXPANDABLE: u32 = 9;
pub const EXPANDED: u32 = 10;
pub const FOCUSABLE: u32 = 11;
pub const FOCUSED: u32 = 12;
pub const MULTI_LINE: u32 = 17;
pub const SELECTABLE: u32 = 22;
pub const SELECTED: u32 = 23;
pub const SENSITIVE: u32 = 24;
pub const SHOWING: u32 = 25;
pub const VERTICAL: u32 = 29;
pub const VISIBLE: u32 = 30;

/// `ATSPI_RELATION_LABELLED_BY`.
pub const LABELLED_BY: u32 = 2;

/// What a node does when it is asked anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Life {
    Alive,
    /// Gone: `UnknownObject` to every call, `GetRole` included (GTK3 and Chromium
    /// after their teardown window, L17-7; Qt at once, L17-1).
    Destroyed,
    /// Never answers, so the caller's timeout is what ends the call.
    Silent,
}

/// One scripted accessible.
#[derive(Clone, Debug)]
pub struct Node {
    pub role: u32,
    pub name: String,
    pub description: String,
    /// The state bits that are set.
    pub states: Vec<u32>,
    /// Interfaces by their short name: `Component`, `Action`, `Text`, `Value`,
    /// `EditableText`. `Accessible` is always listed.
    pub interfaces: Vec<&'static str>,
    /// Children as (bus name, path): the registry's children are applications on
    /// other buses.
    pub children: Vec<(String, String)>,
    pub extents: (i32, i32, i32, i32),
    /// What `GetName(i)` answers for each index: a name, or an error name.
    pub actions: Vec<Result<&'static str, &'static str>>,
    pub text: String,
    pub value: f64,
    pub life: Life,
    /// Members this live node answers `UnknownObject` for (L17-2).
    pub refuses: Vec<&'static str>,
    /// An application root's `ToolkitName` and `Version`.
    pub toolkit: (&'static str, &'static str),
    /// `GetExtents` in window coordinates, where it differs from the screen's.
    pub window_extents: Option<(i32, i32, i32, i32)>,
    /// `Value`'s range.
    pub range: (f64, f64),
    /// The state bit `DoAction` flips, whichever action is done.
    pub flips: Option<u32>,
    /// Where `Component.ScrollTo` moves it.
    pub scrolls_to: Option<(i32, i32, i32, i32)>,
    /// The caret, and the one text selection, in characters.
    pub caret: i32,
    pub selection: Option<(i32, i32)>,
    /// `GetRelationSet`: (relation type, targets as (bus name, path)).
    pub relations: Vec<(u32, Vec<(String, String)>)>,
    /// Members it answers `false` to: it took the call and did nothing.
    pub declines: Vec<&'static str>,
}

impl Node {
    /// A live, showing, enabled node of this role with nothing else.
    pub fn of(role: u32) -> Node {
        Node {
            role,
            name: String::new(),
            description: String::new(),
            states: vec![ENABLED, SENSITIVE, SHOWING, VISIBLE],
            interfaces: Vec::new(),
            children: Vec::new(),
            extents: (0, 0, 0, 0),
            actions: Vec::new(),
            text: String::new(),
            value: 0.0,
            life: Life::Alive,
            refuses: Vec::new(),
            toolkit: ("", ""),
            window_extents: None,
            range: (0.0, 100.0),
            flips: None,
            scrolls_to: None,
            caret: 0,
            selection: None,
            relations: Vec::new(),
            declines: Vec::new(),
        }
    }

    /// With these state bits set as well.
    pub fn reading(mut self, bits: &[u32]) -> Node {
        self.states.extend_from_slice(bits);
        self
    }

    /// Listing these interfaces as well, by their short names.
    pub fn listing(mut self, interfaces: &[&'static str]) -> Node {
        self.interfaces.extend_from_slice(interfaces);
        self
    }

    pub fn named(self, name: &str) -> Node {
        Node {
            name: name.to_string(),
            ..self
        }
    }

    /// On screen at this rectangle, with the Component interface that says so.
    pub fn at(mut self, x: i32, y: i32, w: i32, h: i32) -> Node {
        self.interfaces.push("Component");
        Node {
            extents: (x, y, w, h),
            ..self
        }
    }

    pub fn acting(mut self, actions: &[Result<&'static str, &'static str>]) -> Node {
        self.interfaces.push("Action");
        Node {
            actions: actions.to_vec(),
            ..self
        }
    }

    /// An application root made with this toolkit, with the Application interface
    /// that says so.
    pub fn made_with(mut self, name: &'static str, version: &'static str) -> Node {
        self.interfaces.push("Application");
        Node {
            toolkit: (name, version),
            ..self
        }
    }

    /// Children on the same bus as this node, by path.
    pub fn holding(self, bus: &str, paths: &[&str]) -> Node {
        Node {
            children: paths
                .iter()
                .map(|path| (bus.to_string(), path.to_string()))
                .collect(),
            ..self
        }
    }
}

/// The application most tests script, and its process.
pub const APP: &str = ":1.10";
pub const PID: i32 = 4711;

/// One application (`APP`, process `PID`) with one active window `/w` holding
/// `top`, and every other node at its path.
pub fn one_window(top: &[&str], nodes: Vec<(&str, Node)>) -> Desktop {
    let mut desktop = Desktop::with_applications(vec![(
        APP,
        PID as u32,
        Node::of(75).named("fixture").holding(APP, &["/w"]),
    )]);
    let window = Node {
        states: vec![ACTIVE, ENABLED, SENSITIVE, SHOWING, VISIBLE],
        ..Node::of(23)
            .named("Fixture")
            .at(0, 0, 800, 600)
            .holding(APP, top)
    };
    desktop.put(APP, "/w", window);
    for (path, node) in nodes {
        desktop.put(APP, path, node);
    }
    desktop
}

/// One call the fake received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub bus: String,
    pub path: String,
    pub interface: String,
    pub member: String,
    /// The first argument as text, where the call has one worth asserting on.
    pub argument: String,
    /// The call did not carry `NO_AUTO_START` (L1-5).
    pub auto_start: bool,
    /// The body's signature, without the parentheses a body of several arguments
    /// prints with.
    pub signature: String,
}

/// The whole scripted desktop, and what was asked of it.
#[derive(Default)]
pub struct Desktop {
    pub nodes: HashMap<(String, String), Node>,
    /// The process behind each application's bus name.
    pub pids: HashMap<String, u32>,
    /// Applications that have quit: every call to them is `ServiceUnknown`.
    pub quit: Vec<String>,
    /// What `org.a11y.Bus.GetAddress` answers, for the session-bus half.
    pub address: String,
    /// The systemd user manager's `Environment`, for the session-bus half.
    pub manager_environment: Vec<String>,
    pub calls: Vec<Call>,
}

impl Desktop {
    /// A registry with these applications on it, each (bus name, pid, root node).
    pub fn with_applications(applications: Vec<(&str, u32, Node)>) -> Desktop {
        let mut desktop = Desktop::default();
        let mut registry = Node::of(14);
        for (bus, pid, root) in applications {
            registry.children.push((bus.to_string(), ROOT.to_string()));
            desktop.pids.insert(bus.to_string(), pid);
            desktop.put(bus, ROOT, root);
        }
        desktop.put(REGISTRY, ROOT, registry);
        desktop
    }

    pub fn put(&mut self, bus: &str, path: &str, node: Node) {
        self.nodes.insert((bus.to_string(), path.to_string()), node);
    }
}

/// The desktop's end of the connection, serving on its own thread until the
/// reader's end goes away.
pub struct Fake {
    desktop: Arc<Mutex<Desktop>>,
    server: Connection,
    serving: Option<JoinHandle<()>>,
}

impl Fake {
    /// Serve `desktop`, and hand back the reader's end of the connection with
    /// `timeout` as every call's timeout.
    pub fn start(desktop: Desktop, timeout: Duration) -> (Fake, Connection) {
        let (ours, theirs) = UnixStream::pair().expect("a socket pair");
        let guid = zbus::Guid::generate();
        let server = Builder::async_io_unix_stream(ours)
            .server(guid)
            .expect("a server guid")
            .p2p();
        let client = Builder::async_io_unix_stream(theirs)
            .p2p()
            .method_timeout(timeout);
        let (server, client) =
            async_io::block_on(futures_util::future::join(server.build(), client.build()));
        let (server, client) = (
            server.expect("the fake's end"),
            client.expect("the reader's end"),
        );

        let desktop = Arc::new(Mutex::new(desktop));
        // Subscribed HERE, before the reader's end can send anything: a stream made
        // on the serving thread could miss a call sent before it existed.
        let calls = MessageStream::from(&server);
        let serving = {
            let (server, desktop) = (server.clone(), desktop.clone());
            std::thread::spawn(move || async_io::block_on(serve(calls, server, desktop)))
        };
        let fake = Fake {
            desktop,
            server,
            serving: Some(serving),
        };
        (fake, client)
    }

    fn desktop(&self) -> MutexGuard<'_, Desktop> {
        self.desktop.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every call received, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.desktop().calls.clone()
    }

    /// The calls of one member, as `path argument`, in order.
    pub fn sent(&self, member: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|call| call.member == member)
            .map(|call| format!("{} {}", call.path, call.argument))
            .collect()
    }

    /// What one node is scripted as now, writes included.
    pub fn node(&self, bus: &str, path: &str) -> Node {
        self.desktop()
            .nodes
            .get(&(bus.to_string(), path.to_string()))
            .expect("a scripted node")
            .clone()
    }

    /// Change what one node answers from now on.
    pub fn change(&self, bus: &str, path: &str, edit: impl FnOnce(&mut Node)) {
        let mut desktop = self.desktop();
        let node = desktop
            .nodes
            .get_mut(&(bus.to_string(), path.to_string()))
            .expect("a scripted node");
        edit(node);
    }

    /// The application on this bus quit.
    pub fn quits(&self, bus: &str) {
        self.desktop().quit.push(bus.to_string());
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        // Closing this end ends the serving loop's stream, so the join returns.
        if let Err(error) = async_io::block_on(self.server.clone().close()) {
            eprintln!("fake desktop: closing its end failed: {error}");
        }
        if let Some(serving) = self.serving.take() {
            if serving.join().is_err() {
                eprintln!("fake desktop: the serving thread panicked");
            }
        }
    }
}

/// Answer every call until the connection closes.
async fn serve(mut calls: MessageStream, server: Connection, desktop: Arc<Mutex<Desktop>>) {
    while let Some(Ok(message)) = calls.next().await {
        let header = message.header();
        if header.message_type() != Type::MethodCall {
            continue;
        }
        let reply = {
            let mut desktop = desktop.lock().unwrap_or_else(|e| e.into_inner());
            let call = call_of(&header, &message);
            desktop.calls.push(call.clone());
            match written(&mut desktop, &call, &message) {
                Some(reply) => reply,
                None => answer(&desktop, &call, &message),
            }
        };
        if let Err(error) = send(&server, &header, reply).await {
            eprintln!("fake desktop: a reply failed: {error}");
        }
    }
}

fn call_of(header: &Header<'_>, message: &Message) -> Call {
    let text = |part: Option<String>| part.unwrap_or_default();
    Call {
        bus: text(header.destination().map(|name| name.to_string())),
        path: text(header.path().map(|path| path.to_string())),
        interface: text(header.interface().map(|name| name.to_string())),
        member: text(header.member().map(|name| name.to_string())),
        argument: first_argument(message),
        auto_start: !header.primary().flags().contains(Flags::NoAutoStart),
        signature: signature_of(message),
    }
}

fn signature_of(message: &Message) -> String {
    let signature = message.body().signature().to_string();
    match signature.strip_prefix('(') {
        Some(inner) => inner.strip_suffix(')').unwrap_or(inner).to_string(),
        None => signature,
    }
}

/// The first argument, for the calls that have one a test asserts on.
fn first_argument(message: &Message) -> String {
    let body = message.body();
    match signature_of(message).chars().next() {
        Some('s') => body
            .deserialize::<(String,)>()
            .map(|(text,)| text)
            .or_else(|_| body.deserialize::<(String, String)>().map(|(_, name)| name))
            .or_else(|_| {
                body.deserialize::<(String, Vec<String>)>()
                    .map(|(event, _)| event)
            })
            .or_else(|_| {
                body.deserialize::<(String, Vec<String>, String)>()
                    .map(|(event, _, _)| event)
            })
            .unwrap_or_default(),
        Some('i') => body
            .deserialize::<(i32,)>()
            .map(|(index,)| index.to_string())
            .or_else(|_| {
                body.deserialize::<(i32, i32)>()
                    .map(|(start, _)| start.to_string())
            })
            .unwrap_or_default(),
        // A coordinate type (`GetExtents`) or a scroll type (`ScrollTo`).
        Some('u') => body
            .deserialize::<(u32,)>()
            .map(|(kind,)| kind.to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// What the desktop answers to one call.
enum Reply {
    Flag(bool),
    Range((i32, i32)),
    Count(i32),
    Relations(Vec<(u32, Vec<(String, String)>)>),
    Role(u32),
    Text(String),
    Words(Vec<u32>),
    Names(Vec<String>),
    Child(String, String),
    Children(Vec<(String, String)>),
    Extents((i32, i32, i32, i32)),
    Property(Value<'static>),
    Nothing,
    Error(&'static str),
    Silence,
}

/// A call that changes the desktop, or asks where a node sits: answered here, with
/// the desktop to change. `None` for every other call.
fn written(desktop: &mut Desktop, call: &Call, message: &Message) -> Option<Reply> {
    let writes = [
        "DoAction",
        "SetTextContents",
        "InsertText",
        "DeleteText",
        "GrabFocus",
        "SelectChild",
        "ScrollTo",
        "Set",
        "GetIndexInParent",
    ];
    let parent = call.member == "Get" && property_name(message) == "Parent";
    if !writes.contains(&call.member.as_str()) && !parent {
        return None;
    }
    if let Some(refusal) = unanswerable(desktop, call) {
        return Some(refusal);
    }
    let key = (call.bus.clone(), call.path.clone());
    if parent {
        return Some(match parent_of(desktop, &key) {
            Some((bus, at)) => Reply::Property(Value::from((bus, path(&at)))),
            None => Reply::Error(FAILED),
        });
    }
    if call.member == "GetIndexInParent" {
        return Some(match index_in_parent(desktop, &key) {
            Some(index) => Reply::Count(index),
            None => Reply::Count(-1),
        });
    }
    let declined = desktop.nodes[&key].declines.contains(&call.member.as_str());
    if declined {
        return Some(Reply::Flag(false));
    }
    Some(write(desktop, &key, call, message))
}

/// Why a call to this node gets no answer of its own, if it does not: its
/// application quit, it is not there, it is destroyed or silent, or it refuses.
fn unanswerable(desktop: &Desktop, call: &Call) -> Option<Reply> {
    if desktop.quit.contains(&call.bus) {
        return Some(Reply::Error(SERVICE_UNKNOWN));
    }
    let Some(node) = desktop.nodes.get(&(call.bus.clone(), call.path.clone())) else {
        return Some(Reply::Error(UNKNOWN_OBJECT));
    };
    match node.life {
        Life::Destroyed => return Some(Reply::Error(UNKNOWN_OBJECT)),
        Life::Silent => return Some(Reply::Silence),
        Life::Alive => {}
    }
    if node.refuses.contains(&call.member.as_str()) {
        return Some(Reply::Error(UNKNOWN_OBJECT));
    }
    None
}

/// One write, done to the scripted node, answered `true` as a toolkit does.
fn write(desktop: &mut Desktop, key: &(String, String), call: &Call, message: &Message) -> Reply {
    let body = message.body();
    match call.member.as_str() {
        "SelectChild" => {
            let Ok((index,)) = body.deserialize::<(i32,)>() else {
                return Reply::Error(FAILED);
            };
            select_child(desktop, key, index)
        }
        "Set" => {
            let Ok((_, name, value)) = body.deserialize::<(String, String, Value<'_>)>() else {
                return Reply::Error(FAILED);
            };
            let node = desktop.nodes.get_mut(key).expect("checked above");
            match (name.as_str(), f64::try_from(&value)) {
                ("CurrentValue", Ok(number)) => {
                    node.value = number;
                    Reply::Nothing
                }
                _ => Reply::Error(FAILED),
            }
        }
        _ => {
            let node = desktop.nodes.get_mut(key).expect("checked above");
            edit(node, call, message)
        }
    }
}

/// A write to the node itself.
fn edit(node: &mut Node, call: &Call, message: &Message) -> Reply {
    let body = message.body();
    match call.member.as_str() {
        "DoAction" => {
            if let Some(bit) = node.flips {
                flip(&mut node.states, bit);
            }
            Reply::Flag(true)
        }
        "SetTextContents" => match body.deserialize::<(String,)>() {
            Ok((text,)) => {
                node.text = text;
                Reply::Flag(true)
            }
            Err(_) => Reply::Error(FAILED),
        },
        "InsertText" => match body.deserialize::<(i32, String, i32)>() {
            Ok((at, text, _)) => {
                node.text = spliced(&node.text, at, at, &text);
                Reply::Flag(true)
            }
            Err(_) => Reply::Error(FAILED),
        },
        "DeleteText" => match body.deserialize::<(i32, i32)>() {
            Ok((start, end)) => {
                node.text = spliced(&node.text, start, end, "");
                node.selection = None;
                node.caret = start;
                Reply::Flag(true)
            }
            Err(_) => Reply::Error(FAILED),
        },
        "GrabFocus" => {
            if !node.states.contains(&FOCUSED) {
                node.states.push(FOCUSED);
            }
            Reply::Flag(true)
        }
        "ScrollTo" => {
            if let Some(extents) = node.scrolls_to {
                node.extents = extents;
            }
            Reply::Flag(true)
        }
        _ => Reply::Error(UNKNOWN_METHOD),
    }
}

/// `Selection.SelectChild(index)`: that child selected, every other one not.
fn select_child(desktop: &mut Desktop, key: &(String, String), index: i32) -> Reply {
    let children = desktop.nodes[key].children.clone();
    let Some(chosen) = usize::try_from(index).ok().and_then(|at| children.get(at)) else {
        return Reply::Flag(false);
    };
    for child in &children {
        if let Some(node) = desktop.nodes.get_mut(child) {
            node.states.retain(|bit| *bit != SELECTED);
            if child == chosen {
                node.states.push(SELECTED);
            }
        }
    }
    Reply::Flag(true)
}

fn flip(states: &mut Vec<u32>, bit: u32) {
    if states.contains(&bit) {
        states.retain(|set| *set != bit);
    } else {
        states.push(bit);
    }
}

/// `text` with the characters from `start` to `end` replaced by `with`.
fn spliced(text: &str, start: i32, end: i32, with: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let clamp = |at: i32| usize::try_from(at).unwrap_or(0).min(chars.len());
    let (start, end) = (clamp(start), clamp(end));
    let mut out: String = chars[..start].iter().collect();
    out.push_str(with);
    out.extend(chars[end.max(start)..].iter());
    out
}

/// The node that lists this one as a child.
fn parent_of(desktop: &Desktop, key: &(String, String)) -> Option<(String, String)> {
    desktop
        .nodes
        .iter()
        .find(|(_, node)| node.children.contains(key))
        .map(|(parent, _)| parent.clone())
}

fn index_in_parent(desktop: &Desktop, key: &(String, String)) -> Option<i32> {
    let parent = parent_of(desktop, key)?;
    let place = desktop.nodes[&parent]
        .children
        .iter()
        .position(|child| child == key)?;
    i32::try_from(place).ok()
}

fn property_name(message: &Message) -> String {
    message
        .body()
        .deserialize::<(String, String)>()
        .map(|(_, name)| name)
        .unwrap_or_default()
}

fn answer(desktop: &Desktop, call: &Call, message: &Message) -> Reply {
    if call.bus == "org.freedesktop.systemd1" {
        return match call.argument.as_str() {
            "Environment" => Reply::Property(Value::from(desktop.manager_environment.clone())),
            _ => Reply::Error(UNKNOWN_METHOD),
        };
    }
    match call.interface.as_str() {
        "org.a11y.Bus" => Reply::Text(desktop.address.clone()),
        "org.a11y.atspi.Registry" => registration(call),
        "org.freedesktop.DBus" => process_id(desktop, &call.argument),
        _ => from_node(desktop, call, message),
    }
}

/// The registry of at-spi2-core 2.44 (Ubuntu 22.04) takes `RegisterEvent` as `s` or
/// `sas` only, and leaves any other form unanswered after a warning; later ones
/// also take `sass`.
fn registration(call: &Call) -> Reply {
    match call.signature.as_str() {
        "s" | "sas" => Reply::Nothing,
        _ => Reply::Silence,
    }
}

fn process_id(desktop: &Desktop, bus: &str) -> Reply {
    match desktop.pids.get(bus) {
        Some(pid) if !desktop.quit.iter().any(|gone| gone == bus) => Reply::Role(*pid),
        _ => Reply::Error("org.freedesktop.DBus.Error.NameHasNoOwner"),
    }
}

/// A call to one accessible: what its life, its refusals and its script answer.
fn from_node(desktop: &Desktop, call: &Call, message: &Message) -> Reply {
    if desktop.quit.contains(&call.bus) {
        return Reply::Error(SERVICE_UNKNOWN);
    }
    let Some(node) = desktop.nodes.get(&(call.bus.clone(), call.path.clone())) else {
        return Reply::Error(UNKNOWN_OBJECT);
    };
    match node.life {
        Life::Destroyed => return Reply::Error(UNKNOWN_OBJECT),
        Life::Silent => return Reply::Silence,
        Life::Alive => {}
    }
    if node.refuses.contains(&call.member.as_str())
        || node.refuses.contains(&call.argument.as_str())
    {
        return Reply::Error(UNKNOWN_OBJECT);
    }
    match call.member.as_str() {
        "Get" => property(node, message),
        "GetAll" => Reply::Error(FAILED),
        _ => method(node, call),
    }
}

fn method(node: &Node, call: &Call) -> Reply {
    let index: usize = call.argument.parse().unwrap_or(usize::MAX);
    match call.member.as_str() {
        "GetRole" => Reply::Role(node.role),
        "GetState" => Reply::Words(state_words(&node.states)),
        "GetInterfaces" => Reply::Names(interfaces(node)),
        "GetChildAtIndex" => match node.children.get(index) {
            Some((bus, path)) => Reply::Child(bus.clone(), path.clone()),
            None => Reply::Error(FAILED),
        },
        "GetChildren" => Reply::Children(node.children.clone()),
        "GetExtents" if call.argument == "1" => {
            Reply::Extents(node.window_extents.unwrap_or(node.extents))
        }
        "GetExtents" => Reply::Extents(node.extents),
        "GetRelationSet" => Reply::Relations(node.relations.clone()),
        "GetNSelections" => Reply::Count(i32::from(node.selection.is_some())),
        "GetSelection" => match node.selection {
            Some(range) if index == 0 => Reply::Range(range),
            _ => Reply::Error(FAILED),
        },
        "GetName" => match node.actions.get(index) {
            Some(Ok(name)) => Reply::Text(name.to_string()),
            Some(Err(error)) => Reply::Error(error),
            None => Reply::Error(FAILED),
        },
        // Answered like ATK bridge 2.38 would NOT (it aborts the application);
        // what matters is that the call is recorded.
        "GetActions" => Reply::Error(FAILED),
        "GetText" => Reply::Text(node.text.clone()),
        _ => Reply::Error(UNKNOWN_METHOD),
    }
}

fn property(node: &Node, message: &Message) -> Reply {
    let Ok((interface, name)) = message.body().deserialize::<(String, String)>() else {
        return Reply::Error(FAILED);
    };
    let short = interface.trim_start_matches("org.a11y.atspi.");
    if short != "Accessible" && !node.interfaces.contains(&short) {
        return Reply::Error(UNKNOWN_OBJECT);
    }
    let value = match name.as_str() {
        "Name" => Value::from(node.name.clone()),
        "Description" => Value::from(node.description.clone()),
        "ChildCount" => Value::from(node.children.len() as i32),
        "NActions" => Value::from(node.actions.len() as i32),
        "CurrentValue" => Value::from(node.value),
        "MinimumValue" => Value::from(node.range.0),
        "MaximumValue" => Value::from(node.range.1),
        "CaretOffset" => Value::from(node.caret),
        "ToolkitName" => Value::from(node.toolkit.0),
        "Version" => Value::from(node.toolkit.1),
        _ => return Reply::Error(UNKNOWN_OBJECT),
    };
    Reply::Property(value)
}

fn interfaces(node: &Node) -> Vec<String> {
    std::iter::once("Accessible")
        .chain(node.interfaces.iter().copied())
        .map(|name| format!("org.a11y.atspi.{name}"))
        .collect()
}

/// The state set as the bus carries it: bit `n` is bit `n % 32` of word `n / 32`.
fn state_words(bits: &[u32]) -> Vec<u32> {
    let mut words = vec![0u32, 0u32];
    for bit in bits {
        words[(*bit / 32) as usize] |= 1 << (bit % 32);
    }
    words
}

fn path(text: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(text.to_string()).expect("a scripted path")
}

/// Send one reply with the signature AT-SPI's own has: `GetChildAtIndex` and
/// `GetExtents` answer ONE struct, not its fields as arguments.
async fn send(server: &Connection, header: &Header<'_>, reply: Reply) -> zbus::Result<()> {
    match reply {
        Reply::Flag(flag) => server.reply(header, &flag).await,
        Reply::Range(range) => server.reply(header, &range).await,
        Reply::Count(count) => server.reply(header, &count).await,
        Reply::Relations(relations) => {
            let relations: Vec<(u32, Vec<(String, OwnedObjectPath)>)> = relations
                .into_iter()
                .map(|(kind, targets)| {
                    let targets = targets.into_iter().map(|(bus, at)| (bus, path(&at)));
                    (kind, targets.collect())
                })
                .collect();
            server.reply(header, &relations).await
        }
        Reply::Role(number) => server.reply(header, &number).await,
        Reply::Text(text) => server.reply(header, &text).await,
        Reply::Words(words) => server.reply(header, &words).await,
        Reply::Names(names) => server.reply(header, &names).await,
        Reply::Child(bus, at) => server.reply(header, &((bus, path(&at)),)).await,
        Reply::Children(list) => {
            let list: Vec<(String, OwnedObjectPath)> =
                list.into_iter().map(|(bus, at)| (bus, path(&at))).collect();
            server.reply(header, &list).await
        }
        Reply::Extents(extents) => server.reply(header, &(extents,)).await,
        Reply::Property(value) => server.reply(header, &value).await,
        Reply::Nothing => server.reply(header, &()).await,
        Reply::Error(name) => server.reply_error(header, name, &("scripted",)).await,
        Reply::Silence => Ok(()),
    }
}
