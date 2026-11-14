//! Shows a folder, or a file in its folder, in the file manager, through the
//! desktop portal (`org.freedesktop.portal.OpenURI`) on the session bus:
//! no command is run (invariant 4). The portal takes a descriptor, never a
//! `file://` address; it answers by a `Response` signal on its request.

use std::collections::HashMap;
use std::fmt;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_lite::StreamExt as _;
use zbus::address::Transport;
use zbus::zvariant::{Fd, OwnedObjectPath, OwnedValue, Value};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const OBJECT: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.OpenURI";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const REQUESTS: &str = "/org/freedesktop/portal/desktop/request";
const RESPONSE: &str = "Response";
/// The longest a call to the bus waits for its reply (not for the answer,
/// which may wait on the user).
const CALL_WAIT: Duration = Duration::from_secs(10);
/// What the portal answers: done, or the user cancelled; anything else it
/// refused.
const ANSWER_DONE: u32 = 0;
const ANSWER_CANCELLED: u32 = 1;

/// How the object is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shown {
    /// A folder, opened (`OpenFile` on a directory).
    Folder,
    /// A file, in its folder, chosen there (`OpenDirectory`).
    Item,
}

/// Why the file manager did not open.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// The session bus is not reached by a socket: joining it could run a
    /// program (`unixexec:`, `ibus:`).
    Transport(String),
    Bus(zbus::Error),
    /// The user closed what the portal asked.
    Cancelled,
    /// The portal refused, with its code.
    Refused(u32),
    /// The request ended without an answer.
    NoAnswer,
    /// The bus gave the connection no name: its answer could not be found.
    Unnamed,
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(address) => write!(formatter, "session bus not a socket: {address}"),
            Self::Bus(error) => write!(formatter, "{error}"),
            Self::Cancelled => write!(formatter, "cancelled by the user"),
            Self::Refused(code) => write!(formatter, "refused by the portal (code {code})"),
            Self::NoAnswer => write!(formatter, "no answer from the portal"),
            Self::Unnamed => write!(formatter, "no name on the session bus"),
        }
    }
}

impl From<zbus::Error> for Refusal {
    fn from(error: zbus::Error) -> Self {
        Self::Bus(error)
    }
}

/// What an answer code of the portal means.
pub(crate) const fn answered(code: u32) -> Result<(), Refusal> {
    match code {
        ANSWER_DONE => Ok(()),
        ANSWER_CANCELLED => Err(Refusal::Cancelled),
        other => Err(Refusal::Refused(other)),
    }
}

/// The session bus, if it is reached by a socket only.
pub(crate) fn socket_only(address: zbus::Address) -> Result<zbus::Address, Refusal> {
    if matches!(address.transport(), Transport::Unix(_)) {
        Ok(address)
    } else {
        Err(Refusal::Transport(address.to_string()))
    }
}

/// The method of the portal that shows as `shown`.
pub(crate) const fn method_of(shown: Shown) -> &'static str {
    match shown {
        Shown::Folder => "OpenFile",
        Shown::Item => "OpenDirectory",
    }
}

/// The path of the request the portal makes for a call of `sender` with
/// `token`, known before the call so that its answer is never missed.
pub(crate) fn request_path(sender: &str, token: &str) -> String {
    let sender = sender.trim_start_matches(':').replace('.', "_");
    format!("{REQUESTS}/{sender}/{token}")
}

/// Shows `object` (a descriptor that only names it) as `shown`, and waits
/// for the portal's answer.
pub(crate) async fn show(object: OwnedFd, shown: Shown) -> Result<(), Refusal> {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let address = socket_only(zbus::Address::session()?)?;
    let connection = zbus::connection::Builder::address(address)?
        .method_timeout(CALL_WAIT)
        .build()
        .await?;
    let token = format!("okapi{}", CALLS.fetch_add(1, Ordering::Relaxed));
    let sender = connection
        .unique_name()
        .ok_or(Refusal::Unnamed)?
        .to_string();
    let expected = request_path(&sender, &token);
    let request = zbus::Proxy::new(
        &connection,
        DESTINATION,
        expected.as_str(),
        REQUEST_INTERFACE,
    )
    .await?;
    let mut answers = request.receive_signal(RESPONSE).await?;
    let portal = zbus::Proxy::new(&connection, DESTINATION, OBJECT, INTERFACE).await?;
    let options = HashMap::from([("handle_token", Value::from(token.as_str()))]);
    let made: OwnedObjectPath = portal
        .call(method_of(shown), &("", Fd::from(&object), options))
        .await?;
    if made.as_str() != expected {
        // An old portal: its answer comes elsewhere, and is waited for in
        // vain.
        tracing::warn!(%made, %expected, "the portal's request is not where it was awaited");
    }
    let answer = answers.next().await.ok_or(Refusal::NoAnswer)?;
    let (code, _results): (u32, HashMap<String, OwnedValue>) = answer.body().deserialize()?;
    answered(code)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use super::*;

    #[test]
    fn the_bus_is_joined_by_a_socket_only() {
        let address = |text| zbus::Address::from_str(text).unwrap();
        assert!(socket_only(address("unix:path=/run/user/1000/bus")).is_ok());
        assert!(socket_only(address("unix:abstract=/tmp/dbus-x")).is_ok());
        for runs_something in ["unixexec:path=/bin/true", "tcp:host=localhost,port=4000"] {
            assert!(
                matches!(
                    socket_only(address(runs_something)),
                    Err(Refusal::Transport(_))
                ),
                "{runs_something}"
            );
        }
        // Not understood, or refused: never joined.
        for elsewhere in [
            "autolaunch:",
            "launchd:env=DBUS_LAUNCHD_SESSION_BUS_SOCKET",
            "nonce-tcp:host=localhost,port=4000,noncefile=/tmp/n",
            "vsock:cid=2,port=4000",
            "systemd:",
            "unixexec:path=/bin/true;unix:path=/run/user/1000/bus",
        ] {
            assert!(
                !matches!(
                    zbus::Address::from_str(elsewhere).map(socket_only),
                    Ok(Ok(_))
                ),
                "{elsewhere}"
            );
        }
        // Several addresses: the first alone is taken, never a fallback.
        if let Ok(first) =
            zbus::Address::from_str("unix:path=/run/user/1000/bus;unixexec:path=/bin/true")
        {
            assert!(matches!(first.transport(), Transport::Unix(_)), "{first}");
        }
    }

    #[test]
    fn a_folder_is_opened_and_a_file_chosen_in_its_folder() {
        assert_eq!(method_of(Shown::Folder), "OpenFile");
        assert_eq!(method_of(Shown::Item), "OpenDirectory");
    }

    #[test]
    fn the_request_path_is_known_before_the_call() {
        assert_eq!(
            request_path(":1.42", "okapi3"),
            "/org/freedesktop/portal/desktop/request/1_42/okapi3"
        );
    }

    #[test]
    fn the_answer_says_done_cancelled_or_refused() {
        assert!(answered(0).is_ok());
        assert!(matches!(answered(1), Err(Refusal::Cancelled)));
        assert!(matches!(answered(2), Err(Refusal::Refused(2))));
    }
}
