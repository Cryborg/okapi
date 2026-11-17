use std::collections::BTreeSet;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use okapi_platform::{EntryStatus, EntryType, FileIdentity, Ownership, ROOT_USER, sticky_allows};
use okapi_rules::MiscRules;

use crate::options::Exclusions;
use crate::paths::{is_absent, is_absent_kind, is_session_name};
use crate::protection::ProtectedRoots;
use crate::report::{EntryKind, Limit, ScanStats};
use crate::watch::{PROGRESS_EVERY, Progress, Watch};

/// Reads the identity of a file from its metadata.
pub(crate) type IdentityReader = fn(&Metadata) -> Option<FileIdentity>;

/// What the walks read beyond an entry's type: from metadata for the scan,
/// from a status read through a descriptor for the cleaning's owner. The
/// platform's readers in production, stand-ins in tests (a test cannot set a
/// file's status change time back, nor fake a mount point).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Readers {
    pub(crate) identity: IdentityReader,
    /// When the object last changed, content or inode.
    pub(crate) last_change: fn(&Metadata) -> Option<SystemTime>,
    /// Who owns it, and for a directory whether it is sticky (a test cannot
    /// give a file to another user).
    pub(crate) ownership: fn(&Metadata) -> Option<Ownership>,
    /// Who owns an entry the cleaning read (same reason).
    pub(crate) entry_owner: fn(&EntryStatus) -> u32,
    /// Whether the entry at a path, its link never followed, is the root of
    /// a mount; `Ok(None)` when the system cannot tell (a test cannot mount).
    pub(crate) mount_root: fn(&Path) -> io::Result<Option<bool>>,
    /// The type of the filesystem a real path lies on, none when no mount
    /// holds it (a test cannot mount a FAT volume).
    pub(crate) filesystem: fn(&Path) -> io::Result<Option<String>>,
}

impl Readers {
    pub(crate) const SYSTEM: Self = Self {
        identity: okapi_platform::file_identity,
        last_change: okapi_platform::last_change,
        ownership: okapi_platform::ownership,
        entry_owner: status_owner,
        mount_root: okapi_platform::mount_root,
        filesystem: okapi_platform::filesystem_of,
    };
}

const fn status_owner(status: &EntryStatus) -> u32 {
    status.owner
}

/// How far a walk may go below one root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// Directory levels below the root.
    pub(crate) depth: usize,
    /// Entries below the root. The cleaning spends it on every entry it
    /// reads; the scan on every entry it reads that its path alone does not
    /// hold back, but on another user's only once offered: what another user
    /// leaves and the walk holds back never spends it ([`Walker::entry`]).
    pub(crate) entries: usize,
}

impl Limits {
    pub(crate) const DEFAULT: Self = Self {
        depth: 8,
        entries: 250_000,
    };
}

/// An entry the walk can offer: settled, and safe to remove as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    pub(crate) last_change: SystemTime,
    pub(crate) kind: EntryKind,
    pub(crate) identity: FileIdentity,
}

/// What a walk honours, besides its root.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WalkContext<'a> {
    /// Entries modified after this instant are too recent to offer.
    pub(crate) cutoff: SystemTime,
    pub(crate) exclusions: &'a Exclusions,
    pub(crate) misc: &'a MiscRules,
    pub(crate) protected: &'a ProtectedRoots,
    /// The identities of every scan root, withheld ones included, and of
    /// every directory a browser keeps its data in: a root met below another
    /// one keeps its own entries (none, when withheld or a browser's) and is
    /// never entered by the outer walk.
    pub(crate) roots: &'a BTreeSet<FileIdentity>,
    pub(crate) readers: Readers,
    pub(crate) limits: Limits,
    /// The user who will remove what is offered: root ([`ROOT_USER`]) for
    /// what needs the administrator, removed through the privileged helper;
    /// `None` when unknown. The scan holds back another user's entry in a
    /// sticky directory (`/tmp`), which the remover may not remove; with root,
    /// both walks hold back the directories [`Self::refuses_owner`] refuses.
    pub(crate) remover: Option<u32>,
    /// Who owns the scan root: read when the scan opens it, when the cleaning
    /// reaches it; `None` before, when it cannot be read, or for a single file
    /// target.
    pub(crate) root_owner: Option<u32>,
    /// A file with other names (hard links), or whose names cannot be
    /// counted, is held back: removing one of its names frees nothing
    /// (`keepHardLinked`, snapd's cache).
    pub(crate) keep_hard_linked: bool,
    /// A directory changed after the cutoff is held back whole, never
    /// offered entry by entry: what was just moved there keeps the dates it
    /// had elsewhere (a folder the user chose, where nothing is a cache).
    pub(crate) recent_holds_below: bool,
}

/// Why an entry is held back, which keeps its parent directory open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HoldBack {
    Excluded,
    /// A name the rules protect, or one a running session relies on.
    ProtectedName,
    Link,
    /// A socket, a pipe or a device.
    Special,
    /// Its type, identity, date or mount status cannot be read.
    Unreadable,
    /// Another device, or the root of a mount.
    OtherFilesystem,
    /// Another scan root, a browser's data, or a variable's directory or one
    /// of its ancestors.
    Reserved,
    /// A file changed after the cutoff.
    Recent,
    /// Another user's entry in a sticky directory, which the remover may not
    /// remove; or, root removing, a directory that belongs to neither root
    /// nor the root's owner ([`WalkContext::refuses_owner`]).
    NotOwned,
    /// A file with other names, or whose names cannot be counted, below a
    /// root that keeps them ([`WalkContext::keep_hard_linked`]).
    HardLinked,
}

/// What is known of an entry, read without following links. `None` is what
/// could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Observed {
    pub(crate) kind: EntryType,
    pub(crate) identity: Option<FileIdentity>,
    pub(crate) last_change: Option<SystemTime>,
    pub(crate) mount_root: Option<bool>,
    pub(crate) owner: Option<u32>,
    /// How many names (hard links) it has; none when they cannot be
    /// counted, which a root keeping hard-linked files holds back.
    pub(crate) links: Option<u64>,
}

impl Observed {
    /// What the cleaning read through a descriptor.
    pub(crate) fn from_status(status: &EntryStatus, readers: Readers) -> Self {
        Self {
            kind: status.kind,
            identity: Some(status.identity),
            last_change: Some(status.last_change),
            mount_root: status.mount_root,
            owner: Some((readers.entry_owner)(status)),
            links: Some(status.links),
        }
    }

    /// What the scan reads by path, with whether the entry is the root of a
    /// mount, read apart ([`Walker::mount_root`]).
    fn from_metadata(metadata: &Metadata, readers: Readers, mount_root: bool) -> Self {
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            EntryType::Link
        } else if file_type.is_file() {
            EntryType::File
        } else if file_type.is_dir() {
            EntryType::Directory
        } else {
            EntryType::Other
        };
        Self {
            kind,
            identity: (readers.identity)(metadata),
            last_change: (readers.last_change)(metadata),
            mount_root: Some(mount_root),
            owner: (readers.ownership)(metadata).map(|ownership| ownership.owner),
            links: okapi_platform::link_count(metadata),
        }
    }
}

/// Whether a running session relies on the entry `path`, a directory or not,
/// `top_level` when it lies directly below its scan root.
pub(crate) fn by_name(path: &Path, is_directory: bool, top_level: bool) -> Option<HoldBack> {
    path.file_name()
        .is_some_and(|name| is_session_name(name, is_directory, top_level))
        .then_some(HoldBack::ProtectedName)
}

/// An entry the policy accepts, with what it read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Accepted {
    pub(crate) identity: FileIdentity,
    pub(crate) last_change: SystemTime,
}

impl WalkContext<'_> {
    /// What holds an entry back by its path alone, before anything is read.
    pub(crate) fn by_path(&self, path: &Path) -> Option<HoldBack> {
        if self.exclusions.excludes(path) {
            return Some(HoldBack::Excluded);
        }
        self.misc.protects(path).then_some(HoldBack::ProtectedName)
    }

    /// What holds an entry back by what was read of it, on the device
    /// `device`. `own_root` is the scan root the entry itself is, which the
    /// check against the other roots skips (a single file target). A
    /// directory's own date is judged by the caller: the scan after its
    /// contents (it may still offer them one by one; under a folder the user
    /// chose, before), the cleaning before it touches them.
    pub(crate) fn by_status(
        &self,
        observed: &Observed,
        device: u64,
        own_root: Option<FileIdentity>,
    ) -> Result<Accepted, HoldBack> {
        match observed.kind {
            EntryType::Link => return Err(HoldBack::Link),
            EntryType::Other => return Err(HoldBack::Special),
            EntryType::File | EntryType::Directory => {}
        }
        let (Some(identity), Some(last_change), Some(mount_root)) =
            (observed.identity, observed.last_change, observed.mount_root)
        else {
            return Err(HoldBack::Unreadable);
        };
        if self.exclusions.excludes_identity(identity) {
            return Err(HoldBack::Excluded);
        }
        if identity.device() != device || mount_root {
            return Err(HoldBack::OtherFilesystem);
        }
        // Another scan root met here (a file target below a directory root, a
        // nested root) keeps its own entries.
        if own_root != Some(identity) && self.roots.contains(&identity) {
            return Err(HoldBack::Reserved);
        }
        if observed.kind == EntryType::File && last_change > self.cutoff {
            return Err(HoldBack::Recent);
        }
        if observed.kind == EntryType::Directory && self.protected.is_protected_directory(identity)
        {
            return Err(HoldBack::Reserved);
        }
        if self.refuses_owner(observed.kind, observed.owner) {
            return Err(HoldBack::NotOwned);
        }
        if self.keep_hard_linked && observed.kind == EntryType::File && observed.links != Some(1) {
            return Err(HoldBack::HardLinked);
        }
        Ok(Accepted {
            identity,
            last_change,
        })
    }

    /// Whether root, removing what is offered, must neither enter nor offer
    /// an entry of `kind` owned by `owner`: a directory of a third party
    /// ([`Self::is_third_party`]). Another user may have moved there a tree
    /// they could not empty (`/var/crash` is writable by all), which root,
    /// whom nothing stops, would remove whole. A file is never refused so: a
    /// crash report belongs to the user who crashed, and a file moved in is
    /// one its mover could remove already.
    pub(crate) fn refuses_owner(&self, kind: EntryType, owner: Option<u32>) -> bool {
        kind == EntryType::Directory && self.is_third_party(owner)
    }

    fn is_third_party(&self, owner: Option<u32>) -> bool {
        is_third_party(self.remover, self.root_owner, owner)
    }
}

/// Whether, `remover` being root, `owner` is neither root nor `root_owner`,
/// the owner of the root walked (an unknown owner included): a directory of
/// a third party, which root never enters nor removes. Never with another
/// remover. The one rule of the engine and of the helper's vacuum.
#[must_use]
pub fn is_third_party(remover: Option<u32>, root_owner: Option<u32>, owner: Option<u32>) -> bool {
    remover == Some(ROOT_USER)
        && !owner.is_some_and(|owner| owner == ROOT_USER || Some(owner) == root_owner)
}

/// Why a walk could not even list its root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootProblem {
    Unreadable(io::ErrorKind),
    /// It is no longer the directory that was checked.
    Replaced,
}

/// What a walk found below its root.
#[derive(Debug, Default)]
pub(crate) struct Walk {
    pub(crate) found: Vec<Found>,
    pub(crate) stats: ScanStats,
    pub(crate) limits_reached: Vec<Limit>,
    pub(crate) root_problem: Option<RootProblem>,
}

/// What resolving one entry gives: the entries it offers, and whether nothing
/// below it was held back, which is what makes removing it whole safe.
struct Resolved {
    found: Vec<Found>,
    complete: bool,
    size: u64,
}

impl Resolved {
    const fn held_back() -> Self {
        Self {
            found: Vec::new(),
            complete: false,
            size: 0,
        }
    }
}

/// What the whole scan tells its watcher, carried from one root's walk to
/// the next: the scan's cancel token is read here, never in [`WalkContext`],
/// which the cleaning shares (an entry being removed is never cut in two).
#[derive(Debug)]
pub(crate) struct ScanProgress<'w> {
    watch: &'w Watch,
    entries_seen: u64,
    pub(crate) found_bytes: u64,
    pub(crate) issues: usize,
}

impl<'w> ScanProgress<'w> {
    pub(crate) const fn new(watch: &'w Watch) -> Self {
        Self {
            watch,
            entries_seen: 0,
            found_bytes: 0,
            issues: 0,
        }
    }

    /// The entries reached so far, in the whole scan.
    pub(crate) const fn entries_seen(&self) -> u64 {
        self.entries_seen
    }

    /// One more entry reached, at `path`: said every [`PROGRESS_EVERY`].
    fn entry_seen(&mut self, path: &Path) {
        self.entries_seen += 1;
        if self.entries_seen.is_multiple_of(PROGRESS_EVERY) {
            self.watch.emit(Progress::Walking {
                path: path.to_path_buf(),
                entries_seen: self.entries_seen,
                found_bytes: self.found_bytes,
                issues: self.issues,
            });
        }
    }
}

struct Walker<'a, 'p, 'w> {
    context: WalkContext<'a>,
    device: u64,
    visited: usize,
    walk: Walk,
    progress: &'p mut ScanProgress<'w>,
    /// The scan was cancelled: nothing more is read, nothing is complete.
    cancelled: bool,
}

/// Offers what lies below `root`, never `root` itself, which must be a
/// canonical directory with identity `root_identity`.
///
/// An entry is offered when it is a regular file last changed at or before the
/// cutoff (content or inode, see [`Readers::last_change`]), or a directory last
/// changed at or before the cutoff whose whole contents are offered (it is
/// then one entry). Anything else is held back and keeps its parent open: a
/// link, a special file, another device or the root of a mount, a variable's
/// directory or one of its ancestors, another scan root, an unreadable or
/// recent entry, an excluded or protected name, a name a running session
/// relies on, an entry the remover may not remove from a sticky directory, a
/// directory root may not enter ([`WalkContext::refuses_owner`]), a file with
/// other names below a root that keeps them, or a limit reached.
///
/// No link is followed: every entry is judged by metadata read without
/// following links, and a directory is listed only if, once opened, it is
/// still the directory that was judged. A swap between the two reads cannot be
/// excluded by paths alone (a bind mount of a directory onto itself keeps its
/// identity; a component swapped for a link is followed by the path): cleaning
/// checks every entry again, through descriptors.
///
/// The root itself is never judged as a mount: it may be one (`/tmp`).
pub(crate) fn walk(
    root: &Path,
    root_identity: FileIdentity,
    context: WalkContext<'_>,
    progress: &mut ScanProgress<'_>,
) -> Walk {
    let mut walker = Walker {
        context,
        device: root_identity.device(),
        visited: 0,
        walk: Walk::default(),
        progress,
        cancelled: false,
    };
    match walker.open_directory(root, root_identity) {
        Ok((entries, ownership)) => {
            walker.context.root_owner = ownership.map(|ownership| ownership.owner);
            if walker.context.root_owner.is_none() && context.remover == Some(ROOT_USER) {
                tracing::debug!(root = ?root, "owner of the root unreadable: root enters only its own directories");
            }
            let resolved = walker.children(entries, context.limits.depth, ownership);
            walker.walk.found = resolved.found;
        }
        Err(problem) => walker.walk.root_problem = Some(problem),
    }
    walker.walk
}

impl Walker<'_, '_, '_> {
    /// Lists `directory`, after checking that what was opened is still the
    /// directory of `identity` (not a link swapped in since it was judged);
    /// gives its ownership too.
    fn open_directory(
        &self,
        directory: &Path,
        identity: FileIdentity,
    ) -> Result<(fs::ReadDir, Option<Ownership>), RootProblem> {
        let entries =
            fs::read_dir(directory).map_err(|error| RootProblem::Unreadable(error.kind()))?;
        let opened = fs::symlink_metadata(directory)
            .map_err(|error| RootProblem::Unreadable(error.kind()))?;
        let unchanged =
            opened.is_dir() && (self.context.readers.identity)(&opened) == Some(identity);
        if unchanged {
            Ok((entries, (self.context.readers.ownership)(&opened)))
        } else {
            Err(RootProblem::Replaced)
        }
    }

    /// Resolves the entries of a directory owned as `listed`.
    fn children(
        &mut self,
        entries: fs::ReadDir,
        depth_left: usize,
        listed: Option<Ownership>,
    ) -> Resolved {
        let mut resolved = Resolved {
            found: Vec::new(),
            complete: true,
            size: 0,
        };
        for entry in entries {
            // Cancelled, or nothing more can be offered: nothing more is read.
            if self.cancelled || self.walk.limits_reached.contains(&Limit::Entries) {
                resolved.complete = false;
                break;
            }
            let child = match entry {
                Ok(entry) => self.entry(&entry.path(), depth_left, listed),
                Err(error) => {
                    tracing::debug!(%error, "directory entry unreadable");
                    self.walk.stats.unreadable += 1;
                    Resolved::held_back()
                }
            };
            resolved.complete &= child.complete;
            resolved.size = resolved.size.saturating_add(child.size);
            resolved.found.extend(child.found);
        }
        resolved
    }

    fn limit(&mut self, limit: Limit) -> Resolved {
        if !self.walk.limits_reached.contains(&limit) {
            self.walk.limits_reached.push(limit);
        }
        Resolved::held_back()
    }

    fn entry(&mut self, path: &Path, depth_left: usize, listed: Option<Ownership>) -> Resolved {
        // Before counting: a cancelled scan reads and counts nothing more.
        if self.progress.watch.is_cancelled() {
            self.cancelled = true;
            return Resolved::held_back();
        }
        self.progress.entry_seen(path);
        if let Some(reason) = self.context.by_path(path) {
            return self.held_back(path, reason);
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            // Gone since it was listed: nothing to count, the parent stays open.
            Err(error) if is_absent(&error) => return Resolved::held_back(),
            Err(error) => {
                tracing::debug!(path = ?path, %error, "entry unreadable");
                self.walk.stats.unreadable += 1;
                return Resolved::held_back();
            }
        };
        let Some(mount_root) = self.mount_root(path, &metadata) else {
            return Resolved::held_back();
        };
        let observed = Observed::from_metadata(&metadata, self.context.readers, mount_root);
        let removable = self.removable_from(listed, observed.owner);
        // Another user's entry spends the budget only once offered: whatever
        // they leave in a shared directory (`/tmp`, `/var/crash`) and the walk
        // holds back would otherwise keep the remover's own entries from being
        // offered. It costs a reading, never a place in the report.
        let third_party = !removable || self.context.is_third_party(observed.owner);
        if !third_party && self.spend_budget() {
            return self.limit(Limit::Entries);
        }
        let accepted = match self.context.by_status(&observed, self.device, None) {
            Ok(accepted) => accepted,
            Err(reason) => return self.held_back(path, reason),
        };
        let is_directory = observed.kind == EntryType::Directory;
        let top_level = depth_left == self.context.limits.depth;
        if let Some(reason) = by_name(path, is_directory, top_level) {
            return self.held_back(path, reason);
        }
        if !removable {
            return self.held_back(path, HoldBack::NotOwned);
        }
        // A third party's directory is never entered (`refuses_owner`).
        if !is_directory {
            if third_party && self.spend_budget() {
                return self.limit(Limit::Entries);
            }
            self.progress.found_bytes = self.progress.found_bytes.saturating_add(metadata.len());
            return Self::file(path, &metadata, accepted);
        }
        let Some(depth_left) = depth_left.checked_sub(1) else {
            return self.limit(Limit::Depth);
        };
        self.directory(path, accepted.last_change, accepted.identity, depth_left)
    }

    /// Spends one entry of the budget; `true` when it was already spent.
    fn spend_budget(&mut self) -> bool {
        self.visited += 1;
        self.visited > self.context.limits.entries
    }

    /// Whether the entry at `path`, a file or a directory, is the root of a
    /// mount, a bind mount of the same device included: such an entry keeps
    /// its parent open, as another device does. `None` when it is gone or its
    /// status cannot be read (traced and counted here). A system that cannot
    /// tell gives `false`, unlike the cleaning's fail-closed reading: the
    /// cleaning refuses to run on such a system (`tells_mount_roots`).
    fn mount_root(&mut self, path: &Path, metadata: &Metadata) -> Option<bool> {
        if !(metadata.is_file() || metadata.is_dir()) {
            return Some(false);
        }
        match (self.context.readers.mount_root)(path) {
            Ok(mount_root) => Some(mount_root.unwrap_or(false)),
            Err(error) if is_absent(&error) => None,
            Err(error) => {
                tracing::debug!(path = ?path, %error, "mount status unreadable");
                self.walk.stats.unreadable += 1;
                None
            }
        }
    }

    /// Whether the remover may remove an entry owned by `owner` from a
    /// directory owned as `listed`, as far as the sticky bit goes. Without a
    /// known remover or directory, nothing more is held back; an entry whose
    /// owner cannot be read in a sticky directory is.
    fn removable_from(&self, listed: Option<Ownership>, owner: Option<u32>) -> bool {
        let (Some(user), Some(directory)) = (self.context.remover, listed) else {
            return true;
        };
        owner.map_or(!directory.sticky, |owner| {
            sticky_allows(user, directory, owner)
        })
    }

    fn held_back(&mut self, path: &Path, reason: HoldBack) -> Resolved {
        tracing::trace!(path = ?path, ?reason, "held back");
        self.walk.stats.count(reason);
        Resolved::held_back()
    }

    fn file(path: &Path, metadata: &Metadata, accepted: Accepted) -> Resolved {
        Resolved {
            found: vec![Found {
                path: path.to_path_buf(),
                size: metadata.len(),
                last_change: accepted.last_change,
                kind: EntryKind::File,
                identity: accepted.identity,
            }],
            complete: true,
            size: metadata.len(),
        }
    }

    fn directory(
        &mut self,
        path: &Path,
        modified: SystemTime,
        identity: FileIdentity,
        depth_left: usize,
    ) -> Resolved {
        // Cancelled while this entry was read: not even opened.
        if self.progress.watch.is_cancelled() {
            self.cancelled = true;
            return Resolved::held_back();
        }
        if self.context.recent_holds_below && modified > self.context.cutoff {
            tracing::trace!(path = ?path, "directory recent, held back whole");
            self.walk.stats.recent += 1;
            return Resolved::held_back();
        }
        let (entries, ownership) = match self.open_directory(path, identity) {
            Ok(opened) => opened,
            Err(RootProblem::Replaced) => {
                tracing::debug!(path = ?path, "directory replaced since judged");
                self.walk.stats.replaced += 1;
                return Resolved::held_back();
            }
            Err(RootProblem::Unreadable(error)) => {
                if !is_absent_kind(error) {
                    tracing::debug!(path = ?path, ?error, "directory unreadable");
                    self.walk.stats.unreadable += 1;
                }
                return Resolved::held_back();
            }
        };
        let children = self.children(entries, depth_left, ownership);
        if !children.complete {
            return children;
        }
        // A directory changed recently is in use (an extraction in progress, a
        // lock taken with `mkdir`), even when everything in it is old: its old
        // entries are still offered one by one, the directory is not.
        if modified > self.context.cutoff {
            tracing::trace!(path = ?path, "directory recent, offered entry by entry");
            self.walk.stats.recent += 1;
            return Resolved {
                complete: false,
                ..children
            };
        }
        // Shown as old as its most recent descendant, which is still before
        // the cutoff.
        let last_change = children
            .found
            .iter()
            .map(|found| found.last_change)
            .fold(modified, SystemTime::max);
        Resolved {
            found: vec![Found {
                path: path.to_path_buf(),
                size: children.size,
                last_change,
                kind: EntryKind::Directory,
                identity,
            }],
            complete: true,
            size: children.size,
        }
    }
}
