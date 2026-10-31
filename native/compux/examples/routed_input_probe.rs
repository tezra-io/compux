//! Post ONE event to a process, through the public `CGEventPostToPid`.
//!
//! **This is not part of the helper and the helper never calls it.** It exists for
//! the owner's routed-input study (M42 §7.3): today every action this build
//! dispatches goes to the input tap, so whatever is in front receives it. macOS also
//! offers a routed post — `CGEventPostToPid` — which hands one event to one process
//! rather than to the screen, and whether that is usable for a background target is
//! a question only a real desktop can answer. This probe is what asks it.
//!
//! It is deliberately one event and nothing else: no sequence, no modifiers, no
//! retry. The study is about whether a routed event ARRIVES and where, and a probe
//! that did more would make its own answer harder to read.
//!
//! **No private API.** `CGEventPostToPid` is a documented CoreGraphics symbol.
//! Nothing here reads or writes anything else about the target process.
//!
//! Run it, on a machine the owner is sitting at and nowhere else:
//!
//! ```sh
//! # A left click at a point, in global logical points:
//! cargo run --example routed_input_probe -- <pid> click <x> <y>
//!
//! # A single key, by its ASCII character:
//! cargo run --example routed_input_probe -- <pid> key a
//! ```
//!
//! It needs the Accessibility grant, like every other synthetic event: without it
//! macOS silently drops the post and the probe will report that it sent one and
//! nothing will have happened, which is itself a finding worth writing down.

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: routed_input_probe <pid> click <x> <y> | routed_input_probe <pid> key <c>";

    match arguments.as_slice() {
        [pid, kind, rest @ ..] => match post(parse_pid(pid), kind, rest) {
            Ok(said) => println!("{said}"),
            Err(reason) => fail(&reason),
        },
        _other => fail(usage),
    }
}

fn parse_pid(text: &str) -> i32 {
    text.parse()
        .unwrap_or_else(|_| fail("the pid must be a number"))
}

fn fail(reason: &str) -> ! {
    eprintln!("routed_input_probe: {reason}");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn post(pid: i32, kind: &str, rest: &[String]) -> Result<String, String> {
    match (kind, rest) {
        ("click", [x, y]) => {
            let point = (
                x.parse::<f64>().map_err(|_| "x must be a number")?,
                y.parse::<f64>().map_err(|_| "y must be a number")?,
            );
            mac::click(pid, point.0, point.1)
        }
        ("key", [character]) => {
            let character = character
                .chars()
                .next()
                .ok_or_else(|| "key needs one character".to_string())?;
            mac::key(pid, character)
        }
        _other => Err("unknown probe; expected `click x y` or `key c`".to_string()),
    }
}

/// Linux has no routed post and no CoreGraphics. Saying so is the whole of it: a
/// probe that silently did nothing would be a study with a made-up result.
#[cfg(not(target_os = "macos"))]
fn post(_pid: i32, _kind: &str, _rest: &[String]) -> Result<String, String> {
    Err("routed input is a macOS question: CGEventPostToPid is CoreGraphics".to_string())
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::ptr;

    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    /// From CGEventTypes.h, stable ABI: left mouse down and up, and the button.
    const LEFT_MOUSE_DOWN: u32 = 1;
    const LEFT_MOUSE_UP: u32 = 2;
    const MOUSE_BUTTON_LEFT: u32 = 0;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventCreateMouseEvent(
            source: *const c_void,
            event_type: u32,
            point: CGPoint,
            button: u32,
        ) -> *mut c_void;
        fn CGEventCreateKeyboardEvent(
            source: *const c_void,
            virtual_key: u16,
            key_down: bool,
        ) -> *mut c_void;
        fn CGEventKeyboardSetUnicodeString(event: *mut c_void, length: usize, string: *const u16);
        /// The documented routed post: this event goes to ONE process rather than to
        /// the window server's input tap.
        fn CGEventPostToPid(pid: i32, event: *mut c_void);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    /// One click, down and up, routed to `pid`. Every event is released on every
    /// path, including the one where the second could not be created.
    pub fn click(pid: i32, x: f64, y: f64) -> Result<String, String> {
        for kind in [LEFT_MOUSE_DOWN, LEFT_MOUSE_UP] {
            let event = unsafe {
                CGEventCreateMouseEvent(ptr::null(), kind, CGPoint { x, y }, MOUSE_BUTTON_LEFT)
            };
            if event.is_null() {
                return Err(format!("could not create the event at ({x}, {y})"));
            }
            unsafe {
                CGEventPostToPid(pid, event);
                CFRelease(event);
            }
        }

        Ok(format!("routed one click at ({x}, {y}) to pid {pid}"))
    }

    /// One key, down and up, routed to `pid`. The character rides as a Unicode
    /// string rather than a virtual key code, so no keyboard layout is assumed.
    pub fn key(pid: i32, character: char) -> Result<String, String> {
        let utf16: Vec<u16> = character.to_string().encode_utf16().collect();

        for down in [true, false] {
            let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), 0, down) };
            if event.is_null() {
                return Err(format!("could not create the key event for {character:?}"));
            }
            unsafe {
                CGEventKeyboardSetUnicodeString(event, utf16.len(), utf16.as_ptr());
                CGEventPostToPid(pid, event);
                CFRelease(event);
            }
        }

        Ok(format!("routed one {character:?} to pid {pid}"))
    }
}
