use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use okapi_platform::IDENTITY_SUPPORTED;
use okapi_rules::{MiscRules, RuleSet, TemplateVariables};

use crate::options::ScanOptions;
use crate::paths::is_absent_kind;
use crate::preconditions::{protection, stops_before_reading};
use crate::protection::ProtectedRoots;
use crate::report::{
    AppFile, Category, FoundItem, GroupAttributes, Origin, ReportBuilder, ScanIssue, ScanReport,
    ScanStats, UnsafeReason,
};
use crate::roots::{PlannedKind, PlannedRoot, Planner, RuleEntry};
use crate::walk::{Found, Limits, Readers, RootProblem, ScanProgress, WalkContext, walk};
use crate::watch::{Progress, Watch};

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
    scan_watched(rules, variables, options, &Watch::none())
}

/// [`scan`], its progress sent to `watch` and cancelled by its token: a
/// cancelled scan offers nothing (a directory half read must not pass for
/// whole), its report holding [`Stop::Cancelled`](crate::Stop::Cancelled) and
/// what it counted so far.
#[must_use]
pub fn scan_watched(
    rules: &RuleSet,
    variables: &TemplateVariables,
    options: &ScanOptions,
    watch: &Watch,
) -> ScanReport {
    let user = okapi_platform::effective_user();
    let _span = tracing::info_span!("scan", os = ?rules.os(), effective_user = ?user).entered();
    let started = Instant::now();
    let stops = stops_before_reading(IDENTITY_SUPPORTED, rules.os(), variables);
    let report = if stops.is_empty() {
        scan_entries(
            &entries_of(rules),
            rules.misc(),
            variables,
            options,
            Readers::SYSTEM,
            Limits::DEFAULT,
            user,
            watch,
        )
    } else {
        let mut report = ReportBuilder::new();
        for stop in stops {
            report.issue(ScanIssue::Stopped(stop));
        }
        report.finish()
    };
    let groups = report.groups();
    tracing::info!(
        groups = groups.len(),
        entries = groups.iter().map(|group| group.items().len()).sum::<usize>(),
        bytes = report.total_size(),
        issues = report.issues().len(),
        stats = ?report.stats(),
        elapsed = ?started.elapsed(),
        "scan finished"
    );
    report
}

/// How a category is offered before a scan is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultChoice {
    /// One of its rules is selected by default: chosen.
    Chosen,
    /// None of its rules is (cache resets and maintenance operations, or no
    /// rule at all): left to the user (invariant 5).
    ToChoose,
}

impl Category {
    /// How `self` is offered before a scan of `rules`.
    #[must_use]
    pub fn default_choice(self, rules: &RuleSet) -> DefaultChoice {
        let chosen = entries_of(rules)
            .iter()
            .any(|entry| entry.category() == self && entry.is_selected_by_default());
        if chosen {
            DefaultChoice::Chosen
        } else {
            DefaultChoice::ToChoose
        }
    }
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

/// [`scan_watched`] over an explicit list of rules, with the metadata
/// readers, the limits and the user who will remove what is offered given.
#[expect(
    clippy::too_many_arguments,
    reason = "each one a seam the tests set apart; a struct would only rename them"
)]
pub(crate) fn scan_entries(
    entries: &[RuleEntry<'_>],
    misc: &MiscRules,
    variables: &TemplateVariables,
    options: &ScanOptions,
    readers: Readers,
    limits: Limits,
    user: Option<u32>,
    watch: &Watch,
) -> ScanReport {
    let mut report = ReportBuilder::new();
    if watch.is_cancelled() {
        report.cancel(0, 0, 0);
        return report.finish();
    }
    let Some((protected, roots_by_rule)) =
        plan(entries, misc, variables, options, readers, &mut report)
    else {
        return report.finish();
    };

    let Grouped {
        roots,
        mut groups,
        attributes,
    } = group_rules(roots_by_rule, entries.len());
    let attributes = merge_attributes(attributes, &mut groups);
    // Every root reserves its place, chosen or not: the walks of the chosen
    // ones skip the others below them.
    let root_identities: BTreeSet<_> = roots.iter().map(|(_, root)| root.identity).collect();
    let roots = chosen_roots(roots, &attributes, &mut groups, options.categories());
    let roots_total = roots
        .iter()
        .filter(|(_, root, _)| root.kind != PlannedKind::Withheld)
        .count();
    watch.emit(Progress::Planned {
        roots: roots_by_category(&roots),
        total: roots_total,
    });
    let mut progress = ScanProgress::new(watch);
    let mut roots_done = 0;
    let mut found_by_group: BTreeMap<usize, Vec<FoundItem>> = BTreeMap::new();
    for (rule, root, categories) in roots {
        if watch.is_cancelled() {
            break;
        }
        let group = groups.find(rule);
        let group_attributes = attributes.get(&group);
        let origins = group_attributes
            .map(|attributes| attributes.origins.clone())
            .unwrap_or_default();
        progress.issues = report.issue_count();
        // What needs the administrator will be removed by root, through the
        // privileged helper: the sticky bit does not stand in its way.
        let needs_admin = group_attributes.is_some_and(|attributes| attributes.needs_admin);
        let remover = if needs_admin { None } else { user };
        let withheld = root.kind == PlannedKind::Withheld;
        let found = match root.kind {
            PlannedKind::File(found) => {
                progress.found_bytes = progress.found_bytes.saturating_add(found.size);
                vec![found]
            }
            PlannedKind::Withheld => Vec::new(),
            PlannedKind::Directory => {
                let context = WalkContext {
                    cutoff: root.cutoff,
                    exclusions: options.exclusions(),
                    misc,
                    protected: &protected,
                    roots: &root_identities,
                    readers,
                    limits,
                    remover,
                };
                walk_root(&mut report, &root, &origins, context, &mut progress)
            }
        };
        if watch.is_cancelled() {
            // Cancelled while or after walking this root: not counted as
            // done, and nothing of the scan is offered.
            break;
        }
        if !withheld {
            roots_done += 1;
            watch.emit(Progress::RootDone {
                categories,
                entries: found.len(),
                bytes: found
                    .iter()
                    .map(|found| found.size)
                    .fold(0, u64::saturating_add),
                issues: report.issue_count(),
            });
        }
        found_by_group
            .entry(group)
            .or_default()
            .extend(found.into_iter().map(|found| FoundItem {
                found,
                root: root.path.clone(),
                root_identity: root.identity,
                cutoff: root.cutoff,
            }));
    }
    if watch.is_cancelled() {
        report.cancel(roots_done, roots_total, progress.entries_seen());
        return report.finish();
    }
    report.roots(root_identities);
    for (group, attributes) in attributes {
        report.group(
            attributes,
            found_by_group.remove(&group).unwrap_or_default(),
        );
    }
    report.finish()
}

/// The roots the rules name, checked, each with the rule it comes from; what
/// kept a root out is recorded in `report`. `None` when the variables'
/// directories cannot be told (the scan then stops).
fn plan(
    entries: &[RuleEntry<'_>],
    misc: &MiscRules,
    variables: &TemplateVariables,
    options: &ScanOptions,
    readers: Readers,
    report: &mut ReportBuilder,
) -> Option<(ProtectedRoots, Vec<(usize, PlannedRoot)>)> {
    let protected = match protection(variables, options, readers.identity) {
        Ok((_, protected)) => protected,
        Err(stop) => {
            report.issue(ScanIssue::Stopped(stop));
            return None;
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
    Some((protected, roots_by_rule))
}

/// The roots of a group in the categories `chosen` (all when none is), each
/// with its categories: those of its group's rules (the sense a group is
/// shown in).
fn chosen_roots(
    roots: Vec<(usize, PlannedRoot)>,
    attributes: &BTreeMap<usize, GroupAttributes>,
    groups: &mut RuleGroups,
    chosen: &BTreeSet<Category>,
) -> Vec<(usize, PlannedRoot, BTreeSet<Category>)> {
    roots
        .into_iter()
        .map(|(rule, root)| {
            let categories = attributes
                .get(&groups.find(rule))
                .map(|attributes| Category::of_group(&attributes.origins))
                .unwrap_or_default();
            (rule, root, categories)
        })
        .filter(|(_, _, categories)| Category::is_chosen(categories, chosen))
        .collect()
}

/// How many roots a scan walks or offers, by category: a withheld root is
/// neither, and a root counts for each of its categories.
fn roots_by_category(
    roots: &[(usize, PlannedRoot, BTreeSet<Category>)],
) -> BTreeMap<Category, usize> {
    let mut counted = BTreeMap::new();
    for (_, _, categories) in roots
        .iter()
        .filter(|(_, root, _)| root.kind != PlannedKind::Withheld)
    {
        for category in categories {
            *counted.entry(*category).or_default() += 1;
        }
    }
    counted
}

/// Walks one directory root and records in `report` what kept it from being
/// walked whole.
fn walk_root(
    report: &mut ReportBuilder,
    root: &PlannedRoot,
    origins: &[Origin],
    context: WalkContext<'_>,
    progress: &mut ScanProgress<'_>,
) -> Vec<Found> {
    let walked = walk(&root.path, root.identity, context, progress);
    tracing::debug!(
        root = %root.path.display(),
        found = walked.found.len(),
        limits = ?walked.limits_reached,
        "root walked"
    );
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
            tracing::debug!(
                path = %existing.path.display(),
                joined = %root.path.display(),
                withheld = existing.kind == PlannedKind::Withheld || root.kind == PlannedKind::Withheld,
                "rules share a root"
            );
            // Walked by whichever rule reaches it plainly, by its own path.
            if existing.kind == PlannedKind::Withheld && root.kind != PlannedKind::Withheld {
                existing.path = root.path;
                existing.kind = root.kind;
            }
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

    #[test]
    fn only_cache_resets_leave_a_category_to_choose() {
        let rules = RuleSet::embedded(Os::Linux).unwrap();
        let choices: Vec<_> = Category::ALL
            .iter()
            .map(|category| category.default_choice(&rules))
            .collect();
        assert_eq!(
            choices,
            [
                DefaultChoice::Chosen,
                DefaultChoice::Chosen,
                DefaultChoice::Chosen,
                DefaultChoice::ToChoose
            ]
        );
        assert!(
            rules
                .gpu_cache()
                .iter()
                .all(|rule| !rule.is_selected_by_default()),
            "GPU caches are all resets"
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
        assert_eq!(
            report.issues(),
            [ScanIssue::Stopped(crate::Stop::UnsupportedSystem)]
        );
        assert!(report.groups().is_empty());
    }
}
