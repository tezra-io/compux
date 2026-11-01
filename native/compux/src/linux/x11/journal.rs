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
            display: display.to_string(),
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
        let dir = self.path.parent().ok_or("the journal has no directory")?;
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("the held-input journal's directory: {error}"))?;
        let scratch = self.path.with_extension("writing");
        let text = render(&self.display, self.pid, self.started, &self.held);
        std::fs::write(&scratch, text)
            .map_err(|error| format!("the held-input journal would not write: {error}"))?;
        std::fs::rename(&scratch, &self.path)
            .map_err(|error| format!("the held-input journal would not write: {error}"))
    }
}

/// What journals of processes no longer running left held on `display`. `alive`
/// says whether a pid still names the process that started when the journal
/// says. Journals of other displays and of running processes are left alone.
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
        if journal.display != display || alive(journal.pid, journal.started) {
            continue;
        }
        found.push(LeftBehind {
            path,
            held: journal.held,
        });
    }
    found
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
