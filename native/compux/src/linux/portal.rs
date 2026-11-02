//! GNOME Wayland pixels through the XDG desktop portal (M86 L2), over zbus.
//!
//! - **Its own name.** Before any other portal call the connection registers as
//!   [`APP_ID`] with `org.freedesktop.host.portal.Registry`. Without that a host
//!   process has the empty app id, which every unsandboxed process shares: one
//!   Allow was seen to grant them all, and anyone's Deny would refuse compux.
//!   A portal with no Registry (xdg-desktop-portal before 1.18) is never used; the
//!   session's pixels are refused instead (`pixels`).
//! - **Only the portal's answers.** Any process on the session bus can send a
//!   signal on any path. A Request's `Response` and a session's `Closed` are
//!   taken only from the unique name that owns the portal when the call is
//!   made, so no other process can name a file for compux to read and remove,
//!   or grant what the person refused.
//! - **Screenshot**, `interactive: false`. Response 0 is the image, read from
//!   the regular file the portal saved (never through a link), which is
//!   removed once it has decoded; 1 is a refusal (the person's, now or stored:
//!   `consent_refused`); 2, or no answer within the consent budget, is
//!   `consent_timeout`. compux never writes the PermissionStore: a stored no is
//!   the person's to reset.
//! - **Input**: one RemoteDesktop session with keyboard and pointer, and a
//!   ScreenCast monitor source that says where the picked monitor is. Asked for
//!   with `persist_mode` 2; the restore token is kept in compux's own state file,
//!   used once, and replaced by the next. Keys go as keysyms, the pointer as
//!   absolute motion in the stream's own coordinates. compux reads no frame from
//!   any stream. The set-up steps before Start show nothing, so each has a short
//!   budget of its own ([`STEP_BUDGET`]); a portal silent through one is
//!   `capture_unavailable`, never the person's silence.
//! - **Its life.** The session is kept for the helper's life, closed on release
//!   and at exit. Closed by the person (the shell's Stop), it is gone, and so is
//!   its token: the next input asks again.

use std::collections::HashMap;
use std::fs::Metadata;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, Message, MessageStream};

/// The app id compux registers, and the one every grant is kept under.
pub const APP_ID: &str = "io.tezra.fermix.ComputerUse";

/// How long a portal dialog is waited on: GNOME's portal was seen to end an
/// unanswered Screenshot dialog itself, with Response 2, at 25 s.
pub const CONSENT_BUDGET: Duration = Duration::from_secs(25);

/// How long a session set-up step that shows nothing (`CreateSession`,
/// `SelectDevices`, `SelectSources`) is waited on. Past it the portal is not
/// answering, which is `capture_unavailable`, not the person's silence.
pub const STEP_BUDGET: Duration = Duration::from_secs(5);

/// How long the portal is waited on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budgets {
    /// A call that may show the person a dialog: `Screenshot` and `Start`.
    pub consent: Duration,
    /// A set-up step, which shows nothing.
    pub step: Duration,
}

impl Budgets {
    /// The portal's own: the consent budget and the set-up step budget.
    pub const PORTAL: Budgets = Budgets {
        consent: CONSENT_BUDGET,
        step: STEP_BUDGET,
    };
}

const DESKTOP: &str = "org.freedesktop.portal.Desktop";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
pub const REGISTRY: &str = "org.freedesktop.host.portal.Registry";
pub const SCREENSHOT: &str = "org.freedesktop.portal.Screenshot";
pub const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
pub const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";

/// RemoteDesktop's device types: keyboard 1, pointer 2.
const KEYBOARD_AND_POINTER: u32 = 3;
/// ScreenCast's source type for a monitor.
const MONITOR: u32 = 1;
/// Keep the grant until the person revokes it.
const PERSIST_UNTIL_REVOKED: u32 = 2;

/// evdev's button codes, which the portal takes.
pub const BTN_LEFT: i32 = 0x110;
pub const BTN_RIGHT: i32 = 0x111;

/// `Shift_L`, tapped once before a session's first key: the first key a
/// remote-desktop session posted was seen dropped, and a lead Shift tap absorbed
/// it.
pub const SHIFT_L: i32 = 0xffe1;

/// The most signals queued while a dialog is open.
const MAX_QUEUED_SIGNALS: usize = 64;

/// The largest screenshot file read.
const MAX_SCREENSHOT_BYTES: u64 = 256 << 20;

/// Why a portal call did not give what was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Response 1: refused, now or by a stored answer.
    Refused(String),
    /// Response 2, or nothing within the budget.
    TimedOut(String),
    /// The call itself failed: no portal, an error, an unreadable answer.
    Failed(String),
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Refused(_) => "consent_refused",
            Refusal::TimedOut(_) => "consent_timeout",
            Refusal::Failed(_) => "capture_unavailable",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Refusal::Refused(why) | Refusal::TimedOut(why) | Refusal::Failed(why) => why,
        }
    }
}

/// Where the picked monitor's stream is, in the compositor's logical space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stream {
    pub node: u32,
    pub position: (i32, i32),
    pub size: (i32, i32),
}

/// One live RemoteDesktop session.
struct Session {
    handle: OwnedObjectPath,
    stream: Stream,
    /// The portal's `Closed` signal for it, from the portal's owner alone, read
    /// before each use.
    closed: MessageStream,
    keyed: bool,
}

/// The portal, for one helper: its registered connection, made on first use, and
/// the session, started on first input.
pub struct Portal {
    connection: Option<Connection>,
    connect: Box<dyn Fn() -> Result<Connection, String>>,
    session: Option<Session>,
    state_file: Option<PathBuf>,
    budgets: Budgets,
    next_token: u32,
}

impl Portal {
    /// On the session bus, with the restore token kept in `state_file`.
    #[cfg(not(test))]
    pub fn on_session_bus(state_file: Option<PathBuf>) -> Portal {
        Portal::with(
            Box::new(|| async_io::block_on(crate::linux::bus::session())),
            state_file,
            Budgets::PORTAL,
        )
    }

    /// Over any connection `connect` makes, waiting `budgets` on it.
    pub fn with(
        connect: Box<dyn Fn() -> Result<Connection, String>>,
        state_file: Option<PathBuf>,
        budgets: Budgets,
    ) -> Portal {
        Portal {
            connection: None,
            connect,
            session: None,
            state_file,
            budgets,
            next_token: 0,
        }
    }

    /// The connection, registered as compux's app id before anything else is
    /// asked of the portal on it.
    fn connection(&mut self) -> Result<Connection, Refusal> {
        if let Some(connection) = &self.connection {
            return Ok(connection.clone());
        }
        let connection = (self.connect)().map_err(Refusal::Failed)?;
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        async_io::block_on(connection.call_method(
            Some(DESKTOP),
            DESKTOP_PATH,
            Some(REGISTRY),
            "Register",
            &(APP_ID, options),
        ))
        .map_err(|error| {
            Refusal::Failed(format!(
                "the portal would not register compux as {APP_ID}, so it is not asked anything \
                 else: {error}"
            ))
        })?;
        self.connection = Some(connection.clone());
        Ok(connection)
    }

    fn token(&mut self) -> String {
        self.next_token = self.next_token.wrapping_add(1);
        format!("compux{}_{}", std::process::id(), self.next_token)
    }

    /// The whole desktop, as the portal's screenshot file holds it.
    pub fn screenshot(&mut self) -> Result<image::RgbaImage, Refusal> {
        let connection = self.connection()?;
        let token = self.token();
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert("interactive", Value::from(false));
        options.insert("modal", Value::from(false));
        let results = async_io::block_on(ask(
            &connection,
            SCREENSHOT,
            "Screenshot",
            &("", options),
            Wait::Consent(self.budgets.consent),
        ))?;
        let uri = text(&results, "uri")
            .ok_or_else(|| Refusal::Failed("the portal's screenshot named no file".to_string()))?;
        read_screenshot(&uri)
    }

    /// The live session, started now if there is none; the dialog shows only
    /// where the restore token no longer serves.
    pub fn session(&mut self) -> Result<Stream, Refusal> {
        if self
            .session
            .as_mut()
            .is_some_and(|session| closed(&mut session.closed))
        {
            // The person stopped it: forget it and its token, so the next one asks.
            self.session = None;
            self.store_token(None);
        }
        if let Some(session) = &self.session {
            return Ok(session.stream);
        }
        let session = self.start()?;
        let stream = session.stream;
        self.session = Some(session);
        Ok(stream)
    }

    /// Create a session, offer the stored restore token, pick a monitor and
    /// start it. A restore token is used once: whatever the start said, the one
    /// offered is spent, and the one the start handed back replaces it. A
    /// session that did not start is closed.
    fn start(&mut self) -> Result<Session, Refusal> {
        let connection = self.connection()?;
        let tokens = Tokens {
            create: self.token(),
            session: self.token(),
            devices: self.token(),
            sources: self.token(),
            start: self.token(),
        };
        let restore = self.stored_token();
        let budgets = self.budgets;
        let (handle, closed) = async_io::block_on(create(&connection, &tokens, budgets.step))?;
        let started = async_io::block_on(begin(
            &connection,
            &handle,
            &tokens,
            restore.as_deref(),
            budgets,
        ));
        let started = match started {
            Ok(started) => started,
            Err(refusal) => {
                self.store_token(None);
                async_io::block_on(close_session(&connection, &handle));
                return Err(refusal);
            }
        };
        self.store_token(text(&started, "restore_token"));
        let Some(stream) = first_stream(&started) else {
            async_io::block_on(close_session(&connection, &handle));
            return Err(Refusal::Failed(
                "the portal started a session with no monitor stream".to_string(),
            ));
        };
        Ok(Session {
            handle,
            stream,
            closed,
            keyed: false,
        })
    }

    /// Absolute pointer motion, in the stream's own coordinates.
    pub fn motion(&mut self, x: f64, y: f64) -> Result<(), String> {
        let (connection, session) = self.live()?;
        notify(
            &connection,
            "NotifyPointerMotionAbsolute",
            &(&session.handle, empty(), session.stream.node, x, y),
        )
    }

    pub fn button(&mut self, code: i32, pressed: bool) -> Result<(), String> {
        let (connection, session) = self.live()?;
        notify(
            &connection,
            "NotifyPointerButton",
            &(&session.handle, empty(), code, u32::from(pressed)),
        )
    }

    /// One notch per step; 0 is the vertical axis, 1 the horizontal.
    pub fn axis(&mut self, horizontal: bool, steps: i32) -> Result<(), String> {
        let (connection, session) = self.live()?;
        notify(
            &connection,
            "NotifyPointerAxisDiscrete",
            &(&session.handle, empty(), u32::from(horizontal), steps),
        )
    }

    /// A key by its keysym. A session's first key is preceded by a Shift tap,
    /// which absorbs the dropped first key and types nothing.
    pub fn keysym(&mut self, keysym: i32, pressed: bool) -> Result<(), String> {
        let (connection, session) = self.live()?;
        let handle = session.handle.clone();
        if !session.keyed {
            session.keyed = true;
            for state in [1u32, 0] {
                notify(
                    &connection,
                    "NotifyKeyboardKeysym",
                    &(&handle, empty(), SHIFT_L, state),
                )?;
            }
        }
        notify(
            &connection,
            "NotifyKeyboardKeysym",
            &(&handle, empty(), keysym, u32::from(pressed)),
        )
    }

    /// The session started earlier, which every input goes through.
    fn live(&mut self) -> Result<(Connection, &mut Session), String> {
        self.session()
            .map_err(|refusal| refusal.detail().to_string())?;
        let connection = self
            .connection
            .clone()
            .ok_or("the portal is not connected")?;
        let session = self.session.as_mut().ok_or("the portal session is gone")?;
        Ok((connection, session))
    }

    /// Close the session, if there is one: on release and at exit.
    pub fn close(&mut self) {
        let (Some(session), Some(connection)) = (self.session.take(), self.connection.clone())
        else {
            return;
        };
        async_io::block_on(close_session(&connection, &session.handle));
    }

    /// The kept restore token. None kept is the usual first start; one that will
    /// not read is said, and the portal then asks again.
    fn stored_token(&self) -> Option<String> {
        let path = self.state_file.as_ref()?;
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                eprintln!("compux: the portal's restore token would not read: {error}");
                return None;
            }
        };
        let token = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|state| state.get("restore_token")?.as_str().map(str::to_string));
        if token.is_none() {
            eprintln!("compux: {} holds no restore token", path.display());
        }
        token
    }

    fn store_token(&self, token: Option<String>) {
        let Some(path) = &self.state_file else {
            return;
        };
        if let Err(why) = write_state(path, token.as_deref()) {
            eprintln!("compux: the portal's restore token was not kept: {why}");
        }
    }
}

impl Drop for Portal {
    fn drop(&mut self) {
        self.close();
    }
}

fn write_state(path: &Path, token: Option<&str>) -> Result<(), String> {
    let Some(token) = token else {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        };
    };
    let state = serde_json::json!({ "restore_token": token });
    super::private_file::write(path, state.to_string().as_bytes())
        .map_err(|error| error.to_string())
}

/// The handle tokens one session start uses, one per Request and one for the
/// session.
struct Tokens {
    create: String,
    session: String,
    devices: String,
    sources: String,
    start: String,
}

/// A new RemoteDesktop session, with its `Closed` listened for from now.
async fn create(
    connection: &Connection,
    tokens: &Tokens,
    step: Duration,
) -> Result<(OwnedObjectPath, MessageStream), Refusal> {
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(tokens.create.as_str()));
    options.insert("session_handle_token", Value::from(tokens.session.as_str()));
    let created = ask(
        connection,
        REMOTE_DESKTOP,
        "CreateSession",
        &(options,),
        Wait::Step(step),
    )
    .await?;
    let handle = text(&created, "session_handle")
        .and_then(|path| OwnedObjectPath::try_from(path).ok())
        .ok_or_else(|| Refusal::Failed("the portal created no session".to_string()))?;
    let closed = closed_signal(connection, &handle).await?;
    Ok((handle, closed))
}

/// Keyboard and pointer, persisted until revoked, on one monitor; then Start,
/// which shows the dialog unless `restore` still serves. The selections show
/// nothing and are held to the step budget; Start waits on the person.
async fn begin(
    connection: &Connection,
    handle: &OwnedObjectPath,
    tokens: &Tokens,
    restore: Option<&str>,
    budgets: Budgets,
) -> Result<HashMap<String, OwnedValue>, Refusal> {
    let mut devices: HashMap<&str, Value<'_>> = HashMap::new();
    devices.insert("handle_token", Value::from(tokens.devices.as_str()));
    devices.insert("types", Value::from(KEYBOARD_AND_POINTER));
    devices.insert("persist_mode", Value::from(PERSIST_UNTIL_REVOKED));
    if let Some(token) = restore {
        devices.insert("restore_token", Value::from(token));
    }
    ask(
        connection,
        REMOTE_DESKTOP,
        "SelectDevices",
        &(handle, devices),
        Wait::Step(budgets.step),
    )
    .await?;
    let mut monitor: HashMap<&str, Value<'_>> = HashMap::new();
    monitor.insert("handle_token", Value::from(tokens.sources.as_str()));
    monitor.insert("types", Value::from(MONITOR));
    monitor.insert("multiple", Value::from(false));
    ask(
        connection,
        SCREEN_CAST,
        "SelectSources",
        &(handle, monitor),
        Wait::Step(budgets.step),
    )
    .await?;
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(tokens.start.as_str()));
    ask(
        connection,
        REMOTE_DESKTOP,
        "Start",
        &(handle, "", options),
        Wait::Consent(budgets.consent),
    )
    .await
}

/// How long one portal call is waited on, and what running out of it means.
#[derive(Clone, Copy, Debug)]
enum Wait {
    /// A call that may show the person a dialog: running out is their silence,
    /// `consent_timeout`.
    Consent(Duration),
    /// A set-up step that shows nothing: running out is a portal that is not
    /// answering, `capture_unavailable`.
    Step(Duration),
}

impl Wait {
    fn budget(self) -> Duration {
        match self {
            Wait::Consent(budget) | Wait::Step(budget) => budget,
        }
    }

    fn ran_out(self, method: &str) -> Refusal {
        match self {
            Wait::Consent(budget) => Refusal::TimedOut(format!(
                "nobody answered the portal's {method} dialog within {} s; nothing was sent",
                budget.as_secs()
            )),
            Wait::Step(budget) => Refusal::Failed(format!(
                "the portal did not answer {method}, which shows no dialog, within {} ms; \
                 nothing was sent",
                budget.as_millis()
            )),
        }
    }
}

fn empty() -> HashMap<&'static str, Value<'static>> {
    HashMap::new()
}

/// One input call on the RemoteDesktop session.
fn notify<B>(connection: &Connection, method: &str, body: &B) -> Result<(), String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    async_io::block_on(connection.call_method(
        Some(DESKTOP),
        DESKTOP_PATH,
        Some(REMOTE_DESKTOP),
        method,
        body,
    ))
    .map(|_| ())
    .map_err(|error| format!("the portal refused {method}: {error}"))
}

/// Call a method that answers with a Request, and wait for its Response within
/// the `wait`'s budget. Every Response from the portal's owner is listened for
/// before the call is made, so one that comes back at once is not missed; the
/// one whose path the call returned is kept. A call some other process
/// answered is refused: the portal changed hands, and its Response would be
/// another process's.
async fn ask<B>(
    connection: &Connection,
    interface: &str,
    method: &str,
    body: &B,
    wait: Wait,
) -> Result<HashMap<String, OwnedValue>, Refusal>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let owner = owner(connection).await?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(owner.as_str())
        .and_then(|rule| rule.interface(REQUEST))
        .and_then(|rule| rule.member("Response"))
        .map(|rule| rule.build())
        .map_err(|error| Refusal::Failed(error.to_string()))?;
    let mut responses = MessageStream::for_match_rule(rule, connection, Some(MAX_QUEUED_SIGNALS))
        .await
        .map_err(|error| Refusal::Failed(format!("the portal's answers: {error}")))?;
    let reply = connection
        .call_method(Some(DESKTOP), DESKTOP_PATH, Some(interface), method, body)
        .await
        .map_err(|error| Refusal::Failed(format!("the portal refused {method}: {error}")))?;
    let request: OwnedObjectPath = reply.body().deserialize().map_err(|error| {
        Refusal::Failed(format!("the portal's {method} named no request: {error}"))
    })?;
    if reply.header().sender().map(|name| name.as_str()) != Some(owner.as_str()) {
        close_request(connection, &request).await;
        return Err(Refusal::Failed(format!(
            "the portal's {method} was answered by another process than {owner}, which owned \
             the portal when it was asked; nothing was sent"
        )));
    }
    let Some(message) = response(&mut responses, &request, wait.budget()).await else {
        close_request(connection, &request).await;
        return Err(wait.ran_out(method));
    };
    responded(method, &message)
}

/// The unique name that owns the portal now, the only sender a Response or a
/// `Closed` is taken from. zbus checks a match rule's unique sender on every
/// message it hands the stream, so a signal another process sends straight to
/// compux, which the bus delivers whatever compux's rules say, is dropped too.
async fn owner(connection: &Connection) -> Result<OwnedUniqueName, Refusal> {
    let reply = connection
        .call_method(Some(BUS), BUS_PATH, Some(BUS), "GetNameOwner", &(DESKTOP,))
        .await
        .map_err(|error| Refusal::Failed(format!("nothing owns the portal: {error}")))?;
    reply
        .body()
        .deserialize()
        .map_err(|error| Refusal::Failed(format!("the portal's owner: {error}")))
}

/// The first Response on the `request`'s path within the `budget`; `None` past
/// it.
async fn response(
    responses: &mut MessageStream,
    request: &OwnedObjectPath,
    budget: Duration,
) -> Option<Message> {
    let response = async {
        while let Some(message) = responses.next().await {
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    eprintln!("compux: an unreadable portal answer was skipped: {error}");
                    continue;
                }
            };
            if message.header().path().map(|path| path.as_str()) == Some(request.as_str()) {
                return Some(message);
            }
        }
        None
    };
    let timer = async {
        async_io::Timer::after(budget).await;
        None
    };
    futures_util::future::select(Box::pin(response), Box::pin(timer))
        .await
        .factor_first()
        .0
}

/// A Response's code and results, as the refusal or the results.
fn responded(method: &str, message: &Message) -> Result<HashMap<String, OwnedValue>, Refusal> {
    let (code, results): (u32, HashMap<String, OwnedValue>) = message
        .body()
        .deserialize()
        .map_err(|error| Refusal::Failed(format!("the portal's {method} answer: {error}")))?;
    match code {
        0 => Ok(results),
        1 => Err(Refusal::Refused(format!(
            "the portal refused {method} for {APP_ID}: the person refused it, now or before. A \
             stored refusal stays until the person resets it; `fermix doctor` prints how. \
             Nothing was sent"
        ))),
        _ => Err(Refusal::TimedOut(format!(
            "the portal ended {method} unanswered (response {code}); nothing was sent"
        ))),
    }
}

/// The session's `Closed` signal from the portal's owner, listened for from its
/// start.
async fn closed_signal(
    connection: &Connection,
    handle: &OwnedObjectPath,
) -> Result<MessageStream, Refusal> {
    let owner = owner(connection).await?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(owner.as_str())
        .and_then(|rule| rule.interface(SESSION))
        .and_then(|rule| rule.member("Closed"))
        .and_then(|rule| rule.path(handle.as_str()))
        .map(|rule| rule.build())
        .map_err(|error| Refusal::Failed(error.to_string()))?;
    MessageStream::for_match_rule(rule, connection, Some(MAX_QUEUED_SIGNALS))
        .await
        .map_err(|error| Refusal::Failed(format!("the portal session's close: {error}")))
}

/// Whether the session's `Closed` arrived; never waits.
fn closed(stream: &mut MessageStream) -> bool {
    let next = futures_util::FutureExt::now_or_never(stream.next());
    matches!(next, Some(Some(_)) | Some(None))
}

async fn close_request(connection: &Connection, request: &OwnedObjectPath) {
    if let Err(error) = connection
        .call_method(Some(DESKTOP), request.as_str(), Some(REQUEST), "Close", &())
        .await
    {
        eprintln!("compux: the portal's dialog was not closed: {error}");
    }
}

async fn close_session(connection: &Connection, handle: &OwnedObjectPath) {
    if let Err(error) = connection
        .call_method(Some(DESKTOP), handle.as_str(), Some(SESSION), "Close", &())
        .await
    {
        eprintln!("compux: the portal session was not closed: {error}");
    }
}

fn text(results: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let value = results.get(key)?;
    match &**value {
        Value::Str(text) => Some(text.to_string()),
        Value::ObjectPath(path) => Some(path.to_string()),
        _ => None,
    }
}

/// The first stream Start named: its node, and where its monitor is.
fn first_stream(results: &HashMap<String, OwnedValue>) -> Option<Stream> {
    let streams: Vec<(u32, HashMap<String, OwnedValue>)> =
        results.get("streams")?.try_clone().ok()?.try_into().ok()?;
    let (node, properties) = streams.into_iter().next()?;
    let pair = |key: &str| -> Option<(i32, i32)> {
        properties.get(key)?.try_clone().ok()?.try_into().ok()
    };
    Some(Stream {
        node,
        position: pair("position").unwrap_or((0, 0)),
        size: pair("size")?,
    })
}

/// The file a `file://` URI names, decoded, then removed: the portal saves every
/// screenshot as a file and leaves it, and this one was taken for compux. Only a
/// regular file is read, never through a link, and it is removed only once it
/// has decoded and only while its path still names it. Which directory it may be
/// in is not checked yet: that waits on where GNOME saves it, which
/// LIVE_CHECK.md's M86 L2 steps B1 and B3 write down.
fn read_screenshot(uri: &str) -> Result<image::RgbaImage, Refusal> {
    let path = uri
        .strip_prefix("file://")
        .map(percent_decoded)
        .ok_or_else(|| Refusal::Failed(format!("the portal's screenshot is not a file: {uri}")))?;
    let named = regular_file(&path)?;
    let picture = decode_screenshot(&path, &named)?;
    remove_screenshot(&path, &named);
    Ok(picture)
}

/// What the path names, refused unless it is a regular file. A link is not
/// followed, and is said.
fn regular_file(path: &Path) -> Result<Metadata, Refusal> {
    let named = std::fs::symlink_metadata(path)
        .map_err(|error| Refusal::Failed(format!("the portal's screenshot: {error}")))?;
    if named.file_type().is_symlink() {
        eprintln!(
            "compux: the portal's screenshot {} is a link; it is neither followed nor removed",
            path.display()
        );
    }
    if !named.file_type().is_file() {
        return Err(Refusal::Failed(format!(
            "the portal's screenshot {} is not a regular file",
            path.display()
        )));
    }
    Ok(named)
}

/// The picture in the file `named` describes. The file opened must be that one,
/// so a path swapped for a link once it was looked at is refused.
fn decode_screenshot(path: &Path, named: &Metadata) -> Result<image::RgbaImage, Refusal> {
    let unread =
        |error: std::io::Error| Refusal::Failed(format!("the portal's screenshot: {error}"));
    let file = std::fs::File::open(path).map_err(unread)?;
    let opened = file.metadata().map_err(unread)?;
    if !same_file(named, &opened) {
        return Err(Refusal::Failed(
            "the portal's screenshot was replaced as it was opened".to_string(),
        ));
    }
    if opened.len() > MAX_SCREENSHOT_BYTES {
        return Err(Refusal::Failed(
            "the portal's screenshot is larger than any screen".to_string(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SCREENSHOT_BYTES)
        .read_to_end(&mut bytes)
        .map_err(unread)?;
    image::load_from_memory(&bytes)
        .map(|picture| picture.to_rgba8())
        .map_err(|error| {
            Refusal::Failed(format!("the portal's screenshot would not decode: {error}"))
        })
}

fn same_file(one: &Metadata, other: &Metadata) -> bool {
    one.dev() == other.dev() && one.ino() == other.ino()
}

/// Remove the screenshot that was read, unless its path names something else by
/// now. One that stays is said.
fn remove_screenshot(path: &Path, named: &Metadata) {
    let still = std::fs::symlink_metadata(path)
        .map(|now| now.file_type().is_file() && same_file(named, &now));
    let removed = match still {
        Ok(true) => std::fs::remove_file(path),
        Ok(false) => Err(std::io::Error::other("its path names another file by now")),
        Err(error) => Err(error),
    };
    if let Err(error) = removed {
        eprintln!(
            "compux: the portal's screenshot {} stays: {error}",
            path.display()
        );
    }
}

/// A URI path with its `%XX` escapes undone.
fn percent_decoded(path: &str) -> PathBuf {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = (bytes[at] == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fake_portal::{Answer, Call, FakePortal, Script};

    /// A consent budget a test can wait out.
    const SHORT: Duration = Duration::from_millis(300);

    /// The most times a test looks again for a signal to land.
    const MAX_LOOKS: usize = 200;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("compux-portal-{name}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("the old scratch dir");
        }
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        dir
    }

    fn over(client: &Connection, state: Option<PathBuf>, consent: Duration) -> Portal {
        let budgets = Budgets {
            consent,
            step: SHORT,
        };
        over_with(client, state, budgets)
    }

    fn over_with(client: &Connection, state: Option<PathBuf>, budgets: Budgets) -> Portal {
        let client = client.clone();
        Portal::with(Box::new(move || Ok(client.clone())), state, budgets)
    }

    /// A picture saved where the portal would say, as the `file://` URI naming it.
    fn saved_picture(dir: &Path, width: u32, height: u32) -> String {
        let path = dir.join("a picture.png");
        image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .expect("the picture");
        format!("file://{}", path.display()).replace(' ', "%20")
    }

    fn by(calls: &[Call], member: &str) -> Vec<Call> {
        calls
            .iter()
            .filter(|call| call.member == member)
            .cloned()
            .collect()
    }

    fn stored(state: &Path) -> Option<String> {
        let text = std::fs::read_to_string(state).ok()?;
        let state: serde_json::Value = serde_json::from_str(&text).expect("the state file is JSON");
        state["restore_token"].as_str().map(str::to_string)
    }

    // A host process with no registered app id shares the empty one with every
    // other, and one Allow was seen to grant them all. compux registers its own
    // before the portal is asked anything, and once per connection.
    #[test]
    fn it_registers_its_own_app_id_before_the_portal_is_asked_anything() {
        let dir = scratch("register");
        let script = Script {
            screenshot_uri: saved_picture(&dir, 8, 6),
            ..Script::default()
        };
        let (fake, client) = FakePortal::start(script);
        let mut portal = over(&client, None, SHORT);

        portal.screenshot().expect("the first screenshot");
        saved_picture(&dir, 8, 6);
        portal.screenshot().expect("the second screenshot");

        let calls = fake.calls();
        assert_eq!(calls[0].member, "Register", "{calls:?}");
        assert_eq!(calls[0].interface, REGISTRY, "{calls:?}");
        assert_eq!(calls[0].detail, APP_ID, "{calls:?}");
        assert_eq!(by(&calls, "Register").len(), 1, "{calls:?}");
        assert_eq!(by(&calls, "Screenshot").len(), 2, "{calls:?}");
    }

    // Without a Registry (xdg-desktop-portal before 1.18) compux would be asked
    // about under the empty app id: it asks nothing at all instead.
    #[test]
    fn a_portal_that_will_not_register_compux_is_asked_nothing_else() {
        let (fake, client) = FakePortal::start(Script {
            registry: false,
            ..Script::default()
        });
        let mut portal = over(&client, None, SHORT);

        let shot = portal
            .screenshot()
            .expect_err("no screenshot without the app id");
        let session = portal.session().expect_err("no session without the app id");

        assert_eq!(shot.code(), "capture_unavailable", "{shot:?}");
        assert!(shot.detail().contains(APP_ID), "{shot:?}");
        assert_eq!(session.code(), "capture_unavailable", "{session:?}");
        let members = fake.members();
        assert!(
            members.iter().all(|member| member == "Register"),
            "{members:?}"
        );
    }

    #[test]
    fn a_granted_screenshot_is_the_picture_the_portal_saved_and_asked_without_a_dialog() {
        let dir = scratch("granted");
        let script = Script {
            screenshot_uri: saved_picture(&dir, 8, 6),
            ..Script::default()
        };
        let (fake, client) = FakePortal::start(script);
        let mut portal = over(&client, None, SHORT);

        let picture = portal.screenshot().expect("the picture");

        assert_eq!((picture.width(), picture.height()), (8, 6));
        assert_eq!(picture.get_pixel(3, 3), &image::Rgba([10, 20, 30, 255]));
        let asked = by(&fake.calls(), "Screenshot");
        assert_eq!(
            asked[0].detail, "interactive=Some(Bool(false))",
            "{asked:?}"
        );
    }

    // The portal saves every screenshot as a file and leaves it there: compux
    // removes the one the answer named once it is read, and nothing beside it.
    #[test]
    fn a_screenshot_file_is_removed_once_read_and_no_other_file_is() {
        let dir = scratch("removed");
        let beside = dir.join("beside.png");
        std::fs::write(&beside, b"the person's").expect("a file beside it");
        let script = Script {
            screenshot_uri: saved_picture(&dir, 8, 6),
            ..Script::default()
        };
        let (_fake, client) = FakePortal::start(script);
        let mut portal = over(&client, None, SHORT);

        portal.screenshot().expect("the picture");

        assert!(!dir.join("a picture.png").exists(), "the screenshot stays");
        assert!(beside.exists(), "a file beside it went too");
    }

    // The portal names the file it saved. A link there is not followed: neither
    // it nor what it points at is read or removed.
    #[test]
    fn a_screenshot_named_through_a_link_is_refused_and_nothing_is_removed() {
        let dir = scratch("link");
        let target = dir.join("target.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]))
            .save(&target)
            .expect("a target");
        let link = dir.join("link.png");
        std::os::unix::fs::symlink(&target, &link).expect("a link");
        let (_fake, client) = FakePortal::start(Script {
            screenshot_uri: format!("file://{}", link.display()),
            ..Script::default()
        });
        let mut portal = over(&client, None, SHORT);

        let refusal = portal.screenshot().expect_err("a link is not read");

        assert_eq!(refusal.code(), "capture_unavailable", "{refusal:?}");
        assert!(link.symlink_metadata().is_ok(), "the link was removed");
        assert!(target.exists(), "what it points at was removed");
    }

    // Only a screenshot compux could read is removed: one that will not decode
    // stays where the portal saved it.
    #[test]
    fn a_screenshot_that_will_not_decode_is_refused_and_left_where_it_was_saved() {
        let dir = scratch("undecoded");
        let saved = dir.join("saved.png");
        std::fs::write(&saved, b"not a picture").expect("a file");
        let (_fake, client) = FakePortal::start(Script {
            screenshot_uri: format!("file://{}", saved.display()),
            ..Script::default()
        });
        let mut portal = over(&client, None, SHORT);

        let refusal = portal.screenshot().expect_err("not a picture");

        assert_eq!(refusal.code(), "capture_unavailable", "{refusal:?}");
        assert!(saved.exists(), "a file compux could not read was removed");
    }

    // Any process on the session bus can send a Response on compux's Request
    // path. One sent before the portal's, naming another file, is not taken:
    // the picture is the portal's, and the other file is neither read nor
    // removed.
    #[test]
    fn a_response_another_process_sends_first_is_ignored_and_its_file_left() {
        let dir = scratch("forged");
        let victim = dir.join("victim.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]))
            .save(&victim)
            .expect("a victim");
        let script = Script {
            screenshot_uri: saved_picture(&dir, 8, 6),
            forged: Some("Screenshot"),
            forged_uri: format!("file://{}", victim.display()),
            ..Script::default()
        };
        let (_fake, client) = FakePortal::start(script);
        let mut portal = over(&client, None, SHORT);

        let picture = portal.screenshot().expect("the portal's picture");

        assert_eq!((picture.width(), picture.height()), (8, 6));
        assert!(victim.exists(), "the forged answer's file was removed");
        assert!(!dir.join("a picture.png").exists(), "the screenshot stays");
    }

    // A Response 0 another process sends for Start would open a session the
    // person refused, and post input through it.
    #[test]
    fn a_start_another_process_grants_stays_the_persons_refusal_and_posts_nothing() {
        let (fake, client) = FakePortal::start(Script {
            start: Answer::Respond(1),
            forged: Some("Start"),
            ..Script::default()
        });
        let mut portal = over(&client, None, SHORT);

        let refused = portal
            .keysym(0x61, true)
            .expect_err("refused by the person");

        assert!(refused.contains("the person refused it"), "{refused}");
        let keys = by(&fake.calls(), "NotifyKeyboardKeysym");
        assert!(keys.is_empty(), "{keys:?}");
    }

    // The bus said who owns the portal, and another process answered the call:
    // the portal changed hands, and its answers are not taken from either.
    #[test]
    fn a_request_answered_by_another_process_than_the_portals_owner_is_refused() {
        let dir = scratch("other-owner");
        let script = Script {
            screenshot_uri: saved_picture(&dir, 8, 6),
            owner: ":1.7",
            ..Script::default()
        };
        let (_fake, client) = FakePortal::start(script);
        let mut portal = over(&client, None, SHORT);

        let refusal = portal.screenshot().expect_err("not the owner's answer");

        assert_eq!(refusal.code(), "capture_unavailable", "{refusal:?}");
        assert!(refusal.detail().contains("Screenshot"), "{refusal:?}");
        assert!(dir.join("a picture.png").exists(), "an unread file went");
    }

    // Response 1 is a refusal, now or stored, and only the person can reset a
    // stored one: the detail points at where `doctor` says how.
    #[test]
    fn a_refused_screenshot_is_consent_refused_and_names_the_reset() {
        let (_fake, client) = FakePortal::start(Script {
            screenshot: Answer::Respond(1),
            ..Script::default()
        });
        let mut portal = over(&client, None, SHORT);

        let refusal = portal.screenshot().expect_err("refused");

        assert_eq!(refusal.code(), "consent_refused", "{refusal:?}");
        assert!(refusal.detail().contains("fermix doctor"), "{refusal:?}");
    }

    #[test]
    fn a_screenshot_the_portal_ended_unanswered_is_consent_timeout() {
        let (_fake, client) = FakePortal::start(Script {
            screenshot: Answer::Respond(2),
            ..Script::default()
        });
        let mut portal = over(&client, None, SHORT);

        let refusal = portal.screenshot().expect_err("ended");

        assert_eq!(refusal.code(), "consent_timeout", "{refusal:?}");
    }

    // The budget is the caller's bound on a dialog nobody answers; past it the
    // dialog is closed, so it does not stay up asking for a call that is over.
    #[test]
    fn a_dialog_nobody_answers_is_consent_timeout_at_the_budget_and_is_closed() {
        let (fake, client) = FakePortal::start(Script {
            screenshot: Answer::Never,
            ..Script::default()
        });
        let mut portal = over(&client, None, SHORT);

        let started = std::time::Instant::now();
        let refusal = portal.screenshot().expect_err("unanswered");
        let waited = started.elapsed();

        assert_eq!(refusal.code(), "consent_timeout", "{refusal:?}");
        assert!(waited >= SHORT && waited < SHORT * 10, "waited {waited:?}");
        assert_eq!(
            CONSENT_BUDGET,
            Duration::from_secs(25),
            "the portal's own, outside a test"
        );
        let closes = by(&fake.calls(), "Close");
        assert_eq!(closes.len(), 1, "{closes:?}");
        assert!(closes[0].path.contains("/request/"), "{closes:?}");
    }

    // The set-up steps show nothing, so a portal that leaves one unanswered is
    // not the person's silence: each step has its own short budget and is
    // `capture_unavailable`, naming the step. Only Start waits on the person.
    #[test]
    fn a_set_up_step_the_portal_leaves_unanswered_is_capture_unavailable_at_its_own_budget() {
        for step in ["CreateSession", "SelectDevices", "SelectSources"] {
            let (_fake, client) = FakePortal::start(Script {
                silent_step: Some(step),
                ..Script::default()
            });
            let budgets = Budgets {
                consent: Duration::from_secs(3),
                step: SHORT,
            };
            let mut portal = over_with(&client, None, budgets);

            let started = std::time::Instant::now();
            let refusal = portal.session().expect_err("never set up");
            let waited = started.elapsed();

            assert_eq!(refusal.code(), "capture_unavailable", "{step}: {refusal:?}");
            assert!(refusal.detail().contains(step), "{refusal:?}");
            assert!(
                waited >= SHORT && waited < Duration::from_secs(2),
                "{step}: waited {waited:?}"
            );
        }
        assert_eq!(STEP_BUDGET, Duration::from_secs(5), "outside a test");
    }

    // persist_mode 2 hands back a restore token; compux keeps it in its own state
    // file and offers it to the next session, which hands back the next one.
    #[test]
    fn a_session_offers_the_kept_restore_token_once_and_keeps_the_next() {
        let state = scratch("restore").join("compux").join("portal.json");
        let (fake, client) = FakePortal::start(Script::default());

        let mut first = over(&client, Some(state.clone()), SHORT);
        let stream = first.session().expect("the first session");
        assert_eq!(stored(&state).as_deref(), Some("token-1"));
        drop(first);
        let mut second = over(&client, Some(state.clone()), SHORT);
        second.session().expect("the second session");

        assert_eq!(
            stream,
            Stream {
                node: 42,
                position: (0, 0),
                size: (1280, 800)
            }
        );
        let selected = by(&fake.calls(), "SelectDevices");
        assert_eq!(
            selected[0].detail, "types=3 persist_mode=2 restore_token=",
            "{selected:?}"
        );
        assert_eq!(
            selected[1].detail, "types=3 persist_mode=2 restore_token=token-1",
            "{selected:?}"
        );
        assert_eq!(stored(&state).as_deref(), Some("token-2"));
        let sources = by(&fake.calls(), "SelectSources");
        assert!(
            sources[0].detail.starts_with("types=1 "),
            "a monitor: {sources:?}"
        );
    }

    // The token restores a grant without a dialog: it is the person's alone to
    // read, and a link planted beside it is never written through.
    #[test]
    fn the_restore_token_is_kept_for_the_person_alone_and_never_through_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("private");
        let victim = dir.join("victim");
        std::fs::write(&victim, "the person's").expect("a victim");
        let state = dir.join("compux").join("portal.json");
        std::fs::create_dir_all(dir.join("compux")).expect("the state directory");
        std::os::unix::fs::symlink(&victim, dir.join("compux").join("portal.writing"))
            .expect("a planted link");

        write_state(&state, Some("secret")).expect("kept");

        let mode = std::fs::metadata(&state)
            .expect("there")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(stored(&state).as_deref(), Some("secret"));
        assert_eq!(
            std::fs::read_to_string(&victim).expect("it reads"),
            "the person's"
        );
    }

    // A token the portal did not honour leads to the dialog; refused there, the
    // token is spent: it is forgotten, and the half-made session is closed.
    #[test]
    fn a_start_refused_or_ended_spends_the_offered_token_and_closes_the_session() {
        for (answer, code) in [(1, "consent_refused"), (2, "consent_timeout")] {
            let state = scratch(&format!("spent-{answer}")).join("portal.json");
            write_state(&state, Some("old")).expect("a kept token");
            let (fake, client) = FakePortal::start(Script {
                start: Answer::Respond(answer),
                ..Script::default()
            });
            let mut portal = over(&client, Some(state.clone()), SHORT);

            let refusal = portal.session().expect_err("not started");

            assert_eq!(refusal.code(), code, "{refusal:?}");
            assert_eq!(stored(&state), None, "the offered token is spent");
            let calls = fake.calls();
            assert!(
                by(&calls, "SelectDevices")[0]
                    .detail
                    .ends_with("restore_token=old"),
                "{calls:?}"
            );
            let closes = by(&calls, "Close");
            assert!(
                closes.iter().any(|call| call.path.contains("/session/")),
                "{calls:?}"
            );
        }
    }

    // The person stops the session from the shell: it is gone, and so is its
    // token, so the next input asks again rather than restoring what they ended.
    #[test]
    fn a_session_the_person_stopped_is_forgotten_with_its_token_and_asked_again() {
        let state = scratch("stopped").join("portal.json");
        let (fake, client) = FakePortal::start(Script::default());
        let mut portal = over(&client, Some(state.clone()), SHORT);
        portal.session().expect("the first session");

        fake.close_session();
        let mut looks = 0;
        while by(&fake.calls(), "CreateSession").len() < 2 && looks < MAX_LOOKS {
            portal.session().expect("a session");
            std::thread::sleep(Duration::from_millis(10));
            looks += 1;
        }

        let selected = by(&fake.calls(), "SelectDevices");
        assert_eq!(selected.len(), 2, "never started again after {looks} looks");
        assert!(
            selected[1].detail.ends_with("restore_token="),
            "{selected:?}"
        );
        assert_eq!(stored(&state).as_deref(), Some("token-2"));
    }

    // A Closed another process sends does not end the session or forget its
    // token: only the portal's does. Each input is a round trip on the one
    // connection, so after the first the forged Closed has landed.
    #[test]
    fn a_close_another_process_sends_leaves_the_session_and_its_token() {
        let state = scratch("forged-close").join("portal.json");
        let (fake, client) = FakePortal::start(Script::default());
        let mut portal = over(&client, Some(state.clone()), SHORT);
        portal.session().expect("the session");

        fake.forge_close();
        portal.motion(1.0, 2.0).expect("moved");
        portal.motion(3.0, 4.0).expect("moved again");

        let calls = fake.calls();
        assert_eq!(by(&calls, "CreateSession").len(), 1, "{calls:?}");
        assert_eq!(stored(&state).as_deref(), Some("token-1"));
    }

    // The first key a remote-desktop session posted was seen dropped. A Shift
    // tap goes first, once per session, and types nothing.
    #[test]
    fn a_sessions_first_key_follows_one_shift_tap() {
        let (fake, client) = FakePortal::start(Script::default());
        let mut portal = over(&client, None, SHORT);

        for keysym in [0x61, 0x62] {
            portal.keysym(keysym, true).expect("pressed");
            portal.keysym(keysym, false).expect("released");
        }

        let keys: Vec<String> = by(&fake.calls(), "NotifyKeyboardKeysym")
            .into_iter()
            .map(|call| call.detail)
            .collect();
        let expected = [
            "0xffe1 1", "0xffe1 0", "0x61 1", "0x61 0", "0x62 1", "0x62 0",
        ];
        assert_eq!(keys, expected);
    }

    // Released, the session is closed; the next input starts another, which the
    // kept token restores without a dialog.
    #[test]
    fn release_closes_the_session_and_the_next_input_starts_another() {
        let (fake, client) = FakePortal::start(Script::default());
        let mut portal = over(&client, None, SHORT);
        portal.motion(1.0, 2.0).expect("moved");

        portal.close();
        portal.motion(3.0, 4.0).expect("moved again");

        let calls = fake.calls();
        let closes = by(&calls, "Close");
        assert_eq!(closes.len(), 1, "{calls:?}");
        assert!(closes[0].path.contains("/session/"), "{calls:?}");
        assert_eq!(by(&calls, "CreateSession").len(), 2, "{calls:?}");
        let moves: Vec<String> = by(&calls, "NotifyPointerMotionAbsolute")
            .into_iter()
            .map(|call| call.detail)
            .collect();
        assert_eq!(moves, ["42 1 2", "42 3 4"]);
    }
}
