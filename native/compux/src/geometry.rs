//! Display geometry and the one coordinate transform — the #1 "clicks land
//! offset" risk. Pure: nothing here reads a screen or posts an event.
//!
//! ## The three spaces
//!
//!   * **Sent pixels** — what the model sees and answers in. A capture is the
//!     display's PHYSICAL frame, cropped to the requested rectangle and downscaled
//!     to fit the sent budgets ([`budget_scale`]).
//!   * **Physical pixels** — the frame the OS actually handed us.
//!   * **Logical points** — what `enigo` takes, and the space the desktop lays its
//!     displays out in. `physical = logical * pixels_per_point`.
//!
//! ## Pixels per point is MEASURED, never assumed
//!
//! On macOS `CGDisplayBounds` is points and `scale_factor()` is the display MODE's
//! backing scale — but whether a capture answers at that backing scale or at 1x is
//! a property of the capture API and the machine, and nobody who can test this has
//! a Retina panel. So the ratio is measured from an image that was really made
//! ([`measure`]) and the transform is built from that ([`Geometry::from_facts`]).
//! Both worlds then map correctly, and a ratio that is neither the mode's scale nor
//! exactly 1 is refused rather than clicked through.
//!
//! Until this slice the constructor read `monitor.width()` — points — into the
//! PHYSICAL width and then divided the already-logical bounds and origin by the
//! backing scale. On a 2x display that made the model see and reach a quarter of
//! the screen (or, if captures answer at 1x, land every click at half its
//! coordinates), and put every second display's origin at half its true place. At
//! 1x every one of those is the identity, which is why it never showed.
//!
//! ## Pixel centre
//!
//! A model pixel `(x, y)` means the CENTRE of that pixel, `(x + 0.5, y + 0.5)`,
//! before the inverse map. Stated once, here, and honoured by [`to_logical`] and
//! [`to_sent`] together: without it a click on a magnified crop lands half a sent
//! pixel up and left of what the model pointed at, which is half a UI element when
//! one sent pixel is a third of a point.

/// Long-edge cap for a sent screenshot (design §5: oversized captures 400 on
/// Anthropic and ground worse).
pub const MAX_EDGE: u32 = 1366;

/// Pixel-area budget for a sent screenshot (M28 B5). The long-edge cap alone
/// punishes extreme aspect ratios: a 3840x1080 super-ultrawide got the same
/// budget as 16:9 but only 384px tall — unreadable, which forced the model to
/// live in magnified crops (and grid-switch errors). A sent image may use
/// whichever budget grants MORE pixels, never upscaled: 16:9 keeps exactly
/// 1366x768, 32:9 recovers ~1931x543, and any capture that fits the long edge
/// today still ships at native resolution.
pub const MAX_AREA: u32 = MAX_EDGE * 768;

/// The sent-image downscale `kz <= 1` fitting `w x h` physical pixels into the
/// budgets: the looser of the long-edge and area rules, capped at native. One
/// formula for full captures and crops, so the two can never diverge.
pub fn budget_scale(w: f32, h: f32) -> f32 {
    let long = w.max(h).max(1.0);
    let edge = (MAX_EDGE as f32 / long).min(1.0);
    let area = (MAX_AREA as f32 / (w * h).max(1.0)).sqrt();
    edge.max(area).min(1.0)
}

/// A measured ratio this far from a whole explanation is not rounding.
const PIXELS_PER_POINT_TOLERANCE: f32 = 0.01;

/// Which platform's reading of the OS numbers applies. Injected rather than
/// `cfg`-selected at the call site, so BOTH arms are tested on every host: a
/// coordinate rule that only runs on the machine that wrote it is how the defect
/// above survived four releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    MacOs,
    Linux,
}

impl Host {
    /// The arm this build runs under. Windows is not a supported target; the
    /// Linux arm is what any non-macOS build gets.
    pub const HERE: Host = if cfg!(target_os = "macos") {
        Host::MacOs
    } else {
        Host::Linux
    };
}

/// What the OS reported about a display, as plain numbers, before anyone decided
/// what they mean. On macOS `width`/`height`/`x`/`y` are POINTS (`CGDisplayBounds`)
/// and `scale_factor` is the mode's backing scale; on X11 they are pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonitorFacts {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
}

/// Display geometry, separated from the OS `Monitor` handle so the coordinate math
/// is pure and unit-testable (a `Monitor` cannot be constructed off a real screen).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// physical capture pixels
    pub phys_w: u32,
    pub phys_h: u32,
    /// logical points (physical / scale_factor)
    pub logical_w: f32,
    pub logical_h: f32,
    /// logical top-left origin in the global desktop space
    pub origin_x: f32,
    pub origin_y: f32,
    /// physical capture pixels per logical point — MEASURED (see the module doc),
    /// not the display mode's backing scale.
    pub scale_factor: f32,
}

/// What one real capture said about a display: the frame's own dimensions, and how
/// many of its pixels there are per logical point.
///
/// The frame's size is carried rather than re-derived, because multiplying the
/// width's ratio by the height in points can land a pixel off the frame that was
/// actually captured — and the crop would then quietly clamp a row away. The
/// transform's physical size IS the picture's size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measurement {
    pub frame_w: u32,
    pub frame_h: u32,
    pub pixels_per_point: f32,
}

/// Measure a display from a frame that was really captured.
///
/// **macOS:** `image_width / width_in_points`, cross-checked twice. The two axes'
/// ratios must agree within one pixel over the whole display, and the ratio must be
/// either the mode's backing scale or exactly 1 — those are the two answers a
/// capture API can give. Anything else is a transform nobody can explain, and a
/// transform nobody can explain must never click: the caller fails the action with
/// `capture_geometry_mismatch`.
///
/// It is deliberately fail-closed, so the refusal has to be diagnosable from a bug
/// report with nothing else in it: every number that went into the decision is in
/// the sentence — the frame, the display in points, the mode's scale, and both
/// measured ratios.
///
/// **Linux:** byte for byte today's arithmetic — the mode's scale, with the image
/// not consulted. It may be wrong under HiDPI X11, nothing here can verify it, and
/// [M42.1] owns it.
///
/// [M42.1]: MILESTONE_42_1_LINUX_COMPUTER_USE.md
pub fn measure(
    facts: &MonitorFacts,
    image_w: u32,
    image_h: u32,
    host: Host,
) -> Result<Measurement, String> {
    if host == Host::Linux {
        return Ok(Measurement {
            frame_w: facts.width,
            frame_h: facts.height,
            pixels_per_point: facts.scale_factor.max(1.0),
        });
    }

    let (width, height) = (facts.width.max(1) as f32, facts.height.max(1) as f32);
    let across = image_w as f32 / width;
    let down = image_h as f32 / height;
    let numbers = format!(
        "frame {image_w}x{image_h}, display {}x{} points, mode scale {:.4}, \
         measured {across:.4} across and {down:.4} down",
        facts.width, facts.height, facts.scale_factor
    );

    if (height * across - image_h as f32).abs() > 1.0 {
        return Err(format!(
            "the two axes disagree by more than a pixel: {numbers}"
        ));
    }

    let explained = (across - facts.scale_factor).abs() < PIXELS_PER_POINT_TOLERANCE
        || (across - 1.0).abs() < PIXELS_PER_POINT_TOLERANCE;

    if explained {
        Ok(Measurement {
            frame_w: image_w,
            frame_h: image_h,
            pixels_per_point: across,
        })
    } else {
        Err(format!(
            "the frame is neither the display mode's scale nor 1 to 1: {numbers}"
        ))
    }
}

impl Geometry {
    /// Build the transform from what the OS said and what a capture measured.
    ///
    /// **macOS:** the bounds and the origin are points and are used as they are —
    /// the defect this replaces divided them by the backing scale, which put every
    /// second display at half its true origin. The physical size is the MEASURED
    /// frame's own size, so what the crop indexes into and what the transform
    /// believes are the same picture on both axes.
    ///
    /// **Linux:** byte for byte today's arithmetic (the bounds are pixels there
    /// and the origin is divided by the scale). See [`measure`].
    pub fn from_facts(facts: &MonitorFacts, measured: Measurement, host: Host) -> Geometry {
        let ppp = measured.pixels_per_point;

        match host {
            Host::MacOs => Geometry {
                phys_w: measured.frame_w.max(1),
                phys_h: measured.frame_h.max(1),
                logical_w: facts.width as f32,
                logical_h: facts.height as f32,
                origin_x: facts.x as f32,
                origin_y: facts.y as f32,
                scale_factor: ppp,
            },

            Host::Linux => Geometry {
                phys_w: facts.width,
                phys_h: facts.height,
                logical_w: facts.width as f32 / ppp,
                logical_h: facts.height as f32 / ppp,
                origin_x: facts.x as f32 / ppp,
                origin_y: facts.y as f32 / ppp,
                scale_factor: ppp,
            },
        }
    }

    /// The whole physical frame as a crop. The one place the full image's size is
    /// derived, so `Region::full` and `CropRect::sent_dims` cannot answer a pixel
    /// apart (they used to: one truncated where the other rounded).
    pub fn full_crop(&self) -> CropRect {
        CropRect {
            left_phys: 0.0,
            top_phys: 0.0,
            w_phys: self.phys_w as f32,
            h_phys: self.phys_h as f32,
        }
    }
}

/// What each display's last real capture measured, so a reply that hands out
/// coordinates WITHOUT capturing an image (`windows`, `elements`) still knows the
/// transform it is answering in. One capture per display per configuration pays for
/// it; guessing would be the old defect with a new name.
///
/// **Keyed on the display's FACTS, not its id.** A remembered ratio is only true of
/// the mode it was measured in: switch a Retina panel from "More Space" to "Larger
/// Text", or plug a different display into a reused id, and a ratio kept on the id
/// alone would have `windows` and `elements` answering in a space no screenshot
/// reproduces — and nothing downstream could catch it, because the observation they
/// mint records the CURRENT facts, so the staleness check would pass. Changed facts
/// are a miss, and a miss costs one capture.
///
/// Owned by the action worker, which is single-threaded, so there is no lock.
#[derive(Default)]
pub struct Measured {
    entries: Vec<(u32, MonitorFacts, Measurement)>,
}

impl Measured {
    pub fn new() -> Measured {
        Measured::default()
    }

    pub fn remember(&mut self, display_id: u32, facts: &MonitorFacts, measured: Measurement) {
        self.entries.retain(|(id, _, _)| *id != display_id);
        self.entries.push((display_id, *facts, measured));
    }

    pub fn get(&self, display_id: u32, facts: &MonitorFacts) -> Option<Measurement> {
        self.entries
            .iter()
            .find(|(id, known, _)| *id == display_id && known == facts)
            .map(|(_, _, measured)| *measured)
    }
}

/// A zoom rectangle in full-display SENT-image pixel space — the coordinates the
/// model reads off a normal screenshot. Absent on a request → the whole display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Region {
    /// The region spanning the entire full-display sent image. Derived from the
    /// PHYSICAL frame through the same crop every other rectangle goes through, so
    /// the full image has exactly one size — it used to come from the logical size
    /// instead, which is the same number only while `physical = logical * scale`
    /// exactly.
    pub fn full(geom: &Geometry) -> Region {
        let (w, h) = geom.full_crop().sent_extent();
        Region {
            x: 0.0,
            y: 0.0,
            w: w as f64,
            h: h as f64,
        }
    }
}

/// A physical crop of the display plus the scale used to send it. Built once from a
/// region and shared by capture and the inverse coordinate map, so the two can never
/// disagree — the #1 "clicks land offset" bug class.
pub struct CropRect {
    pub left_phys: f32,
    pub top_phys: f32,
    pub w_phys: f32,
    pub h_phys: f32,
}

impl CropRect {
    /// Downscale to fit the sent budgets (`budget_scale`); never upscale
    /// (`kz <= 1`). A small crop is therefore sent at native physical
    /// resolution — that is the zoom.
    pub fn sent_scale(&self) -> f32 {
        budget_scale(self.w_phys, self.h_phys)
    }

    /// The sent image's size before it is rounded to whole pixels. `Region::full`
    /// keeps this rather than the rounded pair, because a region is mapped back to
    /// physical pixels by dividing, and half a pixel of rounding there comes back
    /// as a whole one: the crop is then a hair smaller, the area budget a hair
    /// looser, and the full image reports 1932 pixels where it sent 1931.
    fn sent_extent(&self) -> (f32, f32) {
        let kz = self.sent_scale();
        (self.w_phys * kz, self.h_phys * kz)
    }

    pub fn sent_dims(&self) -> (u32, u32) {
        let (w, h) = self.sent_extent();
        (w.round().max(1.0) as u32, h.round().max(1.0) as u32)
    }
}

/// The full-display "sent scale" `k`: sent pixels per LOGICAL point for a full
/// screenshot. The full image is the PHYSICAL display downscaled to fit the sent
/// budgets (`budget_scale` → `kz_full`), so `k = sent_dim / logical_dim = kz_full *
/// scale_factor`. Region coordinates are read off that sent image, so `crop_rect` /
/// `Region::full` MUST use this physical-derived `k`. A logical-derived `k` diverges
/// whenever the logical long edge already fits the budget but the physical one does
/// not (e.g. a 13" Retina at 2560x1600@2x → 1280x800 logical) and mislocates region
/// zooms — the #1 offset bug.
pub fn sent_scale(geom: &Geometry) -> f32 {
    budget_scale(geom.phys_w as f32, geom.phys_h as f32) * geom.scale_factor
}

/// The physical crop for a region (or the whole display when the region spans it).
/// `region` is in full-display SENT-image pixels; convert through the full-display
/// sent scale `k` to logical, then to physical, clamped to the display bounds.
pub fn crop_rect(geom: &Geometry, region: &Region) -> CropRect {
    let k = sent_scale(geom);
    let sf = geom.scale_factor;
    // Clamp left/top in-bounds (an out-of-range region can't produce a degenerate or
    // out-of-image crop); width/height then fill the remaining space, min 1px.
    let max_left = (geom.phys_w as f32 - 1.0).max(0.0);
    let max_top = (geom.phys_h as f32 - 1.0).max(0.0);
    let left = (region.x as f32 / k * sf).clamp(0.0, max_left);
    let top = (region.y as f32 / k * sf).clamp(0.0, max_top);
    let w = (region.w as f32 / k * sf)
        .min(geom.phys_w as f32 - left)
        .max(1.0);
    let h = (region.h as f32 / k * sf)
        .min(geom.phys_h as f32 - top)
        .max(1.0);
    CropRect {
        left_phys: left,
        top_phys: top,
        w_phys: w,
        h_phys: h,
    }
}

/// The continuous logical point at a sent-image coordinate's own EDGE — the corner
/// of pixel `(x, y)`, not its middle.
///
/// This is the mapping a RECTANGLE's corners take. A rectangle's corner is a
/// boundary between pixels, so adding the pixel-centre half would move the whole
/// rectangle half a pixel every time one was mapped: a caller that re-derives a
/// crop from its own image each turn (Fermix does, after every mutating action)
/// watched the view walk across the screen and shrink a pixel at a time.
fn logical_edge(geom: &Geometry, region: &Region, x: f64, y: f64) -> (f32, f32) {
    let crop = crop_rect(geom, region);
    let kz = crop.sent_scale();
    (
        geom.origin_x + (crop.left_phys + x as f32 / kz) / geom.scale_factor,
        geom.origin_y + (crop.top_phys + y as f32 / kz) / geom.scale_factor,
    )
}

/// The continuous logical point a sent-image coordinate POINTS AT: the centre of
/// that pixel, which is the convention the module doc states and the one a click
/// takes. Half a pixel further in than [`logical_edge`], and the difference matters
/// exactly where the two are used — a point is aimed, a rectangle is bounded.
fn logical_point(geom: &Geometry, region: &Region, x: f64, y: f64) -> (f32, f32) {
    logical_edge(geom, region, x + 0.5, y + 0.5)
}

/// Map a coordinate from a sent image to a global LOGICAL point for enigo.
///
/// One convention for full and zoomed views: a full screenshot is a region spanning
/// the whole sent image, so this reduces to `origin + (x,y)/k` there. With a region
/// the image is a physical crop downscaled by `kz`; the inverse adds the crop's
/// logical offset. Capture and this share `crop_rect`, so they cannot disagree.
///
/// The result is rounded, because enigo takes whole logical points. On a magnified
/// crop one point can be several sent pixels wide, so that rounding — not this
/// map — is the limit on how finely a click can be aimed.
pub fn to_logical(geom: &Geometry, region: &Region, x: f64, y: f64) -> (i32, i32) {
    let (lx, ly) = logical_point(geom, region, x, y);
    (lx.round() as i32, ly.round() as i32)
}

/// Inverse of `to_logical`: a global LOGICAL point → the sent-image coordinate for
/// `region`, or None when it falls outside the sent image. Used by `elements` to
/// place accessibility frames back onto the coordinates the model reads.
///
/// The answer is the INDEX of the pixel the point falls in, which is what the
/// pixel-centre convention makes it: the continuous coordinate, floored.
pub fn to_sent(geom: &Geometry, region: &Region, lx: f64, ly: f64) -> Option<(i64, i64)> {
    let crop = crop_rect(geom, region);
    let kz = crop.sent_scale();
    let sf = geom.scale_factor;
    let sx = ((lx as f32 - geom.origin_x) * sf - crop.left_phys) * kz;
    let sy = ((ly as f32 - geom.origin_y) * sf - crop.top_phys) * kz;
    let (sw, sh) = crop.sent_dims();

    if sx < 0.0 || sy < 0.0 || sx >= sw as f32 || sy >= sh as f32 {
        None
    } else {
        Some((sx.floor() as i64, sy.floor() as i64))
    }
}

/// A rectangle given in global LOGICAL points, expressed in one image's own sent
/// pixels.
///
/// What `elements` publishes a control's `bounds` in, so every number in that
/// reply lives in ONE space — the image the reply names — rather than a caller
/// having to know that the click point is in sent pixels and the frame is in
/// points. Slice 3 took the second coordinate space off the wire; this is what
/// keeps it off.
///
/// Maps EDGES, not pixel centres, for the reason [`rect_through`] does: a
/// control's frame is a rectangle on the screen rather than a set of pixels, and
/// the centre convention would shrink it by half a pixel at every edge.
///
/// **Not clipped to the image.** A control whose click point is in view can still
/// have a frame that runs past an edge, and a caller reading a negative `x` there
/// learns something true; clamping would tell it the control is smaller than it is.
pub fn sent_rect(geom: &Geometry, region: &Region, x: f64, y: f64, w: f64, h: f64) -> Region {
    let crop = crop_rect(geom, region);
    let kz = f64::from(crop.sent_scale());
    let sf = f64::from(geom.scale_factor);

    Region {
        x: ((x - f64::from(geom.origin_x)) * sf - f64::from(crop.left_phys)) * kz,
        y: ((y - f64::from(geom.origin_y)) * sf - f64::from(crop.top_phys)) * kz,
        w: w * sf * kz,
        h: h * sf * kz,
    }
}

/// A rectangle read in ONE observation's image, expressed in the full-display sent
/// pixels of the geometry in force now.
///
/// This is what lets `screenshot`, `elements` and `wait_for_change` take a `region`
/// beside an `observation_id`: the caller draws the rectangle on the image it can
/// see, and the transform that image was made with — not one the caller repeated —
/// puts it back on the display. Out-of-range corners are left to `crop_rect`, which
/// clamps them to the display exactly as it does for a rectangle read off a full
/// image: looking slightly past an edge is harmless.
///
/// The corners map through [`logical_edge`], NOT the pixel centre: a rectangle
/// that covers a whole image must come back as that whole image, however many
/// times it is mapped.
pub fn rect_through(
    observed: &Geometry,
    observed_region: &Region,
    rect: &Region,
    now: &Geometry,
) -> Region {
    let (left, top) = logical_edge(observed, observed_region, rect.x, rect.y);
    let (right, bottom) = logical_edge(observed, observed_region, rect.x + rect.w, rect.y + rect.h);

    let k = sent_scale(now);
    Region {
        x: ((left - now.origin_x) * k) as f64,
        y: ((top - now.origin_y) * k) as f64,
        w: ((right - left) * k).max(1.0) as f64,
        h: ((bottom - top) * k).max(1.0) as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x laptop panel as macOS reports it: 1512x982 POINTS, backing scale 2.
    fn retina_facts() -> MonitorFacts {
        MonitorFacts {
            x: 0,
            y: 0,
            width: 1512,
            height: 982,
            scale_factor: 2.0,
        }
    }

    /// The owner's panel: 3840x1080 at 1x, where points and pixels are the same
    /// numbers and every arm of this module is the identity.
    fn ultrawide_facts() -> MonitorFacts {
        MonitorFacts {
            x: 0,
            y: 0,
            width: 3840,
            height: 1080,
            scale_factor: 1.0,
        }
    }

    fn geom(facts: &MonitorFacts, image: (u32, u32)) -> Geometry {
        let measured = measure(facts, image.0, image.1, Host::MacOs).expect("a measurable capture");
        Geometry::from_facts(facts, measured, Host::MacOs)
    }

    /// The measurement a capture at `ratio` on this display would have produced.
    fn measurement(facts: &MonitorFacts, ratio: f32) -> Measurement {
        Measurement {
            frame_w: (facts.width as f32 * ratio) as u32,
            frame_h: (facts.height as f32 * ratio) as u32,
            pixels_per_point: ratio,
        }
    }

    // THE DEFECT. On a 2x panel the old constructor read the 1512 POINTS into the
    // physical width and then halved the already-logical bounds, so the model saw
    // and could reach only the top-left quarter. Whether the capture answers at the
    // backing scale or at 1x is not knowable from here, so BOTH answers must build
    // a geometry that shows the whole screen and clicks where it looks.
    #[test]
    fn a_retina_display_is_whole_however_the_capture_answers() {
        for image in [(3024, 1964), (1512, 982)] {
            let g = geom(&retina_facts(), image);

            assert_eq!(
                (g.logical_w, g.logical_h),
                (1512.0, 982.0),
                "the bounds are points and stay points: {image:?}"
            );
            assert_eq!((g.phys_w, g.phys_h), image, "the frame that was captured");

            // The full image covers the whole display: its far corner maps to the
            // far corner of the points space, not to the middle of it.
            let region = Region::full(&g);
            let (sw, sh) = g.full_crop().sent_dims();
            let (lx, ly) = to_logical(&g, &region, (sw - 1) as f64, (sh - 1) as f64);
            assert!((lx - 1512).abs() <= 2, "{image:?}: right edge at {lx}");
            assert!((ly - 982).abs() <= 2, "{image:?}: bottom edge at {ly}");

            // And the middle of the image is the middle of the display.
            let (mx, my) = to_logical(&g, &region, (sw / 2) as f64, (sh / 2) as f64);
            assert!((mx - 756).abs() <= 2, "{image:?}: centre at {mx}");
            assert!((my - 491).abs() <= 2, "{image:?}: centre at {my}");
        }
    }

    // A second display's origin is in POINTS and is used as it is. The old
    // constructor halved it, so every click on a 2x second display landed on the
    // display next door; a negative origin (a panel placed left of or above the
    // main one) landed further still.
    #[test]
    fn a_second_display_keeps_its_origin_whatever_its_sign() {
        for origin in [(1512, 0), (-1512, -240)] {
            let facts = MonitorFacts {
                x: origin.0,
                y: origin.1,
                ..retina_facts()
            };

            for image in [(3024, 1964), (1512, 982)] {
                let g = geom(&facts, image);
                assert_eq!((g.origin_x, g.origin_y), (origin.0 as f32, origin.1 as f32));

                let region = Region::full(&g);
                let (lx, ly) = to_logical(&g, &region, 0.0, 0.0);
                assert!((lx - origin.0).abs() <= 2, "{image:?}: {lx} at {origin:?}");
                assert!((ly - origin.1).abs() <= 2, "{image:?}: {ly} at {origin:?}");
            }
        }
    }

    // The only panel anyone here can run this on. Points are pixels, so the whole
    // module is the identity and nothing about this display may move.
    #[test]
    fn a_1x_display_is_exactly_what_it_was() {
        let facts = ultrawide_facts();
        let g = geom(&facts, (3840, 1080));

        assert_eq!((g.phys_w, g.phys_h), (3840, 1080));
        assert_eq!((g.logical_w, g.logical_h), (3840.0, 1080.0));
        assert_eq!(g.scale_factor, 1.0);
        assert_eq!((g.origin_x, g.origin_y), (0.0, 0.0));
    }

    // Two axes that disagree, and a ratio that is neither the mode's scale nor 1,
    // are the two ways a capture can mean something nobody can explain. Both are
    // refused with the numbers in the sentence: a transform nobody can explain
    // must never click.
    #[test]
    fn an_unexplainable_capture_is_refused_with_its_numbers() {
        let facts = retina_facts();

        // Every refusal has to be diagnosable from a bug report and nothing else,
        // so both of them carry the frame, the display, the mode and both ratios.
        for refusal in [
            measure(&facts, 3024, 1200, Host::MacOs).expect_err("the axes disagree"),
            measure(&facts, 2268, 1473, Host::MacOs).expect_err("1.5 is neither 2 nor 1"),
        ] {
            assert!(refusal.contains("display 1512x982 points"), "{refusal}");
            assert!(refusal.contains("mode scale 2.0000"), "{refusal}");
            assert!(refusal.contains("across"), "{refusal}");
            assert!(refusal.contains("down"), "{refusal}");
        }

        assert!(measure(&facts, 3024, 1200, Host::MacOs)
            .unwrap_err()
            .contains("frame 3024x1200"));

        // Both answers a capture API can give are accepted, and the frame's OWN
        // size is what comes back — never the ratio multiplied out again, which on
        // the far axis can miss the picture by a pixel and clamp a row away.
        assert_eq!(
            measure(&facts, 3024, 1964, Host::MacOs),
            Ok(Measurement {
                frame_w: 3024,
                frame_h: 1964,
                pixels_per_point: 2.0
            })
        );
        let short = measure(&facts, 1512, 981, Host::MacOs).expect("one pixel of rounding");
        assert_eq!(short.frame_h, 981, "the frame that exists, not 982");
        assert_eq!(short.pixels_per_point, 1.0);

        let g = Geometry::from_facts(&facts, short, Host::MacOs);
        assert_eq!((g.phys_w, g.phys_h), (1512, 981));
    }

    // A ratio is only true of the mode it was measured in. Keyed on the display's
    // id alone, a "More Space" switch — or a reused id on a different panel — would
    // have `windows` and `elements` answering in a space no screenshot reproduces,
    // and nothing downstream could catch it: the observation they mint records the
    // CURRENT facts, so the staleness check passes.
    #[test]
    fn a_measurement_is_only_remembered_for_the_display_it_was_taken_on() {
        let facts = retina_facts();
        let mut measured = Measured::new();
        measured.remember(7, &facts, measurement(&facts, 2.0));

        assert_eq!(measured.get(7, &facts), Some(measurement(&facts, 2.0)));
        assert_eq!(measured.get(8, &facts), None, "another display");

        // Every fact is part of the key, because every one of them can change
        // under a running session.
        for changed in [
            MonitorFacts {
                scale_factor: 1.0,
                ..facts
            },
            MonitorFacts {
                width: 1728,
                ..facts
            },
            MonitorFacts {
                height: 1117,
                ..facts
            },
            MonitorFacts { x: 1512, ..facts },
            MonitorFacts { y: -240, ..facts },
        ] {
            assert_eq!(
                measured.get(7, &changed),
                None,
                "{changed:?} must be measured again"
            );
        }

        // And re-measuring replaces rather than accumulating, so the memo cannot
        // answer for a configuration that is two changes old.
        measured.remember(7, &facts, measurement(&facts, 1.0));
        assert_eq!(measured.get(7, &facts), Some(measurement(&facts, 1.0)));
    }

    // Linux is byte for byte what it was: the mode's scale, the image not
    // consulted. It may be wrong under HiDPI X11 and M42.1 owns that.
    #[test]
    fn the_linux_arm_is_todays_arithmetic() {
        let facts = MonitorFacts {
            x: 200,
            y: 0,
            width: 2560,
            height: 1440,
            scale_factor: 2.0,
        };

        let measured = measure(&facts, 1, 1, Host::Linux).expect("the image is not consulted");
        assert_eq!(measured.pixels_per_point, 2.0);

        let g = Geometry::from_facts(&facts, measured, Host::Linux);
        assert_eq!((g.phys_w, g.phys_h), (2560, 1440));
        assert_eq!((g.logical_w, g.logical_h), (1280.0, 720.0));
        assert_eq!((g.origin_x, g.origin_y), (100.0, 0.0));
    }

    // Retina reference display: 2880x1800 physical, 2x scale -> 1440x900 logical,
    // origin (0,0). Geometry is constructible without a real Monitor, so the
    // coordinate math (the #1 offset-bug class) is unit-tested here, not on-device.
    fn retina_geom() -> Geometry {
        Geometry {
            phys_w: 2880,
            phys_h: 1800,
            logical_w: 1440.0,
            logical_h: 900.0,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_factor: 2.0,
        }
    }

    /// 13" Retina: 2560x1600 physical, 2x -> 1280x800 logical. logical_long (1280) <=
    /// MAX_EDGE < phys_long (2560) — the regime a logical-derived sent scale got wrong.
    pub(super) fn macbook_air_geom() -> Geometry {
        Geometry {
            phys_w: 2560,
            phys_h: 1600,
            logical_w: 1280.0,
            logical_h: 800.0,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_factor: 2.0,
        }
    }

    #[test]
    fn full_screenshot_mapping_matches_the_simple_formula() {
        // A full screenshot is a region spanning the whole sent image, so the unified
        // map must reduce to the original `origin + (x,y)/k`, give or take the half
        // pixel the centre convention adds.
        let g = retina_geom();
        let region = Region::full(&g);
        let k = sent_scale(&g);
        let (lx, ly) = to_logical(&g, &region, 683.0, 450.0);
        assert!((lx - (683.5_f32 / k).round() as i32).abs() <= 1);
        assert!((ly - (450.5_f32 / k).round() as i32).abs() <= 1);
    }

    #[test]
    fn region_zoom_corners_map_back_into_the_region() {
        let g = retina_geom();
        let k = sent_scale(&g);
        // The lower-right quadrant, expressed in full-display sent pixels.
        let region = Region {
            x: (720.0 * k) as f64,
            y: (450.0 * k) as f64,
            w: (720.0 * k) as f64,
            h: (450.0 * k) as f64,
        };

        // Top-left of the zoomed image is the region origin in logical points.
        let (lx, ly) = to_logical(&g, &region, 0.0, 0.0);
        assert!((lx - 720).abs() <= 1, "lx={lx}");
        assert!((ly - 450).abs() <= 1, "ly={ly}");

        // Bottom-right of the zoomed image is the region's far corner.
        let crop = crop_rect(&g, &region);
        let (sw, sh) = crop.sent_dims();
        let (lx, ly) = to_logical(&g, &region, sw as f64, sh as f64);
        assert!((lx - 1440).abs() <= 2, "lx={lx}");
        assert!((ly - 900).abs() <= 2, "ly={ly}");
    }

    /// Every integer pixel of a sent image, mapped out to a logical point and back.
    ///
    /// The tolerance is the interesting part. `to_logical` rounds to a whole logical
    /// point because that is all enigo takes, so on a crop magnified past the point
    /// grid — a small crop on a 3x display sends 3 image pixels per point — half a
    /// point of rounding IS more than one sent pixel. One sent pixel is therefore
    /// the floor, and half a logical point the true bound.
    ///
    /// The outermost point's worth of pixels is left out for the same reason: a
    /// pixel within half a point of an edge can round to a point just outside the
    /// crop, which is a correct place on the screen and not a place in this image.
    fn round_trips(geom: &Geometry, region: &Region) {
        let crop = crop_rect(geom, region);
        let (sw, sh) = crop.sent_dims();
        let sent_per_point = crop.sent_scale() * geom.scale_factor;
        let tolerance = (sent_per_point / 2.0).max(1.0) + 0.001;

        let inset = sent_per_point.ceil() as u32;
        assert!(sw > 2 * inset && sh > 2 * inset, "the crop is all edge");

        let step = (sw.max(sh) / 16).max(1);
        for sx in (inset..sw - inset).step_by(step as usize) {
            for sy in (inset..sh - inset).step_by(step as usize) {
                let (lx, ly) = to_logical(geom, region, sx as f64, sy as f64);
                let (rx, ry) = to_sent(geom, region, lx as f64, ly as f64)
                    .unwrap_or_else(|| panic!("({sx},{sy}) left the image at {lx},{ly}"));

                assert!(
                    (rx - sx as i64).abs() as f32 <= tolerance,
                    "x: sent={sx} back={rx} tolerance={tolerance}"
                );
                assert!(
                    (ry - sy as i64).abs() as f32 <= tolerance,
                    "y: sent={sy} back={ry} tolerance={tolerance}"
                );
            }
        }
    }

    #[test]
    fn every_pixel_round_trips_at_every_scale_and_origin() {
        for scale in [1.0_f32, 2.0, 3.0] {
            for origin in [(0.0_f32, 0.0_f32), (1512.0, 240.0), (-1512.0, -240.0)] {
                let facts = MonitorFacts {
                    x: origin.0 as i32,
                    y: origin.1 as i32,
                    width: 1512,
                    height: 982,
                    scale_factor: scale,
                };
                let image = ((1512.0 * scale) as u32, (982.0 * scale) as u32);
                let g = geom(&facts, image);

                // The full image.
                round_trips(&g, &Region::full(&g));

                // A non-origin crop: the lower-right quadrant, and a small one
                // magnified to native pixels.
                let k = sent_scale(&g);
                round_trips(
                    &g,
                    &Region {
                        x: (756.0 * k) as f64,
                        y: (491.0 * k) as f64,
                        w: (756.0 * k) as f64,
                        h: (491.0 * k) as f64,
                    },
                );
                round_trips(
                    &g,
                    &Region {
                        x: (300.0 * k) as f64,
                        y: (200.0 * k) as f64,
                        w: (160.0 * k).max(2.0) as f64,
                        h: (120.0 * k).max(2.0) as f64,
                    },
                );
            }
        }
    }

    #[test]
    fn to_sent_round_trips_with_nonzero_origin_and_physical_mismatch() {
        let mut g = macbook_air_geom();
        g.origin_x = 1440.0;
        g.origin_y = 100.0;
        let region = Region::full(&g);
        let crop = crop_rect(&g, &region);
        let (sw, sh) = crop.sent_dims();
        let (sx, sy) = ((sw as f64) / 3.0, (sh as f64) / 3.0);
        let (lx, ly) = to_logical(&g, &region, sx, sy);
        let (rx, ry) = to_sent(&g, &region, lx as f64, ly as f64).expect("in bounds");
        assert!((rx - sx as i64).abs() <= 2, "x: sent={sx} back={rx}");
        assert!((ry - sy as i64).abs() <= 2, "y: sent={sy} back={ry}");
    }

    #[test]
    fn to_sent_is_none_outside_the_sent_image() {
        let g = retina_geom();
        let region = Region::full(&g);
        // Left of / above the display origin, and past the far edge.
        assert_eq!(to_sent(&g, &region, -100.0, 10.0), None);
        assert_eq!(
            to_sent(
                &g,
                &region,
                g.logical_w as f64 + 100.0,
                g.logical_h as f64 + 100.0
            ),
            None
        );
    }

    // The full image's size is derived once. `Region::full` used to truncate where
    // `CropRect::sent_dims` rounded, so the two could name images a pixel apart —
    // and a coordinate on that last column mapped through a rectangle that did not
    // contain it.
    #[test]
    fn the_full_image_has_one_size() {
        for facts in [retina_facts(), ultrawide_facts()] {
            for ratio in [1.0_f32, 2.0] {
                let g = Geometry::from_facts(&facts, measurement(&facts, ratio), Host::MacOs);
                let full = Region::full(&g);
                let (cw, ch) = crop_rect(&g, &full).sent_dims();
                assert_eq!(
                    (full.w.round() as u32, full.h.round() as u32),
                    (cw, ch),
                    "the full image's size must survive the round trip through the crop"
                );
            }
        }
    }

    // --- M28 B5: the area budget ---------------------------------------------

    /// The display the window listing exists for: 3840x1080 at 1x, where a full
    /// capture is squeezed and a window crop wins most of it back.
    pub(super) fn ultrawide_geom() -> Geometry {
        let facts = ultrawide_facts();
        Geometry::from_facts(&facts, measurement(&facts, 1.0), Host::MacOs)
    }

    /// The incident display: a pure long-edge cap sent 1366x384 (unreadable);
    /// the area budget recovers ~1931x543 — same pixel count as 1366x768.
    #[test]
    fn ultrawide_full_view_uses_the_area_budget() {
        let g = ultrawide_geom();
        let region = Region::full(&g);
        let crop = crop_rect(&g, &region);
        let (sw, sh) = crop.sent_dims();

        assert_eq!((sw, sh), (1931, 543), "sent dims");
        assert!(
            sw > MAX_EDGE,
            "the long edge may exceed MAX_EDGE under the area rule"
        );
        assert!(
            sw * sh <= MAX_AREA + sw,
            "within the area budget (rounding slack)"
        );

        // The center of the sent image still maps to the display center.
        let (lx, ly) = to_logical(&g, &region, (sw as f64) / 2.0, (sh as f64) / 2.0);
        assert!((lx - 1920).abs() <= 2, "lx={lx}");
        assert!((ly - 540).abs() <= 2, "ly={ly}");
    }

    /// A 16:9 display is the budget's fixed point: 1366x768 exactly, as before.
    #[test]
    fn sixteen_nine_full_view_is_unchanged_by_the_area_budget() {
        let g = Geometry {
            phys_w: 1920,
            phys_h: 1080,
            logical_w: 1920.0,
            logical_h: 1080.0,
            origin_x: 0.0,
            origin_y: 0.0,
            scale_factor: 1.0,
        };
        let crop = crop_rect(&g, &Region::full(&g));
        assert_eq!(crop.sent_dims(), (1366, 768));
    }

    /// A crop that fits the long edge ships native — the incident's 1355x959
    /// region crop (1.30MP) must NOT be shrunk by the area rule; the looser
    /// budget wins. This is the regression a pure-area budget would introduce.
    #[test]
    fn a_crop_that_fits_the_long_edge_stays_native() {
        let crop = CropRect {
            left_phys: 64.7,
            top_phys: 30.9,
            w_phys: 1355.0,
            h_phys: 959.0,
        };
        assert!((crop.sent_scale() - 1.0).abs() < f32::EPSILON);
        assert_eq!(crop.sent_dims(), (1355, 959));
    }

    /// Retina 16:10 (2880x1800): the edge rule (0.474) beats the area rule
    /// (0.450) — behavior identical to the pure long-edge cap.
    #[test]
    fn retina_full_view_keeps_the_long_edge_budget() {
        let g = retina_geom();
        let crop = crop_rect(&g, &Region::full(&g));
        let (sw, _sh) = crop.sent_dims();
        assert_eq!(sw, MAX_EDGE);
    }

    #[test]
    fn full_mapping_is_correct_when_logical_fits_but_physical_does_not() {
        // The full sent image is the PHYSICAL display downscaled (1366 wide), NOT the
        // logical one left at 1.0. A click read off it must map to the logical center,
        // and Region::full's coordinate space must equal the real sent dims.
        let g = macbook_air_geom();
        let region = Region::full(&g);

        let crop = crop_rect(&g, &region);
        let (sw, sh) = crop.sent_dims();

        // sent_scale is sent_dim/logical_dim — not a clamped 1.0.
        let k = sent_scale(&g);
        assert!((k - sw as f32 / g.logical_w).abs() < 0.01, "k={k} sw={sw}");

        // Region::full's reported width equals the actual sent width (the canary that
        // a logical-derived scale would break: it would report 1280, not ~1366).
        assert_eq!(region.w.round() as u32, sw);
        assert!(sw > 1300 && sw <= MAX_EDGE, "sw={sw}");

        // The center of the sent image maps to the logical center (640, 400).
        let (lx, ly) = to_logical(&g, &region, (sw as f64) / 2.0, (sh as f64) / 2.0);
        assert!((lx - 640).abs() <= 2, "lx={lx}");
        assert!((ly - 400).abs() <= 2, "ly={ly}");
    }

    // Fermix re-derives its crop after every mutating action: a `screenshot` whose
    // `region` is the WHOLE of the image it just addressed. That must be a fixed
    // point. It was not — the corners took the pixel-centre half, so the view slid
    // half a pixel and lost one off its width every single round, and ten actions
    // in a row walked it visibly across the screen.
    #[test]
    fn re_deriving_a_view_from_its_own_image_does_not_move_it() {
        for ratio in [1.0_f32, 2.0] {
            let facts = retina_facts();
            let g = Geometry::from_facts(&facts, measurement(&facts, ratio), Host::MacOs);
            let k = sent_scale(&g);

            for start in [
                Region::full(&g),
                // A non-origin crop: the lower-right quadrant.
                Region {
                    x: (756.0 * k) as f64,
                    y: (491.0 * k) as f64,
                    w: (756.0 * k) as f64,
                    h: (491.0 * k) as f64,
                },
            ] {
                let mut region = start;
                let mut dims = crop_rect(&g, &region).sent_dims();
                let original_dims = dims;
                let mut settled = None;

                for round in 1..=20 {
                    let whole = Region {
                        x: 0.0,
                        y: 0.0,
                        w: dims.0 as f64,
                        h: dims.1 as f64,
                    };
                    region = rect_through(&g, &region, &whole, &g);
                    dims = crop_rect(&g, &region).sent_dims();

                    // The image is the same image, every round, from the first.
                    // This is the assertion that fails on the defect: the width
                    // shrank a pixel at a time, 1366x887 to 1366x886 and on down.
                    assert_eq!(
                        dims, original_dims,
                        "round {round} at ratio {ratio} resized the image"
                    );

                    // And the rectangle is a fixed point. The first round may
                    // round the full image's fractional extent to whole pixels —
                    // that is one snap, not a walk, so everything after it must be
                    // identical.
                    match settled {
                        None => settled = Some(region),
                        Some(first) => assert_eq!(
                            region, first,
                            "round {round} at ratio {ratio} moved the view"
                        ),
                    }
                }
            }
        }
    }

    // A rectangle drawn on a zoomed image lands where it was drawn, not where a
    // rectangle with the same numbers on the FULL image would.
    #[test]
    fn a_rectangle_read_in_one_image_lands_through_that_image() {
        let g = retina_geom();
        let k = sent_scale(&g);
        let observed = Region {
            x: (720.0 * k) as f64,
            y: (450.0 * k) as f64,
            w: (720.0 * k) as f64,
            h: (450.0 * k) as f64,
        };
        let (ow, oh) = crop_rect(&g, &observed).sent_dims();

        // The middle quarter of THAT image is the middle of the lower-right quadrant.
        let rect = Region {
            x: (ow / 4) as f64,
            y: (oh / 4) as f64,
            w: (ow / 2) as f64,
            h: (oh / 2) as f64,
        };
        let placed = rect_through(&g, &observed, &rect, &g);

        let expect_x = (720.0 + 720.0 / 4.0) * k;
        let expect_w = (720.0 / 2.0) * k;
        assert!((placed.x - expect_x as f64).abs() < 2.0, "{placed:?}");
        assert!((placed.w - expect_w as f64).abs() < 2.0, "{placed:?}");

        // The same rectangle read on the FULL image is somewhere else entirely,
        // which is the whole reason the image has to be named.
        let full = Region::full(&g);
        let straight = rect_through(&g, &full, &rect, &g);
        assert!(straight.x < placed.x - 10.0, "{straight:?} vs {placed:?}");
    }

    // A control's frame reaches the wire in the SAME pixels as the click point
    // beside it: one reply, one coordinate space. The click point is the frame's
    // centre through `to_sent`, so the two must agree by construction.
    #[test]
    fn a_frame_and_its_click_point_land_in_the_same_space() {
        let g = geom(&retina_facts(), (3024, 1964));
        let full = Region::full(&g);

        // A control at logical (400, 300), 80 by 24 points.
        let bounds = sent_rect(&g, &full, 400.0, 300.0, 80.0, 24.0);
        let (cx, cy) = to_sent(&g, &full, 440.0, 312.0).expect("its centre is in view");

        assert!(
            (bounds.x + bounds.w / 2.0 - cx as f64).abs() <= 1.0,
            "the click point must sit at the middle of the bounds: {bounds:?} vs {cx}"
        );
        assert!(
            (bounds.y + bounds.h / 2.0 - cy as f64).abs() <= 1.0,
            "{bounds:?} vs {cy}"
        );

        // It is a rectangle on the screen, so its width survives the mapping: at
        // 2x on a 1366-wide budget, 80 points is 80 * 2 * (1366/3024) sent pixels.
        let expected = 80.0 * 2.0 * (crop_rect(&g, &full).sent_scale() as f64);
        assert!((bounds.w - expected).abs() < 1.0, "{bounds:?}");
    }

    // A control whose click point is in view can still run past an edge, and the
    // caller reading a negative x there learns something true. Clamping would say
    // the control is smaller than it is, which is worse than saying where it goes.
    #[test]
    fn a_frame_that_runs_past_an_edge_is_not_clamped() {
        let g = geom(&ultrawide_facts(), (3840, 1080));
        let crop = Region {
            x: 400.0,
            y: 100.0,
            w: 600.0,
            h: 400.0,
        };

        let bounds = sent_rect(&g, &crop, 0.0, 0.0, 200.0, 50.0);
        assert!(
            bounds.x < 0.0,
            "it starts to the left of this crop: {bounds:?}"
        );
        assert!(bounds.w > 0.0);
    }
}
