//! What a click at a point of a bound window would reach instead of it, among
//! the windows no window manager lists. Menus, tooltips, notifications and a
//! desktop shell's own windows are override-redirect: they are in no stacking
//! list, and a click where one takes input reaches it, not the window under it.
//!
//! So the root's children are read in the server's own stacking order, and each
//! one above the bound window's top-level that is viewable and takes input at
//! the point is what a click there would reach. A window takes input where its
//! SHAPE input region is, or over all of itself and its border where it has
//! none. `TranslateCoordinates` cannot answer this: it ignores input regions,
//! so it names a shell's full-screen stage over every point. Nothing moves the
//! pointer and nothing is grabbed. A program holding a pointer grab with no
//! window at the point takes the click all the same, and cannot be seen here.

use x11rb::connection::RequestConnection;
use x11rb::cookie::Cookie;
use x11rb::errors::ReplyError;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, GetGeometryReply, GetWindowAttributesReply, MapState,
    Window as XWindow,
};
use x11rb::protocol::ErrorKind;
use x11rb::rust_connection::RustConnection;

use super::windows::Rect;
use crate::linux::display::Env;

/// The most windows read above a bound window's top-level. A desktop with more
/// is refused rather than half read: the ones left out would be the front-most.
pub const MAX_ABOVE: usize = 1_024;

/// The most parents walked from a client to its top-level.
const MAX_FRAME_DEPTH: usize = 8;

/// The longest class or title read from a window that covers the point.
const MAX_NAME_BYTES: u32 = 1_024;

/// The longest name a refusal carries: another program's text reaching a reply
/// the model reads.
const MAX_NAME_CHARS: usize = 80;

/// What a refusal calls a window that names itself nothing.
pub const UNNAMED: &str = "a menu or popup";

/// A window stacked above the bound window's top-level, as one look found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Above {
    pub viewable: bool,
    /// Its outer corner, border included, in root pixels.
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub border: u16,
    /// Where it takes input, in its own pixels from the corner inside its
    /// border; `None` where the server has no SHAPE, which is the whole window
    /// and its border.
    pub input: Option<Vec<Rect>>,
}

/// The root's children above `top`, bottom to top, out of the server's
/// bottom-to-top list of them. `None` where `top` is not one of them.
pub fn stacked_above(bottom_to_top: &[XWindow], top: XWindow) -> Option<&[XWindow]> {
    let at = bottom_to_top.iter().position(|&window| window == top)?;
    Some(&bottom_to_top[at + 1..])
}

/// The front-most of `above` (bottom to top) that a click at root (`x`, `y`)
/// would reach.
pub fn takes_click(above: &[Above], x: i32, y: i32) -> Option<usize> {
    above
        .iter()
        .rposition(|window| window.viewable && takes_input(window, x, y))
}

/// Whether `window` takes input at root (`x`, `y`): somewhere in its input
/// region, which counts from the corner inside its border, or anywhere on it
/// and its border where it has none.
fn takes_input(window: &Above, x: i32, y: i32) -> bool {
    let border = i64::from(window.border);
    let (inside_x, inside_y) = (i64::from(window.x) + border, i64::from(window.y) + border);
    let (px, py) = (i64::from(x) - inside_x, i64::from(y) - inside_y);
    let whole = [Rect {
        x: -i32::from(window.border),
        y: -i32::from(window.border),
        width: u32::from(window.width) + 2 * u32::from(window.border),
        height: u32::from(window.height) + 2 * u32::from(window.border),
    }];
    let region = window.input.as_deref().unwrap_or(&whole);
    region.iter().any(|r| {
        let (left, top) = (i64::from(r.x), i64::from(r.y));
        px >= left && py >= top && px < left + i64::from(r.width) && py < top + i64::from(r.height)
    })
}

/// A covering window by its class, else its title, else as [`UNNAMED`], on one
/// bounded line.
pub fn named(class: &str, title: &str) -> String {
    let name = [class, title]
        .into_iter()
        .map(|text| crate::ax::one_line(text, MAX_NAME_CHARS))
        .find(|text| !text.is_empty());
    name.unwrap_or_else(|| UNNAMED.to_string())
}

/// What takes a click at root (`x`, `y`) above `client`'s top-level, among the
/// windows no window manager lists, named. `Err` when the server will not say,
/// or holds more than [`MAX_ABOVE`] windows above it.
pub fn covering(env: &Env, client: XWindow, x: i32, y: i32) -> Result<Option<String>, String> {
    let (connection, _) = super::connect(env)?;
    let (top, root) = top_level(&connection, client)?;
    let children = connection
        .query_tree(root)
        .map_err(lost)?
        .reply()
        .map_err(lost)?
        .children;
    let ids = stacked_above(&children, top)
        .ok_or("the window is no longer on the screen's root, so what is over it cannot be told")?;
    if ids.len() > MAX_ABOVE {
        return Err(format!(
            "more than {MAX_ABOVE} windows are stacked above it, so what a click would reach \
             cannot be told"
        ));
    }
    let above = read(&connection, ids)?;
    let Some(index) = takes_click(&above, x, y) else {
        return Ok(None);
    };
    name_of(&connection, ids[index]).map(Some)
}

/// The client's top-level (the root's child holding it), and the root.
fn top_level(connection: &RustConnection, client: XWindow) -> Result<(XWindow, XWindow), String> {
    let mut window = client;
    for _ in 0..MAX_FRAME_DEPTH {
        let tree = connection
            .query_tree(window)
            .map_err(lost)?
            .reply()
            .map_err(lost)?;
        if tree.parent == tree.root || tree.parent == 0 {
            return Ok((window, tree.root));
        }
        window = tree.parent;
    }
    Err(format!(
        "the window sits more than {MAX_FRAME_DEPTH} windows below the root, so what is over it \
         cannot be told"
    ))
}

/// Requests for one window above, sent before any is answered.
struct Asked<'c> {
    attributes: Cookie<'c, RustConnection, GetWindowAttributesReply>,
    geometry: Cookie<'c, RustConnection, GetGeometryReply>,
    input: Option<Cookie<'c, RustConnection, shape::GetRectanglesReply>>,
}

/// Every window above, read in one round trip's wait. A window gone meanwhile
/// is read as not viewable.
fn read(connection: &RustConnection, ids: &[XWindow]) -> Result<Vec<Above>, String> {
    let shaped = connection
        .extension_information(shape::X11_EXTENSION_NAME)
        .map_err(lost)?
        .is_some();
    let mut asked = Vec::with_capacity(ids.len());
    for &id in ids {
        asked.push(Asked {
            attributes: connection.get_window_attributes(id).map_err(lost)?,
            geometry: connection.get_geometry(id).map_err(lost)?,
            input: match shaped {
                true => Some(
                    connection
                        .shape_get_rectangles(id, shape::SK::INPUT)
                        .map_err(lost)?,
                ),
                false => None,
            },
        });
    }
    asked.into_iter().map(answer).collect()
}

fn answer(asked: Asked<'_>) -> Result<Above, String> {
    let attributes = present(asked.attributes.reply())?;
    let geometry = present(asked.geometry.reply())?;
    let input = match asked.input {
        Some(cookie) => present(cookie.reply())?.map(|reply| {
            reply
                .rectangles
                .iter()
                .map(|r| Rect {
                    x: i32::from(r.x),
                    y: i32::from(r.y),
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                })
                .collect()
        }),
        None => None,
    };
    let (Some(attributes), Some(geometry)) = (attributes, geometry) else {
        return Ok(gone());
    };
    Ok(Above {
        viewable: attributes.map_state == MapState::VIEWABLE,
        x: i32::from(geometry.x),
        y: i32::from(geometry.y),
        width: geometry.width,
        height: geometry.height,
        border: geometry.border_width,
        input,
    })
}

/// A window destroyed between the listing and its reply takes no click.
fn gone() -> Above {
    Above {
        viewable: false,
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        border: 0,
        input: None,
    }
}

/// A reply, or `None` for a window gone; any other refusal is the answer.
fn present<T>(reply: Result<T, ReplyError>) -> Result<Option<T>, String> {
    match reply {
        Ok(reply) => Ok(Some(reply)),
        Err(ReplyError::X11Error(error))
            if matches!(error.error_kind, ErrorKind::Window | ErrorKind::Drawable) =>
        {
            Ok(None)
        }
        Err(error) => Err(lost(error)),
    }
}

/// The covering window's class and title, as a refusal names it.
fn name_of(connection: &RustConnection, window: XWindow) -> Result<String, String> {
    let words = MAX_NAME_BYTES / 4;
    let class = connection
        .get_property(
            false,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            0,
            words,
        )
        .map_err(lost)?;
    let title = connection
        .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::ANY, 0, words)
        .map_err(lost)?;
    let text = |reply: Option<x11rb::protocol::xproto::GetPropertyReply>| {
        reply
            .map(|reply| String::from_utf8_lossy(&reply.value).into_owned())
            .unwrap_or_default()
    };
    let class = text(present(class.reply())?);
    let title = text(present(title.reply())?);
    // WM_CLASS is "instance\0Class\0": the class names the program.
    Ok(named(class.split('\0').nth(1).unwrap_or_default(), &title))
}

fn lost(error: impl std::fmt::Display) -> String {
    format!("what is over the window could not be read: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn popup(x: i32, y: i32, input: Option<Vec<Rect>>) -> Above {
        Above {
            viewable: true,
            x,
            y,
            width: 120,
            height: 60,
            border: 0,
            input,
        }
    }

    // The server lists the root's children bottom to top: what is above the
    // target's top-level is everything after it, and a top-level the root no
    // longer holds says nothing.
    #[test]
    fn only_the_roots_children_after_the_targets_are_above_it() {
        assert_eq!(stacked_above(&[1, 2, 3, 4], 2), Some(&[3, 4][..]));
        assert_eq!(stacked_above(&[1, 2, 3, 4], 4), Some(&[][..]));
        assert_eq!(stacked_above(&[1, 2, 3, 4], 9), None);
    }

    // A menu over the point takes the click, whatever its input region is
    // where the server has no SHAPE: the whole window.
    #[test]
    fn a_popup_over_the_point_takes_the_click() {
        assert_eq!(takes_click(&[popup(300, 300, None)], 350, 330), Some(0));
        assert_eq!(
            takes_click(
                &[popup(300, 300, Some(vec![rect(0, 0, 120, 60)]))],
                350,
                330
            ),
            Some(0)
        );
        assert_eq!(takes_click(&[popup(300, 300, None)], 420, 330), None);
    }

    // A shell's full-screen stage takes input only where its own windows are:
    // a point its input region leaves out reaches the window under it.
    #[test]
    fn a_window_whose_input_region_leaves_the_point_out_takes_nothing() {
        let stage = Above {
            viewable: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            border: 0,
            input: Some(vec![rect(0, 0, 1920, 32), rect(1500, 40, 400, 120)]),
        };
        assert_eq!(takes_click(std::slice::from_ref(&stage), 700, 500), None);
        assert_eq!(
            takes_click(std::slice::from_ref(&stage), 1600, 100),
            Some(0)
        );
        assert_eq!(takes_click(&[popup(0, 0, Some(Vec::new()))], 10, 10), None);
    }

    #[test]
    fn a_window_not_viewable_takes_nothing() {
        let hidden = Above {
            viewable: false,
            ..popup(300, 300, None)
        };
        assert_eq!(takes_click(&[hidden], 350, 330), None);
    }

    // The input region counts from the corner inside the border; with none, the
    // border takes clicks too.
    #[test]
    fn a_windows_input_counts_from_inside_its_border() {
        let bordered = |input| Above {
            border: 5,
            ..popup(100, 100, input)
        };
        assert_eq!(takes_click(&[bordered(None)], 100, 100), Some(0));
        assert_eq!(takes_click(&[bordered(None)], 229, 169), Some(0));
        assert_eq!(takes_click(&[bordered(None)], 230, 170), None);
        let corner = Some(vec![rect(0, 0, 10, 10)]);
        assert_eq!(takes_click(&[bordered(corner.clone())], 105, 105), Some(0));
        assert_eq!(takes_click(&[bordered(corner.clone())], 104, 104), None);
        assert_eq!(takes_click(&[bordered(corner)], 115, 115), None);
    }

    // Two windows over the point: the front-most, last in stacking order, is
    // what the click reaches and what the refusal names.
    #[test]
    fn the_front_most_window_over_the_point_is_the_one_named() {
        let above = [
            popup(300, 300, None),
            popup(900, 900, None),
            popup(320, 310, None),
        ];
        assert_eq!(takes_click(&above, 350, 330), Some(2));
    }

    #[test]
    fn a_covering_window_is_named_by_its_class_then_its_title() {
        assert_eq!(named("Gnome-shell", "mutter guard"), "Gnome-shell");
        assert_eq!(named("", "Compux Popup"), "Compux Popup");
        assert_eq!(named("", ""), UNNAMED);
        let long = named("", &format!("one\ntwo {}", "x".repeat(300)));
        assert!(!long.contains('\n'), "{long}");
        assert!(long.chars().count() <= MAX_NAME_CHARS, "{long}");
    }
}
