//! The ownership indicator: the badge that tells the person this helper is working
//! in their window, and the Pause and Stop on it that reach the gate directly.
//!
//! ## Why it is a second process
//!
//! A panel needs AppKit, AppKit needs the main thread's run loop, and this
//! process's main thread runs the input worker — deliberately, because some of
//! enigo's keyboard-layout calls are only safe there. A child gets its own main
//! thread, cannot stall or crash the worker, and ships inside the same signed
//! bundle, so it is one identity in Privacy settings and needs no grant of its own.
//!
//! ## The path its buttons take
//!
//! Child stdout, this module's reader thread, [`Gate`] — and only then the owner,
//! as a `session_event`. **The gate is flipped before anything is written**, for
//! exactly the reason `control.rs` flips it before writing an acknowledgement: a
//! Stop must stop the machine even when the daemon is suspended, and the report is
//! a report rather than a request for permission.
//!
//! ## No ready indicator, no target mutation
//!
//! A bound window is the helper working where the person is working, and the badge
//! is the only thing that says so. Until the child says `ready`, and after it has
//! exited, a mutating request that names a target is refused
//! `control_surface_unavailable`. One restart, then the refusal stands until the
//! target is reselected — a badge that keeps dying is a fault to report, not a loop
//! to run. Display-level requests are untouched: they are today's foreground mode
//! with today's `/pause`.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child as OsChild, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::capture::Emitter;
use crate::gate::{Clock, Gate};
use crate::window_server::{self, Bounds, Windows};
use crate::wire;

/// The executable that draws the badge, beside this one inside the bundle.
pub const INDICATOR_BINARY: &str = "compux-indicator";

/// How often the window is re-read while a target is held. Four times a second is
/// fast enough that a dragged window's badge keeps up and slow enough that it costs
/// nothing; it is a poll rather than a subscription because the window server has no
/// "this window moved" notification a non-AppKit process can take.
pub const WATCH_INTERVAL_MS: u64 = 250;

/// How long `stop` waits for the child to go after its input is closed. It exits on
/// EOF, so this only ever covers scheduling — and it is bounded because a wedged
/// badge must never hold a release open.
pub const STOP_WAIT_MS: u64 = 1_000;

/// **The badge's possible area — the contract between this process and the panel.**
///
/// The child may draw the badge in one of two places relative to the window's
/// top-left corner: OUTSIDE, floating just above the window's frame, or INSIDE,
/// below the traffic lights where a title bar has room. Which one it picks is the
/// child's business (`native/indicator/Placement.swift`); what is NOT the child's
/// business is where this process looks for something covering it, so the numbers
/// live here, once, and the child is written against them.
///
/// The rectangle below covers BOTH placements. It starts [`BADGE_ABOVE`] points
/// above the window's top edge and runs to [`BADGE_INSIDE_DROP`] plus
/// [`BADGE_HEIGHT`] below it, [`BADGE_WIDTH`] wide from the window's left edge. A
/// window in front of the target that touches any of it is a window the badge
/// cannot be drawn over, and the child hides rather than drawing on somebody else's
/// window.
///
/// The width is the child's own ceiling, not a guess: `Placement.inset` (8) plus
/// `Placement.maxBadgeWidth` (440), which the child enforces on its panel and
/// proves in its headless self-test; a badge that cannot fit inside this rectangle
/// hides. The height is `Placement.badgeHeight`. Change one side and the other
/// must move with it.
pub const BADGE_WIDTH: f64 = 448.0;
pub const BADGE_HEIGHT: f64 = 28.0;
/// How far above the window's top edge the outside placement floats.
pub const BADGE_ABOVE: f64 = 34.0;
/// How far below the window's top edge the inside placement sits — clear of the
/// traffic lights.
pub const BADGE_INSIDE_DROP: f64 = 52.0;

/// Every point the badge might be drawn on, for a window at these bounds.
///
/// One rectangle rather than one corner: the badge is a panel with a size, it sits
/// ABOVE the window's frame in one of its two placements, and a hit test of the
/// window's own corner asks about a point the badge may not even cover.
pub fn badge_area(bounds: &Bounds) -> Bounds {
    Bounds {
        x: bounds.x,
        y: bounds.y - BADGE_ABOVE,
        w: BADGE_WIDTH,
        h: BADGE_ABOVE + BADGE_INSIDE_DROP + BADGE_HEIGHT,
    }
}

/// What the badge shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Showing {
    Working,
    Paused,
    /// The window cannot be seen or reached just now — minimized, gone, or its
    /// frames stopped. The badge says so rather than sitting there claiming work.
    Unavailable,
}

impl Showing {
    fn as_str(self) -> &'static str {
        match self {
            Showing::Working => "working",
            Showing::Paused => "paused",
            Showing::Unavailable => "unavailable",
        }
    }
}

/// One state line, as the child reads it.
///
/// `bounds` is `None` when there is nowhere to put the badge, which the child
/// treats as "hide it": a badge with no window under it would sit on somebody
/// else's, which is the one thing it must never do.
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub showing: Showing,
    pub app: String,
    pub title: String,
    pub bounds: Option<Bounds>,
    pub occluded: bool,
}

impl State {
    pub fn line(&self) -> String {
        let bounds = match self.bounds {
            None => Value::Null,
            Some(bounds) => json!({
                "x": bounds.x, "y": bounds.y, "w": bounds.w, "h": bounds.h
            }),
        };

        json!({
            "state": self.showing.as_str(),
            "app": self.app,
            "title": self.title,
            "bounds": bounds,
            "occluded": self.occluded,
        })
        .to_string()
    }
}

/// A button on the badge, or the child saying it is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressed {
    Ready,
    Pause,
    Resume,
    Stop,
}

impl Pressed {
    fn parse(line: &str) -> Option<Pressed> {
        let frame: Value = serde_json::from_str(line).ok()?;

        match frame.get("event").and_then(Value::as_str)? {
            "ready" => Some(Pressed::Ready),
            "pause" => Some(Pressed::Pause),
            "resume" => Some(Pressed::Resume),
            "stop" => Some(Pressed::Stop),
            _unknown => None,
        }
    }

    /// The `session_event` this is reported to the owner as. `Ready` is not one:
    /// it is the child coming up, which nothing outside this process acts on.
    fn event(self) -> Option<&'static str> {
        match self {
            Pressed::Ready => None,
            Pressed::Pause => Some("operator_pause"),
            Pressed::Resume => Some("operator_resume"),
            Pressed::Stop => Some("operator_stop"),
        }
    }
}

/// What a press does, the instant it is read.
///
/// A trait so the whole path — read a line, flip the gate, report it, mark the
/// child up or gone — is proved with no process at all.
pub trait Buttons: Send + Sync {
    fn pressed(&self, pressed: Pressed);
    /// The child's stdout reached EOF: it has exited, and no press can arrive
    /// again until it is restarted.
    fn ended(&self);
}

/// The production one: the gate first, the owner second.
pub struct GateButtons {
    gate: Gate,
    emitter: Emitter,
    ready: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
}

impl Buttons for GateButtons {
    fn pressed(&self, pressed: Pressed) {
        if pressed == Pressed::Ready {
            self.ready.store(true, Ordering::SeqCst);
            return;
        }

        // Applied to the gate AT ONCE, exactly as `control.rs` applies a control
        // frame: a Stop has to stop the machine whether or not anything is
        // listening for the report that follows.
        //
        // A Stop installs the same barrier a Pause does. The helper cannot end the
        // session — that is the owner's, and the `operator_stop` beside this is how
        // it is asked to — but it CAN see to it that nothing else is dispatched
        // while the asking happens, which is what a person pressing Stop means.
        let applied = match pressed {
            Pressed::Resume => self.gate.control(wire::ControlAction::Resume, None),
            _pause_or_stop => self.gate.control(wire::ControlAction::Pause, None),
        };

        if let Some(event) = pressed.event() {
            self.emitter.emit_frame(&wire::session_event(
                self.gate.envelope(),
                "indicator",
                event,
                applied.authorization_generation,
            ));
        }
    }

    fn ended(&self) {
        self.ready.store(false, Ordering::SeqCst);
        self.alive.store(false, Ordering::SeqCst);
    }
}

/// One running indicator process.
///
/// Shared between the worker (which writes state lines on selection and on pause)
/// and the watch thread (which writes them when the window moves), so it is an
/// `Arc` and its writer is behind a mutex — held for one line and never across
/// anything else.
pub trait Child: Send + Sync {
    /// Write one state line. `Err` means the child is gone.
    fn write_line(&self, line: &str) -> Result<(), String>;

    /// Its process id, while it is running. The watch needs it to tell the badge's
    /// own windows from windows that really cover the target: the badge sits over
    /// the target's corner by design, and counting it would have the badge report
    /// the window it is about as covered by itself.
    fn pid(&self) -> Option<i32>;

    /// Close its input, wait for it to exit, and reap it. Idempotent, because it is
    /// reached from a release, from a replacement and from `Drop`.
    fn stop(&self);
}

/// What starts one.
pub trait Launcher: Send + Sync {
    /// Is the badge's executable there at all?
    ///
    /// **Read-only: it never spawns anything.** `hello` publishes this so the
    /// caller can gate the feature before it selects anything, and a handshake that
    /// showed a panel would be a handshake with a side effect.
    fn present(&self) -> bool;

    /// Start it, with the reader thread that applies its buttons.
    fn spawn(&self, buttons: Arc<dyn Buttons>) -> Result<Arc<dyn Child>, String>;
}

// --- the real child -----------------------------------------------------------

/// The indicator beside this executable inside the bundle.
pub struct Bundled {
    path: PathBuf,
}

impl Bundled {
    /// Resolved from the running executable's own directory, which is what makes
    /// this work from the bundle, from a Homebrew install and from a `cargo build`
    /// tree alike without any of them being a configured path.
    pub fn new() -> Bundled {
        let path = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(INDICATOR_BINARY)))
            .unwrap_or_else(|| PathBuf::from(INDICATOR_BINARY));

        Bundled { path }
    }
}

impl Default for Bundled {
    fn default() -> Self {
        Bundled::new()
    }
}

impl Launcher for Bundled {
    fn present(&self) -> bool {
        self.path.is_file()
    }

    fn spawn(&self, buttons: Arc<dyn Buttons>) -> Result<Arc<dyn Child>, String> {
        if !self.present() {
            return Err(format!(
                "{} is not beside this helper, so there is nothing to show the person that \
                 their window is being worked in",
                self.path.display()
            ));
        }

        let mut child = Command::new(&self.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("the indicator would not start: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "the indicator has no input".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "the indicator has no output".to_string())?;

        // The reader owns stdout for the child's whole life and ends when it does,
        // which is the one signal that the badge is gone.
        let reader = std::thread::Builder::new()
            .name("compux-indicator".to_string())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if let Some(pressed) = Pressed::parse(line.trim()) {
                        buttons.pressed(pressed);
                    }
                }
                buttons.ended();
            })
            .map_err(|e| format!("the indicator reader would not start: {e}"))?;

        Ok(Arc::new(Process {
            pid: child.id(),
            input: Mutex::new(Some(stdin)),
            child: Mutex::new(Some(child)),
            reader: Mutex::new(Some(reader)),
        }))
    }
}

/// The child, its pipe and its reader thread, in one place — so `stop` releases all
/// three and there is no path that releases one and leaks the others.
struct Process {
    pid: u32,
    input: Mutex<Option<std::process::ChildStdin>>,
    child: Mutex<Option<OsChild>>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// A child that is dropped without being stopped is a child nobody ever waits on —
/// a zombie for the life of the process, and a badge still on somebody's screen.
///
/// The paths that reach here are the ones `Supervisor::start` takes when it fails
/// AFTER spawning: the first state line the child would not take, or a watch thread
/// that would not start. Both drop the `Arc` and return an error, and without this
/// the spawn they had already done would never be reaped.
impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Child for Process {
    fn pid(&self) -> Option<i32> {
        i32::try_from(self.pid).ok()
    }

    fn write_line(&self, line: &str) -> Result<(), String> {
        let mut input = self.input.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pipe) = input.as_mut() else {
            return Err("the indicator is not running".to_string());
        };

        writeln!(pipe, "{line}")
            .and_then(|()| pipe.flush())
            .map_err(|e| format!("the indicator did not take its state: {e}"))
    }

    fn stop(&self) {
        // Closing the pipe is how it is asked to go; it exits on EOF.
        self.input.lock().unwrap_or_else(|e| e.into_inner()).take();

        let child = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut child) = child {
            // Waited on, so no zombie is left behind — and bounded, so a wedged
            // badge cannot hold a release open. A child that will not go on EOF is
            // killed and then reaped, because an unreaped kill is the same zombie.
            if !reaped(&mut child) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }

        let reader = self.reader.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(reader) = reader {
            // The pipe is closed and the child is gone, so this returns at once.
            let _ = reader.join();
        }
    }
}

/// Wait a bounded time for the child to exit of its own accord.
fn reaped(child: &mut OsChild) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(STOP_WAIT_MS);

    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_status)) => return true,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_gone) => return true,
        }
    }

    false
}

// --- supervision --------------------------------------------------------------

/// The window one badge is about, as the watch re-reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct Watching {
    pub window_id: u32,
    pub app: String,
    pub title: String,
}

/// The 4 Hz watch: one look at the window server, and a state line only when
/// something a person can see has changed.
///
/// `tick` is the whole of it and is pure but for the two seams it is given, so the
/// rules — what a moved window shows, what a covered one shows, what a minimized
/// one shows, and that an unchanged window writes nothing — are unit tests.
pub struct Watch {
    windows: Arc<dyn Windows>,
    child: Arc<dyn Child>,
    watching: Watching,
    /// Read for its pause flag, not written. The badge shows what the GATE is
    /// doing, so a pause from a control frame, from the badge's own button and from
    /// a release all reach it by one route instead of three.
    gate: Gate,
    last: Mutex<Option<State>>,
}

impl Watch {
    pub fn new(
        windows: Arc<dyn Windows>,
        child: Arc<dyn Child>,
        watching: Watching,
        gate: Gate,
    ) -> Watch {
        Watch {
            windows,
            child,
            watching,
            gate,
            last: Mutex::new(None),
        }
    }

    /// What the badge should show right now.
    pub fn state(&self) -> State {
        let showing = if self.gate.paused() {
            Showing::Paused
        } else {
            Showing::Working
        };

        let base = State {
            showing,
            app: self.watching.app.clone(),
            title: self.watching.title.clone(),
            bounds: None,
            occluded: false,
        };

        let Ok(listed) = self.windows.list() else {
            return State {
                showing: Showing::Unavailable,
                ..base
            };
        };

        let Some(window) = window_server::find(&listed, self.watching.window_id) else {
            return State {
                showing: Showing::Unavailable,
                ..base
            };
        };

        if !window.on_screen {
            return State {
                showing: Showing::Unavailable,
                ..base
            };
        }

        // Occluded is about the badge's whole possible AREA, not the window's own
        // corner: the badge has a size, one of its two placements is above the
        // window's frame entirely, and a hit test of one point inside the window
        // asks about somewhere the badge may never be drawn.
        //
        // The badge's own windows are excluded, because they are over that area by
        // design — and a window BEHIND the target does not cover it, which is what
        // `obstruction_in` walking front to back already means.
        let area = badge_area(&window.bounds);
        let covered = window_server::obstruction_in(&listed, window.id, &area, self.child.pid());

        match covered {
            Ok(covering) => State {
                bounds: Some(window.bounds),
                occluded: covering.is_some(),
                ..base
            },
            // The window went between the `find` above and here. Nothing to sit on.
            Err(window_server::TargetMissing) => State {
                showing: Showing::Unavailable,
                ..base
            },
        }
    }

    /// One look, and a line only if it says something new. Writing every 250 ms
    /// whatever happened would be this process talking to itself four times a
    /// second for the life of a target.
    pub fn tick(&self) -> Result<(), String> {
        let state = self.state();
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());

        if last.as_ref() == Some(&state) {
            return Ok(());
        }

        self.child.write_line(&state.line())?;
        *last = Some(state);
        Ok(())
    }

    /// Say it again whatever it says, for the moments a state is not a change but
    /// still has to reach the badge: selection, and a pause or resume.
    pub fn publish(&self) -> Result<(), String> {
        let state = self.state();
        self.child.write_line(&state.line())?;
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
        Ok(())
    }
}

/// Everything one bound target's badge needs, and the one `Drop` that ends it.
///
/// It owns the child, the reader thread (through the child), the watch thread and
/// the two flags the reader sets. Releasing a target drops this, which stops all
/// four — and so does replacing a target, losing one, and the process exiting.
pub struct Supervisor {
    child: Arc<dyn Child>,
    ready: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    ticker: Option<std::thread::JoinHandle<()>>,
}

impl Supervisor {
    /// Start the badge for a target, or say why there is none.
    ///
    /// A missing binary is `control_surface_unavailable` and never a silent skip:
    /// the person would then be worked around in their own window with nothing on
    /// screen saying so, which is the one outcome this slice exists to prevent.
    pub fn start(
        launcher: &Arc<dyn Launcher>,
        windows: Arc<dyn Windows>,
        gate: &Gate,
        emitter: &Emitter,
        clock: Arc<dyn Clock>,
        watching: Watching,
    ) -> Result<Supervisor, String> {
        let ready = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));

        let buttons: Arc<dyn Buttons> = Arc::new(GateButtons {
            gate: gate.clone(),
            emitter: emitter.clone(),
            ready: ready.clone(),
            alive: alive.clone(),
        });

        let child = launcher.spawn(buttons)?;
        let watch = Arc::new(Watch::new(windows, child.clone(), watching, gate.clone()));

        // From here on the child EXISTS, so every path out of this function stops
        // it. A start that fails after the spawn and simply returns would leave a
        // process nobody ever waits on and a badge on somebody's screen — and
        // `Drop for Process` is the structural backstop for the paths nobody wrote,
        // not a reason to leave this one implicit.
        //
        // The first line goes out before anything else, so the badge knows which
        // window it is about from the instant it comes up.
        if let Err(reason) = watch.publish() {
            child.stop();
            return Err(reason);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let (watch, stop) = (watch.clone(), stop.clone());
            let started = std::thread::Builder::new()
                .name("compux-indicator-watch".to_string())
                .spawn(move || {
                    // Bounded by the flag, and by nothing else: it ends with the
                    // target, and a write that fails means the badge is gone.
                    while !stop.load(Ordering::SeqCst) {
                        if watch.tick().is_err() {
                            break;
                        }
                        clock.sleep(WATCH_INTERVAL_MS);
                    }
                });

            match started {
                Ok(ticker) => ticker,
                Err(e) => {
                    child.stop();
                    return Err(format!("the indicator watch would not start: {e}"));
                }
            }
        };

        Ok(Supervisor {
            child,
            ready,
            alive,
            stop,
            ticker: Some(ticker),
        })
    }

    /// Has the badge said it is up, and is it still there?
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst) && self.alive.load(Ordering::SeqCst)
    }

    /// Has it exited? The one condition a restart is offered for.
    pub fn gone(&self) -> bool {
        !self.alive.load(Ordering::SeqCst)
    }
}

/// Every path: the watch stops, the child's input closes, the child is waited on,
/// and the reader thread joins. A release, a replacement, a target that went away
/// and the process exiting all come through here.
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The child first: a closed pipe fails the watch's next write, which is what
        // ends its loop without waiting out a sleep.
        self.child.stop();

        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
    }
}

// --- the scripted indicator (tests only) --------------------------------------

/// A badge a test writes down: it records every state line, answers presses on
/// demand, and counts its own starts and stops.
#[cfg(test)]
#[derive(Default)]
pub struct Scripted {
    lines: Mutex<Vec<String>>,
    stops: Mutex<usize>,
    running: Mutex<bool>,
    present: Mutex<bool>,
    spawns: Mutex<usize>,
    refuse: Mutex<Option<String>>,
    writes_fail: Mutex<bool>,
    buttons: Mutex<Option<Arc<dyn Buttons>>>,
}

#[cfg(test)]
impl Scripted {
    pub fn new() -> Arc<Scripted> {
        Arc::new(Scripted {
            present: Mutex::new(true),
            ..Scripted::default()
        })
    }

    /// The binary is not in the bundle.
    pub fn missing(&self) {
        *self.present.lock().unwrap() = false;
    }

    /// The next spawn fails.
    pub fn refuses(&self, reason: &str) {
        *self.refuse.lock().unwrap() = Some(reason.to_string());
    }

    /// The child comes up and then will not take a line — the shape of a badge
    /// that spawned and died between the spawn and the first write.
    pub fn takes_no_lines(&self) {
        *self.writes_fail.lock().unwrap() = true;
    }

    /// The child presses something, exactly as its stdout would deliver it.
    pub fn press(&self, pressed: Pressed) {
        let buttons = self.buttons.lock().unwrap().clone();
        if let Some(buttons) = buttons {
            buttons.pressed(pressed);
        }
    }

    /// The child exited: its reader thread would see EOF and say so.
    pub fn exits(&self) {
        let buttons = self.buttons.lock().unwrap().clone();
        if let Some(buttons) = buttons {
            buttons.ended();
        }
        *self.running.lock().unwrap() = false;
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    pub fn states(&self) -> Vec<Value> {
        self.lines()
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    pub fn stops(&self) -> usize {
        *self.stops.lock().unwrap()
    }

    pub fn spawns(&self) -> usize {
        *self.spawns.lock().unwrap()
    }

    pub fn running(&self) -> bool {
        *self.running.lock().unwrap()
    }
}

#[cfg(test)]
impl Launcher for Scripted {
    fn present(&self) -> bool {
        *self.present.lock().unwrap()
    }

    fn spawn(&self, buttons: Arc<dyn Buttons>) -> Result<Arc<dyn Child>, String> {
        if !self.present() {
            return Err("the indicator is not beside this helper".to_string());
        }
        if let Some(reason) = self.refuse.lock().unwrap().take() {
            return Err(reason);
        }

        *self.spawns.lock().unwrap() += 1;
        *self.running.lock().unwrap() = true;
        *self.buttons.lock().unwrap() = Some(buttons);

        Ok(Arc::new(ScriptedChild {
            owner: self as *const Scripted as usize,
            pid: SCRIPTED_CHILD_PID,
        }))
    }
}

/// The pid a scripted badge claims, so a test can put "the badge's own window" in a
/// desktop listing.
#[cfg(test)]
pub const SCRIPTED_CHILD_PID: i32 = 4242;

/// The scripted child writes into the launcher that made it. It holds the address
/// rather than an `Arc` because a `Scripted` outlives every child a test gives it
/// and a cycle would leak both.
#[cfg(test)]
struct ScriptedChild {
    owner: usize,
    pid: i32,
}

#[cfg(test)]
impl ScriptedChild {
    /// SAFETY: a test keeps its `Arc<Scripted>` alive for the whole test, which is
    /// the invariant the raw address rides on.
    fn owner(&self) -> &Scripted {
        unsafe { &*(self.owner as *const Scripted) }
    }
}

#[cfg(test)]
impl Child for ScriptedChild {
    fn pid(&self) -> Option<i32> {
        Some(self.pid)
    }

    fn write_line(&self, line: &str) -> Result<(), String> {
        let owner = self.owner();
        if !owner.running() || *owner.writes_fail.lock().unwrap() {
            return Err("the indicator is not running".to_string());
        }
        owner.lines.lock().unwrap().push(line.to_string());
        Ok(())
    }

    fn stop(&self) {
        let owner = self.owner();
        *owner.stops.lock().unwrap() += 1;
        *owner.running.lock().unwrap() = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::SystemClock;
    use crate::window_server::{window, Recorder};

    fn at(x: f64, y: f64, w: f64, h: f64) -> Bounds {
        Bounds { x, y, w, h }
    }

    fn test_gate() -> Gate {
        Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()))
    }

    fn watching() -> Watching {
        Watching {
            window_id: 20,
            app: "FixtureApp".to_string(),
            title: "Untitled".to_string(),
        }
    }

    fn desktop() -> Vec<crate::window_server::WindowFacts> {
        vec![
            window(10, 100, at(600.0, 600.0, 100.0, 60.0)),
            window(20, 200, at(0.0, 0.0, 800.0, 600.0)),
        ]
    }

    fn watch(
        listed: Vec<crate::window_server::WindowFacts>,
    ) -> (Arc<Watch>, Arc<Scripted>, Arc<Recorder>) {
        let windows = Recorder::new(listed);
        let scripted = Scripted::new();
        let child = scripted
            .spawn(Arc::new(Silent))
            .expect("the scripted child starts");
        let watch = Arc::new(Watch::new(windows.clone(), child, watching(), test_gate()));
        (watch, scripted, windows)
    }

    /// A `Buttons` that does nothing, for the tests that are about state lines.
    struct Silent;

    impl Buttons for Silent {
        fn pressed(&self, _pressed: Pressed) {}
        fn ended(&self) {}
    }

    #[test]
    fn the_first_look_says_where_the_window_is_and_that_it_is_working() {
        let (watch, scripted, _) = watch(desktop());

        watch.tick().expect("the child takes it");

        let states = scripted.states();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0]["state"], json!("working"));
        assert_eq!(states[0]["app"], json!("FixtureApp"));
        assert_eq!(states[0]["bounds"]["w"], json!(800.0));
        assert_eq!(states[0]["occluded"], json!(false));
    }

    // Four times a second for the life of a target is a lot of lines to say
    // nothing. Only a change is written.
    #[test]
    fn an_unchanged_window_writes_nothing_after_the_first_line() {
        let (watch, scripted, _) = watch(desktop());

        for _ in 0..5 {
            watch.tick().unwrap();
        }

        assert_eq!(scripted.lines().len(), 1);
    }

    #[test]
    fn a_window_that_moved_writes_its_new_bounds() {
        let (watch, scripted, windows) = watch(desktop());
        watch.tick().unwrap();

        windows.change(20, |window| window.bounds.x = 400.0);
        watch.tick().unwrap();

        let states = scripted.states();
        assert_eq!(states.len(), 2);
        assert_eq!(states[1]["bounds"]["x"], json!(400.0));
    }

    // The badge has a size and two possible placements, one of them ABOVE the
    // window's frame. Something in front of the target anywhere in that area means
    // there is nowhere to put it, and the child hides it rather than drawing on
    // whatever is there.
    #[test]
    fn anything_over_the_badges_area_is_reported_as_occluded() {
        let (watch, scripted, windows) = watch(desktop());
        watch.tick().unwrap();

        windows.change(10, |window| window.bounds = at(0.0, 0.0, 200.0, 100.0));
        watch.tick().unwrap();

        assert_eq!(scripted.states()[1]["occluded"], json!(true));
    }

    // The placement the window's own corner cannot see: a window covering the strip
    // ABOVE the target, where the outside placement floats.
    #[test]
    fn a_window_over_the_strip_above_the_target_occludes_it_too() {
        let (watch, scripted, windows) = watch(desktop());
        watch.tick().unwrap();

        // Entirely above the target's top edge, so no point inside the window is
        // covered at all — and the badge still has nowhere to go.
        windows.change(10, |window| window.bounds = at(0.0, -30.0, 200.0, 20.0));
        watch.tick().unwrap();

        assert_eq!(scripted.states()[1]["occluded"], json!(true));
    }

    // The badge sits over that area by design. Counting it would have the badge
    // report the window it is about as covered by itself, and then hide.
    #[test]
    fn the_badges_own_window_is_never_what_covers_the_target() {
        let mut listed = desktop();
        listed.insert(
            0,
            window(90, SCRIPTED_CHILD_PID, at(0.0, -34.0, 448.0, 28.0)),
        );
        let (watch, scripted, _) = watch(listed);

        watch.tick().unwrap();

        assert_eq!(scripted.states()[0]["occluded"], json!(false));
    }

    // A window BEHIND the target is under the badge as well as under the window,
    // however much of the area it contains.
    #[test]
    fn a_window_behind_the_target_does_not_occlude_the_badge() {
        let mut listed = desktop();
        listed.push(window(30, 300, at(0.0, -100.0, 1920.0, 1080.0)));
        let (watch, scripted, _) = watch(listed);

        watch.tick().unwrap();

        assert_eq!(scripted.states()[0]["occluded"], json!(false));
    }

    // The contract the Swift side is written against, pinned so a change here is a
    // change somebody has to make there too.
    #[test]
    fn the_badges_area_covers_both_placements_from_the_windows_corner() {
        let area = badge_area(&at(400.0, 300.0, 800.0, 600.0));

        assert_eq!(area.x, 400.0);
        assert_eq!(area.y, 300.0 - BADGE_ABOVE);
        assert_eq!(area.w, BADGE_WIDTH);
        assert_eq!(area.h, BADGE_ABOVE + BADGE_INSIDE_DROP + BADGE_HEIGHT);
    }

    // Minimized, gone, or a window server that will not answer: three ways for the
    // badge to have nothing to sit on, and all three say so rather than leaving it
    // where the window used to be.
    #[test]
    fn a_window_that_cannot_be_seen_says_unavailable_with_no_bounds() {
        for break_it in [
            |windows: &Recorder| windows.change(20, |window| window.on_screen = false),
            |windows: &Recorder| windows.remove(20),
            |windows: &Recorder| windows.refuses("the window server refused"),
        ] {
            let (watch, scripted, windows) = watch(desktop());
            watch.tick().unwrap();

            break_it(&windows);
            watch.tick().unwrap();

            let states = scripted.states();
            assert_eq!(states[1]["state"], json!("unavailable"));
            assert_eq!(states[1]["bounds"], Value::Null);
        }
    }

    #[test]
    fn a_pause_shows_on_the_badge() {
        let windows = Recorder::new(desktop());
        let scripted = Scripted::new();
        let child = scripted.spawn(Arc::new(Silent)).unwrap();
        let gate = test_gate();
        let watch = Watch::new(windows, child, watching(), gate.clone());

        watch.tick().unwrap();
        gate.control(wire::ControlAction::Pause, None);
        watch.tick().unwrap();

        let states = scripted.states();
        assert_eq!(states[0]["state"], json!("working"));
        assert_eq!(states[1]["state"], json!("paused"));
    }

    // The whole point of the badge: its buttons reach the gate directly, with no
    // daemon in the path, and the report follows.
    #[test]
    fn a_press_flips_the_gate_before_anything_is_reported() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let emitter = Emitter::capturing();
        let ready = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));

        let buttons = GateButtons {
            gate: gate.clone(),
            emitter: emitter.clone(),
            ready: ready.clone(),
            alive,
        };

        buttons.pressed(Pressed::Ready);
        assert!(ready.load(Ordering::SeqCst));
        assert!(
            emitter.captured().is_empty(),
            "ready is not the owner's news"
        );

        buttons.pressed(Pressed::Pause);
        assert_eq!(gate.checkpoint(), Err(crate::gate::Refusal::Cancelled));

        let frames = emitter.captured();
        assert_eq!(frames[0]["type"], json!("session_event"));
        assert_eq!(frames[0]["kind"], json!("indicator"));
        assert_eq!(frames[0]["event"], json!("operator_pause"));
        // The authority the gate minted when it installed the barrier: without it
        // the caller's next request is refused and it cannot tell why.
        assert_eq!(frames[0]["authorization_generation"], json!(2));

        // A resume lifts the barrier. The cancel flag is cleared on the next
        // admission, as it always has been, so what a resume proves HERE is that
        // the pause is no longer installed.
        buttons.pressed(Pressed::Resume);
        assert!(!gate.paused());
        assert_eq!(emitter.captured()[1]["event"], json!("operator_resume"));
    }

    // A Stop cannot end the session from here — that is the owner's — but it CAN
    // see to it that nothing else is dispatched while the owner is being told.
    #[test]
    fn a_stop_installs_the_barrier_and_says_so() {
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));
        let emitter = Emitter::capturing();

        GateButtons {
            gate: gate.clone(),
            emitter: emitter.clone(),
            ready: Arc::new(AtomicBool::new(true)),
            alive: Arc::new(AtomicBool::new(true)),
        }
        .pressed(Pressed::Stop);

        assert_eq!(gate.checkpoint(), Err(crate::gate::Refusal::Cancelled));
        assert_eq!(emitter.captured()[0]["event"], json!("operator_stop"));
    }

    #[test]
    fn every_button_the_child_can_press_is_understood_and_nothing_else_is() {
        for (line, expected) in [
            (r#"{"event":"ready"}"#, Some(Pressed::Ready)),
            (r#"{"event":"pause"}"#, Some(Pressed::Pause)),
            (r#"{"event":"resume"}"#, Some(Pressed::Resume)),
            (r#"{"event":"stop"}"#, Some(Pressed::Stop)),
            (r#"{"event":"explode"}"#, None),
            (r#"{"state":"working"}"#, None),
            ("not json", None),
        ] {
            assert_eq!(Pressed::parse(line), expected, "{line}");
        }
    }

    #[test]
    fn a_state_line_is_exactly_the_five_fields_the_child_reads() {
        let line = State {
            showing: Showing::Working,
            app: "FixtureApp".to_string(),
            title: "Untitled".to_string(),
            bounds: Some(at(1.0, 2.0, 3.0, 4.0)),
            occluded: false,
        }
        .line();

        let parsed: Value = serde_json::from_str(&line).expect("one JSON object");
        let fields: Vec<&String> = parsed.as_object().unwrap().keys().collect();
        assert_eq!(fields, vec!["app", "bounds", "occluded", "state", "title"]);
    }

    #[test]
    fn a_missing_binary_is_a_refusal_and_never_a_silent_skip() {
        let scripted = Scripted::new();
        scripted.missing();

        assert!(!scripted.present());
        assert!(scripted.spawn(Arc::new(Silent)).is_err());
        assert_eq!(scripted.spawns(), 0);
    }

    // The whole of supervision, end to end: a badge that comes up, is written to,
    // and is stopped — with its child reaped and its watch thread joined — when the
    // target it belongs to goes.
    #[test]
    fn a_supervised_badge_starts_says_it_is_up_and_stops_with_its_target() {
        let launcher: Arc<dyn Launcher> = Scripted::new();
        let scripted = Scripted::new();
        let windows: Arc<dyn Windows> = Recorder::new(desktop());
        let gate = test_gate();
        let emitter = Emitter::capturing();

        let supervisor = Supervisor::start(
            &(scripted.clone() as Arc<dyn Launcher>),
            windows,
            &gate,
            &emitter,
            Arc::new(SystemClock::new()),
            watching(),
        )
        .expect("the scripted badge starts");

        assert_eq!(scripted.spawns(), 1);
        assert!(!supervisor.ready(), "nothing is ready before it says so");
        assert_eq!(
            scripted.states()[0]["state"],
            json!("working"),
            "the first line goes out before anything else"
        );

        scripted.press(Pressed::Ready);
        assert!(supervisor.ready());
        assert!(!supervisor.gone());

        scripted.exits();
        assert!(!supervisor.ready(), "a badge that exited is not up");
        assert!(supervisor.gone());

        drop(supervisor);
        assert!(scripted.stops() >= 1, "the child is stopped and reaped");
        assert!(!scripted.running());
        let _ = launcher;
    }

    // A start that failed leaves nothing behind, which is what makes
    // `control_surface_unavailable` a fact rather than a guess.
    #[test]
    fn a_badge_that_will_not_start_leaves_nothing_running() {
        let scripted = Scripted::new();
        scripted.refuses("the indicator would not start");

        let started = Supervisor::start(
            &(scripted.clone() as Arc<dyn Launcher>),
            Recorder::new(desktop()),
            &test_gate(),
            &Emitter::capturing(),
            Arc::new(SystemClock::new()),
            watching(),
        );

        assert!(started.is_err());
        assert_eq!(scripted.spawns(), 0);
        assert!(!scripted.running());
    }

    // A start that fails AFTER the spawn is the dangerous one: the child exists,
    // nobody holds it, and without this it is never waited on — a zombie for the
    // life of the process and a badge left on somebody's screen.
    #[test]
    fn a_badge_that_came_up_and_then_failed_the_start_is_stopped_anyway() {
        let scripted = Scripted::new();

        let launcher: Arc<dyn Launcher> = scripted.clone();
        let spawned = launcher.spawn(Arc::new(Silent)).expect("it comes up");
        assert_eq!(scripted.spawns(), 1);
        drop(spawned);

        // Now the real path: it spawns, and the first state line will not go.
        let scripted = Scripted::new();
        scripted.takes_no_lines();

        let started = Supervisor::start(
            &(scripted.clone() as Arc<dyn Launcher>),
            Recorder::new(desktop()),
            &test_gate(),
            &Emitter::capturing(),
            Arc::new(SystemClock::new()),
            watching(),
        );

        assert!(started.is_err(), "a badge that takes no lines is no badge");
        assert_eq!(scripted.spawns(), 1, "and it really did spawn one");
        assert_eq!(scripted.stops(), 1, "so the spawn is stopped and reaped");
    }

    #[test]
    fn a_child_that_exited_is_no_longer_ready() {
        let ready = Arc::new(AtomicBool::new(true));
        let alive = Arc::new(AtomicBool::new(true));
        let buttons = GateButtons {
            gate: Gate::new("boot-1".to_string(), Arc::new(SystemClock::new())),
            emitter: Emitter::capturing(),
            ready: ready.clone(),
            alive: alive.clone(),
        };

        buttons.ended();

        assert!(!ready.load(Ordering::SeqCst));
        assert!(!alive.load(Ordering::SeqCst));
    }
}
