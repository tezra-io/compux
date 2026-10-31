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
pub const EDITABLE: u32 = 7;
pub const ENABLED: u32 = 8;
pub const SENSITIVE: u32 = 24;
pub const SHOWING: u32 = 25;
pub const VISIBLE: u32 = 30;

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
        }
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
            answer(&desktop, &call, &message)
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
        _ => String::new(),
    }
}

/// What the desktop answers to one call.
enum Reply {
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

fn answer(desktop: &Desktop, call: &Call, message: &Message) -> Reply {
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
        "GetExtents" => Reply::Extents(node.extents),
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
