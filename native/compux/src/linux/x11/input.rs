//! Input on X11: XTEST for the pointer and the keys, behind `held::Platform`.
//!
//! - **Keys** are the server's own keycodes, from `GetKeyboardMapping`, read again
//!   whenever the server's `MappingNotify` says the keyboard map changed. compux
//!   never changes the map (`keymap`).
//! - **Held keys and buttons** are written to this process's journal before their
//!   press is posted and forgotten after their release (`journal`), so a helper
//!   killed in between has them released by the next one. An action that would
//!   hold one is refused before its first input where no journal can be written.
//!   A tap, a click and a wheel notch hold nothing past themselves: each goes
//!   whole in one write and is checked once after it, so a helper killed while
//!   it waits has sent all of one or none.
//! - **Text** is typed only once every character is known to be on the layout,
//!   and only while the keyboard is neutral; the caller refuses otherwise before
//!   anything is posted ([`Desktop::ready_to_type`]).
//! - **The clipboard** is the selection owner's (`clipboard`), started on the
//!   first `paste` and kept for the session.

use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::ReplyError;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ConnectionExt as _, Mapping, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::CURRENT_TIME;

use super::clipboard::Clipboard;
use super::journal::{self, Held, Journal};
use super::keymap::{self, KeyboardState, Keymap, Outside, Stroke};
use crate::held::{Axis, Button, Direction, Key, Platform};
use crate::linux::display::Env;
use crate::linux::session;

/// The pause after each typed character, as libxdo's default typing delay that
/// enigo used: some applications drop keys that arrive faster.
pub const KEY_PACE_MS: u64 = 12;

/// The most events read off the input connection before an action: only
/// `MappingNotify` arrives there, and rarely.
const MAX_DRAINED_EVENTS: usize = 256;

/// XKB's name for the core keyboard.
const CORE_KEYBOARD: xkb::DeviceSpec = 0x100;

/// What `probe` reports of the X11 pixel route.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub xtest: bool,
    pub keymap: bool,
}

/// The X11 desktop for one session: its input connection, built on first use,
/// and the clipboard owner, started on the first paste.
pub struct Desktop {
    env: Env,
    input: Option<Input>,
    clipboard: Option<Clipboard>,
}

/// The input connection, the keymap read through it, and the journal.
struct Input {
    connection: RustConnection,
    root: u32,
    keymap: Option<Keymap>,
    journal: Option<Journal>,
}

impl Desktop {
    pub fn new(env: Env) -> Desktop {
        Desktop {
            env,
            input: None,
            clipboard: None,
        }
    }

    /// Release what helpers that died on this display left held, and clear their
    /// journals. Read once, at start.
    #[cfg_attr(test, allow(dead_code))]
    pub fn release_left_behind(&mut self) {
        let (Some(dir), Some(display)) = (self.env.runtime_dir.clone(), self.env.display.clone())
        else {
            return;
        };
        let left = journal::left_behind(&dir, &display, |pid, started| {
            session::started_at(pid) == Some(started)
        });
        if left.is_empty() {
            return;
        }
        let input = match self.input() {
            Ok(input) => input,
            Err(why) => {
                eprintln!("compux: a dead helper left keys held, and {why}: they stay held");
                return;
            }
        };
        for journal in left {
            input.release_journal(&journal);
        }
    }

    /// Whether `text` can be typed now: every character on the layout, the
    /// keyboard in its first group with nothing held and Caps Lock off. `Ok(Err)`
    /// is a refusal and says which; `Err` is the display failing to answer.
    pub fn ready_to_type(&mut self, text: &str) -> Result<Result<(), Refusal>, String> {
        let input = self.input()?;
        if let Err(why) = keymap::neutral(input.state()?) {
            return Ok(Err(Refusal::Keyboard(why)));
        }
        Ok(input
            .keymap()?
            .strokes(text)
            .map(|_| ())
            .map_err(Refusal::Outside))
    }

    /// Where the pointer is on the root, or `None` where the display will not say.
    pub fn pointer(&mut self) -> Option<(i32, i32)> {
        let input = self.input().ok()?;
        let reply = input
            .connection
            .query_pointer(input.root)
            .ok()?
            .reply()
            .ok()?;
        Some((i32::from(reply.root_x), i32::from(reply.root_y)))
    }

    /// XTEST and the keymap, read on a connection of its own and closed again.
    #[cfg_attr(test, allow(dead_code))]
    pub fn facts(env: &Env) -> Option<Facts> {
        let (connection, _) = super::connect(env).ok()?;
        let xtest = connection
            .extension_information(xtest::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some();
        let keymap =
            read_keymap(&connection).is_ok_and(|map| map.keycode(keymap::SHIFT_L).is_some());
        Some(Facts { xtest, keymap })
    }

    fn input(&mut self) -> Result<&mut Input, String> {
        if self.input.is_none() {
            self.input = Some(Input::open(&self.env)?);
        }
        self.input
            .as_mut()
            .ok_or_else(|| "input unavailable".to_string())
    }

    fn clipboard(&mut self) -> Result<&mut Clipboard, String> {
        if self.clipboard.is_none() {
            self.clipboard = Some(Clipboard::start(&self.env)?);
        }
        self.clipboard
            .as_mut()
            .ok_or_else(|| "clipboard unavailable".to_string())
    }
}

/// Why text was refused before anything was typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Outside(Outside),
    Keyboard(String),
}

impl Input {
    /// Release everything a dead helper's journal holds, last pressed first, and
    /// remove the journal once all of it is up; a journal with anything still held
    /// stays for the next start to try again.
    fn release_journal(&mut self, journal: &journal::LeftBehind) {
        let mut stuck = 0;
        for held in journal.held.iter().rev() {
            if let Err(why) = self.release(*held) {
                eprintln!("compux: {held:?}, held by a helper that died, stays held: {why}");
                stuck += 1;
            }
        }
        if stuck > 0 {
            return;
        }
        eprintln!(
            "compux: released {} held by a helper that died",
            journal.held.len()
        );
        if let Err(error) = std::fs::remove_file(&journal.path) {
            eprintln!("compux: the dead helper's journal stays: {error}");
        }
    }

    fn open(env: &Env) -> Result<Input, String> {
        let (connection, screen) =
            super::connect(env).map_err(|why| format!("init input: {why}"))?;
        let root = connection
            .setup()
            .roots
            .get(screen)
            .map(|screen| screen.root)
            .ok_or("init input: the X display has no such screen")?;
        if connection
            .extension_information(xtest::X11_EXTENSION_NAME)
            .map_err(|error| format!("init input: {error}"))?
            .is_none()
        {
            return Err("init input: the X display has no XTEST extension".to_string());
        }
        let xkb = connection
            .xkb_use_extension(1, 0)
            .map_err(|error| format!("init input: {error}"))?
            .reply()
            .map_err(|error| format!("init input: the X display has no XKB: {error}"))?;
        if !xkb.supported {
            return Err("init input: the X display's XKB is too old".to_string());
        }
        let journal = match (&env.runtime_dir, &env.display) {
            (Some(dir), Some(display)) => {
                let pid = std::process::id() as i32;
                session::started_at(pid).map(|started| Journal::new(dir, display, pid, started))
            }
            _ => None,
        };
        Ok(Input {
            connection,
            root,
            keymap: None,
            journal,
        })
    }

    /// The keymap, read again if the server said it changed since it was read.
    fn keymap(&mut self) -> Result<&Keymap, String> {
        // No batch is waiting, so any error read here is only said.
        self.drain(&[])?;
        if self.keymap.is_none() {
            self.keymap = Some(read_keymap(&self.connection)?);
        }
        self.keymap.as_ref().ok_or_else(|| "no keymap".to_string())
    }

    fn state(&self) -> Result<KeyboardState, String> {
        let state = self
            .connection
            .xkb_get_state(CORE_KEYBOARD)
            .map_err(|error| format!("the keyboard state would not read: {error}"))?
            .reply()
            .map_err(|error| format!("the keyboard state would not read: {error}"))?;
        Ok(KeyboardState {
            group: u8::from(state.group),
            held: u16::from(state.base_mods) | u16::from(state.latched_mods),
            locked: u16::from(state.locked_mods),
        })
    }

    /// What the server sent on the input connection since it was last read: a
    /// keyboard `MappingNotify` drops the keymap so it is read again, and an error
    /// for a request of `batch` (by the low 16 bits of its sequence number, all an
    /// X error carries) is that batch's refusal. Any other error is said. More
    /// than [`MAX_DRAINED_EVENTS`] waiting is an error, so no refusal is left
    /// unread behind them.
    fn drain(&mut self, batch: &[u16]) -> Result<Option<String>, String> {
        let mut refused = None;
        for _ in 0..MAX_DRAINED_EVENTS {
            let event = self
                .connection
                .poll_for_event()
                .map_err(|error| format!("the X display went away: {error}"))?;
            match event {
                None => return Ok(refused),
                Some(Event::MappingNotify(notify)) if notify.request == Mapping::KEYBOARD => {
                    self.keymap = None;
                }
                Some(Event::Error(error))
                    if refused.is_none() && batch.contains(&error.sequence) =>
                {
                    refused = Some(super::refused(&error));
                }
                Some(Event::Error(error)) => {
                    eprintln!("compux: input: {}", super::refused(&error));
                }
                Some(_) => {}
            }
        }
        Err(format!(
            "more than {MAX_DRAINED_EVENTS} events waited on the input connection"
        ))
    }

    /// Input that holds nothing past itself (a tap, a click, a wheel notch), sent
    /// whole in one write, then checked with one round trip. Nothing of it is
    /// down while compux waits, so a helper killed then has sent all of it or
    /// none, and it needs no journal. An error the server answered for any of it
    /// is an error here; the rest of it still went, so what it pressed it also
    /// released.
    fn batch(&mut self, events: &[(u8, u8)]) -> Result<(), String> {
        let mut sent = Vec::with_capacity(events.len());
        for &(kind, detail) in events {
            let cookie = self
                .connection
                .xtest_fake_input(kind, detail, CURRENT_TIME, self.root, 0, 0, 0)
                .map_err(|error| error.to_string())?;
            // An X error names only the low 16 bits of the sequence number.
            sent.push(cookie.sequence_number() as u16);
            // Unchecked: an error for it arrives as an event, read below.
            drop(cookie);
        }
        self.flush()?;
        self.connection
            .get_input_focus()
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| format!("the X display did not answer: {error}"))?;
        match self.drain(&sent)? {
            Some(refused) => Err(refused),
            None => Ok(()),
        }
    }

    /// One XTEST event, checked: a round trip says whether the server took it,
    /// so input it refused is an error here, never a success.
    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        self.connection
            .xtest_fake_input(kind, detail, CURRENT_TIME, self.root, x, y, 0)
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| match error {
                ReplyError::X11Error(error) => super::refused(&error),
                other => other.to_string(),
            })
    }

    fn flush(&self) -> Result<(), String> {
        self.connection.flush().map_err(|error| error.to_string())
    }

    /// Write it down, then press it: a press that cannot be written is not posted.
    fn press(&mut self, held: Held) -> Result<(), String> {
        let journal = self
            .journal
            .as_mut()
            .ok_or("there is no runtime directory to journal a held key in, so none is pressed")?;
        journal.record(held)?;
        let pressed = match held {
            Held::Key(code) => self.fake(KEY_PRESS_EVENT, code, 0, 0),
            Held::Button(number) => self.fake(BUTTON_PRESS_EVENT, number, 0, 0),
        };
        let Err(why) = pressed else {
            return self.flush();
        };
        // The server took no press, so nothing is held to release next start.
        if let Some(journal) = self.journal.as_mut() {
            journal.forget(held)?;
        }
        Err(why)
    }

    /// Release it, then forget it.
    fn release(&mut self, held: Held) -> Result<(), String> {
        match held {
            Held::Key(code) => self.fake(KEY_RELEASE_EVENT, code, 0, 0)?,
            Held::Button(number) => self.fake(BUTTON_RELEASE_EVENT, number, 0, 0)?,
        }
        self.flush()?;
        if let Some(journal) = self.journal.as_mut() {
            journal.forget(held)?;
        }
        Ok(())
    }

    /// One character's key, with its level keys around it, in one batch.
    fn tap(&mut self, stroke: Stroke) -> Result<(), String> {
        let levels: Vec<u8> = [stroke.shift, stroke.level3]
            .into_iter()
            .flatten()
            .collect();
        let mut events: Vec<(u8, u8)> =
            levels.iter().map(|code| (KEY_PRESS_EVENT, *code)).collect();
        events.push((KEY_PRESS_EVENT, stroke.keycode));
        events.push((KEY_RELEASE_EVENT, stroke.keycode));
        events.extend(levels.iter().rev().map(|code| (KEY_RELEASE_EVENT, *code)));
        self.batch(&events)
    }

    fn stroke(&mut self, key: Key) -> Result<Stroke, String> {
        let keymap = self.keymap()?;
        let found = match key {
            Key::Unicode(c) => keymap
                .strokes(&c.to_string())
                .ok()
                .and_then(|s| s.first().copied()),
            named => keymap.stroke_for(keysym(named)),
        };
        found.ok_or_else(|| format!("the keyboard layout has no key for {key:?}"))
    }
}

/// The core keyboard mapping, read now.
fn read_keymap(connection: &RustConnection) -> Result<Keymap, String> {
    let setup = connection.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let count = max.saturating_sub(min).saturating_add(1);
    let reply = connection
        .get_keyboard_mapping(min, count)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| format!("the keyboard map would not read: {error}"))?;
    Ok(Keymap::new(min, reply.keysyms_per_keycode, reply.keysyms))
}

/// The keysym of a named key; `Unicode` is looked up by its character instead.
fn keysym(key: Key) -> u32 {
    match key {
        Key::Unicode(c) => u32::from(c),
        Key::Return => keymap::RETURN,
        Key::Tab => keymap::TAB,
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
        Key::Meta => keymap::SUPER_L,
        Key::Control => keymap::CONTROL_L,
        Key::Alt => keymap::ALT_L,
        Key::Shift => keymap::SHIFT_L,
    }
}

/// X's button numbers: 1 left, 3 right; the wheel is 4 to 7.
fn button_number(button: Button) -> u8 {
    match button {
        Button::Left => 1,
        Button::Right => 3,
    }
}

/// The wheel button for one notch in a direction: up 4, down 5, left 6, right 7.
fn wheel(length: i32, axis: Axis) -> u8 {
    match (axis, length < 0) {
        (Axis::Vertical, true) => 4,
        (Axis::Vertical, false) => 5,
        (Axis::Horizontal, true) => 6,
        (Axis::Horizontal, false) => 7,
    }
}

/// A root coordinate, which X carries in 16 bits.
fn coordinate(value: i32) -> Result<i16, String> {
    i16::try_from(value).map_err(|_| format!("{value} is outside the X screen"))
}

impl Platform for Desktop {
    fn key(&mut self, key: Key, direction: Direction) -> Result<(), String> {
        let input = self.input()?;
        let stroke = input.stroke(key).map_err(|why| match direction {
            Direction::Press => format!("modifier: {why}"),
            Direction::Click => format!("key: {why}"),
            Direction::Release => why,
        })?;
        match direction {
            Direction::Press => input.press(Held::Key(stroke.keycode)),
            Direction::Release => input.release(Held::Key(stroke.keycode)),
            Direction::Click => input.tap(stroke).map_err(|why| format!("key: {why}")),
        }
    }

    fn button(&mut self, button: Button, direction: Direction) -> Result<(), String> {
        let input = self.input()?;
        let number = button_number(button);
        match direction {
            Direction::Press => input
                .press(Held::Button(number))
                .map_err(|e| format!("press: {e}")),
            Direction::Release => input.release(Held::Button(number)),
            Direction::Click => input
                .batch(&[(BUTTON_PRESS_EVENT, number), (BUTTON_RELEASE_EVENT, number)])
                .map_err(|e| format!("click: {e}")),
        }
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<(), String> {
        let input = self.input()?;
        input
            .fake(MOTION_NOTIFY_EVENT, 0, coordinate(x)?, coordinate(y)?)
            .and_then(|()| input.flush())
            .map_err(|e| format!("move: {e}"))
    }

    /// XTEST motion is applied in order, so one round trip proves it landed; the
    /// pointer must then read where it was sent. One that did not get there (a
    /// grab, a barrier) is a click that must not be posted.
    fn settle(&mut self, x: i32, y: i32) -> Result<(), String> {
        let input = self.input()?;
        let reply = input
            .connection
            .query_pointer(input.root)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| format!("settle: {e}"))?;
        let at = (i32::from(reply.root_x), i32::from(reply.root_y));
        if at == (x, y) {
            return Ok(());
        }
        Err(format!(
            "the pointer did not reach ({x},{y}): it is at ({},{})",
            at.0, at.1
        ))
    }

    /// X11 motion while a button is held IS the drag; there is no other event.
    fn drag_step(&mut self, x: i32, y: i32) -> Result<(), String> {
        let input = self.input()?;
        input
            .fake(MOTION_NOTIFY_EVENT, 0, coordinate(x)?, coordinate(y)?)
            .and_then(|()| input.flush())
            .map_err(|e| format!("drag: {e}"))
    }

    /// One press and release of a wheel button per notch, at most
    /// [`MAX_SCROLL_NOTCHES`](crate::wire::MAX_SCROLL_NOTCHES), which the request
    /// was held to before it got here.
    fn scroll(&mut self, length: i32, axis: Axis) -> Result<(), String> {
        let notches = length.unsigned_abs();
        if notches > crate::wire::MAX_SCROLL_NOTCHES.unsigned_abs() {
            return Err(format!("scroll: {notches} notches is past the cap"));
        }
        let input = self.input()?;
        let number = wheel(length, axis);
        for _ in 0..notches {
            input
                .batch(&[(BUTTON_PRESS_EVENT, number), (BUTTON_RELEASE_EVENT, number)])
                .map_err(|e| format!("scroll: {e}"))?;
        }
        Ok(())
    }

    /// Every character is resolved before the first is typed, so text the layout
    /// cannot type sends nothing at all.
    fn text(&mut self, text: &str) -> Result<(), String> {
        let input = self.input()?;
        let strokes = input
            .keymap()?
            .strokes(text)
            .map_err(|outside| format!("type: {}", outside.describe()))?;
        for stroke in strokes {
            input.tap(stroke).map_err(|e| format!("type: {e}"))?;
            std::thread::sleep(std::time::Duration::from_millis(KEY_PACE_MS));
        }
        Ok(())
    }

    fn sleep(&mut self, ms: u64) -> Result<(), String> {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(())
    }

    fn clipboard_text(&mut self) -> Result<Option<String>, String> {
        self.clipboard()?.text()
    }

    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String> {
        self.clipboard()?.own(text)
    }

    /// A press goes only where its journal can be written, and this says so
    /// before the action's first input rather than at the press: without
    /// `$XDG_RUNTIME_DIR` there is no journal at all.
    fn ready_to_hold(&mut self) -> Result<(), String> {
        let Some(dir) = self.env.runtime_dir.clone() else {
            return Err(
                "there is no $XDG_RUNTIME_DIR to journal a held key or button in, so \
                        nothing that holds one is sent"
                    .to_string(),
            );
        };
        let journal = self.input()?.journal.as_ref().ok_or_else(|| {
            format!(
                "no held-input journal can be kept in {}: this process's start time would not \
                 read, so nothing that holds a key or button is sent",
                dir.display()
            )
        })?;
        journal
            .ready()
            .map_err(|why| format!("{why}, so nothing that holds a key or button is sent"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake_x::{self, FakeX};
    use super::*;

    fn over(connection: RustConnection, journal: Option<Journal>) -> Input {
        Input {
            connection,
            root: fake_x::ROOT,
            keymap: None,
            journal,
        }
    }

    // A key or button the X server refused is an error the action sees, never a
    // success: each XTEST request is checked.
    #[test]
    fn input_the_x_server_refuses_is_an_error_not_a_success() {
        let (_server, connection) = FakeX::start(fake_x::BAD_VALUE);
        let mut input = over(connection, None);

        let released = input.release(Held::Button(1));
        let tapped = input.tap(Stroke {
            keycode: 38,
            shift: Some(50),
            level3: None,
        });
        let clicked = input.fake(BUTTON_PRESS_EVENT, 1, 0, 0);

        let released = released.expect_err("the server refused the release");
        assert!(released.contains("refused"), "{released}");
        assert!(tapped.is_err(), "{tapped:?}");
        assert!(clicked.is_err(), "{clicked:?}");
    }

    // A tap, a click and a wheel notch hold nothing past themselves, so they keep
    // no journal: each must reach the server whole before compux waits on it. A
    // helper killed while it waits then has sent all of one or none of it, and
    // never leaves a key, Shift or a button down that no journal names.
    #[test]
    fn a_tap_a_click_and_a_notch_each_go_whole_before_any_reply_is_awaited() {
        let (server, connection) = FakeX::accepting();
        let mut desktop = Desktop {
            env: Env::default(),
            input: Some(over(connection, None)),
            clipboard: None,
        };
        let stroke = Stroke {
            keycode: 38,
            shift: Some(50),
            level3: Some(92),
        };

        let input = desktop.input.as_mut().expect("the input connection");
        input.tap(stroke).expect("tapped");
        desktop
            .button(Button::Left, Direction::Click)
            .expect("clicked");
        desktop.scroll(2, Axis::Vertical).expect("scrolled");

        assert_eq!(server.syncs(), vec![6, 2, 2, 2]);
    }

    // The wire holds a scroll to its cap; the loop here holds itself to it too,
    // and refuses past it before a notch is sent.
    #[test]
    fn a_scroll_past_the_notch_cap_is_refused_here_too() {
        let (_server, connection) = FakeX::start(fake_x::BAD_VALUE);
        let mut desktop = Desktop {
            env: Env::default(),
            input: Some(over(connection, None)),
            clipboard: None,
        };

        let refused = desktop.scroll(crate::wire::MAX_SCROLL_NOTCHES + 1, Axis::Vertical);

        let refused = refused.expect_err("past the cap");
        assert!(refused.contains("past the cap"), "{refused}");
    }

    // A press the server refused holds nothing, so the journal does not say it
    // does: the next start would release a key nobody pressed.
    #[test]
    fn a_press_the_x_server_refuses_is_not_left_in_the_journal() {
        let dir = std::env::temp_dir().join(format!("compux-refused-{}", std::process::id()));
        let (_server, connection) = FakeX::start(fake_x::BAD_VALUE);
        let mut input = over(connection, Some(Journal::new(&dir, ":7", 4242, 99)));

        let pressed = input.press(Held::Key(50));

        assert!(pressed.is_err(), "{pressed:?}");
        assert!(
            !dir.join(journal::DIR).join("4242").exists(),
            "the journal holds a key the server refused"
        );
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("cleaned up");
        }
    }

    // A drag or a chord with modifiers asks before its first input: with no
    // runtime directory there is no journal, and the refusal names it before
    // the X display is even opened.
    #[test]
    fn nothing_can_be_held_without_a_runtime_directory_and_the_refusal_names_it() {
        let mut desktop = Desktop::new(Env::default());

        let refused = desktop.ready_to_hold().expect_err("no journal");

        assert!(refused.contains("$XDG_RUNTIME_DIR"), "{refused}");
        assert!(desktop.input.is_none(), "the display was opened");
    }

    // A runtime directory the journal cannot be written in refuses too, and the
    // refusal names the directory.
    #[test]
    fn nothing_can_be_held_where_the_journal_cannot_be_written() {
        let dir = std::env::temp_dir().join(format!("compux-unheld-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("elsewhere")).expect("a directory");
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join(journal::DIR))
            .expect("a planted link");
        let (_server, connection) = FakeX::start(fake_x::BAD_VALUE);
        let env = Env {
            runtime_dir: Some(dir.clone()),
            ..Env::default()
        };
        let journal = Journal::new(&dir, ":7", 4242, 99);
        let mut desktop = Desktop {
            env,
            input: Some(over(connection, Some(journal))),
            clipboard: None,
        };

        let refused = desktop.ready_to_hold().expect_err("not writable");

        let named = dir.join(journal::DIR).display().to_string();
        assert!(refused.contains(&named), "{refused}");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    #[test]
    fn the_wheel_is_buttons_four_to_seven() {
        assert_eq!(wheel(-3, Axis::Vertical), 4);
        assert_eq!(wheel(3, Axis::Vertical), 5);
        assert_eq!(wheel(-1, Axis::Horizontal), 6);
        assert_eq!(wheel(1, Axis::Horizontal), 7);
    }

    #[test]
    fn a_point_outside_sixteen_bits_is_refused_not_wrapped() {
        assert_eq!(coordinate(1279), Ok(1279));
        assert!(coordinate(40_000).is_err());
        assert!(coordinate(-40_000).is_err());
    }

    #[test]
    fn the_modifiers_are_the_left_hand_keys_and_meta_is_super() {
        assert_eq!(keysym(Key::Meta), keymap::SUPER_L);
        assert_eq!(keysym(Key::Control), keymap::CONTROL_L);
        assert_eq!(button_number(Button::Right), 3);
    }
}
