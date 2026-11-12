use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use okapi_platform::IDENTITY_SUPPORTED;
use okapi_rules::{MiscRules, RuleSet, TemplateVariables, Variable};

use crate::options::ScanOptions;
use crate::paths::is_absent_kind;
use crate::protection::ProtectedRoots;
use crate::report::{
    AppFile, FoundItem, GroupAttributes, Origin, ReportBuilder, ScanIssue, ScanReport, ScanStats,
    UnsafeReason,
};
use crate::roots::{PlannedKind, PlannedRoot, Planner, RuleEntry};
use crate::walk::{Found, Limits, Readers, RootProblem, WalkContext, walk};

/// Scans the system targets and the application rules of `rules` for what
/// can be cleaned, without privileges and without changing anything.
///
/// Only directories the rules name are walked, by their real location (the
/// rule's own path is resolved first, links in its ancestors included). Below
/// a root, no link is followed and no other device is entered, and a root
/// never equals or holds a variable's directory, `${TMPDIR}`'s own directory
/// excepted (invariant 6).
///
/// Rules the scan does not handle yet offer nothing: maintenance operations
/// are left to the privileged helper and not reported, recursive and file
/// matchers are reported as [`ScanIssue::Unsupported`].
#[must_use]
pub fn scan(rules: &RuleSet, variables: &TemplateVariables, options: &ScanOptions) -> ScanReport {
    if !IDENTITY_SUPPORTED {
        let mut report = ReportBuilder::new();
        report.issue(ScanIssue::UnsupportedSystem);
        return report.finish();
    }
    // A variable without a value protects nothing (invariant 6): a rule naming
    // `/tmp` would then walk into a `${HOME}` placed below it.
    let missing: Vec<Variable> = Variable::ALL
        .into_iter()
        .filter(|variable| variable.is_available_on(rules.os()))
        .filter(|variable| !variables.iter().any(|(set, _)| set == *variable))
        .collect();
    if !missing.is_empty() {
        let mut report = ReportBuilder::new();
        for variable in missing {
            report.issue(ScanIssue::MissingVariable { variable });
        }
        return report.finish();
    }
    scan_entries(
        &entries_of(rules),
        rules.misc(),
        variables,
        options,
        Readers::SYSTEM,
        Limits::DEFAULT,
    )
}

/// The rules [`scan`] handles, in the order of their files.
fn entries_of(rules: &RuleSet) -> Vec<RuleEntry<'_>> {
    let system = rules.system();
    let apps = [
        (AppFile::Apps, rules.apps()),
        (AppFile::Gaming, rules.gaming()),
        (AppFile::GpuCache, rules.gpu_cache()),
    ];
    system
        .clean_targets()
        .iter()
        .map(RuleEntry::Target)
        .chain(system.single_file_targets().iter().map(RuleEntry::File))
        .chain(
            apps.into_iter().flat_map(|(file, rules)| {
                rules.iter().map(move |rule| RuleEntry::App { file, rule })
            }),
        )
        .collect()
}

/// [`scan`] over an explicit list of rules, with the metadata readers and the
/// limits given.
pub(crate) fn scan_entries(
    entries: &[RuleEntry<'_>],
    misc: &MiscRules,
    variables: &TemplateVariables,
    options: &ScanOptions,
    readers: Readers,
    limits: Limits,
) -> ScanReport {
    let mut report = ReportBuilder::new();
    if options.cutoff(None).is_none() {
        report.issue(ScanIssue::MinAgeBeyondClock);
        return report.finish();
    }
    let protected = match ProtectedRoots::new(variables, readers.identity) {
        Ok(protected) => protected,
        Err(unreadable) => {
            report.issue(ScanIssue::UnreadableVariable {
                variable: unreadable.variable,
                path: unreadable.path,
                error: unreadable.error,
            });
            return report.finish();
        }
    };
    let mut planner = Planner {
        variables,
        options,
        misc,
        protected: &protected,
        readers,
        issues: Vec::new(),
        stats: ScanStats::default(),
    };
    let roots_by_rule: Vec<(usize, PlannedRoot)> = entries
        .iter()
        .enumerate()
        .flat_map(|(rule, entry)| {
            planner
                .plan(*entry)
                .into_iter()
                .map(move |root| (rule, root))
        })
        .collect();
    for issue in planner.issues {
        report.issue(issue);
    }
    report.add_stats(&planner.stats);

    let Grouped {
        roots,
        mut groups,
        attributes,
    } = group_rules(roots_by_rule, entries.len());
    let attributes = merge_attributes(attributes, &mut groups);
    let root_identities: BTreeSet<_> = roots.iter().map(|(_, root)| root.identity).collect();
    let mut found_by_group: BTreeMap<usize, Vec<FoundItem>> = BTreeMap::new();
    for (rule, root) in roots {
        let group = groups.find(rule);
        let origins = attributes
            .get(&group)
            .map(|attributes| attributes.origins.clone())
            .unwrap_or_default();
        let found = match root.kind {
            PlannedKind::File(found) => vec![found],
            PlannedKind::Directory => {
                let context = WalkContext {
                    cutoff: root.cutoff,
                    exclusions: options.exclusions(),
                    misc,
                    protected: &protected,
                    roots: &root_identities,
                    readers,
                    limits,
                };
                walk_root(&mut report, &root, &origins, context)
            }
        };
        found_by_group
            .entry(group)
            .or_default()
            .extend(found.into_iter().map(|found| FoundItem {
                found,
                root: root.path.clone(),
                cutoff: root.cutoff,
            }));
    }
    for (group, attributes) in attributes {
        report.group(
            attributes,
            found_by_group.remove(&group).unwrap_or_default(),
        );
    }
    report.finish()
}

/// Walks one directory root and records in `report` what kept it from being
/// walked whole.
fn walk_root(
    report: &mut ReportBuilder,
    root: &PlannedRoot,
    origins: &[Origin],
    context: WalkContext<'_>,
) -> Vec<Found> {
    let walked = walk(&root.path, root.identity, context);
    report.add_stats(&walked.stats);
    match walked.root_problem {
        // Gone since it was planned: the normal case of an absent root.
        Some(RootProblem::Unreadable(error)) if is_absent_kind(error) => {}
        None => {}
        Some(RootProblem::Unreadable(error)) => report.issue(ScanIssue::UnreadableRoot {
            origins: origins.to_vec(),
            path: root.path.clone(),
            error,
        }),
        Some(RootProblem::Replaced) => report.issue(ScanIssue::UnsafeRoot {
            origins: origins.to_vec(),
            path: root.path.clone(),
            reason: UnsafeReason::Replaced,
        }),
    }
    for limit in walked.limits_reached {
        report.issue(ScanIssue::LimitReached {
            origins: origins.to_vec(),
            root: root.path.clone(),
            limit,
        });
    }
    walked.found
}

/// The attributes of each group, from those of its rules.
fn merge_attributes(
    attributes: BTreeMap<usize, GroupAttributes>,
    groups: &mut RuleGroups,
) -> BTreeMap<usize, GroupAttributes> {
    let mut by_group: BTreeMap<usize, GroupAttributes> = BTreeMap::new();
    for (rule, rule_attributes) in attributes {
        match by_group.entry(groups.find(rule)) {
            Entry::Vacant(slot) => {
                slot.insert(rule_attributes);
            }
            Entry::Occupied(mut slot) => slot.get_mut().merge(rule_attributes),
        }
    }
    by_group
}

/// Rules joined into groups: rules that share a root are one group.
struct RuleGroups {
    parent: Vec<usize>,
}

impl RuleGroups {
    fn new(rules: usize) -> Self {
        Self {
            parent: (0..rules).collect(),
        }
    }

    /// The group of `rule`: the first rule of the group, in rule order.
    fn find(&mut self, rule: usize) -> usize {
        let mut current = rule;
        while self.parent[current] != current {
            self.parent[current] = self.parent[self.parent[current]];
            current = self.parent[current];
        }
        current
    }

    fn join(&mut self, first: usize, second: usize) {
        let (first, second) = (self.find(first), self.find(second));
        self.parent[first.max(second)] = first.min(second);
    }
}

struct Grouped {
    /// Each root once, with the rule it is first found for.
    roots: Vec<(usize, PlannedRoot)>,
    groups: RuleGroups,
    /// The group attributes of every rule that has a root, by rule.
    attributes: BTreeMap<usize, GroupAttributes>,
}

/// Gathers the roots of all rules: a root shared by several rules is walked
/// once, with the strictest of their cutoffs, and joins their groups. A group
/// is shown in the place of its first rule.
fn group_rules(roots_by_rule: Vec<(usize, PlannedRoot)>, rules: usize) -> Grouped {
    let mut grouped = Grouped {
        roots: Vec::new(),
        groups: RuleGroups::new(rules),
        attributes: BTreeMap::new(),
    };
    let mut index_of = BTreeMap::new();
    for (rule, root) in roots_by_rule {
        grouped
            .attributes
            .entry(rule)
            .or_insert_with(|| root.attributes.clone());
        if let Some(&index) = index_of.get(&root.identity) {
            let (first_rule, existing): &mut (usize, PlannedRoot) = &mut grouped.roots[index];
            existing.cutoff = existing.cutoff.min(root.cutoff);
            let first_rule = *first_rule;
            grouped.groups.join(first_rule, rule);
        } else {
            index_of.insert(root.identity, grouped.roots.len());
            grouped.roots.push((rule, root));
        }
    }
    grouped
}

#[cfg(test)]
mod tests {
    use okapi_rules::{Os, RuleSet};

    use super::*;

    #[test]
    fn every_handled_rule_is_scanned_in_file_order() {
        let rules = RuleSet::embedded(Os::Linux).unwrap();
        let entries = entries_of(&rules);
        let system = rules.system();
        let expected = system.clean_targets().len()
            + system.single_file_targets().len()
            + rules.apps().len()
            + rules.gaming().len()
            + rules.gpu_cache().len();
        assert_eq!(entries.len(), expected);
        assert!(matches!(entries.first(), Some(RuleEntry::Target(_))));
        assert!(matches!(
            entries.last(),
            Some(RuleEntry::App {
                file: AppFile::GpuCache,
                ..
            })
        ));
        let files: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                RuleEntry::App { file, .. } => Some(*file),
                RuleEntry::Target(_) | RuleEntry::File(_) => None,
            })
            .collect();
        assert!(files.is_sorted(), "apps, then gaming, then GPU caches");
        assert!(
            entries
                .iter()
                .any(|entry| matches!(entry, RuleEntry::File(_)))
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn a_system_without_file_identity_scans_nothing() {
        let rules = RuleSet::embedded(Os::Windows).unwrap();
        let options = ScanOptions::new(
            std::time::SystemTime::now(),
            crate::DEFAULT_MIN_AGE,
            crate::Exclusions::none(),
        );
        let report = scan(&rules, &TemplateVariables::new(), &options);
        assert_eq!(report.issues(), [ScanIssue::UnsupportedSystem]);
        assert!(report.groups().is_empty());
    }
}
