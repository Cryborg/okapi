//! The file manager, asked through the desktop portal (`crate::portal`)
//! to show a folder or a file in its folder: the screens hand it what an
//! analysis read, and it opens nothing that is no longer that.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use okapi_platform::FileIdentity;

/// How long a call to the file manager may take before another is allowed.
pub(crate) const OPEN_WAIT: Duration = Duration::from_secs(20);

/// The file manager asked to show `path`: a folder opened, or a file chosen
/// in its folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    pub path: PathBuf,
    pub folder: bool,
    /// What a folder or a file must still be (as the analysis read it).
    pub expected: Option<FileIdentity>,
}

/// Asks the file manager, then calls back with whether it opened (or the
/// user closed what it asked: nothing to say then).
pub type Open = Rc<dyn Fn(&OpenRequest, Box<dyn FnOnce(bool)>)>;

/// What a screen hears of its call, once: whether the file manager opened
/// (or the user closed what it asked); `None` when it did not answer in
/// time.
type Answered = Box<dyn FnOnce(Option<bool>)>;

/// The call to the file manager a screen awaits: one at a time. A file
/// manager that never answers frees the screen after [`OPEN_WAIT`]; the
/// answer to a call given up is no longer heard.
#[derive(Clone, Default)]
pub(crate) struct Calls(Rc<RefCell<CallsState>>);

#[derive(Default)]
struct CallsState {
    made: u64,
    awaited: Option<(u64, Answered)>,
    timer: slint::Timer,
}

impl std::fmt::Debug for Calls {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Calls")
            .field("made", &self.0.borrow().made)
            .finish_non_exhaustive()
    }
}

impl Calls {
    /// Whether a call is awaited.
    pub(crate) fn awaiting(&self) -> bool {
        self.0.borrow().awaited.is_some()
    }

    /// Asks `open` to show `request`, unless a call is awaited (then
    /// nothing is asked): `answered` hears its answer once.
    pub(crate) fn ask(
        &self,
        open: &Open,
        request: &OpenRequest,
        answered: impl FnOnce(Option<bool>) + 'static,
    ) {
        let call = {
            let mut state = self.0.borrow_mut();
            if state.awaited.is_some() {
                tracing::debug!("a call to the file manager already awaited: none asked");
                return;
            }
            state.made += 1;
            let call = state.made;
            state.awaited = Some((call, Box::new(answered)));
            call
        };
        tracing::info!(call, path = %request.path.display(), folder = request.folder, "file manager asked");
        // Set first: an answer may come at once.
        let weak = Rc::downgrade(&self.0);
        self.0
            .borrow()
            .timer
            .start(slint::TimerMode::SingleShot, OPEN_WAIT, move || {
                Self::answer(&weak, call, None);
            });
        let weak = Rc::downgrade(&self.0);
        open(
            request,
            Box::new(move |opened| Self::answer(&weak, call, Some(opened))),
        );
    }

    /// The call awaited is given up: its answer would speak of another
    /// line.
    pub(crate) fn forget(&self) {
        let mut state = self.0.borrow_mut();
        if let Some((call, _)) = state.awaited.take() {
            tracing::debug!(call, "call to the file manager no longer awaited");
        }
        state.timer.stop();
    }

    fn answer(calls: &Weak<RefCell<CallsState>>, call: u64, opened: Option<bool>) {
        let Some(calls) = calls.upgrade() else {
            return;
        };
        let answered = {
            let mut state = calls.borrow_mut();
            match state.awaited.take() {
                Some((awaited, answered)) if awaited == call => {
                    state.timer.stop();
                    answered
                }
                other => {
                    state.awaited = other;
                    tracing::debug!(call, ?opened, "answer of a call no longer awaited");
                    return;
                }
            }
        };
        if opened.is_none() {
            tracing::warn!(call, "the file manager did not answer in time");
        }
        answered(opened);
    }
}

/// The system's way to the file manager: the desktop portal.
#[cfg(target_os = "linux")]
pub(crate) fn open_in_file_manager(request: &OpenRequest, done: Box<dyn FnOnce(bool)>) {
    use crate::portal;

    let opened = still_analyzed(
        okapi_platform::open_path(&request.path, request.folder),
        request.expected,
    );
    let fd = match opened {
        Ok(fd) => fd,
        Err(error) => {
            tracing::warn!(path = %request.path.display(), %error, "not shown in the file manager");
            done(false);
            return;
        }
    };
    let shown = if request.folder {
        portal::Shown::Folder
    } else {
        portal::Shown::Item
    };
    let path = request.path.clone();
    // Answered once: by the portal, or at once when it cannot be asked.
    let done = Rc::new(std::cell::Cell::new(Some(done)));
    let answer_to = Rc::clone(&done);
    let spawned = slint::spawn_local(async move {
        let answer = portal::show(fd, shown).await;
        match &answer {
            Ok(()) => tracing::info!(path = %path.display(), "shown in the file manager"),
            Err(portal::Refusal::Cancelled) => {
                tracing::info!(path = %path.display(), "the file manager closed by the user");
            }
            Err(refusal) => {
                tracing::warn!(path = %path.display(), %refusal, "the file manager did not open");
            }
        }
        if let Some(done) = answer_to.take() {
            done(matches!(answer, Ok(()) | Err(portal::Refusal::Cancelled)));
        }
    });
    if let Err(error) = spawned {
        tracing::warn!(%error, "the file manager could not be asked");
        if let Some(done) = done.take() {
            done(false);
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn open_in_file_manager(_request: &OpenRequest, done: Box<dyn FnOnce(bool)>) {
    done(false);
}

/// The descriptor of what the analysis read, if it still is: `expected`, a
/// folder's or a file's identity then.
#[cfg(target_os = "linux")]
fn still_analyzed(
    opened: std::io::Result<(std::os::fd::OwnedFd, okapi_platform::EntryStatus)>,
    expected: Option<FileIdentity>,
) -> std::io::Result<std::os::fd::OwnedFd> {
    let (fd, status) = opened?;
    match expected {
        Some(expected) if status.identity != expected => {
            Err(std::io::Error::other("no longer what was analyzed"))
        }
        _ => Ok(fd),
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    #[test]
    fn a_folder_replaced_since_the_analysis_is_never_shown() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("small");
        std::fs::create_dir(&folder).unwrap();
        let identity = |opened: std::io::Result<_>| {
            let (_, status): (std::os::fd::OwnedFd, okapi_platform::EntryStatus) = opened.unwrap();
            status.identity
        };
        let read = identity(okapi_platform::open_path(&folder, true));
        assert!(still_analyzed(okapi_platform::open_path(&folder, true), Some(read)).is_ok());
        assert!(still_analyzed(okapi_platform::open_path(&folder, true), None).is_ok());
        std::fs::rename(&folder, home.path().join("moved")).unwrap();
        std::fs::create_dir(&folder).unwrap();
        assert!(still_analyzed(okapi_platform::open_path(&folder, true), Some(read)).is_err());
    }
}
