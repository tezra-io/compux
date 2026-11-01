//! The badge's row, in pixels: where each piece goes, and which two rectangles
//! take clicks.
//!
//! ```text
//!   ● Fermix  Files — Home       Working  [ Pause ] [ Stop ]
//! ```
//!
//! The target's label is the one piece that gives ground: it is cut, with an
//! ellipsis, until the row fits the widest the contract allows. The state word and
//! the buttons keep their size, so a long title costs characters and never costs
//! the person a Stop button. The buttons are sized for their longer titles, so a
//! pause does not move them under the person's pointer.

use crate::place::{Rect, HEIGHT, MAX_WIDTH};
use crate::wire::Activity;

pub const PAD_LEFT: i32 = 10;
pub const PAD_RIGHT: i32 = 8;
pub const SPACING: i32 = 8;
/// The state mark's diameter.
pub const DOT: i32 = 10;
/// Either side of a button's title.
pub const BUTTON_PAD: i32 = 8;
pub const BUTTON_HEIGHT: i32 = 20;
/// The application's name, which is not untrusted.
pub const NAME: &str = "Fermix";

/// A button on the badge. Which event the first one sends is the helper's state's
/// business: Pause while working, Resume while paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Pause,
    Stop,
}

/// A run of text and the left edge it is drawn from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    pub text: String,
    pub x: i32,
}

/// Every piece of the row, placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: i32,
    pub dot: Rect,
    pub name: Text,
    pub label: Text,
    pub word: Text,
    pub pause: Rect,
    pub pause_title: Text,
    pub stop: Rect,
    pub stop_title: Text,
}

/// The state, as a word.
pub fn word(activity: Activity) -> &'static str {
    match activity {
        Activity::Working => "Working",
        Activity::Paused => "Paused",
        Activity::Unavailable => "Unavailable",
    }
}

/// The first button's title: what pressing it would do.
pub fn pause_title(activity: Activity) -> &'static str {
    match activity {
        Activity::Paused => "Resume",
        Activity::Working | Activity::Unavailable => "Pause",
    }
}

/// Lay the row out. `measure` is the drawn width of a string in pixels, so the
/// rules here are proved with any font, including a fixed-width stand-in.
pub fn layout(activity: Activity, label: &str, measure: &dyn Fn(&str) -> i32) -> Layout {
    let dot = Rect {
        x: PAD_LEFT,
        y: (HEIGHT - DOT) / 2,
        w: DOT,
        h: DOT,
    };
    let name = Text {
        text: NAME.to_string(),
        x: dot.right() + SPACING,
    };
    let label_x = name.x + measure(NAME) + SPACING;

    // Everything right of the label is the same width in every state.
    let word_slot = [Activity::Working, Activity::Paused, Activity::Unavailable]
        .map(|state| measure(word(state)))
        .into_iter()
        .max()
        .unwrap_or(0);
    let pause_w = measure("Pause").max(measure("Resume")) + 2 * BUTTON_PAD;
    let stop_w = measure("Stop") + 2 * BUTTON_PAD;
    let right = word_slot + SPACING + pause_w + SPACING + stop_w + PAD_RIGHT;

    let room = MAX_WIDTH - label_x - SPACING - right;
    let label = Text {
        text: fit(label, room, measure),
        x: label_x,
    };
    let word_x = label_x + measure(&label.text) + SPACING;

    let top = (HEIGHT - BUTTON_HEIGHT) / 2;
    let pause = Rect {
        x: word_x + word_slot + SPACING,
        y: top,
        w: pause_w,
        h: BUTTON_HEIGHT,
    };
    let stop = Rect {
        x: pause.right() + SPACING,
        y: top,
        w: stop_w,
        h: BUTTON_HEIGHT,
    };

    Layout {
        width: stop.right() + PAD_RIGHT,
        dot,
        name,
        label,
        word: Text {
            text: word(activity).to_string(),
            x: word_x,
        },
        pause,
        pause_title: centred(pause_title(activity), pause, measure),
        stop,
        stop_title: centred("Stop", stop, measure),
    }
}

/// `text`, or as much of it as fits in `room` with an ellipsis after it. Bounded
/// by its length: the label is already cut to a few dozen characters.
fn fit(text: &str, room: i32, measure: &dyn Fn(&str) -> i32) -> String {
    if measure(text) <= room {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    for keep in (0..chars.len()).rev() {
        let cut: String = chars[..keep]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string()
            + "…";
        if measure(&cut) <= room {
            return cut;
        }
    }
    String::new()
}

/// A button's title, centred in it.
fn centred(title: &str, button: Rect, measure: &dyn Fn(&str) -> i32) -> Text {
    Text {
        text: title.to_string(),
        x: button.x + (button.w - measure(title)) / 2,
    }
}

impl Layout {
    /// The button under a point in the badge's own coordinates, if any.
    pub fn hit(&self, x: i32, y: i32) -> Option<Button> {
        let inside =
            |rect: Rect| x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom();
        if inside(self.pause) {
            return Some(Button::Pause);
        }
        if inside(self.stop) {
            return Some(Button::Stop);
        }
        None
    }

    /// The only pixels that take a click. Everywhere else on the badge, a click
    /// reaches whatever is under it.
    pub fn input_region(&self) -> [Rect; 2] {
        [self.pause, self.stop]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seven pixels a character: a stand-in font.
    fn fixed(text: &str) -> i32 {
        7 * text.chars().count() as i32
    }

    fn centre(rect: Rect) -> (i32, i32) {
        (rect.x + rect.w / 2, rect.y + rect.h / 2)
    }

    #[test]
    fn the_words_and_titles_follow_the_state() {
        assert_eq!(word(Activity::Working), "Working");
        assert_eq!(word(Activity::Paused), "Paused");
        assert_eq!(word(Activity::Unavailable), "Unavailable");
        assert_eq!(pause_title(Activity::Working), "Pause");
        assert_eq!(pause_title(Activity::Paused), "Resume");
        assert_eq!(pause_title(Activity::Unavailable), "Pause");
    }

    #[test]
    fn a_short_label_lays_out_whole_in_reading_order() {
        let row = layout(Activity::Working, "Files — Home", &fixed);
        assert_eq!(row.label.text, "Files — Home");
        assert_eq!(row.name.text, NAME);
        assert!(row.dot.right() <= row.name.x);
        assert!(row.name.x + fixed(NAME) <= row.label.x);
        assert!(row.label.x + fixed(&row.label.text) <= row.word.x);
        assert!(row.word.x < row.pause.x && row.pause.right() <= row.stop.x);
        assert_eq!(row.width, row.stop.right() + PAD_RIGHT);
        assert!(row.width < MAX_WIDTH);
    }

    #[test]
    fn a_long_label_is_cut_with_an_ellipsis_and_the_row_stays_within_the_contract() {
        let row = layout(Activity::Working, &"W".repeat(75), &fixed);
        assert!(row.width <= MAX_WIDTH, "{}", row.width);
        assert!(row.label.text.ends_with('…'), "{}", row.label.text);
        assert!(row.label.text.chars().count() < 75);
        assert_eq!(row.stop.w, fixed("Stop") + 2 * BUTTON_PAD);
    }

    #[test]
    fn the_buttons_do_not_move_when_the_target_pauses() {
        let working = layout(Activity::Working, "Files — Home", &fixed);
        let paused = layout(Activity::Paused, "Files — Home", &fixed);
        assert_eq!(working.pause, paused.pause);
        assert_eq!(working.stop, paused.stop);
        assert_eq!(paused.pause_title.text, "Resume");
        assert_eq!(working.pause_title.text, "Pause");
    }

    #[test]
    fn the_two_buttons_are_the_whole_input_region() {
        let row = layout(Activity::Working, "Files — Home", &fixed);
        let [first, second] = row.input_region();
        assert_eq!((first, second), (row.pause, row.stop));
        assert!(first.intersection(second).is_none());
        let badge = Rect {
            x: 0,
            y: 0,
            w: row.width,
            h: HEIGHT,
        };
        assert!(badge.contains(first) && badge.contains(second));
        assert_eq!(first.h, BUTTON_HEIGHT);
    }

    #[test]
    fn a_click_is_a_button_only_inside_one() {
        let row = layout(Activity::Working, "Files — Home", &fixed);
        let (x, y) = centre(row.pause);
        assert_eq!(row.hit(x, y), Some(Button::Pause));
        let (x, y) = centre(row.stop);
        assert_eq!(row.hit(x, y), Some(Button::Stop));
        assert_eq!(row.hit(row.label.x + 2, HEIGHT / 2), None, "the label");
        assert_eq!(row.hit(row.dot.x + 1, HEIGHT / 2), None, "the mark");
        assert_eq!(
            row.hit(row.pause.right(), HEIGHT / 2),
            None,
            "the gap after Pause"
        );
        assert_eq!(row.hit(row.stop.x, row.stop.y - 1), None, "just above Stop");
        assert_eq!(
            row.hit(row.stop.x, row.stop.y),
            Some(Button::Stop),
            "Stop's corner"
        );
        assert_eq!(row.hit(-1, HEIGHT / 2), None);
        assert_eq!(row.hit(row.width + 3, HEIGHT / 2), None);
    }

    #[test]
    fn button_titles_are_centred_in_their_buttons() {
        let row = layout(Activity::Paused, "x", &fixed);
        let left = row.pause_title.x - row.pause.x;
        let right = row.pause.right() - (row.pause_title.x + fixed("Resume"));
        assert!((left - right).abs() <= 1, "{left} {right}");
        assert_eq!(row.stop_title.x - row.stop.x, BUTTON_PAD);
    }
}
