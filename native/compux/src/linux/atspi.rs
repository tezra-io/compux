//! The Linux reader: AT-SPI over D-Bus, behind [`Ax`] (M86 L1).
//!
//! The walk is the shared one in `tree.rs`; this module answers its per-node
//! questions from the accessibility bus, and holds to four rules measured on real
//! toolkits (agent-desktop's ledger):
//!
//! - **Never `Action.GetActions`.** ATK bridge 2.38 (Ubuntu 22.04) aborts the
//!   APPLICATION when asked it. Action names are read as
//!   `NActions`, then `GetName(i)` for each index: the one call every toolkit
//!   answers with the machine name (L3-20). An index that errors is left out, and
//!   the control says its actions are partial.
//! - **Never `Properties.GetAll`.** It kills a Qt 6.4 application (L41-7).
//!   Every property is a single `Get`, and an optional interface's are read only
//!   after `GetInterfaces` lists it.
//! - **An error name is not proof of absence.** `UnknownObject` is what a
//!   destroyed object answers, and also what a live one answers for something it
//!   does not implement (L17-2). Whether a node is there comes from its own
//!   `GetRole`, which every live node of every toolkit answers (L17-11); whether it
//!   is still in the tree comes from reaching it through its parent's child list,
//!   because a destroyed GTK3 or Chromium object keeps answering for about 16 s
//!   (L17-6, L17-8). `ServiceUnknown` means the application is gone.
//! - **Concurrency.** A node's reads go out together, and up to [`IN_FLIGHT`]
//!   nodes are read at once (L2-5: 2.5 to 3.5 times faster than one call at a
//!   time). The walker asks for one node at a time, so the first question about a
//!   node that came in a page of children reads it together with the siblings
//!   after it, and answers them from that read when the walker reaches them.
//!
//! Every node a walk reads is remembered as the read left it ([`Known`]): which
//! verbs it has a route for is decided from that read (`routes`), so a listing
//! costs no message per route. Its writes are in `acting`, and finding a reference
//! again before an action in `identity`.
//!
//! Calls block on `async_io::block_on` from compux's one action worker, each held
//! to a read's timeout ([`ax::READ_TIMEOUT_MS`]) or its write's; there is no runtime
//! of our own and no tokio.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::time::Duration;

use futures_util::future::{join, join3, join_all, select, Either};
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use zbus::zvariant::{DynamicDeserialize, DynamicType, OwnedObjectPath, OwnedValue};
use zbus::Connection;

use super::display::Endpoint;
use super::routes::{self, Toolkit, CHROMIUM_SWITCH, GTK4_SWITCH, QT_SWITCH};
use super::vocabulary::{
    self, ACTIVE, COMBO_BOX, EDITABLE, ENTRY, EXPANDED, FOCUSED, MULTI_LINE, PASSWORD_TEXT,
    SCROLL_PANE, SHOWING, TEXT as TEXT_ROLE,
};
use super::{bus, session};
use crate::ax::{
    self, Ax, Facts, Focus, Frame, Handle, Identity, Refusal, Retained, Route, Routes, States,
    Unread, Write,
};
use crate::roles;

pub(super) const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
pub(super) const ACTION: &str = "org.a11y.atspi.Action";
pub(super) const COMPONENT: &str = "org.a11y.atspi.Component";
pub(super) const TEXT: &str = "org.a11y.atspi.Text";
pub(super) const VALUE: &str = "org.a11y.atspi.Value";
pub(super) const EDITABLE_TEXT: &str = "org.a11y.atspi.EditableText";
pub(super) const SELECTION: &str = "org.a11y.atspi.Selection";
const APPLICATION: &str = "org.a11y.atspi.Application";
pub(super) const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const BUS_DAEMON: &str = "org.freedesktop.DBus";
const BUS_DAEMON_PATH: &str = "/org/freedesktop/DBus";

/// What a child slot holds when there is no child in it.
const NULL_PATH: &str = "/org/a11y/atspi/null";

/// The error names that say something on their own.
const SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";
const NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";
const UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const UNKNOWN_INTERFACE: &str = "org.freedesktop.DBus.Error.UnknownInterface";

/// How many nodes are read at once (L2-5: 16 in flight was within 5–15% of 128).
pub const IN_FLIGHT: usize = 8;

/// How many of one control's actions are named. More is a control that says its
/// actions are partial.
const MAX_ACTIONS: usize = 16;

/// How many applications, and how many windows of one, are looked at for the one
/// in front.
const MAX_APPLICATIONS: usize = 256;
pub(super) const MAX_WINDOWS: usize = 64;

/// How much of a field's text is read. The wire carries 80 characters of it; this
/// leaves room for the whitespace that is folded away first, without reading a
/// document to name a field.
const TEXT_READ_CHARS: i32 = 4096;

/// A nameless row is named by a label at most this deep inside it, among the first
/// [`NAMING_CHILDREN`] children of each node on the way: GTK puts the label
/// straight inside the row, or inside the one widget the row holds.
const NAMING_DEPTH: usize = 2;
const NAMING_CHILDREN: usize = 3;

/// How many nodes the search for the focused element reads, at most.
const MAX_FOCUS_SEARCH: usize = 512;

/// `ATSPI_COORD_TYPE_SCREEN`: extents in the screen's own coordinates.
const SCREEN: u32 = 0;

/// `ATSPI_RELATION_LABELLED_BY`.
const LABELLED_BY: u32 = 2;

/// The character that stands for an embedded object in AT-SPI text: Chromium
/// reads a `<select>`'s value as one. It names nothing.
const EMBEDDED_OBJECT: char = '\u{FFFC}';

/// What one failed call says on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failed {
    /// The application left the bus.
    Quit,
    /// What a destroyed object answers — and a live one, for what it does not
    /// implement (L17-2). Alone it proves nothing.
    Suspect,
    /// No answer within the call's timeout.
    Late,
    /// The connection itself failed.
    Lost,
    /// Any other refusal.
    Other,
}

fn failed(error: &zbus::Error) -> Failed {
    match error {
        zbus::Error::MethodError(name, _, _) => match name.as_str() {
            SERVICE_UNKNOWN | NAME_HAS_NO_OWNER => Failed::Quit,
            UNKNOWN_OBJECT | UNKNOWN_METHOD | UNKNOWN_INTERFACE => Failed::Suspect,
            _ => Failed::Other,
        },
        zbus::Error::InputOutput(io) if io.kind() == std::io::ErrorKind::TimedOut => Failed::Late,
        zbus::Error::InputOutput(_) | zbus::Error::Connection(..) => Failed::Lost,
        _ => Failed::Other,
    }
}

/// What a node's own `GetRole` says about the node: it is the one read every
/// live node answers, so a suspect error HERE is the corroboration a destroyed
/// object needs. Any read that said the application quit settles it too.
pub(super) fn alive(role: Result<u32, Failed>, quit: bool) -> Result<u32, Unread> {
    if quit {
        return Err(Unread::Gone);
    }
    role.map_err(|why| match why {
        Failed::Quit | Failed::Suspect => Unread::Gone,
        Failed::Late | Failed::Lost => Unread::NoAnswer,
        Failed::Other => Unread::Refused,
    })
}

/// What any other failed read means: nothing about whether the node is there,
/// unless the application quit.
pub(super) fn unread(why: Failed) -> Unread {
    match why {
        Failed::Quit => Unread::Gone,
        Failed::Late | Failed::Lost => Unread::NoAnswer,
        Failed::Suspect | Failed::Other => Unread::Refused,
    }
}

/// Where one accessible is: its application's bus name and its object path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Address {
    pub bus: String,
    pub path: String,
}

impl Address {
    pub fn new(bus: &str, path: &str) -> Address {
        Address {
            bus: bus.to_string(),
            path: path.to_string(),
        }
    }

    /// The root of the application on this one's bus.
    pub fn root(&self) -> Address {
        Address::new(&self.bus, bus::ROOT_PATH)
    }

    /// A child slot's answer, or `None` for the slot that holds nothing.
    pub fn of_child((bus, path): (String, OwnedObjectPath)) -> Option<Address> {
        let path = path.to_string();
        (!bus.is_empty() && path != NULL_PATH).then_some(Address { bus, path })
    }
}

/// The interfaces a node said it has, as far as this reader asks them anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Interfaces {
    pub component: bool,
    pub action: bool,
    pub text: bool,
    pub value: bool,
    pub editable_text: bool,
    pub selection: bool,
}

impl Interfaces {
    fn of(names: &[String]) -> Interfaces {
        let listed = |wanted: &str| names.iter().any(|name| name == wanted);
        Interfaces {
            component: listed(COMPONENT),
            action: listed(ACTION),
            text: listed(TEXT),
            value: listed(VALUE),
            editable_text: listed(EDITABLE_TEXT),
            selection: listed(SELECTION),
        }
    }
}

/// What the latest read of a node said: everything its routes, its value and its
/// writes are decided from.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Known {
    pub role: u32,
    /// The state words `GetState` answered.
    pub states: Rc<[u32]>,
    pub interfaces: Interfaces,
    /// Its actions that read, by index.
    pub actions: Rc<[(i32, String)]>,
    pub toolkit: Toolkit,
    pub scrolled: bool,
    pub frame: Option<Frame>,
    /// How many children it said it has.
    pub children: usize,
    /// What it was called, as its facts say.
    pub label: Option<String>,
}

impl Known {
    pub fn read(&self) -> routes::Read<'_> {
        routes::Read {
            role: self.role,
            states: &self.states,
            component: self.interfaces.component,
            text: self.interfaces.text,
            value: self.interfaces.value,
            editable_text: self.interfaces.editable_text,
            actions: &self.actions,
            toolkit: self.toolkit,
            scrolled: self.scrolled,
            children: self.children,
        }
    }

    fn editable(&self) -> bool {
        vocabulary::has(&self.states, EDITABLE)
    }
}

/// Where a node was reached, and what it inherits from there.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Place {
    /// Its child-index path from the application root, where it is known: how a
    /// reference is found again before an action.
    pub path: Option<Rc<[usize]>>,
    /// It sits inside a scroll pane.
    pub scrolled: bool,
    /// The nearest scroll pane's frame: a node wholly outside it is not on screen.
    pub clip: Option<Frame>,
}

impl Place {
    /// The place of a node at the top of its application: one of its windows.
    pub fn top(index: usize) -> Place {
        Place {
            path: Some(Rc::from([index])),
            ..Place::default()
        }
    }

    /// The place of child `index` of a node at this place that read as `known`.
    fn child(&self, known: Option<&Known>, index: usize) -> Place {
        let path = self
            .path
            .as_ref()
            .map(|path| path.iter().copied().chain([index]).collect());
        let pane = known.filter(|known| known.role == SCROLL_PANE);
        Place {
            path,
            scrolled: self.scrolled || pane.is_some(),
            clip: pane.and_then(|pane| pane.frame).or(self.clip),
        }
    }
}

/// The reads every live node answers, each with its own answer.
struct Universal {
    role: Result<u32, Failed>,
    name: Result<Option<String>, Failed>,
    description: Result<Option<String>, Failed>,
    states: Result<Vec<u32>, Failed>,
    interfaces: Result<Vec<String>, Failed>,
    count: Result<usize, Failed>,
}

impl Universal {
    /// Whether any read other than the role failed this way.
    fn failed(&self, why: Failed) -> bool {
        [
            self.name.as_ref().err(),
            self.description.as_ref().err(),
            self.states.as_ref().err(),
            self.interfaces.as_ref().err(),
            self.count.as_ref().err(),
        ]
        .contains(&Some(&why))
    }
}

/// One read of one node: its facts and its child count, each with its own answer.
struct Read {
    facts: Result<Facts, Unread>,
    count: Result<usize, Unread>,
    known: Option<Known>,
}

impl Read {
    fn unanswered() -> Read {
        Read {
            facts: Err(Unread::NoAnswer),
            count: Err(Unread::NoAnswer),
            known: None,
        }
    }
}

/// One reference this reader handed out.
pub(super) struct Held {
    pub at: Address,
    pub place: Place,
    /// What the read made for it answered, until the question it answers is asked.
    facts: Option<Result<Facts, Unread>>,
    count: Option<Result<usize, Unread>>,
    pub known: Option<Known>,
    /// Not read yet, so the next read of a sibling before it may take it along.
    unread: bool,
    /// The page of siblings it came in, and its place there.
    page: Rc<[u64]>,
    index: usize,
}

/// The action names one control advertises, by index, and whether that is all of
/// them.
struct Actions {
    names: Vec<(i32, String)>,
    partial: bool,
}

/// The reader. One per worker; everything it holds is that thread's.
pub struct Real {
    /// The accessibility bus, opened on first use and let go of when the
    /// connection fails, so the next question opens it again.
    bus: RefCell<Option<Connection>>,
    open: fn() -> Result<Connection, String>,
    pub(super) held: RefCell<HashMap<u64, Held>>,
    next: Cell<u64>,
    messages: Cell<u64>,
    /// The toolkit of each application (by bus name), as last learned when it was
    /// looked up ([`Real::learn_toolkit`]).
    toolkits: RefCell<HashMap<String, Toolkit>>,
    /// Whether this session places nodes on the screen at all: an X11 session
    /// does, a Wayland session gives no client a global position, and a display
    /// taken from the user manager draws nothing. Decided on the first read that
    /// needs it, from `display`.
    places: Cell<Option<bool>>,
    /// Where this process's display is.
    display: Endpoint,
}

impl Default for Real {
    fn default() -> Real {
        Real::new()
    }
}

/// Open this session's accessibility bus. Every call is held to its own timeout
/// as well ([`Real::call_within`]); the connection's is a write's whole budget.
fn open_session_bus() -> Result<Connection, String> {
    bus::connect(Duration::from_millis(ax::WRITE_BUDGET_MS))
}

/// The timeout a read is held to.
pub(super) fn read_timeout() -> Duration {
    Duration::from_millis(ax::READ_TIMEOUT_MS)
}

impl Real {
    /// A reader of this session's accessibility bus, which it opens on the first
    /// question — never at start, so a handshake touches no bus.
    pub fn new() -> Real {
        Real::on(Endpoint::own())
    }

    /// A reader on this display, which opens its bus on the first question.
    pub fn on(display: Endpoint) -> Real {
        Real {
            display,
            ..Real::with(None, open_session_bus, None)
        }
    }

    /// A reader of a bus already open, which never opens another, on a session
    /// that places its nodes.
    #[cfg(test)]
    pub fn over(bus: Connection) -> Real {
        fn none() -> Result<Connection, String> {
            Err("this reader reads only the bus it was given".to_string())
        }
        Real::with(Some(bus), none, Some(true))
    }

    /// The same reader on a session that places nothing (Wayland).
    #[cfg(test)]
    pub fn without_positions(self) -> Real {
        self.places.set(Some(false));
        self
    }

    /// The same reader on this display, deciding afresh whether it places nodes.
    #[cfg(test)]
    pub fn on_display(self, display: Endpoint) -> Real {
        self.places.set(None);
        Real { display, ..self }
    }

    fn with(
        bus: Option<Connection>,
        open: fn() -> Result<Connection, String>,
        places: Option<bool>,
    ) -> Real {
        Real {
            bus: RefCell::new(bus),
            open,
            held: RefCell::new(HashMap::new()),
            next: Cell::new(0),
            messages: Cell::new(0),
            toolkits: RefCell::new(HashMap::new()),
            places: Cell::new(places),
            display: Endpoint::default(),
        }
    }

    /// The bus, opening it if it is not open. A bus that will not open is said on
    /// stderr, and the question it was for answers as if nothing is there.
    pub(super) fn bus(&self) -> Option<Connection> {
        if let Some(bus) = self.bus.borrow().as_ref() {
            return Some(bus.clone());
        }
        match (self.open)() {
            Ok(bus) => {
                self.bus.replace(Some(bus.clone()));
                Some(bus)
            }
            Err(why) => {
                eprintln!("compux: accessibility bus: {why}");
                None
            }
        }
    }

    /// Whether this session places nodes, deciding it the first time it is asked.
    fn places_nodes(&self) -> bool {
        if let Some(places) = self.places.get() {
            return places;
        }
        let places = self.display.places();
        if !places {
            eprintln!(
                "compux: this session gives no screen positions: controls are listed without them"
            );
        }
        self.places.set(Some(places));
        places
    }

    /// One call, held to a read's timeout.
    pub(super) async fn call<B, R>(
        &self,
        bus: &Connection,
        at: &Address,
        interface: &str,
        member: &str,
        body: &B,
    ) -> Result<R, Failed>
    where
        B: Serialize + DynamicType,
        R: for<'d> DynamicDeserialize<'d>,
    {
        self.call_within(bus, at, (interface, member), body, read_timeout())
            .await
    }

    /// One call, counted before it is sent (M7), and given up on as `Late` once
    /// `limit` has passed. A call given up on may still have arrived.
    pub(super) async fn call_within<B, R>(
        &self,
        bus: &Connection,
        at: &Address,
        (interface, member): (&str, &str),
        body: &B,
        limit: Duration,
    ) -> Result<R, Failed>
    where
        B: Serialize + DynamicType,
        R: for<'d> DynamicDeserialize<'d>,
    {
        self.messages.set(self.messages.get() + 1);
        let reply = std::pin::pin!(bus.call_method(
            Some(at.bus.as_str()),
            at.path.as_str(),
            Some(interface),
            member,
            body,
        ));
        let timer = std::pin::pin!(async_io::Timer::after(limit));
        let reply = match select(reply, timer).await {
            Either::Left((reply, _)) => reply,
            Either::Right(_) => return Err(Failed::Late),
        };
        let answer = reply.and_then(|message| message.body().deserialize::<R>());
        answer.map_err(|error| self.failure(&error))
    }

    /// Classify a failed call, and let go of a connection that failed.
    fn failure(&self, error: &zbus::Error) -> Failed {
        let why = failed(error);
        if why == Failed::Lost {
            self.bus.replace(None);
        }
        why
    }

    /// One property, by itself: never `GetAll`.
    pub(super) async fn property<T>(
        &self,
        bus: &Connection,
        at: &Address,
        interface: &str,
        name: &str,
    ) -> Result<T, Failed>
    where
        T: TryFrom<OwnedValue>,
    {
        self.property_within(bus, at, (interface, name), read_timeout())
            .await
    }

    /// One property, by itself, given up on as `Late` once `limit` has passed.
    pub(super) async fn property_within<T>(
        &self,
        bus: &Connection,
        at: &Address,
        (interface, name): (&str, &str),
        limit: Duration,
    ) -> Result<T, Failed>
    where
        T: TryFrom<OwnedValue>,
    {
        let value: OwnedValue = self
            .call_within(bus, at, (PROPERTIES, "Get"), &(interface, name), limit)
            .await?;
        T::try_from(value).map_err(|_| Failed::Other)
    }

    pub(super) async fn accessible<R>(
        &self,
        bus: &Connection,
        at: &Address,
        member: &str,
    ) -> Result<R, Failed>
    where
        R: for<'d> DynamicDeserialize<'d>,
    {
        self.call(bus, at, ACCESSIBLE, member, &()).await
    }

    /// Text that names a node; an empty one names nothing.
    async fn words(
        &self,
        bus: &Connection,
        at: &Address,
        name: &str,
    ) -> Result<Option<String>, Failed> {
        let text: String = self.property(bus, at, ACCESSIBLE, name).await?;
        Ok(Some(text).filter(|text| !text.trim().is_empty()))
    }

    async fn count(&self, bus: &Connection, at: &Address) -> Result<usize, Failed> {
        let count: i32 = self.property(bus, at, ACCESSIBLE, "ChildCount").await?;
        usize::try_from(count).map_err(|_| Failed::Other)
    }

    /// The six reads every live node answers (L17-11), sent together.
    async fn universal(&self, bus: &Connection, at: &Address) -> Universal {
        let ((role, name, description), (states, interfaces, count)) = join(
            join3(
                self.accessible::<u32>(bus, at, "GetRole"),
                self.words(bus, at, "Name"),
                self.words(bus, at, "Description"),
            ),
            join3(
                self.accessible::<Vec<u32>>(bus, at, "GetState"),
                self.accessible::<Vec<String>>(bus, at, "GetInterfaces"),
                self.count(bus, at),
            ),
        )
        .await;
        Universal {
            role,
            name,
            description,
            states,
            interfaces,
            count,
        }
    }

    /// Read one node: the universal reads, then extents and action names only where
    /// its interfaces say it has them. Whether it is there is its role's to say
    /// (an error name alone proves nothing); a read that did not come back in time
    /// makes the node one that did not answer, never one that is not on screen.
    async fn read(&self, bus: &Connection, at: &Address, place: &Place) -> Read {
        let first = self.universal(bus, at).await;
        let alive = alive(first.role, first.failed(Failed::Quit));
        let count = match alive {
            Err(Unread::Gone) => Err(Unread::Gone),
            _ => first.count.map_err(unread),
        };
        let role = match alive {
            Ok(_) if first.failed(Failed::Late) => Err(Unread::NoAnswer),
            other => other,
        };
        let Ok(role) = role else {
            return Read {
                facts: role.map(|_| Facts::default()),
                count,
                known: None,
            };
        };
        self.read_answered(bus, at, place, role, first, count).await
    }

    /// The rest of a node that answered its role: where it is, what it does, what
    /// it is called, and whether its children are walked.
    async fn read_answered(
        &self,
        bus: &Connection,
        at: &Address,
        place: &Place,
        role: u32,
        first: Universal,
        count: Result<usize, Unread>,
    ) -> Read {
        let states: Rc<[u32]> = first.states.unwrap_or_default().into();
        let interfaces = Interfaces::of(&first.interfaces.unwrap_or_default());
        let toolkit = self.toolkit(&at.bus);
        let placed = self.places_nodes() && toolkit != Toolkit::Gtk4;
        let own = (first.name.ok().flatten(), first.description.ok().flatten());
        let ((frame, actions), named) = join(
            join(
                self.frame(bus, at, interfaces, &states, placed),
                self.actions(bus, at, interfaces),
            ),
            self.name(bus, at, role, own, count.unwrap_or(0)),
        )
        .await;
        let frame = frame.map(|frame| frame.filter(|frame| inside(frame, place.clip)));
        let known = Known {
            role,
            states,
            interfaces,
            actions: actions.names.into(),
            toolkit,
            scrolled: place.scrolled,
            frame: frame.ok().flatten(),
            children: count.unwrap_or(0),
            label: ax::label(named.0, named.1),
        };
        // A combo box that is not open holds what a click would open, at places it
        // is not (Qt), or the selected option again (Chromium).
        let closed = role == COMBO_BOX && !vocabulary::has(&known.states, EXPANDED);
        let placeless = !placed && vocabulary::has(&known.states, SHOWING);
        Read {
            facts: frame.map(|frame| facts(&known, frame, placeless, actions.partial)),
            count: if closed { Ok(0) } else { count },
            known: Some(known),
        }
    }

    /// Where a node is on the screen: only a node that says it is SHOWING, through
    /// a Component it listed, with an area, on a session and toolkit that place
    /// nodes. Anything else says nowhere; extents that did not come back in time
    /// are a node that did not answer.
    async fn frame(
        &self,
        bus: &Connection,
        at: &Address,
        interfaces: Interfaces,
        states: &[u32],
        placed: bool,
    ) -> Result<Option<Frame>, Unread> {
        if !placed || !interfaces.component || !vocabulary::has(states, SHOWING) {
            return Ok(None);
        }
        let extents: Result<(i32, i32, i32, i32), Failed> = self
            .call(bus, at, COMPONENT, "GetExtents", &(SCREEN,))
            .await;
        match extents {
            Ok(extents) => Ok(area(extents)),
            Err(Failed::Late | Failed::Lost) => Err(Unread::NoAnswer),
            Err(_) => Ok(None),
        }
    }

    /// A control's action names, by index, never `GetActions`: `NActions`, then
    /// `GetName(i)` for each.
    async fn actions(&self, bus: &Connection, at: &Address, interfaces: Interfaces) -> Actions {
        if !interfaces.action {
            return Actions {
                names: Vec::new(),
                partial: false,
            };
        }
        let Ok(count) = self.property::<i32>(bus, at, ACTION, "NActions").await else {
            return Actions {
                names: Vec::new(),
                partial: true,
            };
        };
        let count = usize::try_from(count).unwrap_or(0);
        let asked = i32::try_from(count.min(MAX_ACTIONS)).unwrap_or(0);
        let named = join_all((0..asked).map(|index| async move {
            let name: Result<String, Failed> =
                self.call(bus, at, ACTION, "GetName", &(index,)).await;
            name.map(|name| (index, name))
        }))
        .await;
        let names: Vec<(i32, String)> = named
            .into_iter()
            .filter_map(Result::ok)
            .filter(|(_, name)| !name.is_empty())
            .collect();
        Actions {
            partial: names.len() < count,
            names,
        }
    }

    /// What a node is called: its own name and description, except that a combo
    /// box goes by the label it is labelled by (GTK 4 and Qt name it by its current
    /// item), and a nameless row by the first label inside it.
    async fn name(
        &self,
        bus: &Connection,
        at: &Address,
        role: u32,
        own: (Option<String>, Option<String>),
        count: usize,
    ) -> (Option<String>, Option<String>) {
        if role == COMBO_BOX {
            if let Some(label) = self.labelled_by(bus, at).await {
                return (Some(label), own.1);
            }
        }
        if vocabulary::row(role) && own.0.is_none() && own.1.is_none() && count > 0 {
            return (self.label_inside(bus, at, count).await, None);
        }
        own
    }

    /// The name of the first node this one says it is labelled by.
    async fn labelled_by(&self, bus: &Connection, at: &Address) -> Option<String> {
        let relations: Vec<(u32, Vec<(String, OwnedObjectPath)>)> =
            self.accessible(bus, at, "GetRelationSet").await.ok()?;
        let (_, targets) = relations
            .into_iter()
            .find(|(kind, targets)| *kind == LABELLED_BY && !targets.is_empty())?;
        let label = targets.into_iter().find_map(Address::of_child)?;
        self.words(bus, &label, "Name").await.ok().flatten()
    }

    /// The first label inside a node, [`NAMING_DEPTH`] levels down at most, looking
    /// at the first [`NAMING_CHILDREN`] children of each node on the way.
    async fn label_inside(&self, bus: &Connection, at: &Address, count: usize) -> Option<String> {
        let mut level = vec![(at.clone(), count)];
        for _ in 0..NAMING_DEPTH {
            match self.caption_in(bus, level).await {
                ControlFlow::Break(name) => return Some(name),
                ControlFlow::Continue(below) => level = below,
            }
        }
        None
    }

    /// One level of that look: the name of the first caption among its nodes'
    /// children, or the children one level down, each with its child count.
    async fn caption_in(
        &self,
        bus: &Connection,
        level: Vec<(Address, usize)>,
    ) -> ControlFlow<String, Vec<(Address, usize)>> {
        let mut below = Vec::new();
        for (parent, count) in level {
            below.extend(self.caption_among(bus, &parent, count).await?);
        }
        ControlFlow::Continue(below)
    }

    /// The first caption among one node's first [`NAMING_CHILDREN`] children, by
    /// its name; else those children that are not captions, each with its count.
    async fn caption_among(
        &self,
        bus: &Connection,
        parent: &Address,
        count: usize,
    ) -> ControlFlow<String, Vec<(Address, usize)>> {
        let mut inside = Vec::new();
        for index in 0..count.min(NAMING_CHILDREN) {
            let Ok(Some(child)) = self.child_at(bus, parent, index).await else {
                continue;
            };
            let role: Result<u32, Failed> = self.accessible(bus, &child, "GetRole").await;
            if !role.is_ok_and(vocabulary::caption) {
                inside.push((child.clone(), self.count(bus, &child).await.unwrap_or(0)));
                continue;
            }
            if let Ok(Some(name)) = self.words(bus, &child, "Name").await {
                return ControlFlow::Break(name);
            }
        }
        ControlFlow::Continue(inside)
    }

    /// One child slot, `None` when it holds nothing.
    pub(super) async fn child_at(
        &self,
        bus: &Connection,
        at: &Address,
        index: usize,
    ) -> Result<Option<Address>, Failed> {
        Ok(Address::of_child(self.accessible_at(bus, at, index).await?))
    }

    /// Give out a reference for each address, as one page of siblings, each at
    /// its place: `places` names them in order.
    fn mint(
        &self,
        owner: &Rc<dyn Ax>,
        addresses: Vec<Address>,
        places: impl Fn(usize) -> Place,
    ) -> Vec<Retained> {
        let first = self.next.get() + 1;
        self.next.set(self.next.get() + addresses.len() as u64);
        let page: Rc<[u64]> = (first..first + addresses.len() as u64).collect();

        let mut held = self.held.borrow_mut();
        for (index, at) in addresses.into_iter().enumerate() {
            held.insert(
                page[index],
                Held {
                    at,
                    place: places(index),
                    facts: None,
                    count: None,
                    known: None,
                    unread: true,
                    page: page.clone(),
                    index,
                },
            );
        }
        page.iter()
            .map(|id| Retained::new(Handle(*id), owner.clone()))
            .collect()
    }

    /// Give out one reference at a place.
    pub(super) fn mint_one(&self, owner: &Rc<dyn Ax>, at: Address, place: Place) -> Retained {
        let mut minted = self.mint(owner, vec![at], |_| place.clone());
        minted.remove(0)
    }

    /// The places of a parent's children from `start`: each the parent's place,
    /// one index further down.
    fn below(&self, parent: Handle, start: usize) -> impl Fn(usize) -> Place {
        let (place, known) = self
            .held
            .borrow()
            .get(&parent.0)
            .map(|held| (held.place.clone(), held.known.clone()))
            .unwrap_or_default();
        move |index| place.child(known.as_ref(), start + index)
    }

    pub(super) fn address(&self, handle: Handle) -> Option<Address> {
        self.held
            .borrow()
            .get(&handle.0)
            .map(|held| held.at.clone())
    }

    /// This node and up to [`IN_FLIGHT`]` - 1` siblings after it that have not been
    /// read: the next nodes the walker will ask about.
    fn window(&self, handle: Handle) -> Vec<(u64, Address, Place)> {
        let held = self.held.borrow();
        let Some(this) = held.get(&handle.0) else {
            return Vec::new();
        };
        let after = this.page[this.index + 1..].iter().filter_map(|id| {
            let sibling = held.get(id).filter(|sibling| sibling.unread)?;
            Some((*id, sibling.at.clone(), sibling.place.clone()))
        });
        std::iter::once((handle.0, this.at.clone(), this.place.clone()))
            .chain(after.take(IN_FLIGHT - 1))
            .collect()
    }

    /// Read a node, with its window of siblings, and keep each answer for the
    /// question it answers.
    fn read_window(&self, handle: Handle) {
        let window = self.window(handle);
        let reads: Vec<Read> = match self.bus() {
            Some(bus) => async_io::block_on(join_all(
                window
                    .iter()
                    .map(|(_, at, place)| self.read(&bus, at, place)),
            )),
            // No bus to read it from: still there, as far as anyone knows, and not
            // answering.
            None => window.iter().map(|_| Read::unanswered()).collect(),
        };

        let mut held = self.held.borrow_mut();
        for ((id, _, _), read) in window.into_iter().zip(reads) {
            let Some(node) = held.get_mut(&id) else {
                continue;
            };
            node.facts = Some(read.facts);
            node.count = Some(read.count);
            node.known = read.known;
            node.unread = false;
        }
    }

    /// Read one node again now, alone, and keep what it said: what an action
    /// decides from is never older than the action.
    pub(super) fn reread(&self, handle: Handle) -> Result<Facts, Unread> {
        if let Some(held) = self.held.borrow_mut().get_mut(&handle.0) {
            held.unread = true;
            held.facts = None;
        }
        self.read_window(handle);
        self.take(handle, |held| held.facts.take())
            .unwrap_or(Err(Unread::Gone))
    }

    pub(super) fn known(&self, handle: Handle) -> Option<Known> {
        self.held
            .borrow()
            .get(&handle.0)
            .and_then(|held| held.known.clone())
    }

    pub(super) fn place(&self, handle: Handle) -> Option<Place> {
        self.held
            .borrow()
            .get(&handle.0)
            .map(|held| held.place.clone())
    }

    /// The toolkit an application was made with, as last learned.
    fn toolkit(&self, bus: &str) -> Toolkit {
        self.toolkits.borrow().get(bus).copied().unwrap_or_default()
    }

    /// The registered applications, as the registry lists them.
    async fn applications(&self, bus: &Connection) -> Vec<Address> {
        let registry = Address::new(bus::REGISTRY, bus::ROOT_PATH);
        self.children_of(bus, &registry, MAX_APPLICATIONS)
            .await
            .unwrap_or_else(|why| {
                eprintln!("compux: accessibility registry: {why:?}");
                Vec::new()
            })
    }

    /// One node's children, in one `GetChildren`, the first `limit` of them. The
    /// reply is read whole before `limit` applies: only zbus's cap on a message
    /// (128 MiB) bounds it. So this is for the registry, an application's windows,
    /// and a node that will not count its children; a counted list is read a page
    /// at a time (`page`).
    pub(super) async fn children_of(
        &self,
        bus: &Connection,
        at: &Address,
        limit: usize,
    ) -> Result<Vec<Address>, Failed> {
        let listed: Vec<(String, OwnedObjectPath)> =
            self.accessible(bus, at, "GetChildren").await?;
        Ok(listed
            .into_iter()
            .filter_map(Address::of_child)
            .take(limit)
            .collect())
    }

    /// The process behind a bus name, as the bus daemon says.
    async fn process(&self, bus: &Connection, name: &str) -> Option<i32> {
        let daemon = Address::new(BUS_DAEMON, BUS_DAEMON_PATH);
        let pid: Result<u32, Failed> = self
            .call(
                bus,
                &daemon,
                BUS_DAEMON,
                "GetConnectionUnixProcessID",
                &(name,),
            )
            .await;
        pid.ok().and_then(|pid| i32::try_from(pid).ok())
    }

    /// The one registered application this process is. Several would be a guess,
    /// and none is answered.
    async fn application_of(&self, bus: &Connection, pid: i32) -> Option<Address> {
        let applications = self.applications(bus).await;
        let matching: Vec<Address> =
            stream::iter(applications)
                .map(|app| async move {
                    (self.process(bus, &app.bus).await == Some(pid)).then_some(app)
                })
                .buffered(IN_FLIGHT)
                .filter_map(|app| async move { app })
                .collect()
                .await;
        match <[Address; 1]>::try_from(matching) {
            Ok([app]) => Some(app),
            Err(matching) if matching.is_empty() => None,
            Err(matching) => {
                eprintln!(
                    "compux: process {pid} has {} accessible applications",
                    matching.len()
                );
                None
            }
        }
    }

    /// Learn which toolkit an application was made with: GTK 4 has no global
    /// coordinates, on X11 too (it answers every `GetExtents` in screen coordinates
    /// at (0, 0), as measured on GTK 4.14 under Xvfb in a container), and some
    /// routes are one toolkit's. The Application
    /// interface is read only when the root lists it; a toolkit that does not say
    /// is taken at its word.
    async fn learn_toolkit(&self, bus: &Connection, app: &Address) {
        let listed: Result<Vec<String>, Failed> = self.accessible(bus, app, "GetInterfaces").await;
        let named = match listed {
            Ok(listed) if listed.iter().any(|name| name == APPLICATION) => {
                join(
                    self.property::<String>(bus, app, APPLICATION, "ToolkitName"),
                    self.property::<String>(bus, app, APPLICATION, "Version"),
                )
                .await
            }
            _ => (Err(Failed::Other), Err(Failed::Other)),
        };
        let toolkit = match named {
            (Ok(name), Ok(version)) => Toolkit::of(&name, &version),
            _ => Toolkit::Other,
        };
        let before = self.toolkits.borrow_mut().insert(app.bus.clone(), toolkit);
        if toolkit == Toolkit::Gtk4 && before != Some(toolkit) {
            eprintln!(
                "compux: GTK 4 gives no screen positions: its controls are listed without them"
            );
        }
    }

    /// The states of each of an application's windows, in order; a window whose
    /// states will not read reads none.
    async fn window_states(&self, bus: &Connection, app: &Address) -> Vec<(Address, Vec<u32>)> {
        let windows = self
            .children_of(bus, app, MAX_WINDOWS)
            .await
            .unwrap_or_default();
        let states = join_all(
            windows
                .iter()
                .map(|window| self.accessible::<Vec<u32>>(bus, window, "GetState")),
        )
        .await;
        windows
            .into_iter()
            .zip(states)
            .map(|(window, states)| (window, states.unwrap_or_default()))
            .collect()
    }

    /// Whether any window of this application reads ACTIVE. One whose windows will
    /// not list, or whose states will not read, has not said one is: the one in
    /// front is the one that says so.
    async fn has_active_window(&self, bus: &Connection, app: &Address) -> bool {
        self.window_states(bus, app)
            .await
            .iter()
            .any(|(_, words)| vocabulary::has(words, ACTIVE))
    }

    /// The process whose window reads ACTIVE, when exactly one does.
    async fn active_process(&self, bus: &Connection) -> Option<i32> {
        let applications = self.applications(bus).await;
        let active: Vec<Address> = stream::iter(applications)
            .map(|app| async move { self.has_active_window(bus, &app).await.then_some(app) })
            .buffered(IN_FLIGHT)
            .filter_map(|app| async move { app })
            .collect()
            .await;
        match <[Address; 1]>::try_from(active) {
            Ok([app]) => self.process(bus, &app.bus).await,
            Err(active) if active.is_empty() => None,
            Err(active) => {
                eprintln!(
                    "compux: {} applications have a window that reads active",
                    active.len()
                );
                None
            }
        }
    }

    /// One page of a node's children, by index: one `GetChildAtIndex` each, up to
    /// [`IN_FLIGHT`] at once. A slot that holds nothing ends the page early: the
    /// node listed fewer than it counted. A slot that fails fails the page, with
    /// the most telling reason any slot gave.
    async fn page(
        &self,
        bus: &Connection,
        at: &Address,
        start: usize,
        count: usize,
    ) -> Result<Vec<Address>, Unread> {
        let slots: Vec<Result<(String, OwnedObjectPath), Failed>> =
            stream::iter(start..start + count)
                .map(|index| self.accessible_at(bus, at, index))
                .buffered(IN_FLIGHT)
                .collect()
                .await;
        if let Some(why) = worst(slots.iter().filter_map(|slot| slot.as_ref().err().copied())) {
            return Err(why);
        }
        Ok(slots
            .into_iter()
            .filter_map(Result::ok)
            .map_while(Address::of_child)
            .collect())
    }

    async fn accessible_at(
        &self,
        bus: &Connection,
        at: &Address,
        index: usize,
    ) -> Result<(String, OwnedObjectPath), Failed> {
        let index = i32::try_from(index).map_err(|_| Failed::Other)?;
        self.call(bus, at, ACCESSIBLE, "GetChildAtIndex", &(index,))
            .await
    }

    /// A listed control's value: a number through `Value` or text through `Text`,
    /// each only where its interface is listed. Never a password field's.
    async fn value_of(&self, bus: &Connection, at: &Address, known: &Known) -> Option<String> {
        if known.role == PASSWORD_TEXT {
            return None;
        }
        if vocabulary::numeric(known.role) && known.interfaces.value {
            let value: f64 = self.property(bus, at, VALUE, "CurrentValue").await.ok()?;
            return number(value);
        }
        if vocabulary::textual(known.role, known.editable()) && known.interfaces.text {
            let text: String = self
                .call(bus, at, TEXT, "GetText", &(0i32, TEXT_READ_CHARS))
                .await
                .ok()?;
            return Some(ax::bounded_value(text.replace(EMBEDDED_OBJECT, "")));
        }
        None
    }

    /// The application's window that reads ACTIVE, and its index among the
    /// application's children.
    async fn active_window(&self, bus: &Connection, app: &Address) -> Option<(usize, Address)> {
        self.window_states(bus, app)
            .await
            .into_iter()
            .enumerate()
            .find(|(_, (_, words))| vocabulary::has(words, ACTIVE))
            .map(|(index, (window, _))| (index, window))
    }

    /// The node inside `top` that reads FOCUSED, and its path below `top`: a
    /// breadth-first look at [`MAX_FOCUS_SEARCH`] nodes at most. AT-SPI has no
    /// question for an application's focused element.
    async fn focused_in(&self, bus: &Connection, top: &Address) -> Option<(Address, Vec<usize>)> {
        let mut level = vec![(top.clone(), Vec::new())];
        let mut read = 0;
        while !level.is_empty() && read < MAX_FOCUS_SEARCH {
            let below = match self.focused_level(bus, level, read).await {
                ControlFlow::Break(found) => return Some(found),
                ControlFlow::Continue(below) => below,
            };
            read += below.len();
            level = below;
        }
        None
    }

    /// One level of that look: the node that reads FOCUSED among its nodes'
    /// children, or every child read, each with its path, `read` nodes having been
    /// read before.
    async fn focused_level(
        &self,
        bus: &Connection,
        level: Vec<(Address, Vec<usize>)>,
        read: usize,
    ) -> ControlFlow<(Address, Vec<usize>), Vec<(Address, Vec<usize>)>> {
        let mut below = Vec::new();
        for (parent, path) in level {
            let room = MAX_FOCUS_SEARCH.saturating_sub(read + below.len());
            below.extend(self.focused_among(bus, &parent, &path, room).await?);
        }
        ControlFlow::Continue(below)
    }

    /// The child of one node that reads FOCUSED, among its first `room` children;
    /// else those children, each with its path.
    async fn focused_among(
        &self,
        bus: &Connection,
        parent: &Address,
        path: &[usize],
        room: usize,
    ) -> ControlFlow<(Address, Vec<usize>), Vec<(Address, Vec<usize>)>> {
        let count = self.count(bus, parent).await.unwrap_or(0);
        let children = self.page(bus, parent, 0, count.min(room)).await;
        let mut read = Vec::new();
        for (index, child) in children.unwrap_or_default().into_iter().enumerate() {
            let child_path = [path, &[index]].concat();
            let states: Vec<u32> = self
                .accessible(bus, &child, "GetState")
                .await
                .unwrap_or_default();
            if vocabulary::has(&states, FOCUSED) {
                return ControlFlow::Break((child, child_path));
            }
            read.push((child, child_path));
        }
        ControlFlow::Continue(read)
    }

    /// Take one kept answer from a reference, if it is held and the answer is kept.
    fn take<T>(&self, handle: Handle, answer: impl FnOnce(&mut Held) -> Option<T>) -> Option<T> {
        self.held.borrow_mut().get_mut(&handle.0).and_then(answer)
    }
}

/// Of several reasons a page failed, the one that says most: an application that
/// quit, then one that did not answer, then a refusal.
fn worst(reasons: impl Iterator<Item = Failed>) -> Option<Unread> {
    reasons.map(unread).min_by_key(|why| match why {
        Unread::Gone => 0,
        Unread::NoAnswer => 1,
        Unread::Refused => 2,
    })
}

/// A number as a person writes it: no `.0` on a whole one.
pub(super) fn number(value: f64) -> Option<String> {
    value.is_finite().then(|| format!("{value}"))
}

/// Extents with an area, as a frame.
pub(super) fn area((x, y, w, h): (i32, i32, i32, i32)) -> Option<Frame> {
    (w > 0 && h > 0).then(|| Frame {
        x: f64::from(x),
        y: f64::from(y),
        w: f64::from(w),
        h: f64::from(h),
    })
}

/// Whether a frame meets the scroll pane it sits in, where it sits in one.
fn inside(frame: &Frame, clip: Option<Frame>) -> bool {
    let Some(clip) = clip else {
        return true;
    };
    frame.x < clip.x + clip.w
        && clip.x < frame.x + frame.w
        && frame.y < clip.y + clip.h
        && clip.y < frame.y + frame.h
}

/// What one node answers, from what its read said: its role, its name and
/// description, its states, where it is and what it does.
fn facts(known: &Known, frame: Option<Frame>, placeless: bool, actions_partial: bool) -> Facts {
    let text = matches!(known.role, TEXT_ROLE | ENTRY);
    let multi_line = text && vocabulary::has(&known.states, MULTI_LINE);
    Facts {
        role: Some(vocabulary::role(known.role)),
        subrole: multi_line.then(|| roles::MULTI_LINE.to_string()),
        label: known.label.clone(),
        // A node whose states would not read did not say it is disabled.
        enabled: known.states.is_empty() || vocabulary::enabled(&known.states),
        secure: known.role == PASSWORD_TEXT,
        actions: routes::seam_actions(&known.read()),
        actions_partial,
        frame,
        placeless,
        // A node whose states would not read did not say it is off screen.
        hidden: !known.states.is_empty() && !vocabulary::has(&known.states, SHOWING),
    }
}

impl Ax for Real {
    fn application(&self, owner: &Rc<dyn Ax>, pid: i32) -> Option<Retained> {
        let bus = self.bus()?;
        let app = async_io::block_on(async {
            let app = self.application_of(&bus, pid).await?;
            self.learn_toolkit(&bus, &app).await;
            Some(app)
        })?;
        Some(self.mint_one(
            owner,
            app,
            Place {
                path: Some(Rc::from([])),
                ..Place::default()
            },
        ))
    }

    fn windows(&self, owner: &Rc<dyn Ax>, pid: i32) -> Vec<Retained> {
        let Some(bus) = self.bus() else {
            return Vec::new();
        };
        let windows = async_io::block_on(async {
            let app = self.application_of(&bus, pid).await?;
            self.learn_toolkit(&bus, &app).await;
            self.children_of(&bus, &app, MAX_WINDOWS).await.ok()
        });
        self.mint(owner, windows.unwrap_or_default(), Place::top)
    }

    // The answer read ahead with a sibling, else a read now. Either way it is read
    // once and taken once: a revalidation later reads the node again.
    fn facts(&self, handle: Handle) -> Result<Facts, Unread> {
        if let Some(facts) = self.take(handle, |held| held.facts.take()) {
            return facts;
        }
        self.read_window(handle);
        self.take(handle, |held| held.facts.take())
            .unwrap_or(Err(Unread::Gone))
    }

    fn identify(&self, handle: Handle) -> Result<Facts, Identity> {
        self.found_again(handle)
    }

    fn value(&self, handle: Handle) -> Option<String> {
        let known = self.known(handle)?;
        let at = self.address(handle)?;
        let bus = self.bus()?;
        async_io::block_on(self.value_of(&bus, &at, &known))
    }

    fn settable(&self, handle: Handle) -> bool {
        self.known(handle)
            .is_some_and(|known| routes::settable(&known.read()))
    }

    fn child_count(&self, handle: Handle) -> Result<usize, Unread> {
        if let Some(count) = self.take(handle, |held| held.count.take()) {
            return count;
        }
        self.read_window(handle);
        self.take(handle, |held| held.count.take())
            .unwrap_or(Err(Unread::Gone))
    }

    fn children(
        &self,
        owner: &Rc<dyn Ax>,
        handle: Handle,
        start: usize,
        count: usize,
    ) -> Result<Vec<Retained>, Unread> {
        let at = self.address(handle).ok_or(Unread::Gone)?;
        let bus = self.bus().ok_or(Unread::NoAnswer)?;
        let page = async_io::block_on(self.page(&bus, &at, start, count))?;
        Ok(self.mint(owner, page, self.below(handle, start)))
    }

    fn children_uncounted(
        &self,
        owner: &Rc<dyn Ax>,
        handle: Handle,
        limit: usize,
    ) -> Result<Vec<Retained>, Unread> {
        let at = self.address(handle).ok_or(Unread::Gone)?;
        let bus = self.bus().ok_or(Unread::NoAnswer)?;
        let children = async_io::block_on(self.children_of(&bus, &at, limit)).map_err(unread)?;
        Ok(self.mint(owner, children, self.below(handle, 0)))
    }

    fn perform(&self, handle: Handle, action: &str, timeout_ms: u64) -> Result<(), Refusal> {
        self.act(handle, action, Duration::from_millis(timeout_ms))
    }

    fn set_value(&self, handle: Handle, value: &str, timeout_ms: u64) -> Result<(), Refusal> {
        self.set(handle, value, Duration::from_millis(timeout_ms))
    }

    fn release(&self, handle: Handle) {
        self.held.borrow_mut().remove(&handle.0);
    }

    fn process_started_at(&self, pid: i32) -> Option<u64> {
        session::started_at(pid)
    }

    fn frontmost_pid(&self) -> Option<i32> {
        let bus = self.bus()?;
        async_io::block_on(self.active_process(&bus))
    }

    fn messages(&self) -> u64 {
        self.messages.get()
    }

    fn places(&self) -> bool {
        self.places_nodes()
    }

    fn tree_switch(&self, pid: i32) -> Option<&'static str> {
        let bus = self.bus()?;
        let toolkit = async_io::block_on(async {
            let app = self.application_of(&bus, pid).await?;
            if !self.toolkits.borrow().contains_key(&app.bus) {
                self.learn_toolkit(&bus, &app).await;
            }
            Some(self.toolkit(&app.bus))
        })?;
        toolkit.switch()
    }

    /// The writes the node's last read gives it a route for: no message.
    fn routes(&self, handle: Handle) -> Routes {
        self.known(handle)
            .map(|known| routes::routes(&known.read()))
            .unwrap_or_default()
    }

    /// Every route comes from the same read, so asking one costs what asking all
    /// does: nothing.
    fn route(&self, handle: Handle, _route: Route) -> Routes {
        self.routes(handle)
    }

    /// The node's states, from one `GetState`.
    fn states(&self, handle: Handle, on: bool) -> Result<States, Unread> {
        let at = self.address(handle).ok_or(Unread::Gone)?;
        let bus = self.bus().ok_or(Unread::NoAnswer)?;
        let words: Vec<u32> =
            async_io::block_on(self.accessible(&bus, &at, "GetState")).map_err(unread)?;
        Ok(vocabulary::states(&words, on))
    }

    /// A container selects its children where it lists Selection.
    fn selects_children(&self, container: Handle) -> bool {
        self.known(container)
            .is_some_and(|known| known.interfaces.selection)
    }

    /// The node's `Parent`, at the place one up from the node's.
    fn parent(&self, owner: &Rc<dyn Ax>, handle: Handle) -> Option<Retained> {
        let at = self.address(handle)?;
        let bus = self.bus()?;
        let parent: (String, OwnedObjectPath) =
            async_io::block_on(self.property(&bus, &at, ACCESSIBLE, "Parent")).ok()?;
        let parent = Address::of_child(parent)?;
        let path = self.place(handle).and_then(|place| place.path);
        let place = Place {
            path: path.and_then(|path| path.split_last().map(|(_, up)| Rc::from(up))),
            ..Place::default()
        };
        let retained = self.mint_one(owner, parent, place);
        // Read now, so what it is — and whether it selects its children — is known
        // before it is asked; the read is kept for the walker's own question.
        self.read_window(retained.handle());
        Some(retained)
    }

    fn write(&self, handle: Handle, write: Write<'_>, timeout_ms: u64) -> Result<(), Refusal> {
        self.written(handle, write, Duration::from_millis(timeout_ms))
    }

    /// Whether the node's window — the application's child its path starts at —
    /// reads ACTIVE. A node whose place is unknown has not said.
    fn in_focused_window(&self, handle: Handle) -> Option<bool> {
        let top = *self.place(handle)?.path?.first()?;
        let at = self.address(handle)?;
        let bus = self.bus()?;
        async_io::block_on(async {
            let window = self.child_at(&bus, &at.root(), top).await.ok()??;
            let words: Vec<u32> = self.accessible(&bus, &window, "GetState").await.ok()?;
            Some(vocabulary::has(&words, ACTIVE))
        })
    }

    /// The application's window that reads ACTIVE, or the node inside it that
    /// reads FOCUSED.
    fn focused(&self, owner: &Rc<dyn Ax>, pid: i32, what: Focus) -> Option<Retained> {
        let bus = self.bus()?;
        let found = async_io::block_on(async {
            let app = self.application_of(&bus, pid).await?;
            self.learn_toolkit(&bus, &app).await;
            let (index, window) = self.active_window(&bus, &app).await?;
            match what {
                Focus::Window => Some((window, vec![index])),
                Focus::Element => {
                    let (element, below) = self.focused_in(&bus, &window).await?;
                    Some((element, std::iter::once(index).chain(below).collect()))
                }
            }
        });
        let (at, path) = found?;
        let place = Place {
            path: Some(path.into()),
            ..Place::default()
        };
        Some(self.mint_one(owner, at, place))
    }
}

/// The application in front and its name, for `elements` to read: the one whose
/// window reads ACTIVE on the accessibility bus.
pub fn frontmost_application(ax: &Rc<dyn Ax>) -> Result<(i32, String), String> {
    let pid = ax.frontmost_pid().ok_or_else(|| {
        format!(
            "no application window reads active on the accessibility bus; an application \
             whose toolkit publishes no tree is not on it: for Qt, {QT_SWITCH}; for Chromium, \
             {CHROMIUM_SWITCH}; and {GTK4_SWITCH}"
        )
    })?;
    let name = session::name(pid).unwrap_or_else(|| format!("pid {pid}"));
    Ok((pid, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::SystemClock;
    use crate::linux::display;
    use crate::linux::fake::{
        self, Call, Desktop, Fake, Life, Node, ACTIVE, EDITABLE, ENABLED, EXPANDABLE, EXPANDED,
        FOCUSABLE, MULTI_LINE, SELECTABLE, SELECTED, SENSITIVE, SHOWING, VISIBLE,
    };
    use crate::tree::{self, Walk};

    use crate::linux::fake::{APP, PID};
    const OTHER: &str = ":1.11";
    const OTHER_PID: i32 = 4712;
    /// Every call's timeout here: long enough for a fake on the same machine, short
    /// enough that a node that never answers costs a test little.
    const TIMEOUT: Duration = Duration::from_millis(200);

    const PUSH_BUTTON: u32 = 43;
    const PANEL: u32 = 39;
    const APPLICATION: u32 = 75;

    fn button(name: &str, x: i32) -> Node {
        Node::of(PUSH_BUTTON)
            .named(name)
            .at(x, 10, 80, 24)
            .acting(&[Ok("click")])
    }

    fn panel(name: &str, children: &[&str]) -> Node {
        Node::of(PANEL).named(name).holding(APP, children)
    }

    fn desktop(top: &[&str], nodes: Vec<(&str, Node)>) -> Desktop {
        fake::one_window(top, nodes)
    }

    fn reader(desktop: Desktop) -> (Fake, Rc<dyn Ax>) {
        let (fake, bus) = Fake::start(desktop, TIMEOUT);
        let ax: Rc<dyn Ax> = Rc::new(Real::over(bus));
        (fake, ax)
    }

    fn walk(ax: &Rc<dyn Ax>) -> Walk {
        tree::walk(ax, PID, &SystemClock::new(), u64::MAX)
    }

    fn labels(found: &Walk) -> Vec<&str> {
        found
            .nodes
            .iter()
            .map(|node| node.label.as_deref().unwrap_or("-"))
            .collect()
    }

    fn calls_to<'a>(calls: &'a [Call], path: &str) -> Vec<&'a Call> {
        calls.iter().filter(|call| call.path == path).collect()
    }

    // --- what is asked: never GetActions, never GetAll ----------------------------

    // Every property is a single Get, action names come by index, and
    // neither GetAll nor GetActions is ever sent — on a tree whose controls
    // advertise actions, which is exactly where a reader would be tempted.
    #[test]
    fn a_walk_reads_single_properties_and_names_actions_by_index() {
        let (fake, ax) = reader(desktop(
            &["/w/bar"],
            vec![
                ("/w/bar", panel("Toolbar", &["/w/bar/save", "/w/bar/open"])),
                ("/w/bar/save", button("Save", 10)),
                (
                    "/w/bar/open",
                    Node::of(PUSH_BUTTON)
                        .named("Open")
                        .at(100, 10, 80, 24)
                        .acting(&[Ok("click"), Ok("menu")]),
                ),
            ],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["Save", "Open"]);
        assert_eq!(found.nodes[0].role, "push button");
        assert_eq!(found.nodes[0].path, vec!["fixture", "Fixture", "Toolbar"]);
        assert_eq!(found.truncated, None);
        let calls = fake.calls();
        for forbidden in ["GetAll", "GetActions"] {
            assert!(
                calls.iter().all(|call| call.member != forbidden),
                "{forbidden} must never be sent"
            );
        }
        let named: Vec<&str> = calls_to(&calls, "/w/bar/open")
            .into_iter()
            .filter(|call| call.member == "GetName")
            .map(|call| call.argument.as_str())
            .collect();
        assert_eq!(named, vec!["0", "1"], "each action by its index");
        assert!(!found.nodes[1].actions_partial);
    }

    // An optional interface is asked nothing unless the node's
    // own interface list names it. Qt's bridge crashes on calls it does not
    // implement.
    #[test]
    fn a_node_is_asked_nothing_of_an_interface_it_does_not_list() {
        let (fake, ax) = reader(desktop(
            &["/w/label"],
            vec![("/w/label", Node::of(29).named("Just text"))],
        ));

        let found = walk(&ax);

        assert!(found.nodes.is_empty(), "a label is no control");
        let asked: Vec<String> = calls_to(&fake.calls(), "/w/label")
            .into_iter()
            .map(|call| format!("{}.{} {}", call.interface, call.member, call.argument))
            .collect();
        assert!(
            asked.contains(&"org.freedesktop.DBus.Properties.Get Name".to_string()),
            "the label was read: {asked:?}"
        );
        for call in &asked {
            assert!(
                call.starts_with("org.a11y.atspi.Accessible.")
                    || call.starts_with("org.freedesktop.DBus.Properties.Get "),
                "only universal reads: {asked:?}"
            );
            assert!(!call.contains("NActions") && !call.contains("CurrentValue"));
        }
    }

    // An index whose name will not read is left out, never guessed, and the
    // control says what it lists is not all it has.
    #[test]
    fn an_action_whose_name_will_not_read_is_left_out_and_the_control_says_so() {
        let (_fake, ax) = reader(desktop(
            &["/w/odd"],
            vec![(
                "/w/odd",
                Node::of(PANEL)
                    .named("Odd")
                    .at(0, 0, 50, 50)
                    .acting(&[Err("org.gtk.GDBus.UnmappedGError.Quark"), Ok("click")]),
            )],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["Odd"], "listed by the action it named");
        assert!(found.nodes[0].actions_partial);
    }

    // --- what a node says about itself ---------------------------------------------

    // L3-6: GTK4 sets SENSITIVE and never ENABLED; a control with neither is
    // disabled.
    #[test]
    fn ledger_l3_6_a_control_is_enabled_by_either_bit_and_disabled_by_neither() {
        let (_fake, ax) = reader(desktop(
            &["/w/gtk4", "/w/off"],
            vec![
                (
                    "/w/gtk4",
                    Node {
                        states: vec![SENSITIVE, SHOWING],
                        ..button("Gtk4", 10)
                    },
                ),
                (
                    "/w/off",
                    Node {
                        states: vec![SHOWING],
                        ..button("Off", 100)
                    },
                ),
            ],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["Gtk4", "Off"]);
        assert!(found.nodes[0].enabled);
        assert!(!found.nodes[1].enabled);
    }

    // A password field (role 40) is listed, and its text is never asked for —
    // not read and thrown away, never sent.
    #[test]
    fn a_password_field_is_listed_and_its_text_is_never_asked_for() {
        let field = |role: u32, name: &str, x: i32| Node {
            interfaces: vec!["Component", "Text", "EditableText"],
            states: vec![ENABLED, SENSITIVE, SHOWING, EDITABLE],
            extents: (x, 40, 120, 24),
            text: "hunter2".to_string(),
            ..Node::of(role).named(name)
        };
        let (fake, ax) = reader(desktop(
            &["/w/name", "/w/secret"],
            vec![
                ("/w/name", field(61, "Name", 10)),
                ("/w/secret", field(PASSWORD_TEXT, "Password", 200)),
            ],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["Name", "Password"]);
        assert_eq!(found.nodes[0].value.as_deref(), Some("hunter2"));
        assert!(found.nodes[1].secure);
        assert_eq!(found.nodes[1].value, None);
        assert!(
            calls_to(&fake.calls(), "/w/secret")
                .iter()
                .all(|call| call.member != "GetText"),
            "a password field's text is never asked for"
        );
    }

    #[test]
    fn a_slider_says_its_number_and_a_read_only_text_says_nothing() {
        let (_fake, ax) = reader(desktop(
            &["/w/volume", "/w/notes"],
            vec![
                (
                    "/w/volume",
                    Node {
                        interfaces: vec!["Component", "Value"],
                        value: 50.0,
                        extents: (10, 80, 200, 24),
                        ..Node::of(vocabulary::SLIDER).named("Volume")
                    },
                ),
                (
                    "/w/notes",
                    Node {
                        interfaces: vec!["Component", "Text"],
                        text: "read only".to_string(),
                        extents: (10, 120, 200, 80),
                        ..Node::of(vocabulary::TEXT).named("Notes")
                    },
                ),
            ],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["Volume", "Notes"]);
        assert_eq!(found.nodes[0].value.as_deref(), Some("50"));
        assert!(ax.settable(found.nodes[0].element.handle()));
        assert_eq!(found.nodes[1].value, None, "content, not a field's value");
        assert!(!ax.settable(found.nodes[1].element.handle()));
    }

    // Where a node is, only while it says it is SHOWING and has an area: a hidden
    // tab's button keeps the place it had, and listing it would offer a click on
    // whatever is there now.
    #[test]
    fn a_control_that_is_not_showing_has_no_frame_and_is_not_listed() {
        let (fake, ax) = reader(desktop(
            &["/w/hidden", "/w/flat", "/w/shown"],
            vec![
                (
                    "/w/hidden",
                    Node {
                        states: vec![ENABLED, SENSITIVE, VISIBLE],
                        ..button("Hidden", 10)
                    },
                ),
                ("/w/flat", button("Flat", 100).at(100, 10, 0, 0)),
                ("/w/shown", button("Shown", 200)),
            ],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["Shown"]);
        assert!(
            calls_to(&fake.calls(), "/w/hidden")
                .iter()
                .all(|call| call.member != "GetExtents"),
            "nothing to measure on a node that is not on screen"
        );
    }

    // --- absence is decided, not inferred from an error name ---------------------

    // A destroyed object answers UnknownObject to its own GetRole too: it is gone,
    // its subtree with it, and a gone node is not a truncation.
    #[test]
    fn an_object_that_was_destroyed_takes_its_subtree_with_it() {
        let (fake, ax) = reader(desktop(
            &["/w/dialog", "/w/ok"],
            vec![
                (
                    "/w/dialog",
                    Node {
                        life: Life::Destroyed,
                        ..panel("Dialog", &["/w/dialog/yes"])
                    },
                ),
                ("/w/dialog/yes", button("Yes", 10)),
                ("/w/ok", button("OK", 100)),
            ],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["OK"]);
        assert_eq!(found.truncated, None);
        assert!(calls_to(&fake.calls(), "/w/dialog/yes").is_empty());
    }

    // L17-2: a LIVE object answers UnknownObject for what it does not implement.
    // Its GetRole answers, so it is there: listed by what did read.
    #[test]
    fn a_live_object_that_answers_unknown_object_is_still_there() {
        let (_fake, ax) = reader(desktop(
            &["/w/qt"],
            vec![(
                "/w/qt",
                Node {
                    description: "Save the file".to_string(),
                    refuses: vec!["Name"],
                    ..button("ignored", 10)
                },
            )],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["Save the file"]);
    }

    // L17-6: a destroyed GTK3 object keeps answering its own reads for ~16 s. What
    // decides it is gone is its parent's list, which no longer holds it — and the
    // walk only ever reaches a node through that list.
    #[test]
    fn an_object_that_left_its_parent_is_not_listed_however_well_it_answers() {
        let (fake, ax) = reader(desktop(
            &["/w/kept"],
            vec![
                ("/w/kept", button("Kept", 10)),
                ("/w/zombie", button("Zombie", 100)),
            ],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["Kept"]);
        assert!(calls_to(&fake.calls(), "/w/zombie").is_empty());
    }

    // ServiceUnknown means the application is gone, whatever the node's own
    // reads said a moment before.
    #[test]
    fn an_application_that_quit_is_gone_and_names_nothing() {
        let (fake, ax) = reader(desktop(&["/w/save"], vec![("/w/save", button("Save", 10))]));
        let app = ax
            .application(&ax, PID)
            .expect("the application is registered");
        fake.quits(APP);

        assert_eq!(ax.facts(app.handle()), Err(Unread::Gone));
        assert_eq!(ax.child_count(app.handle()), Err(Unread::Gone));
    }

    // A node that never answers costs one timeout, is skipped, and the listing says
    // it is incomplete; its siblings are still read.
    #[test]
    fn a_node_that_does_not_answer_in_time_is_skipped_and_the_walk_says_so() {
        let (_fake, ax) = reader(desktop(
            &["/w/slow", "/w/fine"],
            vec![
                (
                    "/w/slow",
                    Node {
                        life: Life::Silent,
                        ..button("Slow", 10)
                    },
                ),
                ("/w/fine", button("Fine", 100)),
            ],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["Fine"]);
        assert_eq!(found.truncated, Some("time"));
    }

    // --- children, concurrency and counting ----------------------------------------

    // Children by count, then by index, a page at a time: never the whole list in
    // one copy, which on a GTK3 table builds an accessible for every row.
    #[test]
    fn children_are_read_by_count_and_index_in_pages() {
        let rows: Vec<String> = (0..130).map(|row| format!("/w/list/{row}")).collect();
        let mut nodes = vec![(
            "/w/list",
            Node::of(31).holding(APP, &rows.iter().map(String::as_str).collect::<Vec<_>>()),
        )];
        for row in &rows {
            nodes.push((row.as_str(), Node::of(32).named(row).at(0, 0, 100, 20)));
        }
        let (fake, ax) = reader(desktop(&["/w/list"], nodes));

        let found = walk(&ax);

        assert_eq!(found.nodes.len(), 130);
        let calls = fake.calls();
        let list = calls_to(&calls, "/w/list");
        assert_eq!(
            list.iter()
                .filter(|call| call.member == "GetChildAtIndex")
                .count(),
            130
        );
        assert!(list.iter().all(|call| call.member != "GetChildren"));
    }

    // A node that will not count its children has them copied once instead.
    #[test]
    fn a_node_that_will_not_count_its_children_has_them_copied_once() {
        let (fake, ax) = reader(desktop(
            &["/w/odd"],
            vec![
                (
                    "/w/odd",
                    Node {
                        refuses: vec!["ChildCount"],
                        ..panel("Odd", &["/w/odd/a"])
                    },
                ),
                ("/w/odd/a", button("A", 10)),
            ],
        ));

        let found = walk(&ax);

        assert_eq!(labels(&found), vec!["A"]);
        assert_eq!(
            calls_to(&fake.calls(), "/w/odd")
                .iter()
                .filter(|call| call.member == "GetChildren")
                .count(),
            1
        );
    }

    // L2-5: up to eight siblings are read at once. The eighth sibling's first read
    // goes out before the first sibling's second round (which waits on its first),
    // and the ninth waits for the walker to reach it.
    #[test]
    fn eight_siblings_are_read_together_and_the_ninth_after_them() {
        let paths: Vec<String> = (0..9).map(|index| format!("/w/b{index}")).collect();
        let nodes = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                (
                    path.as_str(),
                    button(&format!("B{index}"), 10 * index as i32),
                )
            })
            .collect();
        let (fake, ax) = reader(desktop(
            &paths.iter().map(String::as_str).collect::<Vec<_>>(),
            nodes,
        ));

        let found = walk(&ax);

        assert_eq!(found.nodes.len(), 9);
        let calls = fake.calls();
        let at = |path: &str, member: &str| {
            calls
                .iter()
                .position(|call| call.path == path && call.member == member)
                .expect("the call was made")
        };
        assert!(at("/w/b7", "GetRole") < at("/w/b0", "GetExtents"));
        assert!(at("/w/b0", "GetExtents") < at("/w/b8", "GetRole"));
    }

    // A read made ahead answers once. Asked again — the revalidation before an
    // action — the node is read again, so a renamed control is seen as renamed.
    #[test]
    fn a_node_read_ahead_answers_once_and_is_read_again_after() {
        let (fake, ax) = reader(desktop(&["/w/save"], vec![("/w/save", button("Save", 10))]));
        let found = walk(&ax);
        fake.change(APP, "/w/save", |save| save.name = "Save As".to_string());

        let again = ax.facts(found.nodes[0].element.handle());

        assert_eq!(
            again.map(|facts| facts.label),
            Ok(Some("Save As".to_string()))
        );
    }

    // M7: every call sent is counted, so `ax_messages` is the number on the wire.
    #[test]
    fn every_call_is_counted() {
        let (fake, ax) = reader(desktop(&["/w/save"], vec![("/w/save", button("Save", 10))]));

        let _found = walk(&ax);

        assert_eq!(ax.messages(), fake.calls().len() as u64);
    }

    // Every reference given out is let go of when the walk's nodes are.
    #[test]
    fn a_reference_is_held_until_it_is_released() {
        let reader_of = |desktop| {
            let (fake, bus) = Fake::start(desktop, TIMEOUT);
            (fake, Rc::new(Real::over(bus)))
        };
        let (_fake, real) = reader_of(desktop(&["/w/save"], vec![("/w/save", button("Save", 10))]));
        let ax: Rc<dyn Ax> = real.clone();

        let found = walk(&ax);
        assert_eq!(real.held.borrow().len(), 1, "the one listed control");
        drop(found);

        assert!(real.held.borrow().is_empty());
    }

    fn made_with(desktop: &mut Desktop, name: &'static str, version: &'static str) {
        let root = Node::of(APPLICATION)
            .named("fixture")
            .holding(APP, &["/w"])
            .made_with(name, version);
        desktop.put(APP, fake::ROOT, root);
    }

    // GTK 4 answers GetExtents in screen coordinates with every node at (0, 0)
    // (measured on GTK 4.14 under Xvfb in a container; its window-relative extents
    // are right). A frame read
    // that way would aim every click at the corner of the screen, so a GTK 4
    // application's controls are listed with no frame and no click point, and its
    // extents are never asked for. GTK 3's are placed.
    #[test]
    fn a_gtk4_application_has_no_screen_positions_so_its_controls_are_listed_without() {
        let toolkit = |version| {
            let mut desktop = desktop(&["/w/save"], vec![("/w/save", button("Save", 10))]);
            made_with(&mut desktop, "GTK", version);
            reader(desktop)
        };
        let (gtk4_fake, gtk4) = toolkit("4.14.2");
        let (_gtk3_fake, gtk3) = toolkit("3.24.41");

        let listed = walk(&gtk4);
        assert_eq!(labels(&listed), vec!["Save"]);
        assert_eq!(listed.nodes[0].frame, None);
        assert!(gtk4_fake.sent("GetExtents").is_empty());
        assert!(walk(&gtk3).nodes[0].frame.is_some());
    }

    // A Wayland session gives no client a global position, whatever its toolkit:
    // every control is listed without one, and no extents are asked for.
    #[test]
    fn on_a_session_without_positions_every_control_is_listed_without_a_frame() {
        let (fake, bus) = Fake::start(
            desktop(&["/w/save"], vec![("/w/save", button("Save", 10))]),
            TIMEOUT,
        );
        let ax: Rc<dyn Ax> = Rc::new(Real::over(bus).without_positions());

        let listed = walk(&ax);

        assert_eq!(labels(&listed), vec!["Save"]);
        assert_eq!(listed.nodes[0].frame, None);
        assert!(fake.sent("GetExtents").is_empty());
    }

    // A display taken from the user manager is reached with the display and the
    // authority it named, as values: its controls are placed exactly as on the
    // process's own display, because its pixels are reached the same way.
    #[test]
    fn a_reader_on_a_display_taken_from_the_user_manager_places_it_as_its_own() {
        let (_x11, env) = display::answering_x11();
        let window = || desktop(&["/w/save"], vec![("/w/save", button("Save", 10))]);
        let listed_on = |adopted: bool| {
            let (fake, bus) = Fake::start(window(), TIMEOUT);
            let display = Endpoint {
                env: env.clone(),
                adopted,
            };
            let ax: Rc<dyn Ax> = Rc::new(Real::over(bus).on_display(display));
            (walk(&ax), fake.sent("GetExtents").len())
        };

        let (own, own_extents) = listed_on(false);
        let (adopted, adopted_extents) = listed_on(true);

        assert!(own.nodes[0].frame.is_some(), "its own display places it");
        assert_eq!(labels(&adopted), vec!["Save"]);
        assert_eq!(adopted.nodes[0].frame, own.nodes[0].frame);
        assert_eq!(adopted_extents, own_extents);
    }

    // A node that is not SHOWING is not listed where nothing is placed either: it
    // has no frame because it is not on screen, not because nothing is placed.
    #[test]
    fn a_hidden_node_is_not_listed_where_nothing_is_placed() {
        let hidden = Node {
            states: vec![ENABLED, SENSITIVE],
            ..button("Hidden", 10)
        };
        let (_fake, bus) = Fake::start(desktop(&["/w/h"], vec![("/w/h", hidden)]), TIMEOUT);
        let ax: Rc<dyn Ax> = Rc::new(Real::over(bus).without_positions());

        assert!(walk(&ax).nodes.is_empty());
    }

    // AT-SPI's role number does not tell a one-line field from a multi-line one;
    // the MULTI_LINE state does, and the reader says so as the node's subrole.
    #[test]
    fn a_multi_line_text_says_so_beside_its_role() {
        let text = |name: &str, x: i32, bits: &[u32]| {
            Node::of(61)
                .named(name)
                .at(x, 10, 80, 24)
                .reading(&[EDITABLE])
                .reading(bits)
        };
        let (_fake, ax) = reader(desktop(
            &["/w/one", "/w/many"],
            vec![
                ("/w/one", text("One", 10, &[])),
                ("/w/many", text("Many", 100, &[MULTI_LINE])),
            ],
        ));

        let listed = walk(&ax);

        assert_eq!(listed.nodes[0].kind(), "textfield");
        assert_eq!(listed.nodes[0].subrole, None);
        assert_eq!(listed.nodes[1].kind(), "textarea");
        assert_eq!(listed.nodes[1].subrole.as_deref(), Some("multi line"));
    }

    // Chromium's <select> reads its value through Text with U+FFFC standing for the
    // embedded option; the character names nothing and is dropped.
    #[test]
    fn a_text_value_drops_the_embedded_object_character() {
        let select = Node {
            text: "\u{FFFC}Apple\u{FFFC}".to_string(),
            ..Node::of(11)
                .named("Fruit")
                .at(10, 10, 80, 24)
                .listing(&["Text"])
        };
        let (_fake, ax) = reader(desktop(&["/w/select"], vec![("/w/select", select)]));

        let listed = walk(&ax);

        assert_eq!(listed.nodes[0].value.as_deref(), Some("Apple"));
    }

    // GTK's list rows have no name of their own: their text is a label inside
    // them. A nameless row is named by the first label it holds, two levels down
    // at most.
    #[test]
    fn a_nameless_row_is_named_by_the_label_inside_it() {
        let row = |child: &str| Node::of(32).at(10, 10, 80, 20).holding(APP, &[child]);
        let (_fake, ax) = reader(desktop(
            &["/w/list"],
            vec![
                ("/w/list", panel("Fruit", &["/w/apple", "/w/deep"])),
                ("/w/apple", row("/w/apple/label")),
                ("/w/apple/label", Node::of(29).named("Apple")),
                ("/w/deep", row("/w/deep/box")),
                (
                    "/w/deep/box",
                    Node::of(PANEL).holding(APP, &["/w/deep/box/in"]),
                ),
                (
                    "/w/deep/box/in",
                    Node::of(PANEL).holding(APP, &["/w/deep/box/in/label"]),
                ),
                ("/w/deep/box/in/label", Node::of(29).named("Too deep")),
            ],
        ));

        let listed = walk(&ax);

        assert_eq!(labels(&listed), vec!["Apple", "-"]);
    }

    // GTK 4 and Qt name a combo box by its current item. The label it is labelled
    // by names it instead, where it has one.
    #[test]
    fn a_combo_box_is_named_by_the_label_it_is_labelled_by() {
        let combo = Node {
            relations: vec![(
                fake::LABELLED_BY,
                vec![(APP.to_string(), "/w/caption".to_string())],
            )],
            ..Node::of(11).named("First").at(10, 10, 80, 24)
        };
        let (_fake, ax) = reader(desktop(
            &["/w/caption", "/w/combo", "/w/plain"],
            vec![
                ("/w/caption", Node::of(29).named("Fruit picker")),
                ("/w/combo", combo),
                (
                    "/w/plain",
                    Node::of(11).named("Unlabelled").at(100, 10, 80, 24),
                ),
            ],
        ));

        assert_eq!(labels(&walk(&ax)), vec!["Fruit picker", "Unlabelled"]);
    }

    // Qt's closed popup items read SHOWING with false places, and Chromium lists a
    // closed <select>'s option: a combo box is walked into only while it reads
    // EXPANDED.
    #[test]
    fn a_combo_box_that_is_not_expanded_is_not_walked_into() {
        let (fake, ax) = reader(desktop(
            &["/w/combo"],
            vec![
                (
                    "/w/combo",
                    Node::of(11)
                        .named("Fruit")
                        .at(10, 10, 80, 24)
                        .reading(&[EXPANDABLE])
                        .holding(APP, &["/w/combo/first"]),
                ),
                (
                    "/w/combo/first",
                    Node::of(32).named("First").at(0, 0, 80, 20),
                ),
            ],
        ));

        assert_eq!(labels(&walk(&ax)), vec!["Fruit"]);

        fake.change(APP, "/w/combo", |combo| combo.states.push(EXPANDED));
        assert_eq!(labels(&walk(&ax)), vec!["Fruit", "First"]);
    }

    // GTK 4 marks every row of a scrolled list SHOWING. Where positions are
    // trustworthy, a row wholly outside the scroll pane it sits in is not on
    // screen, and is not listed.
    #[test]
    fn a_row_outside_its_scroll_pane_is_not_listed() {
        let row = |name: &str, y: i32| Node::of(32).named(name).at(10, y, 80, 20);
        let (_fake, ax) = reader(desktop(
            &["/w/pane"],
            vec![
                (
                    "/w/pane",
                    Node::of(49)
                        .at(0, 100, 200, 100)
                        .holding(APP, &["/w/pane/view"]),
                ),
                (
                    "/w/pane/view",
                    Node::of(68)
                        .at(0, 100, 200, 4000)
                        .holding(APP, &["/w/in", "/w/edge", "/w/out"]),
                ),
                ("/w/in", row("In", 120)),
                ("/w/edge", row("Edge", 190)),
                ("/w/out", row("Out", 900)),
            ],
        ));

        assert_eq!(labels(&walk(&ax)), vec!["In", "Edge"]);
    }

    // Each listed control lists exactly the verbs its own read gives it a route for:
    // a GTK 3 check box presses and toggles by `click` and takes focus; a field
    // takes its value and text; a tree cell expands by its own action.
    #[test]
    fn a_listed_control_lists_the_verbs_its_own_read_gives_it() {
        let focusable = |node: Node| node.reading(&[FOCUSABLE]);
        let (_fake, ax) = reader(desktop(
            &["/w/check", "/w/field", "/w/cell", "/w/text"],
            vec![
                (
                    "/w/check",
                    focusable(
                        Node::of(7)
                            .named("Bold")
                            .at(10, 10, 80, 24)
                            .acting(&[Ok("click")]),
                    ),
                ),
                (
                    "/w/field",
                    focusable(
                        Node::of(61)
                            .named("Name")
                            .at(10, 40, 80, 24)
                            .reading(&[EDITABLE])
                            .listing(&["Text", "EditableText"]),
                    ),
                ),
                (
                    "/w/cell",
                    Node::of(56)
                        .named("Projects")
                        .at(10, 70, 80, 24)
                        .reading(&[EXPANDABLE])
                        .acting(&[Ok("expand or contract"), Ok("edit"), Ok("activate")]),
                ),
                (
                    "/w/text",
                    Node::of(116)
                        .named("Plain")
                        .at(10, 100, 80, 24)
                        .acting(&[Ok("doDefault")]),
                ),
            ],
        ));

        let listed = walk(&ax);

        assert_eq!(
            labels(&listed),
            vec!["Bold", "Name", "Projects"],
            "doDefault lists nothing"
        );
        assert_eq!(listed.nodes[0].verbs(), vec!["press", "toggle", "focus"]);
        assert_eq!(listed.nodes[1].verbs(), vec!["set_value", "type", "focus"]);
        assert_eq!(listed.nodes[2].verbs(), vec!["press", "expand", "collapse"]);
    }

    // A container that lists Selection selects its children: its items list
    // `select`, and Qt's lists, which do not, do not.
    #[test]
    fn an_item_lists_select_where_its_container_has_selection() {
        let item = |name: &str, y: i32| {
            Node::of(32)
                .named(name)
                .at(10, y, 80, 20)
                .reading(&[SELECTABLE])
        };
        let (_fake, ax) = reader(desktop(
            &["/w/gtk", "/w/qt"],
            vec![
                (
                    "/w/gtk",
                    Node::of(98)
                        .named("Fruit")
                        .listing(&["Selection"])
                        .holding(APP, &["/w/gtk/apple"]),
                ),
                ("/w/gtk/apple", item("Apple", 10)),
                (
                    "/w/qt",
                    Node::of(31).named("History").holding(APP, &["/w/qt/one"]),
                ),
                ("/w/qt/one", item("One", 40).acting(&[Ok("Toggle")])),
            ],
        ));

        let listed = walk(&ax);

        assert_eq!(labels(&listed), vec!["Apple", "One"]);
        assert_eq!(listed.nodes[0].verbs(), vec!["select"]);
        assert!(
            listed.nodes[1].verbs().is_empty(),
            "Qt's Toggle on an item is no press"
        );
    }

    // --- which application -----------------------------------------------------------

    // An application is found by its process, among everything registered.
    #[test]
    fn an_application_is_found_by_its_process() {
        let mut both = desktop(&[], vec![]);
        both.pids.insert(OTHER.to_string(), OTHER_PID as u32);
        both.put(OTHER, fake::ROOT, Node::of(APPLICATION).named("other"));
        both.nodes
            .get_mut(&(fake::REGISTRY.to_string(), fake::ROOT.to_string()))
            .expect("the registry")
            .children
            .push((OTHER.to_string(), fake::ROOT.to_string()));
        let (_fake, ax) = reader(both);

        let other = ax.application(&ax, OTHER_PID).expect("registered");
        let facts = ax.facts(other.handle()).expect("it answers");

        assert_eq!(facts.label.as_deref(), Some("other"));
        assert!(ax.application(&ax, 999).is_none(), "nobody is process 999");
    }

    // The application in front is the one whose window reads ACTIVE.
    #[test]
    fn the_application_in_front_is_the_one_whose_window_reads_active() {
        let (fake, ax) = reader(desktop(&[], vec![]));

        assert_eq!(ax.frontmost_pid(), Some(PID));

        fake.change(APP, "/w", |window| {
            window.states.retain(|bit| *bit != ACTIVE)
        });
        assert_eq!(ax.frontmost_pid(), None, "no window reads active");
    }

    // A node that answers with a refusal is alive and unreadable — not
    // listed, its children still read — and never mistaken for one that is gone.
    #[test]
    fn a_role_that_is_refused_is_a_node_that_is_there() {
        assert_eq!(alive(Err(Failed::Other), false), Err(Unread::Refused));
        assert_eq!(alive(Err(Failed::Suspect), false), Err(Unread::Gone));
        assert_eq!(alive(Err(Failed::Late), false), Err(Unread::NoAnswer));
        assert_eq!(
            alive(Ok(43), true),
            Err(Unread::Gone),
            "the application quit"
        );
        assert_eq!(
            unread(Failed::Suspect),
            Unread::Refused,
            "alone it proves nothing"
        );
    }

    // --- surfaces --------------------------------------------------------------

    // An open menu is the menu inside a window the application put up for it (GTK
    // 3), or a popup menu of its own (Qt); a dialog and an alert are children of
    // the application. A menu that is not showing is not open: GTK 3 keeps a menu's
    // window once it has been opened, and Qt every menu it has made.
    #[test]
    fn an_open_menu_a_dialog_and_an_alert_are_surfaces_and_a_hidden_menu_is_not() {
        const WINDOW: u32 = 69;
        const MENU: u32 = 33;
        const POPUP_MENU: u32 = 41;
        const DIALOG: u32 = 16;
        const ALERT: u32 = 2;
        let hidden = |node: Node| Node {
            states: vec![ENABLED, SENSITIVE],
            ..node
        };
        let mut desktop = desktop(
            &[],
            vec![
                (
                    "/popup",
                    Node::of(WINDOW)
                        .at(10, 10, 100, 200)
                        .holding(APP, &["/popup/menu"]),
                ),
                (
                    "/popup/menu",
                    Node::of(MENU).named("Edit").at(10, 10, 100, 200),
                ),
                (
                    "/kept",
                    hidden(Node::of(WINDOW).holding(APP, &["/kept/menu"])),
                ),
                ("/kept/menu", hidden(Node::of(MENU).named("File"))),
                (
                    "/qt",
                    Node::of(POPUP_MENU).named("Context").at(40, 40, 90, 120),
                ),
                ("/qtkept", hidden(Node::of(POPUP_MENU).named("View"))),
                (
                    "/save",
                    Node::of(DIALOG).named("Save As").at(50, 50, 400, 300),
                ),
                (
                    "/full",
                    Node::of(ALERT).named("Disk full").at(60, 60, 300, 100),
                ),
            ],
        );
        desktop.put(
            APP,
            fake::ROOT,
            Node::of(75).named("fixture").holding(
                APP,
                &["/w", "/popup", "/kept", "/qt", "/qtkept", "/save", "/full"],
            ),
        );
        let (_fake, ax) = reader(desktop);

        let found: Vec<(&str, String, Option<String>)> = crate::semantic::surfaces(&ax, PID)
            .into_iter()
            .map(|surface| (surface.kind, surface.native_kind, surface.label))
            .collect();

        let named =
            |kind, native: &str, label: &str| (kind, native.to_string(), Some(label.to_string()));
        assert_eq!(
            found,
            vec![
                named("menu", "menu", "Edit"),
                named("menu", "popup menu", "Context"),
                named("dialog", "dialog", "Save As"),
                named("alert", "alert", "Disk full"),
            ]
        );
    }

    // GTK 3 puts the menu bar in a box inside the window, and a menu-bar menu is
    // open when it reads SELECTED: the menu itself is the surface.
    #[test]
    fn a_menu_bar_menu_inside_the_window_is_open_while_it_reads_selected() {
        const FILLER: u32 = 20;
        const MENU_BAR: u32 = 34;
        const MENU: u32 = 33;
        const MENU_ITEM: u32 = 35;
        let (fake, ax) = reader(desktop(
            &["/w/box"],
            vec![
                (
                    "/w/box",
                    Node::of(FILLER).holding(APP, &["/w/bar", "/w/body"]),
                ),
                ("/w/bar", Node::of(MENU_BAR).holding(APP, &["/w/actions"])),
                ("/w/body", Node::of(FILLER)),
                (
                    "/w/actions",
                    Node::of(MENU)
                        .named("Actions")
                        .at(0, 0, 60, 20)
                        .reading(&[SELECTABLE])
                        .holding(APP, &["/w/export"]),
                ),
                ("/w/export", Node::of(MENU_ITEM).named("Export Report")),
            ],
        ));
        let kinds = |ax: &Rc<dyn Ax>| -> Vec<(&'static str, Option<String>)> {
            crate::semantic::surfaces(ax, PID)
                .into_iter()
                .map(|surface| (surface.kind, surface.label))
                .collect()
        };
        assert!(kinds(&ax).is_empty(), "no menu reads selected");

        fake.change(APP, "/w/actions", |menu| menu.states.push(SELECTED));

        assert_eq!(kinds(&ax), vec![("menu", Some("Actions".to_string()))]);
    }

    // Qt's menu-bar item is a menu item and never reads SELECTED: its menu is a
    // popup menu below it, kept while closed and open while it reads SHOWING. The
    // items of a closed menu are never read.
    #[test]
    fn a_qt_menu_bar_items_popup_menu_is_open_while_it_reads_showing() {
        const MENU_BAR: u32 = 34;
        const MENU_ITEM: u32 = 35;
        const POPUP_MENU: u32 = 41;
        let closed = |node: Node| Node {
            states: vec![ENABLED, SENSITIVE],
            ..node
        };
        let title = |name: &str, menu: &str| Node::of(MENU_ITEM).named(name).holding(APP, &[menu]);
        let (fake, ax) = reader(desktop(
            &["/w/bar", "/w/body"],
            vec![
                (
                    "/w/bar",
                    Node::of(MENU_BAR).holding(APP, &["/w/actions", "/w/view"]),
                ),
                ("/w/body", Node::of(PANEL)),
                ("/w/actions", title("Actions", "/w/actions/menu")),
                (
                    "/w/actions/menu",
                    closed(Node::of(POPUP_MENU).holding(APP, &["/w/export"])),
                ),
                ("/w/export", Node::of(MENU_ITEM).named("Export Report")),
                ("/w/view", title("View", "/w/view/menu")),
                (
                    "/w/view/menu",
                    closed(Node::of(POPUP_MENU).holding(APP, &["/w/zoom"])),
                ),
                ("/w/zoom", Node::of(MENU_ITEM).named("Zoom")),
            ],
        ));
        let kinds = |ax: &Rc<dyn Ax>| -> Vec<(&'static str, String)> {
            crate::semantic::surfaces(ax, PID)
                .into_iter()
                .map(|surface| (surface.kind, surface.native_kind))
                .collect()
        };
        assert!(kinds(&ax).is_empty(), "no menu reads showing");

        fake.change(APP, "/w/actions", |item| item.states.push(fake::FOCUSED));
        fake.change(APP, "/w/actions/menu", |menu| {
            menu.states.extend([SHOWING, VISIBLE])
        });

        assert_eq!(kinds(&ax), vec![("menu", "popup menu".to_string())]);
        assert!(
            calls_to(&fake.calls(), "/w/zoom").is_empty(),
            "the closed menu's items were read"
        );
    }
}
