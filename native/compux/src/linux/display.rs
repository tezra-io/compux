//! Which display server this session runs, and where a helper started without one
//! finds it.
//!
//! **Decided by connecting.** A variable locates an endpoint; it proves
//! nothing. `WAYLAND_DISPLAY` names a socket under `XDG_RUNTIME_DIR`, `DISPLAY` an
//! X socket, and the session is the one whose socket answers: a Wayland socket that
//! answers is Wayland, with an X11 endpoint beside it where `DISPLAY` answers too
//! (XWayland); an X socket alone is X11. `XDG_SESSION_TYPE` is never read — it says
//! `tty` over SSH (agent-desktop L1-4, L13-1).
//!
//! **A helper started before login.** A daemon that a systemd user unit
//! started before anyone logged in hands its helper no display. When this process
//! has neither `DISPLAY` nor `WAYLAND_DISPLAY`, it reads exactly three names —
//! `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY` — from the systemd user manager's
//! environment, where a GNOME login puts them, and uses them for its own
//! connections. It logs only which of the three it took, never a value and never
//! another name, and changes nothing anywhere.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The three names read from the user manager, and the only ones.
pub const NAMES: [&str; 3] = ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"];

/// Where X servers put their sockets.
const X11_SOCKETS: &str = "/tmp/.X11-unix";

/// X display `n` listens on TCP port 6000 + n.
const X11_TCP_BASE: u16 = 6000;

/// How long one connection to a display socket may take.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

const SYSTEMD: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";

/// The display server this session runs, as its sockets answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Server {
    X11,
    /// `x11`: an X endpoint answers beside it (XWayland), which reaches only the
    /// session's X clients.
    Wayland {
        x11: bool,
    },
    None,
}

impl Server {
    /// The server of this process's own session, by connecting to its sockets.
    pub fn here() -> Server {
        let env = Env::of_process();
        let wayland = wayland_socket(&env).is_some_and(|socket| answers_unix(&socket));
        let x11 = env
            .display
            .as_deref()
            .and_then(|display| x11_socket(display, Path::new(X11_SOCKETS)))
            .is_some_and(|socket| socket.answers());
        decide(wayland, x11)
    }

    /// The name `probe` reports it by.
    pub fn name(self) -> &'static str {
        match self {
            Server::X11 => "x11",
            Server::Wayland { .. } => "wayland",
            Server::None => "none",
        }
    }

    /// Whether an X endpoint answers: the session's own on X11, XWayland beside
    /// Wayland.
    pub fn x11_endpoint(self) -> bool {
        matches!(self, Server::X11 | Server::Wayland { x11: true })
    }

    /// Whether a client's accessible extents are global positions: on X11 they
    /// are; Wayland gives no client a global position.
    pub fn places(self) -> bool {
        self == Server::X11
    }
}

/// The session as its sockets answered: a Wayland socket that answers decides it,
/// whatever else answers.
pub fn decide(wayland: bool, x11: bool) -> Server {
    match (wayland, x11) {
        (true, x11) => Server::Wayland { x11 },
        (false, true) => Server::X11,
        (false, false) => Server::None,
    }
}

/// The variables that locate a display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Env {
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub runtime_dir: Option<PathBuf>,
}

impl Env {
    fn of_process() -> Env {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        Env {
            display: var("DISPLAY"),
            wayland_display: var("WAYLAND_DISPLAY"),
            runtime_dir: var("XDG_RUNTIME_DIR").map(PathBuf::from),
        }
    }
}

/// The Wayland socket `WAYLAND_DISPLAY` names: a path as it is, or a name under
/// `XDG_RUNTIME_DIR`.
pub fn wayland_socket(env: &Env) -> Option<PathBuf> {
    let name = Path::new(env.wayland_display.as_deref()?);
    if name.is_absolute() {
        return Some(name.to_path_buf());
    }
    Some(env.runtime_dir.as_ref()?.join(name))
}

/// Where an X display listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X11Socket {
    Unix(PathBuf),
    Tcp(String, u16),
}

impl X11Socket {
    fn answers(&self) -> bool {
        match self {
            X11Socket::Unix(path) => answers_unix(path),
            X11Socket::Tcp(host, port) => answers_tcp(host, *port),
        }
    }
}

/// The socket a `DISPLAY` names (`[host]:number[.screen]`): a local display's under
/// `sockets`, a remote one's on TCP. `None` for anything that is not a display.
pub fn x11_socket(display: &str, sockets: &Path) -> Option<X11Socket> {
    let (host, rest) = display.rsplit_once(':')?;
    let number = rest.split('.').next()?;
    let number: u16 = number.parse().ok()?;
    match host {
        "" | "unix" => Some(X11Socket::Unix(sockets.join(format!("X{number}")))),
        host => Some(X11Socket::Tcp(
            host.to_string(),
            X11_TCP_BASE.checked_add(number)?,
        )),
    }
}

/// Whether something accepts a connection on this socket. The connection is
/// closed at once: nothing is sent on it.
fn answers_unix(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

fn answers_tcp(host: &str, port: u16) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(addresses) = (host, port).to_socket_addrs() else {
        return false;
    };
    addresses
        .take(4)
        .any(|address| TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).is_ok())
}

/// Exactly the three names, out of the manager's `NAME=value` lines; every other
/// line is left unread.
pub fn picked(environment: &[String]) -> Vec<(String, String)> {
    environment
        .iter()
        .filter_map(|line| line.split_once('='))
        .filter(|(name, value)| NAMES.contains(name) && !value.is_empty())
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// The systemd user manager's environment: one property `Get`, nothing written.
pub async fn user_manager(session: &zbus::Connection) -> Result<Vec<String>, String> {
    let reply = session
        .call_method(
            Some(SYSTEMD),
            SYSTEMD_PATH,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(MANAGER, "Environment"),
        )
        .await
        .map_err(|error| format!("the user manager did not answer: {error}"))?;
    let value: zbus::zvariant::OwnedValue = reply
        .body()
        .deserialize()
        .map_err(|error| format!("an unreadable user manager answer: {error}"))?;
    Vec::<String>::try_from(value)
        .map_err(|error| format!("an unreadable user manager environment: {error}"))
}

/// Whether this process needs a display from the user manager: it has neither
/// display variable.
pub fn needs_display(display: Option<&str>, wayland_display: Option<&str>) -> bool {
    display.is_none_or(str::is_empty) && wayland_display.is_none_or(str::is_empty)
}

/// When this process was started with no display, take the three names from
/// the user manager for its own connections. Called once, first thing in `main`,
/// before compux starts a thread of its own: the variables it sets are this
/// process's own, and every connection it makes after reads them. A manager that does not answer
/// is said on stderr, and the helper goes on with no display.
pub fn adopt_from_user_manager() {
    let own = |name: &str| std::env::var(name).ok();
    if !needs_display(own("DISPLAY").as_deref(), own("WAYLAND_DISPLAY").as_deref()) {
        return;
    }
    let read = async_io::block_on(async {
        let session = zbus::connection::Builder::session()
            .map_err(|error| format!("no session bus: {error}"))?
            .method_timeout(CONNECT_TIMEOUT)
            .build()
            .await
            .map_err(|error| format!("no session bus: {error}"))?;
        user_manager(&session).await
    });
    match read {
        Ok(environment) => {
            let taken = picked(&environment);
            let names: Vec<&str> = taken.iter().map(|(name, _)| name.as_str()).collect();
            eprintln!("compux: no display of its own; took {names:?} from the user manager");
            for (name, value) in taken {
                std::env::set_var(name, value);
            }
        }
        Err(why) => eprintln!("compux: no display of its own, and {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fake::{Desktop, Fake};
    use std::os::unix::net::UnixListener;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("compux-display-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    // What answers decides. Wayland with an X endpoint beside it is XWayland,
    // still Wayland; an X socket alone is X11.
    #[test]
    fn the_session_is_the_one_whose_socket_answers() {
        assert_eq!(decide(true, false), Server::Wayland { x11: false });
        assert_eq!(decide(true, true), Server::Wayland { x11: true });
        assert_eq!(decide(false, true), Server::X11);
        assert_eq!(decide(false, false), Server::None);

        assert!(Server::X11.places());
        assert!(!Server::Wayland { x11: true }.places());
        assert!(Server::Wayland { x11: true }.x11_endpoint());
        assert_eq!(Server::Wayland { x11: true }.name(), "wayland");
    }

    #[test]
    fn a_display_names_a_local_socket_or_a_remote_port() {
        let sockets = Path::new("/tmp/.X11-unix");
        assert_eq!(
            x11_socket(":1", sockets),
            Some(X11Socket::Unix(sockets.join("X1")))
        );
        assert_eq!(
            x11_socket("unix:0.0", sockets),
            Some(X11Socket::Unix(sockets.join("X0")))
        );
        assert_eq!(
            x11_socket("box:10.0", sockets),
            Some(X11Socket::Tcp("box".to_string(), 6010))
        );
        assert_eq!(x11_socket("nonsense", sockets), None);
        assert_eq!(x11_socket(":x", sockets), None);
    }

    #[test]
    fn a_wayland_name_is_found_under_the_runtime_directory() {
        let env = Env {
            wayland_display: Some("wayland-0".to_string()),
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..Env::default()
        };
        assert_eq!(
            wayland_socket(&env),
            Some(PathBuf::from("/run/user/1000/wayland-0"))
        );
        let absolute = Env {
            wayland_display: Some("/tmp/w".to_string()),
            ..Env::default()
        };
        assert_eq!(wayland_socket(&absolute), Some(PathBuf::from("/tmp/w")));
        assert_eq!(wayland_socket(&Env::default()), None);
    }

    // A variable proves nothing: a socket nobody listens on does not answer, and
    // one somebody does, does.
    #[test]
    fn a_socket_answers_only_while_something_listens() {
        let dir = scratch("listen");
        let path = dir.join("X7");
        assert!(!answers_unix(&path), "nothing there yet");
        let listener = UnixListener::bind(&path).expect("a listener");
        assert!(answers_unix(&path));
        assert!(X11Socket::Unix(path.clone()).answers());
        drop(listener);
        std::fs::remove_dir_all(&dir).expect("scratch removed");
        assert!(!answers_unix(&path));
    }

    // Exactly the three names, and nothing else the manager holds.
    #[test]
    fn only_the_three_names_are_taken_from_the_user_manager() {
        let environment: Vec<String> = [
            "HOME=/home/someone",
            "DISPLAY=:1",
            "SECRET_TOKEN=hunter2",
            "XAUTHORITY=/run/user/1000/gdm/Xauthority",
            "DISPLAYS=:9",
            "WAYLAND_DISPLAY=",
            "PATH=/usr/bin",
        ]
        .iter()
        .map(|line| line.to_string())
        .collect();

        assert_eq!(
            picked(&environment),
            vec![
                ("DISPLAY".to_string(), ":1".to_string()),
                (
                    "XAUTHORITY".to_string(),
                    "/run/user/1000/gdm/Xauthority".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_process_with_either_display_variable_needs_none() {
        assert!(needs_display(None, None));
        assert!(needs_display(Some(""), None));
        assert!(!needs_display(Some(":1"), None));
        assert!(!needs_display(None, Some("wayland-0")));
    }

    // The manager is asked one property, and nothing is written to it.
    #[test]
    fn the_user_manager_is_asked_its_environment_once_and_told_nothing() {
        let desktop = Desktop {
            manager_environment: vec!["DISPLAY=:1".to_string(), "HOME=/h".to_string()],
            ..Desktop::default()
        };
        let (fake, session) = Fake::start(desktop, Duration::from_millis(200));

        let read = async_io::block_on(user_manager(&session)).expect("it answers");

        assert_eq!(read, vec!["DISPLAY=:1".to_string(), "HOME=/h".to_string()]);
        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].bus, SYSTEMD);
        assert_eq!(calls[0].member, "Get");
        assert_eq!(calls[0].argument, "Environment");
    }
}
