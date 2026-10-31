//! The tree walk, above the accessibility seam (M86 P1).
//!
//! Until P1 the whole traversal lived inside the macOS reader, so none of its
//! rules ran under test and a second platform would have had to repeat it. Here it
//! asks [`Ax`] per-node questions — one node's facts, how many children it has, one
//! page of them, and for a node it lists, its value and whether that may be
//! written — so the budgets, which nodes are listed, the paging and the ancestor
//! paths are unit-tested against scripted trees on any machine, and a second
//! platform answers the same questions rather than walking its own way.
//!
//! Depth first, in the order the application gives its children, a parent before
//! its children: the order a listing numbers its references in.
//!
//! A node that answers nothing is one of three things ([`Unread`]), and only one of
//! them is silent: a node that is gone takes its subtree with it. A node that did
//! not answer in time is skipped and its children still tried, and the listing
//! says it is incomplete — a partial tree passed off as the whole one is how a
//! caller concludes a control is not there.

use std::rc::Rc;

use crate::ax::{Ax, Facts, Frame, Handle, Retained, Unread, PRESS};
use crate::gate::Clock;

/// Wall-clock budget for ONE traversal, checked between nodes.
///
/// The node, visit and depth caps bound the SHAPE of a tree and say nothing about
/// how slowly an application answers: a wedged app with twelve nodes can hold the
/// walk for as long as the messaging timeout allows, once per node. This is the
/// bound on the walk itself, and a reply that hit it says `truncated: "time"`
/// rather than passing off a partial tree as the whole one.
pub const WALK_BUDGET_MS: u64 = 1_500;

/// Depth of the tree walk, elements COLLECTED, and nodes TRAVERSED. A large
/// sparse subtree has few interactive nodes and many to walk, which is why
/// the last two are separate numbers.
const MAX_DEPTH: usize = 14;
const MAX_NODES: usize = 250;
const MAX_VISITED: usize = 3000;

/// How many of one parent's children are read. A table with thousands of rows
/// would otherwise spend the whole visit budget before the walk reached the
/// toolbar beside it; a parent with more says the walk did not see everything.
const MAX_CHILDREN: usize = 250;

/// Children are read a page at a time (M3), never as one copy of the whole list.
const CHILD_PAGE: usize = 128;

/// How much of an element's ancestry is worth carrying: enough to tell two
/// "Save" buttons apart, not enough to be a second tree.
const MAX_PATH_LABELS: usize = 3;
const MAX_PATH_CHARS: usize = 60;

/// The three reasons a walk stops early, in the words the wire uses.
const TRUNCATED_NODES: &str = "nodes";
const TRUNCATED_DEPTH: &str = "depth";
const TRUNCATED_TIME: &str = "time";

/// Roles worth surfacing as targets. A password field is an `AXTextField`
/// here and is told apart by its subrole, so it is listed like any other
/// field — a control the caller cannot see at all is one it invents a reason
/// for missing — and its value is withheld instead.
const INTERACTIVE: &[&str] = &[
    "AXButton",
    "AXMenuItem",
    "AXMenuButton",
    "AXPopUpButton",
    "AXCheckBox",
    "AXRadioButton",
    "AXTextField",
    "AXTextArea",
    "AXComboBox",
    "AXLink",
    "AXTabButton",
    "AXSlider",
    "AXDisclosureTriangle",
    "AXCell",
];

/// The actions that make a node a control whatever its role (M4): it does
/// something when operated. Not `AXShowMenu`, `AXScrollToVisible`, `AXRaise` or
/// a focus-only action — every container answers those, and listing on them would
/// list the whole tree.
const PRIMARY_ACTIONS: &[&str] = &[
    "AXPress",
    "AXConfirm",
    "AXPick",
    "AXIncrement",
    "AXDecrement",
    "AXOpen",
];

/// Whether a node gets a reference: its role is one a person operates, OR its own
/// action list advertises a primary action (M4) — which picks up the SwiftUI and
/// web groups and rows that act like buttons and are named like containers.
///
/// A node with no role is never listed: a reference is revalidated by its role
/// before every action, and there would be nothing to compare.
pub fn eligible(role: Option<&str>, actions: &[String]) -> bool {
    let Some(role) = role else {
        return false;
    };
    INTERACTIVE.contains(&role)
        || actions
            .iter()
            .any(|action| PRIMARY_ACTIONS.contains(&action.as_str()))
}

/// An interactive element, as one walk found it. Everything the wire publishes
/// about a control is read once, here, so a reply is one traversal rather than a
/// second round of queries per element.
#[derive(Debug)]
pub struct Node {
    pub role: String,
    /// Title, else description. NOT the value: a field's contents are its own
    /// field, and using them as a name made a text box look like its text.
    pub label: Option<String>,
    /// Bounded, and absent for a secure field.
    pub value: Option<String>,
    pub secure: bool,
    pub enabled: bool,
    /// Its OWN action list contains `AXPress`. Never inferred from the role: a
    /// control that looks pressable and is not would be a refusal the caller was
    /// invited to expect.
    pub press: bool,
    /// The platform says its value may be written.
    pub settable: bool,
    pub frame: Frame,
    /// Up to three ancestor labels, nearest last.
    pub path: Vec<String>,
    pub element: Retained,
}

/// What one traversal answered.
#[derive(Debug)]
pub struct Walk {
    pub nodes: Vec<Node>,
    /// Why it stopped before it had seen everything, as the wire spells it:
    /// `"nodes"` (it collected or traversed as many as it may, or a parent had
    /// more children than it reads), `"depth"` (a subtree was deeper than the walk
    /// goes) or `"time"` (it ran out of its wall-clock budget, or a node did not
    /// answer within its own timeout). `None` means it saw the whole tree.
    pub truncated: Option<&'static str>,
}

/// Walk ONE application's tree from its own application element, held for as long
/// as the walk reads through it.
pub fn walk(ax: &Rc<dyn Ax>, pid: i32, clock: &dyn Clock, deadline_ms: u64) -> Walk {
    let Some(root) = ax.application(ax, pid) else {
        return Walk {
            nodes: Vec::new(),
            truncated: None,
        };
    };
    walk_from(ax, root.handle(), clock, deadline_ms)
}

/// The same walk, from a root this process is already holding — the window a
/// target is bound to. Same collection, same bounds, same truncation reasons; only
/// where it starts differs, which is what keeps a bound window's controls the
/// window's rather than the whole application's.
pub fn walk_from(ax: &Rc<dyn Ax>, root: Handle, clock: &dyn Clock, deadline_ms: u64) -> Walk {
    let mut walker = Walker {
        ax,
        clock,
        deadline_ms,
        visited: 0,
        truncated: None,
        nodes: Vec::new(),
        ancestors: Vec::new(),
    };
    walker.root(root);

    Walk {
        nodes: walker.nodes,
        truncated: walker.truncated,
    }
}

/// One traversal in progress.
struct Walker<'a> {
    ax: &'a Rc<dyn Ax>,
    clock: &'a dyn Clock,
    deadline_ms: u64,
    visited: usize,
    truncated: Option<&'static str>,
    nodes: Vec<Node>,
    /// The labels of the nodes above the one being visited, outermost first.
    ancestors: Vec<String>,
}

impl Walker<'_> {
    /// The application or window the walk starts from. Never listed — what a
    /// listing names is inside it — but its label is the outermost ancestor of
    /// everything that is.
    fn root(&mut self, root: Handle) {
        if self.stop(0) {
            return;
        }
        self.visited += 1;

        match self.ax.facts(root) {
            Ok(facts) => self.descend(root, facts.label, 0),
            Err(why) => self.unanswered(why, root, 0),
        }
    }

    fn visit(&mut self, element: Retained, depth: usize) {
        if self.stop(depth) {
            return;
        }
        self.visited += 1;

        let handle = element.handle();
        // Listed or not, the element stays held until its children have been read:
        // they are read through it. That holds on every path out of here.
        let facts = match self.ax.facts(handle) {
            Ok(facts) => facts,
            Err(why) => return self.unanswered(why, handle, depth),
        };
        let label = facts.label.clone();
        let _unlisted = self.collect(facts, element);
        self.descend(handle, label, depth);
    }

    /// A node whose facts answered nothing. It is not listed — there is nothing to
    /// list it by — and what happens below it depends on why.
    fn unanswered(&mut self, why: Unread, handle: Handle, depth: usize) {
        match why {
            // It names nothing any more, and nothing inside it can be read either.
            Unread::Gone => return,
            // Still there and slow: what is inside it may answer, and the listing
            // says it did not see everything.
            Unread::NoAnswer => self.note(TRUNCATED_TIME),
            // It answered and would not say: what is inside it may.
            Unread::Refused => {}
        }
        self.descend(handle, None, depth);
    }

    /// Read a node's children with its own label as their nearest ancestor.
    fn descend(&mut self, parent: Handle, label: Option<String>, depth: usize) {
        let named = label.is_some();
        if let Some(label) = label {
            self.ancestors.push(label);
        }
        self.children(parent, depth + 1);
        if named {
            self.ancestors.pop();
        }
    }

    /// One parent's children, at `depth`: by count and in pages, or in one copy
    /// when it will not count them.
    fn children(&mut self, parent: Handle, depth: usize) {
        match self.ax.child_count(parent) {
            Ok(count) => self.pages(parent, count, depth),
            // Its children may be there; the listing says it did not see them.
            Err(Unread::NoAnswer) => self.note(TRUNCATED_TIME),
            // Gone since its own read: there is nothing to read.
            Err(Unread::Gone) => {}
            // It will not count them. The count is only how a long list is read in
            // pages, so the list is copied once instead.
            Err(Unread::Refused) => self.uncounted(parent, depth, 0),
        }
    }

    /// `count` children a page at a time — each page visited before the next is
    /// read, so a walk that stops part way never reads a page it will not use. At
    /// most [`MAX_CHILDREN`] of them; a parent with more is a tree the walk did not
    /// see all of, and it says so. A page that fails is read exactly as a count
    /// that fails is: it is not the end of the list.
    fn pages(&mut self, parent: Handle, count: usize, depth: usize) {
        let wanted = count.min(MAX_CHILDREN);

        let mut read = 0;
        while read < wanted && !self.stop(depth) {
            let asked = CHILD_PAGE.min(wanted - read);
            let page = match self.ax.children(self.ax, parent, read, asked) {
                Ok(page) => page,
                // The rest may be there; the listing says it did not see them.
                Err(Unread::NoAnswer) => return self.note(TRUNCATED_TIME),
                // Gone since its count: there is nothing further to read.
                Err(Unread::Gone) => return,
                // It will not page them: the rest come from one copy instead.
                Err(Unread::Refused) => return self.uncounted(parent, depth, read),
            };
            // The application listed fewer than it counted: there is nothing
            // further to read, and asking again would ask the same question.
            if page.is_empty() {
                return;
            }
            read += page.len();
            for child in page {
                self.visit(child, depth);
            }
        }

        if read >= wanted && count > wanted {
            self.note(TRUNCATED_NODES);
        }
    }

    /// The children of a parent that will not count or page them, in ONE copy of
    /// the list: the first [`MAX_CHILDREN`] are visited, and a parent with more says
    /// so, exactly as a counted one does. One more than that is asked for, which is
    /// how "more" is known without a count. The first `visited` were already read
    /// a page at a time, and are not listed twice.
    fn uncounted(&mut self, parent: Handle, depth: usize, visited: usize) {
        let mut children = match self
            .ax
            .children_uncounted(self.ax, parent, MAX_CHILDREN + 1)
        {
            Ok(children) => children,
            Err(Unread::NoAnswer) => return self.note(TRUNCATED_TIME),
            // Gone, or this refused too: there is no list to read.
            Err(Unread::Gone | Unread::Refused) => return,
        };
        let more = children.len() > MAX_CHILDREN;
        // The one past the cap is released here, unvisited.
        children.truncate(MAX_CHILDREN);

        // The ones already visited are released here, unvisited again.
        for child in children.into_iter().skip(visited) {
            self.visit(child, depth);
        }
        if more {
            self.note(TRUNCATED_NODES);
        }
    }

    /// Every bound the walk has, in one place. Time is checked first.
    fn stop(&mut self, depth: usize) -> bool {
        if self.clock.now_ms() >= self.deadline_ms {
            self.note(TRUNCATED_TIME);
            return true;
        }
        if self.nodes.len() >= MAX_NODES || self.visited >= MAX_VISITED {
            self.note(TRUNCATED_NODES);
            return true;
        }
        if depth > MAX_DEPTH {
            self.note(TRUNCATED_DEPTH);
            return true;
        }
        false
    }

    /// Why the walk did not see everything: the first reason it met — except time,
    /// which wins whenever it is met. A walk that ran out of time, or met a node
    /// that did not answer in time, would very likely have met the other bounds
    /// too, and "the application was slow" is the useful answer.
    fn note(&mut self, reason: &'static str) {
        if reason == TRUNCATED_TIME || self.truncated.is_none() {
            self.truncated = Some(reason);
        }
    }

    /// List this node if it is a control the wire can name: eligible, and saying
    /// where it is, because a listing gives every control a click point. Only then
    /// is it asked whether its value may be written and — unless it is secure —
    /// what the value is: a message each, and the walk passes many more nodes than
    /// it lists. Otherwise the element comes back, so it stays held while its
    /// children are read through it.
    fn collect(&mut self, facts: Facts, element: Retained) -> Option<Retained> {
        if !eligible(facts.role.as_deref(), &facts.actions) {
            return Some(element);
        }
        let (Some(role), Some(frame)) = (facts.role, facts.frame) else {
            return Some(element);
        };
        let handle = element.handle();

        self.nodes.push(Node {
            press: facts.actions.iter().any(|action| action == PRESS),
            // A secure field's value is never asked for: not asking is a stronger
            // guarantee than asking and throwing the answer away.
            value: if facts.secure {
                None
            } else {
                self.ax.value(handle)
            },
            settable: self.ax.settable(handle),
            secure: facts.secure,
            enabled: facts.enabled,
            label: facts.label,
            path: self.path(),
            role,
            frame,
            element,
        });
        None
    }

    /// The ancestor labels worth carrying: the nearest three, nearest LAST,
    /// trimmed to a total that stays readable beside the control itself.
    fn path(&self) -> Vec<String> {
        let nearest: Vec<&String> = self
            .ancestors
            .iter()
            .rev()
            .take(MAX_PATH_LABELS)
            .rev()
            .collect();

        let mut total = 0;
        let mut kept = Vec::new();
        // Nearest last, so a path that has to be cut loses the OUTERMOST label
        // first: "Toolbar" tells two Save buttons apart where the window's own
        // name does not.
        for label in nearest.into_iter().rev() {
            total += label.chars().count();
            if total > MAX_PATH_CHARS {
                break;
            }
            kept.push(label.clone());
        }
        kept.reverse();
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::{Recorder, Scripted, SECURE_TEXT_FIELD};
    use crate::gate::SystemClock;
    use std::sync::{Arc, Mutex};

    const PID: i32 = 4711;

    /// A clock a test moves by hand, so a wall-clock budget is asserted rather
    /// than waited for.
    struct TestClock {
        ms: Mutex<u64>,
    }

    impl TestClock {
        fn new() -> Arc<TestClock> {
            Arc::new(TestClock { ms: Mutex::new(0) })
        }
    }

    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            *self.ms.lock().unwrap()
        }

        fn now_ns(&self) -> u128 {
            self.now_ms() as u128 * 1_000_000
        }

        fn now_mach(&self) -> u64 {
            self.now_ms() * 1_000_000
        }

        fn sleep(&self, ms: u64) {
            *self.ms.lock().unwrap() += ms;
        }
    }

    fn frame(x: f64, y: f64) -> Frame {
        Frame {
            x,
            y,
            w: 80.0,
            h: 24.0,
        }
    }

    fn platform(elements: Vec<Scripted>) -> (Rc<Recorder>, Rc<dyn Ax>) {
        let recorder = Recorder::new(PID, elements);
        let ax: Rc<dyn Ax> = recorder.clone();
        (recorder, ax)
    }

    /// The whole tree, with no clock to run out of.
    fn whole(ax: &Rc<dyn Ax>) -> Walk {
        walk(ax, PID, &SystemClock::new(), u64::MAX)
    }

    /// The listed controls' labels, in the order the listing numbers them.
    fn labels(found: &Walk) -> Vec<&str> {
        found
            .nodes
            .iter()
            .map(|node| node.label.as_deref().unwrap_or("-"))
            .collect()
    }

    /// A node of this role with these actions, somewhere on screen.
    fn acting(role: &str, label: &str, actions: &[&str]) -> Scripted {
        Scripted {
            label: Some(label.to_string()),
            actions: actions.iter().map(|action| action.to_string()).collect(),
            frame: Some(frame(0.0, 0.0)),
            ..Scripted::of(role)
        }
    }

    /// `count` static texts, the nodes a walk passes on its way down and never lists.
    fn texts(count: usize) -> Vec<Scripted> {
        (0..count)
            .map(|index| Scripted {
                label: Some(format!("text {index}")),
                frame: Some(frame(0.0, 0.0)),
                ..Scripted::of("AXStaticText")
            })
            .collect()
    }

    // --- eligibility (M4) ------------------------------------------------------

    // A control is a role a person operates, or anything whose OWN action list
    // advertises a primary action — which is what picks up the SwiftUI and web
    // groups and rows that act like buttons.
    #[test]
    fn a_node_is_listed_for_its_role_or_for_a_primary_action_of_its_own() {
        let none: Vec<String> = Vec::new();
        let actions =
            |names: &[&str]| -> Vec<String> { names.iter().map(|name| name.to_string()).collect() };

        assert!(eligible(Some("AXButton"), &none));
        assert!(eligible(Some("AXTextField"), &none));
        assert!(eligible(Some("AXCell"), &none));
        assert!(!eligible(Some("AXGroup"), &none));
        assert!(!eligible(Some("AXStaticText"), &none));

        for primary in [
            "AXPress",
            "AXConfirm",
            "AXPick",
            "AXIncrement",
            "AXDecrement",
            "AXOpen",
        ] {
            assert!(
                eligible(Some("AXGroup"), &actions(&[primary])),
                "{primary} makes a control"
            );
        }
    }

    // Every container answers these, so listing on them would list the whole tree.
    #[test]
    fn menus_scrolling_raising_and_focus_alone_make_nothing_a_control() {
        let secondary = ["AXShowMenu", "AXScrollToVisible", "AXRaise", "AXFocus"];
        for action in secondary {
            assert!(
                !eligible(Some("AXGroup"), &[action.to_string()]),
                "{action} alone is not a primary action"
            );
        }
        assert!(!eligible(
            Some("AXRow"),
            &secondary.map(|action| action.to_string())
        ));
    }

    // A reference is revalidated by its role before every action, so a node with
    // none can never be listed: there would be nothing to compare.
    #[test]
    fn a_node_with_no_role_is_never_listed() {
        assert!(!eligible(None, &["AXPress".to_string()]));
    }

    // The walk applies the rule: a group that presses is listed, with `press`
    // only where its own list has AXPress; a group that only shows a menu is not.
    #[test]
    fn a_walk_lists_a_group_by_its_own_actions() {
        let (_recorder, ax) = platform(vec![
            acting("AXGroup", "Card", &["AXPress", "AXShowMenu"]),
            acting("AXRow", "Inbox", &["AXConfirm"]),
            acting("AXGroup", "Panel", &["AXShowMenu", "AXScrollToVisible"]),
            acting("AXButton", "Save", &[]),
        ]);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Card", "Inbox", "Save"]);
        assert!(found.nodes[0].press);
        assert!(!found.nodes[1].press, "AXConfirm lists it, and is no press");
        assert!(
            !found.nodes[2].press,
            "a role lists it, and its own list has no press"
        );
    }

    // A subrole is never a role, and a password field is listed like any other
    // field by the role it really answers.
    #[test]
    fn the_secure_spelling_is_not_an_interactive_role() {
        assert!(!eligible(Some(SECURE_TEXT_FIELD), &[]));
        assert!(eligible(Some("AXTextField"), &[]));
    }

    // --- order and paths -------------------------------------------------------

    // Depth first, a parent before its children, siblings in the application's
    // order: the order a listing numbers its references in.
    #[test]
    fn a_walk_lists_depth_first_with_a_parent_before_its_children() {
        let mut card = acting("AXGroup", "Card", &["AXPress"]);
        card.children = vec![1, 2];
        let (_recorder, ax) = platform(vec![
            card,
            Scripted::button("Open", frame(0.0, 0.0)),
            Scripted::button("Share", frame(0.0, 0.0)),
            Scripted::button("Close", frame(0.0, 0.0)),
        ]);

        assert_eq!(labels(&whole(&ax)), vec!["Card", "Open", "Share", "Close"]);
    }

    // The path exists to tell two "Save" buttons apart, so it carries the nearest
    // three labelled ancestors, nearest last; an unnamed container adds nothing.
    #[test]
    fn a_path_is_the_nearest_three_named_ancestors_nearest_last() {
        let (_recorder, ax) = platform(vec![
            Scripted::group("Window", &[1]),
            Scripted::group("Split", &[2]),
            Scripted {
                children: vec![3],
                ..Scripted::of("AXGroup")
            },
            Scripted::group("Sidebar", &[4]),
            Scripted::group("Toolbar", &[5]),
            Scripted::button("Save", frame(0.0, 0.0)),
            Scripted::button("Top", frame(0.0, 0.0)),
        ]);

        let found = whole(&ax);

        assert_eq!(found.nodes[0].path, vec!["Split", "Sidebar", "Toolbar"]);
        assert!(
            found.nodes[1].path.is_empty(),
            "a control with no named ancestry has no path"
        );
    }

    // When the labels are too long together, the OUTERMOST goes first: the nearest
    // one is what distinguishes.
    #[test]
    fn a_path_that_is_too_long_loses_its_outermost_labels() {
        let long = "x".repeat(50);
        let (_recorder, ax) = platform(vec![
            Scripted::group(&long, &[1]),
            Scripted::group(&long, &[2]),
            Scripted::group("Toolbar", &[3]),
            Scripted::button("Save", frame(0.0, 0.0)),
        ]);

        assert_eq!(whole(&ax).nodes[0].path, vec![long, "Toolbar".to_string()]);
    }

    // A walk sanitises at the source, so nothing above it has to remember to.
    #[test]
    fn a_walk_flattens_every_piece_of_text_it_collects() {
        let (_recorder, ax) = platform(vec![
            Scripted::group("Doc\nument", &[1]),
            Scripted::field("Full\nname", "first\nsecond", frame(0.0, 0.0)),
        ]);

        let found = whole(&ax);
        let node = &found.nodes[0];

        assert_eq!(node.label.as_deref(), Some("Full name"));
        assert_eq!(node.value.as_deref(), Some("first second"));
        assert_eq!(node.path, vec!["Doc ument".to_string()]);
    }

    // A control that will not say where it is has no click point, so it cannot be
    // listed — but what is inside it is still walked, under its name.
    #[test]
    fn a_control_with_no_frame_is_not_listed_and_its_children_still_are() {
        let mut card = acting("AXGroup", "Card", &["AXPress"]);
        card.frame = None;
        card.children = vec![1];
        let (_recorder, ax) = platform(vec![card, Scripted::button("Open", frame(0.0, 0.0))]);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Open"]);
        assert_eq!(found.nodes[0].path, vec!["Card"]);
    }

    // The privacy blocker, stated as the thing that was wrong: a password field
    // has the ORDINARY text-field role and is told apart by its SUBROLE. The walk
    // must list the control and withhold its value.
    #[test]
    fn a_password_field_is_listed_and_never_carries_its_value() {
        let (recorder, ax) = platform(vec![
            Scripted::field("Name", "ada", frame(0.0, 0.0)),
            Scripted::secure_field("Password", "hunter2", frame(0.0, 40.0)),
        ]);

        let found = whole(&ax);
        let (plain, secret) = (&found.nodes[0], &found.nodes[1]);

        assert_eq!(
            secret.role, "AXTextField",
            "the same role as the plain field"
        );
        assert!(secret.secure, "and told apart by its subrole");
        assert_eq!(secret.value, None, "its value never leaves the platform");
        assert!(!plain.secure);
        assert_eq!(plain.value.as_deref(), Some("ada"));
        assert_eq!(
            recorder.values_read(),
            vec![0],
            "the plain field's value was read; the password field's was never asked for"
        );
    }

    // --- budgets ---------------------------------------------------------------

    // Depth 14 is walked and depth 15 is not, and the walk SAYS a subtree was
    // deeper than it goes.
    #[test]
    fn a_walk_goes_fourteen_deep_and_says_when_a_subtree_went_deeper() {
        // Thirteen nested groups put their last child at depth fourteen.
        let mut elements: Vec<Scripted> = (0..13)
            .map(|level| Scripted {
                children: vec![level + 1],
                ..Scripted::of("AXGroup")
            })
            .collect();
        elements[12].children = vec![13, 14];
        elements.push(Scripted::button("At fourteen", frame(0.0, 0.0)));
        elements.push(Scripted {
            children: vec![15],
            ..Scripted::of("AXGroup")
        });
        elements.push(Scripted::button("At fifteen", frame(0.0, 0.0)));
        let (_recorder, ax) = platform(elements);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["At fourteen"]);
        assert_eq!(found.truncated, Some("depth"));
    }

    // At most 250 controls are collected, and a walk that stopped there says so.
    #[test]
    fn a_walk_collects_at_most_two_hundred_and_fifty_controls() {
        let mut elements = vec![
            Scripted::group("Left", &(2..152).collect::<Vec<_>>()),
            Scripted::group("Right", &(152..302).collect::<Vec<_>>()),
        ];
        elements
            .extend((0..300).map(|index| Scripted::button(&format!("b{index}"), frame(0.0, 0.0))));
        let (recorder, ax) = platform(elements);

        let found = whole(&ax);

        assert_eq!(found.nodes.len(), 250);
        assert_eq!(found.truncated, Some("nodes"));
        drop(found);
        assert_eq!(recorder.counts().2, 0, "everything it retained went back");
    }

    // A large sparse subtree has few controls and many nodes: the walk stops after
    // 3000 visits whatever it collected, and says so.
    #[test]
    fn a_walk_visits_at_most_three_thousand_nodes() {
        let groups = 13;
        let per_group = 240;
        let mut elements: Vec<Scripted> = (0..groups)
            .map(|group| {
                let first = groups + 1 + group * per_group;
                Scripted::group(
                    &format!("g{group}"),
                    &(first..first + per_group).collect::<Vec<_>>(),
                )
            })
            .collect();
        elements.push(Scripted::button("Past the budget", frame(0.0, 0.0)));
        elements.extend(texts(groups * per_group));
        let (_recorder, ax) = platform(elements);

        let found = whole(&ax);

        assert!(
            found.nodes.is_empty(),
            "the button after the budget is not reached"
        );
        assert_eq!(found.truncated, Some("nodes"));
    }

    // The bound the caps could not give: an application slow enough that the walk
    // runs out of wall clock rather than out of nodes. It stops and SAYS it
    // stopped, because a partial tree passed off as the whole one is how a caller
    // concludes a control is not there.
    #[test]
    fn a_walk_stops_on_its_wall_clock_budget_and_names_the_reason() {
        let (recorder, ax) = platform(vec![
            Scripted::button("One", frame(0.0, 0.0)),
            Scripted::button("Two", frame(0.0, 40.0)),
            Scripted::button("Three", frame(0.0, 80.0)),
            Scripted::button("Four", frame(0.0, 120.0)),
        ]);
        let clock = TestClock::new();
        recorder.costs_per_node(700, clock.clone());

        let found = walk(&ax, PID, clock.as_ref(), WALK_BUDGET_MS);

        assert_eq!(WALK_BUDGET_MS, 1_500);
        assert_eq!(found.truncated, Some("time"));
        // The budget is checked between nodes: the third read starts at 1400 ms.
        assert_eq!(labels(&found), vec!["One", "Two", "Three"]);

        drop(found);
        assert_eq!(
            recorder.counts().2,
            0,
            "a truncated walk still releases everything it retained"
        );
    }

    // Time wins whenever it fires: once the clock is past the deadline every other
    // bound would stop the walk too, and "we ran out of time" is the useful answer.
    #[test]
    fn running_out_of_time_is_the_reason_given_over_any_other() {
        // A table cut at 250 rows notes "nodes" first; a slow button after it then
        // runs the clock out before the last one is read.
        let mut elements = vec![
            Scripted::group("Table", &(3..263).collect::<Vec<_>>()),
            Scripted::button("Slow", frame(0.0, 0.0)),
            Scripted::button("Never read", frame(0.0, 0.0)),
        ];
        elements.extend(texts(260));
        let (recorder, ax) = platform(elements);
        let clock = TestClock::new();
        recorder.costs_per_node(0, clock.clone());
        recorder.change(1, |slow| slow.cost_ms = 2_000);

        let found = walk(&ax, PID, clock.as_ref(), WALK_BUDGET_MS);

        assert_eq!(labels(&found), vec!["Slow"]);
        assert_eq!(found.truncated, Some("time"));
    }

    // --- paging (M3) -------------------------------------------------------------

    // Children are read a page at a time, 128 to a page, and no more than 250 of
    // one parent's — a table with thousands of rows is never copied whole to keep a
    // few hundred. A parent with more children than that is a tree the walk did not
    // see all of, and it says so.
    #[test]
    fn children_are_read_in_pages_and_a_long_list_is_cut_and_named() {
        let mut elements = vec![Scripted::group("Table", &(1..301).collect::<Vec<_>>())];
        elements.extend(texts(300));
        let (recorder, ax) = platform(elements);

        let found = whole(&ax);

        assert_eq!(recorder.pages(), vec![(0, 1), (0, 128), (128, 122)]);
        assert_eq!(found.truncated, Some("nodes"));
    }

    // A short list is one page, and a whole tree read says nothing was cut.
    #[test]
    fn a_short_list_is_one_page_and_a_whole_tree_is_not_truncated() {
        let (recorder, ax) = platform(vec![
            Scripted::button("One", frame(0.0, 0.0)),
            Scripted::button("Two", frame(0.0, 40.0)),
            Scripted::button("Three", frame(0.0, 80.0)),
        ]);

        let found = whole(&ax);

        assert_eq!(recorder.pages(), vec![(0, 3)]);
        assert_eq!(found.truncated, None);
        assert_eq!(found.nodes.len(), 3);
    }

    // A node with no children costs no page read at all.
    #[test]
    fn a_leaf_costs_no_page() {
        let (recorder, ax) = platform(vec![Scripted::group("Empty", &[])]);

        whole(&ax);

        assert_eq!(recorder.pages(), vec![(0, 1)], "only the application's own");
    }

    // The count is what the application said a moment ago; the list is what it
    // has now. A list shorter than its count is read to its end, and the page that
    // comes back empty ends the reading — nothing was cut, so nothing is said.
    #[test]
    fn a_count_that_overstates_the_list_reads_what_is_there_and_stops() {
        let (recorder, ax) = platform(vec![
            Scripted::group("List", &[1, 2, 3]),
            Scripted::button("One", frame(0.0, 0.0)),
            Scripted::button("Two", frame(0.0, 40.0)),
            Scripted::button("Three", frame(0.0, 80.0)),
        ]);
        recorder.counts_children(0, 5);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["One", "Two", "Three"]);
        assert_eq!(recorder.pages(), vec![(0, 1), (0, 5), (3, 2)]);
        assert_eq!(found.truncated, None);
    }

    // A page that comes back empty is the end of that parent's list, however many
    // the count promised: asking again would ask the same question forever.
    #[test]
    fn an_empty_page_ends_the_reading_of_that_parent() {
        let (recorder, ax) = platform(vec![Scripted::group("Quiet", &[])]);
        recorder.counts_children(0, 300);

        let found = whole(&ax);

        assert_eq!(recorder.pages(), vec![(0, 1), (0, 128)], "one empty page");
        assert!(found.nodes.is_empty());
        assert_eq!(found.truncated, None);
    }

    // --- nodes that answer nothing -----------------------------------------------

    /// A pressable card holding one button, beside one more button: the shape every
    /// test below breaks one read of.
    fn card_and_buttons() -> Vec<Scripted> {
        let mut card = acting("AXGroup", "Card", &["AXPress"]);
        card.children = vec![1];
        vec![
            card,
            Scripted::button("Open", frame(0.0, 0.0)),
            Scripted::button("Close", frame(0.0, 40.0)),
        ]
    }

    // A container that did not answer in time is still there and slow: it is not
    // listed, what is inside it is still read, and the listing SAYS it did not see
    // everything — a slow container's subtree silently dropped, under a reply that
    // claimed the whole tree, is what this exists to stop.
    #[test]
    fn a_node_that_did_not_answer_is_skipped_and_its_children_are_still_read() {
        let (recorder, ax) = platform(card_and_buttons());
        recorder.fails_facts_for(0, Unread::NoAnswer);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Open", "Close"]);
        assert!(found.nodes[0].path.is_empty(), "it gave no name to carry");
        assert_eq!(found.truncated, Some("time"));
    }

    // A node that is gone takes its subtree with it, and that is not a walk that
    // missed something: there is nothing there to miss.
    #[test]
    fn a_node_that_is_gone_takes_its_subtree_with_it_and_nothing_is_said() {
        let (recorder, ax) = platform(card_and_buttons());
        recorder.fails_facts_for(0, Unread::Gone);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Close"]);
        assert_eq!(found.truncated, None);
        assert_eq!(
            recorder.pages(),
            vec![(0, 2)],
            "nothing asked of what is gone"
        );
    }

    // A node that answered and would not say what it is cannot be listed, and what
    // is inside it can.
    #[test]
    fn a_node_that_refuses_its_facts_is_not_listed_and_its_children_are() {
        let (recorder, ax) = platform(card_and_buttons());
        recorder.fails_facts_for(0, Unread::Refused);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Open", "Close"]);
        assert_eq!(found.truncated, None);
    }

    // A child count that did not answer in time leaves the children unread, and
    // the listing says so.
    #[test]
    fn a_child_count_that_did_not_answer_reads_no_children_and_says_so() {
        let (recorder, ax) = platform(card_and_buttons());
        recorder.fails_count_for(0, Unread::NoAnswer);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Card", "Close"]);
        assert_eq!(found.truncated, Some("time"));
        assert_eq!(recorder.copies(), 0, "a slow count is not refused");
    }

    // A node gone between its own read and its count has nothing to read.
    #[test]
    fn a_child_count_from_a_node_that_is_gone_reads_as_a_leaf() {
        let (recorder, ax) = platform(card_and_buttons());
        recorder.fails_count_for(0, Unread::Gone);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Card", "Close"]);
        assert_eq!(found.truncated, None);
        assert_eq!(recorder.copies(), 0);
    }

    // Xcode answers AXWindows and refuses to count it. The count is only how a long
    // list is read in pages, so a refused count is one copy of the list instead.
    #[test]
    fn a_child_count_that_is_refused_falls_back_to_one_uncounted_copy() {
        let (recorder, ax) = platform(card_and_buttons());
        recorder.fails_count_for(0, Unread::Refused);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Card", "Open", "Close"]);
        assert_eq!(found.nodes[1].path, vec!["Card"]);
        assert_eq!(found.truncated, None);
        assert_eq!(recorder.copies(), 1);
        assert_eq!(recorder.pages(), vec![(0, 2)], "no page of the card's list");
    }

    // One copy keeps no more of a parent than a count would: 250, and a parent with
    // more says so. The one copied past the cap goes back unvisited.
    #[test]
    fn an_uncounted_copy_keeps_two_hundred_and_fifty_and_says_there_were_more() {
        let mut elements = vec![Scripted::group("Table", &(1..252).collect::<Vec<_>>())];
        elements.extend(texts(250));
        elements.push(Scripted::button("Past the cap", frame(0.0, 0.0)));
        let (recorder, ax) = platform(elements);
        recorder.fails_count_for(0, Unread::Refused);

        let found = whole(&ax);

        assert!(found.nodes.is_empty(), "the 251st child is never visited");
        assert_eq!(found.truncated, Some("nodes"));
        drop(found);
        assert_eq!(recorder.counts().2, 0, "everything it copied went back");
    }

    // The copy is a read like any other: one that did not answer in time says so,
    // and one that is refused as well leaves a leaf.
    #[test]
    fn an_uncounted_copy_that_did_not_answer_says_so() {
        let (recorder, ax) = platform(card_and_buttons());
        recorder.fails_count_for(0, Unread::Refused);
        recorder.fails_copy_for(0, Unread::NoAnswer);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Card", "Close"]);
        assert_eq!(found.truncated, Some("time"));

        let (recorder, ax) = platform(card_and_buttons());
        recorder.fails_count_for(0, Unread::Refused);
        recorder.fails_copy_for(0, Unread::Refused);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Card", "Close"]);
        assert_eq!(found.truncated, None);
    }

    // --- a page that fails after its count answered ---------------------------------

    /// A table of 130 pressable rows: two pages, (0, 128) and (128, 2). Every test
    /// below breaks the second.
    fn two_page_table() -> Vec<Scripted> {
        let mut elements = vec![Scripted::group("Table", &(1..131).collect::<Vec<_>>())];
        elements.extend((0..130).map(|row| Scripted::button(&format!("r{row}"), frame(0.0, 0.0))));
        elements
    }

    /// The labels of the first `rows` rows of [`two_page_table`].
    fn rows(rows: usize) -> Vec<String> {
        (0..rows).map(|row| format!("r{row}")).collect()
    }

    // A page that did not answer in time is not the end of the list: what the
    // pages before it held is listed, and the listing says it did not see the rest.
    #[test]
    fn a_page_that_did_not_answer_stops_that_parent_and_says_so() {
        let (recorder, ax) = platform(two_page_table());
        recorder.fails_page_for(0, 128, Unread::NoAnswer);

        let found = whole(&ax);

        assert_eq!(labels(&found), rows(128));
        assert_eq!(found.truncated, Some("time"));
        assert_eq!(recorder.copies(), 0, "a slow page is not refused");
    }

    // A parent gone between two of its pages has nothing further to read, and that
    // is not a walk that missed something.
    #[test]
    fn a_page_from_a_parent_that_is_gone_stops_that_parent_and_says_nothing() {
        let (recorder, ax) = platform(two_page_table());
        recorder.fails_page_for(0, 128, Unread::Gone);

        let found = whole(&ax);

        assert_eq!(labels(&found), rows(128));
        assert_eq!(found.truncated, None);
        assert_eq!(recorder.copies(), 0);
    }

    // A page that is refused is read from one copy of the whole list instead, as a
    // refused count is — and the rows the earlier pages already listed are not
    // listed twice.
    #[test]
    fn a_refused_page_falls_back_to_one_copy_and_lists_no_row_twice() {
        let (recorder, ax) = platform(two_page_table());
        recorder.fails_page_for(0, 128, Unread::Refused);

        let found = whole(&ax);

        assert_eq!(labels(&found), rows(130));
        assert_eq!(found.truncated, None);
        assert_eq!(recorder.copies(), 1);
        assert_eq!(recorder.pages(), vec![(0, 1), (0, 128), (128, 2)]);
        drop(found);
        assert_eq!(
            recorder.counts().2,
            0,
            "the copied rows already listed went back"
        );
    }

    // --- references --------------------------------------------------------------

    // The invariant the whole design exists for: a walk retains every node it
    // passes, gives back the ones it did not list as it goes, and every node it
    // listed releases when it is dropped.
    #[test]
    fn every_reference_a_walk_retains_is_released_when_its_nodes_are_dropped() {
        let (recorder, ax) = platform(vec![
            Scripted::group("Toolbar", &[1, 2]),
            Scripted::button("Save", frame(10.0, 10.0)),
            Scripted::button("Cancel", frame(100.0, 10.0)),
            Scripted::field("Name", "ada", frame(10.0, 50.0)),
        ]);

        let found = whole(&ax);
        assert_eq!(found.nodes.len(), 3);
        assert_eq!(
            recorder.counts(),
            (5, 2, 3),
            "the application and the toolbar went back; the three controls are held"
        );

        drop(found);
        assert_eq!(
            recorder.counts(),
            (5, 5, 0),
            "a walk nobody kept holds nothing"
        );
    }

    // Half of a walk kept and half dropped is the ordinary case — the view filter
    // and the badge cap both do it — and the two halves must account separately.
    #[test]
    fn keeping_some_nodes_releases_only_the_others() {
        let (recorder, ax) = platform(vec![
            Scripted::button("One", frame(0.0, 0.0)),
            Scripted::button("Two", frame(0.0, 40.0)),
            Scripted::button("Three", frame(0.0, 80.0)),
        ]);

        let mut found = whole(&ax);
        let kept = found.nodes.remove(0);
        drop(found);

        assert_eq!(recorder.counts(), (4, 3, 1), "one reference is still held");
        drop(kept);
        assert_eq!(recorder.counts(), (4, 4, 0));
    }

    #[test]
    fn a_walk_of_another_application_finds_nothing() {
        let (recorder, ax) = platform(vec![Scripted::button("Save", frame(0.0, 0.0))]);

        assert!(walk(&ax, 9999, &SystemClock::new(), u64::MAX)
            .nodes
            .is_empty());
        assert_eq!(recorder.counts(), (0, 0, 0));
    }

    // A walk from a window a target holds lists that window's controls and no
    // other window's: only where it starts differs.
    #[test]
    fn a_walk_from_a_window_lists_only_what_is_inside_it() {
        let (recorder, ax) = platform(vec![
            Scripted::button("Elsewhere", frame(0.0, 0.0)),
            Scripted {
                label: Some("Document".to_string()),
                children: vec![2],
                ..Scripted::of("AXWindow")
            },
            Scripted::button("Inside", frame(0.0, 0.0)),
        ]);
        recorder.windows_are(&[1]);
        let windows = crate::ax::application_windows(&ax, PID);

        let found = walk_from(&ax, windows[0].handle(), &SystemClock::new(), u64::MAX);

        assert_eq!(labels(&found), vec!["Inside"]);
        assert_eq!(found.nodes[0].path, vec!["Document"]);
    }

    // --- messages (M7) -------------------------------------------------------------

    // Every question the walk asks the platform is a message, and the platform
    // counts them, so a reply can say what its read cost.
    #[test]
    fn a_walk_costs_one_message_per_question_it_asks() {
        let (_recorder, ax) = platform(vec![
            Scripted::button("Save", frame(0.0, 0.0)),
            Scripted::field("Name", "ada", frame(0.0, 40.0)),
        ]);
        let before = ax.messages();

        whole(&ax);

        // The application: its facts, its child count, one page. Each control: its
        // facts, its value, whether that may be written, and its child count — and
        // no page for a leaf.
        assert_eq!(ax.messages() - before, 3 + 2 * 4);
    }

    // A node the walk does not list is asked nothing past its facts and its child
    // count: a value and whether it may be written cost a message each, and the walk
    // passes many more nodes than it lists.
    #[test]
    fn only_a_listed_control_is_asked_for_its_value() {
        let (recorder, ax) = platform(vec![
            Scripted::group("Toolbar", &[1, 2]),
            Scripted::button("Save", frame(0.0, 0.0)),
            Scripted {
                value: Some("Hello".to_string()),
                ..Scripted::of("AXStaticText")
            },
        ]);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Save"]);
        assert_eq!(
            recorder.values_read(),
            vec![1],
            "the button's, and no other"
        );
    }
}
