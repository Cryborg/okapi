//! Which groups a run shows and which it cleans.

use std::collections::BTreeSet;

use okapi_engine::{Category, Origin, checked_by_default};

/// What a run does with a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Selected for removal: what `--clean` removes, or would remove.
    Selected,
    /// Shown, but outside the categories asked for.
    OutsideCategories,
    /// Never removed unless chosen: a cache reset or a maintenance operation
    /// (invariant 5).
    NotPreselected,
    /// Its removal needs the administrator, through the privileged helper.
    NeedsAdmin,
}

/// The fate of a group of rules `origins`: a group belongs to the categories
/// asked for (all when none) if one of its rules does.
pub fn decide(
    origins: &[Origin],
    selected_by_default: bool,
    needs_admin: bool,
    categories: &BTreeSet<Category>,
) -> Decision {
    if !Category::is_chosen(&Category::of_group(origins), categories) {
        Decision::OutsideCategories
    } else if checked_by_default(selected_by_default, needs_admin) {
        Decision::Selected
    } else if !selected_by_default {
        Decision::NotPreselected
    } else {
        Decision::NeedsAdmin
    }
}

/// A rule as the rules name it: the subcategory of a system target, the
/// identifier of an application rule.
pub fn rule_name(origin: &Origin) -> String {
    match origin {
        Origin::SystemTarget { subcategory, .. } | Origin::SystemFile { subcategory, .. } => {
            subcategory.to_string()
        }
        Origin::App { id, .. } => id.to_string(),
        Origin::Browser { browser, directory } => format!("{}/{directory}", browser.key()),
    }
}

#[cfg(test)]
mod tests {
    use okapi_engine::AppFile;
    use okapi_rules::RuleId;

    use super::*;

    fn app(file: AppFile, id: &str) -> Origin {
        Origin::App {
            file,
            id: RuleId::try_from(id.to_owned()).unwrap(),
        }
    }

    fn categories(list: &[Category]) -> BTreeSet<Category> {
        list.iter().copied().collect()
    }

    #[test]
    fn a_system_target_is_never_cleaned_by_apps() {
        let target = Origin::SystemTarget {
            subcategory: okapi_rules::Label::try_from("Temp".to_owned()).unwrap(),
            path: okapi_rules::TemplatePath::try_from("/tmp".to_owned()).unwrap(),
        };
        assert_eq!(
            decide(&[target], true, false, &categories(&[Category::Apps])),
            Decision::OutsideCategories,
        );
    }

    #[test]
    fn a_shared_root_belongs_to_the_categories_of_each_of_its_rules() {
        let unity = [
            app(AppFile::Apps, "unity"),
            app(AppFile::GpuCache, "unity-cache"),
        ];
        for asked in [Category::Apps, Category::Gpu] {
            assert_eq!(
                decide(&unity, true, false, &categories(&[asked])),
                Decision::Selected,
                "{asked:?}"
            );
        }
        assert_eq!(
            decide(&unity, true, false, &categories(&[Category::Gaming])),
            Decision::OutsideCategories
        );
    }

    #[test]
    fn only_preselected_groups_that_need_no_administrator_are_cleaned() {
        let pip = [app(AppFile::Apps, "pip")];
        let all = BTreeSet::new();
        assert_eq!(decide(&pip, true, false, &all), Decision::Selected);
        assert_eq!(decide(&pip, false, false, &all), Decision::NotPreselected);
        assert_eq!(decide(&pip, true, true, &all), Decision::NeedsAdmin);
        assert_eq!(
            decide(&pip, false, true, &all),
            Decision::NotPreselected,
            "a reset stays a reset, administrator or not"
        );
    }
}
