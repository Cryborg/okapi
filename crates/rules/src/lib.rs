//! Cleaning rules: serde types, template variable resolution and validation,
//! and the texts of the rules as the user reads them ([`label`]).
//!
//! The rules are data, embedded at compile time from `data/<os>/*.json` (taken
//! from Kudu, see `THIRD_PARTY_NOTICES`). Each value is validated while it is
//! read, so it always satisfies the format's own constraints. The constraints
//! that span several files (unique rule identifiers and database labels, paths
//! whose variable or root exists on the OS) are checked by [`RuleSet`]: only a
//! `RuleSet` is valid as a whole. A rule never carries a command (invariant 4).
//!
//! ```
//! use okapi_rules::{Os, RuleSet};
//!
//! let rules = RuleSet::embedded(Os::Linux)?;
//! assert!(rules.apps().iter().any(|rule| rule.id().as_str() == "discord"));
//! # Ok::<(), okapi_rules::RulesError>(())
//! ```

mod app;
mod browser;
mod database;
mod effect;
mod error;
mod misc;
mod name;
mod os;
mod rule_set;
mod steam;
mod system;
mod template;
mod texts;
mod values;

pub use app::{
    AgeDays, AnchorPath, AnchorSegment, AppCacheRule, AppDeletion, DirectFileMatch, MaxDepth,
    PathMatcher, RecursivePathMatch,
};
pub use browser::{
    BrowserRules, ChromiumBrowser, ChromiumBrowserKind, ChromiumCacheDirectories,
    ChromiumCacheDirectory, FirefoxFork, FirefoxForkKind, FirefoxPaths, SafariPaths,
};
pub use database::{DatabaseRules, DatabaseTarget, ProfileLayout};
pub use effect::{CleanupAction, Effect};
pub use error::{RulesError, TemplateError, ValidationError};
pub use misc::MiscRules;
pub use name::{Name, NamePattern, RelativePath};
pub use os::Os;
pub use rule_set::{RuleFileKind, RuleSet};
pub use steam::SteamRules;
pub use system::{CleanTarget, SingleFileTarget, SystemRules, TargetDeletion};
pub use template::{TemplatePath, TemplateVariables, Variable};
pub use texts::{Language, SESSION_VARIABLES, group_label, label};
pub use values::{Label, NonEmpty, RuleId};

/// The notices of the third-party material embedded here (the rules come from
/// Kudu, under the MIT license): shown wherever Okapi lists its licenses.
pub const THIRD_PARTY_NOTICES: &str = include_str!("../../../THIRD_PARTY_NOTICES");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notices_carry_the_kudu_license() {
        assert!(THIRD_PARTY_NOTICES.contains("https://github.com/adventdevinc/kudu"));
        assert!(THIRD_PARTY_NOTICES.contains("Copyright (c) 2026 Advent Development Inc"));
        assert!(
            THIRD_PARTY_NOTICES.contains("The above copyright notice and this permission notice")
        );
    }
}
