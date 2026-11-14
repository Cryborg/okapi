//! The desktop portal on the session bus: shows a folder, or a file in its
//! folder, in the file manager (`org.freedesktop.portal.OpenURI`), lets the
//! user choose a folder (`org.freedesktop.portal.FileChooser`), and tells
//! whether the desktop prefers dark (`org.freedesktop.portal.Settings`). No
//! command is run (invariant 4). `OpenURI` takes a descriptor, never a
//! `file://` address; the portal answers each call by a `Response` signal on
//! its request.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_lite::StreamExt as _;
use zbus::address::Transport;
use zbus::proxy::SignalStream;
use zbus::zvariant::{Fd, OwnedObjectPath, OwnedValue, Value};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const OBJECT: &str = "/org/freedesktop/portal/desktop";
const OPEN_URI: &str = "org.freedesktop.portal.OpenURI";
const FILE_CHOOSER: &str = "org.freedesktop.portal.FileChooser";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
/// Where the desktop says its colour scheme, and what says dark.
const APPEARANCE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME: &str = "color-scheme";
const PREFERS_DARK: u32 = 1;
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const REQUESTS: &str = "/org/freedesktop/portal/desktop/request";
const RESPONSE: &str = "Response";
/// What the file chooser answers: the addresses chosen.
const CHOSEN: &str = "uris";
/// The longest a call to the bus waits for its reply (not for the answer,
/// which may wait on the user).
const CALL_WAIT: Duration = Duration::from_secs(10);
/// What the portal answers: done, or the user cancelled; anything else it
/// refused.
const ANSWER_DONE: u32 = 0;
const ANSWER_CANCELLED: u32 = 1;
/// The address of a local file.
const FILE_SCHEME: &str = "file://";
/// The host a local file's address may name.
const LOCAL_HOST: &str = "localhost";

/// How the object is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shown {
    /// A folder, opened (`OpenFile` on a directory).
    Folder,
    /// A file, in its folder, chosen there (`OpenDirectory`).
    Item,
}

/// Why a call to the portal gave nothing: the file manager did not open,
/// the folder chooser gave no folder, or the colour scheme is unknown.
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
    /// The file chooser answered without a local folder: none, several, or
    /// an address that is not a local path (the answer is traced).
    NoFolder,
    /// The desktop's colour scheme is not a number.
    NoScheme,
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
            Self::NoFolder => write!(formatter, "no local folder in the answer"),
            Self::NoScheme => write!(formatter, "the colour scheme is not a number"),
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

/// The session bus, joined by a socket only.
async fn connect() -> Result<zbus::Connection, Refusal> {
    let address = socket_only(zbus::Address::session()?)?;
    Ok(zbus::connection::Builder::address(address)?
        .method_timeout(CALL_WAIT)
        .build()
        .await?)
}

/// The path of the request the portal makes for a call of `sender` with
/// `token`, known before the call so that its answer is never missed.
pub(crate) fn request_path(sender: &str, token: &str) -> String {
    let sender = sender.trim_start_matches(':').replace('.', "_");
    format!("{REQUESTS}/{sender}/{token}")
}

/// A call to the portal under way: its connection, the options that name
/// its request, and the answers awaited there before the call is made, so
/// that none is missed.
struct Request {
    connection: zbus::Connection,
    token: String,
    expected: String,
    answers: SignalStream<'static>,
}

impl Request {
    /// Joins the session bus and awaits the answer of the next call.
    async fn prepare() -> Result<Self, Refusal> {
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let connection = connect().await?;
        let token = format!("okapi{}", CALLS.fetch_add(1, Ordering::Relaxed));
        let sender = connection
            .unique_name()
            .ok_or(Refusal::Unnamed)?
            .to_string();
        let expected = request_path(&sender, &token);
        let request = zbus::Proxy::new_owned(
            connection.clone(),
            DESTINATION,
            expected.clone(),
            REQUEST_INTERFACE,
        )
        .await?;
        let answers = request.receive_signal(RESPONSE).await?;
        Ok(Self {
            connection,
            token,
            expected,
            answers,
        })
    }

    /// The portal's `interface`.
    async fn portal(&self, interface: &'static str) -> Result<zbus::Proxy<'static>, Refusal> {
        Ok(zbus::Proxy::new_owned(self.connection.clone(), DESTINATION, OBJECT, interface).await?)
    }

    /// The options of the call, which name its request.
    fn options(&self) -> HashMap<&'static str, Value<'_>> {
        HashMap::from([("handle_token", Value::from(self.token.as_str()))])
    }

    /// The portal's answer to the call that `made` the request: its results
    /// when it is done.
    async fn answer(self, made: &OwnedObjectPath) -> Result<HashMap<String, OwnedValue>, Refusal> {
        let mut answers = if made.as_str() == self.expected {
            self.answers
        } else {
            // An old portal: its answer comes on the request it made, listened
            // to only now (an answer given at once would be missed).
            tracing::warn!(%made, expected = %self.expected, "the portal's request is not where it was awaited: listened to where it is");
            zbus::Proxy::new_owned(
                self.connection.clone(),
                DESTINATION,
                made.clone(),
                REQUEST_INTERFACE,
            )
            .await?
            .receive_signal(RESPONSE)
            .await?
        };
        let answer = answers.next().await.ok_or(Refusal::NoAnswer)?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = answer.body().deserialize()?;
        answered(code).map(|()| results)
    }
}

/// Shows `object` (a descriptor that only names it) as `shown`, and waits
/// for the portal's answer.
pub(crate) async fn show(object: OwnedFd, shown: Shown) -> Result<(), Refusal> {
    let request = Request::prepare().await?;
    let made: OwnedObjectPath = request
        .portal(OPEN_URI)
        .await?
        .call(
            method_of(shown),
            &("", Fd::from(&object), request.options()),
        )
        .await?;
    request.answer(&made).await.map(drop)
}

/// Asks the user to choose a folder, in a window titled `title`, and waits
/// for it: the folder chosen.
pub(crate) async fn choose_folder(title: &str) -> Result<PathBuf, Refusal> {
    let request = Request::prepare().await?;
    let mut options = request.options();
    options.insert("directory", Value::from(true));
    let made: OwnedObjectPath = request
        .portal(FILE_CHOOSER)
        .await?
        .call("OpenFile", &("", title, options))
        .await?;
    let results = request.answer(&made).await?;
    let addresses = results
        .get(CHOSEN)
        .and_then(|value| Vec::<String>::try_from(value.try_clone().ok()?).ok());
    match addresses.as_deref() {
        Some([address]) => path_of_uri(address).ok_or_else(|| {
            tracing::warn!(%address, "the folder chosen is not a local path");
            Refusal::NoFolder
        }),
        other => {
            tracing::warn!(?other, "the file chooser answered no single folder");
            Err(Refusal::NoFolder)
        }
    }
}

/// Whether the desktop prefers dark, as it says now: a later change is not
/// followed.
pub(crate) async fn desktop_prefers_dark() -> Result<bool, Refusal> {
    let connection = connect().await?;
    let settings = zbus::Proxy::new(&connection, DESTINATION, OBJECT, SETTINGS).await?;
    let scheme: OwnedValue = settings
        .call("ReadOne", &(APPEARANCE, COLOR_SCHEME))
        .await?;
    u32::try_from(scheme)
        .map(prefers_dark)
        .map_err(|_| Refusal::NoScheme)
}

/// Whether the desktop's colour `scheme` says dark: no preference (0) and
/// light (2) do not.
const fn prefers_dark(scheme: u32) -> bool {
    scheme == PREFERS_DARK
}

/// The local path a `file://` address names (no host, or `localhost`), its
/// escapes (`%20`) read as the bytes they stand for; none for another
/// address, one malformed, or one that would hide a NUL.
pub(crate) fn path_of_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix(FILE_SCHEME)?;
    let path = rest.strip_prefix(LOCAL_HOST).unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut escaped = path.bytes();
    while let Some(byte) = escaped.next() {
        let byte = match byte {
            b'%' => {
                let digit = |byte: Option<u8>| char::from(byte?).to_digit(16);
                let value = digit(escaped.next())? << 4 | digit(escaped.next())?;
                u8::try_from(value).ok()?
            }
            // A query or a fragment: never part of a local file's address.
            b'?' | b'#' => return None,
            byte => byte,
        };
        if byte == 0 {
            return None;
        }
        bytes.push(byte);
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
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
    fn a_local_address_is_read_as_its_path() {
        let path = |uri| path_of_uri(uri).map(|path| path.into_os_string().into_vec());
        assert_eq!(
            path("file:///home/ana/Mes%20projets/%C3%A9t%C3%A9"),
            Some("/home/ana/Mes projets/été".into())
        );
        assert_eq!(path("file://localhost/data"), Some("/data".into()));
        // Bytes that are not UTF-8 are a name all the same.
        assert_eq!(path("file:///data/%FF"), Some(b"/data/\xff".to_vec()));
        for refused in [
            "file://server/data",
            "file:data",
            "file://",
            "https://example.org/data",
            "/data",
            "file:///data%2",
            "file:///data%zz",
            "file:///data%00x",
            "file:///data?x",
            "file:///data#x",
        ] {
            assert_eq!(path(refused), None, "{refused}");
        }
    }

    #[test]
    fn only_a_scheme_that_prefers_dark_is_dark() {
        assert!(prefers_dark(1));
        for not_dark in [0, 2, 3] {
            assert!(!prefers_dark(not_dark), "{not_dark}");
        }
    }

    #[test]
    fn the_answer_says_done_cancelled_or_refused() {
        assert!(answered(0).is_ok());
        assert!(matches!(answered(1), Err(Refusal::Cancelled)));
        assert!(matches!(answered(2), Err(Refusal::Refused(2))));
    }
}
