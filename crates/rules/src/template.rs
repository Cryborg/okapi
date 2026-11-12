use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;
use std::path::{Component, MAIN_SEPARATOR, Path, PathBuf};

use serde::Deserialize;

use crate::error::{TemplateError, ValidationError};
use crate::name::{RULE_SEPARATOR, RelativePath};
use crate::os::Os;

/// A variable that a template path can start with, such as `${CACHE}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Variable {
    Home,
    TempDirectory,
    Config,
    Cache,
    LocalShare,
    Library,
    Caches,
    AppSupport,
    LocalAppData,
    AppData,
    WindowsDirectory,
    ProgramData,
    ProgramFiles,
    ProgramFilesX86,
    SystemDrive,
}

impl Variable {
    /// Every variable, on every operating system.
    pub const ALL: [Self; 15] = [
        Self::Home,
        Self::TempDirectory,
        Self::Config,
        Self::Cache,
        Self::LocalShare,
        Self::Library,
        Self::Caches,
        Self::AppSupport,
        Self::LocalAppData,
        Self::AppData,
        Self::WindowsDirectory,
        Self::ProgramData,
        Self::ProgramFiles,
        Self::ProgramFilesX86,
        Self::SystemDrive,
    ];

    /// The name written between `${` and `}` in rule files.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Home => "HOME",
            Self::TempDirectory => "TMPDIR",
            Self::Config => "CONFIG",
            Self::Cache => "CACHE",
            Self::LocalShare => "LOCAL_SHARE",
            Self::Library => "LIBRARY",
            Self::Caches => "CACHES",
            Self::AppSupport => "APP_SUPPORT",
            Self::LocalAppData => "LOCALAPPDATA",
            Self::AppData => "APPDATA",
            Self::WindowsDirectory => "WINDIR",
            Self::ProgramData => "PROGRAMDATA",
            Self::ProgramFiles => "PROGRAMFILES",
            Self::ProgramFilesX86 => "PROGRAMFILES_X86",
            Self::SystemDrive => "SYSTEMDRIVE",
        }
    }

    /// Whether rules for `os` may use this variable.
    #[must_use]
    pub const fn is_available_on(self, os: Os) -> bool {
        match self {
            Self::Home | Self::TempDirectory => true,
            Self::Config | Self::Cache | Self::LocalShare => matches!(os, Os::Linux),
            Self::Library | Self::Caches | Self::AppSupport => matches!(os, Os::MacOs),
            Self::LocalAppData
            | Self::AppData
            | Self::WindowsDirectory
            | Self::ProgramData
            | Self::ProgramFiles
            | Self::ProgramFilesX86
            | Self::SystemDrive => matches!(os, Os::Windows),
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|variable| variable.name() == name)
    }
}

/// Where a template path starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum PathRoot {
    /// A variable such as `${HOME}`, resolved per user and per OS.
    Variable(Variable),
    /// The Unix filesystem root, written `/`.
    FilesystemRoot,
    /// A Windows drive, written `D:/`; the letter is stored in uppercase.
    Drive(char),
}

/// A path from a rule file, written with `/` separators.
///
/// It is always anchored: it starts with a variable, the Unix root or a Windows
/// drive, never relative to the working directory. A literal root or drive is
/// always followed by at least one segment, so a rule never names `/` or `D:/`
/// itself. A variable is never a root either, except `${SYSTEMDRIVE}` (see
/// [`TemplateVariables::insert`]), which only bounded rules may use alone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct TemplatePath {
    root: PathRoot,
    relative: Option<RelativePath>,
}

impl TemplatePath {
    #[cfg(test)]
    pub(crate) const fn root(&self) -> PathRoot {
        self.root
    }

    /// Whether this path is a variable alone, such as `${HOME}`.
    pub(crate) const fn is_bare_variable(&self) -> bool {
        matches!((self.root, &self.relative), (PathRoot::Variable(_), None))
    }

    /// Rejects a bare variable as a directory to empty entirely: `${HOME}` alone
    /// would offer the whole home directory. `${TMPDIR}` is the exception, its
    /// whole contents being temporary by definition (and its value never holds
    /// another variable's, see [`TemplateVariables::insert`]).
    ///
    /// This check only reads the rule: a literal path such as `/tmp` can still
    /// resolve onto a user directory on some systems. The engine checks every
    /// resolved, canonical root against the variable values (invariant 6).
    pub(crate) fn ensure_bounded_deletion(&self) -> Result<(), ValidationError> {
        match (self.root, &self.relative) {
            (PathRoot::Variable(Variable::TempDirectory), None)
            | (PathRoot::Variable(_) | PathRoot::FilesystemRoot | PathRoot::Drive(_), Some(_)) => {
                Ok(())
            }
            (PathRoot::Variable(_), None) => Err(ValidationError::WholeVariableDirectory {
                path: self.to_string(),
            }),
            // A literal root always has a relative part (see `try_from`).
            (PathRoot::FilesystemRoot | PathRoot::Drive(_), None) => {
                Err(ValidationError::InvalidTemplatePath {
                    path: self.to_string(),
                })
            }
        }
    }

    /// Whether this path can exist on `os`: its variable is defined there, and a
    /// literal root matches the OS (`/` on Unix, a drive on Windows).
    #[must_use]
    pub const fn is_available_on(&self, os: Os) -> bool {
        match self.root {
            PathRoot::Variable(variable) => variable.is_available_on(os),
            PathRoot::FilesystemRoot => !matches!(os, Os::Windows),
            PathRoot::Drive(_) => matches!(os, Os::Windows),
        }
    }

    /// The real path, with the variable replaced by its value in `variables`.
    ///
    /// # Errors
    ///
    /// - [`TemplateError::MissingVariable`] when `variables` has no value for the
    ///   variable this path starts with.
    /// - [`TemplateError::NotAbsolute`] when the result is not absolute on the
    ///   running system, as a Windows drive is on Unix or `/` on Windows.
    pub fn resolve(&self, variables: &TemplateVariables) -> Result<PathBuf, TemplateError> {
        let mut path = match self.root {
            PathRoot::Variable(variable) => variables.get(variable)?.to_path_buf(),
            PathRoot::FilesystemRoot => PathBuf::from(MAIN_SEPARATOR.to_string()),
            PathRoot::Drive(letter) => PathBuf::from(format!("{letter}:{MAIN_SEPARATOR}")),
        };
        for segment in self.relative.iter().flat_map(RelativePath::segments) {
            path.push(segment.as_str());
        }
        if path.is_absolute() {
            Ok(path)
        } else {
            Err(TemplateError::NotAbsolute { path })
        }
    }
}

impl TryFrom<String> for TemplatePath {
    type Error = ValidationError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        let invalid = || ValidationError::InvalidTemplatePath { path: path.clone() };

        let (root, rest) = if let Some(after_opening) = path.strip_prefix("${") {
            let (name, rest) = after_opening.split_once('}').ok_or_else(invalid)?;
            let variable =
                Variable::from_name(name).ok_or_else(|| ValidationError::UnknownVariable {
                    name: name.to_owned(),
                })?;
            (PathRoot::Variable(variable), rest)
        } else if path.starts_with(RULE_SEPARATOR) {
            (PathRoot::FilesystemRoot, path.as_str())
        } else {
            let mut characters = path.chars();
            match (characters.next(), characters.next()) {
                (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => (
                    PathRoot::Drive(letter.to_ascii_uppercase()),
                    characters.as_str(),
                ),
                _ => return Err(invalid()),
            }
        };

        let relative = match rest.strip_prefix(RULE_SEPARATOR) {
            Some(relative) => Some(RelativePath::try_from(relative.to_owned()).map_err(|_| {
                ValidationError::InvalidRelativePart {
                    path: path.clone(),
                    relative: relative.to_owned(),
                }
            })?),
            None if rest.is_empty() && matches!(root, PathRoot::Variable(_)) => None,
            None => return Err(invalid()),
        };
        Ok(Self { root, relative })
    }
}

impl fmt::Display for TemplatePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.root {
            PathRoot::Variable(variable) => write!(formatter, "${{{}}}", variable.name())?,
            PathRoot::FilesystemRoot => {}
            PathRoot::Drive(letter) => write!(formatter, "{letter}:")?,
        }
        if let Some(relative) = &self.relative {
            write!(formatter, "{RULE_SEPARATOR}{relative}")?;
        }
        Ok(())
    }
}

/// A [`TemplatePath`] to a directory that is emptied as a whole (a trash, a
/// browser cache root): never a variable alone, except `${TMPDIR}`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct BoundedPath(TemplatePath);

impl Deref for BoundedPath {
    type Target = TemplatePath;

    fn deref(&self) -> &TemplatePath {
        &self.0
    }
}

impl TryFrom<String> for BoundedPath {
    type Error = ValidationError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        let path = TemplatePath::try_from(path)?;
        path.ensure_bounded_deletion()?;
        Ok(Self(path))
    }
}

/// A [`TemplatePath`] to a single file: never a variable alone, `${TMPDIR}`
/// included, since a variable names a directory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct FilePath(TemplatePath);

impl Deref for FilePath {
    type Target = TemplatePath;

    fn deref(&self) -> &TemplatePath {
        &self.0
    }
}

impl TryFrom<String> for FilePath {
    type Error = ValidationError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        let path = TemplatePath::try_from(path)?;
        if path.is_bare_variable() {
            return Err(ValidationError::WholeVariableDirectory {
                path: path.to_string(),
            });
        }
        Ok(Self(path))
    }
}

/// The value of each variable for the current user, supplied by the platform.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateVariables(BTreeMap<Variable, PathBuf>);

impl TemplateVariables {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the value of `variable`, replacing any previous one. On Windows,
    /// `${SYSTEMDRIVE}` is given with its root, as `C:\`.
    ///
    /// # Errors
    ///
    /// - [`TemplateError::RelativeValue`] when `value` is not absolute: every
    ///   path would depend on the working directory.
    /// - [`TemplateError::UnsafeValue`] when `value` is a bare root (`/`, `C:\`
    ///   for any variable but `${SYSTEMDRIVE}`) or holds `..`: a rule could then
    ///   reach the whole filesystem.
    /// - [`TemplateError::TempDirectoryContainsVariable`] when `${TMPDIR}` would
    ///   be another variable's value or one of its ancestors, whichever is set
    ///   first: the temporary directory is the one directory rules may empty
    ///   whole. The comparison is lexical; the engine repeats it on canonical
    ///   paths (invariant 6).
    pub fn insert(&mut self, variable: Variable, value: PathBuf) -> Result<(), TemplateError> {
        if !value.is_absolute() {
            return Err(TemplateError::RelativeValue { variable, value });
        }
        let has_only_plain_components = value.components().all(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        });
        let is_bare_root = !value
            .components()
            .any(|component| matches!(component, Component::Normal(_)));
        if !has_only_plain_components || (is_bare_root && variable != Variable::SystemDrive) {
            return Err(TemplateError::UnsafeValue { variable, value });
        }
        self.ensure_temp_directory_holds_no_variable(variable, &value)?;
        self.0.insert(variable, value);
        Ok(())
    }

    fn ensure_temp_directory_holds_no_variable(
        &self,
        variable: Variable,
        value: &Path,
    ) -> Result<(), TemplateError> {
        let contained = if variable == Variable::TempDirectory {
            self.0
                .iter()
                .find(|(other, other_value)| {
                    **other != Variable::TempDirectory && other_value.starts_with(value)
                })
                .map(|(other, other_value)| (value, *other, other_value.as_path()))
        } else {
            self.value_of(Variable::TempDirectory)
                .filter(|temp| value.starts_with(temp))
                .map(|temp| (temp, variable, value))
        };
        match contained {
            Some((temp, variable, value)) => Err(TemplateError::TempDirectoryContainsVariable {
                temp: temp.to_path_buf(),
                variable,
                value: value.to_path_buf(),
            }),
            None => Ok(()),
        }
    }

    fn value_of(&self, variable: Variable) -> Option<&Path> {
        self.0.get(&variable).map(PathBuf::as_path)
    }

    fn get(&self, variable: Variable) -> Result<&Path, TemplateError> {
        self.value_of(variable)
            .ok_or(TemplateError::MissingVariable { variable })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(text: &str) -> Result<TemplatePath, ValidationError> {
        TemplatePath::try_from(text.to_owned())
    }

    /// An absolute path on the running system, below its filesystem root.
    fn absolute(relative: &str) -> PathBuf {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        PathBuf::from(root).join(relative)
    }

    fn variables(values: &[(Variable, &str)]) -> TemplateVariables {
        let mut variables = TemplateVariables::new();
        for (variable, relative) in values {
            variables.insert(*variable, absolute(relative)).unwrap();
        }
        variables
    }

    #[test]
    fn variable_names_round_trip() {
        for variable in Variable::ALL {
            assert_eq!(Variable::from_name(variable.name()), Some(variable));
        }
    }

    #[test]
    fn each_variable_exists_on_its_own_systems() {
        use Variable::{
            AppData, AppSupport, Cache, Caches, Config, Home, Library, LocalAppData, LocalShare,
            ProgramData, ProgramFiles, ProgramFilesX86, SystemDrive, TempDirectory,
            WindowsDirectory,
        };
        // Linux, macOS, Windows.
        let expected = [
            (Home, [true, true, true]),
            (TempDirectory, [true, true, true]),
            (Config, [true, false, false]),
            (Cache, [true, false, false]),
            (LocalShare, [true, false, false]),
            (Library, [false, true, false]),
            (Caches, [false, true, false]),
            (AppSupport, [false, true, false]),
            (LocalAppData, [false, false, true]),
            (AppData, [false, false, true]),
            (WindowsDirectory, [false, false, true]),
            (ProgramData, [false, false, true]),
            (ProgramFiles, [false, false, true]),
            (ProgramFilesX86, [false, false, true]),
            (SystemDrive, [false, false, true]),
        ];
        assert_eq!(expected.len(), Variable::ALL.len());
        for (variable, availability) in expected {
            for (os, available) in Os::ALL.into_iter().zip(availability) {
                assert_eq!(
                    variable.is_available_on(os),
                    available,
                    "{variable:?} on {os}"
                );
            }
        }
    }

    #[test]
    fn parses_each_root_and_prints_it_back() {
        for text in [
            "${CACHE}/google-chrome",
            "${TMPDIR}",
            "/var/cache/apt/archives",
            "D:/SteamLibrary",
        ] {
            assert_eq!(template(text).unwrap().to_string(), text);
        }
        assert_eq!(template("d:/Games").unwrap().root(), PathRoot::Drive('D'));
    }

    #[test]
    fn rejects_unanchored_or_malformed_paths() {
        for text in [
            "",
            "cache/foo",
            "${CACHE",
            "${CACHE}foo",
            "D:",
            "D:SteamLibrary",
            r"D:\x",
            "1:/x",
        ] {
            assert_eq!(
                template(text),
                Err(ValidationError::InvalidTemplatePath {
                    path: text.to_owned()
                }),
                "{text}"
            );
        }
    }

    #[test]
    fn names_the_faulty_relative_part() {
        for (text, relative) in [
            ("${CACHE}/", ""),
            ("${CACHE}/../etc", "../etc"),
            ("/", ""),
            ("//x", "/x"),
            ("/var//log", "var//log"),
            ("D:/", ""),
            ("${HOME}/C:/Windows", "C:/Windows"),
        ] {
            assert_eq!(
                template(text),
                Err(ValidationError::InvalidRelativePart {
                    path: text.to_owned(),
                    relative: relative.to_owned()
                }),
                "{text}"
            );
        }
    }

    #[test]
    fn rejects_an_unknown_variable() {
        assert_eq!(
            template("${NOPE}/x"),
            Err(ValidationError::UnknownVariable {
                name: "NOPE".to_owned()
            })
        );
    }

    #[test]
    fn availability_follows_the_variable_and_the_literal_root() {
        let linux_only = template("${CONFIG}/x").unwrap();
        assert!(linux_only.is_available_on(Os::Linux));
        assert!(!linux_only.is_available_on(Os::MacOs));
        assert!(!linux_only.is_available_on(Os::Windows));

        let unix_root = template("/var/tmp").unwrap();
        assert!(unix_root.is_available_on(Os::Linux));
        assert!(unix_root.is_available_on(Os::MacOs));
        assert!(!unix_root.is_available_on(Os::Windows));

        let drive = template("E:/SteamLibrary").unwrap();
        assert!(drive.is_available_on(Os::Windows));
        assert!(!drive.is_available_on(Os::Linux));
    }

    #[test]
    fn only_the_temporary_directory_may_be_emptied_whole() {
        let mut refused = 0;
        for variable in Variable::ALL {
            let bare = template(&format!("${{{}}}", variable.name())).unwrap();
            if variable == Variable::TempDirectory {
                assert_eq!(bare.ensure_bounded_deletion(), Ok(()));
            } else {
                assert_eq!(
                    bare.ensure_bounded_deletion(),
                    Err(ValidationError::WholeVariableDirectory {
                        path: bare.to_string()
                    })
                );
                refused += 1;
            }
            let below = template(&format!("${{{}}}/cache", variable.name())).unwrap();
            assert_eq!(below.ensure_bounded_deletion(), Ok(()));
        }
        assert_eq!(refused, Variable::ALL.len() - 1);
    }

    #[test]
    fn resolves_the_right_variable_and_its_segments() {
        let variables = variables(&[
            (Variable::Cache, "c"),
            (Variable::Config, "k"),
            (Variable::Home, "h"),
        ]);
        let path = template("${CONFIG}/BraveSoftware/Brave-Browser").unwrap();
        assert_eq!(
            path.resolve(&variables).unwrap(),
            absolute("k/BraveSoftware/Brave-Browser")
        );
        assert_eq!(
            template("${HOME}").unwrap().resolve(&variables).unwrap(),
            absolute("h")
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_the_filesystem_root_but_not_a_drive_on_unix() {
        let path = template("/var/log/journal").unwrap();
        assert_eq!(
            path.resolve(&TemplateVariables::new()).unwrap(),
            PathBuf::from("/var/log/journal")
        );
        assert_eq!(
            template("D:/Games")
                .unwrap()
                .resolve(&TemplateVariables::new()),
            Err(TemplateError::NotAbsolute {
                path: PathBuf::from(r"D:/Games")
            })
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolves_a_drive_but_not_the_unix_root_on_windows() {
        let path = template("d:/Games/x").unwrap();
        assert_eq!(
            path.resolve(&TemplateVariables::new()).unwrap(),
            PathBuf::from(r"D:\Games\x")
        );
        assert!(matches!(
            template("/var/tmp")
                .unwrap()
                .resolve(&TemplateVariables::new()),
            Err(TemplateError::NotAbsolute { .. })
        ));
    }

    #[test]
    fn resolving_without_the_variable_fails() {
        let path = template("${HOME}/.steam").unwrap();
        assert_eq!(
            path.resolve(&variables(&[(Variable::Cache, "c")])),
            Err(TemplateError::MissingVariable {
                variable: Variable::Home
            })
        );
    }

    #[test]
    fn a_variable_value_must_be_absolute() {
        let mut variables = variables(&[(Variable::Home, "h")]);
        for value in ["someone", ""] {
            assert_eq!(
                variables.insert(Variable::Home, PathBuf::from(value)),
                Err(TemplateError::RelativeValue {
                    variable: Variable::Home,
                    value: PathBuf::from(value)
                })
            );
        }
        assert_eq!(
            template("${HOME}").unwrap().resolve(&variables).unwrap(),
            absolute("h"),
            "a refused value leaves the previous one"
        );
    }

    #[test]
    fn a_variable_value_is_never_a_root_or_an_escape() {
        let mut variables = TemplateVariables::new();
        for variable in Variable::ALL {
            let mut values = vec![absolute("home/../etc/x"), absolute("home/x/../..")];
            if variable != Variable::SystemDrive {
                values.push(absolute(""));
            }
            for value in values {
                assert_eq!(
                    variables.insert(variable, value.clone()),
                    Err(TemplateError::UnsafeValue { variable, value }),
                    "{variable:?}"
                );
            }
        }
    }

    #[test]
    fn a_trailing_dot_is_dropped_by_the_path_itself() {
        // `Path::components` never yields `.` inside an absolute path: `home/.`
        // is `home`, so it is accepted, and `/.` is refused as a bare root.
        let mut variables = TemplateVariables::new();
        assert_eq!(variables.insert(Variable::Home, absolute("home/.")), Ok(()));
        assert!(matches!(
            variables.insert(Variable::Cache, absolute(".")),
            Err(TemplateError::UnsafeValue { .. })
        ));
    }

    #[test]
    fn the_temporary_directory_never_contains_another_variable() {
        for (first, second) in [
            (
                (Variable::Home, "home/someone"),
                (Variable::TempDirectory, "home/someone"),
            ),
            (
                (Variable::Home, "home/someone"),
                (Variable::TempDirectory, "home"),
            ),
            (
                (Variable::TempDirectory, "home"),
                (Variable::Home, "home/someone"),
            ),
            (
                (Variable::Cache, "tmp/someone/cache"),
                (Variable::TempDirectory, "tmp"),
            ),
            (
                (Variable::TempDirectory, "tmp"),
                (Variable::Config, "tmp/someone/config"),
            ),
        ] {
            let mut variables = variables(&[first, (Variable::TempDirectory, "spare")]);
            if first.0 == Variable::TempDirectory {
                variables = self::variables(&[first]);
            }
            let (variable, relative) = second;
            let before = variables.clone();
            assert!(
                matches!(
                    variables.insert(variable, absolute(relative)),
                    Err(TemplateError::TempDirectoryContainsVariable { .. })
                ),
                "{first:?} then {second:?}"
            );
            assert_eq!(variables, before, "a refused value changes nothing");
        }
        let mut variables = variables(&[(Variable::Home, "home/someone")]);
        assert_eq!(
            variables.insert(Variable::TempDirectory, absolute("tmp")),
            Ok(())
        );
        assert_eq!(
            variables.insert(Variable::TempDirectory, absolute("home/someone/.cache/tmp")),
            Ok(()),
            "a temporary directory inside home does not contain it"
        );
    }

    #[test]
    fn an_unsafe_value_leaves_the_previous_one() {
        let mut variables = variables(&[(Variable::Home, "h")]);
        assert!(variables.insert(Variable::Home, absolute("")).is_err());
        assert_eq!(
            template("${HOME}").unwrap().resolve(&variables).unwrap(),
            absolute("h")
        );
    }

    #[test]
    fn a_new_value_replaces_the_previous_one() {
        let mut variables = variables(&[(Variable::Home, "old")]);
        variables.insert(Variable::Home, absolute("new")).unwrap();
        assert_eq!(
            template("${HOME}").unwrap().resolve(&variables).unwrap(),
            absolute("new")
        );
    }

    #[test]
    fn the_system_drive_may_be_a_bare_root() {
        let mut variables = TemplateVariables::new();
        assert_eq!(
            variables.insert(Variable::SystemDrive, absolute("")),
            Ok(())
        );
        assert_eq!(
            template("${SYSTEMDRIVE}/Windows")
                .unwrap()
                .resolve(&variables)
                .unwrap(),
            absolute("Windows")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_values_are_checked_like_any_other() {
        let mut variables = TemplateVariables::new();
        for value in [r"C:\", r"\\?\C:\", r"\\server\share\"] {
            assert!(
                matches!(
                    variables.insert(Variable::Home, PathBuf::from(value)),
                    Err(TemplateError::UnsafeValue { .. })
                ),
                "{value}"
            );
        }
        for value in [r"C:", r"\Users"] {
            assert!(
                matches!(
                    variables.insert(Variable::Home, PathBuf::from(value)),
                    Err(TemplateError::RelativeValue { .. })
                ),
                "{value}"
            );
        }
        assert_eq!(
            variables.insert(Variable::Home, PathBuf::from(r"\\?\C:\Users\x")),
            Ok(())
        );
        assert_eq!(
            variables.insert(Variable::SystemDrive, PathBuf::from(r"C:\")),
            Ok(())
        );
    }
}
