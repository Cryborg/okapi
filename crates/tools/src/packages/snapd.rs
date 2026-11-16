//! snapd's REST interface, on its socket: no command is run (invariant 4).
//! One request per connection, HTTP/1.1 with `Connection: close`; the
//! answer's body is read whole, by its length or its chunks, within a
//! deadline and a cap, then its envelope (`type`, `status-code`, `result`)
//! is told apart: an error of snapd is a [`SnapdError::Refused`]. Reads, a
//! snap's removal and snaps' refresh, polkit asking the user for the rights;
//! the change they start followed to its end.

use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

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
/// The HTTP statuses of a request refused for its rights (unauthorized,
/// forbidden), whatever kind snapd gives it.
const HTTP_NOT_ALLOWED: [u16; 2] = [401, 403];
/// What a publisher's `validation` is when the store vouches for it.
pub const VALIDATION_VOUCHED: [&str; 2] = ["verified", "starred"];
/// The largest answer read: beyond, snapd is not what answers.
const LARGEST_ANSWER: usize = 16 * 1024 * 1024;
/// What is read at a time.
const READ_SIZE: usize = 64 * 1024;
/// The end of an answer's head.
const HEAD_END: &[u8] = b"\r\n\r\n";
const LINE_END: &[u8] = b"\r\n";
/// The envelope of an error.
const ERROR_TYPE: &str = "error";
/// How much of an answer that cannot be read is traced.
const TRACED_EXCERPT: usize = 200;
/// What an HTTP status of success is.
const SUCCESS: std::ops::Range<u16> = 200..300;

/// Why snapd gave no answer that could be used.
#[derive(Debug, thiserror::Error)]
pub enum SnapdError {
    /// Nothing listens on its socket: snapd is not installed or not running.
    #[error("snapd is not listening on {}: {source}", socket.display())]
    Absent { socket: PathBuf, source: io::Error },
    /// A name that is no snap's, or none at all: nothing was asked.
    #[error("not a snap's name: {0:?}")]
    InvalidName(String),
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
        envelope.change.ok_or(SnapdError::Malformed(
            "an asynchronous answer without its change",
        ))
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
        envelope.change.ok_or(SnapdError::Malformed(
            "an asynchronous answer without its change",
        ))
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
        envelope.change.ok_or(SnapdError::Malformed(
            "an asynchronous answer without its change",
        ))
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
    /// Whatever kept snapd from answering, or its refusal.
    pub fn change(&self, id: &str) -> Result<Change, SnapdError> {
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
        let mut stream = UnixStream::connect(&self.socket).map_err(|source| {
            if matches!(
                source.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) {
                SnapdError::Absent {
                    socket: self.socket.clone(),
                    source,
                }
            } else {
                SnapdError::Io(source)
            }
        })?;
        stream
            .set_write_timeout(Some(request.wait))
            .map_err(SnapdError::Io)?;
        stream
            .write_all(&request_bytes(request))
            .map_err(SnapdError::Io)?;
        let raw = read_whole(&mut stream, Instant::now() + request.wait)?;
        let (status, body) = parse_answer(&raw)?;
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

/// The start of `bytes`, as text, for a trace.
fn excerpt(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(TRACED_EXCERPT)]).into_owned()
}

/// The bytes of `request`.
fn request_bytes(request: &Request<'_>) -> Vec<u8> {
    let interaction = if request.interactive {
        "X-Allow-Interaction: true\r\n"
    } else {
        ""
    };
    let (content, body) = request.body.as_deref().map_or((String::new(), ""), |body| {
        (
            format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ),
            body,
        )
    });
    format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: okapi\r\n{interaction}{content}Connection: close\r\n\r\n{body}",
        request.method, request.path
    )
    .into_bytes()
}

/// Everything until the connection closes, by `deadline`, within the cap.
fn read_whole(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>, SnapdError> {
    let mut raw = Vec::new();
    let mut chunk = vec![0; READ_SIZE];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(SnapdError::Io(io::ErrorKind::TimedOut.into()));
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(SnapdError::Io)?;
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(SnapdError::Io(error)),
        };
        if read == 0 {
            return Ok(raw);
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.len() > LARGEST_ANSWER {
            return Err(SnapdError::TooLarge);
        }
    }
}

/// The status and the body of an answer read whole.
fn parse_answer(raw: &[u8]) -> Result<(u16, Vec<u8>), SnapdError> {
    let head_length = find(raw, HEAD_END).ok_or(SnapdError::Truncated { read: raw.len() })?;
    let head = std::str::from_utf8(&raw[..head_length])
        .map_err(|_| SnapdError::Malformed("head not in UTF-8"))?;
    let rest = &raw[head_length + HEAD_END.len()..];
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.strip_prefix("HTTP/1."))
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(SnapdError::Malformed("no status line"))?;
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(SnapdError::Malformed("a header without a colon"));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.eq_ignore_ascii_case("chunked");
        } else if name.eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| SnapdError::Malformed("a length that is not a number"))?,
            );
        }
    }
    let body = if chunked {
        dechunk(rest, raw.len())?
    } else if let Some(length) = length {
        rest.get(..length)
            .ok_or(SnapdError::Truncated { read: raw.len() })?
            .to_vec()
    } else {
        rest.to_vec()
    };
    Ok((status, body))
}

/// The body sent in chunks, put back together.
fn dechunk(mut rest: &[u8], read: usize) -> Result<Vec<u8>, SnapdError> {
    let mut body = Vec::new();
    loop {
        let line_length = find(rest, LINE_END).ok_or(SnapdError::Truncated { read })?;
        let size = std::str::from_utf8(&rest[..line_length])
            .ok()
            .and_then(|line| line.split(';').next())
            .and_then(|size| usize::from_str_radix(size.trim(), 16).ok())
            .ok_or(SnapdError::Malformed("a chunk size that is not a number"))?;
        rest = &rest[line_length + LINE_END.len()..];
        if size == 0 {
            return Ok(body);
        }
        let chunk = rest.get(..size).ok_or(SnapdError::Truncated { read })?;
        body.extend_from_slice(chunk);
        rest = rest
            .get(size..)
            .and_then(|after| after.strip_prefix(LINE_END))
            .ok_or(SnapdError::Truncated { read })?;
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod fake;
