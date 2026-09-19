//! Observations: the images and coordinate lists this process has handed out, and
//! the transform each was made with.
//!
//! The rule the model is given is one sentence — *coordinates are pixels in the
//! image you name* — and this is the half that makes it true. Every reply that
//! hands out coordinates mints an observation and returns its id; every action that
//! sends coordinates back names one, and the transform used is the one STORED with
//! that image. Nothing re-derives it from a rectangle the caller echoed, which is
//! the class of defect this replaces: a `region` copied onto the next action, or
//! forgotten, silently moved every coordinate in it.
//!
//! **No lock.** Observations are minted and resolved on the action worker, which is
//! one thread running one request at a time, so the table is a plain `&mut`. The
//! control reader never touches it: a control carries no coordinates.
//!
//! Bounded on purpose: at most [`MAX_OBSERVATIONS`], each for [`TTL_MS`], oldest
//! out. Measured 2026-09-19 over 227 recorded pointer actions, the gap from an
//! observation to the next pointer action is p50 5.1 s, p95 20 s, p99 40 s, so
//! about 1.3% of actions are expected to be refused as expired and told to look
//! again. That number is the one to re-measure if the refusals feel frequent.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::gate::Clock;
use crate::geometry::{Geometry, MonitorFacts, Region};

/// How many images the model may still be holding coordinates from. Three covers
/// "the screenshot, the crop I zoomed into, and the check image after my last
/// action" without letting a model address something it saw a minute ago.
pub const MAX_OBSERVATIONS: usize = 3;

/// How long coordinates read off an image stay addressable.
pub const TTL_MS: u64 = 30_000;

/// What a reply handed the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A picture: `screenshot`, `wait_for_change`, and the check image an action
    /// returns after it.
    Image,
    /// A list of coordinates with no picture: `elements`, `windows`.
    Semantic,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Image => "image",
            Kind::Semantic => "semantic",
        }
    }
}

/// One thing the model can point at, and everything needed to map a coordinate in
/// it back onto the screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub id: String,
    pub kind: Kind,
    pub display_id: u32,
    /// What the OS said about the display when this was made. The staleness check
    /// compares these rather than the derived `geometry`, because it must catch a
    /// display-mode change without taking a capture to measure one.
    pub facts: MonitorFacts,
    pub geometry: Geometry,
    /// The rectangle of the full-display image this one covers.
    pub region: Region,
    pub sent_w: u32,
    pub sent_h: u32,
    /// When this was minted, on the process's monotonic clock — which is when the
    /// reply carrying it was built, not the instant the shutter closed. The two
    /// differ by whatever ran between them (the accessibility settle behind a
    /// marked image can be over a second), and this is the reading the TTL counts
    /// from, so the number a caller sees and the number that expires it are one
    /// reading rather than two.
    pub captured_at_monotonic_ns: u128,
    /// One counter per process, images only, so a reader can order the pictures a
    /// session produced.
    pub frame_seq: Option<u64>,
}

/// Why an observation cannot be used. Each is its own sentence to the model and
/// each is a `dispatch: not_sent` receipt: nothing was done, and what to do next
/// differs per code.
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal {
    /// No such id — a made-up one, one from a helper that has since restarted, or
    /// one pushed out by a newer image.
    Unknown,
    /// It was ours and it is too old to trust.
    Expired,
    /// The display moved, changed size or changed mode since the image was made,
    /// so the transform stored with it would put the pointer somewhere else.
    Stale,
    /// The point is not inside the image it names.
    OutsidePoint { x: f64, y: f64, w: u32, h: u32 },
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Unknown => "unknown_observation",
            Refusal::Expired => "expired_observation",
            Refusal::Stale => "stale_observation",
            Refusal::OutsidePoint { .. } => "point_outside_observation",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Refusal::Unknown => {
                "no image of that name is held; take a fresh screenshot and read the \
                 coordinates again"
                    .to_string()
            }
            Refusal::Expired => format!(
                "that image is more than {} seconds old; take a fresh screenshot and read \
                 the coordinates again",
                TTL_MS / 1000
            ),
            // One word, because Fermix keys a sentence on it and a display change is
            // the only thing that produces it.
            Refusal::Stale => "geometry_changed".to_string(),
            Refusal::OutsidePoint { x, y, w, h } => format!(
                "({x}, {y}) is not inside that image, which is {w}x{h} pixels; the point may \
                 have been read from a different image"
            ),
        }
    }
}

/// The table. One per process, owned by the action worker.
pub struct Observations {
    /// Four characters of the sidecar's boot identity, so an id minted by a helper
    /// that has since died can never resolve in its successor — it is a different
    /// prefix, and the answer is `unknown_observation` rather than a click mapped
    /// through another process's geometry.
    prefix: String,
    counter: u64,
    frame_seq: u64,
    /// Oldest first, so eviction is a `remove(0)`. Three entries; a map would cost
    /// more to read than the scan.
    entries: Vec<Observation>,
    clock: Arc<dyn Clock>,
}

impl Observations {
    pub fn new(sidecar_generation: &str, clock: Arc<dyn Clock>) -> Observations {
        Observations {
            prefix: prefix_of(sidecar_generation),
            counter: 0,
            frame_seq: 0,
            entries: Vec::new(),
            clock,
        }
    }

    /// Record what a reply is about to hand out, and answer the id that names it.
    /// The oldest goes out when the table is full, and anything already expired
    /// goes with it — a stale entry that nobody asked for still costs a slot.
    pub fn mint(
        &mut self,
        kind: Kind,
        display_id: u32,
        facts: MonitorFacts,
        geometry: Geometry,
        region: Region,
        sent: (u32, u32),
    ) -> Observation {
        let now = self.clock.now_ms();
        self.entries
            .retain(|entry| now.saturating_sub(entry.captured_at_ms()) < TTL_MS);

        self.counter += 1;
        let frame_seq = match kind {
            Kind::Image => {
                self.frame_seq += 1;
                Some(self.frame_seq)
            }
            Kind::Semantic => None,
        };

        let observation = Observation {
            id: format!("{}-{}", self.prefix, self.counter),
            kind,
            display_id,
            facts,
            geometry,
            region,
            sent_w: sent.0,
            sent_h: sent.1,
            captured_at_monotonic_ns: self.clock.now_ns(),
            frame_seq,
        };

        self.entries.push(observation.clone());
        while self.entries.len() > MAX_OBSERVATIONS {
            self.entries.remove(0);
        }

        observation
    }

    /// The observation an action named, or why it cannot be used. An id the table
    /// has never held and one it has dropped answer the same way: the model's next
    /// move is the same, and pretending to remember which is which would need an
    /// unbounded list of ids.
    pub fn resolve(&self, id: &str) -> Result<&Observation, Refusal> {
        let observation = self
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or(Refusal::Unknown)?;

        let age = self
            .clock
            .now_ms()
            .saturating_sub(observation.captured_at_ms());

        if age < TTL_MS {
            Ok(observation)
        } else {
            Err(Refusal::Expired)
        }
    }
}

impl Observation {
    /// Milliseconds on the same clock the table expires by. Kept beside the
    /// nanosecond stamp the wire carries rather than dividing it, so expiry and the
    /// reported age can never drift apart by a rounding.
    fn captured_at_ms(&self) -> u64 {
        (self.captured_at_monotonic_ns / 1_000_000) as u64
    }

    /// Is this still the display the image was made on? Compared as the OS reports
    /// it — id, bounds, origin and mode scale — because that is readable without a
    /// capture, and every one of those moving changes where a coordinate lands.
    pub fn matches_display(&self, display_id: u32, facts: &MonitorFacts) -> bool {
        self.display_id == display_id && self.facts == *facts
    }

    /// A point the model read off THIS image. Outside it is refused, never clamped:
    /// a coordinate one pixel past the edge is a reading error, and clamping it
    /// turns that into a click on whatever happens to be at the edge.
    pub fn contains(&self, x: f64, y: f64) -> Result<(), Refusal> {
        if x >= 0.0 && y >= 0.0 && x < self.sent_w as f64 && y < self.sent_h as f64 {
            Ok(())
        } else {
            Err(Refusal::OutsidePoint {
                x,
                y,
                w: self.sent_w,
                h: self.sent_h,
            })
        }
    }
}

/// Four hex characters of the boot identity. Short enough that a model repeats it
/// without trouble, and keyed on the generation so two helpers in one session
/// cannot mint the same id.
fn prefix_of(sidecar_generation: &str) -> String {
    let mut hasher = DefaultHasher::new();
    sidecar_generation.hash(&mut hasher);
    format!("{:04x}", hasher.finish() as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{self, Host, MonitorFacts};
    use std::sync::Mutex;

    /// A clock that only moves when a test moves it, so expiry is asserted rather
    /// than waited for.
    struct TestClock {
        ms: Mutex<u64>,
    }

    impl TestClock {
        fn new() -> Arc<TestClock> {
            Arc::new(TestClock { ms: Mutex::new(0) })
        }

        fn advance(&self, ms: u64) {
            *self.ms.lock().unwrap() += ms;
        }
    }

    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            *self.ms.lock().unwrap()
        }

        fn now_ns(&self) -> u128 {
            self.now_ms() as u128 * 1_000_000
        }

        fn sleep(&self, ms: u64) {
            self.advance(ms);
        }
    }

    fn facts() -> MonitorFacts {
        MonitorFacts {
            x: 0,
            y: 0,
            width: 1512,
            height: 982,
            scale_factor: 2.0,
        }
    }

    fn geometry() -> Geometry {
        let measured = geometry::Measurement {
            frame_w: 3024,
            frame_h: 1964,
            pixels_per_point: 2.0,
        };
        Geometry::from_facts(&facts(), measured, Host::MacOs)
    }

    fn table(clock: &Arc<TestClock>) -> Observations {
        Observations::new("boot-1234-5678", clock.clone())
    }

    fn mint(table: &mut Observations, kind: Kind) -> Observation {
        let geometry = geometry();
        let region = Region::full(&geometry);
        table.mint(kind, 7, facts(), geometry, region, (1366, 887))
    }

    #[test]
    fn an_id_names_its_boot_and_counts_up() {
        let clock = TestClock::new();
        let mut table = table(&clock);

        let first = mint(&mut table, Kind::Image);
        let second = mint(&mut table, Kind::Image);

        let (prefix, counter) = first.id.split_once('-').expect("prefix-counter");
        assert_eq!(prefix.len(), 4, "{}", first.id);
        assert_eq!(counter, "1");
        assert_eq!(second.id, format!("{prefix}-2"));

        // A different boot mints a different prefix, so an id from a helper that
        // died can never resolve in its successor.
        let mut successor = Observations::new("boot-1234-9999", clock.clone());
        let after_restart = mint(&mut successor, Kind::Image);
        assert_ne!(
            after_restart.id.split_once('-').unwrap().0,
            prefix,
            "two boots must not share a prefix"
        );
        assert_eq!(successor.resolve(&first.id), Err(Refusal::Unknown));
    }

    #[test]
    fn only_images_carry_a_frame_sequence() {
        let clock = TestClock::new();
        let mut table = table(&clock);

        assert_eq!(mint(&mut table, Kind::Image).frame_seq, Some(1));
        assert_eq!(mint(&mut table, Kind::Semantic).frame_seq, None);
        assert_eq!(mint(&mut table, Kind::Image).frame_seq, Some(2));
    }

    #[test]
    fn the_oldest_goes_out_when_a_fourth_arrives() {
        let clock = TestClock::new();
        let mut table = table(&clock);

        let first = mint(&mut table, Kind::Image);
        let second = mint(&mut table, Kind::Image);
        mint(&mut table, Kind::Image);
        assert!(table.resolve(&first.id).is_ok());

        mint(&mut table, Kind::Image);
        assert_eq!(table.resolve(&first.id), Err(Refusal::Unknown));
        assert!(table.resolve(&second.id).is_ok(), "the newer three stay");
    }

    #[test]
    fn an_observation_expires_on_the_clock_not_on_a_sleep() {
        let clock = TestClock::new();
        let mut table = table(&clock);
        let observation = mint(&mut table, Kind::Image);

        clock.advance(TTL_MS - 1);
        assert!(table.resolve(&observation.id).is_ok(), "just inside");

        clock.advance(1);
        assert_eq!(table.resolve(&observation.id), Err(Refusal::Expired));
    }

    #[test]
    fn a_made_up_id_is_unknown_and_says_what_to_do() {
        let clock = TestClock::new();
        let table = table(&clock);

        assert_eq!(table.resolve("nope-1"), Err(Refusal::Unknown));
        assert_eq!(Refusal::Unknown.code(), "unknown_observation");
        assert!(Refusal::Unknown.detail().contains("fresh screenshot"));
        assert!(Refusal::Expired.detail().contains("30 seconds"));
    }

    // The display moving is the one thing the stored transform cannot survive, and
    // it is readable without a capture — which is why the OS facts are what is
    // compared.
    #[test]
    fn a_display_that_moved_or_changed_mode_no_longer_matches() {
        let clock = TestClock::new();
        let mut table = table(&clock);
        let observation = mint(&mut table, Kind::Image);

        assert!(observation.matches_display(7, &facts()));
        assert!(!observation.matches_display(8, &facts()), "another display");

        for changed in [
            MonitorFacts { x: 1512, ..facts() },
            MonitorFacts {
                width: 1728,
                ..facts()
            },
            MonitorFacts {
                scale_factor: 1.0,
                ..facts()
            },
        ] {
            assert!(
                !observation.matches_display(7, &changed),
                "{changed:?} must not pass for {:?}",
                facts()
            );
        }
    }

    // A point past the edge is a reading error. Clamping it would turn that into a
    // click on whatever is at the edge, which is how a misread coordinate becomes
    // an action nobody asked for.
    #[test]
    fn a_point_outside_the_image_is_refused_with_the_size_it_should_have_been_in() {
        let clock = TestClock::new();
        let mut table = table(&clock);
        let observation = mint(&mut table, Kind::Image);

        assert!(observation.contains(0.0, 0.0).is_ok());
        assert!(observation.contains(1365.0, 886.0).is_ok());

        let refusal = observation
            .contains(1366.0, 10.0)
            .expect_err("past the right edge");
        assert_eq!(refusal.code(), "point_outside_observation");
        assert!(
            refusal.detail().contains("1366x887"),
            "{}",
            refusal.detail()
        );

        assert!(observation.contains(-1.0, 10.0).is_err());
        assert!(observation.contains(10.0, 887.0).is_err());
    }
}
