use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime};

use okapi_platform::{
    DESCRIPTOR_REMOVAL_SUPPORTED, Directory, EntryStatus, EntryType, FileIdentity,
    IDENTITY_SUPPORTED, tells_mount_roots,
};
use okapi_rules::{MiscRules, RuleSet, TemplateVariables};

use crate::options::ScanOptions;
use crate::preconditions::{Stop, protection, stops_before_reading};
use crate::protection::ProtectedRoots;
use crate::report::{BrowserLock, EntryKind, FoundItem, ItemId, ScanReport};
use crate::walk::{HoldBack, Limits, Observed, Readers, WalkContext, by_name};
use crate::watch::{Progress, Watch};

/// Why an entry, or what is left of it, was not removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// The identifier does not come from this report (invariant 1).
    UnknownItem,
    /// It is no longer what the scan saw: another object, another type, or a
    /// link on the way to it.
    Replaced,
    /// It, or something in it, changed after its cutoff (invariant 2).
    Recent,
    /// A directory whose contents are no longer all removable, for another
    /// reason than age: a link, a special file, a mount, an exclusion, a
    /// protected name or a limit appeared in it.
    Changed,
    /// An exclusion in force covers it, or a directory on the way to it.
    Excluded,
    /// It, or its scan root, holds a variable's directory (invariant 6) or
    /// another scan root, or it has a protected name.
    Protected,
    /// The system refuses, or cannot tell what the cleaning must check; `code`
    /// is the system's own error number, when there is one.
    Failed {
        kind: io::ErrorKind,
        code: Option<i32>,
    },
    /// The cleaning was cancelled ([`crate::Cancel`]) before its turn: it is
    /// untouched. Never the reason of a partial removal: an entry is removed
    /// whole or not at all.
    Cancelled,
    /// The browser whose cache it is runs: its lock is in its user-data
    /// directory.
    BrowserOpen,
}

impl KeepReason {
    /// Its stable name, for scripts and the history.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::UnknownItem => "unknown_item",
            Self::Replaced => "replaced",
            Self::Recent => "recent",
            Self::Changed => "changed",
            Self::Excluded => "excluded",
            Self::Protected => "protected",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
            Self::BrowserOpen => "browser_open",
        }
    }
}

/// What became of one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanResult {
    /// Removed with everything in it; `bytes` is the size of the files
    /// removed, measured as the scan measures it (a hard link counts).
    Removed { bytes: u64 },
    /// A directory emptied in part: removal stopped at the first entry that
    /// had to stay, which the check made beforehand did not foresee (it changed
    /// meanwhile, or the system refused what it seemed to allow).
    Partial { bytes: u64, reason: KeepReason },
    /// Already gone.
    Gone,
    /// Nothing was touched.
    Kept(KeepReason),
}

impl CleanResult {
    /// Its stable name, for scripts and the history.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Removed { .. } => "removed",
            Self::Partial { .. } => "partial",
            Self::Gone => "gone",
            Self::Kept(_) => "kept",
        }
    }

    /// What it freed: the size of every file removed.
    #[must_use]
    pub const fn removed_bytes(self) -> u64 {
        match self {
            Self::Removed { bytes } | Self::Partial { bytes, .. } => bytes,
            Self::Gone | Self::Kept(_) => 0,
        }
    }
}

/// One entry asked for, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanOutcome {
    id: ItemId,
    path: Option<PathBuf>,
    result: CleanResult,
    blocked: Option<PathBuf>,
}

impl CleanOutcome {
    /// Records the outcome: a refusal of the system or a removal stopped
    /// halfway is a warning, anything else an information.
    fn trace(&self) {
        let path = self.path.as_deref().map(Path::display);
        let blocked = self.blocked.as_deref().map(Path::display);
        match self.result {
            CleanResult::Removed { .. } => {
                tracing::info!(path = ?path, result = ?self.result, "entry removed");
            }
            CleanResult::Gone => tracing::info!(path = ?path, "entry already gone"),
            CleanResult::Partial { .. } | CleanResult::Kept(KeepReason::Failed { .. }) => {
                tracing::warn!(path = ?path, result = ?self.result, blocked = ?blocked, "entry not removed");
            }
            CleanResult::Kept(_) => {
                tracing::info!(path = ?path, result = ?self.result, blocked = ?blocked, "entry kept");
            }
        }
    }

    #[must_use]
    pub const fn id(&self) -> ItemId {
        self.id
    }

    /// The entry's path, unless the identifier is unknown.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    #[must_use]
    pub const fn result(&self) -> CleanResult {
        self.result
    }

    /// The entry inside a directory that kept it, or part of it, from being
    /// removed; `None` when the reason lies in the entry itself.
    #[must_use]
    pub fn blocked(&self) -> Option<&Path> {
        self.blocked.as_deref()
    }
}

/// What a cleaning did, entry by entry.
#[derive(Debug, PartialEq, Eq)]
pub struct CleanReceipt {
    stops: Vec<Stop>,
    outcomes: Vec<CleanOutcome>,
}

impl CleanReceipt {
    /// What kept the cleaning from touching anything: then there is no outcome.
    #[must_use]
    pub fn stops(&self) -> &[Stop] {
        &self.stops
    }

    /// One outcome per identifier asked for, in the order asked, each once.
    #[must_use]
    pub fn outcomes(&self) -> &[CleanOutcome] {
        &self.outcomes
    }

    /// Whether everything asked for is gone: nothing stopped the cleaning and
    /// every entry was removed, or already gone (true when nothing was asked).
    /// An entry whose root was moved away is gone from where it was scanned.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.stops.is_empty()
            && self.outcomes.iter().all(|outcome| {
                matches!(
                    outcome.result,
                    CleanResult::Removed { .. } | CleanResult::Gone
                )
            })
    }

    /// The size of every file removed.
    #[must_use]
    pub fn removed_bytes(&self) -> u64 {
        self.outcomes
            .iter()
            .map(|outcome| outcome.result.removed_bytes())
            .fold(0, u64::saturating_add)
    }

    fn stopped(stops: Vec<Stop>) -> Self {
        for stop in &stops {
            tracing::error!(?stop, "cleaning stopped");
        }
        Self {
            stops,
            outcomes: Vec::new(),
        }
    }
}

/// Removes the entries of `report` named by `ids`, and only them, after
/// checking each again with the rules, variables, exclusions and minimum age
/// in force.
///
/// An entry is reached through directory descriptors from its scan root,
/// recognised by identity, never through a link (invariant 3), and must still
/// be the object the scan saw. Every entry asked for is first checked whole,
/// descendants included, without removing anything: its age against the
/// stricter of the scan's cutoff and the one `options` give (invariant 2), its
/// filesystem, its name, and the permission to remove it (write access, the
/// sticky bit, immutable files). Then the entries that passed are removed,
/// each entry read and judged again just before it goes; the first one that
/// must stay stops the removal of that entry. A root or an entry that holds a
/// variable's directory is never touched (invariant 6).
///
/// Remaining risks: a file renamed over, or written in place, between its last
/// check and its removal is removed; a directory moved elsewhere while it is
/// being emptied keeps being emptied where it went; a browser started while
/// an entry of its cache is being removed sees it go (its lock is read before
/// each entry, in each pass).
#[must_use]
pub fn clean(
    report: &ScanReport,
    ids: &[ItemId],
    rules: &RuleSet,
    variables: &TemplateVariables,
    options: &ScanOptions,
) -> CleanReceipt {
    clean_watched(report, ids, rules, variables, options, &Watch::none())
}

/// [`clean`], its progress sent to `watch` and cancelled by its token: an
/// entry not yet removed when the token is cancelled stays, `Kept(Cancelled)`,
/// unless its check already refused it (it keeps its reason) or its
/// identifier is not of this report (`UnknownItem`, invariant 1). An entry is
/// never cut in two: the token is read between entries only.
#[must_use]
pub fn clean_watched(
    report: &ScanReport,
    ids: &[ItemId],
    rules: &RuleSet,
    variables: &TemplateVariables,
    options: &ScanOptions,
    watch: &Watch,
) -> CleanReceipt {
    let _span = tracing::info_span!("clean", requested = ids.len()).entered();
    let started = Instant::now();
    let supported = IDENTITY_SUPPORTED && DESCRIPTOR_REMOVAL_SUPPORTED;
    let stops = stops_before_reading(supported, rules.os(), variables);
    let receipt = if stops.is_empty() {
        clean_items(
            report,
            ids,
            rules.misc(),
            variables,
            options,
            Limits::DEFAULT,
            watch,
        )
    } else {
        CleanReceipt::stopped(stops)
    };
    tracing::info!(
        outcomes = receipt.outcomes().len(),
        removed_bytes = receipt.removed_bytes(),
        complete = receipt.is_complete(),
        cancelled = watch.is_cancelled(),
        elapsed = ?started.elapsed(),
        "cleaning finished"
    );
    receipt
}

/// What the check of one identifier found.
enum Checked<'a> {
    Unknown,
    Refused(&'a FoundItem, Outcome),
    /// Checked, with the locks of the browsers its group empties.
    Ready(&'a FoundItem, &'a BTreeSet<BrowserLock>),
    /// Cancelled before its check.
    Cancelled(&'a FoundItem),
}

/// What became of an entry, and where inside it the reason lies.
type Outcome = (CleanResult, Option<PathBuf>);

/// [`clean`] once the system and the variables are known to be complete, with
/// the walking limits given. The token is read between entries, never inside
/// the walk of one (`Walking` and `WalkContext` do not carry it): an entry is
/// removed whole or not at all (invariant 2).
pub(crate) fn clean_items(
    report: &ScanReport,
    ids: &[ItemId],
    misc: &MiscRules,
    variables: &TemplateVariables,
    options: &ScanOptions,
    limits: Limits,
    watch: &Watch,
) -> CleanReceipt {
    if !tells_mount_roots() {
        return CleanReceipt::stopped(vec![Stop::UnsupportedSystem]);
    }
    let (global_cutoff, protected) = match protection(variables, options, Readers::SYSTEM.identity)
    {
        Ok(prepared) => prepared,
        Err(stop) => return CleanReceipt::stopped(vec![stop]),
    };
    let cleaner = Cleaner {
        report,
        misc,
        options,
        protected: &protected,
        global_cutoff,
        limits,
    };
    let mut dates = RemovedDates::new();
    let mut seen = BTreeSet::new();
    let unique: Vec<ItemId> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
    let total = unique.len();
    let checked = check_all(&cleaner, unique, watch, &mut dates);
    let mut removed_bytes: u64 = 0;
    let mut cancelled = false;
    let outcomes = checked
        .into_iter()
        .enumerate()
        .map(|(index, (id, checked))| {
            let cancelled_outcome = (CleanResult::Kept(KeepReason::Cancelled), None);
            let (path, (result, blocked)) = match checked {
                Checked::Unknown => (None, (CleanResult::Kept(KeepReason::UnknownItem), None)),
                Checked::Refused(item, outcome) => (Some(item), outcome),
                Checked::Cancelled(item) => (Some(item), cancelled_outcome),
                Checked::Ready(item, _) if watch.is_cancelled() => (Some(item), cancelled_outcome),
                Checked::Ready(item, locks) => (
                    Some(item),
                    held(locks, id).unwrap_or_else(|| {
                        cleaner
                            .run(item, CleanPass::Remove, &mut dates)
                            .map_or_else(
                                |outcome| outcome,
                                |bytes| (CleanResult::Removed { bytes }, None),
                            )
                    }),
                ),
            };
            if result == CleanResult::Kept(KeepReason::Cancelled) && !cancelled {
                cancelled = true;
                tracing::info!(
                    reached = index,
                    total,
                    removed_bytes,
                    "cleaning cancelled, the entries left untouched"
                );
            }
            removed_bytes = removed_bytes.saturating_add(result.removed_bytes());
            let outcome = CleanOutcome {
                id,
                path: path.map(|item| item.found.path.clone()),
                result,
                blocked,
            };
            outcome.trace();
            watch.emit(Progress::Cleaning {
                pass: CleanPass::Remove,
                done: index + 1,
                total,
                removed_bytes,
            });
            outcome
        })
        .collect();
    CleanReceipt {
        stops: Vec::new(),
        outcomes,
    }
}

/// The entry `id` kept, when one of the browsers whose cache it is in runs.
fn held(locks: &BTreeSet<BrowserLock>, id: ItemId) -> Option<Outcome> {
    let lock = locks.iter().find(|lock| lock.is_held())?;
    // Each outcome is traced with its reason: this says which lock.
    tracing::debug!(?id, browser = lock.browser().key(), lock = %lock.lock().display(), "browser open: entry kept");
    Some((
        CleanResult::Kept(KeepReason::BrowserOpen),
        Some(lock.lock().to_path_buf()),
    ))
}

/// The check pass over `ids`, each once: an identifier of another report is
/// always told (invariant 1); an entry reached after the token is cancelled
/// is not checked.
fn check_all<'r>(
    cleaner: &Cleaner<'r>,
    ids: Vec<ItemId>,
    watch: &Watch,
    dates: &mut RemovedDates,
) -> Vec<(ItemId, Checked<'r>)> {
    let total = ids.len();
    let mut cancelled_seen = false;
    ids.into_iter()
        .enumerate()
        .map(|(index, id)| {
            let checked = match cleaner.report.find(id) {
                None => Checked::Unknown,
                Some((group, item)) => {
                    let item = item.found_item();
                    if watch.is_cancelled() {
                        if !cancelled_seen {
                            cancelled_seen = true;
                            tracing::info!(
                                reached = index,
                                total,
                                "cleaning cancelled during its checks"
                            );
                        }
                        Checked::Cancelled(item)
                    } else if let Some(outcome) = held(group.locks(), id) {
                        Checked::Refused(item, outcome)
                    } else {
                        match cleaner.run(item, CleanPass::Check, dates) {
                            Ok(_) => Checked::Ready(item, group.locks()),
                            Err(outcome) => Checked::Refused(item, outcome),
                        }
                    }
                }
            };
            watch.emit(Progress::Cleaning {
                pass: CleanPass::Check,
                done: index + 1,
                total,
                removed_bytes: 0,
            });
            (id, checked)
        })
        .collect()
}

/// The two passes of a cleaning: every entry is first checked whole, then
/// removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanPass {
    Check,
    Remove,
}

/// For each file with other names (hard links) that this cleaning removed one
/// name of: its last change and its content date, read before. Removing a
/// name changes the inode date of the other names, which must not make them
/// look recent.
pub(crate) type RemovedDates = BTreeMap<FileIdentity, (SystemTime, SystemTime)>;

struct Cleaner<'a> {
    report: &'a ScanReport,
    misc: &'a MiscRules,
    options: &'a ScanOptions,
    protected: &'a ProtectedRoots,
    global_cutoff: SystemTime,
    limits: Limits,
}

/// What a removal has done so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Removed {
    pub(crate) bytes: u64,
    pub(crate) entries: u64,
}

impl Removed {
    const fn add(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries = self.entries.saturating_add(1);
    }

    /// What stopping now for `reason` amounts to.
    pub(crate) const fn stopped(self, reason: KeepReason) -> CleanResult {
        if self.entries == 0 {
            CleanResult::Kept(reason)
        } else {
            CleanResult::Partial {
                bytes: self.bytes,
                reason,
            }
        }
    }
}

/// The state of one entry's walk.
pub(crate) struct Walking<'a> {
    pub(crate) context: WalkContext<'a>,
    pub(crate) device: u64,
    pub(crate) pass: CleanPass,
    pub(crate) removed: Removed,
    pub(crate) visited: usize,
    pub(crate) dates: &'a mut RemovedDates,
    /// The entry, at or below the one walked, that stopped the walk.
    pub(crate) blocked: Option<PathBuf>,
}

impl Cleaner<'_> {
    fn context(&self, item: &FoundItem) -> WalkContext<'_> {
        WalkContext {
            cutoff: item.cutoff.min(self.global_cutoff),
            exclusions: self.options.exclusions(),
            misc: self.misc,
            protected: self.protected,
            roots: self.report.roots(),
            readers: Readers::SYSTEM,
            limits: self.limits,
            // Not read by the cleaning's walk: `ensure_removable` judges the
            // right to remove each entry.
            remover: None,
        }
    }

    /// Checks the entry, and with [`CleanPass::Remove`] removes it, giving the
    /// bytes removed. The error is the outcome when the entry, or part of it,
    /// stays.
    fn run(
        &self,
        item: &FoundItem,
        pass: CleanPass,
        dates: &mut RemovedDates,
    ) -> Result<u64, Outcome> {
        let context = self.context(item);
        let (parent, name, status) = self
            .judge_entry(item, &context, pass, dates)
            .map_err(|result| (result, None))?;
        let depth = depth_below_root(item);
        let mut walking = Walking {
            context,
            device: item.root_identity.device(),
            pass,
            removed: Removed::default(),
            visited: 0,
            dates,
            blocked: None,
        };
        let present = if status.kind == EntryType::Directory {
            let Some(depth_left) = context.limits.depth.checked_sub(depth) else {
                return Err((CleanResult::Kept(KeepReason::Changed), None));
            };
            walk_directory(
                &parent,
                name,
                &status,
                &item.found.path,
                depth_left,
                &mut walking,
            )
        } else if pass == CleanPass::Remove {
            remove_file(&parent, name, &status, &mut walking)
        } else {
            Ok(true)
        };
        let removed = walking.removed;
        match present {
            // Gone before we touched it; gone after we emptied it counts as
            // removed, with what we removed.
            Ok(false) if removed.entries == 0 => Err((CleanResult::Gone, None)),
            Ok(_) => Ok(removed.bytes),
            Err(reason) => Err((removed.stopped(reason), walking.blocked)),
        }
    }

    /// Reaches the entry and judges it alone: its root, its identity, its age,
    /// its filesystem, its name, and (when checking) the right to remove it.
    fn judge_entry<'a>(
        &self,
        item: &'a FoundItem,
        context: &WalkContext<'_>,
        pass: CleanPass,
        dates: &RemovedDates,
    ) -> Result<(Directory, &'a OsStr, EntryStatus), CleanResult> {
        let kept = CleanResult::Kept;
        if self.protected.refusal(item.root_identity).is_some() {
            return Err(kept(KeepReason::Protected));
        }
        if self
            .options
            .exclusions()
            .excludes_by_identity(&item.root, Readers::SYSTEM.identity)
        {
            return Err(kept(KeepReason::Excluded));
        }
        match context.by_path(&item.found.path) {
            Some(HoldBack::Excluded) => return Err(kept(KeepReason::Excluded)),
            Some(_) => return Err(kept(KeepReason::Protected)),
            None => {}
        }
        let (parent, name) = reach_parent(item)?;
        let status = parent
            .status_of(name)
            .map_err(|error| gone_or_failed(&error))?;
        let status = remembered(status, dates);
        if status.identity != item.found.identity || status.kind != entry_type(item.found.kind) {
            return Err(kept(KeepReason::Replaced));
        }
        context
            .by_status(
                &observed(&status),
                item.root_identity.device(),
                Some(item.root_identity),
            )
            .map_err(|reason| kept(keep_reason(reason)))?;
        let is_directory = status.kind == EntryType::Directory;
        if by_name(&item.found.path, is_directory, depth_below_root(item) <= 1).is_some() {
            return Err(kept(KeepReason::Protected));
        }
        if pass == CleanPass::Check {
            parent
                .ensure_removable(&status)
                .map_err(|error| kept(keep(&error)))?;
        }
        Ok((parent, name, status))
    }
}

/// How many components below its scan root the entry lies; 0 for a single
/// file target, which is its own root.
fn depth_below_root(item: &FoundItem) -> usize {
    item.found
        .path
        .strip_prefix(&item.root)
        .map_or(usize::MAX, |below| below.components().count())
}

/// The directory holding the entry, reached from its scan root through
/// descriptors, and the entry's name in it. The root is reached from the
/// filesystem root, one directory at a time and never through a link, so its
/// path is real by construction, and is recognised by its identity.
fn reach_parent(item: &FoundItem) -> Result<(Directory, &OsStr), CleanResult> {
    let path = &item.found.path;
    let replaced = || CleanResult::Kept(KeepReason::Replaced);
    if *path == item.root {
        // A single file target: its own directory is not a scan root, only
        // the file's identity vouches for it.
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(replaced());
        };
        return Ok((open_real(parent)?, name));
    }
    let mut directory = open_real(&item.root)?;
    let root = directory
        .status()
        .map_err(|error| CleanResult::Kept(keep(&error)))?;
    // The root itself is often a mount (`/tmp`): it is recognised by identity.
    if root.identity != item.root_identity {
        return Err(replaced());
    }
    let below = path.strip_prefix(&item.root).map_err(|_| replaced())?;
    let mut names: Vec<&OsStr> = below.iter().collect();
    let Some(name) = names.pop() else {
        return Err(replaced());
    };
    for component in names {
        let status = directory
            .status_of(component)
            .map_err(|error| gone_or_failed(&error))?;
        if !is_plain_directory_on(&status, item.root_identity.device()) {
            return Err(replaced());
        }
        directory = enter_or_stop(&directory, component, &status)?;
    }
    Ok((directory, name))
}

/// Opens the directory at the absolute path `path`, from the filesystem root
/// down, never through a link: what it opens is where `path` really is.
fn open_real(path: &Path) -> Result<Directory, CleanResult> {
    let mut directory = Directory::open(Path::new(FILESYSTEM_ROOT))
        .map_err(|error| CleanResult::Kept(keep(&error)))?;
    for component in path.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            // A canonical path holds neither `.` nor `..`.
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                return Err(CleanResult::Kept(KeepReason::Replaced));
            }
        };
        // A link or a file here fails to open, and `enter` reads it again to
        // say it was swapped.
        let status = directory
            .status_of(name)
            .map_err(|error| gone_or_failed(&error))?;
        directory = enter_or_stop(&directory, name, &status)?;
    }
    Ok(directory)
}

/// Where every absolute path starts.
const FILESYSTEM_ROOT: &str = "/";

/// [`enter`], with a gone or refused directory as the outcome of the entry.
fn enter_or_stop(
    parent: &Directory,
    name: &OsStr,
    status: &EntryStatus,
) -> Result<Directory, CleanResult> {
    match enter(parent, name, status) {
        Ok(Some(entered)) => Ok(entered),
        Ok(None) => Err(CleanResult::Gone),
        Err(reason) => Err(CleanResult::Kept(reason)),
    }
}

/// Whether `status` is a directory of the device `device` and no mount root:
/// what the descent below a scan root may go through.
pub(crate) fn is_plain_directory_on(status: &EntryStatus, device: u64) -> bool {
    status.kind == EntryType::Directory
        && status.identity.device() == device
        && status.mount_root == Some(false)
}

/// Opens the subdirectory `name`, which must still be the one read as
/// `status`; `None` when it is gone.
pub(crate) fn enter(
    parent: &Directory,
    name: &OsStr,
    status: &EntryStatus,
) -> Result<Option<Directory>, KeepReason> {
    let directory = match parent.open_child(name) {
        Ok(directory) => directory,
        // Read it again to tell a removal or a swap from a refusal.
        Err(error) => {
            return match parent.status_of(name) {
                Err(again) if again.kind() == io::ErrorKind::NotFound => Ok(None),
                Ok(again)
                    if again.kind != EntryType::Directory || again.identity != status.identity =>
                {
                    Err(KeepReason::Replaced)
                }
                _ => Err(keep(&error)),
            };
        }
    };
    let opened = directory.status().map_err(|error| keep(&error))?;
    if opened.identity == status.identity {
        Ok(Some(directory))
    } else {
        Err(KeepReason::Replaced)
    }
}

/// Checks, or removes, the directory `name` of `parent`, read as `status`, at
/// `path`, and everything in it; `false` when it is gone. Its own date is
/// judged when it is reached, before anything in it is touched: removing its
/// entries changes it, and a directory changed recently is in use.
fn walk_directory(
    parent: &Directory,
    name: &OsStr,
    status: &EntryStatus,
    path: &Path,
    depth_left: usize,
    walking: &mut Walking<'_>,
) -> Result<bool, KeepReason> {
    if status.last_change > walking.context.cutoff {
        return Err(KeepReason::Recent);
    }
    let Some(directory) = enter(parent, name, status)? else {
        return Ok(false);
    };
    walk_entries(&directory, path, depth_left, walking)?;
    if walking.pass == CleanPass::Remove {
        if !remove_directory(parent, name, status)? {
            return Ok(false);
        }
        walking.removed.add(0);
    }
    Ok(true)
}

/// Checks, or removes, everything in `directory`, at `path`, each entry
/// judged just before it is touched.
pub(crate) fn walk_entries(
    directory: &Directory,
    path: &Path,
    depth_left: usize,
    walking: &mut Walking<'_>,
) -> Result<(), KeepReason> {
    for name in directory.names().map_err(|error| keep(&error))? {
        let entry_path = path.join(&name);
        if let Err(reason) = walk_entry(directory, &name, &entry_path, depth_left, walking) {
            walking.blocked.get_or_insert(entry_path);
            return Err(reason);
        }
    }
    Ok(())
}

fn walk_entry(
    directory: &Directory,
    name: &OsStr,
    path: &Path,
    depth_left: usize,
    walking: &mut Walking<'_>,
) -> Result<(), KeepReason> {
    walking.visited += 1;
    if walking.visited > walking.context.limits.entries {
        return Err(KeepReason::Changed);
    }
    if let Some(reason) = walking.context.by_path(path) {
        return Err(keep_reason(reason));
    }
    let status = match directory.status_of(name) {
        Ok(status) => remembered(status, walking.dates),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(keep(&error)),
    };
    walking
        .context
        .by_status(&observed(&status), walking.device, None)
        .map_err(keep_reason)?;
    let is_directory = status.kind == EntryType::Directory;
    if let Some(reason) = by_name(path, is_directory, false) {
        return Err(keep_reason(reason));
    }
    if walking.pass == CleanPass::Check {
        directory
            .ensure_removable(&status)
            .map_err(|error| keep(&error))?;
    }
    if !is_directory {
        if walking.pass == CleanPass::Remove {
            remove_file(directory, name, &status, walking)?;
        }
        return Ok(());
    }
    let Some(depth_left) = depth_left.checked_sub(1) else {
        return Err(KeepReason::Changed);
    };
    walk_directory(directory, name, &status, path, depth_left, walking).map(drop)
}

/// Removes the file `name`, read as `status`; `false` when it was gone.
fn remove_file(
    directory: &Directory,
    name: &OsStr,
    status: &EntryStatus,
    walking: &mut Walking<'_>,
) -> Result<bool, KeepReason> {
    match directory.remove_file(name) {
        Ok(()) => {
            if status.links > 1 {
                walking
                    .dates
                    .entry(status.identity)
                    .or_insert((status.last_change, status.modified));
            } else {
                // Its last name: the inode may be reused by a new file.
                walking.dates.remove(&status.identity);
            }
            walking.removed.add(status.size);
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(keep(&error)),
    }
}

/// Removes the emptied subdirectory `name`, if it is still the one read as
/// `status`; `false` when it was gone.
pub(crate) fn remove_directory(
    parent: &Directory,
    name: &OsStr,
    status: &EntryStatus,
) -> Result<bool, KeepReason> {
    let now = match parent.status_of(name) {
        Ok(now) => now,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(keep(&error)),
    };
    if now.identity != status.identity {
        return Err(KeepReason::Replaced);
    }
    match parent.remove_directory(name) {
        Ok(()) => Ok(true),
        Err(error) => match error.kind() {
            io::ErrorKind::NotFound => Ok(false),
            // Something appeared in it since it was emptied.
            io::ErrorKind::DirectoryNotEmpty => Err(KeepReason::Changed),
            _ => Err(keep(&error)),
        },
    }
}

/// `status`, dated as before this cleaning removed another name of the same
/// file, if its content is still the one read then: a write through this
/// name, or a new file reusing the inode, keeps its own date.
pub(crate) fn remembered(mut status: EntryStatus, dates: &RemovedDates) -> EntryStatus {
    if let Some(&(before, modified)) = dates.get(&status.identity)
        && modified == status.modified
    {
        status.last_change = before;
    }
    status
}

const fn observed(status: &EntryStatus) -> Observed {
    Observed {
        kind: status.kind,
        identity: Some(status.identity),
        last_change: Some(status.last_change),
        mount_root: status.mount_root,
    }
}

const fn entry_type(kind: EntryKind) -> EntryType {
    match kind {
        EntryKind::File => EntryType::File,
        EntryKind::Directory => EntryType::Directory,
    }
}

/// What holding back an entry below the one cleaned means for it.
const fn keep_reason(reason: HoldBack) -> KeepReason {
    match reason {
        HoldBack::Recent => KeepReason::Recent,
        HoldBack::Reserved => KeepReason::Protected,
        HoldBack::Unreadable => KeepReason::Failed {
            kind: io::ErrorKind::Unsupported,
            code: None,
        },
        // Never from the cleaning's walk: `by_path` and `by_status` never hold
        // back for ownership, `ensure_removable` judges the right to remove.
        HoldBack::NotOwned => KeepReason::Failed {
            kind: io::ErrorKind::PermissionDenied,
            code: None,
        },
        HoldBack::Excluded
        | HoldBack::ProtectedName
        | HoldBack::Link
        | HoldBack::Special
        | HoldBack::OtherFilesystem => KeepReason::Changed,
    }
}

fn keep(error: &io::Error) -> KeepReason {
    KeepReason::Failed {
        kind: error.kind(),
        code: error.raw_os_error(),
    }
}

/// An entry that is no longer there is gone, not failed.
fn gone_or_failed(error: &io::Error) -> CleanResult {
    if error.kind() == io::ErrorKind::NotFound {
        CleanResult::Gone
    } else {
        CleanResult::Kept(keep(error))
    }
}
