//! A fake XDG desktop portal, for the portal route's unit tests.
//!
//! One peer-to-peer D-Bus connection, no bus daemon and no desktop, as the fake
//! AT-SPI desktop (`fake`) is. It answers the calls compux makes of the portal
//! frontend (the Registry, Screenshot, RemoteDesktop, ScreenCast, Request and
//! Session closes), of the session bus (`NameHasOwner`, `GetNameOwner`, the
//! portal's `Introspect`) and of the PermissionStore (`Lookup`), from a script,
//! and it RECORDS every call in order, so a test asserts on what was asked: that
//! the app id was registered before anything else, which restore token was
//! offered, which keysym went out first, which call may start a service.
//!
//! A portal or store the script says is not running answers a call that carries
//! `NO_AUTO_START` as dbus-daemon does, `NameHasNoOwner` ("Name … does not
//! exist"); one without the flag starts it, as the bus would, and is answered.
//!
//! A Request answers as the portal's do: the call returns its object path at
//! once, and the `Response` signal on that path follows, with the scripted code,
//! or never.
//!
//! On a bus, dbus-daemon stamps every message with its sender's unique name; a
//! peer-to-peer connection stamps nothing, so the fake does. The portal's own
//! replies and signals come from [`PORTAL`], and a script can have another
//! process on the same bus, [`FORGER`], answer first.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::StreamExt;
use zbus::connection::Builder;
use zbus::message::{Body, Flags, Header, Type};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Message, MessageStream};

/// The PermissionStore's writes: compux reads a stored answer and never changes
/// one.
const FORBIDDEN: [&str; 5] = [
    "Set",
    "SetValue",
    "SetPermission",
    "Delete",
    "DeletePermission",
];

/// How long compux's end waits on any one call before failing it.
const METHOD_TIMEOUT: Duration = Duration::from_secs(5);

/// The portal's unique name on the fake bus.
pub const PORTAL: &str = ":1.42";

/// Another process on the same session bus, which can send any signal on any
/// path.
pub const FORGER: &str = ":1.66";

const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";

/// How a Request is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// `Response` with this code: 0 granted, 1 refused, 2 ended.
    Respond(u32),
    /// No `Response` at all: the dialog is never answered.
    Never,
}

/// What the fake portal does.
#[derive(Clone, Debug)]
pub struct Script {
    /// Whether `org.freedesktop.host.portal.Registry` answers `Register`.
    pub registry: bool,
    pub screenshot: Answer,
    /// The file a granted screenshot names.
    pub screenshot_uri: String,
    pub start: Answer,
    /// The monitor stream a started session names: node, position, size.
    pub stream: (u32, (i32, i32), (i32, i32)),
    /// The restore token each granted start hands out, numbered.
    pub token_prefix: String,
    /// The session bus names that have an owner.
    pub owned: Vec<&'static str>,
    /// The interfaces the portal object introspects as.
    pub interfaces: Vec<&'static str>,
    /// The PermissionStore's `screenshot` table: app id to answer; `None` is no
    /// table at all.
    pub stored: Option<Vec<(&'static str, &'static str)>>,
    /// Whether the portal frontend runs before anything starts it.
    pub portal_running: bool,
    /// Whether the PermissionStore runs before anything starts it.
    pub store_running: bool,
    /// A session set-up step (`CreateSession`, `SelectDevices`, `SelectSources`)
    /// whose Request is never answered.
    pub silent_step: Option<&'static str>,
    /// The unique name the bus says owns the portal (`GetNameOwner`).
    pub owner: &'static str,
    /// A method (`Screenshot` or `Start`) whose Request first gets a Response 0
    /// from [`FORGER`], before the portal's own.
    pub forged: Option<&'static str>,
    /// The file a forged Screenshot Response names.
    pub forged_uri: String,
    /// An error the portal answers its `Introspect` with, running or not.
    pub introspect_error: Option<&'static str>,
}

impl Default for Script {
    fn default() -> Script {
        Script {
            registry: true,
            screenshot: Answer::Respond(0),
            screenshot_uri: String::new(),
            start: Answer::Respond(0),
            stream: (42, (0, 0), (1280, 800)),
            token_prefix: "token".to_string(),
            owned: vec!["org.gnome.Shell"],
            interfaces: vec![
                "org.freedesktop.host.portal.Registry",
                "org.freedesktop.portal.Screenshot",
                "org.freedesktop.portal.RemoteDesktop",
                "org.freedesktop.portal.ScreenCast",
            ],
            stored: None,
            portal_running: true,
            store_running: true,
            silent_step: None,
            owner: PORTAL,
            forged: None,
            forged_uri: String::new(),
            introspect_error: None,
        }
    }
}

/// One call the fake was sent.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub interface: String,
    pub member: String,
    pub path: String,
    /// `Register`'s app id; `NotifyKeyboardKeysym`'s keysym and state;
    /// `NotifyPointerMotionAbsolute`'s stream and point; what a selection asked
    /// for, the restore token among it.
    pub detail: String,
    /// The call did not carry `NO_AUTO_START`: it may start the service.
    pub auto_start: bool,
}

struct State {
    script: Script,
    calls: Vec<Call>,
    sessions: Vec<String>,
    tokens: u32,
}

/// The fake portal's end of the connection, and what it recorded.
pub struct FakePortal {
    state: Arc<Mutex<State>>,
    server: Connection,
    serving: Option<JoinHandle<()>>,
}

impl FakePortal {
    /// The fake, and compux's end of the connection to it.
    pub fn start(script: Script) -> (FakePortal, Connection) {
        let (ours, theirs) = UnixStream::pair().expect("a socket pair");
        let guid = zbus::Guid::generate();
        let server = Builder::async_io_unix_stream(ours)
            .server(guid)
            .expect("a server guid")
            .p2p();
        let client = Builder::async_io_unix_stream(theirs)
            .p2p()
            .method_timeout(METHOD_TIMEOUT);
        let (server, client) =
            async_io::block_on(futures_util::future::join(server.build(), client.build()));
        let (server, client) = (
            server.expect("the fake's end"),
            client.expect("compux's end"),
        );
        let state = Arc::new(Mutex::new(State {
            script,
            calls: Vec::new(),
            sessions: Vec::new(),
            tokens: 0,
        }));
        let calls = MessageStream::from(&server);
        let serving = {
            let (server, state) = (server.clone(), Arc::clone(&state));
            std::thread::spawn(move || async_io::block_on(serve(calls, server, state)))
        };
        let fake = FakePortal {
            state,
            server,
            serving: Some(serving),
        };
        (fake, client)
    }

    pub fn calls(&self) -> Vec<Call> {
        lock(&self.state).calls.clone()
    }

    /// The members called, in order.
    pub fn members(&self) -> Vec<String> {
        self.calls().into_iter().map(|call| call.member).collect()
    }

    /// The person stops the last session from the shell: its `Closed` signal.
    pub fn close_session(&self) {
        self.closed_by(PORTAL);
    }

    /// Another process sends the last session's `Closed` signal.
    pub fn forge_close(&self) {
        self.closed_by(FORGER);
    }

    fn closed_by(&self, sender: &'static str) {
        let session = lock(&self.state)
            .sessions
            .last()
            .cloned()
            .expect("a session");
        let options: (HashMap<&str, Value<'_>>,) = (HashMap::new(),);
        let closed = signal_from(&self.server, sender, &session, SESSION, "Closed", &options);
        async_io::block_on(closed).expect("the Closed signal");
    }
}

impl Drop for FakePortal {
    fn drop(&mut self) {
        // Closing this end ends the serving loop's stream, so the join returns.
        if let Err(error) = async_io::block_on(self.server.clone().close()) {
            eprintln!("fake portal: closing its end failed: {error}");
        }
        if let Some(serving) = self.serving.take() {
            if serving.join().is_err() {
                eprintln!("fake portal: the serving thread panicked");
            }
        }
        let forbidden: Vec<String> = self
            .calls()
            .into_iter()
            .filter(|call| FORBIDDEN.contains(&call.member.as_str()))
            .map(|call| format!("{}.{} on {}", call.interface, call.member, call.path))
            .collect();
        // A test already failing says why itself; a second panic would abort.
        if !forbidden.is_empty() && !std::thread::panicking() {
            panic!("the fake portal was sent what compux must never send: {forbidden:?}");
        }
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

async fn serve(mut calls: MessageStream, server: Connection, state: Arc<Mutex<State>>) {
    while let Some(Ok(message)) = calls.next().await {
        let header = message.header();
        if header.message_type() != Type::MethodCall {
            continue;
        }
        if let Err(error) = answer(&server, &header, &message, &state).await {
            eprintln!("fake portal: a reply failed: {error}");
        }
    }
}

fn text(part: Option<String>) -> String {
    part.unwrap_or_default()
}

fn option_text(options: &HashMap<String, OwnedValue>, key: &str) -> String {
    match options.get(key).map(|value| &**value) {
        Some(Value::Str(text)) => text.to_string(),
        _ => String::new(),
    }
}

/// One call: what it asked is recorded, then it is answered as scripted.
async fn answer(
    server: &Connection,
    header: &Header<'_>,
    message: &Message,
    state: &Mutex<State>,
) -> zbus::Result<()> {
    let member = text(header.member().map(|name| name.to_string()));
    let call = Call {
        interface: text(header.interface().map(|name| name.to_string())),
        member: member.clone(),
        path: text(header.path().map(|path| path.to_string())),
        detail: String::new(),
        auto_start: !header.primary().flags().contains(Flags::NoAutoStart),
    };
    let body = message.body();
    match member.as_str() {
        "NameHasOwner" | "GetNameOwner" | "Introspect" | "Lookup" => {
            let stopped = not_started(&call, &lock(state).script);
            record(state, call);
            if stopped {
                let nobody = "org.freedesktop.DBus.Error.NameHasNoOwner";
                return server.reply_error(header, nobody, &("not running",)).await;
            }
            answer_bus(server, header, &body, state).await
        }
        "Register" => register(server, header, &body, state, call).await,
        "Screenshot" => screenshot(server, header, &body, state, call).await,
        "CreateSession" | "SelectDevices" | "SelectSources" => {
            set_up(server, header, &body, state, call).await
        }
        "Start" => start(server, header, &body, state, call).await,
        _ => input(server, header, &body, state, call).await,
    }
}

/// Whether a read goes to a service that is not running and may not start it.
fn not_started(call: &Call, script: &Script) -> bool {
    let running = match call.member.as_str() {
        "Introspect" => script.portal_running,
        "Lookup" => script.store_running,
        _ => true,
    };
    !running && !call.auto_start
}

/// The session bus's and the PermissionStore's answers: who owns a name, what
/// the portal introspects as, and the stored Screenshot table.
async fn answer_bus(
    server: &Connection,
    header: &Header<'_>,
    body: &Body,
    state: &Mutex<State>,
) -> zbus::Result<()> {
    let script = lock(state).script.clone();
    match header.member().map(|name| name.as_str()) {
        Some("NameHasOwner") => {
            let (name,): (String,) = body.deserialize()?;
            let owned = script.owned.iter().any(|owner| *owner == name);
            server.reply(header, &owned).await
        }
        Some("GetNameOwner") => server.reply(header, &script.owner).await,
        Some("Introspect") if script.introspect_error.is_some() => {
            let error = script.introspect_error.unwrap_or_default();
            server.reply_error(header, error, &("refused",)).await
        }
        Some("Introspect") => {
            let xml: String = script
                .interfaces
                .iter()
                .map(|name| format!("<interface name=\"{name}\"></interface>"))
                .collect();
            server.reply(header, &format!("<node>{xml}</node>")).await
        }
        _ => {
            let Some(entries) = script.stored else {
                let not_found = "org.freedesktop.portal.Error.NotFound";
                return server.reply_error(header, not_found, &("no table",)).await;
            };
            let table: HashMap<String, Vec<String>> = entries
                .into_iter()
                .map(|(app, answer)| (app.to_string(), vec![answer.to_string()]))
                .collect();
            server.reply(header, &(table, Value::from(0u8))).await
        }
    }
}

async fn register(
    server: &Connection,
    header: &Header<'_>,
    body: &Body,
    state: &Mutex<State>,
    mut call: Call,
) -> zbus::Result<()> {
    let (app_id, _options): (String, HashMap<String, OwnedValue>) = body.deserialize()?;
    call.detail = app_id;
    let registry = lock(state).script.registry;
    record(state, call);
    if registry {
        return server.reply(header, &()).await;
    }
    let unknown = "org.freedesktop.DBus.Error.UnknownInterface";
    server
        .reply_error(header, unknown, &("no such interface",))
        .await
}

async fn screenshot(
    server: &Connection,
    header: &Header<'_>,
    body: &Body,
    state: &Mutex<State>,
    mut call: Call,
) -> zbus::Result<()> {
    let (_parent, options): (String, HashMap<String, OwnedValue>) = body.deserialize()?;
    call.detail = format!("interactive={:?}", options.get("interactive").map(|v| &**v));
    record(state, call);
    let (answer, uri, forged) = {
        let state = lock(state);
        let script = &state.script;
        let forged = (script.forged == Some("Screenshot")).then(|| script.forged_uri.clone());
        (script.screenshot, script.screenshot_uri.clone(), forged)
    };
    let results = HashMap::from([("uri", Value::from(uri))]);
    let forged = forged.map(|uri| HashMap::from([("uri", Value::from(uri))]));
    let token = option_text(&options, "handle_token");
    request(server, header, &token, answer, results, forged).await
}

/// CreateSession and the two selections: granted, the session's handle named
/// after its token, unless the script leaves one unanswered.
async fn set_up(
    server: &Connection,
    header: &Header<'_>,
    body: &Body,
    state: &Mutex<State>,
    mut call: Call,
) -> zbus::Result<()> {
    let mut results: HashMap<&str, Value<'_>> = HashMap::new();
    let options: HashMap<String, OwnedValue> = if call.member == "CreateSession" {
        let (options,): (HashMap<String, OwnedValue>,) = body.deserialize()?;
        let session = format!(
            "/org/freedesktop/portal/desktop/session/fake/{}",
            option_text(&options, "session_handle_token")
        );
        lock(state).sessions.push(session.clone());
        results.insert("session_handle", Value::from(session));
        options
    } else {
        let (_session, options): (OwnedObjectPath, HashMap<String, OwnedValue>) =
            body.deserialize()?;
        call.detail = selected(&options);
        options
    };
    let silent = lock(state).script.silent_step == Some(call.member.as_str());
    record(state, call);
    let answer = if silent {
        Answer::Never
    } else {
        Answer::Respond(0)
    };
    let token = option_text(&options, "handle_token");
    request(server, header, &token, answer, results, None).await
}

/// Start: the scripted answer, one monitor stream, and the next restore token.
async fn start(
    server: &Connection,
    header: &Header<'_>,
    body: &Body,
    state: &Mutex<State>,
    call: Call,
) -> zbus::Result<()> {
    let (_session, _parent, options): (OwnedObjectPath, String, HashMap<String, OwnedValue>) =
        body.deserialize()?;
    record(state, call);
    let (answer, stream, token, forged) = {
        let mut state = lock(state);
        state.tokens += 1;
        let token = format!("{}-{}", state.script.token_prefix, state.tokens);
        let forged = state.script.forged == Some("Start");
        (state.script.start, state.script.stream, token, forged)
    };
    let results = started(stream, token);
    let forged = forged.then(|| started(stream, "forged".to_string()));
    let token = option_text(&options, "handle_token");
    request(server, header, &token, answer, results, forged).await
}

/// What a granted Start names: keyboard and pointer, the monitor stream, and a
/// restore token.
fn started(
    (node, position, size): (u32, (i32, i32), (i32, i32)),
    token: String,
) -> HashMap<&'static str, Value<'static>> {
    let properties = HashMap::from([
        ("position", Value::from(position)),
        ("size", Value::from(size)),
    ]);
    HashMap::from([
        ("devices", Value::from(3u32)),
        ("streams", Value::from(vec![(node, properties)])),
        ("restore_token", Value::from(token)),
    ])
}

/// The input calls, each with what it sent as its detail, and any other call
/// (the closes) as it is.
async fn input(
    server: &Connection,
    header: &Header<'_>,
    body: &Body,
    state: &Mutex<State>,
    mut call: Call,
) -> zbus::Result<()> {
    type Options = HashMap<String, OwnedValue>;
    call.detail = match call.member.as_str() {
        "NotifyKeyboardKeysym" | "NotifyPointerButton" => {
            let (_s, _o, code, pressed): (OwnedObjectPath, Options, i32, u32) =
                body.deserialize()?;
            format!("{code:#x} {pressed}")
        }
        "NotifyPointerMotionAbsolute" => {
            let (_s, _o, node, x, y): (OwnedObjectPath, Options, u32, f64, f64) =
                body.deserialize()?;
            format!("{node} {x} {y}")
        }
        _ => String::new(),
    };
    record(state, call);
    server.reply(header, &()).await
}

/// What a selection asked for: its types, how long to keep the grant, and the
/// restore token offered.
fn selected(options: &HashMap<String, OwnedValue>) -> String {
    let number = |key: &str| match options.get(key).map(|value| &**value) {
        Some(Value::U32(number)) => number.to_string(),
        _ => "-".to_string(),
    };
    format!(
        "types={} persist_mode={} restore_token={}",
        number("types"),
        number("persist_mode"),
        option_text(options, "restore_token")
    )
}

fn record(state: &Mutex<State>, call: Call) {
    lock(state).calls.push(call);
}

/// Answer with the Request's path, then its Response as scripted; a `forged`
/// Response from [`FORGER`] goes first.
async fn request(
    server: &Connection,
    header: &Header<'_>,
    token: &str,
    answer: Answer,
    results: HashMap<&str, Value<'_>>,
    forged: Option<HashMap<&str, Value<'_>>>,
) -> zbus::Result<()> {
    let path = format!("/org/freedesktop/portal/desktop/request/fake/{token}");
    let handle = OwnedObjectPath::try_from(path.as_str()).expect("a request path");
    let reply = Message::method_return(header)?
        .sender(PORTAL)?
        .build(&handle)?;
    server.send(&reply).await?;
    if let Some(forged) = forged {
        signal_from(server, FORGER, &path, REQUEST, "Response", &(0u32, forged)).await?;
    }
    let Answer::Respond(code) = answer else {
        return Ok(());
    };
    signal_from(server, PORTAL, &path, REQUEST, "Response", &(code, results)).await
}

/// A signal stamped with `sender`, as the bus would deliver it.
async fn signal_from<B>(
    server: &Connection,
    sender: &str,
    path: &str,
    interface: &str,
    member: &str,
    body: &B,
) -> zbus::Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let signal = Message::signal(path, interface, member)?
        .sender(sender)?
        .build(body)?;
    server.send(&signal).await
}
