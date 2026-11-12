//! The results of an analysis: the groups found, shown under the categories
//! chosen, which are checked, and what a cleaning would take. Plain Rust over
//! plain facts of each group, tested without a report; the report itself, and
//! its identifiers, stay with the screen (invariant 1).

use std::collections::BTreeSet;

use okapi_engine::{Category, ScanGroup};

/// What the results need of one group of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupFacts {
    pub label: String,
    pub categories: BTreeSet<Category>,
    /// Never a cache reset nor a maintenance (invariant 5).
    pub preselected: bool,
    pub needs_admin: bool,
    /// Checked before the user chooses: the engine's one rule.
    pub checked_by_default: bool,
    pub entries: usize,
    pub bytes: u64,
}

impl GroupFacts {
    #[must_use]
    pub fn of(group: &ScanGroup) -> Self {
        Self {
            label: group.label().to_string(),
            categories: Category::of_group(group.origins()),
            preselected: group.is_selected_by_default(),
            needs_admin: group.needs_admin(),
            checked_by_default: group.is_checked_by_default(),
            entries: group.items().len(),
            bytes: group.total_size(),
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
/// category hides its groups, so the focus stays where it is.
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
    Group {
        index: usize,
        checked: bool,
        hidden: bool,
    },
}

/// The groups of a report, by category chosen, and the user's choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Results {
    groups: Vec<GroupFacts>,
    chosen: BTreeSet<Category>,
    checked: BTreeSet<usize>,
    folded: BTreeSet<Category>,
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

    fn selectable(&self, category: Category) -> impl Iterator<Item = usize> + '_ {
        self.groups_under(category)
            .filter(|index| !self.groups[*index].needs_admin)
    }

    fn state(&self, category: Category) -> CheckState {
        let (mut checked, mut unchecked) = (false, false);
        for index in self.selectable(category) {
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

    /// The lines shown, categories in the order of the rules, each followed
    /// by its groups.
    #[must_use]
    pub fn rows(&self) -> Vec<Line> {
        let mut rows = Vec::new();
        for category in Category::ALL {
            let groups: Vec<usize> = self.groups_under(category).collect();
            if groups.is_empty() {
                continue;
            }
            let folded = self.folded.contains(&category);
            rows.push(Line::Category {
                category,
                state: self.state(category),
                enabled: self.category_does_something(category),
                folded,
                entries: groups.iter().map(|index| self.groups[*index].entries).sum(),
                bytes: groups
                    .iter()
                    .map(|index| self.groups[*index].bytes)
                    .fold(0, u64::saturating_add),
            });
            rows.extend(groups.into_iter().map(|index| Line::Group {
                index,
                checked: self.checked.contains(&index),
                hidden: folded,
            }));
        }
        rows
    }

    fn category_does_something(&self, category: Category) -> bool {
        self.selectable(category)
            .any(|index| self.checked.contains(&index) || self.groups[index].checked_by_default)
    }

    /// Checks or unchecks a group; never one that needs the administrator,
    /// which the interface cannot clean yet.
    pub fn toggle_group(&mut self, index: usize) {
        let Some(group) = self.groups.get(index) else {
            return;
        };
        if group.needs_admin {
            tracing::warn!(index, "an administrator's group cannot be checked");
            return;
        }
        if !self.checked.remove(&index) {
            self.checked.insert(index);
        }
    }

    /// A category with a group checked is unchecked whole; otherwise its
    /// preselected groups are checked, never one left to the user's choice
    /// (invariant 5): only its own checkbox checks it.
    pub fn toggle_category(&mut self, category: Category) {
        let selectable: Vec<usize> = self.selectable(category).collect();
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

    pub fn toggle_folded(&mut self, category: Category) {
        if !self.folded.remove(&category) {
            self.folded.insert(category);
        }
    }

    /// The groups checked, by their index in the report.
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
                (entries + group.entries, bytes.saturating_add(group.bytes))
            })
    }

    /// The entries of every group of the report.
    #[must_use]
    pub fn entries(&self) -> usize {
        self.groups.iter().map(|group| group.entries).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(categories: &[Category], preselected: bool, needs_admin: bool) -> GroupFacts {
        GroupFacts {
            label: String::from("group"),
            categories: categories.iter().copied().collect(),
            preselected,
            needs_admin,
            checked_by_default: okapi_engine::checked_by_default(preselected, needs_admin),
            entries: 2,
            bytes: 10,
        }
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
        results.toggle_category(Category::Apps);
        assert!(results.checked().is_empty());
        assert_eq!(category_row(&results).0, CheckState::Unchecked);
        results.toggle_category(Category::Apps);
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
        results.toggle_category(Category::Gpu);
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
                Line::Group { .. } => None,
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
        results.toggle_folded(Category::Apps);
        let rows = results.rows();
        assert_eq!(rows.len(), before);
        assert!(
            rows.iter()
                .all(|row| !matches!(row, Line::Group { hidden: false, .. }))
        );
        results.deselect_all();
        assert!(results.checked().is_empty());
    }
}
