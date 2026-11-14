//! What the user set, in `settings.json`: read by both interfaces, written
//! by the settings screen.

use std::fs;
use std::path::{Path, PathBuf};

use okapi_rules::Language;
use serde::{Deserialize, Serialize};

use crate::{StoreError, file};

const SETTINGS_FILE: &str = "settings.json";
/// The settings format this Okapi reads and writes.
const SETTINGS_VERSION: u32 = 1;

/// The interface light or dark, as the user chose it over the desktop's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Light,
    Dark,
}

/// What the user set. A missing file means none of it; an optional setting
/// absent leaves its default (the engine's age, the session's language, the
/// desktop's theme), and is written absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Exclusions as written (`/home/ana/keep`, `*.log`): resolved again at
    /// each scan, never kept resolved.
    pub exclusions: Vec<String>,
    /// The minimum age, in minutes; `None` leaves the engine's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_age_minutes: Option<u64>,
    /// Whether each cleaning's history line lists every entry.
    pub deletion_log: bool,
    /// The language of both interfaces; `None` is the session's (D-7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// The interface's theme; `None` follows the desktop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<Theme>,
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
    language: Option<Language>,
    theme: Option<Theme>,
}

impl Settings {
    /// The file the settings of `directory` live in.
    #[must_use]
    pub fn path(directory: &Path) -> PathBuf {
        directory.join(SETTINGS_FILE)
    }

    /// Whether the settings of `directory` are a link (kept with the user's
    /// dotfiles): never written then (D-9), which the settings screen says
    /// before anything is tried.
    #[must_use]
    pub fn is_link(directory: &Path) -> bool {
        fs::symlink_metadata(Self::path(directory)).is_ok_and(|metadata| metadata.is_symlink())
    }

    /// Replaces the settings of `directory` by these, unless those there
    /// cannot be read (D-10: they are the user's to repair).
    ///
    /// # Errors
    ///
    /// Those of [`Self::load`] (the settings there are left as they are),
    /// and those of an atomic write: [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`] (a link there is never written through,
    /// D-9) or [`StoreError::Unwritable`].
    pub fn save(&self, directory: &Path) -> Result<(), StoreError> {
        file::replaceable(Self::load(directory))?;
        file::write_versioned(directory, SETTINGS_FILE, SETTINGS_VERSION, self)
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
            language: raw.language,
            theme: raw.theme,
        };
        tracing::info!(path = %path.display(), ?settings, "settings loaded");
        Ok(settings)
    }

    /// The language of both interfaces: the one chosen in the settings of
    /// `directory`, else the `session`'s (D-7), also when they cannot be
    /// read (traced; what uses them says why, in that language).
    #[must_use]
    pub fn language(directory: Option<&Path>, session: Language) -> Language {
        let chosen = directory.and_then(|directory| {
            Self::load(directory)
                .inspect_err(|error| {
                    tracing::warn!(%error, "settings unreadable: the session's language");
                })
                .ok()?
                .language
        });
        chosen.unwrap_or(session)
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
                "deletionLog": true, "language": "en", "theme": "dark"}"#,
        )
        .unwrap();
        assert_eq!(settings, every_field());
        assert_eq!(load(r#"{"version": 1}"#).unwrap(), Settings::default());
        for unknown in [r#""language": "de""#, r#""theme": "blue""#] {
            assert!(
                matches!(
                    load(&format!(r#"{{"version": 1, {unknown}}}"#)),
                    Err(StoreError::Invalid { .. })
                ),
                "{unknown}"
            );
        }
    }

    /// Settings where no field has its default.
    fn every_field() -> Settings {
        Settings {
            exclusions: vec!["/data/keep".to_owned(), "*.log".to_owned()],
            min_age_minutes: Some(90),
            deletion_log: true,
            language: Some(Language::English),
            theme: Some(Theme::Dark),
        }
    }

    #[test]
    fn saved_settings_are_read_back_whole() {
        let directory = tempfile::tempdir().unwrap();
        every_field().save(directory.path()).unwrap();
        assert_eq!(Settings::load(directory.path()).unwrap(), every_field());
        let light = Settings {
            theme: Some(Theme::Light),
            language: Some(Language::French),
            ..Settings::default()
        };
        light.save(directory.path()).unwrap();
        assert_eq!(Settings::load(directory.path()).unwrap(), light);
    }

    #[test]
    fn the_language_is_the_session_s_unless_readable_settings_choose_one() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join(SETTINGS_FILE);
        // Each session, the other language chosen: no answer right by chance.
        for (session, other, code) in [
            (Language::English, Language::French, "fr"),
            (Language::French, Language::English, "en"),
        ] {
            let language = || Settings::language(Some(directory.path()), session);
            if file.exists() {
                fs::remove_file(&file).unwrap();
            }
            assert_eq!(language(), session, "no file");
            assert_eq!(Settings::language(None, session), session);
            fs::write(&file, format!(r#"{{"version": 1, "language": "{code}"}}"#)).unwrap();
            assert_eq!(language(), other);
            fs::write(&file, r#"{"version": 1}"#).unwrap();
            assert_eq!(language(), session);
            fs::write(
                &file,
                format!(r#"{{"version": 1, "language": "{code}", "unknown": 1}}"#),
            )
            .unwrap();
            assert_eq!(
                language(),
                session,
                "unreadable: the session's, never the language of a file refused"
            );
        }
    }

    #[test]
    fn a_default_is_written_absent() {
        let directory = tempfile::tempdir().unwrap();
        Settings::default().save(directory.path()).unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join(SETTINGS_FILE)).unwrap(),
            "{\n  \"version\": 1,\n  \"exclusions\": [],\n  \"deletionLog\": false\n}\n"
        );
    }

    #[test]
    fn unreadable_settings_are_never_replaced() {
        for text in [
            "not json",
            r#"{"version": 1, "exclusion": []}"#,
            r#"{"version": 2}"#,
        ] {
            let directory = tempfile::tempdir().unwrap();
            fs::write(directory.path().join(SETTINGS_FILE), text).unwrap();
            assert!(
                matches!(
                    every_field().save(directory.path()),
                    Err(StoreError::Invalid { .. } | StoreError::UnsupportedVersion { .. })
                ),
                "{text}: the error of reading them"
            );
            assert_eq!(
                fs::read_to_string(directory.path().join(SETTINGS_FILE)).unwrap(),
                text
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn settings_that_are_a_link_are_never_written() {
        let directory = tempfile::tempdir().unwrap();
        let dotfiles = directory.path().join("dotfiles.json");
        fs::write(&dotfiles, r#"{"version": 1}"#).unwrap();
        let okapi = directory.path().join("okapi");
        fs::create_dir(&okapi).unwrap();
        assert!(!Settings::is_link(&okapi), "no settings");
        std::os::unix::fs::symlink(&dotfiles, okapi.join(SETTINGS_FILE)).unwrap();
        assert!(Settings::is_link(&okapi));
        assert!(matches!(
            every_field().save(&okapi),
            Err(StoreError::NotAFile { .. })
        ));
        assert_eq!(fs::read_to_string(&dotfiles).unwrap(), r#"{"version": 1}"#);
        let plain = directory.path().join("plain");
        Settings::default().save(&plain).unwrap();
        assert!(!Settings::is_link(&plain), "a file");
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
