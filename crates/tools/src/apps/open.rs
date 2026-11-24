//! An application open in the session: the scopes a desktop started for it
//! (named by one of its launchers, or a snap's own), and the user's consent
//! to close it passed to the session's systemd, which signals them
//! (invariant 4: Okapi itself sends no signal). A scope Okapi or one of its
//! ancestors runs in (the terminal it was started from) is never one of
//! them; where they run is not known, none is. A flatpak owns its sandbox's
//! scopes alone, and those are never another source's.

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::{Mutex, PoisonError};
#[cfg(target_os = "linux")]
use std::time::Instant;

use okapi_platform::SCOPE_SUFFIX;

use super::uninstall::Target;
#[cfg(target_os = "linux")]
use crate::packages::systemd_session::{Session, SessionError, Signal};

/// How `flatpak run` names the scope of a sandbox: `app-flatpak-<id>-<n>`.
const FLATPAK_SCOPE_PREFIX: &str = "app-flatpak-";

/// A scope of an application open, as the session listed it. Only
/// [`open_scopes`] makes one: nothing else is ever closed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OpenScope(String);

impl OpenScope {
    #[must_use]
    pub fn unit(&self) -> &str {
        &self.0
    }
}

/// How an application open is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Asked to end (`SIGTERM`).
    Asked,
    /// Ended at once (`SIGKILL`), on a second consent.
    Forced,
}

/// The scopes alive in the session, and where Okapi runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scopes {
    pub alive: Vec<String>,
    /// The paths in the unified hierarchy of Okapi and of each of its
    /// ancestors: every unit on one holds what Okapi lives by.
    pub okapi_cgroups: Vec<String>,
}

/// Okapi lives by a scope of the application, or where it runs is not
/// known: closing the application could end Okapi, it is never closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("Okapi lives by a scope of the application, or where it runs is unknown")]
pub struct HoldsOkapi;

/// The scopes of `scopes` that are the application's whose launchers are
/// `launchers` and that `target` uninstalls: a scope whose launcher is one
/// of them, or a snap's scope of that very snap, a flatpak's sandbox
/// scopes for a flatpak alone.
///
/// # Errors
/// One of them is where Okapi or an ancestor runs, or Okapi's lineage is
/// empty: none can be closed (traced).
pub fn open_scopes(
    launchers: &[String],
    target: &Target,
    scopes: &Scopes,
) -> Result<Vec<OpenScope>, HoldsOkapi> {
    let open: Vec<OpenScope> = scopes
        .alive
        .iter()
        .filter(|unit| unit.ends_with(SCOPE_SUFFIX))
        .filter(|unit| {
            unit.starts_with(FLATPAK_SCOPE_PREFIX) == matches!(target, Target::Flatpak(_))
        })
        .filter(|unit| {
            let by_launcher =
                okapi_platform::desktop_id(unit).is_some_and(|id| launchers.contains(&id));
            let by_snap = matches!(target, Target::Snap { name, .. }
                if okapi_platform::scope_snap(unit) == Some(name.as_str()));
            by_launcher || by_snap
        })
        .map(|unit| OpenScope(unit.clone()))
        .collect();
    let holding = open.iter().find(|scope| {
        scopes
            .okapi_cgroups
            .iter()
            .any(|cgroup| cgroup.split('/').any(|part| part == scope.unit()))
    });
    if let Some(holding) = holding {
        tracing::warn!(
            unit = holding.unit(),
            "Okapi lives by a scope of the application: never closed"
        );
        return Err(HoldsOkapi);
    }
    if !open.is_empty() && scopes.okapi_cgroups.is_empty() {
        tracing::warn!("where Okapi runs is unknown: the application is never closed");
        return Err(HoldsOkapi);
    }
    Ok(open)
}

/// The session's systemd did not answer as asked: what failed is traced
/// where it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the session's systemd did not answer as asked")]
pub struct Unreached;

/// The session's scopes, and how one is closed: the systemd of the session
/// on this system, or a test's double.
pub trait Sessions: Send + Sync {
    /// The scopes alive in the session.
    ///
    /// # Errors
    /// The session's systemd did not say, or where Okapi runs is unknown.
    fn scopes(&self) -> Result<Scopes, Unreached>;

    /// Has every process of `scope` signalled as `ending` says.
    ///
    /// # Errors
    /// The session's systemd did not.
    fn close(&self, scope: &OpenScope, ending: Ending) -> Result<(), Unreached>;
}

/// The systemd of this session, its connection kept while it answers.
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
pub struct SystemSessions {
    joined: Mutex<Option<Session>>,
    /// The bus was not joined last time: said once until it is.
    unjoined: AtomicBool,
}

#[cfg(target_os = "linux")]
impl SystemSessions {
    /// `call` on the session joined, joined first if it is not; a bus
    /// that fails lets the connection go, the next call joins again (a
    /// refusal of systemd keeps it).
    fn with_session<T>(
        &self,
        call: impl FnOnce(&Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let mut joined = self.joined.lock().unwrap_or_else(PoisonError::into_inner);
        let session = match joined.take() {
            Some(session) => session,
            None => match Session::connect() {
                Ok(session) => {
                    self.unjoined.store(false, Ordering::Relaxed);
                    session
                }
                Err(error) => {
                    if !self.unjoined.swap(true, Ordering::Relaxed) {
                        tracing::warn!(%error, "the session's systemd not joined");
                    }
                    return Err(error);
                }
            },
        };
        let called = call(&session);
        let bus_failed = matches!(
            &called,
            Err(SessionError::Bus(error)) if !matches!(error, zbus::Error::MethodError(..))
        );
        if !bus_failed {
            *joined = Some(session);
        }
        called
    }
}

#[cfg(target_os = "linux")]
impl Sessions for SystemSessions {
    fn scopes(&self) -> Result<Scopes, Unreached> {
        let started = Instant::now();
        let alive = self.with_session(Session::scopes).map_err(|error| {
            tracing::debug!(%error, "the session's scopes not read");
            Unreached
        })?;
        let okapi_cgroups = okapi_platform::lineage_cgroups().map_err(|error| {
            tracing::warn!(%error, "where Okapi runs is unknown: nothing is closed");
            Unreached
        })?;
        tracing::debug!(scopes = alive.len(), ?okapi_cgroups, elapsed = ?started.elapsed(), "session scopes read");
        Ok(Scopes {
            alive,
            okapi_cgroups,
        })
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "invariant 4: the one way the user's consent to close an application reaches systemd"
    )]
    fn close(&self, scope: &OpenScope, ending: Ending) -> Result<(), Unreached> {
        let signal = match ending {
            Ending::Asked => Signal::Terminate,
            Ending::Forced => Signal::Kill,
        };
        // `kill` traces systemd's refusal, `with_session` a bus not joined.
        match self.with_session(|session| session.kill(scope.unit(), signal)) {
            Ok(()) => Ok(()),
            // Sent, its answer late: it may have gone out, so it counts as
            // asked, never asked twice.
            Err(SessionError::Bus(zbus::Error::InputOutput(error)))
                if error.kind() == std::io::ErrorKind::TimedOut =>
            {
                tracing::warn!(
                    unit = scope.unit(),
                    "systemd answered no signal in time: counted as asked"
                );
                Ok(())
            }
            Err(error) => {
                tracing::debug!(%error, unit = scope.unit(), "scope not closed");
                Err(Unreached)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::flatpak::FlatpakRef;

    const UUID: &str = "0b6f2c1e-4d2a-4c1e-9a7f-1f2e3d4c5b6a";
    const SESSION: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice";

    fn deb() -> Target {
        Target::Deb {
            package: "lmstudio-bionic".to_owned(),
            architecture: "amd64".to_owned(),
        }
    }

    fn snap(name: &str) -> Target {
        Target::Snap {
            name: name.to_owned(),
            purge: false,
        }
    }

    /// The open scopes, Okapi and its ancestors in `okapi_cgroups`.
    fn open_from(
        launchers: &[&str],
        target: &Target,
        alive: &[String],
        okapi_cgroups: &[String],
    ) -> Result<Vec<String>, HoldsOkapi> {
        let launchers: Vec<String> = launchers
            .iter()
            .map(|&launcher| launcher.to_owned())
            .collect();
        let scopes = Scopes {
            alive: alive.to_vec(),
            okapi_cgroups: okapi_cgroups.to_vec(),
        };
        open_scopes(&launchers, target, &scopes)
            .map(|open| open.iter().map(|scope| scope.unit().to_owned()).collect())
    }

    /// The open scopes, Okapi running in a scope of its own.
    fn open(launchers: &[&str], target: &Target, alive: &[String]) -> Vec<String> {
        open_from(
            launchers,
            target,
            alive,
            &[
                format!("{SESSION}/app-gnome-okapi-1.scope"),
                "/init.scope".to_owned(),
            ],
        )
        .unwrap()
    }

    fn flatpak(id: &str) -> Target {
        Target::Flatpak(FlatpakRef::app(id, "x86_64", "stable").unwrap())
    }

    fn units(units: &[&str]) -> Vec<String> {
        units.iter().map(|&unit| unit.to_owned()).collect()
    }

    #[test]
    fn a_scope_named_by_one_of_its_launchers_is_the_application_s() {
        // Chrome's main process runs in a scope named by its launcher kept
        // from the menu.
        let alive = units(&[
            r"app-gnome-google\x2dchrome-30780.scope",
            "app-com.google.Chrome-30780.scope",
            r"app-gnome-google\x2dchrome\x2dbeta-1.scope",
            "app-gnome-google-1.scope",
            "init.scope",
        ]);
        assert_eq!(
            open(&["google-chrome", "com.google.Chrome"], &deb(), &alive),
            alive[..2]
        );
        let alive = units(&[
            "app-gnome-ai.elementlabs.bionic-1234.scope",
            "app-gnome-ai.elementlabs.bionic2-1.scope",
            "app-gnome-ai.elementlabs.bionic@1.service",
        ]);
        assert_eq!(open(&["ai.elementlabs.bionic"], &deb(), &alive), alive[..1]);
    }

    #[test]
    fn a_flatpak_s_scope_is_its_by_its_whole_id() {
        let alive = units(&[
            "app-flatpak-org.gimp.GIMP-12.scope",
            "app-flatpak-org.gimp.GIMP.Plugin-13.scope",
        ]);
        assert_eq!(
            open(&["org.gimp.GIMP"], &flatpak("org.gimp.GIMP"), &alive),
            alive[..1]
        );
    }

    #[test]
    fn a_deb_and_a_flatpak_of_one_id_never_close_each_other() {
        // Both installed: the deb's launcher bears the flatpak's id.
        let alive = units(&[
            "app-flatpak-org.gnome.TextEditor-5.scope",
            "app-gnome-org.gnome.TextEditor-6.scope",
        ]);
        let launchers = ["org.gnome.TextEditor"];
        assert_eq!(open(&launchers, &deb(), &alive), alive[1..]);
        assert_eq!(
            open(&launchers, &flatpak("org.gnome.TextEditor"), &alive),
            alive[..1]
        );
    }

    #[test]
    fn a_snap_s_own_scopes_are_its_never_another_instance_s() {
        let alive = units(&[
            &format!("snap.firefox.firefox-{UUID}.scope"),
            &format!("snap.firefox.geckodriver-{UUID}.scope"),
            "app-gnome-firefox_firefox-1.scope",
            &format!("snap.firefox_beta.firefox-{UUID}.scope"),
            "snap.firefox.firefox.service",
        ]);
        assert_eq!(
            open(&["firefox_firefox"], &snap("firefox"), &alive),
            alive[..3]
        );
        assert_eq!(
            open(&[], &snap("firefox"), &alive),
            alive[..2],
            "without a launcher, by its own scopes"
        );
        assert_eq!(
            open(&[], &deb(), &alive),
            Vec::<String>::new(),
            "a snap's scope is a snap's"
        );
    }

    #[test]
    fn an_application_okapi_or_an_ancestor_runs_in_is_never_closed_at_all() {
        // Another window of it open too: closing that one alone would say
        // it closed while it is open.
        let alive = units(&["app-gnome-code-1.scope", "app-gnome-code-2.scope"]);
        let okapi_in = |cgroup: String| [cgroup, "/init.scope".to_owned()];
        assert_eq!(
            open_from(
                &["code"],
                &deb(),
                &alive,
                &okapi_in(format!("{SESSION}/app-gnome-code-1.scope"))
            ),
            Err(HoldsOkapi)
        );
        // In a cgroup the application made below its scope.
        assert_eq!(
            open_from(
                &["code"],
                &deb(),
                &alive,
                &okapi_in(format!("{SESSION}/app-gnome-code-2.scope/terminal"))
            ),
            Err(HoldsOkapi)
        );
        // Started from the application's terminal: closing it would end
        // the shell Okapi runs under.
        let alive = units(&["app-gnome-org.gnome.Console-7.scope"]);
        assert_eq!(
            open_from(
                &["org.gnome.Console"],
                &deb(),
                &alive,
                &[
                    format!("{SESSION}/vte-spawn-1.scope"),
                    format!("{SESSION}/vte-spawn-1.scope"),
                    format!("{SESSION}/app-gnome-org.gnome.Console-7.scope"),
                    "/init.scope".to_owned(),
                ]
            ),
            Err(HoldsOkapi)
        );
        // Where Okapi runs unknown: never closed.
        assert_eq!(
            open_from(&["code"], &deb(), &alive_code(), &[]),
            Err(HoldsOkapi)
        );
        // Not open, it needs no lineage.
        assert_eq!(open_from(&["code"], &deb(), &[], &[]), Ok(Vec::new()));
    }

    fn alive_code() -> Vec<String> {
        units(&["app-gnome-code-1.scope"])
    }
}
