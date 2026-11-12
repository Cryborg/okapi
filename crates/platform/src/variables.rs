use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

use okapi_rules::{TemplateError, TemplateVariables, Variable};
use thiserror::Error;

/// Where temporary files go when `TMPDIR` is not set (POSIX).
const DEFAULT_TEMP_DIRECTORY: &str = "/tmp";

/// The XDG base directories, with their environment variable and their
/// default below the home directory (XDG Base Directory Specification).
const XDG_DIRECTORIES: [(Variable, &str, &str); 3] = [
    (Variable::Config, "XDG_CONFIG_HOME", ".config"),
    (Variable::Cache, "XDG_CACHE_HOME", ".cache"),
    (Variable::LocalShare, "XDG_DATA_HOME", ".local/share"),
];

/// The values of the rules' variables cannot be read from this session.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum VariablesError {
    #[error("the home directory cannot be found: `HOME` is not set")]
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

/// Where Okapi keeps its state (the journal): `$XDG_STATE_HOME/okapi`, or
/// `~/.local/state/okapi` when it is not set or not absolute.
#[must_use]
pub fn state_directory() -> Option<PathBuf> {
    state_directory_from(|name| env::var_os(name), env::home_dir())
}

/// The application's own directory name below the XDG ones.
const APPLICATION: &str = "okapi";

fn state_directory_from(
    read: impl Fn(&str) -> Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let base = read("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|value| value.is_absolute())
        .or_else(|| {
            home.filter(|home| home.is_absolute())
                .map(|home| home.join(".local/state"))
        })?;
    Some(base.join(APPLICATION))
}

/// [`template_variables`], reading the environment through `read`.
fn variables_from(
    read: impl Fn(&str) -> Option<OsString>,
    home: Option<PathBuf>,
) -> Result<TemplateVariables, VariablesError> {
    let home = home
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or(VariablesError::NoHome)?;
    let mut variables = TemplateVariables::new();
    for (variable, name, default) in XDG_DIRECTORIES {
        // The specification ignores an empty or relative value.
        let value = read(name)
            .map(PathBuf::from)
            .filter(|value| value.is_absolute())
            .unwrap_or_else(|| home.join(default));
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
            state_directory_from(read(None), Some(PathBuf::from("/home/ana"))),
            Some(PathBuf::from("/home/ana/.local/state/okapi"))
        );
        assert_eq!(
            state_directory_from(read(Some("/data/state")), Some(PathBuf::from("/home/ana"))),
            Some(PathBuf::from("/data/state/okapi"))
        );
        assert_eq!(
            state_directory_from(read(Some("relative")), Some(PathBuf::from("/home/ana"))),
            Some(PathBuf::from("/home/ana/.local/state/okapi"))
        );
        assert_eq!(state_directory_from(read(None), None), None);
    }

    #[test]
    fn the_session_has_its_variables() {
        assert!(template_variables().is_ok());
    }
}
