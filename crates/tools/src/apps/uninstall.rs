//! Uninstalling an application through the system's own services: a deb by
//! `PackageKit` (simulated first, so that what goes is shown, then removed
//! exactly as shown), a snap by snapd, a flatpak of the system's
//! installation by flatpak's system helper (with the extensions that go with
//! it, found when it is checked). Never a command run; the rights are asked
//! by the service's polkit window. What the system needs never goes.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;

use super::Source;
use super::dpkg::Status;
use super::flatpak::FlatpakRef;

/// What is to be uninstalled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Deb {
        package: String,
        architecture: String,
    },
    /// `purge`: its data goes now, no copy kept.
    Snap { name: String, purge: bool },
    /// In the system's installation, the only one its helper changes.
    Flatpak(FlatpakRef),
}

impl Target {
    /// Who installed it.
    #[must_use]
    pub const fn source(&self) -> Source {
        match self {
            Self::Deb { .. } => Source::Deb,
            Self::Snap { .. } => Source::Snap,
            Self::Flatpak(_) => Source::Flatpak,
        }
    }
}

/// A package that would go with it (a flatpak's extension).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Going {
    /// The service's own identifier of it, passed back to remove it (a
    /// flatpak's reference).
    pub id: String,
    pub name: String,
    pub architecture: String,
    /// What it takes, when known.
    pub bytes: Option<u64>,
}

impl Going {
    /// Whether it is `target`, an architecture of every one (`all`)
    /// matching any: the service and dpkg may each say it their way.
    #[must_use]
    pub fn is(&self, target: &Target) -> bool {
        match target {
            Target::Deb {
                package,
                architecture,
            } => self.name == *package && same_architecture(&self.architecture, architecture),
            Target::Snap { name, .. } => self.name == *name,
            Target::Flatpak(reference) => self.id == reference.to_string(),
        }
    }
}

/// Whether two architectures name the same package's: equal, or one of
/// them every one's (`all`); PackageKit and dpkg may each say it their way.
#[must_use]
pub(crate) fn same_architecture(one: &str, other: &str) -> bool {
    one == other || one == "all" || other == "all"
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// What would go, the application first.
    Going(Vec<Going>),
    /// It would take what the system needs, named: never removed.
    Refused(Vec<String>),
}

/// Why an uninstallation did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// The service is not on this system.
    Absent,
    /// The rights were not given (cancelled, refused, no polkit agent).
    NotAuthorized,
    /// Another installation holds the package manager.
    Busy,
    /// The service stopped answering: the outcome is unknown.
    Unanswered,
    /// No answer in time; it may still happen if the rights were given.
    TimedOut,
    /// It is no longer installed.
    NotFound,
    /// What would go cannot be told for sure (dpkg's state unreadable, an
    /// answer of the service not read whole, a removal that would also
    /// install or change packages, a flatpak's deployment or metadata
    /// unreadable): nothing removed.
    Unclear,
    /// The service refused, in its own words.
    Service(String),
}

/// What an uninstallation under way says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// It may still be cancelled, or no longer.
    Cancellable(bool),
    /// Another installation holds the package manager and it waits its
    /// turn, or no longer.
    Waiting(bool),
}

/// How an uninstallation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    Done(Gone),
    Cancelled,
    Failed(Why),
}

/// What stayed of what was shown once an uninstallation is done: a deb's
/// and a snap's leave nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Gone {
    /// What was shown and not removed (a flatpak's extension), by its
    /// [`Going::id`].
    pub left: Vec<String>,
    /// The space is not known to be free (flatpak's repository not pruned,
    /// or a part left).
    pub space_unknown: bool,
}

/// What uninstalls: the system's services, or a test's. Never called
/// directly: [`check`] and [`remove_as_shown`] hold the guards.
pub trait Uninstaller: Send + Sync {
    /// What uninstalling `target` would take with it, as the service says
    /// (for a flatpak, as its deployment's metadata says), the application
    /// first.
    ///
    /// # Errors
    ///
    /// [`Why`] it cannot be told.
    fn simulate(&self, target: &Target) -> Result<Vec<Going>, Why>;

    /// What dpkg says is installed, read now.
    ///
    /// # Errors
    ///
    /// [`Why`] it cannot be read.
    fn status(&self) -> Result<Status, Why>;

    /// Removes `target` and exactly `going`, telling `steps` how it goes
    /// and cancelled when `cancel` is set, when the service can; no sender of
    /// `steps` is kept once it returns (its end is waited for). No guard
    /// here.
    fn remove(
        &self,
        target: &Target,
        going: &[Going],
        steps: &Sender<Step>,
        cancel: &AtomicBool,
    ) -> Ended;
}

/// What uninstalling `target` would take with it, refused when it would take
/// what the system needs (dpkg's state read again now).
///
/// # Errors
///
/// [`Why`] it cannot be told.
pub fn check(uninstaller: &dyn Uninstaller, target: &Target) -> Result<Checked, Why> {
    let mut going = uninstaller.simulate(target)?;
    // What goes must be the application asked, never another variant of it
    // (another architecture): it comes first.
    let Some(first) = going.iter().position(|package| package.is(target)) else {
        tracing::warn!(
            ?target,
            ?going,
            "what would go is not the application asked: nothing removed"
        );
        return Err(Why::NotFound);
    };
    going.swap(0, first);
    if let Target::Deb { .. } = target {
        let names = refused(&going, &uninstaller.status()?);
        if !names.is_empty() {
            tracing::info!(?target, ?names, "refused: the system needs them");
            return Ok(Checked::Refused(names));
        }
    }
    Ok(Checked::Going(going))
}

/// How a removal asked for went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    Ended(Ended),
    /// What would go is no longer what was shown: nothing removed, the new
    /// check said.
    Changed(Result<Checked, Why>),
}

/// Removes `target` if what would go is still `going`, as shown and
/// confirmed (a deb is checked once more; a snap has no list that changes;
/// a flatpak's extensions are asked as shown, one already gone counts gone).
pub fn remove_as_shown(
    uninstaller: &dyn Uninstaller,
    target: &Target,
    going: &[Going],
    steps: &Sender<Step>,
    cancel: &AtomicBool,
) -> Removal {
    if let Target::Deb { .. } = target {
        match check(uninstaller, target) {
            Ok(Checked::Going(again)) if same_packages(&again, going) => {}
            other => {
                tracing::warn!(
                    ?target,
                    ?other,
                    "what would go changed since it was shown: nothing removed"
                );
                return Removal::Changed(other);
            }
        }
    }
    let started = std::time::Instant::now();
    let ended = uninstaller.remove(target, going, steps, cancel);
    tracing::info!(?target, packages = going.len(), ?ended, elapsed = ?started.elapsed(), "removal ended");
    Removal::Ended(ended)
}

/// Whether `one` and `other` take the same packages, whatever their order
/// (the application first in both).
fn same_packages(one: &[Going], other: &[Going]) -> bool {
    let sorted = |going: &[Going]| {
        let mut going = going.to_vec();
        going.sort_by(|one, other| one.id.cmp(&other.id));
        going
    };
    one.first() == other.first() && sorted(one) == sorted(other)
}

/// The packages of `going` that never go: those the system needs
/// (essential, protected, required), those that gather the system's
/// (metapackages), and those `status` does not know (installed since: never
/// taken unseen).
#[must_use]
pub fn refused(going: &[Going], status: &Status) -> Vec<String> {
    going
        .iter()
        .filter(|package| {
            status
                .installed
                .iter()
                .find(|installed| {
                    installed.name == package.name
                        && same_architecture(&installed.architecture, &package.architecture)
                })
                .is_none_or(|installed| installed.system || installed.metapackage)
        })
        .map(|package| package.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use super::*;
    use crate::apps::dpkg::parse_status;

    const STATUS: &str = "Package: gimp\nStatus: install ok installed\nArchitecture: amd64\n\n\
         Package: help\nStatus: install ok installed\nArchitecture: amd64\n\n\
         Package: other\nStatus: install ok installed\nArchitecture: amd64\n\n\
         Package: base\nStatus: install ok installed\nArchitecture: amd64\nPriority: required\n";

    /// A service that simulates in turn what it is given, and counts.
    struct Service {
        simulations: Mutex<Vec<Result<Vec<Going>, Why>>>,
        simulated: AtomicUsize,
        removed: AtomicUsize,
    }

    impl Service {
        fn new(simulations: Vec<Result<Vec<Going>, Why>>) -> Self {
            Self {
                simulations: Mutex::new(simulations),
                simulated: AtomicUsize::new(0),
                removed: AtomicUsize::new(0),
            }
        }
    }

    impl Uninstaller for Service {
        fn simulate(&self, _target: &Target) -> Result<Vec<Going>, Why> {
            self.simulated.fetch_add(1, Ordering::Relaxed);
            self.simulations.lock().unwrap().remove(0)
        }

        fn status(&self) -> Result<Status, Why> {
            Ok(parse_status(STATUS))
        }

        fn remove(
            &self,
            _target: &Target,
            _going: &[Going],
            _steps: &Sender<Step>,
            _cancel: &AtomicBool,
        ) -> Ended {
            self.removed.fetch_add(1, Ordering::Relaxed);
            Ended::Done(Gone::default())
        }
    }

    fn deb() -> Target {
        Target::Deb {
            package: "gimp".to_owned(),
            architecture: "amd64".to_owned(),
        }
    }

    fn removal(service: &Service, target: &Target, going: &[Going]) -> Removal {
        let (steps, _) = mpsc::channel();
        remove_as_shown(service, target, going, &steps, &AtomicBool::new(false))
    }

    #[test]
    fn a_check_never_lets_what_the_system_needs_go() {
        let service = Service::new(vec![Ok(vec![going("gimp"), going("base")])]);
        assert_eq!(
            check(&service, &deb()),
            Ok(Checked::Refused(vec!["base".to_owned()]))
        );
        let service = Service::new(vec![Ok(Vec::new())]);
        assert_eq!(
            check(&service, &deb()),
            Err(Why::NotFound),
            "nothing would go"
        );
        let service = Service::new(vec![Err(Why::Busy)]);
        assert_eq!(check(&service, &deb()), Err(Why::Busy));
    }

    #[test]
    fn what_goes_is_what_was_shown_or_nothing_goes() {
        let shown = [going("gimp"), going("help")];
        let service = Service::new(vec![Ok(shown.to_vec())]);
        assert_eq!(
            removal(&service, &deb(), &shown),
            Removal::Ended(Ended::Done(Gone::default()))
        );
        assert_eq!(service.removed.load(Ordering::Relaxed), 1);

        // As many packages, another one among them.
        let service = Service::new(vec![Ok(vec![going("gimp"), going("other")])]);
        assert!(matches!(
            removal(&service, &deb(), &shown),
            Removal::Changed(Ok(Checked::Going(_)))
        ));
        // What the system needs, come since.
        let service = Service::new(vec![Ok(vec![going("gimp"), going("base")])]);
        assert!(matches!(
            removal(&service, &deb(), &shown),
            Removal::Changed(Ok(Checked::Refused(_)))
        ));
        let service = Service::new(vec![Err(Why::Unanswered)]);
        assert_eq!(
            removal(&service, &deb(), &shown),
            Removal::Changed(Err(Why::Unanswered))
        );
        assert_eq!(service.removed.load(Ordering::Relaxed), 0, "never removed");

        // A snap has no list that changes: removed as asked.
        let snap = Target::Snap {
            name: "x".to_owned(),
            purge: true,
        };
        let service = Service::new(Vec::new());
        assert_eq!(
            removal(&service, &snap, &[going("x")]),
            Removal::Ended(Ended::Done(Gone::default()))
        );
        assert_eq!(service.simulated.load(Ordering::Relaxed), 0);
    }

    fn going(name: &str) -> Going {
        going_for(name, "amd64")
    }

    fn going_for(name: &str, architecture: &str) -> Going {
        Going {
            id: format!("{name};1;{architecture};installed"),
            name: name.to_owned(),
            architecture: architecture.to_owned(),
            bytes: None,
        }
    }

    #[test]
    fn what_goes_must_hold_the_application_asked_put_first() {
        // Catches a check that takes any package for the target, or an
        // architecture rule that no longer matches `all`.
        let service = Service::new(vec![Ok(vec![going_for("gimp", "i386")])]);
        assert_eq!(
            check(&service, &deb()),
            Err(Why::NotFound),
            "another architecture"
        );
        let service = Service::new(vec![Ok(vec![going("help"), going("other")])]);
        assert_eq!(check(&service, &deb()), Err(Why::NotFound), "not there");
        let service = Service::new(vec![Ok(vec![going("help"), going("gimp")])]);
        assert_eq!(
            check(&service, &deb()),
            Ok(Checked::Going(vec![going("gimp"), going("help")]))
        );
        let service = Service::new(vec![Ok(vec![going_for("gimp", "all")])]);
        assert!(matches!(check(&service, &deb()), Ok(Checked::Going(_))));
    }

    #[test]
    fn the_same_packages_in_another_order_are_what_was_shown() {
        let shown = [going("gimp"), going("help"), going("other")];
        let service = Service::new(vec![Ok(vec![going("gimp"), going("other"), going("help")])]);
        assert_eq!(
            removal(&service, &deb(), &shown),
            Removal::Ended(Ended::Done(Gone::default()))
        );
    }

    #[test]
    fn a_package_of_every_architecture_is_known_to_dpkg_either_way() {
        // Catches a guard that compares architectures as they are written:
        // dpkg says `all`, PackageKit the machine's (or the other way).
        let status = parse_status(
            "Package: gimp\nStatus: install ok installed\nArchitecture: amd64\n\n\
             Package: gimp-data\nStatus: install ok installed\nArchitecture: all\n",
        );
        assert_eq!(
            refused(&[going("gimp"), going("gimp-data")], &status),
            Vec::<String>::new()
        );
        assert_eq!(
            refused(&[going_for("gimp", "all")], &status),
            Vec::<String>::new()
        );
    }

    #[test]
    fn each_guard_alone_refuses_its_package() {
        let status = parse_status(
            "Package: gimp\nStatus: install ok installed\nArchitecture: amd64\n\n\
             Package: essential\nStatus: install ok installed\nArchitecture: amd64\nEssential: yes\n\n\
             Package: protected\nStatus: install ok installed\nArchitecture: amd64\nProtected: yes\n\n\
             Package: required\nStatus: install ok installed\nArchitecture: amd64\nPriority: required\n\n\
             Package: meta\nStatus: install ok installed\nArchitecture: amd64\nSection: metapackages\n\n\
             Package: other-arch\nStatus: install ok installed\nArchitecture: i386\n",
        );
        assert_eq!(refused(&[going("gimp")], &status), Vec::<String>::new());
        for guarded in [
            "essential",
            "protected",
            "required",
            "meta",
            "unknown",
            "other-arch",
        ] {
            assert_eq!(
                refused(&[going("gimp"), going(guarded)], &status),
                [guarded],
                "{guarded}"
            );
        }
    }
}
