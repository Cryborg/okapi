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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Exclusions as written (`/home/ana/keep`, `*.log`): resolved again at
    /// each scan, never kept resolved.
    pub exclusions: Vec<String>,
    /// The folders the user wants emptied, as written: checked again at
    /// each scan, never kept resolved.
    pub custom_folders: Vec<String>,
    /// The minimum age, in minutes; `None` leaves the engine's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_age_minutes: Option<u64>,
    /// Whether each cleaning's history lists every entry (on a line before
    /// the cleaning's).
    pub deletion_log: bool,
    /// Whether an analysis offers what needs the administrator: on unless
    /// the user turned it off.
    pub admin_operations: bool,
    /// The language of both interfaces; `None` is the session's (D-7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// The interface's theme; `None` follows the desktop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<Theme>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            exclusions: Vec::new(),
            custom_folders: Vec::new(),
            min_age_minutes: None,
            deletion_log: false,
            admin_operations: admin_operations_by_default(),
            language: None,
            theme: None,
        }
    }
}

/// Whether what needs the administrator is offered when nothing says.
const fn admin_operations_by_default() -> bool {
    true
}

/// What [`Settings::save`] did with settings it could read.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replaced {
    Written,
    /// They no longer held what the caller expected: left as they are.
    ChangedElsewhere,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawSettings {
    #[expect(dead_code, reason = "checked by `file::check_version` first")]
    version: u32,
    #[serde(default)]
    exclusions: Vec<String>,
    #[serde(default)]
    custom_folders: Vec<String>,
    min_age_minutes: Option<u64>,
    #[serde(default)]
    deletion_log: bool,
    #[serde(default = "admin_operations_by_default")]
    admin_operations: bool,
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

    /// Replaces the settings of `directory` by these, if they still hold
    /// `over` (what the caller read or wrote there last) or already hold
    /// these (nothing is lost then), and unless they
    /// cannot be read (D-10: they are the user's to repair). Settings
    /// changed elsewhere since (by hand, by another window) are never
    /// replaced: the caller reads them again. Their values are compared, not
    /// their bytes: a file laid out again by hand is no change; none there
    /// holds the defaults. A change made between that reading and the end of
    /// the replacement (the time of one write) is still lost.
    ///
    /// # Errors
    ///
    /// Those of [`Self::load`] (the settings there are left as they are),
    /// and those of an atomic write: [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`] (a link there is never written through,
    /// D-9) or [`StoreError::Unwritable`].
    pub fn save(&self, directory: &Path, over: &Self) -> Result<Replaced, StoreError> {
        let there = file::replaceable(Self::load(directory))?;
        if there != *over && there != *self {
            tracing::warn!(path = %Self::path(directory).display(), changed = ?there.differing(over), lost = ?self.differing(over), "settings changed elsewhere: not written");
            return Ok(Replaced::ChangedElsewhere);
        }
        file::write_versioned(directory, SETTINGS_FILE, SETTINGS_VERSION, self)?;
        Ok(Replaced::Written)
    }

    /// The names of the settings that differ from `other`'s: what a refusal
    /// traces (the file's values are in its reading traced just before, the
    /// caller's where it made them). Every
    /// field is named, which the compiler checks.
    fn differing(&self, other: &Self) -> Vec<&'static str> {
        let Self {
            exclusions,
            custom_folders,
            min_age_minutes,
            deletion_log,
            admin_operations,
            language,
            theme,
        } = self;
        [
            ("exclusions", *exclusions != other.exclusions),
            ("customFolders", *custom_folders != other.custom_folders),
            ("minAgeMinutes", *min_age_minutes != other.min_age_minutes),
            ("deletionLog", *deletion_log != other.deletion_log),
            (
                "adminOperations",
                *admin_operations != other.admin_operations,
            ),
            ("language", *language != other.language),
            ("theme", *theme != other.theme),
        ]
        .into_iter()
        .filter_map(|(name, differs)| differs.then_some(name))
        .collect()
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
            custom_folders: raw.custom_folders,
            min_age_minutes: raw.min_age_minutes,
            deletion_log: raw.deletion_log,
            admin_operations: raw.admin_operations,
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
            r#"{"version": 1, "exclusions": ["/data/keep", "*.log"],
                "customFolders": ["/data/build", "/home/ana/tmp", "/srv/drop"],
                "minAgeMinutes": 90, "deletionLog": true, "adminOperations": false,
                "language": "en", "theme": "dark"}"#,
        )
        .unwrap();
        assert_eq!(settings, every_field());
        let defaults = load(r#"{"version": 1}"#).unwrap();
        assert_eq!(defaults, Settings::default());
        assert!(defaults.admin_operations, "offered unless turned off");
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
            custom_folders: vec![
                "/data/build".to_owned(),
                "/home/ana/tmp".to_owned(),
                "/srv/drop".to_owned(),
            ],
            min_age_minutes: Some(90),
            deletion_log: true,
            admin_operations: false,
            language: Some(Language::English),
            theme: Some(Theme::Dark),
        }
    }

    #[test]
    fn saved_settings_are_read_back_whole() {
        let directory = tempfile::tempdir().unwrap();
        let written = every_field()
            .save(directory.path(), &Settings::default())
            .unwrap();
        assert_eq!(written, Replaced::Written);
        assert_eq!(Settings::load(directory.path()).unwrap(), every_field());
        let light = Settings {
            theme: Some(Theme::Light),
            language: Some(Language::French),
            ..Settings::default()
        };
        let written = light.save(directory.path(), &every_field()).unwrap();
        assert_eq!(written, Replaced::Written);
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
    fn settings_changed_elsewhere_are_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join(SETTINGS_FILE);
        let by_hand = r#"{"version": 1, "exclusions": ["/data/keep"]}"#;
        fs::write(&file, by_hand).unwrap();
        // Read before the exclusion was added by hand: the defaults.
        let written = every_field()
            .save(directory.path(), &Settings::default())
            .unwrap();
        assert_eq!(written, Replaced::ChangedElsewhere);
        assert_eq!(fs::read_to_string(&file).unwrap(), by_hand);
        // Removed by hand since it was read: none there is the defaults.
        fs::remove_file(&file).unwrap();
        let lighter = Settings {
            theme: Some(Theme::Light),
            ..every_field()
        };
        let written = lighter.save(directory.path(), &every_field()).unwrap();
        assert_eq!(written, Replaced::ChangedElsewhere);
        assert!(!file.exists());
    }

    #[test]
    fn settings_changed_elsewhere_to_these_very_ones_are_no_refusal() {
        let directory = tempfile::tempdir().unwrap();
        // Another window made the same change first.
        let written = every_field()
            .save(directory.path(), &Settings::default())
            .unwrap();
        assert_eq!(written, Replaced::Written);
        let written = every_field()
            .save(directory.path(), &Settings::default())
            .unwrap();
        assert_eq!(written, Replaced::Written);
        assert_eq!(Settings::load(directory.path()).unwrap(), every_field());
    }

    #[test]
    fn a_refusal_names_each_setting_that_differs() {
        assert_eq!(
            every_field().differing(&Settings::default()),
            [
                "exclusions",
                "customFolders",
                "minAgeMinutes",
                "deletionLog",
                "adminOperations",
                "language",
                "theme"
            ]
        );
        assert_eq!(every_field().differing(&every_field()), Vec::<&str>::new());
        let lighter = Settings {
            theme: Some(Theme::Light),
            ..every_field()
        };
        assert_eq!(lighter.differing(&every_field()), ["theme"]);
    }

    #[test]
    fn settings_laid_out_again_by_hand_are_no_change() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(SETTINGS_FILE),
            r#"{"theme":"dark","exclusions":["/data/keep","*.log"],"version":1,
                "customFolders":["/data/build","/home/ana/tmp","/srv/drop"],
                "language":"en","deletionLog":true,"minAgeMinutes":90,"adminOperations":false}"#,
        )
        .unwrap();
        let written = Settings::default()
            .save(directory.path(), &every_field())
            .unwrap();
        assert_eq!(written, Replaced::Written);
        assert_eq!(
            Settings::load(directory.path()).unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn a_default_is_written_absent() {
        let directory = tempfile::tempdir().unwrap();
        let written = Settings::default()
            .save(directory.path(), &Settings::default())
            .unwrap();
        assert_eq!(written, Replaced::Written);
        assert_eq!(
            fs::read_to_string(directory.path().join(SETTINGS_FILE)).unwrap(),
            "{\n  \"version\": 1,\n  \"exclusions\": [],\n  \"customFolders\": [],\n  \"deletionLog\": false,\n  \"adminOperations\": true\n}\n"
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
                    every_field().save(directory.path(), &Settings::default()),
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
            every_field().save(&okapi, &Settings::default()),
            Err(StoreError::NotAFile { .. })
        ));
        assert_eq!(fs::read_to_string(&dotfiles).unwrap(), r#"{"version": 1}"#);
        let plain = directory.path().join("plain");
        let written = Settings::default()
            .save(&plain, &Settings::default())
            .unwrap();
        assert_eq!(written, Replaced::Written);
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
