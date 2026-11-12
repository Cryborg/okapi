//! Okapi command line interface: scans what the cleaning rules can remove,
//! and with `--clean` removes the preselected groups, in the same process
//! (identifiers never outlive the report that gave them, invariant 1).

mod args;
mod exit;
mod journal;
mod output;
mod selection;
mod text;

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Instant, SystemTime};

use okapi_engine::{
    CleanOutcome, CleanReceipt, Exclusions, ItemId, ScanIssue, ScanOptions, Stop, clean, scan,
};
use okapi_rules::{Os, RuleSet, THIRD_PARTY_NOTICES, TemplateVariables};

use crate::args::{Command, Run, Verbosity};
use crate::output::{Output, Shown, error_line, summaries};
use crate::selection::Decision;
use crate::text::{Language, Message, language, text};

/// Okapi's own license, shown by `okapi licenses`.
const LICENSE: &str = include_str!("../../../LICENSE");

fn main() -> ExitCode {
    let language = language(|name| env::var_os(name));
    let say = |message: &Message<'_>| text(language, message);
    let mut output = Output::new();
    let run = match args::parse(env::args_os().skip(1)) {
        Ok(Command::Help) => return informational(&mut output, language, &[&say(&Message::Help)]),
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
    let (journal, rotation) = match journal::start(verbose, language) {
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
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%panic, %backtrace, "okapi panicked");
        default_hook(panic);
    }));
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

/// What a scan needs, read from the session and the command line.
struct Prepared {
    variables: TemplateVariables,
    rules: RuleSet,
    options: ScanOptions,
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
    // Always the user's exclusions, never none as a fallback: an exclusion
    // that cannot be used stops the run.
    let exclusions = Exclusions::new(run.exclusions.iter().cloned()).map_err(|error| {
        tracing::warn!(?error, "exclusion refused");
        error_line(&say(&Message::Exclusion(&error)));
        exit::INVALID_ARGUMENTS
    })?;
    // A path that does not exist as written is most likely a typing mistake
    // (matching ignores case, so it may still cover something): said before
    // anything is removed.
    for missing in run
        .exclusions
        .iter()
        .filter(|path| path.is_absolute() && fs::symlink_metadata(path).is_err())
    {
        tracing::warn!(path = %missing.display(), "exclusion matches nothing");
        error_line(&say(&Message::ExclusionMissing(missing)));
    }
    Ok(Prepared {
        variables,
        rules,
        options: ScanOptions::new(SystemTime::now(), run.min_age, exclusions),
    })
}

/// Scans, cleans with `--clean`, writes what happened; gives the exit code.
fn run_scan(run: &Run, language: Language, journal: Option<&Path>, output: &mut Output) -> u8 {
    tracing::info!(options = ?run, "run options");
    let Prepared {
        variables,
        rules,
        options,
    } = match prepare(run, language) {
        Ok(prepared) => prepared,
        Err(code) => return code,
    };
    let report = scan(&rules, &variables, &options);
    let groups = summaries(&report, &run.categories);
    for group in &groups {
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
    let clean_stops: &[Stop] = receipt.as_ref().map_or(&[], CleanReceipt::stops);
    // Said on standard error whatever the verbosity: nothing was looked at,
    // or nothing was cleaned.
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
    exit::exit_code(stops, found_entries, results)
}
