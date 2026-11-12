use serde::Deserialize;

use crate::name::RelativePath;
use crate::rule_set::{RuleFileKind, steam_kind};
use crate::template::TemplatePath;
use crate::values::UniqueList;

/// The content of `steam.json`: where Steam libraries usually are, and the
/// folders of redistributable installers that games leave behind.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SteamRules {
    #[serde(rename = "type", deserialize_with = "steam_kind")]
    pub(crate) kind: RuleFileKind,
    libraries: UniqueList<TemplatePath>,
    #[serde(rename = "redistPatterns")]
    redistributable_patterns: UniqueList<RelativePath>,
}

impl SteamRules {
    /// The usual locations of Steam libraries.
    #[must_use]
    pub fn libraries(&self) -> &[TemplatePath] {
        &self.libraries
    }

    /// Redistributable installer folders, relative to a game directory, such
    /// as `_CommonRedist` or `WindowsNoEditor/Engine/Extras/Redist`.
    #[must_use]
    pub fn redistributable_patterns(&self) -> &[RelativePath] {
        &self.redistributable_patterns
    }

    pub(crate) fn template_paths(&self) -> impl Iterator<Item = &TemplatePath> {
        self.libraries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_of(json: &str) -> String {
        serde_json::from_str::<SteamRules>(json)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn rejects_repeated_entries_escapes_and_unknown_fields() {
        for (json, expected) in [
            (
                r#"{"type": "steam", "libraries": ["${HOME}/s", "${HOME}/s"], "redistPatterns": []}"#,
                "duplicate value `${HOME}/s`",
            ),
            (
                r#"{"type": "steam", "libraries": [], "redistPatterns": ["Redist", "Redist"]}"#,
                "duplicate value `Redist`",
            ),
            (
                r#"{"type": "steam", "libraries": [], "redistPatterns": ["../Redist"]}"#,
                "`../Redist` is not a relative path",
            ),
            (
                r#"{"type": "steam", "libraries": [], "redistPatterns": [], "command": "x"}"#,
                "unknown field `command`",
            ),
        ] {
            let error = error_of(json);
            assert!(error.starts_with(expected), "{json}: {error}");
        }
    }
}
