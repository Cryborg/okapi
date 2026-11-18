//! What applications uninstalled left in the home folder, found by their
//! identifier: `~/snap/<name>` that no snap installed bears, `~/.var/app/<id>`
//! that no flatpak bears (in any installation, renamed from, or running). A
//! source, or a folder of leftovers, not read whole proposes nothing. Each
//! folder is known by an identifier of the report that found it (invariant
//! 1), and goes to the trash only when the sources, read again, still bear
//! nothing of its name, nothing in it is excluded, and the folder, reached
//! from `/` without a link, is the same to its deepest entry (invariant 7).

pub mod installed;
mod walk;

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use okapi_engine::{Exclusions, ProtectedRoots, report_nonce};
use okapi_platform::{
    Directory, EntryStatus, EntryType, FileIdentity, Trash, Unreached, reach, tells_mount_roots,
};

pub use crate::trashing::Refusal;
pub use installed::{Installed, InstalledPlaces, Known};
pub use walk::MOST_ENTRIES;
use walk::{Print, Unprinted, Walk, entered};

use super::names_a_folder;
use crate::packages::snapd::is_snap_name;
use crate::trashing::{Moved, deleted_now, moved, one_at_a_time};

use super::flatpak::FLATPAK_DATA;
use super::snap::SNAP_DATA;

/// Whose a leftover is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LeftoverKind {
    Snap,
    Flatpak,
}

impl LeftoverKind {
    const ALL: [Self; 2] = [Self::Snap, Self::Flatpak];

    /// Its folder under the home folder.
    const fn folder(self) -> &'static str {
        match self {
            Self::Snap => SNAP_DATA,
            Self::Flatpak => FLATPAK_DATA,
        }
    }

    /// Whether `name` may be one's identifier.
    fn names(self, name: &str) -> bool {
        match self {
            Self::Snap => is_snap_name(name),
            Self::Flatpak => names_a_folder(name),
        }
    }

    /// What is installed of this kind.
    const fn known(self, installed: &Installed) -> &Known {
        match self {
            Self::Snap => &installed.snaps,
            Self::Flatpak => &installed.flatpaks,
        }
    }
}

/// The identifier of a leftover, valid only in the report that gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeftoverId {
    report: u64,
    index: usize,
}

/// A folder an application uninstalled left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    id: LeftoverId,
    kind: LeftoverKind,
    name: String,
    path: PathBuf,
    /// The folder holding it, by its rank in the report's.
    parent: usize,
    identity: FileIdentity,
    /// What it held; `None` when too large to be checked.
    print: Option<Print>,
}

impl Leftover {
    #[must_use]
    pub const fn id(&self) -> LeftoverId {
        self.id
    }

    #[must_use]
    pub const fn kind(&self) -> LeftoverKind {
        self.kind
    }

    /// The snap's name, or the flatpak's id.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What it takes on disk; `None` when too large to be checked.
    #[must_use]
    pub fn bytes(&self) -> Option<u64> {
        self.print.map(|print| print.bytes)
    }

    /// Whether it can be checked, hence moved: not too large.
    #[must_use]
    pub const fn checkable(&self) -> bool {
        self.print.is_some()
    }
}

/// A folder holding leftovers, reached without a link.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parent {
    /// Canonical.
    path: PathBuf,
    identity: FileIdentity,
}

/// What an analysis found. It cannot be built outside this crate: moving a
/// leftover takes the report and an identifier of it (invariant 1).
#[derive(Debug)]
pub struct LeftoverReport {
    id: u64,
    leftovers: Vec<Leftover>,
    parents: Vec<Parent>,
    unread: Vec<LeftoverKind>,
    /// Folders no application bears that are not proposed for safety.
    withheld: usize,
    /// Who must own them, and the most entries one may hold.
    user: u32,
    most_entries: u64,
}

impl LeftoverReport {
    #[must_use]
    pub fn leftovers(&self) -> &[Leftover] {
        &self.leftovers
    }

    /// The kinds whose source, or folder of leftovers, could not be read
    /// whole: none of theirs is proposed.
    #[must_use]
    pub fn unread(&self) -> &[LeftoverKind] {
        &self.unread
    }

    /// How many folders no application bears are not proposed for safety:
    /// a mount, another user's entry, one that could not be read or that
    /// changed while read.
    #[must_use]
    pub const fn withheld(&self) -> usize {
        self.withheld
    }

    /// The leftover `id` names, if it is of this report.
    #[must_use]
    pub fn find(&self, id: LeftoverId) -> Option<&Leftover> {
        (id.report == self.id)
            .then(|| self.leftovers.get(id.index))
            .flatten()
    }
}

/// Where leftovers are looked for, and for whom.
#[derive(Debug, Clone)]
pub struct Places {
    pub home: PathBuf,
    /// The user whose folders they must be.
    pub user: u32,
    /// The most entries a leftover may hold to be checked
    /// ([`MOST_ENTRIES`]).
    pub most_entries: u64,
}

/// What the analysis reads by.
struct Rules<'a> {
    installed: &'a Installed,
    protected: &'a ProtectedRoots,
    exclusions: &'a Exclusions,
    places: &'a Places,
}

/// What the home folder holds that applications uninstalled left, as
/// `installed` says; never a folder `protected` holds (invariant 6) nor one
/// the user excludes.
#[must_use]
pub fn find(
    places: &Places,
    installed: &Installed,
    protected: &ProtectedRoots,
    exclusions: &Exclusions,
) -> LeftoverReport {
    let _span = tracing::info_span!("leftovers").entered();
    let mut report = LeftoverReport {
        id: report_nonce(),
        leftovers: Vec::new(),
        parents: Vec::new(),
        unread: Vec::new(),
        withheld: 0,
        user: places.user,
        most_entries: places.most_entries,
    };
    let rules = Rules {
        installed,
        protected,
        exclusions,
        places,
    };
    for kind in LeftoverKind::ALL {
        if let Err(why) = report.find_of(kind, &rules) {
            tracing::warn!(?kind, why, "none of its leftovers proposed");
            report.unread.push(kind);
        }
    }
    tracing::info!(
        found = report.leftovers.len(),
        withheld = report.withheld,
        unread = ?report.unread,
        "leftovers found"
    );
    report
}

impl LeftoverReport {
    /// Adds the leftovers of `kind`.
    ///
    /// # Errors
    ///
    /// Why its source, or its folder of leftovers, could not be read.
    fn find_of(&mut self, kind: LeftoverKind, rules: &Rules<'_>) -> Result<(), String> {
        if let Known::Unread(why) = kind.known(rules.installed) {
            return Err(format!("source unread: {why}"));
        }
        let path = rules.places.home.join(kind.folder());
        let unread = |error: io::Error| format!("{} unread: {error}", path.display());
        let Some((parent, directory)) = opened_parent(&path).map_err(unread)? else {
            return Ok(());
        };
        let names = directory.names().map_err(unread)?;
        let rank = self.parents.len();
        let device = parent.identity.device();
        self.parents.push(parent);
        let mut sorted: Vec<String> = names
            .iter()
            .filter_map(|name| name.to_str())
            .filter(|name| kind.names(name))
            .map(str::to_owned)
            .collect();
        sorted.sort_unstable();
        for name in sorted {
            let path = self.parents[rank].path.join(&name);
            let candidate = Candidate {
                kind,
                name: &name,
                path: &path,
                device,
            };
            match candidate.examined(&directory, rules) {
                Verdict::NotOne => {}
                Verdict::Withheld => self.withheld += 1,
                Verdict::Leftover(identity, print) => {
                    tracing::info!(
                        path = %path.display(),
                        checkable = print.is_some(),
                        bytes = print.map(|print| print.bytes),
                        "leftover proposed"
                    );
                    let index = self.leftovers.len();
                    self.leftovers.push(Leftover {
                        id: LeftoverId {
                            report: self.id,
                            index,
                        },
                        kind,
                        name,
                        path,
                        parent: rank,
                        identity,
                        print,
                    });
                }
            }
        }
        Ok(())
    }
}

/// The folder at `path`, reached from `/` without a link; `None` when there
/// is none.
///
/// # Errors
///
/// It is not a plain folder (a link: never followed), it cannot be read,
/// or it changed while reached: what it holds is unknown.
fn opened_parent(path: &Path) -> io::Result<Option<(Parent, Directory)>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Err(io::Error::other("not a folder (a link?)"));
    }
    // Its own identity, read without following: the path canonicalized must
    // lead to it, never to a link swapped in meanwhile.
    let identity = okapi_platform::file_identity(&metadata)
        .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
    let canonical = std::fs::canonicalize(path)?;
    let directory = reach(&canonical, identity, &[]).map_err(|unreached| match unreached {
        Unreached::Io(error) => error,
        Unreached::Changed => io::Error::other("changed while reached"),
    })?;
    Ok(Some((
        Parent {
            path: canonical,
            identity,
        },
        directory,
    )))
}

/// Whether `exclusions` touch the folder `path`: it, what holds it (by path
/// or by identity, through a bind mount), or a path below it. What lies
/// inside is read by the walk.
fn excluded(exclusions: &Exclusions, path: &Path) -> bool {
    exclusions.excludes(path)
        || exclusions.excludes_below(path)
        || exclusions.excludes_through_identity(path)
}

/// What a folder no application may bear turned out to be.
enum Verdict {
    /// A leftover: its identity, and its print (`None`: too large).
    Leftover(FileIdentity, Option<Print>),
    /// None: no folder, installed, a variable's, excluded.
    NotOne,
    /// Maybe one, not proposed for safety: a mount, another user's entry,
    /// or one that could not be read.
    Withheld,
}

/// A folder that may be a leftover.
struct Candidate<'a> {
    kind: LeftoverKind,
    name: &'a str,
    path: &'a Path,
    /// The device of the folder holding it.
    device: u64,
}

impl Candidate<'_> {
    /// A leftover if it is a folder of the user's, no link nor mount,
    /// nothing installed bearing its name, neither a variable's folder nor
    /// an ancestor of one, nothing in it excluded, no mount nor another's
    /// entry inside. Each one passed over is traced with why.
    fn examined(&self, parent: &Directory, rules: &Rules<'_>) -> Verdict {
        let not_one = |reason: &str| {
            tracing::debug!(path = %self.path.display(), reason, "not a leftover");
            Verdict::NotOne
        };
        let withheld = |reason: &dyn std::fmt::Debug| {
            tracing::info!(path = %self.path.display(), ?reason, "not proposed for safety");
            Verdict::Withheld
        };
        let status = match parent.status_of(OsStr::new(self.name)) {
            Ok(status) => status,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return not_one("gone"),
            Err(error) => return withheld(&error),
        };
        if status.kind != EntryType::Directory {
            return not_one("not a folder");
        }
        if self.kind.known(rules.installed).bears(self.name) != Some(false) {
            return not_one("installed");
        }
        if rules.protected.is_protected_directory(status.identity) {
            return not_one("a variable's folder or its ancestor");
        }
        if excluded(rules.exclusions, self.path) {
            return not_one("excluded, or holding what is");
        }
        if status.identity.device() != self.device || status.mount_root == Some(true) {
            return withheld(&"a mount");
        }
        if status.owner != rules.places.user {
            return withheld(&"another's");
        }
        let walk = Walk {
            owner: rules.places.user,
            device: self.device,
            most: rules.places.most_entries,
            exclusions: rules.exclusions,
        };
        match printed(parent, OsStr::new(self.name), &status, &walk) {
            Ok(print) => Verdict::Leftover(status.identity, Some(print)),
            Err(Unprinted::TooLarge) => Verdict::Leftover(status.identity, None),
            Err(Unprinted::Excluded) => not_one("holding what is excluded"),
            Err(unprinted) => withheld(&unprinted),
        }
    }
}

/// The print of the folder `name` of `parent`, read as `status`.
fn printed(
    parent: &Directory,
    name: &OsStr,
    status: &EntryStatus,
    walk: &Walk<'_>,
) -> Result<Print, Unprinted> {
    walk.print(entered(parent, name, status.identity)?)
}

/// Why a leftover stayed where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// The identifier is not of this report (invariant 1).
    UnknownLeftover,
    /// Too large to be checked.
    NotCheckable,
    /// Its application is installed again.
    Installed,
    /// Its source could not be read again: whether it is installed is
    /// unknown.
    SourceUnread,
    /// The user excluded it, or something in it, since the analysis.
    Excluded,
    /// A variable's folder, or one holding it, since the analysis
    /// (invariant 6).
    Protected,
    /// No longer the folder the analysis read: replaced, written, or a
    /// folder on its way changed.
    Changed,
    /// The leftover itself is gone.
    Gone,
    /// On another filesystem than the trash.
    OtherDevice,
    /// This system cannot tell a mount point from a folder: nothing moves.
    UnsupportedSystem,
    /// The system refused to read or move it.
    Refused(Refusal),
}

/// What became of a leftover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashResult {
    /// In the trash, restorable; its size is freed once the trash is emptied.
    Trashed {
        bytes: u64,
    },
    Kept(KeepReason),
    /// A folder went to the trash in its name that is not known to be the
    /// leftover as checked: it stays there, restorable, nothing counted.
    Unchecked,
}

/// What became of one identifier asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrashOutcome {
    pub id: LeftoverId,
    pub result: TrashResult,
}

/// One outcome per identifier asked for, in their order, each once.
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

/// What a move to the trash checks again, read afresh: the folders of the
/// variables (invariant 6) and the user's exclusions.
#[derive(Debug)]
pub struct TrashRules<'a> {
    pub protected: &'a ProtectedRoots,
    pub exclusions: &'a Exclusions,
}

/// Moves the leftovers `ids` of `report` to `bin`, each only if what is
/// installed now (`installed_now`, read before anything moves) still bears
/// nothing of its name, it is no variable's folder nor holds one, the user
/// does not exclude it (`rules`) and it is the same folder to its deepest
/// entry. One move at a time, with the duplicates' too: never called on the
/// interface's thread.
#[must_use]
pub fn trash(
    report: &LeftoverReport,
    ids: &[LeftoverId],
    bin: &Trash,
    installed_now: &dyn Fn() -> Installed,
    rules: &TrashRules<'_>,
) -> TrashReceipt {
    let _span = tracing::info_span!("leftovers_trash", requested = ids.len()).entered();
    // Read before the lock: snapd may take its time.
    let installed = installed_now();
    for kind in LeftoverKind::ALL {
        if let Known::Unread(why) = kind.known(&installed) {
            tracing::warn!(
                ?kind,
                why,
                "source unread again: none of its leftovers moved"
            );
        }
    }
    let _one_at_a_time = one_at_a_time();
    let supported = tells_mount_roots();
    let deleted_at = deleted_now();
    let mut seen = std::collections::BTreeSet::new();
    let outcomes: Vec<TrashOutcome> = ids
        .iter()
        .copied()
        .filter(|id| seen.insert(*id))
        .map(|id| {
            let result = match report.find(id) {
                None => TrashResult::Kept(KeepReason::UnknownLeftover),
                Some(_) if !supported => TrashResult::Kept(KeepReason::UnsupportedSystem),
                Some(leftover) => Mover {
                    report,
                    installed: &installed,
                    rules,
                    bin,
                    deleted_at: &deleted_at,
                }
                .run(leftover),
            };
            trace(report, id, result);
            TrashOutcome { id, result }
        })
        .collect();
    let receipt = TrashReceipt { outcomes };
    tracing::info!(
        bytes = receipt.trashed_bytes(),
        "leftovers moved to the trash"
    );
    receipt
}

fn trace(report: &LeftoverReport, id: LeftoverId, result: TrashResult) {
    let path = report
        .find(id)
        .map_or_else(String::new, |leftover| leftover.path.display().to_string());
    match result {
        TrashResult::Trashed { bytes } => {
            tracing::info!(path, bytes, "leftover moved to the trash");
        }
        TrashResult::Unchecked => {
            tracing::warn!(
                path,
                "a folder not known to be the leftover went to the trash"
            );
        }
        TrashResult::Kept(KeepReason::UnknownLeftover) => {
            tracing::warn!(
                ?id,
                "leftover of another report asked for: kept (invariant 1)"
            );
        }
        TrashResult::Kept(reason @ KeepReason::Refused(_)) => {
            tracing::warn!(path, ?reason, "leftover kept: the system refused");
        }
        TrashResult::Kept(reason) => tracing::info!(path, ?reason, "leftover kept"),
    }
}

/// What every leftover's move shares.
struct Mover<'a> {
    report: &'a LeftoverReport,
    installed: &'a Installed,
    rules: &'a TrashRules<'a>,
    bin: &'a Trash,
    deleted_at: &'a str,
}

impl Mover<'_> {
    fn run(&self, leftover: &Leftover) -> TrashResult {
        let (folder, print) = match self.checked(leftover) {
            Ok(checked) => checked,
            Err(reason) => return TrashResult::Kept(reason),
        };
        // Checked last, moved right after: the window for a write between
        // them is as small as can be; the trash keeps it restorable.
        let name = OsStr::new(&leftover.name);
        let answer = self
            .bin
            .put(&folder, name, &leftover.path, self.deleted_at, |moved| {
                moved.kind == EntryType::Directory && moved.identity == leftover.identity
            });
        match moved(answer) {
            Moved::Trashed => TrashResult::Trashed { bytes: print.bytes },
            Moved::Changed => TrashResult::Kept(KeepReason::Changed),
            Moved::OtherDevice => TrashResult::Kept(KeepReason::OtherDevice),
            Moved::Unchecked(trashed_as) => {
                tracing::warn!(path = %leftover.path.display(), ?trashed_as, "in the trash under this name");
                TrashResult::Unchecked
            }
            Moved::Refused(refusal) => TrashResult::Kept(KeepReason::Refused(refusal)),
        }
    }

    /// The folder holding `leftover`, opened, and its print, once every
    /// check of invariant 7 holds.
    ///
    /// # Errors
    ///
    /// Why it stays.
    fn checked(&self, leftover: &Leftover) -> Result<(Directory, Print), KeepReason> {
        let print = leftover.print.ok_or(KeepReason::NotCheckable)?;
        match leftover.kind.known(self.installed).bears(&leftover.name) {
            Some(false) => {}
            Some(true) => return Err(KeepReason::Installed),
            None => return Err(KeepReason::SourceUnread),
        }
        if excluded(self.rules.exclusions, &leftover.path) {
            return Err(KeepReason::Excluded);
        }
        let parent = self
            .report
            .parents
            .get(leftover.parent)
            .ok_or(KeepReason::Changed)?;
        let folder =
            reach(&parent.path, parent.identity, &[]).map_err(|unreached| match unreached {
                Unreached::Changed => KeepReason::Changed,
                Unreached::Io(error) => KeepReason::Refused((&error).into()),
            })?;
        let device = folder
            .status()
            .map_err(|error| KeepReason::Refused((&error).into()))?
            .identity
            .device();
        if device != self.bin.device() {
            return Err(KeepReason::OtherDevice);
        }
        let name = OsStr::new(&leftover.name);
        let status = folder
            .status_of(name)
            .map_err(|error| refused_or_gone(&error))?;
        if status.kind != EntryType::Directory || status.identity != leftover.identity {
            return Err(KeepReason::Changed);
        }
        if self.rules.protected.is_protected_directory(status.identity) {
            return Err(KeepReason::Protected);
        }
        let walk = Walk {
            owner: self.report.user,
            device,
            most: self.report.most_entries,
            exclusions: self.rules.exclusions,
        };
        match printed(&folder, name, &status, &walk) {
            Ok(again) if again.same_as(&print) => Ok((folder, print)),
            Ok(again) => {
                tracing::debug!(path = %leftover.path.display(), ?print, ?again, "print changed");
                Err(KeepReason::Changed)
            }
            Err(Unprinted::Excluded) => Err(KeepReason::Excluded),
            Err(Unprinted::Io(error)) => Err(KeepReason::Refused((&error).into())),
            Err(unprinted) => {
                tracing::debug!(path = %leftover.path.display(), ?unprinted, "no longer what was read");
                Err(KeepReason::Changed)
            }
        }
    }
}

/// What the system refused with `error` reading the leftover itself, a
/// leftover gone being no refusal.
fn refused_or_gone(error: &io::Error) -> KeepReason {
    if error.kind() == io::ErrorKind::NotFound {
        KeepReason::Gone
    } else {
        KeepReason::Refused(error.into())
    }
}

#[cfg(test)]
mod tests;
