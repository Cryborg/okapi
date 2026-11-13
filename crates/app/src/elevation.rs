//! Running the privileged helper through pkexec, the interface's one command
//! (invariant 4): a fixed program, fixed arguments, one request line on its
//! standard input, kept open until the answer (its end cancels the helper).
//! What the reply says is the helper library's (`okapi_helper::read_reply`).

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use okapi_engine::Cancel;
use okapi_helper::{Operation, Reply, ReplyError, read_reply};

/// How often the run looks at the cancel token and at the helper.
const LOOK_EVERY: Duration = Duration::from_millis(50);
/// How much of an answer that cannot be acted on the journal keeps.
const TRACED_OUTPUT_CHARACTERS: usize = 200;

/// How far a run of the helper went, for the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElevationStep {
    /// pkexec runs: the system may be asking for the password.
    Launched,
    /// The helper wrote its first trace: the authentication is over.
    Authenticated,
}

/// Why a run of the helper gave no reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElevationFailure {
    /// Cancelled before it was launched: nothing asked.
    Cancelled,
    /// The helper is not installed where pkexec is told to find it.
    NotInstalled,
    /// pkexec could not be started.
    Unlaunchable { kind: io::ErrorKind },
    /// It was launched, and its end could not be watched: what it did is
    /// unknown (its traces may say).
    Unwatched { kind: io::ErrorKind },
    /// It ran, and gave nothing to act on.
    Reply(ReplyError),
}

/// How the helper is run: pkexec and the helper's path, or, in a test, a
/// script that plays them.
#[derive(Debug, Clone)]
pub struct Launcher {
    program: PathBuf,
    arguments: Vec<OsString>,
    /// What must exist for the run to make sense.
    helper: PathBuf,
}

impl Launcher {
    /// pkexec on the installed helper; never through `PATH`, and never its
    /// own text agent (a terminal nobody reads), so that without a
    /// graphical agent it fails at once.
    #[must_use]
    pub fn pkexec() -> Self {
        Self {
            program: okapi_platform::PKEXEC_PATH.into(),
            arguments: vec![
                "--disable-internal-agent".into(),
                okapi_platform::HELPER_PATH.into(),
            ],
            helper: okapi_platform::HELPER_PATH.into(),
        }
    }

    /// `script` run by bash, which plays pkexec and the helper.
    #[cfg(test)]
    #[must_use]
    pub fn fake(script: &str) -> Self {
        Self {
            program: "/bin/bash".into(),
            arguments: vec!["-c".into(), script.into(), "fake-helper".into()],
            helper: "/bin/bash".into(),
        }
    }

    /// Runs the helper on `line` for `operation`, its traces added to the
    /// journal, its steps sent to `steps`; `cancel` closes its standard
    /// input, which cancels it once it runs (not pkexec's question). Nothing
    /// is launched once `cancel` is cancelled.
    ///
    /// # Errors
    ///
    /// [`ElevationFailure`]: nothing to act on.
    #[expect(
        clippy::disallowed_types,
        reason = "pkexec on the helper: the interface's one command, its program and arguments fixed"
    )]
    pub fn run(
        &self,
        operation: Operation,
        line: Vec<u8>,
        cancel: &Cancel,
        steps: &Sender<ElevationStep>,
    ) -> Result<Reply, ElevationFailure> {
        #[cfg(test)]
        assert!(
            self.program != std::path::Path::new(okapi_platform::PKEXEC_PATH),
            "no test runs pkexec"
        );
        if cancel.is_cancelled() {
            tracing::info!(?operation, "cancelled before the helper was launched");
            return Err(ElevationFailure::Cancelled);
        }
        if !self.helper.exists() {
            tracing::error!(helper = %self.helper.display(), "the helper is not installed");
            return Err(ElevationFailure::NotInstalled);
        }
        let started = Instant::now();
        let mut child = std::process::Command::new(&self.program)
            .args(&self.arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                tracing::error!(kind = ?error.kind(), code = error.raw_os_error(), %error, program = %self.program.display(), "pkexec cannot be started");
                ElevationFailure::Unlaunchable { kind: error.kind() }
            })?;
        tracing::info!(?operation, pid = child.id(), "helper launched");
        send(steps, ElevationStep::Launched);
        let watched = watch(&mut child, line, cancel, steps);
        // Whatever happened, the helper is waited for: never a zombie.
        let status = match watched {
            Ok(status) => Ok(status),
            Err(error) => {
                if let Err(waited) = child.wait() {
                    tracing::error!(kind = ?waited.kind(), code = waited.raw_os_error(), %waited, "the helper cannot be waited for");
                }
                Err(error)
            }
        };
        let (status, output) = match status {
            Ok((status, output)) => (status, output),
            Err(error) => {
                tracing::error!(kind = ?error.kind(), code = error.raw_os_error(), %error, "the helper's end cannot be watched");
                return Err(ElevationFailure::Unwatched { kind: error.kind() });
            }
        };
        let reply = read_reply(operation, status.code(), &output);
        match &reply {
            Ok(_) => {
                tracing::info!(code = status.code(), elapsed = ?started.elapsed(), "helper answered");
            }
            Err(error @ (ReplyError::Dismissed | ReplyError::NotAuthorized)) => {
                tracing::warn!(%error, code = status.code(), "helper not authorized");
            }
            Err(error) => tracing::error!(
                %error,
                code = status.code(),
                elapsed = ?started.elapsed(),
                output = %String::from_utf8_lossy(&output).chars().take(TRACED_OUTPUT_CHARACTERS).collect::<String>(),
                "helper gave no answer to act on"
            ),
        }
        reply.map_err(ElevationFailure::Reply)
    }
}

/// Feeds the request to `child` and reads it to its end: its status and its
/// standard output. Its standard error goes to the journal meanwhile.
fn watch(
    child: &mut std::process::Child,
    line: Vec<u8>,
    cancel: &Cancel,
    steps: &Sender<ElevationStep>,
) -> io::Result<(std::process::ExitStatus, Vec<u8>)> {
    let missing = || io::Error::other("a stream of the helper is missing");
    let input = child.stdin.take().ok_or_else(missing)?;
    let output = child.stdout.take().ok_or_else(missing)?;
    let errors = child.stderr.take().ok_or_else(missing)?;
    let span = tracing::Span::current();
    let traces = {
        let steps = steps.clone();
        let span = span.clone();
        thread::Builder::new()
            .name("okapi-helper-traces".to_owned())
            .spawn(move || span.in_scope(|| journal_traces(BufReader::new(errors), &steps)))?
    };
    let answer = thread::Builder::new()
        .name("okapi-helper-answer".to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            BufReader::new(output)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        })?;
    let ended = Arc::new(AtomicBool::new(false));
    let writer = {
        let (cancel, ended) = (cancel.clone(), Arc::clone(&ended));
        thread::Builder::new()
            .name("okapi-helper-input".to_owned())
            .spawn(move || span.in_scope(|| feed(input, line, &cancel, &ended)))?
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(LOOK_EVERY),
            Err(error) => {
                // Its input is let go: never a thread left feeding it.
                ended.store(true, Ordering::Relaxed);
                return Err(error);
            }
        }
    };
    ended.store(true, Ordering::Relaxed);
    for (name, joined) in [("input", writer.join()), ("traces", traces.join())] {
        if joined.is_err() {
            tracing::error!(name, "a reader of the helper panicked");
        }
    }
    let output = answer
        .join()
        .map_err(|_| io::Error::other("the reader of the helper's answer panicked"))??;
    Ok((status, output))
}

/// Sends `step`; a screen gone away stops nothing.
fn send(steps: &Sender<ElevationStep>, step: ElevationStep) {
    if steps.send(step).is_err() {
        tracing::debug!(?step, "step of the helper unread");
    }
}

/// Adds every line the helper (and pkexec) writes on standard error to the
/// journal, as it comes, to its end: a line that is not UTF-8 is added
/// inexactly, never a reason to stop reading (the helper would then write
/// into a closed pipe). The first JSON line ends the authentication.
fn journal_traces(mut errors: impl BufRead, steps: &Sender<ElevationStep>) {
    let mut authenticated = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        match errors.read_until(b'\n', &mut line) {
            Ok(0) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches('\n');
                if !authenticated && text.starts_with('{') {
                    authenticated = true;
                    send(steps, ElevationStep::Authenticated);
                }
                // At the helper's own level; a line of pkexec's is no trace.
                match okapi_store::journal::level_of(text) {
                    Some(tracing::Level::ERROR) => {
                        tracing::error!(target: "okapi_helper", line = %text, "helper trace");
                    }
                    Some(tracing::Level::WARN) | None => {
                        tracing::warn!(target: "okapi_helper", line = %text, "helper trace");
                    }
                    Some(_) => tracing::info!(target: "okapi_helper", line = %text, "helper trace"),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                tracing::warn!(kind = ?error.kind(), %error, "the helper's traces cannot be read");
                return;
            }
        }
    }
}

/// Writes `line` on the helper's standard input, then keeps it open until
/// the helper ends or `cancel` is cancelled: closing it cancels the helper.
fn feed(mut input: ChildStdin, mut line: Vec<u8>, cancel: &Cancel, ended: &AtomicBool) {
    line.push(b'\n');
    if let Err(error) = input.write_all(&line).and_then(|()| input.flush()) {
        // pkexec ended at once: its code says why.
        tracing::warn!(kind = ?error.kind(), %error, "the request cannot be written");
        return;
    }
    while !ended.load(Ordering::Relaxed) {
        if cancel.is_cancelled() {
            tracing::info!("helper's input closed: cancelled");
            return;
        }
        thread::sleep(LOOK_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use okapi_helper::{PKEXEC_DISMISSED, PKEXEC_NOT_AUTHORIZED, ReplyError};

    use super::*;

    /// A scan answer of no group, as the helper writes it.
    const EMPTY_SCAN: &str =
        r#"{"version":2,"operation":"scan","stops":[],"groups":[],"issues":[]}"#;

    fn run(script: &str, cancel: &Cancel) -> (Result<Reply, ElevationFailure>, Vec<ElevationStep>) {
        let (sender, steps) = mpsc::channel();
        let reply = Launcher::fake(script).run(Operation::Scan, b"{}".to_vec(), cancel, &sender);
        (reply, steps.try_iter().collect())
    }

    #[test]
    fn an_answer_is_read_and_its_steps_said() {
        // The request read, a trace, then the answer once its input is seen
        // still open: a wait that times out (above 128), not an end (1).
        let script = format!(
            "read -r request; echo '{{\"fields\":{{}}}}' >&2; \
             read -r -t 1 more; [ $? -gt 128 ] || exit 9; echo '{EMPTY_SCAN}'; exit 0"
        );
        let (reply, steps) = run(&script, &Cancel::new());
        let Ok(Reply::Answer(answer)) = reply else {
            panic!("expected an answer: {reply:?}");
        };
        assert_eq!(answer.operation(), Operation::Scan);
        assert_eq!(
            steps,
            [ElevationStep::Launched, ElevationStep::Authenticated]
        );
    }

    #[test]
    fn the_codes_of_pkexec_are_said() {
        for (code, error) in [
            (PKEXEC_DISMISSED, ReplyError::Dismissed),
            (PKEXEC_NOT_AUTHORIZED, ReplyError::NotAuthorized),
        ] {
            let (reply, steps) = run(&format!("echo 'not json' >&2; exit {code}"), &Cancel::new());
            assert_eq!(reply, Err(ElevationFailure::Reply(error)));
            assert_eq!(steps, [ElevationStep::Launched], "never authenticated");
        }
    }

    #[test]
    fn cancelling_closes_the_helpers_input() {
        let cancel = Cancel::new();
        let canceller = {
            let cancel = cancel.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(200));
                cancel.cancel();
            })
        };
        // Its request read, it answers on the end of its input: a wait of
        // ten seconds would mean the input stayed open.
        let script = "read -r request || exit 8; read -r -t 10 more; \
                      [ $? -le 128 ] || exit 9; echo 'no answer'; exit 1";
        let (reply, _) = run(script, &cancel);
        canceller.join().unwrap();
        assert!(
            matches!(
                reply,
                Err(ElevationFailure::Reply(ReplyError::Unreadable {
                    code: 1,
                    ..
                }))
            ),
            "{reply:?}"
        );
    }

    #[test]
    fn nothing_is_launched_once_cancelled() {
        let cancel = Cancel::new();
        cancel.cancel();
        let (reply, steps) = run("exit 0", &cancel);
        assert_eq!(reply, Err(ElevationFailure::Cancelled));
        assert!(steps.is_empty(), "nothing launched");
    }

    #[test]
    fn every_trace_is_read_to_the_end_a_bad_byte_included() {
        let (sender, steps) = mpsc::channel();
        journal_traces(&b"not \xff utf-8\n{\"fields\":{}}\nlast"[..], &sender);
        assert_eq!(
            steps.try_iter().collect::<Vec<_>>(),
            [ElevationStep::Authenticated],
            "read past the bad line, to its first JSON line"
        );
    }

    #[test]
    fn a_missing_helper_is_said_without_running_anything() {
        let launcher = Launcher {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "exit 0".into()],
            helper: "/nowhere/okapi-helper".into(),
        };
        let (sender, steps) = mpsc::channel();
        assert_eq!(
            launcher.run(Operation::Scan, Vec::new(), &Cancel::new(), &sender),
            Err(ElevationFailure::NotInstalled)
        );
        assert!(steps.try_iter().next().is_none(), "nothing launched");
    }

    #[test]
    #[should_panic(expected = "no test runs pkexec")]
    fn no_test_runs_pkexec() {
        let (sender, _steps) = mpsc::channel();
        let _ = Launcher::pkexec().run(Operation::Scan, Vec::new(), &Cancel::new(), &sender);
    }
}
