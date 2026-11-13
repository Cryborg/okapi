use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use okapi_engine::{CleanOutcome, CleanReceipt, CleanResult, KeepReason};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::StoreError;

const HISTORY_FILE: &str = "history.jsonl";
/// The format of each history line.
const HISTORY_VERSION: u32 = 1;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminRecord {
    /// It ran to its end over all it was asked for and said what became of
    /// each entry (cancelled ones included). `false`: it did not (it stopped,
    /// or part of what it was asked for was left), `reason` says why; its outcomes, if
    /// any, say what it touched, and when it gave none what it removed may
    /// still be unknown (its traces in the journal say).
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

#[derive(Serialize)]
struct Line<'a> {
    version: u32,
    #[serde(flatten)]
    entry: &'a HistoryEntry,
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
        let line = Line {
            version: HISTORY_VERSION,
            entry,
        };
        let mut bytes = serde_json::to_vec(&line).map_err(|error| StoreError::Unwritable {
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
}
