use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use okapi_engine::{CleanOutcome, CleanReceipt, CleanResult, KeepReason};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{StoreError, file};

const HISTORY_FILE: &str = "history.jsonl";
/// The format of each history line.
const HISTORY_VERSION: u32 = 1;
/// A bound on what is read, on the caller's thread, to find the last line:
/// a longer one (a link to another file, or a deletion log beyond any seen)
/// is not read, which is traced.
const MAX_LINE: u64 = 256 * 1024 * 1024;
/// The blocks the history is read by from its end, none of them kept.
const BLOCK: u64 = 64 * 1024;

/// One cleaning, as kept in the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    /// Every entry, when the user keeps a deletion log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<HistoryOutcome>>,
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
    /// The line of a cleaning that ended at `now`, with every entry when the
    /// user keeps a deletion log.
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
            entries: deletion_log.then_some(outcomes),
        }
    }
}

impl HistoryOutcome {
    /// What became of `outcome`, as scripts and the history read it.
    #[must_use]
    pub fn of(outcome: &CleanOutcome) -> Self {
        let inexact = [outcome.path(), outcome.blocked()]
            .into_iter()
            .flatten()
            .any(|path| path.to_str().is_none());
        if inexact {
            tracing::warn!(path = ?outcome.path(), "a name not in UTF-8 is recorded inexactly");
        }
        let lossy = |path: &Path| path.to_string_lossy().into_owned();
        let result = outcome.result();
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
            path: outcome.path().map(lossy),
            result: result.key().to_owned(),
            bytes,
            reason: reason.map(|reason| reason.key().to_owned()),
            error,
            code,
            blocked: outcome.blocked().map(lossy),
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

    /// The result another process said, as the history and the receipt
    /// count it: one that cannot be read back is an entry the system kept
    /// for an unknown reason (never one of another report), traced.
    #[must_use]
    pub fn clean_result_or_refused(&self) -> CleanResult {
        self.clean_result().unwrap_or_else(|| {
            tracing::error!(outcome = ?self, "a result that cannot be read back: counted kept");
            CleanResult::Kept(KeepReason::Failed {
                kind: io::ErrorKind::Other,
                code: None,
            })
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallEntry {
    /// Seconds since the clock's origin, negative before it.
    pub time: i64,
    pub source: UninstallSource,
    /// The application, as the user saw it.
    pub name: String,
    /// What went, by the names its source gives (packages, a snap).
    pub packages: Vec<String>,
    /// What it freed, as estimated before; absent when a size was not
    /// known. Never `removedBytes`: a reader that knows no kind would take
    /// the line for a cleaning's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freed_bytes: Option<u64>,
    /// Its data went with it.
    pub data_removed: bool,
}

/// Who removed an application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UninstallSource {
    Deb,
    Snap,
}

/// The kind a line says: a cleaning's says none, and one unknown makes the
/// line unreadable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LineKind {
    Uninstall,
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
}

impl History {
    /// Opens the history in `directory` (created private if needed).
    ///
    /// # Errors
    ///
    /// [`StoreError::Unwritable`].
    pub fn open(directory: &Path) -> Result<Self, StoreError> {
        let path = Self::path(directory);
        let unwritable = |error| StoreError::Unwritable {
            path: path.clone(),
            error,
        };
        okapi_platform::create_private_directory(directory).map_err(unwritable)?;
        let file = okapi_platform::open_journal(&path).map_err(unwritable)?;
        Ok(Self { file, path })
    }

    /// Adds the line of `receipt`, a cleaning that ended now, with every
    /// entry when the user keeps a deletion log.
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
    /// state directory is known). The deletion log the user may ask for is
    /// never lost: then a history that cannot be kept is the error, and
    /// nothing must be removed.
    ///
    /// # Errors
    ///
    /// [`HistoryUnavailable`], with a deletion log asked for only.
    pub fn open_for_cleaning(
        directory: Option<&Path>,
        deletion_log: bool,
    ) -> Result<Opened, HistoryUnavailable> {
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

    /// The last cleaning the history of `directory` keeps, read from its
    /// last line, past the lines of the applications uninstalled after it:
    /// `None` when there is none, or when the line met does not say one (a
    /// write cut short, another version or kind, too long), traced at each
    /// reading. A line before one that cannot be read is never taken for
    /// the last cleaning: a cleaning may have come after. All the lines read
    /// are bounded together as one line is.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unreadable`] (the file failing to be read, the line
    /// included), [`StoreError::BrokenLink`] or [`StoreError::NotAFile`].
    pub fn last_cleaning(directory: &Path) -> Result<Option<LastCleaning>, StoreError> {
        let path = Self::path(directory);
        let _span = tracing::info_span!("last_cleaning", path = %path.display()).entered();
        let Some(history) = file::open_regular(&path)? else {
            tracing::info!("no history: no last cleaning");
            return Ok(None);
        };
        find_last_cleaning(&history, &path, MAX_LINE)
    }

    /// Adds `entry` as one line, written at once and synced to the disk: `Ok`
    /// means the line is kept (but for a history file created by this very
    /// run, whose name a power cut may still lose). A last line an interrupted run left unfinished
    /// is ended with a line break first, so an interrupted write spoils at
    /// most its own line.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unwritable`].
    pub fn record(&mut self, entry: &HistoryEntry) -> Result<(), StoreError> {
        self.write(&Line {
            version: HISTORY_VERSION,
            kind: None,
            entry,
        })
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
        self.write(&Line {
            version: HISTORY_VERSION,
            kind: Some(LineKind::Uninstall),
            entry,
        })
        .inspect_err(
            |error| tracing::error!(%error, name = %entry.name, "uninstallation not recorded"),
        )
    }

    fn write<T: Serialize>(&mut self, line: &Line<'_, T>) -> Result<(), StoreError> {
        let mut bytes = serde_json::to_vec(line).map_err(|error| StoreError::Unwritable {
            path: self.path.clone(),
            error: io::Error::other(error),
        })?;
        bytes.push(b'\n');
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
fn find_last_cleaning(
    history: &File,
    path: &Path,
    max_line: u64,
) -> Result<Option<LastCleaning>, StoreError> {
    let started = Instant::now();
    let unreadable = |error| StoreError::Unreadable {
        path: path.to_owned(),
        error,
    };
    // Its length is taken once: a line added meanwhile is not seen.
    let length = history.metadata().map_err(unreadable)?.len();
    let mut until = length;
    let mut scanned = 0;
    let mut uninstalls = 0;
    loop {
        // The lines passed count against the bound, as one line would.
        let room = max_line.saturating_sub(length - until);
        let (tail, read) = last_line(history, until, BLOCK, room).map_err(unreadable)?;
        scanned += read;
        let (start, end) = match tail {
            Tail::Empty => {
                tracing::info!(
                    scanned,
                    uninstalls,
                    "history without a cleaning line: no last cleaning"
                );
                return Ok(None);
            }
            Tail::TooLong => {
                tracing::warn!(scanned, uninstalls, max = max_line, elapsed = ?started.elapsed(), "history lines too long: not read");
                return Ok(None);
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
                    uninstalls,
                    ?elapsed,
                    ?last,
                    "last cleaning read"
                );
                return Ok(Some(last));
            }
            Ok(LineRead::Uninstall) => {
                uninstalls += 1;
                until = start;
            }
            Err(error @ (StoreError::Invalid { .. } | StoreError::UnsupportedVersion { .. })) => {
                tracing::warn!(offset = start, length = end - start, scanned, uninstalls, ?elapsed, %error, "history line unreadable: no last cleaning");
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
    }
}

/// The last cleaning a history keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastCleaning {
    /// Seconds since the clock's origin, negative before it.
    pub time: i64,
    /// What it freed; unknown when root's part did not end, as what it
    /// removed may then be missing.
    pub freed: Option<u64>,
}

/// What the last cleaning is read from in a line, in one pass: the rest is
/// skipped. A cleaning's line says no kind and its bytes; an
/// uninstallation's says its kind, and its time like any.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LastLine {
    kind: Option<LineKind>,
    time: i64,
    removed_bytes: Option<u64>,
    admin: Option<AdminRecord>,
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
    /// An application uninstalled: the line before is read.
    Uninstall,
}

/// What the line of `history` from `start` to `end` says, read as it
/// streams by (a deletion log is skipped, never kept), its version checked
/// first: a line of another version is never read as this one's, nor one of
/// a kind unknown as a cleaning's.
fn parse(history: &File, start: u64, end: u64, path: &Path) -> Result<LineRead, StoreError> {
    let line = || {
        let mut reader = history;
        reader.seek(SeekFrom::Start(start))?;
        // Never past the line found: one added since is not read with it.
        Ok(BufReader::new(reader.take(end - start)))
    };
    let unreadable = |error| StoreError::Unreadable {
        path: path.to_owned(),
        error,
    };
    file::check_version(line().map_err(unreadable)?, path, HISTORY_VERSION)?;
    let read: LastLine = serde_json::from_reader(line().map_err(unreadable)?)
        .map_err(|error| file::json_error(path, error))?;
    if let Some(LineKind::Uninstall) = read.kind {
        return Ok(LineRead::Uninstall);
    }
    let removed_bytes = read
        .removed_bytes
        .ok_or_else(|| file::json_error(path, serde::de::Error::missing_field("removedBytes")))?;
    Ok(LineRead::Cleaning(LastCleaning {
        time: read.time,
        freed: read
            .admin
            .is_none_or(|admin| admin.done)
            .then_some(removed_bytes),
    }))
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

    fn entry(removed: usize, entries: Option<Vec<HistoryOutcome>>) -> HistoryEntry {
        HistoryEntry {
            time: 1_700_000_000,
            stops: vec!["minAgeBeyondClock".to_owned()],
            removed_bytes: 10,
            removed,
            partial: 2,
            gone: 3,
            kept: 4,
            complete: false,
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
        assert_eq!(lines.len(), 2);
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
        assert_eq!(lines[1]["removed"], 2);
        assert_eq!(lines[1]["entries"][0]["path"], "/tmp/x");
        assert_eq!(lines[1]["removedBytes"], 10);
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

    fn outcome(result: &str, bytes: Option<u64>, reason: Option<&str>) -> HistoryOutcome {
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
    fn recorded(directory: &Path, cleanings: &[(i64, u64, Option<bool>)]) {
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

    fn append(directory: &Path, bytes: &[u8]) {
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

    fn uninstalled(directory: &Path, time: i64) {
        History::open(directory)
            .unwrap()
            .record_uninstall(&UninstallEntry {
                time,
                source: UninstallSource::Snap,
                name: "Android Studio".to_owned(),
                packages: vec!["android-studio".to_owned()],
                freed_bytes: Some(3),
                data_removed: false,
            })
            .unwrap();
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
        uninstalled(directory.path(), 300);
        let path = History::path(directory.path());
        let history = File::open(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let line_lengths: Vec<u64> = text.lines().map(|line| line.len() as u64 + 1).collect();
        // Room for each line alone, not for the three together.
        let longest = *line_lengths.iter().max().unwrap();
        assert_eq!(find_last_cleaning(&history, &path, longest).unwrap(), None);
        let all = line_lengths.iter().sum::<u64>();
        assert_eq!(
            find_last_cleaning(&history, &path, all)
                .unwrap()
                .map(|last| last.time),
            Some(100)
        );
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

    #[test]
    fn a_last_line_without_its_facts_is_not_read() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None)]);
        append(directory.path(), b"{\"version\":1}\n");
        assert_eq!(last(directory.path()), None);
    }

    /// A deletion log over several blocks is skipped as it streams by.
    #[test]
    fn a_last_line_over_several_blocks_is_read() {
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
        let length = std::fs::metadata(directory.path().join(HISTORY_FILE))
            .unwrap()
            .len();
        assert!(length > 2 * BLOCK, "{length}");
        assert_eq!(
            last(directory.path()),
            Some(LastCleaning {
                time: 200,
                freed: Some(9)
            })
        );
    }
}
