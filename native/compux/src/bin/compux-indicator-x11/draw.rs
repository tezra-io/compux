//! The badge's pixels, drawn in this process from an embedded font.
//!
//! Nothing here talks to the X server: a layout and a state in, a buffer of 32-bit
//! pixels out (blue, green, red, unused: a 24-bit TrueColor visual's least
//! significant byte first), which `x11.rs` puts on the window as it is. The font is
//! Noto Sans, unmodified, under the SIL Open Font License that sits beside it in
//! `fonts/` and that `--license` prints.
//!
//! Glyphs are drawn one after another in the order the characters arrive, with the
//! font's kerning and nothing more: no shaping and no bidirectional reordering, so
//! a script that needs either reads wrongly, and a character the font has no glyph
//! for is drawn as its missing-glyph box. Neither can move anything else on the row.

use ab_glyph::{point, Font, FontRef, GlyphId, PxScale, ScaleFont};

use crate::layout::{Layout, Text};
use crate::place::{Rect, HEIGHT};
use crate::wire::Activity;

/// The font, compiled in: the badge reads no file at run time.
pub const FONT: &[u8] = include_bytes!("../../../fonts/NotoSans-Regular.ttf");
/// Its licence, which `--license` prints.
pub const FONT_LICENSE: &str = include_str!("../../../fonts/OFL.txt");
/// Every piece of text on the badge is drawn at this size, in pixels per em.
pub const TEXT_PX: f32 = 13.0;
/// How thick the unavailable state's ring is.
pub const RING: f32 = 1.5;

pub const BACKGROUND: u32 = 0x2b2b2b;
pub const BORDER: u32 = 0x4a4a4a;
pub const TEXT: u32 = 0xf2f2f2;
pub const SECONDARY: u32 = 0xb4b4b4;
pub const BUTTON: u32 = 0x3d3d3d;
pub const BUTTON_BORDER: u32 = 0x5c5c5c;
pub const GREEN: u32 = 0x34c759;
pub const ORANGE: u32 = 0xff9f0a;
pub const GREY: u32 = 0x8e8e93;

/// The font, ready to measure and draw.
pub struct Painter {
    font: FontRef<'static>,
    scale: PxScale,
}

impl Painter {
    pub fn new() -> Result<Painter, String> {
        let font = FontRef::try_from_slice(FONT)
            .map_err(|e| format!("the embedded font did not load: {e}"))?;
        let units = font
            .units_per_em()
            .ok_or_else(|| "the embedded font names no units per em".to_string())?;
        // ab_glyph scales by ascent-to-descent height, not by the em.
        let scale = PxScale::from(TEXT_PX * font.height_unscaled() / units);
        Ok(Painter { font, scale })
    }

    /// The width `text` takes when drawn, in whole pixels: the same advances and
    /// kerning `text` below draws with.
    pub fn measure(&self, text: &str) -> i32 {
        let scaled = self.font.as_scaled(self.scale);
        let mut width = 0.0f32;
        let mut previous: Option<GlyphId> = None;
        for c in text.chars() {
            let id = scaled.glyph_id(c);
            width += previous.map_or(0.0, |before| scaled.kern(before, id));
            width += scaled.h_advance(id);
            previous = Some(id);
        }
        width.ceil() as i32
    }

    /// The whole badge for this layout and state.
    pub fn render(&self, layout: &Layout, activity: Activity) -> Canvas {
        let mut canvas = Canvas::new(layout.width, HEIGHT, BACKGROUND);
        canvas.frame(
            Rect {
                x: 0,
                y: 0,
                w: layout.width,
                h: HEIGHT,
            },
            BORDER,
        );

        mark(&mut canvas, layout.dot, activity);
        self.text(&mut canvas, &layout.name, TEXT);
        self.text(&mut canvas, &layout.label, SECONDARY);
        let word = if activity == Activity::Unavailable {
            SECONDARY
        } else {
            TEXT
        };
        self.text(&mut canvas, &layout.word, word);

        for (button, title) in [
            (layout.pause, &layout.pause_title),
            (layout.stop, &layout.stop_title),
        ] {
            canvas.fill(button, BUTTON);
            canvas.frame(button, BUTTON_BORDER);
            self.text(&mut canvas, title, TEXT);
        }
        canvas
    }

    /// One run of text on the row's baseline, glyph after glyph.
    fn text(&self, canvas: &mut Canvas, run: &Text, colour: u32) {
        let scaled = self.font.as_scaled(self.scale);
        let baseline = ((HEIGHT as f32 - scaled.height()) / 2.0 + scaled.ascent()).round();
        let mut x = run.x as f32;
        let mut previous: Option<GlyphId> = None;
        for c in run.text.chars() {
            let id = scaled.glyph_id(c);
            x += previous.map_or(0.0, |before| scaled.kern(before, id));
            self.glyph(canvas, id, (x, baseline), colour);
            x += scaled.h_advance(id);
            previous = Some(id);
        }
    }

    /// One glyph's coverage, blended onto the canvas. A glyph with no outline
    /// (a space) draws nothing.
    fn glyph(&self, canvas: &mut Canvas, id: GlyphId, at: (f32, f32), colour: u32) {
        let glyph = id.with_scale_and_position(self.scale, point(at.0, at.1));
        let Some(outlined) = self.font.outline_glyph(glyph) else {
            return;
        };
        let bounds = outlined.px_bounds();
        let (left, top) = (bounds.min.x as i32, bounds.min.y as i32);
        outlined.draw(|x, y, coverage| {
            canvas.blend(left + x as i32, top + y as i32, colour, coverage);
        });
    }
}

/// The state mark: a green dot, two orange bars, or a grey ring. Never a spinner:
/// one glance says the state and nothing moves.
fn mark(canvas: &mut Canvas, dot: Rect, activity: Activity) {
    match activity {
        Activity::Working => canvas.disc(dot, GREEN),
        Activity::Paused => {
            let bar = |x: i32| Rect {
                x: dot.x + x,
                y: dot.y,
                w: 3,
                h: dot.h,
            };
            canvas.fill(bar(1), ORANGE);
            canvas.fill(bar(6), ORANGE);
        }
        Activity::Unavailable => canvas.ring(dot, GREY),
    }
}

/// A buffer of pixels, rows top to bottom, four bytes each.
pub struct Canvas {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: i32, height: i32, colour: u32) -> Canvas {
        let pixel = bytes(colour);
        let count = (width.max(0) * height.max(0)) as usize;
        Canvas {
            width,
            height,
            pixels: pixel.repeat(count),
        }
    }

    /// The colour at a point, as 0xRRGGBB, or `None` off the canvas.
    #[cfg(test)]
    pub fn pixel(&self, x: i32, y: i32) -> Option<u32> {
        let at = self.offset(x, y)?;
        let [blue, green, red, _] = [0, 1, 2, 3].map(|i| u32::from(self.pixels[at + i]));
        Some(red << 16 | green << 8 | blue)
    }

    /// Where a point's four bytes start, or `None` off the canvas. Every write goes
    /// through here, so nothing a glyph does can reach outside the buffer.
    fn offset(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        Some(((y * self.width + x) * 4) as usize)
    }

    fn fill(&mut self, rect: Rect, colour: u32) {
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                self.blend(x, y, colour, 1.0);
            }
        }
    }

    /// A one-pixel outline just inside `rect`.
    fn frame(&mut self, rect: Rect, colour: u32) {
        for (x, w, y, h) in [
            (rect.x, rect.w, rect.y, 1),
            (rect.x, rect.w, rect.bottom() - 1, 1),
            (rect.x, 1, rect.y, rect.h),
            (rect.right() - 1, 1, rect.y, rect.h),
        ] {
            self.fill(Rect { x, y, w, h }, colour);
        }
    }

    /// A filled circle inscribed in `rect`, its edge smoothed.
    fn disc(&mut self, rect: Rect, colour: u32) {
        self.circle(rect, colour, f32::INFINITY);
    }

    /// A circle's outline inscribed in `rect`.
    fn ring(&mut self, rect: Rect, colour: u32) {
        self.circle(rect, colour, RING);
    }

    /// Coverage is how much of each pixel lies between the outer edge and an inner
    /// one `thickness` in from it.
    fn circle(&mut self, rect: Rect, colour: u32, thickness: f32) {
        let radius = rect.w.min(rect.h) as f32 / 2.0;
        let (cx, cy) = (rect.x as f32 + radius, rect.y as f32 + radius);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                let distance = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
                let outer = (radius + 0.5 - distance).clamp(0.0, 1.0);
                let inner = (radius - thickness + 0.5 - distance).clamp(0.0, 1.0);
                self.blend(x, y, colour, outer - inner);
            }
        }
    }

    /// `colour` over what is there, by `coverage` from 0 (none) to 1 (all).
    fn blend(&mut self, x: i32, y: i32, colour: u32, coverage: f32) {
        let Some(at) = self.offset(x, y) else {
            return;
        };
        let coverage = coverage.clamp(0.0, 1.0);
        if coverage <= 0.0 {
            return;
        }
        let over = bytes(colour);
        for (under, over) in self.pixels[at..at + 3].iter_mut().zip(over) {
            let mixed = f32::from(*under) + (f32::from(over) - f32::from(*under)) * coverage;
            *under = mixed.round() as u8;
        }
    }
}

/// One pixel's bytes, least significant first: blue, green, red, then a byte the
/// visual ignores, set opaque.
fn bytes(colour: u32) -> [u8; 4] {
    [
        colour as u8,
        (colour >> 8) as u8,
        (colour >> 16) as u8,
        0xff,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::layout;
    use crate::place::Rect;

    fn painted(activity: Activity, label: &str) -> (Layout, Canvas) {
        let painter = Painter::new().expect("the embedded font loads");
        let row = layout(activity, label, &|text| painter.measure(text));
        let canvas = painter.render(&row, activity);
        (row, canvas)
    }

    fn inked(canvas: &Canvas, area: Rect, background: u32) -> usize {
        let mut count = 0;
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                if canvas.pixel(x, y).expect("on the canvas") != background {
                    count += 1;
                }
            }
        }
        count
    }

    #[test]
    fn the_embedded_font_measures_text_by_its_glyphs() {
        let painter = Painter::new().expect("the embedded font loads");
        assert_eq!(painter.measure(""), 0);
        let name = painter.measure("Fermix");
        assert!((25..80).contains(&name), "{name}");
        assert!(painter.measure("WWWW") > painter.measure("iiii"));
        assert!(painter.measure("Files — Home…") > painter.measure("Files Home"));
    }

    #[test]
    fn the_licence_beside_the_font_is_the_open_font_licence() {
        assert!(FONT_LICENSE.contains("SIL OPEN FONT LICENSE Version 1.1"));
        assert!(FONT.len() > 100_000);
    }

    #[test]
    fn a_badge_is_its_layouts_size_in_four_byte_pixels() {
        let (row, canvas) = painted(Activity::Working, "Files — Home");
        assert_eq!((canvas.width, canvas.height), (row.width, HEIGHT));
        assert_eq!(canvas.pixels.len(), (row.width * HEIGHT * 4) as usize);
        assert_eq!(canvas.pixel(0, 0), Some(BORDER));
        assert_eq!(canvas.pixel(2, 2), Some(BACKGROUND));
        assert_eq!(canvas.pixel(row.width, 0), None);
    }

    #[test]
    fn the_state_mark_is_a_green_dot_orange_bars_or_a_grey_ring() {
        let (row, working) = painted(Activity::Working, "x");
        let (cx, cy) = (row.dot.x + row.dot.w / 2, row.dot.y + row.dot.h / 2);
        assert_eq!(working.pixel(cx, cy), Some(GREEN));

        let (_, paused) = painted(Activity::Paused, "x");
        assert_eq!(paused.pixel(row.dot.x + 2, cy), Some(ORANGE));
        assert_eq!(paused.pixel(cx, cy), Some(BACKGROUND), "between the bars");

        let (_, unavailable) = painted(Activity::Unavailable, "x");
        assert_eq!(
            unavailable.pixel(cx, cy),
            Some(BACKGROUND),
            "the ring's hole"
        );
        let edge = unavailable.pixel(row.dot.x, cy).expect("on the canvas");
        assert!(
            distance(edge, GREY) < distance(edge, BACKGROUND),
            "the ring: {edge:06x}"
        );
    }

    #[test]
    fn text_is_drawn_where_the_layout_puts_it_and_nowhere_between() {
        let (row, canvas) = painted(Activity::Working, "Files — Home");
        let painter = Painter::new().expect("font");
        let name = Rect {
            x: row.name.x,
            y: 4,
            w: painter.measure(NAME_FOR_TEST),
            h: 20,
        };
        assert!(inked(&canvas, name, BACKGROUND) > 40, "the name has ink");
        let label_end = row.label.x + painter.measure(&row.label.text);
        let gap = Rect {
            x: label_end + 2,
            y: 2,
            w: row.word.x - label_end - 3,
            h: HEIGHT - 4,
        };
        assert_eq!(
            inked(&canvas, gap, BACKGROUND),
            0,
            "the gap before the state word is clear"
        );
    }

    const NAME_FOR_TEST: &str = crate::layout::NAME;

    /// How far apart two colours are, channel by channel.
    fn distance(a: u32, b: u32) -> u32 {
        [0, 8, 16]
            .iter()
            .map(|shift| ((a >> shift) & 0xff).abs_diff((b >> shift) & 0xff))
            .sum()
    }

    #[test]
    fn the_buttons_are_drawn_as_buttons_with_their_titles() {
        let (row, canvas) = painted(Activity::Working, "x");
        assert_eq!(
            canvas.pixel(row.stop.x, row.stop.y + 5),
            Some(BUTTON_BORDER)
        );
        assert_eq!(canvas.pixel(row.stop.x + 2, row.stop.y + 2), Some(BUTTON));
        let inside = Rect {
            x: row.stop.x + 1,
            y: row.stop.y + 1,
            w: row.stop.w - 2,
            h: row.stop.h - 2,
        };
        assert!(inked(&canvas, inside, BUTTON) > 20, "Stop is written on it");
    }

    // Its value is that hostile text does not panic the painter: the canvas
    // comes back at its own size, and nothing here looks at what was drawn.
    #[test]
    fn hostile_text_is_drawn_inside_the_canvas_whatever_it_holds() {
        let (_, canvas) = painted(Activity::Working, "ÅÉÎ̈g̃ 漢字 \u{10FFFD} ﷽ ꧅ 𒐫𒐫𒐫");
        assert_eq!(canvas.pixels.len(), (canvas.width * HEIGHT * 4) as usize);
    }
}
