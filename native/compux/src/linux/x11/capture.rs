//! The monitors, from RandR, and one monitor's frame, from the root window with
//! the core `GetImage` in strips (`strips`). Like the xcap grab it replaces: what
//! the root window holds, which under a compositor is only as honest as the
//! compositor makes it: the live check on a GPU-composited desktop says whether
//! it is.
//!
//! X has no logical coordinates. A monitor is reported in root pixels with a
//! scale of 1, which is also what XTEST takes, so an image's pixel and the
//! pointer's are the same unit on X11 whatever `Xft.dpi` says.

// Reached only through a live X server: the Xvfb stage drives it, and a unit
// test never connects to a display.
#![cfg_attr(test, allow(dead_code))]

use image::RgbaImage;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

use super::strips::{self, Format};
use crate::linux::display::Env;

/// One monitor, as RandR names and places it on the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Monitor {
    /// Its first output's id, which stays the same while the monitor is attached;
    /// the monitor's name atom where it lists no output.
    pub id: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// The active monitors, in the order the server lists them, on a connection of
/// its own.
pub fn monitors(env: &Env) -> Result<Vec<Monitor>, String> {
    let (connection, screen) =
        super::connect(env).map_err(|why| format!("enumerate displays: {why}"))?;
    let (root, _) = root_of(&connection, screen)?;
    listed(&connection, root)
}

/// One monitor's frame, found again by its id: a monitor that went away is
/// `display_disconnected`, never whatever took its place.
pub fn capture(env: &Env, id: u32) -> Result<RgbaImage, String> {
    let (connection, screen) = super::connect(env).map_err(|why| format!("capture: {why}"))?;
    let (root, depth) = root_of(&connection, screen)?;
    let monitor = listed(&connection, root)?
        .into_iter()
        .find(|monitor| monitor.id == id)
        .ok_or_else(|| "display_disconnected".to_string())?;
    let format = pixel_format(&connection, depth)?;
    grab(&connection, root, format, &monitor)
}

/// The screen's root window and its depth.
fn root_of(connection: &RustConnection, screen: usize) -> Result<(u32, u8), String> {
    connection
        .setup()
        .roots
        .get(screen)
        .map(|screen| (screen.root, screen.root_depth))
        .ok_or_else(|| "the X display has no such screen".to_string())
}

fn listed(connection: &RustConnection, root: u32) -> Result<Vec<Monitor>, String> {
    let reply = connection
        .randr_get_monitors(root, true)
        .map_err(|error| format!("enumerate displays: {error}"))?
        .reply()
        .map_err(|error| format!("enumerate displays: {error}"))?;
    Ok(reply
        .monitors
        .iter()
        .map(|info| Monitor {
            id: info.outputs.first().copied().unwrap_or(info.name),
            x: info.x,
            y: info.y,
            width: info.width,
            height: info.height,
        })
        .collect())
}

/// The root's pixels under one monitor, strip by strip.
fn grab(
    connection: &RustConnection,
    root: u32,
    format: Format,
    monitor: &Monitor,
) -> Result<RgbaImage, String> {
    let rows = strips::strip_rows(
        monitor.width,
        monitor.height,
        connection.maximum_request_bytes(),
    );
    let (width, height) = (usize::from(monitor.width), usize::from(monitor.height));
    let mut rgba = Vec::with_capacity(width * height * 4);
    for (top, count) in strips::strips(monitor.height, rows) {
        let y = i16::try_from(i32::from(monitor.y) + i32::from(top))
            .map_err(|_| "capture: the monitor reaches past the X screen".to_string())?;
        let reply = connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                root,
                monitor.x,
                y,
                monitor.width,
                count,
                u32::MAX,
            )
            .map_err(|error| format!("capture: {error}"))?
            .reply()
            .map_err(|error| format!("capture: {error}"))?;
        strips::to_rgba(&reply.data, width, usize::from(count), format, &mut rgba)?;
    }
    RgbaImage::from_raw(u32::from(monitor.width), u32::from(monitor.height), rgba)
        .ok_or_else(|| "capture: the frame did not add up to the monitor".to_string())
}

/// How the root's depth stores a pixel.
fn pixel_format(connection: &RustConnection, depth: u8) -> Result<Format, String> {
    let setup = connection.setup();
    let bits_per_pixel = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == depth)
        .map(|format| format.bits_per_pixel)
        .ok_or_else(|| format!("capture: the X display has no format for depth {depth}"))?;
    Ok(Format {
        bits_per_pixel,
        msb_first: setup.image_byte_order == ImageOrder::MSB_FIRST,
    })
}
