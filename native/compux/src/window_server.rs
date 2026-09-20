//! What the window server says about the windows on this machine, behind a trait.
//!
//! A bound target lives or dies by facts only the window server has: is that window
//! still listed, is it minimized, where is it now, and what is in front of it at the
//! point we are about to click. Every one of those is a rule with a wrong answer
//! that costs the person something — a click on whatever covered their window, a
//! click at the place a window used to be — so every one of them is a pure function
//! over a list, and the list comes through [`Windows`].
//!
//! **One method.** The window server answers the whole list in one call and every
//! fact this module needs is in it, including the front-to-back ORDER, which is not
//! a property of any single window. Splitting it into `bounds(id)`, `minimized(id)`
//! and `z_order()` would be three calls that can disagree with each other about a
//! window that moved between them.
//!
//! The macOS implementation is the only place that touches CoreGraphics; everything
//! above it is tested against [`Recorder`].

/// A rectangle in the global LOGICAL points the desktop lays its displays out in —
/// the same space `ax::Frame` and `Geometry::origin_*` are in, and the space
/// `kCGWindowBounds` answers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Bounds {
    /// Is this point inside the rectangle? Half-open, like every other rectangle
    /// in this process: the left and top edges belong to it and the right and
    /// bottom edges belong to whatever is next to it.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    /// Do two rectangles describe the same place, to within the tolerance the
    /// accessibility binding is allowed? Used to match one of an application's
    /// accessibility windows against the window the caller named.
    pub fn matches(&self, other: &Bounds, tolerance: f64) -> bool {
        (self.x - other.x).abs() <= tolerance
            && (self.y - other.y).abs() <= tolerance
            && (self.w - other.w).abs() <= tolerance
            && (self.h - other.h).abs() <= tolerance
    }

    /// Do two rectangles overlap at all? Half-open on the same edges `contains` is,
    /// so two rectangles that merely touch do not overlap.
    pub fn intersects(&self, other: &Bounds) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

/// The target was not in the listing at all — it closed, or its application quit,
/// between one look and the next.
///
/// Its own type rather than "nothing is in the way", because the two are opposite
/// answers: a hit test that cannot find the target has no idea what a click would
/// reach, and reporting that as clear is how a click lands on whatever took the
/// window's place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetMissing;

/// One window, as the window server describes it.
///
/// `app` and `title` are another application's own text and are sanitised at this
/// boundary exactly as an accessibility label is: they reach a reply the model
/// reads, and a title with a newline in it could otherwise forge a line of one.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowFacts {
    /// `CGWindowID` on macOS — the id a `windows` listing publishes, and the one
    /// `select_target` takes.
    pub id: u32,
    pub pid: i32,
    pub app: String,
    pub title: String,
    pub bounds: Bounds,
    /// The window server's own layer. Zero is "a window a person works in"; the
    /// Dock, the menu bar and floating panels live above it.
    pub layer: i64,
    /// Whether the window is on screen at all. A minimized window, and one on
    /// another desktop, are both listed and both off screen.
    pub on_screen: bool,
}

/// The window server, as this build reads it.
///
/// `Send + Sync` because the indicator's watch thread reads it four times a second
/// while the action worker reads it per request: two readers, one listing call,
/// no shared state of its own.
pub trait Windows: Send + Sync {
    /// Every window, FRONT TO BACK among the ones on screen. `Err` when the OS
    /// refuses the listing, which is a different fact from an empty desktop and
    /// must never be flattened into one.
    fn list(&self) -> Result<Vec<WindowFacts>, String>;
}

/// The window a given id names, out of one listing.
pub fn find(listed: &[WindowFacts], id: u32) -> Option<&WindowFacts> {
    listed.iter().find(|window| window.id == id)
}

/// What covers the target at this point, if anything.
///
/// Front to back, so everything BEFORE the target in the list is in front of it.
/// A window that contains the point and sits in front of the target is what a
/// click at that point would really reach, and naming it is the whole value of the
/// answer: "something is in front" sends the caller looking, "Slack — Messages is
/// in front" tells it what to do.
///
/// Every layer counts. The Dock and the menu bar are not the target's window and a
/// click under them lands on them, so pretending they are transparent would be this
/// build deciding that some obstructions do not matter.
///
/// A target that is not in the listing is [`TargetMissing`] and never `None`: a
/// walk that never meets the target would otherwise run to the end of the desktop
/// and answer with the frontmost window over that point, which is the opposite of
/// the truth — "the window you bound is gone" reported as "nothing is in the way".
pub fn obstruction_at(
    listed: &[WindowFacts],
    target_id: u32,
    x: f64,
    y: f64,
) -> Result<Option<&WindowFacts>, TargetMissing> {
    covering(listed, target_id, &|window: &WindowFacts| {
        window.bounds.contains(x, y)
    })
}

/// What covers the target inside this rectangle, ignoring one process's own
/// windows.
///
/// The badge is the reason for the exclusion: it sits over the target's corner by
/// design, and a hit test that counted it would report the target as covered by the
/// very thing that says it is being worked in.
pub fn obstruction_in<'a>(
    listed: &'a [WindowFacts],
    target_id: u32,
    area: &Bounds,
    ignoring: Option<i32>,
) -> Result<Option<&'a WindowFacts>, TargetMissing> {
    covering(listed, target_id, &|window: &WindowFacts| {
        Some(window.pid) != ignoring && window.bounds.intersects(area)
    })
}

/// The frontmost on-screen window ahead of the target that the test accepts.
///
/// Front to back, so everything BEFORE the target in the list is in front of it —
/// and the target has to BE in the list for that sentence to mean anything.
fn covering<'a>(
    listed: &'a [WindowFacts],
    target_id: u32,
    hit: &dyn Fn(&WindowFacts) -> bool,
) -> Result<Option<&'a WindowFacts>, TargetMissing> {
    let on_screen = || listed.iter().filter(|window| window.on_screen);

    if !on_screen().any(|window| window.id == target_id) {
        return Err(TargetMissing);
    }

    Ok(on_screen()
        .take_while(|window| window.id != target_id)
        .find(|window| hit(window)))
}

/// The same application's other on-screen windows — a sheet, a dialog, a panel it
/// opened — that were not there when the target was selected.
///
/// Reported, never guessed at: the helper cannot know that an action "needed" the
/// sheet it just opened, so it says which windows appeared and the caller selects
/// one explicitly if that is what it meant.
pub fn children_of<'a>(
    listed: &'a [WindowFacts],
    pid: i32,
    target_id: u32,
    at_selection: &[u32],
) -> Vec<&'a WindowFacts> {
    listed
        .iter()
        .filter(|window| window.on_screen)
        .filter(|window| window.pid == pid)
        .filter(|window| window.id != target_id)
        .filter(|window| !at_selection.contains(&window.id))
        .collect()
}

/// The ids of one application's on-screen windows right now, which is what a later
/// listing's new windows are measured against.
pub fn ids_of(listed: &[WindowFacts], pid: i32) -> Vec<u32> {
    listed
        .iter()
        .filter(|window| window.on_screen && window.pid == pid)
        .map(|window| window.id)
        .collect()
}

/// Is this application's window the front one? The frontmost on-screen window at
/// the ordinary window layer, because the Dock and the menu bar are always above
/// everything and are nobody's idea of "what the person is working in".
pub fn front_window(listed: &[WindowFacts]) -> Option<&WindowFacts> {
    listed
        .iter()
        .find(|window| window.on_screen && window.layer == 0)
}

// --- macOS --------------------------------------------------------------------

/// The real window server.
///
/// One `CGWindowListCopyWindowInfo` per call, with desktop elements excluded and
/// both on-screen and off-screen windows included: a minimized window has to be
/// LISTED for `target_minimized` to be tellable from `target_unavailable`, and it
/// is absent from an on-screen-only listing.
#[cfg(target_os = "macos")]
pub mod mac {
    use super::{Bounds, WindowFacts, Windows};
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::string::CFString;
    use std::ffi::c_void;

    /// `kCGWindowListExcludeDesktopElements`. Deliberately NOT paired with
    /// `kCGWindowListOptionOnScreenOnly`: the off-screen windows are exactly the
    /// ones a minimized target is among.
    const EXCLUDE_DESKTOP: u32 = 1 << 4;
    /// `kCFNumberSInt64Type` and `kCFNumberDoubleType`.
    const SINT64: isize = 4;
    const DOUBLE: isize = 13;

    /// A window title or application name is another process's text reaching a
    /// reply the model reads, bounded and flattened exactly as a control's label
    /// is.
    const MAX_TITLE_CHARS: usize = 80;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFTypeRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: CFTypeRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
        fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        fn CFNumberGetValue(number: CFTypeRef, the_type: isize, out: *mut c_void) -> bool;
        fn CFBooleanGetValue(boolean: CFTypeRef) -> bool;
    }

    #[derive(Default)]
    pub struct Real;

    impl Real {
        pub fn new() -> Real {
            Real
        }
    }

    impl Windows for Real {
        fn list(&self) -> Result<Vec<WindowFacts>, String> {
            unsafe { copy_window_info() }
        }
    }

    unsafe fn copy_window_info() -> Result<Vec<WindowFacts>, String> {
        let list_ref = CGWindowListCopyWindowInfo(EXCLUDE_DESKTOP, 0);
        if list_ref.is_null() {
            return Err("the window server refused the window listing".to_string());
        }
        let list = CFType::wrap_under_create_rule(list_ref);

        let keys = Keys::new();
        let mut windows = Vec::new();
        for index in 0..CFArrayGetCount(list.as_CFTypeRef()) {
            let dict = CFArrayGetValueAtIndex(list.as_CFTypeRef(), index);
            if dict.is_null() {
                continue;
            }
            if let Some(window) = read_window(dict, &keys) {
                windows.push(window);
            }
        }
        Ok(windows)
    }

    /// The dictionary keys, built once per listing rather than per window: a
    /// hundred windows would otherwise allocate a thousand CFStrings that all say
    /// the same ten things.
    struct Keys {
        number: CFString,
        pid: CFString,
        owner: CFString,
        name: CFString,
        bounds: CFString,
        layer: CFString,
        on_screen: CFString,
    }

    impl Keys {
        fn new() -> Keys {
            Keys {
                number: CFString::new("kCGWindowNumber"),
                pid: CFString::new("kCGWindowOwnerPID"),
                owner: CFString::new("kCGWindowOwnerName"),
                name: CFString::new("kCGWindowName"),
                bounds: CFString::new("kCGWindowBounds"),
                layer: CFString::new("kCGWindowLayer"),
                on_screen: CFString::new("kCGWindowIsOnscreen"),
            }
        }
    }

    /// One window, or `None` when the dictionary does not carry the two facts a
    /// window is useless without: which window it is, and where it is. A window
    /// with neither cannot be bound, cannot be hit-tested and must not be listed.
    unsafe fn read_window(dict: CFTypeRef, keys: &Keys) -> Option<WindowFacts> {
        let id = read_i64(dict, &keys.number)?;
        let bounds = read_bounds(dict, &keys.bounds)?;

        Some(WindowFacts {
            id: id as u32,
            pid: read_i64(dict, &keys.pid).unwrap_or(0) as i32,
            app: read_text(dict, &keys.owner),
            title: read_text(dict, &keys.name),
            bounds,
            layer: read_i64(dict, &keys.layer).unwrap_or(0),
            on_screen: read_bool(dict, &keys.on_screen),
        })
    }

    // CFDictionaryGetValue returns a borrowed (Get-rule) ref — no release here.
    unsafe fn read_i64(dict: CFTypeRef, key: &CFString) -> Option<i64> {
        let value = CFDictionaryGetValue(dict, key.as_CFTypeRef());
        if value.is_null() {
            return None;
        }
        let mut out: i64 = 0;
        if CFNumberGetValue(value, SINT64, &mut out as *mut _ as *mut c_void) {
            Some(out)
        } else {
            None
        }
    }

    unsafe fn read_f64(dict: CFTypeRef, key: &CFString) -> Option<f64> {
        let value = CFDictionaryGetValue(dict, key.as_CFTypeRef());
        if value.is_null() {
            return None;
        }
        let mut out: f64 = 0.0;
        if CFNumberGetValue(value, DOUBLE, &mut out as *mut _ as *mut c_void) {
            Some(out)
        } else {
            None
        }
    }

    unsafe fn read_bool(dict: CFTypeRef, key: &CFString) -> bool {
        let value = CFDictionaryGetValue(dict, key.as_CFTypeRef());
        !value.is_null() && CFBooleanGetValue(value)
    }

    /// Absent is an empty string, never a failure: a window with no title is
    /// ordinary (an untitled document, a palette), and refusing to list it would
    /// hide a window the caller can plainly see.
    unsafe fn read_text(dict: CFTypeRef, key: &CFString) -> String {
        let value = CFDictionaryGetValue(dict, key.as_CFTypeRef());
        if value.is_null() {
            return String::new();
        }
        match CFType::wrap_under_get_rule(value).downcast::<CFString>() {
            Some(text) => crate::ax::one_line(&text.to_string(), MAX_TITLE_CHARS),
            None => String::new(),
        }
    }

    /// `kCGWindowBounds` is a CGRect serialised as a dictionary of four numbers,
    /// in global logical points with the origin at the top left.
    unsafe fn read_bounds(dict: CFTypeRef, key: &CFString) -> Option<Bounds> {
        let value = CFDictionaryGetValue(dict, key.as_CFTypeRef());
        if value.is_null() {
            return None;
        }
        let rect = CFType::wrap_under_get_rule(value);
        let (x, y) = (
            read_f64(rect.as_CFTypeRef(), &CFString::new("X"))?,
            read_f64(rect.as_CFTypeRef(), &CFString::new("Y"))?,
        );
        let (w, h) = (
            read_f64(rect.as_CFTypeRef(), &CFString::new("Width"))?,
            read_f64(rect.as_CFTypeRef(), &CFString::new("Height"))?,
        );

        if w > 0.0 && h > 0.0 {
            Some(Bounds { x, y, w, h })
        } else {
            None
        }
    }
}

#[cfg(target_os = "macos")]
pub use mac::Real;

// --- everywhere else ----------------------------------------------------------

/// Linux has no window server this build reads, and saying so is not the same as
/// answering an empty desktop: a caller that cannot tell them apart writes the
/// wrong sentence about both.
#[cfg(not(target_os = "macos"))]
mod stub {
    use super::{WindowFacts, Windows};

    #[derive(Default)]
    pub struct Real;

    impl Real {
        pub fn new() -> Real {
            Real
        }
    }

    impl Windows for Real {
        fn list(&self) -> Result<Vec<WindowFacts>, String> {
            Err("window targeting is only supported on macOS".to_string())
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub use stub::Real;

// --- the recording window server (tests only) ---------------------------------

/// A window server a test writes down, front to back. Every rule above is proved
/// against this, so none of them needs a desktop.
#[cfg(test)]
pub struct Recorder {
    listed: std::sync::Mutex<Result<Vec<WindowFacts>, String>>,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl Recorder {
    /// Front to back: the first entry is the window in front.
    pub fn new(listed: Vec<WindowFacts>) -> std::sync::Arc<Recorder> {
        std::sync::Arc::new(Recorder {
            listed: std::sync::Mutex::new(Ok(listed)),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// The OS refuses the listing from now on.
    pub fn refuses(&self, reason: &str) {
        *self.state() = Err(reason.to_string());
    }

    /// Change one window in place — move it, minimize it, put it behind another.
    pub fn change(&self, id: u32, edit: impl FnOnce(&mut WindowFacts)) {
        if let Ok(listed) = self.state().as_mut() {
            if let Some(window) = listed.iter_mut().find(|window| window.id == id) {
                edit(window);
            }
        }
    }

    /// The window is gone from the server altogether — closed, or its application
    /// quit.
    pub fn remove(&self, id: u32) {
        if let Ok(listed) = self.state().as_mut() {
            listed.retain(|window| window.id != id);
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Result<Vec<WindowFacts>, String>> {
        self.listed.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
impl Windows for Recorder {
    fn list(&self) -> Result<Vec<WindowFacts>, String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.state().clone()
    }
}

/// One ordinary window, for a test that cares about two of its facts and not the
/// other five.
#[cfg(test)]
pub fn window(id: u32, pid: i32, bounds: Bounds) -> WindowFacts {
    WindowFacts {
        id,
        pid,
        app: format!("App{pid}"),
        title: format!("Window {id}"),
        bounds,
        layer: 0,
        on_screen: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f64, y: f64, w: f64, h: f64) -> Bounds {
        Bounds { x, y, w, h }
    }

    /// A desktop, front to back: a small panel in front, the target under it, and
    /// a big background window under both.
    fn desktop() -> Vec<WindowFacts> {
        vec![
            window(10, 100, at(200.0, 200.0, 100.0, 60.0)),
            window(20, 200, at(0.0, 0.0, 800.0, 600.0)),
            window(30, 300, at(0.0, 0.0, 1920.0, 1080.0)),
        ]
    }

    #[test]
    fn a_point_the_target_owns_is_not_obstructed() {
        let listed = desktop();
        assert_eq!(obstruction_at(&listed, 20, 400.0, 400.0), Ok(None));
    }

    // The whole rule, at the point that proves it: a window IN FRONT of the target
    // covering the place the click would land. Nothing raises it and nothing
    // clicks through it — the caller is told what is there.
    #[test]
    fn a_window_in_front_at_that_point_obstructs_and_is_named() {
        let listed = desktop();
        let covering = obstruction_at(&listed, 20, 250.0, 230.0)
            .expect("the target is listed")
            .expect("the panel is in front");
        assert_eq!(covering.id, 10);
    }

    // Behind is not in front. The big background window contains every point the
    // target does, and it obstructs none of them.
    #[test]
    fn a_window_behind_the_target_never_obstructs() {
        let listed = desktop();
        assert_eq!(obstruction_at(&listed, 20, 10.0, 10.0), Ok(None));
        assert_eq!(obstruction_at(&listed, 20, 799.0, 599.0), Ok(None));
    }

    // A target that is not in the listing is a target that is GONE, and a hit test
    // that walked past the end of the desktop would answer with whatever is in
    // front of the place it used to be.
    #[test]
    fn a_target_that_is_not_listed_is_missing_and_never_clear() {
        let mut listed = desktop();
        listed.retain(|window| window.id != 20);

        assert_eq!(
            obstruction_at(&listed, 20, 400.0, 400.0),
            Err(TargetMissing)
        );
        // The point the panel covers, which a walk to the end would have reported
        // as an ordinary obstruction rather than as a window that is gone.
        assert_eq!(
            obstruction_at(&listed, 20, 250.0, 230.0),
            Err(TargetMissing)
        );

        // Off screen is the same fact: nothing can be in front of a window nobody
        // can see.
        let mut minimized = desktop();
        minimized[1].on_screen = false;
        assert_eq!(
            obstruction_at(&minimized, 20, 400.0, 400.0),
            Err(TargetMissing)
        );
    }

    // A window that is not on screen is not in front of anything, whatever the
    // list order says: a minimized panel cannot catch a click.
    #[test]
    fn an_off_screen_window_is_not_an_obstruction() {
        let mut listed = desktop();
        listed[0].on_screen = false;
        assert_eq!(obstruction_at(&listed, 20, 250.0, 230.0), Ok(None));
    }

    // The Dock and the menu bar are in front and a click under them lands on them,
    // so they obstruct like anything else. This is the case that would be wrong if
    // the hit test filtered by layer.
    #[test]
    fn a_shell_window_in_front_obstructs_like_any_other() {
        let mut listed = desktop();
        listed[0].layer = 20;
        let covering = obstruction_at(&listed, 20, 250.0, 230.0)
            .expect("the target is listed")
            .expect("the Dock is in front");
        assert_eq!(covering.layer, 20);
    }

    // The badge's area, not one corner point — and the badge's own windows are not
    // an obstruction of the window the badge is about.
    #[test]
    fn an_area_hit_test_ignores_the_watcher_own_windows() {
        let mut listed = desktop();
        // The badge itself, over the target's top left, owned by pid 999.
        listed.insert(0, window(90, 999, at(0.0, -34.0, 180.0, 28.0)));

        let area = at(0.0, -34.0, 180.0, 114.0);
        assert_eq!(obstruction_in(&listed, 20, &area, Some(999)), Ok(None));

        // Without the exclusion the badge reports the window it sits on as covered.
        assert_eq!(
            obstruction_in(&listed, 20, &area, None)
                .expect("listed")
                .map(|window| window.id),
            Some(90)
        );

        // Somebody ELSE's window over the same place is an obstruction either way.
        listed.insert(0, window(11, 400, at(100.0, 0.0, 300.0, 200.0)));
        assert_eq!(
            obstruction_in(&listed, 20, &area, Some(999))
                .expect("listed")
                .map(|window| window.id),
            Some(11)
        );
    }

    // A window BEHIND the target does not cover the badge, however much of the
    // badge's area it contains.
    #[test]
    fn a_window_behind_the_target_never_covers_the_badge_area() {
        let listed = desktop();
        let area = at(0.0, -34.0, 180.0, 114.0);

        assert_eq!(obstruction_in(&listed, 20, &area, None), Ok(None));
    }

    #[test]
    fn two_rectangles_overlap_only_where_they_really_do() {
        let bounds = at(10.0, 20.0, 100.0, 50.0);

        assert!(bounds.intersects(&at(100.0, 60.0, 40.0, 40.0)));
        assert!(bounds.intersects(&at(0.0, 0.0, 1000.0, 1000.0)));
        // Edge to edge is not overlapping, on the same half-open rule `contains` has.
        assert!(!bounds.intersects(&at(110.0, 20.0, 10.0, 10.0)));
        assert!(!bounds.intersects(&at(10.0, 70.0, 10.0, 10.0)));
    }

    // Half-open, so two windows edge to edge never both own the same point.
    #[test]
    fn a_rectangle_owns_its_left_and_top_edges_and_not_the_others() {
        let bounds = at(10.0, 20.0, 100.0, 50.0);
        assert!(bounds.contains(10.0, 20.0));
        assert!(bounds.contains(109.999, 69.999));
        assert!(!bounds.contains(110.0, 40.0));
        assert!(!bounds.contains(40.0, 70.0));
        assert!(!bounds.contains(9.999, 40.0));
    }

    #[test]
    fn a_frame_matches_within_the_tolerance_and_not_past_it() {
        let bounds = at(10.0, 20.0, 100.0, 50.0);
        assert!(bounds.matches(&at(11.5, 21.0, 99.0, 51.0), 2.0));
        assert!(!bounds.matches(&at(13.0, 20.0, 100.0, 50.0), 2.0));
        assert!(!bounds.matches(&at(10.0, 20.0, 104.0, 50.0), 2.0));
    }

    // A window the same application opened SINCE the target was selected. The ones
    // that were already there are not news, and the target is not its own child.
    #[test]
    fn a_child_is_a_window_of_the_same_application_that_appeared_since() {
        let mut listed = desktop();
        listed.insert(0, window(11, 200, at(300.0, 300.0, 200.0, 100.0)));

        let at_selection = ids_of(&desktop(), 200);
        let children = children_of(&listed, 200, 20, &at_selection);

        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, 11);

        // Another application's new window is not this target's child.
        assert!(children_of(&listed, 200, 20, &ids_of(&listed, 200)).is_empty());
    }

    #[test]
    fn the_front_window_is_the_frontmost_ordinary_one() {
        let mut listed = desktop();
        listed.insert(
            0,
            WindowFacts {
                layer: 25,
                ..window(99, 400, at(0.0, 0.0, 1920.0, 24.0))
            },
        );

        let front = front_window(&listed).expect("something is in front");
        assert_eq!(
            front.id, 10,
            "the menu bar is not what anyone is working in"
        );
    }

    #[test]
    fn a_refusal_is_not_an_empty_desktop() {
        let recorder = Recorder::new(desktop());
        assert_eq!(recorder.list().map(|listed| listed.len()), Ok(3));

        recorder.refuses("the window server refused the window listing");
        assert!(recorder.list().is_err());
        assert_eq!(recorder.calls(), 2);
    }

    #[test]
    fn a_window_is_found_by_its_own_id_and_nothing_else() {
        let listed = desktop();
        assert_eq!(find(&listed, 20).map(|window| window.pid), Some(200));
        assert_eq!(find(&listed, 21), None);
    }
}
