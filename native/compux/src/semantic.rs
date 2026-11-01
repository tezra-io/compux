//! The verbs on a reference, the wait on the tree and the surfaces (protocol 12,
//! M86 P2), above the accessibility seam.
//!
//! A verb is three steps, and only the middle one sends anything: [`plan`] proves
//! the control has a route for it and reads its state first, [`send`] is the one
//! message, and [`judge`] reads back what it did. A verb whose state already holds
//! plans to send nothing; a control with no route is refused, and nothing here
//! ever turns a refused verb into a click.
//!
//! A wait never sends input: [`wait`] looks at the tree every [`POLL_MS`], each
//! look bounded by the walk's own budget, until a [`Condition`] holds or its
//! timeout passes.

use std::rc::Rc;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::ax::{
    Ax, Elements, Entry, Expander, Facts, Focus, Frame, Handle, Retained, Route, Unread, Write,
    PRESS, SCROLL_TO_VISIBLE,
};
use crate::gate::{Clock, Gate, Phase};
use crate::roles;
use crate::tree::{Query, Shape, Walk};
use crate::window_server::{owned_by, WindowFacts, Windows};
use crate::wire::{Effect, Failure};

/// How often a wait looks at the tree.
pub const POLL_MS: u64 = 100;

/// A condition's timeout when it names none, and the most it may name.
pub const DEFAULT_UNTIL_MS: u64 = 5_000;
pub const MAX_UNTIL_MS: u64 = 25_000;

/// How many nodes a scan for surfaces reads, at most.
pub const MAX_SURFACE_NODES: usize = 64;

/// Where the menu bar sits: macOS puts it on the application; GTK 3 in a box
/// inside the window and Qt directly inside it, so on Linux a window's children,
/// and the children of a box among them, are looked at for one.
const MENU_BAR_IN_WINDOW: bool = cfg!(target_os = "linux");

/// Whether a menu-bar item that cannot be selected may hold its menu below it. On
/// Linux, Qt's menu-bar item is a plain menu item that never reads SELECTED, and
/// its menu is the popup menu below it, open while that menu reads SHOWING: a menu
/// kept closed says it is hidden there. macOS's menu-bar item is not read for it.
const MENU_BELOW_ITEM: bool = cfg!(target_os = "linux");

/// The AT-SPI name of a menu item that is not itself a menu.
const PLAIN_MENU_ITEM: &str = "menu item";

fn invalid(detail: String) -> Failure {
    Failure::new("invalid_argument", detail)
}

// --- where a walk starts -------------------------------------------------------

/// Where an `elements` walk or a condition starts (PROTOCOL.md §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Window,
    App,
    Focused,
    Menu,
    Dialog,
}

impl Scope {
    pub fn parse(name: &str) -> Option<Scope> {
        match name {
            "window" => Some(Scope::Window),
            "app" => Some(Scope::App),
            "focused" => Some(Scope::Focused),
            "menu" => Some(Scope::Menu),
            "dialog" => Some(Scope::Dialog),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Window => "window",
            Scope::App => "app",
            Scope::Focused => "focused",
            Scope::Menu => "menu",
            Scope::Dialog => "dialog",
        }
    }

    /// The scope a request that names none reads: its bound window, else the
    /// application, which is what protocol 11 read.
    pub fn default_for(scene: &Scene) -> Scope {
        if scene.window.is_some() {
            Scope::Window
        } else {
            Scope::App
        }
    }
}

/// The application a listing or a condition reads, and the bound window when
/// there is one.
#[derive(Clone)]
pub struct Scene {
    pub pid: i32,
    pub window: Option<Bound>,
}

/// A bound window, as a scene reads it: its accessibility element, which the
/// target holds, its id on the window server, and the window server that says
/// which windows are its own.
#[derive(Clone)]
pub struct Bound {
    pub element: Handle,
    pub id: u32,
    pub windows: Arc<dyn Windows>,
}

/// What an `elements` request asks for beyond protocol 11.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub scope: Option<Scope>,
    pub shape: Shape,
    pub root: Option<String>,
}

/// An `elements` request's `scope`, `query`, `skeleton` and `root`, checked.
pub fn listing(body: &Value) -> Result<Listing, Failure> {
    let scope = body
        .get("scope")
        .map(scope_of)
        .transpose()
        .map_err(invalid)?;
    let query = body
        .get("query")
        .map(query_of)
        .transpose()
        .map_err(invalid)?;
    let skeleton = match body.get("skeleton") {
        None => false,
        Some(Value::Bool(skeleton)) => *skeleton,
        Some(other) => return Err(invalid(format!("skeleton is true or false, not {other}"))),
    };
    let root = match body.get("root") {
        None => None,
        Some(Value::String(root)) if !root.is_empty() => Some(root.clone()),
        Some(other) => {
            return Err(invalid(format!(
                "root is a reference like \"e12\", not {other}"
            )))
        }
    };

    if skeleton && query.is_some() {
        return Err(invalid(
            "skeleton lists the outline and query finds nodes in all of it: ask for one"
                .to_string(),
        ));
    }
    if root.is_some() && scope.is_some() {
        return Err(invalid(
            "root drills into a control of a listing, which already says where it is: drop scope"
                .to_string(),
        ));
    }
    if root.is_some() && body.get("observation_id").is_none() {
        return Err(Failure::new(
            "observation_required",
            "root names a control of a listing: send the observation_id that listed it".to_string(),
        ));
    }

    Ok(Listing {
        scope,
        shape: Shape {
            query,
            skeleton,
            list_root: false,
        },
        root,
    })
}

fn scope_of(value: &Value) -> Result<Scope, String> {
    value
        .as_str()
        .and_then(Scope::parse)
        .ok_or_else(|| format!("scope is window, app, focused, menu or dialog, not {value}"))
}

/// A `query` object: `role` and/or `name`, and nothing else.
fn query_of(value: &Value) -> Result<Query, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("query is an object with role and/or name, not {value}"))?;
    if let Some(key) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "role" | "name"))
    {
        return Err(format!("query takes role and name, not {key}"));
    }
    find(object)
}

/// The `role` and `name` of a find, checked: at least one, a role from the table,
/// a name that says something.
fn find(object: &Map<String, Value>) -> Result<Query, String> {
    let role = match object.get("role") {
        None => None,
        Some(Value::String(role)) if roles::ROLES.contains(&role.as_str()) => Some(role.clone()),
        Some(other) => {
            return Err(format!(
                "role is a normalised role such as \"button\", not {other}"
            ))
        }
    };
    let name = match object.get("name") {
        None => None,
        Some(Value::String(name)) if !name.is_empty() => Some(name.clone()),
        Some(other) => return Err(format!("name is a non-empty string, not {other}")),
    };
    if role.is_none() && name.is_none() {
        return Err("a find names a role, a name, or both".to_string());
    }
    Ok(Query { role, name })
}

/// The roots a scope's walk starts from.
pub enum Roots {
    /// The application's own element.
    App,
    /// A window this process already holds: the bound target's.
    Window(Handle),
    /// Roots read for this walk, released with it.
    Held(Vec<Retained>),
}

/// Why a scope has no root.
#[derive(Debug)]
pub enum NoRoot {
    /// The request asks for a scope it may not read.
    Refused(Failure),
    /// Nothing is there to start from: `scope_not_found`, with the sentence it
    /// carries. `unseen` is why the look that found nothing stopped before it
    /// finished, where it did: then nothing is proven missing.
    Missing {
        detail: String,
        unseen: Option<&'static str>,
    },
}

impl From<NoRoot> for Failure {
    fn from(no: NoRoot) -> Failure {
        match no {
            NoRoot::Refused(failure) => failure,
            NoRoot::Missing { detail, .. } => Failure::new("scope_not_found", detail),
        }
    }
}

/// Where a scope starts, or why it cannot.
pub fn roots(ax: &Rc<dyn Ax>, scene: &Scene, scope: Scope) -> Result<Roots, NoRoot> {
    let missing = |what: &str| NoRoot::Missing {
        detail: format!("{what} in the application"),
        unseen: None,
    };
    match scope {
        Scope::App if scene.window.is_some() => Err(NoRoot::Refused(invalid(
            "the app scope lists the whole application, and this request is bound to one window: \
             release the target to list the application"
                .to_string(),
        ))),
        Scope::App => Ok(Roots::App),
        Scope::Window => match &scene.window {
            Some(bound) => Ok(Roots::Window(bound.element)),
            None => ax
                .focused(ax, scene.pid, Focus::Window)
                .map(|window| Roots::Held(vec![window]))
                .ok_or_else(|| missing("there is no focused window")),
        },
        Scope::Focused => focused(ax, scene),
        Scope::Menu => open(ax, scene, &["menu"], "menu"),
        Scope::Dialog => open(ax, scene, &["dialog", "alert"], "dialog"),
    }
}

/// The focused element, as a root: under a binding, only one inside the bound
/// window. Focus in another window of the application is no root, and the
/// sentence says where it is.
fn focused(ax: &Rc<dyn Ax>, scene: &Scene) -> Result<Roots, NoRoot> {
    let found = match &scene.window {
        None => ax.focused(ax, scene.pid, Focus::Element),
        Some(bound) => ax.focused_in(ax, bound.element),
    };
    if let Some(element) = found {
        return Ok(Roots::Held(vec![element]));
    }
    let detail = match &scene.window {
        None => "nothing is focused in the application",
        Some(_) if ax.focused(ax, scene.pid, Focus::Element).is_some() => {
            "focus is in another window of the application, not the bound one: list the bound \
             window (scope window) or a dialog or menu of it (scope dialog, scope menu), or \
             release the target to read the application's focus"
        }
        Some(_) => "nothing is focused in the bound window",
    };
    Err(NoRoot::Missing {
        detail: detail.to_string(),
        unseen: None,
    })
}

/// The surfaces of these kinds the scene sees, as roots. None open is
/// `scope_not_found` naming `what`, proven only by a scan that saw everything.
fn open(ax: &Rc<dyn Ax>, scene: &Scene, kinds: &[&str], what: &str) -> Result<Roots, NoRoot> {
    let seen = surfaces(ax, scene).map_err(NoRoot::Refused)?;
    let held: Vec<Retained> = seen
        .found
        .into_iter()
        .filter(|surface| kinds.contains(&surface.kind))
        .map(|surface| surface.element)
        .collect();
    if !held.is_empty() {
        return Ok(Roots::Held(held));
    }
    let detail = match (seen.stopped, &scene.window) {
        (None, None) => format!("no {what} is open in the application"),
        (None, Some(_)) => format!(
            "no {what} of the bound window is open, and one open in another window of the \
             application is not this window's: release the target to list the application's \
             {what}s"
        ),
        (Some(why), _) => format!(
            "the look for an open {what} stopped before it finished ({}), so whether one is \
             open is not known: look again, or list the window and find its controls there",
            stopped_because(why)
        ),
    };
    Err(NoRoot::Missing {
        detail,
        unseen: seen.stopped,
    })
}

/// Why a scan stopped, in words, from its wire word.
fn stopped_because(word: &str) -> &'static str {
    if word == crate::tree::TRUNCATED_TIME {
        "an element did not answer in time"
    } else {
        "it read as many elements as it may"
    }
}

/// One walk of a scope, listing what `shape` asks for. The focused scope lists
/// its root: what is focused is what the caller asked about.
pub fn walk_scope(
    ax: &Rc<dyn Ax>,
    scene: &Scene,
    scope: Scope,
    shape: &Shape,
    clock: &dyn Clock,
    deadline_ms: u64,
) -> Result<Walk, NoRoot> {
    let shape = Shape {
        list_root: scope == Scope::Focused,
        ..shape.clone()
    };
    Ok(match roots(ax, scene, scope)? {
        Roots::App => crate::tree::walk_app(ax, scene.pid, &shape, clock, deadline_ms),
        Roots::Window(window) => crate::tree::walk_handle(ax, window, &shape, clock, deadline_ms),
        Roots::Held(held) => crate::tree::walk_roots(ax, held, &shape, clock, deadline_ms),
    })
}

// --- surfaces ------------------------------------------------------------------

/// A dialog, alert, menu or popup open in an application (PROTOCOL.md §6.2).
#[derive(Debug)]
pub struct Surface {
    pub kind: &'static str,
    pub native_kind: String,
    pub label: Option<String>,
    pub element: Retained,
}

/// What one scan for surfaces found, and whether it saw everything it looks at.
#[derive(Debug)]
pub struct Surfaces {
    pub found: Vec<Surface>,
    /// Why the scan stopped before it finished: `"time"` (a node did not answer
    /// in time) or `"nodes"` (it read as many nodes as it may). `None` is a scan
    /// that saw everything it looks at, the only one that proves a surface is not
    /// open.
    pub stopped: Option<&'static str>,
}

/// The surfaces a scene sees, the one answer the `menu` and `dialog` scopes, the
/// menu conditions and a receipt all read. Unbound, those open in the application:
/// its windows and their children, and the menu of a menu-bar item that reads
/// selected. Bound, the bound window's own: the window and what is inside it, and
/// what is in a window the window server says is its own, transient for it
/// directly or through a chain ([`MAX_TRANSIENT_HOPS`]). Where the window server
/// marks no window as another's (macOS), that is only what is inside it.
///
/// A scan reads at most [`MAX_SURFACE_NODES`] nodes. A node the platform says is
/// off screen is no surface, and nothing below it is looked at. A node that will
/// not answer is passed over, and one that does not answer in time ends the scan:
/// it is a bounded look, not a census, and what it reports is what it could name,
/// and whether it saw all it looks at.
///
/// [`MAX_TRANSIENT_HOPS`]: crate::window_server::MAX_TRANSIENT_HOPS
pub fn surfaces(ax: &Rc<dyn Ax>, scene: &Scene) -> Result<Surfaces, Failure> {
    let mut scan = Scan {
        ax,
        read: 0,
        stopped: None,
        found: Vec::new(),
    };
    match &scene.window {
        None => scan.application(scene.pid),
        Some(bound) => scan.bound(scene.pid, bound)?,
    }
    Ok(Surfaces {
        found: scan.found,
        stopped: scan.stopped,
    })
}

/// One scan for surfaces in progress.
struct Scan<'a> {
    ax: &'a Rc<dyn Ax>,
    read: usize,
    /// A node did not answer in time (`"time"`), or the bound left something
    /// unread (`"nodes"`): nothing more is asked. A receipt waits on this scan,
    /// and a slow application would hold it for a timeout per node.
    stopped: Option<&'static str>,
    found: Vec<Surface>,
}

impl Scan<'_> {
    /// Every child of the application.
    fn application(&mut self, pid: i32) {
        let Some(app) = self.ax.application(self.ax, pid) else {
            return;
        };
        for child in self.kids(app.handle()) {
            self.top(child);
        }
    }

    /// A bound window's own surfaces: the window read as a child of the
    /// application is, then the children of the application that sit in one of
    /// the windows the window server says are its own.
    fn bound(&mut self, pid: i32, bound: &Bound) -> Result<(), Failure> {
        let family = bound.windows.family(pid).map_err(|why| {
            Failure::new(
                "target_unavailable",
                format!(
                    "the window server would not say which windows are the bound window's \
                     ({why}): release the target to read the application's"
                ),
            )
        })?;
        let window = self
            .ax
            .retain_again(self.ax, bound.element)
            .ok_or_else(|| {
                Failure::new(
                    "target_unavailable",
                    "the bound window is no longer held: select the target again".to_string(),
                )
            })?;
        self.top(window);
        let owned = owned_by(&family, bound.id);
        if owned.is_empty() {
            return Ok(());
        }
        let Some(app) = self.ax.application(self.ax, pid) else {
            return Ok(());
        };
        for child in self.kids(app.handle()) {
            self.owned_top(child, &family, &owned);
        }
        Ok(())
    }

    /// Stop asking anything, for the first reason met.
    fn stop(&mut self, why: &'static str) {
        self.stopped.get_or_insert(why);
    }

    /// Count one node read against the bound; `false` once the scan has stopped.
    fn counted(&mut self) -> bool {
        if self.read >= MAX_SURFACE_NODES {
            self.stop(crate::tree::TRUNCATED_NODES);
        }
        if self.stopped.is_some() {
            return false;
        }
        self.read += 1;
        true
    }

    /// Note why a read answered nothing: only a slow application stops the scan.
    fn unread(&mut self, why: Unread) {
        if why == Unread::NoAnswer {
            self.stop(crate::tree::TRUNCATED_TIME);
        }
    }

    fn facts(&mut self, handle: Handle) -> Option<Facts> {
        if !self.counted() {
            return None;
        }
        self.ax.facts(handle).map_err(|why| self.unread(why)).ok()
    }

    /// As many of a node's children as the bound has room for. One with more than
    /// that leaves the scan unfinished.
    fn kids(&mut self, handle: Handle) -> Vec<Retained> {
        if self.stopped.is_some() {
            return Vec::new();
        }
        let room = MAX_SURFACE_NODES - self.read;
        match kids(self.ax, handle, room) {
            Ok((kids, cut)) => {
                if cut {
                    self.stop(crate::tree::TRUNCATED_NODES);
                }
                kids
            }
            Err(why) => {
                self.unread(why);
                Vec::new()
            }
        }
    }

    /// A child of the application: the menu bar, a surface, or a window whose
    /// children may be. Held while its children are read through it.
    fn top(&mut self, node: Retained) {
        let Some(facts) = self.facts(node.handle()).filter(|facts| !facts.hidden) else {
            return;
        };
        self.top_read(&facts, node);
    }

    /// A child of the application read as [`Scan::top`] reads it, when its frame
    /// ties it to one of the bound window's own windows (`owned`, out of the
    /// application's windows `family`).
    fn owned_top(&mut self, node: Retained, family: &[WindowFacts], owned: &[u32]) {
        let Some(facts) = self.facts(node.handle()).filter(|facts| !facts.hidden) else {
            return;
        };
        if facts
            .frame
            .is_some_and(|frame| crate::target::tied(frame, family, owned))
        {
            self.top_read(&facts, node);
        }
    }

    /// What [`Scan::top`] does with a child of the application once read.
    fn top_read(&mut self, facts: &Facts, node: Retained) {
        let handle = node.handle();
        if is_menu_bar(facts) {
            return self.menu_bar(handle);
        }
        let Some(_held) = self.keep(facts, node) else {
            return;
        };
        for child in self.kids(handle) {
            self.in_window(child);
        }
    }

    /// A window's child: a surface, or on Linux the menu bar, or a box that may
    /// hold it.
    fn in_window(&mut self, node: Retained) {
        let handle = node.handle();
        let Some(facts) = self.facts(handle).filter(|facts| !facts.hidden) else {
            return;
        };
        if MENU_BAR_IN_WINDOW && is_menu_bar(&facts) {
            return self.menu_bar(handle);
        }
        let Some(_held) = self.keep(&facts, node) else {
            return;
        };
        if !MENU_BAR_IN_WINDOW || kind_of(&facts) != "group" {
            return;
        }
        for child in self.kids(handle) {
            self.menu_bar_in(child);
        }
    }

    /// A box's child, read only to find the menu bar.
    fn menu_bar_in(&mut self, node: Retained) {
        let handle = node.handle();
        if self
            .facts(handle)
            .is_some_and(|facts| !facts.hidden && is_menu_bar(&facts))
        {
            self.menu_bar(handle);
        }
    }

    fn menu_bar(&mut self, bar: Handle) {
        for item in self.kids(bar) {
            self.menu_of(item);
        }
    }

    /// A menu-bar item's menu is its child whether or not it is open, so an item
    /// that can be selected has one open only while it reads selected. One that
    /// cannot be selected may hold a menu that shows while open.
    fn menu_of(&mut self, item: Retained) {
        if !self.counted() {
            return;
        }
        let selected = match self.ax.states(item.handle(), false) {
            Ok(states) => states.selected,
            Err(why) => return self.unread(why),
        };
        match selected {
            Some(true) => self.selected_menu(item),
            Some(false) => {}
            None if MENU_BELOW_ITEM => self.menu_below(item),
            None => {}
        }
    }

    /// A selected item's menu: the item itself (GTK 3), else its child.
    fn selected_menu(&mut self, item: Retained) {
        let Some(item) = self.surface(item) else {
            return;
        };
        for menu in self.kids(item.handle()) {
            self.surface(menu);
        }
    }

    /// A plain menu item's menu, kept only while it shows (Qt). Nothing below an
    /// item of any other role is read: a GTK 3 menu's children are its items.
    fn menu_below(&mut self, item: Retained) {
        let plain = self
            .facts(item.handle())
            .is_some_and(|facts| facts.role.as_deref() == Some(PLAIN_MENU_ITEM));
        if !plain {
            return;
        }
        for menu in self.kids(item.handle()) {
            self.surface(menu);
        }
    }

    /// Read a node and keep it if it is a surface. One that is not is handed back,
    /// and one that does not answer is given back.
    fn surface(&mut self, node: Retained) -> Option<Retained> {
        let facts = self.facts(node.handle()).filter(|facts| !facts.hidden)?;
        self.keep(&facts, node)
    }

    /// Keep a node that is a surface; give back one that is not.
    fn keep(&mut self, facts: &Facts, node: Retained) -> Option<Retained> {
        let role = facts.role.as_deref().unwrap_or("");
        let Some((kind, native_kind)) = roles::surface(role, facts.subrole.as_deref()) else {
            return Some(node);
        };
        self.found.push(Surface {
            kind,
            native_kind,
            label: facts.label.clone(),
            element: node,
        });
        None
    }
}

fn kind_of(facts: &Facts) -> &'static str {
    roles::normalise(
        facts.role.as_deref().unwrap_or(""),
        facts.subrole.as_deref(),
    )
}

fn is_menu_bar(facts: &Facts) -> bool {
    kind_of(facts) == "menubar"
}

/// Up to `limit` of a node's children, by count or in one copy where it will not
/// count them, and whether it may have more than that: a node with no room left
/// is not asked, and a copy that filled its room may have been cut.
fn kids(ax: &Rc<dyn Ax>, handle: Handle, limit: usize) -> Result<(Vec<Retained>, bool), Unread> {
    if limit == 0 {
        return Ok((Vec::new(), true));
    }
    match ax.child_count(handle) {
        Ok(0) => Ok((Vec::new(), false)),
        Ok(count) => Ok((ax.children(ax, handle, 0, count.min(limit))?, count > limit)),
        Err(Unread::Refused) => {
            let copy = ax.children_uncounted(ax, handle, limit)?;
            let full = copy.len() >= limit;
            Ok((copy, full))
        }
        Err(why) => Err(why),
    }
}

// --- conditions ----------------------------------------------------------------

/// What a wait waits for (PROTOCOL.md §5.1). `text` is a find by name, so it is an
/// [`Condition::Appears`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    Appears {
        query: Query,
        scope: Option<Scope>,
    },
    DisappearsElement(String),
    Disappears {
        query: Query,
        scope: Option<Scope>,
    },
    Enabled(String),
    Value {
        element: String,
        value: String,
    },
    WindowTitle {
        contains: String,
        scope: Option<Scope>,
    },
    MenuOpen,
    MenuClosed,
}

impl Condition {
    /// Whether it reads a control of the request's own listing.
    fn names_an_element(&self) -> bool {
        matches!(
            self,
            Condition::DisappearsElement(_) | Condition::Enabled(_) | Condition::Value { .. }
        )
    }
}

/// A condition and how long it may take to hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Until {
    pub condition: Condition,
    pub timeout_ms: u64,
}

/// An `until` object, checked. `observation` is whether the request names the
/// listing an `element_ref` condition reads its control from. The error is the
/// sentence an `invalid_argument` carries.
pub fn parse_until(value: &Value, observation: bool) -> Result<Until, String> {
    let object = value.as_object().ok_or_else(|| {
        format!("until is an object such as {{\"kind\": \"appears\", …}}, not {value}")
    })?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "until needs a kind".to_string())?;
    let by_reference = object.contains_key("element_ref");
    let takes: &[&str] = match kind {
        "appears" => &["role", "name", "scope"],
        "disappears" if by_reference => &["element_ref"],
        "disappears" => &["role", "name", "scope"],
        "enabled" => &["element_ref"],
        "value" => &["element_ref", "value"],
        "window_title" | "text" => &["contains", "scope"],
        "menu_open" | "menu_closed" => &[],
        other => return Err(format!("{other} is not a condition this helper reads")),
    };
    let stray = object.keys().find(|key| {
        !matches!(key.as_str(), "kind" | "timeout_ms") && !takes.contains(&key.as_str())
    });
    if let Some(key) = stray {
        return Err(format!("a {kind} condition does not take {key}"));
    }

    let condition = condition(kind, object)?;
    if condition.names_an_element() && !observation {
        return Err(format!(
            "a {kind} condition names a control of the listing the request's observation_id names, \
             and this request names none"
        ));
    }
    Ok(Until {
        condition,
        timeout_ms: timeout_of(object.get("timeout_ms"))?,
    })
}

/// The condition a checked `kind` names, read off its own fields.
fn condition(kind: &str, object: &Map<String, Value>) -> Result<Condition, String> {
    let scope = || object.get("scope").map(scope_of).transpose();
    let text = |key: &str| match object.get(key) {
        Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
        other => Err(format!(
            "a {kind} condition needs {key}: a non-empty string, not {other:?}"
        )),
    };
    Ok(match kind {
        "appears" => Condition::Appears {
            query: find(object)?,
            scope: scope()?,
        },
        "disappears" if object.contains_key("element_ref") => {
            Condition::DisappearsElement(text("element_ref")?)
        }
        "disappears" => Condition::Disappears {
            query: find(object)?,
            scope: scope()?,
        },
        "enabled" => Condition::Enabled(text("element_ref")?),
        "value" => Condition::Value {
            element: text("element_ref")?,
            value: match object.get("value") {
                Some(Value::String(value)) => value.clone(),
                other => {
                    return Err(format!(
                        "a value condition needs value: a string, not {other:?}"
                    ))
                }
            },
        },
        "window_title" => Condition::WindowTitle {
            contains: text("contains")?,
            scope: scope()?,
        },
        "text" => Condition::Appears {
            query: Query {
                role: None,
                name: Some(text("contains")?),
            },
            scope: scope()?,
        },
        "menu_open" => Condition::MenuOpen,
        _ => Condition::MenuClosed,
    })
}

fn timeout_of(value: Option<&Value>) -> Result<u64, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_UNTIL_MS);
    };
    match value.as_u64() {
        Some(ms) if (1..=MAX_UNTIL_MS).contains(&ms) => Ok(ms),
        _ => Err(format!("timeout_ms is 1 to {MAX_UNTIL_MS}, not {value}")),
    }
}

/// What one look saw: the condition holds, with what matched when it lists one, or
/// it does not.
#[derive(Debug)]
pub enum Seen {
    Holds(Option<Walk>),
    Not,
    /// Nothing it waits to be gone was seen, and the look stopped before it saw
    /// everything, so the absence is not proven: the walk's word for why
    /// (`"nodes"`, `"depth"`, `"time"`). It does not hold.
    Unproven(&'static str),
}

/// What a look reads with.
pub struct Looking<'a> {
    pub ax: &'a Rc<dyn Ax>,
    pub scene: Scene,
    /// The listing an `element_ref` condition names its control in.
    pub named: Option<&'a Elements>,
    pub clock: &'a dyn Clock,
}

/// One look at the tree. It sends no input.
pub fn look(at: &Looking<'_>, condition: &Condition, deadline_ms: u64) -> Result<Seen, Failure> {
    let finding = |query: &Query, scope: &Option<Scope>| {
        let shape = Shape {
            query: Some(query.clone()),
            ..Shape::default()
        };
        walk_in(
            at,
            scope.unwrap_or(Scope::default_for(&at.scene)),
            &shape,
            deadline_ms,
        )
    };
    match condition {
        Condition::Appears { query, scope } => Ok(match finding(query, scope)? {
            Scoped::Open(found) if !found.nodes.is_empty() => Seen::Holds(Some(found)),
            _ => Seen::Not,
        }),
        Condition::Disappears { query, scope } => Ok(match finding(query, scope)? {
            Scoped::Open(found) if !found.nodes.is_empty() => Seen::Not,
            Scoped::Open(found) => absent(found.truncated),
            Scoped::Missing(unseen) => absent(unseen),
        }),
        Condition::DisappearsElement(reference) => {
            let (named, entry) = named_entry(at, reference)?;
            let gone = matches!(answer(at.ax, named, entry), Err(Unread::Gone));
            Ok(holds(gone))
        }
        Condition::Enabled(reference) => {
            let (named, entry) = named_entry(at, reference)?;
            let enabled = answer(at.ax, named, entry).is_ok_and(|facts| facts.enabled);
            Ok(holds(enabled))
        }
        Condition::Value { element, value } => value_holds(at, element, value),
        Condition::WindowTitle { contains, .. } => Ok(holds(titled(at, contains))),
        Condition::MenuOpen => Ok(
            match walk_in(at, Scope::Menu, &Shape::default(), deadline_ms)? {
                Scoped::Open(found) => Seen::Holds(Some(found)),
                Scoped::Missing(_) => Seen::Not,
            },
        ),
        Condition::MenuClosed => {
            let seen = surfaces(at.ax, &at.scene)?;
            let open = seen.found.iter().any(|surface| surface.kind == "menu");
            Ok(if open {
                Seen::Not
            } else {
                absent(seen.stopped)
            })
        }
    }
}

fn holds(yes: bool) -> Seen {
    if yes {
        Seen::Holds(None)
    } else {
        Seen::Not
    }
}

/// What a look that found nothing proves of an absence: that it holds when the
/// look saw everything, and nothing when it stopped short.
fn absent(truncated: Option<&'static str>) -> Seen {
    match truncated {
        None => Seen::Holds(None),
        Some(word) => Seen::Unproven(word),
    }
}

/// What a condition's walk of a scope found.
enum Scoped {
    /// The scope is open, and this is what its walk listed.
    Open(Walk),
    /// The scope has no root, and why the look that found none stopped short
    /// where it did.
    Missing(Option<&'static str>),
}

/// A walk of a scope for a condition. A scope that is not open is read as empty,
/// never refused: waiting for a dialog's button before the dialog exists is the
/// ordinary case. One a look stopped short of is not proven empty.
fn walk_in(
    at: &Looking<'_>,
    scope: Scope,
    shape: &Shape,
    deadline_ms: u64,
) -> Result<Scoped, Failure> {
    match walk_scope(at.ax, &at.scene, scope, shape, at.clock, deadline_ms) {
        Ok(found) => Ok(Scoped::Open(found)),
        Err(NoRoot::Missing { unseen, .. }) => Ok(Scoped::Missing(unseen)),
        Err(NoRoot::Refused(failure)) => Err(failure),
    }
}

/// What a listed control answers now. A process that quit, or whose pid now
/// names another, answers nothing: the control is gone.
fn answer(ax: &Rc<dyn Ax>, named: &Elements, entry: &Entry) -> Result<Facts, Unread> {
    if ax.process_started_at(named.pid) != Some(named.started_at) {
        return Err(Unread::Gone);
    }
    ax.facts(entry.element.handle())
}

/// The control a condition names, in the request's own listing. The request
/// names an observation ([`parse_until`] refuses one that does not), so no listing
/// here is an observation that listed no controls — a screenshot's — and the
/// reference names nothing in it.
fn named_entry<'a>(
    at: &Looking<'a>,
    reference: &str,
) -> Result<(&'a Elements, &'a Entry), Failure> {
    let named = at.named.ok_or_else(|| {
        Failure::new(
            "stale_element",
            format!(
                "the observation this request names listed no controls, so it has no \
                 {reference}: list them with elements"
            ),
        )
    })?;
    let entry = named.get(reference).ok_or_else(|| {
        Failure::new(
            "stale_element",
            format!("the listing this request names has no {reference}: list the controls again"),
        )
    })?;
    Ok((named, entry))
}

/// A value condition. Refused on a secure field, by the listing's word or by what
/// it says now, before its value is ever asked for: whether a guess was right is
/// itself the secret.
fn value_holds(at: &Looking<'_>, reference: &str, wanted: &str) -> Result<Seen, Failure> {
    let (named, entry) = named_entry(at, reference)?;
    let facts = answer(at.ax, named, entry);
    if entry.secure || facts.as_ref().is_ok_and(|facts| facts.secure) {
        return Err(invalid(format!(
            "{reference} is a secure field: a value condition on it would say whether a guess was right"
        )));
    }
    if facts.is_err() {
        return Ok(Seen::Not);
    }
    let value = at.ax.value(entry.element.handle());
    Ok(holds(
        value.as_deref() == Some(crate::ax::bounded_value(wanted.to_string()).as_str()),
    ))
}

/// Whether the bound window's title, else the focused window's, contains `text`,
/// ignoring case. No window is no title.
fn titled(at: &Looking<'_>, text: &str) -> bool {
    let focused;
    let window = match &at.scene.window {
        Some(bound) => bound.element,
        None => {
            focused = at.ax.focused(at.ax, at.scene.pid, Focus::Window);
            let Some(window) = focused.as_ref() else {
                return false;
            };
            window.handle()
        }
    };
    let title = at.ax.facts(window).ok().and_then(|facts| facts.label);
    title.is_some_and(|title| title.to_lowercase().contains(&text.to_lowercase()))
}

/// What a wait came to.
#[derive(Debug)]
pub struct Waited {
    pub met: bool,
    pub looks: u64,
    pub waited_ms: u64,
    pub found: Option<Walk>,
    /// A wait for an absence that ended not met with its last look cut short:
    /// why that look stopped. The absence may hold beyond what it saw.
    pub truncated: Option<&'static str>,
}

/// Look until the condition holds or `timeout_ms` passes: a look every
/// [`POLL_MS`], at most `timeout_ms / POLL_MS + 1` of them, each walk bounded by
/// its own budget. A pause ends it at once (`cancelled`). It never sends input.
///
/// A look that is slow counts against the timeout: the sleep after it is what is
/// left of its poll, so a wait never runs a full poll on top of a long look.
pub fn wait(
    gate: &Gate,
    timeout_ms: u64,
    mut look: impl FnMut(u64) -> Result<Seen, Failure>,
) -> Result<Waited, Failure> {
    let started = gate.now_ms();
    let most = timeout_ms / POLL_MS + 1;
    let mut looks = 0;
    loop {
        let at = gate.now_ms();
        looks += 1;
        let seen = look(at.saturating_add(crate::tree::WALK_BUDGET_MS))?;
        let waited_ms = gate.now_ms().saturating_sub(started);
        let truncated = match seen {
            Seen::Holds(found) => {
                return Ok(Waited {
                    met: true,
                    looks,
                    waited_ms,
                    found,
                    truncated: None,
                })
            }
            Seen::Not => None,
            Seen::Unproven(word) => Some(word),
        };
        if looks >= most || waited_ms >= timeout_ms {
            return Ok(Waited {
                met: false,
                looks,
                waited_ms,
                found: None,
                truncated,
            });
        }
        let spent = gate.now_ms().saturating_sub(at);
        gate.sleep(POLL_MS.saturating_sub(spent))
            .map_err(|refusal| Failure::new(refusal.code(), refusal.detail().to_string()))?;
    }
}

/// Whether the condition already holds, read BEFORE an action's input (§5.2): one
/// that held before cannot show what the action did. Its look is held to the
/// request's cutoff, as the wait after the input is.
pub fn held_before(
    at: &Looking<'_>,
    until: &Until,
    now_ms: u64,
    cutoff_ms: Option<u64>,
) -> Result<bool, Failure> {
    let deadline = now_ms
        .saturating_add(crate::tree::WALK_BUDGET_MS)
        .min(cutoff_ms.unwrap_or(u64::MAX));
    let seen = look(at, &until.condition, deadline)?;
    Ok(matches!(seen, Seen::Holds(_)))
}

/// Look until the condition holds, for no longer than its timeout or what is left
/// before the request's cutoff, whichever is less; each look's walk is held to the
/// cutoff too. `true` beside the answer when the cutoff is what ended it.
pub fn wait_until(
    gate: &Gate,
    at: &Looking<'_>,
    until: &Until,
    cutoff_ms: Option<u64>,
) -> Result<(Waited, bool), Failure> {
    let cutoff = cutoff_ms.unwrap_or(u64::MAX);
    let timeout_ms = until.timeout_ms.min(cutoff.saturating_sub(gate.now_ms()));
    let waited = wait(gate, timeout_ms, |deadline| {
        look(at, &until.condition, deadline.min(cutoff))
    })?;
    let cut = !waited.met && timeout_ms < until.timeout_ms;
    Ok((waited, cut))
}

// --- the verbs on a reference --------------------------------------------------

/// A verb on a reference, with its argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verb {
    Focus,
    Select,
    Toggle(bool),
    Expand,
    Collapse,
    ScrollTo,
    Type(String),
}

impl Verb {
    /// The verb a request names, with its argument read off the body.
    pub fn from_request(action: &str, body: &Value) -> Result<Verb, Failure> {
        match action {
            "focus" => Ok(Verb::Focus),
            "select" => Ok(Verb::Select),
            "expand" => Ok(Verb::Expand),
            "collapse" => Ok(Verb::Collapse),
            "scroll_to" => Ok(Verb::ScrollTo),
            "toggle" => match body.get("on") {
                Some(Value::Bool(on)) => Ok(Verb::Toggle(*on)),
                _ => Err(invalid(
                    "toggle needs on: true or false, the state the control should end in"
                        .to_string(),
                )),
            },
            "type" => match body.get("text") {
                Some(Value::String(text)) if !text.is_empty() => Ok(Verb::Type(text.clone())),
                _ => Err(invalid("type needs text: a non-empty string".to_string())),
            },
            other => Err(Failure::new(
                "unknown_action",
                format!("{other} is not a verb on a reference"),
            )),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Verb::Focus => "focus",
            Verb::Select => "select",
            Verb::Toggle(_) => "toggle",
            Verb::Expand => "expand",
            Verb::Collapse => "collapse",
            Verb::ScrollTo => "scroll_to",
            Verb::Type(_) => "type",
        }
    }
}

/// Which state a verb ends in, and the words a receipt names it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateOf {
    Focused,
    Selected,
    /// `AXExpanded` or `AXDisclosing`.
    Expanded,
    /// A check state.
    On,
    /// A disclosure's value, named as expanded or collapsed.
    Disclosed,
}

impl StateOf {
    /// The word a receipt names a reading by.
    fn word(self, yes: bool) -> &'static str {
        match (self, yes) {
            (StateOf::Focused, true) => "focused",
            (StateOf::Focused, false) => "unfocused",
            (StateOf::Selected, true) => "selected",
            (StateOf::Selected, false) => "unselected",
            (StateOf::Expanded | StateOf::Disclosed, true) => "expanded",
            (StateOf::Expanded | StateOf::Disclosed, false) => "collapsed",
            (StateOf::On, true) => "on",
            (StateOf::On, false) => "off",
        }
    }

    /// This state as the control reads it now; `None` when it will not say.
    fn read(self, ax: &Rc<dyn Ax>, handle: Handle) -> Option<bool> {
        let on = matches!(self, StateOf::On | StateOf::Disclosed);
        let states = ax.states(handle, on).ok()?;
        match self {
            StateOf::Focused => states.focused,
            StateOf::Selected => states.selected,
            StateOf::Expanded => states.expanded,
            StateOf::On | StateOf::Disclosed => states.on,
        }
    }
}

/// The one message a verb sends.
#[derive(Debug)]
pub enum Op {
    Focused,
    Selected,
    /// `AXSelectedChildren` on the container, which is held here.
    SelectedChildren(Retained),
    Expand(Expander, bool),
    Text(String),
    Press,
    ScrollToVisible,
}

/// What a verb will send and what it read first.
#[derive(Debug)]
pub struct Plan {
    pub op: Op,
    /// The state it is to end in and the state it read: `(which, wanted, was)`.
    pub state: Option<(StateOf, bool, bool)>,
    /// Where it was, for `scroll_to`.
    pub frame: Option<Frame>,
    /// What it held, for `type`; never read of a secure field.
    pub value: Option<String>,
    pub secure: bool,
}

impl Plan {
    /// The state already holds, so nothing is to be sent.
    pub fn already(&self) -> bool {
        self.state.is_some_and(|(_, wanted, was)| wanted == was)
    }

    fn of(op: Op) -> Plan {
        Plan {
            op,
            state: None,
            frame: None,
            value: None,
            secure: false,
        }
    }
}

/// Prove the control has a route for this verb and read what it holds first.
/// Every refusal here is before anything is sent. `pid` is the application that
/// listed the control: `focus` refuses unless it is the one in front and the
/// control's window is its focused one.
pub fn plan(
    ax: &Rc<dyn Ax>,
    handle: Handle,
    pid: i32,
    facts: &Facts,
    secure: bool,
    verb: &Verb,
) -> Result<Plan, Failure> {
    let kind = roles::normalise(
        facts.role.as_deref().unwrap_or(""),
        facts.subrole.as_deref(),
    );
    let (op, state) = routed(ax, handle, pid, facts, kind, verb)?;
    let state = match state {
        None => None,
        Some((which, wanted)) => {
            Some((which, wanted, state_before(ax, handle, kind, verb, which)?))
        }
    };

    Ok(Plan {
        state,
        frame: facts.frame,
        // The value is read for `type` alone, and never of a secure field.
        value: match (&op, secure) {
            (Op::Text(_), false) => ax.value(handle),
            _ => None,
        },
        secure,
        ..Plan::of(op)
    })
}

/// The one message a verb sends on this control, and the state it is to end in.
/// Only the route the verb takes is asked: on macOS each is a message of its own.
fn routed(
    ax: &Rc<dyn Ax>,
    handle: Handle,
    pid: i32,
    facts: &Facts,
    kind: &str,
    verb: &Verb,
) -> Result<(Op, Option<(StateOf, bool)>), Failure> {
    let advertises = |name: &str| facts.actions.iter().any(|action| action == name);
    let unsupported = || {
        Failure::new(
            "ax_action_unsupported",
            format!(
                "this {kind} has no route for {}: nothing was sent, and nothing is clicked instead",
                verb.name()
            ),
        )
    };
    let takes = |route: Route| ax.route(handle, route);

    match verb {
        Verb::Focus if takes(Route::Focus).focus => {
            focusable(ax, handle, pid)?;
            Ok((Op::Focused, Some((StateOf::Focused, true))))
        }
        Verb::Select if takes(Route::Select).select => {
            Ok((Op::Selected, Some((StateOf::Selected, true))))
        }
        Verb::Select => {
            let container = selecting_container(ax, handle).ok_or_else(unsupported)?;
            Ok((
                Op::SelectedChildren(container),
                Some((StateOf::Selected, true)),
            ))
        }
        Verb::Toggle(on) if roles::has_check_state(kind) && advertises(PRESS) => {
            Ok((Op::Press, Some((StateOf::On, *on))))
        }
        Verb::Expand | Verb::Collapse => {
            let wanted = *verb == Verb::Expand;
            match takes(Route::Expand).expand {
                Some(expander) => Ok((
                    Op::Expand(expander, wanted),
                    Some((StateOf::Expanded, wanted)),
                )),
                None if kind == "disclosure" && advertises(PRESS) => {
                    Ok((Op::Press, Some((StateOf::Disclosed, wanted))))
                }
                None => Err(unsupported()),
            }
        }
        Verb::ScrollTo if advertises(SCROLL_TO_VISIBLE) => Ok((Op::ScrollToVisible, None)),
        Verb::Type(text) if takes(Route::Text).text => Ok((Op::Text(text.clone()), None)),
        _ => Err(unsupported()),
    }
}

/// The state a verb is to end in, as the control reads it before anything is
/// sent. One that will not say refuses the verb: it could not decide whether to
/// act.
fn state_before(
    ax: &Rc<dyn Ax>,
    handle: Handle,
    kind: &str,
    verb: &Verb,
    which: StateOf,
) -> Result<bool, Failure> {
    which.read(ax, handle).ok_or_else(|| {
        Failure::new(
            "state_unreadable",
            format!(
                "this {kind} would not say whether it is {}, so {} could not decide whether to \
                 act: nothing was sent",
                which.word(true),
                verb.name()
            ),
        )
    })
}

/// Focus moves the keyboard inside a window that is already active. The
/// control's application must be the one in front and its window the focused
/// one; a window that will not say is never read as yes.
fn focusable(ax: &Rc<dyn Ax>, handle: Handle, pid: i32) -> Result<(), Failure> {
    let front = ax.frontmost_pid() == Some(pid);
    if front && ax.in_focused_window(handle) == Some(true) {
        return Ok(());
    }
    Err(Failure::new(
        "focus_refused",
        "focus never activates a window: this control's window is not the active one, so nothing \
         was sent"
            .to_string(),
    ))
}

/// The container that selects this item for it, held for the write.
fn selecting_container(ax: &Rc<dyn Ax>, handle: Handle) -> Option<Retained> {
    let container = ax.parent(ax, handle)?;
    ax.selects_children(container.handle()).then_some(container)
}

/// The one message.
pub fn send(
    ax: &Rc<dyn Ax>,
    handle: Handle,
    plan: &Plan,
    timeout_ms: u64,
) -> Result<(), crate::ax::Refusal> {
    match &plan.op {
        Op::Focused => ax.write(handle, Write::Focused, timeout_ms),
        Op::Selected => ax.write(handle, Write::Selected, timeout_ms),
        Op::SelectedChildren(container) => ax.write(
            container.handle(),
            Write::SelectedChildren(handle),
            timeout_ms,
        ),
        Op::Expand(expander, open) => ax.write(handle, Write::Expand(*expander, *open), timeout_ms),
        Op::Text(text) => ax.write(handle, Write::SelectedText(text), timeout_ms),
        Op::Press => ax.perform(handle, PRESS, timeout_ms),
        Op::ScrollToVisible => ax.perform(handle, SCROLL_TO_VISIBLE, timeout_ms),
    }
}

/// What the read-back proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judged {
    pub effect: Effect,
    pub already: bool,
    pub state_before: Option<&'static str>,
    pub state_after: Option<&'static str>,
    /// What a field holds after `type`; never for a secure one.
    pub value: Option<String>,
}

/// Read back what the verb did, polling briefly for an application that shows it
/// a moment late. Infallible, like every step between the latch and the check: a
/// pause ends the polling and the last read is the answer.
pub fn judge(gate: &Gate, ax: &Rc<dyn Ax>, handle: Handle, plan: &Plan) -> Judged {
    let unread = Judged {
        effect: Effect::Unknown,
        already: false,
        state_before: None,
        state_after: None,
        value: None,
    };
    if let Some((which, wanted, was)) = plan.state {
        let after = poll(gate, || which.read(ax, handle), |now| *now == Some(wanted));
        return Judged {
            effect: compared(after.map(|after| after == wanted)),
            state_before: Some(which.word(was)),
            state_after: after.map(|after| which.word(after)),
            ..unread
        };
    }
    match plan.op {
        Op::ScrollToVisible => {
            let frame = |now: &Option<Frame>| now.is_some() && *now != plan.frame;
            let after = poll(
                gate,
                || ax.facts(handle).ok().and_then(|facts| facts.frame),
                frame,
            );
            Judged {
                effect: compared(after.map(|after| Some(after) != plan.frame)),
                ..unread
            }
        }
        // A secure field is never read back: nothing it holds is asked for.
        Op::Text(_) if plan.secure => unread,
        Op::Text(_) => {
            let before = plan.value.clone();
            let after = poll(
                gate,
                || ax.value(handle),
                |now| now.is_some() && *now != before,
            );
            // A field that would not say what it held cannot show a change (§4.7).
            // On macOS an empty field answers "", so `None` is never "it was empty".
            let shows = match (&before, &after) {
                (Some(before), Some(after)) => Some(after != before),
                _ => None,
            };
            Judged {
                effect: compared(shows),
                value: after,
                ..unread
            }
        }
        _ => unread,
    }
}

/// `changed` for a read-back that shows what was asked, `not_observed` for one that
/// does not, `unknown` for one that could not be read.
fn compared(shows: Option<bool>) -> Effect {
    match shows {
        Some(true) => Effect::Changed,
        Some(false) => Effect::NotObserved,
        None => Effect::Unknown,
    }
}

/// Read until `done` or the read-back cap; the last read is the answer. The
/// waits go through the gate, so a pause ends it at once.
fn poll<T>(gate: &Gate, mut read: impl FnMut() -> T, done: impl Fn(&T) -> bool) -> T {
    let mut now = read();
    let mut waited = 0;
    while !done(&now) && waited < crate::READ_BACK_CAP_MS {
        let started = gate.now_ms();
        let paused = gate.sleep(crate::READ_BACK_POLL_MS).is_err();
        gate.record(Phase::Settle, gate.now_ms().saturating_sub(started));
        if paused {
            break;
        }
        waited += crate::READ_BACK_POLL_MS;
        now = read();
    }
    now
}

/// What a verb whose state already held reports: nothing sent, the state as read.
pub fn held(plan: &Plan) -> Judged {
    let words = plan.state.map(|(which, _, was)| which.word(was));
    Judged {
        effect: Effect::NotObserved,
        already: true,
        state_before: words,
        state_after: words,
        value: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::{Focus, Recorder, Routes, Scripted, States, Unread, PRESS};
    use crate::gate::SystemClock;
    use crate::window_server::{Bounds, Recorder as WindowRecorder, WindowFacts};
    use crate::wire::ControlAction;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    const PID: i32 = 4711;

    /// A clock a test moves by hand: a wait is asserted, never waited for.
    struct TestClock {
        ms: Mutex<u64>,
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

    fn test_gate() -> Gate {
        Gate::new(
            "boot-test".to_string(),
            Arc::new(TestClock { ms: Mutex::new(0) }),
        )
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

    fn app_scene() -> Scene {
        Scene {
            pid: PID,
            window: None,
        }
    }

    /// The surfaces open in the application, as an unbound scene sees them.
    fn every(ax: &Rc<dyn Ax>) -> Surfaces {
        surfaces(ax, &app_scene()).expect("an unbound scan is never refused")
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

    fn subroled(role: &str, subrole: &str, label: &str, children: &[usize]) -> Scripted {
        Scripted {
            subrole: Some(subrole.to_string()),
            ..holding(role, label, children)
        }
    }

    fn labels(found: &Walk) -> Vec<&str> {
        found
            .nodes
            .iter()
            .map(|node| node.label.as_deref().unwrap_or("-"))
            .collect()
    }

    fn whole(ax: &Rc<dyn Ax>, scene: &Scene, scope: Scope) -> Result<Walk, Failure> {
        walk_scope(
            ax,
            scene,
            scope,
            &Shape::default(),
            &SystemClock::new(),
            u64::MAX,
        )
        .map_err(Failure::from)
    }

    // --- listings ------------------------------------------------------------------

    #[test]
    fn every_scope_parses_and_spells_itself() {
        for name in ["window", "app", "focused", "menu", "dialog"] {
            let scope = Scope::parse(name).expect(name);
            assert_eq!(scope.as_str(), name);
        }
        assert_eq!(Scope::parse("desktop"), None);
    }

    // Absent, every new parameter is absent: a protocol-11 request reads what it
    // always read.
    #[test]
    fn a_listing_with_none_of_the_new_parameters_is_protocol_eleven() {
        assert_eq!(
            listing(&json!({"action": "elements"})),
            Ok(Listing::default())
        );
    }

    #[test]
    fn a_listing_reads_its_scope_query_skeleton_and_root() {
        let found = listing(&json!({
            "scope": "dialog",
            "query": {"role": "button", "name": "Save"}
        }))
        .expect("a valid listing");
        assert_eq!(found.scope, Some(Scope::Dialog));
        assert_eq!(
            found.shape.query,
            Some(Query {
                role: Some("button".to_string()),
                name: Some("Save".to_string())
            })
        );

        let cut = listing(&json!({"skeleton": true})).expect("a skeleton");
        assert!(cut.shape.skeleton);

        let drill = listing(&json!({"root": "e12", "observation_id": "o-1"})).expect("a drill");
        assert_eq!(drill.root.as_deref(), Some("e12"));
    }

    // Every combination the walk cannot honour is refused by name before anything
    // is read.
    #[test]
    fn a_listing_refuses_what_it_cannot_honour() {
        for (body, code) in [
            (json!({"scope": "desktop"}), "invalid_argument"),
            (json!({"query": {}}), "invalid_argument"),
            (json!({"query": {"role": "AXButton"}}), "invalid_argument"),
            (json!({"query": {"name": ""}}), "invalid_argument"),
            (
                json!({"query": {"role": "button", "label": "x"}}),
                "invalid_argument",
            ),
            (json!({"query": "button"}), "invalid_argument"),
            (json!({"skeleton": "yes"}), "invalid_argument"),
            (
                json!({"skeleton": true, "query": {"role": "button"}}),
                "invalid_argument",
            ),
            (
                json!({"root": "e1", "scope": "app", "observation_id": "o-1"}),
                "invalid_argument",
            ),
            (json!({"root": "e1"}), "observation_required"),
            (
                json!({"root": "", "observation_id": "o-1"}),
                "invalid_argument",
            ),
        ] {
            let refused = listing(&body).expect_err(&body.to_string());
            assert_eq!(refused.code, code, "{body}");
            assert!(refused.detail.is_some(), "{body}: the sentence says why");
        }
    }

    // --- scopes ------------------------------------------------------------------

    /// An application with a window (a button and a sheet holding a button), a
    /// context menu, and a focused field.
    fn desktop() -> Vec<Scripted> {
        vec![
            holding("AXWindow", "Document", &[1, 2, 4]),
            Scripted::button("Save", frame(0.0, 0.0)),
            holding("AXSheet", "Clear all items?", &[3]),
            Scripted::button("Clear", frame(0.0, 40.0)),
            Scripted::field("Name", "ada", frame(0.0, 80.0)),
            holding("AXMenu", "Context", &[6]),
            Scripted {
                label: Some("Copy".to_string()),
                actions: vec![PRESS.to_string()],
                frame: Some(frame(0.0, 120.0)),
                ..Scripted::of("AXMenuItem")
            },
        ]
    }

    #[test]
    fn the_app_scope_is_the_whole_application() {
        let (_recorder, ax) = platform(desktop());
        let found = whole(&ax, &app_scene(), Scope::App).expect("the app");
        assert_eq!(labels(&found), vec!["Save", "Clear", "Name", "Copy"]);
    }

    // The bound window is a target's own scope: listing the application around it
    // would name controls of windows the caller never bound.
    #[test]
    fn the_app_scope_is_refused_beside_a_bound_window() {
        let (recorder, ax) = platform(desktop());
        recorder.windows_are(&[0]);
        let windows = crate::ax::application_windows(&ax, PID);
        let bound = bound_to(&windows[0], 10, &WindowRecorder::new(Vec::new()));

        let refused = whole(&ax, &bound, Scope::App).expect_err("app with a target");
        assert_eq!(refused.code, "invalid_argument");

        let found = whole(&ax, &bound, Scope::Window).expect("the bound window");
        assert_eq!(labels(&found), vec!["Save", "Clear", "Name"]);
    }

    #[test]
    fn the_window_scope_without_a_target_is_the_focused_window() {
        let (recorder, ax) = platform(desktop());
        recorder.focuses(Some(0), None);
        let found = whole(&ax, &app_scene(), Scope::Window).expect("the focused window");
        assert_eq!(labels(&found), vec!["Save", "Clear", "Name"]);

        recorder.focuses(None, None);
        let refused = whole(&ax, &app_scene(), Scope::Window).expect_err("no window");
        assert_eq!(refused.code, "scope_not_found");
    }

    // The focused scope lists the focused element itself.
    #[test]
    fn the_focused_scope_lists_the_focused_element() {
        let (recorder, ax) = platform(desktop());
        recorder.focuses(None, Some(4));
        let found = whole(&ax, &app_scene(), Scope::Focused).expect("the focus");
        assert_eq!(labels(&found), vec!["Name"]);

        recorder.focuses(None, None);
        assert_eq!(
            whole(&ax, &app_scene(), Scope::Focused).unwrap_err().code,
            "scope_not_found"
        );
    }

    // The focused element is asked, like any listed node, whether its container
    // selects it: a focused row of a list that selects its rows lists `select`.
    #[test]
    fn a_focused_row_lists_select_when_its_list_selects_its_rows() {
        let list = Scripted {
            selects_children: true,
            ..holding("AXTable", "Inbox", &[1])
        };
        let (recorder, ax) = platform(vec![list, holding("AXRow", "Hello", &[])]);
        recorder.focuses(None, Some(1));

        let found = whole(&ax, &app_scene(), Scope::Focused).expect("the focus");

        assert_eq!(labels(&found), vec!["Hello"]);
        assert!(found.nodes[0].routes.select, "its list selects it");
        drop(found);
        assert_eq!(recorder.counts().2, 0, "the list was given back");
    }

    #[test]
    fn the_menu_and_dialog_scopes_are_what_is_open() {
        let (_recorder, ax) = platform(desktop());
        let menus = whole(&ax, &app_scene(), Scope::Menu).expect("a menu is open");
        assert_eq!(labels(&menus), vec!["Copy"]);
        let dialogs = whole(&ax, &app_scene(), Scope::Dialog).expect("a sheet is open");
        assert_eq!(labels(&dialogs), vec!["Clear"]);

        let (_recorder, ax) = platform(vec![Scripted::button("Save", frame(0.0, 0.0))]);
        for scope in [Scope::Menu, Scope::Dialog] {
            let refused = whole(&ax, &app_scene(), scope).expect_err("nothing open");
            assert_eq!(refused.code, "scope_not_found", "{scope:?}");
        }
    }

    // --- a bound window's scopes ---------------------------------------------------

    /// A scene bound to `window`, whose window-server id is `id`.
    fn bound_to(window: &Retained, id: u32, windows: &Arc<WindowRecorder>) -> Scene {
        Scene {
            pid: PID,
            window: Some(Bound {
                element: window.handle(),
                id,
                windows: windows.clone(),
            }),
        }
    }

    fn span(x: f64, w: f64) -> Frame {
        Frame {
            x,
            y: 100.0,
            w,
            h: 300.0,
        }
    }

    /// Two windows of one application: the first holds Save, the second a field
    /// and a menu, and each has a dialog of its own in a window of its own.
    fn two_windows() -> Vec<Scripted> {
        let placed = |frame: Frame, node: Scripted| Scripted {
            frame: Some(frame),
            ..node
        };
        vec![
            placed(span(0.0, 400.0), holding("AXWindow", "First", &[4])),
            placed(span(500.0, 400.0), holding("AXWindow", "Second", &[5, 6])),
            placed(
                span(100.0, 200.0),
                subroled("AXWindow", "AXDialog", "Save first?", &[8]),
            ),
            placed(
                span(600.0, 200.0),
                subroled("AXWindow", "AXDialog", "Save second?", &[9]),
            ),
            Scripted::button("Save", frame(0.0, 120.0)),
            Scripted::field("Name", "ada", frame(500.0, 120.0)),
            holding("AXMenu", "Context", &[7]),
            Scripted {
                label: Some("Copy".to_string()),
                actions: vec![PRESS.to_string()],
                frame: Some(frame(500.0, 160.0)),
                ..Scripted::of("AXMenuItem")
            },
            Scripted::button("Keep first", frame(100.0, 200.0)),
            Scripted::button("Keep second", frame(600.0, 200.0)),
        ]
    }

    /// What the window server says of them: the windows are 10 and 11, and each
    /// dialog's window is transient for its own.
    fn two_windows_server() -> Arc<WindowRecorder> {
        let window = |id: u32, frame: Frame, of: Option<u32>| WindowFacts {
            transient_for: of,
            ..crate::window_server::window(
                id,
                PID,
                Bounds {
                    x: frame.x,
                    y: frame.y,
                    w: frame.w,
                    h: frame.h,
                },
            )
        };
        WindowRecorder::new(vec![
            window(10, span(0.0, 400.0), None),
            window(11, span(500.0, 400.0), None),
            window(12, span(100.0, 200.0), Some(10)),
            window(13, span(600.0, 200.0), Some(11)),
        ])
    }

    // Two windows of one application, the first bound, focus in the second. The
    // focused scope answers only a control inside the bound window, so focus
    // elsewhere is no scope at all, with a sentence that says where it is, and no
    // reference to the second window's control is minted.
    #[test]
    fn focus_in_another_window_of_the_application_is_not_the_bound_windows() {
        let (recorder, ax) = platform(two_windows());
        recorder.windows_are(&[0, 1]);
        let windows = crate::ax::application_windows(&ax, PID);
        let first = bound_to(&windows[0], 10, &two_windows_server());
        recorder.focuses(Some(1), Some(5));

        let refused = whole(&ax, &first, Scope::Focused).expect_err("focus elsewhere");
        assert_eq!(refused.code, "scope_not_found");
        let detail = refused.detail.unwrap_or_default();
        assert!(detail.contains("another window"), "{detail}");
        drop(windows);
        assert_eq!(
            recorder.counts().2,
            0,
            "nothing of the second window is held"
        );

        let windows = crate::ax::application_windows(&ax, PID);
        let first = bound_to(&windows[0], 10, &two_windows_server());
        recorder.focuses(Some(0), Some(4));
        let found = whole(&ax, &first, Scope::Focused).expect("focus in the bound window");
        assert_eq!(labels(&found), vec!["Save"]);

        recorder.focuses(Some(1), Some(5));
        let found = whole(&ax, &app_scene(), Scope::Focused).expect("unbound, as before");
        assert_eq!(labels(&found), vec!["Name"]);
    }

    // A dialog in a window of its own is the bound window's only when the window
    // server says its window is transient for the bound one; without a binding
    // every dialog of the application is listed, as before.
    #[test]
    fn a_dialog_is_listed_only_under_the_window_it_is_transient_for() {
        let (recorder, ax) = platform(two_windows());
        recorder.windows_are(&[0, 1]);
        let windows = crate::ax::application_windows(&ax, PID);
        let server = two_windows_server();

        let first = whole(&ax, &bound_to(&windows[0], 10, &server), Scope::Dialog)
            .expect("the first window's dialog");
        assert_eq!(labels(&first), vec!["Keep first"]);
        let second = whole(&ax, &bound_to(&windows[1], 11, &server), Scope::Dialog)
            .expect("the second window's dialog");
        assert_eq!(labels(&second), vec!["Keep second"]);

        let every = whole(&ax, &app_scene(), Scope::Dialog).expect("unbound");
        assert_eq!(labels(&every), vec!["Keep first", "Keep second"]);
    }

    // A window transient for a window that is the bound one's belongs to it too,
    // and a dialog no window marks as anybody's is no bound window's: then the
    // dialog scope says none of the bound window's is open, and how to list the
    // application's.
    #[test]
    fn a_dialog_belongs_through_its_transient_chain_and_never_without_one() {
        let (recorder, ax) = platform(two_windows());
        recorder.windows_are(&[0, 1]);
        let windows = crate::ax::application_windows(&ax, PID);
        let server = two_windows_server();
        server.change(13, |dialog| dialog.transient_for = Some(12));

        let first =
            whole(&ax, &bound_to(&windows[0], 10, &server), Scope::Dialog).expect("both dialogs");
        assert_eq!(labels(&first), vec!["Keep first", "Keep second"]);

        server.change(12, |dialog| dialog.transient_for = None);
        server.change(13, |dialog| dialog.transient_for = None);
        let refused = whole(&ax, &bound_to(&windows[0], 10, &server), Scope::Dialog)
            .expect_err("no dialog is the first window's");
        assert_eq!(refused.code, "scope_not_found");
        let detail = refused.detail.unwrap_or_default();
        assert!(detail.contains("bound window"), "{detail}");
        assert!(detail.contains("release the target"), "{detail}");
    }

    // A menu open in another window of the application neither satisfies
    // menu_open nor keeps menu_closed from holding under a binding, and the menu
    // scope does not list it; without a binding it is the application's menu, as
    // before.
    #[test]
    fn a_menu_in_another_window_is_not_the_bound_windows() {
        let (recorder, ax) = platform(two_windows());
        recorder.windows_are(&[0, 1]);
        let windows = crate::ax::application_windows(&ax, PID);
        let server = two_windows_server();
        let clock = SystemClock::new();
        let seen = |scene: Scene, condition: Condition| {
            let at = Looking {
                ax: &ax,
                scene,
                named: None,
                clock: &clock,
            };
            holds(look(&at, &condition, u64::MAX).expect("a look"))
        };

        let first = bound_to(&windows[0], 10, &server);
        assert!(!seen(first.clone(), Condition::MenuOpen));
        assert!(seen(first.clone(), Condition::MenuClosed));
        let refused = whole(&ax, &first, Scope::Menu).expect_err("not the first's");
        assert_eq!(refused.code, "scope_not_found");
        let receipt = surfaces(&ax, &first).expect("a scan");
        let open: Vec<(&str, Option<&str>)> = kinds(&receipt.found)
            .into_iter()
            .map(|(kind, _native, label)| (kind, label))
            .collect();
        assert_eq!(
            open,
            vec![("dialog", Some("Save first?"))],
            "a receipt sees the first window's dialog and not the second's menu"
        );

        let second = bound_to(&windows[1], 11, &server);
        assert!(seen(second.clone(), Condition::MenuOpen));
        assert!(!seen(second.clone(), Condition::MenuClosed));
        let menu = whole(&ax, &second, Scope::Menu).expect("the second's menu");
        assert_eq!(labels(&menu), vec!["Copy"]);

        assert!(seen(app_scene(), Condition::MenuOpen));
        assert!(!seen(app_scene(), Condition::MenuClosed));
    }

    // A bound window that is itself a dialog is the dialog its scope lists, as
    // without a binding: its own subtree starts at it.
    #[test]
    fn a_bound_dialog_window_is_its_own_dialog() {
        let (recorder, ax) = platform(two_windows());
        recorder.windows_are(&[0, 1, 2, 3]);
        let windows = crate::ax::application_windows(&ax, PID);
        let dialog = bound_to(&windows[2], 12, &two_windows_server());

        let found = whole(&ax, &dialog, Scope::Dialog).expect("the bound dialog");
        assert_eq!(labels(&found), vec!["Keep first"]);
    }

    // Where the window server marks no window as another's (macOS), a bound
    // window's surfaces are the ones inside it: a sheet of its own, never a
    // dialog in a window of its own.
    #[test]
    fn where_nothing_marks_ownership_a_bound_window_sees_only_what_is_inside_it() {
        let (recorder, ax) = platform(desktop());
        recorder.windows_are(&[0]);
        let windows = crate::ax::application_windows(&ax, PID);
        let unmarked = WindowRecorder::new(Vec::new());
        let bound = bound_to(&windows[0], 10, &unmarked);

        let sheet = whole(&ax, &bound, Scope::Dialog).expect("its own sheet");
        assert_eq!(labels(&sheet), vec!["Clear"]);
        assert!(
            whole(&ax, &bound, Scope::Menu).is_err(),
            "the context menu is the application's, not the window's"
        );
    }

    // --- surfaces ----------------------------------------------------------------

    fn kinds(found: &[Surface]) -> Vec<(&str, &str, Option<&str>)> {
        found
            .iter()
            .map(|surface| {
                (
                    surface.kind,
                    surface.native_kind.as_str(),
                    surface.label.as_deref(),
                )
            })
            .collect()
    }

    #[test]
    fn a_sheet_an_alert_a_popover_and_a_context_menu_are_surfaces() {
        let (_recorder, ax) = platform(vec![
            holding("AXWindow", "Document", &[1, 2, 3]),
            holding("AXSheet", "Save changes?", &[]),
            holding("AXPopover", "Font", &[]),
            Scripted::button("Save", frame(0.0, 0.0)),
            subroled("AXWindow", "AXApplicationAlertDialog", "Disk full", &[]),
            holding("AXMenu", "Context", &[]),
            subroled("AXWindow", "AXStandardWindow", "Other", &[]),
        ]);

        assert_eq!(
            kinds(&every(&ax).found),
            vec![
                ("dialog", "AXSheet", Some("Save changes?")),
                ("popup", "AXPopover", Some("Font")),
                ("alert", "AXApplicationAlertDialog", Some("Disk full")),
                ("menu", "AXMenu", Some("Context")),
            ]
        );
    }

    // A menu-bar item's menu is a child of it whether or not it is open, so only
    // the item that reads selected counts.
    #[test]
    fn a_menu_bar_menu_is_open_only_under_a_selected_item() {
        let selected = States {
            selected: Some(true),
            ..States::default()
        };
        let (recorder, ax) = platform(vec![
            holding("AXMenuBar", "", &[1, 3]),
            holding("AXMenuBarItem", "File", &[2]),
            holding("AXMenu", "File", &[]),
            holding("AXMenuBarItem", "Edit", &[4]),
            holding("AXMenu", "Edit", &[]),
        ]);
        assert!(every(&ax).found.is_empty(), "no item is selected");

        recorder.change(3, |item| item.states = selected);
        assert_eq!(
            kinds(&every(&ax).found),
            vec![("menu", "AXMenu", Some("Edit"))]
        );
    }

    // The scan is bounded: an application with a hundred windows costs 64 reads.
    #[test]
    fn a_scan_for_surfaces_reads_at_most_sixty_four_nodes() {
        let mut many: Vec<Scripted> = (0..100)
            .map(|index| holding("AXWindow", &format!("W{index}"), &[]))
            .collect();
        many.push(holding("AXSheet", "Late", &[]));
        let (_recorder, ax) = platform(many);
        let before = ax.messages();

        let found = every(&ax);

        assert!(
            found.found.is_empty(),
            "the sheet past the bound is not reached"
        );
        assert_eq!(found.stopped, Some("nodes"), "and the scan says so");
        assert!(
            ax.messages() - before <= 2 * MAX_SURFACE_NODES as u64 + 2,
            "{} messages",
            ax.messages() - before
        );
    }

    // A scan says whether it saw everything it looks at: a node that does not
    // answer in time stops it, and one that saw every window is complete.
    #[test]
    fn a_scan_for_surfaces_says_whether_it_finished() {
        let (recorder, ax) = platform(vec![
            holding("AXWindow", "Slow", &[]),
            holding("AXMenu", "Context", &[]),
        ]);
        recorder.fails_facts_for(0, Unread::NoAnswer);
        let slow = every(&ax);
        assert!(slow.found.is_empty(), "nothing after the slow node is read");
        assert_eq!(slow.stopped, Some("time"));

        let (_recorder, ax) = platform(desktop());
        let whole = every(&ax);
        assert_eq!(whole.found.len(), 2);
        assert_eq!(whole.stopped, None);
    }

    // A scan that stopped before it reached the menu proves nothing closed:
    // menu_closed does not hold, and says why the scan stopped. One that saw
    // everything and found no menu holds.
    #[test]
    fn menu_closed_holds_only_on_a_scan_that_saw_everything() {
        let clock = SystemClock::new();
        let closed = |ax: &Rc<dyn Ax>| {
            let at = Looking {
                ax,
                scene: app_scene(),
                named: None,
                clock: &clock,
            };
            look(&at, &Condition::MenuClosed, u64::MAX).expect("a look")
        };
        let mut many: Vec<Scripted> = (0..100)
            .map(|index| holding("AXWindow", &format!("W{index}"), &[]))
            .collect();
        many.push(holding("AXMenu", "Context", &[]));
        let (_recorder, ax) = platform(many);
        let seen = closed(&ax);
        assert!(matches!(seen, Seen::Unproven("nodes")), "{seen:?}");

        let (recorder, ax) = platform(vec![
            holding("AXWindow", "Slow", &[]),
            holding("AXMenu", "Context", &[]),
        ]);
        recorder.fails_facts_for(0, Unread::NoAnswer);
        let seen = closed(&ax);
        assert!(matches!(seen, Seen::Unproven("time")), "{seen:?}");

        let (_recorder, ax) = platform(vec![holding("AXWindow", "Document", &[])]);
        assert!(holds(closed(&ax)));
    }

    // A menu or dialog scope that a scan stopped short of is not proven closed:
    // `elements` says the look stopped before it finished, still as
    // scope_not_found, and a condition reads the scope as unproven, never as
    // empty.
    #[test]
    fn a_scope_an_incomplete_scan_found_nothing_in_is_not_proven_closed() {
        let mut many: Vec<Scripted> = (0..100)
            .map(|index| holding("AXWindow", &format!("W{index}"), &[]))
            .collect();
        many.push(holding("AXSheet", "Clear all items?", &[101]));
        many.push(Scripted::button("Clear", frame(0.0, 40.0)));
        let (_recorder, ax) = platform(many);

        let refused = whole(&ax, &app_scene(), Scope::Dialog).expect_err("not found");
        assert_eq!(refused.code, "scope_not_found");
        let detail = refused.detail.unwrap_or_default();
        assert!(detail.contains("stopped before it finished"), "{detail}");

        let clock = SystemClock::new();
        let at = Looking {
            ax: &ax,
            scene: app_scene(),
            named: None,
            clock: &clock,
        };
        let clear = Query {
            role: Some("button".to_string()),
            name: Some("clear".to_string()),
        };
        let gone = Condition::Disappears {
            query: clear.clone(),
            scope: Some(Scope::Dialog),
        };
        let seen = look(&at, &gone, u64::MAX).expect("a look");
        assert!(matches!(seen, Seen::Unproven("nodes")), "{seen:?}");
        let shows = Condition::Appears {
            query: clear,
            scope: Some(Scope::Dialog),
        };
        assert!(matches!(
            look(&at, &shows, u64::MAX).expect("a look"),
            Seen::Not
        ));
        assert!(matches!(
            look(&at, &Condition::MenuOpen, u64::MAX).expect("a look"),
            Seen::Not
        ));
    }

    #[test]
    fn surfaces_hold_their_references_and_give_them_back() {
        let (recorder, ax) = platform(vec![
            holding("AXWindow", "Document", &[1]),
            holding("AXSheet", "Save changes?", &[]),
        ]);
        let found = every(&ax);
        assert_eq!(found.found.len(), 1);
        drop(found);
        assert_eq!(recorder.counts().2, 0, "nothing is still held");
    }

    // --- conditions ----------------------------------------------------------------

    fn until(value: serde_json::Value) -> Result<Until, String> {
        parse_until(&value, true)
    }

    #[test]
    fn every_condition_kind_parses() {
        let find = |role: Option<&str>, name: Option<&str>| Query {
            role: role.map(str::to_string),
            name: name.map(str::to_string),
        };
        for (value, condition) in [
            (
                json!({"kind": "appears", "role": "button", "name": "Done", "scope": "app"}),
                Condition::Appears {
                    query: find(Some("button"), Some("Done")),
                    scope: Some(Scope::App),
                },
            ),
            (
                json!({"kind": "disappears", "element_ref": "e4"}),
                Condition::DisappearsElement("e4".to_string()),
            ),
            (
                json!({"kind": "disappears", "role": "dialog"}),
                Condition::Disappears {
                    query: find(Some("dialog"), None),
                    scope: None,
                },
            ),
            (
                json!({"kind": "enabled", "element_ref": "e7"}),
                Condition::Enabled("e7".to_string()),
            ),
            (
                json!({"kind": "value", "element_ref": "e2", "value": "42"}),
                Condition::Value {
                    element: "e2".to_string(),
                    value: "42".to_string(),
                },
            ),
            (
                json!({"kind": "window_title", "contains": "Draft"}),
                Condition::WindowTitle {
                    contains: "Draft".to_string(),
                    scope: None,
                },
            ),
            (json!({"kind": "menu_open"}), Condition::MenuOpen),
            (json!({"kind": "menu_closed"}), Condition::MenuClosed),
            (
                json!({"kind": "text", "contains": "Saved", "scope": "window"}),
                Condition::Appears {
                    query: find(None, Some("Saved")),
                    scope: Some(Scope::Window),
                },
            ),
        ] {
            let parsed = until(value.clone()).unwrap_or_else(|why| panic!("{value}: {why}"));
            assert_eq!(parsed.condition, condition, "{value}");
            assert_eq!(parsed.timeout_ms, DEFAULT_UNTIL_MS, "{value}");
        }
        let bounded = until(json!({"kind": "menu_open", "timeout_ms": 25000})).expect("bounded");
        assert_eq!(bounded.timeout_ms, MAX_UNTIL_MS);
    }

    #[test]
    fn a_condition_that_cannot_be_read_is_refused_with_a_reason() {
        for value in [
            json!("appears"),
            json!({}),
            json!({"kind": "flashes"}),
            json!({"kind": "appears"}),
            json!({"kind": "appears", "role": "AXButton"}),
            json!({"kind": "appears", "role": "button", "colour": "red"}),
            json!({"kind": "disappears"}),
            json!({"kind": "disappears", "element_ref": "e1", "role": "button"}),
            json!({"kind": "disappears", "element_ref": "e1", "scope": "app"}),
            json!({"kind": "enabled"}),
            json!({"kind": "enabled", "element_ref": "e1", "scope": "app"}),
            json!({"kind": "value", "element_ref": "e1"}),
            json!({"kind": "value", "element_ref": "e1", "value": 4}),
            json!({"kind": "window_title"}),
            json!({"kind": "window_title", "contains": ""}),
            json!({"kind": "text", "contains": "x", "scope": "desktop"}),
            json!({"kind": "menu_open", "role": "menu"}),
            json!({"kind": "menu_open", "timeout_ms": 0}),
            json!({"kind": "menu_open", "timeout_ms": 25001}),
            json!({"kind": "menu_open", "timeout_ms": "5000"}),
        ] {
            let why = until(value.clone()).expect_err(&value.to_string());
            assert!(!why.is_empty(), "{value}");
        }
    }

    // A reference means nothing without the listing that minted it.
    #[test]
    fn a_condition_on_a_reference_needs_the_listing_it_was_read_from() {
        for value in [
            json!({"kind": "enabled", "element_ref": "e1"}),
            json!({"kind": "value", "element_ref": "e1", "value": "x"}),
            json!({"kind": "disappears", "element_ref": "e1"}),
        ] {
            let why = parse_until(&value, false).expect_err(&value.to_string());
            assert!(why.contains("observation_id"), "{value}: {why}");
        }
        assert!(parse_until(&json!({"kind": "menu_open"}), false).is_ok());
    }

    /// The listing a condition's reference is read in: every listed control of the
    /// scripted application, numbered as a reply would number them.
    fn listing_of(ax: &Rc<dyn Ax>, recorder: &Recorder) -> Elements {
        let found = crate::tree::walk(ax, PID, &SystemClock::new(), u64::MAX);
        let entries = found
            .nodes
            .into_iter()
            .enumerate()
            .map(|(index, node)| crate::ax::Entry {
                reference: crate::ax::reference_for(index),
                role: node.role,
                secure: node.secure,
                under: None,
                element: node.element,
            })
            .collect();
        Elements::new(PID, recorder.started_at(), None, entries)
    }

    fn holds(seen: Seen) -> bool {
        matches!(seen, Seen::Holds(_))
    }

    #[test]
    fn each_condition_holds_exactly_when_the_tree_says_so() {
        let (recorder, ax) = platform(desktop());
        let named = listing_of(&ax, &recorder);
        let clock = SystemClock::new();
        let at = Looking {
            ax: &ax,
            scene: app_scene(),
            named: Some(&named),
            clock: &clock,
        };
        let check = |condition: Condition| holds(look(&at, &condition, u64::MAX).expect("a look"));
        let find = |role: &str, name: &str| Query {
            role: Some(role.to_string()),
            name: Some(name.to_string()),
        };

        assert!(check(Condition::Appears {
            query: find("button", "clear"),
            scope: None
        }));
        assert!(!check(Condition::Appears {
            query: find("button", "delete"),
            scope: None
        }));
        assert!(check(Condition::Appears {
            query: find("button", "clear"),
            scope: Some(Scope::Dialog)
        }));
        assert!(
            !check(Condition::Appears {
                query: find("button", "save"),
                scope: Some(Scope::Dialog)
            }),
            "Save is not in the sheet"
        );
        assert!(check(Condition::Enabled("e1".to_string())));
        assert!(check(Condition::Value {
            element: "e3".to_string(),
            value: "ada".to_string()
        }));
        assert!(!check(Condition::Value {
            element: "e3".to_string(),
            value: "bob".to_string()
        }));
        assert!(check(Condition::MenuOpen));
        assert!(!check(Condition::MenuClosed));
        assert!(!check(Condition::DisappearsElement("e1".to_string())));

        recorder.change(1, |save| save.enabled = false);
        assert!(!check(Condition::Enabled("e1".to_string())));
        recorder.vanish(1);
        assert!(check(Condition::DisappearsElement("e1".to_string())));
        assert!(check(Condition::Disappears {
            query: find("menuitem", "copy"),
            scope: Some(Scope::Dialog)
        }));
    }

    // A look that stopped before it saw everything proves no absence: a Save
    // button the walk never reached is not gone, and the look says why it stopped.
    // Only a complete look that lists no match holds; a match is no disappearance,
    // as before.
    #[test]
    fn a_disappearance_by_query_holds_only_on_a_complete_look() {
        let (_recorder, ax) = platform(desktop());
        let clock = SystemClock::new();
        let at = Looking {
            ax: &ax,
            scene: app_scene(),
            named: None,
            clock: &clock,
        };
        let gone = |name: &str| Condition::Disappears {
            query: Query {
                role: Some("button".to_string()),
                name: Some(name.to_string()),
            },
            scope: None,
        };
        let spent = 0;

        for name in ["save", "delete"] {
            let cut = look(&at, &gone(name), spent).expect("a look");
            assert!(matches!(cut, Seen::Unproven("time")), "{name}: {cut:?}");
        }
        assert!(holds(look(&at, &gone("delete"), u64::MAX).expect("a look")));
        assert!(matches!(
            look(&at, &gone("save"), u64::MAX).expect("a look"),
            Seen::Not
        ));
    }

    // A wait for an absence whose looks are all cut ends not met, never met by
    // default, and says why its last look stopped; so does the look before an
    // action's input, which never reads an incomplete look as held.
    #[test]
    fn a_wait_for_an_absence_its_looks_cannot_prove_says_they_were_cut() {
        let (_recorder, ax) = platform(desktop());
        let clock = SystemClock::new();
        let at = Looking {
            ax: &ax,
            scene: app_scene(),
            named: None,
            clock: &clock,
        };
        let gone = Until {
            condition: Condition::Disappears {
                query: Query {
                    role: Some("button".to_string()),
                    name: Some("delete".to_string()),
                },
                scope: None,
            },
            timeout_ms: 300,
        };

        let waited = wait(&test_gate(), gone.timeout_ms, |_deadline| {
            look(&at, &gone.condition, 0)
        })
        .expect("a wait");

        assert!(!waited.met);
        assert_eq!(waited.truncated, Some("time"));
        assert!(!held_before(&at, &gone, 0, Some(0)).expect("a look"));

        let whole = wait(&test_gate(), gone.timeout_ms, |deadline| {
            look(&at, &gone.condition, deadline)
        })
        .expect("a wait");
        assert!(whole.met);
        assert_eq!(whole.truncated, None);
    }

    // A scope that is not open yet reads as empty: waiting for a dialog's button
    // before the dialog exists is the ordinary case, not a refusal.
    #[test]
    fn a_condition_whose_scope_is_not_open_reads_it_as_empty() {
        let (_recorder, ax) = platform(vec![Scripted::button("Save", frame(0.0, 0.0))]);
        let clock = SystemClock::new();
        let at = Looking {
            ax: &ax,
            scene: app_scene(),
            named: None,
            clock: &clock,
        };
        let appears = Condition::Appears {
            query: Query {
                role: Some("button".to_string()),
                name: None,
            },
            scope: Some(Scope::Dialog),
        };
        assert!(!holds(look(&at, &appears, u64::MAX).expect("a look")));
        assert!(holds(
            look(&at, &Condition::MenuClosed, u64::MAX).expect("a look")
        ));
    }

    #[test]
    fn the_window_title_is_the_bound_or_focused_windows() {
        let (recorder, ax) = platform(desktop());
        recorder.focuses(Some(0), None);
        let clock = SystemClock::new();
        let at = Looking {
            ax: &ax,
            scene: app_scene(),
            named: None,
            clock: &clock,
        };
        let titled = |text: &str| Condition::WindowTitle {
            contains: text.to_string(),
            scope: None,
        };
        assert!(holds(look(&at, &titled("docu"), u64::MAX).expect("a look")));
        assert!(!holds(
            look(&at, &titled("Draft"), u64::MAX).expect("a look")
        ));
    }

    // A value condition on a password field would publish whether a guess was
    // right, so it is refused.
    #[test]
    fn a_value_condition_on_a_secure_field_is_refused_and_never_reads_it() {
        let (recorder, ax) = platform(vec![Scripted::secure_field(
            "Password",
            "hunter2",
            frame(0.0, 0.0),
        )]);
        let named = listing_of(&ax, &recorder);
        let clock = SystemClock::new();
        let at = Looking {
            ax: &ax,
            scene: app_scene(),
            named: Some(&named),
            clock: &clock,
        };
        let guess = Condition::Value {
            element: "e1".to_string(),
            value: "hunter2".to_string(),
        };

        let refused = look(&at, &guess, u64::MAX).expect_err("a secure field");
        assert_eq!(refused.code, "invalid_argument");
        assert!(
            recorder.values_read().is_empty(),
            "the value was never asked"
        );
    }

    #[test]
    fn a_condition_on_a_reference_nobody_listed_is_stale() {
        let (recorder, ax) = platform(desktop());
        let named = listing_of(&ax, &recorder);
        let clock = SystemClock::new();
        let at = Looking {
            ax: &ax,
            scene: app_scene(),
            named: Some(&named),
            clock: &clock,
        };
        let refused = look(&at, &Condition::Enabled("e99".to_string()), u64::MAX)
            .expect_err("nobody listed e99");
        assert_eq!(refused.code, "stale_element");
    }

    // --- waits ---------------------------------------------------------------------

    #[test]
    fn a_wait_that_is_met_says_how_many_looks_it_took() {
        let gate = test_gate();
        let mut seen = 0;
        let waited = wait(&gate, 5_000, |_deadline| {
            seen += 1;
            Ok(if seen == 3 {
                Seen::Holds(None)
            } else {
                Seen::Not
            })
        })
        .expect("a wait");

        assert!(waited.met);
        assert_eq!(waited.looks, 3);
        assert_eq!(waited.waited_ms, 2 * POLL_MS);
    }

    // Not met by the deadline is an answer: the wait ran, and says how long.
    #[test]
    fn a_wait_that_is_not_met_stops_at_its_timeout_with_a_bounded_number_of_looks() {
        let gate = test_gate();
        let mut seen = 0;
        let waited = wait(&gate, 1_000, |_deadline| {
            seen += 1;
            Ok(Seen::Not)
        })
        .expect("a wait");

        assert!(!waited.met);
        assert_eq!(waited.looks, 1_000 / POLL_MS + 1);
        assert_eq!(seen, 11);
        assert_eq!(waited.waited_ms, 1_000);
    }

    // A look that is slow still counts against the timeout: the wait does not
    // sleep a full poll on top of a look that already took longer.
    #[test]
    fn a_slow_look_counts_against_the_timeout() {
        let clock = Arc::new(TestClock { ms: Mutex::new(0) });
        let gate = Gate::new("boot-test".to_string(), clock.clone());
        let waited = wait(&gate, 1_000, |_deadline| {
            clock.sleep(400);
            Ok(Seen::Not)
        })
        .expect("a wait");

        assert!(!waited.met);
        assert!(waited.looks <= 3, "{} looks", waited.looks);
    }

    // Each look gets the walk's own budget as its deadline.
    #[test]
    fn each_look_is_bounded_by_the_walks_budget() {
        let gate = test_gate();
        let mut deadlines = Vec::new();
        wait(&gate, 200, |deadline| {
            deadlines.push(deadline);
            Ok(Seen::Not)
        })
        .expect("a wait");

        assert_eq!(deadlines[0], crate::tree::WALK_BUDGET_MS);
        assert_eq!(deadlines[1], POLL_MS + crate::tree::WALK_BUDGET_MS);
    }

    #[test]
    fn a_pause_ends_a_wait_at_once() {
        let gate = test_gate();
        let waited = wait(&gate, 5_000, |_deadline| {
            gate.control(ControlAction::Pause, None);
            Ok(Seen::Not)
        });

        assert_eq!(waited.expect_err("paused").code, "cancelled");
    }

    // A wait looks and never acts: nothing is pressed, written or typed however
    // long it goes on.
    #[test]
    fn a_wait_never_sends_input() {
        let (recorder, ax) = platform(desktop());
        let gate = test_gate();
        let clock = SystemClock::new();
        let at = Looking {
            ax: &ax,
            scene: app_scene(),
            named: None,
            clock: &clock,
        };
        let never = Condition::Appears {
            query: Query {
                role: Some("button".to_string()),
                name: Some("Never".to_string()),
            },
            scope: None,
        };

        let waited = wait(&gate, 500, |deadline| look(&at, &never, deadline)).expect("a wait");

        assert!(!waited.met);
        assert!(recorder.performed().is_empty());
        assert_eq!(recorder.counts().2, 0, "every look gave back what it held");
    }

    // A reference in a condition names a control of the listing the request names.
    // An observation that listed no controls (a screenshot's) names none: the
    // reference is stale, and the request did name an observation.
    #[test]
    fn a_condition_on_a_reference_an_image_never_listed_is_stale() {
        let (_recorder, ax) = platform(desktop());
        let clock = SystemClock::new();
        let at = Looking {
            ax: &ax,
            scene: app_scene(),
            named: None,
            clock: &clock,
        };

        let refused = look(&at, &Condition::Enabled("e1".to_string()), u64::MAX)
            .expect_err("nothing was listed");

        assert_eq!(refused.code, "stale_element");
    }

    // --- verbs ---------------------------------------------------------------------

    #[test]
    fn a_verb_is_read_off_its_request() {
        assert_eq!(Verb::from_request("focus", &json!({})), Ok(Verb::Focus));
        assert_eq!(Verb::from_request("select", &json!({})), Ok(Verb::Select));
        assert_eq!(Verb::from_request("expand", &json!({})), Ok(Verb::Expand));
        assert_eq!(
            Verb::from_request("collapse", &json!({})),
            Ok(Verb::Collapse)
        );
        assert_eq!(
            Verb::from_request("scroll_to", &json!({})),
            Ok(Verb::ScrollTo)
        );
        assert_eq!(
            Verb::from_request("toggle", &json!({"on": false})),
            Ok(Verb::Toggle(false))
        );
        assert_eq!(
            Verb::from_request("type", &json!({"text": "hi"})),
            Ok(Verb::Type("hi".to_string()))
        );

        for (action, body) in [
            ("toggle", json!({})),
            ("toggle", json!({"on": "true"})),
            ("type", json!({})),
            ("type", json!({"text": ""})),
        ] {
            let refused = Verb::from_request(action, &body).expect_err(action);
            assert_eq!(refused.code, "invalid_argument", "{action} {body}");
        }
    }

    // The states a receipt names, one pair of words per state.
    #[test]
    fn an_already_held_state_plans_nothing_and_is_reported_as_read() {
        let (_recorder, ax) = platform(vec![Scripted::checkbox("Bold", true, frame(0.0, 0.0))]);
        let root = ax.application(&ax, PID).expect("the application");
        let page = ax.children(&ax, root.handle(), 0, 1).expect("one control");
        let handle = page[0].handle();
        let facts = ax.facts(handle).expect("its facts");

        let on = plan(&ax, handle, PID, &facts, false, &Verb::Toggle(true)).expect("a plan");
        assert!(on.already());
        let reported = held(&on);
        assert!(reported.already);
        assert_eq!(reported.effect, Effect::NotObserved);
        assert_eq!(
            (reported.state_before, reported.state_after),
            (Some("on"), Some("on"))
        );

        let off = plan(&ax, handle, PID, &facts, false, &Verb::Toggle(false)).expect("a plan");
        assert!(!off.already());
        assert!(matches!(off.op, Op::Press));
    }

    // A verb asks only the route it takes: on macOS each route is a message of its
    // own, and asking all of them paid for routes the verb never used.
    #[test]
    fn a_verb_asks_only_the_route_it_takes() {
        let field = Scripted {
            routes: Routes {
                focus: true,
                select: true,
                expand: Some(Expander::Expanded),
                text: true,
                ..Routes::default()
            },
            states: States {
                focused: Some(false),
                selected: Some(false),
                expanded: Some(false),
                ..States::default()
            },
            ..Scripted::field("Name", "ada", frame(0.0, 0.0))
        };
        let (recorder, ax) = platform(vec![field]);
        let root = ax.application(&ax, PID).expect("the application");
        let page = ax.children(&ax, root.handle(), 0, 1).expect("one control");
        let handle = page[0].handle();
        let facts = ax.facts(handle).expect("its facts");

        for (verb, route) in [
            (Verb::Focus, Route::Focus),
            (Verb::Select, Route::Select),
            (Verb::Expand, Route::Expand),
            (Verb::Type("x".to_string()), Route::Text),
        ] {
            let before = recorder.routes_asked().len();
            plan(&ax, handle, PID, &facts, false, &verb).expect("a plan");
            assert_eq!(recorder.routes_asked()[before..], [(0, route)], "{verb:?}");
        }

        let before = recorder.routes_asked().len();
        plan(&ax, handle, PID, &facts, false, &Verb::Toggle(true)).expect_err("not a checkbox");
        assert_eq!(
            recorder.routes_asked().len(),
            before,
            "toggle takes no route"
        );
    }

    // A field that would not say what it held before `type` cannot show what the
    // typing changed: the read-back is `unknown`, whatever it reads after (§4.7).
    // On macOS an empty field answers "", so nothing here is an empty field.
    #[test]
    fn typing_into_a_field_that_would_not_say_what_it_held_is_unknown() {
        let (_recorder, ax) = platform(vec![Scripted::field("Name", "ada", frame(0.0, 0.0))]);
        let root = ax.application(&ax, PID).expect("the application");
        let page = ax.children(&ax, root.handle(), 0, 1).expect("one control");
        let handle = page[0].handle();
        let typed = |before: Option<&str>| Plan {
            value: before.map(str::to_string),
            ..Plan::of(Op::Text("a".to_string()))
        };

        let unread = judge(&test_gate(), &ax, handle, &typed(None));
        assert_eq!(unread.effect, Effect::Unknown);
        assert_eq!(unread.value.as_deref(), Some("ada"), "what it holds now");

        let read = judge(&test_gate(), &ax, handle, &typed(Some("ad")));
        assert_eq!(read.effect, Effect::Changed);
    }

    // Focus moves the keyboard inside a window that is already active and
    // never brings one forward.
    #[test]
    fn focus_is_refused_unless_the_controls_window_is_the_active_one() {
        let field = Scripted {
            routes: Routes {
                focus: true,
                ..Routes::default()
            },
            states: States {
                focused: Some(false),
                ..States::default()
            },
            ..Scripted::field("Name", "ada", frame(0.0, 0.0))
        };
        let (recorder, ax) = platform(vec![field]);
        let root = ax.application(&ax, PID).expect("the application");
        let page = ax.children(&ax, root.handle(), 0, 1).expect("one control");
        let handle = page[0].handle();
        let facts = ax.facts(handle).expect("its facts");

        assert!(plan(&ax, handle, PID, &facts, false, &Verb::Focus).is_ok());

        recorder.set_frontmost(Some(PID + 1));
        let refused = plan(&ax, handle, PID, &facts, false, &Verb::Focus).expect_err("behind");
        assert_eq!(refused.code, "focus_refused");

        recorder.set_frontmost(Some(PID));
        recorder.change(0, |field| field.in_focused_window = Some(false));
        let refused =
            plan(&ax, handle, PID, &facts, false, &Verb::Focus).expect_err("other window");
        assert_eq!(refused.code, "focus_refused");

        recorder.change(0, |field| field.in_focused_window = None);
        let refused = plan(&ax, handle, PID, &facts, false, &Verb::Focus).expect_err("unknown");
        assert_eq!(
            refused.code, "focus_refused",
            "unknown is never read as yes"
        );
        assert!(recorder.performed().is_empty());
    }

    #[test]
    fn a_state_that_cannot_be_read_first_refuses_the_verb() {
        let (recorder, ax) = platform(vec![Scripted::checkbox("Bold", true, frame(0.0, 0.0))]);
        recorder.change(0, |bold| bold.states.on = None);
        let root = ax.application(&ax, PID).expect("the application");
        let page = ax.children(&ax, root.handle(), 0, 1).expect("one control");
        let handle = page[0].handle();
        let facts = ax.facts(handle).expect("its facts");

        let refused =
            plan(&ax, handle, PID, &facts, false, &Verb::Toggle(true)).expect_err("unreadable");
        assert_eq!(refused.code, "state_unreadable");
        assert!(recorder.performed().is_empty());
    }

    // The application's focus, as a test reads a scripted node through the seam.
    #[test]
    fn a_focused_element_is_read_through_the_seam() {
        let (recorder, ax) = platform(desktop());
        recorder.focuses(Some(0), Some(4));
        let window = ax.focused(&ax, PID, Focus::Window).expect("a window");
        assert_eq!(
            ax.facts(window.handle()).ok().and_then(|facts| facts.label),
            Some("Document".to_string())
        );
    }
}
