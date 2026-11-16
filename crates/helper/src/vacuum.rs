//! The vacuum of systemd's journal, as `journalctl --vacuum-time
//! --vacuum-size` does it (systemd's `journal_directory_vacuum`), coded here
//! rather than run (invariant 4): in the local machine's directory of the
//! journal (as journalctl without `--merge`), the archived journals whose
//! first entry is older than [`JOURNAL_MAX_AGE_DAYS`], then the oldest while
//! the directory takes more than [`JOURNAL_MAX_USE_GIGABYTES`]; never the
//! journal being written.
//!
//! Stricter than systemd: nothing is followed through a link, nor a
//! directory of a third party entered (`okapi_engine::is_third_party`); an
//! archive changed after the age floor of the request, excluded, or locked
//! (immutable or append-only), stays, and the size then takes no newer one
//! in its place; each file is read again just before it is removed
//! (invariant 2). Unlike systemd, a file is removed without being truncated
//! first: one a reader keeps open (`journalctl -f`) frees its room when that
//! reader closes it.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use okapi_engine::{
    CleanResult, KeepReason, Origin, ScanIssue, ScanOptions, UnsafeReason, Watch, is_third_party,
};
use okapi_platform::{
    Directory, EntryStatus, EntryType, Reached, Unreached, reach_path, reach_with,
};
use okapi_rules::CleanupAction;
use okapi_store::HistoryOutcome;

/// An archived journal whose first entry is older than this many days goes.
/// The one source of this limit: the rule's description shows it
/// ([`description_values`]).
pub const JOURNAL_MAX_AGE_DAYS: u64 = 30;

/// Archived journals go, oldest first, while a machine's directory takes
/// more than this many gigabytes (decimal, as Okapi shows sizes). The one
/// source of this limit, as [`JOURNAL_MAX_AGE_DAYS`].
pub const JOURNAL_MAX_USE_GIGABYTES: u64 = 1;

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
const BYTES_PER_GIGABYTE: u64 = 1_000_000_000;

/// The values of the placeholders of the description of a rule that runs
/// `action`, by name: those of the operation the helper does.
#[must_use]
pub const fn description_values(action: CleanupAction) -> &'static [(&'static str, u64)] {
    match action {
        CleanupAction::JournalVacuum => &[
            ("days", JOURNAL_MAX_AGE_DAYS),
            ("gigabytes", JOURNAL_MAX_USE_GIGABYTES),
        ],
        CleanupAction::UvPrune
        | CleanupAction::PnpmPrune
        | CleanupAction::WindowsComponents
        | CleanupAction::DeliveryOptimization
        | CleanupAction::DockerBuildPrune => &[],
    }
}

/// The limits of a vacuum: an archive older than `max_age` goes, then the
/// oldest while a directory takes more than `max_use` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) max_age: Duration,
    pub(crate) max_use: u64,
}

impl Limits {
    /// The journal's, the only ones outside the tests.
    pub(crate) const JOURNAL: Self = Self {
        max_age: Duration::from_secs(JOURNAL_MAX_AGE_DAYS * SECONDS_PER_DAY),
        max_use: JOURNAL_MAX_USE_GIGABYTES * BYTES_PER_GIGABYTE,
    };
}

/// How long the name of a machine's directory is: its identifier, in
/// hexadecimal.
const MACHINE_ID_DIGITS: usize = 32;
/// The digits of a sequence number or a time in a journal's name.
const COUNTER_DIGITS: usize = 16;
const ARCHIVED_SUFFIX: &str = ".journal";
const CORRUPTED_SUFFIX: &str = ".journal~";

/// Where a journal sorts among the archives of its directory: by the time
/// of its first entry, then by its sequence, then by its name. Unlike
/// systemd, which sorts two archives of one sequence by their numbers
/// first: that comparison is no total order, which Rust's sort requires.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Order {
    /// Microseconds since the clock's origin.
    realtime: u128,
    sequence: Option<(u128, u64)>,
    name: OsString,
}

/// What the name of a journal says of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Named {
    /// `<prefix>@<sequence id>-<sequence number>-<realtime>.journal`: closed
    /// by journald, it may go.
    Archived {
        sequence_id: u128,
        sequence_number: u64,
        realtime: u64,
    },
    /// `<prefix>@<realtime>-<random>.journal~`: put aside dirty or
    /// corrupted, it may go.
    Corrupted { realtime: u64 },
    /// Another `.journal` or `.journal~`, the one being written included:
    /// it takes room, but never goes.
    Active,
}

/// Reads `digits` hexadecimal digits, exactly.
fn hexadecimal(text: &str, digits: usize) -> Option<u128> {
    (text.len() == digits && text.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| u128::from_str_radix(text, 16).ok())
        .flatten()
}

fn counter(text: &str) -> Option<u64> {
    hexadecimal(text, COUNTER_DIGITS).and_then(|value| u64::try_from(value).ok())
}

/// What the file `name` is to the vacuum; `None` when it is no journal,
/// which is never touched nor counted.
fn named(name: &OsStr) -> Option<Named> {
    let name = name.to_str()?;
    if let Some(stem) = name.strip_suffix(ARCHIVED_SUFFIX) {
        let archived = || {
            let (rest, realtime) = stem.rsplit_once('-')?;
            let (rest, sequence_number) = rest.rsplit_once('-')?;
            let (prefix, sequence_id) = rest.rsplit_once('@')?;
            // As systemd, which wants a prefix.
            (!prefix.is_empty()).then_some(())?;
            Some(Named::Archived {
                sequence_id: hexadecimal(sequence_id, MACHINE_ID_DIGITS)?,
                sequence_number: counter(sequence_number)?,
                realtime: counter(realtime)?,
            })
        };
        return Some(archived().unwrap_or(Named::Active));
    }
    let stem = name.strip_suffix(CORRUPTED_SUFFIX)?;
    let corrupted = || {
        let (rest, random) = stem.rsplit_once('-')?;
        let (prefix, realtime) = rest.rsplit_once('@')?;
        (!prefix.is_empty()).then_some(())?;
        counter(random)?;
        Some(Named::Corrupted {
            realtime: counter(realtime)?,
        })
    };
    Some(corrupted().unwrap_or(Named::Active))
}

/// Whether `name` is that of a machine's directory of the journal.
fn is_machine_directory(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.len() == MACHINE_ID_DIGITS
            && name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// `instant` in microseconds since the clock's origin; 0 before it, older
/// than any limit, as it is.
fn microseconds(instant: SystemTime) -> u128 {
    instant
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros())
}

/// The room a file takes on disk, as systemd counts it.
fn usage(status: &EntryStatus) -> u64 {
    status.allocated.unwrap_or(status.size)
}

/// A journal to remove, as the plan read it.
#[derive(Debug)]
struct Removal {
    name: OsString,
    status: EntryStatus,
    usage: u64,
}

/// The local machine's directory of the journal, open, and what goes from
/// it.
#[derive(Debug)]
struct Machine {
    path: PathBuf,
    directory: Directory,
    removals: Vec<Removal>,
}

/// What a vacuum would remove, read now; and what it could not read.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    machine: Option<Machine>,
    pub(crate) issues: Vec<ScanIssue>,
}

impl Plan {
    fn removals(&self) -> impl Iterator<Item = &Removal> {
        self.machine.iter().flat_map(|machine| &machine.removals)
    }

    /// How many journals go.
    pub(crate) fn entries(&self) -> usize {
        self.removals().count()
    }

    /// The room they take.
    pub(crate) fn bytes(&self) -> u64 {
        self.removals()
            .map(|removal| removal.usage)
            .fold(0, u64::saturating_add)
    }
}

/// The file that names the local machine (`machine-id(5)`).
pub const MACHINE_ID: &str = "/etc/machine-id";

/// The most of [`MACHINE_ID`] read: its identifier, a line break, and room
/// to tell a longer file, which holds no identifier.
const MACHINE_ID_BYTES: usize = 2 * MACHINE_ID_DIGITS;

/// A machine's identifier: 32 lowercase hexadecimal digits, the name of its
/// directory of the journal, never anything that leads elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineId(String);

impl MachineId {
    /// The identifier `bytes` hold, a line break after it allowed.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidData`]: `bytes` hold no identifier.
    pub(crate) fn parse(bytes: &[u8]) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "no machine identifier");
        let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
        let identifier = text.strip_suffix('\n').unwrap_or(text);
        if is_machine_directory(OsStr::new(identifier)) {
            Ok(Self(identifier.to_owned()))
        } else {
            Err(invalid())
        }
    }

    /// The local machine's (`/etc/machine-id`), whose journal alone is
    /// vacuumed, as journalctl does without `--merge`.
    ///
    /// # Errors
    ///
    /// [`MACHINE_ID`] cannot be read, or holds no identifier
    /// ([`io::ErrorKind::InvalidData`]).
    pub fn local() -> io::Result<Self> {
        Self::parse(&okapi_platform::read_regular_capped(
            Path::new(MACHINE_ID),
            MACHINE_ID_BYTES,
        )?)
    }

    fn as_os_str(&self) -> &OsStr {
        OsStr::new(&self.0)
    }
}

/// Why a vacuum of the journal at `root` would refuse whatever it is asked,
/// as anyone may tell beforehand, without the rights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unfit {
    /// The system does not tell the roots of mounts: root never removes
    /// where a bind mount cannot be told from a folder.
    MountsUntold,
    /// A link lies on the way to the journal, which root never follows.
    LinkOnTheWay,
}

/// Why the vacuum of the journal at `root` would refuse whatever it is
/// asked, if anyone may tell beforehand: a line offering it would wait for
/// rights that change nothing. `None` when only root's plan can tell. For
/// the user's side only: it resolves `root`, which root never does.
#[must_use]
pub fn unfit(root: &Path) -> Option<Unfit> {
    if !okapi_platform::tells_mount_roots() {
        return Some(Unfit::MountsUntold);
    }
    // Absent or unreadable: root's plan says.
    match fs::canonicalize(root) {
        Ok(real) if real != root => Some(Unfit::LinkOnTheWay),
        Ok(_) | Err(_) => None,
    }
}

/// What a vacuum of the journal at `root` (the rule `origin`'s path) would
/// remove now from the directory of the local `machine`, with the age and
/// exclusions of `options`: nothing when the journal or the machine's
/// directory is absent, or that directory excluded; an issue when the
/// journal is excluded, the machine unknown (why it could not be read), or
/// a directory cannot be read safely.
pub(crate) fn plan(
    root: &Path,
    machine: Result<&MachineId, io::ErrorKind>,
    origin: &Origin,
    options: &ScanOptions,
    limits: Limits,
) -> Plan {
    let _span = tracing::info_span!(
        "vacuum_plan",
        ?root,
        machine_known = machine.is_ok(),
        ?limits,
        now = ?options.now(),
        cutoff = ?options.cutoff(None)
    )
    .entered();
    let mut plan = Plan::default();
    if options.exclusions().excludes(root) {
        tracing::info!("the journal is excluded: nothing goes");
        plan.issues.push(ScanIssue::Excluded {
            origins: vec![origin.clone()],
            path: root.to_path_buf(),
        });
        return plan;
    }
    // As `unfit` says, but never resolving a path as root (invariant 3):
    // a link on the way, `reach_path` refuses below.
    if !okapi_platform::tells_mount_roots() {
        tracing::warn!("the system does not tell the roots of mounts: nothing goes");
        plan.issues.push(ScanIssue::UnreadableRoot {
            origins: vec![origin.clone()],
            path: root.to_path_buf(),
            error: io::ErrorKind::Unsupported,
        });
        return plan;
    }
    let machine = match machine {
        Ok(machine) => machine,
        Err(error) => {
            tracing::warn!(?error, "the local machine unknown: nothing goes");
            plan.issues.push(ScanIssue::UnreadableRoot {
                origins: vec![origin.clone()],
                path: PathBuf::from(MACHINE_ID),
                error,
            });
            return plan;
        }
    };
    let path = root.join(machine.as_os_str());
    if options.exclusions().excludes(&path) {
        tracing::info!(?path, "the machine's journal is excluded: nothing goes");
        return plan;
    }
    let Some(directory) = machine_directory(root, machine, &path, origin, &mut plan.issues) else {
        return plan;
    };
    let age_limit = options.now().checked_sub(limits.max_age).map(microseconds);
    match candidates(&directory, &path) {
        Ok((used, candidates)) => {
            let removals = chosen(candidates, used, &path, age_limit, options, limits);
            plan.machine = Some(Machine {
                path,
                directory,
                removals,
            });
        }
        Err(error) => plan.issues.push(unreadable(origin, &path, &error)),
    }
    tracing::info!(
        entries = plan.entries(),
        bytes = plan.bytes(),
        issues = plan.issues.len(),
        "vacuum planned"
    );
    plan
}

/// The directory of `machine` at `path` below the journal at `root`,
/// reached from `/` without a link, never a third party's: `None` when it
/// is absent, or refused or unreadable (said in `issues`).
fn machine_directory(
    root: &Path,
    machine: &MachineId,
    path: &Path,
    origin: &Origin,
    issues: &mut Vec<ScanIssue>,
) -> Option<Directory> {
    let unsafe_root = |path: &Path| ScanIssue::UnsafeRoot {
        origins: vec![origin.clone()],
        path: path.to_path_buf(),
        reason: unsafe_reason(path),
    };
    let root_status = match reach_path(root).map(|reached| reached.map(|root| root.status())) {
        Ok(Some(Ok(status))) => status,
        Ok(None) => {
            tracing::info!("no journal");
            return None;
        }
        Ok(Some(Err(error))) | Err(Unreached::Io(error)) => {
            issues.push(unreadable(origin, root, &error));
            return None;
        }
        Err(Unreached::Changed) => {
            tracing::warn!("the journal's path is not a folder of its own");
            issues.push(unsafe_root(root));
            return None;
        }
    };
    let remover = okapi_platform::effective_user();
    let reached = reach_with(
        root,
        root_status.identity,
        &[machine.as_os_str()],
        |_, entry| !is_third_party(remover, Some(root_status.owner), Some(entry.owner)),
    );
    match reached {
        Ok(Some(Reached { directory, .. })) => Some(directory),
        Ok(None) => {
            tracing::info!(?path, "no journal of the local machine");
            None
        }
        Err(Unreached::Changed) => {
            tracing::warn!(
                ?path,
                root_owner = root_status.owner,
                "the machine's directory refused: a link, a third party's, another mount, or replaced"
            );
            issues.push(unsafe_root(path));
            None
        }
        Err(Unreached::Io(error)) => {
            issues.push(unreadable(origin, path, &error));
            None
        }
    }
}

/// The journals of the machine's `directory` at `path` that may go, each
/// where it sorts, and the room all its journals take, active ones included.
fn candidates(directory: &Directory, path: &Path) -> io::Result<(u64, Vec<(Order, Removal)>)> {
    let mut used: u64 = 0;
    let mut candidates = Vec::new();
    for name in directory.names()? {
        let Some(named) = named(&name) else {
            let bytes = name.as_encoded_bytes();
            if [ARCHIVED_SUFFIX, CORRUPTED_SUFFIX]
                .iter()
                .any(|suffix| bytes.ends_with(suffix.as_bytes()))
            {
                tracing::warn!(
                    ?path,
                    ?name,
                    "a journal of a name not in UTF-8: left, not counted"
                );
            } else {
                tracing::debug!(?name, "no journal: left");
            }
            continue;
        };
        let status = match directory.status_of(&name) {
            Ok(status) => status,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(?path, ?name, "a journal gone meanwhile");
                continue;
            }
            Err(error) => {
                tracing::warn!(?path, ?name, kind = ?error.kind(), code = error.raw_os_error(), %error, "a journal unreadable: left, not counted");
                continue;
            }
        };
        if status.kind != EntryType::File {
            tracing::warn!(?path, ?name, kind = ?status.kind, "a journal that is no plain file: left, not counted");
            continue;
        }
        let usage = usage(&status);
        used = used.saturating_add(usage);
        let (realtime, sequence) = match named {
            Named::Active => {
                tracing::debug!(?name, usage, "an active journal: counted, kept");
                continue;
            }
            Named::Archived {
                sequence_id,
                sequence_number,
                realtime,
            } => (realtime, Some((sequence_id, sequence_number))),
            Named::Corrupted { realtime } => (realtime, None),
        };
        // A file's own dates (modification, change) may say it older than
        // its name; systemd reads its access and birth times too.
        let realtime = [status.modified, status.changed]
            .into_iter()
            .map(microseconds)
            .fold(u128::from(realtime), u128::min);
        candidates.push((
            Order {
                realtime,
                sequence,
                name: name.clone(),
            },
            Removal {
                name,
                status,
                usage,
            },
        ));
    }
    candidates.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok((used, candidates))
}

/// Those of `candidates`, sorted, that go from the machine's directory at
/// `path`, which takes `used` bytes, in the order they go.
fn chosen(
    candidates: Vec<(Order, Removal)>,
    mut used: u64,
    path: &Path,
    age_limit: Option<u128>,
    options: &ScanOptions,
    limits: Limits,
) -> Vec<Removal> {
    let cutoff = options.cutoff(None);
    let used_before = used;
    let candidates_count = candidates.len();
    // Once an archive stays, the size takes no newer one in its place:
    // systemd would have removed it, and stopped sooner.
    let mut size_held = false;
    let mut removals = Vec::new();
    for (index, (order, removal)) in candidates.into_iter().enumerate() {
        let aged = age_limit.is_some_and(|limit| order.realtime < limit);
        let over = !size_held && used > limits.max_use;
        let file = path.join(&removal.name);
        let last_change = removal.status.last_change;
        if !aged && !over {
            let left = candidates_count - index;
            if size_held && used > limits.max_use {
                tracing::info!(
                    ?file,
                    realtime = order.realtime,
                    used,
                    left,
                    "the archives left stay: an older one was kept"
                );
            } else {
                tracing::info!(
                    ?file,
                    realtime = order.realtime,
                    used,
                    left,
                    "the archives left are within the limits"
                );
            }
            break;
        }
        // Immutable or append-only: the system would refuse it.
        if removal.status.locked {
            tracing::info!(?file, "an archive locked: kept, counted");
            size_held = true;
            continue;
        }
        if cutoff.is_none_or(|cutoff| last_change > cutoff) {
            tracing::info!(
                ?file,
                ?last_change,
                ?cutoff,
                "an archive changed too recently: kept, counted"
            );
            size_held = true;
            continue;
        }
        if options.exclusions().excludes(&file) {
            tracing::info!(?file, "an archive excluded: kept, counted");
            size_held = true;
            continue;
        }
        tracing::info!(
            ?file,
            realtime = order.realtime,
            usage = removal.usage,
            aged,
            over,
            "an archive goes"
        );
        used = used.saturating_sub(removal.usage);
        removals.push(removal);
    }
    tracing::info!(
        ?path,
        used_before,
        used_after = used,
        goes = removals.len(),
        "machine's journal planned"
    );
    removals
}

/// Why `path` was refused on the way, read without following it.
fn unsafe_reason(path: &Path) -> UnsafeReason {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => UnsafeReason::Link,
        Ok(metadata) if !metadata.is_dir() => UnsafeReason::NotADirectory,
        _ => UnsafeReason::Replaced,
    }
}

fn unreadable(origin: &Origin, path: &Path, error: &io::Error) -> ScanIssue {
    tracing::warn!(?path, kind = ?error.kind(), code = error.raw_os_error(), %error, "a folder of the journal unreadable");
    ScanIssue::UnreadableRoot {
        origins: vec![origin.clone()],
        path: path.to_path_buf(),
        error: error.kind(),
    }
}

/// Removes what `plan` holds, each journal read again just before (the same
/// file, unchanged, still older than `cutoff`); `watch` cancels what is
/// left. One kept here (changed since the plan, refused by the system) holds
/// none of the others back, as systemd does: the plan already kept what it
/// could tell would stay. Each outcome with its size as planned.
pub(crate) fn clean(
    plan: Plan,
    cutoff: Option<SystemTime>,
    watch: &Watch,
) -> Vec<(HistoryOutcome, u64)> {
    let _span = tracing::info_span!("vacuum_clean", ?cutoff).entered();
    let mut outcomes = Vec::new();
    if let Some(machine) = plan.machine {
        for removal in machine.removals {
            let path = machine.path.join(&removal.name);
            let result = if watch.is_cancelled() {
                CleanResult::Kept(KeepReason::Cancelled)
            } else {
                removed(&machine.directory, &removal, cutoff)
            };
            match result {
                CleanResult::Removed { bytes } => tracing::info!(?path, bytes, "journal removed"),
                CleanResult::Gone => tracing::info!(?path, "journal gone already"),
                other => tracing::warn!(?path, result = ?other, "journal kept"),
            }
            outcomes.push((
                HistoryOutcome::of_result(Some(&path), result, None),
                removal.usage,
            ));
        }
    }
    outcomes
}

/// Removes the journal of `removal` from `directory` if it is still the one
/// planned and old enough.
fn removed(directory: &Directory, removal: &Removal, cutoff: Option<SystemTime>) -> CleanResult {
    let failed = |error: io::Error| match error.kind() {
        io::ErrorKind::NotFound => CleanResult::Gone,
        kind => CleanResult::Kept(KeepReason::Failed {
            kind,
            code: error.raw_os_error(),
        }),
    };
    let status = match directory.status_of(&removal.name) {
        Ok(status) => status,
        Err(error) => return failed(error),
    };
    if status.kind != EntryType::File || status.identity != removal.status.identity {
        return CleanResult::Kept(KeepReason::Replaced);
    }
    // Its size too: two writes within one tick of the clock leave the same
    // date.
    if status.last_change != removal.status.last_change
        || status.size != removal.status.size
        || cutoff.is_none_or(|cutoff| status.last_change > cutoff)
    {
        return CleanResult::Kept(KeepReason::Recent);
    }
    match directory
        .ensure_removable(&status)
        .and_then(|()| directory.remove_file(&removal.name))
    {
        Ok(()) => CleanResult::Removed {
            bytes: removal.usage,
        },
        Err(error) => failed(error),
    }
}

// The vacuum removes through directory descriptors, on Linux.
#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;
