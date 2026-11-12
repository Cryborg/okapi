//! What a run shows: each group summed up once, then written as text or as
//! one JSON document. Identifiers never leave the process.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;
use std::time::SystemTime;

use okapi_engine::{
    CleanOutcome, CleanReceipt, CleanResult, ItemId, KeepReason, Origin, ScanGroup, ScanReport,
};
use serde::Serialize;

use crate::args::Verbosity;
use crate::selection::{self, Category, Decision, decide};
use crate::text::{Language, Message, text};

/// A group as the command line shows it and decides about it.
#[derive(Debug, Serialize)]
pub struct GroupSummary {
    pub label: String,
    pub rules: Vec<String>,
    pub entries: Vec<EntrySummary>,
    pub bytes: u64,
    pub preselected: bool,
    pub cache_reset: bool,
    pub needs_admin: bool,
    pub decision: Decision,
    #[serde(skip)]
    pub ids: Vec<ItemId>,
}

#[derive(Debug, Serialize)]
pub struct EntrySummary {
    pub path: String,
    pub bytes: u64,
    /// Seconds since the clock's origin, negative before it.
    pub last_change: i64,
}

/// Every group of `report`, with what a run asking for `categories` does
/// with it.
pub fn summaries(report: &ScanReport, categories: &BTreeSet<Category>) -> Vec<GroupSummary> {
    report
        .groups()
        .iter()
        .map(|group| summary(group, categories))
        .collect()
}

fn summary(group: &ScanGroup, categories: &BTreeSet<Category>) -> GroupSummary {
    GroupSummary {
        label: group.label().to_string(),
        rules: group.origins().iter().map(qualified_rule_name).collect(),
        entries: group
            .items()
            .iter()
            .map(|item| EntrySummary {
                path: item.path().to_string_lossy().into_owned(),
                bytes: item.size(),
                last_change: seconds(item.last_change()),
            })
            .collect(),
        bytes: group.total_size(),
        preselected: group.is_selected_by_default(),
        cache_reset: group.is_cache_reset(),
        needs_admin: group.needs_admin(),
        decision: decide(
            group.origins(),
            group.is_selected_by_default(),
            group.needs_admin(),
            categories,
        ),
        ids: group
            .items()
            .iter()
            .map(okapi_engine::ScanItem::id)
            .collect(),
    }
}

/// A rule as `category:name`, stable for scripts and the journal.
fn qualified_rule_name(origin: &Origin) -> String {
    format!(
        "{}:{}",
        Category::of(origin).name(),
        selection::rule_name(origin)
    )
}

/// Writes `line` on standard error. If even that fails there is nowhere left
/// to say it: the failure is dropped rather than turned into a panic.
pub fn error_line(line: &str) {
    let _ = writeln!(io::stderr(), "{line}");
}

fn seconds(instant: SystemTime) -> i64 {
    match instant.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => {
            i64::try_from(before.duration().as_secs()).map_or(i64::MIN, |seconds| -seconds)
        }
    }
}

/// Standard output, written line by line. A closed pipe (`okapi | head`)
/// silences it; any other error silences it too and is kept, to fail the
/// run.
pub struct Output {
    stdout: io::StdoutLock<'static>,
    closed: bool,
    failure: Option<io::Error>,
}

impl Output {
    pub fn new() -> Self {
        Self {
            stdout: io::stdout().lock(),
            closed: false,
            failure: None,
        }
    }

    pub fn line(&mut self, line: &str) {
        if self.closed {
            return;
        }
        if let Err(error) = writeln!(self.stdout, "{line}") {
            self.closed = true;
            if error.kind() == io::ErrorKind::BrokenPipe {
                tracing::debug!("standard output closed by the reader");
            } else {
                tracing::error!(%error, "standard output failed");
                self.failure = Some(error);
            }
        }
    }

    /// The error that cut standard output, if any (a closed pipe is none).
    pub const fn failure(&self) -> Option<&io::Error> {
        self.failure.as_ref()
    }

    fn fail(&mut self, error: io::Error) {
        self.closed = true;
        self.failure = Some(error);
    }
}

/// What a run found and did, for the text and JSON outputs.
pub struct Shown<'a> {
    pub language: Language,
    pub report: &'a ScanReport,
    pub groups: &'a [GroupSummary],
    pub receipt: Option<&'a CleanReceipt>,
    pub clean: bool,
    pub verbosity: Verbosity,
    pub stopped: bool,
    pub journal: Option<&'a Path>,
}

impl Shown<'_> {
    fn shown(&self) -> impl Iterator<Item = &GroupSummary> {
        self.groups
            .iter()
            .filter(|group| group.decision != Decision::OutsideCategories)
    }

    /// Entries and bytes offered in the categories asked for.
    pub fn found(&self) -> (usize, u64) {
        self.shown().fold((0, 0), |(entries, bytes), group| {
            (
                entries + group.entries.len(),
                bytes.saturating_add(group.bytes),
            )
        })
    }

    pub fn write_text(&self, output: &mut Output) {
        let say = |message: &Message<'_>| text(self.language, message);
        let quiet = self.verbosity == Verbosity::Quiet;
        if let Some(journal) = self.journal
            && !quiet
        {
            output.line(&say(&Message::Journal(journal)));
        }
        if !quiet {
            for group in self.shown() {
                output.line(&say(&Message::Group {
                    label: &group.label,
                    entries: group.entries.len(),
                    bytes: group.bytes,
                    decision: group.decision,
                    clean: self.clean,
                }));
                if self.verbosity == Verbosity::Verbose {
                    for entry in &group.entries {
                        output.line(&say(&Message::Entry {
                            path: Path::new(&entry.path),
                            bytes: entry.bytes,
                        }));
                    }
                }
            }
        }
        // Stops go to standard error, always: see `main`. Other issues are
        // shown even quietly: they are errors.
        for issue in self
            .report
            .issues()
            .iter()
            .filter(|issue| !issue.stops_the_scan())
        {
            output.line(&say(&Message::Issue(issue)));
        }
        if self.stopped {
            return;
        }
        let (entries, bytes) = self.found();
        if entries == 0 {
            output.line(&say(&Message::NothingFound));
        } else {
            output.line(&say(&Message::Found { entries, bytes }));
        }
        if let Some(receipt) = self.receipt {
            if receipt.outcomes().is_empty() && entries > 0 {
                output.line(&say(&Message::NothingToClean));
            }
            let removed = receipt
                .outcomes()
                .iter()
                .filter(|outcome| matches!(outcome.result(), CleanResult::Removed { .. }))
                .count();
            if !receipt.outcomes().is_empty() {
                output.line(&say(&Message::Cleaned {
                    bytes: receipt.removed_bytes(),
                    entries: removed,
                }));
            }
            for outcome in receipt.outcomes() {
                if let (Some(path), false) = (
                    outcome.path(),
                    matches!(
                        outcome.result(),
                        CleanResult::Removed { .. } | CleanResult::Gone
                    ),
                ) {
                    output.line(&say(&Message::NotRemoved {
                        path,
                        result: outcome.result(),
                        blocked: outcome.blocked(),
                    }));
                }
            }
        }
    }

    pub fn write_json(&self, output: &mut Output) {
        let (entries, bytes) = self.found();
        let document = Document {
            groups: self.shown().collect(),
            issues: self
                .report
                .issues()
                .iter()
                .map(|issue| IssueJson {
                    stops: issue.stops_the_scan(),
                    detail: format!("{issue:?}"),
                })
                .collect(),
            found_entries: entries,
            found_bytes: bytes,
            clean: self.receipt.map(clean_json),
            journal: self.journal.map(|path| path.to_string_lossy().into_owned()),
        };
        match serde_json::to_string_pretty(&document) {
            Ok(json) => output.line(&json),
            Err(error) => {
                tracing::error!(%error, "JSON output failed");
                output.fail(io::Error::other(error));
            }
        }
    }
}

#[derive(Serialize)]
struct Document<'a> {
    groups: Vec<&'a GroupSummary>,
    issues: Vec<IssueJson>,
    found_entries: usize,
    found_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    clean: Option<CleanJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    journal: Option<String>,
}

#[derive(Serialize)]
struct IssueJson {
    stops: bool,
    detail: String,
}

#[derive(Serialize)]
struct CleanJson {
    stops: Vec<String>,
    removed_bytes: u64,
    complete: bool,
    outcomes: Vec<OutcomeJson>,
}

#[derive(Serialize)]
struct OutcomeJson {
    path: Option<String>,
    result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocked: Option<String>,
}

fn clean_json(receipt: &CleanReceipt) -> CleanJson {
    CleanJson {
        stops: receipt
            .stops()
            .iter()
            .map(|stop| format!("{stop:?}"))
            .collect(),
        removed_bytes: receipt.removed_bytes(),
        complete: receipt.is_complete(),
        outcomes: receipt.outcomes().iter().map(outcome_json).collect(),
    }
}

fn outcome_json(outcome: &CleanOutcome) -> OutcomeJson {
    let lossy = |path: &Path| path.to_string_lossy().into_owned();
    let (result, bytes, reason) = match outcome.result() {
        CleanResult::Removed { bytes } => ("removed", Some(bytes), None),
        CleanResult::Partial { bytes, reason } => ("partial", Some(bytes), Some(reason)),
        CleanResult::Gone => ("gone", None, None),
        CleanResult::Kept(reason) => ("kept", None, Some(reason)),
    };
    let (error, code) = match reason {
        Some(KeepReason::Failed { kind, code }) => (Some(format!("{kind:?}")), code),
        _ => (None, None),
    };
    OutcomeJson {
        path: outcome.path().map(lossy),
        result,
        bytes,
        reason: reason.map(reason_name),
        error,
        code,
        blocked: outcome.blocked().map(lossy),
    }
}

/// A stable name for scripts.
const fn reason_name(reason: KeepReason) -> &'static str {
    match reason {
        KeepReason::UnknownItem => "unknown_item",
        KeepReason::Replaced => "replaced",
        KeepReason::Recent => "recent",
        KeepReason::Changed => "changed",
        KeepReason::Excluded => "excluded",
        KeepReason::Protected => "protected",
        KeepReason::Failed { .. } => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_signed_seconds() {
        let after = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5);
        let before = SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(5);
        assert_eq!(seconds(after), 5);
        assert_eq!(seconds(before), -5);
    }

    #[test]
    fn reasons_have_stable_names() {
        assert_eq!(reason_name(KeepReason::Recent), "recent");
        assert_eq!(
            reason_name(KeepReason::Failed {
                kind: io::ErrorKind::PermissionDenied,
                code: Some(13)
            }),
            "failed"
        );
    }
}
