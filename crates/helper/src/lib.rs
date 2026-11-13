//! The privileged helper and its protocol: `okapi-helper`, run as root by
//! pkexec (the path `okapi_platform::HELPER_PATH`, which its polkit policy
//! names), scans and cleans the system targets of its own embedded rules that
//! need the administrator, and nothing else.
//!
//! It reads one request on one line of its standard input ([`Request`]): the
//! targets by the path the rules write them with, never a path to remove
//! (invariant 1), an age no younger than [`HELPER_MIN_AGE_FLOOR`], and
//! exclusions. It scans those targets itself and, for a cleaning, removes
//! what that scan offers there, through the engine and its checks. It writes
//! one JSON document on its standard output ([`Answer`], or [`RefusalAnswer`])
//! and its traces on its standard error, never a file of its own (no
//! journal). It runs no command (invariant 4).
//!
//! The end of its standard input, after the request, cancels it: the caller
//! keeps it open until the answer, as a user cannot signal a process of root
//! (but from a terminal: by hand, end it with Ctrl-D, never Ctrl-C, which
//! kills it mid-cleaning). Besides its own [`Exit`] codes, the caller meets
//! pkexec's (126: the authentication dismissed; 127: not authorized, the
//! authentication failed, or an error of pkexec, the helper missing
//! included) and 101 for a panic, traced first.

mod answer;
mod operation;
mod reply;
mod request;

use std::io::{self, Read};
use std::process::ExitCode;
use std::thread::{self, JoinHandle};

use okapi_engine::Cancel;

pub use answer::{Answer, GroupAnswer, IssueAnswer, OutcomeAnswer, RefusalAnswer};
pub use operation::operate;
pub use reply::{PANICKED, PKEXEC_DISMISSED, PKEXEC_NOT_AUTHORIZED, Reply, ReplyError, read_reply};
pub use request::{
    HELPER_MIN_AGE_FLOOR, MAX_REQUEST_BYTES, Operation, Refusal, Request, Selection, admin_targets,
    read_request, request_line,
};

/// The version of the request and of the answer; a helper of another version
/// refuses the request (`unsupportedVersion`).
pub const PROTOCOL_VERSION: u32 = 2;

/// How the helper ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The answer is complete, which is not to say the targets are clean:
    /// its issues say which roots the scan could not see whole (unreadable,
    /// a limit, a group withheld; the entries held back below a root are
    /// only traced), and a cleaning's outcomes which entries it kept and
    /// why. The caller reads both.
    Success,
    /// Something stopped the scan or the cleaning (the answer says what), or
    /// the helper could not answer (no complete answer on standard output,
    /// its traces say why).
    Failure,
    /// The request was refused: nothing was read nor touched.
    Refused,
    /// The helper does not run as root: it read nothing.
    NotRoot,
}

impl Exit {
    /// The process's exit code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Refused => 2,
            Self::NotRoot => 3,
        }
    }

    /// The ending of exit code `code`, if it is one of the helper's.
    #[must_use]
    pub fn from_code(code: i32) -> Option<Self> {
        [Self::Success, Self::Failure, Self::Refused, Self::NotRoot]
            .into_iter()
            .find(|exit| i32::from(exit.code()) == code)
    }
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        Self::from(exit.code())
    }
}

/// Whether the helper may run: as root, really and effectively. The real
/// user matters too: the cleaning's checks ask the system what the real user
/// may remove (`accessat`).
#[must_use]
pub fn runs_as_root(real: Option<u32>, effective: Option<u32>) -> bool {
    let root = Some(okapi_platform::ROOT_USER);
    real == root && effective == root
}

/// Cancels `cancel` once `input` ends, or cannot be read (its caller is no
/// longer to be trusted): the caller's one way to stop the helper.
pub fn cancel_at_end_of(mut input: impl Read + Send + 'static, cancel: Cancel) -> JoinHandle<()> {
    let span = tracing::Span::current();
    thread::spawn(move || {
        let _span = span.entered();
        // Also the end of a run whose answer is written: the answer's stops
        // say whether anything was cancelled.
        match io::copy(&mut input, &mut io::sink()) {
            Ok(ignored) => tracing::info!(ignored, "end of input: cancelled"),
            Err(error) => tracing::warn!(
                kind = ?error.kind(),
                code = error.raw_os_error(),
                %error,
                "input unreadable: cancelled"
            ),
        }
        cancel.cancel();
    })
}

// The helper's operations remove through directory descriptors, on Linux.
#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;
