//! A snapd on a socket that answers each request, one per connection, with
//! the next of its answers, and tells what it was asked: for the tests of
//! what speaks to snapd.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::time::Duration;

use super::Snapd;

/// The status line of `body`, as its `status-code` says (200 without one).
fn status_of(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("status-code")?.as_u64())
        .map_or_else(|| "200 OK".to_owned(), |code| format!("{code} Answer"))
}

/// snapd answering `answers` in order (`None`: closed unanswered); each
/// request read comes back on the receiver, in order.
pub(crate) fn scripted(
    answers: Vec<Option<String>>,
) -> (tempfile::TempDir, Snapd, mpsc::Receiver<String>) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("snapd.socket");
    let listener = UnixListener::bind(&socket).unwrap();
    let (sender, requests) = mpsc::channel();
    std::thread::spawn(move || {
        for answer in answers {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; 4096];
            let read = stream.read(&mut request).unwrap();
            // Not every test reads them back.
            let _ = sender.send(String::from_utf8_lossy(&request[..read]).into_owned());
            if let Some(body) = answer {
                let _ = write!(
                    stream,
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\n\r\n{body}",
                    status_of(&body),
                    body.len()
                );
            }
        }
    });
    let snapd = Snapd::at(&socket, Duration::from_secs(5));
    (directory, snapd, requests)
}
