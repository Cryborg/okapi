//! `okapi-helper`, the privileged helper: its protocol is the library's
//! (`okapi_helper`).

use std::process::ExitCode;

use okapi_helper::Exit;
use okapi_store::journal;

fn main() -> ExitCode {
    journal::start_on_standard_error();
    journal::trace_panics();
    // Who asked pkexec, traced only: the helper acts on its own rules alone.
    let caller = std::env::var_os("PKEXEC_UID");
    let _run = tracing::info_span!("helper", pid = std::process::id(), caller = ?caller).entered();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "okapi-helper started");
    let exit = run();
    tracing::info!(?exit, code = exit.code(), "okapi-helper finished");
    ExitCode::from(exit)
}

#[cfg(not(target_os = "linux"))]
fn run() -> Exit {
    tracing::error!("the helper runs on Linux only");
    Exit::Failure
}

#[cfg(target_os = "linux")]
fn run() -> Exit {
    use std::io;
    use std::time::SystemTime;

    use okapi_engine::{Cancel, Watch};
    use okapi_helper::{cancel_at_end_of, operate, runs_as_root};

    let (real, effective) = (
        okapi_platform::real_user(),
        okapi_platform::effective_user(),
    );
    if !runs_as_root(real, effective) {
        tracing::error!(?real, ?effective, "not run as root: nothing read");
        return Exit::NotRoot;
    }
    let rules = match okapi_rules::RuleSet::embedded(okapi_rules::Os::Linux) {
        Ok(rules) => rules,
        Err(error) => {
            tracing::error!(%error, "embedded rules unreadable");
            return Exit::Failure;
        }
    };
    let now = SystemTime::now();
    let request = match accepted_request(&rules, now) {
        Ok(request) => request,
        Err(exit) => return exit,
    };
    // Root's own (`HOME=/root`, no `XDG_*` under pkexec): what invariant 6
    // protects here is root's directories.
    let variables = match okapi_platform::template_variables() {
        Ok(variables) => variables,
        Err(error) => {
            tracing::error!(%error, "variables unavailable");
            return Exit::Failure;
        }
    };
    tracing::info!(?variables, "variables");
    let cancel = Cancel::new();
    cancel_at_end_of(io::stdin(), cancel.clone());
    let answer = operate(
        request.operation(),
        request.selection(),
        &rules,
        &variables,
        request.options(now),
        &Watch::new(cancel, None),
    );
    written(&answer, answer.exit())
}

/// The request read from standard input and checked against `rules`; or how
/// the helper ends, its refusal answered.
#[cfg(target_os = "linux")]
fn accepted_request(
    rules: &okapi_rules::RuleSet,
    now: std::time::SystemTime,
) -> Result<okapi_helper::Request, Exit> {
    use okapi_helper::{RefusalAnswer, Request, read_request};

    let line = read_request(&mut std::io::stdin().lock()).map_err(|error| {
        tracing::error!(kind = ?error.kind(), code = error.raw_os_error(), %error, "the request cannot be read");
        Exit::Failure
    })?;
    match Request::read(&line, rules, now) {
        Ok(request) => {
            tracing::info!(?request, "request accepted");
            Ok(request)
        }
        Err(refusal) => {
            tracing::warn!(refused = refusal.key(), target = ?refusal.target(), %refusal, "request refused");
            Err(written(&RefusalAnswer::of(&refusal), Exit::Refused))
        }
    }
}

/// Writes `document` on one line of standard output; `exit` once written,
/// [`Exit::Failure`] when it cannot be.
#[cfg(target_os = "linux")]
fn written(document: &impl serde::Serialize, exit: Exit) -> Exit {
    use std::io::{self, Write};

    let write = || -> io::Result<()> {
        let mut bytes = serde_json::to_vec(document).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let mut output = io::stdout().lock();
        output.write_all(&bytes)?;
        output.flush()
    };
    match write() {
        Ok(()) => exit,
        Err(error) => {
            tracing::error!(kind = ?error.kind(), code = error.raw_os_error(), %error, "the answer cannot be written");
            Exit::Failure
        }
    }
}
