//! What the process tells its caller.

use std::io;

use okapi_engine::{CleanResult, KeepReason, Stop};

/// The run did what was asked.
pub const SUCCESS: u8 = 0;
/// The run failed: a stop (nothing was looked at), an unusable session or
/// settings file, a deletion log asked for that could not be kept, an output
/// that could not be written, an internal fault.
pub const FAILURE: u8 = 1;
/// What the user wrote is invalid: an argument, the settings, an exclusion
/// or an age, from the command line or the settings.
pub const INVALID_ARGUMENTS: u8 = 2;
/// The system refused to remove at least one entry.
pub const PERMISSION_DENIED: u8 = 3;
/// At least one entry was not removed, for another reason.
pub const PARTIAL: u8 = 4;
/// The scan found nothing in the categories asked for.
pub const NOTHING_FOUND: u8 = 5;

/// The exit code of a run that got as far as scanning: `stops` what kept the
/// scan or the cleaning from looking at anything, `found` the entries offered
/// in the categories asked for, `results` what became of each entry cleaned.
pub fn exit_code<'a>(
    stops: impl IntoIterator<Item = &'a Stop>,
    found: usize,
    results: impl IntoIterator<Item = CleanResult>,
) -> u8 {
    let mut stopped = false;
    for stop in stops {
        // An age beyond the clock comes from the command line or the settings.
        if *stop == Stop::MinAgeBeyondClock {
            return INVALID_ARGUMENTS;
        }
        stopped = true;
    }
    if stopped {
        return FAILURE;
    }
    let mut code = if found == 0 { NOTHING_FOUND } else { SUCCESS };
    for result in results {
        let reason = match result {
            CleanResult::Removed { .. } | CleanResult::Gone => continue,
            CleanResult::Partial { reason, .. } | CleanResult::Kept(reason) => reason,
        };
        let this = match reason {
            // The command line only passes identifiers of its own report.
            KeepReason::UnknownItem => return FAILURE,
            KeepReason::Failed {
                kind: io::ErrorKind::PermissionDenied,
                ..
            } => PERMISSION_DENIED,
            _ => PARTIAL,
        };
        code = if code == PERMISSION_DENIED || this == PERMISSION_DENIED {
            PERMISSION_DENIED
        } else {
            this
        };
    }
    code
}

/// `code` once the cleaning's history line is known lost (`lost_log`) while
/// the user asked for a deletion log: the run failed, cleaned or not, unless
/// the input (command line or settings) was invalid in the first place.
pub const fn with_deletion_log(code: u8, lost_log: bool) -> u8 {
    if lost_log && code != INVALID_ARGUMENTS {
        FAILURE
    } else {
        code
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused() -> KeepReason {
        KeepReason::Failed {
            kind: io::ErrorKind::PermissionDenied,
            code: Some(13),
        }
    }

    #[test]
    fn a_lost_deletion_log_fails_the_run() {
        assert_eq!(with_deletion_log(SUCCESS, true), FAILURE);
        assert_eq!(with_deletion_log(PARTIAL, true), FAILURE);
        assert_eq!(
            with_deletion_log(INVALID_ARGUMENTS, true),
            INVALID_ARGUMENTS
        );
        assert_eq!(with_deletion_log(PARTIAL, false), PARTIAL);
    }

    #[test]
    fn codes_follow_their_precedence() {
        let removed = CleanResult::Removed { bytes: 1 };
        assert_eq!(exit_code(&[Stop::UnsupportedSystem], 3, [removed]), FAILURE);
        assert_eq!(
            exit_code(&[Stop::UnsupportedSystem, Stop::MinAgeBeyondClock], 3, []),
            INVALID_ARGUMENTS,
            "an age beyond the clock is an invalid argument"
        );
        assert_eq!(exit_code(&[], 0, []), NOTHING_FOUND);
        assert_eq!(exit_code(&[], 2, [removed, CleanResult::Gone]), SUCCESS);
        assert_eq!(
            exit_code(&[], 2, []),
            SUCCESS,
            "found, nothing to clean by default"
        );
        assert_eq!(
            exit_code(&[], 2, [removed, CleanResult::Kept(KeepReason::Recent)]),
            PARTIAL
        );
        // Both orders: permission denied wins over a partial removal.
        let partial = CleanResult::Partial {
            bytes: 1,
            reason: KeepReason::Changed,
        };
        assert_eq!(exit_code(&[], 2, [partial]), PARTIAL);
        // Only a refusal of permission is said apart, not every failure.
        let failed = KeepReason::Failed {
            kind: io::ErrorKind::NotFound,
            code: Some(2),
        };
        assert_eq!(exit_code(&[], 2, [CleanResult::Kept(failed)]), PARTIAL);
        assert_eq!(
            exit_code(
                &[],
                2,
                [CleanResult::Partial {
                    bytes: 1,
                    reason: failed
                }]
            ),
            PARTIAL
        );
        assert_eq!(
            exit_code(&[], 2, [partial, CleanResult::Kept(refused())]),
            PERMISSION_DENIED
        );
        assert_eq!(
            exit_code(&[], 2, [CleanResult::Kept(refused()), partial]),
            PERMISSION_DENIED
        );
        assert_eq!(
            exit_code(
                &[],
                2,
                [CleanResult::Partial {
                    bytes: 1,
                    reason: refused()
                }]
            ),
            PERMISSION_DENIED
        );
        assert_eq!(
            exit_code(
                &[],
                2,
                [CleanResult::Kept(KeepReason::UnknownItem), partial]
            ),
            FAILURE
        );
    }
}
