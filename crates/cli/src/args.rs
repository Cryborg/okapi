//! The command line, read by hand: every message about it is translated.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::selection::Category;

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Licenses,
    Run(Run),
}

/// How much the run says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Normal,
    Verbose,
}

/// A scan, and a cleaning with `--clean`.
#[derive(Debug, PartialEq, Eq)]
pub struct Run {
    pub clean: bool,
    pub json: bool,
    pub verbosity: Verbosity,
    /// Empty means every category.
    pub categories: BTreeSet<Category>,
    pub exclusions: Vec<PathBuf>,
    /// `None` leaves the settings' age, or the engine's default.
    pub min_age: Option<Duration>,
}

/// What is wrong with the command line.
#[derive(Debug, PartialEq, Eq)]
pub enum ArgumentError {
    Unknown(String),
    MissingValue(&'static str),
    InvalidMinAge(String),
    VerboseAndQuiet,
}

const CLEAN: &str = "--clean";
const JSON: &str = "--json";
const VERBOSE: &str = "--verbose";
const QUIET: &str = "--quiet";
const EXCLUDE: &str = "--exclude";
const MIN_AGE: &str = "--min-age";
pub const SECONDS_PER_MINUTE: u64 = 60;

/// Reads the arguments that follow the program's name.
pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, ArgumentError> {
    let arguments: Vec<OsString> = arguments.into_iter().collect();
    // Help and version win over everything else, wherever they are.
    let asked = |flags: [&str; 2]| {
        arguments
            .iter()
            .any(|argument| flags.map(OsString::from).contains(argument))
    };
    if asked(["-h", "--help"]) {
        return Ok(Command::Help);
    }
    if asked(["-V", "--version"]) {
        return Ok(Command::Version);
    }
    let mut arguments = arguments.into_iter();
    let mut run = Run {
        clean: false,
        json: false,
        verbosity: Verbosity::Normal,
        categories: BTreeSet::new(),
        exclusions: Vec::new(),
        min_age: None,
    };
    let mut first = true;
    let (mut verbose, mut quiet) = (false, false);
    while let Some(argument) = arguments.next() {
        let text = argument.to_string_lossy();
        match text.as_ref() {
            "licenses" if first => {
                // Nothing may follow it: an argument would be silently ignored.
                return match arguments.next() {
                    None => Ok(Command::Licenses),
                    Some(extra) => {
                        Err(ArgumentError::Unknown(extra.to_string_lossy().into_owned()))
                    }
                };
            }
            CLEAN => run.clean = true,
            JSON => run.json = true,
            VERBOSE | "-v" => verbose = true,
            QUIET | "-q" => quiet = true,
            EXCLUDE => {
                let value = arguments
                    .next()
                    .ok_or(ArgumentError::MissingValue(EXCLUDE))?;
                run.exclusions.push(PathBuf::from(value));
            }
            MIN_AGE => {
                let value = arguments
                    .next()
                    .ok_or(ArgumentError::MissingValue(MIN_AGE))?;
                let minutes = minutes(&value.to_string_lossy())
                    .ok_or_else(|| ArgumentError::InvalidMinAge(value.to_string_lossy().into()))?;
                run.min_age = Some(minutes);
            }
            flag => {
                let category = Category::ALL
                    .into_iter()
                    .find(|category| flag.strip_prefix("--") == Some(category.name()))
                    .ok_or_else(|| ArgumentError::Unknown(flag.to_owned()))?;
                run.categories.insert(category);
            }
        }
        first = false;
    }
    run.verbosity = match (verbose, quiet) {
        (true, true) => return Err(ArgumentError::VerboseAndQuiet),
        (true, false) => Verbosity::Verbose,
        (false, true) => Verbosity::Quiet,
        (false, false) => Verbosity::Normal,
    };
    Ok(Command::Run(run))
}

/// `text` minutes, if it is a whole number that the clock can hold.
fn minutes(text: &str) -> Option<Duration> {
    duration_of_minutes(text.parse().ok()?)
}

/// `minutes` as a duration, if the clock can hold it: the one conversion for
/// the command line and the settings.
pub fn duration_of_minutes(minutes: u64) -> Option<Duration> {
    Some(Duration::from_secs(
        minutes.checked_mul(SECONDS_PER_MINUTE)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_text(arguments: &[&str]) -> Result<Command, ArgumentError> {
        parse(arguments.iter().map(OsString::from))
    }

    fn parsed_run(arguments: &[&str]) -> Run {
        match parse_text(arguments).unwrap() {
            Command::Run(run) => run,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_argument_scans_everything_without_cleaning() {
        let run = parsed_run(&[]);
        assert!(!run.clean && !run.json);
        assert_eq!(run.verbosity, Verbosity::Normal);
        assert!(run.categories.is_empty());
        assert_eq!(run.min_age, None);
    }

    #[test]
    fn every_flag_is_read() {
        let run = parsed_run(&[
            "--clean",
            "--json",
            "-v",
            "--apps",
            "--gpu",
            "--exclude",
            "/data/keep",
            "--exclude",
            "*.log",
            "--min-age",
            "0",
            "--apps",
        ]);
        assert!(run.clean && run.json);
        assert_eq!(run.verbosity, Verbosity::Verbose);
        assert_eq!(
            run.categories,
            BTreeSet::from([Category::Apps, Category::Gpu])
        );
        assert_eq!(
            run.exclusions,
            [PathBuf::from("/data/keep"), PathBuf::from("*.log")]
        );
        assert_eq!(run.min_age, Some(Duration::ZERO));
        assert_eq!(
            parsed_run(&["--min-age", "90"]).min_age,
            Some(Duration::from_mins(90))
        );
        assert_eq!(
            parsed_run(&["-q", "--system", "--gaming"]).verbosity,
            Verbosity::Quiet
        );
    }

    #[test]
    fn help_version_and_licenses() {
        assert_eq!(parse_text(&["--help"]), Ok(Command::Help));
        assert_eq!(parse_text(&["--clean", "-h"]), Ok(Command::Help));
        assert_eq!(
            parse_text(&["--cleen", "--help"]),
            Ok(Command::Help),
            "help wins over a mistake before it"
        );
        assert_eq!(parse_text(&["-V"]), Ok(Command::Version));
        assert_eq!(parse_text(&["licenses"]), Ok(Command::Licenses));
        assert_eq!(
            parse_text(&["licenses", "--clean"]),
            Err(ArgumentError::Unknown("--clean".to_owned()))
        );
        assert_eq!(
            parse_text(&["--clean", "licenses"]),
            Err(ArgumentError::Unknown("licenses".to_owned())),
            "a command comes first"
        );
    }

    #[test]
    fn mistakes_are_refused() {
        assert_eq!(
            parse_text(&["--cleen"]),
            Err(ArgumentError::Unknown("--cleen".to_owned()))
        );
        assert_eq!(
            parse_text(&["--exclude"]),
            Err(ArgumentError::MissingValue(EXCLUDE))
        );
        assert_eq!(
            parse_text(&["--min-age"]),
            Err(ArgumentError::MissingValue(MIN_AGE))
        );
        for value in ["-1", "ten", "1.5", "18446744073709551615"] {
            assert_eq!(
                parse_text(&["--min-age", value]),
                Err(ArgumentError::InvalidMinAge(value.to_owned())),
                "{value}"
            );
        }
        assert_eq!(
            parse_text(&["-v", "-q"]),
            Err(ArgumentError::VerboseAndQuiet)
        );
        assert_eq!(
            parse_text(&["--", "--clean"]),
            Err(ArgumentError::Unknown("--".to_owned()))
        );
    }
}
