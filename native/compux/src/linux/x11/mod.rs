//! X11 pixels on x11rb (M86 L2): input through XTEST with the server's own
//! keymap, display capture through the core `GetImage` in strips, the monitors
//! from RandR, the windows from EWMH, and the `CLIPBOARD` owned in-process for
//! `paste`. Pure Rust, no C library, and every connection is made to the display
//! and with the authority the endpoint carries as values (`display::Endpoint`):
//! the process environment is never read for them here, and never written.
//!
//! - `keymap`, `strips`, `selection`, `xauth`, `journal`: the decisions, pure or
//!   file-only, tested without a display;
//! - `input`: the XTEST platform behind `held::Platform`;
//! - `capture`: the monitors and one display's frame;
//! - `windows`: the EWMH window listing, and the window server a bound window
//!   lives by;
//! - `above`: what takes a click over a bound window among the windows no
//!   window manager lists;
//! - `frames`: a bound window's frames, through Composite;
//! - `clipboard`: the selection owner's thread.

mod above;
mod capture;
mod clipboard;
#[cfg(test)]
mod fake_x;
mod frames;
mod input;
pub mod journal;
pub mod keymap;
pub mod selection;
pub mod strips;
mod windows;
pub mod xauth;

#[cfg(not(test))]
pub use capture::capture;
pub use capture::{monitors, Monitor};
pub use frames::Composite;
pub use input::{Desktop, Facts, Refusal, KEY_PACE_MS};
pub use windows::{list as windows, Window, WindowServer};

use x11rb::reexports::x11rb_protocol::parse_display::parse_display;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::x11_utils::X11Error;

use super::display::Env;

/// A connection to the X display `env` names, authorised with the cookie its
/// authority file holds for that display. Neither is read from the environment.
pub fn connect(env: &Env) -> Result<(RustConnection, usize), String> {
    let name = env
        .display
        .as_deref()
        .ok_or("no X display is named for this session")?;
    let parsed = parse_display(Some(name))
        .map_err(|error| format!("the X display {name:?} is not a display name: {error}"))?;
    let screen = usize::from(parsed.screen);
    let mut failed = String::from("no address to try");
    for address in parsed.connect_instruction() {
        let (stream, (family, peer)) = match DefaultStream::connect(&address) {
            Ok(connected) => connected,
            Err(error) => {
                failed = error.to_string();
                continue;
            }
        };
        let (method, cookie) = match &env.xauthority {
            Some(path) => xauth::cookie(path, (family, &peer), parsed.display)?.unwrap_or_default(),
            None => (Vec::new(), Vec::new()),
        };
        let connection =
            RustConnection::connect_to_stream_with_auth_info(stream, screen, method, cookie)
                .map_err(|error| format!("the X display {name} refused compux: {error}"))?;
        return Ok((connection, screen));
    }
    Err(format!("the X display {name} did not answer: {failed}"))
}

/// An X error as one line: the request it answered, and why it was refused.
pub fn refused(error: &X11Error) -> String {
    let request = match (error.request_name, error.extension_name.as_deref()) {
        (Some(name), _) => name.to_string(),
        (None, Some(extension)) => format!("{extension} request {}", error.minor_opcode),
        (None, None) => format!("request {}", error.major_opcode),
    };
    format!(
        "the X display refused {request} ({:?}, value {:#x}, sequence {})",
        error.error_kind, error.bad_value, error.sequence
    )
}
