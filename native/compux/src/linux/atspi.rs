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
//! Calls block on `async_io::block_on` from compux's one action worker, each with
//! the connection's timeout ([`ax::READ_TIMEOUT_MS`]); there is no runtime of our
//! own and no tokio.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use futures_util::future::{join, join3, join_all};
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use zbus::zvariant::{DynamicDeserialize, DynamicType, OwnedObjectPath, OwnedValue};
use zbus::Connection;

use super::vocabulary::{self, ACTIVE, EDITABLE, PASSWORD_TEXT, SHOWING};
use super::{bus, session};
use crate::ax::{
    self, Ax, Facts, Focus, Frame, Handle, Refusal, Retained, Route, Routes, States, Unread, Write,
};

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const ACTION: &str = "org.a11y.atspi.Action";
const COMPONENT: &str = "org.a11y.atspi.Component";
const TEXT: &str = "org.a11y.atspi.Text";
const VALUE: &str = "org.a11y.atspi.Value";
const EDITABLE_TEXT: &str = "org.a11y.atspi.EditableText";
const APPLICATION: &str = "org.a11y.atspi.Application";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
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
const MAX_WINDOWS: usize = 64;

/// How much of a field's text is read. The wire carries 80 characters of it; this
/// leaves room for the whitespace that is folded away first, without reading a
/// document to name a field.
const TEXT_READ_CHARS: i32 = 4096;

/// `ATSPI_COORD_TYPE_SCREEN`: extents in the screen's own coordinates.
const SCREEN: u32 = 0;

/// What one failed call says on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failed {
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
fn alive(role: Result<u32, Failed>, quit: bool) -> Result<u32, Unread> {
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
fn unread(why: Failed) -> Unread {
    match why {
        Failed::Quit => Unread::Gone,
        Failed::Late | Failed::Lost => Unread::NoAnswer,
        Failed::Suspect | Failed::Other => Unread::Refused,
    }
}

/// Where one accessible is: its application's bus name and its object path.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Address {
    bus: String,
    path: String,
}

impl Address {
    fn new(bus: &str, path: &str) -> Address {
        Address {
            bus: bus.to_string(),
            path: path.to_string(),
        }
    }

    /// A child slot's answer, or `None` for the slot that holds nothing.
    fn of_child((bus, path): (String, OwnedObjectPath)) -> Option<Address> {
        let path = path.to_string();
        (!bus.is_empty() && path != NULL_PATH).then_some(Address { bus, path })
    }
}

/// The interfaces a node said it has, as far as this reader asks them anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Interfaces {
    component: bool,
    action: bool,
    text: bool,
    value: bool,
    editable_text: bool,
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
        }
    }
}

/// What the latest read of a node said about where its value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Known {
    role: u32,
    editable: bool,
    interfaces: Interfaces,
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
struct Held {
    at: Address,
    /// What the read made for it answered, until the question it answers is asked.
    facts: Option<Result<Facts, Unread>>,
    count: Option<Result<usize, Unread>>,
    known: Option<Known>,
    /// Not read yet, so the next read of a sibling before it may take it along.
    unread: bool,
    /// The page of siblings it came in, and its place there.
    page: Rc<[u64]>,
    place: usize,
}

/// The action names one control advertises, and whether that is all of them.
struct Actions {
    names: Vec<String>,
    partial: bool,
}

/// The reader. One per worker; everything it holds is that thread's.
pub struct Real {
    /// The accessibility bus, opened on first use and let go of when the
    /// connection fails, so the next question opens it again.
    bus: RefCell<Option<Connection>>,
    open: fn() -> Result<Connection, String>,
    held: RefCell<HashMap<u64, Held>>,
    next: Cell<u64>,
    messages: Cell<u64>,
    /// The applications (by bus name) whose toolkit gives no screen positions, as
    /// last learned when each was looked up ([`Real::learn_positions`]).
    screenless: RefCell<HashSet<String>>,
}

impl Default for Real {
    fn default() -> Real {
        Real::new()
    }
}

/// Open this session's accessibility bus, every call held to a read's timeout.
fn open_session_bus() -> Result<Connection, String> {
    bus::connect(Duration::from_millis(ax::READ_TIMEOUT_MS))
}

impl Real {
    /// A reader of this session's accessibility bus, which it opens on the first
    /// question — never at start, so a handshake touches no bus.
    pub fn new() -> Real {
        Real::with(None, open_session_bus)
    }

    /// A reader of a bus already open, which never opens another.
    #[cfg(test)]
    pub fn over(bus: Connection) -> Real {
        fn none() -> Result<Connection, String> {
            Err("this reader reads only the bus it was given".to_string())
        }
        Real::with(Some(bus), none)
    }

    fn with(bus: Option<Connection>, open: fn() -> Result<Connection, String>) -> Real {
        Real {
            bus: RefCell::new(bus),
            open,
            held: RefCell::new(HashMap::new()),
            next: Cell::new(0),
            messages: Cell::new(0),
            screenless: RefCell::new(HashSet::new()),
        }
    }

    /// The bus, opening it if it is not open. A bus that will not open is said on
    /// stderr, and the question it was for answers as if nothing is there.
    fn bus(&self) -> Option<Connection> {
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

    /// One call, counted before it is sent (M7).
    async fn call<B, R>(
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
        self.messages.set(self.messages.get() + 1);
        let reply = bus
            .call_method(
                Some(at.bus.as_str()),
                at.path.as_str(),
                Some(interface),
                member,
                body,
            )
            .await;
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
    async fn property<T>(
        &self,
        bus: &Connection,
        at: &Address,
        interface: &str,
        name: &str,
    ) -> Result<T, Failed>
    where
        T: TryFrom<OwnedValue>,
    {
        let value: OwnedValue = self
            .call(bus, at, PROPERTIES, "Get", &(interface, name))
            .await?;
        T::try_from(value).map_err(|_| Failed::Other)
    }

    async fn accessible<R>(&self, bus: &Connection, at: &Address, member: &str) -> Result<R, Failed>
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
    /// (an error name alone proves nothing); a read that did not come back in time makes the node one that did not
    /// answer, never one that is not on screen.
    async fn read(&self, bus: &Connection, at: &Address) -> Read {
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

        let states = first.states.unwrap_or_default();
        let interfaces = Interfaces::of(&first.interfaces.unwrap_or_default());
        let (frame, actions) = join(
            self.frame(bus, at, interfaces, &states),
            self.actions(bus, at, interfaces),
        )
        .await;
        let known = Known {
            role,
            editable: vocabulary::has(&states, EDITABLE),
            interfaces,
        };
        let named = (first.name.ok().flatten(), first.description.ok().flatten());
        Read {
            facts: frame.map(|frame| facts(role, named, &states, frame, actions)),
            count,
            known: Some(known),
        }
    }

    /// Where a node is on the screen: only a node that says it is SHOWING, through
    /// a Component it listed, with an area. Anything else says nowhere; extents that
    /// did not come back in time are a node that did not answer.
    async fn frame(
        &self,
        bus: &Connection,
        at: &Address,
        interfaces: Interfaces,
        states: &[u32],
    ) -> Result<Option<Frame>, Unread> {
        let screenless = self.screenless.borrow().contains(&at.bus);
        if screenless || !interfaces.component || !vocabulary::has(states, SHOWING) {
            return Ok(None);
        }
        let extents: Result<(i32, i32, i32, i32), Failed> = self
            .call(bus, at, COMPONENT, "GetExtents", &(SCREEN,))
            .await;
        let (x, y, w, h) = match extents {
            Ok(extents) => extents,
            Err(Failed::Late | Failed::Lost) => return Err(Unread::NoAnswer),
            Err(_) => return Ok(None),
        };
        Ok((w > 0 && h > 0).then(|| Frame {
            x: f64::from(x),
            y: f64::from(y),
            w: f64::from(w),
            h: f64::from(h),
        }))
    }

    /// A control's action names, by index, never `GetActions`: `NActions`, then `GetName(i)` for
    /// each. Never `GetActions`.
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
        let asked = count.min(MAX_ACTIONS);
        let named = join_all((0..asked).map(|index| async move {
            let index = i32::try_from(index).map_err(|_| Failed::Other)?;
            self.call::<_, String>(bus, at, ACTION, "GetName", &(index,))
                .await
        }))
        .await;
        let names: Vec<String> = named
            .into_iter()
            .filter_map(Result::ok)
            .filter(|name| !name.is_empty())
            .collect();
        Actions {
            partial: names.len() < count,
            names,
        }
    }

    /// Give out a reference for each address, as one page of siblings.
    fn mint(&self, owner: &Rc<dyn Ax>, addresses: Vec<Address>) -> Vec<Retained> {
        let first = self.next.get() + 1;
        self.next.set(self.next.get() + addresses.len() as u64);
        let page: Rc<[u64]> = (first..first + addresses.len() as u64).collect();

        let mut held = self.held.borrow_mut();
        for (place, at) in addresses.into_iter().enumerate() {
            held.insert(
                page[place],
                Held {
                    at,
                    facts: None,
                    count: None,
                    known: None,
                    unread: true,
                    page: page.clone(),
                    place,
                },
            );
        }
        page.iter()
            .map(|id| Retained::new(Handle(*id), owner.clone()))
            .collect()
    }

    fn address(&self, handle: Handle) -> Option<Address> {
        self.held
            .borrow()
            .get(&handle.0)
            .map(|held| held.at.clone())
    }

    /// This node and up to [`IN_FLIGHT`]` - 1` siblings after it that have not been
    /// read: the next nodes the walker will ask about.
    fn window(&self, handle: Handle) -> Vec<(u64, Address)> {
        let held = self.held.borrow();
        let Some(this) = held.get(&handle.0) else {
            return Vec::new();
        };
        let after = this.page[this.place + 1..].iter().filter_map(|id| {
            let sibling = held.get(id).filter(|sibling| sibling.unread)?;
            Some((*id, sibling.at.clone()))
        });
        std::iter::once((handle.0, this.at.clone()))
            .chain(after.take(IN_FLIGHT - 1))
            .collect()
    }

    /// Read a node, with its window of siblings, and keep each answer for the
    /// question it answers.
    fn read_window(&self, handle: Handle) {
        let window = self.window(handle);
        let reads: Vec<Read> = match self.bus() {
            Some(bus) => {
                async_io::block_on(join_all(window.iter().map(|(_, at)| self.read(&bus, at))))
            }
            // No bus to read it from: still there, as far as anyone knows, and not
            // answering.
            None => window.iter().map(|_| Read::unanswered()).collect(),
        };

        let mut held = self.held.borrow_mut();
        for ((id, _), read) in window.into_iter().zip(reads) {
            let Some(node) = held.get_mut(&id) else {
                continue;
            };
            node.facts = Some(read.facts);
            node.count = Some(read.count);
            node.known = read.known;
            node.unread = false;
        }
    }

    fn known(&self, handle: Handle) -> Option<Known> {
        self.held
            .borrow()
            .get(&handle.0)
            .and_then(|held| held.known)
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

    /// One node's children, in one `GetChildren`, the first `limit` of them.
    async fn children_of(
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

    /// Learn whether this application's toolkit gives screen positions. GTK 4 has no
    /// global coordinates, on X11 too: it answers every `GetExtents` in screen
    /// coordinates at (0, 0), so its nodes get no frame (L0(a)). The Application
    /// interface is read only when the root lists it; a toolkit that does not
    /// say is taken at its word.
    async fn learn_positions(&self, bus: &Connection, app: &Address) {
        let listed: Result<Vec<String>, Failed> = self.accessible(bus, app, "GetInterfaces").await;
        let toolkit = match listed {
            Ok(listed) if listed.iter().any(|name| name == APPLICATION) => {
                join(
                    self.property::<String>(bus, app, APPLICATION, "ToolkitName"),
                    self.property::<String>(bus, app, APPLICATION, "Version"),
                )
                .await
            }
            _ => (Err(Failed::Other), Err(Failed::Other)),
        };
        let Ok(version) = toolkit.1.as_ref() else {
            self.screenless.borrow_mut().remove(&app.bus);
            return;
        };
        let name = toolkit.0.unwrap_or_default();
        if !without_screen_positions(&name, version) {
            self.screenless.borrow_mut().remove(&app.bus);
        } else if self.screenless.borrow_mut().insert(app.bus.clone()) {
            eprintln!(
                "compux: {name} {version} gives no screen positions: its controls are not listed"
            );
        }
    }

    /// Whether any window of this application reads ACTIVE. One whose windows will
    /// not list, or whose states will not read, has not said one is: the one in
    /// front is the one that says so.
    async fn has_active_window(&self, bus: &Connection, app: &Address) -> bool {
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
        states
            .into_iter()
            .filter_map(Result::ok)
            .any(|words| vocabulary::has(&words, ACTIVE))
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
    async fn value_of(&self, bus: &Connection, at: &Address, known: Known) -> Option<String> {
        if known.role == PASSWORD_TEXT {
            return None;
        }
        if vocabulary::numeric(known.role) && known.interfaces.value {
            let value: f64 = self.property(bus, at, VALUE, "CurrentValue").await.ok()?;
            return number(value);
        }
        if vocabulary::textual(known.role, known.editable) && known.interfaces.text {
            let text: String = self
                .call(bus, at, TEXT, "GetText", &(0i32, TEXT_READ_CHARS))
                .await
                .ok()?;
            return Some(ax::bounded_value(text));
        }
        None
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
fn number(value: f64) -> Option<String> {
    value.is_finite().then(|| format!("{value}"))
}

/// What one node answers, from what its reads said: its role, its name and
/// description, its states, where it is and what it does.
fn facts(
    role: u32,
    (name, description): (Option<String>, Option<String>),
    states: &[u32],
    frame: Option<Frame>,
    actions: Actions,
) -> Facts {
    Facts {
        role: Some(vocabulary::role(role)),
        subrole: None,
        label: ax::label(name, description),
        // A node whose states would not read did not say it is disabled.
        enabled: states.is_empty() || vocabulary::enabled(states),
        secure: role == PASSWORD_TEXT,
        actions: actions.names,
        actions_partial: actions.partial,
        frame,
    }
}

impl Ax for Real {
    fn application(&self, owner: &Rc<dyn Ax>, pid: i32) -> Option<Retained> {
        let bus = self.bus()?;
        let app = async_io::block_on(async {
            let app = self.application_of(&bus, pid).await?;
            self.learn_positions(&bus, &app).await;
            Some(app)
        })?;
        self.mint(owner, vec![app]).pop()
    }

    fn windows(&self, owner: &Rc<dyn Ax>, pid: i32) -> Vec<Retained> {
        let Some(bus) = self.bus() else {
            return Vec::new();
        };
        let windows = async_io::block_on(async {
            let app = self.application_of(&bus, pid).await?;
            self.children_of(&bus, &app, MAX_WINDOWS).await.ok()
        });
        self.mint(owner, windows.unwrap_or_default())
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

    fn value(&self, handle: Handle) -> Option<String> {
        let known = self.known(handle)?;
        let at = self.address(handle)?;
        let bus = self.bus()?;
        async_io::block_on(self.value_of(&bus, &at, known))
    }

    fn settable(&self, handle: Handle) -> bool {
        let Some(known) = self.known(handle) else {
            return false;
        };
        let text = known.interfaces.editable_text && known.editable;
        let number = known.interfaces.value && vocabulary::adjustable(known.role);
        known.role != PASSWORD_TEXT && (text || number)
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
        Ok(self.mint(owner, page))
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
        Ok(self.mint(owner, children))
    }

    fn perform(&self, _handle: Handle, _action: &str, _timeout_ms: u64) -> Result<(), Refusal> {
        Err(not_yet())
    }

    fn set_value(&self, _handle: Handle, _value: &str, _timeout_ms: u64) -> Result<(), Refusal> {
        Err(not_yet())
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

    /// No write is offered yet: the Linux reader lists controls and acts on none.
    fn routes(&self, _handle: Handle) -> Routes {
        Routes::default()
    }

    fn route(&self, _handle: Handle, _route: Route) -> Routes {
        Routes::default()
    }

    /// The node's states, from one `GetState`.
    fn states(&self, handle: Handle, on: bool) -> Result<States, Unread> {
        let at = self.address(handle).ok_or(Unread::Gone)?;
        let bus = self.bus().ok_or(Unread::NoAnswer)?;
        let words: Vec<u32> =
            async_io::block_on(self.accessible(&bus, &at, "GetState")).map_err(unread)?;
        Ok(vocabulary::states(&words, on))
    }

    fn selects_children(&self, _container: Handle) -> bool {
        false
    }

    fn parent(&self, _owner: &Rc<dyn Ax>, _handle: Handle) -> Option<Retained> {
        None
    }

    fn write(&self, _handle: Handle, _write: Write<'_>, _timeout_ms: u64) -> Result<(), Refusal> {
        Err(not_yet())
    }

    fn in_focused_window(&self, _handle: Handle) -> Option<bool> {
        None
    }

    fn focused(&self, _owner: &Rc<dyn Ax>, _pid: i32, _what: Focus) -> Option<Retained> {
        None
    }
}

impl Real {
    /// Take one kept answer from a reference, if it is held and the answer is kept.
    fn take<T>(&self, handle: Handle, answer: impl FnOnce(&mut Held) -> Option<T>) -> Option<T> {
        self.held.borrow_mut().get_mut(&handle.0).and_then(answer)
    }
}

/// GTK 4: the toolkit with no global coordinates.
fn without_screen_positions(toolkit: &str, version: &str) -> bool {
    toolkit.eq_ignore_ascii_case("gtk") && version.split('.').next() == Some("4")
}

/// Nothing was sent: the Linux reader reads, and acts in a later stage (M86 L1).
fn not_yet() -> Refusal {
    Refusal::new(
        false,
        "accessibility actions are not supported on Linux yet",
    )
}

/// The application in front and its name, for `elements` to read: the one whose
/// window reads ACTIVE on the accessibility bus.
pub fn frontmost_application(ax: &Rc<dyn Ax>) -> Result<(i32, String), String> {
    let pid = ax
        .frontmost_pid()
        .ok_or("no application window reads active on the accessibility bus")?;
    let name = session::name(pid).unwrap_or_else(|| format!("pid {pid}"));
    Ok((pid, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::SystemClock;
    use crate::linux::fake::{
        self, Call, Desktop, Fake, Life, Node, ACTIVE, EDITABLE, ENABLED, SENSITIVE, SHOWING,
        VISIBLE,
    };
    use crate::tree::{self, Walk};

    const APP: &str = ":1.10";
    const OTHER: &str = ":1.11";
    const PID: i32 = 4711;
    const OTHER_PID: i32 = 4712;
    /// Every call's timeout here: long enough for a fake on the same machine, short
    /// enough that a node that never answers costs a test little.
    const TIMEOUT: Duration = Duration::from_millis(200);

    const PUSH_BUTTON: u32 = 43;
    const FRAME: u32 = 23;
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

    /// One application (`APP`, process `PID`) with one active window `/w` holding
    /// `top`, and every other node at its path.
    fn desktop(top: &[&str], nodes: Vec<(&str, Node)>) -> Desktop {
        let mut desktop = Desktop::with_applications(vec![(
            APP,
            PID as u32,
            Node::of(APPLICATION).named("fixture").holding(APP, &["/w"]),
        )]);
        let window = Node {
            states: vec![ACTIVE, ENABLED, SENSITIVE, SHOWING, VISIBLE],
            ..Node::of(FRAME)
                .named("Fixture")
                .at(0, 0, 800, 600)
                .holding(APP, top)
        };
        desktop.put(APP, "/w", window);
        for (path, node) in nodes {
            desktop.put(APP, path, node);
        }
        desktop
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
    fn a_control_is_enabled_by_either_bit_and_disabled_by_neither() {
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

    // GTK 4 answers GetExtents in screen coordinates with every node at (0, 0)
    // (L0(a): GTK 4.14 on X11; its window-relative extents are right). A frame read
    // that way would aim every click at the corner of the screen, so a GTK 4
    // application's controls have no frame, and are not listed, until bounds are
    // optional on the wire (P2). GTK 3's are.
    #[test]
    fn a_gtk4_application_has_no_screen_positions_so_its_controls_are_not_listed() {
        let made_with = |version| {
            let mut desktop = desktop(&["/w/save"], vec![("/w/save", button("Save", 10))]);
            let root = Node::of(APPLICATION)
                .named("fixture")
                .holding(APP, &["/w"])
                .made_with("GTK", version);
            desktop.put(APP, fake::ROOT, root);
            desktop
        };
        let (_gtk4_fake, gtk4) = reader(made_with("4.14.2"));
        let (_gtk3_fake, gtk3) = reader(made_with("3.24.41"));

        assert_eq!(labels(&walk(&gtk4)), Vec::<&str>::new());
        assert_eq!(labels(&walk(&gtk3)), vec!["Save"]);
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
}
