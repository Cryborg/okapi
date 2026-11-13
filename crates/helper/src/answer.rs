//! What the helper answers, one JSON document on its standard output, which
//! its caller reads back with the same types.

use std::path::Path;

use okapi_engine::{Origin, ScanGroup, ScanIssue};
use okapi_store::HistoryOutcome;
use serde::{Deserialize, Serialize};

use crate::request::{Operation, Refusal, Selection};
use crate::{Exit, PROTOCOL_VERSION};

/// What a scan offers in the targets asked for and, for a cleaning, what
/// became of each entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Answer {
    version: u32,
    operation: Operation,
    /// What stopped the scan or the cleaning, by its stable name
    /// (`okapi_engine::Stop::key`): a cleaning cancelled midway keeps what it
    /// removed, its outcomes say which. Unlike a history line's stops (what
    /// kept a cleaning from touching anything), never to copy there as is.
    stops: Vec<String>,
    groups: Vec<GroupAnswer>,
    issues: Vec<IssueAnswer>,
    /// For a cleaning only: each entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcomes: Option<Vec<OutcomeAnswer>>,
}

impl Answer {
    pub(crate) const fn scan(
        stops: Vec<String>,
        groups: Vec<GroupAnswer>,
        issues: Vec<IssueAnswer>,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            operation: Operation::Scan,
            stops,
            groups,
            issues,
            outcomes: None,
        }
    }

    pub(crate) const fn clean(
        stops: Vec<String>,
        groups: Vec<GroupAnswer>,
        issues: Vec<IssueAnswer>,
        outcomes: Vec<OutcomeAnswer>,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            operation: Operation::Clean,
            stops,
            groups,
            issues,
            outcomes: Some(outcomes),
        }
    }

    /// How the helper ends: [`Exit::Failure`] when something stopped the
    /// scan or the cleaning, else [`Exit::Success`], entries kept or not.
    #[must_use]
    pub const fn exit(&self) -> Exit {
        if self.stops.is_empty() {
            Exit::Success
        } else {
            Exit::Failure
        }
    }

    #[must_use]
    pub const fn operation(&self) -> Operation {
        self.operation
    }

    #[must_use]
    pub fn stops(&self) -> &[String] {
        &self.stops
    }

    #[must_use]
    pub fn groups(&self) -> &[GroupAnswer] {
        &self.groups
    }

    #[must_use]
    pub fn issues(&self) -> &[IssueAnswer] {
        &self.issues
    }

    /// Each entry of a cleaning; none for a scan.
    #[must_use]
    pub fn outcomes(&self) -> Option<&[OutcomeAnswer]> {
        self.outcomes.as_deref()
    }
}

/// A group of entries offered: the targets asked for that it gathers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupAnswer {
    targets: Vec<String>,
    entries: usize,
    bytes: u64,
}

impl GroupAnswer {
    pub(crate) fn of(group: &ScanGroup, selection: &Selection) -> Self {
        Self {
            targets: names(group.origins(), selection),
            entries: group.items().len(),
            bytes: group.total_size(),
        }
    }

    /// The paths of the targets it gathers, as they were asked.
    #[must_use]
    pub fn targets(&self) -> &[String] {
        &self.targets
    }

    #[must_use]
    pub const fn entries(&self) -> usize {
        self.entries
    }

    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Something the scan could not do, or chose not to do, for a target asked
/// for, and every stop; `detail` is for the journal, not to act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueAnswer {
    /// Its stable name (`ScanIssue::key`, or [`WITHHELD`]).
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stop: Option<String>,
    targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// A name in `path` is not UTF-8: its bad bytes were replaced.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    inexact: bool,
}

/// The kind of the issue of a group the helper does not offer: it joins a
/// rule not asked for (a root shared with it).
pub(crate) const WITHHELD: &str = "withheld";

impl IssueAnswer {
    /// `issue`, if it stopped the scan or concerns a target asked for.
    pub(crate) fn of(issue: &ScanIssue, selection: &Selection) -> Option<Self> {
        let targets = names(issue.origins(), selection);
        if !issue.stops_the_scan() && targets.is_empty() {
            return None;
        }
        let (stop, path, detail): (_, Option<&Path>, _) = match issue {
            ScanIssue::Stopped(stop) => (Some(stop.key().to_owned()), None, None),
            ScanIssue::AgeBeyondClock { .. } | ScanIssue::Unsupported { .. } => (None, None, None),
            ScanIssue::Excluded { path, .. } => (None, Some(path), None),
            ScanIssue::Unresolvable { error, .. } => (None, None, Some(error.to_string())),
            ScanIssue::UnreadableRoot { path, error, .. } => {
                (None, Some(path), Some(error.to_string()))
            }
            ScanIssue::UnsafeRoot { path, reason, .. } => {
                (None, Some(path), Some(format!("{reason:?}")))
            }
            ScanIssue::ProtectedRoot { path, variable, .. } => {
                (None, Some(path), Some(variable.name().to_owned()))
            }
            ScanIssue::LimitReached { root, limit, .. } => {
                (None, Some(root), Some(format!("{limit:?}")))
            }
        };
        let inexact = path.is_some_and(|path| path.to_str().is_none());
        if inexact {
            tracing::warn!(?path, "a name not in UTF-8 is answered inexactly");
        }
        Some(Self {
            kind: issue.key().to_owned(),
            stop,
            targets,
            path: path.map(|path| path.to_string_lossy().into_owned()),
            detail,
            inexact,
        })
    }

    /// A group walked for a target asked for but not offered: it joins a
    /// rule not asked for.
    pub(crate) fn withheld(group: &ScanGroup, selection: &Selection) -> Self {
        Self {
            kind: WITHHELD.to_owned(),
            stop: None,
            targets: names(group.origins(), selection),
            path: None,
            detail: None,
            inexact: false,
        }
    }

    /// The targets asked for it concerns; none for a stop.
    #[must_use]
    pub fn targets(&self) -> &[String] {
        &self.targets
    }
}

/// What became of one entry of a cleaning, as the history keeps it, with
/// its size when root's scan saw it (a kept entry is shown at that size).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutcomeAnswer {
    outcome: HistoryOutcome,
    scanned_bytes: u64,
}

impl OutcomeAnswer {
    pub(crate) const fn new(outcome: HistoryOutcome, scanned_bytes: u64) -> Self {
        Self {
            outcome,
            scanned_bytes,
        }
    }

    #[must_use]
    pub const fn outcome(&self) -> &HistoryOutcome {
        &self.outcome
    }

    #[must_use]
    pub const fn scanned_bytes(&self) -> u64 {
        self.scanned_bytes
    }
}

/// Why a request was refused: nothing was read nor touched. The versions say
/// whether the helper installed is the one the caller expects. Read whatever
/// its version, fields it adds included: a refusal is what another version
/// answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefusalAnswer {
    version: u32,
    helper_version: String,
    /// Its stable name ([`Refusal::key`]).
    refused: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    detail: String,
}

impl RefusalAnswer {
    #[must_use]
    pub fn of(refusal: &Refusal) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            helper_version: env!("CARGO_PKG_VERSION").to_owned(),
            refused: refusal.key().to_owned(),
            target: refusal.target().map(str::to_owned),
            detail: refusal.to_string(),
        }
    }

    /// The stable name of the refusal ([`Refusal::key`]).
    #[must_use]
    pub fn refused(&self) -> &str {
        &self.refused
    }

    /// Whether installing the helper again is the likely cure: it does not
    /// read the request's version, or does not know a target of the caller's
    /// rules (most likely another version's rules).
    #[must_use]
    pub fn asks_to_reinstall(&self) -> bool {
        [
            Refusal::UnsupportedVersion { version: 0 },
            Refusal::UnknownTarget {
                target: String::new(),
            },
        ]
        .iter()
        .any(|refusal| refusal.key() == self.refused)
    }

    #[must_use]
    pub fn helper_version(&self) -> &str {
        &self.helper_version
    }

    #[must_use]
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }
}

/// The paths the targets asked for among `origins` were asked by.
fn names(origins: &[Origin], selection: &Selection) -> Vec<String> {
    origins
        .iter()
        .filter_map(|origin| selection.name_of(origin))
        .map(str::to_owned)
        .collect()
}
