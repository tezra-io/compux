//! The badge's window on the X server.
//!
//! An override-redirect window, so no window manager frames it, moves it, lists it
//! or gives it focus, and this process never asks for focus either. Its input
//! region is the two buttons and nothing else (Shape), so a click anywhere else on
//! it reaches the window under it. It is connected to the display and with the
//! authority the helper put in this process's environment (`DISPLAY`,
//! `XAUTHORITY`), and it goes with the connection: when this process exits, for
//! whatever reason, the server destroys the window.
//!
//! Nothing here decides anything. Where the badge goes is `place.rs`, what it
//! looks like is `draw.rs`, and which pixels take clicks is `layout.rs`.

use std::sync::mpsc::Sender;
use std::sync::Arc;

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    EventMask, ImageFormat, ImageOrder, PropMode, StackMode, VisualClass, WindowClass,
};
use x11rb::protocol::{ErrorKind, Event};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::draw::{Canvas, BACKGROUND};
use crate::place::{Rect, Screen};

/// The badge's title, for anything that lists windows by name.
pub const TITLE: &str = "Fermix computer use";
/// The Shape version that first has input regions.
const SHAPE_INPUT: (u32, u32) = (1, 1);
/// The RandR version that first names monitors.
const RANDR_MONITORS: (u32, u32) = (1, 5);
/// How many `_NET_WORKAREA` values are read: four per desktop, for 64 desktops.
const MAX_WORKAREA_VALUES: u32 = 256;
/// The most windows read above the badge when it looks for a menu or popup open
/// over it. Past this none is read, and the badge stays where it is.
pub const MAX_ABOVE: usize = 1_024;

/// What the X side hands the badge's one loop.
pub enum Delivered {
    Event(Event),
    /// The connection failed; nothing more will arrive.
    Lost(String),
}

/// A window stacked above the badge, as one look found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stacked {
    pub id: u32,
    pub viewable: bool,
    pub override_redirect: bool,
    /// Where it takes input, in root pixels; `None` where it was not read (a
    /// window not viewable, one the window manager manages, or one nowhere near
    /// the badge).
    pub input: Option<Vec<Rect>>,
}

/// The atoms the badge reads or writes.
struct Atoms {
    workarea: u32,
    current_desktop: u32,
}

/// The badge's window and the connection it lives on.
pub struct Window {
    conn: Arc<RustConnection>,
    root: u32,
    id: u32,
    gc: u32,
    depth: u8,
    monitors: bool,
    atoms: Atoms,
    mapped: bool,
}

impl Window {
    /// Connect, check the display draws the badge's pixels as they are, and make
    /// the window, unmapped.
    pub fn open() -> Result<Window, String> {
        let (conn, screen_number) = RustConnection::connect(None)
            .map_err(|e| format!("could not connect to the X display: {e}"))?;
        let screen = conn
            .setup()
            .roots
            .get(screen_number)
            .ok_or_else(|| format!("the X display has no screen {screen_number}"))?
            .clone();
        pixel_format(conn.setup(), &screen)?;
        shape_input(&conn)?;
        let monitors = names_monitors(&conn)?;

        let id = conn.generate_id().map_err(lost)?;
        let aux = CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(BACKGROUND)
            .border_pixel(0)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE);
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            id,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &aux,
        )
        .map_err(lost)?;
        let gc = conn.generate_id().map_err(lost)?;
        conn.create_gc(gc, id, &CreateGCAux::new().graphics_exposures(0))
            .map_err(lost)?;

        let atoms = name(&conn, id)?;
        watch_root(&conn, screen.root, monitors)?;
        conn.flush().map_err(lost)?;

        Ok(Window {
            conn: Arc::new(conn),
            root: screen.root,
            id,
            gc,
            depth: screen.root_depth,
            monitors,
            atoms,
            mapped: false,
        })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// Hand every event, and the connection's end, to `deliver` (each wrapped by
    /// `wrap`) from a thread of its own. Bounded by the connection: it ends when the
    /// connection does or when nothing is listening any more.
    pub fn listen<T: Send + 'static>(
        &self,
        deliver: Sender<T>,
        wrap: fn(Delivered) -> T,
    ) -> Result<(), String> {
        let conn = self.conn.clone();
        std::thread::Builder::new()
            .name("compux-indicator-x11 events".to_string())
            .spawn(move || loop {
                let delivered = match conn.wait_for_event() {
                    Ok(event) => Delivered::Event(event),
                    Err(e) => Delivered::Lost(e.to_string()),
                };
                let last = matches!(delivered, Delivered::Lost(_));
                if deliver.send(wrap(delivered)).is_err() || last {
                    return;
                }
            })
            .map(|_thread| ())
            .map_err(|e| format!("the X event reader would not start: {e}"))
    }

    /// The monitors, each with the part of it the desktop's panels leave free.
    pub fn screens(&self) -> Result<Vec<Screen>, String> {
        let frames = self.monitor_frames()?;
        let work = self.workarea()?;
        Ok(frames
            .into_iter()
            .map(|frame| Screen {
                frame,
                visible: match work {
                    None => frame,
                    // A monitor the work area misses has no room at all.
                    Some(work) => frame.intersection(work).unwrap_or(Rect {
                        w: 0,
                        h: 0,
                        ..frame
                    }),
                },
            })
            .collect())
    }

    /// Put the badge at `frame`, taking clicks only on `buttons`, drawn. Where it
    /// stacks is [`Window::raise`]'s. The input region is set before the window
    /// is mapped, so there is no moment when the whole badge swallows clicks.
    pub fn show(&mut self, frame: Rect, canvas: &Canvas, buttons: [Rect; 2]) -> Result<(), String> {
        let input = buttons.map(|button| xproto::Rectangle {
            x: button.x as i16,
            y: button.y as i16,
            width: button.w as u16,
            height: button.h as u16,
        });
        self.conn
            .shape_rectangles(
                shape::SO::SET,
                shape::SK::INPUT,
                xproto::ClipOrdering::UNSORTED,
                self.id,
                0,
                0,
                &input,
            )
            .map_err(lost)?;
        let place = ConfigureWindowAux::new()
            .x(frame.x)
            .y(frame.y)
            .width(frame.w as u32)
            .height(frame.h as u32);
        self.conn.configure_window(self.id, &place).map_err(lost)?;
        if !self.mapped {
            self.conn.map_window(self.id).map_err(lost)?;
            self.mapped = true;
        }
        self.paint(canvas)
    }

    /// Off the screen. Nothing is drawn where nothing true can be said.
    pub fn hide(&mut self) -> Result<(), String> {
        if self.mapped {
            self.conn.unmap_window(self.id).map_err(lost)?;
            self.mapped = false;
        }
        self.conn.flush().map_err(lost)
    }

    /// Another window's outer rectangle, border included, in root pixels; `None`
    /// where it is gone.
    pub fn outer_of(&self, window: u32) -> Result<Option<Rect>, String> {
        let geometry = present(self.conn.get_geometry(window).map_err(lost)?.reply())?;
        Ok(geometry.map(|g| outer(g.x, g.y, g.width, g.height, g.border_width)))
    }

    /// The root's children stacked above the badge, bottom to top, each read as
    /// far as telling a menu or popup open over `badge` needs; `None` where more
    /// than [`MAX_ABOVE`] are, and none is read. A window gone since is left out.
    pub fn above(&self, badge: Rect) -> Result<Option<Vec<Stacked>>, String> {
        let tree = self
            .conn
            .query_tree(self.root)
            .map_err(lost)?
            .reply()
            .map_err(lost)?;
        let at = tree
            .children
            .iter()
            .position(|&window| window == self.id)
            .ok_or_else(|| "the badge's window is not among the root's children".to_string())?;
        let ids = &tree.children[at + 1..];
        if ids.len() > MAX_ABOVE {
            return Ok(None);
        }
        self.stacked(ids, badge).map(Some)
    }

    /// Each of `ids`: whether it is viewable and override-redirect, and, for one
    /// that is both and meets `badge`, where it takes input.
    fn stacked(&self, ids: &[u32], badge: Rect) -> Result<Vec<Stacked>, String> {
        let mut asked = Vec::with_capacity(ids.len());
        for &id in ids {
            let attributes = self.conn.get_window_attributes(id).map_err(lost)?;
            asked.push((id, attributes, self.conn.get_geometry(id).map_err(lost)?));
        }
        let mut found = Vec::with_capacity(asked.len());
        for (id, attributes, geometry) in asked {
            let (Some(attributes), Some(geometry)) =
                (present(attributes.reply())?, present(geometry.reply())?)
            else {
                continue;
            };
            let viewable = attributes.map_state == xproto::MapState::VIEWABLE;
            let override_redirect = attributes.override_redirect;
            let g = &geometry;
            let near = outer(g.x, g.y, g.width, g.height, g.border_width)
                .intersection(badge)
                .is_some();
            let input = if viewable && override_redirect && near {
                self.input_of(id, g)?
            } else {
                None
            };
            found.push(Stacked {
                id,
                viewable,
                override_redirect,
                input,
            });
        }
        Ok(found)
    }

    /// Where `id` takes input, in root pixels: its SHAPE input region (the whole
    /// window and its border where none was set), which counts from the corner
    /// inside its border. `None` where it is gone.
    fn input_of(
        &self,
        id: u32,
        geometry: &xproto::GetGeometryReply,
    ) -> Result<Option<Vec<Rect>>, String> {
        let reply = self
            .conn
            .shape_get_rectangles(id, shape::SK::INPUT)
            .map_err(lost)?
            .reply();
        let Some(reply) = present(reply)? else {
            return Ok(None);
        };
        let border = i32::from(geometry.border_width);
        let (x, y) = (
            i32::from(geometry.x) + border,
            i32::from(geometry.y) + border,
        );
        Ok(Some(
            reply
                .rectangles
                .iter()
                .map(|r| Rect {
                    x: x + i32::from(r.x),
                    y: y + i32::from(r.y),
                    w: i32::from(r.width),
                    h: i32::from(r.height),
                })
                .collect(),
        ))
    }

    /// Back on top of every other window, after one was raised over it.
    pub fn raise(&self) -> Result<(), String> {
        let top = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);
        self.conn.configure_window(self.id, &top).map_err(lost)?;
        self.conn.flush().map_err(lost)
    }

    /// The whole badge, as drawn.
    pub fn paint(&self, canvas: &Canvas) -> Result<(), String> {
        self.conn
            .put_image(
                ImageFormat::Z_PIXMAP,
                self.id,
                self.gc,
                canvas.width as u16,
                canvas.height as u16,
                0,
                0,
                0,
                self.depth,
                &canvas.pixels,
            )
            .map_err(lost)?;
        self.conn.flush().map_err(lost)
    }

    /// Every monitor RandR names, or the whole root window on a server too old to
    /// name them.
    fn monitor_frames(&self) -> Result<Vec<Rect>, String> {
        if !self.monitors {
            let root = self
                .conn
                .get_geometry(self.root)
                .map_err(lost)?
                .reply()
                .map_err(lost)?;
            return Ok(vec![Rect {
                x: 0,
                y: 0,
                w: i32::from(root.width),
                h: i32::from(root.height),
            }]);
        }
        let reply = self
            .conn
            .randr_get_monitors(self.root, true)
            .map_err(lost)?
            .reply()
            .map_err(lost)?;
        Ok(reply
            .monitors
            .iter()
            .map(|monitor| Rect {
                x: i32::from(monitor.x),
                y: i32::from(monitor.y),
                w: i32::from(monitor.width),
                h: i32::from(monitor.height),
            })
            .collect())
    }

    /// The current desktop's `_NET_WORKAREA`, or `None` when the window manager
    /// publishes none.
    fn workarea(&self) -> Result<Option<Rect>, String> {
        let desktop = self
            .cardinals(self.atoms.current_desktop, 1)?
            .first()
            .copied()
            .unwrap_or(0);
        let values = self.cardinals(self.atoms.workarea, MAX_WORKAREA_VALUES)?;
        let at = 4 * desktop as usize;
        let Some(quad) = values.get(at..at + 4).or_else(|| values.get(0..4)) else {
            return Ok(None);
        };
        Ok(Some(Rect {
            x: quad[0] as i32,
            y: quad[1] as i32,
            w: quad[2] as i32,
            h: quad[3] as i32,
        }))
    }

    /// A CARDINAL property of the root window, empty when it is not set.
    fn cardinals(&self, atom: u32, count: u32) -> Result<Vec<u32>, String> {
        let reply = self
            .conn
            .get_property(false, self.root, atom, AtomEnum::CARDINAL, 0, count)
            .map_err(lost)?
            .reply()
            .map_err(lost)?;
        Ok(reply.value32().map(Iterator::collect).unwrap_or_default())
    }
}

/// The badge writes 32-bit pixels, blue first: a TrueColor visual with eight bits
/// a channel, least significant byte first. Any other display is refused by name
/// rather than drawn in the wrong colours.
fn pixel_format(setup: &xproto::Setup, screen: &xproto::Screen) -> Result<(), String> {
    if setup.image_byte_order != ImageOrder::LSB_FIRST {
        return Err(
            "the X display orders image bytes most significant first; the badge draws least first"
                .into(),
        );
    }
    let format = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == screen.root_depth);
    if format.map(|format| format.bits_per_pixel) != Some(32) {
        return Err(format!(
            "the X display's depth {} is not 32 bits a pixel",
            screen.root_depth
        ));
    }
    let visual = screen
        .allowed_depths
        .iter()
        .filter(|depth| depth.depth == screen.root_depth)
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual_id == screen.root_visual)
        .ok_or("the X display's root visual is not among its own visuals")?;
    let masks = (visual.red_mask, visual.green_mask, visual.blue_mask);
    if visual.class != VisualClass::TRUE_COLOR || masks != (0xff0000, 0x00ff00, 0x0000ff) {
        return Err(format!(
            "the X display's visual is not 8-bit TrueColor ({:?} {masks:x?})",
            visual.class
        ));
    }
    Ok(())
}

/// Clicks outside the buttons must reach the window under the badge, which only
/// an input region does. A server without one is refused: a badge that swallowed
/// every click on it would be in the person's way.
fn shape_input(conn: &RustConnection) -> Result<(), String> {
    let version = conn
        .shape_query_version()
        .map_err(lost)?
        .reply()
        .map_err(|e| format!("the X display has no SHAPE extension: {e}"))?;
    if (
        u32::from(version.major_version),
        u32::from(version.minor_version),
    ) < SHAPE_INPUT
    {
        return Err(format!(
            "the X display's SHAPE {}.{} has no input regions",
            version.major_version, version.minor_version
        ));
    }
    Ok(())
}

/// Whether RandR names monitors here. Without it the badge places itself on the
/// whole root window, which is one monitor's worth of truth on a one-monitor
/// display and is said nowhere else.
fn names_monitors(conn: &RustConnection) -> Result<bool, String> {
    let cookie = match conn.randr_query_version(RANDR_MONITORS.0, RANDR_MONITORS.1) {
        Ok(cookie) => cookie,
        Err(x11rb::errors::ConnectionError::UnsupportedExtension) => return Ok(false),
        Err(e) => return Err(lost(e)),
    };
    match cookie.reply() {
        Ok(version) => Ok((version.major_version, version.minor_version) >= RANDR_MONITORS),
        Err(x11rb::errors::ReplyError::ConnectionError(e)) => Err(lost(e)),
        Err(_no_randr) => Ok(false),
    }
}

/// Its names and its kind, for anything that lists windows; and the atoms the
/// badge reads off the root.
fn name(conn: &RustConnection, id: u32) -> Result<Atoms, String> {
    let atom = |name: &[u8]| -> Result<u32, String> {
        Ok(conn
            .intern_atom(false, name)
            .map_err(lost)?
            .reply()
            .map_err(lost)?
            .atom)
    };
    let utf8 = atom(b"UTF8_STRING")?;
    let kind = atom(b"_NET_WM_WINDOW_TYPE")?;
    let notification = atom(b"_NET_WM_WINDOW_TYPE_NOTIFICATION")?;
    let pid = std::process::id();

    conn.change_property8(
        PropMode::REPLACE,
        id,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        TITLE.as_bytes(),
    )
    .map_err(lost)?;
    conn.change_property8(
        PropMode::REPLACE,
        id,
        atom(b"_NET_WM_NAME")?,
        utf8,
        TITLE.as_bytes(),
    )
    .map_err(lost)?;
    conn.change_property8(
        PropMode::REPLACE,
        id,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"compux-indicator-x11\0Fermix\0",
    )
    .map_err(lost)?;
    conn.change_property32(PropMode::REPLACE, id, kind, AtomEnum::ATOM, &[notification])
        .map_err(lost)?;
    conn.change_property32(
        PropMode::REPLACE,
        id,
        atom(b"_NET_WM_PID")?,
        AtomEnum::CARDINAL,
        &[pid],
    )
    .map_err(lost)?;

    Ok(Atoms {
        workarea: atom(b"_NET_WORKAREA")?,
        current_desktop: atom(b"_NET_CURRENT_DESKTOP")?,
    })
}

/// Hear when a window is mapped or restacked over the badge, and when the
/// monitors change. Watching the root's children is shared with every other
/// client; it redirects nothing.
fn watch_root(conn: &RustConnection, root: u32, monitors: bool) -> Result<(), String> {
    let aux = xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY);
    conn.change_window_attributes(root, &aux).map_err(lost)?;
    if monitors {
        conn.randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE)
            .map_err(lost)?;
    }
    Ok(())
}

/// A window's outer rectangle from its corner, its size inside its border, and
/// the border.
pub fn outer(x: i16, y: i16, width: u16, height: u16, border: u16) -> Rect {
    let border = i32::from(border);
    Rect {
        x: i32::from(x),
        y: i32::from(y),
        w: i32::from(width) + 2 * border,
        h: i32::from(height) + 2 * border,
    }
}

/// A reply, or `None` where the window it asks about is gone.
fn present<T>(reply: Result<T, ReplyError>) -> Result<Option<T>, String> {
    match reply {
        Ok(reply) => Ok(Some(reply)),
        Err(ReplyError::X11Error(e))
            if matches!(e.error_kind, ErrorKind::Window | ErrorKind::Drawable) =>
        {
            Ok(None)
        }
        Err(e) => Err(lost(e)),
    }
}

fn lost(error: impl std::fmt::Display) -> String {
    format!("the X display refused the badge or went away: {error}")
}
