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
#[expect(
    clippy::struct_excessive_bools,
    reason = "the rule's own flags, each independent of the others but those validation refuses together"
)]
pub struct TargetDeletion {
    path: TemplatePath,
    child_subdirectory: Option<RelativePath>,
    min_age: Option<AgeDays>,
    deep_recency_check: bool,
    cache_reset: bool,
    keep_hard_linked: bool,
    trash: bool,
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

    /// Whether a file with other names (hard links) is kept: removing one
    /// of its names frees nothing (snapd's cache holds a name of each snap
    /// installed).
    #[must_use]
    pub const fn keeps_hard_linked(&self) -> bool {
        self.keep_hard_linked
    }

    /// Whether `path` is the `files` folder of a freedesktop trash: an entry
    /// of it is offered whole, and only once its `.trashinfo` says it went
    /// there longer ago than [`Self::min_age`]; it is removed for good.
    #[must_use]
    pub const fn is_trash(&self) -> bool {
        self.trash
    }
}

/// A system directory to clean: temporary files, logs, crash dumps, package caches.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawCleanTarget")]
pub struct CleanTarget {
    subcategory: Label,
    needs_admin: bool,
    effect: Effect<TargetDeletion, TemplatePath>,
    description: Option<Label>,
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
    /// performance cache, a maintenance operation nor a trash, whose entries
    /// go for good (invariant 5).
    #[must_use]
    pub const fn is_selected_by_default(&self) -> bool {
        match &self.effect {
            Effect::Delete(deletion) => !deletion.cache_reset && !deletion.trash,
            Effect::Maintenance { .. } => false,
        }
    }

    #[must_use]
    pub const fn description(&self) -> Option<&Label> {
        self.description.as_ref()
    }

    /// The target's path, deleted or only affected (a maintenance's), for
    /// what does not delete anything: checks, keys.
    #[must_use]
    pub const fn template_path(&self) -> &TemplatePath {
        match &self.effect {
            Effect::Delete(deletion) => &deletion.path,
            Effect::Maintenance { affected, .. } => affected,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the rule file's own flags, read as written before `CleanTarget` checks them"
)]
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
    #[serde(default)]
    keep_hard_linked: bool,
    #[serde(default)]
    trash: bool,
    cleanup_action: Option<CleanupAction>,
    description: Option<Label>,
}

impl TryFrom<RawCleanTarget> for CleanTarget {
    type Error = ValidationError;

    fn try_from(raw: RawCleanTarget) -> Result<Self, Self::Error> {
        let conflict = |first, second| ValidationError::ConflictingFields {
            rule: raw.path.to_string(),
            first,
            second,
        };
        let action_conflict = |second| conflict("cleanupAction", second);
        let effect = match raw.cleanup_action {
            Some(_) if raw.child_subdir.is_some() => return Err(action_conflict("childSubdir")),
            Some(_) if raw.min_age_days.is_some() => return Err(action_conflict("minAgeDays")),
            Some(_) if raw.deep_recency_check => return Err(action_conflict("deepRecencyCheck")),
            Some(_) if raw.cache_reset => return Err(action_conflict("cacheReset")),
            Some(_) if raw.keep_hard_linked => return Err(action_conflict("keepHardLinked")),
            Some(_) if raw.trash => return Err(action_conflict("trash")),
            Some(action) => Effect::Maintenance {
                action,
                affected: raw.path,
            },
            None => {
                if raw.trash {
                    // What it holds is the user's, offered by when it went
                    // there, never root's to empty.
                    for (set, second) in [
                        (raw.child_subdir.is_some(), "childSubdir"),
                        (raw.needs_admin, "needsAdmin"),
                        (raw.cache_reset, "cacheReset"),
                        (raw.keep_hard_linked, "keepHardLinked"),
                    ] {
                        if set {
                            return Err(conflict("trash", second));
                        }
                    }
                    if raw.min_age_days.is_none() {
                        return Err(ValidationError::TrashWithoutAge {
                            rule: raw.path.to_string(),
                        });
                    }
                }
                raw.path.ensure_bounded_deletion()?;
                Effect::Delete(TargetDeletion {
                    path: raw.path,
                    child_subdirectory: raw.child_subdir,
                    min_age: raw.min_age_days,
                    deep_recency_check: raw.deep_recency_check,
                    cache_reset: raw.cache_reset,
                    keep_hard_linked: raw.keep_hard_linked,
                    trash: raw.trash,
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
    description: Option<Label>,
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
    pub const fn description(&self) -> Option<&Label> {
        self.description.as_ref()
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
        assert!(!deletion.keeps_hard_linked());
        assert!(!deletion.is_trash());
    }

    #[test]
    fn a_target_never_describes_itself_with_a_blank() {
        for json in [
            r#"{"path": "/var/tmp", "subcategory": "Temp", "description": ""}"#,
            r#"{"path": "/var/log/journal", "subcategory": "J", "cleanupAction": "journal-vacuum", "description": " "}"#,
        ] {
            let error = target(json).unwrap_err().to_string();
            assert!(error.contains("empty text"), "{error}");
        }
        let error = serde_json::from_str::<SingleFileTarget>(
            r#"{"path": "${HOME}/.xsession-errors.old", "subcategory": "X", "description": ""}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("empty text"), "{error}");
    }

    #[test]
    fn reads_every_optional_field_of_a_deletion() {
        let target = target(
            r#"{"path": "${HOME}/.var/app", "subcategory": "Flatpak", "needsAdmin": true,
                "childSubdir": "common/.cache", "minAgeDays": 30, "deepRecencyCheck": true, "cacheReset": true,
                "keepHardLinked": true, "description": "App caches"}"#,
        )
        .unwrap();
        assert!(target.needs_admin());
        assert!(
            !target.is_selected_by_default(),
            "a cache reset is never preselected"
        );
        assert_eq!(target.description().map(Label::as_str), Some("App caches"));
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
        assert!(deletion.keeps_hard_linked());
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
            (r#""keepHardLinked": true"#, "keepHardLinked"),
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
    fn a_trash_is_never_selected_and_needs_its_age_alone() {
        let trash = target(
            r#"{"path": "${LOCAL_SHARE}/Trash/files", "subcategory": "Trash", "minAgeDays": 30, "trash": true}"#,
        )
        .unwrap();
        assert!(!trash.is_selected_by_default());
        let Effect::Delete(deletion) = trash.effect() else {
            panic!("expected a deletion");
        };
        assert!(deletion.is_trash());
        assert!(!deletion.is_cache_reset());
        assert_eq!(deletion.min_age().map(AgeDays::days), Some(30));

        let error =
            target(r#"{"path": "${LOCAL_SHARE}/Trash/files", "subcategory": "T", "trash": true}"#)
                .unwrap_err()
                .to_string();
        assert!(error.contains("needs `minAgeDays`"), "{error}");
        for (extra, second) in [
            (r#""childSubdir": "x""#, "childSubdir"),
            (r#""needsAdmin": true"#, "needsAdmin"),
            (r#""cacheReset": true"#, "cacheReset"),
            (r#""keepHardLinked": true"#, "keepHardLinked"),
        ] {
            let json = format!(
                r#"{{"path": "${{LOCAL_SHARE}}/Trash/files", "subcategory": "T", "minAgeDays": 30, "trash": true, {extra}}}"#
            );
            let error = target(&json).unwrap_err().to_string();
            assert!(
                error.contains(&format!("combines `trash` and `{second}`")),
                "{error}"
            );
        }
        let error = target(
            r#"{"path": "/var/log/journal", "subcategory": "J", "cleanupAction": "journal-vacuum", "trash": true}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("combines `cleanupAction` and `trash`"),
            "{error}"
        );
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
