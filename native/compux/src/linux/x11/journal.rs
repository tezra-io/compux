//! Held input, written down outside the process, so the next start releases what
//! a helper that died holding it left down.
//!
//! The in-process registry (`held::Guard`) releases on every path a live process
//! takes. It cannot help when the process is killed between a press and its
//! release: an XTEST key or button stays down in the X server after its client is
//! gone. So each press is written here BEFORE it is posted, and forgotten after
//! its release went out. One file per helper, under `$XDG_RUNTIME_DIR` (the
//! user's own tmpfs, gone at logout), naming the display, the pid and the
//! process's start time; the next helper on the same display releases what a
//! journal of a process that is no longer running names, then removes it.
//!
//! Bounded: a journal holds at most [`MAX_HELD`] items (a press past that is
//! refused, never posted unwritten), and a start reads at most [`MAX_JOURNALS`]
//! files.

use std::path::{Path, PathBuf};

/// The directory under `$XDG_RUNTIME_DIR`.
pub const DIR: &str = "compux-held";

/// The most items one journal holds: far more than any sequence presses (a chord
/// holds four modifiers, a drag one button).
pub const MAX_HELD: usize = 16;

/// The most journals a start reads.
pub const MAX_JOURNALS: usize = 64;

/// The largest journal read; a larger file is not one of ours.
const MAX_JOURNAL_BYTES: u64 = 4096;

const HEADER: &str = "compux held 1";

/// One thing held down, by the number the X server knows it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    Key(u8),
    Button(u8),
}

/// This process's journal.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    display: String,
    pid: i32,
    started: u64,
    held: Vec<Held>,
}

/// A journal a process that is gone left behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftBehind {
    pub path: PathBuf,
    pub held: Vec<Held>,
}

impl Journal {
    pub fn new(runtime_dir: &Path, display: &str, pid: i32, started: u64) -> Journal {
        Journal {
            path: runtime_dir.join(DIR).join(format!("{pid}")),
            display: display_key(display),
            pid,
            started,
            held: Vec::new(),
        }
    }

    /// Write `held` down; called before its press is posted. A press that cannot
    /// be written is not posted.
    pub fn record(&mut self, held: Held) -> Result<(), String> {
        if self.held.len() >= MAX_HELD {
            return Err(format!(
                "compux already holds {MAX_HELD} keys and buttons; it holds no more"
            ));
        }
        self.held.push(held);
        let written = self.write();
        if written.is_err() {
            self.held.pop();
        }
        written
    }

    /// Forget `held`; called after its release went out. The file goes when
    /// nothing is held.
    pub fn forget(&mut self, held: Held) -> Result<(), String> {
        let Some(at) = self.held.iter().rposition(|item| *item == held) else {
            return Ok(());
        };
        self.held.remove(at);
        self.write()
    }

    /// Whether a press could be written down now: the journal is written as it
    /// stands, as a press would write it, and cleared again if it holds
    /// nothing. A process killed in between leaves a journal that holds nothing.
    pub fn ready(&self) -> Result<(), String> {
        let text = render(&self.display, self.pid, self.started, &self.held);
        let written = crate::linux::private_file::write(&self.path, text.as_bytes());
        let cleared = written.and_then(|()| {
            if self.held.is_empty() {
                std::fs::remove_file(&self.path)
            } else {
                Ok(())
            }
        });
        let dir = self.path.parent().unwrap_or(&self.path);
        cleared.map_err(|error| {
            format!(
                "the held-input journal cannot be written in {}: {error}",
                dir.display()
            )
        })
    }

    #[cfg(test)]
    pub fn held(&self) -> &[Held] {
        &self.held
    }

    fn write(&self) -> Result<(), String> {
        if self.held.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("the held-input journal would not clear: {error}")),
            };
        }
        let text = render(&self.display, self.pid, self.started, &self.held);
        crate::linux::private_file::write(&self.path, text.as_bytes())
            .map_err(|error| format!("the held-input journal would not write: {error}"))
    }
}

/// What journals of processes no longer running left held on `display`, under
/// any of its names ([`display_key`]). `alive` says whether a pid still names the
/// process that started when the journal says. Journals of other displays and of
/// running processes are left alone.
pub fn left_behind(
    runtime_dir: &Path,
    display: &str,
    alive: impl Fn(i32, u64) -> bool,
) -> Vec<LeftBehind> {
    let Ok(listing) = std::fs::read_dir(runtime_dir.join(DIR)) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in listing.flatten().take(MAX_JOURNALS) {
        let path = entry.path();
        let Some(text) = read_small(&path) else {
            continue;
        };
        let Some(journal) = parse(&text) else {
            eprintln!(
                "compux: {} is not a held-input journal; left as it is",
                path.display()
            );
            continue;
        };
        let other_display = display_key(&journal.display) != display_key(display);
        if other_display || alive(journal.pid, journal.started) {
            continue;
        }
        found.push(LeftBehind {
            path,
            held: journal.held,
        });
    }
    found
}

/// One display however its name is written: `:0`, `:0.0` and `unix:0` are the
/// same display. The host (none, or `unix`, is this machine) and the display
/// number; the screen is not part of it. A name that is no display name is its
/// own key.
pub fn display_key(name: &str) -> String {
    let Some((host, rest)) = name.rsplit_once(':') else {
        return name.to_string();
    };
    let number = rest
        .split_once('.')
        .map_or(rest, |(number, _screen)| number);
    let Ok(number) = number.parse::<u16>() else {
        return name.to_string();
    };
    let host = if host == "unix" { "" } else { host };
    format!("{host}:{number}")
}

fn read_small(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_JOURNAL_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// A journal as its file says it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Parsed {
    display: String,
    pid: i32,
    started: u64,
    held: Vec<Held>,
}

fn render(display: &str, pid: i32, started: u64, held: &[Held]) -> String {
    let mut text = format!("{HEADER}\ndisplay {display}\npid {pid} {started}\n");
    for item in held {
        match item {
            Held::Key(code) => text.push_str(&format!("key {code}\n")),
            Held::Button(number) => text.push_str(&format!("button {number}\n")),
        }
    }
    text
}

fn parse(text: &str) -> Option<Parsed> {
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let display = lines.next()?.strip_prefix("display ")?.to_string();
    let (pid, started) = lines.next()?.strip_prefix("pid ")?.split_once(' ')?;
    let mut held = Vec::new();
    for line in lines.take(MAX_HELD) {
        let (kind, number) = line.split_once(' ')?;
        let number: u8 = number.parse().ok()?;
        held.push(match kind {
            "key" => Held::Key(number),
            "button" => Held::Button(number),
            _ => return None,
        });
    }
    Some(Parsed {
        display,
        pid: pid.parse().ok()?,
        started: started.parse().ok()?,
        held,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("compux-journal-{name}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("the old scratch directory");
        }
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    // Asked before an action's first input: a journal that can be written says
    // so, and leaves no file while nothing is held.
    #[test]
    fn a_journal_that_can_be_written_is_ready_and_leaves_nothing_behind() {
        let dir = scratch("ready");
        let journal = Journal::new(&dir, ":7", 4242, 99);

        journal.ready().expect("ready");

        assert!(!dir.join(DIR).join("4242").exists(), "a file stays");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    // A journal that holds something keeps it through the check.
    #[test]
    fn a_ready_check_keeps_what_the_journal_holds() {
        let dir = scratch("ready-held");
        let mut journal = Journal::new(&dir, ":7", 4242, 99);
        journal.record(Held::Button(1)).expect("written");

        journal.ready().expect("ready");

        let text = std::fs::read_to_string(dir.join(DIR).join("4242")).expect("it reads");
        assert_eq!(
            parse(&text).map(|parsed| parsed.held),
            Some(vec![Held::Button(1)])
        );
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    // A journal directory planted as a link cannot take the journal: the check
    // fails and names the directory.
    #[test]
    fn a_journal_that_cannot_be_written_is_not_ready_and_names_its_directory() {
        let dir = scratch("unready");
        std::fs::create_dir_all(dir.join("elsewhere")).expect("a directory");
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join(DIR)).expect("a link");
        let journal = Journal::new(&dir, ":7", 4242, 99);

        let refused = journal.ready().expect_err("not ready");

        assert!(
            refused.contains(&dir.join(DIR).display().to_string()),
            "{refused}"
        );
        assert!(
            !dir.join("elsewhere").join("4242").exists(),
            "written through the link"
        );
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    // Written before the press, gone after the release: a file exists exactly
    // while something is held.
    #[test]
    fn a_press_is_written_before_it_is_posted_and_forgotten_after_its_release() {
        let dir = scratch("cycle");
        let mut journal = Journal::new(&dir, ":7", 4242, 99);
        let file = dir.join(DIR).join("4242");
        assert!(!file.exists());

        journal.record(Held::Key(50)).unwrap();
        journal.record(Held::Button(1)).unwrap();
        let text = std::fs::read_to_string(&file).expect("written");
        assert_eq!(
            parse(&text),
            Some(Parsed {
                display: ":7".to_string(),
                pid: 4242,
                started: 99,
                held: vec![Held::Key(50), Held::Button(1)],
            })
        );

        journal.forget(Held::Button(1)).unwrap();
        journal.forget(Held::Key(50)).unwrap();
        assert!(!file.exists(), "nothing held, no journal");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // The journal is the person's alone, in a directory of their own, and a link
    // planted at its scratch name is never written through.
    #[test]
    fn a_journal_is_the_owners_alone_and_never_written_through_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("private");
        let victim = dir.join("victim");
        std::fs::write(&victim, "the person's").expect("a victim");
        std::fs::create_dir_all(dir.join(DIR)).expect("the journal directory");
        std::os::unix::fs::symlink(&victim, dir.join(DIR).join("4242.writing"))
            .expect("a planted link");
        let mut journal = Journal::new(&dir, ":7", 4242, 99);

        journal.record(Held::Key(50)).expect("written");

        let mode = |path: &Path| std::fs::metadata(path).expect("there").permissions().mode();
        assert_eq!(mode(&dir.join(DIR).join("4242")) & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&victim).expect("it reads"),
            "the person's"
        );
        let fresh = scratch("private-dir");
        Journal::new(&fresh, ":7", 1, 1)
            .record(Held::Key(50))
            .expect("written");
        assert_eq!(mode(&fresh.join(DIR)) & 0o777, 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&fresh).unwrap();
    }

    #[test]
    fn a_journal_holds_a_bounded_number_and_refuses_the_next() {
        let dir = scratch("bound");
        let mut journal = Journal::new(&dir, ":7", 1, 1);
        for code in 0..MAX_HELD {
            journal.record(Held::Key(code as u8)).unwrap();
        }
        assert!(journal.record(Held::Key(200)).is_err());
        assert_eq!(journal.held().len(), MAX_HELD);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // Only a journal of a process that is gone, on this display, is released;
    // a running helper's and another display's are left alone.
    #[test]
    fn the_next_start_finds_what_a_dead_helper_on_this_display_held() {
        let dir = scratch("left");
        Journal::new(&dir, ":7", 100, 1)
            .record(Held::Key(37))
            .unwrap();
        Journal::new(&dir, ":7", 200, 2)
            .record(Held::Button(1))
            .unwrap();
        Journal::new(&dir, ":8", 300, 3)
            .record(Held::Key(50))
            .unwrap();
        std::fs::write(dir.join(DIR).join("junk"), "not a journal").unwrap();

        let alive = |pid: i32, started: u64| pid == 200 && started == 2;
        let found = left_behind(&dir, ":7", alive);

        assert_eq!(
            found,
            vec![LeftBehind {
                path: dir.join(DIR).join("100"),
                held: vec![Held::Key(37)],
            }]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // One display has several names; a journal is found under any of them, and
    // another display's under none.
    #[test]
    fn a_display_is_one_display_however_its_name_is_written() {
        for (name, same) in [(":0", ":0.0"), (":0", "unix:0"), ("unix:0.1", ":0")] {
            assert_eq!(display_key(name), display_key(same), "{name} and {same}");
        }
        assert_ne!(display_key(":0"), display_key(":1"));
        assert_ne!(display_key(":0"), display_key("otherhost:0"));
        assert_eq!(display_key("not a display"), "not a display");

        let dir = scratch("names");
        Journal::new(&dir, ":0", 100, 1)
            .record(Held::Key(37))
            .unwrap();
        Journal::new(&dir, ":1", 200, 2)
            .record(Held::Key(50))
            .unwrap();
        for name in [":0", ":0.0", "unix:0"] {
            let found = left_behind(&dir, name, |_, _| false);
            assert_eq!(found.len(), 1, "{name}: {found:?}");
            assert_eq!(found[0].held, vec![Held::Key(37)], "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // A reused pid is a different process: the start time decides.
    #[test]
    fn a_pid_now_naming_another_process_counts_as_gone() {
        let dir = scratch("reuse");
        Journal::new(&dir, ":7", 100, 1)
            .record(Held::Key(37))
            .unwrap();
        let found = left_behind(&dir, ":7", |pid, started| pid == 100 && started == 5);
        assert_eq!(found.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_journal_directory_is_nothing_left_behind() {
        let dir = scratch("none");
        assert!(left_behind(&dir, ":7", |_, _| false).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
