use std::collections::BTreeMap;
use std::fmt;

use serde::de::value::SeqAccessDeserializer;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::error::ValidationError;
use crate::name::{NamePattern, RelativePath};
use crate::rule_set::{RuleFileKind, databases_kind};
use crate::template::TemplatePath;
use crate::values::{Label, NonEmpty, ensure_unique, first_duplicate};

/// The profile directories looked at when a multi-profile target names none.
const DEFAULT_PROFILE_PATTERNS: [&str; 2] = ["Default", "Profile *"];

/// Where the databases of a target sit below its base path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileLayout {
    /// Directly below the base path.
    Single,
    /// In each profile directory below the base path whose name matches one
    /// of these patterns.
    PerProfile(NonEmpty<NamePattern>),
}

/// SQLite databases of one application, to compact with `VACUUM`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseTarget {
    label: Label,
    base_path: TemplatePath,
    database_files: NonEmpty<RelativePath>,
    profiles: ProfileLayout,
    description: Option<String>,
}

impl DatabaseTarget {
    #[must_use]
    pub const fn label(&self) -> &Label {
        &self.label
    }

    #[must_use]
    pub const fn base_path(&self) -> &TemplatePath {
        &self.base_path
    }

    /// The database files, relative to the base path or to each profile.
    #[must_use]
    pub fn database_files(&self) -> &[RelativePath] {
        &self.database_files
    }

    #[must_use]
    pub const fn profiles(&self) -> &ProfileLayout {
        &self.profiles
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
}

/// The content of `databases.json`, with every `$name` reference to
/// `sharedDbFileSets` replaced by the files of that set.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawDatabaseRules")]
pub struct DatabaseRules {
    pub(crate) kind: RuleFileKind,
    targets: Vec<DatabaseTarget>,
}

impl DatabaseRules {
    #[must_use]
    pub fn targets(&self) -> &[DatabaseTarget] {
        &self.targets
    }

    pub(crate) fn template_paths(&self) -> impl Iterator<Item = &TemplatePath> {
        self.targets.iter().map(DatabaseTarget::base_path)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawDatabaseRules {
    #[serde(rename = "type", deserialize_with = "databases_kind")]
    kind: RuleFileKind,
    #[serde(
        default,
        rename = "sharedDbFileSets",
        deserialize_with = "unique_key_map"
    )]
    shared_file_sets: BTreeMap<String, FileList>,
    targets: Vec<TargetEntry>,
}

/// A non-empty list of database files without repeats, checked while it is
/// read so the error points at the list.
#[derive(Clone)]
struct FileList(NonEmpty<RelativePath>);

impl<'de> Deserialize<'de> for FileList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let files = NonEmpty::<RelativePath>::deserialize(deserializer)?;
        match first_duplicate(files.iter()) {
            Some(duplicate) => Err(de::Error::custom(ValidationError::DuplicateValue {
                value: duplicate.to_string(),
            })),
            None => Ok(Self(files)),
        }
    }
}

/// `dbFiles`: either the files themselves, or `$name`, a reference to an
/// entry of `sharedDbFileSets`.
enum RawDatabaseFiles {
    Files(FileList),
    SetReference(String),
}

impl<'de> Deserialize<'de> for RawDatabaseFiles {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FilesVisitor;

        impl<'de> Visitor<'de> for FilesVisitor {
            type Value = RawDatabaseFiles;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a list of database files or a `$name` reference")
            }

            fn visit_str<E: de::Error>(self, reference: &str) -> Result<Self::Value, E> {
                Ok(RawDatabaseFiles::SetReference(reference.to_owned()))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, files: A) -> Result<Self::Value, A::Error> {
                FileList::deserialize(SeqAccessDeserializer::new(files))
                    .map(RawDatabaseFiles::Files)
            }
        }

        deserializer.deserialize_any(FilesVisitor)
    }
}

/// A database target as written, with everything but its `$name` reference checked.
#[derive(Deserialize)]
#[serde(try_from = "RawDatabaseTarget")]
struct TargetEntry {
    label: Label,
    base_path: TemplatePath,
    database_files: RawDatabaseFiles,
    profiles: ProfileLayout,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawDatabaseTarget {
    label: Label,
    base_path: TemplatePath,
    #[serde(rename = "dbFiles")]
    database_files: RawDatabaseFiles,
    #[serde(default)]
    multi_profile: bool,
    #[serde(rename = "profilePattern")]
    profile_patterns: Option<NonEmpty<NamePattern>>,
    description: Option<String>,
}

impl TryFrom<RawDatabaseTarget> for TargetEntry {
    type Error = ValidationError;

    fn try_from(raw: RawDatabaseTarget) -> Result<Self, Self::Error> {
        let profiles = match (raw.multi_profile, raw.profile_patterns) {
            (false, None) => ProfileLayout::Single,
            (false, Some(_)) => {
                return Err(ValidationError::ProfilePatternWithoutProfiles {
                    label: raw.label.to_string(),
                });
            }
            (true, Some(patterns)) => {
                ensure_unique("profilePattern", &patterns)?;
                ProfileLayout::PerProfile(patterns)
            }
            (true, None) => ProfileLayout::PerProfile(default_profile_patterns()?),
        };
        Ok(Self {
            label: raw.label,
            base_path: raw.base_path,
            database_files: raw.database_files,
            profiles,
            description: raw.description,
        })
    }
}

/// Reads a JSON object into a map, rejecting a key written twice instead of
/// silently keeping the last value.
fn unique_key_map<'de, D, V>(deserializer: D) -> Result<BTreeMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct UniqueKeyVisitor<V>(std::marker::PhantomData<V>);

    impl<'de, V: Deserialize<'de>> Visitor<'de> for UniqueKeyVisitor<V> {
        type Value = BTreeMap<String, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object without repeated keys")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Self::Value, A::Error> {
            let mut map = BTreeMap::new();
            while let Some((key, value)) = entries.next_entry::<String, V>()? {
                if map.contains_key(&key) {
                    return Err(de::Error::custom(ValidationError::DuplicateEntry {
                        field: "sharedDbFileSets",
                        value: key,
                    }));
                }
                map.insert(key, value);
            }
            Ok(map)
        }
    }

    deserializer.deserialize_map(UniqueKeyVisitor(std::marker::PhantomData))
}

fn default_profile_patterns() -> Result<NonEmpty<NamePattern>, ValidationError> {
    let patterns = DEFAULT_PROFILE_PATTERNS
        .into_iter()
        .map(|pattern| NamePattern::try_from(pattern.to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    NonEmpty::new(patterns)
}

impl TryFrom<RawDatabaseRules> for DatabaseRules {
    type Error = ValidationError;

    fn try_from(raw: RawDatabaseRules) -> Result<Self, Self::Error> {
        let sets = raw.shared_file_sets;
        let targets = raw
            .targets
            .into_iter()
            .map(|target| {
                let FileList(database_files) = match target.database_files {
                    RawDatabaseFiles::Files(files) => files,
                    RawDatabaseFiles::SetReference(reference) => reference
                        .strip_prefix('$')
                        .and_then(|name| sets.get(name))
                        .cloned()
                        .ok_or_else(|| ValidationError::UnknownDatabaseFileSet {
                            label: target.label.to_string(),
                            reference,
                        })?,
                };
                Ok(DatabaseTarget {
                    label: target.label,
                    base_path: target.base_path,
                    database_files,
                    profiles: target.profiles,
                    description: target.description,
                })
            })
            .collect::<Result<_, ValidationError>>()?;
        Ok(Self {
            kind: raw.kind,
            targets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(targets: &str) -> Result<DatabaseRules, serde_json::Error> {
        serde_json::from_str(&format!(
            r#"{{"type": "databases",
                 "sharedDbFileSets": {{"aaa": ["a.db"], "chromium": ["History", "Network/Cookies"],
                                       "firefox": ["places.sqlite"], "zzz": ["z.db"]}},
                 "targets": [{targets}]}}"#
        ))
    }

    fn error_of(targets: &str) -> String {
        rules(targets).unwrap_err().to_string()
    }

    fn file_names(target: &DatabaseTarget) -> Vec<String> {
        target
            .database_files()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn pattern_names(target: &DatabaseTarget) -> Vec<&str> {
        match target.profiles() {
            ProfileLayout::Single => Vec::new(),
            ProfileLayout::PerProfile(patterns) => {
                patterns.iter().map(NamePattern::as_str).collect()
            }
        }
    }

    #[test]
    fn replaces_a_set_reference_by_the_files_of_that_set() {
        let rules = rules(
            r#"{"label": "Firefox", "basePath": "${HOME}/.mozilla/firefox", "dbFiles": "$firefox", "multiProfile": true},
               {"label": "Code", "basePath": "${CONFIG}/Code", "dbFiles": ["User/globalStorage/state.vscdb"]}"#,
        )
        .unwrap();
        let [firefox, code] = rules.targets() else {
            panic!("expected two targets");
        };
        assert_eq!(file_names(firefox), ["places.sqlite"]);
        assert_eq!(file_names(code), ["User/globalStorage/state.vscdb"]);
        assert_eq!(code.profiles(), &ProfileLayout::Single);
    }

    #[test]
    fn a_multi_profile_target_without_patterns_gets_the_default_ones() {
        let rules = rules(r#"{"label": "Chrome", "basePath": "${CONFIG}/c", "dbFiles": "$chromium", "multiProfile": true}"#).unwrap();
        assert_eq!(pattern_names(&rules.targets()[0]), ["Default", "Profile *"]);
        assert_eq!(
            file_names(&rules.targets()[0]),
            ["History", "Network/Cookies"]
        );
    }

    #[test]
    fn keeps_explicit_profile_patterns() {
        let rules = rules(
            r#"{"label": "Firefox", "basePath": "${HOME}/.mozilla/firefox", "dbFiles": ["places.sqlite"],
                "multiProfile": true, "profilePattern": ["*.default*", "*.dev-edition*"]}"#,
        )
        .unwrap();
        assert_eq!(
            pattern_names(&rules.targets()[0]),
            ["*.default*", "*.dev-edition*"]
        );
    }

    #[test]
    fn profile_patterns_need_profiles_and_at_least_one_pattern() {
        let without_profiles = error_of(
            r#"{"label": "Firefox", "basePath": "${HOME}/f", "dbFiles": ["a"], "profilePattern": ["*"]}"#,
        );
        assert!(
            without_profiles.starts_with("`profilePattern` needs `multiProfile: true`"),
            "{without_profiles}"
        );
        let empty = error_of(
            r#"{"label": "Firefox", "basePath": "${HOME}/f", "dbFiles": ["a"], "multiProfile": true, "profilePattern": []}"#,
        );
        assert!(empty.starts_with("empty list"), "{empty}");
    }

    #[test]
    fn rejects_a_reference_to_a_missing_set_or_without_its_dollar_sign() {
        for reference in ["$safari", "chromium"] {
            let error = error_of(&format!(
                r#"{{"label": "A", "basePath": "${{CONFIG}}/a", "dbFiles": "{reference}"}}"#
            ));
            assert!(
                error.starts_with(&format!("database target `A`: `{reference}` does not name")),
                "{error}"
            );
        }
    }

    #[test]
    fn keeps_the_real_cause_of_a_bad_file_list() {
        for (files, expected) in [
            ("[]", "empty list"),
            (
                r#"["../secrets.db"]"#,
                "`../secrets.db` is not a relative path",
            ),
            (r#"["History", "History"]"#, "duplicate value `History`"),
            (
                "3",
                "invalid type: integer `3`, expected a list of database files",
            ),
        ] {
            let error = error_of(&format!(
                r#"{{"label": "A", "basePath": "${{CONFIG}}/a", "dbFiles": {files}}}"#
            ));
            assert!(error.starts_with(expected), "{files}: {error}");
        }
    }

    #[test]
    fn profile_patterns_are_distinct_single_segments() {
        for (patterns, expected) in [
            (
                r#"["Default", "Default"]"#,
                "duplicate value `Default` in `profilePattern`",
            ),
            (r#"["../*"]"#, "`../*` is not a single name pattern"),
        ] {
            let error = error_of(&format!(
                r#"{{"label": "A", "basePath": "${{HOME}}/a", "dbFiles": ["a"], "multiProfile": true, "profilePattern": {patterns}}}"#
            ));
            assert!(error.starts_with(expected), "{patterns}: {error}");
        }
    }

    #[test]
    fn a_shared_set_is_a_list_of_distinct_files() {
        let raw = r#"{"type": "databases", "sharedDbFileSets": {"x": ["a", "a"]}, "targets": []}"#;
        let error = serde_json::from_str::<DatabaseRules>(raw)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("duplicate value `a`"), "{error}");
    }

    #[test]
    fn rejects_a_shared_set_written_twice() {
        let raw = r#"{"type": "databases", "sharedDbFileSets": {"chromium": ["History"], "chromium": ["Cookies"]}, "targets": []}"#;
        let error = serde_json::from_str::<DatabaseRules>(raw)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("duplicate value `chromium` in `sharedDbFileSets`"),
            "{error}"
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = error_of(
            r#"{"label": "A", "basePath": "${CONFIG}/a", "dbFiles": ["a"], "command": "x"}"#,
        );
        assert!(error.starts_with("unknown field `command`"), "{error}");
    }
}
