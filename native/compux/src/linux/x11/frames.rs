//! A bound window's frames on X11, through Composite: the window is redirected
//! (`RedirectWindow`, Automatic, so the server still draws it on the screen), its
//! off-screen pixmap is named (`NameWindowPixmap`), and that pixmap is read. A
//! window another window covers still has its own pixmap, so it is still seen.
//!
//! **What proves the pixmap is the window's.** Redirecting a window gives it a
//! new pixmap, and the server fills it with what the screen showed there, which
//! may be another window. Only once the window has drawn itself over all of it
//! is it the window's own. So the damage reports are watched from before the
//! redirect: the redirect's own report comes first and is set aside, then the
//! window's repaint must cover the whole picture within [`REPAINT_WAIT_MS`], or
//! the binding is refused. A window a compositing manager already redirected is
//! drawn by that manager from the same pixmap, which therefore is the window as
//! it shows. A window mapped again or resized gets a new pixmap, and is proved
//! again the same way before anything more is published.
//!
//! **What a frame carries.** The window manager's frame and GTK's shadow are
//! accounted for exactly as the window list accounts for them, so a picture is
//! the window's listed bounds, pixel for pixel. Every [`FRAME_INTERVAL_MS`] the
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
//! Everything the server holds for the stream (the redirect, the damage, the
//! pixmap, the event selections) belongs to its connection, and goes when the
//! connection closes, on every path out.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat, ImageOrder,
    MapState, Window as XWindow,
};
use x11rb::protocol::{ErrorKind, Event};
use x11rb::rust_connection::RustConnection;

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

/// The pause between two looks while a proof waits.
const PROOF_POLL_MS: u64 = 2;

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
    /// The window was mapped again or resized: it has a new pixmap to prove.
    Prove,
    /// The server reported damage: read the window again.
    Read,
    /// Nothing changed since the last read.
    Idle,
}

/// The step for what the sync found.
pub fn step(viewable: bool, was_hidden: bool, resized: bool, damaged: bool) -> Step {
    if !viewable {
        Step::Hidden
    } else if was_hidden || resized {
        Step::Prove
    } else if damaged {
        Step::Read
    } else {
        Step::Idle
    }
}

/// The bounding box of the damage reported so far, in the frame's pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Area {
    pub fn of(x: i16, y: i16, width: u16, height: u16) -> Area {
        Area {
            left: i32::from(x),
            top: i32::from(y),
            right: i32::from(x) + i32::from(width),
            bottom: i32::from(y) + i32::from(height),
        }
    }

    pub fn merged(self, other: Area) -> Area {
        Area {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// Does the damage reach every pixel of `crop`?
    pub fn covers(self, crop: Rect) -> bool {
        let (right, bottom) = (
            i64::from(crop.x) + i64::from(crop.width),
            i64::from(crop.y) + i64::from(crop.height),
        );
        self.left <= crop.x
            && self.top <= crop.y
            && i64::from(self.right) >= right
            && i64::from(self.bottom) >= bottom
    }
}

/// Is `crop` inside a drawable of this size?
pub fn inside(crop: Rect, width: u16, height: u16) -> bool {
    let right = i64::from(crop.x) + i64::from(crop.width);
    let bottom = i64::from(crop.y) + i64::from(crop.height);
    crop.x >= 0 && crop.y >= 0 && right <= i64::from(width) && bottom <= i64::from(height)
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
    slot: Arc<Slot>,
    running: Mutex<Option<Running>>,
}

impl Composite {
    pub fn new(env: Env, clock: Arc<dyn Clock>) -> Composite {
        Composite {
            env,
            clock,
            slot: Slot::new(),
            running: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl WindowFrames for Composite {
    /// The window is watched, redirected and proved here, and its first frame
    /// published, before the watch thread takes over: a window that cannot be
    /// proved is a start that failed, and nothing is left running.
    fn start(&self, window: StreamWindow) -> Result<(), FrameError> {
        self.stop();
        let mut watch = Watch::open(&self.env, window.id)?;
        watch.read(&self.slot, self.clock.now_mach(), &*self.clock)?;
        let running = Running::spawn(watch, self.slot.clone(), self.clock.clone())?;
        *self.lock() = Some(running);
        Ok(())
    }

    fn latest(&self, after: Option<Fence>) -> Result<WindowFrame, FrameError> {
        if self.lock().is_none() {
            return Err(FrameError::Unavailable(
                "no window capture is running".to_string(),
            ));
        }
        self.slot.peek(after, self.clock.now_ns())
    }

    fn stop(&self) {
        let running = self.lock().take();
        if let Some(running) = running {
            running.end();
        }
        self.slot.clear();
    }
}

impl Drop for Composite {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The watch's thread, and the two channels that start and confirm its end.
struct Running {
    stop: Sender<()>,
    ended: Receiver<()>,
    thread: std::thread::JoinHandle<()>,
}

impl Running {
    fn spawn(watch: Watch, slot: Arc<Slot>, clock: Arc<dyn Clock>) -> Result<Running, FrameError> {
        let (stop, stopped) = mpsc::channel();
        let (finished, ended) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("compux-window-frames".to_string())
            .spawn(move || {
                watch.run(&slot, &*clock, &stopped);
                // Nobody waiting for this is a stop that already gave up.
                finished.send(()).ok();
            })
            .map_err(|error| {
                FrameError::Unavailable(format!("the window capture would not start: {error}"))
            })?;
        Ok(Running {
            stop,
            ended,
            thread,
        })
    }

    /// Ask the watch to end, and wait for it, bounded. A watch stuck on a
    /// server that stopped answering is left to end when its connection does.
    fn end(self) {
        // A watch that already ended has dropped its receiver, which is the same
        // answer as this one.
        self.stop.send(()).ok();
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
    /// Whether this watch redirected the window, rather than a compositing
    /// manager before it.
    ours: bool,
    format: Format,
    frame_atom: u32,
    shadow_atom: u32,
    size: (u16, u16),
    damaged: bool,
    resized: bool,
    hidden: bool,
}

impl Watch {
    fn open(env: &Env, client: XWindow) -> Result<Watch, FrameError> {
        let (connection, _) = super::connect(env).map_err(unavailable)?;
        versions(&connection)?;
        let top = top_level(&connection, client)?;
        watch_structure(&connection, client, top)?;
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
            ours: false,
            format,
            frame_atom,
            shadow_atom,
            size: (geometry.width, geometry.height),
            damaged: true,
            resized: false,
            hidden: false,
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

    /// Damage reports first, then the redirect, then the proof, then the
    /// pixmap's name: each step is the one before it holding.
    fn redirect(&mut self) -> Result<(), FrameError> {
        let connection = &self.connection;
        self.damage = connection.generate_id().map_err(no_id)?;
        connection
            .damage_create(self.damage, self.top, ReportLevel::BOUNDING_BOX)
            .map_err(lost)?
            .check()
            .map_err(|error| reply_error(error, "damage reports on the window"))?;
        let composited = already_redirected(connection, self.top)?;
        connection
            .composite_redirect_window(self.top, Redirect::AUTOMATIC)
            .map_err(lost)?
            .check()
            .map_err(|error| reply_error(error, "redirecting the window"))?;
        self.ours = !composited;
        if self.ours {
            self.prove()?;
        }
        self.name()
    }

    /// The redirect's own report, set aside, then the window's repaint over the
    /// whole picture.
    fn prove(&mut self) -> Result<(), FrameError> {
        let crop = self.crop()?;
        self.connection.flush().map_err(lost)?;
        if self.damage_within(REDIRECT_WAIT_MS, None)?.is_none() {
            return Err(unproven(format!(
                "the X server reported no drawing within {REDIRECT_WAIT_MS} ms of redirecting it"
            )));
        }
        self.repaired()?;
        match self.damage_within(REPAINT_WAIT_MS, Some(crop))? {
            Some(_) => Ok(()),
            None => Err(unproven(format!(
                "it did not repaint all of itself within {REPAINT_WAIT_MS} ms of being redirected"
            ))),
        }
    }

    /// The pixmap the window is drawn into now, under a name of this watch's.
    fn name(&mut self) -> Result<(), FrameError> {
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

    /// Wait up to `wait_ms` for damage: any at all, or enough to cover `crop`.
    fn damage_within(
        &mut self,
        wait_ms: u64,
        crop: Option<Rect>,
    ) -> Result<Option<Area>, FrameError> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut seen: Option<Area> = None;
        for _ in 0..MAX_EVENTS {
            let Some(area) = self.next_damage()? else {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(PROOF_POLL_MS));
                continue;
            };
            let total = seen.map_or(area, |earlier| earlier.merged(area));
            seen = Some(total);
            if crop.is_none_or(|crop| total.covers(crop)) {
                return Ok(seen);
            }
        }
        Err(drowned())
    }

    /// The next damage report waiting, after any other events before it.
    fn next_damage(&mut self) -> Result<Option<Area>, FrameError> {
        for _ in 0..MAX_EVENTS {
            let Some(event) = self.connection.poll_for_event().map_err(lost)? else {
                return Ok(None);
            };
            if let Some(area) = self.took(event)? {
                return Ok(Some(area));
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

    /// One event: a damage report on this window is returned, a change to the
    /// window is noted, and a window gone ends the stream.
    fn took(&mut self, event: Event) -> Result<Option<Area>, FrameError> {
        match event {
            Event::DamageNotify(report) if report.damage == self.damage => {
                self.damaged = true;
                let area = report.area;
                return Ok(Some(Area::of(area.x, area.y, area.width, area.height)));
            }
            Event::DestroyNotify(gone) if gone.window == self.client => return Err(destroyed()),
            Event::DestroyNotify(gone) if gone.window == self.top => return Err(reframed()),
            Event::ReparentNotify(moved) if moved.window == self.client => return Err(reframed()),
            Event::ConfigureNotify(change) if change.window == self.top => {
                self.resized |= (change.width, change.height) != self.size;
                self.size = (change.width, change.height);
            }
            Event::Error(error) => {
                eprintln!("compux: the window capture: {}", super::refused(&error))
            }
            _other => {}
        }
        Ok(None)
    }

    /// One frame interval after another, until the stop arrives or a fault
    /// ends the stream; the fault is the slot's to answer.
    fn run(mut self, slot: &Slot, clock: &dyn Clock, stop: &Receiver<()>) {
        loop {
            match stop.recv_timeout(Duration::from_millis(FRAME_INTERVAL_MS)) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            }
            if let Err(fault) = self.tick(slot, clock) {
                slot.fault(fault);
                return;
            }
        }
    }

    /// Sync, read what the server said before it, and take the one step that
    /// calls for. The stamp is taken before the sync, so whatever is read after
    /// it is the window as it was then or later.
    fn tick(&mut self, slot: &Slot, clock: &dyn Clock) -> Result<(), FrameError> {
        let stamp = clock.now_mach();
        let viewable = self.viewable()?;
        self.drain()?;
        match step(viewable, self.hidden, self.resized, self.damaged) {
            Step::Hidden => {
                self.hidden = true;
                slot.note(sample(slot, clock, stamp, Health::Minimized));
                Ok(())
            }
            Step::Prove => {
                self.prove_again()?;
                self.read(slot, stamp, clock)
            }
            Step::Read => self.read(slot, stamp, clock),
            Step::Idle => {
                slot.note(sample(slot, clock, stamp, Health::Idle));
                Ok(())
            }
        }
    }

    /// A window mapped again or resized has a new pixmap. One this watch
    /// redirected is redirected again, so the server's copy and the window's
    /// repaint come in the order the proof reads them.
    fn prove_again(&mut self) -> Result<(), FrameError> {
        if self.ours {
            self.connection
                .composite_unredirect_window(self.top, Redirect::AUTOMATIC)
                .map_err(lost)?
                .check()
                .map_err(|error| reply_error(error, "redirecting the window again"))?;
            self.repaired()?;
            self.connection
                .composite_redirect_window(self.top, Redirect::AUTOMATIC)
                .map_err(lost)?
                .check()
                .map_err(|error| reply_error(error, "redirecting the window again"))?;
            self.prove()?;
        }
        self.name()?;
        self.hidden = false;
        self.resized = false;
        self.damaged = true;
        Ok(())
    }

    /// Read the window as it is now, and publish it stamped `stamp`.
    fn read(&mut self, slot: &Slot, stamp: u64, clock: &dyn Clock) -> Result<(), FrameError> {
        self.connection
            .damage_subtract(self.damage, 0u32, 0u32)
            .map_err(lost)?;
        self.damaged = false;
        let crop = self.crop()?;
        let pixels = self.grab(crop)?;
        let width = crop.width;
        slot.offer(WindowFrame {
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
        });
        Ok(())
    }

    /// Where the listed window is inside the redirected one, in its pixels: the
    /// client's place in it, less GTK's shadow, with the window manager's frame.
    fn crop(&self) -> Result<Rect, FrameError> {
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
        let crop = placed(inner, frame.reply_extents()?, shadow.reply_extents()?)
            .map(|(seen, _)| seen)
            .filter(|seen| inside(*seen, outer.width, outer.height));
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

    /// The pixmap's pixels under `crop`, in strips, as BGRA.
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

fn unproven(why: String) -> FrameError {
    FrameError::Unavailable(format!(
        "the window's own picture could not be proved: {why}, so what compux holds of it could \
         still be what the screen showed there; select it again"
    ))
}

fn drowned() -> FrameError {
    FrameError::Unavailable(format!(
        "more than {MAX_EVENTS} events arrived at once for the window, so the capture stopped \
         reading them"
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

    // The repaint must reach every pixel of the picture, edge to edge; damage
    // short by one row proves nothing about that row.
    #[test]
    fn damage_covers_a_picture_only_to_its_last_pixel() {
        let crop = rect(0, 0, 982, 795);
        let whole = Area::of(0, 0, 982, 795);
        assert!(whole.covers(crop));
        assert!(!Area::of(0, 0, 982, 794).covers(crop));
        assert!(!Area::of(1, 0, 981, 795).covers(crop));

        let halves = Area::of(0, 0, 982, 400).merged(Area::of(0, 400, 982, 395));
        assert!(halves.covers(crop));
        assert!(Area::of(0, 0, 100, 100).covers(rect(10, 10, 90, 90)));
    }

    #[test]
    fn a_picture_is_cut_only_from_inside_its_drawable() {
        assert!(inside(rect(0, 0, 982, 795), 982, 795));
        assert!(inside(rect(20, 14, 800, 600), 840, 640));
        assert!(!inside(rect(0, 0, 983, 795), 982, 795));
        assert!(!inside(rect(-1, 0, 10, 10), 982, 795));
        assert!(!inside(rect(0, 790, 10, 10), 982, 795));
    }

    #[test]
    fn rgba_rows_become_bgra_pixel_by_pixel() {
        assert_eq!(
            bgra(vec![1, 2, 3, 255, 10, 20, 30, 255]),
            vec![3, 2, 1, 255, 30, 20, 10, 255]
        );
    }
}
