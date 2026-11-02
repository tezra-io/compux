//! Where the badge goes, as a function of the target's rectangle and the monitors.
//!
//! The same rules as the macOS badge's `Placement`, in the one space X11 has: root
//! pixels, top-left origin, y growing downwards. Above the window's top edge when
//! the monitor has room there, else just inside its top-left below the title bar;
//! and always inside the rectangle the helper hit-tests for anything covering the
//! badge (`badge_area.rs`), or not drawn at all.

use crate::badge_area::{BADGE_ABOVE, BADGE_HEIGHT, BADGE_INSIDE_DROP, BADGE_WIDTH};
use crate::wire::{Activity, Bounds, TargetState};

/// Between the badge and the window's top edge when it sits outside.
pub const GAP: i32 = 6;
/// From the window's left edge when it sits inside.
pub const INSET: i32 = 8;
/// The badge's widest: the contract's width less the inset.
pub const MAX_WIDTH: i32 = BADGE_WIDTH as i32 - INSET;
/// The badge's height, always.
pub const HEIGHT: i32 = BADGE_HEIGHT as i32;
/// Coordinates past this are clamped to it, so no sum below can overflow. A
/// million pixels is beyond any desktop.
pub const MAX_COORDINATE: f64 = 1_048_576.0;

/// A rectangle in root pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(self) -> i32 {
        self.y + self.h
    }

    /// The overlap, or `None` when the two do not share a pixel.
    pub fn intersection(self, other: Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            return None;
        }
        Some(Rect {
            x,
            y,
            w: right - x,
            h: bottom - y,
        })
    }

    #[cfg(test)]
    pub fn contains(self, other: Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }
}

/// One monitor: all of it, and the part the badge may use (less the panels).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen {
    pub frame: Rect,
    pub visible: Rect,
}

/// Every pixel the badge may be drawn on for a window at `target`: the helper's
/// `badge_area`, seen from this side.
pub fn contract(target: Rect) -> Rect {
    Rect {
        x: target.x,
        y: target.y - BADGE_ABOVE as i32,
        w: BADGE_WIDTH as i32,
        h: (BADGE_ABOVE + BADGE_INSIDE_DROP + BADGE_HEIGHT) as i32,
    }
}

/// Where a badge of this size belongs, or `None` when it must not be shown.
///
/// Hidden, and each is a rule rather than a fallback: the state is unavailable,
/// the target is occluded, the helper sent no bounds, the target is on no monitor,
/// or the part of the contract that monitor shows is too small to hold the badge.
pub fn badge_frame(state: &TargetState, size: (i32, i32), screens: &[Screen]) -> Option<Rect> {
    if state.activity == Activity::Unavailable || state.occluded {
        return None;
    }
    let target = target_rect(state.bounds?);
    let screen = screen_showing(target, screens)?;

    // Where it may go at all: the contract, and only the part this monitor shows.
    let allowed = contract(target).intersection(screen.visible)?;
    let (width, height) = size;
    if allowed.w < width || allowed.h < height {
        return None;
    }

    // Above the window's top edge when the monitor has room there, else inside
    // its top-left, as deep as the contract allows: below a title bar or a
    // client-side header bar.
    let above = Rect {
        x: target.x,
        y: target.y - GAP - height,
        w: width,
        h: height,
    };
    let inside = Rect {
        x: target.x + INSET,
        y: target.y + BADGE_INSIDE_DROP as i32,
        w: width,
        h: height,
    };
    let chosen = if above.y >= screen.visible.y {
        above
    } else {
        inside
    };
    Some(clamp(chosen, allowed))
}

/// The target's rectangle in whole pixels, clamped so no sum can overflow.
fn target_rect(bounds: Bounds) -> Rect {
    let whole = |value: f64| value.round().clamp(-MAX_COORDINATE, MAX_COORDINATE) as i32;
    Rect {
        x: whole(bounds.x),
        y: whole(bounds.y),
        w: whole(bounds.w).max(1),
        h: whole(bounds.h).max(1),
    }
}

/// The monitor showing most of the target: the one the person is looking at.
fn screen_showing(target: Rect, screens: &[Screen]) -> Option<&Screen> {
    let area = |screen: &&Screen| {
        screen
            .frame
            .intersection(target)
            .map_or(0, |overlap| i64::from(overlap.w) * i64::from(overlap.h))
    };
    screens
        .iter()
        .filter(|screen| area(screen) > 0)
        .max_by_key(area)
}

/// Keep the badge inside `bounds`; when it is wider, its left edge wins.
fn clamp(rect: Rect, bounds: Rect) -> Rect {
    let x = rect
        .x
        .max(bounds.x)
        .min(bounds.x.max(bounds.right() - rect.w));
    let y = rect
        .y
        .max(bounds.y)
        .min(bounds.y.max(bounds.bottom() - rect.h));
    Rect { x, y, ..rect }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    fn screen(frame: Rect) -> Screen {
        Screen {
            frame,
            visible: frame,
        }
    }

    fn at(x: f64, y: f64, w: f64, h: f64) -> TargetState {
        TargetState {
            bounds: Some(Bounds { x, y, w, h }),
            ..TargetState::initial()
        }
    }

    const BADGE: (i32, i32) = (300, HEIGHT);

    #[test]
    fn the_contract_is_the_helpers_badge_area() {
        assert_eq!(contract(rect(100, 200, 800, 600)), rect(100, 166, 448, 114));
        assert_eq!(MAX_WIDTH, 440);
    }

    #[test]
    fn with_room_above_the_window_the_badge_floats_over_its_top_edge() {
        let screens = [screen(rect(0, 0, 1920, 1080))];
        let frame = badge_frame(&at(100.0, 200.0, 800.0, 600.0), BADGE, &screens);
        assert_eq!(frame, Some(rect(100, 166, 300, 28)));
    }

    #[test]
    fn without_room_above_it_sits_inside_below_the_title_bar() {
        let screens = [screen(rect(0, 0, 1920, 1080))];
        let frame = badge_frame(&at(100.0, 10.0, 800.0, 600.0), BADGE, &screens);
        assert_eq!(frame, Some(rect(108, 62, 300, 28)));
    }

    #[test]
    fn a_panel_at_the_top_of_the_monitor_counts_as_no_room() {
        let screens = [Screen {
            frame: rect(0, 0, 1920, 1080),
            visible: rect(0, 32, 1920, 1048),
        }];
        let frame = badge_frame(&at(0.0, 40.0, 800.0, 600.0), BADGE, &screens).expect("placed");
        assert_eq!(frame, rect(8, 92, 300, 28));
    }

    #[test]
    fn the_badge_hides_when_it_has_nothing_true_to_say_where_it_would_sit() {
        let screens = [screen(rect(0, 0, 1920, 1080))];
        let place = |state: &TargetState| badge_frame(state, BADGE, &screens);
        let window = at(100.0, 200.0, 800.0, 600.0);
        assert!(place(&window).is_some());
        assert_eq!(
            place(&TargetState {
                activity: Activity::Unavailable,
                ..window.clone()
            }),
            None
        );
        assert_eq!(
            place(&TargetState {
                occluded: true,
                ..window.clone()
            }),
            None
        );
        assert_eq!(
            place(&TargetState {
                bounds: None,
                ..window.clone()
            }),
            None
        );
        assert_eq!(
            place(&at(5000.0, 200.0, 800.0, 600.0)),
            None,
            "on no monitor"
        );
        assert_eq!(
            place(&at(1700.0, 200.0, 200.0, 600.0)),
            None,
            "contract cut by the edge"
        );
    }

    #[test]
    fn a_paused_target_still_shows_its_badge() {
        let screens = [screen(rect(0, 0, 1920, 1080))];
        let paused = TargetState {
            activity: Activity::Paused,
            ..at(100.0, 200.0, 800.0, 600.0)
        };
        assert!(badge_frame(&paused, BADGE, &screens).is_some());
    }

    #[test]
    fn a_window_across_two_monitors_is_badged_on_the_one_holding_most_of_it() {
        let left = Screen {
            frame: rect(0, 0, 1920, 1080),
            visible: rect(0, 0, 1920, 1080),
        };
        let right = Screen {
            frame: rect(1920, 0, 1280, 1024),
            visible: rect(1920, 40, 1280, 984),
        };
        // Mostly on the right one: placed there, pulled onto it, under its panel.
        // On the left one the contract's sliver is too narrow and it would hide.
        let frame = badge_frame(&at(1800.0, 30.0, 900.0, 500.0), BADGE, &[left, right]);
        assert_eq!(frame, Some(rect(1920, 82, 300, 28)));
        let frame = badge_frame(&at(1920.0, 30.0, 900.0, 500.0), BADGE, &[left, right]);
        assert_eq!(frame, Some(rect(1928, 82, 300, 28)));
    }

    #[test]
    fn wherever_the_window_is_the_badge_never_leaves_the_contract_or_the_monitor() {
        let screens = [
            Screen {
                frame: rect(0, 0, 1920, 1080),
                visible: rect(0, 27, 1920, 1053),
            },
            screen(rect(1920, -200, 1080, 1920)),
        ];
        let mut placed = 0;
        for x in (-400..3200).step_by(97) {
            for y in (-300..1900).step_by(53) {
                for width in [120, 300, 440] {
                    let state = at(f64::from(x), f64::from(y), 640.0, 400.0);
                    let Some(frame) = badge_frame(&state, (width, HEIGHT), &screens) else {
                        continue;
                    };
                    placed += 1;
                    assert!(
                        contract(rect(x, y, 640, 400)).contains(frame),
                        "{x},{y}: {frame:?}"
                    );
                    assert!(
                        screens.iter().any(|s| s.visible.contains(frame)),
                        "{x},{y}: {frame:?}"
                    );
                    assert_eq!((frame.w, frame.h), (width, HEIGHT));
                }
            }
        }
        assert!(placed > 500, "only {placed} placements were tried");
    }

    #[test]
    fn coordinates_past_any_desktop_are_clamped_rather_than_overflowing() {
        let screens = [screen(rect(0, 0, 1920, 1080))];
        assert_eq!(
            badge_frame(&at(1e300, -1e300, 1e300, 1e300), BADGE, &screens),
            None
        );
    }

    #[test]
    fn rectangles_overlap_only_where_they_share_pixels() {
        assert_eq!(
            rect(0, 0, 10, 10).intersection(rect(5, 5, 10, 10)),
            Some(rect(5, 5, 5, 5))
        );
        assert_eq!(rect(0, 0, 10, 10).intersection(rect(10, 0, 10, 10)), None);
    }
}
