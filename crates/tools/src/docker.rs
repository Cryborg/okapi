//! Docker's engine, on its socket: its build cache pruned
//! (`POST /build/prune`), no command run (invariant 4, D-14). The user
//! speaks to the daemon with their own access (the `docker` group, or a
//! rootless or Desktop daemon of theirs), never root's.
//!
//! Nothing connects to the daemon before the prune: with systemd's socket
//! activation, a connection would start it and its containers. Whether the
//! user may speak to it is told by the socket itself ([`access`]).
//!
//! The prune asks for the entries unused for [`BUILD_CACHE_UNUSED_DAYS`]
//! days (or the settings' minimum age, if longer) while the cache takes
//! more than [`BUILD_CACHE_KEPT_GIGABYTES`]: under that, nothing goes
//! (`BuildKit` keeps it as a floor). Both names of that floor are sent, with
//! the same value, without a version prefix: a daemon of API 1.48 or later
//! reads `reserved-space` (`keep-storage` when it is 0), an older one
//! `keep-storage` alone.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::unix_http::{self, Failure, SUCCESS, excerpt};

/// An entry of the build cache unused for this many days may go. The one
/// source of this limit: the rule's description shows it
/// ([`DESCRIPTION_VALUES`]).
pub const BUILD_CACHE_UNUSED_DAYS: u64 = 7;

/// The build cache is kept up to this many gigabytes (decimal, as Okapi
/// shows sizes). The one source of this limit, as
/// [`BUILD_CACHE_UNUSED_DAYS`].
pub const BUILD_CACHE_KEPT_GIGABYTES: u64 = 10;

/// The values of the placeholders of the prune's description, by name.
pub const DESCRIPTION_VALUES: &[(&str, u64)] = &[
    ("days", BUILD_CACHE_UNUSED_DAYS),
    ("gigabytes", BUILD_CACHE_KEPT_GIGABYTES),
];

const SECONDS_PER_HOUR: u64 = 60 * 60;
const HOURS_PER_DAY: u64 = 24;
const BYTES_PER_GIGABYTE: u64 = 1_000_000_000;
/// The longest a prune is waited for: a large cache takes minutes.
const PRUNE_WAIT: Duration = Duration::from_mins(10);
/// What says where the user's Docker CLI speaks.
const HOST_VARIABLE: &str = "DOCKER_HOST";
/// What names the context the user's Docker CLI speaks to.
const CONTEXT_VARIABLE: &str = "DOCKER_CONTEXT";
/// The context of the default daemon: no file of its own.
const DEFAULT_CONTEXT: &str = "default";
/// The only scheme of an endpoint Okapi speaks.
const UNIX_SCHEME: &str = "unix://";
/// The system's daemon.
const SYSTEM_SOCKET: &str = "/var/run/docker.sock";
/// A rootless daemon's, in the session's runtime directory.
const ROOTLESS_SOCKET: &str = "docker.sock";
const RUNTIME_VARIABLE: &str = "XDG_RUNTIME_DIR";
/// Docker's folder of the user, in the home folder.
const DOCKER_FOLDER: &str = ".docker";
/// Docker Desktop's socket, in [`DOCKER_FOLDER`].
const DESKTOP_SOCKET: &str = "desktop/docker.sock";
/// The CLI's configuration and its contexts, in [`DOCKER_FOLDER`].
const CONFIG_FILE: &str = "config.json";
const CONTEXTS_FOLDER: &str = "contexts/meta";
const CONTEXT_FILE: &str = "meta.json";
const HOME_VARIABLE: &str = "HOME";
/// Where the CLI keeps its configuration and contexts, when not in
/// [`DOCKER_FOLDER`].
const CONFIG_VARIABLE: &str = "DOCKER_CONFIG";
/// The first status of a server's own error.
const SERVER_ERROR: u16 = 500;
/// The largest file of Docker's configuration read.
const LARGEST_CONFIG: u64 = 1024 * 1024;

/// Why Docker gave no answer that could be used.
#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    /// Nothing listens on the socket: the daemon is not running.
    #[error("Docker is not listening on {}: {source}", socket.display())]
    Absent { socket: PathBuf, source: io::Error },
    /// The user may not speak to the daemon (not in the `docker` group).
    #[error("Docker refuses this user on {}: {source}", socket.display())]
    NotAllowed { socket: PathBuf, source: io::Error },
    /// The socket could not be reached otherwise: nothing was asked.
    #[error("Docker could not be reached: {0}")]
    Unreached(#[source] io::Error),
    /// The prune was sent, whole or in part, then Docker failed or fell
    /// silent.
    #[error("Docker did not answer: {0}")]
    Io(#[source] io::Error),
    /// Cancelled while Docker worked.
    #[error("cancelled while Docker worked")]
    Cancelled,
    /// Not the answer Docker gives.
    #[error("Docker's answer cannot be read: {0}")]
    Unreadable(String),
    /// Docker answered with an error of its own.
    #[error("Docker refused ({status}): {message}")]
    Refused { status: u16, message: String },
}

impl DockerError {
    /// Whether what the prune removed is unknown: it was asked, and no
    /// answer said what it did. A refusal of the server's own (5xx) may
    /// come once `BuildKit` removed records: unknown too; a refusal of the
    /// request (4xx) comes before anything ran.
    #[must_use]
    pub const fn leaves_unknown(&self) -> bool {
        match self {
            Self::Io(_) | Self::Cancelled | Self::Unreadable(_) => true,
            Self::Refused { status, .. } => *status >= SERVER_ERROR,
            Self::Absent { .. } | Self::NotAllowed { .. } | Self::Unreached(_) => false,
        }
    }
}

/// What a prune removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    /// How many entries of the cache went.
    pub caches: usize,
    /// The room they took, as Docker counts it.
    pub bytes: u64,
}

/// Whether the user may speak to the daemon, as its socket says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Allowed,
    /// The socket is not writable by the user (not in the `docker` group,
    /// or the group not yet in this session).
    NotAllowed,
}

/// What `POST /build/prune` answers.
#[derive(Debug, Deserialize)]
struct PruneAnswer {
    // `null` when nothing went.
    #[serde(rename = "CachesDeleted", default)]
    caches_deleted: Option<Vec<String>>,
    // Always sent by Docker: an answer without it is no answer of its.
    #[serde(rename = "SpaceReclaimed")]
    space_reclaimed: u64,
}

/// What an error of Docker says.
#[derive(Debug, Deserialize)]
struct ErrorAnswer {
    message: String,
}

/// The CLI's configuration, as far as the context goes.
#[derive(Debug, Deserialize)]
struct Config {
    #[serde(rename = "currentContext", default)]
    current_context: Option<String>,
}

/// A context of the CLI (`contexts/meta/<digest>/meta.json`).
#[derive(Debug, Deserialize)]
struct Context {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Endpoints", default)]
    endpoints: Endpoints,
}

#[derive(Debug, Default, Deserialize)]
struct Endpoints {
    #[serde(default)]
    docker: Option<Endpoint>,
}

#[derive(Debug, Deserialize)]
struct Endpoint {
    #[serde(rename = "Host", default)]
    host: String,
}

/// The socket the user's Docker CLI would speak to, as it chooses it: the
/// one [`HOST_VARIABLE`] names; else the endpoint of the context
/// [`CONTEXT_VARIABLE`] or the CLI's configuration names; else the first
/// there of the system's, a rootless daemon's and Docker Desktop's. One
/// named is given whether or not it exists, as the CLI would try it. None
/// for an endpoint of another scheme than `unix://` (Okapi speaks no TCP),
/// or a context named but not found.
#[must_use]
pub fn socket() -> Option<PathBuf> {
    socket_with(
        |name| std::env::var_os(name),
        |path| fs::symlink_metadata(path).is_ok(),
    )
}

/// [`socket`], the environment read by `variable` and what exists told by
/// `exists`; the CLI's files read in its folder ([`CONFIG_VARIABLE`], else
/// [`DOCKER_FOLDER`] in the home folder).
pub(crate) fn socket_with(
    variable: impl Fn(&str) -> Option<OsString>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let absolute = |name: &str| {
        variable(name)
            .map(PathBuf::from)
            .filter(|base| base.is_absolute())
    };
    let home_folder = absolute(HOME_VARIABLE).map(|home| home.join(DOCKER_FOLDER));
    let config_folder = absolute(CONFIG_VARIABLE).or_else(|| home_folder.clone());
    if let Some(host) = variable(HOST_VARIABLE).filter(|host| !host.is_empty()) {
        let socket = unix_path(&host);
        if socket.is_none() {
            tracing::info!(
                ?host,
                "{HOST_VARIABLE} names no local socket: no Docker line"
            );
        }
        tracing::debug!(?host, ?socket, "Docker's socket from {HOST_VARIABLE}");
        return socket;
    }
    let context = variable(CONTEXT_VARIABLE)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| config_folder.as_deref().and_then(current_context));
    if let Some(context) = context.filter(|name| name != DEFAULT_CONTEXT) {
        let socket = config_folder
            .as_deref()
            .and_then(|folder| context_socket(folder, &context));
        if socket.is_none() {
            tracing::info!(
                context,
                "Docker's context gives no local socket: no Docker line"
            );
        }
        tracing::debug!(context, ?socket, "Docker's socket from its context");
        return socket;
    }
    let found = [
        Some(PathBuf::from(SYSTEM_SOCKET)),
        absolute(RUNTIME_VARIABLE).map(|runtime| runtime.join(ROOTLESS_SOCKET)),
        home_folder.map(|folder| folder.join(DESKTOP_SOCKET)),
    ]
    .into_iter()
    .flatten()
    .find(|path| exists(path));
    if let Some(socket) = &found {
        tracing::debug!(?socket, "Docker's socket");
    } else {
        tracing::info!("no socket of Docker");
    }
    found
}

/// The path of an endpoint `unix://<absolute path>`, its bytes kept as
/// they are.
fn unix_path(host: &OsStr) -> Option<PathBuf> {
    host.as_bytes()
        .strip_prefix(UNIX_SCHEME.as_bytes())
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .filter(|path| path.is_absolute())
}

/// The JSON file at `path`, read whole within a cap; none (traced) when it
/// cannot be.
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let read = fs::File::open(path).and_then(|file| {
        let mut text = Vec::new();
        io::Read::read_to_end(&mut io::Read::take(file, LARGEST_CONFIG), &mut text)?;
        Ok(text)
    });
    match read {
        Ok(text) => serde_json::from_slice(&text)
            .inspect_err(|error| tracing::warn!(?path, %error, "Docker's file unreadable"))
            .ok(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(?path, %error, code = error.raw_os_error(), "Docker's file unreadable");
            None
        }
    }
}

/// The context the CLI's configuration in `config_folder` names.
fn current_context(config_folder: &Path) -> Option<String> {
    read_json::<Config>(&config_folder.join(CONFIG_FILE))?.current_context
}

/// The socket of the context `name`, from its file among the CLI's
/// contexts in `config_folder`.
fn context_socket(config_folder: &Path, name: &str) -> Option<PathBuf> {
    let folders = fs::read_dir(config_folder.join(CONTEXTS_FOLDER))
        .inspect_err(|error| tracing::warn!(name, %error, "Docker's contexts unreadable"))
        .ok()?;
    folders
        .filter_map(|folder| {
            folder
                .inspect_err(|error| tracing::warn!(name, %error, "a context of Docker unreadable"))
                .ok()
        })
        .filter_map(|folder| read_json::<Context>(&folder.path().join(CONTEXT_FILE)))
        .find(|context| context.name == name)
        .and_then(|context| context.endpoints.docker)
        .and_then(|endpoint| unix_path(OsStr::new(&endpoint.host)))
}

/// Whether the user may speak to the daemon on `socket`, as the system
/// judges the socket for this process: nothing is connected.
///
/// # Errors
///
/// The socket cannot be judged: [`io::ErrorKind::NotFound`] when it is not
/// there.
pub fn access(socket: &Path) -> io::Result<Access> {
    let allowed = okapi_platform::may_write(socket)?;
    tracing::info!(?socket, allowed, "Docker's socket judged");
    Ok(if allowed {
        Access::Allowed
    } else {
        Access::NotAllowed
    })
}

/// The query of a prune: the entries unused for the days (or `min_age`, if
/// longer, in whole hours), the cache kept up to the gigabytes, under both
/// names of that floor.
fn prune_path(min_age: Duration) -> String {
    let hours =
        (BUILD_CACHE_UNUSED_DAYS * HOURS_PER_DAY).max(min_age.as_secs().div_ceil(SECONDS_PER_HOUR));
    let filters = encoded(&format!(r#"{{"until":{{"{hours}h":true}}}}"#));
    let kept = BUILD_CACHE_KEPT_GIGABYTES * BYTES_PER_GIGABYTE;
    format!("/build/prune?filters={filters}&keep-storage={kept}&reserved-space={kept}")
}

/// `text` as a query's value: every byte but the unreserved ones written
/// `%XX`.
fn encoded(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Docker, reached on a socket.
#[derive(Debug, Clone)]
pub struct Docker {
    socket: PathBuf,
}

impl Docker {
    /// Docker on `socket`.
    #[must_use]
    pub fn at(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
        }
    }

    /// Prunes the build cache ([`BUILD_CACHE_UNUSED_DAYS`], or `min_age` if
    /// longer, and [`BUILD_CACHE_KEPT_GIGABYTES`]); `cancelled` is looked at
    /// while Docker works, the connection then closed.
    ///
    /// # Errors
    ///
    /// [`DockerError`]: nothing known to be pruned
    /// ([`DockerError::leaves_unknown`] says whether something may have
    /// been).
    pub fn prune_build_cache(
        &self,
        min_age: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Pruned, DockerError> {
        let path = prune_path(min_age);
        let started = Instant::now();
        tracing::info!(socket = ?self.socket, path, "Docker's build cache to prune");
        let (status, body) = unix_http::exchange(
            &self.socket,
            &unix_http::Request {
                method: "POST",
                path: &path,
                headers: &[],
                body: None,
                wait: PRUNE_WAIT,
            },
            cancelled,
        )
        .map_err(|failure| self.error_of(failure))
        .inspect_err(|error| {
            tracing::warn!(%error, unknown = error.leaves_unknown(), elapsed = ?started.elapsed(), "Docker's build cache not pruned");
        })?;
        if !SUCCESS.contains(&status) {
            let message = serde_json::from_slice::<ErrorAnswer>(&body)
                .map_or_else(|_| excerpt(&body), |answer| answer.message);
            tracing::warn!(status, message, "Docker refused to prune its build cache");
            return Err(DockerError::Refused { status, message });
        }
        let answer: PruneAnswer = serde_json::from_slice(&body).map_err(|error| {
            tracing::warn!(%error, body = %excerpt(&body), "Docker's answer to the prune is unreadable");
            DockerError::Unreadable(error.to_string())
        })?;
        let pruned = Pruned {
            caches: answer.caches_deleted.map_or(0, |caches| caches.len()),
            bytes: answer.space_reclaimed,
        };
        tracing::info!(?pruned, elapsed = ?started.elapsed(), "Docker's build cache pruned");
        Ok(pruned)
    }

    fn error_of(&self, failure: Failure) -> DockerError {
        match failure {
            Failure::Absent(source) => DockerError::Absent {
                socket: self.socket.clone(),
                source,
            },
            Failure::NotAllowed(source) => DockerError::NotAllowed {
                socket: self.socket.clone(),
                source,
            },
            Failure::Unreached(error) => DockerError::Unreached(error),
            Failure::Io(error) => DockerError::Io(error),
            Failure::Cancelled => DockerError::Cancelled,
            Failure::Truncated { read } => {
                DockerError::Unreadable(format!("cut after {read} bytes"))
            }
            Failure::TooLarge => DockerError::Unreadable("too large".to_owned()),
            Failure::Malformed(what) => DockerError::Unreadable(what.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests;
