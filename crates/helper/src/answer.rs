//! What the helper answers, one JSON document on its standard output.

use std::path::Path;

use okapi_engine::{Origin, ScanGroup, ScanIssue};
use okapi_store::HistoryOutcome;
use serde::Serialize;

use crate::request::{Operation, Refusal, Selection};
use crate::{Exit, PROTOCOL_VERSION};

/// What a scan offers in the targets asked for and, for a cleaning, what
/// became of each entry.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    version: u32,
    operation: Operation,
    /// What stopped the scan or the cleaning, by its stable name
    /// (`okapi_engine::Stop::key`): a cleaning cancelled midway keeps what it
    /// removed, its outcomes say which. Unlike a history line's stops (what
    /// kept a cleaning from touching anything), never to copy there as is.
    stops: Vec<&'static str>,
    groups: Vec<GroupAnswer>,
    issues: Vec<IssueAnswer>,
    /// For a cleaning only: each entry, as the history keeps it.
    #[serde(skip_serializing_if = "Option::is_none")]
    outcomes: Option<Vec<HistoryOutcome>>,
}

impl Answer {
    pub(crate) const fn scan(
        stops: Vec<&'static str>,
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
        stops: Vec<&'static str>,
        groups: Vec<GroupAnswer>,
        issues: Vec<IssueAnswer>,
        outcomes: Vec<HistoryOutcome>,
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
}

/// A group of entries offered: the targets asked for that it gathers.
#[derive(Debug, Serialize)]
pub(crate) struct GroupAnswer {
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
}

/// Something the scan could not do, or chose not to do, for a target asked
/// for, and every stop; `detail` is for the journal, not to act on.
#[derive(Debug, Serialize)]
pub(crate) struct IssueAnswer {
    /// Its stable name (`ScanIssue::key`, or [`WITHHELD`]).
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<&'static str>,
    targets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// A name in `path` is not UTF-8: its bad bytes were replaced.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
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
            ScanIssue::Stopped(stop) => (Some(stop.key()), None, None),
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
            kind: issue.key(),
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
            kind: WITHHELD,
            stop: None,
            targets: names(group.origins(), selection),
            path: None,
            detail: None,
            inexact: false,
        }
    }
}

/// Why a request was refused: nothing was read nor touched. The versions say
/// whether the helper installed is the one the caller expects.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefusalAnswer {
    version: u32,
    helper_version: &'static str,
    /// Its stable name ([`Refusal::key`]).
    refused: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    detail: String,
}

impl RefusalAnswer {
    #[must_use]
    pub fn of(refusal: &Refusal) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            helper_version: env!("CARGO_PKG_VERSION"),
            refused: refusal.key(),
            target: refusal.target().map(str::to_owned),
            detail: refusal.to_string(),
        }
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
