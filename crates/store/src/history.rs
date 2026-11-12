use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

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
    /// What kept the cleaning from touching anything.
    pub stops: Vec<String>,
    pub removed_bytes: u64,
    pub removed: usize,
    pub partial: usize,
    pub gone: usize,
    pub kept: usize,
    pub complete: bool,
    /// Every entry, when the user keeps a deletion log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<HistoryOutcome>>,
}

/// What became of one entry, in the form scripts and the history read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryOutcome {
    pub path: Option<String>,
    /// `removed`, `partial`, `gone` or `kept`.
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    /// A name in `path` or `blocked` is not UTF-8: its bad bytes were
    /// replaced, so the text is not exactly the path.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub inexact: bool,
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
            result: "removed",
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
}
