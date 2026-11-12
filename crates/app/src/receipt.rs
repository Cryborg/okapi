//! What a cleaning did, as its receipt shows it: how it ended, what it freed,
//! and its entries by result, the kept ones by reason. Plain Rust over plain
//! facts of each outcome, tested without a cleaning.

use std::path::PathBuf;

use okapi_engine::{CleanOutcome, CleanReceipt, CleanResult, KeepReason, ScanReport};

/// How many paths a line of the receipt lists; the journal has them all.
pub const PATHS_SHOWN: usize = 20;

/// What the receipt needs of one outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeFacts {
    pub result: CleanResult,
    pub path: Option<PathBuf>,
    pub blocked: Option<PathBuf>,
    /// Its size when the analysis saw it.
    pub scanned_bytes: u64,
}

impl OutcomeFacts {
    /// `outcome`, its size taken from the `report` it was cleaned from.
    #[must_use]
    pub fn of(outcome: &CleanOutcome, report: &ScanReport) -> Self {
        Self {
            result: outcome.result(),
            path: outcome.path().map(PathBuf::from),
            blocked: outcome.blocked().map(PathBuf::from),
            scanned_bytes: report.find(outcome.id()).map_or(0, |(_, item)| item.size()),
        }
    }

    /// The facts of every outcome of `receipt`.
    #[must_use]
    pub fn all(receipt: &CleanReceipt, report: &ScanReport) -> Vec<Self> {
        receipt
            .outcomes()
            .iter()
            .map(|outcome| Self::of(outcome, report))
            .collect()
    }
}

/// How a cleaning ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Done,
    /// The user cancelled it: what was not yet removed stays.
    Cancelled,
    /// Something kept it from touching anything.
    Stopped,
}

/// Why entries were kept, as the receipt sorts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kept {
    Recent,
    Changed,
    Replaced,
    Excluded,
    Protected,
    Refused,
    Cancelled,
    /// An identifier from another report: a fault of the interface.
    Unknown,
}

impl Kept {
    const fn of(reason: KeepReason) -> Self {
        match reason {
            KeepReason::Recent => Self::Recent,
            KeepReason::Changed => Self::Changed,
            KeepReason::Replaced => Self::Replaced,
            KeepReason::Excluded => Self::Excluded,
            KeepReason::Protected => Self::Protected,
            KeepReason::Failed { .. } => Self::Refused,
            KeepReason::Cancelled => Self::Cancelled,
            KeepReason::UnknownItem => Self::Unknown,
        }
    }
}

/// What a line of the receipt counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LineKind {
    Removed,
    Partial,
    Gone,
    Kept(Kept),
}

/// One line of the receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptLine {
    pub kind: LineKind,
    pub count: usize,
    /// Freed for the removed ones, in part or whole; their size at the
    /// analysis for the others.
    pub bytes: u64,
    /// Up to [`PATHS_SHOWN`], each with what blocked it, if anything.
    pub paths: Vec<(String, Option<String>)>,
    /// How many more there are than those listed.
    pub more: usize,
}

/// The receipt of a cleaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub ending: Ending,
    pub freed: u64,
    pub removed: usize,
    pub partial: usize,
    pub gone: usize,
    pub kept: usize,
    pub lines: Vec<ReceiptLine>,
}

impl Receipt {
    /// The receipt of a cleaning that `stopped`, or did what `outcomes` say.
    #[must_use]
    pub fn new(stopped: bool, outcomes: &[OutcomeFacts]) -> Self {
        let kind = |result: CleanResult| match result {
            CleanResult::Removed { .. } => LineKind::Removed,
            CleanResult::Partial { .. } => LineKind::Partial,
            CleanResult::Gone => LineKind::Gone,
            CleanResult::Kept(reason) => LineKind::Kept(Kept::of(reason)),
        };
        let mut lines: Vec<ReceiptLine> = Vec::new();
        for outcome in outcomes {
            let line_kind = kind(outcome.result);
            let position = match lines.binary_search_by_key(&line_kind, |line| line.kind) {
                Ok(position) => position,
                Err(position) => {
                    lines.insert(
                        position,
                        ReceiptLine {
                            kind: line_kind,
                            count: 0,
                            bytes: 0,
                            paths: Vec::new(),
                            more: 0,
                        },
                    );
                    position
                }
            };
            let line = &mut lines[position];
            line.count += 1;
            let bytes = match outcome.result {
                CleanResult::Removed { .. } | CleanResult::Partial { .. } => {
                    outcome.result.removed_bytes()
                }
                CleanResult::Gone | CleanResult::Kept(_) => outcome.scanned_bytes,
            };
            line.bytes = line.bytes.saturating_add(bytes);
            if line.paths.len() < PATHS_SHOWN {
                let lossy = |path: &PathBuf| path.display().to_string();
                line.paths.push((
                    outcome.path.as_ref().map(lossy).unwrap_or_default(),
                    outcome.blocked.as_ref().map(lossy),
                ));
            } else {
                line.more += 1;
            }
        }
        let count = |wanted: fn(&LineKind) -> bool| {
            lines
                .iter()
                .filter(|line| wanted(&line.kind))
                .map(|line| line.count)
                .sum()
        };
        let cancelled = lines
            .iter()
            .any(|line| line.kind == LineKind::Kept(Kept::Cancelled));
        Self {
            ending: if stopped {
                Ending::Stopped
            } else if cancelled {
                Ending::Cancelled
            } else {
                Ending::Done
            },
            freed: outcomes
                .iter()
                .map(|outcome| outcome.result.removed_bytes())
                .fold(0, u64::saturating_add),
            removed: count(|kind| *kind == LineKind::Removed),
            partial: count(|kind| *kind == LineKind::Partial),
            gone: count(|kind| *kind == LineKind::Gone),
            kept: count(|kind| matches!(kind, LineKind::Kept(_))),
            lines,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use super::*;

    fn outcome(result: CleanResult, path: &str, scanned_bytes: u64) -> OutcomeFacts {
        OutcomeFacts {
            result,
            path: Some(PathBuf::from(path)),
            blocked: None,
            scanned_bytes,
        }
    }

    #[test]
    fn the_receipt_counts_each_result_and_sorts_the_kept_by_reason() {
        let refused = KeepReason::Failed {
            kind: ErrorKind::PermissionDenied,
            code: Some(13),
        };
        let mut blocked = outcome(
            CleanResult::Partial {
                bytes: 4,
                reason: refused,
            },
            "/p",
            9,
        );
        blocked.blocked = Some(PathBuf::from("/p/in-use"));
        let receipt = Receipt::new(
            false,
            &[
                outcome(CleanResult::Removed { bytes: 3 }, "/a", 3),
                outcome(CleanResult::Removed { bytes: 5 }, "/b", 5),
                blocked,
                outcome(CleanResult::Gone, "/g", 7),
                outcome(CleanResult::Kept(KeepReason::Recent), "/r", 11),
                outcome(CleanResult::Kept(refused), "/f", 13),
            ],
        );
        assert_eq!(receipt.ending, Ending::Done);
        assert_eq!(receipt.freed, 12, "removed whole and in part");
        assert_eq!(
            (receipt.removed, receipt.partial, receipt.gone, receipt.kept),
            (2, 1, 1, 2)
        );
        let lines: Vec<_> = receipt
            .lines
            .iter()
            .map(|line| (line.kind, line.count, line.bytes))
            .collect();
        assert_eq!(
            lines,
            [
                (LineKind::Removed, 2, 8),
                (LineKind::Partial, 1, 4),
                (LineKind::Gone, 1, 7),
                (LineKind::Kept(Kept::Recent), 1, 11),
                (LineKind::Kept(Kept::Refused), 1, 13),
            ]
        );
        assert_eq!(
            receipt.lines[1].paths,
            [(String::from("/p"), Some(String::from("/p/in-use")))],
            "what blocked it"
        );
    }

    #[test]
    fn a_receipt_says_how_the_cleaning_ended() {
        let cancelled = [
            outcome(CleanResult::Removed { bytes: 1 }, "/a", 1),
            outcome(CleanResult::Kept(KeepReason::Cancelled), "/b", 2),
        ];
        assert_eq!(Receipt::new(false, &cancelled).ending, Ending::Cancelled);
        assert_eq!(Receipt::new(true, &[]).ending, Ending::Stopped);
        assert_eq!(Receipt::new(false, &[]).ending, Ending::Done);
    }

    #[test]
    fn a_line_lists_a_bounded_number_of_paths() {
        let many: Vec<_> = (0..=PATHS_SHOWN + 1)
            .map(|index| outcome(CleanResult::Removed { bytes: 1 }, &format!("/{index}"), 1))
            .collect();
        let receipt = Receipt::new(false, &many);
        assert_eq!(receipt.lines[0].paths.len(), PATHS_SHOWN);
        assert_eq!(receipt.lines[0].more, 2);
        assert_eq!(receipt.lines[0].count, PATHS_SHOWN + 2);
    }

    #[test]
    fn the_kept_entries_are_sorted_by_their_own_reason() {
        let reasons = [
            (KeepReason::Recent, Kept::Recent),
            (KeepReason::Changed, Kept::Changed),
            (KeepReason::Replaced, Kept::Replaced),
            (KeepReason::Excluded, Kept::Excluded),
            (KeepReason::Protected, Kept::Protected),
            (
                KeepReason::Failed {
                    kind: ErrorKind::PermissionDenied,
                    code: None,
                },
                Kept::Refused,
            ),
            (KeepReason::Cancelled, Kept::Cancelled),
            (KeepReason::UnknownItem, Kept::Unknown),
        ];
        for (reason, kept) in reasons {
            let receipt = Receipt::new(
                false,
                &[OutcomeFacts {
                    result: CleanResult::Kept(reason),
                    path: None,
                    blocked: None,
                    scanned_bytes: 1,
                }],
            );
            assert_eq!(receipt.lines[0].kind, LineKind::Kept(kept), "{reason:?}");
        }
    }
}
