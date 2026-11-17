//! Docker's daemon for the cleaner: its socket, and whether it accepts the
//! user, looked at as an analysis ends without asking the daemon anything (a
//! daemon its socket starts would start for nothing); the prune of its build
//! cache, asked on the cleaning's thread once the user's part is over.

use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use okapi_rules::CleanupAction;
use okapi_store::{OperationEnd, OperationReason, OperationRecord};
use okapi_tools::docker::{self, Access, Docker, DockerError, Pruned};

use crate::shown::{shown, size};
use crate::ui;

/// Docker's socket for the session, and whether the daemon accepts the user.
pub type Find = Rc<dyn Fn() -> Option<(PathBuf, Access)>>;
/// Prunes the build cache of the daemon on a socket: entries unused for the
/// duration given at least, the wait cancelled when the function says.
pub type Prune =
    Arc<dyn Fn(&Path, Duration, &dyn Fn() -> bool) -> Result<Pruned, DockerError> + Send + Sync>;

/// What the cleaner reads of Docker and asks of it: the system's, or a
/// test's, which never touches the machine.
#[derive(Clone)]
pub struct DockerSeam {
    pub find: Find,
    pub prune: Prune,
}

impl DockerSeam {
    /// The session's Docker: the socket its CLI would speak to, as the
    /// system judges it for the user.
    pub fn system() -> Self {
        Self {
            find: Rc::new(|| {
                let socket = docker::socket()?;
                match docker::access(&socket) {
                    Ok(access) => {
                        tracing::debug!(socket = %socket.display(), ?access, "Docker's socket");
                        Some((socket, access))
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        tracing::info!(socket = %socket.display(), "Docker's socket not there: no line");
                        None
                    }
                    Err(error) => {
                        tracing::warn!(socket = %socket.display(), %error, code = error.raw_os_error(), "Docker's socket cannot be judged: no line");
                        None
                    }
                }
            }),
            prune: Arc::new(|socket, min_age, cancelled| {
                Docker::at(socket).prune_build_cache(min_age, cancelled)
            }),
        }
    }
}

/// What became of the prune a cleaning asked for.
#[derive(Debug)]
pub enum Pruning {
    Pruned(Pruned),
    /// The cleaning was cancelled before Docker was asked: nothing went.
    NotAsked,
    /// Docker was asked, or tried: [`DockerError::leaves_unknown`] says
    /// whether something may have gone (a cancellation while it worked
    /// included).
    Failed(DockerError),
}

impl Pruning {
    /// Whether it ran to its end.
    pub const fn is_done(&self) -> bool {
        matches!(self, Self::Pruned(_))
    }

    /// The room it freed, as far as known (see [`OperationEnd`]).
    pub const fn freed(&self) -> u64 {
        match self {
            Self::Pruned(pruned) => pruned.bytes,
            Self::NotAsked | Self::Failed(_) => 0,
        }
    }

    /// Its line among the operations of the history line.
    pub const fn record(&self) -> OperationRecord {
        let end = match self {
            Self::Pruned(pruned) => OperationEnd::Done {
                entries: pruned.caches,
                bytes: pruned.bytes,
            },
            Self::NotAsked => OperationEnd::NotDone {
                reason: OperationReason::Cancelled,
            },
            Self::Failed(error) if error.leaves_unknown() => OperationEnd::Unknown {
                reason: reason_of(error),
            },
            Self::Failed(error) => OperationEnd::NotDone {
                reason: reason_of(error),
            },
        };
        OperationRecord {
            action: CleanupAction::DockerBuildPrune,
            end,
        }
    }
}

/// Why `error` kept the prune from ending as asked, as the history says it.
const fn reason_of(error: &DockerError) -> OperationReason {
    match error {
        DockerError::Absent { .. } => OperationReason::Absent,
        DockerError::NotAllowed { .. } => OperationReason::NotAllowed,
        DockerError::Unreached(_) => OperationReason::Unreached,
        DockerError::Io(_) => OperationReason::Unanswered,
        DockerError::Cancelled => OperationReason::Cancelled,
        DockerError::Unreadable(_) => OperationReason::Unreadable,
        DockerError::Refused { .. } => OperationReason::Refused,
    }
}

/// The line of the receipt that says what an operation of the cleaning
/// did, from its history record (the cleaner's receipt and the history's
/// detail say it alike); none for an operation that is not Docker's prune,
/// which no cleaning records.
pub fn operation_shown(record: &OperationRecord) -> Option<ui::OperationShown> {
    if record.action != CleanupAction::DockerBuildPrune {
        tracing::warn!(action = ?record.action, "an operation of a cleaning that is not Docker's prune");
        return None;
    }
    let (end, entries, bytes) = match record.end {
        OperationEnd::Done {
            entries: 0,
            bytes: 0,
        } => (ui::OperationEnd::Nothing, 0, 0),
        OperationEnd::Done { entries, bytes } => (ui::OperationEnd::Done, entries, bytes),
        OperationEnd::Unknown {
            reason: OperationReason::Cancelled,
        } => (ui::OperationEnd::CutShort, 0, 0),
        OperationEnd::Unknown { .. } => (ui::OperationEnd::Unknown, 0, 0),
        OperationEnd::NotDone { reason } => (
            match reason {
                OperationReason::Cancelled => ui::OperationEnd::Cancelled,
                OperationReason::Absent => ui::OperationEnd::Absent,
                OperationReason::NotAllowed => ui::OperationEnd::NotAllowed,
                OperationReason::Refused => ui::OperationEnd::Refused,
                // Said as never reached: nothing ran.
                OperationReason::Unreached
                | OperationReason::Unanswered
                | OperationReason::Unreadable => ui::OperationEnd::Unreached,
            },
            0,
            0,
        ),
    };
    Some(ui::OperationShown {
        end,
        entries: shown(entries),
        size: size(bytes),
    })
}

#[cfg(test)]
pub mod tests {
    use std::sync::Mutex;

    use super::*;

    /// No Docker on the machine of a test.
    pub fn none() -> DockerSeam {
        DockerSeam {
            find: Rc::new(|| None),
            prune: Arc::new(|_, _, _| {
                Err(DockerError::Unreached(io::Error::other(
                    "no Docker in a test",
                )))
            }),
        }
    }

    /// The socket of the Docker [`fake`] plays.
    pub const FAKE_SOCKET: &str = "/run/docker.sock";

    /// A Docker on [`FAKE_SOCKET`] that accepts the user or not (`access`),
    /// and whose prune gives what `answer` makes of the cancellation it is
    /// given; the minimum ages it was asked for land in `asked`.
    pub fn fake(
        access: Access,
        asked: Arc<Mutex<Vec<Duration>>>,
        answer: impl Fn(&dyn Fn() -> bool) -> Result<Pruned, DockerError> + Send + Sync + 'static,
    ) -> DockerSeam {
        DockerSeam {
            find: Rc::new(move || Some((PathBuf::from(FAKE_SOCKET), access))),
            prune: Arc::new(move |socket, min_age, cancelled| {
                assert_eq!(
                    socket,
                    Path::new(FAKE_SOCKET),
                    "the socket found at the analysis"
                );
                asked.lock().unwrap().push(min_age);
                answer(cancelled)
            }),
        }
    }

    #[test]
    fn a_prune_is_recorded_by_its_end() {
        let pruned = Pruning::Pruned(Pruned {
            caches: 2,
            bytes: 300,
        });
        assert_eq!(
            pruned.record(),
            OperationRecord {
                action: CleanupAction::DockerBuildPrune,
                end: OperationEnd::Done {
                    entries: 2,
                    bytes: 300
                }
            }
        );
        assert_eq!(pruned.freed(), 300);
        assert!(pruned.is_done());
        let end = |pruning: Pruning| pruning.record().end;
        assert_eq!(
            end(Pruning::NotAsked),
            OperationEnd::NotDone {
                reason: OperationReason::Cancelled
            },
            "not asked: nothing went"
        );
        assert_eq!(
            end(Pruning::Failed(DockerError::Cancelled)),
            OperationEnd::Unknown {
                reason: OperationReason::Cancelled
            },
            "cancelled while Docker worked: it may have pruned"
        );
        assert_eq!(
            end(Pruning::Failed(DockerError::Unreadable("?".to_owned()))),
            OperationEnd::Unknown {
                reason: OperationReason::Unreadable
            }
        );
        assert_eq!(
            end(Pruning::Failed(DockerError::Absent {
                socket: PathBuf::from(FAKE_SOCKET),
                source: io::ErrorKind::ConnectionRefused.into(),
            })),
            OperationEnd::NotDone {
                reason: OperationReason::Absent
            }
        );
        let refused = |status| {
            Pruning::Failed(DockerError::Refused {
                status,
                message: "no".to_owned(),
            })
        };
        assert_eq!(
            end(refused(400)),
            OperationEnd::NotDone {
                reason: OperationReason::Refused
            }
        );
        assert_eq!(
            end(refused(500)),
            OperationEnd::Unknown {
                reason: OperationReason::Refused
            },
            "a failure of the server's own may come once records went"
        );
        assert!(!refused(400).is_done());
        assert_eq!(refused(400).freed(), 0);
    }

    #[test]
    fn each_end_is_said_in_words_of_its_own() {
        let shown_of = |end: OperationEnd| {
            operation_shown(&OperationRecord {
                action: CleanupAction::DockerBuildPrune,
                end,
            })
            .unwrap()
        };
        let not_done = |reason| shown_of(OperationEnd::NotDone { reason }).end;
        assert_eq!(
            shown_of(OperationEnd::Done {
                entries: 0,
                bytes: 0
            })
            .end,
            ui::OperationEnd::Nothing
        );
        assert_eq!(
            shown_of(OperationEnd::Done {
                entries: 1,
                bytes: 0
            })
            .end,
            ui::OperationEnd::Done
        );
        let room_alone = shown_of(OperationEnd::Done {
            entries: 0,
            bytes: 2_000,
        });
        assert_eq!(room_alone.end, ui::OperationEnd::Done, "room freed: done");
        assert_eq!(room_alone.size, size(2_000));
        assert_eq!(
            shown_of(OperationEnd::Unknown {
                reason: OperationReason::Unanswered
            })
            .end,
            ui::OperationEnd::Unknown
        );
        assert_eq!(
            shown_of(OperationEnd::Unknown {
                reason: OperationReason::Cancelled
            })
            .end,
            ui::OperationEnd::CutShort,
            "cancelled while Docker worked: said apart"
        );
        assert_eq!(
            not_done(OperationReason::Cancelled),
            ui::OperationEnd::Cancelled
        );
        assert_eq!(not_done(OperationReason::Absent), ui::OperationEnd::Absent);
        assert_eq!(
            not_done(OperationReason::NotAllowed),
            ui::OperationEnd::NotAllowed
        );
        assert_eq!(
            not_done(OperationReason::Refused),
            ui::OperationEnd::Refused
        );
        assert_eq!(
            not_done(OperationReason::Unreached),
            ui::OperationEnd::Unreached
        );
        assert!(
            operation_shown(&OperationRecord {
                action: CleanupAction::UvPrune,
                end: OperationEnd::Done {
                    entries: 1,
                    bytes: 1
                }
            })
            .is_none()
        );
    }
}
