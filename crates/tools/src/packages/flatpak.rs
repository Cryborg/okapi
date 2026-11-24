//! Flatpak's system helper, on the system bus: the root service libflatpak
//! itself asks to change the system's installation. No command is run
//! (invariant 4); the triggers run in the helper, as dpkg's in PackageKit.
//! A removal is what libflatpak's transaction does, the application first on
//! purpose (libflatpak removes its extensions before it): a refusal then
//! removes nothing; then the extensions that go with it, the repository
//! pruned and the triggers run. The rights are asked by polkit, for the
//! application once.
//! The user's installation has no service: it is never changed.

use std::time::{Duration, Instant};

use super::bus;

const DESTINATION: &str = "org.freedesktop.Flatpak.SystemHelper";
const OBJECT: &str = "/org/freedesktop/Flatpak/SystemHelper";
const INTERFACE: &str = "org.freedesktop.Flatpak.SystemHelper";
const UNINSTALL: &str = "Uninstall";
const PRUNE: &str = "PruneLocalRepo";
const RUN_TRIGGERS: &str = "RunTriggers";
/// No flag: polkit may ask the user, the reference goes (not `KEEP_REF`),
/// never while it runs (not `FORCE_REMOVE`: flatpak sets it aside).
const NO_FLAGS: u32 = 0;
/// The system's default installation, by its name for the helper.
const DEFAULT_INSTALLATION: &str = "";
/// How long a removal is waited for at most: polkit's window waits on the
/// user, then the helper removes. Beyond it, it may still happen.
const UNINSTALL_WAIT: Duration = Duration::from_mins(30);
/// How long pruning the repository, or running the triggers, is waited for
/// at most.
const CLEANUP_WAIT: Duration = Duration::from_mins(5);

/// What the bus, or systemd starting it, answers for a service none
/// provides: flatpak absent, or its unit removed or masked.
const NOT_PROVIDED: [&str; 3] = [
    "org.freedesktop.DBus.Error.Spawn.ServiceNotFound",
    "org.freedesktop.systemd1.NoSuchUnit",
    "org.freedesktop.systemd1.UnitMasked",
];
/// A refusal of the rights: polkit's, or flatpak's own.
const NOT_AUTHORIZED: [&str; 3] = [
    "org.freedesktop.DBus.Error.AccessDenied",
    "org.freedesktop.Flatpak.Error.NotAuthorized",
    "org.freedesktop.Flatpak.Error.PermissionDenied",
];
const NOT_INSTALLED: [&str; 2] = [
    "org.freedesktop.Flatpak.Error.NotInstalled",
    "org.freedesktop.Flatpak.Error.RefNotFound",
];
/// No answer came: the outcome is unknown.
const UNANSWERED: [&str; 4] = [
    "org.freedesktop.DBus.Error.NoReply",
    "org.freedesktop.DBus.Error.Timeout",
    "org.freedesktop.DBus.Error.TimedOut",
    "org.freedesktop.DBus.Error.Disconnected",
];
/// The helper could not be started (it exists: [`NOT_PROVIDED`] and
/// [`bus::UNKNOWN_SERVICE`] say otherwise).
const SPAWN_FAILED: &str = "org.freedesktop.DBus.Error.Spawn.";

/// Why the helper did not do what it was asked.
#[derive(Debug, thiserror::Error)]
pub enum FlatpakError {
    /// The system bus is not reached by a socket.
    #[error("the system bus is not reached by a socket: {0}")]
    Transport(String),
    /// Nobody provides the helper: flatpak is not on this system.
    #[error("flatpak's system helper is not on the system bus")]
    Absent,
    /// The rights were not given.
    #[error("not authorized: {0}")]
    NotAuthorized(String),
    /// What was asked is not installed.
    #[error("not installed: {0}")]
    NotInstalled(String),
    /// No answer came, its outcome unknown.
    #[error("no answer: {0}")]
    Unanswered(String),
    /// Asked, and no answer in its time: it may still happen.
    #[error("no answer after {waited:?}")]
    TimedOut { waited: Duration },
    /// The helper refused, in its own words.
    #[error("{name}: {message}")]
    Service { name: String, message: String },
    /// The bus itself failed (the connection lost...).
    #[error(transparent)]
    Bus(#[from] zbus::Error),
}

/// What the helper's error `name` says, with its `message` (its name when
/// it says none: never an empty refusal).
fn of_reply(name: &str, message: String) -> FlatpakError {
    let message = if message.is_empty() {
        name.to_owned()
    } else {
        message
    };
    if bus::UNKNOWN_SERVICE.contains(&name) || NOT_PROVIDED.contains(&name) {
        FlatpakError::Absent
    } else if NOT_AUTHORIZED.contains(&name) {
        FlatpakError::NotAuthorized(message)
    } else if NOT_INSTALLED.contains(&name) {
        FlatpakError::NotInstalled(message)
    } else if UNANSWERED.contains(&name) || name.starts_with(SPAWN_FAILED) {
        FlatpakError::Unanswered(format!("{name}: {message}"))
    } else {
        FlatpakError::Service {
            name: name.to_owned(),
            message,
        }
    }
}

/// An error of the bus, told by its name when the helper answered one.
fn classified(error: zbus::Error) -> FlatpakError {
    match error {
        zbus::Error::MethodError(name, message, _) => {
            of_reply(name.as_str(), message.unwrap_or_default())
        }
        other => FlatpakError::Bus(other),
    }
}

/// How long each call is waited for.
#[derive(Debug, Clone, Copy)]
struct Waits {
    uninstall: Duration,
    cleanup: Duration,
}

const WAITS: Waits = Waits {
    uninstall: UNINSTALL_WAIT,
    cleanup: CLEANUP_WAIT,
};

/// What a removal did besides removing the application.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Removed {
    /// The extensions asked that did not go, by reference.
    pub left: Vec<String>,
    /// Those already gone when asked: gone, but not freed by this removal.
    pub already_gone: Vec<String>,
    /// The repository was pruned: the space is free (or will be, once the
    /// application, if it runs, closes).
    pub pruned: bool,
}

/// What became of a runtime asked to go.
#[derive(Debug)]
pub enum RuntimeEnd {
    Removed,
    /// Not installed when asked: gone, but not freed by this removal.
    AlreadyGone,
    /// Not asked: an earlier one was refused the rights, or unanswered.
    NotAsked,
    Failed(FlatpakError),
}

/// What a removal of runtimes did.
#[derive(Debug)]
pub struct RuntimesRemoved {
    /// Each runtime's end, in the order asked.
    pub ends: Vec<RuntimeEnd>,
    /// The repository was pruned: the space of those removed is free (or
    /// will be, once an application still running them closes).
    pub pruned: bool,
}

/// Flatpak's system helper, joined on the system bus.
#[derive(Debug, Clone)]
pub struct SystemHelper {
    connection: zbus::Connection,
    waits: Waits,
}

impl SystemHelper {
    /// Joins the system bus, waiting [`bus::CALL_WAIT`] at most.
    ///
    /// # Errors
    /// A bus that is not a socket, that cannot be joined, or that does not
    /// answer in time.
    pub fn connect() -> Result<Self, FlatpakError> {
        let address =
            bus::socket_only(zbus::Address::system()?).map_err(FlatpakError::Transport)?;
        Ok(Self {
            // Its calls are bounded each by its own wait.
            connection: bus::joined(address, bus::CALL_WAIT, None).map_err(
                |error| match error {
                    bus::JoinError::Bus(error) => FlatpakError::Bus(error),
                    unanswered @ bus::JoinError::Unanswered(_) => {
                        FlatpakError::Unanswered(unanswered.to_string())
                    }
                },
            )?,
            waits: WAITS,
        })
    }

    /// Removes the application `app` (a reference, `app/<id>/<arch>/<branch>`),
    /// then each of `related` (extensions, never fatal), then prunes the
    /// repository and runs the triggers (never fatal: the application is
    /// gone).
    ///
    /// # Errors
    /// Why the application did not go: nothing else was then asked.
    pub fn remove(&self, app: &str, related: &[String]) -> Result<Removed, FlatpakError> {
        let _span = tracing::info_span!("flatpak", reference = app).entered();
        if let Err(error) = self.call(
            UNINSTALL,
            &(NO_FLAGS, app, DEFAULT_INSTALLATION),
            self.waits.uninstall,
        ) {
            tracing::warn!(
                ?related,
                "the application not removed: its extensions not asked"
            );
            return Err(error);
        }
        let mut removed = Removed::default();
        for reference in related {
            tracing::info!(%reference, "an extension of the application asked to go");
            // Authorized by the application's rights: polkit's
            // `app-uninstall` implies `runtime-uninstall`.
            match self.call(
                UNINSTALL,
                &(NO_FLAGS, reference.as_str(), DEFAULT_INSTALLATION),
                self.waits.uninstall,
            ) {
                Ok(()) => {}
                Err(FlatpakError::NotInstalled(_)) => {
                    tracing::info!(%reference, "an extension of the application already gone");
                    removed.already_gone.push(reference.clone());
                }
                Err(error) => {
                    tracing::warn!(%reference, %error, "an extension of the application left");
                    removed.left.push(reference.clone());
                }
            }
        }
        match self.call(PRUNE, &(NO_FLAGS, DEFAULT_INSTALLATION), self.waits.cleanup) {
            Ok(()) => removed.pruned = true,
            Err(error) => {
                tracing::warn!(%error, "repository not pruned: the space may not be freed yet");
            }
        }
        // The menu and its icons refreshed; the space is no matter of theirs.
        let triggered = self
            .call(
                RUN_TRIGGERS,
                &(NO_FLAGS, DEFAULT_INSTALLATION),
                self.waits.cleanup,
            )
            .inspect_err(
                |error| tracing::warn!(%error, "triggers not run: the menu may still show it"),
            )
            .is_ok();
        tracing::info!(
            left = removed.left.len(),
            pruned = removed.pruned,
            triggered,
            "flatpak removed"
        );
        Ok(removed)
    }

    /// Removes each runtime of `references` (`runtime/<id>/<arch>/<branch>`),
    /// in their order, then prunes the repository once if one went (no
    /// triggers: flatpak runs them for an application only). The rights not
    /// given, or the helper silent or gone, asks nothing more: the others are
    /// not asked. The helper refusing one (`Can't remove …, it is needed
    /// for …`) goes on with the next. `heard` is told each end as it comes,
    /// by the rank of its reference.
    pub fn remove_runtimes(
        &self,
        references: &[String],
        heard: &dyn Fn(usize, &RuntimeEnd),
    ) -> RuntimesRemoved {
        let _span = tracing::info_span!("flatpak_runtimes", count = references.len()).entered();
        let mut ends = Vec::with_capacity(references.len());
        let mut stopped = false;
        for (rank, reference) in references.iter().enumerate() {
            if stopped {
                heard(rank, &RuntimeEnd::NotAsked);
                ends.push(RuntimeEnd::NotAsked);
                continue;
            }
            let called = self.call(
                UNINSTALL,
                &(NO_FLAGS, reference.as_str(), DEFAULT_INSTALLATION),
                self.waits.uninstall,
            );
            let end = match called {
                Ok(()) => {
                    tracing::info!(%reference, "a runtime removed");
                    RuntimeEnd::Removed
                }
                Err(FlatpakError::NotInstalled(_)) => {
                    tracing::info!(%reference, "a runtime already gone");
                    RuntimeEnd::AlreadyGone
                }
                Err(error @ FlatpakError::Service { .. }) => {
                    tracing::warn!(%reference, %error, "a runtime kept by the helper");
                    RuntimeEnd::Failed(error)
                }
                Err(error) => {
                    tracing::warn!(%reference, %error, "a runtime not removed: the next ones not asked");
                    stopped = true;
                    RuntimeEnd::Failed(error)
                }
            };
            heard(rank, &end);
            ends.push(end);
        }
        let pruned = ends.iter().any(|end| matches!(end, RuntimeEnd::Removed))
            && self
                .call(PRUNE, &(NO_FLAGS, DEFAULT_INSTALLATION), self.waits.cleanup)
                .inspect_err(|error| tracing::warn!(%error, "repository not pruned: the space may not be freed yet"))
                .is_ok();
        tracing::info!(pruned, "flatpak's runtimes asked to go");
        RuntimesRemoved { ends, pruned }
    }

    /// Calls `method` with `body`, waiting `wait` at most.
    fn call<B>(&self, method: &str, body: &B, wait: Duration) -> Result<(), FlatpakError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let started = Instant::now();
        let answered = async {
            self.connection
                .call_method(Some(DESTINATION), OBJECT, Some(INTERFACE), method, body)
                .await
                .map(drop)
                .map_err(classified)
        };
        let deadline = async {
            async_io::Timer::after(wait).await;
            Err(FlatpakError::TimedOut { waited: wait })
        };
        let ended = futures_lite::future::block_on(futures_lite::future::or(answered, deadline));
        match &ended {
            Ok(()) => {
                tracing::debug!(method, elapsed = ?started.elapsed(), "flatpak's helper answered");
            }
            // Expected of an extension gone meanwhile: its caller says so.
            Err(error @ FlatpakError::NotInstalled(_)) => {
                tracing::debug!(method, %error, elapsed = ?started.elapsed(), "flatpak's helper: not installed");
            }
            Err(error) => {
                tracing::warn!(method, %error, elapsed = ?started.elapsed(), "flatpak's helper failed");
            }
        }
        ended
    }
}

#[cfg(test)]
pub(crate) mod tests;
