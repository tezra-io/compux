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
//!
//! Protocol 12 (M86 P2) gives a walk a [`Shape`]: a find (`query`) lists the nodes
//! that match wherever they are, a `skeleton` stops at depth three and names what
//! it cut, and a walk may start from several roots (the open menus or dialogs) or
//! list its root (the focused element). Every listed node also says which writes it
//! takes and what its states read, which is what its `verbs` are decided from.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ax::{Ax, Facts, Frame, Handle, Retained, Routes, States, Unread};
use crate::gate::Clock;
use crate::roles;

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

/// How deep a skeleton reads below its root before it cuts (protocol 12).
pub const SKELETON_DEPTH: usize = 3;

/// The three reasons a walk stops early, in the words the wire uses.
const TRUNCATED_NODES: &str = "nodes";
const TRUNCATED_DEPTH: &str = "depth";
const TRUNCATED_TIME: &str = "time";

/// Roles worth surfacing as targets, in each reader's own words: macOS's AX roles,
/// then AT-SPI's (`linux::vocabulary`). A password field is listed like any other
/// field — a control the caller cannot see at all is one it invents a reason for
/// missing — and its value is withheld instead: on macOS it is an `AXTextField`
/// told apart by its subrole, on Linux a `password text`.
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
    "push button",
    "toggle button",
    "check box",
    "radio button",
    "menu item",
    "check menu item",
    "radio menu item",
    "combo box",
    "text",
    "entry",
    "password text",
    "spin button",
    "slider",
    "link",
    "page tab",
    "list item",
    "table cell",
    "tree item",
    "push button menu",
    "switch",
];

/// The actions that make a node a control whatever its role (M4): it does
/// something when operated. Not `AXShowMenu`, `AXScrollToVisible`, `AXRaise` or
/// a focus-only action — every container answers those, and listing on them would
/// list the whole tree.
///
/// Then AT-SPI's, as `GetName(i)` names them: the press names (click, press,
/// activate, jump, check, open) and Qt's toggle and stepping names (L3-19). Not
/// `doDefault`, which Chromium advertises on plain text where it does nothing
/// (L18-16), nor `clickAncestor`, Chromium's text inside a link or a clickable div,
/// whose ancestor is the control, nor `showContextMenu`, `ShowMenu` or `SetFocus`.
/// The toolkits disagree on case, so these are compared without it.
const PRIMARY_ACTIONS: &[&str] = &[
    "AXPress",
    "AXConfirm",
    "AXPick",
    "AXIncrement",
    "AXDecrement",
    "AXOpen",
    "click",
    "press",
    "activate",
    "jump",
    "check",
    "open",
    "toggle",
    "increase",
    "decrease",
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
        || actions.iter().any(|action| {
            PRIMARY_ACTIONS
                .iter()
                .any(|primary| primary.eq_ignore_ascii_case(action))
        })
}

/// A find (protocol 12): the nodes whose normalised role is `role` and whose text
/// contains `name`, ignoring case. Either may be left out, never both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub role: Option<String>,
    pub name: Option<String>,
}

/// What a walk lists (protocol 12). The default is protocol 11's: every control,
/// below a root that is not itself listed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    /// Only the nodes that match, control or not.
    pub query: Option<Query>,
    /// Read [`SKELETON_DEPTH`] deep and no further; a node there that has children
    /// is listed as a cut, with how many it hides.
    pub skeleton: bool,
    /// List each root itself (the `focused` scope).
    pub list_root: bool,
}

/// An interactive element, as one walk found it. Everything the wire publishes
/// about a control is read once, here, so a reply is one traversal rather than a
/// second round of queries per element.
#[derive(Debug)]
pub struct Node {
    /// The platform's own role: `native_role` on the wire, and what a reference is
    /// revalidated against.
    pub role: String,
    pub subrole: Option<String>,
    /// Title, else description. NOT the value: a field's contents are its own
    /// field, and using them as a name made a text box look like its text.
    pub label: Option<String>,
    /// Bounded, and absent for a secure field.
    pub value: Option<String>,
    pub secure: bool,
    pub enabled: bool,
    /// Its OWN action names. Never inferred from the role: a control that looks
    /// pressable and is not would be a refusal the caller was invited to expect.
    pub actions: Vec<String>,
    /// Some of its actions could not be named, so `actions` is not all it has.
    pub actions_partial: bool,
    /// Which of its attributes may be written; `select` also when its container
    /// selects its children.
    pub routes: Routes,
    pub states: States,
    /// `None` where the platform places nothing (GTK 4, a Wayland session). The
    /// macOS walk lists no such node.
    pub frame: Option<Frame>,
    /// Up to three ancestor labels, nearest last.
    pub path: Vec<String>,
    /// A skeleton's cut node: how many children it hides.
    pub hidden: Option<usize>,
    /// The nearest listed node it sits inside, as an index into the walk's nodes.
    pub under: Option<usize>,
    pub element: Retained,
}

impl Node {
    /// Its normalised role (PROTOCOL.md §6.1).
    pub fn kind(&self) -> &'static str {
        roles::normalise(&self.role, self.subrole.as_deref())
    }

    /// Exactly the verbs it has a route for.
    pub fn verbs(&self) -> Vec<&'static str> {
        roles::verbs(self.kind(), &self.actions, &self.routes, &self.states)
    }
}

/// What one traversal answered.
#[derive(Debug, Default)]
pub struct Walk {
    pub nodes: Vec<Node>,
    /// Why it stopped before it had seen everything, as the wire spells it:
    /// `"nodes"` (it collected or traversed as many as it may, or a parent had
    /// more children than it reads), `"depth"` (a subtree was deeper than the walk
    /// goes) or `"time"` (it ran out of its wall-clock budget, or a node did not
    /// answer within its own timeout). `None` means it saw the whole tree.
    pub truncated: Option<&'static str>,
    /// Children behind skeleton cuts that could not be listed (no position).
    pub hidden: usize,
}

/// Walk ONE application's tree from its own application element, held for as long
/// as the walk reads through it.
pub fn walk(ax: &Rc<dyn Ax>, pid: i32, clock: &dyn Clock, deadline_ms: u64) -> Walk {
    walk_app(ax, pid, &Shape::default(), clock, deadline_ms)
}

/// The same walk, from a root this process is already holding — the window a
/// target is bound to. Same collection, same bounds, same truncation reasons; only
/// where it starts differs, which is what keeps a bound window's controls the
/// window's rather than the whole application's.
pub fn walk_from(ax: &Rc<dyn Ax>, root: Handle, clock: &dyn Clock, deadline_ms: u64) -> Walk {
    walk_handle(ax, root, &Shape::default(), clock, deadline_ms)
}

/// [`walk`], listing what `shape` asks for.
pub fn walk_app(
    ax: &Rc<dyn Ax>,
    pid: i32,
    shape: &Shape,
    clock: &dyn Clock,
    deadline_ms: u64,
) -> Walk {
    let Some(root) = ax.application(ax, pid) else {
        return Walk::default();
    };
    walk_handle(ax, root.handle(), shape, clock, deadline_ms)
}

/// [`walk_from`], listing what `shape` asks for.
pub fn walk_handle(
    ax: &Rc<dyn Ax>,
    root: Handle,
    shape: &Shape,
    clock: &dyn Clock,
    deadline_ms: u64,
) -> Walk {
    let mut walker = Walker::new(ax, shape, clock, deadline_ms);
    walker.root(root);
    walker.done()
}

/// One walk over several roots in order — the open menus, the open dialogs — with
/// one set of budgets. With `list_root` each root is listed itself.
pub fn walk_roots(
    ax: &Rc<dyn Ax>,
    roots: Vec<Retained>,
    shape: &Shape,
    clock: &dyn Clock,
    deadline_ms: u64,
) -> Walk {
    let mut walker = Walker::new(ax, shape, clock, deadline_ms);
    for root in roots {
        if shape.list_root {
            walker.listed_root(root);
        } else {
            // Held until its own walk is done: what is listed below it was read
            // through it.
            walker.root(root.handle());
        }
    }
    walker.done()
}

/// Whether a node is what a find asks for: its normalised role, and its text — its
/// label, or for static text its value — containing the name, ignoring case.
pub fn matches(query: &Query, kind: &str, label: Option<&str>, value: Option<&str>) -> bool {
    if query.role.as_deref().is_some_and(|role| role != kind) {
        return false;
    }
    let Some(name) = &query.name else {
        return true;
    };
    let needle = name.to_lowercase();
    let says = |text: Option<&str>| text.is_some_and(|text| text.to_lowercase().contains(&needle));
    // A field's value is what it holds, not what it is called.
    says(label) || (kind == "text" && says(value))
}

/// One traversal in progress.
struct Walker<'a> {
    ax: &'a Rc<dyn Ax>,
    shape: &'a Shape,
    clock: &'a dyn Clock,
    deadline_ms: u64,
    visited: usize,
    truncated: Option<&'static str>,
    nodes: Vec<Node>,
    /// The labels of the nodes above the one being visited, outermost first.
    ancestors: Vec<String>,
    /// The listed nodes above the one being visited, as indices into `nodes`.
    listed: Vec<usize>,
    /// Whether each container read so far selects its children, by handle: asked
    /// once per container, not once per child.
    selecting: HashMap<u64, bool>,
    hidden: usize,
}

impl<'a> Walker<'a> {
    fn new(
        ax: &'a Rc<dyn Ax>,
        shape: &'a Shape,
        clock: &'a dyn Clock,
        deadline_ms: u64,
    ) -> Walker<'a> {
        Walker {
            ax,
            shape,
            clock,
            deadline_ms,
            visited: 0,
            truncated: None,
            nodes: Vec::new(),
            ancestors: Vec::new(),
            listed: Vec::new(),
            selecting: HashMap::new(),
            hidden: 0,
        }
    }

    fn done(self) -> Walk {
        Walk {
            nodes: self.nodes,
            truncated: self.truncated,
            hidden: self.hidden,
        }
    }

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

    /// A root listed itself (the `focused` scope), read through its own parent so
    /// it is asked, like any listed node, whether its container selects it. The
    /// parent is held while it is visited.
    fn listed_root(&mut self, root: Retained) {
        let parent = self.ax.parent(self.ax, root.handle());
        let through = parent.as_ref().map(Retained::handle);
        self.visit(root, through, 0);
        if let Some(parent) = through {
            // The answer holds only while the parent does.
            self.selecting.remove(&parent.0);
        }
    }

    /// One node at `depth`, read through `parent`.
    fn visit(&mut self, element: Retained, parent: Option<Handle>, depth: usize) {
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
        if self.shape.skeleton && depth == SKELETON_DEPTH {
            return self.cut(facts, element, parent);
        }
        let label = facts.label.clone();
        let before = self.nodes.len();
        let _unlisted = self.consider(facts, element, parent, depth == 0);

        // What is inside a listed node names it as the node it sits inside.
        let listed = (self.nodes.len() > before).then_some(before);
        self.listed.extend(listed);
        self.descend(handle, label, depth);
        if listed.is_some() {
            self.listed.pop();
        }
    }

    /// A skeleton's last level. A node there with children is listed whatever it
    /// is, as the cut the caller drills into, and its children are counted and not
    /// read; one with none is listed as any node is. A cut that cannot be listed
    /// still counts what it hides, so the caller knows the skeleton is not all.
    fn cut(&mut self, facts: Facts, element: Retained, parent: Option<Handle>) {
        let hidden = self.hidden_below(element.handle());
        if hidden == 0 {
            let _unlisted = self.consider(facts, element, parent, false);
            return;
        }
        let before = self.nodes.len();
        let _unlisted = self.collect(facts, element, parent, Some(hidden), None);
        if self.nodes.len() == before {
            self.hidden += hidden;
        }
    }

    /// How many children a cut hides: its count, or one copy's length where it
    /// will not count them. Never more than a walk would read of one parent.
    fn hidden_below(&mut self, handle: Handle) -> usize {
        let counted = match self.ax.child_count(handle) {
            Ok(count) => Ok(count.min(MAX_CHILDREN)),
            Err(Unread::Refused) => self
                .ax
                .children_uncounted(self.ax, handle, MAX_CHILDREN)
                .map(|children| children.len()),
            Err(why) => Err(why),
        };
        match counted {
            Ok(count) => count,
            Err(Unread::NoAnswer) => {
                self.note(TRUNCATED_TIME);
                0
            }
            // Gone, or it will not say: there is nothing to hide.
            Err(Unread::Gone | Unread::Refused) => 0,
        }
    }

    /// List this node if the walk's shape asks for it: by default a control (or
    /// the root of a walk that lists its root), on a find only a match. Otherwise
    /// the element comes back, so it stays held while its children are read.
    fn consider(
        &mut self,
        facts: Facts,
        element: Retained,
        parent: Option<Handle>,
        root: bool,
    ) -> Option<Retained> {
        let shape = self.shape;
        let Some(query) = &shape.query else {
            let wanted =
                (root && shape.list_root) || eligible(facts.role.as_deref(), &facts.actions);
            return if wanted {
                self.collect(facts, element, parent, None, None)
            } else {
                Some(element)
            };
        };

        let kind = roles::normalise(
            facts.role.as_deref().unwrap_or(""),
            facts.subrole.as_deref(),
        );
        // Static text is found by what it says, so a find by name reads its value;
        // nothing else is asked for one unless it is listed.
        let text = query.name.is_some() && kind == "text" && !facts.secure;
        let value = if text {
            self.ax.value(element.handle())
        } else {
            None
        };
        if !matches(query, kind, facts.label.as_deref(), value.as_deref()) {
            return Some(element);
        }
        self.collect(facts, element, parent, None, text.then_some(value))
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
        // The answer holds only while the parent does: once it is released, its
        // handle may name something else.
        self.selecting.remove(&parent.0);
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
                self.visit(child, Some(parent), depth);
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
            self.visit(child, Some(parent), depth);
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

    /// List a node the shape asked for, if it says where it is — or is on screen
    /// where the platform places nothing, which is listed with no click point.
    /// Only then is it asked which writes it
    /// takes, what its states read and — unless it is secure — what its value is: a
    /// message each, and the walk passes many more nodes than it lists. `value` is
    /// the value a find already read, so it is not asked twice. Otherwise the
    /// element comes back, so it stays held while its children are read through
    /// it.
    fn collect(
        &mut self,
        facts: Facts,
        element: Retained,
        parent: Option<Handle>,
        hidden: Option<usize>,
        value: Option<Option<String>>,
    ) -> Option<Retained> {
        let Some(role) = facts
            .role
            .filter(|_| facts.frame.is_some() || facts.placeless)
        else {
            return Some(element);
        };
        let handle = element.handle();
        let kind = roles::normalise(&role, facts.subrole.as_deref());

        let value = match value {
            Some(value) => value,
            // A secure field's value is never asked for: not asking is a stronger
            // guarantee than asking and throwing the answer away.
            None if facts.secure => None,
            None => self.ax.value(handle),
        };
        let routes = self.routes(handle, parent);
        let states = self.states(handle, kind);
        let under = self.listed.last().copied();

        self.nodes.push(Node {
            value,
            routes,
            states,
            actions_partial: facts.actions_partial,
            secure: facts.secure,
            enabled: facts.enabled,
            label: facts.label,
            path: self.path(),
            actions: facts.actions,
            subrole: facts.subrole,
            hidden,
            under,
            role,
            frame: facts.frame,
            element,
        });
        None
    }

    /// Which writes a listed node takes. An item that cannot select itself is
    /// selected through its container, which is asked once for all its children.
    fn routes(&mut self, handle: Handle, parent: Option<Handle>) -> Routes {
        let routes = self.ax.routes(handle);
        let Some(parent) = parent.filter(|_| !routes.select) else {
            return routes;
        };
        let ax = self.ax;
        let select = *self
            .selecting
            .entry(parent.0)
            .or_insert_with(|| ax.selects_children(parent));
        Routes { select, ..routes }
    }

    /// What a listed node's states read; its check state only where its role has
    /// one, or it is a disclosure, whose value is whether it is open. A state that
    /// did not answer is not reported and lists no verb that needs it; one that did
    /// not answer in time is a listing that did not see everything.
    fn states(&mut self, handle: Handle, kind: &str) -> States {
        let on = roles::has_check_state(kind) || kind == "disclosure";
        match self.ax.states(handle, on) {
            Ok(states) => states,
            Err(Unread::NoAnswer) => {
                self.note(TRUNCATED_TIME);
                States::default()
            }
            Err(Unread::Gone | Unread::Refused) => States::default(),
        }
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
        let presses = |node: &Node| node.actions.iter().any(|action| action == "AXPress");
        assert!(presses(&found.nodes[0]));
        assert!(
            !presses(&found.nodes[1]),
            "AXConfirm lists it, and is no press"
        );
        assert!(
            !presses(&found.nodes[2]),
            "a role lists it, and its own list has no press"
        );
    }

    // The Linux reader answers AT-SPI's own words (M86 L1): its roles, and the
    // action names `GetName(i)` gives, which the toolkits spell in their own case —
    // GTK `click`, Qt `Press` and `Toggle`, Chromium `clickAncestor` (L3-19).
    #[test]
    fn an_at_spi_node_is_listed_for_its_role_or_a_primary_action_in_any_case() {
        let none: Vec<String> = Vec::new();
        for role in [
            "push button",
            "toggle button",
            "check box",
            "radio button",
            "menu item",
            "check menu item",
            "radio menu item",
            "combo box",
            "text",
            "entry",
            "password text",
            "spin button",
            "slider",
            "link",
            "page tab",
            "list item",
            "table cell",
            "tree item",
            "push button menu",
            "switch",
        ] {
            assert!(eligible(Some(role), &none), "{role} is a control");
        }
        for role in ["panel", "filler", "label", "frame", "scroll pane", "static"] {
            assert!(
                !eligible(Some(role), &none),
                "{role} alone is not a control"
            );
        }

        for primary in [
            "click", "Press", "press", "activate", "jump", "check", "open", "Toggle", "Increase",
            "Decrease",
        ] {
            assert!(
                eligible(Some("panel"), &[primary.to_string()]),
                "{primary} makes a control"
            );
        }
        // `clickAncestor` is Chromium's text inside a link or a clickable div: the
        // ancestor is the control, and the text would list it twice (L18-16).
        for secondary in [
            "showContextMenu",
            "ShowMenu",
            "SetFocus",
            "doDefault",
            "menu",
            "clickAncestor",
        ] {
            assert!(
                !eligible(Some("panel"), &[secondary.to_string()]),
                "{secondary} alone is not a primary action"
            );
        }
    }

    // An action that could not be named is not invented, and the control says its
    // list is not all of them.
    #[test]
    fn a_control_whose_actions_were_partly_named_says_so() {
        let (_recorder, ax) = platform(vec![
            Scripted {
                actions_partial: true,
                ..acting("push button", "Save", &["click"])
            },
            acting("push button", "Open", &["click"]),
        ]);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Save", "Open"]);
        assert!(found.nodes[0].actions_partial);
        assert!(!found.nodes[1].actions_partial);
    }

    // A platform that places nothing (GTK 4, a Wayland session) says so, and its
    // controls are listed with no frame; a node with no frame on a platform that
    // places its nodes is not on screen, and is not.
    #[test]
    fn a_control_the_platform_cannot_place_is_listed_without_a_frame() {
        let (_recorder, ax) = platform(vec![
            Scripted {
                placeless: true,
                ..Scripted::of("push button")
            },
            Scripted {
                label: Some("Offscreen".to_string()),
                ..Scripted::of("push button")
            },
            Scripted {
                placeless: true,
                ..acting("push button", "Placed", &["click"])
            },
        ]);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["-", "Placed"]);
        assert_eq!(found.nodes[0].frame, None);
        assert_eq!(found.nodes[1].frame, Some(frame(0.0, 0.0)));
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
        // facts, its value, which of its attributes may be written, its states and
        // its child count — and no page for a leaf. Their shared parent is asked once
        // whether it selects its children.
        assert_eq!(ax.messages() - before, 3 + 2 * 5 + 1);
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

    // --- protocol 12: what a walk lists -------------------------------------------

    fn shaped(ax: &Rc<dyn Ax>, shape: &Shape) -> Walk {
        walk_app(ax, PID, shape, &SystemClock::new(), u64::MAX)
    }

    fn query(role: Option<&str>, name: Option<&str>) -> Query {
        Query {
            role: role.map(str::to_string),
            name: name.map(str::to_string),
        }
    }

    fn find(role: Option<&str>, name: Option<&str>) -> Shape {
        Shape {
            query: Some(query(role, name)),
            ..Shape::default()
        }
    }

    fn skeleton() -> Shape {
        Shape {
            skeleton: true,
            ..Shape::default()
        }
    }

    /// A node of this role and label at a frame, with these children.
    fn holding(role: &str, label: &str, children: &[usize]) -> Scripted {
        Scripted {
            label: Some(label.to_string()),
            frame: Some(frame(0.0, 0.0)),
            children: children.to_vec(),
            ..Scripted::of(role)
        }
    }

    // A find compares the normalised role exactly and the name as a substring of
    // the node's text, ignoring case; static text is found by what it says.
    #[test]
    fn a_find_matches_a_normalised_role_and_a_name_ignoring_case() {
        assert!(matches(
            &query(Some("button"), None),
            "button",
            Some("Save"),
            None
        ));
        assert!(matches(
            &query(None, Some("SAV")),
            "button",
            Some("Save as…"),
            None
        ));
        assert!(matches(
            &query(Some("button"), Some("save")),
            "button",
            Some("Save"),
            None
        ));
        assert!(!matches(
            &query(Some("button"), Some("save")),
            "link",
            Some("Save"),
            None
        ));
        assert!(!matches(&query(None, Some("save")), "button", None, None));
        assert!(matches(
            &query(None, Some("total")),
            "text",
            None,
            Some("Total: 4")
        ));
        assert!(
            !matches(
                &query(None, Some("ada")),
                "textfield",
                Some("Name"),
                Some("ada")
            ),
            "a field's value is what it holds, not what it is called"
        );
    }

    // A find lists what matches wherever it is, whether or not it is a control, and
    // nothing else — the controls that do not match included.
    #[test]
    fn a_find_lists_every_match_control_or_not_and_nothing_else() {
        let (_recorder, ax) = platform(vec![
            Scripted::group("Sidebar", &[1, 2]),
            Scripted::button("Save", frame(0.0, 0.0)),
            holding("AXStaticText", "Saved 3 files", &[]),
            Scripted::field("Name", "ada", frame(0.0, 40.0)),
        ]);

        assert_eq!(
            labels(&shaped(&ax, &find(None, Some("save")))),
            vec!["Save", "Saved 3 files"]
        );
        assert_eq!(
            labels(&shaped(&ax, &find(Some("text"), None))),
            vec!["Saved 3 files"]
        );
        assert!(shaped(&ax, &find(Some("button"), Some("cancel")))
            .nodes
            .is_empty());
    }

    // Static text is found by its value, so a find by name reads the value of the
    // static text it passes and of what it lists — never a field it does not list.
    #[test]
    fn a_find_by_name_reads_the_value_of_static_text_and_of_nothing_it_skips() {
        let (recorder, ax) = platform(vec![
            Scripted {
                value: Some("Total: 4".to_string()),
                frame: Some(frame(0.0, 0.0)),
                ..Scripted::of("AXStaticText")
            },
            Scripted::field("Name", "ada", frame(0.0, 40.0)),
        ]);

        let found = shaped(&ax, &find(None, Some("total")));

        assert_eq!(found.nodes.len(), 1);
        assert_eq!(found.nodes[0].value.as_deref(), Some("Total: 4"));
        assert_eq!(
            recorder.values_read(),
            vec![0],
            "the field's value was never asked"
        );
    }

    /// A window holding a sidebar that holds a list of two buttons — the list at
    /// depth three — and a button beside the sidebar at depth two.
    fn deep_window(list_frame: Option<Frame>) -> Vec<Scripted> {
        vec![
            holding("AXWindow", "Main", &[1, 5]),
            holding("AXGroup", "Sidebar", &[2]),
            Scripted {
                frame: list_frame,
                ..holding("AXList", "Files", &[3, 4])
            },
            Scripted::button("One", frame(0.0, 0.0)),
            Scripted::button("Two", frame(0.0, 30.0)),
            Scripted::button("Top", frame(0.0, 60.0)),
        ]
    }

    // A skeleton reads three levels and stops; a node there with children is
    // listed as a cut, whatever it is, and says how many it hides.
    #[test]
    fn a_skeleton_cuts_at_depth_three_and_names_what_it_hides() {
        let (recorder, ax) = platform(deep_window(Some(frame(0.0, 0.0))));

        let found = shaped(&ax, &skeleton());

        assert_eq!(labels(&found), vec!["Files", "Top"]);
        assert_eq!(found.nodes[0].hidden, Some(2));
        assert_eq!(
            found.nodes[1].hidden, None,
            "a control above the cut is no cut"
        );
        assert_eq!(found.truncated, None, "a cut is not a truncation");
        assert_eq!(found.hidden, 0);
        // The application's, the window's and the sidebar's pages, and never the
        // list's: what a cut hides is not read.
        assert_eq!(recorder.pages(), vec![(0, 1), (0, 2), (0, 1)]);
    }

    // A cut with nowhere to click cannot be listed, and what it hides is still
    // counted, so a caller knows the skeleton is not everything.
    #[test]
    fn a_cut_that_cannot_be_listed_is_still_counted() {
        let (_recorder, ax) = platform(deep_window(None));

        let found = shaped(&ax, &skeleton());

        assert_eq!(labels(&found), vec!["Top"]);
        assert_eq!(found.hidden, 2);
    }

    // A control AT the cut with nothing inside it is listed as any control is.
    #[test]
    fn a_control_at_the_cut_with_no_children_is_listed_as_a_control() {
        let (_recorder, ax) = platform(vec![
            holding("AXWindow", "Main", &[1]),
            holding("AXGroup", "Toolbar", &[2]),
            Scripted::button("Save", frame(0.0, 0.0)),
        ]);

        let found = shaped(&ax, &skeleton());

        assert_eq!(labels(&found), vec!["Save"]);
        assert_eq!(found.nodes[0].hidden, None);
    }

    // The focused scope lists its root and what is inside it, and each listed node
    // names the listed node it sits inside.
    #[test]
    fn a_listed_root_comes_first_and_its_children_sit_inside_it() {
        let (recorder, ax) = platform(vec![
            holding("AXGroup", "Form", &[1]),
            Scripted::button("Save", frame(0.0, 0.0)),
        ]);
        recorder.focuses(None, Some(0));
        let focused = ax
            .focused(&ax, PID, crate::ax::Focus::Element)
            .expect("the scripted focus");
        let shape = Shape {
            list_root: true,
            ..Shape::default()
        };

        let found = walk_roots(&ax, vec![focused], &shape, &SystemClock::new(), u64::MAX);

        assert_eq!(labels(&found), vec!["Form", "Save"]);
        assert_eq!(found.nodes[0].under, None);
        assert_eq!(found.nodes[1].under, Some(0));
        drop(found);
        assert_eq!(recorder.counts().2, 0, "everything it held went back");
    }

    // Several roots — the open menus — are walked in order with one set of budgets.
    #[test]
    fn several_roots_are_walked_in_order() {
        let (recorder, ax) = platform(vec![
            holding("AXMenu", "File", &[1]),
            acting("AXMenuItem", "Open", &["AXPress"]),
            holding("AXMenu", "Edit", &[3]),
            acting("AXMenuItem", "Copy", &["AXPress"]),
        ]);
        recorder.windows_are(&[2, 0]);
        let roots = crate::ax::application_windows(&ax, PID);

        let found = walk_roots(&ax, roots, &Shape::default(), &SystemClock::new(), u64::MAX);

        assert_eq!(labels(&found), vec!["Copy", "Open"]);
        assert_eq!(found.nodes[0].path, vec!["Edit"]);
    }

    // A listed node records the nearest listed node above it; one with none above
    // it records none.
    #[test]
    fn a_listed_node_names_the_listed_node_it_sits_inside() {
        let (_recorder, ax) = platform(vec![
            Scripted {
                children: vec![1],
                ..acting("AXGroup", "Card", &["AXPress"])
            },
            holding("AXGroup", "Body", &[2]),
            Scripted::button("Open", frame(0.0, 0.0)),
            Scripted::button("Elsewhere", frame(0.0, 40.0)),
        ]);

        let found = whole(&ax);

        assert_eq!(labels(&found), vec!["Card", "Open", "Elsewhere"]);
        assert_eq!(found.nodes[1].under, Some(0), "through an unlisted group");
        assert_eq!(found.nodes[2].under, None);
    }

    // A listed control says which writes it takes and what its states read; a
    // check state is asked only of a control that has one.
    #[test]
    fn a_listed_control_carries_its_routes_and_its_states() {
        let (_recorder, ax) = platform(vec![
            Scripted {
                routes: Routes {
                    focus: true,
                    ..Routes::default()
                },
                ..Scripted::checkbox("Bold", true, frame(0.0, 0.0))
            },
            Scripted {
                states: States {
                    on: Some(true),
                    focused: Some(true),
                    ..States::default()
                },
                ..Scripted::field("Name", "1", frame(0.0, 40.0))
            },
        ]);

        let found = whole(&ax);

        assert_eq!(found.nodes[0].states.on, Some(true));
        assert!(found.nodes[0].routes.focus);
        assert!(!found.nodes[0].routes.value);
        assert!(found.nodes[1].routes.value, "AXValue is its own route");
        assert_eq!(found.nodes[1].states.focused, Some(true));
        assert_eq!(
            found.nodes[1].states.on, None,
            "a field's value is text, never read as a check state"
        );
    }

    // An item that cannot select itself is selected through its container, and
    // the container is asked once for all of its children.
    #[test]
    fn a_container_that_selects_its_children_gives_each_of_them_select() {
        let (_recorder, ax) = platform(vec![
            Scripted {
                selects_children: true,
                ..holding("AXList", "Files", &[1, 2])
            },
            acting("AXRow", "One", &["AXConfirm"]),
            acting("AXRow", "Two", &["AXConfirm"]),
            holding("AXGroup", "Other", &[4]),
            acting("AXRow", "Three", &["AXConfirm"]),
        ]);
        let before = ax.messages();

        let found = whole(&ax);
        let spent = ax.messages() - before;

        let select: Vec<bool> = found.nodes.iter().map(|node| node.routes.select).collect();
        assert_eq!(select, vec![true, true, false]);
        // The application: facts, count, page. The list and the group: facts, count,
        // page each. Each row: facts, value, routes, states, count. Asked once each
        // whether they select their children: the list and the group.
        assert_eq!(spent, 3 + 2 * 3 + 3 * 5 + 2);
    }
}
