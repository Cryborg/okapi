//! A bus, the system's or the session's, joined by a socket only: an
//! address that is not one (`unixexec:`) would run a program to reach it.

use std::time::Duration;

use zbus::address::Transport;

/// The longest a call that answers at once waits, the daemon's start by the
/// bus included; flatpak's client also bounds joining the bus by it.
pub const CALL_WAIT: Duration = Duration::from_secs(10);
/// What the bus answers for a service nobody provides.
pub const UNKNOWN_SERVICE: [&str; 2] = [
    "org.freedesktop.DBus.Error.ServiceUnknown",
    "org.freedesktop.DBus.Error.NameHasNoOwner",
];

/// Why a bus was not joined.
#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error(transparent)]
    Bus(#[from] zbus::Error),
    #[error("the bus not joined in {0:?}")]
    Unanswered(Duration),
}

/// The bus at `address`, joined within `wait`: a socket that never answers
/// would hold the thread forever. Each call then waits `calls_wait` at
/// most; `None` leaves the bound to the caller.
///
/// # Errors
/// The bus's refusal, or no answer within `wait`.
pub fn joined(
    address: zbus::Address,
    wait: Duration,
    calls_wait: Option<Duration>,
) -> Result<zbus::Connection, JoinError> {
    let mut builder = zbus::connection::Builder::address(address)?;
    if let Some(calls_wait) = calls_wait {
        builder = builder.method_timeout(calls_wait);
    }
    let built = async { builder.build().await.map_err(JoinError::from) };
    let deadline = async {
        async_io::Timer::after(wait).await;
        Err(JoinError::Unanswered(wait))
    };
    futures_lite::future::block_on(futures_lite::future::or(built, deadline))
}

/// The bus at `address`, if it is reached by a socket only; otherwise the
/// address refused.
///
/// # Errors
/// The address, when its transport is not a socket.
pub fn socket_only(address: zbus::Address) -> Result<zbus::Address, String> {
    if matches!(address.transport(), Transport::Unix(_)) {
        Ok(address)
    } else {
        Err(address.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use super::*;

    #[test]
    fn a_bus_that_never_answers_is_joined_no_longer_than_its_wait() {
        // Catches a connection without a bound: a socket that accepts and
        // never speaks.
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("bus");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let held = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));
        let address = zbus::Address::from_str(&format!("unix:path={}", socket.display())).unwrap();
        assert!(matches!(
            joined(address, Duration::from_millis(200), None),
            Err(JoinError::Unanswered(_))
        ));
        drop(held);
    }

    #[test]
    fn the_bus_is_joined_by_a_socket_only() {
        let address = |text| zbus::Address::from_str(text).unwrap();
        assert!(socket_only(address("unix:path=/run/dbus/system_bus_socket")).is_ok());
        assert!(socket_only(address("unix:abstract=/tmp/dbus-x")).is_ok());
        for runs_something in ["unixexec:path=/bin/true", "tcp:host=localhost,port=4000"] {
            assert_eq!(
                socket_only(address(runs_something)).map_err(|refused| refused.is_empty()),
                Err(false),
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
            "unixexec:path=/bin/true;unix:path=/run/dbus/system_bus_socket",
        ] {
            assert!(
                !matches!(
                    zbus::Address::from_str(elsewhere).map(socket_only),
                    Ok(Ok(_))
                ),
                "{elsewhere}"
            );
        }
        // Several addresses: zbus reads the whole as one socket's path, never
        // as a fallback to the second: a socket all the same, nothing run.
        let several =
            zbus::Address::from_str("unix:path=/run/user/1000/bus;unixexec:path=/bin/true")
                .unwrap();
        let joined = socket_only(several).unwrap();
        assert!(matches!(joined.transport(), Transport::Unix(_)), "{joined}");
    }
}
