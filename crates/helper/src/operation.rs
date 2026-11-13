//! The helper's two operations, over the engine: it scans the targets asked
//! for itself, as root, and removes only what that scan offers in them.

use okapi_engine::{
    ItemId, KeepReason, ScanGroup, ScanIssue, ScanItem, ScanOptions, ScanReport, Stop, Watch,
    clean_watched, scan_watched,
};
use okapi_rules::{RuleSet, TemplateVariables};
use okapi_store::HistoryOutcome;

use crate::answer::{Answer, GroupAnswer, IssueAnswer};
use crate::request::{Operation, Selection};

/// Scans the targets of `selection` with `options` and, for a cleaning,
/// removes every entry of the groups it offers ([`admin_groups`]): a group
/// offered is cleaned whole, as in the interface. `watch` cancels either.
/// `rules` are those `selection` was read against.
#[must_use]
pub fn operate(
    operation: Operation,
    selection: &Selection,
    rules: &RuleSet,
    variables: &TemplateVariables,
    options: ScanOptions,
    watch: &Watch,
) -> Answer {
    let _span = tracing::info_span!("operation", ?operation).entered();
    let options = selection.restrict(options);
    let report = scan_watched(rules, variables, &options, watch);
    let (offered, withheld) = admin_groups(&report, selection);
    let mut stops = scan_stops(&report);
    let issues = answer_issues(&report, &withheld, selection);
    let groups = offered
        .iter()
        .map(|group| GroupAnswer::of(group, selection))
        .collect();
    match operation {
        Operation::Scan => Answer::scan(stops, groups, issues),
        Operation::Clean if !stops.is_empty() => {
            tracing::info!(?stops, "cleaning not attempted: the scan stopped");
            Answer::clean(stops, groups, issues, Vec::new())
        }
        Operation::Clean => {
            let ids = ids_of(&offered);
            let receipt = clean_watched(&report, &ids, rules, variables, &options, watch);
            stops.extend(receipt.stops().iter().map(Stop::key));
            let outcomes: Vec<HistoryOutcome> =
                receipt.outcomes().iter().map(HistoryOutcome::of).collect();
            // The engine says a cancelled cleaning entry by entry; the
            // answer says it as a stop, as for a scan.
            if is_cancelled(&outcomes) {
                stops.push(Stop::Cancelled.key());
            }
            Answer::clean(stops, groups, issues, outcomes)
        }
    }
}

/// What stopped the scan, by stable name.
fn scan_stops(report: &ScanReport) -> Vec<&'static str> {
    report
        .issues()
        .iter()
        .filter_map(|issue| match issue {
            ScanIssue::Stopped(stop) => Some(stop.key()),
            _ => None,
        })
        .collect()
}

/// The issues of `report` the answer keeps, then the groups `withheld`.
fn answer_issues(
    report: &ScanReport,
    withheld: &[&ScanGroup],
    selection: &Selection,
) -> Vec<IssueAnswer> {
    report
        .issues()
        .iter()
        .filter_map(|issue| IssueAnswer::of(issue, selection))
        .chain(
            withheld
                .iter()
                .map(|group| IssueAnswer::withheld(group, selection)),
        )
        .collect()
}

/// The entries of `report` the helper removes: those of the groups whose
/// every rule was asked for. A group that joins a rule left out (sharing a
/// root with it) is walked by a scan that chooses one of its rules, but
/// never cleaned, whether the scan chose its rules or not. [`operate`] takes
/// them from the groups it offers; this tests the filter on any report.
#[cfg(test)]
pub(crate) fn cleanable(report: &ScanReport, selection: &Selection) -> Vec<ItemId> {
    ids_of(&admin_groups(report, selection).0)
}

/// Whether the cleaning that gave `outcomes` was cancelled before it was
/// done.
pub(crate) fn is_cancelled(outcomes: &[HistoryOutcome]) -> bool {
    outcomes
        .iter()
        .any(|outcome| outcome.reason == Some(KeepReason::Cancelled.key()))
}

fn ids_of(groups: &[&ScanGroup]) -> Vec<ItemId> {
    groups
        .iter()
        .flat_map(|group| group.items())
        .map(ScanItem::id)
        .collect()
}

/// The groups of `report` with a rule asked for: first those the helper
/// offers and removes, which need the administrator and whose every rule was
/// asked for; then those it withholds, traced. Never a group without a rule,
/// which every rule would otherwise vouch for.
fn admin_groups<'a>(
    report: &'a ScanReport,
    selection: &Selection,
) -> (Vec<&'a ScanGroup>, Vec<&'a ScanGroup>) {
    let asked = report.groups().iter().filter(|group| {
        group
            .origins()
            .iter()
            .any(|origin| selection.contains(origin))
    });
    // A selection holds only targets that need the administrator; the check
    // stays, should it ever hold others.
    let (offered, withheld): (Vec<_>, Vec<_>) = asked.partition(|group| {
        group.needs_admin()
            && group
                .origins()
                .iter()
                .all(|origin| selection.contains(origin))
    });
    for group in &offered {
        tracing::info!(origins = ?group.origins(), entries = group.items().len(), bytes = group.total_size(), "group offered");
    }
    for group in &withheld {
        tracing::warn!(origins = ?group.origins(), entries = group.items().len(), bytes = group.total_size(), "group withheld: it joins a rule not asked for");
    }
    (offered, withheld)
}
