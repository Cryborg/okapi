use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use okapi_engine::{CleanOutcome, CleanReceipt, CleanResult, KeepReason};
use okapi_rules::CleanupAction;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{StoreError, file};

mod lines;
mod rotation;

pub use lines::{Deletions, HistoryRead, HistoryRecord};

/// How many times the history is opened, at most, when each opening finds
/// it set aside before its lock.
const OPEN_ATTEMPTS: u32 = 3;
/// How many times the shared lock of the history is tried, and the wait
/// between two tries: a run setting it aside holds it for a moment only.
const LOCK_ATTEMPTS: u32 = 50;
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

const HISTORY_FILE: &str = "history.jsonl";
/// The format of each history line.
const HISTORY_VERSION: u32 = 1;
/// A bound on what is read, on the caller's thread, to find the last
/// cleaning: its line and the uninstallations after it are short, its
/// deletion log is on a line of its own before it, never read. More (a link
/// to another file, a line of a deletion log whose cleaning line is missing,
/// a cleaning line written by an earlier Okapi with its deletion log in it)
/// is not read, which is traced. The lines of updates installed after it
/// count too, one entry per package each: enough of them without a
/// cleaning in between say no last cleaning.
const MAX_LINE: u64 = 1024 * 1024;
/// The blocks the history is read by from its end, none of them kept.
const BLOCK: u64 = 64 * 1024;

/// One cleaning, as kept in the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// Seconds since the clock's origin, negative before it.
    pub time: i64,
    /// What kept the user's part of the cleaning from touching anything
    /// (root's part says its own in `admin`).
    pub stops: Vec<String>,
    pub removed_bytes: u64,
    pub removed: usize,
    pub partial: usize,
    pub gone: usize,
    pub kept: usize,
    pub complete: bool,
    /// The part of the cleaning that needed the administrator, when there
    /// was one; absent otherwise, so a line of the user's alone is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin: Option<AdminRecord>,
    /// The operations that removed no file of their own (Docker's build
    /// cache pruned), when there were some; absent otherwise. Their bytes
    /// are in `removed_bytes`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<OperationRecord>,
    /// Every entry, when the user keeps a deletion log: written on a line
    /// of its own (`kind: "deletions"`, the same `time`) just before the
    /// cleaning's, so that the cleaning's stays short to read back however
    /// many entries there are. Read back, it is `None`: the deletion log
    /// read comes beside it ([`HistoryRecord::Cleaning`]).
    #[serde(skip)]
    pub entries: Option<Vec<HistoryOutcome>>,
}

/// The deletion log of a cleaning, on the line before the cleaning's.
#[derive(Serialize)]
struct DeletionLog<'a> {
    time: i64,
    entries: &'a [HistoryOutcome],
}

/// What became of an operation of a cleaning that removes no file of its
/// own, a daemon doing the work (Docker's build cache pruned).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecord {
    /// Its rule's operation, by its name in the rules (`docker-build-prune`).
    pub action: CleanupAction,
    #[serde(flatten)]
    pub end: OperationEnd,
}

/// How an operation of a cleaning ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "end", rename_all = "camelCase")]
pub enum OperationEnd {
    /// It ran to its end: how many entries it removed, and their room.
    Done { entries: usize, bytes: u64 },
    /// It removed nothing: it was not asked, or the daemon refused before
    /// anything ran.
    NotDone { reason: OperationReason },
    /// It was asked and no answer said what it did: what it removed is
    /// unknown.
    Unknown { reason: OperationReason },
}

impl OperationEnd {
    /// Whether what it removed is unknown, and so what the cleaning freed.
    #[must_use]
    pub const fn leaves_freed_unknown(self) -> bool {
        matches!(self, Self::Unknown { .. })
    }

    /// Whether it may have removed something: room or entries it says, or
    /// an end no answer said.
    #[must_use]
    pub const fn may_have_removed(self) -> bool {
        match self {
            Self::Done { entries, bytes } => entries > 0 || bytes > 0,
            Self::Unknown { .. } => true,
            Self::NotDone { .. } => false,
        }
    }
}

/// Why an operation of a cleaning did not end as asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationReason {
    /// The cleaning was cancelled: before the daemon was asked, or while it
    /// worked.
    Cancelled,
    /// The daemon does not run.
    Absent,
    /// The daemon refuses the user.
    NotAllowed,
    /// The daemon could not be reached otherwise.
    Unreached,
    /// The daemon refused the operation.
    Refused,
    /// The daemon failed or fell silent once asked.
    Unanswered,
    /// The daemon's answer could not be read.
    Unreadable,
}

/// What became of the part of a cleaning that needed the administrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminRecord {
    /// It ran to its end over all it was asked for and said what became of
    /// each entry. `false`: it did not (it stopped, was cancelled even after
    /// removing entries, or part of what it was asked for was left),
    /// `reason` says why; its outcomes, if any, say what it touched, and
    /// when it gave none what it removed may still be unknown (its traces in
    /// the journal say).
    pub done: bool,
    /// Why it was not done, by a stable name of the caller's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What became of one entry, in the form scripts and the history read, and
/// the privileged helper answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryOutcome {
    pub path: Option<String>,
    /// `removed`, `partial`, `gone` or `kept` (`CleanResult::key`).
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// `KeepReason::key`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    /// A name in `path` or `blocked` is not UTF-8: its bad bytes were
    /// replaced, so the text is not exactly the path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inexact: bool,
}

impl HistoryEntry {
    /// The line of a cleaning that ended at `now`, with every entry (its
    /// deletion log) when the user keeps one.
    #[must_use]
    pub(crate) fn of(receipt: &CleanReceipt, now: SystemTime, deletion_log: bool) -> Self {
        Self::of_outcomes(
            receipt
                .stops()
                .iter()
                .map(|stop| stop.key().to_owned())
                .collect(),
            receipt
                .outcomes()
                .iter()
                .map(|outcome| (outcome.result(), HistoryOutcome::of(outcome)))
                .collect(),
            None,
            now,
            deletion_log,
        )
    }

    /// The line of a cleaning that ended at `now`, made of parts that may
    /// come from several processes: what stopped the user's part (`stops`),
    /// what became of each entry (counted on its result, never read back from
    /// its names), and the administrator's part, if any, which leaves the
    /// cleaning not complete when it was not done.
    #[must_use]
    pub fn of_outcomes(
        stops: Vec<String>,
        outcomes: Vec<(CleanResult, HistoryOutcome)>,
        admin: Option<AdminRecord>,
        now: SystemTime,
        deletion_log: bool,
    ) -> Self {
        let (results, outcomes): (Vec<CleanResult>, Vec<HistoryOutcome>) =
            outcomes.into_iter().unzip();
        let count = |wanted: fn(&CleanResult) -> bool| {
            results.iter().filter(|result| wanted(result)).count()
        };
        Self {
            time: seconds(now),
            removed_bytes: results
                .iter()
                .map(|result| result.removed_bytes())
                .fold(0, u64::saturating_add),
            removed: count(|result| matches!(result, CleanResult::Removed { .. })),
            partial: count(|result| matches!(result, CleanResult::Partial { .. })),
            gone: count(|result| matches!(result, CleanResult::Gone)),
            kept: count(|result| matches!(result, CleanResult::Kept(_))),
            complete: stops.is_empty()
                && admin.as_ref().is_none_or(|admin| admin.done)
                && results.iter().all(|result| {
                    matches!(result, CleanResult::Removed { .. } | CleanResult::Gone)
                }),
            stops,
            admin,
            operations: Vec::new(),
            entries: deletion_log.then_some(outcomes),
        }
    }

    /// The line with `operations` too: their bytes counted as removed, the
    /// cleaning complete only when each was done.
    #[must_use]
    pub fn with_operations(mut self, operations: Vec<OperationRecord>) -> Self {
        self.removed_bytes = operations
            .iter()
            .filter_map(|operation| match operation.end {
                OperationEnd::Done { bytes, .. } => Some(bytes),
                OperationEnd::NotDone { .. } | OperationEnd::Unknown { .. } => None,
            })
            .fold(self.removed_bytes, u64::saturating_add);
        self.complete &= operations
            .iter()
            .all(|operation| matches!(operation.end, OperationEnd::Done { .. }));
        self.operations = operations;
        self
    }

    /// What it freed; unknown when root's part did not end, or an operation
    /// ended unknown, as what they removed may then be missing.
    #[must_use]
    pub fn freed(&self) -> Option<u64> {
        freed(self.removed_bytes, self.admin.as_ref(), &self.operations)
    }
}

/// What a cleaning that removed `removed_bytes` freed, as
/// [`HistoryEntry::freed`] says it.
fn freed(
    removed_bytes: u64,
    admin: Option<&AdminRecord>,
    operations: &[OperationRecord],
) -> Option<u64> {
    let known = admin.is_none_or(|admin| admin.done)
        && !operations
            .iter()
            .any(|operation| operation.end.leaves_freed_unknown());
    known.then_some(removed_bytes)
}

impl HistoryOutcome {
    /// What became of `outcome`, as scripts and the history read it.
    #[must_use]
    pub fn of(outcome: &CleanOutcome) -> Self {
        Self::of_result(outcome.path(), outcome.result(), outcome.blocked())
    }

    /// What became of the entry at `path` (none when it is unknown):
    /// `result`, `blocked` naming what kept a directory whole; also for what
    /// the engine did not remove itself (the privileged helper's vacuum of
    /// systemd's journal).
    #[must_use]
    pub fn of_result(path: Option<&Path>, result: CleanResult, blocked: Option<&Path>) -> Self {
        let inexact = [path, blocked]
            .into_iter()
            .flatten()
            .any(|path| path.to_str().is_none());
        if inexact {
            tracing::warn!(?path, "a name not in UTF-8 is recorded inexactly");
        }
        let lossy = |path: &Path| path.to_string_lossy().into_owned();
        let (bytes, reason) = match result {
            CleanResult::Removed { bytes } => (Some(bytes), None),
            CleanResult::Partial { bytes, reason } => (Some(bytes), Some(reason)),
            CleanResult::Gone => (None, None),
            CleanResult::Kept(reason) => (None, Some(reason)),
        };
        let (error, code) = match reason {
            Some(KeepReason::Failed { kind, code }) => (Some(format!("{kind:?}")), code),
            _ => (None, None),
        };
        Self {
            path: path.map(lossy),
            result: result.key().to_owned(),
            bytes,
            reason: reason.map(|reason| reason.key().to_owned()),
            error,
            code,
            blocked: blocked.map(lossy),
            inexact,
        }
    }

    /// The result it says, read back from its stable names; `None` when they
    /// name none.
    #[must_use]
    pub fn clean_result(&self) -> Option<CleanResult> {
        let reason = match &self.reason {
            Some(key) => Some(KeepReason::from_key(key, self.code)?),
            None => None,
        };
        CleanResult::from_key(&self.result, self.bytes, reason)
    }

    /// What a result that cannot be read back is counted as: an entry the
    /// system kept for an unknown reason, never one of another report.
    pub const UNREADABLE: CleanResult = CleanResult::Kept(KeepReason::Failed {
        kind: io::ErrorKind::Other,
        code: None,
    });

    /// The result another process said, as the history and the receipt
    /// count it: one that cannot be read back is [`Self::UNREADABLE`],
    /// traced.
    #[must_use]
    pub fn clean_result_or_refused(&self) -> CleanResult {
        self.clean_result().unwrap_or_else(|| {
            tracing::error!(outcome = ?self, "a result that cannot be read back: counted kept");
            Self::UNREADABLE
        })
    }
}

/// `instant` in seconds since the clock's origin, negative before it.
#[must_use]
pub fn seconds(instant: SystemTime) -> i64 {
    match instant.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => {
            i64::try_from(before.duration().as_secs()).map_or(i64::MIN, |seconds| -seconds)
        }
    }
}

/// Why a cleaning has no history to add its line to.
#[derive(Debug, Error)]
pub enum HistoryUnavailable {
    #[error("no state directory is known")]
    NoDirectory,

    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The history a cleaning adds its line to, or why there is none.
#[derive(Debug)]
pub enum Opened {
    Open(History),
    /// The cleaning goes on, its line lost: the caller says so.
    Unavailable(HistoryUnavailable),
}

/// One application uninstalled, as kept in the history beside the
/// cleanings: its line says its kind, a cleaning's does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallEntry {
    /// Seconds since the clock's origin, negative before it.
    pub time: i64,
    pub source: PackageSource,
    /// The application, as the user saw it.
    pub name: String,
    /// What went, by the names its source gives (packages, a snap, a
    /// flatpak and its extensions).
    pub packages: Vec<String>,
    /// What it freed, as estimated before; absent when a size was not
    /// known, or a part stayed or is not known freed yet. Never
    /// `removedBytes`: a reader that knows no kind would take the line for a
    /// cleaning's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freed_bytes: Option<u64>,
    /// Its data went with it.
    pub data_removed: bool,
    /// What of it went: the application, or what it no longer uses; absent
    /// for an application, so its line is unchanged.
    #[serde(default, skip_serializing_if = "UninstallPart::is_app")]
    pub part: UninstallPart,
    /// What its source gives back later, beside `freed_bytes`: an old
    /// version of a snap whose file snapd's download cache still holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub later_bytes: Option<u64>,
}

/// What an uninstallation removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UninstallPart {
    /// An application, and what goes with it.
    #[default]
    App,
    /// An old version a snap kept to go back to.
    OldVersion,
    /// A flatpak runtime or extension no application uses.
    UnusedRuntime,
}

impl UninstallPart {
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's skip_serializing_if passes a reference"
    )]
    const fn is_app(&self) -> bool {
        matches!(self, Self::App)
    }
}

/// Who removed an application, or updated packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageSource {
    Deb,
    Snap,
    Flatpak,
}

/// One installation of updates (the system's, or snaps), as kept in the
/// history beside the cleanings: its line says its kind. Written once the
/// list was read again, so that what each update came to is known. It
/// says no bytes: a reader that knows no kind would take the line for a
/// cleaning's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEntry {
    /// When it ended, in seconds since the clock's origin, negative before
    /// it.
    pub time: i64,
    /// Deb or snap: flatpak's updates are not read yet.
    pub source: PackageSource,
    /// Every update asked, in the order asked.
    pub packages: Vec<UpdatedPackage>,
    /// Every update asked is known to have gone (whatever its daemon said:
    /// the list read again decides).
    pub done: bool,
    /// Why it was not done, by a stable name of the caller's, said once for
    /// all when none is known to have gone; absent when some went, the
    /// others then saying why themselves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The system asked to be restarted to finish it.
    pub restart_required: bool,
}

/// An update asked, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatedPackage {
    /// As its source names it.
    pub name: String,
    /// The version it was to be updated to.
    pub version: String,
    /// The version it was to replace, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    /// It is known to have gone. Not done is not always not installed: its
    /// reason may say its fate is not known (the caller's name for it).
    pub done: bool,
    /// Why it is not done, by a stable name of the caller's: when its line
    /// does not say it for all, or when its fate is not known (said even
    /// beside the line's reason).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The kind a line says: a cleaning's says none, and one unknown makes the
/// line unreadable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LineKind {
    Uninstall,
    Update,
    /// The deletion log of the cleaning on the next line.
    Deletions,
}

#[derive(Serialize)]
struct Line<'a, T> {
    version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<LineKind>,
    #[serde(flatten)]
    entry: &'a T,
}

/// The history file, open for appending: opened before a cleaning so that a
/// history that cannot be kept is known before anything is removed.
#[derive(Debug)]
pub struct History {
    file: File,
    path: PathBuf,
    /// It holds its shared lock: never set aside under it.
    locked: bool,
}

impl History {
    /// Opens the history in `directory` (created private if needed), and
    /// holds a shared lock on it for as long as it is open: the history is
    /// never set aside under it ([`Self::open_for_cleaning`]). Where the
    /// system refuses the lock, it looks before each write that the history
    /// is still its file, and refuses to write to one set aside.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unwritable`], also when the history stays held whole
    /// by a run setting it aside, or is set aside at each opening.
    pub fn open(directory: &Path) -> Result<Self, StoreError> {
        Self::open_locked(directory, okapi_platform::try_lock_journal_shared)
    }

    /// [`Self::open`], its shared lock tried with `try_lock`.
    fn open_locked(
        directory: &Path,
        try_lock: impl Fn(&File) -> io::Result<bool>,
    ) -> Result<Self, StoreError> {
        let path = Self::path(directory);
        let unwritable = |error| StoreError::Unwritable {
            path: path.clone(),
            error,
        };
        okapi_platform::create_private_directory(directory).map_err(unwritable)?;
        // Locked shared, then looked at: a history set aside between its
        // opening and the lock is opened again, so a line never goes to the
        // one set aside, which may be replaced.
        for _ in 0..OPEN_ATTEMPTS {
            let file = okapi_platform::open_journal(&path).map_err(unwritable)?;
            let locked = lock_shared(&file, &path, &try_lock)?;
            if rotation::still_named(&file, &path).map_err(unwritable)? {
                return Ok(Self { file, path, locked });
            }
            tracing::info!(path = %path.display(), "history set aside meanwhile: opened again");
        }
        tracing::error!(path = %path.display(), attempts = OPEN_ATTEMPTS, "history set aside at each opening");
        // Busy, as the user is told: never a system's error.
        Err(unwritable(io::ErrorKind::ResourceBusy.into()))
    }

    /// Adds the line of `receipt`, a cleaning that ended now, after the line
    /// of its deletion log when the user keeps one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unwritable`], traced.
    pub fn record_cleaning(
        &mut self,
        receipt: &CleanReceipt,
        deletion_log: bool,
    ) -> Result<(), StoreError> {
        self.record(&HistoryEntry::of(receipt, SystemTime::now(), deletion_log))
            .inspect_err(|error| tracing::error!(%error, deletion_log, "history not recorded"))
    }

    /// Opens the history before a cleaning, in `directory` (none when no
    /// state directory is known). A history grown beyond what is kept is
    /// first set aside as `history.1.jsonl`, the one set aside before
    /// replaced, its lines lost; one that cannot be set aside stays as it
    /// is, traced. The deletion log the user may ask for is never lost: then
    /// a history that cannot be kept is the error, and nothing must be
    /// removed.
    ///
    /// # Errors
    ///
    /// [`HistoryUnavailable`], with a deletion log asked for only.
    pub fn open_for_cleaning(
        directory: Option<&Path>,
        deletion_log: bool,
    ) -> Result<Opened, HistoryUnavailable> {
        // Before a cleaning only: its line follows, and the last cleaning is
        // looked for in the history set aside too.
        if let Some(directory) = directory {
            rotation::rotate_if_grown(directory, rotation::ROTATION_SIZE).trace(directory);
        }
        let unavailable = match directory.map(Self::open) {
            Some(Ok(history)) => return Ok(Opened::Open(history)),
            Some(Err(error)) => HistoryUnavailable::Store(error),
            None => HistoryUnavailable::NoDirectory,
        };
        tracing::warn!(error = %unavailable, deletion_log, "history unavailable");
        if deletion_log {
            tracing::error!("the deletion log asked for cannot be kept: nothing is removed");
            return Err(unavailable);
        }
        Ok(Opened::Unavailable(unavailable))
    }

    /// The file the history of `directory` lives in.
    #[must_use]
    pub fn path(directory: &Path) -> PathBuf {
        directory.join(HISTORY_FILE)
    }

    /// The file the history of `directory` is set aside as.
    pub(crate) fn rotated_path(directory: &Path) -> PathBuf {
        rotation::rotated_path(directory)
    }

    /// The last cleaning the history of `directory` keeps, read from its
    /// last line, past the lines of the applications uninstalled and the
    /// updates installed after it:
    /// `None` when there is none, or when the line met does not say one (a
    /// write cut short, another version or kind, too long), traced at each
    /// reading. A line before one that cannot be read, or before a deletion
    /// log whose cleaning line is missing, is never taken for the last
    /// cleaning: a cleaning may have come after. A history of
    /// uninstallations and updates alone, or none, is read on into the one
    /// set aside before it. All the lines read are bounded together as one
    /// line is.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unreadable`] (a file failing to be read, the line
    /// included), [`StoreError::BrokenLink`] or [`StoreError::NotAFile`],
    /// the history set aside's as the history's.
    pub fn last_cleaning(directory: &Path) -> Result<Option<LastCleaning>, StoreError> {
        let path = Self::path(directory);
        let _span = tracing::info_span!("last_cleaning", path = %path.display()).entered();
        // Its start reached past other lines alone (or none): the
        // history set aside before it is read on, within what is left of
        // the bound.
        let room = match file::open_regular(&path)? {
            Some(history) => match find_last_cleaning(&history, &path, MAX_LINE)? {
                Searched::Found(last) => return Ok(last),
                Searched::Start { length } => MAX_LINE.saturating_sub(length),
            },
            None => MAX_LINE,
        };
        let rotated = rotation::rotated_path(directory);
        let Some(previous) = file::open_regular(&rotated)? else {
            tracing::info!("no cleaning line in the history: no last cleaning");
            return Ok(None);
        };
        match find_last_cleaning(&previous, &rotated, room)? {
            Searched::Found(last) => Ok(last),
            Searched::Start { .. } => {
                tracing::info!("no cleaning line in the history: no last cleaning");
                Ok(None)
            }
        }
    }

    /// Every line the history of `directory` keeps, the one set aside's
    /// first, oldest first, each deletion log folded into a `D` entry by
    /// entry as it is read (never held whole unless `D` holds it): none when
    /// there is no history. A line that cannot be read (a write cut short,
    /// another version or kind), or a deletion log whose cleaning line does
    /// not follow it, is skipped, counted and traced, never a reason to
    /// refuse the rest; the history's last line not ended yet (a write under
    /// way) is neither read nor counted, the one set aside's is (cut short).
    ///
    /// # Errors
    ///
    /// [`StoreError::Unreadable`] (a file failing to be read),
    /// [`StoreError::BrokenLink`] or [`StoreError::NotAFile`], the history
    /// set aside's as the history's: a part never passes for the whole.
    pub fn read_all<D: Default + Extend<HistoryOutcome>>(
        directory: &Path,
    ) -> Result<HistoryRead<D>, StoreError> {
        let path = Self::path(directory);
        let _span = tracing::info_span!("history_read", path = %path.display()).entered();
        // The history first: set aside between the two openings, it is the
        // one set aside too, read once.
        let current = file::open_regular(&path)?;
        let rotated = rotation::rotated_path(directory);
        let previous = file::open_regular(&rotated)?.filter(|previous| {
            current
                .as_ref()
                .is_none_or(|current| !same_file(current, previous))
        });
        let mut read = HistoryRead::default();
        for (history, path, set_aside) in [(previous, &rotated, true), (current, &path, false)] {
            if let Some(history) = history {
                let part = lines::read(&history, path, set_aside)?;
                read.records.extend(part.records);
                read.skipped += part.skipped;
            }
        }
        Ok(read)
    }

    /// Adds `entry` as one line, after the line of its deletion log if it
    /// has one, written at once and synced to the disk: `Ok` means the lines
    /// are kept (but for a history file created by this very run, whose
    /// name a power cut may still lose). A last line an interrupted run left
    /// unfinished is ended with a line break first, so an interrupted write
    /// spoils at most its own lines.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unwritable`].
    pub fn record(&mut self, entry: &HistoryEntry) -> Result<(), StoreError> {
        let mut bytes = match &entry.entries {
            Some(entries) => self.line(&Line {
                version: HISTORY_VERSION,
                kind: Some(LineKind::Deletions),
                entry: &DeletionLog {
                    time: entry.time,
                    entries,
                },
            })?,
            None => Vec::new(),
        };
        bytes.extend(self.line(&Line {
            version: HISTORY_VERSION,
            kind: None,
            entry,
        })?);
        self.write(bytes)
    }

    /// Adds the line of `entry`, an application uninstalled, as [`record`]
    /// adds a cleaning's.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unwritable`], traced.
    ///
    /// [`record`]: Self::record
    pub fn record_uninstall(&mut self, entry: &UninstallEntry) -> Result<(), StoreError> {
        self.line(&Line {
            version: HISTORY_VERSION,
            kind: Some(LineKind::Uninstall),
            entry,
        })
        .and_then(|bytes| self.write(bytes))
        .inspect_err(
            |error| tracing::error!(%error, name = %entry.name, "uninstallation not recorded"),
        )
    }

    /// Adds the line of `entry`, an installation of updates, as [`record`]
    /// adds a cleaning's.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unwritable`], traced.
    ///
    /// [`record`]: Self::record
    pub fn record_update(&mut self, entry: &UpdateEntry) -> Result<(), StoreError> {
        self.line(&Line {
            version: HISTORY_VERSION,
            kind: Some(LineKind::Update),
            entry,
        })
        .and_then(|bytes| self.write(bytes))
        .inspect_err(|error| {
            tracing::error!(%error, source = ?entry.source, packages = entry.packages.len(), "update not recorded");
        })
    }

    /// `line`, as written: JSON, then a line break.
    fn line<T: Serialize>(&self, line: &Line<'_, T>) -> Result<Vec<u8>, StoreError> {
        let mut bytes = serde_json::to_vec(line).map_err(|error| StoreError::Unwritable {
            path: self.path.clone(),
            error: io::Error::other(error),
        })?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Adds the lines `bytes` at once, synced to the disk.
    fn write(&mut self, mut bytes: Vec<u8>) -> Result<(), StoreError> {
        // Checked now, not at opening: another run may have been cut short
        // meanwhile.
        let ends_cleanly =
            ends_with_line_break(&self.file).map_err(|error| StoreError::Unwritable {
                path: self.path.clone(),
                error,
            })?;
        if !ends_cleanly {
            tracing::warn!(path = %self.path.display(), "history ended mid-line, closed with a line break");
            bytes.insert(0, b'\n');
        }
        // Without its lock, a run that got its own may have set it aside
        // meanwhile: never written there, which the next setting aside
        // replaces (a moment between this look and the write remains).
        if !self.locked
            && !rotation::still_named(&self.file, &self.path).map_err(|error| {
                StoreError::Unwritable {
                    path: self.path.clone(),
                    error,
                }
            })?
        {
            tracing::error!(path = %self.path.display(), "history set aside under a run without its lock: not written");
            return Err(StoreError::Unwritable {
                path: self.path.clone(),
                error: io::ErrorKind::ResourceBusy.into(),
            });
        }
        self.file
            .write_all(&bytes)
            .map_err(|error| StoreError::Unwritable {
                path: self.path.clone(),
                error,
            })?;
        // Without it, a full disk or a network filesystem would report the
        // failure only at close, which nobody sees.
        self.file
            .sync_data()
            .map_err(|error| StoreError::Unwritable {
                path: self.path.clone(),
                error,
            })?;
        tracing::info!(path = %self.path.display(), bytes = bytes.len(), "history recorded");
        Ok(())
    }
}

/// The last cleaning of `history` (at `path`), as [`History::last_cleaning`]
/// finds it, reading at most `max_line` bytes of lines.
fn find_last_cleaning(history: &File, path: &Path, max_line: u64) -> Result<Searched, StoreError> {
    let started = Instant::now();
    let unreadable = |error| StoreError::Unreadable {
        path: path.to_owned(),
        error,
    };
    // Its length is taken once: a line added meanwhile is not seen.
    let length = history.metadata().map_err(unreadable)?.len();
    let mut until = length;
    let mut scanned = 0;
    let mut skipped = 0;
    loop {
        // The lines passed count against the bound, as one line would.
        let room = max_line.saturating_sub(length - until);
        let (tail, read) = last_line(history, until, BLOCK, room).map_err(unreadable)?;
        scanned += read;
        let (start, end) = match tail {
            Tail::Empty => {
                tracing::info!(
                    path = %path.display(),
                    scanned,
                    skipped,
                    "start of the history reached without a cleaning line"
                );
                return Ok(Searched::Start { length });
            }
            Tail::TooLong => {
                tracing::warn!(scanned, skipped, max = max_line, elapsed = ?started.elapsed(), "history lines too long: not read");
                return Ok(Searched::Found(None));
            }
            Tail::Line { start, end } => (start, end),
        };
        let elapsed = started.elapsed();
        match parse(history, start, end, path) {
            Ok(LineRead::Cleaning(last)) => {
                tracing::info!(
                    offset = start,
                    length = end - start,
                    scanned,
                    skipped,
                    ?elapsed,
                    ?last,
                    "last cleaning read"
                );
                return Ok(Searched::Found(Some(last)));
            }
            Ok(LineRead::Skipped) => {
                skipped += 1;
                until = start;
            }
            Ok(LineRead::Deletions) => {
                tracing::warn!(
                    offset = start,
                    length = end - start,
                    scanned,
                    skipped,
                    ?elapsed,
                    "deletion log without its cleaning line: no last cleaning"
                );
                return Ok(Searched::Found(None));
            }
            Err(error @ (StoreError::Invalid { .. } | StoreError::UnsupportedVersion { .. })) => {
                tracing::warn!(offset = start, length = end - start, scanned, skipped, ?elapsed, %error, "history line unreadable: no last cleaning");
                return Ok(Searched::Found(None));
            }
            Err(error) => return Err(error),
        }
    }
}

/// What looking for the last cleaning in one file found.
#[derive(Debug, PartialEq, Eq)]
enum Searched {
    /// What it says of the last cleaning: one, or none (a line that cannot
    /// be read, a deletion log without its cleaning line, too much to read).
    Found(Option<LastCleaning>),
    /// Its start reached, `length` bytes read, past lines that are not
    /// cleanings alone: the file before it says.
    Start { length: u64 },
}

/// Takes a shared lock on `history` (at `path`), tried for a little while:
/// only a run setting the history aside holds it whole, for a moment.
/// Where the system refuses the lock, the history is written without,
/// traced: whether it holds it. Without it, the file may be set aside by
/// a run that got the lock, which the writer looks for before it writes.
///
/// # Errors
///
/// [`StoreError::Unwritable`] when it stays held whole.
fn lock_shared(
    history: &File,
    path: &Path,
    try_lock: &impl Fn(&File) -> io::Result<bool>,
) -> Result<bool, StoreError> {
    for _ in 0..LOCK_ATTEMPTS {
        match try_lock(history) {
            Ok(true) => return Ok(true),
            Ok(false) => std::thread::sleep(LOCK_WAIT),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, code = error.raw_os_error(), "history lock unavailable: written without");
                return Ok(false);
            }
        }
    }
    tracing::error!(path = %path.display(), waited = ?LOCK_WAIT * LOCK_ATTEMPTS, "history held whole: not opened");
    Err(StoreError::Unwritable {
        path: path.to_owned(),
        error: io::ErrorKind::ResourceBusy.into(),
    })
}

/// Whether `one` and `other` are the same file; never taken to where files
/// have no known identity.
fn same_file(one: &File, other: &File) -> bool {
    let identity = |file: &File| {
        file.metadata()
            .ok()
            .and_then(|metadata| okapi_platform::file_identity(&metadata))
    };
    identity(one).is_some_and(|one| Some(one) == identity(other))
}

/// The last cleaning a history keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastCleaning {
    /// Seconds since the clock's origin, negative before it.
    pub time: i64,
    /// What it freed; unknown when root's part did not end, or an
    /// operation ended unknown, as what they removed may then be missing.
    pub freed: Option<u64>,
}

/// What the last cleaning is read from in a line, in one pass: the rest is
/// skipped. A cleaning's line says no kind and its bytes; any other says
/// its kind, and its time like any.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LastLine {
    kind: Option<LineKind>,
    time: i64,
    removed_bytes: Option<u64>,
    admin: Option<AdminRecord>,
    #[serde(default)]
    operations: Vec<OperationRecord>,
}

/// Where the last line of a history file lies.
#[derive(Debug, PartialEq, Eq)]
enum Tail {
    /// Not one line that is not empty.
    Empty,
    /// The last line is longer than allowed, or more than that was read
    /// without reaching its start.
    TooLong,
    /// From `start` to `end` (excluded), in bytes from the file's start.
    Line { start: u64, end: u64 },
}

/// Where the last line of `file` that is not empty lies, if at most
/// `max_line` bytes long, and how many bytes were read to find it: read
/// from the end by blocks of `block` bytes, none of them kept, so at most
/// `max_line` of the line, as much of line breaks after it, and a block.
/// Only what lies before `until` is read: a line added since it was taken
/// is not seen.
fn last_line(mut file: &File, until: u64, block: u64, max_line: u64) -> io::Result<(Tail, u64)> {
    let in_bytes = |index: usize| u64::try_from(index).map_err(io::Error::other);
    let length = until;
    let block = block.max(1);
    let mut buffer = vec![0; usize::try_from(block).map_err(io::Error::other)?];
    // The file is read from `position` to its end; `end` is where its last
    // line ends, once met.
    let mut position = length;
    let mut end = None;
    let within = |start: u64, end: u64| {
        if end - start > max_line {
            Tail::TooLong
        } else {
            Tail::Line { start, end }
        }
    };
    while position > 0 {
        if end.unwrap_or(length) - position > max_line {
            return Ok((Tail::TooLong, length - position));
        }
        let size = block.min(position);
        position -= size;
        let read = &mut buffer[..usize::try_from(size).map_err(io::Error::other)?];
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(read)?;
        let mut searched = &read[..];
        if end.is_none() {
            let Some(last) = read.iter().rposition(|byte| *byte != b'\n') else {
                continue;
            };
            end = Some(position + in_bytes(last + 1)?);
            searched = &read[..last];
        }
        if let (Some(end), Some(line_break)) =
            (end, searched.iter().rposition(|byte| *byte == b'\n'))
        {
            let start = position + in_bytes(line_break + 1)?;
            return Ok((within(start, end), length - position));
        }
    }
    Ok((end.map_or(Tail::Empty, |end| within(0, end)), length))
}

/// What a line of the history says, for the last cleaning.
#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    Cleaning(LastCleaning),
    /// A line written after the last cleaning that is not one (an
    /// application uninstalled, updates installed): the line before is read.
    Skipped,
    /// A deletion log, whose cleaning line should follow: a write cut short.
    Deletions,
}

/// What the line of `history` from `start` to `end` says, read as it
/// streams by (a deletion log is skipped, never kept), its version checked
/// first: a line of another version is never read as this one's, nor one of
/// a kind unknown as a cleaning's.
fn parse(history: &File, start: u64, end: u64, path: &Path) -> Result<LineRead, StoreError> {
    check_line_version(history, start, end, path)?;
    let read: LastLine = read_line(history, start, end, path)?;
    match read.kind {
        Some(LineKind::Uninstall | LineKind::Update) => return Ok(LineRead::Skipped),
        Some(LineKind::Deletions) => return Ok(LineRead::Deletions),
        None => {}
    }
    let removed_bytes = read
        .removed_bytes
        .ok_or_else(|| file::json_error(path, serde::de::Error::missing_field("removedBytes")))?;
    Ok(LineRead::Cleaning(LastCleaning {
        time: read.time,
        freed: freed(removed_bytes, read.admin.as_ref(), &read.operations),
    }))
}

/// The line of `history` (at `path`) from `start` to `end`, to be read as
/// it streams by: never past its end, so a line added since is not read
/// with it.
fn line_reader<'a>(
    mut history: &'a File,
    start: u64,
    end: u64,
    path: &Path,
) -> Result<BufReader<io::Take<&'a File>>, StoreError> {
    history
        .seek(SeekFrom::Start(start))
        .map_err(|error| StoreError::Unreadable {
            path: path.to_owned(),
            error,
        })?;
    Ok(BufReader::new(history.take(end - start)))
}

/// Checks the version of the line of `history` from `start` to `end` before
/// anything else of it: a line of another version is never read as this
/// one's.
fn check_line_version(history: &File, start: u64, end: u64, path: &Path) -> Result<(), StoreError> {
    file::check_version(
        line_reader(history, start, end, path)?,
        path,
        HISTORY_VERSION,
    )
}

/// What the line of `history` from `start` to `end` says, read as it
/// streams by.
fn read_line<T: serde::de::DeserializeOwned>(
    history: &File,
    start: u64,
    end: u64,
    path: &Path,
) -> Result<T, StoreError> {
    serde_json::from_reader(line_reader(history, start, end, path)?)
        .map_err(|error| file::json_error(path, error))
}

/// Whether `file` is empty or ends with a line break. Its offset moves, which
/// appending ignores.
fn ends_with_line_break(mut file: &File) -> io::Result<bool> {
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn entry(removed: usize, entries: Option<Vec<HistoryOutcome>>) -> HistoryEntry {
        HistoryEntry {
            time: 1_700_000_000,
            stops: vec!["minAgeBeyondClock".to_owned()],
            removed_bytes: 10,
            removed,
            partial: 2,
            gone: 3,
            kept: 4,
            complete: false,
            operations: Vec::new(),
            admin: None,
            entries,
        }
    }

    fn lines(directory: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(directory.join(HISTORY_FILE))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn cleanings_are_appended_one_line_each_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        History::open(&state)
            .unwrap()
            .record(&entry(1, None))
            .unwrap();
        let outcome = HistoryOutcome {
            path: Some("/tmp/x".to_owned()),
            result: "removed".to_owned(),
            bytes: Some(10),
            reason: None,
            error: None,
            code: None,
            blocked: None,
            inexact: false,
        };
        History::open(&state)
            .unwrap()
            .record(&entry(2, Some(vec![outcome])))
            .unwrap();
        let lines = lines(&state);
        assert_eq!(
            lines.len(),
            3,
            "the second's deletion log on a line of its own"
        );
        // The format on disk, which readers rely on: every field named.
        assert_eq!(
            lines[0],
            serde_json::json!({
                "version": 1,
                "time": 1_700_000_000,
                "stops": ["minAgeBeyondClock"],
                "removedBytes": 10,
                "removed": 1,
                "partial": 2,
                "gone": 3,
                "kept": 4,
                "complete": false,
            }),
            "no deletion log, no entries"
        );
        assert_eq!(
            lines[1],
            serde_json::json!({
                "version": 1,
                "kind": "deletions",
                "time": 1_700_000_000,
                "entries": [{"path": "/tmp/x", "result": "removed", "bytes": 10}],
            }),
            "before its cleaning, at its time"
        );
        assert_eq!(lines[2]["removed"], 2);
        assert_eq!(lines[2]["removedBytes"], 10);
        assert_eq!(
            lines[2].get("entries"),
            None,
            "never on the cleaning's line"
        );
    }

    #[test]
    fn a_deletion_log_asked_for_is_written_even_empty() {
        let directory = tempfile::tempdir().unwrap();
        History::open(directory.path())
            .unwrap()
            .record(&entry(0, Some(Vec::new())))
            .unwrap();
        let lines = lines(directory.path());
        assert_eq!(lines[0]["kind"], "deletions");
        assert_eq!(lines[0]["entries"], serde_json::json!([]));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn a_line_cut_short_does_not_spoil_the_next() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(HISTORY_FILE), b"{\"version\":1,\"tim").unwrap();
        History::open(directory.path())
            .unwrap()
            .record(&entry(3, None))
            .unwrap();
        let text = std::fs::read_to_string(directory.path().join(HISTORY_FILE)).unwrap();
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["removed"], 3);
        assert_eq!(
            text.lines().count(),
            2,
            "the cut line stays a line of its own"
        );
    }

    #[test]
    fn a_line_cut_short_after_opening_does_not_spoil_the_next() {
        use std::io::Write as _;
        let directory = tempfile::tempdir().unwrap();
        let mut history = History::open(directory.path()).unwrap();
        // Another run, cut short while this one was scanning.
        std::fs::OpenOptions::new()
            .append(true)
            .open(directory.path().join(HISTORY_FILE))
            .unwrap()
            .write_all(b"{\"version\":1,\"tim")
            .unwrap();
        history.record(&entry(3, None)).unwrap();
        let text = std::fs::read_to_string(directory.path().join(HISTORY_FILE)).unwrap();
        assert_eq!(text.lines().count(), 2);
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["removed"], 3);
    }

    #[cfg(unix)]
    #[test]
    fn the_history_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        History::open(&state).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&state), 0o700);
        assert_eq!(mode(&state.join(HISTORY_FILE)), 0o600);
    }

    /// A history set aside between its opening and its lock, the lock
    /// taken or refused by the system: the run writes to the new one, never
    /// to the one set aside, which the next setting aside replaces.
    #[cfg(unix)]
    #[test]
    fn a_history_set_aside_before_the_lock_is_opened_again() {
        for locks in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let path = History::path(directory.path());
            let rotated = rotation::rotated_path(directory.path());
            History::open(directory.path())
                .unwrap()
                .record(&entry(1, None))
                .unwrap();
            let tries = std::cell::Cell::new(0);
            // As another run setting it aside before the first lock.
            let try_lock = |_: &File| {
                tries.set(tries.get() + 1);
                if tries.get() == 1 {
                    std::fs::rename(&path, &rotated).unwrap();
                }
                if locks {
                    Ok(true)
                } else {
                    Err(io::ErrorKind::Unsupported.into())
                }
            };
            History::open_locked(directory.path(), try_lock)
                .unwrap()
                .record(&HistoryEntry {
                    time: 2,
                    ..entry(2, None)
                })
                .unwrap();
            assert_eq!(tries.get(), 2, "opened again: {locks}");
            let set_aside = std::fs::read_to_string(&rotated).unwrap();
            assert_eq!(set_aside.lines().count(), 1, "{locks}: {set_aside}");
            let new: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(new["removed"], 2, "{locks}");
        }
    }

    /// Written without its lock (refused by the system), the history set
    /// aside by a run that got its own: never written to.
    #[cfg(unix)]
    #[test]
    fn a_history_without_its_lock_is_never_written_once_set_aside() {
        let directory = tempfile::tempdir().unwrap();
        let path = History::path(directory.path());
        let rotated = rotation::rotated_path(directory.path());
        let refused = |_: &File| Err(io::ErrorKind::Unsupported.into());
        let mut history = History::open_locked(directory.path(), refused).unwrap();
        history.record(&entry(1, None)).unwrap();
        std::fs::rename(&path, &rotated).unwrap();
        assert!(matches!(
            history.record(&entry(2, None)),
            Err(StoreError::Unwritable { error, .. }) if error.kind() == io::ErrorKind::ResourceBusy
        ));
        let set_aside = std::fs::read_to_string(&rotated).unwrap();
        assert_eq!(set_aside.lines().count(), 1, "{set_aside}");
    }

    #[cfg(unix)]
    #[test]
    fn a_history_held_whole_or_set_aside_at_each_opening_is_busy() {
        let directory = tempfile::tempdir().unwrap();
        History::open(directory.path()).unwrap();
        let path = History::path(directory.path());
        let held = okapi_platform::open_journal(&path).unwrap();
        assert!(okapi_platform::try_lock_journal_exclusive(&held).unwrap());
        let busy = |opened: Result<History, StoreError>| {
            matches!(
                opened,
                Err(StoreError::Unwritable { error, .. }) if error.kind() == io::ErrorKind::ResourceBusy
            )
        };
        assert!(busy(History::open(directory.path())));
        drop(held);
        History::open(directory.path()).unwrap();
        let rotated = rotation::rotated_path(directory.path());
        let set_aside_each_time = |_: &File| {
            std::fs::rename(&path, &rotated).unwrap();
            Ok(true)
        };
        assert!(busy(History::open_locked(
            directory.path(),
            set_aside_each_time
        )));
    }

    #[test]
    fn a_history_that_cannot_be_opened_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let not_a_directory = directory.path().join("file");
        std::fs::write(&not_a_directory, b"").unwrap();
        assert!(matches!(
            History::open(&not_a_directory),
            Err(StoreError::Unwritable { .. })
        ));
    }

    #[test]
    fn dates_are_signed_seconds() {
        let after = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5);
        let before = SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(5);
        assert_eq!(seconds(after), 5);
        assert_eq!(seconds(before), -5);
    }

    #[test]
    fn a_deletion_log_is_never_lost_to_an_unavailable_history() {
        let directory = tempfile::tempdir().unwrap();
        let not_a_directory = directory.path().join("file");
        std::fs::write(&not_a_directory, b"x").unwrap();
        for deletion_log in [false, true] {
            assert!(matches!(
                History::open_for_cleaning(Some(directory.path()), deletion_log),
                Ok(Opened::Open(_))
            ));
        }
        assert!(matches!(
            History::open_for_cleaning(None, false),
            Ok(Opened::Unavailable(HistoryUnavailable::NoDirectory))
        ));
        assert!(matches!(
            History::open_for_cleaning(Some(&not_a_directory), false),
            Ok(Opened::Unavailable(HistoryUnavailable::Store(_)))
        ));
        assert!(matches!(
            History::open_for_cleaning(None, true),
            Err(HistoryUnavailable::NoDirectory)
        ));
        assert!(matches!(
            History::open_for_cleaning(Some(&not_a_directory), true),
            Err(HistoryUnavailable::Store(_))
        ));
    }

    /// A cleaning stopped before reading: its stops by name, nothing counted.
    /// What becomes of each entry is fixed by the command line's tests.
    #[test]
    fn a_stopped_cleaning_keeps_its_stops_by_name() {
        let rules = okapi_rules::RuleSet::embedded(okapi_rules::Os::Linux).unwrap();
        let variables = okapi_rules::TemplateVariables::new();
        let options = okapi_engine::ScanOptions::new(
            SystemTime::now(),
            okapi_engine::DEFAULT_MIN_AGE,
            okapi_engine::Exclusions::none(),
        );
        let report = okapi_engine::scan(&rules, &variables, &options);
        let receipt = okapi_engine::clean(&report, &[], &rules, &variables, &options);
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(9);
        let line = HistoryEntry::of(&receipt, now, true);
        assert_eq!(line.time, 9);
        assert_ne!(line.stops.len(), 0, "no variable: stopped");
        assert!(
            line.stops.iter().all(|stop| stop == "missingVariable"),
            "{:?}",
            line.stops
        );
        assert_eq!(
            (line.removed, line.partial, line.gone, line.kept),
            (0, 0, 0, 0)
        );
        assert!(!line.complete);
        assert_eq!(line.entries, Some(Vec::new()));
    }

    pub(super) fn outcome(
        result: &str,
        bytes: Option<u64>,
        reason: Option<&str>,
    ) -> HistoryOutcome {
        HistoryOutcome {
            path: Some("/var/crash/core".to_owned()),
            result: result.to_owned(),
            bytes,
            reason: reason.map(str::to_owned),
            error: None,
            code: reason.filter(|reason| *reason == "failed").map(|_| 13),
            blocked: None,
            inexact: false,
        }
    }

    /// A line made of parts from several processes counts what each says.
    #[test]
    fn a_line_counts_the_outcomes_of_every_part() {
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(7);
        let outcomes = vec![
            outcome("removed", Some(5), None),
            outcome("removed", Some(11), None),
            outcome("partial", Some(3), Some("failed")),
            outcome("gone", None, None),
            outcome("kept", None, Some("recent")),
        ];
        // As the caller pairs another process's outcomes with their result.
        let read = |outcomes: Vec<HistoryOutcome>| -> Vec<(CleanResult, HistoryOutcome)> {
            outcomes
                .into_iter()
                .map(|outcome| (outcome.clean_result_or_refused(), outcome))
                .collect()
        };
        let line = HistoryEntry::of_outcomes(Vec::new(), read(outcomes.clone()), None, now, true);
        assert_eq!(
            (line.removed, line.partial, line.gone, line.kept),
            (2, 1, 1, 1)
        );
        assert_eq!(line.removed_bytes, 19);
        assert_eq!(line.time, 7);
        assert!(!line.complete);
        assert_eq!(line.entries, Some(outcomes));

        let done = read(vec![
            outcome("removed", Some(5), None),
            outcome("gone", None, None),
        ]);
        assert!(HistoryEntry::of_outcomes(Vec::new(), done.clone(), None, now, false).complete);
        let stopped =
            HistoryEntry::of_outcomes(vec!["cancelled".to_owned()], done.clone(), None, now, false);
        assert!(!stopped.complete, "a stop: never complete");
        assert_eq!(stopped.stops, ["cancelled"], "the user's part's, by name");
        assert_eq!(stopped.entries, None, "no deletion log, no entries");

        let unreadable = HistoryEntry::of_outcomes(
            Vec::new(),
            read(vec![outcome("vanished", Some(1), None)]),
            None,
            now,
            false,
        );
        assert_eq!(
            (
                unreadable.kept,
                unreadable.removed_bytes,
                unreadable.complete
            ),
            (1, 0, false),
            "kept by the system for an unknown reason"
        );

        let not_done = AdminRecord {
            done: false,
            reason: Some("authDismissed".to_owned()),
        };
        let without_root =
            HistoryEntry::of_outcomes(Vec::new(), done.clone(), Some(not_done), now, false);
        assert!(!without_root.complete, "root's part not done");
        assert_eq!(
            serde_json::to_value(&without_root).unwrap()["admin"],
            serde_json::json!({ "done": false, "reason": "authDismissed" })
        );
        let with_root = AdminRecord {
            done: true,
            reason: None,
        };
        assert!(HistoryEntry::of_outcomes(Vec::new(), done, Some(with_root), now, false).complete);
    }

    /// The history's files keep these names: what reads them relies on it.
    #[test]
    fn each_reason_of_an_operation_is_kept_by_its_name() {
        for (reason, name) in [
            (OperationReason::Cancelled, "cancelled"),
            (OperationReason::Absent, "absent"),
            (OperationReason::NotAllowed, "notAllowed"),
            (OperationReason::Unreached, "unreached"),
            (OperationReason::Refused, "refused"),
            (OperationReason::Unanswered, "unanswered"),
            (OperationReason::Unreadable, "unreadable"),
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), name);
        }
    }

    /// An operation that removes no file (Docker's build cache): its bytes
    /// counted as removed, the cleaning complete only when it was done, the
    /// field absent when there is none, read back as written.
    #[test]
    fn a_line_says_its_operations_their_bytes_removed() {
        let now = SystemTime::UNIX_EPOCH;
        let done = vec![(
            CleanResult::Removed { bytes: 5 },
            HistoryOutcome::of_result(None, CleanResult::Removed { bytes: 5 }, None),
        )];
        let plain = HistoryEntry::of_outcomes(Vec::new(), done.clone(), None, now, false);
        assert!(
            serde_json::to_value(&plain)
                .unwrap()
                .get("operations")
                .is_none()
        );
        let record = |end| OperationRecord {
            action: CleanupAction::DockerBuildPrune,
            end,
        };
        let line_with = |end| {
            HistoryEntry::of_outcomes(Vec::new(), done.clone(), None, now, false)
                .with_operations(vec![record(end)])
        };
        let pruned = record(OperationEnd::Done {
            entries: 3,
            bytes: 1_000,
        });
        let line = line_with(pruned.end);
        assert_eq!(line.removed_bytes, 1_005);
        assert!(line.complete);
        assert_eq!(line.freed(), Some(1_005));
        assert_eq!(
            serde_json::to_value(&line).unwrap()["operations"],
            serde_json::json!([{ "action": "docker-build-prune", "end": "done", "entries": 3, "bytes": 1000 }])
        );
        let read: HistoryEntry =
            serde_json::from_str(&serde_json::to_string(&line).unwrap()).unwrap();
        assert_eq!(read.operations, [pruned]);

        let refused = line_with(OperationEnd::NotDone {
            reason: OperationReason::NotAllowed,
        });
        assert_eq!(refused.removed_bytes, 5);
        assert!(!refused.complete, "an operation not done");
        assert_eq!(refused.freed(), Some(5), "nothing went: known");

        let cut = line_with(OperationEnd::Unknown {
            reason: OperationReason::Unanswered,
        });
        assert_eq!(cut.freed(), None, "what it removed is unknown");
        assert_eq!(
            serde_json::to_value(&cut).unwrap()["operations"],
            serde_json::json!([{ "action": "docker-build-prune", "end": "unknown", "reason": "unanswered" }])
        );
    }

    /// The form the helper answers with is read back as it was written.
    #[test]
    fn an_outcome_reads_back_as_written() {
        let refused = outcome("kept", None, Some("failed"));
        let text = serde_json::to_string(&refused).unwrap();
        let read: HistoryOutcome = serde_json::from_str(&text).unwrap();
        assert_eq!(read, refused);
        assert_eq!(
            read.clean_result(),
            Some(CleanResult::Kept(KeepReason::Failed {
                kind: io::ErrorKind::PermissionDenied,
                code: Some(13)
            }))
        );
        let removed: HistoryOutcome =
            serde_json::from_str(r#"{"path": "/x", "result": "removed", "bytes": 4}"#).unwrap();
        assert!(!removed.inexact, "absent: exact");
        assert_eq!(
            removed.clean_result(),
            Some(CleanResult::Removed { bytes: 4 })
        );
        assert_eq!(outcome("kept", None, Some("odd")).clean_result(), None);
    }

    /// The history of `directory`, one line per `(time, removed bytes,
    /// root's part done)` of `cleanings`, written as a cleaning writes it.
    pub(super) fn recorded(directory: &Path, cleanings: &[(i64, u64, Option<bool>)]) {
        let mut history = History::open(directory).unwrap();
        for &(time, removed_bytes, admin_done) in cleanings {
            history
                .record(&HistoryEntry {
                    time,
                    removed_bytes,
                    admin: admin_done.map(|done| AdminRecord { done, reason: None }),
                    ..entry(1, Some(vec![outcome("removed", Some(4), None)]))
                })
                .unwrap();
        }
    }

    pub(super) fn append(directory: &Path, bytes: &[u8]) {
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(directory.join(HISTORY_FILE))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn last(directory: &Path) -> Option<LastCleaning> {
        History::last_cleaning(directory).unwrap()
    }

    fn length(file: &File) -> u64 {
        file.metadata().unwrap().len()
    }

    pub(super) fn uninstalled(directory: &Path, time: i64) {
        History::open(directory)
            .unwrap()
            .record_uninstall(&UninstallEntry {
                time,
                source: PackageSource::Snap,
                name: "Android Studio".to_owned(),
                packages: vec!["android-studio".to_owned()],
                freed_bytes: Some(3),
                data_removed: false,
                part: UninstallPart::App,
                later_bytes: None,
            })
            .unwrap();
    }

    #[test]
    fn an_old_version_removed_says_what_it_was_and_what_comes_back_later() {
        // Catches the part or the bytes given back later lost on the way to
        // the file or back.
        let directory = tempfile::tempdir().unwrap();
        let entry = UninstallEntry {
            time: 7,
            source: PackageSource::Snap,
            name: "Android Studio".to_owned(),
            packages: vec!["android-studio 248".to_owned()],
            freed_bytes: Some(0),
            data_removed: true,
            part: UninstallPart::OldVersion,
            later_bytes: Some(1_674_694_656),
        };
        History::open(directory.path())
            .unwrap()
            .record_uninstall(&entry)
            .unwrap();
        let line = lines(directory.path()).remove(0);
        assert_eq!(line["part"], "oldVersion");
        assert_eq!(line["laterBytes"], 1_674_694_656);
        assert_eq!(
            serde_json::from_value::<UninstallEntry>(line).unwrap(),
            entry
        );
        assert_eq!(
            serde_json::to_value(UninstallPart::UnusedRuntime).unwrap(),
            "unusedRuntime"
        );
    }

    #[test]
    fn an_uninstallation_says_its_kind_and_never_removed_bytes() {
        let directory = tempfile::tempdir().unwrap();
        uninstalled(directory.path(), 5);
        assert_eq!(
            lines(directory.path()),
            [serde_json::json!({
                "version": 1,
                "kind": "uninstall",
                "time": 5,
                "source": "snap",
                "name": "Android Studio",
                "packages": ["android-studio"],
                "freedBytes": 3,
                "dataRemoved": false,
            })]
        );
    }

    #[test]
    fn each_source_of_an_uninstallation_is_written_by_its_name() {
        for (source, name) in [
            (PackageSource::Deb, "deb"),
            (PackageSource::Snap, "snap"),
            (PackageSource::Flatpak, "flatpak"),
        ] {
            assert_eq!(serde_json::to_value(source).unwrap(), name);
        }
    }

    /// Snaps, one refused while its app was open.
    pub(super) fn snaps_updated(time: i64) -> UpdateEntry {
        UpdateEntry {
            time,
            source: PackageSource::Snap,
            packages: vec![
                UpdatedPackage {
                    name: "spotify".to_owned(),
                    version: "1.2.74".to_owned(),
                    previous_version: None,
                    done: false,
                    reason: Some("appsOpen".to_owned()),
                },
                UpdatedPackage {
                    name: "firefox".to_owned(),
                    version: "145.0-1".to_owned(),
                    previous_version: None,
                    done: true,
                    reason: None,
                },
            ],
            done: false,
            reason: None,
            restart_required: false,
        }
    }

    pub(super) fn updated(directory: &Path, entry: &UpdateEntry) {
        History::open(directory)
            .unwrap()
            .record_update(entry)
            .unwrap();
    }

    #[test]
    fn an_update_says_its_kind_each_package_and_never_removed_bytes() {
        let directory = tempfile::tempdir().unwrap();
        updated(
            directory.path(),
            &UpdateEntry {
                time: 1_763_300_000,
                source: PackageSource::Deb,
                packages: vec![UpdatedPackage {
                    name: "firefox".to_owned(),
                    version: "145.0+build1-0ubuntu1".to_owned(),
                    previous_version: Some("144.0.2+build1-0ubuntu1".to_owned()),
                    done: true,
                    reason: None,
                }],
                done: true,
                reason: None,
                restart_required: true,
            },
        );
        updated(directory.path(), &snaps_updated(1_763_300_400));
        updated(
            directory.path(),
            &UpdateEntry {
                packages: Vec::new(),
                reason: Some("cancelled".to_owned()),
                ..snaps_updated(1_763_300_500)
            },
        );
        // The format on disk, which readers rely on: every field named.
        assert_eq!(
            lines(directory.path()),
            [
                serde_json::json!({
                    "version": 1,
                    "kind": "update",
                    "time": 1_763_300_000,
                    "source": "deb",
                    "packages": [{
                        "name": "firefox",
                        "version": "145.0+build1-0ubuntu1",
                        "previousVersion": "144.0.2+build1-0ubuntu1",
                        "done": true,
                    }],
                    "done": true,
                    "restartRequired": true,
                }),
                serde_json::json!({
                    "version": 1,
                    "kind": "update",
                    "time": 1_763_300_400,
                    "source": "snap",
                    "packages": [
                        {"name": "spotify", "version": "1.2.74", "done": false, "reason": "appsOpen"},
                        {"name": "firefox", "version": "145.0-1", "done": true},
                    ],
                    "done": false,
                    "restartRequired": false,
                }),
                serde_json::json!({
                    "version": 1,
                    "kind": "update",
                    "time": 1_763_300_500,
                    "source": "snap",
                    "packages": [],
                    "done": false,
                    "reason": "cancelled",
                    "restartRequired": false,
                }),
            ]
        );
    }

    #[test]
    fn the_last_cleaning_is_found_past_the_updates_and_uninstallations_after_it() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        updated(directory.path(), &snaps_updated(200));
        uninstalled(directory.path(), 300);
        updated(directory.path(), &snaps_updated(400));
        assert_eq!(
            last(directory.path()),
            Some(LastCleaning {
                time: 100,
                freed: Some(7)
            })
        );
    }

    #[test]
    fn updates_alone_are_no_last_cleaning_and_the_one_set_aside_says() {
        let directory = tempfile::tempdir().unwrap();
        updated(directory.path(), &snaps_updated(200));
        assert_eq!(last(directory.path()), None);
        // Set aside with a cleaning in it, the history read on into it.
        recorded(directory.path(), &[(300, 5, None)]);
        std::fs::rename(
            History::path(directory.path()),
            History::rotated_path(directory.path()),
        )
        .unwrap();
        updated(directory.path(), &snaps_updated(400));
        assert_eq!(last(directory.path()).map(|last| last.time), Some(300));
    }

    #[test]
    fn the_last_cleaning_is_found_past_the_uninstallations_after_it() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        uninstalled(directory.path(), 200);
        uninstalled(directory.path(), 300);
        assert_eq!(
            last(directory.path()),
            Some(LastCleaning {
                time: 100,
                freed: Some(7)
            })
        );
        recorded(directory.path(), &[(400, 2, None)]);
        assert_eq!(last(directory.path()).map(|last| last.time), Some(400));
    }

    #[test]
    fn uninstallations_alone_are_no_last_cleaning() {
        let directory = tempfile::tempdir().unwrap();
        uninstalled(directory.path(), 200);
        uninstalled(directory.path(), 300);
        assert_eq!(last(directory.path()), None);
    }

    #[test]
    fn a_line_cut_short_before_an_uninstallation_says_no_last_cleaning() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        append(directory.path(), b"{\"version\":1,\"tim");
        uninstalled(directory.path(), 200);
        assert_eq!(last(directory.path()), None, "never the line before it");
    }

    #[test]
    fn a_line_of_a_kind_unknown_says_no_last_cleaning() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        append(
            directory.path(),
            b"{\"version\":1,\"kind\":\"install\",\"time\":200,\"removedBytes\":4}\n",
        );
        assert_eq!(last(directory.path()), None);
    }

    #[test]
    fn a_line_without_its_facts_is_never_passed() {
        for line in [
            &b"{\"version\":1,\"kind\":\"uninstall\"}\n"[..],
            b"{\"version\":1,\"kind\":\"update\"}\n",
            b"{\"version\":1,\"time\":200}\n",
        ] {
            let directory = tempfile::tempdir().unwrap();
            recorded(directory.path(), &[(100, 7, None)]);
            append(directory.path(), line);
            assert_eq!(last(directory.path()), None, "{line:?}");
        }
    }

    #[test]
    fn the_lines_read_back_are_bounded_together() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        uninstalled(directory.path(), 200);
        updated(directory.path(), &snaps_updated(300));
        let path = History::path(directory.path());
        let history = File::open(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let line_lengths: Vec<u64> = text.lines().map(|line| line.len() as u64 + 1).collect();
        // Room for each line alone, not for the three together.
        let longest = *line_lengths.iter().max().unwrap();
        assert_eq!(
            find_last_cleaning(&history, &path, longest).unwrap(),
            Searched::Found(None)
        );
        let all = line_lengths.iter().sum::<u64>();
        assert!(matches!(
            find_last_cleaning(&history, &path, all).unwrap(),
            Searched::Found(Some(last)) if last.time == 100
        ));
    }

    #[test]
    fn the_last_cleaning_is_the_last_line() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None), (200, 9, Some(true))]);
        assert_eq!(
            last(directory.path()),
            Some(LastCleaning {
                time: 200,
                freed: Some(9)
            })
        );
    }

    #[test]
    fn what_root_s_part_unfinished_freed_is_unknown() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, Some(false))]);
        assert_eq!(
            last(directory.path()),
            Some(LastCleaning {
                time: 100,
                freed: None
            })
        );
    }

    #[test]
    fn a_last_line_cut_short_says_no_last_cleaning_until_the_next() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        append(directory.path(), b"{\"version\":1,\"tim");
        assert_eq!(last(directory.path()), None, "never the line before it");
        recorded(directory.path(), &[(300, 5, None)]);
        assert_eq!(last(directory.path()).map(|last| last.time), Some(300));
    }

    #[test]
    fn a_last_line_of_another_version_is_not_read() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None), (200, 9, None)]);
        let text = std::fs::read_to_string(directory.path().join(HISTORY_FILE)).unwrap();
        let other = text.replacen(
            "{\"version\":1,\"time\":200",
            "{\"version\":2,\"time\":200",
            1,
        );
        assert_ne!(other, text);
        std::fs::write(directory.path().join(HISTORY_FILE), other).unwrap();
        assert_eq!(last(directory.path()), None);
    }

    #[test]
    fn without_a_line_there_is_no_last_cleaning() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(last(directory.path()), None, "no file");
        for content in [&b""[..], b"\n", b"\n\n"] {
            std::fs::write(directory.path().join(HISTORY_FILE), content).unwrap();
            assert_eq!(last(directory.path()), None, "{content:?}");
        }
    }

    #[test]
    fn a_history_that_is_not_a_file_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(HISTORY_FILE)).unwrap();
        assert!(matches!(
            History::last_cleaning(directory.path()),
            Err(StoreError::NotAFile { .. })
        ));
    }

    /// Every block size finds the same line: none is cut or joined wrong
    /// where blocks meet.
    #[test]
    fn the_last_line_is_the_same_whatever_the_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        for (content, start, end) in [
            (
                &b"first\nthe last line, longer than the one before\n\n"[..],
                6,
                47,
            ),
            (b"first\nlast", 6, 10),
            (b"\n\nlast\n", 2, 6),
            (b"alone\n", 0, 5),
            (b"solo", 0, 4),
        ] {
            std::fs::write(&path, content).unwrap();
            let file = File::open(&path).unwrap();
            for block in 1..=content.len() as u64 + 1 {
                assert_eq!(
                    last_line(&file, length(&file), block, MAX_LINE).unwrap().0,
                    Tail::Line { start, end },
                    "{content:?} by {block}"
                );
            }
        }
    }

    #[test]
    fn a_line_too_long_is_not_read_whole() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HISTORY_FILE);
        std::fs::write(&path, b"first\n0123456789\n\n\n").unwrap();
        let file = File::open(&path).unwrap();
        for block in [2, 64] {
            assert_eq!(
                last_line(&file, length(&file), block, 9).unwrap().0,
                Tail::TooLong
            );
            assert_eq!(
                last_line(&file, length(&file), block, 10).unwrap().0,
                Tail::Line { start: 6, end: 16 },
                "the line breaks after it never count"
            );
        }
        // Never read whole: given up once more than allowed and a block is read.
        let (tail, read) = last_line(&file, length(&file), 1, 4).unwrap();
        assert_eq!(tail, Tail::TooLong);
        assert!(read <= 3 + 4 + 1, "{read} bytes read");
    }

    /// A line added between finding the last one and reading it is not
    /// read with it.
    #[test]
    fn a_line_added_meanwhile_is_not_read() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        let path = directory.path().join(HISTORY_FILE);
        let file = File::open(&path).unwrap();
        let Tail::Line { start, end } = last_line(&file, length(&file), BLOCK, MAX_LINE).unwrap().0
        else {
            panic!("a line");
        };
        recorded(directory.path(), &[(200, 9, None)]);
        assert!(
            matches!(parse(&file, start, end, &path).unwrap(), LineRead::Cleaning(last) if last.time == 100)
        );
    }

    #[test]
    fn a_cleaning_without_root_s_part_says_what_it_freed() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, Some(true)), (300, 5, None)]);
        assert_eq!(
            last(directory.path()),
            Some(LastCleaning {
                time: 300,
                freed: Some(5)
            })
        );
    }

    /// An operation whose end is unknown makes what the last cleaning freed
    /// unknown, as root's part does.
    #[test]
    fn a_last_cleaning_with_an_operation_ended_unknown_freed_an_unknown_room() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        let mut entry = entry(1, None).with_operations(vec![OperationRecord {
            action: CleanupAction::DockerBuildPrune,
            end: OperationEnd::Unknown {
                reason: OperationReason::Cancelled,
            },
        }]);
        entry.time = 300;
        History::open(directory.path())
            .unwrap()
            .record(&entry)
            .unwrap();
        assert_eq!(
            last(directory.path()),
            Some(LastCleaning {
                time: 300,
                freed: None
            })
        );
    }

    #[test]
    fn a_last_line_without_its_facts_is_not_read() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        append(directory.path(), b"{\"version\":1}\n");
        assert_eq!(last(directory.path()), None);
    }

    /// A deletion log over several blocks is never read back: the last
    /// cleaning is found within a bound far shorter than it.
    #[test]
    fn a_deletion_log_however_long_is_never_read_back() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        History::open(directory.path())
            .unwrap()
            .record(&HistoryEntry {
                time: 200,
                removed_bytes: 9,
                ..entry(1, Some(vec![outcome("removed", Some(4), None); 4000]))
            })
            .unwrap();
        let path = History::path(directory.path());
        let history = File::open(&path).unwrap();
        assert!(length(&history) > 2 * BLOCK, "{}", length(&history));
        assert_eq!(
            find_last_cleaning(&history, &path, BLOCK).unwrap(),
            Searched::Found(Some(LastCleaning {
                time: 200,
                freed: Some(9)
            }))
        );
    }

    #[test]
    fn a_deletion_log_without_its_cleaning_line_is_met_past_uninstallations() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        let text = std::fs::read_to_string(History::path(directory.path())).unwrap();
        let deletions = text.lines().next().unwrap().to_owned();
        append(directory.path(), format!("{deletions}\n").as_bytes());
        uninstalled(directory.path(), 200);
        assert_eq!(last(directory.path()), None, "never the one before");
    }

    #[test]
    fn a_deletion_log_without_its_cleaning_line_says_no_last_cleaning() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        let text = std::fs::read_to_string(History::path(directory.path())).unwrap();
        let deletions = text.lines().next().unwrap().to_owned();
        assert!(deletions.contains("\"kind\":\"deletions\""), "{deletions}");
        // A write cut right after the deletion log.
        append(directory.path(), format!("{deletions}\n").as_bytes());
        assert_eq!(last(directory.path()), None, "never the one before");
    }
}
