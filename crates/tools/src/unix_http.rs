//! HTTP/1.1 on a Unix socket, as the daemons Okapi asks speak it (snapd,
//! Docker): one request per connection, `Connection: close`, the answer
//! read whole, by its length or its chunks, within a deadline and a cap. No
//! command is run (invariant 4).

use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// The largest answer read: beyond, the daemon is not what answers.
pub(crate) const LARGEST_ANSWER: usize = 16 * 1024 * 1024;
/// What an HTTP status of success is.
pub(crate) const SUCCESS: std::ops::Range<u16> = 200..300;
/// What is read at a time.
const READ_SIZE: usize = 64 * 1024;
/// How long a read waits before the request's cancellation is looked at.
const CANCEL_LOOK_EVERY: Duration = Duration::from_millis(250);
/// The end of an answer's head.
const HEAD_END: &[u8] = b"\r\n\r\n";
const LINE_END: &[u8] = b"\r\n";
/// How much of an answer that cannot be read is traced.
const TRACED_EXCERPT: usize = 200;

/// A request: its method, path (with its query), extra headers, JSON body,
/// and the longest it waits.
pub(crate) struct Request<'a> {
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    pub(crate) headers: &'a [(&'a str, &'a str)],
    pub(crate) body: Option<String>,
    pub(crate) wait: Duration,
}

/// Why no answer could be read.
#[derive(Debug)]
pub(crate) enum Failure {
    /// Nothing listens on the socket (absent, or refused).
    Absent(io::Error),
    /// The socket is there, but the user may not connect to it.
    NotAllowed(io::Error),
    /// The connection failed otherwise: nothing was sent.
    Unreached(io::Error),
    /// The request was sent, then the daemon failed or fell silent
    /// (`TimedOut` past the deadline): what it did is unknown.
    Io(io::Error),
    /// The answer ended before what it announced.
    Truncated {
        read: usize,
    },
    TooLarge,
    /// Not HTTP as the daemons speak it.
    Malformed(&'static str),
    /// The caller cancelled while waiting: the connection is closed, what
    /// the daemon did is unknown.
    Cancelled,
}

/// Connects to `socket`.
///
/// # Errors
///
/// [`Failure::Absent`], [`Failure::NotAllowed`] or [`Failure::Unreached`].
fn connect(socket: &Path) -> Result<UnixStream, Failure> {
    UnixStream::connect(socket).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Failure::Absent(error),
        io::ErrorKind::PermissionDenied => Failure::NotAllowed(error),
        _ => Failure::Unreached(error),
    })
}

/// The status and the body of the daemon's answer to `request` on
/// `socket`; `cancelled` is looked at while waiting.
///
/// # Errors
///
/// [`Failure`]: no answer read.
pub(crate) fn exchange(
    socket: &Path,
    request: &Request<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(u16, Vec<u8>), Failure> {
    let mut stream = connect(socket)?;
    stream
        .set_write_timeout(Some(request.wait))
        .map_err(Failure::Unreached)?;
    stream
        .write_all(&request_bytes(request))
        .map_err(Failure::Io)?;
    let raw = read_whole(&mut stream, Instant::now() + request.wait, cancelled)?;
    parse_answer(&raw)
}

/// The start of `bytes`, as text, for a trace.
pub(crate) fn excerpt(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(TRACED_EXCERPT)]).into_owned()
}

/// The bytes of `request`.
fn request_bytes(request: &Request<'_>) -> Vec<u8> {
    let headers: String = request
        .headers
        .iter()
        .map(|(name, value)| [*name, ": ", *value, "\r\n"].concat())
        .collect();
    let (content, body) = request.body.as_deref().map_or_else(
        // A request that may carry a body says it carries none.
        || match request.method {
            "GET" => (String::new(), ""),
            _ => ("Content-Length: 0\r\n".to_owned(), ""),
        },
        |body| {
            (
                format!(
                    "Content-Type: application/json\r\nContent-Length: {}\r\n",
                    body.len()
                ),
                body,
            )
        },
    );
    format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: okapi\r\n{headers}{content}Connection: close\r\n\r\n{body}",
        request.method, request.path
    )
    .into_bytes()
}

/// Everything until the connection closes, by `deadline`, within the cap;
/// `cancelled` looked at between short reads.
fn read_whole(
    stream: &mut UnixStream,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<u8>, Failure> {
    let mut raw = Vec::new();
    let mut chunk = vec![0; READ_SIZE];
    loop {
        if cancelled() {
            return Err(Failure::Cancelled);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Failure::Io(io::ErrorKind::TimedOut.into()));
        }
        stream
            .set_read_timeout(Some(left.min(CANCEL_LOOK_EVERY)))
            .map_err(Failure::Io)?;
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(Failure::Io(error)),
        };
        if read == 0 {
            return Ok(raw);
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.len() > LARGEST_ANSWER {
            return Err(Failure::TooLarge);
        }
    }
}

/// The status and the body of an answer read whole.
fn parse_answer(raw: &[u8]) -> Result<(u16, Vec<u8>), Failure> {
    let head_length = find(raw, HEAD_END).ok_or(Failure::Truncated { read: raw.len() })?;
    let head = std::str::from_utf8(&raw[..head_length])
        .map_err(|_| Failure::Malformed("head not in UTF-8"))?;
    let rest = &raw[head_length + HEAD_END.len()..];
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.strip_prefix("HTTP/1."))
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(Failure::Malformed("no status line"))?;
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(Failure::Malformed("a header without a colon"));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.eq_ignore_ascii_case("chunked");
        } else if name.eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| Failure::Malformed("a length that is not a number"))?,
            );
        }
    }
    let body = if chunked {
        dechunk(rest, raw.len())?
    } else if let Some(length) = length {
        rest.get(..length)
            .ok_or(Failure::Truncated { read: raw.len() })?
            .to_vec()
    } else {
        rest.to_vec()
    };
    Ok((status, body))
}

/// The body sent in chunks, put back together.
fn dechunk(mut rest: &[u8], read: usize) -> Result<Vec<u8>, Failure> {
    let mut body = Vec::new();
    loop {
        let line_length = find(rest, LINE_END).ok_or(Failure::Truncated { read })?;
        let size = std::str::from_utf8(&rest[..line_length])
            .ok()
            .and_then(|line| line.split(';').next())
            .and_then(|size| usize::from_str_radix(size.trim(), 16).ok())
            .ok_or(Failure::Malformed("a chunk size that is not a number"))?;
        rest = &rest[line_length + LINE_END.len()..];
        if size == 0 {
            return Ok(body);
        }
        let chunk = rest.get(..size).ok_or(Failure::Truncated { read })?;
        body.extend_from_slice(chunk);
        rest = rest
            .get(size..)
            .and_then(|after| after.strip_prefix(LINE_END))
            .ok_or(Failure::Truncated { read })?;
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// A daemon that reads the request and never answers: the caller's
    /// cancellation ends the wait, the deadline being far.
    #[test]
    fn a_cancelled_wait_ends_without_an_answer() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.socket");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; 4096];
            let _ = stream.read(&mut request);
            std::thread::sleep(Duration::from_secs(30));
        });
        let cancelled = AtomicBool::new(false);
        let request = Request {
            method: "POST",
            path: "/slow",
            headers: &[],
            body: None,
            wait: Duration::from_secs(60),
        };
        let started = Instant::now();
        let waiting = std::thread::scope(|scope| {
            let waiting =
                scope.spawn(|| exchange(&socket, &request, &|| cancelled.load(Ordering::SeqCst)));
            std::thread::sleep(Duration::from_millis(300));
            cancelled.store(true, Ordering::SeqCst);
            waiting.join().unwrap()
        });
        assert!(matches!(waiting, Err(Failure::Cancelled)), "{waiting:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_socket_nobody_listens_on_is_absent() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("none.socket");
        assert!(matches!(connect(&socket), Err(Failure::Absent(_))));
    }
}
