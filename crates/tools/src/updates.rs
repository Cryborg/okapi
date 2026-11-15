//! What waits for an update: the system's packages (PackageKit), the snaps
//! (snapd), and whether a restart is awaited. Each source is read on its
//! own ([`read_system`], [`read_snaps`], [`read_reboot`]), on a thread the
//! caller starts, so that a slow or failing one never hides the others.
//! They take their [`Sources`] as a seam: the daemons are simulated by the
//! tests. Nothing here installs anything.

use std::io;
use std::time::{Instant, SystemTime};

use jiff::Timestamp;
use okapi_rules::Language;

use crate::packages::apt;
use crate::packages::packagekit::{self, Package, PackageId, PackageKitError, error_code, info};
use crate::packages::reboot::{self, RebootRequired};
use crate::packages::snapd::{self, Refresh, Schedule, SnapdError};

/// How an update matters, in the order they are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Security,
    Other,
    /// Held back by the system: it will not be installed.
    Blocked,
}

/// A package of the system with a newer version waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemUpdate {
    pub name: String,
    /// As the package names it (`1:3.5.1-1ubuntu2`).
    pub version: String,
    pub summary: String,
    pub kind: Kind,
}

/// A snap with a newer revision waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapUpdate {
    pub name: String,
    /// Its title, or its name when it has none.
    pub title: String,
    pub publisher: Option<String>,
    /// The store vouches for its publisher.
    pub verified: bool,
    pub version: String,
    pub download_size: Option<u64>,
}

/// The snaps waiting, and when snapd refreshes them by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snaps {
    pub updates: Vec<SnapUpdate>,
    pub next_refresh: Option<Timestamp>,
}

/// Why a source could not be read, as the user is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Another program installs: the package manager is held.
    Busy,
    /// The network or the repositories could not be reached.
    Offline,
    /// The system refused the rights (polkit).
    Refused,
    /// No answer in time.
    Unanswered,
    /// Anything else (traced with its cause).
    Broken,
}

/// What a source came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<T> {
    /// It is not on this system.
    Absent,
    Read(T),
    Failed(Failure),
}

/// What became of the package lists' download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListsRefresh {
    NotAsked,
    Done,
    /// Not downloaded: the updates are what the old lists say.
    Failed(Failure),
}

/// One reading of the system's packages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemReading {
    /// Security first, held back last, by name.
    pub updates: Outcome<Vec<SystemUpdate>>,
    pub refresh: ListsRefresh,
    /// When apt last downloaded its lists with success, if known: unknown
    /// is no proof they are fresh.
    pub listed_at: Option<Timestamp>,
}

/// Where the updates are read from, on threads of their own. A source that
/// waits for another program tells `waiting` when it starts and stops.
pub trait Sources: Send + Sync {
    /// Downloads the system's package lists again.
    ///
    /// # Errors
    /// Why PackageKit did not.
    fn refresh_system(&self, waiting: &dyn Fn(bool)) -> Result<(), PackageKitError>;
    /// The system's packages with an update waiting.
    ///
    /// # Errors
    /// Why PackageKit did not say.
    fn system(&self, waiting: &dyn Fn(bool)) -> Result<Vec<Package>, PackageKitError>;
    /// When apt last downloaded its lists.
    ///
    /// # Errors
    /// Why it could not be read.
    fn lists_downloaded_at(&self) -> io::Result<Option<SystemTime>>;
    /// The snaps with a newer revision.
    ///
    /// # Errors
    /// Why snapd did not say.
    fn snaps(&self) -> Result<Vec<Refresh>, SnapdError>;
    /// When snapd refreshes by itself.
    ///
    /// # Errors
    /// Why snapd did not say.
    fn schedule(&self) -> Result<Schedule, SnapdError>;
    /// Whether a restart is awaited.
    ///
    /// # Errors
    /// Why it could not be checked.
    fn reboot(&self) -> io::Result<Option<RebootRequired>>;
}

/// The daemons and the files of this system.
#[derive(Debug, Clone)]
pub struct SystemSources {
    locale: &'static str,
}

impl SystemSources {
    /// The texts PackageKit gives (packages' summaries) in `language`.
    #[must_use]
    pub const fn new(language: Language) -> Self {
        Self {
            locale: crate::packagekit_locale(language),
        }
    }
}

impl Sources for SystemSources {
    fn refresh_system(&self, waiting: &dyn Fn(bool)) -> Result<(), PackageKitError> {
        packagekit::PackageKit::connect(self.locale)?.refresh_cache(waiting)
    }

    fn system(&self, waiting: &dyn Fn(bool)) -> Result<Vec<Package>, PackageKitError> {
        packagekit::PackageKit::connect(self.locale)?.get_updates(waiting)
    }

    fn lists_downloaded_at(&self) -> io::Result<Option<SystemTime>> {
        apt::lists_downloaded_at(std::path::Path::new(apt::PERIODIC))
    }

    fn snaps(&self) -> Result<Vec<Refresh>, SnapdError> {
        snapd::Snapd::system().refreshes()
    }

    fn schedule(&self) -> Result<Schedule, SnapdError> {
        snapd::Snapd::system().schedule()
    }

    fn reboot(&self) -> io::Result<Option<RebootRequired>> {
        reboot::reboot_required(std::path::Path::new(reboot::DIRECTORY))
    }
}

/// Reads the system's packages; `refresh` downloads their lists first.
pub fn read_system(sources: &dyn Sources, refresh: bool, waiting: &dyn Fn(bool)) -> SystemReading {
    let _system = tracing::info_span!("system", refresh).entered();
    let started = Instant::now();
    let refresh = if refresh {
        match sources.refresh_system(waiting) {
            Ok(()) => ListsRefresh::Done,
            // No PackageKit is no failed download: the reading below says
            // `Absent`.
            Err(PackageKitError::Absent) => ListsRefresh::NotAsked,
            Err(error) => {
                tracing::warn!(%error, "package lists not downloaded again");
                ListsRefresh::Failed(failure_of_packagekit(&error))
            }
        }
    } else {
        ListsRefresh::NotAsked
    };
    let updates = match sources.system(waiting) {
        Ok(packages) => {
            system_updates(&packages).map_or(Outcome::Failed(Failure::Broken), Outcome::Read)
        }
        Err(PackageKitError::Absent) => {
            tracing::info!("no PackageKit on this system");
            Outcome::Absent
        }
        Err(error) => {
            tracing::warn!(%error, "system updates not read");
            Outcome::Failed(failure_of_packagekit(&error))
        }
    };
    let listed_at = sources
        .lists_downloaded_at()
        .inspect_err(|error| tracing::warn!(%error, "when apt's lists were downloaded is unknown"))
        .inspect(|at| {
            if at.is_none() {
                tracing::info!("apt left no stamp of its last download: the lists' age is unknown");
            }
        })
        .ok()
        .flatten()
        .and_then(|at| {
            Timestamp::try_from(at)
                .inspect_err(|error| tracing::warn!(%error, "apt's stamp out of range"))
                .ok()
        });
    match &updates {
        Outcome::Read(updates) => tracing::info!(
            elapsed = ?started.elapsed(),
            ?refresh,
            listed_at = ?listed_at,
            updates = updates.len(),
            security = updates.iter().filter(|update| update.kind == Kind::Security).count(),
            blocked = updates.iter().filter(|update| update.kind == Kind::Blocked).count(),
            "system updates read"
        ),
        other => tracing::info!(
            elapsed = ?started.elapsed(),
            ?refresh,
            outcome = ?other,
            "system updates not read"
        ),
    }
    SystemReading {
        updates,
        refresh,
        listed_at,
    }
}

/// Reads the snaps waiting, and when snapd refreshes them.
pub fn read_snaps(sources: &dyn Sources) -> Outcome<Snaps> {
    let _snaps = tracing::info_span!("snaps").entered();
    let started = Instant::now();
    let outcome = match sources.snaps() {
        Ok(refreshes) => Outcome::Read(Snaps {
            updates: snap_updates(refreshes),
            next_refresh: next_refresh(sources.schedule()),
        }),
        Err(SnapdError::Absent { socket, .. }) => {
            tracing::info!(socket = %socket.display(), "no snapd on this system");
            Outcome::Absent
        }
        Err(error) => {
            tracing::warn!(%error, "snap updates not read");
            Outcome::Failed(failure_of_snapd(&error))
        }
    };
    tracing::info!(
        elapsed = ?started.elapsed(),
        snaps = ?match &outcome {
            Outcome::Read(snaps) => Ok(snaps.updates.len()),
            other => Err(other),
        },
        "snap updates done"
    );
    outcome
}

/// Whether a restart is awaited; unknown is none, traced.
pub fn read_reboot(sources: &dyn Sources) -> Option<RebootRequired> {
    sources
        .reboot()
        .inspect_err(|error| tracing::warn!(%error, "whether a restart is awaited is unknown"))
        .ok()
        .flatten()
}

/// The packages PackageKit names, sorted; none when an identifier cannot be
/// read (traced): a list short of one would pass for the whole. A name met
/// for two architectures carries its own.
fn system_updates(packages: &[Package]) -> Option<Vec<SystemUpdate>> {
    let ids: Vec<(PackageId<'_>, &Package)> = packages
        .iter()
        .map(|package| {
            PackageId::parse(&package.id)
                .map(|id| (id, package))
                .ok_or(&package.id)
        })
        .collect::<Result<_, _>>()
        .inspect_err(|id| tracing::warn!(%id, "an update's identifier unreadable"))
        .ok()?;
    let mut updates: Vec<SystemUpdate> = ids
        .iter()
        .map(|(id, package)| {
            let shared = ids
                .iter()
                .filter(|(other, _)| other.name == id.name)
                .count()
                > 1;
            SystemUpdate {
                name: if shared {
                    format!("{}:{}", id.name, id.arch)
                } else {
                    id.name.to_owned()
                },
                version: id.version.to_owned(),
                summary: package.summary.clone(),
                kind: match package.info {
                    info::SECURITY => Kind::Security,
                    info::BLOCKED => Kind::Blocked,
                    _ => Kind::Other,
                },
            }
        })
        .collect();
    updates.sort_by(|first, second| (first.kind, &first.name).cmp(&(second.kind, &second.name)));
    Some(updates)
}

fn snap_updates(refreshes: Vec<Refresh>) -> Vec<SnapUpdate> {
    let mut updates: Vec<SnapUpdate> = refreshes
        .into_iter()
        .map(|refresh| SnapUpdate {
            title: refresh
                .title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| refresh.name.clone()),
            name: refresh.name,
            verified: refresh.publisher.as_ref().is_some_and(|publisher| {
                publisher
                    .validation
                    .as_deref()
                    .is_some_and(|validation| snapd::VALIDATION_VOUCHED.contains(&validation))
            }),
            publisher: refresh.publisher.map(|publisher| publisher.display_name),
            version: refresh.version,
            download_size: refresh.download_size,
        })
        .collect();
    updates.sort_by_cached_key(|update| update.title.to_lowercase());
    updates
}

/// The next automatic refresh, if snapd says one that can be read: never
/// before refreshes are held until.
fn next_refresh(schedule: Result<Schedule, SnapdError>) -> Option<Timestamp> {
    let schedule = schedule
        .inspect_err(|error| tracing::warn!(%error, "snapd's schedule not read"))
        .ok()?;
    let read = |said: Option<String>| -> Option<Timestamp> {
        let said = said?;
        said.parse()
            .inspect_err(|error| tracing::warn!(%said, %error, "snapd's moment unreadable"))
            .ok()
    };
    let next = read(schedule.next)?;
    Some(read(schedule.hold).map_or(next, |hold| next.max(hold)))
}

fn failure_of_packagekit(error: &PackageKitError) -> Failure {
    match error {
        PackageKitError::Failed { code, .. } => match *code {
            error_code::CANNOT_GET_LOCK | error_code::LOCK_REQUIRED => Failure::Busy,
            error_code::NO_NETWORK
            | error_code::REPO_NOT_AVAILABLE
            | error_code::NO_MORE_MIRRORS_TO_TRY
            | error_code::CANNOT_FETCH_SOURCES => Failure::Offline,
            error_code::NOT_AUTHORIZED => Failure::Refused,
            _ => Failure::Broken,
        },
        PackageKitError::TimedOut { waiting: true, .. } => Failure::Busy,
        PackageKitError::TimedOut { .. } => Failure::Unanswered,
        // A call to the bus that got no reply in time (`method_timeout`).
        PackageKitError::Bus(zbus::Error::InputOutput(error))
            if error.kind() == io::ErrorKind::TimedOut =>
        {
            Failure::Unanswered
        }
        PackageKitError::Transport(_)
        | PackageKitError::Absent
        | PackageKitError::Bus(_)
        | PackageKitError::Ended { .. }
        | PackageKitError::Vanished
        | PackageKitError::Unreadable { .. }
        // A removal's: never one of the updates.
        | PackageKitError::NotOnlyRemoving { .. } => Failure::Broken,
    }
}

fn failure_of_snapd(error: &SnapdError) -> Failure {
    if error.timed_out() {
        Failure::Unanswered
    } else if error.offline() {
        Failure::Offline
    } else {
        Failure::Broken
    }
}

#[cfg(test)]
mod tests;
