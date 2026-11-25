//! The maintenance operations the rules name (`cleanupAction`), offered when
//! what they work on is there: never scanned, never run here. Whoever runs
//! one (the privileged helper for the journal's vacuum) says what it would
//! remove; a rule only names it (invariant 4).

use std::fs;
use std::path::{Path, PathBuf};

use okapi_rules::{CleanupAction, Effect, Label, TemplatePath, TemplateVariables};

use crate::options::ScanOptions;
use crate::paths::is_absent_kind;
use crate::report::Origin;
use crate::roots::{Planning, RuleEntry};

/// A maintenance operation the scan found the place of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceFound {
    origin: Origin,
    action: CleanupAction,
    label: Label,
    description: Option<Label>,
    needs_admin: bool,
    paths: Vec<PathBuf>,
}

impl MaintenanceFound {
    /// Its rule.
    #[must_use]
    pub const fn origin(&self) -> &Origin {
        &self.origin
    }

    #[must_use]
    pub const fn action(&self) -> CleanupAction {
        self.action
    }

    /// Its rule's label, in English.
    #[must_use]
    pub const fn label(&self) -> &Label {
        &self.label
    }

    /// What its rule says it does, in English.
    #[must_use]
    pub const fn description(&self) -> Option<&Label> {
        self.description.as_ref()
    }

    #[must_use]
    pub const fn needs_admin(&self) -> bool {
        self.needs_admin
    }

    /// The places it works on that are there, as the rule resolves them.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// The maintenance operations of `entries` that the planning's options
/// choose, each offered when one of its places is there and not excluded:
/// read without following it, nothing run.
pub(crate) fn found(entries: &[RuleEntry<'_>], planning: &Planning<'_>) -> Vec<MaintenanceFound> {
    let Planning {
        variables, options, ..
    } = planning;
    entries
        .iter()
        .filter_map(|entry| {
            let (origin, action, affected, label, description, needs_admin) = match entry {
                RuleEntry::Target(target) => match target.effect() {
                    Effect::Maintenance { action, affected } => (
                        Origin::of_target(target),
                        *action,
                        std::slice::from_ref(affected),
                        target.subcategory(),
                        target.description(),
                        target.needs_admin(),
                    ),
                    Effect::Delete(_) => return None,
                },
                RuleEntry::App { file, rule } => match rule.effect() {
                    Effect::Maintenance { action, affected } => (
                        Origin::App {
                            file: *file,
                            id: rule.id().clone(),
                        },
                        *action,
                        &affected[..],
                        rule.name(),
                        rule.description(),
                        false,
                    ),
                    Effect::Delete(_) => return None,
                },
                // A folder of the user's is emptied, never maintained.
                RuleEntry::File(_) | RuleEntry::Browser { .. } | RuleEntry::Custom(_) => {
                    return None;
                }
            };
            if !options.chooses(std::slice::from_ref(&origin)) {
                return None;
            }
            if !options.offers(needs_admin) {
                tracing::debug!(
                    ?origin,
                    ?action,
                    "a maintenance that needs the administrator, not offered"
                );
                return None;
            }
            let paths = present(&origin, affected, variables, options);
            if paths.is_empty() {
                tracing::debug!(
                    ?origin,
                    ?action,
                    "a maintenance's place not there: not offered"
                );
                return None;
            }
            tracing::debug!(?origin, ?action, ?paths, "maintenance offered");
            Some(MaintenanceFound {
                origin,
                action,
                label: label.clone(),
                description: description.cloned(),
                needs_admin,
                paths,
            })
        })
        .collect()
}

/// The places of `affected` that are there and not excluded.
fn present(
    origin: &Origin,
    affected: &[TemplatePath],
    variables: &TemplateVariables,
    options: &ScanOptions,
) -> Vec<PathBuf> {
    affected
        .iter()
        .filter_map(|path| match path.resolve(variables) {
            Ok(resolved) => Some(resolved),
            Err(error) => {
                tracing::debug!(?origin, %path, %error, "a maintenance's place unresolvable: not offered there");
                None
            }
        })
        .filter(|path| is_there(origin, path, options))
        .collect()
}

/// Whether the place `path` is there and not excluded, compared as a root
/// is: by its real location and by identity (a bind mount).
fn is_there(origin: &Origin, path: &Path, options: &ScanOptions) -> bool {
    let exclusions = options.exclusions();
    // Absent or unreadable, it is judged below as written.
    let real = fs::canonicalize(path).unwrap_or_else(|error| {
        tracing::debug!(?origin, ?path, %error, "a maintenance's place not resolved: compared as written");
        path.to_path_buf()
    });
    if exclusions.excludes(path)
        || exclusions.excludes(&real)
        || exclusions.excludes_through_identity(&real)
    {
        tracing::debug!(
            ?origin,
            ?path,
            "a maintenance's place excluded: not offered there"
        );
        return false;
    }
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if is_absent_kind(error.kind()) => false,
        // Whoever runs it says whether it is there after all: root may read
        // what the user may not.
        Err(error) => {
            tracing::info!(?origin, ?path, %error, "a maintenance's place unreadable: offered, its operation will tell");
            true
        }
    }
}
