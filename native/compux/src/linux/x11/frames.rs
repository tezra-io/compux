//! A bound window's frames on X11, through Composite: the window is redirected
//! (`RedirectWindow`, Automatic, so the server still draws it on the screen), its
//! off-screen pixmap is named (`NameWindowPixmap`), and that pixmap is read. A
//! window another window covers still has its own pixmap, so it is still seen.
//!
//! **What proves the pixmap is the window's.** Redirecting a window gives it a
//! new pixmap, and the server fills it with what the screen showed there, which
//! under another window is that window's pixels. A pixmap a compositing manager
//! redirected long ago may hold the same. So no pixel is sent until drawing after
//! a cut-off has reached it: damage is reported rectangle by rectangle from
//! before the redirect, the redirect's own report (the copy) comes first and is
//! set aside, everything reported up to then is set aside, and then every window
//! the picture is drawn from (the window manager's frame and its decorations, the
//! client and the client's own windows) is cleared and sent an Expose. The
//! server repaints what has a background, and the application and the window
//! manager draw the rest; that drawing must reach every pixel of the picture,
//! frame and GTK's shadow included, within [`REPAINT_WAIT_MS`], or the binding is
//! refused. Nothing in the proof depends on which windows were over it, so a
//! window moving meanwhile changes nothing. A window resized, hidden or shown
//! during the proof voids it, and so does one read in the drain at the cut-off:
//! the window is renewed from the moment its pixmap is named. A window mapped
//! again, resized or given a new border width gets a new pixmap, and is proved
//! again the same way before anything more is published, and so is one whose
//! new size shows first in the geometry a read is given; a proof that falls
//! short then is retried at each of the next [`MAX_REPROOF_RETRIES`] intervals,
//! and the binding ends if none holds. While a window waits to be proved again,
//! no frame of it is served: the one held is of the window before it changed.
//! Any part of a picture the proof did not reach is sent black.
//!
//! **What a frame carries.** The window manager's frame and GTK's shadow are
//! accounted for exactly as the window list accounts for them, so a picture is
//! the window's listed bounds, pixel for pixel. The pixmap holds the window's X
//! border too; the picture is read from inside it. Every [`FRAME_INTERVAL_MS`] the
//! server is synced: a window it reported damage on is read again, and one it
//! did not is an idle sample, stamped when the sync was sent, which is the
//! window server saying nothing changed up to then.
//!
//! **The binding's generation.** The stream's connection watches the window
//! from the moment it is bound. A window destroyed under that watch, or a watch
//! whose connection ended, is gone for good: an X window id is reused, so the
//! window that holds the id later is another window, and the stream answers
//! [`FrameError::Gone`] for the rest of the binding.
//!
//! **The bind is bounded.** The window is watched, redirected and proved, and
//! its first frame read, on the watch's own thread, where every request waits
//! on the server for as long as it takes. The start waits for that at most
//! [`START_WAIT_MS`]; past it nothing is bound, and the watch's socket is shut,
//! which ends any wait on the server it is in.
//!
//! Everything the server holds for the stream (the redirect, the damage, the
//! pixmap, the event selections) belongs to its connection, and goes when the
//! connection closes, on every path out.

use std::net::Shutdown;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat, ImageOrder,
    MapState, Window as XWindow, WindowClass,
};
use x11rb::protocol::{ErrorKind, Event};
use x11rb::rust_connection::RustConnection;
use x11rb::x11_utils::X11Error;

use super::strips::{self, Format};
use super::windows::{extents, placed, Rect};
use crate::gate::Clock;
use crate::linux::display::Env;
use crate::window_frames::{
    Fence, FrameError, Health, Sample, Slot, StreamWindow, WindowFrame, WindowFrames,
    BYTES_PER_PIXEL, MAX_FRAMES_PER_SECOND,
};

/// How often the server is synced and a damaged window read again.
pub const FRAME_INTERVAL_MS: u64 = 1_000 / MAX_FRAMES_PER_SECOND as u64;

/// How long the redirect's own damage report is waited for.
pub const REDIRECT_WAIT_MS: u64 = 200;

/// How long the window's repaint after a redirect is waited for.
pub const REPAINT_WAIT_MS: u64 = 1_000;

/// The most windows a bound window's picture may be drawn from (its frame, the
/// frame's decorations, the client and the client's own windows) for the proof
/// to ask each to repaint. A window holding more is refused.
const MAX_WINDOWS: usize = 1_024;

/// The deepest that tree is walked.
const MAX_TREE_DEPTH: usize = 16;

/// The most undrawn pieces a proof follows. Drawing scattered finely enough to
/// split the picture past this is refused rather than followed.
const MAX_PIECES: usize = 4_096;

/// The pause between two looks while a proof waits.
const PROOF_POLL_MS: u64 = 2;

/// How many times in a row a window already bound is proved again at the next
/// interval after a proof that fell short, before the binding ends.
pub const MAX_REPROOF_RETRIES: u32 = 3;

/// How long `start` waits for the watch to bind the window: to watch it,
/// redirect it, prove its picture and read its first frame. That covers the
/// redirect's report and the repaint, with room for a slow server.
pub const START_WAIT_MS: u64 = 3_000;

/// How long a request waits for the watch to catch up with the server. A watch
/// mid-proof answers once the proof is done, so this covers the redirect's report
/// and the window's repaint together.
const CATCH_UP_WAIT_MS: u64 = 2_000;

/// How long `stop` waits for the watch's thread to end. It ends within one
/// event poll; this covers a server that stopped answering mid-read.
const STOP_WAIT_MS: u64 = 2_000;

/// The most events read in one look. Past this the window is drowning the watch,
/// and the stream says so rather than reading forever.
const MAX_EVENTS: usize = 4_096;

/// The most parents walked from a client to the window manager's frame.
const MAX_FRAME_DEPTH: usize = 8;

/// What one frame interval does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The window is not viewable: nothing to show.
    Hidden,
    /// The window was mapped again or resized, or no proof holds for it: it has
    /// a pixmap to prove.
    Prove,
    /// The server reported damage: read the window again.
    Read,
    /// Nothing changed since the last read.
    Idle,
}

/// The step for what the sync found.
pub fn step(viewable: bool, was_hidden: bool, renewed: bool, damaged: bool) -> Step {
    if !viewable {
        Step::Hidden
    } else if was_hidden || renewed {
        Step::Prove
    } else if damaged {
        Step::Read
    } else {
        Step::Idle
    }
}

/// What a proof came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proved {
    /// Drawing after the cut-off reached every pixel of this picture.
    Whole(Rect),
    /// The window was resized, mapped or unmapped after its pixmap was named,
    /// so it has, or will have, a pixmap the proof says nothing of.
    Void,
    /// This many pieces of the picture were not drawn in time.
    Short(usize),
}

/// A proof at its cut-off, once every report the server sent before it is
/// read. `renewed` is whether any event read since the pixmap was named
/// renewed the window, those reports included: a pixmap replaced then is not
/// the one being proved, so the proof is void there and nothing is asked to
/// repaint. `None` goes on to the repaint. A resize the server reported before
/// the pixmap was named, and read only after, voids a proof whose pixmap is
/// already the new one: that errs on the safe side, and the proof is tried
/// again.
pub fn at_cut_off(renewed: bool) -> Option<Proved> {
    renewed.then_some(Proved::Void)
}

/// A proof once its wait for drawing is over, with `left` pieces of `crop`
/// undrawn.
pub fn after_drawing(crop: Rect, renewed: bool, left: usize) -> Proved {
    if renewed {
        Proved::Void
    } else if left == 0 {
        Proved::Whole(crop)
    } else {
        Proved::Short(left)
    }
}

/// What an interval does after proving a bound window again, `retries` being
/// how many proofs in a row fell short before this one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reproved {
    /// Proved: read it, and the count of retries starts again.
    Read(Rect),
    /// Not proved: nothing is read, and the next interval proves it again.
    Retry { retries: u32 },
    /// Short after every retry: the binding ends, `left` pieces undrawn.
    End { left: usize },
}

/// A void proof is retried without counting: the window is still changing. A
/// short one is retried at the next interval, [`MAX_REPROOF_RETRIES`] times in
/// a row, and then the binding ends.
pub fn reproved(proved: Proved, retries: u32) -> Reproved {
    match proved {
        Proved::Whole(crop) => Reproved::Read(crop),
        Proved::Void => Reproved::Retry { retries },
        Proved::Short(left) if retries >= MAX_REPROOF_RETRIES => Reproved::End { left },
        Proved::Short(_) => Reproved::Retry {
            retries: retries + 1,
        },
    }
}

/// What an X error says of the clears the last proof sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnClear {
    /// It answers another request.
    NotAClear,
    /// The window was gone before it was cleared: it needs no repaint, and the
    /// proof still waits for drawing over every pixel.
    Gone,
    /// The server would not clear a window of the picture: the proof ends.
    Refused,
}

/// Whether the error the server sent for request `sequence` answers one of
/// `clears` (the sequence numbers of the clears the last proof sent), and what
/// it says of it.
pub fn on_clear(clears: &[u64], sequence: u64, kind: ErrorKind) -> OnClear {
    if !clears.contains(&sequence) {
        return OnClear::NotAClear;
    }
    match kind {
        ErrorKind::Window | ErrorKind::Drawable => OnClear::Gone,
        _refused => OnClear::Refused,
    }
}

/// What of `pieces` the damage rectangle `drawn` has not reached: each piece
/// less the rectangle, at most four pieces for one. Damage is taken rectangle by
/// rectangle, never as a bounding box, so pixels between two drawn places stay
/// unproven.
pub fn undrawn(pieces: Vec<Rect>, drawn: Rect) -> Vec<Rect> {
    pieces
        .into_iter()
        .flat_map(|piece| outside(piece, drawn))
        .collect()
}

/// Every pixel of a frame cut at `crop` that lies outside `proven` (all of them
/// when nothing is proven) is sent black and opaque: a pixel nothing showed to be
/// the window's own is never sent as it was read.
pub fn black_out(pixels: &mut [u8], crop: Rect, proven: Option<Rect>) {
    let pieces = match proven {
        Some(proven) => outside(crop, proven),
        None => vec![crop],
    };
    let width = crop.width as usize;
    for piece in pieces {
        let (left, top) = ((piece.x - crop.x) as usize, (piece.y - crop.y) as usize);
        for row in top..top + piece.height as usize {
            let start = (row * width + left) * BYTES_PER_PIXEL;
            let end = start + piece.width as usize * BYTES_PER_PIXEL;
            let (black, _) = pixels[start..end].as_chunks_mut::<BYTES_PER_PIXEL>();
            black.fill([0, 0, 0, 0xff]);
        }
    }
}

/// Where two rectangles share pixels, if anywhere.
fn overlap(a: Rect, b: Rect) -> Option<Rect> {
    let (left, top) = (a.x.max(b.x), a.y.max(b.y));
    let right = right_of(a).min(right_of(b));
    let bottom = bottom_of(a).min(bottom_of(b));
    if right <= i64::from(left) || bottom <= i64::from(top) {
        return None;
    }
    Some(Rect {
        x: left,
        y: top,
        width: u32::try_from(right - i64::from(left)).ok()?,
        height: u32::try_from(bottom - i64::from(top)).ok()?,
    })
}

/// The parts of `rect` outside `bounds`: at most four bands, above, below, left
/// and right of the part inside.
fn outside(rect: Rect, bounds: Rect) -> Vec<Rect> {
    let Some(inner) = overlap(rect, bounds) else {
        return vec![rect];
    };
    let band = |x: i64, y: i64, right: i64, bottom: i64| -> Option<Rect> {
        Some(Rect {
            x: i32::try_from(x).ok()?,
            y: i32::try_from(y).ok()?,
            width: u32::try_from(right - x).ok().filter(|width| *width > 0)?,
            height: u32::try_from(bottom - y)
                .ok()
                .filter(|height| *height > 0)?,
        })
    };
    let (left, top) = (i64::from(rect.x), i64::from(rect.y));
    let (in_left, in_top) = (i64::from(inner.x), i64::from(inner.y));
    let (in_right, in_bottom) = (right_of(inner), bottom_of(inner));
    [
        band(left, top, right_of(rect), in_top),
        band(left, in_bottom, right_of(rect), bottom_of(rect)),
        band(left, in_top, in_left, in_bottom),
        band(in_right, in_top, right_of(rect), in_bottom),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn right_of(rect: Rect) -> i64 {
    i64::from(rect.x) + i64::from(rect.width)
}

fn bottom_of(rect: Rect) -> i64 {
    i64::from(rect.y) + i64::from(rect.height)
}

/// Is `crop` inside a drawable of this size?
pub fn inside(crop: Rect, width: u16, height: u16) -> bool {
    let right = i64::from(crop.x) + i64::from(crop.width);
    let bottom = i64::from(crop.y) + i64::from(crop.height);
    crop.x >= 0 && crop.y >= 0 && right <= i64::from(width) && bottom <= i64::from(height)
}

/// Where `crop`, in the window's own pixels, lies in the window's Composite
/// pixmap. The pixmap holds the window's X border as well, so the window's own
/// first pixel is `border` pixels in from the pixmap's corner. Refused past
/// any coordinate a pixmap can have.
pub fn in_pixmap(crop: Rect, border: u16) -> Result<Rect, FrameError> {
    let moved = |at: i32| {
        at.checked_add(i32::from(border))
            .ok_or_else(|| off_the_pixmap(crop, border))
    };
    Ok(Rect {
        x: moved(crop.x)?,
        y: moved(crop.y)?,
        ..crop
    })
}

/// RGBA rows as the BGRA a window frame carries, in place.
pub fn bgra(mut rgba: Vec<u8>) -> Vec<u8> {
    let (pixels, _) = rgba.as_chunks_mut::<BYTES_PER_PIXEL>();
    for pixel in pixels {
        pixel.swap(0, 2);
    }
    rgba
}

// --- the stream -------------------------------------------------------------

/// One bound window's frames, read by a watch thread of its own.
pub struct Composite {
    env: Env,
    clock: Arc<dyn Clock>,
    running: Mutex<Option<Running>>,
}

impl Composite {
    pub fn new(env: Env, clock: Arc<dyn Clock>) -> Composite {
        Composite {
            env,
            clock,
            running: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl WindowFrames for Composite {
    /// The window is watched, redirected and proved, and its first frame read,
    /// on the watch's own thread, where every request waits on the server for
    /// as long as it takes; this waits for it at most [`START_WAIT_MS`]. Past
    /// that, or for a window that cannot be proved, the start fails and nothing
    /// is left running: the watch's socket is shut, which ends any wait on the
    /// server it is in.
    fn start(&self, window: StreamWindow) -> Result<(), FrameError> {
        self.stop();
        let socket = Arc::new(Socket::default());
        let (running, started) =
            Running::spawn(self.env.clone(), window, self.clock.clone(), socket.clone())?;
        match opened_within(&started, Duration::from_millis(START_WAIT_MS)) {
            Ok(()) => {
                *self.lock() = Some(running);
                Ok(())
            }
            Err(fault) => {
                socket.shut();
                running.end();
                Err(fault)
            }
        }
    }

    /// The window can be closed, and another made under its id, between two
    /// intervals: the watch catches up with the server first, so a window
    /// destroyed before this request is never answered with its last frame.
    fn latest(&self, after: Option<Fence>) -> Result<WindowFrame, FrameError> {
        let running = self.lock();
        let Some(running) = running.as_ref() else {
            return Err(FrameError::Unavailable(
                "no window capture is running".to_string(),
            ));
        };
        served(
            running.catch_up(),
            running.slot.peek(after, self.clock.now_ns()),
        )
    }

    fn stop(&self) {
        let running = self.lock().take();
        if let Some(running) = running {
            let slot = running.slot.clone();
            running.end();
            // A released target holds no pixels, even where its watch outlives
            // the stop.
            slot.clear();
        }
    }
}

impl Drop for Composite {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the watch says once it has opened the window: its first frame is in the
/// watch's slot, or why there is none.
type Opened = Result<(), FrameError>;

/// What the watch said of the window within `wait`: it is bound, or why not.
fn opened_within(started: &Receiver<Opened>, wait: Duration) -> Opened {
    match started.recv_timeout(wait) {
        Ok(opened) => opened,
        Err(RecvTimeoutError::Timeout) => Err(FrameError::Unavailable(format!(
            "the X server did not let the window be bound within {} ms (watched, its picture \
             proved and its first frame read), so nothing was bound; select it again",
            wait.as_millis()
        ))),
        Err(RecvTimeoutError::Disconnected) => Err(FrameError::Unavailable(
            "the window capture's watch ended before it said whether the window could be bound"
                .to_string(),
        )),
    }
}

/// A second handle on the watch's socket, held while a start waits for the
/// watch: a start past its bound shuts it, which ends whatever wait on the
/// server the watch is in, and the server then lets go of everything it holds
/// for the watch.
#[derive(Default)]
struct Socket(Mutex<Option<OwnedFd>>);

impl Socket {
    fn keep(&self, connection: &RustConnection) -> Result<(), FrameError> {
        let fd = connection
            .stream()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| {
                FrameError::Unavailable(format!(
                    "the window capture could not hold its X connection's socket: {error}"
                ))
            })?;
        *self.lock() = Some(fd);
        Ok(())
    }

    /// Shut it, both ways. One not held yet (the watch is still connecting) is
    /// left alone: that watch ends by itself once it finds nobody waiting.
    fn shut(&self) {
        let Some(fd) = self.lock().take() else {
            return;
        };
        // shutdown(2) acts on the socket whatever its family; `UnixStream` is
        // only the std wrapper that offers it.
        if let Err(error) = UnixStream::from(fd).shutdown(Shutdown::Both) {
            eprintln!("compux: the window capture could not shut its X connection: {error}");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<OwnedFd>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What the watch said when it was asked to catch up with the server before a
/// frame is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Caught {
    /// It synced its connection and read every report sent before the sync.
    Up,
    /// It caught up, and the window is waiting to be proved again: it was
    /// resized, shown again or given a new border, or its last proof fell short.
    Reproving,
    /// It is not running: it ended on a fault the slot holds, or without one.
    Ended,
    /// It did not answer within [`CATCH_UP_WAIT_MS`].
    Late,
}

/// Where the watch stands once it has caught up: a window renewed since its
/// pixmap was named, or with no proof holding, waits to be proved again.
fn standing(renewed: bool, proven: Option<Rect>) -> Caught {
    if renewed || proven.is_none() {
        Caught::Reproving
    } else {
        Caught::Up
    }
}

/// What the slot holds, as far as the watch's catching up lets it be served.
/// While the window waits to be proved again, the frame held is of the window
/// before it changed, and is not served.
fn served(
    caught: Caught,
    peeked: Result<WindowFrame, FrameError>,
) -> Result<WindowFrame, FrameError> {
    match (caught, peeked) {
        (Caught::Up, peeked) => peeked,
        (Caught::Reproving, Ok(_before)) => Err(FrameError::Reproving(
            "the window is being proved again after it changed size, was shown again or got a new \
             border, or its last proof fell short, and nothing of it is sent until a proof holds; \
             retry in a moment"
                .to_string(),
        )),
        (Caught::Reproving, refused) => refused,
        (Caught::Late, _) => Err(FrameError::Unavailable(format!(
            "the window capture did not catch up with the X server within {CATCH_UP_WAIT_MS} ms"
        ))),
        (Caught::Ended, Err(fault)) if !matches!(fault, FrameError::NotYet) => Err(fault),
        // Nothing it holds is known to be the window now, and nothing more will come.
        (Caught::Ended, _) => Err(FrameError::Gone(
            "the window capture's watch ended; select the window again".to_string(),
        )),
    }
}

/// What the watch's thread is asked to do between two intervals.
enum Command {
    /// Sync, read every report the server sent before the sync, and say where
    /// the watch stands.
    CatchUp(Sender<Caught>),
    Stop,
}

/// The watch's thread, the two channels that command it and confirm its end,
/// and the slot it publishes to. Each start has a slot of its own, so a watch
/// that outlives the start that gave up on it publishes nowhere anyone reads.
struct Running {
    commands: Sender<Command>,
    ended: Receiver<()>,
    thread: std::thread::JoinHandle<()>,
    slot: Arc<Slot>,
}

impl Running {
    /// The watch's thread: it binds the window ([`Watch::first`]), says so on
    /// the receiver returned, and then takes over.
    fn spawn(
        env: Env,
        window: StreamWindow,
        clock: Arc<dyn Clock>,
        socket: Arc<Socket>,
    ) -> Result<(Running, Receiver<Opened>), FrameError> {
        let slot = Slot::new();
        let (commands, commanded) = mpsc::channel();
        let (finished, ended) = mpsc::channel();
        let (opened, started) = mpsc::channel();
        let publishing = slot.clone();
        let thread = std::thread::Builder::new()
            .name("compux-window-frames".to_string())
            .spawn(move || {
                let first = Watch::first(&env, &window, &publishing, &*clock, &socket, &opened);
                drop(socket);
                if let Some(watch) = first {
                    watch.run(&publishing, &*clock, &commanded);
                }
                // Nobody waiting for this is a stop that already gave up.
                finished.send(()).ok();
            })
            .map_err(|error| {
                FrameError::Unavailable(format!("the window capture would not start: {error}"))
            })?;
        let running = Running {
            commands,
            ended,
            thread,
            slot,
        };
        Ok((running, started))
    }

    /// Ask the watch to catch up, and wait for it, bounded. A watch that already
    /// ended has dropped the request, which drops its answer.
    fn catch_up(&self) -> Caught {
        let (answer, answered) = mpsc::channel();
        if self.commands.send(Command::CatchUp(answer)).is_err() {
            return Caught::Ended;
        }
        match answered.recv_timeout(Duration::from_millis(CATCH_UP_WAIT_MS)) {
            Ok(caught) => caught,
            Err(RecvTimeoutError::Disconnected) => Caught::Ended,
            Err(RecvTimeoutError::Timeout) => Caught::Late,
        }
    }

    /// Ask the watch to end, and wait for it, bounded. A watch stuck on a
    /// server that stopped answering is left to end when its connection does.
    fn end(self) {
        // A watch that already ended has dropped its receiver, which is the same
        // answer as this one.
        self.commands.send(Command::Stop).ok();
        match self.ended.recv_timeout(Duration::from_millis(STOP_WAIT_MS)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if self.thread.join().is_err() {
                    eprintln!("compux: the window capture's watch panicked");
                }
            }
            Err(RecvTimeoutError::Timeout) => eprintln!(
                "compux: the window capture's watch did not end within {STOP_WAIT_MS} ms; it ends \
                 with its X connection"
            ),
        }
    }
}

// --- the watch ----------------------------------------------------------------

/// One window, redirected and watched on a connection of its own.
struct Watch {
    connection: RustConnection,
    client: XWindow,
    /// The window manager's frame around the client, or the client itself where
    /// it has none: what is redirected, and what the picture is cut from.
    top: XWindow,
    damage: u32,
    pixmap: u32,
    /// The part of the window, in its own pixels, that drawing after the last
    /// proof's cut-off reached: all a frame may show. `None` while no proof
    /// holds.
    proven: Option<Rect>,
    format: Format,
    frame_atom: u32,
    shadow_atom: u32,
    /// The redirected window's width, height and X border: the pixmap holds all
    /// three, so a change to any gives the window a new one.
    size: (u16, u16, u16),
    damaged: bool,
    /// Resized, given a new border width, mapped or unmapped since its pixmap
    /// was named: the window has, or will have, a new pixmap that a proof of the
    /// named one says nothing of.
    renewed: bool,
    hidden: bool,
    /// Proofs in a row that fell short since one last held.
    retries: u32,
    /// The sequence numbers of the clears the last proof sent, whose errors come
    /// back as events.
    clears: Vec<u64>,
}

impl Watch {
    /// The listed window bound: watched, redirected, proved and its first frame
    /// put in `slot`, which is said on `opened`. The watch is returned where the
    /// start still waits for it; one that stopped waiting dropped the receiver,
    /// and the watch then ends here, its connection with it.
    fn first(
        env: &Env,
        listed: &StreamWindow,
        slot: &Slot,
        clock: &dyn Clock,
        socket: &Socket,
        opened: &Sender<Opened>,
    ) -> Option<Watch> {
        let bound = Watch::open(env, listed, socket).and_then(|mut watch| {
            match watch.picture(slot, clock.now_mach(), clock)? {
                Some(frame) => {
                    slot.offer(frame);
                    Ok(watch)
                }
                None => Err(unproven(
                    "it was resized while it was being bound".to_string(),
                )),
            }
        });
        match bound {
            Ok(watch) => opened.send(Ok(())).ok().map(|()| watch),
            Err(fault) => {
                opened.send(Err(fault)).ok();
                None
            }
        }
    }

    /// The listed window, watched from here on: a window destroyed after this
    /// is caught by the watch, and one replaced before it by its process, read
    /// again on the watch's own connection.
    fn open(env: &Env, listed: &StreamWindow, socket: &Socket) -> Result<Watch, FrameError> {
        let client = listed.id;
        let (connection, _) = super::connect(env).map_err(unavailable)?;
        socket.keep(&connection)?;
        versions(&connection)?;
        let top = top_level(&connection, client)?;
        watch_structure(&connection, client, top)?;
        let pid = super::windows::owner(&connection, client).map_err(unavailable)?;
        if !super::windows::still_named(listed.pid, pid) {
            return Err(FrameError::Replaced(
                "the window with that id is no longer the one `windows` listed (another process \
                 holds it now), so nothing was bound; take `windows` again and select it"
                    .to_string(),
            ));
        }
        let geometry = connection
            .get_geometry(top)
            .map_err(lost)?
            .reply()
            .map_err(|error| reply_error(error, "the window"))?;
        let format = pixel_format(&connection, geometry.depth)?;
        let (frame_atom, shadow_atom) = (
            atom(&connection, "_NET_FRAME_EXTENTS")?,
            atom(&connection, "_GTK_FRAME_EXTENTS")?,
        );
        let mut watch = Watch {
            connection,
            client,
            top,
            damage: 0,
            pixmap: 0,
            proven: None,
            format,
            frame_atom,
            shadow_atom,
            size: (geometry.width, geometry.height, geometry.border_width),
            damaged: true,
            renewed: false,
            hidden: false,
            retries: 0,
            clears: Vec::new(),
        };
        if !watch.viewable()? {
            return Err(FrameError::Minimized(
                "the window is minimized or on another desktop, so there is nothing to see in it"
                    .to_string(),
            ));
        }
        watch.redirect()?;
        Ok(watch)
    }

    /// Damage reports first, then the redirect, then the pixmap's name, then the
    /// proof: each step is the one before it holding. A window a compositing
    /// manager redirected before is not redirected again, and is proved all the
    /// same.
    fn redirect(&mut self) -> Result<(), FrameError> {
        let connection = &self.connection;
        self.damage = connection.generate_id().map_err(no_id)?;
        connection
            .damage_create(self.damage, self.top, ReportLevel::DELTA_RECTANGLES)
            .map_err(lost)?
            .check()
            .map_err(|error| reply_error(error, "damage reports on the window"))?;
        if !already_redirected(connection, self.top)? {
            connection
                .composite_redirect_window(self.top, Redirect::AUTOMATIC)
                .map_err(lost)?
                .check()
                .map_err(|error| reply_error(error, "redirecting the window"))?;
            self.redirected()?;
        }
        self.name()?;
        self.proven = Some(match self.prove()? {
            Proved::Whole(crop) => crop,
            Proved::Void => {
                return Err(unproven(
                    "it was resized, hidden or shown while it was being proved".to_string(),
                ))
            }
            Proved::Short(left) => return Err(unproven(short(left))),
        });
        Ok(())
    }

    /// The redirect's own report: the server copied what the screen showed into
    /// the window's new pixmap, and reports that as drawing. The copy proves
    /// nothing of the window; the report proves its damage is being reported.
    fn redirected(&mut self) -> Result<(), FrameError> {
        if self.report_within(REDIRECT_WAIT_MS)? {
            return Ok(());
        }
        Err(unproven(format!(
            "the X server reported no drawing within {REDIRECT_WAIT_MS} ms of redirecting it"
        )))
    }

    /// That the window's picture is its own, whatever was over it: everything
    /// reported so far is set aside, every window the picture is drawn from is
    /// cleared and asked to repaint, and the drawing after that must reach every
    /// pixel of the picture (the window manager's frame and GTK's shadow as much
    /// as the client) within [`REPAINT_WAIT_MS`]. Void when the window was
    /// renewed after its pixmap was named, whether a report read at the cut-off
    /// says so or one read during the repaint.
    fn prove(&mut self) -> Result<Proved, FrameError> {
        self.repaired()?;
        if let Some(void) = at_cut_off(self.renewed) {
            return Ok(void);
        }
        let crop = self.crop()?;
        self.expose()?;
        let left = self.repaint_within(REPAINT_WAIT_MS, crop)?;
        Ok(after_drawing(crop, self.renewed, left.len()))
    }

    /// Every window the picture is drawn from is cleared to its background and
    /// sent an Expose for all of it: the server paints what has a background, and
    /// the application or the window manager draws the rest, as after any
    /// exposure. The clears are not checked one by one. x11rb's `check()` sends
    /// no sync once a packet carrying the checked request's own sequence number
    /// has been read (any event the server sends after processing it does), and
    /// then waits for a packet with a later number, which only a later request
    /// brings: none follows the last clear, so the wait would never end. A
    /// refusal comes back as an event instead, matched to its clear by sequence
    /// number ([`Watch::took`]), before the drawing that follows. A window gone
    /// meanwhile needs nothing.
    fn expose(&mut self) -> Result<(), FrameError> {
        let windows = self.drawn_from()?;
        let connection = &self.connection;
        self.clears.clear();
        let mut asked = Vec::with_capacity(windows.len());
        for window in windows {
            asked.push((
                window,
                connection.get_window_attributes(window).map_err(lost)?,
            ));
        }
        for (window, attributes) in asked {
            let Some(attributes) = still(attributes.reply())? else {
                continue;
            };
            // An input-only window has no pixels, and clearing one is an error.
            if attributes.class == WindowClass::INPUT_ONLY {
                continue;
            }
            // Dropped unchecked: its error, if any, is delivered as an event.
            let cleared = connection
                .clear_area(true, window, 0, 0, 0, 0)
                .map_err(lost)?;
            self.clears.push(cleared.sequence_number());
        }
        connection.flush().map_err(lost)
    }

    /// The top-level window and every window inside it, a level at a time.
    fn drawn_from(&self) -> Result<Vec<XWindow>, FrameError> {
        let mut found = vec![self.top];
        let mut level = vec![self.top];
        for _ in 0..MAX_TREE_DEPTH {
            let mut asked = Vec::with_capacity(level.len());
            for window in &level {
                asked.push(self.connection.query_tree(*window).map_err(lost)?);
            }
            level = Vec::new();
            for tree in asked {
                level.extend(
                    still(tree.reply())?
                        .map(|tree| tree.children)
                        .unwrap_or_default(),
                );
            }
            found.extend(&level);
            if found.len() > MAX_WINDOWS {
                return Err(unproven(format!(
                    "it is drawn from more than {MAX_WINDOWS} windows"
                )));
            }
            if level.is_empty() {
                return Ok(found);
            }
        }
        Err(unproven(format!(
            "its windows nest deeper than {MAX_TREE_DEPTH}"
        )))
    }

    /// The pixmap the window is drawn into now, under a name of this watch's.
    /// Whatever renewed the window before this is what gave it that pixmap;
    /// anything read from here on renews it again.
    fn name(&mut self) -> Result<(), FrameError> {
        self.renewed = false;
        if self.pixmap != 0 {
            self.connection.free_pixmap(self.pixmap).map_err(lost)?;
        }
        self.pixmap = self.connection.generate_id().map_err(no_id)?;
        self.connection
            .composite_name_window_pixmap(self.top, self.pixmap)
            .map_err(lost)?
            .check()
            .map_err(|error| reply_error(error, "naming the window's pixmap"))
    }

    /// Read every event waiting, noting what they say.
    fn drain(&mut self) -> Result<(), FrameError> {
        for _ in 0..MAX_EVENTS {
            if self.next_damage()?.is_none() {
                return Ok(());
            }
        }
        Err(drowned())
    }

    /// Wait up to `wait_ms` for any damage report at all.
    fn report_within(&mut self, wait_ms: u64) -> Result<bool, FrameError> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        for _ in 0..MAX_EVENTS {
            if self.next_damage()?.is_some() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(PROOF_POLL_MS));
        }
        Err(drowned())
    }

    /// Wait up to `wait_ms` for drawing over every pixel of `crop`: what is still
    /// undrawn at the deadline, or as soon as the window is renewed.
    fn repaint_within(&mut self, wait_ms: u64, crop: Rect) -> Result<Vec<Rect>, FrameError> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut left = vec![crop];
        for _ in 0..MAX_EVENTS {
            if left.is_empty() || self.renewed {
                return Ok(left);
            }
            if left.len() > MAX_PIECES {
                return Err(unproven(format!(
                    "its drawing split the picture into more than {MAX_PIECES} pieces"
                )));
            }
            match self.next_damage()? {
                Some(drawn) => left = undrawn(left, drawn),
                None if Instant::now() >= deadline => return Ok(left),
                None => std::thread::sleep(Duration::from_millis(PROOF_POLL_MS)),
            }
        }
        Err(drowned())
    }

    /// The next damage report waiting, after any other events before it.
    fn next_damage(&mut self) -> Result<Option<Rect>, FrameError> {
        for _ in 0..MAX_EVENTS {
            let Some((event, sequence)) = self
                .connection
                .poll_for_event_with_sequence()
                .map_err(lost)?
            else {
                return Ok(None);
            };
            if let Some(drawn) = self.took(event, sequence)? {
                return Ok(Some(drawn));
            }
        }
        Err(drowned())
    }

    /// Everything the server reported so far is repaired, and the reports in
    /// flight are read and set aside: what comes after is new.
    fn repaired(&mut self) -> Result<(), FrameError> {
        self.connection
            .damage_subtract(self.damage, 0u32, 0u32)
            .map_err(lost)?;
        self.sync()?;
        self.drain()?;
        self.damaged = false;
        Ok(())
    }

    /// One event, the server's `sequence` number on it: a damage report on this
    /// window is returned, in the window's pixels, a change to the window is
    /// noted, and a window gone ends the stream.
    fn took(&mut self, event: Event, sequence: u64) -> Result<Option<Rect>, FrameError> {
        let ours = |window: XWindow| window == self.top || window == self.client;
        match event {
            Event::DamageNotify(report) if report.damage == self.damage => {
                self.damaged = true;
                let area = report.area;
                return Ok(Some(Rect {
                    x: i32::from(area.x),
                    y: i32::from(area.y),
                    width: u32::from(area.width),
                    height: u32::from(area.height),
                }));
            }
            Event::DestroyNotify(gone) if gone.window == self.client => return Err(destroyed()),
            Event::DestroyNotify(gone) if gone.window == self.top => return Err(reframed()),
            Event::ReparentNotify(moved) if moved.window == self.client => return Err(reframed()),
            Event::ConfigureNotify(change) if change.window == self.top => {
                let size = (change.width, change.height, change.border_width);
                self.renewed |= size != self.size;
                self.size = size;
            }
            // Mapped again, a window is given a new pixmap.
            Event::MapNotify(shown) if ours(shown.window) => self.renewed = true,
            Event::UnmapNotify(hidden) if ours(hidden.window) => self.renewed = true,
            Event::Error(error) => return self.refused(&error, sequence),
            _other => {}
        }
        Ok(None)
    }

    /// An error the server sent: one of the last proof's clears refused ends the
    /// proof, unless its window was gone before it was cleared, which needs no
    /// repaint. Each is logged.
    fn refused(&self, error: &X11Error, sequence: u64) -> Result<Option<Rect>, FrameError> {
        let refused = super::refused(error);
        match on_clear(&self.clears, sequence, error.error_kind) {
            OnClear::Refused => Err(unproven(format!(
                "the X server would not ask a window of it to repaint ({refused})"
            ))),
            OnClear::Gone => {
                eprintln!(
                    "compux: the window capture: a window of the bound one was gone before it \
                     could be asked to repaint ({refused})"
                );
                Ok(None)
            }
            OnClear::NotAClear => {
                eprintln!("compux: the window capture: {refused}");
                Ok(None)
            }
        }
    }

    /// One frame interval after another, catching up whenever a request asks,
    /// until the stop arrives or a fault ends the stream. The fault is the
    /// slot's to answer, and it is there before the request that found it is
    /// answered. Requests never put an interval off: the next one is due at its
    /// time however many arrive before it.
    fn run(mut self, slot: &Slot, clock: &dyn Clock, commands: &Receiver<Command>) {
        let interval = Duration::from_millis(FRAME_INTERVAL_MS);
        let mut due = Instant::now() + interval;
        loop {
            let (outcome, answer) =
                match commands.recv_timeout(due.saturating_duration_since(Instant::now())) {
                    Err(RecvTimeoutError::Timeout) => {
                        due = Instant::now() + interval;
                        (self.tick(slot, clock), None)
                    }
                    Ok(Command::CatchUp(answer)) => (self.catch_up(), Some(answer)),
                    Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                };
            let ended = outcome.is_err();
            if let Err(fault) = outcome {
                slot.fault(fault);
            }
            // Nobody waiting for the answer is a request that already gave up.
            if let Some(answer) = answer {
                answer.send(standing(self.renewed, self.proven)).ok();
            }
            if ended {
                return;
            }
        }
    }

    /// Read everything the server sent before now: a window destroyed before
    /// this ends the stream here, not at the next interval.
    fn catch_up(&mut self) -> Result<(), FrameError> {
        self.sync()?;
        self.drain()
    }

    /// Sync, read what the server said before it, and take the one step that
    /// calls for. The stamp is taken before the sync, so whatever is read after
    /// it is the window as it was then or later.
    fn tick(&mut self, slot: &Slot, clock: &dyn Clock) -> Result<(), FrameError> {
        let stamp = clock.now_mach();
        let viewable = self.viewable()?;
        self.drain()?;
        // A window no proof holds for is proved before it is read.
        let renewed = standing(self.renewed, self.proven) == Caught::Reproving;
        match step(viewable, self.hidden, renewed, self.damaged) {
            Step::Hidden => {
                self.hidden = true;
                slot.note(sample(slot, clock, stamp, Health::Minimized));
                Ok(())
            }
            Step::Prove => self.prove_again(slot, stamp, clock),
            Step::Read => self.read(slot, stamp, clock),
            Step::Idle => {
                slot.note(sample(slot, clock, stamp, Health::Idle));
                Ok(())
            }
        }
    }

    /// A window mapped again or resized has a new pixmap, and one whose last
    /// proof did not hold has none proved: it is named, proved as the first one
    /// was, and read once that holds. Nothing is read while it does not. A proof
    /// the window voided by changing again is retried at the next interval; one
    /// that fell short is retried [`MAX_REPROOF_RETRIES`] times in a row, and
    /// then the binding ends.
    fn prove_again(
        &mut self,
        slot: &Slot,
        stamp: u64,
        clock: &dyn Clock,
    ) -> Result<(), FrameError> {
        self.name()?;
        let proved = self.prove()?;
        self.hidden = false;
        match reproved(proved, self.retries) {
            Reproved::Read(crop) => {
                self.proven = Some(crop);
                self.retries = 0;
                self.damaged = true;
                self.read(slot, stamp, clock)
            }
            Reproved::Retry { retries } => {
                self.proven = None;
                self.retries = retries;
                Ok(())
            }
            Reproved::End { left } => Err(unproven(format!(
                "{}, {} times in a row",
                short(left),
                MAX_REPROOF_RETRIES + 1
            ))),
        }
    }

    /// Read the window as it is now, and publish it stamped `stamp`.
    fn read(&mut self, slot: &Slot, stamp: u64, clock: &dyn Clock) -> Result<(), FrameError> {
        if let Some(frame) = self.picture(slot, stamp, clock)? {
            slot.offer(frame);
        }
        Ok(())
    }

    /// The window as it is now, stamped `stamp`. `None` where the server gives
    /// it a size other than the one its pixmap was named at: it was resized
    /// after the interval's look, so nothing is read from a pixmap it no longer
    /// draws into, and it is renewed, still damaged, to be proved again.
    fn picture(
        &mut self,
        slot: &Slot,
        stamp: u64,
        clock: &dyn Clock,
    ) -> Result<Option<WindowFrame>, FrameError> {
        let (crop, size) = self.places()?;
        if size != self.size {
            self.renewed = true;
            return Ok(None);
        }
        self.connection
            .damage_subtract(self.damage, 0u32, 0u32)
            .map_err(lost)?;
        self.damaged = false;
        let mut pixels = self.grab(in_pixmap(crop, size.2)?)?;
        // The frame extents can move the picture within the window without a new
        // pixmap; whatever of it the proof did not reach is not sent.
        black_out(&mut pixels, crop, self.proven);
        let width = crop.width;
        Ok(Some(WindowFrame {
            pixels,
            width,
            height: crop.height,
            stride: width as usize * BYTES_PER_PIXEL,
            captured_at_ns: clock.now_ns(),
            display_time_mach: Some(stamp),
            seq: slot.next_seq(),
            content_rect: None,
            reported_scale: None,
            status: Health::Live,
        }))
    }

    /// Where the listed window is inside the redirected one, in its pixels.
    fn crop(&self) -> Result<Rect, FrameError> {
        Ok(self.places()?.0)
    }

    /// Where the listed window (the client's place, less GTK's shadow, with the
    /// window manager's frame) is inside the redirected one, in its pixels, and
    /// the redirected window's width, height and X border, from the same
    /// reading.
    fn places(&self) -> Result<(Rect, (u16, u16, u16)), FrameError> {
        let connection = &self.connection;
        let outer = connection.get_geometry(self.top).map_err(lost)?;
        let client = connection.get_geometry(self.client).map_err(lost)?;
        let at = connection
            .translate_coordinates(self.client, self.top, 0, 0)
            .map_err(lost)?;
        let frame = self.margins(self.frame_atom)?;
        let shadow = self.margins(self.shadow_atom)?;
        let outer = outer
            .reply()
            .map_err(|error| reply_error(error, "the window"))?;
        let client = client
            .reply()
            .map_err(|error| reply_error(error, "the window"))?;
        let at = at
            .reply()
            .map_err(|error| reply_error(error, "the window"))?;
        let inner = Rect {
            x: i32::from(at.dst_x),
            y: i32::from(at.dst_y),
            width: u32::from(client.width),
            height: u32::from(client.height),
        };
        let size = (outer.width, outer.height, outer.border_width);
        let crop = placed(inner, frame.reply_extents()?, shadow.reply_extents()?)
            .map(|(seen, _)| (seen, size))
            .filter(|(seen, _)| inside(*seen, outer.width, outer.height));
        crop.ok_or_else(|| {
            FrameError::Unavailable(format!(
                "the window's frame extents put it outside the {}x{} window the window manager \
                 draws it in, so no picture of it can be cut from that one",
                outer.width, outer.height
            ))
        })
    }

    fn margins(&self, atom: u32) -> Result<Margins<'_>, FrameError> {
        self.connection
            .get_property(false, self.client, atom, AtomEnum::CARDINAL, 0, 4)
            .map(Margins)
            .map_err(lost)
    }

    /// The pixmap's pixels under `crop`, in the pixmap's own pixels, in strips,
    /// as BGRA.
    fn grab(&self, crop: Rect) -> Result<Vec<u8>, FrameError> {
        let width = u16::try_from(crop.width).map_err(|_| too_wide(crop))?;
        let height = u16::try_from(crop.height).map_err(|_| too_wide(crop))?;
        let (left, top) = (
            i16::try_from(crop.x).map_err(|_| too_wide(crop))?,
            i16::try_from(crop.y).map_err(|_| too_wide(crop))?,
        );
        WindowFrame::budget(usize::from(width) * BYTES_PER_PIXEL, u32::from(height))?;
        let rows = strips::strip_rows(width, height, self.connection.maximum_request_bytes());
        let mut rgba = Vec::with_capacity(usize::from(width) * usize::from(height) * 4);
        for (first, count) in strips::strips(height, rows) {
            let y = top
                .checked_add(i16::try_from(first).map_err(|_| too_wide(crop))?)
                .ok_or_else(|| too_wide(crop))?;
            let reply = self
                .connection
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    self.pixmap,
                    left,
                    y,
                    width,
                    count,
                    u32::MAX,
                )
                .map_err(lost)?
                .reply()
                .map_err(|error| reply_error(error, "reading the window's pixmap"))?;
            strips::to_rgba(
                &reply.data,
                usize::from(width),
                usize::from(count),
                self.format,
                &mut rgba,
            )
            .map_err(FrameError::Unavailable)?;
        }
        Ok(bgra(rgba))
    }

    /// Whether the client is viewable now. The reply is also the sync: every
    /// event the server sent before it has arrived.
    fn viewable(&self) -> Result<bool, FrameError> {
        let attributes = self
            .connection
            .get_window_attributes(self.client)
            .map_err(lost)?
            .reply()
            .map_err(|error| reply_error(error, "the window"))?;
        Ok(attributes.map_state == MapState::VIEWABLE)
    }

    fn sync(&self) -> Result<(), FrameError> {
        self.connection
            .get_input_focus()
            .map_err(lost)?
            .reply()
            .map_err(|error| reply_error(error, "the X display"))?;
        Ok(())
    }
}

/// A frame-extents property in flight.
struct Margins<'c>(
    x11rb::cookie::Cookie<'c, RustConnection, x11rb::protocol::xproto::GetPropertyReply>,
);

impl Margins<'_> {
    /// No property is no margins; one that is not four sane values is refused.
    fn reply_extents(self) -> Result<super::windows::Extents, FrameError> {
        let reply = self
            .0
            .reply()
            .map_err(|error| reply_error(error, "the window"))?;
        let values: Vec<u32> = reply.value32().map(Iterator::collect).unwrap_or_default();
        if values.is_empty() {
            return Ok(super::windows::Extents::default());
        }
        extents(&values).ok_or_else(|| {
            FrameError::Unavailable(format!(
                "the window's frame extents read {values:?}, not four margins, so where the \
                 window is inside its frame cannot be told"
            ))
        })
    }
}

/// A sample with no pixels, stamped on both clocks.
fn sample(slot: &Slot, clock: &dyn Clock, stamp: u64, status: Health) -> Sample {
    Sample {
        status,
        seq: slot.next_seq(),
        captured_at_ns: clock.now_ns(),
        display_time_mach: Some(stamp),
    }
}

/// Composite 0.4 and Damage 1.1, which every X server a desktop runs has.
fn versions(connection: &RustConnection) -> Result<(), FrameError> {
    connection
        .composite_query_version(0, 4)
        .map_err(lost)?
        .reply()
        .map_err(|error| reply_error(error, "the Composite extension"))?;
    connection
        .damage_query_version(1, 1)
        .map_err(lost)?
        .reply()
        .map_err(|error| reply_error(error, "the Damage extension"))?;
    Ok(())
}

/// The client's top-level window: the window manager's frame around it, or the
/// client itself where nothing frames it.
fn top_level(connection: &RustConnection, client: XWindow) -> Result<XWindow, FrameError> {
    let mut window = client;
    for _ in 0..MAX_FRAME_DEPTH {
        let tree = connection
            .query_tree(window)
            .map_err(lost)?
            .reply()
            .map_err(|error| reply_error(error, "the window"))?;
        if tree.parent == tree.root || tree.parent == 0 {
            return Ok(window);
        }
        window = tree.parent;
    }
    Err(FrameError::Unavailable(format!(
        "the window sits more than {MAX_FRAME_DEPTH} windows below the root, so its frame cannot \
         be found"
    )))
}

/// Ask the server for both windows' structure events: a destroy, a new parent,
/// a size.
fn watch_structure(
    connection: &RustConnection,
    client: XWindow,
    top: XWindow,
) -> Result<(), FrameError> {
    let watching = ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY);
    for window in [client, top] {
        connection
            .change_window_attributes(window, &watching)
            .map_err(lost)?
            .check()
            .map_err(|error| reply_error(error, "watching the window"))?;
    }
    Ok(())
}

/// Whether something (a compositing manager) already redirected the window:
/// only a redirected window's pixmap can be named.
fn already_redirected(connection: &RustConnection, top: XWindow) -> Result<bool, FrameError> {
    let probe = connection.generate_id().map_err(no_id)?;
    let named = connection
        .composite_name_window_pixmap(top, probe)
        .map_err(lost)?
        .check();
    match named {
        Ok(()) => {
            connection.free_pixmap(probe).map_err(lost)?;
            Ok(true)
        }
        Err(ReplyError::X11Error(error)) if error.error_kind == ErrorKind::Match => Ok(false),
        Err(error) => Err(reply_error(error, "the window's pixmap")),
    }
}

fn atom(connection: &RustConnection, name: &str) -> Result<u32, FrameError> {
    connection
        .intern_atom(false, name.as_bytes())
        .map_err(lost)?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| reply_error(error, name))
}

/// How a drawable of this depth stores a pixel.
fn pixel_format(connection: &RustConnection, depth: u8) -> Result<Format, FrameError> {
    let setup = connection.setup();
    let bits_per_pixel = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == depth)
        .map(|format| format.bits_per_pixel)
        .ok_or_else(|| {
            FrameError::Unavailable(format!("the X display has no format for depth {depth}"))
        })?;
    Ok(Format {
        bits_per_pixel,
        msb_first: setup.image_byte_order == ImageOrder::MSB_FIRST,
    })
}

fn unavailable(why: String) -> FrameError {
    FrameError::Unavailable(format!("the window capture: {why}"))
}

/// The connection itself failed: the watch that bound the window is over.
fn lost(error: ConnectionError) -> FrameError {
    FrameError::Gone(format!(
        "the X connection that watched the window ended ({error}), so the window there now \
         cannot be proved to be the one bound; take `windows` and select it again"
    ))
}

/// No id for a new resource: the connection ended, or this connection has used
/// every id the server gave it.
fn no_id(error: ReplyOrIdError) -> FrameError {
    match error {
        ReplyOrIdError::ConnectionError(error) => lost(error),
        other => FrameError::Unavailable(format!(
            "the window capture could not name a new resource: {other}"
        )),
    }
}

/// A request the server refused. A window it no longer has is gone.
fn reply_error(error: ReplyError, what: &str) -> FrameError {
    match error {
        ReplyError::ConnectionError(error) => lost(error),
        ReplyError::X11Error(error) if error.error_kind == ErrorKind::Window => destroyed(),
        ReplyError::X11Error(error) => FrameError::Unavailable(format!(
            "the window capture, at {what}: {}",
            super::refused(&error)
        )),
    }
}

fn destroyed() -> FrameError {
    FrameError::Gone(
        "the window was closed after it was bound; an X window id is reused, so a window that \
         has the id now is another window. Take `windows` and select again"
            .to_string(),
    )
}

fn reframed() -> FrameError {
    FrameError::Unavailable(
        "the window manager took the window out of the frame it was bound in; select it again"
            .to_string(),
    )
}

/// A reply, or `None` for a window that is gone; any other failure is the
/// connection's.
fn still<T>(reply: Result<T, ReplyError>) -> Result<Option<T>, FrameError> {
    match reply {
        Ok(reply) => Ok(Some(reply)),
        Err(ReplyError::X11Error(error))
            if matches!(error.error_kind, ErrorKind::Window | ErrorKind::Drawable) =>
        {
            Ok(None)
        }
        Err(error) => Err(reply_error(error, "a window its picture is drawn from")),
    }
}

fn unproven(why: String) -> FrameError {
    FrameError::Unavailable(format!(
        "the window's own picture could not be proved: {why}, so what compux holds of it could \
         still be what the screen showed there; select it again"
    ))
}

/// A proof that fell short, in words.
fn short(left: usize) -> String {
    format!(
        "{left} part(s) of its picture were not drawn within {REPAINT_WAIT_MS} ms of asking it to \
         repaint"
    )
}

fn drowned() -> FrameError {
    FrameError::Unavailable(format!(
        "more than {MAX_EVENTS} events arrived at once for the window, so the capture stopped \
         reading them"
    ))
}

fn off_the_pixmap(crop: Rect, border: u16) -> FrameError {
    FrameError::Unavailable(format!(
        "the window's picture at ({}, {}) inside a {border}-pixel border is past any pixmap \
         coordinate",
        crop.x, crop.y
    ))
}

fn too_wide(crop: Rect) -> FrameError {
    FrameError::Unavailable(format!(
        "the window is {}x{} at ({}, {}), past what one X request can read",
        crop.width, crop.height, crop.x, crop.y
    ))
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

    // Not viewable comes first whatever else happened, a window that came back
    // or changed size is proved before it is read, and only damage reads it.
    #[test]
    fn each_interval_does_the_one_step_what_the_sync_found_calls_for() {
        assert_eq!(step(false, false, true, true), Step::Hidden);
        assert_eq!(step(true, true, false, false), Step::Prove);
        assert_eq!(step(true, false, true, false), Step::Prove);
        assert_eq!(step(true, false, false, true), Step::Read);
        assert_eq!(step(true, false, false, false), Step::Idle);
    }

    // The drawing must reach every pixel of the picture, edge to edge; drawing
    // short by one row proves nothing about that row.
    #[test]
    fn drawing_proves_a_picture_only_to_its_last_pixel() {
        let crop = rect(0, 0, 982, 795);
        assert!(undrawn(vec![crop], rect(0, 0, 982, 795)).is_empty());
        assert_eq!(
            undrawn(vec![crop], rect(0, 0, 982, 794)),
            vec![rect(0, 794, 982, 1)]
        );
        assert_eq!(
            undrawn(vec![crop], rect(1, 0, 981, 795)),
            vec![rect(0, 0, 1, 795)]
        );
    }

    #[test]
    fn a_picture_is_cut_only_from_inside_its_drawable() {
        assert!(inside(rect(0, 0, 982, 795), 982, 795));
        assert!(inside(rect(20, 14, 800, 600), 840, 640));
        assert!(!inside(rect(0, 0, 983, 795), 982, 795));
        assert!(!inside(rect(-1, 0, 10, 10), 982, 795));
        assert!(!inside(rect(0, 790, 10, 10), 982, 795));
    }

    // The drain at the cut-off reads every event the server sent before it, a
    // resize, map or unmap among them: one read there replaced the pixmap that
    // was named, and a proof that went on would prove the new one and leave the
    // old one, the redirect's copy of the screen, to be read.
    #[test]
    fn a_window_renewed_in_the_cut_offs_drain_voids_the_proof() {
        assert_eq!(at_cut_off(true), Some(Proved::Void));
        assert_eq!(at_cut_off(false), None);
    }

    #[test]
    fn a_proof_holds_only_for_a_pixmap_unrenewed_and_wholly_drawn() {
        let crop = rect(0, 0, 982, 795);
        assert_eq!(after_drawing(crop, false, 0), Proved::Whole(crop));
        assert_eq!(after_drawing(crop, true, 0), Proved::Void);
        assert_eq!(after_drawing(crop, false, 3), Proved::Short(3));
        assert_eq!(after_drawing(crop, true, 3), Proved::Void);
    }

    #[test]
    fn a_short_reproof_is_retried_three_times_then_ends_the_binding() {
        let crop = rect(0, 0, 982, 795);
        assert_eq!(
            reproved(Proved::Short(2), 0),
            Reproved::Retry { retries: 1 }
        );
        assert_eq!(
            reproved(Proved::Short(2), 2),
            Reproved::Retry { retries: 3 }
        );
        assert_eq!(reproved(Proved::Short(2), 3), Reproved::End { left: 2 });
        // A window still changing is not counted against it, and a proof that
        // holds starts the count again.
        assert_eq!(reproved(Proved::Void, 3), Reproved::Retry { retries: 3 });
        assert_eq!(reproved(Proved::Whole(crop), 3), Reproved::Read(crop));
    }

    // Damage and the frame extents count from the window's own corner, inside its
    // X border; its pixmap counts from the border's outer corner.
    #[test]
    fn a_picture_is_read_from_inside_the_windows_border() {
        let crop = rect(6, 30, 800, 600);
        assert_eq!(in_pixmap(crop, 0).ok(), Some(crop));
        assert_eq!(in_pixmap(crop, 8).ok(), Some(rect(14, 38, 800, 600)));
        // An offset no pixmap coordinate can hold says so, in its own words.
        let refused = in_pixmap(rect(i32::MAX, 0, 1, 1), 1).expect_err("past any coordinate");
        assert!(
            refused.detail().contains("past any pixmap coordinate"),
            "{}",
            refused.detail()
        );
    }

    fn frame() -> WindowFrame {
        WindowFrame {
            pixels: vec![0; 4],
            width: 1,
            height: 1,
            stride: 4,
            captured_at_ns: 1,
            display_time_mach: Some(1),
            seq: 1,
            content_rect: None,
            reported_scale: None,
            status: Health::Live,
        }
    }

    // The window can be closed and another made under its id between two
    // intervals, so a frame is served only once the watch has read everything the
    // server sent it up to the request.
    #[test]
    fn a_watch_that_caught_up_serves_what_the_slot_holds() {
        assert_eq!(served(Caught::Up, Ok(frame())).expect("served").seq, 1);
        let closed = FrameError::Gone("the window was closed after it was bound".to_string());
        let refused = served(Caught::Up, Err(closed)).expect_err("gone");
        assert_eq!(refused.code(), "target_unavailable");
    }

    // A window resized, shown again or given a new border is proved again before
    // anything more of it is sent, and so is one whose last proof fell short: the
    // frame held is the window before the change, at its old size.
    #[test]
    fn a_pending_re_proof_serves_nothing() {
        let refused = served(Caught::Reproving, Ok(frame())).expect_err("the held frame");
        assert_eq!(refused.code(), "capture_unavailable");
        let detail = refused.detail();
        assert!(
            detail.contains("proved again") && detail.contains("retry"),
            "{detail}"
        );
        // A look fenced after an action keeps waiting for the frame the proof reads.
        assert!(matches!(
            served(Caught::Reproving, Err(FrameError::NotYet)),
            Err(FrameError::NotYet)
        ));
        let closed = FrameError::Gone("the window was closed after it was bound".to_string());
        let refused = served(Caught::Reproving, Err(closed)).expect_err("gone");
        assert_eq!(refused.code(), "target_unavailable");
    }

    // A clear is sent unchecked, and its error, if any, comes back as an event: it
    // is matched to the clear by the request's own sequence number.
    #[test]
    fn an_error_is_a_clears_only_by_its_sequence_number() {
        let clears = [70_001, 70_002, 70_004];
        assert_eq!(on_clear(&clears, 70_002, ErrorKind::Window), OnClear::Gone);
        assert_eq!(
            on_clear(&clears, 70_004, ErrorKind::Drawable),
            OnClear::Gone
        );
        assert_eq!(
            on_clear(&clears, 70_001, ErrorKind::Match),
            OnClear::Refused
        );
        assert_eq!(
            on_clear(&clears, 70_003, ErrorKind::Window),
            OnClear::NotAClear
        );
        // The same low 16 bits, 65536 requests later, is another request.
        assert_eq!(
            on_clear(&clears, 70_002 + 65_536, ErrorKind::Value),
            OnClear::NotAClear
        );
        assert_eq!(on_clear(&[], 70_002, ErrorKind::Window), OnClear::NotAClear);
    }

    // The bind runs on the watch's thread, where every request waits on the
    // server for as long as it takes; the start waits for it only so long.
    #[test]
    fn a_bind_the_watch_does_not_finish_in_time_is_refused() {
        let wait = Duration::from_millis(20);
        let (silent, started) = mpsc::channel::<Opened>();
        let refused = opened_within(&started, wait).expect_err("past the bound");
        assert_eq!(refused.code(), "capture_unavailable");
        assert!(refused.detail().contains("20 ms"), "{}", refused.detail());
        drop(silent);
        let ended = opened_within(&started, wait).expect_err("the watch ended");
        assert_eq!(ended.code(), "capture_unavailable");
        let (said, started) = mpsc::channel::<Opened>();
        said.send(Err(FrameError::Minimized("hidden".to_string())))
            .unwrap();
        let hidden = opened_within(&started, wait).expect_err("its own refusal");
        assert_eq!(hidden.code(), "target_minimized");
        said.send(Ok(())).unwrap();
        assert!(opened_within(&started, wait).is_ok());
    }

    #[test]
    fn a_window_renewed_or_with_no_proof_holding_waits_to_be_proved_again() {
        let crop = rect(0, 0, 8, 4);
        assert_eq!(standing(false, Some(crop)), Caught::Up);
        assert_eq!(standing(true, Some(crop)), Caught::Reproving);
        assert_eq!(standing(false, None), Caught::Reproving);
    }

    #[test]
    fn a_watch_that_ended_without_a_fault_never_serves_its_last_frame() {
        let refused = served(Caught::Ended, Ok(frame())).expect_err("ended");
        assert_eq!(refused.code(), "target_unavailable");
        let never = served(Caught::Ended, Err(FrameError::NotYet)).expect_err("ended");
        assert_eq!(
            never.code(),
            "target_unavailable",
            "no frame will ever come"
        );
    }

    #[test]
    fn a_watch_that_ended_on_a_fault_answers_that_fault() {
        let closed = FrameError::Gone("the window was closed after it was bound".to_string());
        let refused = served(Caught::Ended, Err(closed)).expect_err("gone");
        assert!(refused.detail().contains("closed"), "{refused:?}");
    }

    #[test]
    fn a_watch_that_did_not_catch_up_serves_nothing() {
        let refused = served(Caught::Late, Ok(frame())).expect_err("late");
        assert_eq!(refused.code(), "capture_unavailable");
        assert!(refused.detail().contains("did not"), "{refused:?}");
    }

    fn area(pieces: &[Rect]) -> u64 {
        pieces
            .iter()
            .map(|piece| u64::from(piece.width) * u64::from(piece.height))
            .sum()
    }

    // The window manager's frame and GTK's shadow are part of the picture, and the
    // copy the redirect made of them is whatever the screen showed there: only
    // damage after it proves them, as it proves the client's part.
    #[test]
    fn the_frame_around_the_client_stays_unproven_until_it_is_drawn() {
        let crop = rect(0, 0, 982, 795);
        let client = rect(1, 25, 980, 764);
        let left = undrawn(vec![crop], client);
        assert_eq!(area(&left), 982 * 795 - 980 * 764, "{left:?}");
        assert!(left.iter().all(|piece| overlap(*piece, client).is_none()));
        assert!(left.iter().any(|piece| piece.y == 0 && piece.height == 25));
        assert!(undrawn(left, rect(0, 0, 982, 795)).is_empty());
    }

    // Damage is read rectangle by rectangle, never as a bounding box: two corners
    // drawn say nothing of the pixels between them.
    #[test]
    fn two_corners_drawn_do_not_prove_the_middle() {
        let crop = rect(0, 0, 100, 100);
        let left = undrawn(
            undrawn(vec![crop], rect(0, 0, 10, 10)),
            rect(90, 90, 10, 10),
        );
        assert_eq!(area(&left), 100 * 100 - 2 * 10 * 10, "{left:?}");
        assert!(left
            .iter()
            .any(|piece| overlap(*piece, rect(50, 50, 1, 1)).is_some()));
    }

    #[test]
    fn damage_in_pieces_proves_the_whole_crop() {
        let crop = rect(20, 14, 800, 600);
        let left = undrawn(
            undrawn(vec![crop], rect(0, 0, 420, 700)),
            rect(420, 0, 500, 700),
        );
        assert!(left.is_empty(), "{left:?}");
    }

    fn grey(width: u32, height: u32) -> Vec<u8> {
        vec![0x80; (width * height) as usize * BYTES_PER_PIXEL]
    }

    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * width + x) as usize) * BYTES_PER_PIXEL;
        [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
    }

    // A pixel nothing proved is the window's own is never sent as it was read.
    #[test]
    fn pixels_outside_the_proven_rectangle_are_sent_black() {
        let crop = rect(5, 5, 4, 3);
        let mut pixels = grey(4, 3);
        black_out(&mut pixels, crop, Some(rect(6, 5, 10, 10)));
        for y in 0..3 {
            assert_eq!(pixel(&pixels, 4, 0, y), [0, 0, 0, 0xff], "row {y}");
            assert_eq!(pixel(&pixels, 4, 1, y), [0x80; 4], "row {y}");
        }

        let mut unproven = grey(4, 3);
        black_out(&mut unproven, crop, None);
        assert!(unproven.chunks(4).all(|p| p == [0, 0, 0, 0xff]));

        let mut proven = grey(4, 3);
        black_out(&mut proven, crop, Some(crop));
        assert_eq!(proven, grey(4, 3));
    }

    #[test]
    fn rgba_rows_become_bgra_pixel_by_pixel() {
        assert_eq!(
            bgra(vec![1, 2, 3, 255, 10, 20, 30, 255]),
            vec![3, 2, 1, 255, 30, 20, 10, 255]
        );
    }
}
