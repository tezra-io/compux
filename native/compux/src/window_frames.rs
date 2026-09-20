//! One window's frames, behind a trait — and the ScreenCaptureKit stream that
//! really produces them.
//!
//! ## Why a stream, and not a capture per look
//!
//! The display path captures a whole frame every time anybody looks, which is what
//! a settle's thirty samples cost. A window's frames arrive whether or not anyone
//! is looking, so the settle reads the latest one for free — and, decisively, every
//! frame carries its own sequence and the mach instant the window server DISPLAYED
//! it. That is what an after-action check needs and what a per-look capture cannot
//! give: proof that the picture being judged was shown AFTER the input went out,
//! rather than a picture that merely arrived after it.
//!
//! ## The slot, and what an idle sample means
//!
//! One frame is kept, the newest picture. There is no queue: a check wants what the
//! window looks like NOW, and a backlog of old frames is a backlog of wrong answers.
//!
//! ScreenCaptureKit delivers samples whether or not anything changed, and a sample
//! for an unchanged window carries **no pixels** and the status
//! `SCFrameStatusIdle` — "new frame was not generated because the display did not
//! change" (`SCStream.h`). That is the NORMAL state of a window somebody is reading
//! rather than typing in, so an idle sample must never replace the picture: [`Slot::note`] keeps the retained pixels and updates only the
//! status, the sequence and the timing. An idle sample that passes the fence is the
//! window server stating that nothing has changed since the dispatch — which is
//! exactly "stable, unchanged", and is an answer rather than a failure.
//!
//! ## The queue and the budget
//!
//! `queueDepth` is 3. The header documents the DEFAULT (8) and the ceiling ("should
//! not exceed 8 frames") and nothing lower, so a depth of two was a number this
//! build had invented; three is Apple's own published floor. That is three native
//! buffers SCK may hold, plus the one copy this slot keeps: four frames, which is
//! what [`MAX_CAPTURE_BYTES`] is measured against and what the budget refuses
//! BEFORE a frame is copied out. The latest-frame semantics every look above is
//! answered from are OURS and are unaffected by the depth.
//!
//! ## The fence
//!
//! [`Fence`] is the rule an after-action look is held to: a frame passes only when
//! its sequence is beyond the one the action was aimed in AND the window server
//! displayed it after the input was dispatched. Both, because either alone has a
//! hole — a sequence says the frame is newer than the picture the model read, and
//! the display time says the frame is newer than the ACTION.
//!
//! **Both halves of the time comparison are read on the mach absolute time base**,
//! the one `SCStreamFrameInfoDisplayTime` is stamped on, through
//! [`crate::gate::Clock::now_mach`]. Nothing converts between clocks: a conversion
//! nobody can verify without a display is exactly how a frame captured before the
//! input and delivered after it would slip through. A sample with no display time
//! is NOT admitted, and the caller's own deadline turns that into an answer.
//!
//! A frame that fails the fence is not an error: [`FrameError::NotYet`] means "ask
//! again", and only the caller's deadline turns it into `effect: unknown` with the
//! dispatch preserved.
//!
//! Everything above the adapter is proved through [`Recorder`], which models the
//! adapter's idle behaviour exactly — no pixels in an idle sample — so the suite
//! cannot pass on a story the real stream does not tell. The adapter itself is
//! compiled on every macOS build and started only on a real machine.

use std::sync::{Arc, Mutex};

use crate::window_server::Bounds;

/// The pixel format every frame arrives in. BGRA with a stride: each row is
/// `stride` bytes and `stride` is at least `width * 4`, because a surface pads its
/// rows to whatever alignment the hardware wants. Nothing here may assume a tight
/// buffer — slice 6's hash and the encoder both did, and a padded row read as
/// pixels is a picture of static.
pub const BYTES_PER_PIXEL: usize = 4;

/// At most ten frames a second. A settle samples faster than this and simply reads
/// the same frame twice, which is exactly right: the view has not changed.
pub const MAX_FRAMES_PER_SECOND: i32 = 10;

/// The stream's own queue depth: three native buffers SCK may hold. Three is the
/// lowest depth Apple's own guidance names; the latest-frame semantics this module
/// answers from are OURS and are unaffected by it.
pub const QUEUE_DEPTH: isize = 3;

/// How many frames' worth of memory one bound window may cost: the three native
/// buffers the stream's queue may hold, plus the one copy the slot keeps.
const FRAMES_IN_FLIGHT: usize = 4;

/// The ceiling on that. 512 MiB is four 128 MiB frames, and a 128 MiB frame is a
/// 4096x8192 window — larger than any window on any panel this runs on. It exists
/// so a configuration nobody anticipated is REFUSED rather than allocated: a frame
/// this big is a number that went wrong, and the caller is told so before the
/// allocation rather than after the machine has swapped.
pub const MAX_CAPTURE_BYTES: usize = 512 * 1024 * 1024;

/// How long a stream may say nothing AT ALL before a look stops trusting the
/// picture it is holding.
///
/// This is a BACKSTOP, not the way a dead stream is normally noticed: the delegate
/// hears `stream:didStopWithError:` and records the reason, which is both faster
/// and says why. This catches the case nothing reports — a stream that simply goes
/// quiet — because without it such a stream answers its last picture as the present
/// for as long as the target is held.
///
/// Five seconds, which is fifty frame intervals, and the margin is deliberate. The
/// header documents `SCFrameStatusIdle` as a sample delivered when the content did
/// not change, but it does not promise one every interval, and a bound tight enough
/// to be sharp would refuse a perfectly good static window on a cadence Apple never
/// published. If a live run ever reports `capture_unavailable` on a window nobody
/// is touching, this is the number, and LIVE_CHECK says so.
pub const SILENCE_LIMIT_MS: u64 = 5_000;

/// What the window server says about the content of one frame.
///
/// Only `Live` is a picture. The other three are the window server telling us this
/// sample is not one — and each is a different fact for a person to act on, which is
/// why they are not one `bad` variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// A complete frame of the window.
    Live,
    /// The window has not changed, so the server sent no new pixels. Not an error,
    /// and not rare: it is what a window nobody is typing in reports ten times a
    /// second. The last picture is still what the window looks like.
    Idle,
    /// The window is minimized or otherwise off screen; there is nothing to show.
    Minimized,
    /// The stream ended — the window closed, the application quit, the server
    /// stopped it.
    Disconnected,
}

/// One sample that is not a picture: everything it carries except pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub status: Health,
    pub seq: u64,
    /// When the sample reached this process, on the gate's monotonic clock.
    pub captured_at_ns: u128,
    /// When the window server displayed it, on the mach absolute time base.
    pub display_time_mach: Option<u64>,
}

/// One frame of one window.
///
/// The picture's own facts — its pixel size, its sequence, when it was displayed —
/// come from the frame itself. Its place on the desktop does NOT: see
/// [`crate::target::transform`], which builds the mapping from the window server's
/// bounds and a scale MEASURED against this frame, because neither attachment below
/// is the quantity a naive reading takes it for.
#[derive(Clone, Debug)]
pub struct WindowFrame {
    /// BGRA, `stride` bytes per row.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    /// When this frame reached this process, on the gate's own monotonic clock.
    /// Used for the silence bound, never for the fence.
    pub captured_at_ns: u128,
    /// When the window server DISPLAYED it, on the mach absolute time base — the
    /// fence's time half. `None` when the sample carried no display time, which is
    /// never admitted rather than guessed at.
    pub display_time_mach: Option<u64>,
    /// One counter per stream, increasing. The fence's hard half.
    pub seq: u64,
    /// `SCStreamFrameInfoContentRect`: where the content sits **inside the
    /// surface**, in points — NOT where the window is on the desktop. It is read
    /// only to detect a surface whose content does not fill it, and never to place
    /// a coordinate.
    pub content_rect: Option<Bounds>,
    /// `SCStreamFrameInfoScaleFactor`: the DISPLAY's pixels-per-point as SCK
    /// reported it ("display resolution ... the pixel to point scaling factor",
    /// `SCStream.h`). Recorded for the diagnosis in a mismatch refusal and used for
    /// nothing else — the surface is requested in pixels and the real ratio is
    /// measured, because the two disagree whenever the request and the panel do.
    /// (`SCStreamFrameInfoContentScale` is the nearer quantity, and it is still a
    /// number to check a measurement against rather than one to map through.)
    pub reported_scale: Option<f32>,
    pub status: Health,
}

impl WindowFrame {
    /// The bytes one frame of this shape costs, and the refusal when that is more
    /// than a bound window may have. Checked BEFORE anything is allocated, which is
    /// the only place the check is worth anything.
    pub fn budget(stride: usize, height: u32) -> Result<usize, FrameError> {
        let bytes = stride.saturating_mul(height as usize);
        let total = bytes.saturating_mul(FRAMES_IN_FLIGHT);

        if total > MAX_CAPTURE_BYTES {
            return Err(FrameError::BudgetExceeded(format!(
                "one frame of this window is {bytes} bytes ({stride} per row x {height} rows), \
                 and {FRAMES_IN_FLIGHT} of them are {total}, past the {MAX_CAPTURE_BYTES} byte \
                 budget one bound window may have"
            )));
        }

        Ok(bytes)
    }

    /// A sample that is not a picture, kept as a frame so the slot can report it
    /// when there are no pixels to keep instead.
    pub fn without_pixels(sample: Sample) -> WindowFrame {
        WindowFrame {
            pixels: Vec::new(),
            width: 0,
            height: 0,
            stride: 0,
            captured_at_ns: sample.captured_at_ns,
            display_time_mach: sample.display_time_mach,
            seq: sample.seq,
            content_rect: None,
            reported_scale: None,
            status: sample.status,
        }
    }

    /// The frame's pixels as the rest of this process reads them: tight RGBA rows,
    /// row padding dropped and the channels put in order.
    ///
    /// Two conversions in one pass on purpose. Slice 6's `view_hash` reads raw rows
    /// out of an `RgbaImage` in place and the encoder writes `as_raw()` straight
    /// out, so both assume a tight buffer in RGBA order; handing either a padded
    /// BGRA surface produces a hash of the padding and a picture with its colours
    /// swapped. One function, at the boundary, is the only place that can be true.
    pub fn to_rgba(&self) -> Result<image::RgbaImage, FrameError> {
        let (width, height) = (self.width as usize, self.height as usize);
        let needed = width * BYTES_PER_PIXEL;

        if self.stride < needed || self.pixels.len() < self.stride * height || width == 0 {
            return Err(FrameError::Unavailable(format!(
                "the frame does not describe a picture: {}x{} at {} bytes a row in {} bytes",
                self.width,
                self.height,
                self.stride,
                self.pixels.len()
            )));
        }

        let mut out = Vec::with_capacity(needed * height);
        for row in 0..height {
            let start = row * self.stride;
            let (pixels, _rest) = self.pixels[start..start + needed].as_chunks::<BYTES_PER_PIXEL>();
            for pixel in pixels {
                out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        }

        image::RgbaImage::from_raw(self.width, self.height, out).ok_or_else(|| {
            FrameError::Unavailable("the converted frame is not a whole image".to_string())
        })
    }
}

/// What an after-action look will accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
    /// The instant the input was dispatched, on the MACH absolute time base — the
    /// same base a frame's display time arrives on.
    pub after_mach: u64,
    /// The sequence of the frame the action was aimed in.
    pub beyond_seq: u64,
}

impl Fence {
    /// Does this frame answer the question the fence is guarding?
    ///
    /// Both halves, and the module doc says why neither is enough alone. A sample
    /// with no display time is not admitted: absent is not "probably after".
    pub fn admits(&self, frame: &WindowFrame) -> bool {
        let shown_after = match frame.display_time_mach {
            Some(shown) => shown > self.after_mach,
            None => false,
        };

        frame.seq > self.beyond_seq && shown_after
    }
}

/// Why a look could not answer with a picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The stream cannot answer at all, and will not without being restarted.
    Unavailable(String),
    /// The window is minimized or off screen. Its own code, because the caller's
    /// remedy — ask the person to bring the window back — is nothing like the
    /// remedy for a capture that broke.
    Minimized(String),
    /// Screen Recording was refused (`SCStreamErrorUserDeclined`). Its own code,
    /// because it is an operator action in System Settings and nothing the caller
    /// can retry its way out of.
    Denied(String),
    /// A frame of this shape is more memory than a bound window may have.
    BudgetExceeded(String),
    /// No frame has passed the fence YET. Not a failure: the caller polls, and only
    /// its own deadline turns this into an answer.
    NotYet,
}

impl FrameError {
    /// The wire code, and the sentence beside it.
    pub fn code(&self) -> &'static str {
        match self {
            FrameError::Unavailable(_) => "capture_unavailable",
            FrameError::Minimized(_) => "target_minimized",
            FrameError::Denied(_) => "screen_recording_not_granted",
            FrameError::BudgetExceeded(_) => "capture_budget_exceeded",
            FrameError::NotYet => "capture_unavailable",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            FrameError::Unavailable(detail)
            | FrameError::Minimized(detail)
            | FrameError::Denied(detail)
            | FrameError::BudgetExceeded(detail) => detail.clone(),
            FrameError::NotYet => {
                "no frame of that window arrived after the action was dispatched".to_string()
            }
        }
    }
}

impl Health {
    /// The refusal a sample in this state answers with, or `None` when the picture
    /// the slot holds is still what the window looks like.
    fn refusal(self) -> Option<FrameError> {
        match self {
            Health::Live | Health::Idle => None,
            Health::Minimized => Some(FrameError::Minimized(
                "the window is minimized or on another desktop, so there is nothing to see in it"
                    .to_string(),
            )),
            Health::Disconnected => Some(FrameError::Unavailable(
                "the window server stopped sending frames of that window".to_string(),
            )),
        }
    }
}

/// The window a stream is opened on, and the surface it is opened at.
///
/// The size is in REAL PIXELS, because `SCStreamConfiguration`'s width and height
/// are documented in pixels while `SCWindow.frame` is in points: asking for the
/// point size on a Retina panel produces a surface at one pixel per point and a
/// picture at half the resolution the window is drawn at. It is a REQUEST, never a
/// promise — the transform measures what really arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamWindow {
    /// `CGWindowID`, as a `windows` listing publishes it.
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

/// One window's frames.
pub trait WindowFrames {
    /// Open the stream. `Err` is a start that failed, and nothing is left running.
    fn start(&self, window: StreamWindow) -> Result<(), FrameError>;

    /// The newest frame, or why there is not one. `after` is the fence an
    /// after-action look is held to; a look with none takes whatever is there.
    fn latest(&self, after: Option<Fence>) -> Result<WindowFrame, FrameError>;

    /// Close the stream and let go of every buffer. Idempotent, because it is
    /// called from `Drop` as well as from a release.
    fn stop(&self);
}

// --- the slot -----------------------------------------------------------------

/// The one picture kept, the sequence counter that stamps every sample, and any
/// standing condition the delivery side recorded.
///
/// Shared between the thread the window server delivers on and the action worker,
/// so it is a mutex — held for exactly as long as it takes to swap one `Option`,
/// never across a capture, a conversion or a native call.
#[derive(Default)]
pub struct Slot {
    held: Mutex<Option<WindowFrame>>,
    seq: Mutex<u64>,
    /// A fault the delivery side cannot answer from — an over-budget frame, a
    /// stream the server stopped, a reader that panicked. Recorded here because
    /// the callback has no caller to return to, and reported by the next look:
    /// otherwise the typed refusal is unreachable and the caller sees only silence.
    condition: Mutex<Option<FrameError>>,
}

impl Slot {
    pub fn new() -> Arc<Slot> {
        Arc::new(Slot::default())
    }

    /// The next sequence number. Taken before a frame is built so the number is
    /// this stream's own count of samples delivered, whatever happens to the sample
    /// afterwards.
    pub fn next_seq(&self) -> u64 {
        let mut seq = self.lock_seq();
        *seq += 1;
        *seq
    }

    /// Keep this picture, dropping whatever was there. The old frame's memory goes
    /// back HERE, which is what keeps one bound window to one retained copy.
    pub fn offer(&self, frame: WindowFrame) {
        *self.lock_held() = Some(frame);
    }

    /// A sample that carried no pixels.
    ///
    /// The picture is KEPT and only the status, the sequence and the timing move
    /// on. An idle sample is the window server saying "unchanged", so the frame it
    /// is unchanged from is still the answer — and it now carries a sequence and a
    /// display time past the fence, which is what makes "the action changed
    /// nothing" a stable, unchanged reading instead of a timeout.
    pub fn note(&self, sample: Sample) {
        let mut held = self.lock_held();

        match held.as_mut() {
            Some(frame) => {
                frame.status = sample.status;
                frame.seq = sample.seq;
                frame.captured_at_ns = sample.captured_at_ns;
                frame.display_time_mach = sample.display_time_mach;
            }
            // Nothing has been captured yet, so there are no pixels to keep. An
            // idle sample here is "unchanged" about a picture nobody has, which is
            // `NotYet` and resolves itself on the first live frame; a minimized or
            // stopped one is a standing fact and is kept so the look can report it.
            None if sample.status == Health::Idle => {}
            None => *held = Some(WindowFrame::without_pixels(sample)),
        }
    }

    /// Record a fault the delivery side could not answer from. The FIRST one
    /// stands: it is the cause, and the ones after it are usually its consequences.
    pub fn fault(&self, error: FrameError) {
        let mut condition = self.lock_condition();
        if condition.is_none() {
            *condition = Some(error);
        }
    }

    /// The frame the slot holds, if there is nothing standing against it and it
    /// satisfies the fence.
    ///
    /// The frame is CLONED rather than taken: a settle looks many times and a view
    /// that has stopped changing must keep answering the same picture, not answer
    /// once and then report that nothing arrived.
    pub fn peek(&self, after: Option<Fence>, now_ns: u128) -> Result<WindowFrame, FrameError> {
        if let Some(condition) = self.lock_condition().clone() {
            return Err(condition);
        }

        let held = self.lock_held();
        let Some(frame) = held.as_ref() else {
            return Err(FrameError::NotYet);
        };

        if let Some(refusal) = frame.status.refusal() {
            return Err(refusal);
        }

        // A stream that has stopped saying anything at all, without a status and
        // without an error, must not keep answering its last picture as the present.
        let silent_ns = now_ns.saturating_sub(frame.captured_at_ns);
        if silent_ns > u128::from(SILENCE_LIMIT_MS) * 1_000_000 {
            return Err(FrameError::Unavailable(format!(
                "the window capture has sent nothing for {} ms, past the {SILENCE_LIMIT_MS} ms \
                 this build waits before it stops trusting the last frame",
                silent_ns / 1_000_000
            )));
        }

        match after {
            Some(fence) if !fence.admits(frame) => Err(FrameError::NotYet),
            _admitted => Ok(frame.clone()),
        }
    }

    /// Let go of the frame and of any standing condition. Called when the stream
    /// stops, so a released target holds no pixels and a restarted one is not
    /// refused for the fault of the stream before it.
    pub fn clear(&self) {
        *self.lock_held() = None;
        *self.lock_condition() = None;
    }

    // A poisoned slot means a delivery thread panicked holding it. Recovering is
    // right here: what it holds is one picture, one counter and one fault.
    fn lock_held(&self) -> std::sync::MutexGuard<'_, Option<WindowFrame>> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_seq(&self) -> std::sync::MutexGuard<'_, u64> {
        self.seq.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_condition(&self) -> std::sync::MutexGuard<'_, Option<FrameError>> {
        self.condition.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// --- the ScreenCaptureKit adapter ---------------------------------------------

#[cfg(target_os = "macos")]
pub mod sck {
    //! The one place ScreenCaptureKit is touched.
    //!
    //! It is COMPILED on every macOS build and started only on a real machine with
    //! a display session and the Screen Recording grant: opening a stream captures
    //! somebody's screen, and asking `SCShareableContent` what is capturable raises
    //! the permission prompt. Everything above it is proved through [`super::Recorder`].
    //!
    //! macOS 13 is the floor and nothing here is newer: `SCShareableContent`,
    //! `SCContentFilter`, `SCStream`, `SCStreamDelegate` and the frame-info
    //! attachment keys are all 12.3, and `SCScreenshotManager` — which would have
    //! been simpler — is 14 and is therefore not used.

    use std::ffi::c_void;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchRetained};
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{define_class, msg_send, AnyThread, DefinedClass};
    use objc2_core_foundation::{CFDictionary, CFNumber, CGRect};
    use objc2_core_graphics::CGRectMakeWithDictionaryRepresentation;
    use objc2_core_media::CMSampleBuffer;
    use objc2_core_video::{
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
        CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };
    use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString};
    use objc2_screen_capture_kit::{
        SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
        SCStreamErrorCode, SCStreamFrameInfoContentRect, SCStreamFrameInfoDisplayTime,
        SCStreamFrameInfoScaleFactor, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
        SCWindow,
    };

    use super::{Fence, FrameError, Health, Sample, Slot, StreamWindow, WindowFrame, WindowFrames};
    use crate::gate::Clock;
    use crate::window_server::Bounds;

    /// `kCVPixelFormatType_32BGRA`, as a FourCC.
    const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

    /// How long the three asynchronous steps may take: asking what is capturable,
    /// starting the stream, and stopping it. Each is one round trip to the window
    /// server on a healthy machine; an unbounded wait here would hold the action
    /// worker for as long as the OS felt like it.
    const HANDSHAKE_MS: u64 = 5_000;

    /// The ivars of the delivery object: the slot every frame lands in, and the
    /// clock that stamps the moment it arrived.
    struct Delivery {
        slot: Arc<Slot>,
        clock: Arc<dyn Clock>,
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, this class implements
        // no `Drop`, and its ivars are `Arc`s of `Send + Sync` values — which they
        // must be, because the window server calls it on its own queue.
        #[unsafe(super(NSObject))]
        #[name = "CompuxWindowFrameOutput"]
        #[ivars = Delivery]
        struct Output;

        unsafe impl NSObjectProtocol for Output {}

        unsafe impl SCStreamOutput for Output {
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            unsafe fn delivered(
                &self,
                _stream: &SCStream,
                buffer: &CMSampleBuffer,
                kind: SCStreamOutputType,
            ) {
                if kind != SCStreamOutputType::Screen {
                    return;
                }
                let delivery = self.ivars();

                // A Rust panic unwinding into Objective-C frames is undefined
                // behaviour and objc2's catch-all is off, so it is caught HERE, at
                // the boundary. A fault the callback cannot return from is recorded
                // in the slot: a stream whose reader died must refuse, not answer
                // the picture from before it died for the rest of the session.
                let caught = catch_unwind(AssertUnwindSafe(|| unsafe {
                    take_sample(buffer, delivery)
                }));

                if caught.is_err() {
                    delivery.slot.fault(FrameError::Unavailable(
                        "the window capture's frame reader faulted; the target must be selected \
                         again"
                            .to_string(),
                    ));
                }
            }
        }

        unsafe impl SCStreamDelegate for Output {
            /// The window server stopped the stream: the window closed, the display
            /// went away, the grant was withdrawn. Without this the slot keeps
            /// answering the last frame as Live for as long as the target is held.
            #[unsafe(method(stream:didStopWithError:))]
            unsafe fn stopped(&self, _stream: &SCStream, error: &NSError) {
                self.ivars()
                    .slot
                    .fault(classify(error, "the window capture stopped"));
            }
        }
    );

    impl Output {
        fn new(slot: Arc<Slot>, clock: Arc<dyn Clock>) -> Retained<Output> {
            let this = Output::alloc().set_ivars(Delivery { slot, clock });
            unsafe { msg_send![super(this), init] }
        }
    }

    /// An `NSError` from ScreenCaptureKit as this process reports it.
    ///
    /// `SCStreamErrorUserDeclined` is the one that earns its own code: it is the
    /// person not having granted Screen Recording, it is the single most likely
    /// first failure on a fresh machine, and the remedy is a trip to System
    /// Settings rather than anything a caller can retry.
    fn classify(error: &NSError, what: &str) -> FrameError {
        if error.code() == SCStreamErrorCode::UserDeclined.0 {
            return FrameError::Denied(
                "Screen Recording is not granted to this helper, so no window can be captured. \
                 The user grants it in System Settings, Privacy & Security, Screen Recording, \
                 and computer use has to be restarted afterwards"
                    .to_string(),
            );
        }

        FrameError::Unavailable(format!("{what}: {error}"))
    }

    /// One sample out of one buffer, into the slot.
    ///
    /// Every native resource this touches is released on every path: the pixel
    /// buffer's base address is unlocked before the function returns, including
    /// the one early return between the lock and the copy, and every CF value the
    /// attachments hand back is borrowed from the buffer and never released here.
    unsafe fn take_sample(buffer: &CMSampleBuffer, delivery: &Delivery) {
        let info = unsafe { frame_info(buffer) };
        let sample = Sample {
            status: info.status,
            seq: delivery.slot.next_seq(),
            captured_at_ns: delivery.clock.now_ns(),
            display_time_mach: info.display_time_mach,
        };

        // Not a picture: the retained pixels STAY and only the sample's own facts
        // move on. An idle sample is the normal state of an unchanged window, and
        // replacing the picture with an empty frame here would blank the target
        // every time the person stopped typing.
        if info.status != Health::Live {
            delivery.slot.note(sample);
            return;
        }

        let Some(image) = (unsafe { buffer.image_buffer() }) else {
            return;
        };
        let pixels: &objc2_core_video::CVPixelBuffer = &image;

        let width = CVPixelBufferGetWidth(pixels) as u32;
        let height = CVPixelBufferGetHeight(pixels) as u32;
        let stride = CVPixelBufferGetBytesPerRow(pixels);
        // Checked BEFORE the lock and before the copy, which is the only point at
        // which refusing costs nothing — and recorded, because a callback has no
        // caller to hand the refusal to.
        if let Err(over) = WindowFrame::budget(stride, height) {
            delivery.slot.fault(over);
            return;
        }

        if unsafe { CVPixelBufferLockBaseAddress(pixels, CVPixelBufferLockFlags::ReadOnly) } != 0 {
            return;
        }

        let base = CVPixelBufferGetBaseAddress(pixels);
        let copied = if base.is_null() {
            None
        } else {
            let bytes = stride * height as usize;
            // SAFETY: the buffer is locked for reading, and `stride * height` is
            // exactly the region CoreVideo describes.
            Some(unsafe { std::slice::from_raw_parts(base as *const u8, bytes) }.to_vec())
        };

        // Unlocked on BOTH paths, before anything can return.
        unsafe { CVPixelBufferUnlockBaseAddress(pixels, CVPixelBufferLockFlags::ReadOnly) };

        let Some(pixels) = copied else {
            return;
        };

        delivery.slot.offer(WindowFrame {
            pixels,
            width,
            height,
            stride,
            captured_at_ns: sample.captured_at_ns,
            display_time_mach: sample.display_time_mach,
            seq: sample.seq,
            content_rect: info.content_rect,
            reported_scale: info.scale,
            status: Health::Live,
        });
    }

    /// What the window server attached to this frame: its verdict, when it was
    /// displayed, the rectangle the content covers inside the surface, and the
    /// display's pixels-per-point.
    ///
    /// All four in ONE pass, under one borrow of the attachments array, because
    /// they are four keys of one dictionary and reading them separately would
    /// retain and release it four times for no reason.
    struct FrameInfo {
        status: Health,
        display_time_mach: Option<u64>,
        content_rect: Option<Bounds>,
        scale: Option<f32>,
    }

    unsafe fn frame_info(buffer: &CMSampleBuffer) -> FrameInfo {
        let unreadable = FrameInfo {
            // Unreadable means we cannot say this is a picture, so it is treated as
            // one that is not: a blank frame published as live is exactly the
            // misleading check this slice exists to remove.
            status: Health::Disconnected,
            display_time_mach: None,
            content_rect: None,
            scale: None,
        };

        let Some(attachments) = (unsafe { buffer.sample_attachments_array(false) }) else {
            return unreadable;
        };
        if attachments.count() < 1 {
            return unreadable;
        }
        let first = unsafe { attachments.value_at_index(0) };
        if first.is_null() {
            return unreadable;
        }
        // SAFETY: the array is retained for this scope and CoreMedia documents its
        // elements as CFDictionaries, one per sample.
        let dictionary = unsafe { &*(first as *const CFDictionary) };

        FrameInfo {
            status: unsafe { read_status(dictionary) },
            display_time_mach: unsafe { read_display_time(dictionary) },
            content_rect: unsafe { read_content_rect(dictionary) },
            scale: unsafe { read_scale(dictionary) },
        }
    }

    /// `SCFrameStatus`: 0 complete, 1 idle, 2 blank, 3 suspended, 4 started,
    /// 5 stopped.
    unsafe fn read_status(dictionary: &CFDictionary) -> Health {
        let Some(number) = (unsafe { number_for(dictionary, SCStreamFrameInfoStatus) }) else {
            return Health::Disconnected;
        };

        match number.as_i32() {
            Some(0) | Some(4) => Health::Live,
            Some(1) => Health::Idle,
            Some(2) | Some(3) => Health::Minimized,
            _stopped_or_unreadable => Health::Disconnected,
        }
    }

    /// `SCStreamFrameInfoDisplayTime`: "the mach absolute time when the frame was
    /// displayed by the window server". The fence's time half, in the units the
    /// gate's `now_mach` reads — nothing converts it.
    unsafe fn read_display_time(dictionary: &CFDictionary) -> Option<u64> {
        let shown = unsafe { number_for(dictionary, SCStreamFrameInfoDisplayTime) }?.as_i64()?;

        u64::try_from(shown).ok()
    }

    unsafe fn read_content_rect(dictionary: &CFDictionary) -> Option<Bounds> {
        let value = unsafe { value_for(dictionary, SCStreamFrameInfoContentRect) }?;
        // SAFETY: the key documents its value as a CGRect dictionary representation.
        let rect_dictionary = unsafe { &*(value as *const CFDictionary) };

        let mut rect = CGRect::default();
        let read =
            unsafe { CGRectMakeWithDictionaryRepresentation(Some(rect_dictionary), &mut rect) };

        if read && rect.size.width > 0.0 && rect.size.height > 0.0 {
            Some(Bounds {
                x: rect.origin.x,
                y: rect.origin.y,
                w: rect.size.width,
                h: rect.size.height,
            })
        } else {
            None
        }
    }

    unsafe fn read_scale(dictionary: &CFDictionary) -> Option<f32> {
        let scale = unsafe { number_for(dictionary, SCStreamFrameInfoScaleFactor) }?.as_f64()?;

        if scale.is_finite() && scale > 0.0 {
            Some(scale as f32)
        } else {
            None
        }
    }

    unsafe fn number_for<'a>(dictionary: &'a CFDictionary, key: &NSString) -> Option<&'a CFNumber> {
        let value = unsafe { value_for(dictionary, key) }?;
        // SAFETY: each of these keys documents its value as a CFNumber.
        Some(unsafe { &*(value as *const CFNumber) })
    }

    /// One value out of the frame-info dictionary. The `SCStreamFrameInfo*` keys
    /// are `NSString`s, which are toll-free bridged to `CFString` and are therefore
    /// the dictionary's own key type.
    unsafe fn value_for(dictionary: &CFDictionary, key: &NSString) -> Option<*const c_void> {
        let value = unsafe { dictionary.value(key as *const NSString as *const c_void) };

        if value.is_null() {
            None
        } else {
            Some(value)
        }
    }

    /// One bound window's stream.
    ///
    /// Everything native it owns — the stream, the delivery object, the queue — is
    /// in one `Option`, so [`Stream::stop`] and `Drop` are the same two lines and
    /// there is no path that releases one of them and not the others.
    pub struct Stream {
        slot: Arc<Slot>,
        clock: Arc<dyn Clock>,
        live: Mutex<Option<Live>>,
    }

    struct Live {
        stream: Retained<SCStream>,
        output: Retained<Output>,
        _queue: DispatchRetained<DispatchQueue>,
    }

    impl Stream {
        pub fn new(clock: Arc<dyn Clock>) -> Stream {
            Stream {
                slot: Slot::new(),
                clock,
                live: Mutex::new(None),
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Option<Live>> {
            self.live.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    impl WindowFrames for Stream {
        fn start(&self, window: StreamWindow) -> Result<(), FrameError> {
            // Replacing a stream stops the old one first: two streams on one helper
            // would be two sets of native buffers and one of them unreachable.
            self.stop();

            let content = shareable_content()?;
            let target = window_named(&content, window.id)?;
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &target)
            };

            let configuration = unsafe { SCStreamConfiguration::new() };
            unsafe {
                // In PIXELS, which is what these two are documented in — the window
                // is measured in points and this is its size on the panel it is on.
                // It is a request and nothing downstream trusts it: the transform
                // measures the ratio the surface really came back at.
                configuration.setWidth(window.width.max(1) as usize);
                configuration.setHeight(window.height.max(1) as usize);
                configuration.setPixelFormat(PIXEL_FORMAT_BGRA);
                configuration.setQueueDepth(super::QUEUE_DEPTH);
                configuration.setShowsCursor(false);
                configuration.setMinimumFrameInterval(objc2_core_media::CMTime {
                    value: 1,
                    timescale: super::MAX_FRAMES_PER_SECOND,
                    flags: objc2_core_media::CMTimeFlags::Valid,
                    epoch: 0,
                });
            }

            let output = Output::new(self.slot.clone(), self.clock.clone());
            let queue = DispatchQueue::new("io.tezra.compux.window-frames", None);
            // The SAME object is the delegate, so a stream the server stops reaches
            // the slot instead of being silence the next look reads as a picture.
            let delegate = ProtocolObject::from_ref(&*output);
            let stream = unsafe {
                SCStream::initWithFilter_configuration_delegate(
                    SCStream::alloc(),
                    &filter,
                    &configuration,
                    Some(delegate),
                )
            };

            let protocol = ProtocolObject::from_ref(&*output);
            unsafe {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        protocol,
                        SCStreamOutputType::Screen,
                        Some(&queue),
                    )
                    .map_err(|error| {
                        classify(&error, "the window capture's frame output was refused")
                    })?;
            }

            await_start(&stream)?;

            *self.lock() = Some(Live {
                stream,
                output,
                _queue: queue,
            });

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
            // Taken out of the mutex first, so nothing is held across the OS calls
            // and a second `stop` finds nothing to do.
            let live = self.lock().take();

            if let Some(live) = live {
                // The output goes FIRST: after this the server has nothing to call
                // back into, which is what makes dropping the delivery object below
                // safe. Then the stop is WAITED ON, bounded — a callback already
                // running on the sample queue would otherwise still be inside the
                // object this function is about to release.
                let protocol = ProtocolObject::from_ref(&*live.output);
                unsafe {
                    let _ = live
                        .stream
                        .removeStreamOutput_type_error(protocol, SCStreamOutputType::Screen);
                }
                await_stop(&live.stream);
            }
            self.slot.clear();
        }
    }

    /// Every path: the stream is stopped and the buffers go back, whether this was
    /// a release, a replacement, a target that went away, or the process exiting.
    impl Drop for Stream {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// What is capturable, bounded. `SCShareableContent` is asynchronous and this
    /// is the one place it is waited on.
    fn shareable_content() -> Result<Retained<SCShareableContent>, FrameError> {
        let (tx, rx) = mpsc::channel::<Result<Retained<SCShareableContent>, FrameError>>();

        let handler = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                let answer = match unsafe { content.as_ref() } {
                    Some(content) => Ok(Retained::from(content)),
                    None => Err(match unsafe { error.as_ref() } {
                        Some(error) => classify(
                            error,
                            "the window server would not say what is \
                                                        capturable",
                        ),
                        None => FrameError::Unavailable(
                            "the window server answered nothing".to_string(),
                        ),
                    }),
                };
                let _ = tx.send(answer);
            },
        );

        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true,
                false,
                &handler,
            );
        }

        match rx.recv_timeout(Duration::from_millis(HANDSHAKE_MS)) {
            Ok(answer) => answer,
            Err(_timeout) => Err(FrameError::Unavailable(
                "the window server did not say what is capturable in time".to_string(),
            )),
        }
    }

    fn window_named(
        content: &SCShareableContent,
        id: u32,
    ) -> Result<Retained<SCWindow>, FrameError> {
        let windows: Retained<NSArray<SCWindow>> = unsafe { content.windows() };

        windows
            .iter()
            .find(|window| unsafe { window.windowID() } == id)
            .ok_or_else(|| {
                FrameError::Unavailable(format!("window {id} is not one the window server offers"))
            })
    }

    /// Start the stream and wait for its answer, bounded. A start that failed
    /// leaves nothing running, which is why the caller stores the stream only after
    /// this returns.
    fn await_start(stream: &SCStream) -> Result<(), FrameError> {
        let (tx, rx) = mpsc::channel::<Option<FrameError>>();

        let handler = RcBlock::new(move |error: *mut NSError| {
            let _ = tx.send(
                unsafe { error.as_ref() }
                    .map(|error| classify(error, "the window capture did not start")),
            );
        });

        unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };

        match rx.recv_timeout(Duration::from_millis(HANDSHAKE_MS)) {
            Ok(None) => Ok(()),
            Ok(Some(refusal)) => Err(refusal),
            Err(_timeout) => Err(FrameError::Unavailable(
                "the window capture did not start in time".to_string(),
            )),
        }
    }

    /// Stop the stream and WAIT for the server to say it has, bounded.
    ///
    /// Waited on rather than fired and forgotten: the delivery object is released
    /// the moment this returns, and a sample callback still running on the queue
    /// would be inside it. The answer itself is nothing to act on — the target is
    /// going either way — so a timeout is not an error, it is this build declining
    /// to wait any longer.
    fn await_stop(stream: &SCStream) {
        let (tx, rx) = mpsc::channel::<()>();

        let handler = RcBlock::new(move |_error: *mut NSError| {
            let _ = tx.send(());
        });

        unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };

        let _ = rx.recv_timeout(Duration::from_millis(HANDSHAKE_MS));
    }
}

/// Linux has no window capture this build speaks. A typed refusal rather than an
/// empty stream: a caller that cannot tell "nothing arrived" from "not supported
/// here" writes the wrong sentence about both. [M42.1] owns the Linux half.
#[cfg(not(target_os = "macos"))]
pub struct Unsupported;

#[cfg(not(target_os = "macos"))]
impl WindowFrames for Unsupported {
    fn start(&self, _window: StreamWindow) -> Result<(), FrameError> {
        Err(FrameError::Unavailable(
            "window capture is only supported on macOS".to_string(),
        ))
    }

    fn latest(&self, _after: Option<Fence>) -> Result<WindowFrame, FrameError> {
        Err(FrameError::Unavailable(
            "window capture is only supported on macOS".to_string(),
        ))
    }

    fn stop(&self) {}
}

// --- the recording implementation (tests, and every rule above the adapter) ----

/// A window's frames, written down by a test.
///
/// Every rule this module has — the fence, the budget, the health codes, the slot
/// keeping the picture through an idle sample, the stream being stopped on every
/// path — is proved against this, because the real stream captures somebody's
/// screen and cannot be started in a test.
///
/// It models the adapter, not a convenient fiction: an idle sample here carries no
/// pixels and goes through [`Slot::note`], exactly as one does on a real machine.
/// The recorder's own "now" is the last sample's arrival, so a test that wants the
/// silence bound has to say so with [`Recorder::goes_silent_for`].
#[cfg(test)]
pub struct Recorder {
    slot: Arc<Slot>,
    started: Mutex<Vec<StreamWindow>>,
    stops: Mutex<usize>,
    running: Mutex<bool>,
    refuse_start: Mutex<Option<FrameError>>,
    now_ns: Mutex<u128>,
}

#[cfg(test)]
impl Recorder {
    pub fn new() -> std::rc::Rc<Recorder> {
        std::rc::Rc::new(Recorder {
            slot: Slot::new(),
            started: Mutex::new(Vec::new()),
            stops: Mutex::new(0),
            running: Mutex::new(false),
            refuse_start: Mutex::new(None),
            now_ns: Mutex::new(0),
        })
    }

    /// Deliver one live frame, exactly as the window server would: the slot stamps
    /// it with the next sequence and the caller says when it arrived.
    pub fn deliver(&self, at_ns: u128, status: Health) -> u64 {
        self.deliver_sized(at_ns, status, 4, 2, 16)
    }

    /// A frame of a stated shape, so the stride and budget rules are testable.
    ///
    /// A status that is not `Live` takes the adapter's own path: no pixels, and the
    /// picture already held stays where it is.
    pub fn deliver_sized(
        &self,
        at_ns: u128,
        status: Health,
        width: u32,
        height: u32,
        stride: usize,
    ) -> u64 {
        let seq = self.slot.next_seq();
        *self.now_ns.lock().unwrap() = at_ns;

        // The recorder scripts both clocks off one number. They are different bases
        // on a real machine and a test never needs them to disagree.
        let sample = Sample {
            status,
            seq,
            captured_at_ns: at_ns,
            display_time_mach: Some(at_ns as u64),
        };

        if status != Health::Live {
            self.slot.note(sample);
            return seq;
        }

        self.slot.offer(WindowFrame {
            // A recognisable BGRA ramp, padded to `stride` so the row padding is
            // really there for `to_rgba` to drop.
            pixels: (0..stride * height as usize)
                .map(|byte| byte as u8)
                .collect(),
            width,
            height,
            stride,
            captured_at_ns: at_ns,
            display_time_mach: sample.display_time_mach,
            seq,
            // No content rect: this recorder knows the surface in pixels and the
            // attachment is in points, and inventing a conversion here would be the
            // suite asserting a unit the adapter does not use. The rule that reads
            // it — a surface the content does not fill — is proved in `target.rs`
            // against frames written out by hand.
            content_rect: None,
            reported_scale: None,
            status,
        });
        seq
    }

    /// A sample with no display time at all — the one shape the fence must never
    /// guess about.
    pub fn deliver_undated(&self, at_ns: u128) -> u64 {
        let seq = self.deliver(at_ns, Health::Live);
        let mut held = self.slot.lock_held();
        if let Some(frame) = held.as_mut() {
            frame.display_time_mach = None;
        }
        seq
    }

    /// Time passes and the stream says nothing at all.
    pub fn goes_silent_for(&self, ms: u64) {
        *self.now_ns.lock().unwrap() += u128::from(ms) * 1_000_000;
    }

    /// The delivery side recorded a fault it could not answer from.
    pub fn faults(&self, error: FrameError) {
        self.slot.fault(error);
    }

    /// The next `start` fails with this reason.
    pub fn refuses_to_start(&self, error: FrameError) {
        *self.refuse_start.lock().unwrap() = Some(error);
    }

    pub fn started(&self) -> Vec<StreamWindow> {
        self.started.lock().unwrap().clone()
    }

    pub fn stops(&self) -> usize {
        *self.stops.lock().unwrap()
    }

    pub fn running(&self) -> bool {
        *self.running.lock().unwrap()
    }
}

#[cfg(test)]
impl WindowFrames for Recorder {
    fn start(&self, window: StreamWindow) -> Result<(), FrameError> {
        if let Some(refusal) = self.refuse_start.lock().unwrap().take() {
            return Err(refusal);
        }
        self.started.lock().unwrap().push(window);
        *self.running.lock().unwrap() = true;
        Ok(())
    }

    fn latest(&self, after: Option<Fence>) -> Result<WindowFrame, FrameError> {
        if !self.running() {
            return Err(FrameError::Unavailable(
                "no window capture is running".to_string(),
            ));
        }
        self.slot.peek(after, *self.now_ns.lock().unwrap())
    }

    fn stop(&self) {
        *self.stops.lock().unwrap() += 1;
        *self.running.lock().unwrap() = false;
        self.slot.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> std::rc::Rc<Recorder> {
        let recorder = Recorder::new();
        recorder.start(window(7)).unwrap();
        recorder
    }

    fn window(id: u32) -> StreamWindow {
        StreamWindow {
            id,
            width: 8,
            height: 4,
        }
    }

    #[test]
    fn a_look_with_no_fence_takes_whatever_is_there() {
        let recorder = frames();
        recorder.deliver(1_000, Health::Live);

        let frame = recorder.latest(None).expect("a frame is there");
        assert_eq!(frame.seq, 1);
        assert_eq!(frame.width, 4);
    }

    #[test]
    fn a_stream_that_has_delivered_nothing_answers_not_yet() {
        let recorder = frames();
        assert_eq!(recorder.latest(None).err(), Some(FrameError::NotYet));
    }

    // The fence, at the two places it can be wrong. A frame from BEFORE the action
    // must never become the after-image, whichever half of the fence catches it.
    #[test]
    fn a_frame_from_before_the_action_never_passes_the_fence() {
        let recorder = frames();
        let before = recorder.deliver(1_000, Health::Live);

        let fence = Fence {
            after_mach: 2_000,
            beyond_seq: before,
        };
        assert_eq!(recorder.latest(Some(fence)).err(), Some(FrameError::NotYet));

        // A newer sequence that was DISPLAYED before the dispatch is still not
        // after it — this is the half a bare arrival time would have let through.
        recorder.deliver(1_500, Health::Live);
        assert_eq!(recorder.latest(Some(fence)).err(), Some(FrameError::NotYet));

        // And a frame displayed after the dispatch, with a newer sequence, is.
        recorder.deliver(2_500, Health::Live);
        assert_eq!(recorder.latest(Some(fence)).map(|f| f.seq), Ok(3));
    }

    // Absent is not "probably after". A sample with no display time is held back
    // until the caller's own deadline answers for it.
    #[test]
    fn a_sample_with_no_display_time_is_never_admitted() {
        let recorder = frames();
        let before = recorder.deliver(1_000, Health::Live);
        recorder.deliver_undated(2_500);

        let fence = Fence {
            after_mach: 2_000,
            beyond_seq: before,
        };
        assert_eq!(recorder.latest(Some(fence)).err(), Some(FrameError::NotYet));
    }

    // BLOCKER: an idle sample is the NORMAL state of an unchanged window, and it
    // carries no pixels. The picture must survive it.
    #[test]
    fn an_idle_sample_keeps_the_picture_it_says_is_unchanged() {
        let recorder = frames();
        recorder.deliver_sized(1_000, Health::Live, 4, 2, 16);

        for tick in 1..=5 {
            recorder.deliver(1_000 + tick * 100, Health::Idle);
        }

        let frame = recorder.latest(None).expect("the picture is still there");
        assert_eq!(frame.width, 4, "the pixels are the live frame's");
        assert_eq!(frame.height, 2);
        assert!(frame.to_rgba().is_ok(), "and it is still a picture");
        assert_eq!(frame.seq, 6, "with the newest sample's sequence");
    }

    // The half of the same rule that makes an unchanged window answerable: an idle
    // sample after the dispatch SATISFIES the fence, because it is the window
    // server stating that nothing changed since then.
    #[test]
    fn an_idle_sample_after_the_action_answers_stable_and_unchanged() {
        let recorder = frames();
        let aimed = recorder.deliver(1_000, Health::Live);

        let fence = Fence {
            after_mach: 1_500,
            beyond_seq: aimed,
        };
        assert_eq!(recorder.latest(Some(fence)).err(), Some(FrameError::NotYet));

        recorder.deliver(2_000, Health::Idle);

        let frame = recorder
            .latest(Some(fence))
            .expect("an idle sample answers the fence");
        assert_eq!(frame.width, 4, "and it answers with the unchanged picture");
    }

    // Before any picture exists there is nothing an idle sample can be about, and
    // an empty frame published here would be a refusal for a condition that is
    // about to resolve itself.
    #[test]
    fn an_idle_sample_before_the_first_picture_is_not_yet() {
        let recorder = frames();
        recorder.deliver(1_000, Health::Idle);

        assert_eq!(recorder.latest(None).err(), Some(FrameError::NotYet));

        recorder.deliver(1_100, Health::Live);
        assert!(recorder.latest(None).is_ok());
    }

    // A settle looks many times at a view that has stopped changing. Each look must
    // answer the same picture, so `peek` clones rather than takes.
    #[test]
    fn a_healthy_unchanged_stream_keeps_answering_the_same_frame() {
        let recorder = frames();
        recorder.deliver(1_000, Health::Live);

        for _ in 0..5 {
            assert_eq!(recorder.latest(None).map(|frame| frame.seq), Ok(1));
        }
    }

    #[test]
    fn only_the_newest_frame_is_kept() {
        let recorder = frames();
        recorder.deliver(1_000, Health::Live);
        recorder.deliver(2_000, Health::Live);
        recorder.deliver(3_000, Health::Live);

        assert_eq!(recorder.latest(None).map(|frame| frame.seq), Ok(3));
    }

    // A window that cannot be seen says so in its own words: the caller's remedy is
    // to ask the person to bring it back, and that is not the remedy for a capture
    // that broke.
    #[test]
    fn a_minimized_window_answers_its_own_code_and_not_a_flattened_one() {
        let recorder = frames();
        recorder.deliver(1_000, Health::Live);
        recorder.deliver(2_000, Health::Minimized);

        let refusal = recorder.latest(None).expect_err("nothing to see");
        assert_eq!(refusal.code(), "target_minimized");

        let recorder = frames();
        recorder.deliver(1_000, Health::Live);
        recorder.deliver(2_000, Health::Disconnected);
        assert_eq!(
            recorder.latest(None).expect_err("stopped").code(),
            "capture_unavailable"
        );
    }

    // A stream that says nothing at all — no frame, no idle sample, no error — must
    // not keep answering its last picture as the present.
    #[test]
    fn a_stream_that_went_silent_stops_being_trusted() {
        let recorder = frames();
        recorder.deliver(1_000, Health::Live);
        assert!(recorder.latest(None).is_ok());

        recorder.goes_silent_for(SILENCE_LIMIT_MS + 1);

        let refusal = recorder.latest(None).expect_err("nothing for two seconds");
        assert_eq!(refusal.code(), "capture_unavailable");
        assert!(
            refusal.detail().contains("sent nothing"),
            "{}",
            refusal.detail()
        );
    }

    #[test]
    fn a_stopped_stream_answers_nothing_and_holds_no_pixels() {
        let recorder = frames();
        recorder.deliver(1_000, Health::Live);

        recorder.stop();

        assert_eq!(recorder.stops(), 1);
        assert!(!recorder.running());
        assert!(matches!(
            recorder.latest(None),
            Err(FrameError::Unavailable(_))
        ));
    }

    // A fault the delivery side recorded has to REACH the caller: a callback has no
    // caller to return to, so an unreported one is a typed refusal nobody can see.
    #[test]
    fn a_fault_the_callback_recorded_is_what_the_next_look_answers() {
        let recorder = frames();
        recorder.deliver(1_000, Health::Live);
        recorder.faults(FrameError::BudgetExceeded("too big".to_string()));

        let refusal = recorder.latest(None).expect_err("the fault stands");
        assert_eq!(refusal.code(), "capture_budget_exceeded");

        // And the FIRST fault stands, because the ones after it are its consequences.
        recorder.faults(FrameError::Unavailable("and then this".to_string()));
        assert_eq!(
            recorder.latest(None).expect_err("still the first").code(),
            "capture_budget_exceeded"
        );

        // A restarted stream is not refused for the stream before it.
        recorder.stop();
        recorder.start(window(7)).unwrap();
        recorder.deliver(2_000, Health::Live);
        assert!(recorder.latest(None).is_ok());
    }

    #[test]
    fn a_refused_grant_is_its_own_code_and_not_a_sentence_inside_another() {
        let denied = FrameError::Denied("Screen Recording is not granted".to_string());
        assert_eq!(denied.code(), "screen_recording_not_granted");

        let recorder = Recorder::new();
        recorder.refuses_to_start(denied);
        assert_eq!(
            recorder.start(window(7)).expect_err("refused").code(),
            "screen_recording_not_granted"
        );
    }

    // Refused BEFORE anything is allocated, which is the only point at which the
    // budget is worth having.
    #[test]
    fn a_frame_larger_than_one_window_may_have_is_refused_by_the_budget() {
        assert!(WindowFrame::budget(4 * 3840, 2160).is_ok());

        let refusal = WindowFrame::budget(4 * 16_384, 16_384).expect_err("far past the budget");
        assert_eq!(refusal.code(), "capture_budget_exceeded");
        assert!(refusal.detail().contains("budget"), "{}", refusal.detail());

        // The four frames in flight are what the ceiling is about, so a frame just
        // over a quarter of it is refused and one just under is not.
        let quarter = MAX_CAPTURE_BYTES / FRAMES_IN_FLIGHT;
        assert!(WindowFrame::budget(quarter, 1).is_ok());
        assert!(WindowFrame::budget(quarter + 1, 1).is_err());
    }

    // The conversion slice 6 assumed away. A padded BGRA surface read as tight RGBA
    // is a hash of the padding and a picture with its colours swapped.
    #[test]
    fn row_padding_is_dropped_and_the_channels_are_put_in_order() {
        let frame = WindowFrame {
            // Two rows of two BGRA pixels, padded to 12 bytes a row.
            pixels: vec![
                1, 2, 3, 4, 5, 6, 7, 8, 0xEE, 0xEE, 0xEE, 0xEE, //
                9, 10, 11, 12, 13, 14, 15, 16, 0xEE, 0xEE, 0xEE, 0xEE,
            ],
            width: 2,
            height: 2,
            stride: 12,
            captured_at_ns: 1,
            display_time_mach: Some(1),
            seq: 1,
            content_rect: None,
            reported_scale: None,
            status: Health::Live,
        };

        let image = frame.to_rgba().expect("a whole picture");
        assert_eq!(image.dimensions(), (2, 2));
        assert_eq!(
            image.as_raw(),
            // BGRA -> RGBA, and not one byte of the 0xEE padding.
            &vec![3, 2, 1, 4, 7, 6, 5, 8, 11, 10, 9, 12, 15, 14, 13, 16]
        );
    }

    #[test]
    fn a_frame_that_does_not_describe_a_picture_is_refused_not_guessed_at() {
        let short = WindowFrame {
            pixels: vec![0; 4],
            width: 4,
            height: 2,
            stride: 16,
            captured_at_ns: 1,
            display_time_mach: Some(1),
            seq: 1,
            content_rect: None,
            reported_scale: None,
            status: Health::Live,
        };

        let refusal = short
            .to_rgba()
            .expect_err("not enough bytes for those rows");
        assert_eq!(refusal.code(), "capture_unavailable");
    }

    #[test]
    fn a_stream_is_replaced_by_stopping_it_first() {
        let recorder = Recorder::new();
        recorder.start(window(7)).unwrap();
        recorder.stop();
        recorder.start(window(9)).unwrap();

        assert_eq!(recorder.started(), vec![window(7), window(9)]);
        assert_eq!(recorder.stops(), 1);
    }

    #[test]
    fn a_start_that_failed_leaves_nothing_running() {
        let recorder = Recorder::new();
        recorder.refuses_to_start(FrameError::Unavailable(
            "the window server refused".to_string(),
        ));

        assert!(recorder.start(window(7)).is_err());
        assert!(!recorder.running());
        assert!(matches!(
            recorder.latest(None),
            Err(FrameError::Unavailable(_))
        ));
    }
}
