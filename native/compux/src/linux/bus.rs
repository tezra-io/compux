//! The accessibility bus: where it is, and a connection to it that answers.
//!
//! AT-SPI does not live on the session bus. The session bus's `org.a11y.Bus` names
//! a second bus, and every accessible object is read there. Its address is asked
//! with an ordinary, auto-starting call: on a session where no assistive technology
//! has run yet the accessibility bus does not exist until something asks, and a
//! `NameHasOwner` check (or a call that refuses to auto-start) reports that healthy
//! cold session as one without accessibility (agent-desktop L1-5).

use std::time::Duration;

use zbus::Connection;

/// The session bus's name for the accessibility bus launcher.
const LAUNCHER: &str = "org.a11y.Bus";
const LAUNCHER_PATH: &str = "/org/a11y/bus";

/// The registry, and the accessible every application's tree hangs from.
pub const REGISTRY: &str = "org.a11y.atspi.Registry";
pub const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const REGISTRY_PATH: &str = "/org/a11y/atspi/registry";

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// The launcher's status: whether the session asked for accessibility.
const STATUS: &str = "org.a11y.Status";

/// What a session bus answers for a name nothing owns and nothing can start.
const SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";

/// The event this process says it listens for. Chromium reports which of its
/// windows is ACTIVE only once some client has registered for it (agent-desktop
/// `system/bus/mod.rs`), and the frame that reads ACTIVE is how the application in
/// front is found. Nothing is ever received for it: no match rule is added, so the
/// bus delivers none of these signals here.
const LISTENED_EVENT: &str = "window:activate";

/// How long finding and opening the bus may take, per call. Generous, because the
/// first `GetAddress` of a session STARTS the bus launcher; every read after that is
/// held to the reader's own timeout.
#[cfg(not(test))]
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Open the accessibility bus of this process's session: ask the session bus where
/// it is, connect with `read_timeout` on every call, say what this client listens
/// for, and read the registry once to prove it answers.
///
/// A registry that refuses the event registration is said on stderr and the bus is
/// used anyway: only Chromium's ACTIVE window depends on it, and every other read
/// does not.
#[cfg(not(test))]
pub fn connect(read_timeout: Duration) -> Result<Connection, String> {
    async_io::block_on(async {
        let session = session().await?;
        let address = address(&session).await;
        // The session bus is needed for nothing else.
        drop(session);

        let bus = open(&address.map_err(|why| why.detail())?, read_timeout).await?;
        if let Err(why) = announce(&bus).await {
            eprintln!("compux: accessibility bus: {why}");
        }
        ready(&bus).await?;
        Ok(bus)
    })
}

/// This process's session bus, with the timeout a first `GetAddress` needs.
#[cfg(not(test))]
pub async fn session() -> Result<Connection, String> {
    zbus::connection::Builder::session()
        .map(|builder| builder.method_timeout(DISCOVERY_TIMEOUT))
        .map_err(|error| format!("no session bus: {error}"))?
        .build()
        .await
        .map_err(|error| format!("no session bus: {error}"))
}

/// A connection to the accessibility bus at `address`, `read_timeout` on every
/// call.
#[cfg(not(test))]
pub async fn open(address: &str, read_timeout: Duration) -> Result<Connection, String> {
    zbus::connection::Builder::address(address)
        .map_err(|error| format!("a bad accessibility bus address: {error}"))?
        .method_timeout(read_timeout)
        .build()
        .await
        .map_err(|error| format!("the accessibility bus would not connect: {error}"))
}

/// Under test there is no session this process may read, whatever the machine
/// running the tests has. A test that lands here built the reader without giving it
/// a bus; the panic names the fix, as `RealDisplays` does for the screen.
#[cfg(test)]
pub fn connect(_read_timeout: Duration) -> Result<Connection, String> {
    panic!(
        "a unit test asked for the real accessibility bus: build the reader with \
         linux::atspi::Real::over(a fake's connection)"
    )
}

/// Why the session bus named no accessibility bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unnamed {
    /// Nothing on the session bus is `org.a11y.Bus`, or can be started as it:
    /// at-spi2-core is not installed for this session.
    NoLauncher(String),
    /// The launcher gave no address that can be used, or no answer.
    NoAddress(String),
}

impl Unnamed {
    pub fn detail(&self) -> String {
        match self {
            Unnamed::NoLauncher(detail) | Unnamed::NoAddress(detail) => detail.clone(),
        }
    }
}

/// The accessibility bus's address, from the session bus: one auto-starting
/// `GetAddress`, never a presence check first (L1-5).
pub async fn address(session: &Connection) -> Result<String, Unnamed> {
    let reply = session
        .call_method(
            Some(LAUNCHER),
            LAUNCHER_PATH,
            Some(LAUNCHER),
            "GetAddress",
            &(),
        )
        .await
        .map_err(|error| {
            let detail = format!("the session bus would not name the accessibility bus: {error}");
            match error {
                zbus::Error::MethodError(name, _, _) if name.as_str() == SERVICE_UNKNOWN => {
                    Unnamed::NoLauncher(detail)
                }
                _ => Unnamed::NoAddress(detail),
            }
        })?;
    let address: String = reply.body().deserialize().map_err(|error| {
        Unnamed::NoAddress(format!("an unreadable accessibility bus address: {error}"))
    })?;
    if address.is_empty() {
        return Err(Unnamed::NoAddress(
            "the session bus named an empty accessibility bus address".to_string(),
        ));
    }
    Ok(address)
}

/// The session's accessibility status, as the launcher's `org.a11y.Status` reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    /// Something asked for accessibility (`IsEnabled`).
    pub is_enabled: bool,
    /// A screen reader runs (`ScreenReaderEnabled`).
    pub screen_reader_enabled: bool,
}

/// The session's accessibility status: two property `Get`s on the launcher,
/// never `GetAll`, and never a `Set`. Turning it on is the person's to do.
pub async fn status(session: &Connection) -> Result<Status, String> {
    Ok(Status {
        is_enabled: flag(session, "IsEnabled").await?,
        screen_reader_enabled: flag(session, "ScreenReaderEnabled").await?,
    })
}

async fn flag(session: &Connection, name: &str) -> Result<bool, String> {
    let reply = session
        .call_method(
            Some(LAUNCHER),
            LAUNCHER_PATH,
            Some(PROPERTIES),
            "Get",
            &(STATUS, name),
        )
        .await
        .map_err(|error| format!("the launcher did not say {name}: {error}"))?;
    let value: zbus::zvariant::OwnedValue = reply
        .body()
        .deserialize()
        .map_err(|error| format!("an unreadable {name}: {error}"))?;
    bool::try_from(value).map_err(|error| format!("an unreadable {name}: {error}"))
}

/// Say what this client listens for ([`LISTENED_EVENT`]), as `(event, properties)`:
/// the form every registry takes. The registry of at-spi2-core 2.44 (Ubuntu 22.04)
/// leaves the newer `(event, properties, application)` unanswered.
pub async fn announce(bus: &Connection) -> Result<(), String> {
    bus.call_method(
        Some(REGISTRY),
        REGISTRY_PATH,
        Some(REGISTRY),
        "RegisterEvent",
        &(LISTENED_EVENT, Vec::<String>::new()),
    )
    .await
    .map(|_| ())
    .map_err(|error| format!("the registry refused the event registration: {error}"))
}

/// Read the registry's child count: the registry answers it even with no
/// application registered, so an answer is proof the bus works and a failure is the
/// reason it does not.
pub async fn ready(bus: &Connection) -> Result<i32, String> {
    let reply = bus
        .call_method(
            Some(REGISTRY),
            ROOT_PATH,
            Some(PROPERTIES),
            "Get",
            &(ACCESSIBLE, "ChildCount"),
        )
        .await
        .map_err(|error| format!("the registry did not answer: {error}"))?;
    let count: zbus::zvariant::OwnedValue = reply
        .body()
        .deserialize()
        .map_err(|error| format!("an unreadable registry answer: {error}"))?;
    i32::try_from(count).map_err(|error| format!("an unreadable registry answer: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fake::{Desktop, Fake, Node};

    const TIMEOUT: Duration = Duration::from_millis(200);

    fn desktop() -> Desktop {
        Desktop {
            address: "unix:path=/tmp/fake-a11y-bus".to_string(),
            ..Desktop::with_applications(vec![(":1.10", 4711, Node::of(75))])
        }
    }

    // L1-5: a cold session has no accessibility bus until something asks for it, so
    // the address is asked with a call that may start it, and nothing checks for
    // the name first.
    #[test]
    fn the_address_is_asked_once_with_auto_start_and_no_presence_check() {
        let (fake, session) = Fake::start(desktop(), TIMEOUT);

        let found = async_io::block_on(address(&session));

        assert_eq!(found.as_deref(), Ok("unix:path=/tmp/fake-a11y-bus"));
        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "one call: {calls:?}");
        assert_eq!(calls[0].member, "GetAddress");
        assert_eq!(calls[0].bus, LAUNCHER);
        assert!(calls[0].auto_start, "GetAddress may start the bus");
    }

    #[test]
    fn an_empty_address_is_no_address() {
        let (_fake, session) = Fake::start(
            Desktop {
                address: String::new(),
                ..desktop()
            },
            TIMEOUT,
        );

        assert!(async_io::block_on(address(&session)).is_err());
    }

    // The client says it listens for window activation — what Chromium waits for
    // before it reports an ACTIVE window — in a form the registry of Ubuntu 22.04
    // answers too (it leaves `sass` unanswered), and the bus is ready when the
    // registry answers its child count. Nothing else is asked.
    #[test]
    fn a_bus_is_announced_to_and_ready_once_the_registry_counts_its_applications() {
        let (fake, bus) = Fake::start(desktop(), TIMEOUT);

        let announced = async_io::block_on(announce(&bus));
        let count = async_io::block_on(ready(&bus));

        assert_eq!(announced, Ok(()));
        assert_eq!(count, Ok(1));
        let asked: Vec<(String, String)> = fake
            .calls()
            .into_iter()
            .map(|call| (call.member, call.argument))
            .collect();
        assert_eq!(
            asked,
            vec![
                ("RegisterEvent".to_string(), LISTENED_EVENT.to_string()),
                ("Get".to_string(), "ChildCount".to_string()),
            ]
        );
    }

    #[test]
    fn a_registry_that_does_not_answer_is_a_bus_that_is_not_ready() {
        let (fake, bus) = Fake::start(desktop(), TIMEOUT);
        fake.change(
            crate::linux::fake::REGISTRY,
            crate::linux::fake::ROOT,
            |root| root.life = crate::linux::fake::Life::Silent,
        );

        let ready = async_io::block_on(ready(&bus));

        assert!(ready.is_err(), "{ready:?}");
    }
}
