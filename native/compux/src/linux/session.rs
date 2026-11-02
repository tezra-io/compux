//! Which process a reference belongs to, from `/proc`.
//!
//! A pid is not an identity: pids are reused. A reference retained against one is
//! only usable while that pid still names the process that started when it did,
//! so the start time is read beside it — the same rule the macOS reader keeps with
//! `proc_pidinfo`.

/// The unit `/proc/<pid>/stat` counts a start time in: `USER_HZ`, the kernel's
/// fixed tick for times it exports, which is 100 on every architecture this crate
/// is built for (x86_64 and aarch64). The value is only ever compared with itself,
/// so a different tick could change the number and never the comparison.
const TICKS_PER_SECOND: u64 = 100;

/// The 22nd field of `/proc/<pid>/stat`, `starttime`, counted from the field after
/// the command name.
///
/// The command name is the second field, in parentheses, and may itself hold
/// spaces and parentheses — a process may call itself anything — so the fields are
/// counted from the LAST `)` rather than by splitting the whole line.
const START_FIELD_AFTER_NAME: usize = 19;

/// When the process started, in microseconds since boot, or `None` when it is gone
/// or its record will not read.
pub fn started_at(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ticks = start_ticks(&stat)?;
    ticks.checked_mul(1_000_000 / TICKS_PER_SECOND)
}

/// The process's own short name (`/proc/<pid>/comm`), for a note that says which
/// application was read. `None` when it is gone.
pub fn name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end().to_string()).filter(|name| !name.is_empty())
}

/// The start time in a `/proc/<pid>/stat` line, in ticks.
pub fn start_ticks(stat: &str) -> Option<u64> {
    let (_, after_name) = stat.rsplit_once(')')?;
    after_name
        .split_whitespace()
        .nth(START_FIELD_AFTER_NAME)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real line from `/proc/self/stat`, with the start time (8423515) where the
    /// kernel puts it.
    const STAT: &str = "4242 (python3) S 1 4242 4242 0 -1 4194560 2871 0 0 0 12 3 0 0 20 0 \
                        4 0 8423515 512000000 9000 18446744073709551615 1 1 0 0 0 0 0 4096 \
                        1260 0 0 0 17 3 0 0 0 0 0";

    #[test]
    fn the_start_time_is_the_twenty_second_field() {
        assert_eq!(start_ticks(STAT), Some(8_423_515));
    }

    // A process may name itself anything, parentheses and spaces included; the
    // fields after the name are counted from the last parenthesis.
    #[test]
    fn a_name_with_spaces_and_parentheses_does_not_move_the_field() {
        let odd = STAT.replace("(python3)", "(a (b) c) d)");
        assert_eq!(start_ticks(&odd), Some(8_423_515));
    }

    #[test]
    fn a_record_that_is_cut_short_has_no_start_time() {
        assert_eq!(start_ticks("4242 (python3) S 1 4242"), None);
        assert_eq!(start_ticks("no name at all"), None);
    }

    // This process is certainly running, and certainly started at one moment only.
    #[test]
    fn this_process_has_a_start_time_and_a_name() {
        let me = std::process::id() as i32;
        let first = started_at(me).expect("this process is running");
        assert_eq!(started_at(me), Some(first));
        assert!(name(me).is_some());
    }

    #[test]
    fn no_pid_names_no_process() {
        assert_eq!(started_at(0), None);
        assert_eq!(started_at(-1), None);
        assert_eq!(name(0), None);
    }
}
