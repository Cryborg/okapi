//! The summary of the last analysis, kept from one launch to the next for
//! the dashboard to show. It names nothing to remove: a cleaning only ever
//! takes the report held in memory (invariant 1).

use std::path::{Path, PathBuf};

use okapi_engine::{ScanOptions, ScanReport};
use serde::{Deserialize, Serialize};

use crate::{History, StoreError, file, seconds};

const LAST_ANALYSIS_FILE: &str = "last-analysis.json";
/// The summary format this Okapi reads.
const LAST_ANALYSIS_VERSION: u32 = 1;

/// What the last analysis found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastAnalysis {
    /// When it started, in seconds since the clock's origin (negative
    /// before): what it found may have changed from then on.
    pub time: i64,
    /// The bytes it offered.
    pub reclaimable_bytes: u64,
    /// The entries it offered: a file, or a whole directory.
    pub entries: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawLastAnalysis {
    #[expect(dead_code, reason = "checked by `file::check_version` first")]
    version: u32,
    time: i64,
    reclaimable_bytes: u64,
    entries: u64,
}

impl LastAnalysis {
    /// The summary of `report`, made with `options` (those it was scanned
    /// with, never a cleaning's): dated from when it started. An analysis stopped or cancelled makes a summary of what it
    /// found, which the caller keeps or not.
    #[must_use]
    pub fn of(report: &ScanReport, options: &ScanOptions) -> Self {
        Self {
            time: seconds(options.now()),
            reclaimable_bytes: report.total_size(),
            entries: report
                .groups()
                .iter()
                .map(|group| u64::try_from(group.items().len()).unwrap_or(u64::MAX))
                .fold(0, u64::saturating_add),
        }
    }

    /// The file the summary of `directory` lives in.
    #[must_use]
    pub fn path(directory: &Path) -> PathBuf {
        directory.join(LAST_ANALYSIS_FILE)
    }

    /// Reads the summary kept in `directory`; `None` when there is none.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unreadable`], [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`], [`StoreError::TooLarge`],
    /// [`StoreError::UnsupportedVersion`] or [`StoreError::Invalid`].
    pub fn load(directory: &Path) -> Result<Option<Self>, StoreError> {
        let path = Self::path(directory);
        let Some(raw) = file::read_versioned::<RawLastAnalysis>(&path, LAST_ANALYSIS_VERSION)?
        else {
            tracing::info!(path = %path.display(), "no last analysis");
            return Ok(None);
        };
        let analysis = Self {
            time: raw.time,
            reclaimable_bytes: raw.reclaimable_bytes,
            entries: raw.entries,
        };
        tracing::info!(path = %path.display(), ?analysis, "last analysis loaded");
        Ok(Some(analysis))
    }

    /// Whether a cleaning was recorded since this analysis started (one
    /// running beside it included), in the history of `directory`, by the
    /// interface or the command line: only a cleaning opens or writes the
    /// history, so its date tells, however long the history. A cleaning that
    /// removed nothing counts too, and so would a history touched by hand:
    /// the summary is then said outdated, which costs a new analysis. Not
    /// seen: a cleaning cut short before its line was written, a history
    /// restored with an older date, a clock set back, or a history on a
    /// filesystem whose clock lags. No history: not outdated.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unreadable`], [`StoreError::BrokenLink`] or
    /// [`StoreError::NotAFile`].
    pub fn outdated(&self, directory: &Path) -> Result<bool, StoreError> {
        let path = History::path(directory);
        let Some(history) = file::open_regular(&path)? else {
            tracing::info!(path = %path.display(), "no history: last analysis not outdated");
            return Ok(false);
        };
        let changed = history
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|error| StoreError::Unreadable {
                path: path.clone(),
                error,
            })?;
        // In whole seconds, like the summary: a history changed within the
        // second the analysis started may come after it, so it outdates it.
        let outdated = seconds(changed) >= self.time;
        tracing::info!(
            path = %path.display(),
            history_changed = seconds(changed),
            analysis_started = self.time,
            outdated,
            "last analysis judged against the history"
        );
        Ok(outdated)
    }

    /// Keeps the summary in `directory`, replacing the one there whole.
    ///
    /// # Errors
    ///
    /// Those of an atomic write: [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`] (a link there is never written through) or
    /// [`StoreError::Unwritable`].
    pub fn save(&self, directory: &Path) -> Result<(), StoreError> {
        file::write_versioned(directory, LAST_ANALYSIS_FILE, LAST_ANALYSIS_VERSION, self)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use std::time::SystemTime;

    use super::*;

    fn load(text: &str) -> Result<Option<LastAnalysis>, StoreError> {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(LAST_ANALYSIS_FILE), text).unwrap();
        LastAnalysis::load(directory.path())
    }

    #[test]
    fn a_summary_comes_back_as_it_was_kept_in_the_documented_form() {
        let directory = tempfile::tempdir().unwrap();
        let analysis = LastAnalysis {
            time: -5,
            reclaimable_bytes: 2_400_000_000,
            entries: 18_312,
        };
        analysis.save(directory.path()).unwrap();
        assert_eq!(
            LastAnalysis::load(directory.path()).unwrap(),
            Some(analysis)
        );
        let text = fs::read_to_string(directory.path().join(LAST_ANALYSIS_FILE)).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({
                "version": 1,
                "time": -5,
                "reclaimableBytes": 2_400_000_000_u64,
                "entries": 18_312,
            })
        );
    }

    #[test]
    fn a_cleaning_recorded_after_the_analysis_makes_it_outdated() {
        let directory = tempfile::tempdir().unwrap();
        let analysis = LastAnalysis {
            time: 1_000,
            reclaimable_bytes: 1,
            entries: 1,
        };
        assert!(!analysis.outdated(directory.path()).unwrap(), "no history");
        let history = fs::File::create(History::path(directory.path())).unwrap();
        let at = |milliseconds: u64| {
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(milliseconds)
        };
        for (changed, outdated) in [
            (999_999, false),
            (1_000_000, true),
            (1_000_999, true),
            (1_001_000, true),
        ] {
            history.set_modified(at(changed)).unwrap();
            assert_eq!(
                analysis.outdated(directory.path()).unwrap(),
                outdated,
                "history changed at {changed} ms"
            );
        }
    }

    #[test]
    fn a_history_that_cannot_be_judged_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let analysis = LastAnalysis {
            time: 1,
            reclaimable_bytes: 1,
            entries: 1,
        };
        fs::create_dir(History::path(directory.path())).unwrap();
        assert!(matches!(
            analysis.outdated(directory.path()),
            Err(StoreError::NotAFile { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_history_behind_a_link_is_judged_by_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let analysis = LastAnalysis {
            time: 1_000,
            reclaimable_bytes: 1,
            entries: 1,
        };
        let target = directory.path().join("kept.jsonl");
        std::os::unix::fs::symlink(&target, History::path(directory.path())).unwrap();
        assert!(matches!(
            analysis.outdated(directory.path()),
            Err(StoreError::BrokenLink { .. })
        ));
        fs::File::create(&target)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000))
            .unwrap();
        assert!(analysis.outdated(directory.path()).unwrap());
    }

    #[test]
    fn no_file_is_no_summary() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(LastAnalysis::load(directory.path()).unwrap(), None);
    }

    #[test]
    fn a_mistake_in_the_file_is_an_error_never_no_summary() {
        for text in [
            r#"{"version": 1, "time": 1, "reclaimableBytes": 2, "entries": 3, "cleaned": true}"#,
            r#"{"version": 1, "time": 1, "entries": 3}"#,
            r#"{"version": 1, "time": 1, "reclaimableBytes": -2, "entries": 3}"#,
        ] {
            assert!(
                matches!(load(text), Err(StoreError::Invalid { .. })),
                "{text}"
            );
        }
        assert!(matches!(
            load(r#"{"version": 2, "time": 1, "categories": []}"#),
            Err(StoreError::UnsupportedVersion {
                version: 2,
                supported: LAST_ANALYSIS_VERSION,
                ..
            })
        ));
    }
}
