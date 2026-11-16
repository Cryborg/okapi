//! Copies moved to the trash, named by identifiers of a report only
//! (invariant 1), never all of a group: each copy is read again just before
//! it goes, and so is another copy of its group, not asked for, which must
//! still be there unchanged (invariant 7).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};

use okapi_engine::Exclusions;
use okapi_platform::{
    Directory, EntryStatus, Trash, TrashError, effective_user, tells_mount_roots,
};

use super::report::{CopyId, DuplicateReport, FileCopy, Group};
use crate::listing::{Unlocated, located, trash_folder};
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
    /// It is no longer the file the search read: written, replaced, given
    /// to another, given another name, or a folder on its way changed.
    Changed,
    /// It is no longer at its path: removed, or it or a folder on its way
    /// moved elsewhere.
    Gone,
    /// No other copy of its group, not asked for, could be read again
    /// unchanged: changed, gone, or refused by the system.
    KeeperChanged,
    /// No other copy left keeps it, and one at least was passed over unread
    /// as unsure: it may be this very file shown again by another filesystem
    /// (FUSE, network, stacked), lies on FAT (where a write dated back goes
    /// unseen) or a filesystem not known, in a trash, or is another
    /// account's.
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
/// excludes (`exclusions`, read afresh, by path or by identity) nor the last of a group.
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
    let user = effective_user();
    if user.is_none() {
        tracing::warn!("the user is unknown: no copy surely keeps another");
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
        user,
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

/// Whether `path` lies in a trash, a folder of its: a file there is on its
/// way out, even under a folder searched on purpose.
fn in_a_trash(path: &std::path::Path) -> bool {
    path.parent().is_some_and(|folder| {
        folder
            .components()
            .any(|component| trash_folder(component.as_os_str()))
    })
}

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
    /// Who moves: a copy of another owner keeps nothing, and none does
    /// when the user is unknown.
    user: Option<u32>,
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
        // Through a bind mount of a folder excluded since (the folder
        // itself shown elsewhere): read only once the copy is known to lie
        // on the trash's disk.
        if self.exclusions.excludes_through_identity(copy.path()) {
            tracing::debug!(
                path = %copy.path().display(),
                "excluded by identity: a folder on its way is an excluded one"
            );
            return kept(KeepReason::Excluded);
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
            |status: &EntryStatus| copy.seen().is_moved(status),
        );
        result_of(copy, moved)
    }

    /// Whether `other`, a copy not asked for, keeps the content of `copy`
    /// when it goes: a sure keeper ([`Self::unsure`]), still there unchanged.
    /// Each one passed over is traced.
    fn keeps(&self, copy: &FileCopy, other: &FileCopy) -> Result<(), KeepReason> {
        let unsure = self.unsure(copy, other);
        let passed_over = if unsure.is_some() {
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
            unsure,
            owner = other.seen().owner,
            user = self.user,
            refusal = ?match passed_over {
                KeepReason::Refused(refusal) => Some(refusal),
                _ => None,
            },
            "keeper passed over"
        );
        Err(passed_over)
    }

    /// Why `other` cannot surely keep the content of `copy`, judged on what
    /// the search read, `None` when it can: its root is not of the report
    /// (`root`); it may be this very file shown again by another filesystem
    /// (`alias`: on the copy's own device, it would have its identity); a
    /// write dated back goes unseen there (`undated`); it lies in a trash,
    /// on its way out (`trash`); or it is another account's, who may change
    /// or remove it once the copy is gone (`owner`). Reading the keeper
    /// again unchanged tells its owner is still the one read.
    fn unsure(&self, copy: &FileCopy, other: &FileCopy) -> Option<&'static str> {
        let Some(root) = self.report.root(other.root()) else {
            return Some("root");
        };
        let aliased = root.may_alias && other.identity().device() != copy.identity().device();
        if aliased {
            Some("alias")
        } else if root.undated {
            Some("undated")
        } else if in_a_trash(other.path()) {
            Some("trash")
        } else if self.user != Some(other.seen().owner) {
            Some("owner")
        } else {
            None
        }
    }

    /// The folder holding `copy`, reached from its root without a link, and
    /// its name there, if it is still the file the search read.
    fn located<'c>(&self, copy: &'c FileCopy) -> Result<(Directory, &'c OsStr), KeepReason> {
        let root = self.report.root(copy.root()).ok_or(KeepReason::Changed)?;
        located(copy.path(), root, copy.seen()).map_err(|unlocated| match unlocated {
            Unlocated::Changed => KeepReason::Changed,
            Unlocated::Gone => KeepReason::Gone,
            Unlocated::Refused(error) => KeepReason::Refused((&error).into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::{Duration, SystemTime};

    use okapi_platform::{EntryType, FileIdentity, Trash};

    use super::*;
    use crate::listing::{Found, Root, SearchStats, Seen};

    const SIZE: u64 = 10;
    /// Who moves the copies, and owns them unless told otherwise.
    const USER: u32 = 1000;
    /// Another account.
    const OTHER_USER: u32 = 1001;

    fn status(inode: u64, size: u64, modified: SystemTime) -> EntryStatus {
        EntryStatus {
            kind: EntryType::File,
            identity: FileIdentity::new(1, inode),
            last_change: modified,
            modified,
            changed: modified,
            links: 1,
            owner: USER,
            sticky: false,
            locked: false,
            size,
            allocated: None,
            mount_root: Some(false),
        }
    }

    /// What `b`, the other copy of the group, is.
    #[derive(Clone, Copy)]
    struct Other {
        device: u64,
        /// Its root's filesystem may show a file again.
        may_alias: bool,
        /// Its root's filesystem keeps no change date of its own.
        undated: bool,
        /// It lies in a trash folder.
        trashed: bool,
        owner: u32,
    }

    const SURE: Other = Other {
        device: 1,
        may_alias: false,
        undated: false,
        trashed: false,
        owner: USER,
    };

    /// A report of one group, `a` and `b`; its dates.
    fn report_with(b: Other) -> (DuplicateReport, SystemTime) {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let found = |root: usize, name: &str, device, inode, owner| {
            let mut status = status(inode, SIZE, modified);
            status.identity = FileIdentity::new(device, inode);
            status.owner = owner;
            Found {
                path: format!("/root{root}/{name}").into(),
                root,
                status: Seen::of(&status),
            }
        };
        let root = |index: u64, may_alias, undated| Root {
            path: format!("/root{index}").into(),
            identity: FileIdentity::new(1, index),
            may_alias,
            undated,
        };
        let report = DuplicateReport::new(
            vec![vec![
                found(0, "a", 1, 2, USER),
                found(
                    1,
                    if b.trashed { "Trash/b" } else { "b" },
                    b.device,
                    3,
                    b.owner,
                ),
            ]],
            vec![root(0, false, false), root(1, b.may_alias, b.undated)],
            SearchStats::default(),
        );
        (report, modified)
    }

    fn report() -> (DuplicateReport, SystemTime) {
        report_with(SURE)
    }

    /// What `ask` makes of `a` and `b` for `user`.
    fn judged<T>(
        b: Other,
        user: Option<u32>,
        ask: impl Fn(&Mover<'_>, &FileCopy, &FileCopy) -> T,
    ) -> T {
        let temporary = tempfile::tempdir().unwrap();
        let bin = Trash::in_folder(temporary.path()).unwrap();
        let (report, _) = report_with(b);
        let mover = Mover {
            report: &report,
            asked: &BTreeSet::new(),
            bin: &bin,
            exclusions: &Exclusions::none(),
            deleted_at: "",
            user,
        };
        let [a, b] = report.groups()[0].copies() else {
            panic!("two copies");
        };
        ask(&mover, a, b)
    }

    /// Whether `b` keeps `a` for `user`. `/root1` does not exist: a keeper
    /// read is `Gone`, one refused before any reading `NoSureKeeper`.
    #[expect(
        clippy::redundant_closure_for_method_calls,
        reason = "`Mover::keeps` is of one lifetime, the closure of any"
    )]
    fn keeps(b: Other, user: Option<u32>) -> Result<(), KeepReason> {
        judged(b, user, |mover, a, b| mover.keeps(a, b))
    }

    #[test]
    #[expect(
        clippy::redundant_closure_for_method_calls,
        reason = "`Mover::unsure` is of one lifetime, the closure of any"
    )]
    fn the_first_cause_of_an_unsure_keeper_is_the_one_told() {
        let unsure = |b| judged(b, Some(USER), |mover, a, b| mover.unsure(a, b));
        assert_eq!(unsure(SURE), None);
        let every = Other {
            device: 2,
            may_alias: true,
            undated: true,
            trashed: true,
            owner: OTHER_USER,
        };
        assert_eq!(unsure(every), Some("alias"));
        let no_alias = Other {
            may_alias: false,
            ..every
        };
        assert_eq!(unsure(no_alias), Some("undated"));
        let dated = Other {
            undated: false,
            ..no_alias
        };
        assert_eq!(unsure(dated), Some("trash"));
        let in_place = Other {
            trashed: false,
            ..dated
        };
        assert_eq!(unsure(in_place), Some("owner"));
    }

    #[test]
    fn a_copy_that_may_be_the_file_itself_is_no_keeper() {
        let user = Some(USER);
        // On another device, through a filesystem that may show files again:
        // refused without reading anything.
        let elsewhere = Other {
            device: 2,
            may_alias: true,
            ..SURE
        };
        assert_eq!(keeps(elsewhere, user), Err(KeepReason::NoSureKeeper));
        // Otherwise it is read.
        let sure_elsewhere = Other {
            may_alias: false,
            ..elsewhere
        };
        assert_eq!(keeps(sure_elsewhere, user), Err(KeepReason::Gone));
        let aliasing_here = Other {
            may_alias: true,
            ..SURE
        };
        assert_eq!(keeps(aliasing_here, user), Err(KeepReason::Gone));
    }

    #[test]
    fn a_copy_on_fat_keeps_nothing_on_any_device() {
        let user = Some(USER);
        let fat_here = Other {
            undated: true,
            ..SURE
        };
        assert_eq!(keeps(fat_here, user), Err(KeepReason::NoSureKeeper));
        let fat_elsewhere = Other {
            device: 2,
            ..fat_here
        };
        assert_eq!(keeps(fat_elsewhere, user), Err(KeepReason::NoSureKeeper));
    }

    #[test]
    fn a_copy_of_another_owner_keeps_nothing() {
        let others = Other {
            owner: OTHER_USER,
            ..SURE
        };
        assert_eq!(keeps(others, Some(USER)), Err(KeepReason::NoSureKeeper));
        // The same copy, for its owner, is read.
        assert_eq!(keeps(others, Some(OTHER_USER)), Err(KeepReason::Gone));
        assert_eq!(keeps(SURE, Some(USER)), Err(KeepReason::Gone));
        assert_eq!(keeps(SURE, None), Err(KeepReason::NoSureKeeper));
    }

    #[test]
    fn a_copy_in_a_trash_is_on_its_way_out() {
        for path in [
            "/home/f/.local/share/Trash/files/a",
            "/media/key/.Trash-1000/files/a",
            "/media/key/.Trash/1000/files/a",
            "/media/win/$RECYCLE.BIN/S-1-5-21/$R1.jpg",
            "/media/c/$Recycle.Bin/a",
        ] {
            assert!(in_a_trash(std::path::Path::new(path)), "{path}");
        }
        for path in [
            "/home/f/Documents/a",
            "/home/f/Trashed/a",
            "/home/f/Trash.txt",
            "/home/f/RECYCLE.BIN/a",
            "/home/f/$RECYCLE.BIN.txt",
            "/home/f/$RECYCLE.BIN.old/a",
            "/home/f/Documents/Recycler/a",
            // A file named as a trash is no trash folder.
            "/home/f/Documents/Trash",
            "/home/f/$RECYCLE.BIN",
        ] {
            assert!(!in_a_trash(std::path::Path::new(path)), "{path}");
        }
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
