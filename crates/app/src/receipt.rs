//! What a cleaning did, as its receipt shows it: how it ended, what it freed,
//! and its entries by result, the kept ones by reason. Plain Rust over plain
//! facts of each outcome, tested without a cleaning.

use std::io::ErrorKind;
use std::path::PathBuf;

use okapi_engine::{CleanOutcome, CleanReceipt, CleanResult, KeepReason, ScanReport};
use okapi_helper::OutcomeAnswer;
use okapi_store::HistoryOutcome;

/// How many paths a line of the receipt lists; the journal has them all.
pub const PATHS_SHOWN: usize = 20;

/// What the receipt needs of one outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeFacts {
    pub result: CleanResult,
    pub path: Option<PathBuf>,
    pub blocked: Option<PathBuf>,
    /// Its size when the analysis saw it; unknown to a history.
    pub scanned_bytes: Option<u64>,
}

impl OutcomeFacts {
    /// `outcome`, its size taken from the `report` it was cleaned from.
    #[must_use]
    pub fn of(outcome: &CleanOutcome, report: &ScanReport) -> Self {
        Self {
            result: outcome.result(),
            path: outcome.path().map(PathBuf::from),
            blocked: outcome.blocked().map(PathBuf::from),
            scanned_bytes: Some(report.size_of(outcome.id()).unwrap_or_default()),
        }
    }

    /// One entry of a deletion log the history keeps: `Err` when its result
    /// cannot be read back, then counted as [`HistoryOutcome::UNREADABLE`]
    /// without a trace of its own (a log may hold thousands).
    ///
    /// # Errors
    ///
    /// The entry counted kept by the system, its result unreadable.
    pub fn of_history(kept: &HistoryOutcome) -> Result<Self, Self> {
        let facts = |result| Self {
            result,
            path: kept.path.as_ref().map(PathBuf::from),
            blocked: kept.blocked.as_ref().map(PathBuf::from),
            scanned_bytes: None,
        };
        kept.clean_result()
            .map(facts)
            .ok_or_else(|| facts(HistoryOutcome::UNREADABLE))
    }

    /// What root's part of a cleaning answered for one entry. A result that
    /// cannot be read back is shown as refused by the system, of unknown
    /// kind: never as an entry of another report, which would be a fault of
    /// the interface.
    #[must_use]
    pub fn of_helper(answered: &OutcomeAnswer) -> Self {
        let outcome = answered.outcome();
        Self {
            result: outcome.clean_result_or_refused(),
            path: outcome.path.as_ref().map(PathBuf::from),
            blocked: outcome.blocked.as_ref().map(PathBuf::from),
            scanned_bytes: Some(answered.scanned_bytes()),
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
    /// A part ended without saying what it did, and nothing else is known:
    /// what it removed is only in the log.
    Interrupted,
}

/// Why entries were kept, as the receipt sorts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kept {
    Recent,
    Changed,
    Replaced,
    Excluded,
    Protected,
    /// The system refused the permission: another account's, or the
    /// system's, which Okapi never forces (no rights asked for again).
    Denied,
    /// The system refused for another reason, or could not tell what the
    /// cleaning must check.
    Refused,
    Cancelled,
    /// Its browser was open.
    BrowserOpen,
    /// The folder the user chose it is in was taken out of the settings.
    NoLongerChosen,
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
            KeepReason::Failed {
                kind: ErrorKind::PermissionDenied,
                ..
            } => Self::Denied,
            KeepReason::Failed { .. } => Self::Refused,
            KeepReason::Cancelled => Self::Cancelled,
            KeepReason::BrowserOpen => Self::BrowserOpen,
            KeepReason::NoLongerChosen => Self::NoLongerChosen,
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
    /// analysis for the others, unknown when one of them has none.
    pub bytes: Option<u64>,
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

/// The lines of a receipt, made one outcome at a time: never more than
/// [`PATHS_SHOWN`] paths a line, however many outcomes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiptLines {
    lines: Vec<ReceiptLine>,
    freed: u64,
}

impl ReceiptLines {
    /// Counts `outcome` in its line.
    pub fn add(&mut self, outcome: &OutcomeFacts) {
        let line_kind = match outcome.result {
            CleanResult::Removed { .. } => LineKind::Removed,
            CleanResult::Partial { .. } => LineKind::Partial,
            CleanResult::Gone => LineKind::Gone,
            CleanResult::Kept(reason) => LineKind::Kept(Kept::of(reason)),
        };
        let position = match self
            .lines
            .binary_search_by_key(&line_kind, |line| line.kind)
        {
            Ok(position) => position,
            Err(position) => {
                self.lines.insert(
                    position,
                    ReceiptLine {
                        kind: line_kind,
                        count: 0,
                        bytes: Some(0),
                        paths: Vec::new(),
                        more: 0,
                    },
                );
                position
            }
        };
        let line = &mut self.lines[position];
        line.count += 1;
        let bytes = match outcome.result {
            CleanResult::Removed { .. } | CleanResult::Partial { .. } => {
                Some(outcome.result.removed_bytes())
            }
            CleanResult::Gone | CleanResult::Kept(_) => outcome.scanned_bytes,
        };
        line.bytes = line
            .bytes
            .zip(bytes)
            .map(|(sum, bytes)| sum.saturating_add(bytes));
        if line.paths.len() < PATHS_SHOWN {
            let lossy = |path: &PathBuf| path.display().to_string();
            line.paths.push((
                outcome.path.as_ref().map(lossy).unwrap_or_default(),
                outcome.blocked.as_ref().map(lossy),
            ));
        } else {
            line.more += 1;
        }
        self.freed = self.freed.saturating_add(outcome.result.removed_bytes());
    }

    /// The lines, by result, the kept ones by reason.
    #[must_use]
    pub fn lines(&self) -> &[ReceiptLine] {
        &self.lines
    }
}

impl Receipt {
    /// The receipt of a cleaning that `stopped`, or did what `outcomes` say.
    #[must_use]
    pub fn new(stopped: bool, outcomes: &[OutcomeFacts]) -> Self {
        let mut made = ReceiptLines::default();
        for outcome in outcomes {
            made.add(outcome);
        }
        let ReceiptLines { lines, freed } = made;
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
            freed,
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
    use super::*;

    /// What root's part answered for an entry is what the receipt shows of
    /// it; one that cannot be read back is kept by the system, never one of
    /// another report.
    #[test]
    fn roots_outcomes_are_shown_as_answered() {
        let answered: OutcomeAnswer = serde_json::from_str(
            r#"{"outcome": {"path": "/var/crash", "result": "partial", "bytes": 5,
                            "reason": "failed", "code": 13, "blocked": "/var/crash/locked"},
                "scannedBytes": 9}"#,
        )
        .unwrap();
        assert_eq!(
            OutcomeFacts::of_helper(&answered),
            OutcomeFacts {
                result: CleanResult::Partial {
                    bytes: 5,
                    reason: KeepReason::Failed {
                        kind: ErrorKind::PermissionDenied,
                        code: Some(13)
                    }
                },
                path: Some(PathBuf::from("/var/crash")),
                blocked: Some(PathBuf::from("/var/crash/locked")),
                scanned_bytes: Some(9),
            }
        );
        let unreadable: OutcomeAnswer = serde_json::from_str(
            r#"{"outcome": {"path": "/var/crash", "result": "vanished"}, "scannedBytes": 1}"#,
        )
        .unwrap();
        assert_eq!(
            OutcomeFacts::of_helper(&unreadable).result,
            CleanResult::Kept(KeepReason::Failed {
                kind: ErrorKind::Other,
                code: None
            })
        );
    }

    fn outcome(result: CleanResult, path: &str, scanned_bytes: u64) -> OutcomeFacts {
        OutcomeFacts {
            result,
            path: Some(PathBuf::from(path)),
            blocked: None,
            scanned_bytes: Some(scanned_bytes),
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
                (LineKind::Removed, 2, Some(8)),
                (LineKind::Partial, 1, Some(4)),
                (LineKind::Gone, 1, Some(7)),
                (LineKind::Kept(Kept::Recent), 1, Some(11)),
                (LineKind::Kept(Kept::Denied), 1, Some(13)),
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

    /// A refusal root met reaches the receipt by its number alone (the
    /// helper's answer): the sticky bit's or a lock's `EPERM`, and
    /// `EACCES`, are refused permissions, as the user's own.
    #[test]
    fn a_refusal_of_roots_is_read_back_by_its_number() {
        for code in [1, 13] {
            let reason = KeepReason::from_key("failed", Some(code)).unwrap();
            assert_eq!(Kept::of(reason), Kept::Denied, "{code}");
        }
        let unknown = KeepReason::from_key("failed", None).unwrap();
        assert_eq!(Kept::of(unknown), Kept::Refused);
    }

    /// The history says what the receipt said: a refused permission it keeps
    /// by its number is one Okapi never forces (D-19).
    #[test]
    fn a_refusal_the_history_keeps_is_read_back_by_its_number() {
        for (code, kept) in [
            (Some(1), Kept::Denied),
            (Some(13), Kept::Denied),
            (Some(5), Kept::Refused),
            (None, Kept::Refused),
        ] {
            let entry = HistoryOutcome {
                path: Some("/var/crash/locked".to_owned()),
                result: "kept".to_owned(),
                bytes: None,
                reason: Some("failed".to_owned()),
                error: None,
                code,
                blocked: None,
                inexact: false,
            };
            let mut lines = ReceiptLines::default();
            lines.add(&OutcomeFacts::of_history(&entry).unwrap());
            assert_eq!(lines.lines()[0].kind, LineKind::Kept(kept), "{code:?}");
        }
    }

    /// What a history keeps of an entry: its result, never its size at the
    /// analysis; a result it cannot read back is kept by the system.
    #[test]
    fn an_entry_of_the_history_has_no_size_at_the_analysis() {
        let kept = |result: &str, bytes: Option<u64>, reason: Option<&str>| HistoryOutcome {
            path: Some("/var/tmp/x".to_owned()),
            result: result.to_owned(),
            bytes,
            reason: reason.map(str::to_owned),
            error: None,
            code: None,
            blocked: Some("/var/tmp/x/in-use".to_owned()),
            inexact: false,
        };
        let removed = OutcomeFacts::of_history(&kept("removed", Some(4), None)).unwrap();
        assert_eq!(
            removed,
            OutcomeFacts {
                result: CleanResult::Removed { bytes: 4 },
                path: Some(PathBuf::from("/var/tmp/x")),
                blocked: Some(PathBuf::from("/var/tmp/x/in-use")),
                scanned_bytes: None,
            }
        );
        let odd = OutcomeFacts::of_history(&kept("vanished", None, None)).unwrap_err();
        assert_eq!(odd.result, HistoryOutcome::UNREADABLE);
        let recent = OutcomeFacts::of_history(&kept("kept", None, Some("recent"))).unwrap();
        let mut lines = ReceiptLines::default();
        for facts in [&removed, &removed, &recent] {
            lines.add(facts);
        }
        let read: Vec<_> = lines
            .lines()
            .iter()
            .map(|line| (line.kind, line.count, line.bytes))
            .collect();
        assert_eq!(
            read,
            [
                (LineKind::Removed, 2, Some(8)),
                (LineKind::Kept(Kept::Recent), 1, None)
            ],
            "what was removed is known, what was kept is not"
        );
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
                Kept::Denied,
            ),
            (
                KeepReason::Failed {
                    kind: ErrorKind::ResourceBusy,
                    code: None,
                },
                Kept::Refused,
            ),
            (KeepReason::Cancelled, Kept::Cancelled),
            (KeepReason::BrowserOpen, Kept::BrowserOpen),
            (KeepReason::NoLongerChosen, Kept::NoLongerChosen),
            (KeepReason::UnknownItem, Kept::Unknown),
        ];
        for (reason, kept) in reasons {
            let receipt = Receipt::new(
                false,
                &[OutcomeFacts {
                    result: CleanResult::Kept(reason),
                    path: None,
                    blocked: None,
                    scanned_bytes: Some(1),
                }],
            );
            assert_eq!(receipt.lines[0].kind, LineKind::Kept(kept), "{reason:?}");
        }
    }
}
