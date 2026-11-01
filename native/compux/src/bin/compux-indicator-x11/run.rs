//! The running badge: the pipe on one side, the window on the other, one loop
//! between them.
//!
//! Two reader threads, one for stdin and one for the X connection, hand what they
//! read to this loop over one channel; everything that draws or writes happens
//! here, on the main thread, so there is no shared state. The loop ends when the
//! helper closes the pipe (it released the target, or it is gone) and the process
//! exits with the window. A press writes its event and changes nothing on screen:
//! the helper's next state line does that, so the badge never shows a state the
//! gate is not in.

use std::io::Write;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use x11rb::protocol::Event;

use crate::draw::{Canvas, Painter};
use crate::label::target_label;
use crate::layout::{layout, Button, Layout};
use crate::place::{badge_frame, Rect, HEIGHT};
use crate::wire::{self, Activity, Line, TargetState, WireError, LINE_LIMIT};
use crate::x11::{outer, Delivered, Stacked, Window, MAX_ABOVE};

/// How many times a second the badge puts itself back on top. A window that keeps
/// raising itself over the badge gets this many answers and no more, so two such
/// windows cannot spin the X server between them.
pub const MAX_RAISES_PER_SECOND: u32 = 10;
/// How many ignored lines are described on stderr before they are only counted.
pub const MAX_LOGGED_IGNORED: u32 = 5;
/// How many sent button events are described on stderr before they are only
/// counted.
pub const MAX_LOGGED_FORGED: u32 = 5;

/// How often the badge looks again while a menu or popup holds it under. A
/// window that takes input over the badge and never unmaps (a desktop shell's
/// stage while its calendar or a notification is open) lets it back on top once
/// it no longer takes input there.
pub const HELD_LOOK_MS: u64 = 500;

/// Raises left in the current second.
pub struct RaiseBudget {
    second: u64,
    used: u32,
}

impl RaiseBudget {
    pub fn new() -> RaiseBudget {
        RaiseBudget { second: 0, used: 0 }
    }

    /// Whether a raise at `now_ms` is within the budget; counts it when it is.
    pub fn allow(&mut self, now_ms: u64) -> bool {
        let second = now_ms / 1000;
        if second != self.second {
            self.second = second;
            self.used = 0;
        }
        if self.used >= MAX_RAISES_PER_SECOND {
            return false;
        }
        self.used += 1;
        true
    }
}

/// Whether `event` is a button press or release another program sent
/// (`SendEvent`), rather than one the server made from the pointer. Any X
/// client can send the badge's window a click on Stop or Resume, so those are
/// never a press. Input made through XTEST is the server's own, as the person's
/// is: under the X11 model nothing tells the two apart, for this window or any
/// other client's.
pub fn forged(event: &Event) -> bool {
    matches!(event, Event::ButtonPress(_) | Event::ButtonRelease(_)) && event.sent_event()
}

/// Whether another window's move, resize, restack or map calls for the badge to
/// go back on top: one the window manager manages, whose outer rectangle now
/// meets the badge's. An override-redirect window never does: it is the
/// person's menu or tooltip, and the badge stays under what they opened.
pub fn comes_over(badge: Rect, window: Rect, override_redirect: bool) -> bool {
    !override_redirect && badge.intersection(window).is_some()
}

/// The person's menu or popup open over the badge at `badge`, among `above`
/// (the root's children stacked over the badge, bottom to top): the front-most
/// that is viewable, override-redirect and takes input somewhere on the badge.
/// A window taking input nowhere there is not one: a desktop shell's stage over
/// the whole screen takes it only at its own panels.
pub fn popup_over(badge: Rect, above: &[Stacked]) -> Option<u32> {
    above
        .iter()
        .rev()
        .find(|window| {
            window.viewable
                && window.override_redirect
                && window
                    .input
                    .iter()
                    .flatten()
                    .any(|input| input.intersection(badge).is_some())
        })
        .map(|window| window.id)
}

/// The window the badge goes just over (still under anything over that), when
/// `came_over`, the managed window that moved, restacked or mapped onto it, is
/// among `above`, the windows stacked over the badge now. `None` where it is
/// under the badge already: the badge stays where it is, since going just over
/// it would lower the badge.
pub fn restack(came_over: u32, above: &[Stacked]) -> Option<u32> {
    above
        .iter()
        .any(|stacked| stacked.id == came_over)
        .then_some(came_over)
}

/// Everything the loop is handed.
enum Message {
    Input(Line),
    InputEnded,
    InputFailed(String),
    X(Delivered),
}

/// What the loop takes next.
enum Next {
    Message(Message),
    /// The badge is held under a window and nothing arrived in a while: look
    /// again whether it still is.
    LookAgain,
    /// Every sender is gone.
    Closed,
}

/// The next message, waited for as long as it takes; or, while the badge is
/// held under a window, for `look_after` at most.
fn next(receiver: &Receiver<Message>, look_after: Option<Duration>) -> Next {
    let Some(wait) = look_after else {
        return receiver.recv().map_or(Next::Closed, Next::Message);
    };
    match receiver.recv_timeout(wait) {
        Ok(message) => Next::Message(message),
        Err(RecvTimeoutError::Timeout) => Next::LookAgain,
        Err(RecvTimeoutError::Disconnected) => Next::Closed,
    }
}

/// How long the loop waits before it looks again, while `held_by` holds the
/// badge under; forever while nothing does.
fn look_after(held_by: Option<u32>) -> Option<Duration> {
    held_by.map(|_| Duration::from_millis(HELD_LOOK_MS))
}

/// Whether the loop goes on.
enum Flow {
    Continue,
    Finish,
}

/// Run the badge until the helper closes the pipe. Exit 0 then; 2 when the badge
/// could not be shown or its display went away, which the helper sees as the
/// badge gone.
pub fn badge() -> ExitCode {
    match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("compux-indicator-x11: {reason}");
            ExitCode::from(2)
        }
    }
}

fn serve() -> Result<(), String> {
    let painter = Painter::new()?;
    let window = Window::open()?;
    let (sender, receiver) = mpsc::channel::<Message>();
    window.listen(sender.clone(), Message::X)?;
    read_input(sender)?;

    let mut badge = Badge {
        row: layout(Activity::Working, "", &|text| painter.measure(text)),
        painter,
        window,
        state: TargetState::initial(),
        canvas: None,
        frame: None,
        pressed: None,
        ignored: 0,
        forged: 0,
        raises: RaiseBudget::new(),
        held_by: None,
        crowded: false,
        started: Instant::now(),
    };
    badge.render()?;
    // The window exists: the helper may hold the target's mutating requests until
    // it has read this line.
    if send(wire::Event::Ready).is_err() {
        return badge.finish();
    }
    badge.run(&receiver)
}

/// The badge's state, owned by the loop.
struct Badge {
    painter: Painter,
    window: Window,
    /// The last thing the helper said.
    state: TargetState,
    row: Layout,
    /// What is on the window now, for an expose; `None` while hidden.
    canvas: Option<Canvas>,
    /// Where the window is now; `None` while hidden.
    frame: Option<Rect>,
    /// The button a press went down on, waiting for its release.
    pressed: Option<Button>,
    ignored: u32,
    /// Button events other programs sent, dropped.
    forged: u32,
    raises: RaiseBudget,
    /// The person's menu or popup open over the badge: the badge stays under it,
    /// and goes back on top when it is unmapped.
    held_by: Option<u32>,
    /// Whether more than [`MAX_ABOVE`] windows above the badge were said so.
    crowded: bool,
    started: Instant,
}

impl Badge {
    /// Bounded by the pipe and the connection: it ends when either does. While
    /// a window holds the badge under, it also looks again every
    /// [`HELD_LOOK_MS`].
    fn run(&mut self, receiver: &Receiver<Message>) -> Result<(), String> {
        loop {
            let message = match next(receiver, look_after(self.held_by)) {
                Next::Message(message) => message,
                Next::LookAgain => {
                    self.back_on_top()?;
                    continue;
                }
                Next::Closed => break,
            };
            let flow = match message {
                Message::Input(line) => self.line(line)?,
                Message::InputEnded => Flow::Finish,
                Message::InputFailed(e) => {
                    return Err(format!("the helper's pipe could not be read: {e}"))
                }
                Message::X(Delivered::Event(event)) => self.event(event)?,
                Message::X(Delivered::Lost(e)) => {
                    return Err(format!("the X connection was lost: {e}"))
                }
            };
            if let Flow::Finish = flow {
                break;
            }
        }
        self.finish()
    }

    /// Everything off the screen. The process exits next, and the window with it.
    fn finish(&mut self) -> Result<(), String> {
        if self.ignored > 0 {
            eprintln!(
                "compux-indicator-x11: {} state line(s) ignored",
                self.ignored
            );
        }
        if self.forged > MAX_LOGGED_FORGED {
            eprintln!(
                "compux-indicator-x11: {} button event(s) another program sent were dropped",
                self.forged
            );
        }
        self.window.hide()
    }

    fn line(&mut self, line: Line) -> Result<Flow, String> {
        let decoded = match line {
            // A blank line is the pipe's punctuation, not a state line.
            Line::Text(text) if text.trim().is_empty() => return Ok(Flow::Continue),
            Line::Text(text) => wire::decode(&text),
            Line::TooLong(count) => Err(WireError::TooLong(count)),
            Line::NotText => Err(WireError::NotText),
        };
        match decoded {
            Ok(state) => {
                self.state = state;
                self.render()?;
            }
            Err(error) => self.ignore(&error),
        }
        Ok(Flow::Continue)
    }

    /// Counted rather than fatal: a helper that writes one bad line must not lose
    /// its badge. The line itself is never echoed: it carries another
    /// application's text, and the hostile line is exactly the one ignored.
    fn ignore(&mut self, error: &WireError) {
        self.ignored += 1;
        if self.ignored <= MAX_LOGGED_IGNORED {
            eprintln!("compux-indicator-x11: ignored a state line: {error}");
        }
        if self.ignored == MAX_LOGGED_IGNORED {
            eprintln!("compux-indicator-x11: further ignored lines are counted, not logged");
        }
    }

    /// Lay out, place and draw the state the helper last sent, or hide. The
    /// badge goes on top unless the person's menu or popup is open over it.
    fn render(&mut self) -> Result<(), String> {
        let label = target_label(&self.state.app, &self.state.title);
        let painter = &self.painter;
        self.row = layout(self.state.activity, &label, &|text| painter.measure(text));
        let screens = self.window.screens()?;
        let Some(frame) = badge_frame(&self.state, (self.row.width, HEIGHT), &screens) else {
            self.canvas = None;
            self.frame = None;
            // A hidden badge is held under nothing.
            self.held_by = None;
            return self.window.hide();
        };
        let canvas = self.painter.render(&self.row, self.state.activity);
        self.window.show(frame, &canvas, self.row.input_region())?;
        self.canvas = Some(canvas);
        self.frame = Some(frame);
        if self.free_above(frame)?.is_some() {
            self.window.raise()?;
        }
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<Flow, String> {
        if forged(&event) {
            self.drop_forged(&event);
            return Ok(Flow::Continue);
        }
        let id = self.window.id();
        match event {
            Event::Expose(e) if e.window == id && e.count == 0 => self.repaint()?,
            Event::ButtonPress(e) if e.event == id && e.detail == 1 => {
                self.pressed = self.row.hit(i32::from(e.event_x), i32::from(e.event_y));
            }
            Event::ButtonRelease(e) if e.event == id && e.detail == 1 => {
                let released = self.row.hit(i32::from(e.event_x), i32::from(e.event_y));
                return Ok(self.release(released));
            }
            // Another window moved, resized, restacked or mapped where the badge is.
            Event::ConfigureNotify(e) if e.window != id => {
                let at = outer(e.x, e.y, e.width, e.height, e.border_width);
                self.over(e.window, at, e.override_redirect)?
            }
            Event::MapNotify(e) if e.window != id && !e.override_redirect => {
                self.mapped(e.window)?
            }
            // The menu or popup the badge stayed under closed.
            Event::UnmapNotify(e) if Some(e.window) == self.held_by => {
                self.held_by = None;
                self.back_on_top()?
            }
            Event::RandrScreenChangeNotify(_) => self.render()?,
            Event::Error(e) => {
                return Err(format!(
                    "the X display refused a request of the badge's: {e:?}"
                ))
            }
            _unrelated => {}
        }
        Ok(Flow::Continue)
    }

    /// Counted, and described while few: a program pressing the badge's buttons
    /// is worth the log, a flood of them is not.
    fn drop_forged(&mut self, event: &Event) {
        self.forged += 1;
        if self.forged <= MAX_LOGGED_FORGED {
            let kind = match event {
                Event::ButtonPress(_) => "press",
                _release => "release",
            };
            eprintln!(
                "compux-indicator-x11: dropped a button {kind} another program sent; only the \
                 pointer's own clicks press the badge"
            );
        }
        if self.forged == MAX_LOGGED_FORGED {
            eprintln!("compux-indicator-x11: further sent button events are counted, not logged");
        }
    }

    fn repaint(&self) -> Result<(), String> {
        match &self.canvas {
            Some(canvas) => self.window.paint(canvas),
            None => Ok(()),
        }
    }

    /// A click is a press and a release on the same button. What the first button
    /// sends is decided by what the helper last said, not by what it shows.
    fn release(&mut self, released: Option<Button>) -> Flow {
        let pressed = self.pressed.take();
        if pressed.is_none() || pressed != released {
            return Flow::Continue;
        }
        let event = match (pressed, self.state.activity) {
            (Some(Button::Stop), _) => wire::Event::Stop,
            (_, Activity::Paused) => wire::Event::Resume,
            _pause => wire::Event::Pause,
        };
        // A pipe nobody reads means the helper is gone, and so is the badge's job.
        match send(event) {
            Ok(()) => Flow::Continue,
            Err(_helper_gone) => Flow::Finish,
        }
    }

    /// Back on top of `window`, now at `at`, where it meets the badge, unless
    /// the window is the person's menu or tooltip.
    fn over(&mut self, window: u32, at: Rect, override_redirect: bool) -> Result<(), String> {
        match self.frame {
            Some(frame) if comes_over(frame, at, override_redirect) => self.on_top(window),
            _elsewhere => Ok(()),
        }
    }

    /// A map says nothing of where: one look at the window says it. A window
    /// gone since needs nothing.
    fn mapped(&mut self, window: u32) -> Result<(), String> {
        match self.window.outer_of(window)? {
            Some(at) => self.over(window, at, false),
            None => Ok(()),
        }
    }

    /// Back over `came_over`, the window that came over the badge, while shown
    /// and within the budget, unless the person's menu or popup is open over
    /// it.
    fn on_top(&mut self, came_over: u32) -> Result<(), String> {
        let Some(frame) = self.frame else {
            return Ok(());
        };
        let now_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !self.raises.allow(now_ms) {
            return Ok(());
        }
        let Some(above) = self.free_above(frame)? else {
            return Ok(());
        };
        match restack(came_over, &above) {
            Some(window) => self.window.raise_above(window),
            None => Ok(()),
        }
    }

    /// Back on top once the window that held the badge under is unmapped or no
    /// longer takes input over it, unless another one does. Outside the raise
    /// budget: it runs once for each popup that closes, and otherwise at most
    /// every [`HELD_LOOK_MS`], so a spent budget never leaves the badge under.
    fn back_on_top(&mut self) -> Result<(), String> {
        let Some(frame) = self.frame else {
            return Ok(());
        };
        if self.free_above(frame)?.is_some() {
            self.window.raise()?;
        }
        Ok(())
    }

    /// The windows stacked above the badge at `frame`, where nothing the person
    /// opened is over it. A menu or popup taking input there is waited for (its
    /// unmap brings the badge back on top), and the answer is then `None`; so it
    /// is past [`MAX_ABOVE`] windows above the badge, which are not read, and the
    /// badge stays where it is.
    fn free_above(&mut self, frame: Rect) -> Result<Option<Vec<Stacked>>, String> {
        let Some(above) = self.window.above(frame)? else {
            if !self.crowded {
                eprintln!(
                    "compux-indicator-x11: more than {MAX_ABOVE} windows are stacked above the \
                     badge; it stays where it is rather than cover one"
                );
                self.crowded = true;
            }
            return Ok(None);
        };
        self.held_by = popup_over(frame, &above);
        Ok(self.held_by.is_none().then_some(above))
    }
}

/// One event line on stdout, flushed at once so the helper sees the press when it
/// happens.
fn send(event: wire::Event) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(event.line().as_bytes())?;
    out.flush()
}

/// Read stdin on a thread of its own, a bounded line at a time. Bounded by the
/// pipe: it ends at its end, at the first failed read, or when nothing listens.
fn read_input(sender: Sender<Message>) -> Result<(), String> {
    std::thread::Builder::new()
        .name("compux-indicator-x11 input".to_string())
        .spawn(move || {
            let mut input = std::io::stdin().lock();
            loop {
                let message = match wire::read_line(&mut input, LINE_LIMIT) {
                    Ok(Some(line)) => Message::Input(line),
                    Ok(None) => Message::InputEnded,
                    Err(e) => Message::InputFailed(e.to_string()),
                };
                let last = !matches!(message, Message::Input(_));
                if sender.send(message).is_err() || last {
                    return;
                }
            }
        })
        .map(|_thread| ())
        .map_err(|e| format!("the input reader would not start: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{
        ButtonPressEvent, KeyButMask, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, EXPOSE_EVENT,
    };

    fn button(response_type: u8) -> ButtonPressEvent {
        ButtonPressEvent {
            response_type,
            detail: 1,
            sequence: 0,
            time: 0,
            root: 1,
            event: 2,
            child: 0,
            root_x: 310,
            root_y: 140,
            event_x: 310,
            event_y: 14,
            state: KeyButMask::default(),
            same_screen: true,
        }
    }

    // Any X client can send any window a button event; the server marks the
    // ones it did not make itself, and only those are refused.
    #[test]
    fn a_button_event_another_program_sent_is_not_a_press() {
        const SENT: u8 = 0x80;
        assert!(forged(&Event::ButtonPress(button(
            BUTTON_PRESS_EVENT | SENT
        ))));
        assert!(forged(&Event::ButtonRelease(button(
            BUTTON_RELEASE_EVENT | SENT
        ))));
        assert!(!forged(&Event::ButtonPress(button(BUTTON_PRESS_EVENT))));
        assert!(!forged(&Event::ButtonRelease(button(BUTTON_RELEASE_EVENT))));
        let expose = x11rb::protocol::xproto::ExposeEvent {
            response_type: EXPOSE_EVENT | SENT,
            sequence: 0,
            window: 2,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            count: 0,
        };
        assert!(!forged(&Event::Expose(expose)), "only buttons act");
    }

    // The badge goes back over a managed window that now meets it (the bound
    // window raised over it, or another moved onto it), never over a menu or a
    // tooltip the person opened, and not for a window elsewhere.
    #[test]
    fn the_badge_goes_back_on_top_only_of_a_managed_window_that_meets_it() {
        let badge = Rect {
            x: 60,
            y: 126,
            w: 440,
            h: 28,
        };
        let over = Rect {
            x: 0,
            y: 120,
            w: 982,
            h: 795,
        };
        let below = Rect {
            x: 0,
            y: 154,
            w: 982,
            h: 795,
        };
        assert!(comes_over(badge, over, false));
        assert!(
            !comes_over(badge, over, true),
            "a menu stays over the badge"
        );
        assert!(!comes_over(badge, below, false), "touching is not meeting");
    }

    fn stacked(id: u32, viewable: bool, override_redirect: bool, input: &[Rect]) -> Stacked {
        Stacked {
            id,
            viewable,
            override_redirect,
            input: Some(input.to_vec()),
        }
    }

    // A menu or popup open over the badge keeps it under: the badge goes back on
    // top only once nothing the person opened is over it.
    #[test]
    fn a_popup_taking_input_over_the_badge_keeps_it_under() {
        let badge = Rect {
            x: 300,
            y: 8,
            w: 432,
            h: 28,
        };
        let menu = Rect {
            x: 280,
            y: -12,
            w: 520,
            h: 320,
        };
        let panel = Rect {
            x: 0,
            y: 0,
            w: 1280,
            h: 4,
        };
        assert_eq!(
            popup_over(badge, &[stacked(7, true, true, &[menu])]),
            Some(7)
        );
        let both = [
            stacked(7, true, true, &[menu]),
            stacked(9, true, true, &[menu]),
        ];
        assert_eq!(popup_over(badge, &both), Some(9), "the front-most");
        assert_eq!(popup_over(badge, &[stacked(7, false, true, &[menu])]), None);
        assert_eq!(
            popup_over(badge, &[stacked(7, true, false, &[menu])]),
            None,
            "a managed window is raised over, not waited for"
        );
        assert_eq!(
            popup_over(badge, &[stacked(7, true, true, &[panel])]),
            None,
            "a shell's stage takes input only at its panel"
        );
        let unread = Stacked {
            input: None,
            ..stacked(7, true, true, &[])
        };
        assert_eq!(popup_over(badge, &[unread]), None);
        assert_eq!(popup_over(badge, &[]), None);
    }

    // A managed window that comes over the badge has the badge go just over it,
    // never to the very top: a menu stacked over that window stays over the badge.
    #[test]
    fn the_badge_goes_just_over_the_window_that_came_over_it() {
        let above = [stacked(7, true, false, &[]), stacked(9, true, true, &[])];
        assert_eq!(restack(7, &above), Some(7));
        assert_eq!(
            restack(5, &above),
            None,
            "a window under the badge needs nothing"
        );
    }

    // A window that holds the badge under and never unmaps is looked at again on
    // a timer; nothing else wakes a loop that waits only for messages.
    #[test]
    fn a_held_badge_looks_again_on_its_own_and_a_free_one_waits_for_a_message() {
        let wait = Duration::from_millis(20);
        let (sender, receiver) = mpsc::channel::<Message>();
        let started = Instant::now();
        assert!(matches!(next(&receiver, Some(wait)), Next::LookAgain));
        assert!(started.elapsed() >= wait);
        sender.send(Message::InputEnded).unwrap();
        assert!(matches!(
            next(&receiver, Some(wait)),
            Next::Message(Message::InputEnded)
        ));
        sender.send(Message::InputEnded).unwrap();
        assert!(matches!(
            next(&receiver, None),
            Next::Message(Message::InputEnded)
        ));
        drop(sender);
        assert!(matches!(next(&receiver, None), Next::Closed));
        assert!(matches!(next(&receiver, Some(wait)), Next::Closed));
        assert_eq!(
            look_after(Some(7)),
            Some(Duration::from_millis(HELD_LOOK_MS))
        );
        assert_eq!(look_after(None), None);
    }

    #[test]
    fn the_badge_raises_itself_at_most_ten_times_a_second() {
        let mut budget = RaiseBudget::new();
        let allowed = (0..25).filter(|i| budget.allow(5_000 + i * 10)).count();
        assert_eq!(allowed, MAX_RAISES_PER_SECOND as usize);
        assert!(!budget.allow(5_999));
        assert!(budget.allow(6_000), "a new second, a new budget");
    }
}
