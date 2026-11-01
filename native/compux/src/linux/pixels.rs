//! Which way pixels reach this session, decided once at start: X11 through x11rb,
//! GNOME Wayland through the portal, or refused with the reason. One route per
//! session and never a second behind it: a failed route is reported, never
//! retried through another (XTEST through XWayland reaches only X clients, and
//! Mutter's own RemoteDesktop asks nobody).
//!
//! What it answers the worker: whether an action is served here, the input
//! platform, the screen (monitors and frames), the pointer, and the facts
//! `probe` reports of both routes, read without a dialog.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::{json, Map, Value};
use zbus::Connection;

use super::display::{Endpoint, Env, Server};
use super::portal::{self, Portal, Refusal};
use super::x11;
use crate::geometry::MonitorFacts;
use crate::held::{Axis, Button, Direction, Key, Platform};

/// What the portal route does not serve: there is no portal clipboard compux
/// owns, and no portal lists windows.
const PORTAL_UNSERVED: [&str; 2] = ["paste", "windows"];

/// The session bus names that say which compositor runs.
const GNOME_SHELL: &str = "org.gnome.Shell";
const KWIN: &str = "org.kde.KWin";

/// The package the portal's frontend comes in.
#[cfg_attr(test, allow(dead_code))]
const PORTAL_PACKAGE: &str = "xdg-desktop-portal";

/// The PermissionStore, read (never written) for a stored Screenshot answer.
const PERMISSION_STORE: &str = "org.freedesktop.impl.portal.PermissionStore";
const PERMISSION_STORE_PATH: &str = "/org/freedesktop/impl/portal/PermissionStore";

/// What a Wayland session says of its portal, read without a dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wayland {
    pub gnome: bool,
    pub kde: bool,
    pub registry: bool,
    pub screenshot: bool,
    pub remote_desktop: bool,
    pub screen_cast: bool,
    /// The installed xdg-desktop-portal, where the package database says.
    pub version: Option<String>,
}

/// The route, as decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    X11,
    Portal,
    Refused(String),
}

/// Pure: the route a session's server and portal facts give.
pub fn decide(server: Server, wayland: Option<&Wayland>) -> Decision {
    let facts = match (server, wayland) {
        (Server::X11, _) => return Decision::X11,
        (Server::None, _) => {
            return Decision::Refused(
                "no display answers this session (neither a Wayland nor an X socket)".to_string(),
            )
        }
        (Server::Wayland { .. }, None) => {
            return Decision::Refused(
                "this Wayland session has no session bus to reach the portal on".to_string(),
            )
        }
        (Server::Wayland { .. }, Some(facts)) => facts,
    };
    if !facts.gnome {
        let which = if facts.kde {
            "KDE Plasma"
        } else {
            "this compositor"
        };
        return Decision::Refused(format!(
            "{which} on Wayland has no pixel route compux has qualified; only GNOME's portal is"
        ));
    }
    if !facts.registry {
        let version = facts.version.as_deref().unwrap_or("of unknown version");
        return Decision::Refused(format!(
            "xdg-desktop-portal {version} has no host Registry (1.18 or later has it), so compux \
             cannot ask for pixels under its own app id {}",
            portal::APP_ID
        ));
    }
    let missing = [
        (facts.screenshot, "Screenshot"),
        (facts.remote_desktop, "RemoteDesktop"),
        (facts.screen_cast, "ScreenCast"),
    ];
    if let Some((_, name)) = missing.iter().find(|(present, _)| !present) {
        return Decision::Refused(format!("the portal has no {name} on this session"));
    }
    Decision::Portal
}

/// This session's pixels.
pub enum Pixels {
    X11(Box<x11::Desktop>, Env),
    Portal(Rc<RefCell<Portal>>, PortalInput),
    Refused(String),
    /// Serves every pixel action into a recording platform, and no screen: what a
    /// unit test's worker is given.
    #[cfg(test)]
    Recorded(crate::held::Recorder),
}

/// A handle on the screen, for the displays and the frames.
#[derive(Clone)]
pub enum Screen {
    X11(Env),
    Portal(Rc<RefCell<Portal>>),
    Refused(String),
}

impl Pixels {
    /// Decide the route for this endpoint, and on X11 release what a helper that
    /// died here left held.
    #[cfg(not(test))]
    pub fn of(endpoint: &Endpoint) -> Pixels {
        let server = endpoint.server();
        let wayland = match server {
            Server::Wayland { .. } => wayland_facts(),
            _ => None,
        };
        let state = state_file(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"));
        Pixels::from_decision(decide(server, wayland.as_ref()), endpoint, state)
    }

    /// Under test there is no session this process may reach: a worker under test
    /// is given its pixels.
    #[cfg(test)]
    pub fn of(_endpoint: &Endpoint) -> Pixels {
        panic!("a unit test asked for the real session's pixels: give the worker its pixels")
    }

    #[cfg(not(test))]
    fn from_decision(decision: Decision, endpoint: &Endpoint, state: Option<PathBuf>) -> Pixels {
        match decision {
            Decision::X11 => {
                let mut desktop = Box::new(x11::Desktop::new(endpoint.env.clone()));
                desktop.release_left_behind();
                Pixels::X11(desktop, endpoint.env.clone())
            }
            Decision::Portal => Pixels::on_portal(Portal::on_session_bus(state)),
            Decision::Refused(why) => Pixels::Refused(why),
        }
    }

    /// The portal route over this portal.
    pub fn on_portal(portal: Portal) -> Pixels {
        let portal = Rc::new(RefCell::new(portal));
        Pixels::Portal(Rc::clone(&portal), PortalInput { portal })
    }

    /// Whether this route serves a pixel action.
    pub fn serves(&self, action: &str) -> bool {
        match self {
            #[cfg(test)]
            Pixels::Recorded(_) => true,
            Pixels::X11(..) => true,
            Pixels::Portal(..) => !PORTAL_UNSERVED.contains(&action),
            Pixels::Refused(_) => false,
        }
    }

    /// The sentence a refused pixel action carries.
    pub fn refusal(&self, action: &str) -> String {
        match self {
            Pixels::Refused(why) => format!("{why}; work by control (`elements` and the verbs on a reference); nothing was sent"),
            _ => format!(
                "on GNOME Wayland compux has no route for `{action}`: the portal offers no \
                 clipboard compux can own and no list of windows; nothing was sent"
            ),
        }
    }

    pub fn screen(&self) -> Screen {
        match self {
            Pixels::X11(_, env) => Screen::X11(env.clone()),
            Pixels::Portal(portal, _) => Screen::Portal(Rc::clone(portal)),
            Pixels::Refused(why) => Screen::Refused(why.clone()),
            #[cfg(test)]
            Pixels::Recorded(_) => Screen::Refused("a unit test has no screen".to_string()),
        }
    }

    /// The input platform, kept for the session.
    pub fn input(&mut self) -> Result<&mut dyn Platform, String> {
        match self {
            Pixels::X11(desktop, _) => Ok(desktop.as_mut()),
            Pixels::Portal(_, input) => Ok(input),
            Pixels::Refused(why) => Err(why.clone()),
            #[cfg(test)]
            Pixels::Recorded(recorder) => Ok(recorder),
        }
    }

    /// Where the pointer is, on X11; the portal never says.
    pub fn pointer(&mut self) -> Option<(i32, i32)> {
        match self {
            Pixels::X11(desktop, _) => desktop.pointer(),
            _ => None,
        }
    }

    /// On X11, whether `text` may be typed now; elsewhere nothing is known to
    /// refuse it by.
    pub fn ready_to_type(&mut self, text: &str) -> Result<Result<(), x11::Refusal>, String> {
        match self {
            Pixels::X11(desktop, _) => desktop.ready_to_type(text),
            _ => Ok(Ok(())),
        }
    }

    /// The client windows, front-most first: X11 only.
    pub fn windows(&self) -> Result<Vec<x11::Window>, String> {
        match self {
            Pixels::X11(_, env) => x11::windows(env),
            _ => Err("windows are listed on X11 only".to_string()),
        }
    }

    /// Let go of the portal session: on release, and at exit by drop.
    pub fn release(&mut self) {
        if let Pixels::Portal(portal, _) = self {
            portal.borrow_mut().close();
        }
    }

    /// Whether the portal's geometry applies: logical points, frames measured.
    pub fn portal(&self) -> bool {
        matches!(self, Pixels::Portal(..))
    }
}

impl Screen {
    /// The monitor at this index, as facts and an id. On the portal, the one
    /// monitor the person picked for the session (starting it if need be).
    pub fn monitor(&self, index: usize) -> Result<(MonitorFacts, u32), String> {
        match self {
            Screen::X11(env) => {
                let monitors = x11::monitors(env)?;
                if monitors.is_empty() {
                    return Err("no_active_display".to_string());
                }
                let monitor = monitors
                    .get(index)
                    .ok_or_else(|| format!("display {index} not found"))?;
                Ok((x11_facts(monitor), monitor.id))
            }
            Screen::Portal(portal) => {
                if index != 0 {
                    return Err(format!(
                        "display {index} not found: on Wayland compux has the one monitor picked \
                         for its session"
                    ));
                }
                let stream = portal
                    .borrow_mut()
                    .session()
                    .map_err(|refusal| refusal.code().to_string())?;
                Ok((portal_facts(&stream), stream.node))
            }
            Screen::Refused(_) => Err("no_active_display".to_string()),
        }
    }
}

/// An X monitor in root pixels, which are XTEST's too: a scale of 1.
pub fn x11_facts(monitor: &x11::Monitor) -> MonitorFacts {
    MonitorFacts {
        x: i32::from(monitor.x),
        y: i32::from(monitor.y),
        width: u32::from(monitor.width),
        height: u32::from(monitor.height),
        scale_factor: 1.0,
    }
}

/// The portal's monitor in the compositor's logical points; how many pixels a
/// point holds is measured from each frame.
pub fn portal_facts(stream: &portal::Stream) -> MonitorFacts {
    MonitorFacts {
        x: stream.position.0,
        y: stream.position.1,
        width: u32::try_from(stream.size.0).unwrap_or(0),
        height: u32::try_from(stream.size.1).unwrap_or(0),
        scale_factor: 1.0,
    }
}

/// The portal's screenshot, as the picked monitor's frame. The portal hands over
/// the whole desktop and says where no monitor but the picked one lies, so a
/// desktop of several monitors is refused rather than cropped by a guess.
pub fn portal_frame(portal: &RefCell<Portal>) -> Result<image::RgbaImage, Refusal> {
    let stream = portal.borrow_mut().session()?;
    let image = portal.borrow_mut().screenshot()?;
    one_monitor(&stream, image.width(), image.height())?;
    Ok(image)
}

/// Pure: whether a desktop image of this size is the stream's monitor alone.
pub fn one_monitor(stream: &portal::Stream, width: u32, height: u32) -> Result<(), Refusal> {
    let (w, h) = (
        f64::from(stream.size.0.max(1)),
        f64::from(stream.size.1.max(1)),
    );
    let (across, down) = (f64::from(width) / w, f64::from(height) / h);
    let alone = stream.position == (0, 0) && (h * across - f64::from(height)).abs() <= 1.0;
    if alone && across >= 1.0 && down >= 1.0 {
        return Ok(());
    }
    Err(Refusal::Failed(format!(
        "the portal's screenshot is {width}x{height} and the picked monitor is {}x{} at ({},{}): \
         the desktop has more than that monitor, and the portal does not say where the others \
         lie, so compux captures a desktop of one monitor only",
        stream.size.0, stream.size.1, stream.position.0, stream.position.1
    )))
}

/// The portal route's input: the shared portal, its session started on first use.
pub struct PortalInput {
    portal: Rc<RefCell<Portal>>,
}

impl PortalInput {
    /// A global logical point as the stream's own coordinates, inside its monitor.
    fn in_stream(&self, x: i32, y: i32) -> Result<(f64, f64), String> {
        let stream = self
            .portal
            .borrow_mut()
            .session()
            .map_err(|refusal| refusal.detail().to_string())?;
        stream_point(&stream, x, y)
    }
}

/// Pure: a global logical point in a stream's coordinates, refused outside it.
pub fn stream_point(stream: &portal::Stream, x: i32, y: i32) -> Result<(f64, f64), String> {
    let (sx, sy) = (x - stream.position.0, y - stream.position.1);
    if sx < 0 || sy < 0 || sx >= stream.size.0 || sy >= stream.size.1 {
        return Err(format!(
            "({x},{y}) is outside the monitor picked for the session"
        ));
    }
    Ok((f64::from(sx), f64::from(sy)))
}

fn portal_button(button: Button) -> i32 {
    match button {
        Button::Left => portal::BTN_LEFT,
        Button::Right => portal::BTN_RIGHT,
    }
}

/// The keysym a key is sent as.
pub fn portal_keysym(key: Key) -> i32 {
    let raw = match key {
        Key::Unicode(c) => xkeysym::Keysym::from_char(c).raw(),
        Key::Meta => x11::keymap::SUPER_L,
        Key::Control => x11::keymap::CONTROL_L,
        Key::Alt => x11::keymap::ALT_L,
        Key::Shift => x11::keymap::SHIFT_L,
        Key::Return => x11::keymap::RETURN,
        Key::Tab => x11::keymap::TAB,
        Key::Escape => 0xff1b,
        Key::Space => 0x20,
        Key::Backspace => 0xff08,
        Key::Delete => 0xffff,
        Key::UpArrow => 0xff52,
        Key::DownArrow => 0xff54,
        Key::LeftArrow => 0xff51,
        Key::RightArrow => 0xff53,
        Key::Home => 0xff50,
        Key::End => 0xff57,
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
        Key::F1 => 0xffbe,
        Key::F2 => 0xffbf,
        Key::F3 => 0xffc0,
        Key::F4 => 0xffc1,
        Key::F5 => 0xffc2,
        Key::F6 => 0xffc3,
        Key::F7 => 0xffc4,
        Key::F8 => 0xffc5,
        Key::F9 => 0xffc6,
        Key::F10 => 0xffc7,
        Key::F11 => 0xffc8,
        Key::F12 => 0xffc9,
    };
    i32::try_from(raw).unwrap_or(0)
}

impl Platform for PortalInput {
    fn key(&mut self, key: Key, direction: Direction) -> Result<(), String> {
        let keysym = portal_keysym(key);
        if keysym == 0 {
            return Err(format!("key: {key:?} has no keysym"));
        }
        let mut portal = self.portal.borrow_mut();
        match direction {
            Direction::Press => portal.keysym(keysym, true),
            Direction::Release => portal.keysym(keysym, false),
            Direction::Click => portal
                .keysym(keysym, true)
                .and_then(|()| portal.keysym(keysym, false)),
        }
    }

    fn button(&mut self, button: Button, direction: Direction) -> Result<(), String> {
        let code = portal_button(button);
        let mut portal = self.portal.borrow_mut();
        match direction {
            Direction::Press => portal.button(code, true),
            Direction::Release => portal.button(code, false),
            Direction::Click => portal
                .button(code, true)
                .and_then(|()| portal.button(code, false)),
        }
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<(), String> {
        let (sx, sy) = self.in_stream(x, y).map_err(|e| format!("move: {e}"))?;
        self.portal.borrow_mut().motion(sx, sy)
    }

    /// The portal applies motion in order and has no way to read the pointer
    /// back, so there is nothing to wait on.
    fn settle(&mut self, _x: i32, _y: i32) -> Result<(), String> {
        Ok(())
    }

    fn drag_step(&mut self, x: i32, y: i32) -> Result<(), String> {
        let (sx, sy) = self.in_stream(x, y).map_err(|e| format!("drag: {e}"))?;
        self.portal.borrow_mut().motion(sx, sy)
    }

    fn scroll(&mut self, length: i32, axis: Axis) -> Result<(), String> {
        self.portal
            .borrow_mut()
            .axis(axis == Axis::Horizontal, length)
            .map_err(|e| format!("scroll: {e}"))
    }

    /// One keysym per character, at the typing pace X11 keeps. The compositor
    /// finds each on its own layout; one it cannot find is not typed, and the
    /// portal does not say so.
    fn text(&mut self, text: &str) -> Result<(), String> {
        let keysyms: Vec<i32> = text
            .chars()
            .map(|c| portal_keysym(Key::Unicode(if c == '\n' { '\r' } else { c })))
            .collect();
        if keysyms.contains(&0) {
            return Err("type: the text holds a character no key can type".to_string());
        }
        let mut portal = self.portal.borrow_mut();
        for keysym in keysyms {
            portal.keysym(keysym, true)?;
            portal.keysym(keysym, false)?;
            std::thread::sleep(std::time::Duration::from_millis(x11::KEY_PACE_MS));
        }
        Ok(())
    }

    fn sleep(&mut self, ms: u64) -> Result<(), String> {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(())
    }

    fn clipboard_text(&mut self) -> Result<Option<String>, String> {
        Err("clipboard: the portal route owns no clipboard".to_string())
    }

    fn set_clipboard_text(&mut self, _text: &str) -> Result<(), String> {
        Err("clipboard: the portal route owns no clipboard".to_string())
    }
}

/// Where the portal's restore token is kept: compux's own state file, under
/// `$XDG_STATE_HOME`, or its default beside `$HOME`.
pub fn state_file(state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = state_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let home = home.filter(|value| !value.is_empty())?;
            Some(PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("compux").join("portal.json"))
}

/// The portal facts of this Wayland session, from the session bus; `None` where
/// there is no session bus.
#[cfg(not(test))]
fn wayland_facts() -> Option<Wayland> {
    async_io::block_on(async {
        let session = super::bus::session().await.ok()?;
        Some(read_wayland(&session, super::probe::installed_version(PORTAL_PACKAGE)).await)
    })
}

/// What the session bus says of the compositor and the portal: two name checks
/// and one introspection, nothing that raises a dialog.
pub async fn read_wayland(session: &Connection, version: Option<String>) -> Wayland {
    let interfaces = introspected(session).await.unwrap_or_default();
    let has = |name: &str| interfaces.contains(&format!("\"{name}\""));
    Wayland {
        gnome: owned(session, GNOME_SHELL).await,
        kde: owned(session, KWIN).await,
        registry: has(portal::REGISTRY),
        screenshot: has(portal::SCREENSHOT),
        remote_desktop: has(portal::REMOTE_DESKTOP),
        screen_cast: has(portal::SCREEN_CAST),
        version,
    }
}

async fn owned(session: &Connection, name: &str) -> bool {
    let reply = session
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &(name,),
        )
        .await;
    reply.is_ok_and(|reply| reply.body().deserialize::<bool>().unwrap_or(false))
}

async fn introspected(session: &Connection) -> Option<String> {
    let reply = session
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.DBus.Introspectable"),
            "Introspect",
            &(),
        )
        .await
        .ok()?;
    reply.body().deserialize::<String>().ok()
}

/// A stored Screenshot answer for compux's app id: `yes`, `no`, or `unset`;
/// `None` where the store would not answer. Read with `Lookup`, never written.
pub async fn stored_screenshot(session: &Connection) -> Option<&'static str> {
    let reply = session
        .call_method(
            Some(PERMISSION_STORE),
            PERMISSION_STORE_PATH,
            Some(PERMISSION_STORE),
            "Lookup",
            &("screenshot", "screenshot"),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        // No table and no entry are the same fact: nothing was ever stored.
        Err(zbus::Error::MethodError(name, _, _)) if name.as_str().ends_with(".NotFound") => {
            return Some("unset")
        }
        Err(_) => return None,
    };
    let (permissions, _data): (HashMap<String, Vec<String>>, zbus::zvariant::OwnedValue) =
        reply.body().deserialize().ok()?;
    Some(
        match permissions
            .get(portal::APP_ID)
            .and_then(|answer| answer.first())
        {
            Some(answer) if answer == "yes" => "yes",
            Some(answer) if answer == "no" => "no",
            _ => "unset",
        },
    )
}

/// The `probe` fields for both routes: `x11_pixels` where an X endpoint answers,
/// `portal` where a session bus does.
pub fn probe_fields(
    x11: Option<x11::Facts>,
    portal: Option<(Wayland, Option<&'static str>)>,
) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "x11_pixels".to_string(),
        x11.map_or(
            Value::Null,
            |facts| json!({"xtest": facts.xtest, "keymap": facts.keymap}),
        ),
    );
    fields.insert(
        "portal".to_string(),
        portal.map_or(Value::Null, |(facts, stored)| {
            json!({"version": facts.version, "registry": facts.registry,
                   "screenshot_permission": stored})
        }),
    );
    fields
}

/// What `probe` reads for the two routes, now.
#[cfg(not(test))]
pub fn probe(endpoint: &Endpoint) -> Map<String, Value> {
    let x11 = endpoint
        .server()
        .x11_endpoint()
        .then(|| x11::Desktop::facts(&endpoint.env))
        .flatten();
    let portal = async_io::block_on(async {
        let session = super::bus::session().await.ok()?;
        let facts = read_wayland(&session, super::probe::installed_version(PORTAL_PACKAGE)).await;
        let stored = stored_screenshot(&session).await;
        Some((facts, stored))
    });
    probe_fields(x11, portal)
}

/// Under test there is no session this process may read.
#[cfg(test)]
pub fn probe(_endpoint: &Endpoint) -> Map<String, Value> {
    panic!("a unit test asked probe for the real session: give probe its facts instead")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fake_portal::{FakePortal, Script};
    use std::time::Duration;

    fn gnome() -> Wayland {
        Wayland {
            gnome: true,
            kde: false,
            registry: true,
            screenshot: true,
            remote_desktop: true,
            screen_cast: true,
            version: Some("1.18.4".to_string()),
        }
    }

    fn refused(decision: Decision) -> String {
        match decision {
            Decision::Refused(why) => why,
            other => panic!("not refused: {other:?}"),
        }
    }

    fn over(client: &Connection) -> Portal {
        let client = client.clone();
        Portal::with(
            Box::new(move || Ok(client.clone())),
            None,
            Duration::from_millis(300),
        )
    }

    fn stream(position: (i32, i32), size: (i32, i32)) -> portal::Stream {
        portal::Stream {
            node: 42,
            position,
            size,
        }
    }

    #[test]
    fn an_x11_session_takes_the_x11_route_whatever_its_bus_says() {
        assert_eq!(decide(Server::X11, None), Decision::X11);
        assert_eq!(decide(Server::X11, Some(&gnome())), Decision::X11);
    }

    // XWayland's X endpoint reaches only the session's X clients: a Wayland
    // session takes the portal even with one beside it, and never XTEST.
    #[test]
    fn a_gnome_wayland_session_takes_the_portal_even_with_xwayland_beside_it() {
        for x11 in [false, true] {
            assert_eq!(
                decide(Server::Wayland { x11 }, Some(&gnome())),
                Decision::Portal
            );
        }
    }

    #[test]
    fn a_portal_without_the_registry_is_refused_naming_its_version() {
        let old = Wayland {
            registry: false,
            version: Some("1.14.4".to_string()),
            ..gnome()
        };

        let why = refused(decide(Server::Wayland { x11: true }, Some(&old)));

        assert!(why.contains("1.14.4") && why.contains("1.18"), "{why}");
        assert!(why.contains(portal::APP_ID), "{why}");
    }

    #[test]
    fn kde_and_other_compositors_are_refused_by_name() {
        let kde = Wayland {
            gnome: false,
            kde: true,
            ..gnome()
        };
        let other = Wayland {
            gnome: false,
            ..gnome()
        };

        assert!(refused(decide(Server::Wayland { x11: false }, Some(&kde))).contains("KDE Plasma"));
        assert!(
            refused(decide(Server::Wayland { x11: false }, Some(&other)))
                .contains("this compositor")
        );
    }

    #[test]
    fn a_portal_missing_an_interface_is_refused_naming_it() {
        let no_input = Wayland {
            remote_desktop: false,
            ..gnome()
        };

        let why = refused(decide(Server::Wayland { x11: false }, Some(&no_input)));

        assert!(why.contains("RemoteDesktop"), "{why}");
    }

    #[test]
    fn no_display_and_no_session_bus_are_refused() {
        assert!(refused(decide(Server::None, None)).contains("no display"));
        assert!(refused(decide(Server::Wayland { x11: false }, None)).contains("session bus"));
    }

    // The portal's screenshot is the whole desktop and says nothing of where any
    // monitor but the picked one lies: only a desktop of that one monitor, at any
    // scale, is taken as its frame.
    #[test]
    fn only_a_desktop_of_the_picked_monitor_alone_is_its_frame() {
        let picked = stream((0, 0), (1280, 800));

        assert!(one_monitor(&picked, 1280, 800).is_ok());
        assert!(one_monitor(&picked, 2560, 1600).is_ok());
        let beside = one_monitor(&picked, 3200, 1080).expect_err("two monitors");
        let elsewhere = one_monitor(&stream((1920, 0), (1280, 800)), 1280, 800).expect_err("moved");

        assert_eq!(beside.code(), "capture_unavailable");
        assert!(beside.detail().contains("one monitor"), "{beside:?}");
        assert_eq!(elsewhere.code(), "capture_unavailable");
    }

    #[test]
    fn a_point_is_aimed_in_the_streams_own_coordinates_and_refused_outside_it() {
        let right = stream((1920, 0), (1280, 800));

        assert_eq!(stream_point(&right, 1920, 0), Ok((0.0, 0.0)));
        assert_eq!(stream_point(&right, 2000, 799), Ok((80.0, 799.0)));
        assert!(stream_point(&right, 1919, 10).is_err());
        assert!(stream_point(&right, 3200, 10).is_err());
    }

    #[test]
    fn keys_go_to_the_portal_as_keysyms() {
        assert_eq!(portal_keysym(Key::Unicode('a')), 0x61);
        assert_eq!(portal_keysym(Key::Unicode('\u{e9}')), 0xe9);
        assert_eq!(portal_keysym(Key::Return), 0xff0d);
        assert_eq!(portal_keysym(Key::Shift), 0xffe1);
        assert_eq!(portal_keysym(Key::F12), 0xffc9);
    }

    #[test]
    fn the_portal_route_serves_neither_paste_nor_windows() {
        let (_fake, client) = FakePortal::start(Script::default());
        let pixels = Pixels::on_portal(over(&client));

        for action in ["screenshot", "left_click", "type", "key", "scroll", "zoom"] {
            assert!(pixels.serves(action), "{action}");
        }
        for action in ["paste", "windows"] {
            assert!(!pixels.serves(action), "{action}");
            assert!(pixels.refusal(action).contains("GNOME Wayland"), "{action}");
        }
        assert!(pixels.portal());
    }

    // The pointer goes as absolute motion in the stream's coordinates, the
    // buttons as evdev codes, and nothing is asked of the portal until then.
    #[test]
    fn portal_input_moves_in_the_stream_and_clicks_by_evdev_code() {
        let (fake, client) = FakePortal::start(Script::default());
        let mut pixels = Pixels::on_portal(over(&client));
        assert!(
            fake.calls().is_empty(),
            "nothing asked before the first input"
        );

        let input = pixels.input().expect("the portal's input");
        input.move_mouse(100, 50).expect("moved");
        input
            .button(Button::Left, Direction::Click)
            .expect("clicked");
        let outside = input.move_mouse(1280, 0).expect_err("outside the monitor");
        pixels.release();

        assert!(outside.contains("outside the monitor"), "{outside}");
        let calls = fake.calls();
        let detail = |member: &str| -> Vec<String> {
            calls
                .iter()
                .filter(|call| call.member == member)
                .map(|call| call.detail.clone())
                .collect()
        };
        assert_eq!(detail("NotifyPointerMotionAbsolute"), ["42 100 50"]);
        assert_eq!(detail("NotifyPointerButton"), ["0x110 1", "0x110 0"]);
        assert_eq!(detail("Close").len(), 1, "released: {calls:?}");
    }

    #[test]
    fn the_portal_frame_is_the_screenshot_of_the_picked_monitor_alone() {
        let dir = std::env::temp_dir().join(format!("compux-pixels-frame-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let wide = dir.join("wide.png");
        image::RgbaImage::new(2560, 800)
            .save(&wide)
            .expect("the picture");
        let (_fake, client) = FakePortal::start(Script {
            screenshot_uri: format!("file://{}", wide.display()),
            ..Script::default()
        });
        let portal = RefCell::new(over(&client));

        let refusal = portal_frame(&portal).expect_err("two monitors wide");

        assert_eq!(refusal.code(), "capture_unavailable", "{refusal:?}");
    }

    // What `probe` reads of a Wayland session comes from two name checks and one
    // introspection: nothing that raises a dialog.
    #[test]
    fn the_wayland_facts_are_read_from_the_bus_names_and_the_portals_interfaces() {
        let (fake, client) = FakePortal::start(Script::default());
        let (old, old_client) = FakePortal::start(Script {
            owned: vec!["org.kde.KWin"],
            interfaces: vec![
                portal::SCREENSHOT,
                portal::REMOTE_DESKTOP,
                portal::SCREEN_CAST,
            ],
            ..Script::default()
        });

        let read = async_io::block_on(read_wayland(&client, Some("1.18.4".to_string())));
        let older = async_io::block_on(read_wayland(&old_client, None));

        assert_eq!(read, gnome());
        assert!(
            !older.gnome && older.kde && !older.registry && older.screenshot,
            "{older:?}"
        );
        let members = [fake.members(), old.members()].concat();
        assert!(
            members
                .iter()
                .all(|m| m == "NameHasOwner" || m == "Introspect"),
            "{members:?}"
        );
    }

    // The PermissionStore is read with Lookup for compux's own app id, and
    // never written: the fake fails a test that sends it a write.
    #[test]
    fn a_stored_screenshot_answer_is_looked_up_for_compux_alone() {
        let cases = [
            (None, Some("unset")),
            (Some(vec![(portal::APP_ID, "no")]), Some("no")),
            (Some(vec![(portal::APP_ID, "yes")]), Some("yes")),
            (Some(vec![("org.example.Other", "no")]), Some("unset")),
        ];
        for (table, expected) in cases {
            let (fake, client) = FakePortal::start(Script {
                stored: table.clone(),
                ..Script::default()
            });

            let answer = async_io::block_on(stored_screenshot(&client));

            assert_eq!(answer, expected, "{table:?}");
            assert_eq!(fake.members(), ["Lookup"]);
        }
    }

    // X has no logical coordinates: a monitor is in root pixels, which XTEST
    // takes too, whatever `Xft.dpi` says.
    #[test]
    fn an_x11_monitor_is_in_root_pixels_at_a_scale_of_one() {
        let monitor = x11::Monitor {
            id: 7,
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };

        let facts = x11_facts(&monitor);

        assert_eq!(
            (facts.x, facts.y, facts.width, facts.height),
            (-1920, 0, 1920, 1080)
        );
        assert_eq!(facts.scale_factor, 1.0);
    }

    #[test]
    fn an_x11_session_serves_every_pixel_action_paste_and_windows_among_them() {
        let pixels = Pixels::X11(Box::new(x11::Desktop::new(Env::default())), Env::default());

        for action in [
            "screenshot",
            "left_click",
            "type",
            "key",
            "paste",
            "windows",
        ] {
            assert!(pixels.serves(action), "{action}");
        }
        assert!(matches!(pixels.screen(), Screen::X11(_)));
        assert!(!pixels.portal());
    }

    #[test]
    fn a_refused_route_serves_nothing_and_every_refusal_carries_its_reason() {
        let mut pixels = Pixels::Refused("KDE Plasma on Wayland has no pixel route".to_string());

        let refusal = pixels.refusal("screenshot");
        let input = pixels.input().err();

        assert!(!pixels.serves("screenshot") && !pixels.serves("left_click"));
        assert!(
            refusal.starts_with("KDE Plasma") && refusal.ends_with("nothing was sent"),
            "{refusal}"
        );
        assert_eq!(
            input.as_deref(),
            Some("KDE Plasma on Wayland has no pixel route")
        );
        assert!(matches!(pixels.screen(), Screen::Refused(why) if why.starts_with("KDE Plasma")));
    }

    // The portal's one monitor is the one picked for the session, started on
    // first need; any other index is not there, and a refused route has none.
    #[test]
    fn the_portal_screen_is_the_one_monitor_picked_for_the_session() {
        let (fake, client) = FakePortal::start(Script {
            stream: (42, (0, 0), (1280, 800)),
            ..Script::default()
        });
        let screen = Pixels::on_portal(over(&client)).screen();

        let other = screen.monitor(1).expect_err("one monitor");
        let (facts, id) = screen.monitor(0).expect("the picked monitor");
        let refused = Screen::Refused("no route".to_string())
            .monitor(0)
            .expect_err("none");

        assert!(other.contains("display 1 not found"), "{other}");
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| call.member == "Start")
                .count(),
            1
        );
        assert_eq!(
            (facts.x, facts.y, facts.width, facts.height),
            (0, 0, 1280, 800)
        );
        assert_eq!(id, 42);
        assert_eq!(refused, "no_active_display");
    }

    #[test]
    fn the_restore_token_is_kept_under_the_state_home_or_its_default() {
        let named = state_file(Some("/s".into()), Some("/home/me".into()));
        let default = state_file(Some("".into()), Some("/home/me".into()));

        assert_eq!(named, Some(PathBuf::from("/s/compux/portal.json")));
        assert_eq!(
            default,
            Some(PathBuf::from("/home/me/.local/state/compux/portal.json"))
        );
        assert_eq!(state_file(None, None), None, "nowhere to keep it");
        assert_eq!(
            state_file(None, Some("".into())),
            None,
            "nowhere to keep it"
        );
    }

    #[test]
    fn probe_names_both_routes_facts_and_null_where_one_is_not_there() {
        let x11 = x11::Facts {
            xtest: true,
            keymap: false,
        };
        let facts = Wayland {
            version: Some("1.18.4".to_string()),
            ..gnome()
        };

        let both = probe_fields(Some(x11), Some((facts, Some("no"))));
        let neither = probe_fields(None, None);

        assert_eq!(both["x11_pixels"], json!({"xtest": true, "keymap": false}));
        assert_eq!(
            both["portal"],
            json!({"version": "1.18.4", "registry": true, "screenshot_permission": "no"})
        );
        assert_eq!(neither["x11_pixels"], Value::Null);
        assert_eq!(neither["portal"], Value::Null);
    }
}
