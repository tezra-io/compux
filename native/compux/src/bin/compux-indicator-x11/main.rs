//! compux-indicator-x11: the ownership badge on an X11 desktop.
//!
//! The X11 counterpart of the macOS `compux-indicator`, spawned by the helper
//! beside it when a window is bound and stopped when it is released. It reads the
//! helper's state lines on stdin and writes four event lines on stdout, the same
//! wire as the macOS badge; diagnostics go to stderr. It shows the person that
//! Fermix is working in their window, and its Pause and Stop reach the helper's
//! gate through that pipe, with no daemon in the path.
//!
//! ```text
//!   compux-indicator-x11            read state on stdin, write events on stdout
//!   compux-indicator-x11 --license  print the licence of the font it draws with
//! ```
//!
//! It holds no secrets, reads nothing from the screen, opens no file, never takes
//! the keyboard focus and changes no desktop setting: it draws one window and
//! reports three button presses. It exits when its input closes, and its window
//! goes with its connection.

#[cfg(target_os = "linux")]
#[path = "../../badge_area.rs"]
mod badge_area;
#[cfg(target_os = "linux")]
mod draw;
#[cfg(target_os = "linux")]
mod label;
#[cfg(target_os = "linux")]
mod layout;
#[cfg(target_os = "linux")]
mod place;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod wire;
#[cfg(target_os = "linux")]
mod x11;

use std::process::ExitCode;

const USAGE: &str = "usage: compux-indicator-x11 [--license]";

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => run::badge(),
        ["--license"] => {
            print!("{}", draw::FONT_LICENSE);
            ExitCode::SUCCESS
        }
        ["--help" | "-h"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _other => {
            eprintln!("compux-indicator-x11: unknown arguments\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Built everywhere so one `cargo build` stays one command, and drawing nowhere
/// but on X11: the macOS badge is `compux-indicator`, in the app bundle.
#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("compux-indicator-x11: this badge draws on an X11 desktop only\n{USAGE}");
    ExitCode::from(2)
}
