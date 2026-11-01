//! The windows a `windows` listing names on X11, from the window manager's own
//! EWMH lists: the stacking order, each client's title and class, whether it is
//! hidden (minimized), which one is active, and where it is on the root. What the
//! xcap listing it replaces read, on the same properties.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window as XWindow};
use x11rb::rust_connection::RustConnection;

use crate::linux::display::Env;

/// The most clients read from the window manager's list.
const MAX_CLIENTS: u32 = 512;

/// The longest title or class read, in bytes.
const MAX_NAME_BYTES: u32 = 1024;

/// One client window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub id: u32,
    pub app: String,
    pub title: String,
    pub focused: bool,
    pub minimized: bool,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

struct Atoms {
    stacking: u32,
    clients: u32,
    active: u32,
    name: u32,
    utf8: u32,
    state: u32,
    hidden: u32,
}

/// The client windows, front-most first, on a connection of its own.
pub fn list(env: &Env) -> Result<Vec<Window>, String> {
    let (connection, screen) =
        super::connect(env).map_err(|why| format!("enumerate windows: {why}"))?;
    let root = connection
        .setup()
        .roots
        .get(screen)
        .map(|screen| screen.root)
        .ok_or("enumerate windows: the X display has no such screen")?;
    let atoms = atoms(&connection)?;
    let mut ids = cardinals(&connection, root, atoms.stacking, MAX_CLIENTS)?;
    if ids.is_empty() {
        ids = cardinals(&connection, root, atoms.clients, MAX_CLIENTS)?;
    }
    let active = cardinals(&connection, root, atoms.active, 1)?
        .first()
        .copied();
    // The stacking list runs bottom to top; the caller reads the front first.
    ids.reverse();
    Ok(ids
        .into_iter()
        .filter_map(|id| client(&connection, root, &atoms, id, active))
        .collect())
}

fn atoms(connection: &RustConnection) -> Result<Atoms, String> {
    let atom = |name: &str| -> Result<u32, String> {
        connection
            .intern_atom(false, name.as_bytes())
            .map_err(|error| error.to_string())?
            .reply()
            .map(|reply| reply.atom)
            .map_err(|error| format!("enumerate windows: {error}"))
    };
    Ok(Atoms {
        stacking: atom("_NET_CLIENT_LIST_STACKING")?,
        clients: atom("_NET_CLIENT_LIST")?,
        active: atom("_NET_ACTIVE_WINDOW")?,
        name: atom("_NET_WM_NAME")?,
        utf8: atom("UTF8_STRING")?,
        state: atom("_NET_WM_STATE")?,
        hidden: atom("_NET_WM_STATE_HIDDEN")?,
    })
}

/// One client, or `None` where it went away while it was being read.
fn client(
    connection: &RustConnection,
    root: XWindow,
    atoms: &Atoms,
    id: u32,
    active: Option<u32>,
) -> Option<Window> {
    let geometry = connection.get_geometry(id).ok()?.reply().ok()?;
    let at = connection
        .translate_coordinates(id, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    let title = text(connection, id, atoms.name, atoms.utf8)
        .filter(|title| !title.is_empty())
        .or_else(|| {
            text(
                connection,
                id,
                AtomEnum::WM_NAME.into(),
                AtomEnum::STRING.into(),
            )
        })
        .unwrap_or_default();
    let class = text(
        connection,
        id,
        AtomEnum::WM_CLASS.into(),
        AtomEnum::STRING.into(),
    )
    .unwrap_or_default();
    let states = cardinals(connection, id, atoms.state, 64).unwrap_or_default();
    Some(Window {
        id,
        // WM_CLASS is "instance\0Class\0": the class names the application.
        app: class.split('\0').nth(1).unwrap_or_default().to_string(),
        title,
        focused: active == Some(id),
        minimized: states.contains(&atoms.hidden),
        x: i32::from(at.dst_x),
        y: i32::from(at.dst_y),
        width: u32::from(geometry.width),
        height: u32::from(geometry.height),
    })
}

/// A property of 32-bit values (windows, atoms), at most `max` of them.
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
    Ok(reply.value32().map(Iterator::collect).unwrap_or_default())
}

/// A text property, as UTF-8 (STRING is Latin-1, which maps to the same code
/// points).
fn text(connection: &RustConnection, window: XWindow, property: u32, kind: u32) -> Option<String> {
    let reply = connection
        .get_property(false, window, property, kind, 0, MAX_NAME_BYTES / 4)
        .ok()?
        .reply()
        .ok()?;
    if reply.type_ == u32::from(AtomEnum::STRING) {
        return Some(reply.value.iter().map(|&b| char::from(b)).collect());
    }
    String::from_utf8(reply.value).ok()
}
