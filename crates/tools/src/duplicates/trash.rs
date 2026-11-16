//! Copies moved to the trash, named by identifiers of a report only
//! (invariant 1), never all of a group: each copy is read again just before
//! it goes, and so is another copy of its group, not asked for, which must
//! still be there unchanged (invariant 7).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io;

use okapi_engine::Exclusions;
use okapi_platform::{Directory, EntryStatus, Trash, TrashError, Unreached, tells_mount_roots};

use super::compare::parent_of;
use super::report::{CopyId, DuplicateReport, FileCopy, Group};
pub use crate::trashing::Refusal;
use crate::trashing::{Moved, deleted_now, moved, one_at_a_time};

/// Why a copy stayed where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// The identifier is not of this report (invariant 1).
    UnknownCopy,
    /// Every copy of its group was asked for: one always stays.
    LastCopy,
    /// It has other names: moving it would free nothing.
    OtherNames,
    /// The user excluded it since the search.
    Excluded,
    /// It is no longer the file the search read: written, replaced, or a
    /// folder on its way changed.
    Changed,
    Gone,
    /// No other copy of its group, not asked for, could be read again
    /// unchanged: changed, gone, or refused by the system.
    KeeperChanged,
    /// The only other copies left may be this very file, shown again by
    /// another filesystem (FUSE, network, stacked), or lie in a trash: none
    /// surely keeps it.
    NoSureKeeper,
    /// It is on another filesystem than the trash.
    OtherDevice,
    /// This system cannot tell a mount point from a folder: nothing moves.
    UnsupportedSystem,
    /// The system refused, with its error.
    Refused(Refusal),
}

impl KeepReason {
    /// A stable key, for traces.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::UnknownCopy => "unknownCopy",
            Self::LastCopy => "lastCopy",
            Self::OtherNames => "otherNames",
            Self::Excluded => "excluded",
            Self::Changed => "changed",
            Self::Gone => "gone",
            Self::KeeperChanged => "keeperChanged",
            Self::NoSureKeeper => "noSureKeeper",
            Self::OtherDevice => "otherDevice",
            Self::UnsupportedSystem => "unsupportedSystem",
            Self::Refused(_) => "refused",
        }
    }
}

/// What the system refused with `error`, a file gone being no refusal.
fn refused_or_gone(error: &io::Error) -> KeepReason {
    if error.kind() == io::ErrorKind::NotFound {
        KeepReason::Gone
    } else {
        KeepReason::Refused(error.into())
    }
}

/// What became of a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashResult {
    /// In the trash; its size is freed once the trash is emptied.
    Trashed {
        bytes: u64,
    },
    Kept(KeepReason),
    /// A file went to the trash in its name that is not known to be the
    /// copy as checked (another program replaced or wrote it meanwhile, or
    /// it could not be read again): it stays there, restorable, and nothing
    /// is counted freed.
    Unchecked,
}

/// What became of one identifier asked for: its copy is found in the
/// report by [`DuplicateReport::find`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrashOutcome {
    pub id: CopyId,
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

/// Moves the copies `ids` of `report` to `bin`, but never one the user
/// excludes (`exclusions`, read afresh, by path) nor the last of a group.
/// Two movings, of one report or of two, never run at once: this one may
/// wait for another, and reads the disks (a mount that stopped answering
/// holds it), so it is never called on the interface's thread.
#[must_use]
pub fn trash(
    report: &DuplicateReport,
    ids: &[CopyId],
    bin: &Trash,
    exclusions: &Exclusions,
) -> TrashReceipt {
    trash_dated(report, ids, bin, exclusions, &deleted_now())
}

pub(crate) fn trash_dated(
    report: &DuplicateReport,
    ids: &[CopyId],
    bin: &Trash,
    exclusions: &Exclusions,
    deleted_at: &str,
) -> TrashReceipt {
    let _span = tracing::info_span!("trash", requested = ids.len()).entered();
    let _one_at_a_time = one_at_a_time();
    let mut seen = BTreeSet::new();
    let ids: Vec<CopyId> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
    let asked: BTreeSet<CopyId> = ids.iter().copied().collect();
    let supported = tells_mount_roots();
    if !supported {
        tracing::warn!("this system cannot tell a mount point: nothing is moved");
    }
    let mut asked_in_group: BTreeMap<usize, usize> = BTreeMap::new();
    for id in &ids {
        if let Some(group) = report.group_of(*id) {
            *asked_in_group.entry(group).or_default() += 1;
        }
    }
    let mover = Mover {
        report,
        asked: &asked,
        bin,
        exclusions,
        deleted_at,
    };
    let outcomes: Vec<TrashOutcome> = ids
        .into_iter()
        .map(|id| {
            let outcome = match report.find(id) {
                None => TrashOutcome {
                    id,
                    result: TrashResult::Kept(KeepReason::UnknownCopy),
                },
                Some((group, copy)) => {
                    let whole = report
                        .group_of(id)
                        .and_then(|index| asked_in_group.get(&index))
                        .is_some_and(|&count| count >= group.copies().len());
                    let result = if !supported {
                        TrashResult::Kept(KeepReason::UnsupportedSystem)
                    } else if whole {
                        TrashResult::Kept(KeepReason::LastCopy)
                    } else {
                        mover.run(group, copy)
                    };
                    TrashOutcome { id, result }
                }
            };
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

fn trace(report: &DuplicateReport, outcome: &TrashOutcome) {
    let path = report
        .find(outcome.id)
        .map_or_else(String::new, |(_, copy)| copy.path().display().to_string());
    match outcome.result {
        TrashResult::Trashed { bytes } => tracing::info!(path, bytes, "copy moved to the trash"),
        TrashResult::Unchecked => tracing::warn!(
            path,
            "a file not known to be the copy as checked went to the trash"
        ),
        TrashResult::Kept(KeepReason::UnknownCopy) => tracing::warn!(
            id = ?outcome.id,
            "copy of another report asked for: kept (invariant 1)"
        ),
        TrashResult::Kept(reason @ KeepReason::Refused(refusal)) => tracing::warn!(
            path,
            reason = reason.key(),
            kind = ?refusal.kind,
            code = refusal.code,
            "copy kept: the system refused"
        ),
        TrashResult::Kept(reason) => tracing::info!(path, reason = reason.key(), "copy kept"),
    }
}

/// Whether `status`, read in the trash once moved, is `copy`: moving
/// changes the inode's change date, so it is known by its identity, its
/// size and its content's date.
fn is_moved_copy(copy: &FileCopy, status: &EntryStatus) -> bool {
    status.identity == copy.identity()
        && status.size == copy.size()
        && status.modified == copy.modified()
}

/// Whether `path` lies in a trash (`Trash`, `.Trash`, `.Trash-<uid>`): a
/// file there is on its way out, even under a folder searched on purpose.
fn in_a_trash(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name == TRASH_FOLDER || name.starts_with(HIDDEN_TRASH))
    })
}

/// The trash folder of the freedesktop specification, in the data folder.
pub(crate) const TRASH_FOLDER: &str = "Trash";
/// What the trash of another disk is named, at its top (`.Trash`,
/// `.Trash-1000`).
const HIDDEN_TRASH: &str = ".Trash";

/// What the trash's answer to moving `copy` makes of it.
fn result_of(copy: &FileCopy, answer: Result<OsString, TrashError>) -> TrashResult {
    let kept = TrashResult::Kept;
    match moved(answer) {
        Moved::Trashed => TrashResult::Trashed { bytes: copy.size() },
        Moved::OtherDevice => kept(KeepReason::OtherDevice),
        Moved::Changed => kept(KeepReason::Changed),
        Moved::Unchecked(_) => TrashResult::Unchecked,
        Moved::Refused(refusal) => kept(KeepReason::Refused(refusal)),
    }
}

/// What every copy's moving shares.
struct Mover<'a> {
    report: &'a DuplicateReport,
    asked: &'a BTreeSet<CopyId>,
    bin: &'a Trash,
    exclusions: &'a Exclusions,
    deleted_at: &'a str,
}

impl Mover<'_> {
    /// Moves `copy` of `group` to the trash, if it is still the file read
    /// and another copy, not asked for, still is too.
    fn run(&self, group: &Group, copy: &FileCopy) -> TrashResult {
        let kept = TrashResult::Kept;
        if !copy.frees() {
            return kept(KeepReason::OtherNames);
        }
        if self.exclusions.excludes(copy.path()) {
            return kept(KeepReason::Excluded);
        }
        // Nothing is read of a copy the trash cannot take.
        if copy.identity().device() != self.bin.device() {
            return kept(KeepReason::OtherDevice);
        }
        let mut no_sure_keeper = false;
        let kept_by_another = group
            .copies()
            .iter()
            .filter(|other| !self.asked.contains(&other.id()))
            .any(|other| match self.keeps(copy, other) {
                Ok(()) => true,
                Err(reason) => {
                    no_sure_keeper |= reason == KeepReason::NoSureKeeper;
                    false
                }
            });
        if !kept_by_another {
            return kept(if no_sure_keeper {
                KeepReason::NoSureKeeper
            } else {
                KeepReason::KeeperChanged
            });
        }
        // The copy is read last, and moved right after: the window for a
        // write between its check and its move is as small as can be.
        let (folder, name) = match self.located(copy) {
            Ok(located) => located,
            Err(reason) => return kept(reason),
        };
        let moved = self.bin.put(
            &folder,
            name,
            copy.path(),
            self.deleted_at,
            |status: &EntryStatus| is_moved_copy(copy, status),
        );
        result_of(copy, moved)
    }

    /// Whether `other`, a copy not asked for, keeps the content of `copy`
    /// when it goes: still there unchanged, and surely another file (a
    /// filesystem that may show a file under another identity counts only on
    /// the copy's own device). Each one passed over is traced.
    fn keeps(&self, copy: &FileCopy, other: &FileCopy) -> Result<(), KeepReason> {
        let aliased = self
            .report
            .root(other.root())
            .is_none_or(|root| root.may_alias)
            && other.identity().device() != copy.identity().device();
        let passed_over = if aliased || in_a_trash(other.path()) {
            KeepReason::NoSureKeeper
        } else {
            match self.located(other) {
                Ok(_) => return Ok(()),
                Err(reason) => reason,
            }
        };
        tracing::debug!(
            copy = %copy.path().display(),
            keeper = %other.path().display(),
            reason = passed_over.key(),
            refusal = ?match passed_over {
                KeepReason::Refused(refusal) => Some(refusal),
                _ => None,
            },
            "keeper passed over"
        );
        Err(passed_over)
    }

    /// The folder holding `copy`, reached from its root without a link, and
    /// its name there, if it is still the file the search read.
    fn located<'c>(&self, copy: &'c FileCopy) -> Result<(Directory, &'c OsStr), KeepReason> {
        let root = self.report.root(copy.root()).ok_or(KeepReason::Changed)?;
        let (folder, name) = parent_of(copy.path(), root).map_err(|unreached| match unreached {
            Unreached::Changed => KeepReason::Changed,
            Unreached::Io(error) => KeepReason::Refused((&error).into()),
        })?;
        match folder.status_of(name) {
            Ok(status) if copy.seen().unchanged(&status) => Ok((folder, name)),
            Ok(_) => Err(KeepReason::Changed),
            Err(error) => Err(refused_or_gone(&error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use okapi_platform::{EntryType, FileIdentity, Trash};

    use super::*;
    use crate::duplicates::report::{Found, Root, SearchStats, Seen};

    const SIZE: u64 = 10;

    fn status(inode: u64, size: u64, modified: SystemTime) -> EntryStatus {
        EntryStatus {
            kind: EntryType::File,
            identity: FileIdentity::new(1, inode),
            last_change: modified,
            modified,
            changed: modified,
            links: 1,
            owner: 0,
            sticky: false,
            locked: false,
            size,
            allocated: None,
            mount_root: Some(false),
        }
    }

    /// A report of one group, `a` and `b` (`b` on device `b_device`, below
    /// a root that may show files again or not); its dates.
    fn report_with(b_device: u64, may_alias: bool) -> (DuplicateReport, SystemTime) {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let found = |root: usize, name: &str, device, inode| {
            let mut status = status(inode, SIZE, modified);
            status.identity = FileIdentity::new(device, inode);
            Found {
                path: format!("/root{root}/{name}").into(),
                root,
                status: Seen::of(&status),
            }
        };
        let root = |index: u64, may_alias| Root {
            path: format!("/root{index}").into(),
            identity: FileIdentity::new(1, index),
            may_alias,
        };
        let report = DuplicateReport::new(
            vec![vec![found(0, "a", 1, 2), found(1, "b", b_device, 3)]],
            vec![root(0, false), root(1, may_alias)],
            SearchStats::default(),
        );
        (report, modified)
    }

    fn report() -> (DuplicateReport, SystemTime) {
        report_with(1, false)
    }

    #[test]
    fn a_copy_that_may_be_the_file_itself_is_no_keeper() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = Trash::in_folder(temporary.path()).unwrap();
        let none = Exclusions::none();
        let asked = BTreeSet::new();
        let keeps = |b_device, may_alias| {
            let (report, _) = report_with(b_device, may_alias);
            let mover = Mover {
                report: &report,
                asked: &asked,
                bin: &bin,
                exclusions: &none,
                deleted_at: "",
            };
            let [a, b] = report.groups()[0].copies() else {
                panic!("two copies");
            };
            mover.keeps(a, b)
        };
        // On another device, through a filesystem that may show files again:
        // refused without reading anything (`/root1` does not exist).
        assert_eq!(keeps(2, true), Err(KeepReason::NoSureKeeper));
        // Otherwise it is read, and here not found.
        assert_eq!(keeps(2, false), Err(KeepReason::Changed));
        assert_eq!(keeps(1, true), Err(KeepReason::Changed));
    }

    #[test]
    fn a_copy_in_a_trash_is_on_its_way_out() {
        for path in [
            "/home/f/.local/share/Trash/files/a",
            "/media/key/.Trash-1000/files/a",
            "/media/key/.Trash/1000/files/a",
        ] {
            assert!(in_a_trash(std::path::Path::new(path)), "{path}");
        }
        for path in [
            "/home/f/Documents/a",
            "/home/f/Trashed/a",
            "/home/f/Trash.txt",
        ] {
            assert!(!in_a_trash(std::path::Path::new(path)), "{path}");
        }
    }

    #[test]
    fn what_was_moved_is_the_copy_only_by_identity_size_and_date() {
        let (report, modified) = report();
        let copy = &report.groups()[0].copies()[0];
        assert!(is_moved_copy(copy, &status(2, SIZE, modified)));
        assert!(
            !is_moved_copy(copy, &status(9, SIZE, modified)),
            "another file"
        );
        assert!(
            !is_moved_copy(copy, &status(2, SIZE + 1, modified)),
            "cut or grown"
        );
        let later = modified + Duration::from_secs(1);
        assert!(!is_moved_copy(copy, &status(2, SIZE, later)), "written");
    }

    #[test]
    fn the_trash_s_answer_says_what_became_of_the_copy() {
        let (report, _) = report();
        let copy = &report.groups()[0].copies()[0];
        assert_eq!(
            result_of(copy, Ok("a".into())),
            TrashResult::Trashed { bytes: SIZE }
        );
        assert_eq!(
            result_of(copy, Err(TrashError::OtherDevice)),
            TrashResult::Kept(KeepReason::OtherDevice)
        );
        assert_eq!(
            result_of(copy, Err(TrashError::Changed)),
            TrashResult::Kept(KeepReason::Changed)
        );
        assert_eq!(
            result_of(copy, Err(TrashError::Unchecked("a.2".into()))),
            TrashResult::Unchecked
        );
        let denied = io::Error::from_raw_os_error(13);
        assert_eq!(
            result_of(copy, Err(TrashError::Io(denied))),
            TrashResult::Kept(KeepReason::Refused(Refusal {
                kind: io::ErrorKind::PermissionDenied,
                code: Some(13),
            }))
        );
        let gone = io::Error::from(io::ErrorKind::NotFound);
        assert!(matches!(
            result_of(copy, Err(TrashError::Io(gone))),
            TrashResult::Kept(KeepReason::Refused(_))
        ));
    }
}
