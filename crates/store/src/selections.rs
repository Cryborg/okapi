//! What the user unchecked among the results, kept from one analysis to the
//! next: the rules of the groups checked by default that they unchecked one
//! by one (D-13). It is state, not a setting: Okapi writes it at each choice,
//! the user never edits it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use okapi_engine::OriginKey;
use serde::{Deserialize, Serialize};

use crate::{StoreError, file};

const SELECTIONS_FILE: &str = "selections.json";
/// The format this Okapi reads.
const SELECTIONS_VERSION: u32 = 1;

/// The user's choices among the results. A missing file means none: every
/// group as the engine checks it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Selections {
    /// The rules of the groups the user unchecked, by their key.
    pub unchecked: BTreeSet<OriginKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawSelections {
    #[expect(dead_code, reason = "checked by `file::check_version` first")]
    version: u32,
    unchecked: BTreeSet<OriginKey>,
}

impl Selections {
    /// The file the choices of `directory` live in.
    #[must_use]
    pub fn path(directory: &Path) -> PathBuf {
        directory.join(SELECTIONS_FILE)
    }

    /// Reads the choices kept in `directory`; none when there is no file.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unreadable`], [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`], [`StoreError::TooLarge`],
    /// [`StoreError::UnsupportedVersion`] or [`StoreError::Invalid`].
    pub fn load(directory: &Path) -> Result<Self, StoreError> {
        let path = Self::path(directory);
        let Some(raw) = file::read_versioned::<RawSelections>(&path, SELECTIONS_VERSION)? else {
            tracing::info!(path = %path.display(), "no choices kept: every group as by default");
            return Ok(Self::default());
        };
        tracing::info!(path = %path.display(), unchecked = raw.unchecked.len(), "choices loaded");
        Ok(Self {
            unchecked: raw.unchecked,
        })
    }

    /// Keeps the choices in `directory`, replacing those there whole,
    /// unless those cannot be read: they are then left as they are.
    ///
    /// # Errors
    ///
    /// Those of [`Self::load`] for choices there that cannot be read, and
    /// those of an atomic write: [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`] (a link there is never written through) or
    /// [`StoreError::Unwritable`].
    pub fn save(&self, directory: &Path) -> Result<(), StoreError> {
        file::replaceable(Self::load(directory))?;
        file::write_versioned(directory, SELECTIONS_FILE, SELECTIONS_VERSION, self)?;
        tracing::info!(path = %Self::path(directory).display(), unchecked = self.unchecked.len(), "choices kept");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn load(text: &str) -> Result<Selections, StoreError> {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(SELECTIONS_FILE), text).unwrap();
        Selections::load(directory.path())
    }

    fn key(parts: &[&str]) -> OriginKey {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[test]
    fn choices_come_back_as_they_were_kept_in_the_documented_form() {
        let directory = tempfile::tempdir().unwrap();
        let selections = Selections {
            unchecked: BTreeSet::from([
                key(&["app", "pip"]),
                key(&["systemTarget", "Crash reports", "/var/crash"]),
            ]),
        };
        selections.save(directory.path()).unwrap();
        assert_eq!(Selections::load(directory.path()).unwrap(), selections);
        let text = fs::read_to_string(Selections::path(directory.path())).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({
                "version": 1,
                "unchecked": [["app", "pip"], ["systemTarget", "Crash reports", "/var/crash"]],
            })
        );
    }

    #[test]
    fn no_file_is_no_choice() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            Selections::load(directory.path()).unwrap(),
            Selections::default()
        );
    }

    #[test]
    fn a_mistake_in_the_file_is_an_error_never_no_choice() {
        for text in [
            r#"{"version": 1}"#,
            r#"{"version": 1, "unchecked": ["app"]}"#,
            r#"{"version": 1, "unchecked": [], "checked": []}"#,
            r#"{"unchecked": []}"#,
            "{",
        ] {
            assert!(
                matches!(load(text), Err(StoreError::Invalid { .. })),
                "{text}"
            );
        }
        assert!(matches!(
            load(r#"{"version": 2, "unchecked": []}"#),
            Err(StoreError::UnsupportedVersion {
                version: 2,
                supported: SELECTIONS_VERSION,
                ..
            })
        ));
    }

    #[test]
    fn choices_that_cannot_be_read_are_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = Selections::path(directory.path());
        let selections = Selections {
            unchecked: BTreeSet::from([key(&["app", "pip"])]),
        };
        for text in [
            "{",
            r#"{"version": 1}"#,
            r#"{"version": 1, "unchecked": 3}"#,
            r#"{"version": 2, "unchecked": []}"#,
        ] {
            fs::write(&path, text).unwrap();
            assert!(selections.save(directory.path()).is_err(), "{text}");
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        fs::remove_file(&path).unwrap();
        selections.save(directory.path()).unwrap();
        assert_eq!(Selections::load(directory.path()).unwrap(), selections);
    }
}
