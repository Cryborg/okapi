//! What lets an entry of a trash go: its `.trashinfo`, read by the scan
//! and again by the cleaning just before the entry is removed (invariant 2),
//! says it went there long enough ago.
//!
//! The date of a `.trashinfo` is local time written without its zone; the
//! engine knows no zone (it would widen what the privileged helper is built
//! from), so it reads it as universal time and asks for [`DATE_MARGIN`]
//! more, which no zone is off by.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use okapi_platform::{
    Deletion, Directory, InformationRefusal, InformationRemoval, Unreached, information_folder,
    read_deletion, remove_information,
};

/// How much older than its cutoff an entry's deletion date must be: a day.
/// Read as universal time, a local date is at most 12 hours too old (west
/// of Greenwich, UTC-12); east of it, it is younger, which only keeps more.
pub(crate) const DATE_MARGIN: Duration = Duration::from_hours(24);

/// Why an entry of a trash may not go.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TrashRefusal {
    /// Its `.trashinfo` does not say when it went there.
    #[error(transparent)]
    Information(InformationRefusal),
    /// It went there less than [`DATE_MARGIN`] before `cutoff` (or its
    /// `.trashinfo` says so): too recently, or in the future.
    #[error("it went to the trash at {date:?}, within a day of {cutoff:?}")]
    Recent {
        date: SystemTime,
        cutoff: SystemTime,
    },
    /// Its `.trashinfo` is not the remover's, or the remover is unknown.
    #[error("its information belongs to user {owner}, not to {remover:?}")]
    NotOwned { owner: u32, remover: Option<u32> },
}

/// Why the `info` folder of a trash cannot be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FolderRefusal {
    /// The root is not the `files` folder of a trash.
    #[error("the root is no `files` folder of a trash")]
    NotATrash,
    #[error("the information folder is not there")]
    Absent,
    /// Something on the way is a link, or no longer a folder.
    #[error("a folder on the way to the information folder changed")]
    Changed,
    #[error(transparent)]
    Unreadable(io::Error),
}

/// The `info` folder of a trash, and what the deletion dates it admits are
/// judged against.
#[derive(Debug)]
pub(crate) struct TrashGate {
    information: Directory,
    /// Where it is, for the journal.
    folder: PathBuf,
    /// The cutoff of the entries: an entry went there [`DATE_MARGIN`]
    /// before it at the latest.
    cutoff: SystemTime,
    /// Who removes the entries, and must own their `.trashinfo`; none
    /// admitted when unknown.
    remover: Option<u32>,
}

impl TrashGate {
    /// The gate of the trash whose `files` folder is `root` (a real path),
    /// for entries of cutoff `cutoff`, removed by `remover`; its `info`
    /// folder reached from `/`, never through a link.
    pub(crate) fn open(
        root: &Path,
        cutoff: SystemTime,
        remover: Option<u32>,
    ) -> Result<Self, FolderRefusal> {
        let folder = information_folder(root).ok_or(FolderRefusal::NotATrash)?;
        let information = match okapi_platform::reach_path(&folder) {
            Ok(Some(information)) => information,
            Ok(None) => return Err(FolderRefusal::Absent),
            Err(Unreached::Changed) => return Err(FolderRefusal::Changed),
            Err(Unreached::Io(error)) => return Err(FolderRefusal::Unreadable(error)),
        };
        Ok(Self {
            information,
            folder,
            cutoff,
            remover,
        })
    }

    /// When the entry `name` of the trash went there, if long enough ago.
    pub(crate) fn admits(&self, name: &OsStr) -> Result<Deletion, TrashRefusal> {
        let deletion = read_deletion(&self.information, name).map_err(TrashRefusal::Information)?;
        let owner = deletion.information.owner;
        if self.remover != Some(owner) {
            return Err(TrashRefusal::NotOwned {
                owner,
                remover: self.remover,
            });
        }
        let long_enough = deletion
            .date
            .checked_add(DATE_MARGIN)
            .is_some_and(|aged| aged <= self.cutoff);
        if long_enough {
            Ok(deletion)
        } else {
            Err(TrashRefusal::Recent {
                date: deletion.date,
                cutoff: self.cutoff,
            })
        }
    }

    /// Whether the `.trashinfo` `deletion` read may be removed: the folder
    /// writable, the file neither immutable nor another's in a sticky one.
    ///
    /// # Errors
    ///
    /// The system's refusal.
    pub(crate) fn ensure_forgettable(&self, deletion: &Deletion) -> io::Result<()> {
        self.information.ensure_removable(&deletion.information)
    }

    /// Removes the `.trashinfo` of the entry `name`, gone with all it held,
    /// if it is still the one `deletion` read; said in the journal either
    /// way, since nothing else tells it. One left behind restores nothing:
    /// the entry is gone, which the receipt says.
    pub(crate) fn forget(&self, name: &OsStr, deletion: &Deletion) {
        match remove_information(&self.information, name, &deletion.information) {
            Ok(InformationRemoval::Removed) => {
                tracing::info!(entry = ?name, "trash information removed with its entry");
            }
            Ok(InformationRemoval::Gone) => {
                tracing::info!(entry = ?name, "trash information already removed");
            }
            Ok(InformationRemoval::Replaced) => {
                tracing::warn!(entry = ?name, information = ?self.folder, expected = ?deletion.information.identity, "trash information replaced since it was read: left as it is");
            }
            Err(error) => {
                tracing::warn!(entry = ?name, information = ?self.folder, %error, code = error.raw_os_error(), "trash information not removed with its entry");
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_trash_is_admitted_a_day_past_its_cutoff_only_by_its_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let data = fs::canonicalize(temporary.path()).unwrap();
        fs::create_dir_all(data.join("Trash/files")).unwrap();
        fs::create_dir_all(data.join("Trash/info")).unwrap();
        let write = |name: &str, date: &str| {
            fs::write(
                data.join("Trash/info").join(format!("{name}.trashinfo")),
                format!("[Trash Info]\nPath=/a\nDeletionDate={date}\n"),
            )
            .unwrap();
        };
        // 2100-01-31T00:00:00, read as universal time.
        let cutoff = SystemTime::UNIX_EPOCH + Duration::from_hours(1_140_288);
        write("old", "2100-01-30T00:00:00");
        write("within", "2100-01-30T00:00:01");
        write("future", "2200-01-01T00:00:00");
        let owner = fs::metadata(data.join("Trash/info/old.trashinfo")).unwrap();
        let owner = std::os::unix::fs::MetadataExt::uid(&owner);
        let gate = TrashGate::open(&data.join("Trash/files"), cutoff, Some(owner)).unwrap();
        assert!(gate.admits(OsStr::new("old")).is_ok());
        for name in ["within", "future"] {
            assert!(
                matches!(
                    gate.admits(OsStr::new(name)),
                    Err(TrashRefusal::Recent { .. })
                ),
                "{name}"
            );
        }
        assert!(matches!(
            gate.admits(OsStr::new("absent")),
            Err(TrashRefusal::Information(InformationRefusal::Absent))
        ));
        let other = TrashGate::open(
            &data.join("Trash/files"),
            cutoff,
            Some(owner.wrapping_add(1)),
        )
        .unwrap();
        assert!(matches!(
            other.admits(OsStr::new("old")),
            Err(TrashRefusal::NotOwned { .. })
        ));
        // A remover unknown owns nothing.
        let unknown = TrashGate::open(&data.join("Trash/files"), cutoff, None).unwrap();
        assert!(matches!(
            unknown.admits(OsStr::new("old")),
            Err(TrashRefusal::NotOwned { .. })
        ));
        // Around the clock's origin, the same day of margin.
        write("ancient", "1969-12-30T00:00:00");
        write("eve", "1969-12-31T00:00:01");
        let early = TrashGate::open(
            &data.join("Trash/files"),
            SystemTime::UNIX_EPOCH,
            Some(owner),
        )
        .unwrap();
        assert!(early.admits(OsStr::new("ancient")).is_ok());
        for name in ["eve", "old"] {
            assert!(
                matches!(
                    early.admits(OsStr::new(name)),
                    Err(TrashRefusal::Recent { .. })
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn an_info_folder_absent_a_link_or_not_beside_files_is_refused() {
        let temporary = tempfile::tempdir().unwrap();
        let data = fs::canonicalize(temporary.path()).unwrap();
        let now = SystemTime::now();
        fs::create_dir_all(data.join("Trash/files")).unwrap();
        assert!(matches!(
            TrashGate::open(&data.join("Trash/files"), now, None),
            Err(FolderRefusal::Absent)
        ));
        fs::create_dir(data.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(data.join("elsewhere"), data.join("Trash/info")).unwrap();
        assert!(matches!(
            TrashGate::open(&data.join("Trash/files"), now, None),
            Err(FolderRefusal::Changed)
        ));
        assert!(matches!(
            TrashGate::open(&data.join("Trash"), now, None),
            Err(FolderRefusal::NotATrash)
        ));
    }
}
