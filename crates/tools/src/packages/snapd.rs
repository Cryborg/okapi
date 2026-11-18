//! snapd's REST interface, on its socket: no command is run (invariant 4).
//! One request per connection, HTTP/1.1 with `Connection: close`; the
//! answer's body is read whole, by its length or its chunks, within a
//! deadline and a cap, then its envelope (`type`, `status-code`, `result`)
//! is told apart: an error of snapd is a [`SnapdError::Refused`]. Reads, a
//! snap's removal and snaps' refresh, polkit asking the user for the rights;
//! the change they start followed to its end.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::unix_http::{self, Failure, LARGEST_ANSWER, SUCCESS, excerpt};

/// Where snapd listens, for every user (its access checks are its own).
const SOCKET: &str = "/run/snapd.socket";
/// The longest a request waits: `select=refresh` asks the store.
const WAIT: Duration = Duration::from_secs(60);
/// How long a request that asks the user may wait: snapd answers once
/// polkit's window is closed.
const ASKING_WAIT: Duration = Duration::from_mins(30);
/// What snapd says when its store cannot be reached (`client/errors.go`).
pub const KIND_NETWORK_TIMEOUT: &str = "network-timeout";
/// The rights were not given: the window closed, or no one logged in.
pub const KIND_AUTH_CANCELLED: &str = "auth-cancelled";
pub const KIND_LOGIN_REQUIRED: &str = "login-required";
/// Another change holds the snap (an automatic refresh).
pub const KIND_CHANGE_CONFLICT: &str = "snap-change-conflict";
/// A snap absent, as a change on it says (`POST /v2/snaps/<name>`).
pub const KIND_NOT_INSTALLED: &str = "snap-not-installed";
/// A snap absent, as reading it says (`GET /v2/snaps/<name>`): not the
/// change's kind, seen on snapd 2.7x.
pub const KIND_NOT_FOUND: &str = "snap-not-found";
/// The kinds of a snap absent, read or changed.
const KINDS_ABSENT: [&str; 2] = [KIND_NOT_INSTALLED, KIND_NOT_FOUND];
pub const KIND_DNS_FAILURE: &str = "dns-failure";
/// What snapd says when it refuses to refresh a snap whose apps run: no
/// kind of its own (snapd 2.78, seen 2026-11-16), only this message, never
/// translated.
const MESSAGE_APPS_RUNNING: &str = " has running apps ";
/// The HTTP statuses of a request refused for its rights (unauthorized,
/// forbidden), whatever kind snapd gives it.
const HTTP_NOT_ALLOWED: [u16; 2] = [401, 403];
/// What a publisher's `validation` is when the store vouches for it.
pub const VALIDATION_VOUCHED: [&str; 2] = ["verified", "starred"];
/// The envelope of an error.
const ERROR_TYPE: &str = "error";

/// Why snapd gave no answer that could be used.
#[derive(Debug, thiserror::Error)]
pub enum SnapdError {
    /// Nothing listens on its socket: snapd is not installed or not running.
    #[error("snapd is not listening on {}: {source}", socket.display())]
    Absent { socket: PathBuf, source: io::Error },
    /// A name that is no snap's, or none at all: nothing was asked.
    #[error("not a snap's name: {0:?}")]
    InvalidName(String),
    /// A revision that is no snap's revision: nothing was asked.
    #[error("not a snap's revision: {0:?}")]
    InvalidRevision(String),
    /// The socket was reached, then failed or fell silent.
    #[error("snapd did not answer: {0}")]
    Io(#[source] io::Error),
    /// The answer ended before what it announced.
    #[error("snapd's answer was cut after {read} bytes")]
    Truncated { read: usize },
    #[error("snapd's answer is beyond {LARGEST_ANSWER} bytes")]
    TooLarge,
    /// Not HTTP as snapd speaks it.
    #[error("snapd's answer is malformed: {0}")]
    Malformed(&'static str),
    #[error("snapd's answer is not the JSON awaited: {0}")]
    Json(#[from] serde_json::Error),
    /// snapd answered with an error of its own; `kind` is empty when it
    /// gave none.
    #[error("snapd refused ({status}, {kind}): {message}")]
    Refused {
        status: u16,
        kind: String,
        message: String,
    },
}

impl SnapdError {
    /// Whether snapd said the snap is absent, read or changed.
    #[must_use]
    pub fn snap_absent(&self) -> bool {
        matches!(self, Self::Refused { kind, .. } if KINDS_ABSENT.contains(&kind.as_str()))
    }

    /// Whether snapd fell silent past the deadline.
    #[must_use]
    pub fn timed_out(&self) -> bool {
        matches!(self, Self::Io(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock))
    }

    /// Whether snapd said its store could not be reached.
    #[must_use]
    pub fn offline(&self) -> bool {
        matches!(self, Self::Refused { kind, .. } if kind == KIND_NETWORK_TIMEOUT || kind == KIND_DNS_FAILURE)
    }

    /// Whether the rights were not given (the window closed, no one logged
    /// in, or a refusal by its HTTP status).
    #[must_use]
    pub fn rights_refused(&self) -> bool {
        matches!(self, Self::Refused { kind, status, .. }
            if kind == KIND_AUTH_CANCELLED || kind == KIND_LOGIN_REQUIRED || HTTP_NOT_ALLOWED.contains(status))
    }

    /// Whether another change holds the snap (an automatic refresh).
    #[must_use]
    pub fn busy(&self) -> bool {
        matches!(self, Self::Refused { kind, .. } if kind == KIND_CHANGE_CONFLICT)
    }

    /// Whether snapd refused a refresh because the snap's apps run.
    #[must_use]
    pub fn apps_running(&self) -> bool {
        matches!(self, Self::Refused { message, .. } if message.contains(MESSAGE_APPS_RUNNING))
    }
}

/// snapd's envelope: `type` is `sync`, `async` or `error`; an `async` one
/// names the change it started.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "status-code")]
    status: u16,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    change: Option<String>,
}

/// Where a change of snapd stands (`/v2/changes/<id>`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Change {
    /// `Do`, `Doing`, `Done`, `Error`, `Undone`, `Hold`, `Abort`...
    pub status: String,
    pub ready: bool,
    /// Why it failed, in snapd's words.
    #[serde(default)]
    pub err: Option<String>,
    /// Its tasks, each with its progress.
    #[serde(default)]
    pub tasks: Vec<Task>,
}

/// A task of a change: `download-snap`, `link-snap`...
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Task {
    pub kind: String,
    /// `Do`, `Doing`, `Done`...
    pub status: String,
    #[serde(default)]
    pub progress: TaskProgress,
}

/// How far a task is: bytes for a download, steps otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct TaskProgress {
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub total: u64,
}

/// The kind of the task that downloads a snap.
pub const TASK_DOWNLOAD: &str = "download-snap";
/// The statuses of a task still to do or under way: no other is a download
/// going on.
pub const TASK_UNDERWAY: [&str; 2] = ["Do", "Doing"];

/// The change status of a change that did all it was asked.
pub const CHANGE_DONE: &str = "Done";
/// How often a change is asked where it stands.
pub const CHANGE_LOOK_EVERY: Duration = Duration::from_millis(500);

/// How following a change ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Followed {
    /// It is over, done or not as its status says.
    Ready(Change),
    /// Still going when the wait ran out.
    TimedOut,
    /// It could not be read that many times in a row.
    Unanswered,
}

/// How a request is made.
struct Request<'a> {
    method: &'a str,
    path: &'a str,
    /// A JSON body.
    body: Option<String>,
    /// polkit may ask the user (`X-Allow-Interaction`).
    interactive: bool,
    wait: Duration,
}

/// What an error's `result` says.
#[derive(Debug, Deserialize)]
struct ErrorResult {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    message: String,
}

/// A snap with a newer revision in the store (`/v2/find?select=refresh`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Refresh {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    pub version: String,
    #[serde(default)]
    pub publisher: Option<Publisher>,
    #[serde(rename = "download-size", default)]
    pub download_size: Option<u64>,
}

/// Who publishes a snap.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Publisher {
    #[serde(rename = "display-name")]
    pub display_name: String,
    /// `verified` or `starred` when the store vouches for it.
    #[serde(default)]
    pub validation: Option<String>,
}

/// When snapd refreshes its snaps by itself (`/v2/system-info`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Schedule {
    /// The next automatic refresh, in RFC 3339, as its timer says it.
    #[serde(default)]
    pub next: Option<String>,
    /// Until when refreshes are held (`refresh.hold`), in RFC 3339: the
    /// timer does not know it.
    #[serde(default)]
    pub hold: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SystemInfo {
    #[serde(default)]
    refresh: Schedule,
}

/// Whether `name` is a snap's name (an instance key after `_`): it is then
/// safe in a path and in a request.
#[must_use]
pub fn is_snap_name(name: &str) -> bool {
    let (base, instance) = name.split_once('_').unwrap_or((name, ""));
    let lower = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    base.bytes().next().is_some_and(lower)
        && base.bytes().all(|byte| lower(byte) || byte == b'-')
        && (!name.contains('_') || (!instance.is_empty() && instance.bytes().all(lower)))
}

/// Whether `revision` is a revision of a snap from the store: a number, as
/// snapd writes it. It is then safe in a request. One installed from a file
/// (`x1`) is not: its file may be elsewhere (`snap try`).
#[must_use]
pub fn is_revision(revision: &str) -> bool {
    !revision.is_empty() && revision.bytes().all(|byte| byte.is_ascii_digit())
}

/// `name`, if it is a snap's name; refused, traced, before anything is
/// asked.
fn checked(name: &str) -> Result<&str, SnapdError> {
    if is_snap_name(name) {
        Ok(name)
    } else {
        tracing::error!(name, "a snap's name refused: nothing asked");
        Err(SnapdError::InvalidName(name.to_owned()))
    }
}

/// `id`, if it may name a change of snapd: digits only, as snapd numbers
/// them, so that it is safe in a request's path; refused, traced, before
/// anything is asked.
fn checked_change(id: &str) -> Result<&str, SnapdError> {
    if !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(id)
    } else {
        tracing::error!(change = id, "a change of snapd refused: not a number");
        Err(SnapdError::Malformed("a change that is not a number"))
    }
}

/// The change an asynchronous answer of snapd started.
///
/// # Errors
/// The answer names none, or one that is not a number: snapd may have
/// started it all the same, never followed then.
fn started(envelope: Envelope) -> Result<String, SnapdError> {
    let change = envelope.change.ok_or(SnapdError::Malformed(
        "an asynchronous answer without its change",
    ))?;
    checked_change(&change)?;
    Ok(change)
}

/// snapd, reached on a socket.
#[derive(Debug, Clone)]
pub struct Snapd {
    socket: PathBuf,
    wait: Duration,
}

impl Snapd {
    /// snapd on this system.
    #[must_use]
    pub fn system() -> Self {
        Self::at(Path::new(SOCKET), WAIT)
    }

    /// snapd on `socket`, each request waiting `wait` at most.
    #[must_use]
    pub fn at(socket: &Path, wait: Duration) -> Self {
        Self {
            socket: socket.to_path_buf(),
            wait,
        }
    }

    /// The snaps with a newer revision waiting.
    ///
    /// # Errors
    /// Whatever kept snapd from answering, or its refusal.
    pub fn refreshes(&self) -> Result<Vec<Refresh>, SnapdError> {
        Ok(serde_json::from_value(
            self.get("/v2/find?select=refresh")?,
        )?)
    }

    /// When snapd refreshes by itself.
    ///
    /// # Errors
    /// Whatever kept snapd from answering, or its refusal.
    pub fn schedule(&self) -> Result<Schedule, SnapdError> {
        Ok(serde_json::from_value::<SystemInfo>(self.get("/v2/system-info")?)?.refresh)
    }

    /// Every snap installed, every revision kept (`select=all`), as snapd
    /// lists them.
    ///
    /// # Errors
    /// Whatever kept snapd from answering, or its refusal.
    pub fn snaps(&self) -> Result<Value, SnapdError> {
        self.get("/v2/snaps?select=all")
    }

    /// The copies of snaps' data snapd keeps, every set (`/v2/snapshots`),
    /// as snapd lists them.
    ///
    /// # Errors
    /// Whatever kept snapd from answering, or its refusal.
    pub fn snapshots(&self) -> Result<Value, SnapdError> {
        self.get("/v2/snapshots")
    }

    /// Forgets the copy of the snap `snap` in the set `set`, polkit asking
    /// the user for the rights: the change it started. Only that snap's: an
    /// empty list would forget the whole set.
    ///
    /// # Errors
    /// A name that is no snap's (nothing asked), whatever kept snapd from
    /// answering, or its refusal (the rights not given, the copy gone).
    pub fn forget(&self, set: u64, snap: &str) -> Result<String, SnapdError> {
        let snap = checked(snap)?;
        let envelope = self.request(&Request {
            method: "POST",
            path: "/v2/snapshots",
            body: Some(
                serde_json::json!({ "action": "forget", "set": set, "snaps": [snap] }).to_string(),
            ),
            interactive: true,
            wait: ASKING_WAIT,
        })?;
        started(envelope)
    }

    /// Removes the snap `name`, its data with it when `purge` (no copy
    /// kept), polkit asking the user for the rights: the change it started.
    ///
    /// # Errors
    /// A name that is no snap's (nothing asked), whatever kept snapd from
    /// answering, or its refusal (the rights not given among them).
    pub fn remove(&self, name: &str, purge: bool) -> Result<String, SnapdError> {
        let name = checked(name)?;
        let envelope = self.request(&Request {
            method: "POST",
            path: &format!("/v2/snaps/{name}"),
            body: Some(serde_json::json!({ "action": "remove", "purge": purge }).to_string()),
            interactive: true,
            wait: ASKING_WAIT,
        })?;
        started(envelope)
    }

    /// Removes the revision `revision` of the snap `name` alone, its data of
    /// that revision with it for every account (no copy kept), polkit asking
    /// the user for the rights: the change it started. snapd refuses the
    /// revision in use, but removes the whole snap when it is its only one,
    /// and the one in use of a snap disabled: the caller never asks either.
    ///
    /// # Errors
    /// A name that is no snap's or a revision that is none (nothing asked),
    /// whatever kept snapd from answering, or its refusal.
    pub fn remove_revision(&self, name: &str, revision: &str) -> Result<String, SnapdError> {
        let name = checked(name)?;
        if !is_revision(revision) {
            tracing::error!(name, revision, "a snap's revision refused: nothing asked");
            return Err(SnapdError::InvalidRevision(revision.to_owned()));
        }
        let envelope = self.request(&Request {
            method: "POST",
            path: &format!("/v2/snaps/{name}"),
            body: Some(
                serde_json::json!({ "action": "remove", "revision": revision }).to_string(),
            ),
            interactive: true,
            wait: ASKING_WAIT,
        })?;
        started(envelope)
    }

    /// Refreshes the snaps `names` to their newer revision, polkit asking
    /// the user for the rights: the change it started. An empty list is
    /// refused without asking: snapd would take it for every snap.
    ///
    /// # Errors
    /// No name, or one that is no snap's (nothing asked), whatever kept
    /// snapd from answering, or its refusal.
    pub fn refresh(&self, names: &[String]) -> Result<String, SnapdError> {
        if names.is_empty() {
            tracing::error!("a refresh of no snap: nothing asked");
            return Err(SnapdError::InvalidName(String::new()));
        }
        for name in names {
            checked(name)?;
        }
        let envelope = self.request(&Request {
            method: "POST",
            path: "/v2/snaps",
            body: Some(serde_json::json!({ "action": "refresh", "snaps": names }).to_string()),
            interactive: true,
            wait: ASKING_WAIT,
        })?;
        started(envelope)
    }

    /// Whether the snap `name` (a name checked by the caller) is installed.
    ///
    /// # Errors
    /// Whatever kept snapd from answering, or a refusal other than the snap
    /// not installed.
    pub fn installed(&self, name: &str) -> Result<bool, SnapdError> {
        match self.get(&format!("/v2/snaps/{name}")) {
            Ok(_) => Ok(true),
            Err(error) if error.snap_absent() => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Where the change `id` stands.
    ///
    /// # Errors
    /// An id that is not a number (nothing asked), whatever kept snapd from
    /// answering, or its refusal.
    pub fn change(&self, id: &str) -> Result<Change, SnapdError> {
        let id = checked_change(id)?;
        Ok(serde_json::from_value(
            self.get(&format!("/v2/changes/{id}"))?,
        )?)
    }

    /// Follows the change `id` until it is ready, telling `heard` where it
    /// stands at each look; no look once `wait` has passed, and it gives up
    /// after `unanswered_after` unreadable looks in a row (snapd restarting
    /// answers nothing a moment).
    pub fn follow(
        &self,
        id: &str,
        wait: Duration,
        unanswered_after: u32,
        heard: &dyn Fn(&Change),
    ) -> Followed {
        let followed = Instant::now();
        let mut failed_looks = 0;
        loop {
            std::thread::sleep(CHANGE_LOOK_EVERY);
            // Before each look: a change never ready ends all the same.
            if followed.elapsed() >= wait {
                tracing::warn!(change = id, ?wait, "snapd's change followed no longer");
                return Followed::TimedOut;
            }
            match self.change(id) {
                Ok(state) if state.ready => return Followed::Ready(state),
                Ok(state) => {
                    failed_looks = 0;
                    heard(&state);
                }
                Err(error) => {
                    failed_looks += 1;
                    tracing::warn!(change = id, %error, failed_looks, "snapd's change unreadable");
                    if failed_looks >= unanswered_after {
                        return Followed::Unanswered;
                    }
                }
            }
        }
    }

    /// What snapd answers to `path`: the `result` of its envelope.
    fn get(&self, path: &str) -> Result<Value, SnapdError> {
        self.request(&Request {
            method: "GET",
            path,
            body: None,
            interactive: false,
            wait: self.wait,
        })
        .map(|envelope| envelope.result)
    }

    /// snapd's answer to `request`: its envelope, when it is no error.
    fn request(&self, request: &Request<'_>) -> Result<Envelope, SnapdError> {
        let path = request.path;
        let interaction = [("X-Allow-Interaction", "true")];
        let exchanged = unix_http::exchange(
            &self.socket,
            &unix_http::Request {
                method: request.method,
                path,
                headers: if request.interactive {
                    &interaction
                } else {
                    &[]
                },
                body: request.body.clone(),
                wait: request.wait,
            },
            &|| false,
        );
        let (status, body) = exchanged.map_err(|failure| match failure {
            Failure::Absent(source) => SnapdError::Absent {
                socket: self.socket.clone(),
                source,
            },
            Failure::NotAllowed(error) | Failure::Unreached(error) | Failure::Io(error) => {
                SnapdError::Io(error)
            }
            Failure::Truncated { read } => SnapdError::Truncated { read },
            Failure::TooLarge => SnapdError::TooLarge,
            Failure::Malformed(what) => SnapdError::Malformed(what),
            // Never cancelled: nothing looks at it.
            Failure::Cancelled => SnapdError::Io(io::ErrorKind::Interrupted.into()),
        })?;
        let envelope: Envelope = serde_json::from_slice(&body).inspect_err(|error| {
            tracing::warn!(path, status, %error, body = %excerpt(&body), "snapd's answer is no envelope");
        })?;
        if envelope.kind == ERROR_TYPE
            || !SUCCESS.contains(&status)
            || !SUCCESS.contains(&envelope.status)
        {
            let (kind, message) =
                match serde_json::from_value::<ErrorResult>(envelope.result.clone()) {
                    Ok(said) => (said.kind, said.message),
                    // Its words, whatever their shape: the error says them.
                    Err(_) => (
                        String::new(),
                        excerpt(envelope.result.to_string().as_bytes()),
                    ),
                };
            return Err(SnapdError::Refused {
                status: if SUCCESS.contains(&status) {
                    envelope.status
                } else {
                    status
                },
                kind,
                message,
            });
        }
        Ok(envelope)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod fake;
