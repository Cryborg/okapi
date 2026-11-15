//! PackageKit, on the system bus: no command is run (invariant 4). Each
//! request is a transaction: created, given its hints, listened to before
//! it is asked (no signal is missed), then followed until it finishes or its
//! deadline passes. What a transaction says is turned into [`Signal`]s and
//! folded by a collector into its packages or its error, pure of any bus;
//! whether it waits for another program is told as soon as it changes, and
//! the wait is over when the transaction is.
//!
//! The values of the protocol's enums are those of `pk-enum.h` (1.3.6);
//! flags and filters are bit fields, `1 << value`.

use std::collections::HashMap;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_lite::{Stream, StreamExt as _};
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedValue;

use super::bus;

const DESTINATION: &str = "org.freedesktop.PackageKit";
const OBJECT: &str = "/org/freedesktop/PackageKit";
const INTERFACE: &str = "org.freedesktop.PackageKit";
const TRANSACTION: &str = "org.freedesktop.PackageKit.Transaction";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const PROPERTIES_CHANGED: &str = "PropertiesChanged";
const STATUS: &str = "Status";
const PERCENTAGE: &str = "Percentage";
const PACKAGE: &str = "Package";
const PACKAGES: &str = "Packages";
const DETAILS: &str = "Details";
const REQUIRE_RESTART: &str = "RequireRestart";
/// The key of a package's details that names it, and its download size.
const DETAILS_ID: &str = "package-id";
const DETAILS_DOWNLOAD_SIZE: &str = "download-size";
/// The signals that end a transaction or say why it failed.
const FINISHED: &str = "Finished";
const ERROR_CODE: &str = "ErrorCode";
/// How long the updates are listed at most: from the cache, seconds.
const UPDATES_WAIT: Duration = Duration::from_mins(2);
/// How long the package lists are downloaded at most: a slow network.
const REFRESH_WAIT: Duration = Duration::from_mins(5);
/// How long a package is looked for, or a removal simulated, at most: from
/// the cache.
const RESOLVE_WAIT: Duration = Duration::from_mins(2);
/// How long a removal is followed at most: polkit's window waits on the
/// user, then the package manager works. Beyond it, said timed out and
/// cancelled if it still can be (it waits for the rights or for another
/// program: nothing then runs behind the screen); dpkg already writing
/// cannot be stopped, so it may still end.
const REMOVE_WAIT: Duration = Duration::from_mins(30);
/// How long the system's update is followed at most: the rights, a large
/// download on a slow network, then dpkg. Beyond it, said timed out and
/// cancelled if it still can be, as a removal ([`REMOVE_WAIT`]).
const UPDATE_WAIT: Duration = Duration::from_hours(2);
/// How often a removal or an update is asked whether it may still be
/// cancelled, and cancelled once asked to and allowed.
const CANCEL_LOOK_EVERY: Duration = Duration::from_millis(300);
const ALLOW_CANCEL: &str = "AllowCancel";

/// The hints of a transaction that reads: never ask the user anything; and
/// of every one: send packages several at once.
const HINT_NOT_INTERACTIVE: &str = "interactive=false";
/// The hint of a removal or an update: polkit may ask the user for the
/// rights. No `frontend-socket` hint is ever given: without it dpkg speaks
/// C.UTF-8, which the backend reads to say each package's progress.
const HINT_INTERACTIVE: &str = "interactive=true";
const HINT_PLURAL_SIGNALS: &str = "supports-plural-signals=true";
const HINT_LOCALE: &str = "locale=";
/// `PkFilterEnum::None`, as a bit field: every package.
const FILTER_NONE: u64 = 1 << 1;
/// `PkFilterEnum::Installed`.
const FILTER_INSTALLED: u64 = 1 << 2;
/// `PkTransactionFlagEnum`: none, or a simulation that changes nothing;
/// only packages signed by a trusted key.
const TRANSACTION_NONE: u64 = 0;
const TRANSACTION_SIMULATE: u64 = 1 << 2;
const TRANSACTION_ONLY_TRUSTED: u64 = 1 << 1;
/// A simulated removal lists what depends on what it removes (`allow_deps`);
/// the removal made is given all of them and allows none more: what came to
/// depend on it since it was shown fails it, never goes unseen. Never what
/// is no longer needed (`autoremove`).
const SIMULATION_ALLOWS_DEPENDENTS: bool = true;
const REMOVAL_ALLOWS_DEPENDENTS: bool = false;
const AUTOREMOVE: bool = false;
/// `RefreshCache` only downloads lists that changed.
const REFRESH_UNFORCED: bool = false;
/// `PkExitEnum::Success`, and `Cancelled`.
const EXIT_SUCCESS: u32 = 1;
pub const EXIT_CANCELLED: u32 = 3;
/// `PkRestartEnum`: the system must restart.
const RESTART_SYSTEM: u32 = 4;
const RESTART_SECURITY_SYSTEM: u32 = 6;
/// What a percentage says when it is not known.
pub const PERCENTAGE_UNKNOWN: u32 = 101;

/// Values of `PkStatusEnum`.
pub mod status {
    /// Queued behind another transaction, or waiting for another program
    /// to let go of the package manager.
    pub const WAIT: u32 = 1;
    pub const DOWNLOAD: u32 = 8;
    /// dpkg runs.
    pub const COMMIT: u32 = 16;
    pub const CANCEL: u32 = 19;
    pub const WAITING_FOR_LOCK: u32 = 30;
    /// Waiting for polkit to give the rights (its window is shown).
    pub const WAITING_FOR_AUTH: u32 = 31;
}

/// The bits of a package's value that say its kind: the upper ones say an
/// update's severity.
const INFO_BITS: u32 = 0xFFFF;

/// What `PkInfoEnum` says of an update.
pub mod info {
    pub const INSTALLED: u32 = 1;
    /// The kinds of an update, from the least to security.
    pub const LOW: u32 = 3;
    pub const SECURITY: u32 = 8;
    /// Held back by the system: it will not be installed.
    pub const BLOCKED: u32 = 9;
    /// What `GetUpdates` lists beside the updates (apt), the effects of
    /// upgrading everything, never updates themselves; and what a simulation
    /// intends (PackageKit 1.3).
    pub const INSTALL: u32 = 27;
    pub const OBSOLETE: u32 = 29;
    pub const DOWNGRADE: u32 = 30;
    /// What a removal takes away: under way, or intended (PackageKit 1.3).
    pub const REMOVING: u32 = 13;
    pub const REMOVE: u32 = 28;
    /// What a simulated update says (apt): a package updated, brought in,
    /// made obsolete, downgraded, or not signed by a trusted key.
    pub const UPDATING: u32 = 11;
    pub const INSTALLING: u32 = 12;
    pub const OBSOLETING: u32 = 15;
    pub const DOWNGRADING: u32 = 20;
    pub const UNTRUSTED: u32 = 23;
    /// What dpkg says of a package as it goes (apt): done, being prepared,
    /// being unpacked.
    pub const FINISHED: u32 = 18;
    pub const PREPARING: u32 = 21;
    pub const DECOMPRESSING: u32 = 22;
}

/// Values of `PkErrorEnum`.
pub mod error_code {
    pub const NO_NETWORK: u32 = 2;
    pub const PACKAGE_NOT_INSTALLED: u32 = 7;
    /// The identifier asked is no longer an update (the lists changed).
    pub const PACKAGE_NOT_FOUND: u32 = 8;
    /// A package could not be downloaded (or, rarely, prepared).
    pub const PACKAGE_DOWNLOAD_FAILED: u32 = 10;
    pub const TRANSACTION_CANCELLED: u32 = 17;
    pub const CANNOT_GET_LOCK: u32 = 26;
    /// A package or a repository not signed by a trusted key.
    pub const BAD_GPG_SIGNATURE: u32 = 30;
    pub const MISSING_GPG_SIGNATURE: u32 = 31;
    pub const REPO_NOT_AVAILABLE: u32 = 37;
    pub const NO_MORE_MIRRORS_TO_TRY: u32 = 43;
    pub const NO_SPACE_ON_DEVICE: u32 = 46;
    /// polkit refused (a session that is not active, no agent).
    pub const NOT_AUTHORIZED: u32 = 48;
    pub const UPDATE_NOT_FOUND: u32 = 49;
    pub const CANNOT_INSTALL_REPO_UNSIGNED: u32 = 50;
    pub const CANNOT_UPDATE_REPO_UNSIGNED: u32 = 51;
    pub const CANNOT_FETCH_SOURCES: u32 = 64;
    pub const LOCK_REQUIRED: u32 = 67;
}

/// A package as a transaction names it, unfiltered: its kind (`PkInfoEnum`),
/// its identifier (`name;version;arch;data`) and its one-line summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub info: u32,
    pub id: String,
    pub summary: String,
}

/// What a package's details say: its download size, when the backend
/// gives it (`download-size`, since PackageKit 1.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Details {
    pub id: String,
    pub download_size: Option<u64>,
}

/// The parts of a package's identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageId<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub arch: &'a str,
    /// The repository, or `installed`, as the backend says.
    pub data: &'a str,
}

impl<'a> PackageId<'a> {
    /// The parts of `id`, if it has the four, a name among them.
    #[must_use]
    pub fn parse(id: &'a str) -> Option<Self> {
        let mut parts = id.split(';');
        let parsed = Self {
            name: parts.next()?,
            version: parts.next()?,
            arch: parts.next()?,
            data: parts.next()?,
        };
        (parts.next().is_none() && !parsed.name.is_empty()).then_some(parsed)
    }
}

/// Why a transaction gave no answer that could be used.
#[derive(Debug, thiserror::Error)]
pub enum PackageKitError {
    /// The system bus is not reached by a socket.
    #[error("the system bus is not reached by a socket: {0}")]
    Transport(String),
    /// Nobody provides PackageKit on the system bus.
    #[error("PackageKit is not on the system bus")]
    Absent,
    #[error(transparent)]
    Bus(#[from] zbus::Error),
    /// PackageKit said why it failed.
    #[error("PackageKit failed (error {code}): {details}")]
    Failed { code: u32, details: String },
    /// It ended without success and without saying why.
    #[error("PackageKit ended with exit {exit}, without an error")]
    Ended { exit: u32 },
    /// It went away before it finished.
    #[error("the transaction was destroyed before it finished")]
    Vanished,
    /// It said something that could not be read: what it came to is not
    /// known whole.
    #[error("a PackageKit signal could not be read: {member}")]
    Unreadable { member: String },
    /// A removal simulated that would also install or change packages.
    #[error("the removal would not only remove: {packages:?}")]
    NotOnlyRemoving { packages: Vec<String> },
    #[error("no end after {waited:?} (waiting for another program: {waiting})")]
    TimedOut {
        waited: Duration,
        /// It was still waiting for another program when the time ran out.
        waiting: bool,
    },
}

impl PackageKitError {
    /// Whether another program held the package manager: it said so, or
    /// the time ran out while it still waited.
    #[must_use]
    pub const fn busy(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                code: error_code::CANNOT_GET_LOCK | error_code::LOCK_REQUIRED,
                ..
            } | Self::TimedOut { waiting: true, .. }
        )
    }

    /// Whether polkit refused the rights.
    #[must_use]
    pub const fn rights_refused(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                code: error_code::NOT_AUTHORIZED,
                ..
            }
        )
    }

    /// Whether it was cancelled, as asked.
    #[must_use]
    pub const fn cancelled(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                code: error_code::TRANSACTION_CANCELLED,
                ..
            } | Self::Ended {
                exit: EXIT_CANCELLED
            }
        )
    }

    /// Whether the network or the repositories could not be reached.
    #[must_use]
    pub const fn offline(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                code: error_code::NO_NETWORK
                    | error_code::REPO_NOT_AVAILABLE
                    | error_code::NO_MORE_MIRRORS_TO_TRY
                    | error_code::CANNOT_FETCH_SOURCES,
                ..
            }
        )
    }
}

/// What a transaction says, as [`Collector`] folds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Signal {
    Package(Package),
    /// Several at once (`supports-plural-signals`).
    Packages(Vec<Package>),
    ErrorCode {
        code: u32,
        details: String,
    },
    Finished {
        exit: u32,
    },
    Destroy,
    /// Its properties changed: its status (`PkStatusEnum`), its percentage
    /// ([`PERCENTAGE_UNKNOWN`] when not known), each when said (they come
    /// apart).
    Properties {
        status: Option<u32>,
        percentage: Option<u32>,
    },
    Details(Details),
    /// It asks for a restart (`PkRestartEnum`).
    RequireRestart {
        kind: u32,
    },
    /// A signal of the transaction whose body could not be read.
    Unreadable {
        member: String,
    },
}

/// What a transaction that ended well said.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Said {
    pub(crate) packages: Vec<Package>,
    pub(crate) details: Vec<Details>,
    /// It asked for the system to restart.
    pub(crate) system_restart: bool,
}

/// What a transaction has said so far: its packages, details, restart,
/// error and status.
#[derive(Debug, Default)]
pub(crate) struct Collector {
    said: Said,
    error: Option<(u32, String)>,
    /// A package said that could not be read: the list is not whole.
    unreadable: Option<String>,
    status: u32,
    /// Its packages only tell its progress (an update): one lost is told to
    /// whoever follows it and traced, never its failure.
    progress_only: bool,
}

impl Collector {
    /// A collector whose packages only tell the progress.
    pub(crate) fn of_progress() -> Self {
        Self {
            progress_only: true,
            ..Self::default()
        }
    }

    /// Takes `signal`; once the transaction has ended, what it came to.
    pub(crate) fn take(&mut self, signal: Signal) -> Option<Result<Said, PackageKitError>> {
        match signal {
            Signal::Package(package) => self.said.packages.push(package),
            Signal::Packages(packages) => self.said.packages.extend(packages),
            Signal::Details(details) => self.said.details.push(details),
            Signal::RequireRestart { kind } => {
                if matches!(kind, RESTART_SYSTEM | RESTART_SECURITY_SYSTEM) {
                    self.said.system_restart = true;
                } else {
                    tracing::info!(kind, "a restart asked, not of the system");
                }
            }
            Signal::ErrorCode { code, details } => {
                if self.error.is_some() {
                    // The first error is the cause; later ones follow from it.
                    tracing::debug!(code, %details, "a later PackageKit error");
                } else {
                    self.error = Some((code, details));
                }
            }
            Signal::Properties { status, .. } => {
                if let Some(status) = status {
                    self.status = status;
                }
            }
            // Its end or its error unreadable: it is over, its outcome unknown
            // unless an error read before says why.
            Signal::Unreadable { member } if member == FINISHED || member == ERROR_CODE => {
                return Some(Err(match self.error.take() {
                    Some((code, details)) => PackageKitError::Failed { code, details },
                    None => PackageKitError::Unreadable { member },
                }));
            }
            Signal::Unreadable { member } if self.progress_only => {
                tracing::warn!(member, "a package of the progress unreadable: skipped");
            }
            Signal::Unreadable { member } => {
                self.unreadable.get_or_insert(member);
            }
            Signal::Finished { exit } => {
                return Some(match (self.error.take(), self.unreadable.take()) {
                    (Some((code, details)), _) => Err(PackageKitError::Failed { code, details }),
                    (None, Some(member)) => Err(PackageKitError::Unreadable { member }),
                    (None, None) if exit == EXIT_SUCCESS => Ok(std::mem::take(&mut self.said)),
                    (None, None) => Err(PackageKitError::Ended { exit }),
                });
            }
            Signal::Destroy => return Some(Err(PackageKitError::Vanished)),
        }
        None
    }

    /// Whether its last status says it waits for another program.
    #[must_use]
    pub(crate) const fn waiting(&self) -> bool {
        matches!(self.status, status::WAIT | status::WAITING_FOR_LOCK)
    }
}

/// Follows `signals` into `collector` until the transaction ends or `wait`
/// passes; tells `heard` each signal, and `waiting` each time it starts or
/// stops waiting for another program, and that it no longer waits once it
/// is over.
async fn follow<S>(
    signals: S,
    wait: Duration,
    mut collector: Collector,
    waiting: &dyn Fn(bool),
    heard: &dyn Fn(&Signal),
) -> Result<Said, PackageKitError>
where
    S: Stream<Item = Signal>,
{
    let mut signals = pin!(signals);
    let followed = async {
        while let Some(signal) = signals.next().await {
            heard(&signal);
            let was_waiting = collector.waiting();
            let ended = collector.take(signal);
            if collector.waiting() != was_waiting {
                waiting(collector.waiting());
            }
            if ended.is_some() {
                return ended;
            }
        }
        Some(Err(PackageKitError::Vanished))
    };
    let deadline = async {
        async_io::Timer::after(wait).await;
        None
    };
    let ended = futures_lite::future::or(followed, deadline).await;
    let still_waiting = collector.waiting();
    if still_waiting {
        waiting(false);
    }
    ended.unwrap_or(Err(PackageKitError::TimedOut {
        waited: wait,
        waiting: still_waiting,
    }))
}

/// What a removal under way says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removing {
    /// It starts or stops waiting for another program.
    Waiting(bool),
    /// It may be cancelled, or no longer.
    Cancellable(bool),
}

/// What the system's update under way says, as PackageKit says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Updating {
    /// It starts or stops waiting for another program.
    Waiting(bool),
    /// It may be cancelled, or no longer.
    Cancellable(bool),
    /// Its status (`PkStatusEnum`, [`status`]).
    Status(u32),
    /// How far it is, of 100; [`PERCENTAGE_UNKNOWN`] when not known.
    Percentage(u32),
    /// A package as it goes: its kind ([`info`]) and its identifier.
    Package { info: u32, id: String },
    /// What it said of a package could not be read: what dpkg did is not
    /// known whole.
    Unreadable,
}

/// What the update under way says, of what its transaction said.
fn updating_of(signal: &Signal) -> Vec<Updating> {
    let package = |package: &Package| Updating::Package {
        info: package.info,
        id: package.id.clone(),
    };
    match signal {
        Signal::Properties { status, percentage } => status
            .map(Updating::Status)
            .into_iter()
            .chain(percentage.map(Updating::Percentage))
            .collect(),
        Signal::Package(said) => vec![package(said)],
        Signal::Packages(said) => said.iter().map(package).collect(),
        Signal::Unreadable { member } if member == PACKAGE || member == PACKAGES => {
            vec![Updating::Unreadable]
        }
        Signal::ErrorCode { .. }
        | Signal::Finished { .. }
        | Signal::Destroy
        | Signal::Details(_)
        | Signal::RequireRestart { .. }
        | Signal::Unreadable { .. } => Vec::new(),
    }
}

/// What the system's update came to, once done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Updated {
    /// PackageKit asked for the system to restart.
    pub system_restart: bool,
}

/// PackageKit, joined on the system bus.
#[derive(Debug, Clone)]
pub struct PackageKit {
    connection: zbus::Connection,
    locale: String,
}

impl PackageKit {
    /// Joins the system bus; the transactions ask for texts in `locale`
    /// (`fr_FR.UTF-8`), and only a removal or an update for the user's
    /// interaction.
    ///
    /// # Errors
    /// A bus that is not a socket, or that cannot be joined.
    pub fn connect(locale: &str) -> Result<Self, PackageKitError> {
        let address =
            bus::socket_only(zbus::Address::system()?).map_err(PackageKitError::Transport)?;
        let connection = futures_lite::future::block_on(
            zbus::connection::Builder::address(address)?
                // A transaction's end is waited for apart, on its signals.
                .method_timeout(bus::CALL_WAIT)
                .build(),
        )?;
        Ok(Self {
            connection,
            locale: locale.to_owned(),
        })
    }

    /// The hints of a transaction: the locale, whether the user may be
    /// asked, several packages at once.
    fn hints(&self, interactive: bool) -> Vec<String> {
        vec![
            format!("{HINT_LOCALE}{}", self.locale),
            if interactive {
                HINT_INTERACTIVE
            } else {
                HINT_NOT_INTERACTIVE
            }
            .to_owned(),
            HINT_PLURAL_SIGNALS.to_owned(),
        ]
    }

    /// The installed packages named `name`, of every architecture.
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, or its own error.
    pub fn resolve(&self, name: &str) -> Result<Vec<Package>, PackageKitError> {
        self.run(
            "Resolve",
            &(FILTER_INSTALLED, [name]),
            Asked::reading(RESOLVE_WAIT),
            &|_| {},
        )
        .map(|said| said.packages)
    }

    /// What removing the packages `ids` would take away (them, and what
    /// depends on them), simulated: nothing changes, nothing is asked.
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, its own error, or a removal
    /// that would also install or change packages.
    pub fn simulate_remove(&self, ids: &[String]) -> Result<Vec<Package>, PackageKitError> {
        let packages = self
            .run(
                "RemovePackages",
                &(
                    TRANSACTION_SIMULATE,
                    ids,
                    SIMULATION_ALLOWS_DEPENDENTS,
                    AUTOREMOVE,
                ),
                Asked::reading(RESOLVE_WAIT),
                &|_| {},
            )?
            .packages;
        only_removing(packages)
    }

    /// Removes the packages `ids` and what depends on them, polkit asking
    /// the user for the rights; `steps` hears whether it waits for another
    /// program and whether it may still be cancelled; it is cancelled once
    /// `cancel` is set and it still can be.
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, or its own error (the user's
    /// refusal and a cancellation among them).
    pub fn remove_packages(
        &self,
        ids: &[String],
        steps: &dyn Fn(Removing),
        cancel: &AtomicBool,
    ) -> Result<(), PackageKitError> {
        let cancellable = |can| steps(Removing::Cancellable(can));
        self.run(
            "RemovePackages",
            &(TRANSACTION_NONE, ids, REMOVAL_ALLOWS_DEPENDENTS, AUTOREMOVE),
            Asked {
                wait: REMOVE_WAIT,
                interactive: true,
                cancel: Some((cancel, &cancellable)),
                heard: None,
            },
            &|waiting| steps(Removing::Waiting(waiting)),
        )
        .map(drop)
    }

    /// What updating the packages `ids` would do, simulated: nothing
    /// changes, nothing is asked. Each package comes with what would become
    /// of it (`info::UPDATING`, `INSTALLING`, `REMOVING`...); none for no
    /// identifier.
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, or its own error.
    pub fn simulate_update(
        &self,
        ids: &[String],
        waiting: &dyn Fn(bool),
    ) -> Result<Vec<Package>, PackageKitError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let call = update_call(true);
        self.run(
            "UpdatePackages",
            &(call.flags, ids),
            Asked {
                wait: call.wait,
                interactive: call.interactive,
                cancel: None,
                heard: None,
            },
            waiting,
        )
        .map(|said| said.packages)
    }

    /// Updates the packages `ids` (signed by a trusted key only), polkit
    /// asking the user for the rights; `heard` hears how it goes; it is
    /// cancelled once `cancel` is set and it still can be. Nothing is asked
    /// for no identifier.
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, or its own error (the user's
    /// refusal and a cancellation among them).
    pub fn update_packages(
        &self,
        ids: &[String],
        heard: &dyn Fn(Updating),
        cancel: &AtomicBool,
    ) -> Result<Updated, PackageKitError> {
        if ids.is_empty() {
            return Ok(Updated {
                system_restart: false,
            });
        }
        let cancellable = |can| heard(Updating::Cancellable(can));
        let progress = |signal: &Signal| {
            for updating in updating_of(signal) {
                heard(updating);
            }
        };
        let call = update_call(false);
        self.run(
            "UpdatePackages",
            &(call.flags, ids),
            Asked {
                wait: call.wait,
                interactive: call.interactive,
                cancel: Some((cancel, &cancellable)),
                heard: Some(&progress),
            },
            &|waiting| heard(Updating::Waiting(waiting)),
        )
        .map(|said| Updated {
            system_restart: said.system_restart,
        })
    }

    /// The details of the packages `ids`, their download size among them;
    /// none for no identifier (the bus refuses an empty list).
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, or its own error.
    pub fn details(
        &self,
        ids: &[String],
        waiting: &dyn Fn(bool),
    ) -> Result<Vec<Details>, PackageKitError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.run("GetDetails", &(ids,), Asked::reading(RESOLVE_WAIT), waiting)
            .map(|said| said.details)
    }

    /// The updates waiting, as the package cache knows them: `GetUpdates`
    /// downloads nothing, [`Self::refresh_cache`] does; `waiting` hears when
    /// another program holds the package manager.
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, or its own error.
    pub fn get_updates(&self, waiting: &dyn Fn(bool)) -> Result<Vec<Package>, PackageKitError> {
        self.run(
            "GetUpdates",
            &(FILTER_NONE,),
            Asked::reading(UPDATES_WAIT),
            waiting,
        )
        .map(|said| said.packages)
    }

    /// Downloads the package lists that changed.
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, or its own error.
    pub fn refresh_cache(&self, waiting: &dyn Fn(bool)) -> Result<(), PackageKitError> {
        self.run(
            "RefreshCache",
            &(REFRESH_UNFORCED,),
            Asked::reading(REFRESH_WAIT),
            waiting,
        )
        .map(drop)
    }

    /// Runs `method` with `body` in a new transaction, as `asked`, until it
    /// ends or its wait passes (then it is cancelled).
    fn run<B>(
        &self,
        method: &str,
        body: &B,
        asked: Asked<'_>,
        waiting: &dyn Fn(bool),
    ) -> Result<Said, PackageKitError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let _transaction = tracing::info_span!("packagekit", method).entered();
        let started = Instant::now();
        let ended = futures_lite::future::block_on(self.transaction(method, body, &asked, waiting));
        match &ended {
            Ok(said) => tracing::debug!(
                packages = said.packages.len(),
                details = said.details.len(),
                system_restart = said.system_restart,
                elapsed = ?started.elapsed(),
                "PackageKit transaction done"
            ),
            // No PackageKit is no failure: the caller says so.
            Err(PackageKitError::Absent) => tracing::debug!("no PackageKit on the system bus"),
            Err(error) if error.cancelled() => tracing::info!(
                %error,
                elapsed = ?started.elapsed(),
                "PackageKit transaction cancelled"
            ),
            Err(error) => tracing::warn!(
                %error,
                elapsed = ?started.elapsed(),
                "PackageKit transaction failed"
            ),
        }
        ended
    }

    async fn transaction<B>(
        &self,
        method: &str,
        body: &B,
        asked: &Asked<'_>,
        waiting: &dyn Fn(bool),
    ) -> Result<Said, PackageKitError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let daemon = zbus::Proxy::new(&self.connection, DESTINATION, OBJECT, INTERFACE).await?;
        let path: zbus::zvariant::OwnedObjectPath = daemon
            .call("CreateTransaction", &())
            .await
            .map_err(absent_or_bus)?;
        let transaction = self.proxy(&path, TRANSACTION).await?;
        let properties = self.proxy(&path, PROPERTIES).await?;
        // Listened to before it is asked: none of its signals is missed.
        let said = transaction.receive_all_signals().await?;
        let changed = properties.receive_signal(PROPERTIES_CHANGED).await?;
        transaction
            .call_method("SetHints", &(self.hints(asked.interactive).as_slice(),))
            .await?;
        transaction.call_method(method, body).await?;
        let signals =
            futures_lite::stream::or(said, changed).filter_map(|message| signal_of(&message));
        let collector = if asked.heard.is_some() {
            Collector::of_progress()
        } else {
            Collector::default()
        };
        let followed = follow(
            signals,
            asked.wait,
            collector,
            waiting,
            asked.heard.unwrap_or(&|_| {}),
        );
        let ended = match asked.cancel {
            None => followed.await,
            Some((cancel, cancellable)) => {
                let watched = watch_cancel(&transaction, &properties, cancel, cancellable);
                futures_lite::future::or(followed, watched).await
            }
        };
        if given_up(&ended) {
            match transaction.call_method("Cancel", &()).await {
                Ok(_) => tracing::debug!(%path, "the transaction given up cancelled"),
                Err(error) => {
                    tracing::warn!(%path, %error, "the transaction given up was not cancelled");
                }
            }
        }
        ended
    }

    async fn proxy(
        &self,
        path: &zbus::zvariant::OwnedObjectPath,
        interface: &'static str,
    ) -> Result<zbus::Proxy<'static>, zbus::Error> {
        zbus::proxy::Builder::new(&self.connection)
            .destination(DESTINATION)?
            .path(path.clone())?
            .interface(interface)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }
}

/// How a transaction is asked: how long it may take, whether the user may
/// be asked for the rights; for one that may be cancelled, the flag that
/// asks it to and who hears whether it still can be; for one whose packages
/// tell its progress (never its outcome), who hears each signal.
#[derive(Clone, Copy)]
struct Asked<'a> {
    wait: Duration,
    interactive: bool,
    cancel: Option<(&'a AtomicBool, &'a dyn Fn(bool))>,
    heard: Option<&'a dyn Fn(&Signal)>,
}

impl Asked<'_> {
    /// A transaction that reads: nothing asked, nothing cancelled.
    const fn reading(wait: Duration) -> Self {
        Self {
            wait,
            interactive: false,
            cancel: None,
            heard: None,
        }
    }
}

/// Looks every so often whether `transaction` may be cancelled, tells
/// `cancellable` when that changes, and cancels it once `cancel` is set and
/// it can be; never ends by itself (the transaction's end does).
async fn watch_cancel(
    transaction: &zbus::Proxy<'_>,
    properties: &zbus::Proxy<'_>,
    cancel: &AtomicBool,
    cancellable: &dyn Fn(bool),
) -> Result<Said, PackageKitError> {
    let mut allowed = false;
    let mut asked = false;
    let mut unreadable_told = false;
    loop {
        async_io::Timer::after(CANCEL_LOOK_EVERY).await;
        let read = properties
            .call::<_, _, OwnedValue>("Get", &(TRANSACTION, ALLOW_CANCEL))
            .await
            .map_err(|error| error.to_string())
            .and_then(|value| bool::try_from(value).map_err(|error| error.to_string()));
        let now = match read {
            Ok(now) => now,
            Err(error) => {
                // Told once, not every look: what it was stays meanwhile.
                if !unreadable_told {
                    unreadable_told = true;
                    tracing::warn!(%error, allowed, "whether the transaction may be cancelled unreadable");
                }
                allowed
            }
        };
        if now != allowed {
            allowed = now;
            cancellable(allowed);
        }
        if allowed && !asked && cancel.load(Ordering::Relaxed) {
            asked = true;
            match transaction.call_method("Cancel", &()).await {
                Ok(_) => tracing::info!("cancel asked of PackageKit"),
                Err(error) => {
                    // Asked once: the screen, seeing no cancelling, says it
                    // came too late.
                    tracing::warn!(%error, "cancel refused by PackageKit");
                    cancellable(false);
                }
            }
        }
    }
}

/// How an update is asked: its flags, how long it may take, whether polkit
/// may ask the user for the rights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UpdateCall {
    flags: u64,
    wait: Duration,
    interactive: bool,
}

/// How an update is asked: only packages signed by a trusted key; a
/// simulation changes nothing, from the cache, asking nobody; the real one
/// may ask for the rights and take long.
const fn update_call(simulated: bool) -> UpdateCall {
    if simulated {
        UpdateCall {
            flags: TRANSACTION_ONLY_TRUSTED | TRANSACTION_SIMULATE,
            wait: RESOLVE_WAIT,
            interactive: false,
        }
    } else {
        UpdateCall {
            flags: TRANSACTION_ONLY_TRUSTED,
            wait: UPDATE_WAIT,
            interactive: true,
        }
    }
}

/// What a simulated removal takes away, when it does nothing else: a
/// package it would install or change is never left unseen.
fn only_removing(packages: Vec<Package>) -> Result<Vec<Package>, PackageKitError> {
    let (removing, other): (Vec<Package>, Vec<Package>) = packages
        .into_iter()
        .partition(|package| matches!(package.info, info::REMOVING | info::REMOVE));
    if other.is_empty() {
        return Ok(removing);
    }
    let packages: Vec<String> = other
        .iter()
        .map(|package| format!("{} ({})", package.id, package.info))
        .collect();
    tracing::warn!(?packages, "a removal simulated would not only remove");
    Err(PackageKitError::NotOnlyRemoving { packages })
}

/// Whether the transaction was left while it may still run: it would hold
/// the next one, it is cancelled. Its end seen, there is nothing to cancel.
fn given_up(ended: &Result<Said, PackageKitError>) -> bool {
    match ended {
        Err(PackageKitError::TimedOut { .. }) => true,
        Err(PackageKitError::Unreadable { member }) => member == FINISHED || member == ERROR_CODE,
        _ => false,
    }
}

/// A missing service is no PackageKit; anything else, the bus (an
/// interface or a method unknown is a PackageKit that changed, never one
/// absent).
fn absent_or_bus(error: zbus::Error) -> PackageKitError {
    match &error {
        zbus::Error::MethodError(name, ..) if bus::UNKNOWN_SERVICE.contains(&name.as_str()) => {
            PackageKitError::Absent
        }
        _ => PackageKitError::Bus(error),
    }
}

/// A package as the bus says it. Its kind shares its value with the
/// update's severity, in the upper half (`info | severity << 16`, as
/// `pk-client.c` reads it back): apt says a security update `0x80008`.
fn package_of((info, id, summary): (u32, String, String)) -> Package {
    Package {
        info: info & INFO_BITS,
        id,
        summary,
    }
}

/// What `message` says, if it is a signal folded. A package, an error or an
/// end that cannot be read is [`Signal::Unreadable`]: the transaction can
/// no longer be told whole. A status that cannot be read is only skipped,
/// traced: it tells a wait, never an outcome.
fn signal_of(message: &zbus::Message) -> Option<Signal> {
    let header = message.header();
    let member = header.member()?.as_str();
    let body = message.body();
    let read = match member {
        PACKAGE => body
            .deserialize::<(u32, String, String)>()
            .map(|said| Signal::Package(package_of(said))),
        PACKAGES => body
            .deserialize::<Vec<(u32, String, String)>>()
            .map(|packages| Signal::Packages(packages.into_iter().map(package_of).collect())),
        ERROR_CODE => body
            .deserialize::<(u32, String)>()
            .map(|(code, details)| Signal::ErrorCode { code, details }),
        FINISHED => body
            .deserialize::<(u32, u32)>()
            .map(|(exit, _runtime)| Signal::Finished { exit }),
        "Destroy" => return Some(Signal::Destroy),
        // A restart or details lost tell less, never a failure: skipped,
        // traced (the restart awaited is read from its file all the same).
        REQUIRE_RESTART => {
            return body
                .deserialize::<(u32, String)>()
                .inspect_err(
                    |error| tracing::warn!(member, %error, "a PackageKit signal unreadable"),
                )
                .ok()
                .map(|(kind, id)| {
                    tracing::info!(kind, id, "PackageKit asks for a restart");
                    Signal::RequireRestart { kind }
                });
        }
        DETAILS => {
            return body
                .deserialize::<(HashMap<String, OwnedValue>,)>()
                .inspect_err(
                    |error| tracing::warn!(member, %error, "a PackageKit signal unreadable"),
                )
                .ok()
                .and_then(|(mut details,)| {
                    let id = details
                        .remove(DETAILS_ID)
                        .and_then(|id| String::try_from(id).ok());
                    let Some(id) = id else {
                        tracing::warn!("details without their package");
                        return None;
                    };
                    let download_size = details
                        .remove(DETAILS_DOWNLOAD_SIZE)
                        .and_then(|size| u64::try_from(size).ok());
                    Some(Signal::Details(Details { id, download_size }))
                });
        }
        PROPERTIES_CHANGED => {
            let mut changed = body
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .inspect_err(
                    |error| tracing::warn!(member, %error, "a PackageKit signal unreadable"),
                )
                .ok()?
                .1;
            let mut read = |name: &str| {
                changed.remove(name).and_then(|value| {
                    u32::try_from(value)
                        .inspect_err(|error| tracing::warn!(name, %error, "a property unreadable"))
                        .ok()
                })
            };
            let status = read(STATUS);
            let percentage = read(PERCENTAGE);
            return (status.is_some() || percentage.is_some())
                .then_some(Signal::Properties { status, percentage });
        }
        _ => return None,
    };
    Some(read.unwrap_or_else(|error| {
        tracing::warn!(member, %error, "a PackageKit signal unreadable");
        Signal::Unreadable {
            member: member.to_owned(),
        }
    }))
}

#[cfg(test)]
mod tests;
