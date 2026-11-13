//! The results of an analysis: the groups found, shown under the categories
//! chosen, which are checked, and what a cleaning would take. Plain Rust over
//! plain facts of each group, tested without a report; the report itself, and
//! its identifiers, stay with the screen (invariant 1).

use std::collections::{BTreeMap, BTreeSet};

use okapi_engine::{Category, Origin, ScanGroup};
use okapi_rules::{ChromiumBrowserKind, Language};

/// What the results need of one group of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupFacts {
    /// Its label: under its browser's line, a browser's cache needs no more.
    pub label: String,
    /// Its label anywhere else: a browser's cache by its browser's name too.
    pub label_alone: String,
    /// The browser whose cache it is.
    pub browser: Option<ChromiumBrowserKind>,
    pub categories: BTreeSet<Category>,
    /// Never a cache reset nor a maintenance (invariant 5).
    pub preselected: bool,
    pub needs_admin: bool,
    /// The targets the privileged helper is asked for to clean the group,
    /// by the path the rules write them with: only when every rule of the
    /// group is one of its targets (a group that joins another rule is never
    /// the helper's to clean).
    pub targets: Option<BTreeSet<String>>,
    /// Where a group that needs the administrator stands.
    pub admin: AdminState,
    /// Checked before the user chooses: the engine's one rule.
    pub checked_by_default: bool,
    /// What the user's own scan saw of it: [`Self::numbers`] are shown.
    pub entries: usize,
    pub bytes: u64,
}

/// Where a group that needs the administrator stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminState {
    /// Out of reach: the rights are not obtained yet, or the group is not
    /// one the helper may clean.
    Locked,
    /// Root's scan offers it: it may be checked, and shows root's numbers,
    /// which go with it when it is locked again.
    Unlocked { entries: usize, bytes: u64 },
    /// Root's scan offers nothing of it.
    NothingToClean,
}

impl GroupFacts {
    /// What the results need of `group`, its label in `language`; `targets`
    /// names each rule the privileged helper may be asked for.
    #[must_use]
    pub fn of(group: &ScanGroup, language: Language, targets: &BTreeMap<Origin, String>) -> Self {
        let english = group.label().as_str();
        let needs_admin = group.needs_admin();
        Self {
            label: okapi_rules::label(language, english).to_owned(),
            label_alone: okapi_rules::group_label(language, group.browser(), english),
            browser: group.browser(),
            categories: Category::of_group(group.origins()),
            preselected: group.is_selected_by_default(),
            needs_admin,
            targets: needs_admin
                .then(|| {
                    group
                        .origins()
                        .iter()
                        .map(|origin| targets.get(origin).cloned())
                        .collect::<Option<BTreeSet<_>>>()
                })
                .flatten()
                .filter(|targets| !targets.is_empty())
                .or_else(|| {
                    if needs_admin {
                        tracing::info!(origins = ?group.origins(), "a group that needs the administrator, out of the helper's reach");
                    }
                    None
                }),
            admin: AdminState::Locked,
            checked_by_default: group.is_checked_by_default(),
            entries: group.items().len(),
            bytes: group.total_size(),
        }
    }

    /// Whether its checkbox may be checked: a group the user removes, or
    /// one root's scan offers.
    #[must_use]
    pub fn is_checkable(&self) -> bool {
        !self.needs_admin || matches!(self.admin, AdminState::Unlocked { .. })
    }

    /// Its entries and size as shown: root's once its scan set the group
    /// (none when it offers nothing of it), the user's own scan's otherwise.
    #[must_use]
    pub fn numbers(&self) -> (usize, u64) {
        match self.admin {
            AdminState::Unlocked { entries, bytes } => (entries, bytes),
            AdminState::NothingToClean => (0, 0),
            AdminState::Locked => (self.entries, self.bytes),
        }
    }
}

/// A checkbox over several groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Checked,
    Unchecked,
    Partial,
}

/// One line of the results, in a list whose length never changes: folding a
/// category or a browser hides what is below, so the focus stays where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Category {
        category: Category,
        state: CheckState,
        /// Its checkbox does something: it has a group checked, or one
        /// checked by default to check.
        enabled: bool,
        folded: bool,
        entries: usize,
        bytes: u64,
    },
    /// A browser under the browsers' category, over the groups of its caches.
    Browser {
        browser: ChromiumBrowserKind,
        state: CheckState,
        enabled: bool,
        folded: bool,
        hidden: bool,
        entries: usize,
        bytes: u64,
    },
    Group {
        index: usize,
        checked: bool,
        hidden: bool,
        /// Under its browser's line.
        nested: bool,
    },
}

/// The groups of a report, by category chosen, and the user's choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Results {
    groups: Vec<GroupFacts>,
    chosen: BTreeSet<Category>,
    checked: BTreeSet<usize>,
    folded: BTreeSet<Category>,
    folded_browsers: BTreeSet<ChromiumBrowserKind>,
}

impl Results {
    /// The groups, by their index in the report, of an analysis of `chosen`;
    /// those the engine checks by default are checked.
    #[must_use]
    pub fn new(groups: Vec<GroupFacts>, chosen: BTreeSet<Category>) -> Self {
        let checked = groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.checked_by_default)
            .map(|(index, _)| index)
            .collect();
        Self {
            groups,
            chosen,
            checked,
            folded: BTreeSet::new(),
            folded_browsers: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn group(&self, index: usize) -> Option<&GroupFacts> {
        self.groups.get(index)
    }

    /// Where a group is shown: the first of its categories that was chosen,
    /// never one the user left out (a shared group is shown once).
    fn category_of(&self, group: &GroupFacts) -> Option<Category> {
        Category::ALL.into_iter().find(|category| {
            group.categories.contains(category)
                && Category::is_chosen(&BTreeSet::from([*category]), &self.chosen)
        })
    }

    /// The category the group of `index` is shown under.
    #[must_use]
    pub fn shown_under(&self, index: usize) -> Option<Category> {
        self.groups
            .get(index)
            .and_then(|group| self.category_of(group))
    }

    fn groups_under(&self, category: Category) -> impl Iterator<Item = usize> + '_ {
        self.groups
            .iter()
            .enumerate()
            .filter(move |(_, group)| self.category_of(group) == Some(category))
            .map(|(index, _)| index)
    }

    /// The browser line a group is shown under: a browser's cache shown
    /// under the browsers' category.
    fn browser_of(&self, index: usize) -> Option<ChromiumBrowserKind> {
        self.groups[index]
            .browser
            .filter(|_| self.shown_under(index) == Some(Category::Browsers))
    }

    /// Those of `groups` a checkbox reaches: never one that needs the
    /// administrator and root's scan does not offer.
    fn selectable(&self, groups: &[usize]) -> Vec<usize> {
        groups
            .iter()
            .copied()
            .filter(|index| self.groups[*index].is_checkable())
            .collect()
    }

    fn state(&self, groups: &[usize]) -> CheckState {
        let (mut checked, mut unchecked) = (false, false);
        for index in self.selectable(groups) {
            if self.checked.contains(&index) {
                checked = true;
            } else {
                unchecked = true;
            }
        }
        match (checked, unchecked) {
            (true, false) => CheckState::Checked,
            (true, true) => CheckState::Partial,
            (false, _) => CheckState::Unchecked,
        }
    }

    /// Whether a checkbox over `groups` does something: one is checked, or
    /// one checked by default is there to check.
    fn does_something(&self, groups: &[usize]) -> bool {
        self.selectable(groups)
            .into_iter()
            .any(|index| self.checked.contains(&index) || self.groups[index].checked_by_default)
    }

    fn sizes(&self, groups: &[usize]) -> (usize, u64) {
        groups.iter().fold((0, 0), |(entries, bytes), index| {
            let (group_entries, group_bytes) = self.groups[*index].numbers();
            (entries + group_entries, bytes.saturating_add(group_bytes))
        })
    }

    /// The lines shown, categories in the order of the rules, each followed
    /// by its groups; a browser's caches under their browser's line.
    #[must_use]
    pub fn rows(&self) -> Vec<Line> {
        let mut rows = Vec::new();
        for category in Category::ALL {
            let groups: Vec<usize> = self.groups_under(category).collect();
            if groups.is_empty() {
                continue;
            }
            let folded = self.folded.contains(&category);
            let (entries, bytes) = self.sizes(&groups);
            rows.push(Line::Category {
                category,
                state: self.state(&groups),
                enabled: self.does_something(&groups),
                folded,
                entries,
                bytes,
            });
            let mut browsers_shown = BTreeSet::new();
            for &index in &groups {
                let Some(browser) = self.browser_of(index) else {
                    rows.push(self.group_line(index, folded, false));
                    continue;
                };
                if !browsers_shown.insert(browser) {
                    continue;
                }
                let caches: Vec<usize> = groups
                    .iter()
                    .copied()
                    .filter(|other| self.browser_of(*other) == Some(browser))
                    .collect();
                let browser_folded = self.folded_browsers.contains(&browser);
                let (entries, bytes) = self.sizes(&caches);
                rows.push(Line::Browser {
                    browser,
                    state: self.state(&caches),
                    enabled: self.does_something(&caches),
                    folded: browser_folded,
                    hidden: folded,
                    entries,
                    bytes,
                });
                rows.extend(
                    caches
                        .into_iter()
                        .map(|cache| self.group_line(cache, folded || browser_folded, true)),
                );
            }
        }
        rows
    }

    fn group_line(&self, index: usize, hidden: bool, nested: bool) -> Line {
        Line::Group {
            index,
            checked: self.checked.contains(&index),
            hidden,
            nested,
        }
    }

    /// The groups a line of a category or a browser stands over.
    fn groups_of(&self, line: &Line) -> Vec<usize> {
        match line {
            Line::Category { category, .. } => self.groups_under(*category).collect(),
            Line::Browser { browser, .. } => self
                .groups_under(Category::Browsers)
                .filter(|index| self.browser_of(*index) == Some(*browser))
                .collect(),
            Line::Group { index, .. } => vec![*index],
        }
    }

    /// Checks or unchecks what the line `row` shows: a group, or all the
    /// groups of a category or a browser (see [`Self::toggle_groups`]).
    pub fn toggle_row(&mut self, row: usize) {
        match self.rows().get(row) {
            Some(Line::Group { index, .. }) => self.toggle_group(*index),
            Some(line) => {
                let groups = self.groups_of(line);
                self.toggle_groups(&groups);
            }
            None => tracing::warn!(row, "no line of the results to check"),
        }
    }

    /// Folds or unfolds the category or the browser of the line `row`.
    pub fn fold_row(&mut self, row: usize) {
        match self.rows().get(row) {
            Some(Line::Category { category, .. }) => {
                if !self.folded.remove(category) {
                    self.folded.insert(*category);
                }
            }
            Some(Line::Browser { browser, .. }) => {
                if !self.folded_browsers.remove(browser) {
                    self.folded_browsers.insert(*browser);
                }
            }
            Some(Line::Group { .. }) | None => {
                tracing::warn!(row, "no line of the results to fold");
            }
        }
    }

    /// Checks or unchecks a group; never one that needs the administrator
    /// and root's scan does not offer.
    fn toggle_group(&mut self, index: usize) {
        let Some(group) = self.groups.get(index) else {
            return;
        };
        if !group.is_checkable() {
            tracing::warn!(index, admin = ?group.admin, "an administrator's group out of reach cannot be checked");
            return;
        }
        if !self.checked.remove(&index) {
            self.checked.insert(index);
        }
    }

    /// Groups with one checked are unchecked whole; otherwise their
    /// preselected ones are checked, never one left to the user's choice
    /// (invariant 5): only its own checkbox checks it.
    fn toggle_groups(&mut self, groups: &[usize]) {
        let selectable = self.selectable(groups);
        if selectable.iter().any(|index| self.checked.contains(index)) {
            for index in selectable {
                self.checked.remove(&index);
            }
        } else {
            self.checked.extend(
                selectable
                    .into_iter()
                    .filter(|index| self.groups[*index].checked_by_default),
            );
        }
    }

    pub fn deselect_all(&mut self) {
        self.checked.clear();
    }

    /// The groups checked, by their index in the report: the screen takes
    /// them part by part ([`Self::user_checked`], [`Self::admin_checked`]).
    #[cfg(test)]
    #[must_use]
    pub const fn checked(&self) -> &BTreeSet<usize> {
        &self.checked
    }

    /// The entries and size of the groups checked.
    #[must_use]
    pub fn selected(&self) -> (usize, u64) {
        self.checked
            .iter()
            .filter_map(|index| self.groups.get(*index))
            .fold((0, 0), |(entries, bytes), group| {
                let (group_entries, group_bytes) = group.numbers();
                (entries + group_entries, bytes.saturating_add(group_bytes))
            })
    }

    /// The entries of every group of the report, as shown.
    #[must_use]
    pub fn entries(&self) -> usize {
        self.groups.iter().map(|group| group.numbers().0).sum()
    }

    /// The groups waiting for the rights: locked, and the helper's to clean.
    fn waiting(&self) -> impl Iterator<Item = &GroupFacts> {
        self.groups
            .iter()
            .filter(|group| group.admin == AdminState::Locked && group.targets.is_some())
    }

    /// How many groups wait for the rights.
    #[must_use]
    pub fn waiting_for_rights(&self) -> usize {
        self.waiting().count()
    }

    /// The targets of every group the helper may clean, unlocked or not:
    /// what root's scan is asked for, its answer setting every group.
    #[must_use]
    pub fn helper_targets(&self) -> BTreeSet<String> {
        self.groups
            .iter()
            .filter_map(|group| group.targets.clone())
            .flatten()
            .collect()
    }

    /// Locks again every group the helper may clean, unchecked: before a new
    /// scan of root's sets them, so that none stays unlocked by another one.
    pub fn relock(&mut self) {
        for (index, group) in self.groups.iter_mut().enumerate() {
            if group.targets.is_some() {
                group.admin = AdminState::Locked;
                self.checked.remove(&index);
            }
        }
    }

    /// Takes in root's scan: a group waiting for the rights whose targets
    /// are exactly those of a group root offers (`offered`: its targets,
    /// entries and bytes) is unlocked at root's numbers, never checked; one
    /// whose targets root says something about (`troubled`), or offers in
    /// another group, stays locked; the others have nothing to clean for
    /// root. Gives how many unlocked.
    pub fn unlock(
        &mut self,
        offered: &[(BTreeSet<String>, usize, u64)],
        troubled: &BTreeSet<String>,
    ) -> usize {
        let mut unlocked = 0;
        let seen: BTreeSet<&String> = offered
            .iter()
            .flat_map(|(targets, ..)| targets)
            .chain(troubled)
            .collect();
        for group in &mut self.groups {
            let Some(targets) = group.targets.as_ref() else {
                continue;
            };
            if group.admin != AdminState::Locked {
                continue;
            }
            if let Some((_, entries, bytes)) =
                offered.iter().find(|(offered, ..)| offered == targets)
            {
                group.admin = AdminState::Unlocked {
                    entries: *entries,
                    bytes: *bytes,
                };
                unlocked += 1;
            } else if targets.iter().all(|target| !seen.contains(target)) {
                group.admin = AdminState::NothingToClean;
            }
            tracing::info!(?targets, admin = ?group.admin, "group after root's scan");
        }
        unlocked
    }

    /// The groups checked that the user's own cleaning removes.
    #[must_use]
    pub fn user_checked(&self) -> BTreeSet<usize> {
        self.checked
            .iter()
            .copied()
            .filter(|index| {
                self.groups
                    .get(*index)
                    .is_some_and(|group| !group.needs_admin)
            })
            .collect()
    }

    /// The groups checked that root's cleaning removes, and their targets.
    #[must_use]
    pub fn admin_checked(&self) -> (BTreeSet<usize>, BTreeSet<String>) {
        let groups: BTreeSet<usize> = self
            .checked
            .iter()
            .copied()
            .filter(|index| {
                self.groups
                    .get(*index)
                    .is_some_and(|group| group.needs_admin)
            })
            .collect();
        let targets = groups
            .iter()
            .filter_map(|index| self.groups[*index].targets.clone())
            .flatten()
            .collect();
        (groups, targets)
    }

    /// The entries of the groups `indices`.
    #[must_use]
    pub fn entries_of(&self, indices: &BTreeSet<usize>) -> usize {
        indices
            .iter()
            .filter_map(|index| self.groups.get(*index))
            .map(|group| group.numbers().0)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(categories: &[Category], preselected: bool, needs_admin: bool) -> GroupFacts {
        GroupFacts {
            label: String::from("group"),
            label_alone: String::from("group"),
            browser: None,
            categories: categories.iter().copied().collect(),
            preselected,
            needs_admin,
            targets: None,
            admin: AdminState::Locked,
            checked_by_default: okapi_engine::checked_by_default(preselected, needs_admin),
            entries: 2,
            bytes: 10,
        }
    }

    /// A group that needs the administrator, the helper's to clean through
    /// `targets`.
    fn admin(targets: &[&str], preselected: bool) -> GroupFacts {
        GroupFacts {
            targets: Some(targets.iter().map(|target| (*target).to_owned()).collect()),
            ..group(&[Category::System], preselected, true)
        }
    }

    fn set(targets: &[&str]) -> BTreeSet<String> {
        targets.iter().map(|target| (*target).to_owned()).collect()
    }

    /// Two groups of two targets and of one, one root says something about,
    /// one out of the helper's reach, and one of the user's.
    fn waiting() -> Results {
        Results::new(
            vec![
                admin(&["/var/crash", "/var/dumps"], true),
                admin(&["/var/cache/apt/archives"], true),
                admin(&["/var/tmp/x"], true),
                admin(&["/var/log/apt"], false),
                group(&[Category::System], true, true),
                group(&[Category::System], true, false),
            ],
            BTreeSet::from([Category::System]),
        )
    }

    #[test]
    fn only_the_groups_the_helper_cleans_wait_for_the_rights() {
        let results = waiting();
        assert_eq!(results.waiting_for_rights(), 4);
        assert_eq!(
            results.helper_targets(),
            set(&[
                "/var/cache/apt/archives",
                "/var/crash",
                "/var/dumps",
                "/var/log/apt",
                "/var/tmp/x"
            ])
        );
        assert_eq!(
            results.checked(),
            &BTreeSet::from([5]),
            "never an administrator's"
        );
    }

    #[test]
    fn roots_scan_unlocks_exactly_the_groups_it_offers() {
        let mut results = waiting();
        let offered = [
            (set(&["/var/crash", "/var/dumps"]), 7, 700),
            // One target of the two-target group alone: no group of ours.
            (set(&["/var/crash"]), 1, 1),
            (set(&["/var/log/apt"]), 3, 30),
        ];
        assert_eq!(results.unlock(&offered, &set(&["/var/tmp/x"])), 2);
        let admin_of = |index: usize| results.group(index).unwrap().admin;
        assert_eq!(
            admin_of(0),
            AdminState::Unlocked {
                entries: 7,
                bytes: 700
            },
            "at root's numbers"
        );
        assert_eq!(
            admin_of(1),
            AdminState::NothingToClean,
            "root offers nothing of it"
        );
        assert_eq!(results.group(1).unwrap().numbers(), (0, 0), "nor shows any");
        assert_eq!(
            admin_of(2),
            AdminState::Locked,
            "root says something about it"
        );
        assert!(matches!(admin_of(3), AdminState::Unlocked { .. }));
        assert_eq!(admin_of(4), AdminState::Locked, "out of the helper's reach");
        assert_eq!(results.group(0).unwrap().numbers(), (7, 700), "shown");
        assert_eq!(
            results.checked(),
            &BTreeSet::from([5]),
            "unlocked, never checked (invariant 5)"
        );
        assert_eq!(results.waiting_for_rights(), 1);
    }

    /// A new scan of root's sets every group again: none stays unlocked, or
    /// checked, by the one before (its age and exclusions are gone).
    #[test]
    fn a_new_scan_of_roots_sets_every_group_again() {
        let mut results = waiting();
        results.unlock(&[(set(&["/var/log/apt"]), 3, 30)], &BTreeSet::new());
        let row = results
            .rows()
            .iter()
            .position(|line| matches!(line, Line::Group { index: 3, .. }))
            .unwrap();
        results.toggle_row(row);
        assert_eq!(results.admin_checked().0, BTreeSet::from([3]));
        let own = (
            results.group(3).unwrap().entries,
            results.group(3).unwrap().bytes,
        );
        assert_ne!(results.group(3).unwrap().numbers(), own);
        results.relock();
        assert_eq!(
            results.group(3).unwrap().numbers(),
            own,
            "root's numbers gone with the rights"
        );
        assert_eq!(
            results.waiting_for_rights(),
            4,
            "nothing left to another scan"
        );
        assert!(results.admin_checked().0.is_empty(), "unchecked with it");
        assert_eq!(results.checked(), &BTreeSet::from([5]), "the user's stay");
        // A target root offers within another group is not nothing to clean.
        results.unlock(&[(set(&["/var/crash"]), 1, 1)], &BTreeSet::new());
        assert_eq!(results.group(0).unwrap().admin, AdminState::Locked);
    }

    #[test]
    fn an_unlocked_group_is_checked_by_its_own_box_alone() {
        let mut results = waiting();
        results.unlock(&[(set(&["/var/log/apt"]), 3, 30)], &BTreeSet::new());
        let row_of = |results: &Results, wanted: usize| {
            results
                .rows()
                .iter()
                .position(|line| matches!(line, Line::Group { index, .. } if *index == wanted))
                .unwrap()
        };
        // The category's box: unchecks the user's group, never checks the
        // administrator's, which is no preselection.
        results.toggle_row(0);
        results.toggle_row(0);
        assert_eq!(results.checked(), &BTreeSet::from([5]));
        let row = row_of(&results, 3);
        results.toggle_row(row);
        assert_eq!(results.checked(), &BTreeSet::from([3, 5]));
        assert_eq!(results.user_checked(), BTreeSet::from([5]));
        assert_eq!(
            results.admin_checked(),
            (BTreeSet::from([3]), set(&["/var/log/apt"]))
        );
        assert_eq!(results.selected(), (5, 40), "root's numbers counted");
        // Now checked, the category's box unchecks it too.
        results.toggle_row(0);
        assert!(results.checked().is_empty());
        let locked = row_of(&results, 1);
        results.toggle_row(locked);
        assert!(results.checked().is_empty(), "a locked group never checks");
    }

    /// One category: a preselected group, one left to choose, one that needs
    /// the administrator though preselected.
    fn mixed() -> Results {
        Results::new(
            vec![
                group(&[Category::Apps], true, false),
                group(&[Category::Apps], false, false),
                group(&[Category::Apps], true, true),
            ],
            BTreeSet::from([Category::Apps]),
        )
    }

    fn category_row(results: &Results) -> (CheckState, bool) {
        match results.rows().first() {
            Some(Line::Category { state, enabled, .. }) => (*state, *enabled),
            other => panic!("expected the category first: {other:?}"),
        }
    }

    #[test]
    fn only_the_preselected_groups_start_checked() {
        let results = mixed();
        assert_eq!(*results.checked(), BTreeSet::from([0]));
        assert_eq!(
            category_row(&results),
            (CheckState::Partial, true),
            "the one left to choose is unchecked"
        );
        assert_eq!(results.selected(), (2, 10));
        assert_eq!(results.entries(), 6);
    }

    #[test]
    fn a_category_unchecks_whole_then_checks_only_its_preselected_groups() {
        let mut results = mixed();
        results.toggle_group(1);
        assert_eq!(
            category_row(&results).0,
            CheckState::Checked,
            "all selectable"
        );
        results.toggle_row(0);
        assert!(results.checked().is_empty());
        assert_eq!(category_row(&results).0, CheckState::Unchecked);
        results.toggle_row(0);
        assert_eq!(
            *results.checked(),
            BTreeSet::from([0]),
            "never the one left to choose, never the administrator's"
        );
        results.toggle_group(1);
        assert_eq!(category_row(&results).0, CheckState::Checked);
        results.toggle_group(0);
        assert_eq!(category_row(&results).0, CheckState::Partial);
    }

    #[test]
    fn an_administrators_group_is_never_checked() {
        let mut results = mixed();
        results.toggle_group(2);
        assert!(!results.checked().contains(&2));
    }

    #[test]
    fn a_category_without_a_preselected_group_has_nothing_to_check() {
        let mut results = Results::new(
            vec![group(&[Category::Gpu], false, false)],
            BTreeSet::from([Category::Gpu]),
        );
        assert_eq!(category_row(&results), (CheckState::Unchecked, false));
        results.toggle_row(0);
        assert!(results.checked().is_empty());
        results.toggle_group(0);
        assert_eq!(category_row(&results), (CheckState::Checked, true));
    }

    #[test]
    fn a_shared_group_is_shown_once_under_a_category_chosen() {
        let shared = group(&[Category::Apps, Category::Gpu], false, false);
        let results = Results::new(
            vec![group(&[Category::System], true, false), shared],
            BTreeSet::from([Category::System, Category::Gpu]),
        );
        let categories: Vec<_> = results
            .rows()
            .into_iter()
            .filter_map(|row| match row {
                Line::Category { category, .. } => Some(category),
                Line::Browser { .. } | Line::Group { .. } => None,
            })
            .collect();
        assert_eq!(
            categories,
            [Category::System, Category::Gpu],
            "never under Applications, which was not chosen"
        );
    }

    #[test]
    fn folding_hides_the_groups_without_changing_the_rows() {
        let mut results = mixed();
        let before = results.rows().len();
        results.fold_row(0);
        let rows = results.rows();
        assert_eq!(rows.len(), before);
        assert!(
            rows.iter()
                .all(|row| !matches!(row, Line::Group { hidden: false, .. }))
        );
        results.deselect_all();
        assert!(results.checked().is_empty());
    }

    fn cache(browser: ChromiumBrowserKind, preselected: bool) -> GroupFacts {
        GroupFacts {
            browser: Some(browser),
            ..group(&[Category::Browsers], preselected, false)
        }
    }

    /// Chrome's cache (preselected) and code cache (left to choose), Edge's
    /// cache, and an application's group.
    fn browsers() -> Results {
        Results::new(
            vec![
                cache(ChromiumBrowserKind::Chrome, true),
                cache(ChromiumBrowserKind::Chrome, false),
                cache(ChromiumBrowserKind::Edge, true),
                group(&[Category::Apps], true, false),
            ],
            BTreeSet::from([Category::Browsers, Category::Apps]),
        )
    }

    /// Each line, shortly: `C` a category, `B` a browser, `G` a group
    /// (`g` nested), `-` when hidden.
    fn shape(results: &Results) -> String {
        results
            .rows()
            .iter()
            .map(|row| match row {
                Line::Category { .. } => "C",
                Line::Browser { hidden: false, .. } => "B",
                Line::Browser { hidden: true, .. } | Line::Group { hidden: true, .. } => "-",
                Line::Group { nested: true, .. } => "g",
                Line::Group { nested: false, .. } => "G",
            })
            .collect()
    }

    #[test]
    fn a_browser_s_caches_are_under_its_line() {
        let mut results = browsers();
        assert_eq!(shape(&results), "CBggBgCG");
        assert!(matches!(
            results.rows()[1],
            Line::Browser {
                browser: ChromiumBrowserKind::Chrome,
                state: CheckState::Partial,
                entries: 4,
                bytes: 20,
                ..
            }
        ));
        results.fold_row(1);
        assert_eq!(shape(&results), "CB--BgCG", "Chrome folded");
        results.fold_row(0);
        assert_eq!(shape(&results), "C-----CG", "the category folded");
        results.fold_row(0);
        assert_eq!(shape(&results), "CB--BgCG", "Chrome still folded");
    }

    #[test]
    fn a_browser_s_checkbox_never_checks_a_cache_left_to_choose() {
        let mut results = browsers();
        results.toggle_row(1);
        assert_eq!(
            *results.checked(),
            BTreeSet::from([2, 3]),
            "Chrome unchecked whole, Edge and the application untouched"
        );
        results.toggle_row(1);
        assert_eq!(
            *results.checked(),
            BTreeSet::from([0, 2, 3]),
            "never the code cache (invariant 5)"
        );
        results.toggle_row(3);
        assert!(results.checked().contains(&1), "its own checkbox checks it");
    }

    #[test]
    fn a_browser_s_cache_shown_elsewhere_is_a_plain_group() {
        let shared = GroupFacts {
            categories: BTreeSet::from([Category::Apps, Category::Browsers]),
            ..cache(ChromiumBrowserKind::Chrome, true)
        };
        let results = Results::new(vec![shared], BTreeSet::from([Category::Apps]));
        assert_eq!(shape(&results), "CG", "under Applications, no browser line");
    }
}
