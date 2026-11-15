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
/// How often a removal is asked whether it may still be cancelled, and
/// cancelled once asked to and allowed.
const CANCEL_LOOK_EVERY: Duration = Duration::from_millis(300);
const ALLOW_CANCEL: &str = "AllowCancel";

/// The hints of a transaction that reads: never ask the user anything; and
/// of every one: send packages several at once.
const HINT_NOT_INTERACTIVE: &str = "interactive=false";
/// The hint of a removal: polkit may ask the user for the rights.
const HINT_INTERACTIVE: &str = "interactive=true";
const HINT_PLURAL_SIGNALS: &str = "supports-plural-signals=true";
const HINT_LOCALE: &str = "locale=";
/// `PkFilterEnum::None`, as a bit field: every package.
const FILTER_NONE: u64 = 1 << 1;
/// `PkFilterEnum::Installed`.
const FILTER_INSTALLED: u64 = 1 << 2;
/// `PkTransactionFlagEnum`: none, or a simulation that changes nothing.
const TRANSACTION_NONE: u64 = 0;
const TRANSACTION_SIMULATE: u64 = 1 << 2;
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
/// `PkStatusEnum`: queued behind another transaction, or waiting for
/// another program to let go of the package manager.
const STATUS_WAIT: u32 = 1;
const STATUS_WAITING_FOR_LOCK: u32 = 30;

/// The bits of a package's value that say its kind: the upper ones say an
/// update's severity.
const INFO_BITS: u32 = 0xFFFF;

/// What `PkInfoEnum` says of an update.
pub mod info {
    pub const INSTALLED: u32 = 1;
    pub const SECURITY: u32 = 8;
    /// Held back by the system: it will not be installed.
    pub const BLOCKED: u32 = 9;
    /// What `GetUpdates` lists beside the updates (apt): the effects of
    /// upgrading everything, never updates themselves.
    pub const INSTALL: u32 = 27;
    pub const OBSOLETE: u32 = 29;
    pub const DOWNGRADE: u32 = 30;
    /// What a removal takes away: under way, or intended (PackageKit 1.3).
    pub const REMOVING: u32 = 13;
    pub const REMOVE: u32 = 28;
}

/// Values of `PkErrorEnum`.
pub mod error_code {
    pub const NO_NETWORK: u32 = 2;
    pub const PACKAGE_NOT_INSTALLED: u32 = 7;
    pub const TRANSACTION_CANCELLED: u32 = 17;
    pub const CANNOT_GET_LOCK: u32 = 26;
    pub const REPO_NOT_AVAILABLE: u32 = 37;
    pub const NO_MORE_MIRRORS_TO_TRY: u32 = 43;
    /// polkit refused (a session that is not active, no agent).
    pub const NOT_AUTHORIZED: u32 = 48;
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
    /// Its status changed (`PkStatusEnum`).
    Status(u32),
    /// A signal of the transaction whose body could not be read.
    Unreadable {
        member: String,
    },
}

/// The packages, the error and the status a transaction has said so far.
#[derive(Debug, Default)]
pub(crate) struct Collector {
    packages: Vec<Package>,
    error: Option<(u32, String)>,
    /// A package said that could not be read: the list is not whole.
    unreadable: Option<String>,
    status: u32,
}

impl Collector {
    /// Takes `signal`; once the transaction has ended, what it came to.
    pub(crate) fn take(&mut self, signal: Signal) -> Option<Result<Vec<Package>, PackageKitError>> {
        match signal {
            Signal::Package(package) => self.packages.push(package),
            Signal::Packages(packages) => self.packages.extend(packages),
            Signal::ErrorCode { code, details } => {
                if self.error.is_some() {
                    // The first error is the cause; later ones follow from it.
                    tracing::debug!(code, %details, "a later PackageKit error");
                } else {
                    self.error = Some((code, details));
                }
            }
            Signal::Status(status) => self.status = status,
            // Its end or its error unreadable: it is over, its outcome unknown
            // unless an error read before says why.
            Signal::Unreadable { member } if member == FINISHED || member == ERROR_CODE => {
                return Some(Err(match self.error.take() {
                    Some((code, details)) => PackageKitError::Failed { code, details },
                    None => PackageKitError::Unreadable { member },
                }));
            }
            Signal::Unreadable { member } => {
                self.unreadable.get_or_insert(member);
            }
            Signal::Finished { exit } => {
                return Some(match (self.error.take(), self.unreadable.take()) {
                    (Some((code, details)), _) => Err(PackageKitError::Failed { code, details }),
                    (None, Some(member)) => Err(PackageKitError::Unreadable { member }),
                    (None, None) if exit == EXIT_SUCCESS => Ok(std::mem::take(&mut self.packages)),
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
        matches!(self.status, STATUS_WAIT | STATUS_WAITING_FOR_LOCK)
    }
}

/// Follows `signals` until the transaction ends or `wait` passes; tells
/// `waiting` each time it starts or stops waiting for another program, and
/// that it no longer waits once it is over.
async fn follow<S>(
    signals: S,
    wait: Duration,
    waiting: &dyn Fn(bool),
) -> Result<Vec<Package>, PackageKitError>
where
    S: Stream<Item = Signal>,
{
    let mut signals = pin!(signals);
    let mut collector = Collector::default();
    let followed = async {
        while let Some(signal) = signals.next().await {
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

/// PackageKit, joined on the system bus.
#[derive(Debug, Clone)]
pub struct PackageKit {
    connection: zbus::Connection,
    locale: String,
}

impl PackageKit {
    /// Joins the system bus; the transactions ask for texts in `locale`
    /// (`fr_FR.UTF-8`), and only a removal for the user's interaction.
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
    }

    /// What removing the packages `ids` would take away (them, and what
    /// depends on them), simulated: nothing changes, nothing is asked.
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, its own error, or a removal
    /// that would also install or change packages.
    pub fn simulate_remove(&self, ids: &[String]) -> Result<Vec<Package>, PackageKitError> {
        let packages = self.run(
            "RemovePackages",
            &(
                TRANSACTION_SIMULATE,
                ids,
                SIMULATION_ALLOWS_DEPENDENTS,
                AUTOREMOVE,
            ),
            Asked::reading(RESOLVE_WAIT),
            &|_| {},
        )?;
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
        self.run(
            "RemovePackages",
            &(TRANSACTION_NONE, ids, REMOVAL_ALLOWS_DEPENDENTS, AUTOREMOVE),
            Asked {
                wait: REMOVE_WAIT,
                interactive: true,
                cancel: Some((cancel, steps)),
            },
            &|waiting| steps(Removing::Waiting(waiting)),
        )
        .map(drop)
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
    ) -> Result<Vec<Package>, PackageKitError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let _transaction = tracing::info_span!("packagekit", method).entered();
        let started = Instant::now();
        let ended = futures_lite::future::block_on(self.transaction(method, body, &asked, waiting));
        match &ended {
            Ok(packages) => tracing::debug!(
                packages = packages.len(),
                elapsed = ?started.elapsed(),
                "PackageKit transaction done"
            ),
            // No PackageKit is no failure: the caller says so.
            Err(PackageKitError::Absent) => tracing::debug!("no PackageKit on the system bus"),
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
    ) -> Result<Vec<Package>, PackageKitError>
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
        let followed = follow(signals, asked.wait, waiting);
        let ended = match asked.cancel {
            None => followed.await,
            Some((cancel, steps)) => {
                let watched = watch_cancel(&transaction, &properties, cancel, steps);
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
/// be asked for the rights, and, for one that may be cancelled, the flag
/// that asks it to and who hears whether it still can be.
#[derive(Clone, Copy)]
struct Asked<'a> {
    wait: Duration,
    interactive: bool,
    cancel: Option<(&'a AtomicBool, &'a dyn Fn(Removing))>,
}

impl Asked<'_> {
    /// A transaction that reads: nothing asked, nothing cancelled.
    const fn reading(wait: Duration) -> Self {
        Self {
            wait,
            interactive: false,
            cancel: None,
        }
    }
}

/// Looks every so often whether `transaction` may be cancelled, tells
/// `steps` when that changes, and cancels it once `cancel` is set and it
/// can be; never ends by itself (the transaction's end does).
async fn watch_cancel(
    transaction: &zbus::Proxy<'_>,
    properties: &zbus::Proxy<'_>,
    cancel: &AtomicBool,
    steps: &dyn Fn(Removing),
) -> Result<Vec<Package>, PackageKitError> {
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
                    tracing::warn!(%error, allowed, "whether the removal may be cancelled unreadable");
                }
                allowed
            }
        };
        if now != allowed {
            allowed = now;
            steps(Removing::Cancellable(allowed));
        }
        if allowed && !asked && cancel.load(Ordering::Relaxed) {
            asked = true;
            match transaction.call_method("Cancel", &()).await {
                Ok(_) => tracing::info!("removal cancel asked of PackageKit"),
                Err(error) => {
                    // Asked once: the screen says it came too late.
                    tracing::warn!(%error, "removal cancel refused by PackageKit");
                    steps(Removing::Cancellable(false));
                }
            }
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
fn given_up(ended: &Result<Vec<Package>, PackageKitError>) -> bool {
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
        "Package" => body
            .deserialize::<(u32, String, String)>()
            .map(|said| Signal::Package(package_of(said))),
        "Packages" => body
            .deserialize::<Vec<(u32, String, String)>>()
            .map(|packages| Signal::Packages(packages.into_iter().map(package_of).collect())),
        ERROR_CODE => body
            .deserialize::<(u32, String)>()
            .map(|(code, details)| Signal::ErrorCode { code, details }),
        FINISHED => body
            .deserialize::<(u32, u32)>()
            .map(|(exit, _runtime)| Signal::Finished { exit }),
        "Destroy" => return Some(Signal::Destroy),
        PROPERTIES_CHANGED => {
            return body
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .inspect_err(
                    |error| tracing::warn!(member, %error, "a PackageKit signal unreadable"),
                )
                .ok()?
                .1
                .remove(STATUS)
                .and_then(|status| {
                    u32::try_from(status)
                        .inspect_err(|error| tracing::warn!(%error, "a status unreadable"))
                        .ok()
                })
                .map(Signal::Status);
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
