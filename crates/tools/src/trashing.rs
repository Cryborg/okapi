//! What every move to the user's trash made by a tool shares: one at a time
//! (a copy a duplicate keeps and a leftover holding it never move together),
//! the date a trashed file says, and what the trash's answer means.

use std::ffi::OsString;
use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError};

use okapi_platform::TrashError;

/// Held while anything is moved: two moves at once, of one tool or of two,
/// could each keep what the other moves.
static MOVING: Mutex<()> = Mutex::new(());

/// The format of a trashed file's date, as the trash specification wants
/// it: local time, without zone.
const DELETION_DATE: &str = "%Y-%m-%dT%H:%M:%S";

/// Holds the right to move until dropped, waiting for another move.
pub(crate) fn one_at_a_time() -> MutexGuard<'static, ()> {
    MOVING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The local time now, as a trashed file is dated.
pub(crate) fn deleted_now() -> String {
    jiff::Zoned::now().strftime(DELETION_DATE).to_string()
}

/// What the system refused with: the kind of its error, and its own code
/// when it gave one (`EIO`, `EINVAL`... that no kind tells apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub kind: io::ErrorKind,
    pub code: Option<i32>,
}

impl From<&io::Error> for Refusal {
    fn from(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            code: error.raw_os_error(),
        }
    }
}

/// What the trash did with what it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Moved {
    Trashed,
    /// What was moved was not what was checked: put back.
    Changed,
    /// On another filesystem than the trash: nothing moved.
    OtherDevice,
    /// Something not known to be what was checked went to the trash and
    /// stays there, restorable, under this name.
    Unchecked(OsString),
    Refused(Refusal),
}

/// What the trash's answer to a move means.
pub(crate) fn moved(answer: Result<OsString, TrashError>) -> Moved {
    match answer {
        Ok(_) => Moved::Trashed,
        Err(TrashError::OtherDevice) => Moved::OtherDevice,
        Err(TrashError::Changed) => Moved::Changed,
        Err(TrashError::Unchecked(name)) => Moved::Unchecked(name),
        // The trash's own folder gone is a refusal, not the thing gone: it
        // was read just before.
        Err(TrashError::Io(error)) => Moved::Refused((&error).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trash_s_answer_says_what_became_of_what_was_moved() {
        assert_eq!(moved(Ok("a".into())), Moved::Trashed);
        assert_eq!(moved(Err(TrashError::OtherDevice)), Moved::OtherDevice);
        assert_eq!(moved(Err(TrashError::Changed)), Moved::Changed);
        assert_eq!(
            moved(Err(TrashError::Unchecked("a.2".into()))),
            Moved::Unchecked("a.2".into())
        );
        let denied = io::Error::from_raw_os_error(13);
        assert_eq!(
            moved(Err(TrashError::Io(denied))),
            Moved::Refused(Refusal {
                kind: io::ErrorKind::PermissionDenied,
                code: Some(13),
            })
        );
    }
}
