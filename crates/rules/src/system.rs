use serde::Deserialize;

use crate::app::AgeDays;
use crate::effect::{CleanupAction, Effect};
use crate::error::ValidationError;
use crate::name::RelativePath;
use crate::rule_set::{RuleFileKind, system_kind};
use crate::template::{FilePath, TemplatePath};
use crate::values::Label;

/// How a system target deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetDeletion {
    path: TemplatePath,
    child_subdirectory: Option<RelativePath>,
    min_age: Option<AgeDays>,
    deep_recency_check: bool,
    cache_reset: bool,
}

impl TargetDeletion {
    #[must_use]
    pub const fn path(&self) -> &TemplatePath {
        &self.path
    }

    /// If set, the contents of `<path>/*/<subdirectory>` are cleaned instead of `<path>`.
    #[must_use]
    pub const fn child_subdirectory(&self) -> Option<&RelativePath> {
        self.child_subdirectory.as_ref()
    }

    /// Entries younger than this are kept; the stricter of this and the
    /// age the user chose applies.
    #[must_use]
    pub const fn min_age(&self) -> Option<AgeDays> {
        self.min_age
    }

    /// Kudu's request for a closer age check of every descendant. It never
    /// weakens invariant 2, which applies to every target.
    #[must_use]
    pub const fn deep_recency_check(&self) -> bool {
        self.deep_recency_check
    }

    /// A performance cache: clearing it slows the next launch.
    #[must_use]
    pub const fn is_cache_reset(&self) -> bool {
        self.cache_reset
    }
}

/// A system directory to clean: temporary files, logs, crash dumps, package caches.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawCleanTarget")]
pub struct CleanTarget {
    subcategory: Label,
    needs_admin: bool,
    effect: Effect<TargetDeletion, TemplatePath>,
    description: Option<String>,
}

impl CleanTarget {
    /// The label that groups this target in the interface.
    #[must_use]
    pub const fn subcategory(&self) -> &Label {
        &self.subcategory
    }

    #[must_use]
    pub const fn needs_admin(&self) -> bool {
        self.needs_admin
    }

    #[must_use]
    pub const fn effect(&self) -> &Effect<TargetDeletion, TemplatePath> {
        &self.effect
    }

    /// Whether the target is selected before the user chooses: never for a
    /// performance cache nor a maintenance operation (invariant 5).
    #[must_use]
    pub const fn is_selected_by_default(&self) -> bool {
        match &self.effect {
            Effect::Delete(deletion) => !deletion.cache_reset,
            Effect::Maintenance { .. } => false,
        }
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The target's path, deleted or only affected, for checks that do not
    /// delete anything.
    pub(crate) const fn template_path(&self) -> &TemplatePath {
        match &self.effect {
            Effect::Delete(deletion) => &deletion.path,
            Effect::Maintenance { affected, .. } => affected,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawCleanTarget {
    path: TemplatePath,
    subcategory: Label,
    #[serde(default)]
    needs_admin: bool,
    child_subdir: Option<RelativePath>,
    min_age_days: Option<AgeDays>,
    #[serde(default)]
    deep_recency_check: bool,
    #[serde(default)]
    cache_reset: bool,
    cleanup_action: Option<CleanupAction>,
    description: Option<String>,
}

impl TryFrom<RawCleanTarget> for CleanTarget {
    type Error = ValidationError;

    fn try_from(raw: RawCleanTarget) -> Result<Self, Self::Error> {
        let conflict = |second| ValidationError::ConflictingFields {
            rule: raw.path.to_string(),
            first: "cleanupAction",
            second,
        };
        let effect = match raw.cleanup_action {
            Some(_) if raw.child_subdir.is_some() => return Err(conflict("childSubdir")),
            Some(_) if raw.min_age_days.is_some() => return Err(conflict("minAgeDays")),
            Some(_) if raw.deep_recency_check => return Err(conflict("deepRecencyCheck")),
            Some(_) if raw.cache_reset => return Err(conflict("cacheReset")),
            Some(action) => Effect::Maintenance {
                action,
                affected: raw.path,
            },
            None => {
                raw.path.ensure_bounded_deletion()?;
                Effect::Delete(TargetDeletion {
                    path: raw.path,
                    child_subdirectory: raw.child_subdir,
                    min_age: raw.min_age_days,
                    deep_recency_check: raw.deep_recency_check,
                    cache_reset: raw.cache_reset,
                })
            }
        };
        Ok(Self {
            subcategory: raw.subcategory,
            needs_admin: raw.needs_admin,
            effect,
            description: raw.description,
        })
    }
}

/// A single system file to clean; its path is never a variable alone.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingleFileTarget {
    path: FilePath,
    subcategory: Label,
    description: Option<String>,
}

impl SingleFileTarget {
    #[must_use]
    pub fn path(&self) -> &TemplatePath {
        &self.path
    }

    #[must_use]
    pub const fn subcategory(&self) -> &Label {
        &self.subcategory
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Whether the file is selected before the user chooses: always, as it
    /// is neither a performance cache nor a maintenance operation
    /// (invariant 5).
    #[must_use]
    pub const fn is_selected_by_default(&self) -> bool {
        true
    }
}

/// The content of `system.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SystemRules {
    #[serde(rename = "type", deserialize_with = "system_kind")]
    pub(crate) kind: RuleFileKind,
    clean_targets: Vec<CleanTarget>,
    #[serde(default)]
    single_file_targets: Vec<SingleFileTarget>,
}

impl SystemRules {
    #[must_use]
    pub fn clean_targets(&self) -> &[CleanTarget] {
        &self.clean_targets
    }

    #[must_use]
    pub fn single_file_targets(&self) -> &[SingleFileTarget] {
        &self.single_file_targets
    }

    pub(crate) fn template_paths(&self) -> impl Iterator<Item = &TemplatePath> {
        self.clean_targets
            .iter()
            .map(CleanTarget::template_path)
            .chain(self.single_file_targets.iter().map(SingleFileTarget::path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(json: &str) -> Result<CleanTarget, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn a_plain_target_deletes_and_is_selected() {
        let target = target(r#"{"path": "/var/tmp", "subcategory": "Temp"}"#).unwrap();
        assert!(!target.needs_admin());
        assert!(target.is_selected_by_default());
        let Effect::Delete(deletion) = target.effect() else {
            panic!("expected a deletion");
        };
        assert_eq!(deletion.path().to_string(), "/var/tmp");
        assert!(!deletion.is_cache_reset());
        assert_eq!(deletion.child_subdirectory(), None);
        assert_eq!(deletion.min_age(), None);
        assert!(!deletion.deep_recency_check());
    }

    #[test]
    fn reads_every_optional_field_of_a_deletion() {
        let target = target(
            r#"{"path": "${HOME}/.var/app", "subcategory": "Flatpak", "needsAdmin": true,
                "childSubdir": "common/.cache", "minAgeDays": 30, "deepRecencyCheck": true, "cacheReset": true,
                "description": "App caches"}"#,
        )
        .unwrap();
        assert!(target.needs_admin());
        assert!(
            !target.is_selected_by_default(),
            "a cache reset is never preselected"
        );
        assert_eq!(target.description(), Some("App caches"));
        let Effect::Delete(deletion) = target.effect() else {
            panic!("expected a deletion");
        };
        assert_eq!(
            deletion.child_subdirectory().unwrap().to_string(),
            "common/.cache"
        );
        assert_eq!(deletion.min_age().map(AgeDays::days), Some(30));
        assert!(deletion.deep_recency_check());
        assert!(deletion.is_cache_reset());
    }

    #[test]
    fn a_maintenance_target_is_not_selected_and_cannot_also_delete() {
        let journal = target(
            r#"{"path": "/var/log/journal", "subcategory": "Journal", "needsAdmin": true,
                "cleanupAction": "journal-vacuum"}"#,
        )
        .unwrap();
        let Effect::Maintenance { action, affected } = journal.effect() else {
            panic!("expected a maintenance operation");
        };
        assert_eq!(*action, CleanupAction::JournalVacuum);
        assert_eq!(affected.to_string(), "/var/log/journal");
        assert!(!journal.is_selected_by_default());

        for (extra, second) in [
            (r#""childSubdir": "x""#, "childSubdir"),
            (r#""minAgeDays": 30"#, "minAgeDays"),
            (r#""deepRecencyCheck": true"#, "deepRecencyCheck"),
            (r#""cacheReset": true"#, "cacheReset"),
        ] {
            let json = format!(
                r#"{{"path": "/var/log/journal", "subcategory": "J", "cleanupAction": "journal-vacuum", {extra}}}"#
            );
            let error = target(&json).unwrap_err().to_string();
            assert!(
                error.contains(&format!("combines `cleanupAction` and `{second}`")),
                "{error}"
            );
        }
    }

    #[test]
    fn only_the_temporary_directory_is_emptied_from_a_bare_variable() {
        assert!(target(r#"{"path": "${TMPDIR}", "subcategory": "Temp"}"#).is_ok());
        for json in [
            r#"{"path": "${HOME}", "subcategory": "Home"}"#,
            r#"{"path": "${HOME}", "subcategory": "H", "childSubdir": "Documents"}"#,
        ] {
            let error = target(json).unwrap_err().to_string();
            assert!(
                error.starts_with("`${HOME}` alone would empty"),
                "{json}: {error}"
            );
        }
        for path in ["${HOME}", "${TMPDIR}"] {
            let json = format!(r#"{{"path": "{path}", "subcategory": "F"}}"#);
            let error = serde_json::from_str::<SingleFileTarget>(&json)
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with(&format!("`{path}` alone would empty")),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_an_empty_subcategory_and_unknown_fields() {
        let empty = target(r#"{"path": "/var/tmp", "subcategory": ""}"#)
            .unwrap_err()
            .to_string();
        assert!(empty.starts_with("empty text"), "{empty}");
        let unknown = target(r#"{"path": "/var/tmp", "subcategory": "Temp", "command": "rm"}"#)
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("unknown field `command`"), "{unknown}");
        let single = serde_json::from_str::<SingleFileTarget>(
            r#"{"path": "/var/x", "subcategory": "X", "command": "rm"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(single.contains("unknown field `command`"), "{single}");
    }
}
