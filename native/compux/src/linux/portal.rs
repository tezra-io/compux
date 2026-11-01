//! GNOME Wayland pixels through the XDG desktop portal (M86 L2), over zbus.
//!
//! - **Its own name.** Before any other portal call the connection registers as
//!   [`APP_ID`] with `org.freedesktop.host.portal.Registry`. Without that a host
//!   process has the empty app id, which every unsandboxed process shares: one
//!   Allow would grant them all, and anyone's Deny would refuse compux (L29-29).
//!   A portal with no Registry (xdg-desktop-portal before 1.18) is never used; the
//!   session's pixels are refused instead (`pixels`).
//! - **Screenshot**, `interactive: false`. Response 0 is the image; 1 is a
//!   refusal (the person's, now or stored: `consent_refused`); 2, or no answer
//!   within the consent budget, is `consent_timeout`. compux never writes the
//!   PermissionStore: a stored no is the person's to reset.
//! - **Input**: one RemoteDesktop session with keyboard and pointer, and a
//!   ScreenCast monitor source that says where the picked monitor is. Asked for
//!   with `persist_mode` 2; the restore token is kept in compux's own state file,
//!   used once, and replaced by the next. Keys go as keysyms, the pointer as
//!   absolute motion in the stream's own coordinates. compux reads no frame from
//!   any stream.
//! - **Its life.** The session is kept for the helper's life, closed on release
//!   and at exit. Closed by the person (the shell's Stop), it is gone, and so is
//!   its token: the next input asks again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, Message, MessageStream};

/// The app id compux registers, and the one every grant is kept under.
pub const APP_ID: &str = "io.tezra.fermix.ComputerUse";

/// How long a portal dialog is waited on: the portal itself ends an unanswered
/// Screenshot dialog with Response 2 at 25 s (L29-33).
pub const CONSENT_BUDGET: Duration = Duration::from_secs(25);

const DESKTOP: &str = "org.freedesktop.portal.Desktop";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
pub const REGISTRY: &str = "org.freedesktop.host.portal.Registry";
pub const SCREENSHOT: &str = "org.freedesktop.portal.Screenshot";
pub const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
pub const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";

/// RemoteDesktop's device types: keyboard 1, pointer 2.
const KEYBOARD_AND_POINTER: u32 = 3;
/// ScreenCast's source type for a monitor.
const MONITOR: u32 = 1;
/// Keep the grant until the person revokes it.
const PERSIST_UNTIL_REVOKED: u32 = 2;

/// evdev's button codes, which the portal takes.
pub const BTN_LEFT: i32 = 0x110;
pub const BTN_RIGHT: i32 = 0x111;

/// `Shift_L`, tapped once before a session's first key (L13-16: the first key of
/// a remote-desktop session was dropped, and a lead Shift tap absorbed it).
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
    /// The portal's `Closed` signal for it, read before each use.
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
    budget: Duration,
    next_token: u32,
}

impl Portal {
    /// On the session bus, with the restore token kept in `state_file`.
    #[cfg(not(test))]
    pub fn on_session_bus(state_file: Option<PathBuf>) -> Portal {
        Portal::with(
            Box::new(|| async_io::block_on(crate::linux::bus::session())),
            state_file,
            CONSENT_BUDGET,
        )
    }

    /// Over any connection `connect` makes, waiting `budget` on a dialog.
    pub fn with(
        connect: Box<dyn Fn() -> Result<Connection, String>>,
        state_file: Option<PathBuf>,
        budget: Duration,
    ) -> Portal {
        Portal {
            connection: None,
            connect,
            session: None,
            state_file,
            budget,
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
            self.budget,
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
        let budget = self.budget;
        let (handle, closed) = async_io::block_on(create(&connection, &tokens, budget))?;
        let started = async_io::block_on(begin(
            &connection,
            &handle,
            &tokens,
            restore.as_deref(),
            budget,
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
    /// which absorbs the first-key drop (L13-16) and types nothing.
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
    let dir = path.parent().ok_or("the state file has no directory")?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let scratch = path.with_extension("writing");
    let state = serde_json::json!({ "restore_token": token });
    std::fs::write(&scratch, state.to_string()).map_err(|error| error.to_string())?;
    std::fs::rename(&scratch, path).map_err(|error| error.to_string())
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
    budget: Duration,
) -> Result<(OwnedObjectPath, MessageStream), Refusal> {
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(tokens.create.as_str()));
    options.insert("session_handle_token", Value::from(tokens.session.as_str()));
    let created = ask(
        connection,
        REMOTE_DESKTOP,
        "CreateSession",
        &(options,),
        budget,
    )
    .await?;
    let handle = text(&created, "session_handle")
        .and_then(|path| OwnedObjectPath::try_from(path).ok())
        .ok_or_else(|| Refusal::Failed("the portal created no session".to_string()))?;
    let closed = closed_signal(connection, &handle).await?;
    Ok((handle, closed))
}

/// Keyboard and pointer, persisted until revoked, on one monitor; then Start,
/// which shows the dialog unless `restore` still serves.
async fn begin(
    connection: &Connection,
    handle: &OwnedObjectPath,
    tokens: &Tokens,
    restore: Option<&str>,
    budget: Duration,
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
        budget,
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
        budget,
    )
    .await?;
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(tokens.start.as_str()));
    ask(
        connection,
        REMOTE_DESKTOP,
        "Start",
        &(handle, "", options),
        budget,
    )
    .await
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
/// `budget`. Every Response is listened for before the call is made, so one that
/// comes back at once is not missed; the one whose path the call returned is kept.
async fn ask<B>(
    connection: &Connection,
    interface: &str,
    method: &str,
    body: &B,
    budget: Duration,
) -> Result<HashMap<String, OwnedValue>, Refusal>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(REQUEST)
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
    let wait = async {
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
    let Some(message) = futures_util::future::select(Box::pin(wait), Box::pin(timer))
        .await
        .factor_first()
        .0
    else {
        close_request(connection, &request).await;
        return Err(Refusal::TimedOut(format!(
            "nobody answered the portal's {method} dialog within {} s; nothing was sent",
            budget.as_secs()
        )));
    };
    responded(method, &message)
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

/// The session's `Closed` signal, listened for from its start.
async fn closed_signal(
    connection: &Connection,
    handle: &OwnedObjectPath,
) -> Result<MessageStream, Refusal> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(SESSION)
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

/// The file a `file://` URI names, decoded.
fn read_screenshot(uri: &str) -> Result<image::RgbaImage, Refusal> {
    let path = uri
        .strip_prefix("file://")
        .map(percent_decoded)
        .ok_or_else(|| Refusal::Failed(format!("the portal's screenshot is not a file: {uri}")))?;
    let size = std::fs::metadata(&path)
        .map_err(|error| Refusal::Failed(format!("the portal's screenshot: {error}")))?
        .len();
    if size > MAX_SCREENSHOT_BYTES {
        return Err(Refusal::Failed(
            "the portal's screenshot is larger than any screen".to_string(),
        ));
    }
    let bytes = std::fs::read(&path)
        .map_err(|error| Refusal::Failed(format!("the portal's screenshot: {error}")))?;
    image::load_from_memory(&bytes)
        .map(|picture| picture.to_rgba8())
        .map_err(|error| {
            Refusal::Failed(format!("the portal's screenshot would not decode: {error}"))
        })
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

    fn over(client: &Connection, state: Option<PathBuf>, budget: Duration) -> Portal {
        let client = client.clone();
        Portal::with(Box::new(move || Ok(client.clone())), state, budget)
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

    // L29-29: a host process with no registered app id shares the empty one with
    // every other, so one Allow granted them all. compux registers its own before
    // the portal is asked anything, and once per connection.
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

    // L13-16: the first key a remote-desktop session posted was dropped. A Shift
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
