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
/// What the bus answers for a service nobody provides.
const UNKNOWN_SERVICE: [&str; 2] = [
    "org.freedesktop.DBus.Error.ServiceUnknown",
    "org.freedesktop.DBus.Error.NameHasNoOwner",
];
/// The longest a call to the bus waits for its reply, the daemon's start
/// by the bus included (a transaction's end is waited for apart).
const CALL_WAIT: Duration = Duration::from_secs(10);
/// How long the updates are listed at most: from the cache, seconds.
const UPDATES_WAIT: Duration = Duration::from_mins(2);
/// How long the package lists are downloaded at most: a slow network.
const REFRESH_WAIT: Duration = Duration::from_mins(5);

/// The hints of every transaction: never ask the user anything (reading
/// only), and send packages several at once.
const HINT_NOT_INTERACTIVE: &str = "interactive=false";
const HINT_PLURAL_SIGNALS: &str = "supports-plural-signals=true";
const HINT_LOCALE: &str = "locale=";
/// `PkFilterEnum::None`, as a bit field: every package.
const FILTER_NONE: u64 = 1 << 1;
/// `RefreshCache` only downloads lists that changed.
const REFRESH_UNFORCED: bool = false;
/// `PkExitEnum::Success`.
const EXIT_SUCCESS: u32 = 1;
/// `PkStatusEnum`: queued behind another transaction, or waiting for
/// another program to let go of the package manager.
const STATUS_WAIT: u32 = 1;
const STATUS_WAITING_FOR_LOCK: u32 = 30;

/// What `PkInfoEnum` says of an update.
pub mod info {
    pub const SECURITY: u32 = 8;
    /// Held back by the system: it will not be installed.
    pub const BLOCKED: u32 = 9;
}

/// Values of `PkErrorEnum`.
pub mod error_code {
    pub const NO_NETWORK: u32 = 2;
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
    #[error("no end after {waited:?} (waiting for another program: {waiting})")]
    TimedOut {
        waited: Duration,
        /// It was still waiting for another program when the time ran out.
        waiting: bool,
    },
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

/// PackageKit, joined on the system bus.
#[derive(Debug, Clone)]
pub struct PackageKit {
    connection: zbus::Connection,
    hints: Vec<String>,
}

impl PackageKit {
    /// Joins the system bus; the transactions ask for texts in `locale`
    /// (`fr_FR.UTF-8`) and never for the user's interaction.
    ///
    /// # Errors
    /// A bus that is not a socket, or that cannot be joined.
    pub fn connect(locale: &str) -> Result<Self, PackageKitError> {
        let address =
            bus::socket_only(zbus::Address::system()?).map_err(PackageKitError::Transport)?;
        let connection = futures_lite::future::block_on(
            zbus::connection::Builder::address(address)?
                .method_timeout(CALL_WAIT)
                .build(),
        )?;
        Ok(Self {
            connection,
            hints: vec![
                format!("{HINT_LOCALE}{locale}"),
                HINT_NOT_INTERACTIVE.to_owned(),
                HINT_PLURAL_SIGNALS.to_owned(),
            ],
        })
    }

    /// The updates waiting, as the package cache knows them: `GetUpdates`
    /// downloads nothing, [`Self::refresh_cache`] does; `waiting` hears when
    /// another program holds the package manager.
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, or its own error.
    pub fn get_updates(&self, waiting: &dyn Fn(bool)) -> Result<Vec<Package>, PackageKitError> {
        self.run("GetUpdates", &(FILTER_NONE,), UPDATES_WAIT, waiting)
    }

    /// Downloads the package lists that changed.
    ///
    /// # Errors
    /// Whatever kept PackageKit from answering, or its own error.
    pub fn refresh_cache(&self, waiting: &dyn Fn(bool)) -> Result<(), PackageKitError> {
        self.run("RefreshCache", &(REFRESH_UNFORCED,), REFRESH_WAIT, waiting)
            .map(drop)
    }

    /// Runs `method` with `body` in a new transaction, until it ends or
    /// `wait` passes (then it is cancelled).
    fn run<B>(
        &self,
        method: &str,
        body: &B,
        wait: Duration,
        waiting: &dyn Fn(bool),
    ) -> Result<Vec<Package>, PackageKitError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let _transaction = tracing::info_span!("packagekit", method).entered();
        let started = Instant::now();
        let ended = futures_lite::future::block_on(self.transaction(method, body, wait, waiting));
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
        wait: Duration,
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
            .call_method("SetHints", &(self.hints.as_slice(),))
            .await?;
        transaction.call_method(method, body).await?;
        let signals =
            futures_lite::stream::or(said, changed).filter_map(|message| signal_of(&message));
        let ended = follow(signals, wait, waiting).await;
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
        zbus::Error::MethodError(name, ..) if UNKNOWN_SERVICE.contains(&name.as_str()) => {
            PackageKitError::Absent
        }
        _ => PackageKitError::Bus(error),
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
            .map(|(info, id, summary)| Signal::Package(Package { info, id, summary })),
        "Packages" => body
            .deserialize::<Vec<(u32, String, String)>>()
            .map(|packages| {
                Signal::Packages(
                    packages
                        .into_iter()
                        .map(|(info, id, summary)| Package { info, id, summary })
                        .collect(),
                )
            }),
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
