//! Installing the updates through the daemons made for it (D-15): the
//! system's packages by PackageKit, simulated first so that what the update
//! removes is shown and accepted, then installed right after the simulation
//! that allowed it; the snaps by snapd, their change followed. The rights
//! are asked by the daemons' polkit, never by Okapi; nothing here runs a
//! command (invariant 4), and what is asked is only what the caller read.
//!
//! What dpkg removes is shown and accepted as well as can be: an update
//! cannot bound what it removes (unlike a removal's `allow_deps`), the real
//! transaction resolves again. A removal never shown is caught as dpkg says
//! it, traced and told.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::packages::dpkg;
use crate::packages::packagekit::{
    self, Package, PackageId, PackageKitError, Updated, Updating, error_code, info, status,
};
use crate::packages::snapd::{self, CHANGE_DONE, Change, Followed, SnapdError};

use super::{SystemSources, SystemUpdate};

/// How long a refresh of snaps is followed at most once snapd took it: a
/// large download on a slow network.
const SNAPS_WAIT: Duration = Duration::from_hours(2);
/// How many looks at a refresh may fail in a row: snapd may refresh itself
/// and restart, answering nothing for a while.
const SNAPS_LOOKS_FAILED: u32 = 60;

/// Why the updates were not installed, as the user is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Another program installs.
    Busy,
    /// A previous installation was cut short: dpkg must finish it first.
    Interrupted,
    /// Packages could not be downloaded.
    Offline,
    /// No room left to download or unpack.
    NoSpace,
    /// The rights were not given.
    Refused,
    /// The updates changed since they were read.
    Changed,
    /// An app of a snap to refresh is open: snapd refuses until it is
    /// closed.
    AppsOpen,
    /// A package or a repository is not signed by a trusted key.
    Unsigned,
    /// No answer, or none in time: the outcome is unknown.
    Unanswered,
    /// Anything else (traced with its cause).
    Broken,
}

/// A package the update brings in or takes away, as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Affected {
    pub name: String,
    pub architecture: String,
    pub summary: String,
}

impl Affected {
    /// What tells it from another: its name and architecture.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}:{}", self.name, self.architecture)
    }
}

/// What updating the system would do, as simulated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Simulated {
    /// The names of the packages updated.
    pub updated: Vec<String>,
    /// Packages brought in (a new kernel...).
    pub added: Vec<Affected>,
    /// Packages taken away or made obsolete: shown, accepted before.
    pub removed: Vec<Affected>,
}

impl Simulated {
    /// How many packages dpkg will handle.
    #[must_use]
    pub fn total(&self) -> usize {
        self.updated.len() + self.added.len() + self.removed.len()
    }

    /// What it removes, by name and architecture, sorted.
    #[must_use]
    pub fn removed_keys(&self) -> BTreeSet<String> {
        self.removed.iter().map(Affected::key).collect()
    }
}

/// Where an installation stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Checked, simulated, or about to start.
    Preparing,
    /// Another program holds the package manager.
    Waiting,
    /// polkit asks the user for the rights.
    Authorizing,
    Downloading,
    /// dpkg (or snapd) installs.
    Installing,
    Cancelling,
}

/// What an installation under way shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub phase: Phase,
    /// Of 100, when known.
    pub percentage: Option<u32>,
    /// The package at hand, by name.
    pub package: Option<String>,
    /// The packages dpkg has finished, of `total` it will handle (0: not
    /// known), never more.
    pub done: usize,
    pub total: usize,
    pub cancellable: bool,
    /// dpkg was said to run: what it did is on the disk, whatever comes.
    pub in_dpkg: bool,
    /// What is downloaded of what is to be, in bytes (snaps).
    pub bytes: Option<(u64, u64)>,
}

impl Progress {
    #[must_use]
    pub const fn preparing() -> Self {
        Self {
            phase: Phase::Preparing,
            percentage: None,
            package: None,
            done: 0,
            total: 0,
            cancellable: false,
            in_dpkg: false,
            bytes: None,
        }
    }
}

/// How an installation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Done,
    Cancelled,
    Failed(Why),
}

/// What an installation came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub end: End,
    /// dpkg was reached: what it did is on the disk, whatever the end.
    pub reached_dpkg: bool,
    /// PackageKit asked for the system to restart.
    pub system_restart: bool,
    /// What dpkg removed that was never shown.
    pub unshown_removals: Vec<String>,
    /// What it said of a package could not always be read: a removal never
    /// shown may have gone unseen.
    pub removals_unchecked: bool,
}

impl Ended {
    /// An end before anything was installed.
    #[must_use]
    pub const fn before(end: End) -> Self {
        Self {
            end,
            reached_dpkg: false,
            system_restart: false,
            unshown_removals: Vec::new(),
            removals_unchecked: false,
        }
    }
}

/// What asking to update the system came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemInstall {
    /// It would remove a package not accepted (yet): shown, nothing done.
    Confirm(Simulated),
    Ended(Ended),
}

/// What installs: the daemons of this system, or a test's.
pub trait Installer: Send + Sync {
    /// What updating the packages `ids` would do, simulated.
    ///
    /// # Errors
    /// Why PackageKit did not say.
    fn simulate_system(
        &self,
        ids: &[String],
        waiting: &dyn Fn(bool),
    ) -> Result<Vec<Package>, PackageKitError>;
    /// Updates the packages `ids`, telling `heard` how it goes, cancelled
    /// when `cancel` is set and it still can be.
    ///
    /// # Errors
    /// Why PackageKit did not, its refusal and a cancellation among them.
    fn update_system(
        &self,
        ids: &[String],
        heard: &dyn Fn(Updating),
        cancel: &AtomicBool,
    ) -> Result<Updated, PackageKitError>;
    /// Whether dpkg was cut short: updates left, no lock held.
    ///
    /// # Errors
    /// Why dpkg's files or the locks could not be read.
    fn dpkg_interrupted(&self) -> io::Result<bool>;
    /// Refreshes the snaps `names`: the change started.
    ///
    /// # Errors
    /// Why snapd did not.
    fn refresh_snaps(&self, names: &[String]) -> Result<String, SnapdError>;
    /// Follows the change `id`, telling `heard` where it stands.
    fn follow_snaps(&self, id: &str, heard: &dyn Fn(&Change)) -> Followed;
}

impl Installer for SystemSources {
    fn simulate_system(
        &self,
        ids: &[String],
        waiting: &dyn Fn(bool),
    ) -> Result<Vec<Package>, PackageKitError> {
        packagekit::PackageKit::connect(self.locale)?.simulate_update(ids, waiting)
    }

    fn update_system(
        &self,
        ids: &[String],
        heard: &dyn Fn(Updating),
        cancel: &AtomicBool,
    ) -> Result<Updated, PackageKitError> {
        packagekit::PackageKit::connect(self.locale)?.update_packages(ids, heard, cancel)
    }

    fn dpkg_interrupted(&self) -> io::Result<bool> {
        // The updates first, the locks then: a dpkg starting between the two
        // holds its lock when they are read (`interrupted` reads them again).
        if dpkg::pending_updates(Path::new(dpkg::DPKG_DIRECTORY))? == 0 {
            return Ok(false);
        }
        let locks = std::fs::read_to_string(dpkg::PROC_LOCKS)?;
        let lock_files = dpkg::LOCK_FILES.map(Path::new);
        dpkg::interrupted(Path::new(dpkg::DPKG_DIRECTORY), &lock_files, &locks)
    }

    fn refresh_snaps(&self, names: &[String]) -> Result<String, SnapdError> {
        snapd::Snapd::system().refresh(names)
    }

    fn follow_snaps(&self, id: &str, heard: &dyn Fn(&Change)) -> Followed {
        snapd::Snapd::system().follow(id, SNAPS_WAIT, SNAPS_LOOKS_FAILED, heard)
    }
}

/// Updates the system's packages `ids` (read by the caller): simulated, then
/// installed right after if it removes nothing but what is `accepted` (by
/// [`Affected::key`]: none at first; what was shown, once confirmed; less is
/// safer); otherwise what it would do is given back to be shown, nothing
/// installed. `heard` hears how it goes.
pub fn install_system(
    installer: &dyn Installer,
    ids: &[String],
    accepted: &BTreeSet<String>,
    heard: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> SystemInstall {
    let _install = tracing::info_span!("install_system", updates = ids.len()).entered();
    tracing::info!(?ids, "system update asked");
    let simulated = match simulate(installer, ids, heard) {
        Ok(simulated) => simulated,
        Err(why) => return SystemInstall::Ended(Ended::before(End::Failed(why))),
    };
    let removed = simulated.removed_keys();
    if !removed.is_subset(accepted) {
        if !accepted.is_empty() {
            tracing::warn!(
                ?accepted,
                ?removed,
                "what the update removes changed since it was shown: nothing installed"
            );
        }
        return SystemInstall::Confirm(simulated);
    }
    SystemInstall::Ended(update(installer, ids, &simulated, heard, cancel))
}

/// What updating `ids` would do, simulated and sorted.
fn simulate(
    installer: &dyn Installer,
    ids: &[String],
    heard: &dyn Fn(Progress),
) -> Result<Simulated, Why> {
    heard(Progress::preparing());
    let waiting = |waiting| {
        heard(Progress {
            phase: if waiting {
                Phase::Waiting
            } else {
                Phase::Preparing
            },
            ..Progress::preparing()
        });
    };
    let simulated = installer
        .simulate_system(ids, &waiting)
        .map_err(|error| why_of_packagekit(&error))
        .and_then(|packages| sort_simulation(&packages, ids))
        .inspect_err(|why| tracing::warn!(?why, "the update not simulated: nothing installed"))?;
    tracing::info!(
        updated = ?simulated.updated,
        added = ?simulated.added,
        removed = ?simulated.removed,
        "the update simulated"
    );
    Ok(simulated)
}

/// Installs `ids`, as `simulated` just before, following dpkg.
fn update(
    installer: &dyn Installer,
    ids: &[String],
    simulated: &Simulated,
    heard: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Ended {
    let tracker = RefCell::new(Tracker::new(simulated));
    let elapsed = Instant::now();
    let updated = installer.update_system(
        ids,
        &|updating| {
            let cancel_asked = cancel.load(std::sync::atomic::Ordering::Relaxed);
            let progress = tracker.borrow_mut().take(updating, cancel_asked);
            if let Some(progress) = progress {
                heard(progress);
            }
        },
        cancel,
    );
    let tracker = tracker.into_inner();
    let ended = Ended {
        end: match &updated {
            Ok(_) => End::Done,
            Err(error) if error.cancelled() => End::Cancelled,
            // Given up while polkit's window waited: cancelled, rights never
            // given.
            Err(PackageKitError::TimedOut { .. }) if tracker.status == status::WAITING_FOR_AUTH => {
                End::Failed(Why::Refused)
            }
            Err(error) => End::Failed(why_of_update(installer, error)),
        },
        reached_dpkg: tracker.in_dpkg,
        system_restart: updated.as_ref().is_ok_and(|updated| updated.system_restart),
        unshown_removals: tracker.unshown,
        removals_unchecked: tracker.unchecked,
    };
    tracing::info!(
        ?ended,
        last_status = tracker.status,
        finished = tracker.finished.len(),
        cancel_asked = cancel.load(std::sync::atomic::Ordering::Relaxed),
        elapsed = ?elapsed.elapsed(),
        "system update ended"
    );
    ended
}

/// Refreshes the snaps `names` (read by the caller) and follows the change
/// to its end; `heard` hears how it goes.
pub fn install_snaps(
    installer: &dyn Installer,
    names: &[String],
    heard: &dyn Fn(Progress),
) -> Ended {
    let _install = tracing::info_span!("install_snaps", snaps = ?names).entered();
    let elapsed = Instant::now();
    heard(Progress::preparing());
    let change = match installer.refresh_snaps(names) {
        Ok(change) => change,
        Err(error) => {
            tracing::warn!(%error, "snapd refused the refresh");
            return Ended::before(End::Failed(why_of_snapd(&error)));
        }
    };
    tracing::info!(change, "snapd refreshes the snaps");
    let end = match installer.follow_snaps(&change, &|state| heard(snap_progress(state))) {
        Followed::Ready(state) if state.status == CHANGE_DONE => End::Done,
        Followed::Ready(state) => {
            tracing::warn!(change, status = %state.status, err = ?state.err, "snapd's refresh did not end done");
            End::Failed(Why::Broken)
        }
        Followed::TimedOut => {
            tracing::warn!(change, wait = ?SNAPS_WAIT, "snapd's refresh followed no longer");
            End::Failed(Why::Unanswered)
        }
        Followed::Unanswered => {
            tracing::warn!(
                change,
                looks = SNAPS_LOOKS_FAILED,
                "snapd's refresh unreadable that many times in a row"
            );
            End::Failed(Why::Unanswered)
        }
    };
    tracing::info!(?end, elapsed = ?elapsed.elapsed(), "snaps refresh ended");
    Ended::before(end)
}

/// The updates of `asked` still waiting in `read`, by name and architecture
/// (a newer version come since is still the same update waiting; held back
/// is still waiting): their names as shown.
#[must_use]
pub fn system_left(asked: &[SystemUpdate], read: &[SystemUpdate]) -> Vec<String> {
    let key = |update: &SystemUpdate| {
        PackageId::parse(&update.id).map(|id| (id.name.to_owned(), id.arch.to_owned()))
    };
    let waiting: HashSet<(String, String)> = read.iter().filter_map(key).collect();
    // An identifier unreadable cannot be told gone: still waiting.
    asked
        .iter()
        .filter(|update| key(update).is_none_or(|key| waiting.contains(&key)))
        .map(|update| update.name.clone())
        .collect()
}

/// What a simulation says, sorted into what is updated, added and removed;
/// refused when it would downgrade, install unsigned packages, bring back an
/// update asked as a new package (removed since the list was read), or do
/// anything else (closed by default: what is not understood is not done).
fn sort_simulation(packages: &[Package], asked: &[String]) -> Result<Simulated, Why> {
    let asked: HashSet<(&str, &str)> = asked
        .iter()
        .filter_map(|id| PackageId::parse(id).map(|id| (id.name, id.arch)))
        .collect();
    let mut simulated = Simulated::default();
    for package in packages {
        let Some(id) = PackageId::parse(&package.id) else {
            tracing::warn!(id = %package.id, "a package simulated unreadable: nothing installed");
            return Err(Why::Broken);
        };
        let affected = || Affected {
            name: id.name.to_owned(),
            architecture: id.arch.to_owned(),
            summary: package.summary.clone(),
        };
        match package.info {
            info::UPDATING => simulated.updated.push(id.name.to_owned()),
            // An update asked, brought in as new or taken away: the list
            // changed since it was read.
            info::INSTALLING
            | info::INSTALL
            | info::REMOVING
            | info::REMOVE
            | info::OBSOLETING
            | info::OBSOLETE
                if asked.contains(&(id.name, id.arch)) =>
            {
                tracing::warn!(id = %package.id, info = package.info, "an update asked would be installed anew or removed: nothing installed");
                return Err(Why::Changed);
            }
            info::INSTALLING | info::INSTALL => simulated.added.push(affected()),
            info::REMOVING | info::REMOVE | info::OBSOLETING | info::OBSOLETE => {
                simulated.removed.push(affected());
            }
            info::UNTRUSTED => {
                tracing::warn!(id = %package.id, "a package not signed by a trusted key: nothing installed");
                return Err(Why::Unsigned);
            }
            other => {
                tracing::warn!(id = %package.id, info = other, "the update would do what is not shown (a downgrade...): nothing installed");
                return Err(Why::Broken);
            }
        }
    }
    Ok(simulated)
}

/// Folds what the update says into what is shown, and watches what dpkg
/// removes.
#[derive(Debug)]
struct Tracker {
    /// What was shown last.
    shown: Progress,
    percentage: Option<u32>,
    package: Option<String>,
    cancellable: bool,
    /// Its last status: waiting for another program or for the rights
    /// among them.
    status: u32,
    /// dpkg has started: what is said of a package is dpkg's.
    in_dpkg: bool,
    /// Removals shown and accepted, and what the update touches otherwise:
    /// a removal of these is no surprise. By name: dpkg says none other.
    expected_removals: HashSet<String>,
    /// What dpkg last said of each package, by name.
    last: HashMap<String, u32>,
    finished: HashSet<String>,
    total: usize,
    unshown: Vec<String>,
    /// A package said could not be read: a removal never shown may have
    /// gone unseen.
    unchecked: bool,
}

impl Tracker {
    fn new(simulated: &Simulated) -> Self {
        Self {
            shown: Progress::preparing(),
            percentage: None,
            package: None,
            cancellable: false,
            status: 0,
            in_dpkg: false,
            expected_removals: simulated
                .removed
                .iter()
                .chain(&simulated.added)
                .map(|affected| affected.name.clone())
                .chain(simulated.updated.iter().cloned())
                .collect(),
            last: HashMap::new(),
            finished: HashSet::new(),
            total: simulated.total(),
            unshown: Vec::new(),
            unchecked: false,
        }
    }

    /// Takes what the update said, `cancel_asked` or not; what is shown,
    /// when it changed.
    fn take(&mut self, updating: Updating, cancel_asked: bool) -> Option<Progress> {
        match updating {
            // Its status says it, and says when it is over.
            Updating::Waiting(_) => {}
            Updating::Cancellable(cancellable) => {
                // apt lets cancelling go right before dpkg; a cancel asked
                // lets it go too, taken or refused: then only dpkg's status
                // or packages tell it started.
                if self.cancellable
                    && !cancellable
                    && !cancel_asked
                    && self.status != status::CANCEL
                {
                    self.enter_dpkg();
                }
                self.cancellable = cancellable;
            }
            Updating::Status(said) => {
                self.status = said;
                if said == status::COMMIT {
                    self.enter_dpkg();
                }
            }
            Updating::Percentage(percentage) => {
                self.percentage =
                    (percentage < packagekit::PERCENTAGE_UNKNOWN).then_some(percentage);
            }
            Updating::Package { info, id } => self.package(info, &id),
            Updating::Unreadable => {
                tracing::warn!(
                    "a package of the update unreadable: what dpkg removes not all checked"
                );
                self.unchecked = true;
            }
        }
        let progress = Progress {
            phase: self.phase(),
            percentage: self.percentage,
            package: self.package.clone(),
            done: self.finished.len().min(self.total),
            total: self.total,
            cancellable: self.cancellable,
            in_dpkg: self.in_dpkg,
            bytes: None,
        };
        (progress != self.shown).then(|| {
            if progress.phase != self.shown.phase {
                tracing::debug!(phase = ?progress.phase, "the update's phase");
            }
            self.shown = progress.clone();
            progress
        })
    }

    fn package(&mut self, info: u32, id: &str) {
        let Some(name) = PackageId::parse(id).map(|id| id.name.to_owned()) else {
            tracing::warn!(
                id,
                "a package of the update unreadable: what dpkg removes not all checked"
            );
            self.unchecked = true;
            return;
        };
        let dpkg = matches!(
            info,
            info::PREPARING | info::DECOMPRESSING | info::INSTALLING | info::REMOVING
        );
        if dpkg {
            self.enter_dpkg();
        }
        if !self.in_dpkg {
            if info != info::FINISHED {
                self.package = Some(name);
            }
            return;
        }
        tracing::debug!(info, name, "dpkg");
        if info == info::REMOVING
            && !self.expected_removals.contains(&name)
            && !self.unshown.contains(&name)
        {
            tracing::error!(name, "dpkg removes a package never shown");
            self.unshown.push(name.clone());
        }
        if info == info::FINISHED {
            // Unpacking says a package finished too, when it moves on: only
            // its configuration or its removal ends it.
            if matches!(
                self.last.get(&name),
                Some(&(info::INSTALLING | info::REMOVING))
            ) {
                self.finished.insert(name);
            }
        } else {
            self.last.insert(name.clone(), info);
            self.package = Some(name);
        }
    }

    /// dpkg starts: the download said its packages finished too, dpkg
    /// counts anew.
    fn enter_dpkg(&mut self) {
        if !self.in_dpkg {
            self.in_dpkg = true;
            self.finished.clear();
            self.last.clear();
        }
    }

    fn phase(&self) -> Phase {
        if matches!(self.status, status::WAIT | status::WAITING_FOR_LOCK) {
            Phase::Waiting
        } else if self.status == status::WAITING_FOR_AUTH {
            Phase::Authorizing
        } else if self.status == status::CANCEL {
            Phase::Cancelling
        } else if self.in_dpkg {
            Phase::Installing
        } else if self.status == status::DOWNLOAD {
            Phase::Downloading
        } else {
            Phase::Preparing
        }
    }
}

/// What a snapd change shows: its downloads while some are to do or under
/// way, then its installation.
fn snap_progress(change: &Change) -> Progress {
    let downloads: Vec<&snapd::Task> = change
        .tasks
        .iter()
        .filter(|task| task.kind == snapd::TASK_DOWNLOAD)
        .collect();
    let downloading = downloads
        .iter()
        .any(|task| snapd::TASK_UNDERWAY.contains(&task.status.as_str()));
    let (done, total) = downloads.iter().fold((0, 0), |(done, total), task| {
        (done + task.progress.done, total + task.progress.total)
    });
    Progress {
        phase: if downloading {
            Phase::Downloading
        } else {
            Phase::Installing
        },
        percentage: (downloading && total > 0)
            .then(|| u32::try_from(done.min(total) * 100 / total).unwrap_or(100)),
        bytes: (downloading && total > 0).then_some((done, total)),
        ..Progress::preparing()
    }
}

/// Why, as the user is told, the update of the system did not happen: a
/// lock never taken is dpkg cut short when it left updates and nobody holds
/// its locks (a dpkg at work, however quiet, holds them).
fn why_of_update(installer: &dyn Installer, error: &PackageKitError) -> Why {
    if let PackageKitError::Failed {
        code: error_code::CANNOT_GET_LOCK,
        ..
    } = error
    {
        match installer.dpkg_interrupted() {
            Ok(true) => {
                tracing::warn!("dpkg was cut short: it must finish first");
                return Why::Interrupted;
            }
            Ok(false) => tracing::info!("a lock of dpkg or apt is held: another installation"),
            // Neither busy nor cut short can be said.
            Err(error) => {
                tracing::warn!(%error, kind = ?error.kind(), "whether dpkg was cut short unknown");
                return Why::Broken;
            }
        }
    }
    why_of_packagekit(error)
}

fn why_of_packagekit(error: &PackageKitError) -> Why {
    match error {
        error if error.busy() => Why::Busy,
        error if error.rights_refused() => Why::Refused,
        error if error.offline() => Why::Offline,
        PackageKitError::Failed { code, .. } => match *code {
            error_code::PACKAGE_DOWNLOAD_FAILED => Why::Offline,
            error_code::NO_SPACE_ON_DEVICE => Why::NoSpace,
            error_code::PACKAGE_NOT_FOUND | error_code::UPDATE_NOT_FOUND => Why::Changed,
            error_code::BAD_GPG_SIGNATURE
            | error_code::MISSING_GPG_SIGNATURE
            | error_code::CANNOT_INSTALL_REPO_UNSIGNED
            | error_code::CANNOT_UPDATE_REPO_UNSIGNED => Why::Unsigned,
            _ => Why::Broken,
        },
        PackageKitError::TimedOut { .. }
        | PackageKitError::Bus(_)
        | PackageKitError::Transport(_)
        | PackageKitError::Vanished
        | PackageKitError::Unreadable { .. } => Why::Unanswered,
        PackageKitError::Absent
        | PackageKitError::Ended { .. }
        | PackageKitError::NotOnlyRemoving { .. } => Why::Broken,
    }
}

fn why_of_snapd(error: &SnapdError) -> Why {
    if error.rights_refused() {
        Why::Refused
    } else if error.busy() {
        Why::Busy
    } else if error.apps_running() {
        Why::AppsOpen
    } else if error.offline() {
        Why::Offline
    } else if error.timed_out() {
        Why::Unanswered
    } else {
        match error {
            SnapdError::Io(_) | SnapdError::Truncated { .. } => Why::Unanswered,
            SnapdError::Absent { .. }
            | SnapdError::InvalidName(_)
            | SnapdError::TooLarge
            | SnapdError::Malformed(_)
            | SnapdError::Json(_)
            | SnapdError::Refused { .. } => Why::Broken,
        }
    }
}

#[cfg(test)]
mod tests;
