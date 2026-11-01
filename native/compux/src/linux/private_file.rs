//! A file only its owner reads, written whole: the portal's restore token and the
//! held-input journal.
//!
//! The directory is made `0700` and must be a directory, not a link to one. The
//! text goes to a scratch file beside the target, created new with mode `0600`
//! (a stale scratch, or a link planted under its name, is removed first, and
//! never followed), then renamed over the target, which replaces whatever entry
//! held that name rather than writing through it.

use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// Write `bytes` to `path` for its owner alone.
pub fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the file has no directory"))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    if !std::fs::symlink_metadata(dir)?.is_dir() {
        return Err(std::io::Error::other(format!(
            "{} is a link, not a directory of compux's own",
            dir.display()
        )));
    }
    let scratch = path.with_extension("writing");
    match std::fs::remove_file(&scratch) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&scratch)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&scratch, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("compux-private-{name}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("the old scratch directory");
        }
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .expect("it is there")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn the_file_and_the_directory_it_makes_are_the_owners_alone() {
        let dir = scratch("modes");
        let file = dir.join("compux").join("state.json");

        write(&file, b"{}").expect("written");

        assert_eq!(mode(&dir.join("compux")), 0o700);
        assert_eq!(mode(&file), 0o600);
        assert_eq!(std::fs::read(&file).expect("it reads"), b"{}");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    // A link planted under the scratch name is removed, never written through,
    // and the target is a file of compux's own afterwards.
    #[test]
    fn a_link_planted_at_the_scratch_name_is_removed_not_followed() {
        let dir = scratch("planted");
        let victim = dir.join("victim");
        std::fs::write(&victim, "the person's").expect("a victim");
        let file = dir.join("state.json");
        std::os::unix::fs::symlink(&victim, dir.join("state.writing")).expect("a planted link");

        write(&file, b"ours").expect("written");

        assert_eq!(
            std::fs::read_to_string(&victim).expect("it reads"),
            "the person's"
        );
        assert!(!std::fs::symlink_metadata(&file)
            .expect("there")
            .is_symlink());
        assert_eq!(std::fs::read(&file).expect("it reads"), b"ours");
        assert_eq!(mode(&file), 0o600);
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }

    // A directory that is a link would put the file wherever the link points.
    #[test]
    fn a_directory_that_is_a_link_is_refused_and_nothing_is_written() {
        let dir = scratch("linked-dir");
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("a directory elsewhere");
        std::os::unix::fs::symlink(&elsewhere, dir.join("compux")).expect("a planted link");

        let refused = write(&dir.join("compux").join("state.json"), b"ours");

        assert!(refused.is_err(), "{refused:?}");
        let written: Vec<_> = std::fs::read_dir(&elsewhere).expect("it lists").collect();
        assert!(written.is_empty(), "{written:?}");
        std::fs::remove_dir_all(&dir).expect("cleaned up");
    }
}
