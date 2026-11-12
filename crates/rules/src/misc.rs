use std::path::Path;

use serde::Deserialize;

use crate::name::Name;
use crate::rule_set::{RuleFileKind, misc_kind};
use crate::template::{BoundedPath, TemplatePath};
use crate::values::UniqueList;

/// The content of `misc.json`.
///
/// Both fields are required, even when empty or `null`: a protection list
/// that silently went missing would leave the files it protects deletable.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MiscRules {
    #[serde(rename = "type", deserialize_with = "misc_kind")]
    pub(crate) kind: RuleFileKind,
    protected_event_logs: UniqueList<Name>,
    // `deserialize_with` makes the key required: serde only fills a missing
    // `Option` field with `None` when it deserializes the field itself.
    #[serde(deserialize_with = "Option::deserialize")]
    trash_path: Option<BoundedPath>,
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

    /// The trash directory, or `None` when the OS manages it itself; never a
    /// variable alone other than `${TMPDIR}`.
    #[must_use]
    pub fn trash_path(&self) -> Option<&TemplatePath> {
        self.trash_path.as_deref()
    }

    pub(crate) fn template_paths(&self) -> impl Iterator<Item = &TemplatePath> {
        self.trash_path.as_deref().into_iter()
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
            r#"{"type": "misc", "protectedEventLogs": ["application.evtx", "security.evtx"], "trashPath": null}"#,
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
        let empty =
            misc(r#"{"type": "misc", "protectedEventLogs": [], "trashPath": null}"#).unwrap();
        assert!(!empty.protects(Path::new("security.evtx")));
    }

    #[test]
    fn reads_the_trash_path() {
        let rules = misc(r#"{"type": "misc", "protectedEventLogs": [], "trashPath": "${LOCAL_SHARE}/Trash/files"}"#).unwrap();
        assert_eq!(
            rules.trash_path().unwrap().to_string(),
            "${LOCAL_SHARE}/Trash/files"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_never_protected() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let rules =
            misc(r#"{"type": "misc", "protectedEventLogs": ["security.evtx"], "trashPath": null}"#)
                .unwrap();
        for bytes in [
            b"security.evtx\xFF".as_slice(),
            b"\xFFsecurity.evtx".as_slice(),
        ] {
            assert!(!rules.protects(Path::new(OsStr::from_bytes(bytes))));
        }
    }

    #[test]
    fn both_fields_are_required_and_the_trash_is_bounded() {
        for (json, expected) in [
            (
                r#"{"type": "misc", "trashPath": null}"#,
                "missing field `protectedEventLogs`",
            ),
            (
                r#"{"type": "misc", "protectedEventLogs": []}"#,
                "missing field `trashPath`",
            ),
            (
                r#"{"type": "misc", "protectedEventLogs": [], "trashPath": "${HOME}"}"#,
                "`${HOME}` alone would empty",
            ),
            (
                r#"{"type": "misc", "protectedEventLogs": ["a.evtx", "a.evtx"], "trashPath": null}"#,
                "duplicate value `a.evtx`",
            ),
        ] {
            let error = misc(json).unwrap_err().to_string();
            assert!(error.starts_with(expected), "{json}: {error}");
        }
    }
}
