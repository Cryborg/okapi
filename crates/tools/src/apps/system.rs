//! The system's services as an [`Uninstaller`]: PackageKit for a deb (its
//! package found installed, the removal simulated, then made), snapd for a
//! snap (its change followed to its end). dpkg's state is read from its
//! files. Nothing here runs a command; the rights are asked by the
//! services' own polkit windows.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::time::Duration;

use okapi_rules::Language;

use super::dpkg::{self, Status};
use super::snap::{is_snap_name, read_snaps};
use super::uninstall::{Ended, Going, Step, Target, Uninstaller, Why};
use super::{Reading, SourceReading};
use crate::packages::packagekit::{
    self, PackageId, PackageKit, PackageKitError, Removing, error_code,
};
use crate::packages::snapd::{self, CHANGE_DONE, Snapd, SnapdError};

/// How often a change of snapd is asked where it stands.
const CHANGE_LOOK_EVERY: Duration = Duration::from_millis(500);

/// PackageKit, snapd and dpkg's files, on this system.
#[derive(Debug, Clone)]
pub struct Services {
    locale: &'static str,
    dpkg: PathBuf,
    snapd: Snapd,
}

impl Services {
    /// The services, PackageKit's texts in the session's language: the
    /// screen shows none of them but a refusal in its own words.
    #[must_use]
    pub fn session() -> Self {
        Self::new(Language::of_session(|name| std::env::var_os(name)))
    }

    /// The services, PackageKit's texts in `language`.
    #[must_use]
    pub fn new(language: Language) -> Self {
        Self {
            locale: crate::packagekit_locale(language),
            dpkg: PathBuf::from(dpkg::DPKG_DIRECTORY),
            snapd: Snapd::system(),
        }
    }

    fn packagekit(&self) -> Result<PackageKit, Why> {
        PackageKit::connect(self.locale).map_err(|error| why_packagekit(&error))
    }

    /// The identifier PackageKit gives `package` installed for
    /// `architecture`: exactly one, or none.
    fn installed_id(
        packagekit: &PackageKit,
        package: &str,
        architecture: &str,
    ) -> Result<String, Why> {
        let resolved = packagekit
            .resolve(package)
            .map_err(|error| why_packagekit(&error))?;
        let mut matching: Vec<String> = resolved
            .into_iter()
            .filter(|found| found.info == packagekit::info::INSTALLED)
            .filter(|found| {
                PackageId::parse(&found.id).is_some_and(|id| {
                    id.name == package
                        && (id.arch == architecture || id.arch == "all" || architecture == "all")
                })
            })
            .map(|found| found.id)
            .collect();
        if matching.len() != 1 {
            tracing::warn!(
                package,
                architecture,
                ?matching,
                "the package installed not found once by PackageKit"
            );
            return Err(Why::NotFound);
        }
        Ok(matching.remove(0))
    }
}

impl Uninstaller for Services {
    fn simulate(&self, target: &Target) -> Result<Vec<Going>, Why> {
        match target {
            Target::Snap { name, .. } => Ok(vec![Going {
                id: name.clone(),
                name: name.clone(),
                architecture: String::new(),
                bytes: None,
            }]),
            Target::Deb {
                package,
                architecture,
            } => {
                let packagekit = self.packagekit()?;
                let id = Self::installed_id(&packagekit, package, architecture)?;
                let going = packagekit
                    .simulate_remove(&[id])
                    .map_err(|error| why_packagekit(&error))?;
                let status = self.status()?;
                going
                    .into_iter()
                    .map(|package| {
                        let Some(id) = PackageId::parse(&package.id) else {
                            tracing::warn!(id = %package.id, "a package PackageKit names unreadably");
                            return Err(Why::Service(package.id.clone()));
                        };
                        let bytes = status
                            .installed
                            .iter()
                            .find(|installed| installed.name == id.name && installed.architecture == id.arch)
                            .and_then(|installed| installed.installed_bytes);
                        Ok(Going {
                            id: package.id.clone(),
                            name: id.name.to_owned(),
                            architecture: id.arch.to_owned(),
                            bytes,
                        })
                    })
                    .collect()
            }
        }
    }

    fn status(&self) -> Result<Status, Why> {
        match dpkg::pending_updates(&self.dpkg) {
            Ok(0) => {}
            Ok(pending) => {
                tracing::info!(pending, "dpkg is writing: its state read later");
                return Err(Why::Busy);
            }
            Err(error) => {
                tracing::warn!(%error, "dpkg's updates unreadable");
                return Err(Why::Service(error.to_string()));
            }
        }
        std::fs::read_to_string(self.dpkg.join("status"))
            .map(|text| dpkg::parse_status(&text))
            .map_err(|error| {
                tracing::warn!(%error, "dpkg's status unreadable");
                Why::Service(error.to_string())
            })
    }

    fn remove(
        &self,
        target: &Target,
        going: &[Going],
        steps: &Sender<Step>,
        cancel: &AtomicBool,
    ) -> Ended {
        match target {
            Target::Deb { .. } => {
                let packagekit = match self.packagekit() {
                    Ok(packagekit) => packagekit,
                    Err(why) => return Ended::Failed(why),
                };
                let ids: Vec<String> = going.iter().map(|package| package.id.clone()).collect();
                let heard = |removing| {
                    let _ = steps.send(match removing {
                        Removing::Waiting(true) => Step::WaitingForLock,
                        Removing::Waiting(false) => return,
                        Removing::Cancellable(can) => Step::Cancellable(can),
                    });
                };
                match packagekit.remove_packages(&ids, &heard, cancel) {
                    Ok(()) => Ended::Done,
                    Err(PackageKitError::Failed {
                        code: error_code::TRANSACTION_CANCELLED,
                        ..
                    }) => Ended::Cancelled,
                    Err(error) => Ended::Failed(why_packagekit(&error)),
                }
            }
            Target::Snap { name, purge } => self.remove_snap(name, *purge),
        }
    }
}

impl Services {
    /// Removes the snap `name`, then follows the change snapd started.
    fn remove_snap(&self, name: &str, purge: bool) -> Ended {
        if !is_snap_name(name) {
            tracing::error!(name, "a snap's name refused: nothing asked");
            return Ended::Failed(Why::NotFound);
        }
        let change = match self.snapd.remove(name, purge) {
            Ok(change) => change,
            Err(error) => {
                tracing::warn!(name, %error, "snapd refused the removal");
                return Ended::Failed(why_snapd(&error));
            }
        };
        tracing::info!(name, purge, change, "snapd removes the snap");
        loop {
            std::thread::sleep(CHANGE_LOOK_EVERY);
            match self.snapd.change(&change) {
                Ok(state) if !state.ready => {}
                Ok(state) if state.status == CHANGE_DONE => return Ended::Done,
                Ok(state) => {
                    tracing::warn!(name, change, status = %state.status, err = ?state.err, "snapd's removal did not end done");
                    return Ended::Failed(Why::Service(state.err.unwrap_or(state.status)));
                }
                Err(error) => {
                    tracing::warn!(name, change, %error, "snapd's change unreadable: the outcome unknown");
                    return Ended::Failed(Why::Unanswered);
                }
            }
        }
    }
}

/// The snaps snapd lists, their data under `home`.
#[must_use]
pub fn snaps(home: Option<&Path>, reading: &Reading) -> SourceReading {
    match Snapd::system().snaps() {
        Ok(result) => read_snaps(&result, Path::new("/"), home, reading),
        Err(SnapdError::Absent { .. }) => {
            tracing::info!("no snapd");
            SourceReading::Absent
        }
        Err(error) => {
            tracing::warn!(%error, "snapd's list not read");
            SourceReading::Failed(error.to_string())
        }
    }
}

/// Why, as the screen says it, PackageKit gave no answer.
fn why_packagekit(error: &PackageKitError) -> Why {
    match error {
        PackageKitError::Absent => Why::Absent,
        PackageKitError::Failed { code, details } => match *code {
            error_code::NOT_AUTHORIZED => Why::NotAuthorized,
            error_code::CANNOT_GET_LOCK | error_code::LOCK_REQUIRED => Why::Busy,
            error_code::PACKAGE_NOT_INSTALLED => Why::NotFound,
            _ => Why::Service(details.clone()),
        },
        PackageKitError::TimedOut { waiting: true, .. } => Why::Busy,
        PackageKitError::TimedOut { waiting: false, .. } => Why::TimedOut,
        PackageKitError::Transport(_)
        | PackageKitError::Bus(_)
        | PackageKitError::Vanished
        | PackageKitError::Unreadable { .. }
        | PackageKitError::Ended { .. } => Why::Unanswered,
    }
}

/// Why, as the screen says it, snapd gave no answer.
fn why_snapd(error: &SnapdError) -> Why {
    match error {
        SnapdError::Absent { .. } => Why::Absent,
        error if error.timed_out() => Why::TimedOut,
        SnapdError::Refused {
            kind,
            status,
            message,
        } => match kind.as_str() {
            snapd::KIND_AUTH_CANCELLED | snapd::KIND_LOGIN_REQUIRED => Why::NotAuthorized,
            snapd::KIND_CHANGE_CONFLICT => Why::Busy,
            snapd::KIND_NOT_INSTALLED => Why::NotFound,
            _ if matches!(status, 401 | 403) => Why::NotAuthorized,
            _ => Why::Service(message.clone()),
        },
        SnapdError::Io(_)
        | SnapdError::Truncated { .. }
        | SnapdError::TooLarge
        | SnapdError::Malformed(_)
        | SnapdError::Json(_) => Why::Unanswered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(status: u16, kind: &str) -> SnapdError {
        SnapdError::Refused {
            status,
            kind: kind.to_owned(),
            message: "words".to_owned(),
        }
    }

    #[test]
    fn each_refusal_of_snapd_is_told_by_its_kind() {
        for (error, why) in [
            (refused(403, snapd::KIND_AUTH_CANCELLED), Why::NotAuthorized),
            (refused(401, snapd::KIND_LOGIN_REQUIRED), Why::NotAuthorized),
            (refused(409, snapd::KIND_CHANGE_CONFLICT), Why::Busy),
            (refused(404, snapd::KIND_NOT_INSTALLED), Why::NotFound),
            (refused(403, ""), Why::NotAuthorized),
            (refused(500, "other"), Why::Service("words".to_owned())),
            (SnapdError::Malformed("x"), Why::Unanswered),
        ] {
            assert_eq!(why_snapd(&error), why, "{error}");
        }
    }

    #[test]
    fn each_failure_of_packagekit_is_told_by_its_code() {
        let failed = |code| PackageKitError::Failed {
            code,
            details: "words".to_owned(),
        };
        for (error, why) in [
            (failed(error_code::NOT_AUTHORIZED), Why::NotAuthorized),
            (failed(error_code::CANNOT_GET_LOCK), Why::Busy),
            (failed(error_code::LOCK_REQUIRED), Why::Busy),
            (failed(error_code::PACKAGE_NOT_INSTALLED), Why::NotFound),
            (failed(4), Why::Service("words".to_owned())),
            (PackageKitError::Absent, Why::Absent),
            (
                PackageKitError::TimedOut {
                    waited: Duration::from_secs(1),
                    waiting: true,
                },
                Why::Busy,
            ),
            (
                PackageKitError::TimedOut {
                    waited: Duration::from_secs(1),
                    waiting: false,
                },
                Why::TimedOut,
            ),
            (PackageKitError::Vanished, Why::Unanswered),
        ] {
            assert_eq!(why_packagekit(&error), why, "{error}");
        }
    }

    #[test]
    fn a_snap_s_check_is_the_snap_alone_never_against_dpkg() {
        let services = Services::new(Language::French);
        let target = Target::Snap {
            name: "android-studio".to_owned(),
            purge: false,
        };
        // No PackageKit, no dpkg asked: a snap's check needs neither.
        let checked = super::super::uninstall::check(&services, &target).unwrap();
        assert!(matches!(
            checked,
            super::super::uninstall::Checked::Going(going) if going.len() == 1 && going[0].is(&target)
        ));
    }
}
