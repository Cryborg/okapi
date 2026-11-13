//! What the caller reads of a run of the helper through pkexec: its exit
//! code and its standard output, read back with the types it was written
//! with. No process here: the caller runs it.

use thiserror::Error;

use crate::answer::{Answer, RefusalAnswer};
use crate::request::{Operation, Versioned};
use crate::{Exit, PROTOCOL_VERSION};

/// pkexec's code when the user dismissed the authentication.
pub const PKEXEC_DISMISSED: i32 = 126;
/// pkexec's code when the caller is not authorized, the authentication
/// failed, or pkexec itself failed (the helper missing included).
pub const PKEXEC_NOT_AUTHORIZED: i32 = 127;
/// Rust's code for a panic, which the helper traces first.
pub const PANICKED: i32 = 101;

/// What a run of the helper gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The answer of the operation asked for: [`Answer::exit`] says
    /// whether something stopped it.
    Answer(Answer),
    /// The request was refused: nothing was read nor touched.
    Refused(RefusalAnswer),
}

/// Why a run of the helper gave no reply to act on.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReplyError {
    #[error("the authentication was dismissed")]
    Dismissed,
    #[error("not authorized, or pkexec failed")]
    NotAuthorized,
    #[error("the helper does not run as root")]
    NotRoot,
    #[error("the helper panicked")]
    Panicked,
    #[error("the helper was ended by a signal")]
    Killed,
    #[error("the helper ended with the unexpected code {code}")]
    UnexpectedCode { code: i32 },
    #[error("the helper speaks version {version} of the protocol")]
    OtherVersion { version: u32 },
    #[error("the helper's answer to {asked:?} cannot be read (code {code}): {detail}")]
    Unreadable {
        asked: Operation,
        code: i32,
        detail: String,
    },
}

/// The reply of a run asked for `operation` that ended with `code` (`None`:
/// ended by a signal) and wrote `output` on its standard output.
///
/// # Errors
///
/// [`ReplyError`]: no answer nor refusal to act on.
pub fn read_reply(
    operation: Operation,
    code: Option<i32>,
    output: &[u8],
) -> Result<Reply, ReplyError> {
    let code = code.ok_or(ReplyError::Killed)?;
    let unreadable = |detail: String| ReplyError::Unreadable {
        asked: operation,
        code,
        detail,
    };
    match code {
        PKEXEC_DISMISSED => return Err(ReplyError::Dismissed),
        PKEXEC_NOT_AUTHORIZED => return Err(ReplyError::NotAuthorized),
        PANICKED => return Err(ReplyError::Panicked),
        _ => {}
    }
    match Exit::from_code(code) {
        Some(Exit::NotRoot) => Err(ReplyError::NotRoot),
        // Any version: a helper of another version says so in a refusal.
        Some(Exit::Refused) => serde_json::from_slice(output)
            .map(Reply::Refused)
            .map_err(|error| unreadable(error.to_string())),
        Some(Exit::Success | Exit::Failure) => {
            let Versioned { version } =
                serde_json::from_slice(output).map_err(|error| unreadable(error.to_string()))?;
            if version != PROTOCOL_VERSION {
                return Err(ReplyError::OtherVersion { version });
            }
            let answer: Answer =
                serde_json::from_slice(output).map_err(|error| unreadable(error.to_string()))?;
            // A cleaning says what became of each entry; a scan removes
            // nothing.
            let outcomes_fit =
                answer.outcomes().is_some() == (answer.operation() == Operation::Clean);
            if answer.operation() != operation
                || i32::from(answer.exit().code()) != code
                || !outcomes_fit
            {
                return Err(unreadable(format!(
                    "an answer to {:?} that ends with {:?}, outcomes given: {}",
                    answer.operation(),
                    answer.exit(),
                    answer.outcomes().is_some()
                )));
            }
            Ok(Reply::Answer(answer))
        }
        None => Err(ReplyError::UnexpectedCode { code }),
    }
}
