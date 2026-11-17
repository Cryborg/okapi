use std::path::Path;

use serde::Deserialize;

use crate::name::Name;
use crate::rule_set::{RuleFileKind, misc_kind};
use crate::values::UniqueList;

/// The content of `misc.json`.
///
/// The protection list is required, even when empty: one that silently went
/// missing would leave the files it protects deletable.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MiscRules {
    #[serde(rename = "type", deserialize_with = "misc_kind")]
    pub(crate) kind: RuleFileKind,
    protected_event_logs: UniqueList<Name>,
}

impl MiscRules {
    /// Whether the file at `path` is an event log that must never be deleted.
    ///
    /// Only the file name counts, compared ignoring ASCII case as Windows does:
    /// the rules list `security.evtx` while the file is named `Security.evtx`.
    /// A name that is not UTF-8 never equals a protected name, which are ASCII.
    #[must_use]
    pub fn protects(&self, path: &Path) -> bool {
        path.file_name()
            .and_then(|file_name| file_name.to_str())
            .is_some_and(|file_name| {
                self.protected_event_logs
                    .iter()
                    .any(|protected| protected.as_str().eq_ignore_ascii_case(file_name))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn misc(json: &str) -> Result<MiscRules, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn protection_compares_whole_names_ignoring_ascii_case() {
        let rules = misc(
            r#"{"type": "misc", "protectedEventLogs": ["application.evtx", "security.evtx"]}"#,
        )
        .unwrap();
        for protected in [
            "SECURITY.EVTX",
            "Application.EVTX",
            "C:/Windows/Logs/Security.evtx",
        ] {
            assert!(rules.protects(Path::new(protected)), "{protected}");
        }
        for free in [
            "Security.evtx.bak",
            "xsecurity.evtx",
            "security.evt",
            "",
            "System.evtx",
        ] {
            assert!(!rules.protects(Path::new(free)), "{free}");
        }
        let empty = misc(r#"{"type": "misc", "protectedEventLogs": []}"#).unwrap();
        assert!(!empty.protects(Path::new("security.evtx")));
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_never_protected() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let rules = misc(r#"{"type": "misc", "protectedEventLogs": ["security.evtx"]}"#).unwrap();
        for bytes in [
            b"security.evtx\xFF".as_slice(),
            b"\xFFsecurity.evtx".as_slice(),
        ] {
            assert!(!rules.protects(Path::new(OsStr::from_bytes(bytes))));
        }
    }

    #[test]
    fn the_protection_list_is_required_and_has_no_duplicate() {
        for (json, expected) in [
            (r#"{"type": "misc"}"#, "missing field `protectedEventLogs`"),
            (
                r#"{"type": "misc", "protectedEventLogs": ["a.evtx", "a.evtx"]}"#,
                "duplicate value `a.evtx`",
            ),
            // The trash is a target of `system.json` now.
            (
                r#"{"type": "misc", "protectedEventLogs": [], "trashPath": null}"#,
                "unknown field `trashPath`",
            ),
        ] {
            let error = misc(json).unwrap_err().to_string();
            assert!(error.starts_with(expected), "{json}: {error}");
        }
    }
}
