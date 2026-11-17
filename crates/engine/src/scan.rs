use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use okapi_platform::{FileIdentity, IDENTITY_SUPPORTED, ROOT_USER};
use okapi_rules::{RuleSet, TemplateVariables};

use crate::custom::PersonalFolders;
use crate::maintenance;
use crate::options::ScanOptions;
use crate::paths::is_absent_kind;
use crate::preconditions::{protection, stops_before_reading};
use crate::protection::ProtectedRoots;
use crate::report::{
    AppFile, Category, FoundItem, GroupAttributes, Origin, ReportBuilder, ScanIssue, ScanReport,
    ScanStats, UnsafeReason,
};
use crate::roots::{BrowserRule, PlannedKind, PlannedRoot, Planner, Planning, RuleEntry};
use crate::trash::{FolderRefusal, TrashGate};
use crate::walk::{Found, Limits, Readers, RootProblem, ScanProgress, WalkContext, walk};
use crate::watch::{Progress, Watch};

/// Scans the system targets and the application rules of `rules` for what
/// can be cleaned, without changing anything.
///
/// Only directories the rules name are walked, and the folders the user
/// chose ([`ScanOptions::with_custom_folders`]) when their category is, by
/// their real location (the path is resolved first, links in its ancestors
/// included). Below
/// a root, no link is followed and no other device is entered, and a root
/// never equals or holds a variable's directory, `${TMPDIR}`'s own directory
/// and its ancestors excepted when they hold no other variable, a walk never
/// entering them (invariant 6).
///
/// A maintenance operation is never scanned: it is offered when its place is
/// there ([`ScanReport::maintenances`]), what it would remove left to whoever
/// runs it. Recursive and file matchers are reported as
/// [`ScanIssue::Unsupported`].
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
        let mut entries = entries_of(rules);
        // Planned only when chosen: a folder on a mount that stopped
        // answering never holds another analysis, and never joins a rule's
        // root in one that does not show it.
        let custom_folders = options.chosen_custom_folders();
        entries.extend(custom_folders.iter().copied().map(RuleEntry::Custom));
        // Read only when a custom folder is to be judged against them.
        let personal = if custom_folders.is_empty() {
            PersonalFolders::none()
        } else {
            PersonalFolders::read(variables, Readers::SYSTEM.identity)
        };
        let planning = Planning {
            misc: rules.misc(),
            variables,
            options,
            readers: Readers::SYSTEM,
            user,
            personal: &personal,
        };
        scan_entries(&entries, &planning, Limits::DEFAULT, watch)
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
        maintenances = report.maintenances().len(),
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
        .chain(browser_entries(rules))
        .chain(
            apps.into_iter().flat_map(|(file, rules)| {
                rules.iter().map(move |rule| RuleEntry::App { file, rule })
            }),
        )
        .collect()
}

/// The cache directories of each browser, browser by browser: those of its
/// profiles, then those a Chromium browser shares among them.
fn browser_entries(rules: &RuleSet) -> impl Iterator<Item = RuleEntry<'_>> {
    let browsers = rules.browsers();
    let directories = browsers.chromium_cache_directories();
    let chromium = browsers.chromium().iter().flat_map(move |browser| {
        let entry = move |shared| {
            move |directory| RuleEntry::Browser {
                browser: BrowserRule::Chromium(browser),
                directory,
                shared,
            }
        };
        directories
            .profile()
            .iter()
            .map(entry(false))
            .chain(directories.shared().iter().map(entry(true)))
    });
    let firefox = browsers.firefox().iter().flat_map(move |browser| {
        browsers
            .firefox_cache_directories()
            .iter()
            .map(move |directory| RuleEntry::Browser {
                browser: BrowserRule::Firefox(browser),
                directory,
                shared: false,
            })
    });
    chromium.chain(firefox)
}

/// [`scan_watched`] over an explicit list of rules, with what plans them
/// (the metadata readers and the user who will remove what is offered
/// included) and the limits given.
pub(crate) fn scan_entries(
    entries: &[RuleEntry<'_>],
    planning: &Planning<'_>,
    limits: Limits,
    watch: &Watch,
) -> ScanReport {
    let mut report = ReportBuilder::new();
    if watch.is_cancelled() {
        report.cancel(0, 0, 0);
        return report.finish();
    }
    let Some(planned) = plan(entries, planning, &mut report) else {
        return report.finish();
    };

    let Grouped {
        roots,
        mut groups,
        attributes,
    } = group_rules(planned.roots_by_rule, entries.len());
    let attributes = merge_attributes(attributes, &mut groups);
    let roots = chosen_roots(roots, &attributes, &mut groups, planning.options);
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
        // privileged helper.
        let needs_admin = group_attributes.is_some_and(|attributes| attributes.needs_admin);
        let remover = needs_admin.then_some(ROOT_USER).or(planning.user);
        let keep_hard_linked =
            group_attributes.is_some_and(|attributes| attributes.keep_hard_linked);
        let trash = group_attributes.is_some_and(|attributes| attributes.trash);
        let withheld = root.kind == PlannedKind::Withheld;
        let found = match &root.kind {
            PlannedKind::File(found) => {
                progress.found_bytes = progress.found_bytes.saturating_add(found.size);
                vec![found.clone()]
            }
            PlannedKind::Withheld => Vec::new(),
            PlannedKind::Directory => {
                let context = WalkContext {
                    cutoff: root.cutoff,
                    exclusions: planning.options.exclusions(),
                    misc: planning.misc,
                    protected: &planned.protected,
                    roots: &planned.places,
                    readers: planning.readers,
                    limits,
                    remover,
                    root_owner: None,
                    keep_hard_linked,
                    // An entry of a trash goes whole: one changed lately
                    // would only be read to be kept.
                    recent_holds_below: trash
                        || group_attributes.is_some_and(GroupAttributes::holds_a_custom_folder),
                };
                walk_root(&mut report, &root, &origins, context, trash, &mut progress)
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
        found_by_group.entry(group).or_default().extend(
            found
                .into_iter()
                .map(|found| found_below(found, &root, keep_hard_linked, trash)),
        );
    }
    if watch.is_cancelled() {
        report.cancel(roots_done, roots_total, progress.entries_seen());
        return report.finish();
    }
    report.roots(planned.places);
    report.maintenances(maintenance::found(entries, planning));
    for (group, attributes) in attributes {
        report.group(
            attributes,
            found_by_group.remove(&group).unwrap_or_default(),
        );
    }
    report.finish()
}

/// `found` below `root`, its group keeping its files with other names or
/// not (`keep_hard_linked`: the group's, its rules merged, never the root's
/// own rule's alone), emptying a trash or not (`trash`, the same way).
fn found_below(found: Found, root: &PlannedRoot, keep_hard_linked: bool, trash: bool) -> FoundItem {
    FoundItem {
        found,
        root: root.path.clone(),
        root_identity: root.identity,
        cutoff: root.cutoff,
        keep_hard_linked,
        trash,
    }
}

/// The roots the rules name, checked, each with the rule it comes from, and
/// the directories reserved to browsers; what kept a root out is recorded in
/// `report`. `None` when the variables' directories cannot be told (the scan
/// then stops).
fn plan(
    entries: &[RuleEntry<'_>],
    planning: &Planning<'_>,
    report: &mut ReportBuilder,
) -> Option<Planned> {
    let started = Instant::now();
    let protected = match protection(
        planning.variables,
        planning.options,
        planning.readers.identity,
    ) {
        Ok((_, protected)) => protected,
        Err(stop) => {
            report.issue(ScanIssue::Stopped(stop));
            return None;
        }
    };
    let mut planner = Planner {
        planning: *planning,
        protected: &protected,
        issues: Vec::new(),
        stats: ScanStats::default(),
        browsers: BTreeMap::new(),
        reserved: BTreeMap::new(),
    };
    let mut roots_by_rule: Vec<(usize, PlannedRoot)> = entries
        .iter()
        .enumerate()
        .flat_map(|(rule, entry)| {
            planner
                .plan(*entry)
                .into_iter()
                .map(move |root| (rule, root))
        })
        .collect();
    // Known once every rule is planned, before rules sharing a root are
    // joined.
    roots_by_rule.retain(|(_, root)| {
        let Some(directory) = browser_data_taken(root, &planner.reserved) else {
            return true;
        };
        tracing::info!(root = ?root.path, origins = ?root.attributes.origins, browser_data = ?directory, "root of a browser's data: left to its rules");
        planner.issues.push(ScanIssue::UnsafeRoot {
            origins: root.attributes.origins.clone(),
            path: root.path.clone(),
            reason: UnsafeReason::BrowserData,
        });
        false
    });
    let Planner {
        issues,
        stats,
        reserved,
        ..
    } = planner;
    // Every rule is planned, chosen or not (its roots reserve their place):
    // only the chosen ones' issues are the analysis's to tell.
    for issue in issues {
        if issue.origins().is_empty() || planning.options.chooses(issue.origins()) {
            report.issue(issue);
        } else {
            tracing::debug!(?issue, "issue of a rule not chosen: not told");
        }
    }
    report.add_stats(&stats);
    tracing::info!(
        rules = entries.len(),
        roots = roots_by_rule.len(),
        elapsed = ?started.elapsed(),
        "scan planned"
    );
    let places = roots_by_rule
        .iter()
        .map(|(_, root)| root.identity)
        .chain(reserved.into_keys())
        .collect();
    Some(Planned {
        protected,
        roots_by_rule,
        places,
    })
}

/// The directory a browser keeps its data in that `root`, of a rule that is
/// no browser's, is or is below (by its canonical path), if any: emptying it
/// would not wait for the browser's lock. A browser's own roots are below
/// its directories; a withheld root is never walked, and still joins the
/// rules that share it (invariant 5).
fn browser_data_taken<'a>(
    root: &PlannedRoot,
    reserved: &'a BTreeMap<FileIdentity, PathBuf>,
) -> Option<&'a Path> {
    let a_browsers = root
        .attributes
        .origins
        .iter()
        .all(|origin| matches!(origin, Origin::Browser { .. }));
    if root.kind == PlannedKind::Withheld || a_browsers {
        return None;
    }
    reserved
        .get(&root.identity)
        .or_else(|| {
            reserved
                .values()
                .find(|directory| root.path.starts_with(directory))
        })
        .map(PathBuf::as_path)
}

/// What [`plan`] finds.
struct Planned {
    protected: ProtectedRoots,
    /// The roots, each with the rule it comes from.
    roots_by_rule: Vec<(usize, PlannedRoot)>,
    /// The identities of every root, chosen or not, and of every directory a
    /// browser keeps its data in: each one's place, which the walks of the
    /// others leave.
    places: BTreeSet<FileIdentity>,
}

/// The roots of a group `options` choose, each with its categories: those of
/// its group's rules (the sense a group is shown in).
fn chosen_roots(
    roots: Vec<(usize, PlannedRoot)>,
    attributes: &BTreeMap<usize, GroupAttributes>,
    groups: &mut RuleGroups,
    options: &ScanOptions,
) -> Vec<(usize, PlannedRoot, BTreeSet<Category>)> {
    roots
        .into_iter()
        .filter_map(|(rule, root)| {
            let origins = attributes
                .get(&groups.find(rule))
                .map_or(&[][..], |attributes| &attributes.origins);
            options
                .chooses(origins)
                .then(|| (rule, root, Category::of_group(origins)))
        })
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

/// The gate of the trash whose `files` folder is `root`, its entries let go
/// by their `.trashinfo` ([`TrashGate`]); `None`, said, when none may go:
/// its `info` folder is unusable, or the remover is unknown or root, who
/// never empties what the user put there (nor reads its `.trashinfo`).
fn trash_gate(
    report: &mut ReportBuilder,
    root: &PlannedRoot,
    origins: &[Origin],
    remover: Option<u32>,
) -> Option<TrashGate> {
    let Some(remover) = remover.filter(|remover| *remover != ROOT_USER) else {
        tracing::info!(root = ?root.path, ?remover, "a trash is never emptied by root nor by a user unknown: not walked");
        return None;
    };
    let information = okapi_platform::information_folder(&root.path);
    match TrashGate::open(&root.path, root.cutoff, Some(remover)) {
        Ok(gate) => Some(gate),
        Err(FolderRefusal::Absent) => {
            tracing::info!(root = ?root.path, "trash without its information folder: nothing of it offered");
            None
        }
        Err(FolderRefusal::NotATrash) => {
            tracing::error!(root = ?root.path, "a trash rule names no `files` folder of a trash: not walked");
            for origin in origins {
                report.issue(ScanIssue::Unsupported {
                    origin: origin.clone(),
                });
            }
            None
        }
        Err(FolderRefusal::Changed) => {
            tracing::info!(information = ?information, "information folder of the trash a link, or not a folder: nothing of it offered");
            report.issue(ScanIssue::UnsafeRoot {
                origins: origins.to_vec(),
                path: information.unwrap_or_else(|| root.path.clone()),
                reason: UnsafeReason::Replaced,
            });
            None
        }
        Err(FolderRefusal::Unreadable(error)) => {
            tracing::info!(information = ?information, %error, code = error.raw_os_error(), "information folder of the trash unreadable: nothing of it offered");
            report.issue(ScanIssue::UnreadableRoot {
                origins: origins.to_vec(),
                path: information.unwrap_or_else(|| root.path.clone()),
                error: error.kind(),
            });
            None
        }
    }
}

/// Walks one directory root, the `files` folder of a trash when `trash`,
/// and records in `report` what kept it from being walked whole.
fn walk_root(
    report: &mut ReportBuilder,
    root: &PlannedRoot,
    origins: &[Origin],
    context: WalkContext<'_>,
    trash: bool,
    progress: &mut ScanProgress<'_>,
) -> Vec<Found> {
    let gate = if trash {
        let Some(gate) = trash_gate(report, root, origins, context.remover) else {
            return Vec::new();
        };
        Some(gate)
    } else {
        None
    };
    let walked = walk(&root.path, root.identity, context, gate.as_ref(), progress);
    tracing::debug!(
        root = ?root.path,
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
                path = ?existing.path,
                joined = ?root.path,
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
        let browsers = rules.browsers();
        let directories = browsers.chromium_cache_directories();
        let expected = system.clean_targets().len()
            + system.single_file_targets().len()
            + browsers.chromium().len()
                * (directories.profile().len() + directories.shared().len())
            + browsers.firefox().len() * browsers.firefox_cache_directories().len()
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
        let categories: Vec<Category> = entries.iter().map(RuleEntry::category).collect();
        assert!(
            categories.is_sorted(),
            "system, browsers, apps, gaming, then GPU caches"
        );
        // Browser by browser, each one's profile directories before its
        // shared ones.
        let browser_entries: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                RuleEntry::Browser {
                    browser, shared, ..
                } => Some((browser.kind(), *shared)),
                _ => None,
            })
            .collect();
        assert!(
            entries.iter().all(|entry| !matches!(
                entry,
                RuleEntry::Browser {
                    browser: BrowserRule::Firefox(_),
                    shared: true,
                    ..
                }
            )),
            "Firefox shares no cache among its profiles"
        );
        let mut by_browser = browser_entries.clone();
        by_browser.dedup_by_key(|(browser, _)| *browser);
        assert_eq!(
            by_browser.len(),
            browsers.chromium().len() + browsers.firefox().len(),
            "each browser once, in a row"
        );
        assert_eq!(
            browser_entries[..=directories.profile().len()]
                .iter()
                .map(|(_, shared)| *shared)
                .collect::<Vec<_>>(),
            [vec![false; directories.profile().len()], vec![true]].concat()
        );
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
                DefaultChoice::Chosen,
                DefaultChoice::ToChoose,
                DefaultChoice::ToChoose
            ],
            "GPU caches are all resets; the user's folders are never chosen beforehand"
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
