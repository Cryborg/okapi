//! Which groups a run shows and which it cleans.

use std::collections::BTreeSet;

use okapi_engine::{AppFile, Origin};

/// A family of rules the command line can pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// The targets and single files of `system.json`.
    System,
    /// `apps.json`.
    Apps,
    /// `gaming.json`.
    Gaming,
    /// `gpu-cache.json`.
    Gpu,
}

impl Category {
    pub const ALL: [Self; 4] = [Self::System, Self::Apps, Self::Gaming, Self::Gpu];

    /// Its name, in its flag (`--apps`) and in qualified rule names
    /// (`apps:pip`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Apps => "apps",
            Self::Gaming => "gaming",
            Self::Gpu => "gpu",
        }
    }

    pub const fn of(origin: &Origin) -> Self {
        match origin {
            Origin::SystemTarget { .. } | Origin::SystemFile { .. } => Self::System,
            Origin::App { file, .. } => match file {
                AppFile::Apps => Self::Apps,
                AppFile::Gaming => Self::Gaming,
                AppFile::GpuCache => Self::Gpu,
            },
        }
    }
}

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
    let asked = categories.is_empty()
        || origins
            .iter()
            .any(|origin| categories.contains(&Category::of(origin)));
    if !asked {
        Decision::OutsideCategories
    } else if !selected_by_default {
        Decision::NotPreselected
    } else if needs_admin {
        Decision::NeedsAdmin
    } else {
        Decision::Selected
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
    }
}

#[cfg(test)]
mod tests {
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
    fn each_rule_file_has_its_category() {
        assert_eq!(Category::of(&app(AppFile::Apps, "pip")), Category::Apps);
        assert_eq!(
            Category::of(&app(AppFile::Gaming, "steam")),
            Category::Gaming
        );
        assert_eq!(Category::of(&app(AppFile::GpuCache, "mesa")), Category::Gpu);
        let label = okapi_rules::Label::try_from("Temp".to_owned()).unwrap();
        let path = okapi_rules::TemplatePath::try_from("/tmp".to_owned()).unwrap();
        let target = Origin::SystemTarget {
            subcategory: label.clone(),
            path: path.clone(),
        };
        let file = Origin::SystemFile {
            subcategory: label,
            path,
        };
        assert_eq!(Category::of(&target), Category::System);
        assert_eq!(Category::of(&file), Category::System);
        assert_eq!(
            decide(&[target], true, false, &categories(&[Category::Apps])),
            Decision::OutsideCategories,
            "a system target is never cleaned by --apps"
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
