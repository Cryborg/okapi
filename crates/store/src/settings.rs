//! What the user set, in `settings.json`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{StoreError, file};

const SETTINGS_FILE: &str = "settings.json";
/// The settings format this Okapi reads.
const SETTINGS_VERSION: u32 = 1;

/// What the user set. A missing file means none of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// Exclusions as written (`/home/ana/keep`, `*.log`): resolved again at
    /// each scan, never kept resolved.
    pub exclusions: Vec<String>,
    /// The minimum age, in minutes; `None` leaves the engine's default.
    pub min_age_minutes: Option<u64>,
    /// Whether each cleaning's history line lists every entry.
    pub deletion_log: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawSettings {
    #[expect(dead_code, reason = "checked by `file::check_version` first")]
    version: u32,
    #[serde(default)]
    exclusions: Vec<String>,
    min_age_minutes: Option<u64>,
    #[serde(default)]
    deletion_log: bool,
}

impl Settings {
    /// The file the settings of `directory` live in.
    #[must_use]
    pub fn path(directory: &Path) -> PathBuf {
        directory.join(SETTINGS_FILE)
    }

    /// Reads the settings in `directory`; the defaults when there are none.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unreadable`], [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`], [`StoreError::TooLarge`],
    /// [`StoreError::Invalid`] (an unknown field included: a typing mistake
    /// must not be ignored) or [`StoreError::UnsupportedVersion`]. Never the
    /// defaults instead.
    pub fn load(directory: &Path) -> Result<Self, StoreError> {
        let path = Self::path(directory);
        let Some(raw) = file::read_versioned::<RawSettings>(&path, SETTINGS_VERSION)? else {
            tracing::info!(path = %path.display(), "no settings, defaults used");
            return Ok(Self::default());
        };
        let settings = Self {
            exclusions: raw.exclusions,
            min_age_minutes: raw.min_age_minutes,
            deletion_log: raw.deletion_log,
        };
        tracing::info!(path = %path.display(), ?settings, "settings loaded");
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::file::MAX_FILE_SIZE;

    fn load(text: &str) -> Result<Settings, StoreError> {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(SETTINGS_FILE), text).unwrap();
        Settings::load(directory.path())
    }

    #[test]
    fn every_field_is_read() {
        let settings = load(
            r#"{"version": 1, "exclusions": ["/data/keep", "*.log"], "minAgeMinutes": 90,
                "deletionLog": true}"#,
        )
        .unwrap();
        assert_eq!(
            settings,
            Settings {
                exclusions: vec!["/data/keep".to_owned(), "*.log".to_owned()],
                min_age_minutes: Some(90),
                deletion_log: true,
            }
        );
        assert_eq!(load(r#"{"version": 1}"#).unwrap(), Settings::default());
    }

    #[test]
    fn without_a_version_a_mistyped_field_is_named_first() {
        match load(r#"{"exclusion": []}"#) {
            Err(StoreError::Invalid { error, .. }) => {
                assert!(
                    error.to_string().contains("unknown field `exclusion`"),
                    "{error}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_file_means_the_defaults() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            Settings::load(directory.path()).unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn a_mistake_is_an_error_naming_the_file_never_the_defaults() {
        for (text, line) in [
            (r#"{"version": 1, "exclusion": []}"#, 1),
            ("{\n  \"exclusions\": []\n}", 3),
            (r#"{"version": 1, "minAgeMinutes": -5}"#, 1),
            ("not json", 1),
        ] {
            match load(text) {
                Err(StoreError::Invalid { path, error }) => {
                    assert!(path.ends_with(SETTINGS_FILE), "{text}");
                    assert_eq!(error.line(), line, "{text}");
                }
                other => panic!("{text}: {other:?}"),
            }
        }
        assert!(matches!(
            load(r#"{"version": 2, "fieldOfLater": true}"#),
            Err(StoreError::UnsupportedVersion {
                version: 2,
                supported: SETTINGS_VERSION,
                ..
            })
        ));
        assert!(matches!(
            load(r#"{"version": 2}"#),
            Err(StoreError::UnsupportedVersion {
                version: 2,
                supported: SETTINGS_VERSION,
                ..
            })
        ));
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(SETTINGS_FILE),
            b"{\"version\": 1, \"exclusions\": [\"\xff\"]}",
        )
        .unwrap();
        assert!(
            matches!(
                Settings::load(directory.path()),
                Err(StoreError::Invalid { .. })
            ),
            "not UTF-8 is invalid content, not unreadable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn settings_behind_a_dangling_link_are_never_taken_for_none() {
        let directory = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            directory.path().join("dotfiles/settings.json"),
            directory.path().join(SETTINGS_FILE),
        )
        .unwrap();
        assert!(matches!(
            Settings::load(directory.path()),
            Err(StoreError::BrokenLink { .. })
        ));
        let linked = directory.path().join("okapi");
        std::os::unix::fs::symlink(directory.path().join("unmounted"), &linked).unwrap();
        assert!(matches!(
            Settings::load(&linked),
            Err(StoreError::BrokenLink { .. })
        ));
    }

    #[test]
    fn settings_that_are_not_a_file_are_an_error() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(SETTINGS_FILE)).unwrap();
        assert!(matches!(
            Settings::load(directory.path()),
            Err(StoreError::NotAFile { .. })
        ));
    }

    #[test]
    fn settings_in_a_file_where_a_directory_belongs_are_unreadable() {
        let directory = tempfile::tempdir().unwrap();
        let not_a_directory = directory.path().join("okapi");
        fs::write(&not_a_directory, b"").unwrap();
        assert!(matches!(
            Settings::load(&not_a_directory),
            Err(StoreError::Unreadable { .. })
        ));
    }

    #[test]
    fn a_settings_file_too_large_is_never_read() {
        let directory = tempfile::tempdir().unwrap();
        let file = fs::File::create(directory.path().join(SETTINGS_FILE)).unwrap();
        file.set_len(MAX_FILE_SIZE + 1).unwrap();
        assert!(matches!(
            Settings::load(directory.path()),
            Err(StoreError::TooLarge {
                limit: MAX_FILE_SIZE,
                ..
            })
        ));
        // At the limit, the content is read (and judged).
        let file = fs::File::create(directory.path().join(SETTINGS_FILE)).unwrap();
        file.set_len(MAX_FILE_SIZE).unwrap();
        assert!(matches!(
            Settings::load(directory.path()),
            Err(StoreError::Invalid { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn settings_behind_a_valid_link_are_read() {
        let directory = tempfile::tempdir().unwrap();
        let dotfiles = directory.path().join("dotfiles");
        fs::create_dir(&dotfiles).unwrap();
        fs::write(
            dotfiles.join(SETTINGS_FILE),
            r#"{"version": 1, "minAgeMinutes": 7, "deletionLog": true}"#,
        )
        .unwrap();
        let okapi = directory.path().join("okapi");
        fs::create_dir(&okapi).unwrap();
        std::os::unix::fs::symlink(dotfiles.join(SETTINGS_FILE), okapi.join(SETTINGS_FILE))
            .unwrap();
        let settings = Settings::load(&okapi).unwrap();
        assert_eq!(settings.min_age_minutes, Some(7));
        assert!(settings.deletion_log);
    }

    #[cfg(unix)]
    #[test]
    fn settings_linked_to_a_device_are_refused_not_read() {
        let directory = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/dev/zero", directory.path().join(SETTINGS_FILE)).unwrap();
        assert!(matches!(
            Settings::load(directory.path()),
            Err(StoreError::NotAFile { .. })
        ));
    }
}
