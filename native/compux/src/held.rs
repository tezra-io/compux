//! Held synthetic input — recorded BEFORE it is posted, released on every exit path.
//!
//! An input sequence that returned early used to leave the press behind on the
//! user's real desktop: anything between `drag`'s press and release left the LEFT
//! BUTTON held, and a failure around `paste`'s keystroke left the user's CLIPBOARD
//! overwritten. Neither was tracked by anything, so nothing could put it back.
//!
//! Held modifier KEYS were a narrower miss than they look, and the difference is
//! worth knowing before anyone "simplifies" this away: enigo's own `Enigo::drop`
//! releases the keys IT recorded, on both backends, because `Settings` defaults
//! `release_keys_when_dropped` to true and we never set it. That cover is not ours
//! to rely on — it is a default we do not state, it tracks keys and never buttons,
//! it cannot know about the clipboard, and when a release fails it goes to a `log`
//! macro this binary never initialises, so the caller is told nothing.
//!
//! [`Guard`] makes all of it ours and explicit. It owns the press stack, records a
//! key or button before the down event is posted, and releases in reverse order
//! from [`Guard::release_all`] — which [`guarded`] calls on the ordinary and the
//! `?` path, and `Drop` calls if a panic unwinds past it. A release that itself
//! fails is named on stderr and folded into the returned error, never swallowed.
//! Releasing through the same `Enigo` also clears its own held list, so a key is
//! never lifted twice.
//!
//! It releases ONLY what this process recorded as held. A global all-keys reset
//! would change the state of the human's own keyboard, which is never ours to do.
//!
//! Every OS call goes through [`Platform`], so the sequences above it are unit
//! tested with no OS call at all — the same injected-sink seam `capture::Emitter`'s
//! buffer already uses for the event wire.

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse};

/// Dwell before putting the user's clipboard back, so the restore does not race the
/// target application's read of the paste we just triggered.
pub const CLIPBOARD_RESTORE_DWELL_MS: u64 = 80;

/// The OS boundary the held-input sequences post through.
///
/// Narrow on purpose: exactly the calls `click`, `drag`, `paste` and `key` make.
/// Screenshots, geometry and the accessibility tree stay where they are — this is
/// the seam the release-on-every-path invariant needs, not a sidecar refactor.
pub trait Platform {
    /// Post a key event. `Press` and `Release` are only ever issued through
    /// [`Guard`], so no down event can escape the registry.
    fn key(&mut self, key: Key, direction: Direction) -> Result<(), String>;

    /// Post a mouse-button event.
    fn button(&mut self, button: Button, direction: Direction) -> Result<(), String>;

    /// Warp the pointer to an absolute logical point.
    fn move_mouse(&mut self, x: i32, y: i32) -> Result<(), String>;

    /// Wait for the pointer to actually be at `(x, y)` before the next event.
    fn settle(&mut self, x: i32, y: i32) -> Result<(), String>;

    /// One intermediate point of a drag, as the platform's drag motion.
    fn drag_step(&mut self, x: i32, y: i32) -> Result<(), String>;

    /// Post a scroll. One call with a repeat count inside it, not a loop of ours.
    fn scroll(&mut self, length: i32, axis: Axis) -> Result<(), String>;

    /// Enter a string. One call with no loop of ours, so it has no checkpoint.
    fn text(&mut self, text: &str) -> Result<(), String>;

    /// Block this thread. Injected so pacing is asserted, not waited on, in tests
    /// — and fallible, because this is where a cancelled sequence finds out.
    fn sleep(&mut self, ms: u64) -> Result<(), String>;

    /// The clipboard's current text. `Ok(None)` is "nothing text-shaped to save";
    /// `Err` is "the clipboard itself is unavailable".
    fn clipboard_text(&mut self) -> Result<Option<String>, String>;

    /// Replace the clipboard's text.
    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String>;
}

/// The real desktop: enigo for key/button/motion, the `pointer` FFI for the warp
/// and the drag event, arboard for the clipboard.
///
/// Both handles are built on first use, so an `init input:` or a `clipboard:`
/// failure still surfaces at exactly the point in the sequence it did before.
#[derive(Default)]
pub struct Real {
    input: Option<Enigo>,
    clipboard: Option<arboard::Clipboard>,
}

impl Real {
    fn input(&mut self) -> Result<&mut Enigo, String> {
        if self.input.is_none() {
            self.input = Some(crate::enigo()?);
        }
        // Unreachable: the line above either filled it or returned the error.
        self.input
            .as_mut()
            .ok_or_else(|| "input unavailable".to_string())
    }

    fn clipboard(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.clipboard.is_none() {
            self.clipboard =
                Some(arboard::Clipboard::new().map_err(|e| format!("clipboard: {e}"))?);
        }
        // Unreachable: the line above either filled it or returned the error.
        self.clipboard
            .as_mut()
            .ok_or_else(|| "clipboard unavailable".to_string())
    }
}

impl Platform for Real {
    /// The wording each direction had at its old call site, so what reaches Fermix
    /// as `error` is unchanged: a press is only ever a modifier here, a click is the
    /// chord's own key, and a release is worded by [`Guard::release_all`], which
    /// names the item it could not lift.
    fn key(&mut self, key: Key, direction: Direction) -> Result<(), String> {
        self.input()?
            .key(key, direction)
            .map_err(|e| match direction {
                Direction::Press => format!("modifier: {e}"),
                Direction::Click => format!("key: {e}"),
                Direction::Release => e.to_string(),
            })
    }

    fn button(&mut self, button: Button, direction: Direction) -> Result<(), String> {
        self.input()?
            .button(button, direction)
            .map_err(|e| match direction {
                Direction::Press => format!("press: {e}"),
                Direction::Click => format!("click: {e}"),
                Direction::Release => e.to_string(),
            })
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.input()?
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| format!("move: {e}"))
    }

    fn settle(&mut self, x: i32, y: i32) -> Result<(), String> {
        crate::pointer::settle(x, y)
    }

    /// macOS posts each step as an EXPLICIT `LeftMouseDragged` (enigo's `move_mouse`
    /// derives its event type from a live `pressedMouseButtons()` read that races
    /// the just-posted mouse-down and then emits `MouseMoved` — a hover mid-press).
    #[cfg(target_os = "macos")]
    fn drag_step(&mut self, x: i32, y: i32) -> Result<(), String> {
        crate::pointer::drag_step(x, y)
    }

    /// X11 motion while the button is pressed IS the drag — no distinct event type —
    /// so enigo's own motion injection is correct here.
    #[cfg(not(target_os = "macos"))]
    fn drag_step(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.input()?
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| format!("drag: {e}"))
    }

    fn scroll(&mut self, length: i32, axis: Axis) -> Result<(), String> {
        self.input()?
            .scroll(length, axis)
            .map_err(|e| format!("scroll: {e}"))
    }

    fn text(&mut self, text: &str) -> Result<(), String> {
        self.input()?.text(text).map_err(|e| format!("type: {e}"))
    }

    fn sleep(&mut self, ms: u64) -> Result<(), String> {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(())
    }

    fn clipboard_text(&mut self) -> Result<Option<String>, String> {
        // A non-text clipboard (image, files) reads as an error and cannot be
        // preserved here; that is `None`, not a failed action.
        Ok(self.clipboard()?.get_text().ok())
    }

    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String> {
        self.clipboard()?
            .set_text(text)
            .map_err(|e| format!("clipboard set: {e}"))
    }
}

/// One thing this process is holding down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Key(Key),
    Button(Button),
}

/// One entry of the press stack.
struct Down {
    item: Item,
    /// Whether the down event was ACCEPTED. An item is recorded before it is posted,
    /// so a press the platform refused sits here as `false`: it is still released
    /// (the event may have gone out before the call failed), but a release that
    /// fails for it is not folded into the caller's error, because its press failure
    /// already is. Without that, a machine with no Accessibility grant answered every
    /// action with the init failure AND "Key(Meta) was not released" — a stuck key
    /// where nothing had ever gone down.
    posted: bool,
}

/// The press stack for one input sequence.
///
/// Construct it through [`guarded`], which folds a failed release into the
/// sequence's own error. Holding it directly gets the `Drop` release but drops the
/// report on the floor.
pub struct Guard<'a, P: Platform> {
    platform: &'a mut P,
    down: Vec<Down>,
    /// The text to put back, set only once this process has actually overwritten
    /// the clipboard. `None` covers both "we never touched it" and "there was no
    /// text to save" — neither restores anything, which is what `paste` did before.
    clipboard_restore: Option<String>,
}

impl<'a, P: Platform> Guard<'a, P> {
    pub fn new(platform: &'a mut P) -> Guard<'a, P> {
        Guard {
            platform,
            down: Vec::new(),
            clipboard_restore: None,
        }
    }

    /// Press each key in order. A failure part way through the list still releases
    /// the earlier keys — the hole in the `hold/3` loop this replaces.
    pub fn press_keys(&mut self, keys: &[Key]) -> Result<(), String> {
        for key in keys {
            self.press(Item::Key(*key))?;
        }
        Ok(())
    }

    /// Press a mouse button.
    pub fn press_button(&mut self, button: Button) -> Result<(), String> {
        self.press(Item::Button(button))
    }

    /// Record the item, THEN post its down event. That order is the whole point: a
    /// call that fails after the event reached the window server must still leave
    /// something for [`Guard::release_all`] to lift.
    fn press(&mut self, item: Item) -> Result<(), String> {
        self.down.push(Down {
            item,
            posted: false,
        });

        match item {
            Item::Key(key) => self.platform.key(key, Direction::Press),
            Item::Button(button) => self.platform.button(button, Direction::Press),
        }?;

        // Reached only when the platform accepted the down event.
        if let Some(last) = self.down.last_mut() {
            last.posted = true;
        }
        Ok(())
    }

    /// A press-and-release in one event: nothing is left held, so nothing is recorded.
    pub fn click_key(&mut self, key: Key) -> Result<(), String> {
        self.platform.key(key, Direction::Click)
    }

    /// A press-and-release in one event: nothing is left held, so nothing is recorded.
    pub fn click_button(&mut self, button: Button) -> Result<(), String> {
        self.platform.button(button, Direction::Click)
    }

    pub fn move_mouse(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.platform.move_mouse(x, y)
    }

    pub fn settle(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.platform.settle(x, y)
    }

    pub fn drag_step(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.platform.drag_step(x, y)
    }

    pub fn sleep(&mut self, ms: u64) -> Result<(), String> {
        self.platform.sleep(ms)
    }

    /// Put `text` on the clipboard and take responsibility for putting the user's
    /// own text back. Read, write and register are one call so the write can never
    /// happen without the restore being armed.
    pub fn take_clipboard(&mut self, text: &str) -> Result<(), String> {
        let prior = self.platform.clipboard_text()?;
        self.platform.set_clipboard_text(text)?;
        self.clipboard_restore = prior;
        Ok(())
    }

    /// Release everything this process is holding, newest first, then put the
    /// clipboard back. Idempotent: what is released is forgotten, so the `Drop`
    /// after an explicit call has nothing left to do.
    ///
    /// Every failure is named on stderr, and one for an item whose press the platform
    /// ACCEPTED is returned as well, because that is a keyboard the human now has to
    /// fix by hand. The loop never stops early: one stuck key must not strand the rest.
    pub fn release_all(&mut self) -> Result<(), String> {
        let mut failures: Vec<String> = Vec::new();

        while let Some(held) = self.down.pop() {
            let item = held.item;
            let released = match item {
                Item::Key(key) => self.platform.key(key, Direction::Release),
                Item::Button(button) => self.platform.button(button, Direction::Release),
            };
            if let Err(reason) = released {
                eprintln!("compux: {item:?} was NOT released: {reason}");
                if held.posted {
                    failures.push(format!("{item:?} was not released: {reason}"));
                }
            }
        }

        if let Some(prior) = self.clipboard_restore.take() {
            // Cleanup, so the dwell is best-effort: a cancelled sequence skips the
            // wait and puts the clipboard back at once, which is what we want.
            let _ = self.platform.sleep(CLIPBOARD_RESTORE_DWELL_MS);
            if let Err(reason) = self.platform.set_clipboard_text(&prior) {
                // Reported, never returned: the action itself landed, and failing it
                // here would have the caller paste the text a second time.
                eprintln!("compux: the clipboard was NOT restored: {reason}");
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

impl<P: Platform> Drop for Guard<'_, P> {
    fn drop(&mut self) {
        // The unwind backstop. `guarded` releases on the ordinary and the `?` path,
        // where the failure still reaches the caller; here there is no caller left,
        // so the stderr line `release_all` already wrote is the whole report.
        let _ = self.release_all();
    }
}

/// Run one input sequence under a [`Guard`] and answer with both halves of the
/// truth: the sequence's own result, and whether everything it held came back up.
pub fn guarded<P: Platform, T>(
    platform: &mut P,
    body: impl FnOnce(&mut Guard<'_, P>) -> Result<T, String>,
) -> Result<T, String> {
    let mut guard = Guard::new(platform);
    let outcome = body(&mut guard);
    let released = guard.release_all();

    match (outcome, released) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(failed), Ok(())) => Err(failed),
        // The sequence "worked" but the desktop is left holding a key: that is a
        // failed action, because the caller has to know before it acts again.
        (Ok(_), Err(stuck)) => Err(stuck),
        (Err(failed), Err(stuck)) => Err(format!("{failed}; {stuck}")),
    }
}

// --- the recording platform (tests only) -------------------------------------

/// One call the sequence made, in order. A call refused by an injected failure is
/// recorded too, so a call's index is stable across the success and failure runs.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Key(Key, Direction),
    Button(Button, Direction),
    MoveMouse(i32, i32),
    Settle(i32, i32),
    DragStep(i32, i32),
    Sleep(u64),
    Scroll(i32, Axis),
    Text(String),
    ClipboardRead,
    ClipboardWrite(String),
}

#[cfg(test)]
impl Call {
    fn is_release(&self) -> bool {
        matches!(
            self,
            Call::Key(_, Direction::Release) | Call::Button(_, Direction::Release)
        )
    }

    fn is_sleep(&self) -> bool {
        matches!(self, Call::Sleep(_))
    }
}

/// A [`Platform`] that records instead of posting, and can be told to refuse the
/// calls at given indices. `down` is what a real desktop would still be holding.
#[cfg(test)]
pub struct Recorder {
    pub calls: Vec<Call>,
    pub down: Vec<Item>,
    pub clipboard: Option<String>,
    fail_at: Vec<usize>,
}

#[cfg(test)]
impl Recorder {
    pub fn new(clipboard: Option<&str>) -> Recorder {
        Recorder {
            calls: Vec::new(),
            down: Vec::new(),
            clipboard: clipboard.map(str::to_string),
            fail_at: Vec::new(),
        }
    }

    /// Record the call, then refuse it if it is one under test. A refused call
    /// posts nothing, exactly as a failed OS call posts nothing.
    fn step(&mut self, call: Call) -> Result<(), String> {
        let index = self.calls.len();
        self.calls.push(call);
        if self.fail_at.contains(&index) {
            return Err("injected".to_string());
        }
        Ok(())
    }

    fn apply(&mut self, item: Item, direction: Direction) {
        match direction {
            Direction::Press => self.down.push(item),
            Direction::Release => {
                if let Some(at) = self.down.iter().rposition(|held| *held == item) {
                    self.down.remove(at);
                }
            }
            Direction::Click => {}
        }
    }
}

#[cfg(test)]
impl Platform for Recorder {
    fn key(&mut self, key: Key, direction: Direction) -> Result<(), String> {
        self.step(Call::Key(key, direction))?;
        self.apply(Item::Key(key), direction);
        Ok(())
    }

    fn button(&mut self, button: Button, direction: Direction) -> Result<(), String> {
        self.step(Call::Button(button, direction))?;
        self.apply(Item::Button(button), direction);
        Ok(())
    }

    fn move_mouse(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.step(Call::MoveMouse(x, y))
    }

    fn settle(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.step(Call::Settle(x, y))
    }

    fn drag_step(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.step(Call::DragStep(x, y))
    }

    fn scroll(&mut self, length: i32, axis: Axis) -> Result<(), String> {
        self.step(Call::Scroll(length, axis))
    }

    fn text(&mut self, text: &str) -> Result<(), String> {
        self.step(Call::Text(text.to_string()))
    }

    fn sleep(&mut self, ms: u64) -> Result<(), String> {
        self.calls.push(Call::Sleep(ms));
        Ok(())
    }

    fn clipboard_text(&mut self) -> Result<Option<String>, String> {
        self.step(Call::ClipboardRead)?;
        Ok(self.clipboard.clone())
    }

    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String> {
        self.step(Call::ClipboardWrite(text.to_string()))?;
        self.clipboard = Some(text.to_string());
        Ok(())
    }
}

/// Run `sequence` once cleanly, then once per fallible call it makes, with that one
/// call refused. After EVERY one of those runs nothing may be left held and the
/// clipboard must read exactly as the user left it.
///
/// The sweep stops at the guard's own first release: a failure injected INTO the
/// release is a key the OS refused to lift, which is the reporting path
/// (`a_failed_release_is_reported_...`), not this invariant.
#[cfg(test)]
pub fn sweep_injected_failures(
    clipboard: Option<&str>,
    sequence: impl Fn(&mut Recorder) -> Result<(), String>,
) -> Vec<Call> {
    let mut clean = Recorder::new(clipboard);
    sequence(&mut clean).expect("the clean run must succeed");
    assert!(clean.down.is_empty(), "clean run held {:?}", clean.down);
    assert_eq!(clean.clipboard.as_deref(), clipboard, "clean run clipboard");

    let body = clean
        .calls
        .iter()
        .position(Call::is_release)
        .unwrap_or(clean.calls.len());

    for at in 0..body {
        if clean.calls[at].is_sleep() {
            continue; // a sleep cannot fail, so there is nothing to inject
        }
        let mut run = Recorder::new(clipboard);
        run.fail_at = vec![at];
        let outcome = sequence(&mut run);

        let call = &clean.calls[at];
        assert!(
            outcome.is_err(),
            "call {at} ({call:?}) failed but returned Ok"
        );
        assert!(
            run.down.is_empty(),
            "call {at} ({call:?}) held {:?}",
            run.down
        );
        assert_eq!(
            run.clipboard.as_deref(),
            clipboard,
            "call {at} ({call:?}) left the clipboard changed"
        );
    }

    clean.calls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_order_is_the_reverse_of_press_order() {
        let mut platform = Recorder::new(None);
        guarded(&mut platform, |input| {
            input.press_keys(&[Key::Meta, Key::Shift, Key::Alt])
        })
        .unwrap();

        assert_eq!(
            platform.calls,
            vec![
                Call::Key(Key::Meta, Direction::Press),
                Call::Key(Key::Shift, Direction::Press),
                Call::Key(Key::Alt, Direction::Press),
                Call::Key(Key::Alt, Direction::Release),
                Call::Key(Key::Shift, Direction::Release),
                Call::Key(Key::Meta, Direction::Release),
            ]
        );
        assert!(platform.down.is_empty());
    }

    // The `hold/3` hole: the second of three modifiers refuses, and the first must
    // still come back up — it was recorded before it was posted.
    #[test]
    fn a_failure_part_way_through_press_keys_releases_what_went_down() {
        sweep_injected_failures(None, |platform| {
            guarded(platform, |input| {
                input.press_keys(&[Key::Meta, Key::Shift, Key::Control])
            })
        });
    }

    // A key the OS refuses to lift is the one thing the guard cannot fix, so it
    // must say so twice: on stderr, and in the error the caller gets back. The
    // loop still lifts everything else.
    #[test]
    fn a_failed_release_is_reported_and_the_rest_still_release() {
        let mut platform = Recorder::new(None);
        // 0,1 press; 2 is the release of Shift (pressed last).
        platform.fail_at = vec![2];
        let outcome = guarded(&mut platform, |input| {
            input.press_keys(&[Key::Meta, Key::Shift])
        });

        let message = outcome.expect_err("a stuck key is a failed action");
        assert!(
            message.contains("Shift") && message.contains("not released"),
            "the error must name the stuck key: {message}"
        );
        assert_eq!(
            platform.calls.last(),
            Some(&Call::Key(Key::Meta, Direction::Release)),
            "the rest of the stack must still be released"
        );
        assert_eq!(platform.down, vec![Item::Key(Key::Shift)]);
    }

    // The body's own error is not lost when the release fails too.
    #[test]
    fn both_failures_reach_the_caller() {
        let mut platform = Recorder::new(None);
        // 0,1 press; 2 = the body's own failure; 3 = Shift's release.
        platform.fail_at = vec![2, 3];
        let outcome = guarded(&mut platform, |input| {
            input.press_keys(&[Key::Meta, Key::Shift])?;
            input.click_key(Key::Unicode('v'))
        });

        assert_eq!(
            outcome,
            Err("injected; Key(Shift) was not released: injected".to_string()),
            "both halves must reach the caller"
        );
    }

    // A press the platform refused is still RELEASED — the down event may have gone
    // out before the call failed — but it is not named back to the caller as a stuck
    // key. Without this, every action on a machine with no Accessibility grant
    // answered "init input: ..." AND "Key(Meta) was not released", which reads as a
    // wedged keyboard where nothing was ever pressed.
    #[test]
    fn a_press_the_platform_refused_is_not_reported_as_a_stuck_key() {
        let mut platform = Recorder::new(None);
        platform.fail_at = vec![0, 1]; // the press, then its release
        let outcome = guarded(&mut platform, |input| input.press_keys(&[Key::Meta]));

        assert_eq!(outcome, Err("injected".to_string()));
        assert_eq!(
            platform.calls,
            vec![
                Call::Key(Key::Meta, Direction::Press),
                Call::Key(Key::Meta, Direction::Release),
            ],
            "the release must still be attempted"
        );
    }

    // The panic path has no `?` to ride, so the release has to come from `Drop`.
    #[test]
    fn a_panic_mid_sequence_still_releases() {
        let mut platform = Recorder::new(None);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guarded(&mut platform, |input| -> Result<(), String> {
                input.press_button(Button::Left)?;
                panic!("the sequence blew up");
            })
        }));

        assert!(panicked.is_err(), "the panic must propagate");
        assert!(platform.down.is_empty(), "left {:?} held", platform.down);
        assert_eq!(
            platform.calls.last(),
            Some(&Call::Button(Button::Left, Direction::Release))
        );
    }

    // "If and only if this process changed them": an untouched clipboard is never
    // written, and a failed write arms no restore.
    #[test]
    fn the_clipboard_is_left_alone_unless_this_process_wrote_to_it() {
        let mut untouched = Recorder::new(Some("the user's text"));
        guarded(&mut untouched, |input| input.press_keys(&[Key::Meta])).unwrap();
        assert!(!untouched
            .calls
            .iter()
            .any(|call| matches!(call, Call::ClipboardWrite(_))));
        assert_eq!(untouched.clipboard.as_deref(), Some("the user's text"));

        let mut refused = Recorder::new(Some("the user's text"));
        refused.fail_at = vec![1]; // the write of our own text
        let outcome = guarded(&mut refused, |input| input.take_clipboard("ours"));
        assert!(outcome.is_err());
        assert_eq!(refused.clipboard.as_deref(), Some("the user's text"));
        assert_eq!(
            refused
                .calls
                .iter()
                .filter(|call| matches!(call, Call::ClipboardWrite(_)))
                .count(),
            1,
            "a refused write must not arm a restore"
        );
    }

    // A clipboard we cannot put back is reported on stderr only: the action itself
    // landed, and failing it would have the caller paste a second time.
    #[test]
    fn a_failed_clipboard_restore_does_not_fail_the_action() {
        let mut platform = Recorder::new(Some("the user's text"));
        // 0 read, 1 our write, 2 the restore dwell, 3 the restore write.
        platform.fail_at = vec![3];
        let outcome = guarded(&mut platform, |input| input.take_clipboard("ours"));

        assert_eq!(outcome, Ok(()));
        assert_eq!(platform.clipboard.as_deref(), Some("ours"));
        assert!(platform.down.is_empty());
    }

    // Nothing was held, so nothing is released — the guard adds no events of its own.
    #[test]
    fn a_sequence_that_holds_nothing_posts_nothing_extra() {
        let mut platform = Recorder::new(None);
        guarded(&mut platform, |input| input.click_button(Button::Left)).unwrap();

        assert_eq!(
            platform.calls,
            vec![Call::Button(Button::Left, Direction::Click)]
        );
    }
}
