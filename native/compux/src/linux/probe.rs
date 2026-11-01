//! What `probe` says about Linux accessibility, for the engine's doctor: whether
//! the accessibility bus answers and how many applications its registry lists,
//! the session's accessibility status, the at-spi2-core and ATK bridge packages,
//! and the toolkit notes that are known without reading any application.
//!
//! Read-only, and never a prompt. The bus is asked for the way the reader asks for
//! it, with a `GetAddress` that may start it (a presence check reports a
//! healthy cold session as one without accessibility); then the registry's child
//! count, the launcher's status with two property `Get`s, and the package
//! database's file. It registers for no event, reads no application, and changes
//! no setting: a status or a switch is the person's to turn on, never compux's.

use std::path::Path;

use serde_json::{json, Map, Value};
use zbus::Connection;

use super::bus::{self, Status, Unnamed};
use super::routes::{CHROMIUM_SWITCH, QT_SWITCH};

/// Where dpkg keeps what is installed. A unit test never reads it.
#[cfg_attr(test, allow(dead_code))]
const DPKG_DATABASE: &str = "/var/lib/dpkg/status";

/// The largest package database read; a larger one is not read at all.
const MAX_DATABASE_BYTES: u64 = 64 << 20;

/// The ATK bridge's package, before and after Ubuntu 24.04's 64-bit time rename.
const BRIDGE_PACKAGES: [&str; 2] = ["libatk-bridge2.0-0", "libatk-bridge2.0-0t64"];

/// The accessibility bus, as `probe` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bus {
    /// The registry answers and lists this many applications, one or more.
    Answering(u32),
    /// The registry answers and lists no application.
    Empty,
    /// No registry answered. `cause` is one word for the doctor, `detail` what
    /// failed.
    Unavailable { cause: &'static str, detail: String },
}

/// The two packages a Linux desktop's accessibility rests on, as the package
/// database names their versions. `source` is `None` where no database this
/// build reads exists (only dpkg's is read).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Packages {
    pub source: Option<&'static str>,
    pub at_spi2_core: Option<String>,
    pub atk_bridge: Option<String>,
}

/// Everything `probe` adds on Linux, read now.
#[cfg(not(test))]
pub fn report(read_timeout: std::time::Duration) -> Map<String, Value> {
    let (bus, status) = async_io::block_on(async {
        match bus::session().await {
            Ok(session) => {
                survey(&session, async |address| {
                    bus::open(address, read_timeout).await
                })
                .await
            }
            Err(why) => (unavailable("no_session_bus", why), None),
        }
    });
    fields(&bus, status, &installed(Path::new(DPKG_DATABASE)))
}

/// Under test there is no session this process may read: the panic names the
/// fix, as `bus::connect` does.
#[cfg(test)]
pub fn report(_read_timeout: std::time::Duration) -> Map<String, Value> {
    panic!("a unit test asked probe for the real session: give probe its facts instead")
}

/// The accessibility bus the session names, opened with `open`, and the
/// session's status.
pub async fn survey(
    session: &Connection,
    open: impl AsyncFnOnce(&str) -> Result<Connection, String>,
) -> (Bus, Option<Status>) {
    let address = match bus::address(session).await {
        Ok(address) => address,
        Err(unnamed) => {
            let cause = match unnamed {
                Unnamed::NoLauncher(_) => "not_installed",
                Unnamed::NoAddress(_) => "no_address",
            };
            return (unavailable(cause, unnamed.detail()), None);
        }
    };
    // A status that will not read is reported as unread: no Qt note is made
    // from it.
    let status = bus::status(session).await.ok();
    let found = match open(&address).await {
        Ok(accessibility) => census(&accessibility).await,
        Err(why) => unavailable("unreachable", why),
    };
    (found, status)
}

/// What the registry's child count says of the bus.
async fn census(accessibility: &Connection) -> Bus {
    match bus::ready(accessibility).await {
        Ok(0) => Bus::Empty,
        Ok(count) => match u32::try_from(count) {
            Ok(count) => Bus::Answering(count),
            Err(_) => unavailable(
                "not_answering",
                format!("the registry counted {count} applications"),
            ),
        },
        Err(why) => unavailable("not_answering", why),
    }
}

fn unavailable(cause: &'static str, detail: impl Into<String>) -> Bus {
    Bus::Unavailable {
        cause,
        detail: detail.into(),
    }
}

/// The packages, from dpkg's database where this system has one.
fn installed(database: &Path) -> Packages {
    let fits = std::fs::metadata(database).is_ok_and(|file| file.len() <= MAX_DATABASE_BYTES);
    if !fits {
        return Packages::default();
    }
    match std::fs::read_to_string(database) {
        Ok(text) => dpkg(&text),
        Err(why) => {
            eprintln!(
                "compux: probe: {} would not read: {why}",
                database.display()
            );
            Packages::default()
        }
    }
}

/// The two packages in a dpkg status database: the first installed record of
/// each.
pub fn dpkg(database: &str) -> Packages {
    let mut packages = Packages {
        source: Some("dpkg"),
        ..Packages::default()
    };
    for (name, version) in database.split("\n\n").filter_map(installed_record) {
        let slot = if name == "at-spi2-core" {
            &mut packages.at_spi2_core
        } else if BRIDGE_PACKAGES.contains(&name) {
            &mut packages.atk_bridge
        } else {
            continue;
        };
        slot.get_or_insert_with(|| version.to_string());
    }
    packages
}

/// One package's installed version in a dpkg status database.
pub fn dpkg_version(database: &str, package: &str) -> Option<String> {
    database
        .split("\n\n")
        .filter_map(installed_record)
        .find(|(name, _)| *name == package)
        .map(|(_, version)| version.to_string())
}

/// One package's installed version, from this system's dpkg database; `None`
/// where it is not installed or there is no database.
#[cfg(not(test))]
pub fn installed_version(package: &str) -> Option<String> {
    let database = Path::new(DPKG_DATABASE);
    let fits = std::fs::metadata(database).is_ok_and(|file| file.len() <= MAX_DATABASE_BYTES);
    if !fits {
        return None;
    }
    dpkg_version(&std::fs::read_to_string(database).ok()?, package)
}

/// One record's package and version, where dpkg says it is installed: its
/// `Status` ends in `installed` (not `half-installed`, not `config-files`).
fn installed_record(record: &str) -> Option<(&str, &str)> {
    let field = |key: &str| {
        record
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::trim)
    };
    if !field("Status:")?.ends_with(" installed") {
        return None;
    }
    Some((field("Package:")?, field("Version:")?))
}

/// The notes known without reading an application, where the bus answers.
pub fn notes(bus: &Bus, status: Option<Status>) -> Vec<Value> {
    if matches!(bus, Bus::Unavailable { .. }) {
        return Vec::new();
    }
    let off = status.is_some_and(|status| !status.is_enabled && !status.screen_reader_enabled);
    let mut notes = Vec::new();
    if off {
        notes.push(note(
            "qt",
            format!(
                "the session's accessibility status reads off, so a Qt application publishes \
                 no tree: {QT_SWITCH}"
            ),
        ));
    }
    notes.push(note(
        "chromium",
        format!("a Chromium-based application publishes no page tree by itself: {CHROMIUM_SWITCH}"),
    ));
    notes
}

fn note(toolkit: &str, note: String) -> Value {
    json!({"toolkit": toolkit, "note": note})
}

/// The fields `probe` adds: `accessibility_bus`, `accessibility_packages` and
/// `toolkit_notes`.
pub fn fields(bus: &Bus, status: Option<Status>, packages: &Packages) -> Map<String, Value> {
    let read = status.map(|status| {
        json!({"is_enabled": status.is_enabled,
               "screen_reader_enabled": status.screen_reader_enabled})
    });
    let accessibility_bus = match bus {
        Bus::Answering(count) => {
            json!({"state": "answering", "applications": count, "status": read})
        }
        Bus::Empty => json!({"state": "empty", "applications": 0, "status": read}),
        Bus::Unavailable { cause, detail } => {
            json!({"state": "unavailable", "cause": cause, "detail": detail, "status": read})
        }
    };
    let mut fields = Map::new();
    fields.insert("accessibility_bus".to_string(), accessibility_bus);
    fields.insert(
        "accessibility_packages".to_string(),
        json!({"source": packages.source, "at_spi2_core": packages.at_spi2_core,
               "atk_bridge": packages.atk_bridge}),
    );
    fields.insert("toolkit_notes".to_string(), Value::from(notes(bus, status)));
    fields
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::linux::fake::{Desktop, Fake, Life, Node, LAUNCHER, REGISTRY, ROOT};

    const TIMEOUT: Duration = Duration::from_millis(200);

    fn desktop(applications: usize) -> Desktop {
        let all = [(":1.10", 4711), (":1.11", 4712)];
        Desktop {
            address: "unix:path=/tmp/fake-a11y-bus".to_string(),
            ..Desktop::with_applications(
                all[..applications]
                    .iter()
                    .map(|(bus, pid)| (*bus, *pid, Node::of(75)))
                    .collect(),
            )
        }
    }

    /// The survey of a fake session whose accessibility bus is the fake itself.
    fn surveyed(desktop: Desktop) -> (Fake, (Bus, Option<Status>)) {
        let (fake, session) = Fake::start(desktop, TIMEOUT);
        let bus = session.clone();
        let found = async_io::block_on(survey(&session, async |_| Ok(bus)));
        (fake, found)
    }

    const OFF: Status = Status {
        is_enabled: false,
        screen_reader_enabled: false,
    };

    // A registry that lists applications is a bus that answers. The survey asks
    // the address the way the reader does, reads the status and the count, and
    // nothing else: no event registered, no application read, never GetAll.
    #[test]
    fn a_registry_that_lists_applications_is_a_bus_that_answers() {
        let (fake, found) = surveyed(desktop(2));

        assert_eq!(found, (Bus::Answering(2), Some(OFF)));
        let asked: Vec<(String, String, String)> = fake
            .calls()
            .into_iter()
            .map(|call| (call.bus, call.member, call.argument))
            .collect();
        let launcher = |member: &str, argument: &str| {
            (
                LAUNCHER.to_string(),
                member.to_string(),
                argument.to_string(),
            )
        };
        assert_eq!(
            asked,
            vec![
                launcher("GetAddress", ""),
                launcher("Get", "IsEnabled"),
                launcher("Get", "ScreenReaderEnabled"),
                (
                    REGISTRY.to_string(),
                    "Get".to_string(),
                    "ChildCount".to_string()
                ),
            ]
        );
        assert!(fake.calls()[0].auto_start, "GetAddress may start the bus");
    }

    #[test]
    fn a_registry_that_lists_no_application_is_a_bus_that_answers_empty() {
        let (_fake, found) = surveyed(Desktop {
            status: (true, false),
            ..desktop(0)
        });

        let on = Status {
            is_enabled: true,
            screen_reader_enabled: false,
        };
        assert_eq!(found, (Bus::Empty, Some(on)));
    }

    // Each way the bus is unavailable is named by its cause.
    #[test]
    fn a_bus_that_is_unavailable_says_why() {
        let mut no_launcher = desktop(1);
        no_launcher.quit.push(LAUNCHER.to_string());
        let no_address = Desktop {
            address: String::new(),
            ..desktop(1)
        };

        let (_, without_launcher) = surveyed(no_launcher);
        let (_, without_address) = surveyed(no_address);
        let (fake, session) = Fake::start(desktop(1), TIMEOUT);
        let refused = async_io::block_on(survey(&session, async |_| {
            Err("the accessibility bus would not connect: refused".to_string())
        }));
        fake.change(REGISTRY, ROOT, |root| root.life = Life::Silent);
        let bus = session.clone();
        let silent = async_io::block_on(survey(&session, async |_| Ok(bus)));

        let cause = |found: &(Bus, Option<Status>)| match &found.0 {
            Bus::Unavailable { cause, .. } => *cause,
            other => panic!("unavailable: {other:?}"),
        };
        assert_eq!(cause(&without_launcher), "not_installed");
        assert_eq!(without_launcher.1, None, "no launcher, no status");
        assert_eq!(cause(&without_address), "no_address");
        assert_eq!(cause(&refused), "unreachable");
        assert_eq!(refused.1, Some(OFF), "the launcher still says its status");
        assert_eq!(cause(&silent), "not_answering");
    }

    // Qt publishes only while the session's status reads on (or with its own
    // switch); Chromium only with its switches, whatever the status. Nothing is
    // noted where no bus answers, and no Qt note where the status went unread.
    #[test]
    fn the_toolkit_notes_follow_the_status_and_the_bus() {
        let toolkits = |notes: Vec<Value>| -> Vec<String> {
            notes
                .iter()
                .map(|note| note["toolkit"].as_str().expect("a toolkit").to_string())
                .collect()
        };
        let on = Status {
            is_enabled: false,
            screen_reader_enabled: true,
        };

        let off = notes(&Bus::Answering(3), Some(OFF));
        assert_eq!(toolkits(off.clone()), vec!["qt", "chromium"]);
        assert!(off[0]["note"].as_str().unwrap().contains(QT_SWITCH));
        assert!(off[1]["note"].as_str().unwrap().contains(CHROMIUM_SWITCH));
        assert_eq!(toolkits(notes(&Bus::Empty, Some(on))), vec!["chromium"]);
        assert_eq!(toolkits(notes(&Bus::Empty, None)), vec!["chromium"]);
        let gone = unavailable("not_installed", "no launcher");
        assert!(notes(&gone, Some(OFF)).is_empty());
    }

    /// The owner's 22.04 record shapes, an i386 copy, a removed package, and
    /// 24.04's renamed bridge.
    const JAMMY: &str = "Package: at-spi2-core\nStatus: install ok installed\nPriority: optional\n\
                         Architecture: amd64\nVersion: 2.44.0-3\nDescription: Assistive \
                         Technology Service Provider Interface (D-Bus core)\n This package \
                         contains the core components.\n\n\
                         Package: libatk-bridge2.0-0\nStatus: install ok installed\n\
                         Architecture: amd64\nMulti-Arch: same\nVersion: 2.38.0-3\n\n\
                         Package: libatk-bridge2.0-0\nStatus: install ok installed\n\
                         Architecture: i386\nVersion: 2.38.0-3\n";

    #[test]
    fn the_packages_are_read_from_dpkg_where_they_are_installed() {
        let removed =
            "Package: at-spi2-core\nStatus: deinstall ok config-files\nVersion: 2.40.0-1\n\n";
        let noble = "Package: libatk-bridge2.0-0t64\nStatus: install ok installed\n\
                     Version: 2.52.0-1build1\n\nPackage: at-spi2-core\nStatus: install ok \
                     installed\nVersion: 2.52.0-1build1\n";

        assert_eq!(
            dpkg(&format!("{removed}{JAMMY}")),
            Packages {
                source: Some("dpkg"),
                at_spi2_core: Some("2.44.0-3".to_string()),
                atk_bridge: Some("2.38.0-3".to_string()),
            }
        );
        assert_eq!(dpkg(noble).atk_bridge.as_deref(), Some("2.52.0-1build1"));
        assert_eq!(
            dpkg(removed),
            Packages {
                source: Some("dpkg"),
                ..Packages::default()
            },
            "a package only configured is not installed"
        );
    }

    #[test]
    fn one_packages_installed_version_is_read_from_dpkg() {
        let portal = "Package: xdg-desktop-portal\nStatus: install ok installed\n\
                      Version: 1.14.4-1ubuntu2~22.04.2\n";
        let removed = "Package: xdg-desktop-portal\nStatus: deinstall ok config-files\n\
                       Version: 1.18.4-1\n";

        assert_eq!(
            dpkg_version(&format!("{JAMMY}\n{portal}"), "xdg-desktop-portal").as_deref(),
            Some("1.14.4-1ubuntu2~22.04.2")
        );
        assert_eq!(dpkg_version(removed, "xdg-desktop-portal"), None);
        assert_eq!(dpkg_version(JAMMY, "xdg-desktop-portal"), None);
    }

    #[test]
    fn a_system_without_a_dpkg_database_says_so() {
        let dir = std::env::temp_dir().join(format!("compux-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let database = dir.join("status");
        std::fs::write(&database, JAMMY).expect("a database");

        let read = installed(&database);
        let missing = installed(&dir.join("none"));
        std::fs::remove_dir_all(&dir).expect("removed");

        assert_eq!(read.source, Some("dpkg"));
        assert_eq!(read.atk_bridge.as_deref(), Some("2.38.0-3"));
        assert_eq!(missing, Packages::default());
    }

    // Each bus state by its name: answering and empty with the applications
    // counted, unavailable with its cause and what failed; the status beside it
    // where it was read, and null where it was not.
    #[test]
    fn each_bus_state_is_reported_by_its_name() {
        let packages = dpkg(JAMMY);

        let answering = fields(&Bus::Answering(4), Some(OFF), &packages);
        let empty = fields(&Bus::Empty, None, &packages);
        let gone = fields(
            &unavailable("not_installed", "no org.a11y.Bus"),
            None,
            &Packages::default(),
        );

        assert_eq!(
            answering["accessibility_bus"],
            json!({"state": "answering", "applications": 4,
                   "status": {"is_enabled": false, "screen_reader_enabled": false}})
        );
        assert_eq!(
            empty["accessibility_bus"],
            json!({"state": "empty", "applications": 0, "status": null})
        );
        assert_eq!(
            gone["accessibility_bus"],
            json!({"state": "unavailable", "cause": "not_installed",
                   "detail": "no org.a11y.Bus", "status": null})
        );
        assert_eq!(
            answering["accessibility_packages"],
            json!({"source": "dpkg", "at_spi2_core": "2.44.0-3", "atk_bridge": "2.38.0-3"})
        );
        assert_eq!(
            gone["accessibility_packages"],
            json!({"source": null, "at_spi2_core": null, "atk_bridge": null})
        );
        assert_eq!(answering["toolkit_notes"].as_array().map(Vec::len), Some(2));
        assert_eq!(gone["toolkit_notes"], json!([]));
    }
}
