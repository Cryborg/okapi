use serde::Deserialize;

/// A built-in maintenance operation that replaces raw deletion for a target.
///
/// A rule only names the operation: the command it runs is fixed in Okapi's
/// code, never supplied by the rule (invariant 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CleanupAction {
    UvPrune,
    PnpmPrune,
    WindowsComponents,
    DeliveryOptimization,
    DockerBuildPrune,
    JournalVacuum,
}

/// What cleaning a rule does: delete files it finds, or run a maintenance
/// operation. The paths live inside each variant, so the paths of a
/// maintenance operation can never be read as directories to empty.
///
/// `D` describes a deletion (its paths and how to search them), `A` the paths a
/// maintenance operation affects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect<D, A> {
    /// Delete what the rule finds, as `D` describes it.
    Delete(D),
    /// Run `action` instead. `affected` only says what the operation works on,
    /// for display: it is never scanned, neither to delete nor to measure a size
    /// (what an operation reclaims is only known once it has run).
    Maintenance { action: CleanupAction, affected: A },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_kebab_case_names_of_the_rule_files() {
        let action: CleanupAction = serde_json::from_str("\"journal-vacuum\"").unwrap();
        assert_eq!(action, CleanupAction::JournalVacuum);
        let error = serde_json::from_str::<CleanupAction>("\"rm -rf /\"").unwrap_err();
        assert!(error.to_string().contains("unknown variant"), "{error}");
    }
}
