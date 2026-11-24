//! The systemd of the user's session, on the session bus: the scopes alive
//! in the session (`ListUnitsByPatterns`), and a signal to every process of
//! one (`KillUnit`), requests fixed here (invariant 4): Okapi itself never
//! sends a signal. Each call waits [`bus::CALL_WAIT`] at most.

use std::time::Instant;

use super::bus;

const DESTINATION: &str = "org.freedesktop.systemd1";
const OBJECT: &str = "/org/freedesktop/systemd1";
const INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const LIST_UNITS: &str = "ListUnitsByPatterns";
const KILL_UNIT: &str = "KillUnit";
/// The states of a unit whose processes may still run.
const ALIVE: [&str; 3] = ["active", "activating", "deactivating"];
const SCOPES: &str = "*.scope";
/// Every process of the unit, not its main one alone.
const EVERY_PROCESS: &str = "all";
/// What systemd answers for a scope gone, or left with no process: it
/// closed meanwhile.
const GONE: [&str; 2] = [
    "org.freedesktop.systemd1.NoSuchUnit",
    "org.freedesktop.systemd1.NoSuchProcess",
];

/// A unit as `ListUnitsByPatterns` lists it: name, description, load
/// state, active state, sub state, following, object, job id, job type,
/// job object.
type Unit = (
    String,
    String,
    String,
    String,
    String,
    String,
    zbus::zvariant::OwnedObjectPath,
    u32,
    String,
    zbus::zvariant::OwnedObjectPath,
);

/// The signal a scope's processes get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// `SIGTERM`: asked to end.
    Terminate,
    /// `SIGKILL`: ended at once.
    Kill,
}

impl Signal {
    const fn number(self) -> i32 {
        const SIGTERM: i32 = 15;
        const SIGKILL: i32 = 9;
        match self {
            Self::Terminate => SIGTERM,
            Self::Kill => SIGKILL,
        }
    }
}

/// Why the session's systemd did not answer as asked.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The session bus is not reached by a socket.
    #[error("the session bus is not reached by a socket: {0}")]
    Transport(String),
    #[error(transparent)]
    Joining(#[from] bus::JoinError),
    /// Never asked: not a scope.
    #[error("not a scope: {0}")]
    NotAScope(String),
    #[error(transparent)]
    Bus(#[from] zbus::Error),
}

/// The systemd of the session, joined on the session bus.
#[derive(Debug, Clone)]
pub struct Session {
    connection: zbus::Connection,
}

impl Session {
    /// Joins the session bus, waiting [`bus::CALL_WAIT`] at most, and as
    /// much for each call.
    ///
    /// # Errors
    /// A bus that is not a socket, that cannot be joined, or that does not
    /// answer in time.
    pub fn connect() -> Result<Self, SessionError> {
        let address =
            bus::socket_only(zbus::Address::session()?).map_err(SessionError::Transport)?;
        Ok(Self {
            connection: bus::joined(address, bus::CALL_WAIT, Some(bus::CALL_WAIT))?,
        })
    }

    /// The scopes alive in the session, by name.
    ///
    /// # Errors
    /// The bus's or systemd's refusal.
    pub fn scopes(&self) -> Result<Vec<String>, SessionError> {
        let reply = futures_lite::future::block_on(self.connection.call_method(
            Some(DESTINATION),
            OBJECT,
            Some(INTERFACE),
            LIST_UNITS,
            &(ALIVE.as_slice(), [SCOPES].as_slice()),
        ))?;
        let units: Vec<Unit> = reply.body().deserialize()?;
        Ok(units.into_iter().map(|unit| unit.0).collect())
    }

    /// Has systemd send `signal` to every process of the scope `unit`; one
    /// gone meanwhile is no failure.
    ///
    /// # Errors
    /// `unit` is no scope (nothing asked), or the bus's or systemd's
    /// refusal.
    pub fn kill(&self, unit: &str, signal: Signal) -> Result<(), SessionError> {
        if !unit.ends_with(okapi_platform::SCOPE_SUFFIX) || unit.contains('/') {
            tracing::error!(unit, "not a scope: nothing asked");
            return Err(SessionError::NotAScope(unit.to_owned()));
        }
        let started = Instant::now();
        let asked = futures_lite::future::block_on(self.connection.call_method(
            Some(DESTINATION),
            OBJECT,
            Some(INTERFACE),
            KILL_UNIT,
            &(unit, EVERY_PROCESS, signal.number()),
        ));
        match asked {
            Ok(_) => {
                tracing::info!(unit, ?signal, elapsed = ?started.elapsed(), "scope signalled by systemd");
                Ok(())
            }
            Err(zbus::Error::MethodError(name, _, _)) if GONE.contains(&name.as_str()) => {
                tracing::debug!(unit, error = %name, "scope gone before its signal");
                Ok(())
            }
            Err(error) => {
                tracing::warn!(unit, ?signal, %error, "scope not signalled");
                Err(error.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::fake_bus;

    fn unit(name: &str) -> Unit {
        let path = zbus::zvariant::OwnedObjectPath::try_from("/").unwrap();
        (
            name.to_owned(),
            String::new(),
            "loaded".to_owned(),
            "active".to_owned(),
            "running".to_owned(),
            String::new(),
            path.clone(),
            0,
            String::new(),
            path,
        )
    }

    #[test]
    fn the_scopes_alive_are_asked_by_their_pattern_and_read_by_name() {
        let (connection, heard) = fake_bus::serving(|call| {
            fake_bus::reply(
                call,
                &(vec![unit("app-gnome-x-1.scope"), unit("init.scope")],),
            )
        });
        let session = Session { connection };
        assert_eq!(
            session.scopes().unwrap(),
            ["app-gnome-x-1.scope", "init.scope"]
        );
        let call = heard.recv().unwrap();
        assert_eq!(
            fake_bus::addressed(&call),
            [DESTINATION, OBJECT, INTERFACE, LIST_UNITS, "(asas)"].map(str::to_owned)
        );
        let (states, patterns): (Vec<String>, Vec<String>) = call.body().deserialize().unwrap();
        assert_eq!(states, ALIVE);
        assert_eq!(patterns, [SCOPES]);
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a fake systemd: nothing signalled"
    )]
    fn a_scope_is_signalled_whole_by_the_signal_asked() {
        let (connection, heard) = fake_bus::serving(|call| fake_bus::reply(call, &()));
        let session = Session { connection };
        for (signal, number) in [(Signal::Terminate, 15), (Signal::Kill, 9)] {
            session.kill("app-gnome-x-1.scope", signal).unwrap();
            let call = heard.recv().unwrap();
            assert_eq!(
                fake_bus::addressed(&call),
                [DESTINATION, OBJECT, INTERFACE, KILL_UNIT, "(ssi)"].map(str::to_owned)
            );
            let asked: (String, String, i32) = call.body().deserialize().unwrap();
            assert_eq!(
                asked,
                ("app-gnome-x-1.scope".to_owned(), "all".to_owned(), number)
            );
        }
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a fake systemd: nothing signalled"
    )]
    fn a_unit_that_is_no_scope_is_never_asked() {
        let (connection, heard) = fake_bus::serving(|call| fake_bus::reply(call, &()));
        let session = Session { connection };
        for unit in [
            "gnome-shell.service",
            "../x.scope/y",
            "../x.scope",
            "a/b.scope",
            "",
        ] {
            assert!(matches!(
                session.kill(unit, Signal::Kill),
                Err(SessionError::NotAScope(_))
            ));
        }
        assert!(heard.try_recv().is_err(), "nothing sent");
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a fake systemd: nothing signalled"
    )]
    fn a_scope_gone_meanwhile_is_no_failure_and_a_refusal_is() {
        let (connection, _heard) = fake_bus::serving(|call| {
            let (unit, _, _): (String, String, i32) = call.body().deserialize().unwrap();
            match unit.as_str() {
                "gone-1.scope" => fake_bus::error(call, GONE[0]),
                "empty-1.scope" => fake_bus::error(call, GONE[1]),
                _ => fake_bus::error(call, "org.freedesktop.DBus.Error.AccessDenied"),
            }
        });
        let session = Session { connection };
        assert!(session.kill("gone-1.scope", Signal::Terminate).is_ok());
        assert!(session.kill("empty-1.scope", Signal::Terminate).is_ok());
        assert!(matches!(
            session.kill("other-1.scope", Signal::Terminate),
            Err(SessionError::Bus(_))
        ));
    }
}
