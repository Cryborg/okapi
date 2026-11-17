//! How the history is kept from growing without end: once a cleaning opens
//! it, a history larger than [`ROTATION_SIZE`] is set aside as
//! `history.1.jsonl`, the one set aside before replaced, its lines (its
//! deletion logs among them) lost. Each file thus holds about the limit,
//! plus what is written between two cleanings (uninstallations included)
//! and the largest cleaning's lines, a cleaning's deletion log never cut;
//! while setting aside is refused or in use, the history grows on, which is
//! traced, the louder the further beyond the limit.
//!
//! No line another run adds is ever lost: every run that writes holds a
//! shared lock on its file for as long as it has it open, and checks once
//! locked that the history's name still leads to it (else it opens it
//! again); setting aside takes the exclusive lock of both files, so it never
//! moves nor replaces a file someone writes to under its lock; a run the
//! system refuses the lock looks again before it writes, and never writes
//! to a file set aside (but for a moment between the look and the write).
//! Where locks are not taken (outside Unix,
//! `okapi_platform::try_lock_journal_shared`), nor the identity of files
//! known, the history is never set aside.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use super::HISTORY_FILE;

/// Beyond it, the history is set aside before the next cleaning.
pub(super) const ROTATION_SIZE: u64 = 8 * 1024 * 1024;

/// How many times the limit a history kept from being set aside grows to
/// before it is said louder.
const FAR_BEYOND: u64 = 2;

/// The history set aside.
pub(super) const ROTATED_FILE: &str = "history.1.jsonl";

/// Why the history was rightly not set aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// Files have no known identity here: never done.
    Unsupported,
    /// Another run set it aside meanwhile.
    Moved,
    /// The name of the one set aside leads elsewhere than the file opened
    /// (a link, a file replaced).
    SetAsideNotAFile,
}

/// What setting the history aside did.
#[derive(Debug)]
pub(super) enum Rotation {
    /// Not grown beyond the limit, or none yet.
    NotGrown,
    Done {
        size: u64,
        replaced: Option<u64>,
    },
    /// Someone writes to the history, or to the one set aside: tried again
    /// before the next cleaning.
    InUse {
        file: &'static str,
        size: u64,
    },
    Refused {
        why: Refusal,
        size: u64,
    },
    Failed {
        step: &'static str,
        error: io::Error,
        /// The history's size, when known before the failure.
        size: Option<u64>,
    },
}

impl Rotation {
    /// Not set aside while far beyond what is kept: said louder, as the
    /// history grows on.
    fn far_beyond_kept(&self) -> bool {
        let size = match self {
            // Set aside by another run meanwhile: not grown on.
            Self::NotGrown
            | Self::Done { .. }
            | Self::Refused {
                why: Refusal::Moved,
                ..
            } => None,
            Self::InUse { size, .. } | Self::Refused { size, .. } => Some(*size),
            Self::Failed { size, .. } => *size,
        };
        size.is_some_and(|size| size > FAR_BEYOND.saturating_mul(ROTATION_SIZE))
    }

    pub(super) fn trace(&self, directory: &Path) {
        let directory = directory.display();
        match self {
            Self::NotGrown => tracing::debug!(%directory, "history not grown: kept as it is"),
            Self::Done { size, replaced } => {
                tracing::info!(%directory, size, ?replaced, "history set aside");
            }
            _ if self.far_beyond_kept() => {
                tracing::error!(%directory, rotation = ?self, "history far beyond what is kept, not set aside");
            }
            // Expected: another run writes to it, or files have no known
            // identity here.
            Self::InUse { file, size } => {
                tracing::info!(%directory, file, size, "history not set aside: in use");
            }
            Self::Refused {
                why: Refusal::Unsupported,
                size,
            } => {
                tracing::info!(%directory, size, "history not set aside: unsupported here");
            }
            Self::Refused {
                why: Refusal::Moved,
                size,
            } => {
                tracing::info!(%directory, size, "history set aside by another run meanwhile");
            }
            Self::Refused { why, size } => {
                tracing::warn!(%directory, ?why, size, "history not set aside");
            }
            Self::Failed { step, error, size } => tracing::warn!(
                %directory,
                step,
                %error,
                code = error.raw_os_error(),
                ?size,
                "history not set aside"
            ),
        }
    }
}

/// Where the history of `directory` is set aside.
pub(super) fn rotated_path(directory: &Path) -> PathBuf {
    directory.join(ROTATED_FILE)
}

/// Whether `path` still names `file` itself (never through a link). Taken
/// to where files have no known identity: the history is never set aside
/// there.
pub(super) fn still_named(file: &File, path: &Path) -> io::Result<bool> {
    if !okapi_platform::IDENTITY_SUPPORTED {
        return Ok(true);
    }
    let opened = okapi_platform::file_identity(&file.metadata()?);
    match fs::symlink_metadata(path) {
        Ok(named) => Ok(opened.is_some() && okapi_platform::file_identity(&named) == opened),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Sets the history of `directory` aside when it is larger than `limit`.
pub(super) fn rotate_if_grown(directory: &Path, limit: u64) -> Rotation {
    let path = directory.join(HISTORY_FILE);
    let size = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Rotation::NotGrown,
        Err(error) => {
            return Rotation::Failed {
                step: "look",
                error,
                size: None,
            };
        }
    };
    if size <= limit {
        return Rotation::NotGrown;
    }
    if !okapi_platform::IDENTITY_SUPPORTED {
        return Rotation::Refused {
            why: Refusal::Unsupported,
            size,
        };
    }
    // Never followed: a link is refused by the opening.
    match okapi_platform::open_journal(&path) {
        Ok(current) => set_aside(&current, directory, limit),
        Err(error) => Rotation::Failed {
            step: "open",
            error,
            size: Some(size),
        },
    }
}

/// Sets `current`, the history of `directory` opened, aside when it is
/// larger than `limit`: in steps a test can come between.
fn set_aside(current: &File, directory: &Path, limit: u64) -> Rotation {
    let path = directory.join(HISTORY_FILE);
    let rotated = rotated_path(directory);
    let size = match current.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            return Rotation::Failed {
                step: "size",
                error,
                size: None,
            };
        }
    };
    let failed = |step| {
        move |error| Rotation::Failed {
            step,
            error,
            size: Some(size),
        }
    };
    match okapi_platform::try_lock_journal_exclusive(current) {
        Ok(true) => {}
        Ok(false) => {
            return Rotation::InUse {
                file: HISTORY_FILE,
                size,
            };
        }
        Err(error) => return failed("lock")(error),
    }
    // Another run may have set it aside between its opening and the lock.
    match still_named(current, &path) {
        Ok(true) => {}
        Ok(false) => {
            return Rotation::Refused {
                why: Refusal::Moved,
                size,
            };
        }
        Err(error) => return failed("look")(error),
    }
    // Its size again, now that no one writes to it.
    let size = match current.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => return failed("size")(error),
    };
    if size <= limit {
        return Rotation::NotGrown;
    }
    // The one set aside before, replaced only if no one still writes to it
    // and its name leads to it itself (a link is refused): opened for
    // writing, as an exclusive lock asks on a network filesystem, or for
    // reading when it is read-only (its renaming needs only the directory).
    // The lock is held until the renaming replaced it.
    let previous = match okapi_platform::open_existing_journal(&rotated) {
        Ok(previous) => Some(previous),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            match okapi_platform::open_for_reading(&rotated) {
                Ok(previous) => Some(previous),
                Err(error) => return failed("open set aside")(error),
            }
        }
        Err(error) => return failed("open set aside")(error),
    };
    let mut replaced = None;
    if let Some(previous) = &previous {
        match okapi_platform::try_lock_journal_exclusive(previous) {
            Ok(true) => {}
            Ok(false) => {
                return Rotation::InUse {
                    file: ROTATED_FILE,
                    size,
                };
            }
            Err(error) => return failed("lock set aside")(error),
        }
        match still_named(previous, &rotated) {
            Ok(true) => {}
            Ok(false) => {
                return Rotation::Refused {
                    why: Refusal::SetAsideNotAFile,
                    size,
                };
            }
            Err(error) => return failed("look set aside")(error),
        }
        // Its size only says how much is lost: unknown, it goes still.
        replaced = previous
            .metadata()
            .inspect_err(|error| {
                tracing::warn!(%error, code = error.raw_os_error(), "size of the history set aside before unknown");
            })
            .ok()
            .map(|metadata| metadata.len());
    }
    match fs::rename(&path, &rotated) {
        Ok(()) => Rotation::Done { size, replaced },
        Err(error) => failed("rename")(error),
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::super::tests::{entry, outcome};
    use super::super::{History, HistoryEntry, HistoryOutcome, HistoryRecord};
    use super::*;
    use crate::StoreError;

    /// A history of `directory` grown to `cleanings` cleanings, each with a
    /// deletion log of one entry.
    fn grown(directory: &Path, cleanings: i64) {
        let mut history = History::open(directory).unwrap();
        for time in 0..cleanings {
            history
                .record(&HistoryEntry {
                    time,
                    ..entry(1, Some(vec![outcome("removed", Some(4), None)]))
                })
                .unwrap();
        }
    }

    fn size_of(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }

    fn times(directory: &Path) -> Vec<i64> {
        History::read_all::<Vec<HistoryOutcome>>(directory)
            .unwrap()
            .records
            .iter()
            .map(|record| match record {
                HistoryRecord::Cleaning { line, .. } => line.time,
                HistoryRecord::Uninstall(line) => line.time,
                HistoryRecord::Update(line) => line.time,
            })
            .collect()
    }

    #[test]
    fn a_history_grown_too_large_is_set_aside_the_older_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        grown(directory.path(), 3);
        let limit = size_of(&path);
        assert!(matches!(
            rotate_if_grown(directory.path(), limit),
            Rotation::NotGrown
        ));
        grown(directory.path(), 1);
        let size = size_of(&path);
        assert!(matches!(
            rotate_if_grown(directory.path(), limit),
            Rotation::Done { size: done, replaced: None } if done == size
        ));
        assert!(!path.exists());
        assert_eq!(size_of(&rotated_path(directory.path())), size);
        // Grown again: the one set aside before is replaced.
        let mut history = History::open(directory.path()).unwrap();
        for time in 10..20 {
            history
                .record(&HistoryEntry {
                    time,
                    ..entry(1, None)
                })
                .unwrap();
        }
        drop(history);
        assert!(matches!(
            rotate_if_grown(directory.path(), 1),
            Rotation::Done { replaced: Some(replaced), .. } if replaced == size
        ));
        assert_eq!(times(directory.path()), (10..20).collect::<Vec<_>>());
    }

    #[test]
    fn a_history_someone_writes_to_is_never_set_aside() {
        let directory = tempfile::tempdir().unwrap();
        grown(directory.path(), 2);
        let writing = History::open(directory.path()).unwrap();
        assert!(matches!(
            rotate_if_grown(directory.path(), 1),
            Rotation::InUse {
                file: HISTORY_FILE,
                ..
            }
        ));
        drop(writing);
        assert!(matches!(
            rotate_if_grown(directory.path(), 1),
            Rotation::Done { .. }
        ));
    }

    /// A run that opened the history before it was set aside still writes
    /// to the file set aside: it is never replaced under it.
    #[test]
    fn a_history_set_aside_someone_writes_to_is_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        grown(directory.path(), 2);
        let rotated = rotated_path(directory.path());
        // As a run that locked it while it was the history.
        fs::write(&rotated, b"").unwrap();
        let held = okapi_platform::open_journal(&rotated).unwrap();
        assert!(okapi_platform::try_lock_journal_shared(&held).unwrap());
        assert!(matches!(
            rotate_if_grown(directory.path(), 1),
            Rotation::InUse {
                file: ROTATED_FILE,
                ..
            }
        ));
        assert!(directory.path().join(HISTORY_FILE).exists());
        drop(held);
        assert!(matches!(
            rotate_if_grown(directory.path(), 1),
            Rotation::Done { .. }
        ));
    }

    /// The history opened, then set aside by another run before the lock:
    /// never moved again (the file in its place is not the one locked).
    #[test]
    fn a_history_set_aside_meanwhile_is_not_set_aside_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        grown(directory.path(), 2);
        let opened = okapi_platform::open_journal(&path).unwrap();
        fs::rename(&path, rotated_path(directory.path())).unwrap();
        grown(directory.path(), 3);
        assert!(matches!(
            set_aside(&opened, directory.path(), 1),
            Rotation::Refused {
                why: Refusal::Moved,
                ..
            }
        ));
        assert!(path.exists(), "the new history stays");
        assert_eq!(times(directory.path()), [0, 1, 0, 1, 2]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_set_aside_that_is_a_link_is_never_followed_nor_replaced() {
        let directory = tempfile::tempdir().unwrap();
        grown(directory.path(), 2);
        let target = directory.path().join("elsewhere");
        fs::write(&target, b"kept").unwrap();
        let rotated = rotated_path(directory.path());
        std::os::unix::fs::symlink(&target, &rotated).unwrap();
        assert!(matches!(
            rotate_if_grown(directory.path(), 1),
            Rotation::Failed {
                step: "open set aside",
                ..
            }
        ));
        assert!(fs::symlink_metadata(&rotated).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "kept");
    }

    /// Set aside with no cleaning line after it yet (a cleaning that wrote
    /// none, an uninstallation since): the last cleaning is still found.
    #[test]
    fn the_last_cleaning_is_found_in_the_history_set_aside() {
        let directory = tempfile::tempdir().unwrap();
        grown(directory.path(), 2);
        assert!(matches!(
            rotate_if_grown(directory.path(), 1),
            Rotation::Done { .. }
        ));
        let last = || {
            History::last_cleaning(directory.path())
                .unwrap()
                .map(|last| last.time)
        };
        assert_eq!(last(), Some(1), "no history yet");
        History::open(directory.path()).unwrap();
        assert_eq!(last(), Some(1), "an empty one");
        super::super::tests::uninstalled(directory.path(), 50);
        assert_eq!(last(), Some(1), "past an uninstallation");
        // A line that cannot be read: never the one before it.
        super::super::tests::append(directory.path(), b"{\"version\":1,\"tim\n");
        assert_eq!(last(), None);
        grown(directory.path(), 1);
        assert_eq!(last(), Some(0), "the new history's own");
    }

    /// Never read on past what says no last cleaning, nor beyond the bound
    /// the two files share, nor through one set aside that cannot be read.
    #[test]
    fn the_history_set_aside_is_read_on_only_past_uninstallations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        let rotated = rotated_path(directory.path());
        grown(directory.path(), 2);
        fs::rename(&path, &rotated).unwrap();
        let last = || History::last_cleaning(directory.path());
        // A deletion log without its cleaning line: a cleaning may be lost.
        let set_aside = fs::read_to_string(&rotated).unwrap();
        let deletions = set_aside.lines().next().unwrap();
        fs::write(&path, format!("{deletions}\n")).unwrap();
        assert_eq!(last().unwrap(), None);
        // Uninstallations filling all the bound but the cleaning's line.
        let cleaning_line = set_aside.lines().last().unwrap().len() as u64 + 1;
        let name = "x".repeat(usize::try_from(super::super::MAX_LINE - cleaning_line).unwrap());
        fs::write(&path, b"").unwrap();
        History::open(directory.path())
            .unwrap()
            .record_uninstall(&super::super::UninstallEntry {
                name,
                time: 50,
                source: super::super::PackageSource::Deb,
                packages: Vec::new(),
                freed_bytes: None,
                data_removed: false,
            })
            .unwrap();
        assert_eq!(last().unwrap(), None, "beyond the bound");
        fs::write(&path, b"").unwrap();
        super::super::tests::uninstalled(directory.path(), 50);
        assert_eq!(last().unwrap().map(|last| last.time), Some(1));
        // One set aside that cannot be read is never taken for none.
        fs::remove_file(&rotated).unwrap();
        fs::create_dir(&rotated).unwrap();
        assert!(matches!(last(), Err(StoreError::NotAFile { .. })));
        assert!(matches!(
            History::read_all::<Vec<HistoryOutcome>>(directory.path()),
            Err(StoreError::NotAFile { .. })
        ));
    }

    /// One set aside that is read-only (restored from a backup) is
    /// replaced still: its renaming needs only the directory.
    #[test]
    fn a_read_only_set_aside_is_replaced() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let rotated = rotated_path(directory.path());
        fs::write(&rotated, b"").unwrap();
        fs::set_permissions(&rotated, fs::Permissions::from_mode(0o400)).unwrap();
        grown(directory.path(), 2);
        assert!(matches!(
            rotate_if_grown(directory.path(), 1),
            Rotation::Done {
                replaced: Some(0),
                ..
            }
        ));
    }

    /// Kept from being set aside, the further beyond the limit, the louder.
    #[test]
    fn a_history_far_beyond_what_is_kept_is_said_louder() {
        let at = |size| Rotation::InUse {
            file: HISTORY_FILE,
            size,
        };
        let far = FAR_BEYOND * ROTATION_SIZE;
        assert!(!at(far).far_beyond_kept());
        assert!(at(far + 1).far_beyond_kept());
        let refused = Rotation::Refused {
            why: Refusal::Unsupported,
            size: far + 1,
        };
        assert!(refused.far_beyond_kept());
        let failed = |size| Rotation::Failed {
            step: "open set aside",
            error: io::ErrorKind::PermissionDenied.into(),
            size,
        };
        assert!(failed(Some(far + 1)).far_beyond_kept());
        assert!(!failed(None).far_beyond_kept(), "size unknown");
        assert!(
            !Rotation::Done {
                size: far + 1,
                replaced: None
            }
            .far_beyond_kept(),
            "set aside"
        );
    }

    /// Both files read, each line once, their skipped lines summed.
    #[test]
    fn every_line_of_both_files_is_read_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        let rotated = rotated_path(directory.path());
        grown(directory.path(), 2);
        super::super::tests::append(directory.path(), b"{\"version\":1,\"tim\n");
        // Cut short, never ended: no one writes to it once set aside.
        super::super::tests::append(directory.path(), b"{\"version\":1");
        fs::rename(&path, &rotated).unwrap();
        grown(directory.path(), 1);
        for _ in 0..2 {
            super::super::tests::append(directory.path(), b"not json\n");
        }
        let read = History::read_all::<Vec<HistoryOutcome>>(directory.path()).unwrap();
        assert_eq!(read.skipped, 4);
        assert_eq!(times(directory.path()), [0, 1, 0]);
        // Set aside between the two openings: the same file, read once.
        fs::remove_file(&rotated).unwrap();
        fs::hard_link(&path, &rotated).unwrap();
        assert_eq!(times(directory.path()), [0]);
    }

    /// The opening before a cleaning sets a history grown too large aside,
    /// never the opening before an uninstallation: no cleaning line would
    /// follow it.
    #[test]
    fn a_cleaning_sets_a_history_grown_too_large_aside() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        History::open(directory.path()).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(ROTATION_SIZE + 1)
            .unwrap();
        super::super::tests::uninstalled(directory.path(), 5);
        assert!(!rotated_path(directory.path()).exists());
        let grown_to = size_of(&path);
        let opened = History::open_for_cleaning(Some(directory.path()), true).unwrap();
        assert!(matches!(opened, super::super::Opened::Open(_)));
        assert_eq!(size_of(&rotated_path(directory.path())), grown_to);
        assert_eq!(size_of(&path), 0, "a new history, for the cleaning's line");
    }

    #[test]
    fn a_history_that_is_a_link_is_never_set_aside() {
        let directory = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        grown(elsewhere.path(), 2);
        std::os::unix::fs::symlink(
            elsewhere.path().join(HISTORY_FILE),
            directory.path().join(HISTORY_FILE),
        )
        .unwrap();
        assert!(matches!(
            rotate_if_grown(directory.path(), 1),
            Rotation::Failed { step: "open", .. }
        ));
        assert!(!rotated_path(directory.path()).exists());
    }
}
