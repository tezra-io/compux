//! The windows on X11, from the window manager's own EWMH lists: the stacking
//! order, each client's title, class, process, frame extents and map state,
//! whether it is hidden (minimized), and which one is active. One walker serves
//! both the `windows` listing and the bound window's window server
//! ([`WindowServer`]): what is in front of what, where each window is, and
//! whether it can be seen.
//!
//! A window's place is the one the person sees: the client's rectangle less the
//! shadow margins GTK draws inside it (`_GTK_FRAME_EXTENTS`), plus the frame the
//! window manager draws around it (`_NET_FRAME_EXTENTS`). The rectangle without
//! the window manager's frame is kept beside it, because toolkits place their
//! accessibility frame at one or the other.
//!
//! Every request for every window is sent before any reply is read, so a
//! listing costs one round trip's wait, not one per property.

use x11rb::connection::Connection;
use x11rb::cookie::Cookie;
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, GetGeometryReply, GetPropertyReply, GetWindowAttributesReply,
    MapState, TranslateCoordinatesReply, Window as XWindow,
};
use x11rb::protocol::ErrorKind;
use x11rb::rust_connection::RustConnection;

use crate::linux::display::Env;
use crate::window_server::{Bounds, WindowFacts, Windows};

/// The most clients read from the window manager's list. A list longer than
/// this is refused rather than cut, since the windows cut would be the
/// front-most ones of a stacking list, which runs bottom to top.
pub const MAX_CLIENTS: u32 = 512;

/// The longest title or class read, in bytes.
const MAX_NAME_BYTES: u32 = 1024;

/// The longest title or application name a bound window's facts carry, as a
/// control's label is bounded: it is another application's text reaching a
/// reply the model reads.
const MAX_TITLE_CHARS: usize = 80;

/// The widest frame or shadow margin taken, in root pixels. A margin past this
/// is a property that went wrong, and the window is not placed by it.
const MAX_EXTENT: u32 = 4096;

/// A rectangle in root pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn bounds(self) -> Bounds {
        Bounds {
            x: f64::from(self.x),
            y: f64::from(self.y),
            w: f64::from(self.width),
            h: f64::from(self.height),
        }
    }
}

/// Four margins, in the order EWMH and GTK both write them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extents {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

/// One client window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub id: u32,
    /// The process that owns it, from `_NET_WM_PID`; `None` where the client
    /// names none.
    pub pid: Option<i32>,
    pub app: String,
    pub title: String,
    pub focused: bool,
    pub minimized: bool,
    /// Mapped, and every ancestor mapped: a window minimized or on another
    /// desktop is not.
    pub viewable: bool,
    /// Where the person sees it: the window manager's frame included, GTK's
    /// shadow left out.
    pub bounds: Rect,
    /// The same without the window manager's frame.
    pub content: Rect,
}

/// The windows, front-most first, and whether that order is the window
/// manager's stacking order: a window manager that publishes only
/// `_NET_CLIENT_LIST` gives the order windows were mapped in instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub windows: Vec<Window>,
    pub stacked: bool,
}

struct Atoms {
    stacking: u32,
    clients: u32,
    active: u32,
    name: u32,
    utf8: u32,
    state: u32,
    hidden: u32,
    pid: u32,
    frame: u32,
    shadow: u32,
}

/// The client windows, front-most first, on a connection of its own.
pub fn list(env: &Env) -> Result<Listed, String> {
    let (connection, screen) =
        super::connect(env).map_err(|why| format!("enumerate windows: {why}"))?;
    let root = connection
        .setup()
        .roots
        .get(screen)
        .map(|screen| screen.root)
        .ok_or("enumerate windows: the X display has no such screen")?;
    let atoms = atoms(&connection)?;
    let stacking = cardinals(&connection, root, atoms.stacking, MAX_CLIENTS)?;
    let stacked = !stacking.is_empty();
    let ids = if stacked {
        stacking
    } else {
        cardinals(&connection, root, atoms.clients, MAX_CLIENTS)?
    };
    let active = cardinals(&connection, root, atoms.active, 1)?
        .first()
        .copied();
    let asked = front_to_back(ids)
        .into_iter()
        .map(|id| Asked::send(&connection, root, &atoms, id))
        .collect::<Result<Vec<_>, String>>()?;
    let mut windows = Vec::with_capacity(asked.len());
    for asked in asked {
        if let Some(window) = asked.answer(&atoms, active)? {
            windows.push(window);
        }
    }
    Ok(Listed { windows, stacked })
}

/// A reply about one window, or `None` where the window went away while it was
/// being read. Any other refusal, and a connection that failed, is the
/// listing's fault, never a window quietly left out.
fn present<T>(reply: Result<T, ReplyError>) -> Result<Option<T>, String> {
    match reply {
        Ok(reply) => Ok(Some(reply)),
        Err(ReplyError::X11Error(error))
            if matches!(error.error_kind, ErrorKind::Window | ErrorKind::Drawable) =>
        {
            Ok(None)
        }
        Err(error) => Err(format!("enumerate windows: {error}")),
    }
}

/// The stacking list runs bottom to top, and a window listed twice is the
/// front-most of its places: the caller reads the front first, each window once.
pub fn front_to_back(bottom_to_top: Vec<u32>) -> Vec<u32> {
    let mut seen = std::collections::HashSet::new();
    bottom_to_top
        .into_iter()
        .rev()
        .filter(|id| seen.insert(*id))
        .collect()
}

/// Four margins out of a property, or `None` where it is not four values each
/// within [`MAX_EXTENT`].
pub fn extents(values: &[u32]) -> Option<Extents> {
    let [left, right, top, bottom] = <[u32; 4]>::try_from(values).ok()?;
    if [left, right, top, bottom]
        .iter()
        .any(|&side| side > MAX_EXTENT)
    {
        return None;
    }
    Some(Extents {
        left,
        right,
        top,
        bottom,
    })
}

/// Where the person sees a client, and its own area without the window
/// manager's frame: the client's root rectangle less GTK's shadow, then the
/// frame around that. `None` where the shadow leaves nothing of the window, or
/// the result does not fit the root's coordinates.
pub fn placed(client: Rect, frame: Extents, shadow: Extents) -> Option<(Rect, Rect)> {
    let content = shrink(client, shadow)?;
    let seen = Rect {
        x: offset(content.x, -i64::from(frame.left))?,
        y: offset(content.y, -i64::from(frame.top))?,
        width: grow(content.width, frame.left, frame.right)?,
        height: grow(content.height, frame.top, frame.bottom)?,
    };
    Some((seen, content))
}

fn shrink(client: Rect, shadow: Extents) -> Option<Rect> {
    let width = client
        .width
        .checked_sub(shadow.left)?
        .checked_sub(shadow.right)?;
    let height = client
        .height
        .checked_sub(shadow.top)?
        .checked_sub(shadow.bottom)?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect {
        x: offset(client.x, i64::from(shadow.left))?,
        y: offset(client.y, i64::from(shadow.top))?,
        width,
        height,
    })
}

fn offset(at: i32, by: i64) -> Option<i32> {
    i32::try_from(i64::from(at) + by).ok()
}

fn grow(side: u32, before: u32, after: u32) -> Option<u32> {
    side.checked_add(before)?.checked_add(after)
}

fn atoms(connection: &RustConnection) -> Result<Atoms, String> {
    let names = [
        "_NET_CLIENT_LIST_STACKING",
        "_NET_CLIENT_LIST",
        "_NET_ACTIVE_WINDOW",
        "_NET_WM_NAME",
        "UTF8_STRING",
        "_NET_WM_STATE",
        "_NET_WM_STATE_HIDDEN",
        "_NET_WM_PID",
        "_NET_FRAME_EXTENTS",
        "_GTK_FRAME_EXTENTS",
    ];
    let cookies = names
        .iter()
        .map(|name| connection.intern_atom(false, name.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("enumerate windows: {error}"))?;
    let atoms = cookies
        .into_iter()
        .map(|cookie| cookie.reply().map(|reply| reply.atom))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("enumerate windows: {error}"))?;
    let [stacking, clients, active, name, utf8, state, hidden, pid, frame, shadow] =
        <[u32; 10]>::try_from(atoms).map_err(|_| "enumerate windows: an atom went missing")?;
    Ok(Atoms {
        stacking,
        clients,
        active,
        name,
        utf8,
        state,
        hidden,
        pid,
        frame,
        shadow,
    })
}

/// Every request one window's facts need, sent and not yet answered.
struct Asked<'c> {
    id: u32,
    geometry: Cookie<'c, RustConnection, GetGeometryReply>,
    at: Cookie<'c, RustConnection, TranslateCoordinatesReply>,
    attributes: Cookie<'c, RustConnection, GetWindowAttributesReply>,
    net_name: Cookie<'c, RustConnection, GetPropertyReply>,
    wm_name: Cookie<'c, RustConnection, GetPropertyReply>,
    class: Cookie<'c, RustConnection, GetPropertyReply>,
    state: Cookie<'c, RustConnection, GetPropertyReply>,
    pid: Cookie<'c, RustConnection, GetPropertyReply>,
    frame: Cookie<'c, RustConnection, GetPropertyReply>,
    shadow: Cookie<'c, RustConnection, GetPropertyReply>,
}

impl<'c> Asked<'c> {
    fn send(
        connection: &'c RustConnection,
        root: XWindow,
        atoms: &Atoms,
        id: u32,
    ) -> Result<Asked<'c>, String> {
        let property = |name: u32, kind: AtomEnum, longs: u32| {
            connection.get_property(false, id, name, kind, 0, longs)
        };
        let text = MAX_NAME_BYTES / 4;
        let sent = (|| {
            Ok::<_, x11rb::errors::ConnectionError>(Asked {
                id,
                geometry: connection.get_geometry(id)?,
                at: connection.translate_coordinates(id, root, 0, 0)?,
                attributes: connection.get_window_attributes(id)?,
                net_name: connection.get_property(false, id, atoms.name, atoms.utf8, 0, text)?,
                wm_name: property(AtomEnum::WM_NAME.into(), AtomEnum::STRING, text)?,
                class: property(AtomEnum::WM_CLASS.into(), AtomEnum::STRING, text)?,
                state: property(atoms.state, AtomEnum::ANY, 64)?,
                pid: property(atoms.pid, AtomEnum::CARDINAL, 1)?,
                frame: property(atoms.frame, AtomEnum::CARDINAL, 4)?,
                shadow: property(atoms.shadow, AtomEnum::CARDINAL, 4)?,
            })
        })();
        sent.map_err(|error| format!("enumerate windows: {error}"))
    }

    /// The window, or `None` where it went away while it was being read, or
    /// cannot be placed. Any other refusal, and a connection that failed, is the
    /// listing's fault ([`present`]).
    fn answer(self, atoms: &Atoms, active: Option<u32>) -> Result<Option<Window>, String> {
        let (Some(geometry), Some(at), Some(attributes)) = (
            present(self.geometry.reply())?,
            present(self.at.reply())?,
            present(self.attributes.reply())?,
        ) else {
            return Ok(None);
        };
        let net_name = text(present(self.net_name.reply())?).filter(|title| !title.is_empty());
        let title = net_name
            .or(text(present(self.wm_name.reply())?))
            .unwrap_or_default();
        let class = text(present(self.class.reply())?).unwrap_or_default();
        let states = values(present(self.state.reply())?);
        let pid = values(present(self.pid.reply())?)
            .first()
            .and_then(|&pid| i32::try_from(pid).ok())
            .filter(|&pid| pid > 0);
        let frame = extents(&values(present(self.frame.reply())?)).unwrap_or_default();
        let shadow = extents(&values(present(self.shadow.reply())?)).unwrap_or_default();
        let client = Rect {
            x: i32::from(at.dst_x),
            y: i32::from(at.dst_y),
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
        };
        let Some((bounds, content)) = placed(client, frame, shadow) else {
            return Ok(None);
        };
        Ok(Some(Window {
            id: self.id,
            pid,
            // WM_CLASS is "instance\0Class\0": the class names the application.
            app: class.split('\0').nth(1).unwrap_or_default().to_string(),
            title,
            focused: active == Some(self.id),
            minimized: states.contains(&atoms.hidden),
            viewable: attributes.map_state == MapState::VIEWABLE,
            bounds,
            content,
        }))
    }
}

/// A property of 32-bit values (windows, atoms), at most `max` of them: a
/// property with more is refused, never cut.
fn cardinals(
    connection: &RustConnection,
    window: XWindow,
    property: u32,
    max: u32,
) -> Result<Vec<u32>, String> {
    let reply = connection
        .get_property(false, window, property, AtomEnum::ANY, 0, max)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| format!("enumerate windows: {error}"))?;
    if reply.bytes_after != 0 {
        return Err(format!(
            "enumerate windows: the window manager lists more than {max} windows, the most \
             compux reads"
        ));
    }
    Ok(values(Some(reply)))
}

fn values(reply: Option<GetPropertyReply>) -> Vec<u32> {
    reply
        .and_then(|reply| reply.value32().map(Iterator::collect))
        .unwrap_or_default()
}

/// A text property, as UTF-8 (STRING is Latin-1, which maps to the same code
/// points).
fn text(reply: Option<GetPropertyReply>) -> Option<String> {
    let reply = reply?;
    if reply.type_ == u32::from(AtomEnum::STRING) {
        return Some(reply.value.iter().map(|&b| char::from(b)).collect());
    }
    String::from_utf8(reply.value).ok()
}

/// The window server a bound window lives by, on X11.
pub struct WindowServer {
    env: Env,
}

impl WindowServer {
    pub fn new(env: Env) -> WindowServer {
        WindowServer { env }
    }
}

impl Windows for WindowServer {
    /// Refused where the window manager publishes no stacking order: without
    /// it, what is in front of a window cannot be told, and a guess is how a
    /// click lands on whatever covers the target.
    fn list(&self) -> Result<Vec<WindowFacts>, String> {
        let listed = list(&self.env)?;
        if !listed.stacked && !listed.windows.is_empty() {
            return Err(
                "the window manager publishes no stacking order (_NET_CLIENT_LIST_STACKING), \
                 so what is in front of a window cannot be told"
                    .to_string(),
            );
        }
        Ok(listed.windows.into_iter().map(facts).collect())
    }

    fn unlisted_cover(&self, target_id: u32, x: i32, y: i32) -> Result<Option<String>, String> {
        super::above::covering(&self.env, target_id, x, y)
    }
}

/// Whether a window read again is still the one the listing named: its
/// `_NET_WM_PID` is the listed process, whose start time the binding holds. An
/// id reused between the two names another process's window. The title is not
/// compared: windows retitle themselves all the time.
pub fn still_named(listed_pid: i32, pid: Option<i32>) -> bool {
    pid == Some(listed_pid)
}

/// One window's `_NET_WM_PID`, read as the listing reads it.
pub fn owner(connection: &RustConnection, window: XWindow) -> Result<Option<i32>, String> {
    let atoms = atoms(connection)?;
    let pid = connection
        .get_property(false, window, atoms.pid, AtomEnum::CARDINAL, 0, 1)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| format!("read the window's process: {error}"))?;
    Ok(values(Some(pid))
        .first()
        .and_then(|&pid| i32::try_from(pid).ok())
        .filter(|&pid| pid > 0))
}

/// One window as a bound target reads it. No pid is `0`, which names no
/// process, so nothing can be bound to it.
pub fn facts(window: Window) -> WindowFacts {
    WindowFacts {
        id: window.id,
        pid: window.pid.unwrap_or(0),
        app: crate::ax::one_line(&window.app, MAX_TITLE_CHARS),
        title: crate::ax::one_line(&window.title, MAX_TITLE_CHARS),
        bounds: window.bounds.bounds(),
        content: Some(window.content.bounds()),
        // X has no layers: docks and panels take their place in the stacking
        // order like any other window.
        layer: 0,
        on_screen: window.viewable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::errors::ConnectionError;
    use x11rb::x11_utils::X11Error;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn margins(left: u32, right: u32, top: u32, bottom: u32) -> Extents {
        Extents {
            left,
            right,
            top,
            bottom,
        }
    }

    // The stacking list is bottom to top; the front is read first, and a window
    // the list names twice is at its front-most place only.
    #[test]
    fn the_stacking_list_is_read_front_to_back_with_each_window_once() {
        assert_eq!(front_to_back(vec![10, 20, 30]), vec![30, 20, 10]);
        assert_eq!(front_to_back(vec![10, 20, 10, 30]), vec![30, 10, 20]);
        assert!(front_to_back(Vec::new()).is_empty());
    }

    // Openbox around the GTK 3 fixture, as the container showed it: a 980x770
    // client at (1, 249) in a frame of 1, 1, 20, 5 is seen as 982x795 at (0, 229),
    // the rectangle GTK 3's accessibility frame reports.
    #[test]
    fn the_window_managers_frame_is_added_around_the_client() {
        let (seen, content) = placed(
            rect(1, 249, 980, 770),
            margins(1, 1, 20, 5),
            Extents::default(),
        )
        .expect("placed");
        assert_eq!(seen, rect(0, 229, 982, 795));
        assert_eq!(content, rect(1, 249, 980, 770));
    }

    // GTK's shadow is inside the client and is not the window: it comes off
    // before the window manager's frame goes on.
    #[test]
    fn gtks_shadow_comes_off_the_client_before_the_frame_goes_on() {
        let (seen, content) = placed(
            rect(100, 100, 840, 640),
            margins(0, 0, 0, 0),
            margins(20, 20, 14, 26),
        )
        .expect("placed");
        assert_eq!(content, rect(120, 114, 800, 600));
        assert_eq!(seen, content);

        let (seen, _) = placed(
            rect(100, 100, 840, 640),
            margins(2, 2, 30, 2),
            margins(20, 20, 14, 26),
        )
        .expect("placed");
        assert_eq!(seen, rect(118, 84, 804, 632));
    }

    // A shadow wider than the window leaves nothing to place, and a window at
    // the edge of the root's coordinates is not wrapped.
    #[test]
    fn a_window_its_extents_leave_nothing_of_is_not_placed() {
        let none = Extents::default();
        assert_eq!(
            placed(rect(0, 0, 40, 40), none, margins(20, 20, 0, 0)),
            None
        );
        assert_eq!(
            placed(rect(0, 0, 40, 40), none, margins(30, 20, 0, 0)),
            None
        );
        assert_eq!(
            placed(rect(i32::MIN, 0, 40, 40), margins(1, 0, 0, 0), none),
            None
        );
        assert_eq!(
            placed(rect(0, 0, u32::MAX, 40), margins(0, 1, 0, 0), none),
            None
        );
    }

    #[test]
    fn extents_are_four_values_each_within_reason() {
        assert_eq!(extents(&[1, 2, 3, 4]), Some(margins(1, 2, 3, 4)));
        assert_eq!(extents(&[1, 2, 3]), None);
        assert_eq!(extents(&[1, 2, 3, 4, 5]), None);
        assert_eq!(extents(&[1, 2, MAX_EXTENT + 1, 4]), None);
        assert_eq!(extents(&[]), None);
    }

    #[test]
    fn a_window_that_went_away_is_left_out_and_a_failed_connection_is_the_listings_fault() {
        let gone = X11Error {
            error_kind: ErrorKind::Window,
            error_code: 3,
            sequence: 9,
            bad_value: 0x40_0001,
            minor_opcode: 0,
            major_opcode: 14,
            extension_name: None,
            request_name: Some("GetGeometry"),
        };
        assert_eq!(present::<u32>(Ok(7)), Ok(Some(7)));
        assert_eq!(
            present::<u32>(Err(ReplyError::X11Error(gone.clone()))),
            Ok(None)
        );
        let refused = X11Error {
            error_kind: ErrorKind::Atom,
            error_code: 5,
            ..gone
        };
        assert!(present::<u32>(Err(ReplyError::X11Error(refused))).is_err());
        let lost = ReplyError::ConnectionError(ConnectionError::UnknownError);
        assert!(present::<u32>(Err(lost)).is_err());
    }

    #[test]
    fn a_window_read_again_is_the_listed_one_by_its_process_alone() {
        assert!(still_named(4242, Some(4242)));
        assert!(!still_named(4242, Some(4243)));
        assert!(!still_named(4242, None));
    }

    // A bound window's facts: no pid names no process, the text is one bounded
    // line, and a window that is not viewable is off screen.
    #[test]
    fn a_window_becomes_the_facts_a_binding_reads() {
        let window = Window {
            id: 0x40_0003,
            pid: None,
            app: "Fixture".to_string(),
            title: format!("line one\nline two {}", "x".repeat(200)),
            focused: false,
            minimized: true,
            viewable: false,
            bounds: rect(0, 229, 982, 795),
            content: rect(1, 249, 980, 770),
        };
        let facts = facts(window);
        assert_eq!(facts.pid, 0);
        assert!(!facts.title.contains('\n'), "{}", facts.title);
        assert!(
            facts.title.chars().count() <= MAX_TITLE_CHARS,
            "{}",
            facts.title
        );
        assert!(!facts.on_screen);
        assert_eq!(facts.bounds, rect(0, 229, 982, 795).bounds());
        assert_eq!(facts.content, Some(rect(1, 249, 980, 770).bounds()));
    }
}
