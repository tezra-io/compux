//! One bound window: what it is, whether it is still there, and what may be done
//! inside it.
//!
//! A target is a promise with four halves, and each of them is a rule that costs
//! the person something if it is wrong:
//!
//!   * **it is still the same window.** The binding is the pid, that process's
//!     START time and the `CGWindowID` together, so a reused pid or a reused window
//!     number can never revive a target that is gone;
//!   * **it is where the caller last saw it.** The window's bounds, display and
//!     scale are part of every target observation, and a difference at action time
//!     is `stale_observation` — a window that moved since the screenshot is never
//!     clicked where it used to be;
//!   * **a pixel really reaches it.** A pointer action needs the target to be
//!     topmost at that exact point, or it is `target_obstructed`. Nothing raises
//!     the window and nothing activates it: the whole point of a bound target is
//!     that the person keeps working where they were;
//!   * **the person can see it happening.** No ready indicator, no target mutation.
//!
//! Everything above the two platform seams — the window server and the frames — is
//! a pure function or a test against a recording implementation, so every rule here
//! is proved on a machine with no display session.

use std::rc::Rc;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::gate::{Clock, Gate, Phase};
use crate::geometry::{Geometry, MonitorFacts};
use crate::indicator;
use crate::observation::Observation;
use crate::window_frames::{Fence, FrameError, StreamWindow, WindowFrame, WindowFrames};
use crate::window_server::{self, Bounds, WindowFacts, Windows};
use crate::wire::{self, Failure};
use crate::Frames;

/// How closely one of the application's accessibility windows must match the window
/// the caller named before it is bound to it. Two points, because a window's
/// accessibility frame and its window-server bounds are the same rectangle read
/// through two APIs that round differently — and because a second window two points
/// from the first is not a thing a desktop has.
const AX_MATCH_TOLERANCE: f64 = 2.0;

/// How far a window may move before the image a caller acted in is stale. Half a
/// point: below that is the rounding two readings of one number disagree by, above
/// it is a window that moved.
const GEOMETRY_TOLERANCE: f32 = 0.5;

/// How long `select_target` waits for the stream's first frame. A stream that is
/// going to deliver does so in a frame interval or two; one that has not in this
/// long is one the caller must be told about rather than left waiting on.
const FIRST_FRAME_MS: u64 = 3_000;

/// How long a look waits for a frame that passes the fence. Frames arrive ten times
/// a second at most, so this is several of them — and it is bounded because a settle
/// looks many times and each look must return.
const FENCE_WAIT_MS: u64 = 400;

/// The interval between two looks at the slot while waiting for one of those.
const SLOT_POLL_MS: u64 = 10;

/// How the accessibility half of a target came out.
///
/// Three answers rather than a boolean, because the caller does something different
/// with each: `bound` offers `press` and `set_value` and an element walk rooted at
/// the window; `ambiguous` says the application has more than one window that looks
/// like this one, so naming a control inside it would be a guess; `unavailable`
/// says the tree could not be read at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxBinding {
    Bound,
    Ambiguous,
    Unavailable,
}

impl AxBinding {
    fn as_str(self) -> &'static str {
        match self {
            AxBinding::Bound => "bound",
            AxBinding::Ambiguous => "ambiguous",
            AxBinding::Unavailable => "unavailable",
        }
    }
}

/// The window this helper is bound to.
///
/// Owned by the action worker, like the observation table, so nothing here is
/// locked — and the one `Drop` that ends it (a release, a replacement, the process
/// exiting) stops the stream and the indicator with it, because both are owned
/// fields rather than things somebody has to remember to stop.
pub struct Target {
    pub id: String,
    /// Bumped on every reselect, so a reply can say WHICH binding it is about even
    /// though there is only ever one at a time.
    pub generation: u64,
    pub window_id: u32,
    pub pid: i32,
    /// The owning process's start time. A pid is not an identity — pids are reused —
    /// so a target is only itself while this still answers what it answered at
    /// selection.
    pub started_at: u64,
    pub app: String,
    pub title: String,
    pub ax_binding: AxBinding,
    /// The bound `AXWindow`, when exactly one of the application's matched. The
    /// root an `elements` walk starts from, and the reason `ax` is offered.
    pub ax_window: Option<crate::ax::Retained>,
    /// The application's on-screen windows at the moment of selection. Anything of
    /// its own that appears later is a child, and is reported.
    pub siblings: Vec<u32>,
    /// The display the window was on when it was bound, and what the OS said about
    /// it — recorded so a target observation carries the same display facts every
    /// other observation does.
    pub display_id: u32,
    pub facts: MonitorFacts,
    /// The frames, and the fence an after-action look is held to.
    pub frames: Rc<TargetFrames>,
    /// The badge. `None` only between a child exiting and the one restart.
    pub indicator: Option<indicator::Supervisor>,
    /// How many times the badge has been restarted for THIS target. One, then the
    /// refusal stands until the target is reselected.
    pub restarts: u32,
}

impl Target {
    /// Is the badge up and able to take a press?
    fn indicator_ready(&self) -> bool {
        self.indicator
            .as_ref()
            .map(indicator::Supervisor::ready)
            .unwrap_or(false)
    }
}

/// The bound window as ONE request sees it: checked alive, its transform built from
/// the newest frame, and the facts an observation of it records.
///
/// Built once per request and used throughout, so every part of one action — where
/// it aims, what its check covers, what its observation records — agrees about where
/// the window was.
#[derive(Debug)]
pub struct Live {
    pub window_id: u32,
    pub geometry: Geometry,
    pub display_id: u32,
    pub facts: MonitorFacts,
    pub frames: Rc<TargetFrames>,
    /// The frame the transform was built from, and the sequence a later fence is
    /// measured against.
    pub seq: u64,
}

// --- the transform ------------------------------------------------------------

/// How far the two axes' measured scales may disagree, over the WHOLE window. One
/// pixel: below that is two divisions of one ratio rounding differently, above it
/// is a surface that is not a picture of this window.
const SCALE_AGREEMENT_PIXELS: f64 = 1.0;

/// How much of the surface the content may fail to cover before the mapping is
/// refused rather than guessed at.
///
/// Two pixels, and it is a compromise between two real failures. A materially
/// letterboxed surface — the window drawn inside a border of empty pixels — maps
/// every coordinate wrong by the size of that border, so it must be refused. A
/// surface that is a pixel short because the window server rounded a point size is
/// the same picture and refusing it would break a working target for nothing.
const LETTERBOX_TOLERANCE_PIXELS: f64 = 2.0;

/// The transform for one window: image pixels to the global logical points the
/// input path takes.
///
/// **Neither of the frame's two placement attachments is the quantity it looks
/// like**, which is the whole reason this function takes the window's bounds:
///
///   * `SCStreamFrameInfoContentRect` is the content's place IN THE SURFACE
///     (`SCStream.h`), not on the desktop. Reading it as an origin puts every
///     mapped coordinate at the window's own offset from the screen's corner, and
///     leaves a comparison of "did it move" reading a number that never changes.
///     It is used here for ONE thing: noticing a surface the content does not
///     fill, which would map every coordinate wrong by the size of the border.
///   * `SCStreamFrameInfoScaleFactor` is the DISPLAY's pixels-per-point, and the
///     surface is requested at a size in pixels that may or may not have been
///     honoured. On a Retina panel the two disagree by a factor of two whenever
///     the request and the panel do.
///
/// So the window's place and size come from `kCGWindowBounds`, which is the one
/// authority on where a window is, and the scale is **measured**: the frame's
/// pixels divided by the window's points, on both axes, which must agree. The
/// frame is still the truth about its own size, exactly as slice 3 decided for the
/// display — the physical size IS the frame's size, so a crop can never clamp a
/// row away — and a surface whose shape does not describe the window is refused
/// with the numbers rather than mapped.
pub fn transform(frame: &WindowFrame, bounds: &Bounds) -> Result<Geometry, Failure> {
    let (width, height) = (f64::from(frame.width), f64::from(frame.height));

    if bounds.w <= 0.0 || bounds.h <= 0.0 || width <= 0.0 || height <= 0.0 {
        return Err(mismatch(format!(
            "the window is {}x{} points and its frame is {width}x{height} pixels, and neither a \
             window nor a picture of one can have a side of nothing",
            bounds.w, bounds.h
        )));
    }

    let across = width / bounds.w;
    let down = height / bounds.h;

    // Agreement is checked over the whole window rather than per point: two ratios
    // that differ in the fourth decimal are one rounding, and two that differ by a
    // pixel over eight hundred are a surface of something else.
    if (bounds.h * across - height).abs() > SCALE_AGREEMENT_PIXELS {
        return Err(mismatch(format!(
            "the frame is {width}x{height} pixels for a window {}x{} points, which is {across:.4} \
             pixels a point across and {down:.4} down; a picture of this window cannot have two \
             scales{}",
            bounds.w,
            bounds.h,
            reported(frame)
        )));
    }

    if let Some(content) = frame.content_rect {
        let inset_x = content.x.abs() * across;
        let inset_y = content.y.abs() * down;
        let short_x = (width - content.w * across).abs();
        let short_y = (height - content.h * down).abs();
        let filled = inset_x.max(inset_y).max(short_x).max(short_y);

        if filled > LETTERBOX_TOLERANCE_PIXELS {
            return Err(mismatch(format!(
                "the window's content covers {:.1}x{:.1} points at ({:.1}, {:.1}) of a \
                 {width}x{height} pixel surface, so the picture is inset inside it and every \
                 coordinate read in it would be off by that border",
                content.w, content.h, content.x, content.y
            )));
        }
    }

    Ok(Geometry {
        phys_w: frame.width,
        phys_h: frame.height,
        logical_w: bounds.w as f32,
        logical_h: bounds.h as f32,
        origin_x: bounds.x as f32,
        origin_y: bounds.y as f32,
        scale_factor: across as f32,
    })
}

/// What the frame CLAIMED its scale was, for a mismatch the person has to diagnose.
/// Reported, never trusted: it is the display's factor, not the surface's.
fn reported(frame: &WindowFrame) -> String {
    match frame.reported_scale {
        Some(scale) => format!(" (the display reports {scale} pixels a point)"),
        None => String::new(),
    }
}

fn mismatch(detail: String) -> Failure {
    Failure::new("capture_geometry_mismatch", detail)
}

/// Is this observation still describing the window as it is now?
///
/// The whole geometry generation, in one comparison: where the window is, how big
/// it is, and what scale it was rendered at. Because [`transform`] puts the window
/// server's own `kCGWindowBounds` into the geometry's origin and logical size, this
/// IS "the bounds then against the bounds now" — a window that moved a point is
/// caught, and so is one that moved to a display of another backing scale. A
/// difference in any of them means a coordinate read in that image maps somewhere
/// nobody chose, so the action is refused before anything is dispatched.
pub fn unmoved(observation: &Observation, live: &Live) -> Result<(), Failure> {
    let (was, now) = (&observation.geometry, &live.geometry);

    let same = near(was.origin_x, now.origin_x)
        && near(was.origin_y, now.origin_y)
        && near(was.logical_w, now.logical_w)
        && near(was.logical_h, now.logical_h)
        && near(was.scale_factor, now.scale_factor)
        && observation.display_id == live.display_id;

    if same {
        Ok(())
    } else {
        Err(Failure::new(
            "stale_observation",
            "geometry_changed".to_string(),
        ))
    }
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() <= GEOMETRY_TOLERANCE
}

// --- the frames seam ----------------------------------------------------------

/// A bound window's frames, as the rest of this process reads any frames.
///
/// This is what makes the settle, the quiet window, `view_hash`, `changed` and the
/// check work on a target with no second code path: slice 6 samples through
/// [`crate::Frames`], and a target is simply another implementation of it.
///
/// The fence rides here rather than on the trait because a check samples MANY times
/// and every one of those samples has to be held to it. It is armed the instant the
/// input is complete and disarmed when the action ends.
pub struct TargetFrames {
    stream: Rc<dyn WindowFrames>,
    clock: Arc<dyn Clock>,
    fence: std::cell::Cell<Option<Fence>>,
}

impl std::fmt::Debug for TargetFrames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TargetFrames({:?})", self.fence.get())
    }
}

impl TargetFrames {
    pub fn new(stream: Rc<dyn WindowFrames>, clock: Arc<dyn Clock>) -> TargetFrames {
        TargetFrames {
            stream,
            clock,
            fence: std::cell::Cell::new(None),
        }
    }

    /// From here on, only a frame the window server DISPLAYED after `at_mach` and
    /// whose sequence is beyond `seq`.
    ///
    /// `at_mach` is read on the mach absolute time base, which is the base a
    /// frame's display time arrives on. The two are never converted into one
    /// another; they are the same clock read at two moments.
    pub fn arm(&self, at_mach: u64, seq: u64) {
        self.fence.set(Some(Fence {
            after_mach: at_mach,
            beyond_seq: seq,
        }));
    }

    pub fn disarm(&self) {
        self.fence.set(None);
    }

    /// The newest frame, waited on for as long as the fence needs and no longer.
    ///
    /// The wait is what lets the settle read this exactly as it reads a display
    /// capture: a display capture takes a couple of hundred milliseconds too, and
    /// the settle's own cap is what turns "still nothing" into an answer.
    pub fn newest(&self) -> Result<WindowFrame, FrameError> {
        let fence = self.fence.get();
        let deadline = self.clock.now_ms() + FENCE_WAIT_MS;

        loop {
            match self.stream.latest(fence) {
                Err(FrameError::NotYet) if self.clock.now_ms() < deadline => {
                    self.clock.sleep(SLOT_POLL_MS)
                }
                answer => return answer,
            }
        }
    }
}

impl Frames for TargetFrames {
    /// The display id is the display path's addressing and means nothing here: a
    /// stream is bound to a window, and the window is on whichever display it is
    /// on. Ignored rather than checked, because checking it would invent a
    /// disagreement between two facts that are not about the same thing.
    fn capture(&self, _display_id: u32) -> Result<image::RgbaImage, Failure> {
        let frame = self.newest().map_err(as_failure)?;
        frame.to_rgba().map_err(as_failure)
    }
}

/// A frame error as the wire reports it.
pub fn as_failure(error: FrameError) -> Failure {
    Failure::new(error.code(), error.detail())
}

// --- selection ----------------------------------------------------------------

/// Everything `select_target` needs that this module does not own.
pub struct Selecting<'a> {
    pub windows: &'a Arc<dyn Windows>,
    pub ax: &'a Rc<dyn crate::ax::Ax>,
    pub launcher: &'a Arc<dyn indicator::Launcher>,
    pub emitter: &'a crate::capture::Emitter,
    /// A fresh stream for this binding. Given rather than made here so a test can
    /// bind a window with no window server and no ScreenCaptureKit.
    pub stream: Rc<dyn WindowFrames>,
    pub display_id: u32,
    pub facts: MonitorFacts,
}

/// What a selection produced: the binding, the first picture of it, and the
/// mapping that picture's coordinates are read through.
///
/// The three travel together because they are one fact — the geometry is measured
/// against THAT frame and THOSE bounds, and handing the caller a frame to build its
/// own mapping from is how the two come to disagree.
pub struct Selected {
    pub target: Target,
    pub first: WindowFrame,
    pub geometry: Geometry,
}

/// Bind one window and answer what was bound.
///
/// Order matters, and it is the order in which a failure costs least: the window
/// first (a window that is not there binds nothing), then the stream (a capture
/// that will not start is a target that can never be looked at), then the badge (no
/// badge, no working in somebody's window), and the accessibility window last,
/// because it is the one part a target is still useful without.
pub fn select(
    window_id: u32,
    generation: u64,
    id: String,
    gate: &Gate,
    selecting: Selecting<'_>,
) -> Result<Selected, Failure> {
    let listed = selecting.windows.list().map_err(unavailable)?;
    let window = window_server::find(&listed, window_id)
        .ok_or_else(|| {
            Failure::new(
                "target_unavailable",
                format!("window {window_id} is not one this machine has; take `windows` again"),
            )
        })?
        .clone();

    if !window.on_screen {
        return Err(Failure::new(
            "target_minimized",
            format!(
                "{} — {} is minimized or on another desktop, so there is nothing to see or act \
                 in; ask the user to bring it back",
                window.app, window.title
            ),
        ));
    }

    let started_at = selecting.ax.process_started_at(window.pid).ok_or_else(|| {
        Failure::new(
            "target_unavailable",
            "the application that owns that window would not say when it started, so a binding \
             to it could not be told apart from a later process that reuses its id"
                .to_string(),
        )
    })?;

    // The surface is asked for in PIXELS — the window's points at the backing scale
    // of the display it is on — because that is the unit `SCStreamConfiguration`
    // documents and the window's own frame is in points. It is a request and
    // nothing downstream trusts it: `transform` measures what really arrived.
    selecting
        .stream
        .start(StreamWindow {
            id: window_id,
            width: surface_pixels(window.bounds.w, selecting.facts.scale_factor),
            height: surface_pixels(window.bounds.h, selecting.facts.scale_factor),
        })
        .map_err(as_failure)?;

    let frames = Rc::new(TargetFrames::new(selecting.stream, gate.clock()));
    let first = first_frame(&frames, gate)?;
    let geometry = transform(&first, &window.bounds)?;

    let (ax_binding, ax_window) = bind_accessibility(selecting.ax, &window);

    let indicator = indicator::Supervisor::start(
        selecting.launcher,
        selecting.windows.clone(),
        gate,
        selecting.emitter,
        gate.clock(),
        indicator::Watching {
            window_id,
            app: window.app.clone(),
            title: window.title.clone(),
        },
    )
    .map_err(|reason| Failure::new("control_surface_unavailable", reason))?;

    let target = Target {
        id,
        generation,
        window_id,
        pid: window.pid,
        started_at,
        app: window.app.clone(),
        title: window.title.clone(),
        ax_binding,
        ax_window,
        siblings: window_server::ids_of(&listed, window.pid),
        display_id: selecting.display_id,
        facts: selecting.facts,
        frames,
        indicator: Some(indicator),
        restarts: 0,
    };

    Ok(Selected {
        target,
        first,
        geometry,
    })
}

/// A window's side in points, as a surface side in real pixels. Rounded up, so a
/// window at a fractional size is never asked for at a pixel less than it covers.
fn surface_pixels(points: f64, scale: f32) -> u32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        f64::from(scale)
    } else {
        1.0
    };

    (points * scale).ceil().max(1.0) as u32
}

/// Wait for the stream's first frame, bounded and through the gate, so a pause
/// during a selection returns at once rather than running the budget out.
fn first_frame(frames: &Rc<TargetFrames>, gate: &Gate) -> Result<WindowFrame, Failure> {
    let deadline = gate.now_ms() + FIRST_FRAME_MS;

    loop {
        let started = gate.now_ms();
        let answer = frames.stream.latest(None);
        gate.record(Phase::Capture, gate.now_ms().saturating_sub(started));

        match answer {
            Ok(frame) => return Ok(frame),
            Err(FrameError::NotYet) if gate.now_ms() < deadline => {
                gate.sleep(SLOT_POLL_MS).map_err(crate::barred)?;
            }
            Err(FrameError::NotYet) => {
                return Err(Failure::new(
                    "capture_unavailable",
                    "the window capture started and sent no frame; the window may be on another \
                     desktop, or Screen Recording may not be granted"
                        .to_string(),
                ))
            }
            Err(other) => return Err(as_failure(other)),
        }
    }
}

/// The application's accessibility window that IS this window, when exactly one of
/// them is.
///
/// Title and frame together, and only an exact count of one. Two windows of one
/// application with the same title and the same frame is `ambiguous`, and an
/// ambiguous binding offers no `ax`: pressing a control by name in the wrong one of
/// two identical windows is the kind of wrong that looks right.
fn bind_accessibility(
    ax: &Rc<dyn crate::ax::Ax>,
    window: &WindowFacts,
) -> (AxBinding, Option<crate::ax::Retained>) {
    let windows = crate::ax::application_windows(ax, window.pid);
    if windows.is_empty() {
        return (AxBinding::Unavailable, None);
    }

    let mut matching: Vec<crate::ax::Retained> = windows
        .into_iter()
        .filter(|candidate| matches_window(ax, candidate.handle(), window))
        .collect();

    match matching.len() {
        1 => (AxBinding::Bound, matching.pop()),
        0 => (AxBinding::Unavailable, None),
        _several => (AxBinding::Ambiguous, None),
    }
}

fn matches_window(
    ax: &Rc<dyn crate::ax::Ax>,
    handle: crate::ax::Handle,
    window: &WindowFacts,
) -> bool {
    let titled = ax.label(handle).unwrap_or_default() == window.title;
    let placed = ax
        .bounds(handle)
        .map(|frame| {
            Bounds {
                x: frame.x,
                y: frame.y,
                w: frame.w,
                h: frame.h,
            }
            .matches(&window.bounds, AX_MATCH_TOLERANCE)
        })
        .unwrap_or(false);

    titled && placed
}

/// The methods this binding really offers. `ax` only where an accessibility window
/// was bound: a caller that reads `ax` here and gets a refusal has been lied to.
pub fn methods(binding: AxBinding) -> Vec<&'static str> {
    match binding {
        AxBinding::Bound => vec!["foreground_hid", "ax"],
        _other => vec!["foreground_hid"],
    }
}

/// The reply `select_target` answers with, less the observation the caller's next
/// coordinates are in — which the caller mints, because minting is the observation
/// table's business.
pub fn selected_payload(target: &Target) -> Value {
    json!({
        "ok": true,
        "target_id": target.id,
        "target_generation": target.generation,
        "window_id": target.window_id,
        "app": target.app,
        "title": target.title,
        "methods": methods(target.ax_binding),
        "ax_binding": target.ax_binding.as_str(),
    })
}

// --- using one ----------------------------------------------------------------

/// The bound window a request names, checked and ready to act in.
///
/// Every check a target has, in the order a failure costs least: is it this
/// helper's target at all, is the application still the one that was bound, is the
/// window still listed and on screen, and only then the frame the transform is
/// built from.
pub fn live(
    target: Option<&Target>,
    named: &str,
    windows: &Arc<dyn Windows>,
    ax: &Rc<dyn crate::ax::Ax>,
) -> Result<Live, Failure> {
    let target = target.ok_or_else(|| {
        Failure::new(
            "target_unavailable",
            format!(
                "{named} is not a window this helper is bound to; `select_target` binds one and \
                 answers the id to use"
            ),
        )
    })?;

    if target.id != named {
        return Err(Failure::new(
            "target_unavailable",
            format!(
                "{named} is not the window this helper is bound to; it is bound to {}, so take \
                 `windows` and select again if you meant another one",
                target.id
            ),
        ));
    }

    // A reused pid never revives a target: the start time is what says this is the
    // same process rather than a later one that happens to have its number.
    if ax.process_started_at(target.pid) != Some(target.started_at) {
        return Err(Failure::new(
            "target_unavailable",
            format!(
                "the application that owned {} — {} — is no longer running; select a window of \
                 the one that is",
                target.app, target.title
            ),
        ));
    }

    let listed = windows.list().map_err(unavailable)?;
    let window = window_server::find(&listed, target.window_id).ok_or_else(|| {
        Failure::new(
            "target_unavailable",
            format!(
                "{} — {} is gone; take `windows` and select another",
                target.app, target.title
            ),
        )
    })?;

    if !window.on_screen {
        return Err(Failure::new(
            "target_minimized",
            format!(
                "{} — {} is minimized or on another desktop, so nothing can be seen or done in \
                 it; ask the user to bring it back",
                target.app, target.title
            ),
        ));
    }

    let frame = target.frames.newest().map_err(as_failure)?;

    Ok(Live {
        window_id: target.window_id,
        // The window server's bounds as of THIS request, which is what makes
        // `unmoved` a comparison of where the window was against where it is.
        geometry: transform(&frame, &window.bounds)?,
        display_id: target.display_id,
        facts: target.facts,
        frames: target.frames.clone(),
        seq: frame.seq,
    })
}

/// The badge is up, or this mutating request is refused.
///
/// One restart, and only for a child that has EXITED: a badge that never said
/// `ready` is one that is starting or broken, and restarting a starting one twice
/// would be this helper racing itself.
pub fn control_surface(target: &mut Target, restarting: Restarting<'_>) -> Result<(), Failure> {
    if target.indicator_ready() {
        return Ok(());
    }

    // Only a child that has EXITED is restarted. One that has not said `ready` yet
    // is starting or broken, and restarting a starting one would be this helper
    // racing itself.
    let gone = target
        .indicator
        .as_ref()
        .map(indicator::Supervisor::gone)
        .unwrap_or(true);

    let mut why = None;
    if gone && target.restarts == 0 {
        target.restarts += 1;
        // Dropped BEFORE the new one starts, so two badges can never be on screen
        // at once — and the drop is what stops the old child, its reader and its
        // watch.
        target.indicator = None;

        match indicator::Supervisor::start(
            restarting.launcher,
            restarting.windows.clone(),
            restarting.gate,
            restarting.emitter,
            restarting.gate.clock(),
            indicator::Watching {
                window_id: target.window_id,
                app: target.app.clone(),
                title: target.title.clone(),
            },
        ) {
            Ok(started) => target.indicator = Some(started),
            // The reason the badge would not come back is the only thing that tells
            // anyone what to do about it — a missing binary and a refused spawn are
            // different problems with different remedies, and swallowing it leaves
            // one sentence for both.
            Err(reason) => why = Some(reason),
        }
    }

    Err(Failure::new(
        "control_surface_unavailable",
        format!(
            "the indicator that shows the user their window is being worked in is not up, so \
             nothing may be done inside {} — {}{}. Tell the user computer use needs restarting, \
             or work at the display level instead",
            target.app,
            target.title,
            match why {
                Some(reason) => format!(" ({reason})"),
                None => String::new(),
            }
        ),
    ))
}

/// A pixel action needs the target to be topmost where it is aiming.
///
/// Nothing raises the window and nothing activates it: a bound target is the helper
/// working where the person is working, and bringing it to the front would be
/// taking the screen back from them to do it. The refusal names what is in front,
/// because "something is covering it" sends the caller looking and "Slack —
/// Messages is in front of it" tells it what to do.
/// What a restart needs — the same four things the first start needed, because a
/// restart IS a start.
pub struct Restarting<'a> {
    pub launcher: &'a Arc<dyn indicator::Launcher>,
    pub windows: &'a Arc<dyn Windows>,
    pub gate: &'a Gate,
    pub emitter: &'a crate::capture::Emitter,
}

pub fn unobstructed(
    live: &Live,
    windows: &Arc<dyn Windows>,
    lx: i32,
    ly: i32,
) -> Result<(), Failure> {
    let listed = windows.list().map_err(unavailable)?;
    let covering =
        window_server::obstruction_at(&listed, live.window_id, f64::from(lx), f64::from(ly))
            .map_err(|_gone| {
                Failure::new(
                    "target_unavailable",
                    "the window is no longer one the window server lists, so nothing can be \
                     clicked in it; take `windows` and select another"
                        .to_string(),
                )
            })?;

    match covering {
        None => Ok(()),
        Some(covering) => Err(Failure::new(
            "target_obstructed",
            format!(
                "{} — {} is in front of the target window at that point, so a click there would \
                 reach it instead. Act on a control by name, aim somewhere the target is not \
                 covered, or ask the user to move it",
                covering.app, covering.title
            ),
        )),
    }
}

/// The same application's windows that appeared since this target was bound.
///
/// Reported, never selected for: the helper cannot know that an action "needed" the
/// sheet it just opened, and choosing one would be it deciding what the caller
/// meant.
/// A window server that will not answer is NOT an empty list of children: "the
/// application opened nothing" and "nobody knows what it opened" are opposite
/// facts, and flattening them would tell the caller a sheet it cannot see is not
/// there.
pub fn children(target: &Target, windows: &Arc<dyn Windows>) -> Result<Vec<Value>, Failure> {
    let listed = windows.list().map_err(unavailable)?;

    Ok(
        window_server::children_of(&listed, target.pid, target.window_id, &target.siblings)
            .into_iter()
            .map(|window| {
                json!({
                    "window_id": window.id,
                    "app": window.app,
                    "title": window.title,
                })
            })
            .collect(),
    )
}

/// Is the window the person is working in the TARGET's?
///
/// The difference between a person busy in this application and a person busy
/// somewhere else entirely, which is what decides whether an accessibility action
/// has to wait for them.
///
/// Gated to the builds that can reach it: it is read by `idle_ms`, which needs a
/// macOS idle counter. The rule is pure and its test runs on every target.
#[cfg(any(target_os = "macos", test))]
pub fn front_is_target(target: &Target, windows: &Arc<dyn Windows>) -> Option<bool> {
    let listed = windows.list().ok()?;
    let front = window_server::front_window(&listed)?;

    Some(front.id == target.window_id)
}

/// A window server that would not answer. Everything a target knows comes from it,
/// so this is the target being unusable rather than an empty desktop.
fn unavailable(reason: String) -> Failure {
    Failure::new(
        "target_unavailable",
        format!("the window server would not say what windows exist: {reason}"),
    )
}

/// `elements` on a target walks from the bound accessibility window. Without one
/// there is nothing to walk FROM, and walking the whole application instead would
/// answer controls from windows the caller did not name.
pub fn ax_root(target: &Target) -> Result<crate::ax::Handle, Failure> {
    match target.ax_window.as_ref() {
        Some(window) => Ok(window.handle()),
        None => Err(Failure::new(
            "ax_binding_unavailable",
            format!(
                "no accessibility window of {} could be matched to this one ({}), so its \
                 controls cannot be listed by name; work in the image instead",
                target.app,
                target.ax_binding.as_str()
            ),
        )),
    }
}

/// Does this action have to be refused when the badge is not up?
///
/// Mutating ones only. A look inside a bound window changes nothing, and refusing
/// one would leave a caller unable to see why it was refused.
pub fn needs_control_surface(action: &str) -> bool {
    wire::touches_input(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window_frames::{Health, Recorder as FrameRecorder};
    use crate::window_server::{window, Recorder as WindowRecorder};

    fn at(x: f64, y: f64, w: f64, h: f64) -> Bounds {
        Bounds { x, y, w, h }
    }

    fn facts() -> MonitorFacts {
        MonitorFacts {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale_factor: 2.0,
        }
    }

    /// A frame of a window `w` x `h` points, captured at `scale` pixels a point.
    ///
    /// Its content rect is what SCK really attaches: the content's place INSIDE the
    /// surface, which for a surface the window fills is the surface itself at the
    /// origin — never the window's place on the desktop.
    fn frame(w: f64, h: f64, scale: f32, seq: u64) -> WindowFrame {
        WindowFrame {
            pixels: Vec::new(),
            width: (w * f64::from(scale)) as u32,
            height: (h * f64::from(scale)) as u32,
            stride: (w * f64::from(scale)) as usize * 4,
            captured_at_ns: 0,
            display_time_mach: Some(0),
            seq,
            content_rect: Some(at(0.0, 0.0, w, h)),
            reported_scale: Some(scale),
            status: Health::Live,
        }
    }

    fn windows_at(bounds: Bounds) -> Arc<WindowRecorder> {
        WindowRecorder::new(vec![
            window(10, 100, at(1_500.0, 800.0, 200.0, 100.0)),
            window(20, 200, bounds),
        ])
    }

    fn live_at(bounds: Bounds, scale: f32) -> Live {
        let frames = FrameRecorder::new();
        frames.start(stream_window()).unwrap();

        Live {
            window_id: 20,
            geometry: transform(&frame(bounds.w, bounds.h, scale, 1), &bounds)
                .expect("a frame of that window"),
            display_id: 7,
            facts: facts(),
            frames: Rc::new(TargetFrames::new(
                frames,
                Arc::new(crate::gate::SystemClock::new()),
            )),
            seq: 1,
        }
    }

    fn stream_window() -> StreamWindow {
        StreamWindow {
            id: 20,
            width: 1600,
            height: 1200,
        }
    }

    // The transform's whole job: a pixel of the window's image is a point on the
    // screen — and the point it is over comes from the WINDOW SERVER's bounds, not
    // from the frame's content rect, which is surface-relative and says (0, 0) for
    // every window wherever it sits.
    #[test]
    fn a_pixel_of_the_window_maps_to_the_point_it_is_over() {
        let bounds = at(400.0, 300.0, 800.0, 600.0);
        let geometry = transform(&frame(800.0, 600.0, 2.0, 1), &bounds).expect("one window");
        let region = crate::geometry::Region::full(&geometry);

        // The image's top left is the window's own top left, and its centre is the
        // window's centre — never the display's and never the surface's corner.
        assert_eq!(
            crate::geometry::to_logical(&geometry, &region, 0.0, 0.0),
            (400, 300)
        );

        let (w, h) = crate::geometry::crop_rect(&geometry, &region).sent_dims();
        let (cx, cy) =
            crate::geometry::to_logical(&geometry, &region, f64::from(w) / 2.0, f64::from(h) / 2.0);
        assert!((cx - 800).abs() <= 1, "{cx}");
        assert!((cy - 600).abs() <= 1, "{cy}");
    }

    // The scale is MEASURED against the frame that arrived, never read off the
    // attachment that reports the DISPLAY's factor: on a Retina panel a surface
    // requested in points comes back at one pixel per point while the attachment
    // still says two.
    #[test]
    fn the_scale_is_measured_from_the_frame_and_not_read_off_the_attachment() {
        let bounds = at(0.0, 0.0, 800.0, 600.0);

        for scale in [1.0_f32, 2.0] {
            let mut frame = frame(800.0, 600.0, scale, 1);
            // Whatever the display claims, the surface is what it is.
            frame.reported_scale = Some(2.0);

            let geometry = transform(&frame, &bounds).expect("one window");
            assert_eq!(geometry.scale_factor, scale, "at {scale}x");
            assert_eq!(geometry.phys_w, (800.0 * f64::from(scale)) as u32);
            assert_eq!(geometry.logical_w, 800.0);
        }
    }

    // A window is where the window server says it is, including at a negative
    // origin on a display to the left of the main one.
    #[test]
    fn a_window_on_a_second_display_maps_through_its_own_negative_origin() {
        let bounds = at(-1_920.0, -200.0, 600.0, 400.0);
        let geometry = transform(&frame(600.0, 400.0, 2.0, 1), &bounds).expect("one window");
        let region = crate::geometry::Region::full(&geometry);

        assert_eq!(geometry.origin_x, -1_920.0);
        assert_eq!(
            crate::geometry::to_logical(&geometry, &region, 0.0, 0.0),
            (-1_920, -200)
        );
    }

    // A surface whose shape is not this window's is refused with the numbers, never
    // mapped: every coordinate read in it would be wrong by a factor nobody chose.
    #[test]
    fn a_surface_that_does_not_describe_the_window_is_refused_with_its_numbers() {
        let bounds = at(0.0, 0.0, 800.0, 600.0);
        let mut squashed = frame(800.0, 600.0, 2.0, 1);
        squashed.height = 900; // 2x across, 1.5x down

        let refusal = transform(&squashed, &bounds).expect_err("two scales");
        assert_eq!(refusal.code, "capture_geometry_mismatch");
        let detail = refusal.detail.unwrap_or_default();
        assert!(detail.contains("1600"), "{detail}");
        assert!(detail.contains("900"), "{detail}");
    }

    // A letterboxed surface maps every coordinate wrong by the size of its border.
    #[test]
    fn a_surface_the_content_does_not_fill_is_refused() {
        let bounds = at(0.0, 0.0, 800.0, 600.0);
        let mut inset = frame(800.0, 600.0, 2.0, 1);
        inset.content_rect = Some(at(20.0, 0.0, 760.0, 600.0));

        let refusal = transform(&inset, &bounds).expect_err("inset inside the surface");
        assert_eq!(refusal.code, "capture_geometry_mismatch");

        // A point of rounding is the same picture and is not refused.
        let mut rounded = frame(800.0, 600.0, 2.0, 1);
        rounded.content_rect = Some(at(0.0, 0.0, 799.5, 600.0));
        assert!(transform(&rounded, &bounds).is_ok());
    }

    // The frame is the truth about its own size, so the physical size IS the
    // frame's and a crop can never clamp a row away.
    #[test]
    fn the_transform_takes_its_physical_size_from_the_frame_itself() {
        let geometry = transform(&frame(801.0, 601.0, 2.0, 1), &at(0.0, 0.0, 801.0, 601.0))
            .expect("one window");

        assert_eq!(geometry.phys_w, 1602);
        assert_eq!(geometry.phys_h, 1202);
        assert_eq!(geometry.scale_factor, 2.0);
    }

    // A window that moved between the screenshot and the click is never clicked
    // where it used to be.
    //
    // This is the case a surface-relative origin could never catch: the content
    // rect says (0, 0) whether the window is at the screen's corner or halfway
    // across it, so a comparison built on it compares a number that never changes.
    #[test]
    fn a_window_that_moved_makes_the_image_it_was_read_in_stale() {
        let live = live_at(at(400.0, 300.0, 800.0, 600.0), 2.0);
        let observation = observation_of(&live);

        assert!(unmoved(&observation, &live).is_ok());

        let moved = live_at(at(420.0, 300.0, 800.0, 600.0), 2.0);
        let refusal = unmoved(&observation, &moved).expect_err("it moved");
        assert_eq!(refusal.code, "stale_observation");
        assert_eq!(refusal.detail.as_deref(), Some("geometry_changed"));
    }

    // End to end through `live`, which is the path a real click takes: the window
    // moves on the desktop, the frame it is captured in does not change shape, and
    // the observation the caller read its coordinates in goes stale.
    #[test]
    fn a_moved_window_makes_a_real_request_stale_through_live() {
        let recorder = crate::ax::Recorder::new(200, Vec::new());
        let ax: Rc<dyn crate::ax::Ax> = recorder.clone();
        let listed = windows_at(at(0.0, 0.0, 800.0, 600.0));
        let windows: Arc<dyn Windows> = listed.clone();
        let target = bound_target(recorder.started_at());

        let was = live(Some(&target), "t1", &windows, &ax).expect("bound and live");
        let observation = observation_of(&was);

        listed.change(20, |window| {
            window.bounds.x = 300.0;
            window.bounds.y = 120.0;
        });

        let now = live(Some(&target), "t1", &windows, &ax).expect("still bound");
        assert_eq!(now.geometry.origin_x, 300.0);
        assert_eq!(
            unmoved(&observation, &now).expect_err("it moved").code,
            "stale_observation"
        );
    }

    #[test]
    fn a_window_that_resized_or_changed_scale_is_stale_too() {
        let live = live_at(at(400.0, 300.0, 800.0, 600.0), 2.0);
        let observation = observation_of(&live);

        for changed in [
            live_at(at(400.0, 300.0, 900.0, 600.0), 2.0),
            live_at(at(400.0, 300.0, 800.0, 600.0), 1.0),
        ] {
            assert!(unmoved(&observation, &changed).is_err());
        }

        // A tenth of a point is two readings of one number, not a window that moved.
        let jittered = live_at(at(400.1, 300.0, 800.0, 600.0), 2.0);
        assert!(unmoved(&observation, &jittered).is_ok());
    }

    fn observation_of(live: &Live) -> Observation {
        Observation {
            id: "7c1e-1".to_string(),
            kind: crate::observation::Kind::Image,
            display_id: live.display_id,
            facts: live.facts,
            geometry: live.geometry,
            region: crate::geometry::Region::full(&live.geometry),
            sent_w: 100,
            sent_h: 50,
            captured_at_monotonic_ns: 0,
            frame_seq: Some(1),
            view_hash: None,
            elements: None,
        }
    }

    // The rule that keeps a click off somebody else's window. Nothing raises and
    // nothing activates: the refusal names what is there.
    #[test]
    fn a_covered_point_is_refused_and_says_what_is_in_front() {
        let live = live_at(at(0.0, 0.0, 800.0, 600.0), 2.0);
        let windows: Arc<dyn Windows> = windows_at(at(0.0, 0.0, 800.0, 600.0));

        assert!(unobstructed(&live, &windows, 400.0 as i32, 400).is_ok());

        let covering: Arc<dyn Windows> = WindowRecorder::new(vec![
            window(10, 100, at(300.0, 300.0, 200.0, 100.0)),
            window(20, 200, at(0.0, 0.0, 800.0, 600.0)),
        ]);
        let refusal = unobstructed(&live, &covering, 350, 350).expect_err("covered");
        assert_eq!(refusal.code, "target_obstructed");
        assert!(
            refusal.detail.unwrap().contains("Window 10"),
            "the refusal must name what is in front"
        );
    }

    // A window server that will not answer is the target being unusable, never an
    // empty desktop that looks like nothing is in the way.
    #[test]
    fn a_window_server_that_refuses_never_reads_as_nothing_in_the_way() {
        let live = live_at(at(0.0, 0.0, 800.0, 600.0), 2.0);
        let recorder = windows_at(at(0.0, 0.0, 800.0, 600.0));
        recorder.refuses("the window server refused");
        let windows: Arc<dyn Windows> = recorder;

        let refusal = unobstructed(&live, &windows, 400, 400).expect_err("nothing is known");
        assert_eq!(refusal.code, "target_unavailable");
    }

    // A reference into a process that has quit — or whose pid another process now
    // has — names nothing, and a target bound to it is gone with it.
    #[test]
    fn a_reused_pid_never_revives_a_target() {
        let recorder = crate::ax::Recorder::new(200, Vec::new());
        let ax: Rc<dyn crate::ax::Ax> = recorder.clone();
        let windows: Arc<dyn Windows> = windows_at(at(0.0, 0.0, 800.0, 600.0));
        let target = bound_target(recorder.started_at());

        assert!(live(Some(&target), "t1", &windows, &ax).is_ok());

        recorder.set_started_at(Some(999_999));
        let refusal = live(Some(&target), "t1", &windows, &ax).expect_err("another process");
        assert_eq!(refusal.code, "target_unavailable");

        recorder.set_started_at(None);
        assert_eq!(
            live(Some(&target), "t1", &windows, &ax)
                .expect_err("the process is gone")
                .code,
            "target_unavailable"
        );
    }

    #[test]
    fn a_minimized_target_says_so_rather_than_answering_an_old_frame() {
        let recorder = crate::ax::Recorder::new(200, Vec::new());
        let ax: Rc<dyn crate::ax::Ax> = recorder.clone();
        let listed = windows_at(at(0.0, 0.0, 800.0, 600.0));
        listed.change(20, |window| window.on_screen = false);
        let windows: Arc<dyn Windows> = listed;

        let refusal = live(
            Some(&bound_target(recorder.started_at())),
            "t1",
            &windows,
            &ax,
        )
        .expect_err("minimized");
        assert_eq!(refusal.code, "target_minimized");
    }

    #[test]
    fn a_target_id_nobody_bound_is_refused_by_name() {
        let recorder = crate::ax::Recorder::new(200, Vec::new());
        let ax: Rc<dyn crate::ax::Ax> = recorder.clone();
        let windows: Arc<dyn Windows> = windows_at(at(0.0, 0.0, 800.0, 600.0));

        assert_eq!(
            live(None, "t1", &windows, &ax)
                .expect_err("nothing bound")
                .code,
            "target_unavailable"
        );
        assert_eq!(
            live(
                Some(&bound_target(recorder.started_at())),
                "t9",
                &windows,
                &ax
            )
            .expect_err("a different binding")
            .code,
            "target_unavailable"
        );
    }

    /// A target bound to window 20 of pid 200, with a stream that has delivered one
    /// frame — enough for every liveness rule above.
    fn bound_target(started_at: u64) -> Target {
        let stream = FrameRecorder::new();
        stream.start(stream_window()).unwrap();
        stream.deliver_sized(1_000, Health::Live, 1600, 1200, 1600 * 4);

        Target {
            id: "t1".to_string(),
            generation: 1,
            window_id: 20,
            pid: 200,
            started_at,
            app: "App200".to_string(),
            title: "Window 20".to_string(),
            ax_binding: AxBinding::Unavailable,
            ax_window: None,
            siblings: vec![20],
            display_id: 7,
            facts: facts(),
            frames: Rc::new(TargetFrames::new(
                stream,
                Arc::new(crate::gate::SystemClock::new()),
            )),
            indicator: None,
            restarts: 0,
        }
    }

    // Reported, never guessed at: a window of the same application that was not
    // there at selection.
    #[test]
    fn a_sheet_the_application_opened_since_selection_is_reported_as_a_child() {
        let target = bound_target(1);
        let listed = WindowRecorder::new(vec![
            window(21, 200, at(100.0, 100.0, 300.0, 200.0)),
            window(20, 200, at(0.0, 0.0, 800.0, 600.0)),
            window(10, 100, at(0.0, 0.0, 200.0, 100.0)),
        ]);
        let windows: Arc<dyn Windows> = listed;

        let children = children(&target, &windows).expect("the window server answered");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["window_id"], json!(21));
    }

    // "The application opened nothing" and "nobody knows what it opened" are
    // opposite facts, and answering the first for the second tells the caller a
    // sheet it cannot see is not there.
    #[test]
    fn a_window_server_that_refuses_is_never_reported_as_no_children() {
        let target = bound_target(1);
        let recorder = windows_at(at(0.0, 0.0, 800.0, 600.0));
        recorder.refuses("the window server refused");
        let windows: Arc<dyn Windows> = recorder;

        let refusal = children(&target, &windows).expect_err("nothing is known");
        assert_eq!(refusal.code, "target_unavailable");
    }

    #[test]
    fn methods_offer_accessibility_only_where_a_window_was_bound() {
        assert_eq!(methods(AxBinding::Bound), vec!["foreground_hid", "ax"]);
        assert_eq!(methods(AxBinding::Ambiguous), vec!["foreground_hid"]);
        assert_eq!(methods(AxBinding::Unavailable), vec!["foreground_hid"]);
    }

    // Without a bound accessibility window there is nothing to walk from, and
    // walking the whole application would answer controls of windows the caller
    // never named.
    #[test]
    fn elements_on_an_unbound_target_refuses_rather_than_walking_the_application() {
        let refusal = ax_root(&bound_target(1)).expect_err("nothing to walk from");
        assert_eq!(refusal.code, "ax_binding_unavailable");
    }

    #[test]
    fn front_is_target_tells_this_application_from_another() {
        let target = bound_target(1);
        let windows: Arc<dyn Windows> = WindowRecorder::new(vec![
            window(20, 200, at(0.0, 0.0, 800.0, 600.0)),
            window(10, 100, at(0.0, 0.0, 200.0, 100.0)),
        ]);
        assert_eq!(front_is_target(&target, &windows), Some(true));

        let elsewhere: Arc<dyn Windows> = WindowRecorder::new(vec![
            window(10, 100, at(0.0, 0.0, 200.0, 100.0)),
            window(20, 200, at(0.0, 0.0, 800.0, 600.0)),
        ]);
        assert_eq!(front_is_target(&target, &elsewhere), Some(false));
    }

    /// A scripted application whose windows are the controls at these indices.
    fn scripted_app(labels: &[(&str, crate::ax::Frame)]) -> Rc<crate::ax::Recorder> {
        let elements = labels
            .iter()
            .map(|(label, frame)| crate::ax::Scripted::button(label, *frame))
            .collect();
        let recorder = crate::ax::Recorder::new(200, elements);
        recorder.windows_are(&(0..labels.len()).collect::<Vec<_>>());
        recorder
    }

    fn ax_frame(bounds: Bounds) -> crate::ax::Frame {
        crate::ax::Frame {
            x: bounds.x,
            y: bounds.y,
            w: bounds.w,
            h: bounds.h,
        }
    }

    // Exactly one of the application's windows matching, by title AND frame, is
    // what earns an accessibility binding — and the reference is retained, so the
    // walk that follows has a root that outlives the reply that made it.
    #[test]
    fn one_matching_accessibility_window_is_bound_and_offers_ax() {
        let bounds = at(0.0, 0.0, 800.0, 600.0);
        let recorder = scripted_app(&[
            ("Window 20", ax_frame(bounds)),
            ("Other", ax_frame(at(900.0, 0.0, 400.0, 300.0))),
        ]);
        let ax: Rc<dyn crate::ax::Ax> = recorder.clone();

        let (binding, bound) = bind_accessibility(&ax, &window(20, 200, bounds));

        assert_eq!(binding, AxBinding::Bound);
        assert!(bound.is_some());
        assert_eq!(methods(binding), vec!["foreground_hid", "ax"]);
        assert_eq!(recorder.counts().2, 1, "only the matched one is still held");

        drop(bound);
        assert_eq!(recorder.counts().2, 0, "and it goes back with the target");
    }

    // Two windows of one application that look identical: naming a control inside
    // "the" window would be a guess, so no `ax` is offered at all.
    #[test]
    fn two_identical_windows_are_ambiguous_and_offer_no_ax() {
        let bounds = at(0.0, 0.0, 800.0, 600.0);
        let recorder = scripted_app(&[
            ("Window 20", ax_frame(bounds)),
            ("Window 20", ax_frame(bounds)),
        ]);
        let ax: Rc<dyn crate::ax::Ax> = recorder.clone();

        let (binding, bound) = bind_accessibility(&ax, &window(20, 200, bounds));

        assert_eq!(binding, AxBinding::Ambiguous);
        assert!(bound.is_none());
        assert_eq!(methods(binding), vec!["foreground_hid"]);
        assert_eq!(recorder.counts().2, 0, "nothing ambiguous is held on to");
    }

    // A frame more than the tolerance away is a different window, however it is
    // titled.
    #[test]
    fn a_window_that_matches_by_title_alone_is_not_bound() {
        let bounds = at(0.0, 0.0, 800.0, 600.0);
        let recorder = scripted_app(&[("Window 20", ax_frame(at(40.0, 0.0, 800.0, 600.0)))]);
        let ax: Rc<dyn crate::ax::Ax> = recorder.clone();

        let (binding, bound) = bind_accessibility(&ax, &window(20, 200, bounds));

        assert_eq!(binding, AxBinding::Unavailable);
        assert!(bound.is_none());

        // Two points of disagreement is two readings of one rectangle, and IS bound.
        let near = scripted_app(&[("Window 20", ax_frame(at(1.5, 0.0, 800.0, 600.0)))]);
        let near_ax: Rc<dyn crate::ax::Ax> = near.clone();
        assert_eq!(
            bind_accessibility(&near_ax, &window(20, 200, bounds)).0,
            AxBinding::Bound
        );
    }

    // No badge, no working in somebody's window — and exactly one restart, after
    // which the refusal stands until the target is reselected.
    #[test]
    fn a_badge_that_never_came_up_refuses_every_mutation_and_is_restarted_once() {
        let mut target = bound_target(1);
        let launcher = crate::indicator::Scripted::new();
        let windows: Arc<dyn Windows> =
            WindowRecorder::new(vec![window(20, 200, at(0.0, 0.0, 800.0, 600.0))]);
        let gate = Gate::new(
            "boot-1".to_string(),
            Arc::new(crate::gate::SystemClock::new()),
        );
        let emitter = crate::capture::Emitter::capturing();

        let handle: Arc<dyn indicator::Launcher> = launcher.clone();
        let restarting = || Restarting {
            launcher: &handle,
            windows: &windows,
            gate: &gate,
            emitter: &emitter,
        };

        // No badge at all: refused, and one restart attempted.
        let refusal = control_surface(&mut target, restarting()).expect_err("no badge");
        assert_eq!(refusal.code, "control_surface_unavailable");
        assert_eq!(target.restarts, 1);
        assert_eq!(
            launcher.spawns(),
            1,
            "one restart, and it really started one"
        );

        // Still not ready, and the refusal now stands: no second restart.
        let refusal = control_surface(&mut target, restarting()).expect_err("still no badge");
        assert_eq!(refusal.code, "control_surface_unavailable");
        assert_eq!(target.restarts, 1);
        assert_eq!(launcher.spawns(), 1);

        // Once it says it is up, the same request is admitted.
        launcher.press(crate::indicator::Pressed::Ready);
        assert!(control_surface(&mut target, restarting()).is_ok());
    }

    // Only a mutation needs the badge. Refusing a look would leave a caller unable
    // to see why it was refused.
    #[test]
    fn only_a_mutating_action_needs_the_badge() {
        for action in [
            "left_click",
            "type",
            "press",
            "set_value",
            "scroll",
            "mouse_move",
        ] {
            assert!(needs_control_surface(action), "{action}");
        }
        for action in ["screenshot", "elements", "inspect"] {
            assert!(!needs_control_surface(action), "{action}");
        }
    }
}
