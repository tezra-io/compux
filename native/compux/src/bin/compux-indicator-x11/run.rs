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
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use x11rb::protocol::Event;

use crate::draw::{Canvas, Painter};
use crate::label::target_label;
use crate::layout::{layout, Button, Layout};
use crate::place::{badge_frame, HEIGHT};
use crate::wire::{self, Activity, Line, TargetState, WireError, LINE_LIMIT};
use crate::x11::{Delivered, Window};

/// How many times a second the badge puts itself back on top. A window that keeps
/// raising itself over the badge gets this many answers and no more, so two such
/// windows cannot spin the X server between them.
pub const MAX_RAISES_PER_SECOND: u32 = 10;
/// How many ignored lines are described on stderr before they are only counted.
pub const MAX_LOGGED_IGNORED: u32 = 5;
/// How many sent button events are described on stderr before they are only
/// counted.
pub const MAX_LOGGED_FORGED: u32 = 5;

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

/// Everything the loop is handed.
enum Message {
    Input(Line),
    InputEnded,
    InputFailed(String),
    X(Delivered),
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
        pressed: None,
        ignored: 0,
        forged: 0,
        raises: RaiseBudget::new(),
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
    /// The button a press went down on, waiting for its release.
    pressed: Option<Button>,
    ignored: u32,
    /// Button events other programs sent, dropped.
    forged: u32,
    raises: RaiseBudget,
    started: Instant,
}

impl Badge {
    /// Bounded by the pipe and the connection: it ends when either does.
    fn run(&mut self, receiver: &Receiver<Message>) -> Result<(), String> {
        for message in receiver.iter() {
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

    /// Lay out, place and draw the state the helper last sent, or hide.
    fn render(&mut self) -> Result<(), String> {
        let label = target_label(&self.state.app, &self.state.title);
        let painter = &self.painter;
        self.row = layout(self.state.activity, &label, &|text| painter.measure(text));
        let screens = self.window.screens()?;
        let Some(frame) = badge_frame(&self.state, (self.row.width, HEIGHT), &screens) else {
            self.canvas = None;
            return self.window.hide();
        };
        let canvas = self.painter.render(&self.row, self.state.activity);
        self.window.show(frame, &canvas, self.row.input_region())?;
        self.canvas = Some(canvas);
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
            // A window placed directly over the badge, or a new one mapped.
            Event::ConfigureNotify(e) if e.window != id && e.above_sibling == id => {
                self.on_top()?
            }
            Event::MapNotify(e) if e.window != id => self.on_top()?,
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

    /// Back on top while shown, within the budget.
    fn on_top(&mut self) -> Result<(), String> {
        if self.canvas.is_none() {
            return Ok(());
        }
        let now_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !self.raises.allow(now_ms) {
            return Ok(());
        }
        self.window.raise()
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

    #[test]
    fn the_badge_raises_itself_at_most_ten_times_a_second() {
        let mut budget = RaiseBudget::new();
        let allowed = (0..25).filter(|i| budget.allow(5_000 + i * 10)).count();
        assert_eq!(allowed, MAX_RAISES_PER_SECOND as usize);
        assert!(!budget.allow(5_999));
        assert!(budget.allow(6_000), "a new second, a new budget");
    }
}
