//! Okapi command line interface: scans what the cleaning rules can remove,
//! and with `--clean` removes the preselected groups, in the same process
//! (identifiers never outlive the report that gave them, invariant 1).

mod args;
mod exit;
mod output;
mod selection;
mod text;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Instant, SystemTime};

use okapi_engine::{
    CleanOutcome, CleanReceipt, DEFAULT_MIN_AGE, Exclusions, ItemId, ScanIssue, ScanOptions, Stop,
    clean, scan,
};
use okapi_rules::{Os, RuleSet, THIRD_PARTY_NOTICES, TemplateVariables};
use okapi_store::{History, Settings, StoreError, journal};

use crate::args::{Command, Run, Verbosity, duration_of_minutes};
use crate::output::{Output, Shown, error_line, history_entry, summaries};
use crate::selection::Decision;
use crate::text::{Language, Message, language, text};

/// Okapi's own license, shown by `okapi licenses`.
const LICENSE: &str = include_str!("../../../LICENSE");

fn main() -> ExitCode {
    let language = language(|name| env::var_os(name));
    let say = |message: &Message<'_>| text(language, message);
    let mut output = Output::new();
    let run = match args::parse(env::args_os().skip(1)) {
        Ok(Command::Help) => {
            let settings =
                okapi_platform::config_directory().map(|directory| Settings::path(&directory));
            let history =
                okapi_platform::state_directory().map(|directory| History::path(&directory));
            let help = say(&Message::Help {
                settings: settings.as_deref(),
                history: history.as_deref(),
            });
            return informational(&mut output, language, &[&help]);
        }
        Ok(Command::Version) => {
            return informational(&mut output, language, &[&say(&Message::Version)]);
        }
        Ok(Command::Licenses) => {
            let header = say(&Message::Licenses);
            return informational(
                &mut output,
                language,
                &[&header, LICENSE, THIRD_PARTY_NOTICES],
            );
        }
        Ok(Command::Run(run)) => Ok(run),
        Err(error) => Err(error),
    };
    // Never as root: its journal would belong to root in the user's directory,
    // so this refusal is deliberately said on standard error only.
    if okapi_platform::running_as_root() {
        error_line(&say(&Message::RunningAsRoot));
        return ExitCode::from(exit::FAILURE);
    }
    let verbose = matches!(&run, Ok(run) if run.verbosity == Verbosity::Verbose);
    let on_write_failure: journal::WriteFailure = Box::new(move |error| {
        error_line(&text(language, &Message::JournalWriteFailed(error)));
    });
    let (journal, rotation) = match journal::start(journal::CLI_FILE, verbose, on_write_failure) {
        Ok((path, rotation)) => (Some(path), Some(rotation)),
        Err(journal::Unavailable::NoStateDirectory) => {
            error_line(&say(&Message::StateDirectoryUnknown));
            (None, None)
        }
        Err(journal::Unavailable::Unopenable { path, error }) => {
            error_line(&say(&Message::JournalUnavailable(&path, &error)));
            (None, None)
        }
    };
    let _run = tracing::info_span!("run", pid = std::process::id()).entered();
    if let Some(rotation) = &rotation {
        rotation.trace();
        if let journal::Rotation::Failed(error) = rotation {
            error_line(&say(&Message::JournalRotationFailed(error)));
        }
    }
    journal::trace_panics();
    let started = Instant::now();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        arguments = ?env::args_os().skip(1).collect::<Vec<_>>(),
        ?language,
        journal = ?journal,
        "okapi started"
    );
    let code = match run {
        Ok(run) => run_scan(&run, language, journal.as_deref(), &mut output),
        Err(error) => {
            tracing::warn!(?error, "invalid arguments");
            error_line(&say(&Message::Argument(&error)));
            exit::INVALID_ARGUMENTS
        }
    };
    let code = match output.failure() {
        Some(error) => {
            error_line(&say(&Message::OutputFailed(error)));
            // An invalid command line stays the first thing to fix.
            let failed = if code == exit::INVALID_ARGUMENTS {
                code
            } else {
                exit::FAILURE
            };
            tracing::error!(computed = code, code = failed, "standard output failed");
            failed
        }
        None => code,
    };
    tracing::info!(code, elapsed = ?started.elapsed(), "okapi finished");
    ExitCode::from(code)
}

/// Writes `lines` for a command that scans nothing and keeps no journal.
fn informational(output: &mut Output, language: Language, lines: &[&str]) -> ExitCode {
    for line in lines {
        output.line(line);
    }
    match output.failure() {
        Some(error) => {
            error_line(&text(language, &Message::OutputFailed(error)));
            ExitCode::from(exit::FAILURE)
        }
        None => ExitCode::from(exit::SUCCESS),
    }
}

/// What a scan needs, read from the session, the settings and the command
/// line.
struct Prepared {
    variables: TemplateVariables,
    rules: RuleSet,
    options: ScanOptions,
    deletion_log: bool,
}

/// The settings, or the exit code of what keeps them from being read, said
/// and traced: never the defaults instead of settings that cannot be read.
fn load_settings(language: Language) -> Result<(Settings, PathBuf), u8> {
    let Some(directory) = okapi_platform::config_directory() else {
        tracing::error!("no settings directory");
        error_line(&text(language, &Message::SettingsDirectoryUnknown));
        return Err(exit::FAILURE);
    };
    match Settings::load(&directory) {
        Ok(settings) => Ok((settings, Settings::path(&directory))),
        Err(error) => {
            tracing::error!(%error, "settings unusable");
            error_line(&text(language, &Message::Store(&error)));
            Err(match error {
                StoreError::Unreadable { .. }
                | StoreError::BrokenLink { .. }
                | StoreError::NotAFile { .. }
                | StoreError::Unwritable { .. } => exit::FAILURE,
                StoreError::TooLarge { .. }
                | StoreError::Invalid { .. }
                | StoreError::UnsupportedVersion { .. } => exit::INVALID_ARGUMENTS,
            })
        }
    }
}

/// The session's variables, the rules and the options; or the exit code of
/// what keeps the scan from starting, said and traced.
fn prepare(run: &Run, language: Language) -> Result<Prepared, u8> {
    let say = |message: &Message<'_>| text(language, message);
    let variables = okapi_platform::template_variables().map_err(|error| {
        tracing::error!(?error, "variables unavailable");
        error_line(&say(&Message::Variables(&error)));
        exit::FAILURE
    })?;
    let rules = RuleSet::embedded(Os::Linux).map_err(|error| {
        tracing::error!(?error, "embedded rules unreadable");
        error_line(&say(&Message::RulesUnavailable));
        exit::FAILURE
    })?;
    let (settings, settings_path) = load_settings(language)?;
    let from_settings: Vec<PathBuf> = settings.exclusions.iter().map(PathBuf::from).collect();
    // Checked alone first, to say which file holds a broken exclusion.
    if let Err(error) = Exclusions::new(from_settings.iter().cloned()) {
        tracing::warn!(?error, path = %settings_path.display(), "settings exclusion refused");
        error_line(&say(&Message::SettingsExclusion(&settings_path, &error)));
        return Err(exit::INVALID_ARGUMENTS);
    }
    let written: Vec<PathBuf> = from_settings
        .into_iter()
        .chain(run.exclusions.iter().cloned())
        .collect();
    // Always the user's exclusions, never none as a fallback: an exclusion
    // that cannot be used stops the run.
    let exclusions = Exclusions::new(written.iter().cloned()).map_err(|error| {
        tracing::warn!(?error, "exclusion refused");
        error_line(&say(&Message::Exclusion(&error)));
        exit::INVALID_ARGUMENTS
    })?;
    // A path that does not exist as written is most likely a typing mistake
    // (matching ignores case, so it may still cover something): said before
    // anything is removed.
    let settings_count = settings.exclusions.len();
    for (index, missing) in written.iter().enumerate() {
        if !missing.is_absolute() || fs::symlink_metadata(missing).is_ok() {
            continue;
        }
        let in_settings = index < settings_count;
        tracing::warn!(path = %missing.display(), in_settings, "exclusion matches nothing");
        error_line(&say(&if in_settings {
            Message::SettingsExclusionMissing(&settings_path, missing)
        } else {
            Message::ExclusionMissing(missing)
        }));
    }
    let min_age = match (run.min_age, settings.min_age_minutes) {
        (Some(age), _) => age,
        (None, Some(minutes)) => duration_of_minutes(minutes)
            // Beyond the clock, the age would stop the scan without naming
            // the settings: said here, where they are known to be the cause.
            .filter(|age| SystemTime::now().checked_sub(*age).is_some())
            .ok_or_else(|| {
                tracing::warn!(minutes, "settings minimum age beyond the clock");
                error_line(&say(&Message::SettingsMinAge(&settings_path, minutes)));
                exit::INVALID_ARGUMENTS
            })?,
        (None, None) => DEFAULT_MIN_AGE,
    };
    tracing::info!(?min_age, exclusions = ?written, deletion_log = settings.deletion_log, "effective options");
    Ok(Prepared {
        variables,
        rules,
        options: ScanOptions::new(SystemTime::now(), min_age, exclusions),
        deletion_log: settings.deletion_log,
    })
}

/// Traces each group found, with what the run does with it.
fn trace_groups(groups: &[output::GroupSummary]) {
    for group in groups {
        tracing::info!(
            label = %group.label,
            rules = ?group.rules,
            entries = group.entries.len(),
            bytes = group.bytes,
            preselected = group.preselected,
            cache_reset = group.cache_reset,
            needs_admin = group.needs_admin,
            decision = ?group.decision,
            "group found"
        );
    }
}

/// Adds the cleaning to the history; `false` when it could not, said and
/// traced.
fn record_history(
    history: &mut History,
    receipt: &CleanReceipt,
    deletion_log: bool,
    language: Language,
) -> bool {
    match history.record(&history_entry(receipt, SystemTime::now(), deletion_log)) {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(%error, "history not recorded");
            error_line(&text(language, &Message::HistoryNotRecorded(&error)));
            false
        }
    }
}

/// The history, opened before a cleaning: with a deletion log asked for, a
/// history that cannot be kept stops the run before anything is removed.
fn open_history(deletion_log: bool, language: Language) -> Result<Option<History>, u8> {
    let opened = okapi_platform::state_directory()
        .ok_or(None)
        .and_then(|directory| History::open(&directory).map_err(Some));
    match opened {
        Ok(history) => Ok(Some(history)),
        Err(error) => {
            if let Some(error) = &error {
                tracing::warn!(%error, "history unavailable");
                error_line(&text(language, &Message::Store(error)));
            } else {
                tracing::warn!("no state directory, no history");
                error_line(&text(language, &Message::HistoryDirectoryUnknown));
            }
            if deletion_log {
                tracing::error!("run stopped: the deletion log asked for cannot be kept");
                error_line(&text(language, &Message::DeletionLogUnavailable));
                Err(exit::FAILURE)
            } else {
                error_line(&text(language, &Message::HistoryContinues));
                Ok(None)
            }
        }
    }
}

/// Scans, cleans with `--clean`, writes what happened; gives the exit code.
fn run_scan(run: &Run, language: Language, journal: Option<&Path>, output: &mut Output) -> u8 {
    tracing::info!(options = ?run, "run options");
    let Prepared {
        variables,
        rules,
        options,
        deletion_log,
    } = match prepare(run, language) {
        Ok(prepared) => prepared,
        Err(code) => return code,
    };
    let mut history = if run.clean {
        match open_history(deletion_log, language) {
            Ok(history) => history,
            Err(code) => return code,
        }
    } else {
        None
    };
    let report = scan(&rules, &variables, &options);
    let groups = summaries(&report, &run.categories);
    trace_groups(&groups);
    let scan_stops: Vec<&Stop> = report
        .issues()
        .iter()
        .filter_map(|issue| match issue {
            ScanIssue::Stopped(stop) => Some(stop),
            _ => None,
        })
        .collect();
    let receipt: Option<CleanReceipt> = (run.clean && scan_stops.is_empty()).then(|| {
        let ids: Vec<ItemId> = groups
            .iter()
            .filter(|group| group.decision == Decision::Selected)
            .flat_map(|group| group.ids.iter().copied())
            .collect();
        clean(&report, &ids, &rules, &variables, &options)
    });
    // Recorded before anything is shown: an output that blocks (a pipe, a
    // pager) and an interruption there must not leave a cleaning without
    // its line.
    let history_failed = match (&mut history, &receipt) {
        (Some(history), Some(receipt)) => !record_history(history, receipt, deletion_log, language),
        _ => false,
    };
    let clean_stops: &[Stop] = receipt.as_ref().map_or(&[], CleanReceipt::stops);
    // Said on standard error whatever the verbosity: nothing was looked at or
    // offered, or nothing was cleaned.
    for stop in &scan_stops {
        error_line(&text(language, &Message::Stop(stop)));
    }
    for stop in clean_stops {
        error_line(&text(language, &Message::CleaningStopped(stop)));
    }
    let stops: Vec<&Stop> = scan_stops.into_iter().chain(clean_stops).collect();
    let shown = Shown {
        language,
        report: &report,
        groups: &groups,
        receipt: receipt.as_ref(),
        clean: run.clean,
        verbosity: run.verbosity,
        stopped: !stops.is_empty(),
        journal,
    };
    if run.json {
        shown.write_json(output);
    } else {
        shown.write_text(output);
    }
    let (found_entries, found_bytes) = shown.found();
    tracing::info!(
        found_entries,
        found_bytes,
        removed_bytes = receipt.as_ref().map_or(0, CleanReceipt::removed_bytes),
        stops = stops.len(),
        issues = report.issues().len(),
        "run summary"
    );
    if receipt
        .iter()
        .flat_map(CleanReceipt::outcomes)
        .any(|outcome| outcome.path().is_none())
    {
        tracing::error!("an unknown identifier reached the cleaning");
        error_line(&text(language, &Message::InternalDefect));
    }
    let results = receipt
        .iter()
        .flat_map(|receipt| receipt.outcomes().iter().map(CleanOutcome::result));
    let code = exit::exit_code(stops, found_entries, results);
    let lost_log = history_failed && deletion_log;
    if lost_log {
        tracing::error!(computed = code, "the deletion log asked for is missing");
    }
    exit::with_deletion_log(code, lost_log)
}
