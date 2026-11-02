//! The badge's size contract: the numbers the helper hit-tests for anything
//! covering the badge, and the numbers the badge draws inside.
//!
//! The helper turns them into one rectangle per window (`indicator::badge_area`);
//! the badge places itself inside that rectangle or not at all. The macOS badge is
//! Swift and repeats them in `native/indicator/Placement.swift`. The X11 badge is a
//! binary of this crate and compiles this file, so on Linux there is one copy.

/// From the window's left edge: the badge's inset (8) plus its widest (440).
pub const BADGE_WIDTH: f64 = 448.0;
/// The badge's height, always.
pub const BADGE_HEIGHT: f64 = 28.0;
/// How far above the window's top edge the outside placement floats.
pub const BADGE_ABOVE: f64 = 34.0;
/// How far below the window's top edge the inside placement may sit, clear of
/// the title bar.
pub const BADGE_INSIDE_DROP: f64 = 52.0;
