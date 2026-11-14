//! How the user last left the interface (the sidebar folded or not), kept
//! from one launch to the next. It is state, not a setting: Okapi writes it
//! at each choice, the user never edits it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{StoreError, file};

const INTERFACE_STATE_FILE: &str = "interface.json";
/// The format this Okapi reads.
const INTERFACE_STATE_VERSION: u32 = 1;

/// How the user last left the interface. A missing file means as it opens
/// by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterfaceState {
    /// The sidebar folded by the user's choice (a narrow window folds it
    /// whatever the choice).
    pub sidebar_folded: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawInterfaceState {
    #[expect(dead_code, reason = "checked by `file::check_version` first")]
    version: u32,
    sidebar_folded: bool,
}

impl InterfaceState {
    /// The file the state of `directory` lives in.
    #[must_use]
    pub fn path(directory: &Path) -> PathBuf {
        directory.join(INTERFACE_STATE_FILE)
    }

    /// Reads the state kept in `directory`; the default when there is none.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unreadable`], [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`], [`StoreError::TooLarge`],
    /// [`StoreError::UnsupportedVersion`] or [`StoreError::Invalid`].
    pub fn load(directory: &Path) -> Result<Self, StoreError> {
        let path = Self::path(directory);
        let Some(raw) = file::read_versioned::<RawInterfaceState>(&path, INTERFACE_STATE_VERSION)?
        else {
            tracing::info!(path = %path.display(), "no interface state, defaults used");
            return Ok(Self::default());
        };
        let state = Self {
            sidebar_folded: raw.sidebar_folded,
        };
        tracing::info!(path = %path.display(), ?state, "interface state loaded");
        Ok(state)
    }

    /// Keeps the state in `directory`, replacing the one there whole,
    /// unless that one cannot be read: it is then left as it is.
    ///
    /// # Errors
    ///
    /// Those of [`Self::load`] for a state there that cannot be read, and
    /// those of an atomic write: [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`] (a link there is never written through) or
    /// [`StoreError::Unwritable`].
    pub fn save(&self, directory: &Path) -> Result<(), StoreError> {
        file::replaceable(Self::load(directory))?;
        file::write_versioned(
            directory,
            INTERFACE_STATE_FILE,
            INTERFACE_STATE_VERSION,
            self,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn load(text: &str) -> Result<InterfaceState, StoreError> {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(INTERFACE_STATE_FILE), text).unwrap();
        InterfaceState::load(directory.path())
    }

    #[test]
    fn a_state_comes_back_as_it_was_kept_in_the_documented_form() {
        let directory = tempfile::tempdir().unwrap();
        for folded in [true, false] {
            let state = InterfaceState {
                sidebar_folded: folded,
            };
            state.save(directory.path()).unwrap();
            assert_eq!(InterfaceState::load(directory.path()).unwrap(), state);
            let text = fs::read_to_string(directory.path().join(INTERFACE_STATE_FILE)).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&text).unwrap(),
                serde_json::json!({ "version": 1, "sidebarFolded": folded })
            );
        }
    }

    #[test]
    fn a_state_that_cannot_be_read_is_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = InterfaceState::path(directory.path());
        let folded = InterfaceState {
            sidebar_folded: true,
        };
        for text in [
            "{",
            r#"{"version": 1}"#,
            r#"{"version": 1, "sidebarFolded": "yes"}"#,
        ] {
            fs::write(&path, text).unwrap();
            assert!(folded.save(directory.path()).is_err(), "{text}");
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        fs::remove_file(&path).unwrap();
        folded.save(directory.path()).unwrap();
        assert_eq!(InterfaceState::load(directory.path()).unwrap(), folded);
    }

    #[test]
    fn no_file_is_the_default_unfolded() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            InterfaceState::load(directory.path()).unwrap(),
            InterfaceState {
                sidebar_folded: false
            }
        );
    }

    #[test]
    fn a_mistake_in_the_file_is_an_error_never_the_default() {
        for text in [
            r#"{"version": 1}"#,
            r#"{"version": 1, "sidebarFolded": "yes"}"#,
            r#"{"version": 1, "sidebarFolded": true, "theme": "dark"}"#,
            r#"{"sidebarFolded": true}"#,
            "{",
        ] {
            assert!(
                matches!(load(text), Err(StoreError::Invalid { .. })),
                "{text}"
            );
        }
        assert!(matches!(
            load(r#"{"version": 2, "sidebarFolded": true, "theme": "dark"}"#),
            Err(StoreError::UnsupportedVersion {
                version: 2,
                supported: INTERFACE_STATE_VERSION,
                ..
            })
        ));
    }
}
