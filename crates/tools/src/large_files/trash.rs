//! Large files moved to the trash, named by identifiers of a report only
//! (invariant 1), one after the other: each is read again just before it
//! goes, and once in the trash (invariant 7).

use std::collections::BTreeSet;
use std::ffi::OsString;

use okapi_engine::Exclusions;
use okapi_platform::{EntryStatus, Trash, TrashError, tells_mount_roots};

use super::report::{FileId, LargeFile, LargeFileReport};
use crate::listing::{Unlocated, located};
pub use crate::trashing::Refusal;
use crate::trashing::{Moved, deleted_now, moved, one_at_a_time};

/// Why a file stayed where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// The identifier is not of this report (invariant 1).
    UnknownFile,
    /// This system cannot tell a mount point from a folder: nothing moves.
    UnsupportedSystem,
    /// It has other names: moving it would free nothing.
    OtherNames,
    /// The user excluded it since the search.
    Excluded,
    /// It is on another filesystem than the trash.
    OtherDevice,
    /// It is no longer the file the search read: written, replaced, given
    /// to another, given another name, or a folder on its way changed.
    Changed,
    /// It is no longer at its path: removed, or it or a folder on its way
    /// moved elsewhere.
    Gone,
    /// The system refused, with its error.
    Refused(Refusal),
}

impl KeepReason {
    /// A stable key, for traces.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::UnknownFile => "unknownFile",
            Self::UnsupportedSystem => "unsupportedSystem",
            Self::OtherNames => "otherNames",
            Self::Excluded => "excluded",
            Self::OtherDevice => "otherDevice",
            Self::Changed => "changed",
            Self::Gone => "gone",
            Self::Refused(_) => "refused",
        }
    }
}

/// What became of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashResult {
    /// In the trash; what it takes on disk, at most, is freed once the
    /// trash is emptied (less when it shares its blocks).
    Trashed {
        bytes: u64,
    },
    Kept(KeepReason),
    /// A file went to the trash in its name that is not known to be the
    /// file as checked (another program replaced or wrote it meanwhile, or
    /// it could not be read again): it stays there, restorable, and nothing
    /// is counted freed.
    Unchecked,
}

/// What became of one identifier asked for: its file is found in the
/// report by [`LargeFileReport::find`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrashOutcome {
    pub id: FileId,
    pub result: TrashResult,
}

/// What a moving to the trash did, one outcome per identifier asked for, in
/// their order, each once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashReceipt {
    outcomes: Vec<TrashOutcome>,
}

impl TrashReceipt {
    #[must_use]
    pub fn outcomes(&self) -> &[TrashOutcome] {
        &self.outcomes
    }

    /// What went to the trash.
    #[must_use]
    pub fn trashed_bytes(&self) -> u64 {
        self.outcomes
            .iter()
            .map(|outcome| match outcome.result {
                TrashResult::Trashed { bytes } => bytes,
                TrashResult::Kept(_) | TrashResult::Unchecked => 0,
            })
            .fold(0, u64::saturating_add)
    }
}

/// Moves the files `ids` of `report` to `bin`, but never one the user
/// excludes (`exclusions`, read afresh, by path or by identity). Two movings, of any tool,
/// never run at once: this one may wait for another, and reads the disks (a
/// mount that stopped answering holds it), so it is never called on the
/// interface's thread.
#[must_use]
pub fn trash(
    report: &LargeFileReport,
    ids: &[FileId],
    bin: &Trash,
    exclusions: &Exclusions,
) -> TrashReceipt {
    trash_dated(
        report,
        ids,
        bin,
        exclusions,
        &deleted_now(),
        #[cfg(test)]
        None,
    )
}

pub(crate) fn trash_dated(
    report: &LargeFileReport,
    ids: &[FileId],
    bin: &Trash,
    exclusions: &Exclusions,
    deleted_at: &str,
    #[cfg(test)] between: Option<fn(&std::path::Path)>,
) -> TrashReceipt {
    let _span = tracing::info_span!("trash", requested = ids.len()).entered();
    let _one_at_a_time = one_at_a_time();
    let mut seen = BTreeSet::new();
    let supported = tells_mount_roots();
    if !supported {
        tracing::warn!("this system cannot tell a mount point: nothing is moved");
    }
    let mover = Mover {
        report,
        bin,
        exclusions,
        deleted_at,
        #[cfg(test)]
        between,
    };
    let outcomes: Vec<TrashOutcome> = ids
        .iter()
        .copied()
        .filter(|id| seen.insert(*id))
        .map(|id| {
            let result = match report.find(id) {
                None => TrashResult::Kept(KeepReason::UnknownFile),
                Some(_) if !supported => TrashResult::Kept(KeepReason::UnsupportedSystem),
                Some(file) => mover.run(file),
            };
            let outcome = TrashOutcome { id, result };
            trace(report, &outcome);
            outcome
        })
        .collect();
    let receipt = TrashReceipt { outcomes };
    tracing::info!(
        trashed = receipt
            .outcomes()
            .iter()
            .filter(|outcome| matches!(outcome.result, TrashResult::Trashed { .. }))
            .count(),
        bytes = receipt.trashed_bytes(),
        "moving to the trash finished"
    );
    receipt
}

fn trace(report: &LargeFileReport, outcome: &TrashOutcome) {
    let path = report
        .find(outcome.id)
        .map_or_else(String::new, |file| file.path().display().to_string());
    match outcome.result {
        TrashResult::Trashed { bytes } => tracing::info!(path, bytes, "file moved to the trash"),
        TrashResult::Unchecked => tracing::warn!(
            path,
            "a file not known to be the file as checked went to the trash"
        ),
        TrashResult::Kept(KeepReason::UnknownFile) => tracing::warn!(
            id = ?outcome.id,
            "file of another report asked for: kept (invariant 1)"
        ),
        TrashResult::Kept(reason @ KeepReason::Refused(refusal)) => tracing::warn!(
            path,
            reason = reason.key(),
            kind = ?refusal.kind,
            code = refusal.code,
            "file kept: the system refused"
        ),
        TrashResult::Kept(reason) => tracing::info!(path, reason = reason.key(), "file kept"),
    }
}

/// What the trash's answer to moving `file` makes of it.
fn result_of(file: &LargeFile, answer: Result<OsString, TrashError>) -> TrashResult {
    let kept = TrashResult::Kept;
    match moved(answer) {
        Moved::Trashed => TrashResult::Trashed {
            bytes: file.on_disk(),
        },
        Moved::OtherDevice => kept(KeepReason::OtherDevice),
        Moved::Changed => kept(KeepReason::Changed),
        Moved::Unchecked(_) => TrashResult::Unchecked,
        Moved::Refused(refusal) => kept(KeepReason::Refused(refusal)),
    }
}

/// What every file's moving shares.
struct Mover<'a> {
    report: &'a LargeFileReport,
    bin: &'a Trash,
    exclusions: &'a Exclusions,
    deleted_at: &'a str,
    /// What a test does to the file between its check and its move.
    #[cfg(test)]
    between: Option<fn(&std::path::Path)>,
}

impl Mover<'_> {
    /// Moves `file` to the trash, if it is still the file read.
    fn run(&self, file: &LargeFile) -> TrashResult {
        let kept = TrashResult::Kept;
        if !file.frees() {
            return kept(KeepReason::OtherNames);
        }
        if self.exclusions.excludes(file.path()) {
            return kept(KeepReason::Excluded);
        }
        // Nothing is read of a file the trash cannot take.
        if !file.lies_on(self.bin) {
            return kept(KeepReason::OtherDevice);
        }
        // Through a bind mount of a folder excluded since (the folder
        // itself shown elsewhere): read only once the file is known to lie
        // on the trash's disk.
        if self.exclusions.excludes_through_identity(file.path()) {
            tracing::debug!(
                path = %file.path().display(),
                "excluded by identity: a folder on its way is an excluded one"
            );
            return kept(KeepReason::Excluded);
        }
        let Some(root) = self.report.root(file.root()) else {
            return kept(KeepReason::Changed);
        };
        // The file is read last, and moved right after: the window for a
        // write between its check and its move is as small as can be.
        let (folder, name) = match located(file.path(), root, file.seen()) {
            Ok(located) => located,
            Err(Unlocated::Changed) => return kept(KeepReason::Changed),
            Err(Unlocated::Gone) => return kept(KeepReason::Gone),
            Err(Unlocated::Refused(error)) => return kept(KeepReason::Refused((&error).into())),
        };
        #[cfg(test)]
        if let Some(between) = self.between {
            between(file.path());
        }
        let moved = self.bin.put(
            &folder,
            name,
            file.path(),
            self.deleted_at,
            |status: &EntryStatus| file.seen().is_moved(status),
        );
        result_of(file, moved)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::{Duration, SystemTime};

    use okapi_platform::{EntryType, FileIdentity};

    use super::*;
    use crate::listing::{Found, Root, SearchStats, Seen};

    const SIZE: u64 = 10;
    const ON_DISK: u64 = 4096;

    /// A report of one file, `/root0/a`, on `device`, of `links` names;
    /// `/root0` does not exist: a file read is `Gone`.
    fn report(device: u64, links: u64) -> LargeFileReport {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let status = EntryStatus {
            kind: EntryType::File,
            identity: FileIdentity::new(device, 2),
            last_change: modified,
            modified,
            changed: modified,
            links,
            owner: 1000,
            sticky: false,
            locked: false,
            size: SIZE,
            allocated: Some(ON_DISK),
            mount_root: Some(false),
        };
        LargeFileReport::new(
            vec![Found {
                path: "/root0/a".into(),
                root: 0,
                status: Seen::of(&status),
            }],
            vec![Root {
                path: "/root0".into(),
                identity: FileIdentity::new(device, 1),
                may_alias: false,
                undated: false,
            }],
            SearchStats::default(),
        )
    }

    /// What moving the file of `report` makes of it, `excluded` or not,
    /// with a trash on the device of the temporary folder.
    fn moved(report: &LargeFileReport, excluded: bool) -> TrashResult {
        let temporary = tempfile::tempdir().unwrap();
        let bin = Trash::in_folder(temporary.path()).unwrap();
        let exclusions = if excluded {
            Exclusions::new([std::path::PathBuf::from("/root0/a")]).unwrap()
        } else {
            Exclusions::none()
        };
        let mover = Mover {
            report,
            bin: &bin,
            exclusions: &exclusions,
            deleted_at: "",
            between: None,
        };
        mover.run(&report.files()[0])
    }

    /// The device of the temporary folders, where the test's trash lies.
    fn trash_device() -> u64 {
        let temporary = tempfile::tempdir().unwrap();
        Trash::in_folder(temporary.path()).unwrap().device()
    }

    #[test]
    fn each_check_before_reading_alone_keeps_the_file_in_its_order() {
        let here = trash_device();
        // Read: the root does not exist.
        assert_eq!(
            moved(&report(here, 1), false),
            TrashResult::Kept(KeepReason::Gone)
        );
        // On another device: refused before any reading.
        assert_eq!(
            moved(&report(here + 1, 1), false),
            TrashResult::Kept(KeepReason::OtherDevice)
        );
        // Excluded: before the device.
        assert_eq!(
            moved(&report(here + 1, 1), true),
            TrashResult::Kept(KeepReason::Excluded)
        );
        // Of other names: before the exclusion.
        assert_eq!(
            moved(&report(here + 1, 2), true),
            TrashResult::Kept(KeepReason::OtherNames)
        );
    }

    #[test]
    fn the_trash_s_answer_says_what_became_of_the_file_counted_on_disk() {
        let report = report(1, 1);
        let file = &report.files()[0];
        assert_eq!(
            result_of(file, Ok("a".into())),
            TrashResult::Trashed { bytes: ON_DISK }
        );
        assert_eq!(
            result_of(file, Err(TrashError::OtherDevice)),
            TrashResult::Kept(KeepReason::OtherDevice)
        );
        assert_eq!(
            result_of(file, Err(TrashError::Changed)),
            TrashResult::Kept(KeepReason::Changed)
        );
        assert_eq!(
            result_of(file, Err(TrashError::Unchecked("a.2".into()))),
            TrashResult::Unchecked
        );
        let denied = io::Error::from_raw_os_error(13);
        assert_eq!(
            result_of(file, Err(TrashError::Io(denied))),
            TrashResult::Kept(KeepReason::Refused(Refusal {
                kind: io::ErrorKind::PermissionDenied,
                code: Some(13),
            }))
        );
    }
}
