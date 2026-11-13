use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use okapi_rules::{TemplateError, TemplateVariables, Variable};
use thiserror::Error;

/// Where temporary files go when `TMPDIR` is not set (POSIX).
const DEFAULT_TEMP_DIRECTORY: &str = "/tmp";

/// The XDG base directories the rules' variables stand for (XDG Base
/// Directory Specification).
const XDG_DIRECTORIES: [(Variable, XdgBase); 3] = [
    (Variable::Config, CONFIG),
    (Variable::Cache, CACHE),
    (Variable::LocalShare, DATA),
];

/// The values of the rules' variables cannot be read from this session.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum VariablesError {
    #[error("the home directory cannot be found: `HOME` is not set or not absolute")]
    NoHome,

    #[error(transparent)]
    Invalid(#[from] TemplateError),
}

/// The values of the rules' variables for the current user, from the
/// environment and the XDG base directories.
///
/// # Errors
///
/// [`VariablesError`] when the home directory is unknown or a value cannot be
/// used (a relative `TMPDIR`, or one that would contain another variable).
pub fn template_variables() -> Result<TemplateVariables, VariablesError> {
    variables_from(|name| env::var_os(name), env::home_dir())
}

/// Where Okapi keeps its state (the journal, the history):
/// `$XDG_STATE_HOME/okapi`, or `~/.local/state/okapi` when it is not set or
/// not absolute.
#[must_use]
pub fn state_directory() -> Option<PathBuf> {
    xdg_directory_from(STATE, |name| env::var_os(name), env::home_dir())
}

/// Where Okapi keeps its settings: `$XDG_CONFIG_HOME/okapi`, or
/// `~/.config/okapi`.
#[must_use]
pub fn config_directory() -> Option<PathBuf> {
    xdg_directory_from(CONFIG, |name| env::var_os(name), env::home_dir())
}

/// The `applications` folders `.desktop` files are looked for in, most
/// personal first: that of `$XDG_DATA_HOME`, then of each `$XDG_DATA_DIRS`.
pub(crate) fn data_directories() -> Vec<PathBuf> {
    data_directories_from(|name| env::var_os(name), env::home_dir())
}

/// The default of `$XDG_DATA_DIRS`.
const DATA_DIRECTORIES: &str = "/usr/local/share:/usr/share";

/// [`data_directories`], reading the environment through `read`.
fn data_directories_from(
    read: impl Fn(&str) -> Option<OsString>,
    home: Option<PathBuf>,
) -> Vec<PathBuf> {
    let personal = DATA.resolve(&read, usable_home(home).as_deref());
    let shared = read("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(DATA_DIRECTORIES));
    personal
        .into_iter()
        .chain(env::split_paths(&shared).filter(|path| path.is_absolute()))
        .map(|directory| directory.join("applications"))
        .collect()
}

/// An XDG base directory: its environment variable, and its default below
/// the home directory.
#[derive(Clone, Copy)]
struct XdgBase {
    variable: &'static str,
    default: &'static str,
}

impl XdgBase {
    /// This directory: the variable when it holds an absolute path (the
    /// specification ignores an empty or relative value), else the default
    /// below `home`; `None` when neither is known.
    fn resolve(
        self,
        read: impl Fn(&str) -> Option<OsString>,
        home: Option<&Path>,
    ) -> Option<PathBuf> {
        read(self.variable)
            .map(PathBuf::from)
            .filter(|value| {
                let absolute = value.is_absolute();
                if !absolute && !value.as_os_str().is_empty() {
                    tracing::warn!(
                        variable = self.variable,
                        value = %value.display(),
                        "not an absolute path, ignored"
                    );
                }
                absolute
            })
            .or_else(|| home.map(|home| home.join(self.default)))
    }
}

const STATE: XdgBase = XdgBase {
    variable: "XDG_STATE_HOME",
    default: ".local/state",
};

const CONFIG: XdgBase = XdgBase {
    variable: "XDG_CONFIG_HOME",
    default: ".config",
};

const CACHE: XdgBase = XdgBase {
    variable: "XDG_CACHE_HOME",
    default: ".cache",
};

const DATA: XdgBase = XdgBase {
    variable: "XDG_DATA_HOME",
    default: ".local/share",
};

/// The application's own directory name below the XDG ones.
const APPLICATION: &str = "okapi";

fn xdg_directory_from(
    base: XdgBase,
    read: impl Fn(&str) -> Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let directory = base.resolve(read, usable_home(home).as_deref())?;
    Some(directory.join(APPLICATION))
}

/// The home directory, when it is an absolute path: an empty or relative one
/// says nothing of where the user's files are.
fn usable_home(home: Option<PathBuf>) -> Option<PathBuf> {
    home.filter(|home| home.is_absolute())
}

/// [`template_variables`], reading the environment through `read`.
fn variables_from(
    read: impl Fn(&str) -> Option<OsString>,
    home: Option<PathBuf>,
) -> Result<TemplateVariables, VariablesError> {
    let home = usable_home(home).ok_or(VariablesError::NoHome)?;
    let mut variables = TemplateVariables::new();
    for (variable, base) in XDG_DIRECTORIES {
        // Never `None`: the home directory is known.
        let value = base
            .resolve(&read, Some(&home))
            .ok_or(VariablesError::NoHome)?;
        variables.insert(variable, value)?;
    }
    let temp = read("TMPDIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_TEMP_DIRECTORY), PathBuf::from);
    variables.insert(Variable::TempDirectory, temp)?;
    variables.insert(Variable::Home, home)?;
    Ok(variables)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;

    fn variables(
        environment: &[(&str, &str)],
        home: Option<&str>,
    ) -> Result<TemplateVariables, VariablesError> {
        let environment: HashMap<&str, &str> = environment.iter().copied().collect();
        variables_from(
            |name| environment.get(name).map(OsString::from),
            home.map(PathBuf::from),
        )
    }

    fn values(variables: &TemplateVariables) -> Vec<(Variable, &Path)> {
        let mut values: Vec<_> = variables.iter().collect();
        values.sort_by_key(|(variable, _)| variable.name());
        values
    }

    #[test]
    fn defaults_follow_the_xdg_specification() {
        let variables = variables(&[], Some("/home/ana")).unwrap();
        assert_eq!(
            values(&variables),
            [
                (Variable::Cache, Path::new("/home/ana/.cache")),
                (Variable::Config, Path::new("/home/ana/.config")),
                (Variable::Home, Path::new("/home/ana")),
                (Variable::LocalShare, Path::new("/home/ana/.local/share")),
                (Variable::TempDirectory, Path::new("/tmp")),
            ]
        );
    }

    #[test]
    fn the_environment_wins_over_the_defaults() {
        let variables = variables(
            &[
                ("XDG_CONFIG_HOME", "/data/config"),
                ("XDG_CACHE_HOME", "/data/cache"),
                ("XDG_DATA_HOME", "/data/share"),
                ("TMPDIR", "/run/user/1000/tmp"),
            ],
            Some("/home/ana"),
        )
        .unwrap();
        assert_eq!(
            values(&variables),
            [
                (Variable::Cache, Path::new("/data/cache")),
                (Variable::Config, Path::new("/data/config")),
                (Variable::Home, Path::new("/home/ana")),
                (Variable::LocalShare, Path::new("/data/share")),
                (Variable::TempDirectory, Path::new("/run/user/1000/tmp")),
            ]
        );
    }

    #[test]
    fn empty_or_relative_xdg_values_are_ignored() {
        let variables = variables(
            &[
                ("XDG_CACHE_HOME", ""),
                ("XDG_CONFIG_HOME", "relative/config"),
            ],
            Some("/home/ana"),
        )
        .unwrap();
        let values = values(&variables);
        assert_eq!(values[0], (Variable::Cache, Path::new("/home/ana/.cache")));
        assert_eq!(
            values[1],
            (Variable::Config, Path::new("/home/ana/.config"))
        );
    }

    #[test]
    fn an_empty_temp_directory_falls_back_and_a_relative_one_is_refused() {
        let variables = variables(&[("TMPDIR", "")], Some("/home/ana")).unwrap();
        assert_eq!(values(&variables)[4].1, Path::new("/tmp"));
        assert!(matches!(
            self::variables(&[("TMPDIR", "tmp")], Some("/home/ana")),
            Err(VariablesError::Invalid(TemplateError::RelativeValue {
                variable: Variable::TempDirectory,
                ..
            }))
        ));
    }

    #[test]
    fn a_temp_directory_holding_the_home_is_refused() {
        assert!(matches!(
            variables(&[("TMPDIR", "/home")], Some("/home/ana")),
            Err(VariablesError::Invalid(
                TemplateError::TempDirectoryContainsVariable { .. }
            ))
        ));
    }

    #[test]
    fn without_a_home_nothing_is_guessed() {
        assert_eq!(variables(&[], None).unwrap_err(), VariablesError::NoHome);
        assert_eq!(
            variables(&[], Some("")).unwrap_err(),
            VariablesError::NoHome
        );
        assert_eq!(
            variables(&[], Some("relative/home")).unwrap_err(),
            VariablesError::NoHome,
            "the same rule as the settings and state directories"
        );
    }

    #[test]
    fn the_state_directory_follows_the_xdg_specification() {
        let read = |value: Option<&'static str>| {
            move |name: &str| {
                (name == "XDG_STATE_HOME")
                    .then_some(value)
                    .flatten()
                    .map(OsString::from)
            }
        };
        assert_eq!(
            xdg_directory_from(STATE, read(None), Some(PathBuf::from("/home/ana"))),
            Some(PathBuf::from("/home/ana/.local/state/okapi"))
        );
        assert_eq!(
            xdg_directory_from(
                STATE,
                read(Some("/data/state")),
                Some(PathBuf::from("/home/ana"))
            ),
            Some(PathBuf::from("/data/state/okapi"))
        );
        assert_eq!(
            xdg_directory_from(
                STATE,
                read(Some("relative")),
                Some(PathBuf::from("/home/ana"))
            ),
            Some(PathBuf::from("/home/ana/.local/state/okapi"))
        );
        assert_eq!(xdg_directory_from(STATE, read(None), None), None);
        let config =
            |name: &str| (name == "XDG_CONFIG_HOME").then(|| OsString::from("/data/config"));
        assert_eq!(
            xdg_directory_from(CONFIG, config, Some(PathBuf::from("/home/ana"))),
            Some(PathBuf::from("/data/config/okapi"))
        );
        assert_eq!(
            xdg_directory_from(CONFIG, |_| None, Some(PathBuf::from("/home/ana"))),
            Some(PathBuf::from("/home/ana/.config/okapi"))
        );
        for home in ["relative/home", ""] {
            for base in [STATE, CONFIG] {
                assert_eq!(
                    xdg_directory_from(base, |_| None, Some(PathBuf::from(home))),
                    None,
                    "{home:?} says nothing of where the user's files are"
                );
            }
        }
        assert_eq!(
            xdg_directory_from(
                STATE,
                read(Some("/data/state")),
                Some(PathBuf::from("relative/home"))
            ),
            Some(PathBuf::from("/data/state/okapi")),
            "an absolute variable is enough"
        );
    }

    #[test]
    fn the_session_has_its_variables() {
        assert!(template_variables().is_ok());
    }

    #[test]
    fn applications_are_looked_for_in_the_personal_data_then_each_shared_one() {
        let read = |values: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                values
                    .iter()
                    .find(|(variable, _)| *variable == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            data_directories_from(read(&[]), Some(PathBuf::from("/home/ana"))),
            [
                PathBuf::from("/home/ana/.local/share/applications"),
                PathBuf::from("/usr/local/share/applications"),
                PathBuf::from("/usr/share/applications"),
            ]
        );
        assert_eq!(
            data_directories_from(
                read(&[
                    ("XDG_DATA_HOME", "/data"),
                    (
                        "XDG_DATA_DIRS",
                        "/usr/share/ubuntu:relative:/var/lib/snapd/desktop"
                    ),
                ]),
                None,
            ),
            [
                PathBuf::from("/data/applications"),
                PathBuf::from("/usr/share/ubuntu/applications"),
                PathBuf::from("/var/lib/snapd/desktop/applications"),
            ]
        );
    }
}
