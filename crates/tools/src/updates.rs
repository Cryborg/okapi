//! What waits for an update: the system's packages (PackageKit), the snaps
//! (snapd), and whether a restart is awaited. Each source is read on its
//! own ([`read_system`], [`read_snaps`], [`read_reboot`]), on a thread the
//! caller starts, so that a slow or failing one never hides the others.
//! They take their [`Sources`] as a seam: the daemons are simulated by the
//! tests. Installing is [`install`]'s.

use std::io;
use std::time::{Instant, SystemTime};

use jiff::Timestamp;
use okapi_rules::Language;

use crate::packages::apt;
use crate::packages::packagekit::{self, Details, Package, PackageId, PackageKitError, info};
use crate::packages::reboot::{self, RebootRequired};
use crate::packages::snapd::{self, InUse, Refresh, Schedule, SnapdError};

pub mod install;

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
    /// PackageKit's identifier of the update (`name;version;arch;data`):
    /// what is asked to install it.
    pub id: String,
    pub name: String,
    /// As the package names it (`1:3.5.1-1ubuntu2`).
    pub version: String,
    /// The version it replaces, when PackageKit says it.
    pub installed: Option<String>,
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
    /// The version it replaces, when snapd says it.
    pub installed: Option<String>,
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
    /// What installing the updates (not those held back) downloads, when
    /// every size is known.
    pub download_size: Option<u64>,
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
    /// The packages installed named as `names`.
    ///
    /// # Errors
    /// Why PackageKit did not say.
    fn installed(&self, names: &[&str]) -> Result<Vec<Package>, PackageKitError>;
    /// The details of the packages `ids`, their download size among them.
    ///
    /// # Errors
    /// Why PackageKit did not say.
    fn details(
        &self,
        ids: &[String],
        waiting: &dyn Fn(bool),
    ) -> Result<Vec<Details>, PackageKitError>;
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
    /// The snaps installed, by their revision in use.
    ///
    /// # Errors
    /// Why snapd did not say.
    fn snaps_in_use(&self) -> Result<Vec<InUse>, SnapdError>;
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

    fn installed(&self, names: &[&str]) -> Result<Vec<Package>, PackageKitError> {
        packagekit::PackageKit::connect(self.locale)?.resolve(names)
    }

    fn details(
        &self,
        ids: &[String],
        waiting: &dyn Fn(bool),
    ) -> Result<Vec<Details>, PackageKitError> {
        packagekit::PackageKit::connect(self.locale)?.details(ids, waiting)
    }

    fn lists_downloaded_at(&self) -> io::Result<Option<SystemTime>> {
        apt::lists_downloaded_at(std::path::Path::new(apt::PERIODIC))
    }

    fn snaps(&self) -> Result<Vec<Refresh>, SnapdError> {
        snapd::Snapd::system().refreshes()
    }

    fn snaps_in_use(&self) -> Result<Vec<InUse>, SnapdError> {
        snapd::Snapd::system().in_use()
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
            system_updates(&packages).map_or(Outcome::Failed(Failure::Broken), |mut updates| {
                add_installed(sources, &mut updates);
                Outcome::Read(updates)
            })
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
    let download_size = match &updates {
        Outcome::Read(updates) => download_size(sources, updates, waiting),
        _ => None,
    };
    match &updates {
        Outcome::Read(updates) => tracing::info!(
            elapsed = ?started.elapsed(),
            ?refresh,
            listed_at = ?listed_at,
            download_size,
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
        download_size,
    }
}

/// What installing `updates` (those held back aside) downloads: the sum of
/// their download sizes, none when one is not known (a part would pass for
/// all) or PackageKit does not say (traced).
fn download_size(
    sources: &dyn Sources,
    updates: &[SystemUpdate],
    waiting: &dyn Fn(bool),
) -> Option<u64> {
    let ids: Vec<String> = updates
        .iter()
        .filter(|update| update.kind != Kind::Blocked)
        .map(|update| update.id.clone())
        .collect();
    if ids.is_empty() {
        return Some(0);
    }
    let details = sources
        .details(&ids, waiting)
        .inspect_err(|error| tracing::warn!(%error, "the updates' download sizes not read"))
        .ok()?;
    let size = |id: &String| {
        details
            .iter()
            .find(|details| details.id == *id)
            .and_then(|details| details.download_size)
    };
    let unknown: Vec<&String> = ids.iter().filter(|id| size(id).is_none()).collect();
    if !unknown.is_empty() {
        tracing::warn!(
            ?unknown,
            said = details.len(),
            "updates without a download size: no total"
        );
        return None;
    }
    Some(ids.iter().filter_map(size).sum())
}

/// Reads the snaps waiting, and when snapd refreshes them.
pub fn read_snaps(sources: &dyn Sources) -> Outcome<Snaps> {
    let _snaps = tracing::info_span!("snaps").entered();
    let started = Instant::now();
    let outcome = match sources.snaps() {
        Ok(refreshes) => {
            let mut updates = snap_updates(refreshes);
            add_snaps_in_use(sources, &mut updates);
            Outcome::Read(Snaps {
                updates,
                next_refresh: next_refresh(sources.schedule()),
            })
        }
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
/// for two architectures carries its own. Only an update's kinds (from low
/// to security) and held back are kept: what apt lists beside them
/// (packages it would install, remove, obsolete or downgrade to upgrade
/// everything) and any kind unknown are no update, left out and traced;
/// installing the updates brings what they need, the simulation shows it.
fn system_updates(packages: &[Package]) -> Option<Vec<SystemUpdate>> {
    let (packages, others): (Vec<&Package>, Vec<&Package>) = packages
        .iter()
        .partition(|package| is_update(package.info) || package.info == info::BLOCKED);
    if !others.is_empty() {
        let others: Vec<String> = others
            .iter()
            .map(|package| format!("{} ({})", package.id, package.info))
            .collect();
        tracing::info!(?others, "listed beside the updates, no update: left out");
    }
    let ids: Vec<(PackageId<'_>, &Package)> = packages
        .into_iter()
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
                id: package.id.clone(),
                name: if shared {
                    format!("{}:{}", id.name, id.arch)
                } else {
                    id.name.to_owned()
                },
                version: id.version.to_owned(),
                installed: None,
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

/// The version each of `updates` replaces, as PackageKit says it, by name
/// and architecture; none when it does not say (traced): the update is
/// then told without it.
fn add_installed(sources: &dyn Sources, updates: &mut [SystemUpdate]) {
    let installed = {
        let mut names: Vec<&str> = updates
            .iter()
            .filter_map(|update| PackageId::parse(&update.id))
            .map(|id| id.name)
            .collect();
        names.sort_unstable();
        names.dedup();
        if names.is_empty() {
            return;
        }
        match sources.installed(&names) {
            Ok(installed) => installed,
            Err(error) => {
                tracing::warn!(%error, "the versions the updates replace not read");
                return;
            }
        }
    };
    for update in updates.iter_mut() {
        update.installed = PackageId::parse(&update.id).and_then(|wanted| {
            installed
                .iter()
                .filter_map(|package| PackageId::parse(&package.id))
                .find(|id| id.name == wanted.name && id.arch == wanted.arch)
                .map(|id| id.version.to_owned())
        });
    }
    let unknown: Vec<&str> = updates
        .iter()
        .filter(|update| update.installed.is_none())
        .map(|update| update.name.as_str())
        .collect();
    if !unknown.is_empty() {
        tracing::info!(?unknown, "updates whose version installed is not said");
    }
}

/// The version each snap of `updates` replaces, as snapd says it; none
/// when it does not say (traced).
fn add_snaps_in_use(sources: &dyn Sources, updates: &mut [SnapUpdate]) {
    if updates.is_empty() {
        return;
    }
    let in_use = match sources.snaps_in_use() {
        Ok(in_use) => in_use,
        Err(error) => {
            tracing::warn!(%error, "the versions the snaps' updates replace not read");
            return;
        }
    };
    for update in updates.iter_mut() {
        update.installed = in_use
            .iter()
            .find(|snap| snap.name == update.name && !snap.version.is_empty())
            .map(|snap| snap.version.clone());
        if update.installed.is_none() {
            tracing::info!(snap = %update.name, "a snap's version in use is not said");
        }
    }
}

/// Whether `info` is an update's kind, from low to security.
const fn is_update(info: u32) -> bool {
    info >= info::LOW && info <= info::SECURITY
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
            installed: None,
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
        error if error.busy() => Failure::Busy,
        error if error.offline() => Failure::Offline,
        error if error.rights_refused() => Failure::Refused,
        PackageKitError::TimedOut { .. } => Failure::Unanswered,
        // A call to the bus that got no reply in time (`method_timeout`).
        PackageKitError::Bus(zbus::Error::InputOutput(error))
            if error.kind() == io::ErrorKind::TimedOut =>
        {
            Failure::Unanswered
        }
        PackageKitError::Failed { .. }
        | PackageKitError::Transport(_)
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
