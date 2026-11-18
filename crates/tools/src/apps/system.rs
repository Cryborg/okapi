//! The system's services as an [`Uninstaller`]: PackageKit for a deb (its
//! package found installed, the removal simulated, then made), snapd for a
//! snap (its change followed to its end), flatpak's system helper for a
//! flatpak (with the extensions that go with it). dpkg's state is read from
//! its files. Nothing here runs a command; the rights are asked by the
//! services' own polkit windows.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use okapi_rules::Language;

use super::dpkg::{self, Status};
use super::flatpak::{FlatpakRef, SYSTEM_INSTALLATION, related};
use super::measure::{Measured, measure};
use super::snap::read_snaps;
use super::uninstall::{Ended, Going, Gone, Step, Target, Uninstaller, Why, same_architecture};
use super::{Reading, SourceReading};
use crate::packages::flatpak::{FlatpakError, SystemHelper};
use crate::packages::packagekit::{
    self, PackageId, PackageKit, PackageKitError, Removing, error_code,
};
use crate::packages::snapd::{CHANGE_DONE, Followed, Snapd, SnapdError};

/// How long a snap's removal is followed at most once snapd took it (the
/// rights given): unmounting and a snapshot of its data, minutes at worst.
const SNAP_REMOVAL_WAIT: Duration = Duration::from_mins(30);
/// How many looks at a change may fail in a row before its outcome is read
/// from the snaps installed: snapd restarting answers nothing a moment.
const CHANGE_LOOKS_FAILED: u32 = 5;

/// PackageKit, snapd, flatpak's helper and dpkg's files, on this system.
#[derive(Debug, Clone)]
pub struct Services {
    locale: &'static str,
    dpkg: PathBuf,
    snapd: Snapd,
    /// The installation flatpak's helper changes: the system's.
    flatpak: PathBuf,
    /// How long a snap's removal is followed at most.
    removal_wait: Duration,
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
            dpkg: PathBuf::from(crate::packages::dpkg::DPKG_DIRECTORY),
            snapd: Snapd::system(),
            flatpak: PathBuf::from(SYSTEM_INSTALLATION),
            removal_wait: SNAP_REMOVAL_WAIT,
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
                    id.name == package && same_architecture(id.arch, architecture)
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
            Target::Flatpak(reference) => {
                let started = Instant::now();
                let related = related(&self.flatpak, reference)?;
                let going = |reference: &FlatpakRef, deployment: &Path| Going {
                    id: reference.to_string(),
                    name: reference.id().to_owned(),
                    architecture: reference.arch().to_owned(),
                    bytes: measured(deployment),
                };
                let going: Vec<Going> = std::iter::once(going(reference, &related.deployment))
                    .chain(
                        related
                            .extensions
                            .iter()
                            .map(|(extension, deployment)| going(extension, deployment)),
                    )
                    .collect();
                tracing::info!(%reference, going = going.len(), elapsed = ?started.elapsed(), "flatpak's removal checked and measured");
                Ok(going)
            }
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
                            return Err(Why::Unclear);
                        };
                        let bytes = status
                            .installed
                            .iter()
                            .find(|installed| {
                                installed.name == id.name
                                    && same_architecture(&installed.architecture, id.arch)
                            })
                            .and_then(|installed| installed.installed_bytes);
                        if bytes.is_none() {
                            tracing::info!(name = id.name, architecture = id.arch, "a package going without its size in dpkg's state");
                        }
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
        match crate::packages::dpkg::pending_updates(&self.dpkg) {
            Ok(0) => {}
            Ok(pending) => {
                tracing::info!(pending, "dpkg is writing: its state read later");
                return Err(Why::Busy);
            }
            Err(error) => {
                tracing::warn!(%error, "dpkg's updates unreadable");
                return Err(Why::Unclear);
            }
        }
        std::fs::read_to_string(self.dpkg.join("status"))
            .map(|text| dpkg::parse_status(&text))
            .map_err(|error| {
                tracing::warn!(%error, "dpkg's status unreadable");
                Why::Unclear
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
                    // The screen gone, nobody hears: the removal goes on.
                    let _ = steps.send(match removing {
                        Removing::Waiting(waiting) => Step::Waiting(waiting),
                        Removing::Cancellable(can) => Step::Cancellable(can),
                    });
                };
                match packagekit.remove_packages(&ids, &heard, cancel) {
                    Ok(()) => Ended::Done(Gone::default()),
                    Err(error) if error.cancelled() => Ended::Cancelled,
                    Err(error) => Ended::Failed(why_packagekit(&error)),
                }
            }
            Target::Snap { name, purge } => self.remove_snap(name, *purge),
            Target::Flatpak(reference) => remove_flatpak(&reference.to_string(), going),
        }
    }
}

/// What a deployment takes, when measured whole.
fn measured(deployment: &Path) -> Option<u64> {
    match measure(deployment, &AtomicBool::new(false)) {
        Measured::Bytes {
            bytes,
            partial: false,
        } => Some(bytes),
        other => {
            tracing::info!(path = %deployment.display(), ?other, "a deployment going without its size");
            None
        }
    }
}

/// Removes the flatpak `app` and the extensions of `going` with it, by
/// flatpak's system helper.
fn remove_flatpak(app: &str, going: &[Going]) -> Ended {
    match SystemHelper::connect() {
        Ok(helper) => removed_by(&helper, app, going),
        Err(error) => {
            tracing::warn!(app, %error, "flatpak's helper not reached");
            Ended::Failed(why_flatpak(&error))
        }
    }
}

/// Removes `app` and the extensions of `going` by `helper`: what stayed of
/// them said, and the space unknown unless all went by this removal and the
/// repository was pruned.
fn removed_by(helper: &SystemHelper, app: &str, going: &[Going]) -> Ended {
    let related: Vec<String> = going
        .iter()
        .filter(|package| package.id != app)
        .map(|package| package.id.clone())
        .collect();
    match helper.remove(app, &related) {
        Ok(removed) => Ended::Done(Gone {
            space_unknown: !removed.pruned
                || !removed.left.is_empty()
                || !removed.already_gone.is_empty(),
            left: removed.left,
        }),
        Err(error) => Ended::Failed(why_flatpak(&error)),
    }
}

impl Services {
    /// Removes the snap `name`, then follows the change snapd started, for
    /// `removal_wait` at most: past it, or after looks failed in a row, the
    /// snap gone is a removal done.
    fn remove_snap(&self, name: &str, purge: bool) -> Ended {
        let change = match self.snapd.remove(name, purge) {
            Ok(change) => change,
            Err(error) => {
                tracing::warn!(name, %error, "snapd refused the removal");
                return Ended::Failed(why_snapd(&error));
            }
        };
        tracing::info!(name, purge, change, "snapd removes the snap");
        let _span = tracing::info_span!("snap_removal", name).entered();
        let gone = || self.snapd.installed(name).map(|installed| !installed);
        match followed(&self.snapd, &change, self.removal_wait, &gone) {
            Ok(()) => Ended::Done(Gone::default()),
            Err(why) => Ended::Failed(why),
        }
    }
}

/// Follows the change `change` of snapd for `wait` at most. Ready, it
/// ended as its status says; followed no longer (past the wait, or after
/// looks failed in a row), what it was to remove is asked: `gone`, it is
/// done; still there, it failed (it may yet end, a list read again shows
/// it).
///
/// # Errors
/// Why it did not end done, as the screen says it.
pub(crate) fn followed(
    snapd: &Snapd,
    change: &str,
    wait: Duration,
    gone: &dyn Fn() -> Result<bool, SnapdError>,
) -> Result<(), Why> {
    let why = match snapd.follow(change, wait, CHANGE_LOOKS_FAILED, &|_| {}) {
        Followed::Ready(state) if state.status == CHANGE_DONE => return Ok(()),
        Followed::Ready(state) => {
            tracing::warn!(change, status = %state.status, err = ?state.err, "snapd's change did not end done");
            return Err(Why::Service(state.err.unwrap_or(state.status)));
        }
        Followed::TimedOut => Why::TimedOut,
        Followed::Unanswered => Why::Unanswered,
    };
    match gone() {
        Ok(true) => {
            tracing::info!(change, "what the change removes is gone: it is done");
            Ok(())
        }
        Ok(false) => {
            tracing::warn!(
                change,
                ?why,
                "what the change removes is still there: not seen to end"
            );
            Err(why)
        }
        Err(error) => {
            tracing::warn!(change, %error, "whether what the change removes is gone unknown");
            Err(Why::Unanswered)
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
        error if error.rights_refused() => Why::NotAuthorized,
        error if error.busy() => Why::Busy,
        PackageKitError::Failed {
            code: error_code::PACKAGE_NOT_INSTALLED,
            ..
        } => Why::NotFound,
        PackageKitError::Failed { details, .. } => Why::Service(details.clone()),
        PackageKitError::NotOnlyRemoving { .. } => Why::Unclear,
        PackageKitError::TimedOut { .. } => Why::TimedOut,
        PackageKitError::Transport(_)
        | PackageKitError::Bus(_)
        | PackageKitError::Vanished
        | PackageKitError::Unreadable { .. }
        | PackageKitError::Ended { .. } => Why::Unanswered,
    }
}

/// Why, as the screen says it, flatpak's helper did not remove.
pub(crate) fn why_flatpak(error: &FlatpakError) -> Why {
    match error {
        FlatpakError::Absent => Why::Absent,
        FlatpakError::NotAuthorized(_) => Why::NotAuthorized,
        FlatpakError::NotInstalled(_) => Why::NotFound,
        FlatpakError::TimedOut { .. } => Why::TimedOut,
        FlatpakError::Service { message, .. } => Why::Service(message.clone()),
        FlatpakError::Transport(_) | FlatpakError::Unanswered(_) | FlatpakError::Bus(_) => {
            Why::Unanswered
        }
    }
}

/// Why, as the screen says it, snapd gave no answer.
pub(crate) fn why_snapd(error: &SnapdError) -> Why {
    match error {
        SnapdError::Absent { .. } => Why::Absent,
        SnapdError::InvalidName(_) => Why::NotFound,
        // Never asked: Okapi held what snapd never writes.
        SnapdError::InvalidRevision(_) => Why::Unclear,
        error if error.timed_out() => Why::TimedOut,
        error if error.rights_refused() => Why::NotAuthorized,
        error if error.busy() => Why::Busy,
        error if error.snap_absent() => Why::NotFound,
        SnapdError::Refused { message, .. } => Why::Service(message.clone()),
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
    use crate::packages::snapd::{self, CHANGE_LOOK_EVERY};

    /// A snapd that answers each request, one per connection, with the
    /// next of `answers` (`None`: closed unanswered); the services that
    /// ask it, a snap's removal followed for `removal_wait`.
    fn snapd_answering(
        answers: Vec<Option<&'static str>>,
        removal_wait: Duration,
    ) -> (tempfile::TempDir, Services) {
        let (directory, snapd, _) = crate::packages::snapd::fake::scripted(
            answers
                .into_iter()
                .map(|answer| answer.map(str::to_owned))
                .collect(),
        );
        let services = Services {
            snapd,
            removal_wait,
            ..Services::new(Language::French)
        };
        (directory, services)
    }

    const STARTED: &str =
        r#"{"type":"async","status-code":202,"status":"Accepted","result":null,"change":"7"}"#;
    const DOING: &str = r#"{"type":"sync","status-code":200,"status":"OK","result":{"status":"Doing","ready":false}}"#;
    const DONE: &str = r#"{"type":"sync","status-code":200,"status":"OK","result":{"status":"Done","ready":true}}"#;
    /// snapd's answer, word for word, to reading a snap absent.
    const GONE: &str = r#"{"type":"error","status-code":404,"status":"Not Found","result":{"message":"snap not installed","kind":"snap-not-found","value":"android-studio"}}"#;
    const THERE: &str =
        r#"{"type":"sync","status-code":200,"status":"OK","result":{"name":"android-studio"}}"#;
    const LONG: Duration = Duration::from_mins(1);

    #[test]
    fn a_snap_s_removal_followed_to_its_end_is_done() {
        let (_directory, services) =
            snapd_answering(vec![Some(STARTED), Some(DOING), Some(DONE)], LONG);
        assert_eq!(
            services.remove_snap("android-studio", false),
            Ended::Done(Gone::default())
        );
    }

    #[test]
    fn a_change_unreadable_a_while_is_told_by_the_snap_gone_or_still_there() {
        // Catches a single failed look taken for a failure, and looks that
        // fail forever never ending the follow.
        let mut gone = vec![Some(STARTED), Some(DOING)];
        gone.extend(std::iter::repeat_n(None, CHANGE_LOOKS_FAILED as usize));
        gone.push(Some(GONE));
        let (_directory, services) = snapd_answering(gone, LONG);
        assert_eq!(
            services.remove_snap("android-studio", false),
            Ended::Done(Gone::default())
        );

        let mut there = vec![Some(STARTED)];
        there.extend(std::iter::repeat_n(None, CHANGE_LOOKS_FAILED as usize));
        there.push(Some(THERE));
        let (_directory, services) = snapd_answering(there, LONG);
        assert_eq!(
            services.remove_snap("android-studio", false),
            Ended::Failed(Why::Unanswered)
        );
    }

    #[test]
    fn a_change_never_ready_is_followed_no_longer_than_its_wait() {
        // Catches a follow without end: snapd saying `Doing` forever.
        let (_directory, services) = snapd_answering(
            vec![Some(STARTED), Some(DOING), Some(THERE)],
            CHANGE_LOOK_EVERY * 3 / 2,
        );
        assert_eq!(
            services.remove_snap("android-studio", false),
            Ended::Failed(Why::TimedOut)
        );
    }

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
            (refused(404, snapd::KIND_NOT_FOUND), Why::NotFound),
            (refused(403, ""), Why::NotAuthorized),
            (refused(500, "other"), Why::Service("words".to_owned())),
            (SnapdError::Malformed("x"), Why::Unanswered),
            (SnapdError::InvalidName("a/b".into()), Why::NotFound),
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
            (
                PackageKitError::Bus(zbus::Error::Failure("gone".into())),
                Why::Unanswered,
            ),
            (
                PackageKitError::Ended {
                    exit: packagekit::EXIT_CANCELLED,
                },
                Why::Unanswered,
            ),
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

    #[test]
    fn a_flatpak_s_check_is_the_application_then_its_extensions_never_against_dpkg() {
        // Catches the extensions left out of what is shown (they would go
        // unseen, or stay), the application not first, dpkg asked.
        let root = tempfile::tempdir().unwrap();
        let installation = root.path().to_path_buf();
        let branch = installation.join("app/org.gimp.GIMP/aarch64/stable");
        std::fs::create_dir_all(branch.join("abc123")).unwrap();
        std::os::unix::fs::symlink("abc123", branch.join("active")).unwrap();
        std::fs::write(
            branch.join("abc123/metadata"),
            "[Extension org.gimp.GIMP.Locale]\n",
        )
        .unwrap();
        // Both from Flathub, the translations saying whose they are.
        std::fs::write(branch.join("abc123/deploy"), b"flathub\0").unwrap();
        let locale = installation.join("runtime/org.gimp.GIMP.Locale/aarch64/stable");
        std::fs::create_dir_all(locale.join("def456")).unwrap();
        std::os::unix::fs::symlink("def456", locale.join("active")).unwrap();
        std::fs::write(locale.join("def456/fr.mo"), [0; 10]).unwrap();
        std::fs::write(locale.join("def456/deploy"), b"flathub\0").unwrap();
        std::fs::write(
            locale.join("def456/metadata"),
            "[ExtensionOf]\nref=app/org.gimp.GIMP/aarch64/stable\n",
        )
        .unwrap();
        std::fs::write(branch.join("abc123/gimp"), vec![0; 256 * 1024]).unwrap();
        let target =
            Target::Flatpak(FlatpakRef::app("org.gimp.GIMP", "aarch64", "stable").unwrap());
        let services = Services {
            flatpak: installation,
            ..Services::new(Language::French)
        };
        let Ok(super::super::uninstall::Checked::Going(going)) =
            super::super::uninstall::check(&services, &target)
        else {
            panic!("checked");
        };
        let shown: Vec<(&str, &str)> = going
            .iter()
            .map(|package| (package.id.as_str(), package.name.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("app/org.gimp.GIMP/aarch64/stable", "org.gimp.GIMP"),
                (
                    "runtime/org.gimp.GIMP.Locale/aarch64/stable",
                    "org.gimp.GIMP.Locale"
                ),
            ]
        );
        assert!(going[0].is(&target));
        assert!(!going[1].is(&target));
        // Each measured, of its own architecture: the space freed is told
        // whole, never the application's alone.
        for package in &going {
            assert_eq!(package.architecture, "aarch64");
            assert!(package.bytes.is_some(), "{package:?}");
        }
        assert!(going[0].bytes > going[1].bytes, "each its own: {going:?}");
    }

    #[test]
    fn a_flatpak_removed_says_what_stayed_by_its_reference_and_the_space_unknown() {
        // Catches the application asked among its extensions, what stayed
        // told by a name two branches share, an extension already gone told
        // left, the space said free when a part stayed or the repository was
        // not pruned.
        use crate::packages::flatpak::tests::{Answer, fake_helper};
        let reference = |kind: &str, id: &str, branch: &str| Going {
            id: format!("{kind}/{id}/x86_64/{branch}"),
            name: id.to_owned(),
            architecture: "x86_64".to_owned(),
            bytes: Some(1),
        };
        let app = reference("app", "org.gimp.GIMP", "stable");
        let going = [
            app.clone(),
            reference("runtime", "org.gimp.GIMP.Plugin", "2"),
            reference("runtime", "org.gimp.GIMP.Plugin", "3"),
            reference("runtime", "org.gimp.GIMP.Locale", "stable"),
        ];
        let (helper, heard) = fake_helper(|member, reference| match (member, reference) {
            ("Uninstall", "runtime/org.gimp.GIMP.Plugin/x86_64/3") => {
                Answer::Error("org.freedesktop.Flatpak.Error.Aborted")
            }
            ("Uninstall", "runtime/org.gimp.GIMP.Locale/x86_64/stable") => {
                Answer::Error("org.freedesktop.Flatpak.Error.NotInstalled")
            }
            _ => Answer::Done,
        });
        assert_eq!(
            removed_by(&helper, &app.id, &going),
            Ended::Done(Gone {
                left: vec!["runtime/org.gimp.GIMP.Plugin/x86_64/3".to_owned()],
                space_unknown: true,
            })
        );
        let asked: Vec<String> = heard
            .try_iter()
            .filter(|heard| heard.member == "Uninstall")
            .map(|heard| heard.strings[0].clone())
            .collect();
        let shown: Vec<String> = going.iter().map(|package| package.id.clone()).collect();
        assert_eq!(asked, shown, "the application first, then each as shown");

        // An extension already gone: gone, but not freed by this removal.
        let (helper, _heard) = fake_helper(|member, reference| {
            if member == "Uninstall" && reference == "runtime/org.gimp.GIMP.Locale/x86_64/stable" {
                Answer::Error("org.freedesktop.Flatpak.Error.NotInstalled")
            } else {
                Answer::Done
            }
        });
        assert_eq!(
            removed_by(&helper, &app.id, &going),
            Ended::Done(Gone {
                left: Vec::new(),
                space_unknown: true,
            })
        );

        // All gone, the repository not pruned: the space is not known free.
        let (helper, _heard) = fake_helper(|member, _| {
            if member == "PruneLocalRepo" {
                Answer::Error("org.freedesktop.Flatpak.Error.Aborted")
            } else {
                Answer::Done
            }
        });
        assert_eq!(
            removed_by(&helper, &app.id, &going),
            Ended::Done(Gone {
                left: Vec::new(),
                space_unknown: true,
            })
        );

        // All gone, the repository pruned: the space is known.
        let (helper, _heard) = fake_helper(|_, _| Answer::Done);
        assert_eq!(
            removed_by(&helper, &app.id, &going),
            Ended::Done(Gone::default())
        );
    }

    #[test]
    fn each_failure_of_flatpak_s_helper_is_told_by_its_kind() {
        for (error, why) in [
            (FlatpakError::Absent, Why::Absent),
            (
                FlatpakError::NotAuthorized("x".to_owned()),
                Why::NotAuthorized,
            ),
            (FlatpakError::NotInstalled("x".to_owned()), Why::NotFound),
            (
                FlatpakError::TimedOut {
                    waited: Duration::from_secs(1),
                },
                Why::TimedOut,
            ),
            (
                FlatpakError::Service {
                    name: "n".to_owned(),
                    message: "words".to_owned(),
                },
                Why::Service("words".to_owned()),
            ),
            (FlatpakError::Unanswered("x".to_owned()), Why::Unanswered),
            (FlatpakError::Transport("x".to_owned()), Why::Unanswered),
            (
                FlatpakError::Bus(zbus::Error::Failure("x".to_owned())),
                Why::Unanswered,
            ),
        ] {
            assert_eq!(why_flatpak(&error), why, "{error}");
        }
    }

    #[test]
    fn a_deployment_measured_in_part_has_no_size() {
        use std::os::unix::fs::PermissionsExt as _;
        // Catches a size read in part taken for the whole: the space freed
        // would be said less than goes.
        if okapi_platform::running_as_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let closed = root.path().join("closed");
        std::fs::create_dir(&closed).unwrap();
        std::fs::write(root.path().join("file"), [0; 10]).unwrap();
        assert!(measured(root.path()).is_some());
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let size = measured(root.path());
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(size, None);
    }
}
