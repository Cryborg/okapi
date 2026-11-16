//! The listing the tools that search the user's folders share: each folder
//! searched, read down from its root one folder at a time, never through a
//! link nor into another mount, a hidden folder, a project, a trash or an
//! exclusion, a file of several names listed once ([`list`]). What it read
//! of each file ([`Seen`]) is what the file must still be when it is read
//! again just before it moves to the trash ([`located`], [`Seen::unchanged`])
//! and once moved ([`Seen::is_moved`]): the check of invariant 7.

mod roots;
mod walk;

use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use okapi_platform::{Directory, EntryStatus, FileIdentity, Unreached, reach_with};

pub(crate) use roots::{Mounts, Opened, list, open_roots};
// The comparison of the duplicates stops on it too.
#[cfg(feature = "duplicates")]
pub(crate) use walk::Cancelled;
pub(crate) use walk::{Counts, Listing, Measure, Owners};

/// A file listed.
#[derive(Debug)]
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    /// The search root it lies below, by its rank among the roots.
    pub(crate) root: usize,
    pub(crate) status: Seen,
}

/// What the search read of a file, and what it must still be to be moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) identity: FileIdentity,
    pub(crate) size: u64,
    /// What it takes on disk ([`EntryStatus::on_disk`]), never compared
    /// again: compressing or sharing its blocks changes it, not the file.
    pub(crate) on_disk: u64,
    pub(crate) modified: SystemTime,
    pub(crate) changed: SystemTime,
    pub(crate) links: u64,
    pub(crate) owner: u32,
}

impl Seen {
    pub(crate) fn of(status: &EntryStatus) -> Self {
        Self {
            identity: status.identity,
            size: status.size,
            on_disk: status.on_disk(),
            modified: status.modified,
            changed: status.changed,
            links: status.links,
            owner: status.owner,
        }
    }

    /// Whether `status` is still this file, unchanged: the same object, of
    /// the same size, dates, owner and names (a name made moves the change
    /// date, but not on every filesystem). A write moves the inode's change
    /// date, even one that sets the modification date back; only two writes
    /// within one tick of a coarse clock could go unseen. FAT keeps no change
    /// date of its own (a date set back sets it back too): the size is the
    /// little this catches there.
    pub(crate) fn unchanged(&self, status: &EntryStatus) -> bool {
        status.identity == self.identity
            && status.changed == self.changed
            && status.size == self.size
            && status.modified == self.modified
            && status.owner == self.owner
            && status.links == self.links
    }

    /// Whether `status`, read in the trash once moved, is this file: moving
    /// changes the inode's change date, so it is known by its identity, its
    /// size and its content's date.
    pub(crate) fn is_moved(&self, status: &EntryStatus) -> bool {
        status.identity == self.identity
            && status.size == self.size
            && status.modified == self.modified
    }
}

/// A folder searched, as it was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Root {
    pub(crate) path: PathBuf,
    pub(crate) identity: FileIdentity,
    /// Whether its filesystem may show the files of another under other
    /// identities (FUSE, network, stacked, or unknown): a copy found there
    /// may be the very file it seems to duplicate.
    pub(crate) may_alias: bool,
    /// Whether its filesystem keeps no change date of its own (FAT, or
    /// unknown): a copy written there then dated back looks unchanged.
    pub(crate) undated: bool,
}

/// What a search met, beyond what it found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SearchStats {
    pub directories: u64,
    /// Plain files listed, of any size (but one mounted on another).
    pub files: u64,
    pub below_minimum: u64,
    /// Files met again under another name (hard links): listed once each.
    pub other_names: u64,
    /// Folders, or entries of one, that could not be read.
    pub unreadable: u64,
    /// Folders replaced, or turned into links, while listed: not read.
    pub changed_directories: u64,
    pub hidden_directories: u64,
    /// Folders holding a `.git` (but a folder the user chose): a project's
    /// files are never offered.
    pub projects: u64,
    pub dependency_directories: u64,
    /// Trash folders: what lies there is on its way out.
    pub trash_directories: u64,
    /// Mounts not entered: folders, or files mounted on another.
    pub other_mounts: u64,
    pub excluded: u64,
    /// Files of another user, never kept (the large files).
    pub other_owners: u64,
    /// Files that changed, went, or could not be read while compared (the
    /// duplicates): never in a group.
    pub dropped: u64,
}

/// Why a search did not start.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("no folder to search")]
    NoRoot,
    #[error("the folder {} cannot be searched", path.display())]
    Root {
        path: PathBuf,
        #[source]
        error: io::Error,
    },
    #[error("the thread of the search could not be started")]
    Thread(#[source] io::Error),
}

/// Whether a folder named `name` is named as a trash is, wherever it lies:
/// freedesktop's (`Trash`, in the data folder; `.Trash` or `.Trash-<uid>`,
/// at the top of another disk) or Windows' on a disk it shares, whatever its
/// letters' case (Windows writes `$Recycle.Bin` on its own disk,
/// `$RECYCLE.BIN` on the others).
pub(crate) fn trash_folder(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name == TRASH_FOLDER
            || name.starts_with(HIDDEN_TRASH)
            || name.eq_ignore_ascii_case(WINDOWS_TRASH)
    })
}

/// The trash folder of the freedesktop specification, in the data folder.
const TRASH_FOLDER: &str = "Trash";
/// What the trash of another disk is named, at its top (`.Trash`,
/// `.Trash-1000`).
const HIDDEN_TRASH: &str = ".Trash";
/// What Windows names the trash at the top of each of its disks, since
/// Vista. The `RECYCLER` of XP is left out: it is also a common name of a
/// folder of the user's (« Recycler »).
const WINDOWS_TRASH: &str = "$RECYCLE.BIN";

/// The folder holding the file at `path`, below `root`, reached from the
/// filesystem root without a link (invariant 3), and the file's name in it;
/// `None` when a folder on the way is gone.
pub(crate) fn parent_of<'a>(
    path: &'a Path,
    root: &Root,
) -> Result<Option<(Directory, &'a OsStr)>, Unreached> {
    let below = path
        .strip_prefix(&root.path)
        .map_err(|_| Unreached::Changed)?;
    let mut names: Vec<&OsStr> = Vec::new();
    for component in below.components() {
        let Component::Normal(name) = component else {
            return Err(Unreached::Changed);
        };
        names.push(name);
    }
    let name = names.pop().ok_or(Unreached::Changed)?;
    let reached = reach_with(&root.path, root.identity, &names, |_, _| true)?;
    Ok(reached.map(|reached| (reached.directory, name)))
}

/// Why a file listed is not, read again, the file the search read.
#[derive(Debug)]
pub(crate) enum Unlocated {
    /// Written, replaced, or a folder on its way changed.
    Changed,
    Gone,
    /// The system refused, on the way or on the file.
    Refused(io::Error),
}

/// The folder holding the file listed at `path` below `root`, reached from
/// `/` without a link, and its name there, if it is still the file the
/// search read as `seen`.
pub(crate) fn located<'a>(
    path: &'a Path,
    root: &Root,
    seen: &Seen,
) -> Result<(Directory, &'a OsStr), Unlocated> {
    let (folder, name) = parent_of(path, root)
        .map_err(|unreached| match unreached {
            Unreached::Changed => Unlocated::Changed,
            Unreached::Io(error) => Unlocated::Refused(error),
        })?
        .ok_or(Unlocated::Gone)?;
    match folder.status_of(name) {
        Ok(status) if seen.unchanged(&status) => Ok((folder, name)),
        Ok(status) => {
            tracing::debug!(
                path = %path.display(),
                listed = ?seen,
                read = ?status,
                "file read again is no longer the one listed"
            );
            Err(Unlocated::Changed)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(Unlocated::Gone),
        Err(error) => Err(Unlocated::Refused(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use okapi_platform::EntryType;

    use super::*;

    const SIZE: u64 = 10;

    fn status() -> EntryStatus {
        let date = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        EntryStatus {
            kind: EntryType::File,
            identity: FileIdentity::new(1, 2),
            last_change: date,
            modified: date,
            changed: date,
            links: 1,
            owner: 1000,
            sticky: false,
            locked: false,
            size: SIZE,
            allocated: None,
            mount_root: Some(false),
        }
    }

    #[test]
    fn a_file_is_unchanged_only_when_all_that_was_read_is_alike() {
        let seen = Seen::of(&status());
        assert!(seen.unchanged(&status()));
        let resized = EntryStatus {
            size: 11,
            ..status()
        };
        assert!(!seen.unchanged(&resized), "cut or grown");
        let redated = EntryStatus {
            modified: status().modified + Duration::from_secs(2),
            ..status()
        };
        // No filesystem known moves one date without the other: a guard.
        assert!(!seen.unchanged(&redated), "written, its change date kept");
        let replaced = EntryStatus {
            identity: FileIdentity::new(1, 3),
            ..status()
        };
        assert!(!seen.unchanged(&replaced), "another file");
        let changed = EntryStatus {
            changed: status().changed + Duration::from_secs(1),
            ..status()
        };
        assert!(!seen.unchanged(&changed), "written, its date set back");
        let given = EntryStatus {
            owner: 1001,
            ..status()
        };
        assert!(!seen.unchanged(&given), "given to another");
        let named = EntryStatus {
            links: 2,
            ..status()
        };
        assert!(!seen.unchanged(&named), "given another name");
        let compressed = EntryStatus {
            allocated: Some(SIZE / 2),
            ..status()
        };
        assert!(seen.unchanged(&compressed), "its blocks shared or packed");
    }

    #[test]
    fn what_was_moved_is_the_copy_only_by_identity_size_and_date() {
        let seen = Seen::of(&status());
        // Moving changes the inode's change date.
        let moved = EntryStatus {
            changed: status().changed + Duration::from_secs(1),
            ..status()
        };
        assert!(seen.is_moved(&moved));
        let other = EntryStatus {
            identity: FileIdentity::new(1, 9),
            ..moved
        };
        assert!(!seen.is_moved(&other), "another file");
        let resized = EntryStatus {
            size: SIZE + 1,
            ..moved
        };
        assert!(!seen.is_moved(&resized), "cut or grown");
        let written = EntryStatus {
            modified: status().modified + Duration::from_secs(1),
            ..moved
        };
        assert!(!seen.is_moved(&written), "written");
    }
}
