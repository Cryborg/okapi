use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::packages::snapd::{Task, TaskProgress};
use crate::updates::Kind;

fn package(info: u32, name: &str) -> Package {
    Package {
        info,
        id: format!("{name};2.0;amd64;resolute-updates"),
        summary: format!("{name}, said briefly"),
    }
}

fn names(list: &[Affected]) -> Vec<&str> {
    list.iter().map(|affected| affected.name.as_str()).collect()
}

/// An installer that simulates in turn what it is given, then says what
/// it is given while it updates, and counts.
struct Fake {
    simulations: Mutex<Vec<Result<Vec<Package>, PackageKitError>>>,
    said: Vec<Updating>,
    updated: fn() -> Result<Updated, PackageKitError>,
    interrupted: fn() -> io::Result<bool>,
    updates: AtomicUsize,
    interrupted_asked: AtomicUsize,
    refresh: fn() -> Result<String, SnapdError>,
    changes: Vec<Change>,
    followed: fn() -> Followed,
}

impl Default for Fake {
    fn default() -> Self {
        Self {
            simulations: Mutex::new(Vec::new()),
            said: Vec::new(),
            updated: || {
                Ok(Updated {
                    system_restart: false,
                })
            },
            interrupted: || Ok(false),
            updates: AtomicUsize::new(0),
            interrupted_asked: AtomicUsize::new(0),
            refresh: || Ok("7".into()),
            changes: Vec::new(),
            followed: || Followed::Ready(change(CHANGE_DONE, Vec::new())),
        }
    }
}

impl Fake {
    fn simulating(simulations: Vec<Vec<Package>>) -> Self {
        Self {
            simulations: Mutex::new(simulations.into_iter().map(Ok).collect()),
            ..Self::default()
        }
    }
}

impl Installer for Fake {
    fn simulate_system(
        &self,
        _ids: &[String],
        waiting: &dyn Fn(bool),
    ) -> Result<Vec<Package>, PackageKitError> {
        waiting(true);
        waiting(false);
        self.simulations.lock().unwrap().remove(0)
    }

    fn update_system(
        &self,
        _ids: &[String],
        heard: &dyn Fn(Updating),
        _cancel: &AtomicBool,
    ) -> Result<Updated, PackageKitError> {
        self.updates.fetch_add(1, Ordering::SeqCst);
        for updating in &self.said {
            heard(updating.clone());
        }
        (self.updated)()
    }

    fn dpkg_interrupted(&self) -> io::Result<bool> {
        self.interrupted_asked.fetch_add(1, Ordering::SeqCst);
        (self.interrupted)()
    }

    fn refresh_snaps(&self, _names: &[String]) -> Result<String, SnapdError> {
        (self.refresh)()
    }

    fn follow_snaps(&self, _id: &str, heard: &dyn Fn(&Change)) -> Followed {
        for change in &self.changes {
            heard(change);
        }
        (self.followed)()
    }
}

fn change(status: &str, tasks: Vec<Task>) -> Change {
    Change {
        status: status.into(),
        ready: status != "Doing",
        err: None,
        tasks,
    }
}

fn task(kind: &str, status: &str, done: u64, total: u64) -> Task {
    Task {
        kind: kind.into(),
        status: status.into(),
        progress: TaskProgress { done, total },
    }
}

fn ids() -> Vec<String> {
    vec!["bash;2.0;amd64;x".into(), "curl;2.0;amd64;x".into()]
}

/// Asks to update, the removals of `accepted` accepted (by name, of the
/// machine's architecture).
fn install(fake: &Fake, accepted: &[&str]) -> SystemInstall {
    let accepted = accepted
        .iter()
        .map(|name| format!("{name}:amd64"))
        .collect();
    install_system(fake, &ids(), &accepted, &|_| {}, &AtomicBool::new(false))
}

fn ended(install: SystemInstall) -> Ended {
    match install {
        SystemInstall::Ended(ended) => ended,
        SystemInstall::Confirm(simulated) => panic!("confirm {simulated:?}"),
    }
}

#[test]
fn a_simulation_is_sorted_and_closed_to_what_it_does_not_show() {
    // Catches an intent of PackageKit 1.3 (`Install`, `Remove`, `Obsolete`)
    // taken for nothing, and a downgrade, an unsigned package or a kind
    // unknown let through.
    let simulated = sort_simulation(
        &[
            package(info::UPDATING, "bash"),
            package(info::UPDATING, "curl"),
            package(info::INSTALLING, "linux-image-7.1"),
            package(info::INSTALL, "linux-modules-7.1"),
            package(info::REMOVING, "libold1"),
            package(info::REMOVE, "libold2"),
            package(info::OBSOLETING, "python3-gone"),
            package(info::OBSOLETE, "python3-gone2"),
        ],
        &ids(),
    )
    .unwrap();
    assert_eq!(simulated.updated, ["bash", "curl"]);
    assert_eq!(
        names(&simulated.added),
        ["linux-image-7.1", "linux-modules-7.1"]
    );
    assert_eq!(
        names(&simulated.removed),
        ["libold1", "libold2", "python3-gone", "python3-gone2"]
    );
    assert_eq!(simulated.removed[0].summary, "libold1, said briefly");
    assert_eq!(simulated.total(), 8);
    for (info, why) in [
        (info::DOWNGRADING, Why::Broken),
        (info::DOWNGRADE, Why::Broken),
        (info::UNTRUSTED, Why::Unsigned),
        (2, Why::Broken),
    ] {
        assert_eq!(
            sort_simulation(
                &[package(info::UPDATING, "bash"), package(info, "odd")],
                &ids()
            ),
            Err(why),
            "{info}"
        );
    }
    // An update asked, taken away: the list changed since it was read.
    assert_eq!(
        sort_simulation(
            &[Package {
                info: info::REMOVING,
                id: "curl;1.0;amd64;installed".into(),
                summary: String::new(),
            }],
            &ids()
        ),
        Err(Why::Changed)
    );
    // An update asked, back as a new package: removed since it was read.
    assert_eq!(
        sort_simulation(
            &[Package {
                info: info::INSTALLING,
                id: "curl;2.0;amd64;x".into(),
                summary: String::new(),
            }],
            &ids()
        ),
        Err(Why::Changed)
    );
}

#[test]
fn an_update_that_removes_nothing_is_installed_at_once() {
    let fake = Fake::simulating(vec![vec![package(info::UPDATING, "bash")]]);
    assert_eq!(ended(install(&fake, &[])).end, End::Done);
    assert_eq!(fake.updates.load(Ordering::SeqCst), 1);
}

#[test]
fn what_an_update_removes_is_installed_only_once_accepted_as_it_is() {
    // Catches a removal installed unseen, an accepted list compared in its
    // order, and a list changed since it was shown let through.
    let removes = |names: &[&str]| {
        let mut packages = vec![package(info::UPDATING, "bash")];
        packages.extend(names.iter().map(|name| package(info::REMOVING, name)));
        packages
    };
    let fake = Fake::simulating(vec![removes(&["libold1", "libold2"])]);
    match install(&fake, &[]) {
        SystemInstall::Confirm(simulated) => {
            assert_eq!(names(&simulated.removed), ["libold1", "libold2"]);
        }
        SystemInstall::Ended(ended) => panic!("installed {ended:?}"),
    }
    assert_eq!(fake.updates.load(Ordering::SeqCst), 0, "never installed");

    let fake = Fake::simulating(vec![removes(&["libold2", "libold1"])]);
    assert_eq!(
        ended(install(&fake, &["libold1", "libold2"])).end,
        End::Done
    );
    assert_eq!(fake.updates.load(Ordering::SeqCst), 1);

    // One more, or another: asked again, nothing installed.
    for since in [
        &["libold1", "libold2", "libold3"][..],
        &["libold1", "libother"],
    ] {
        let fake = Fake::simulating(vec![removes(since)]);
        assert!(
            matches!(
                install(&fake, &["libold1", "libold2"]),
                SystemInstall::Confirm(_)
            ),
            "{since:?}"
        );
        assert_eq!(fake.updates.load(Ordering::SeqCst), 0, "{since:?}");
    }
    // Fewer, or none: safer, installed.
    for since in [&["libold1"][..], &[]] {
        let fake = Fake::simulating(vec![removes(since)]);
        assert_eq!(
            ended(install(&fake, &["libold1", "libold2"])).end,
            End::Done,
            "{since:?}"
        );
    }
    // Accepted for one architecture, removed for another: asked again.
    let fake = Fake::simulating(vec![vec![
        package(info::UPDATING, "bash"),
        Package {
            info: info::REMOVING,
            id: "libold1;1;i386;installed".into(),
            summary: String::new(),
        },
    ]]);
    assert!(matches!(
        install(&fake, &["libold1"]),
        SystemInstall::Confirm(_)
    ));
}

#[test]
fn each_end_of_the_update_is_told() {
    let ending = |updated: fn() -> Result<Updated, PackageKitError>| {
        let fake = Fake {
            updated,
            ..Fake::simulating(vec![vec![package(info::UPDATING, "bash")]])
        };
        ended(install(&fake, &[]))
    };
    let restart = ending(|| {
        Ok(Updated {
            system_restart: true,
        })
    });
    assert_eq!(restart.end, End::Done);
    assert!(restart.system_restart);
    assert_eq!(
        ending(|| Err(PackageKitError::Ended {
            exit: packagekit::EXIT_CANCELLED
        }))
        .end,
        End::Cancelled
    );
    assert_eq!(
        ending(|| Err(PackageKitError::Failed {
            code: error_code::TRANSACTION_CANCELLED,
            details: String::new()
        }))
        .end,
        End::Cancelled
    );
    assert_eq!(
        ending(|| Err(PackageKitError::Failed {
            code: error_code::NOT_AUTHORIZED,
            details: String::new()
        }))
        .end,
        End::Failed(Why::Refused)
    );
    // A simulation that fails installs nothing.
    let fake = Fake {
        simulations: Mutex::new(vec![Err(PackageKitError::Failed {
            code: error_code::UPDATE_NOT_FOUND,
            details: String::new(),
        })]),
        ..Fake::default()
    };
    assert_eq!(
        ended(install(&fake, &[])),
        Ended::before(End::Failed(Why::Changed))
    );
    assert_eq!(fake.updates.load(Ordering::SeqCst), 0);
}

#[test]
fn a_lock_never_taken_is_dpkg_cut_short_only_with_updates_left_and_no_lock_held() {
    type Answer = (fn() -> io::Result<bool>, Why);
    // Catches another installation at work said cut short (and its advice
    // to run dpkg), and a dpkg cut short said busy forever.
    let locked = || {
        Err(PackageKitError::Failed {
            code: error_code::CANNOT_GET_LOCK,
            details: String::new(),
        })
    };
    let answers: [Answer; 3] = [
        (|| Ok(true), Why::Interrupted),
        (|| Ok(false), Why::Busy),
        // Neither busy nor cut short can be said.
        (|| Err(io::ErrorKind::PermissionDenied.into()), Why::Broken),
    ];
    for (interrupted, why) in answers {
        let fake = Fake {
            updated: locked,
            interrupted,
            ..Fake::simulating(vec![vec![package(info::UPDATING, "bash")]])
        };
        assert_eq!(ended(install(&fake, &[])).end, End::Failed(why));
    }
    // A wait that ran out is another program at work: never asked.
    let fake = Fake {
        updated: || {
            Err(PackageKitError::TimedOut {
                waited: Duration::from_secs(1),
                waiting: true,
            })
        },
        interrupted: || Ok(true),
        ..Fake::simulating(vec![vec![package(info::UPDATING, "bash")]])
    };
    assert_eq!(ended(install(&fake, &[])).end, End::Failed(Why::Busy));
    assert_eq!(fake.interrupted_asked.load(Ordering::SeqCst), 0);
    // Another failure: never asked either.
    let fake = Fake {
        updated: || {
            Err(PackageKitError::Failed {
                code: error_code::NO_SPACE_ON_DEVICE,
                details: String::new(),
            })
        },
        interrupted: || Ok(true),
        ..Fake::simulating(vec![vec![package(info::UPDATING, "bash")]])
    };
    assert_eq!(ended(install(&fake, &[])).end, End::Failed(Why::NoSpace));
    assert_eq!(fake.interrupted_asked.load(Ordering::SeqCst), 0);
}

#[test]
fn each_failure_of_packagekit_is_told_in_its_words() {
    let failed = |code| PackageKitError::Failed {
        code,
        details: String::new(),
    };
    for (error, why) in [
        (failed(error_code::CANNOT_GET_LOCK), Why::Busy),
        (failed(error_code::NOT_AUTHORIZED), Why::Refused),
        (failed(error_code::NO_NETWORK), Why::Offline),
        (failed(error_code::PACKAGE_DOWNLOAD_FAILED), Why::Offline),
        (failed(error_code::NO_SPACE_ON_DEVICE), Why::NoSpace),
        (failed(error_code::PACKAGE_NOT_FOUND), Why::Changed),
        (failed(error_code::UPDATE_NOT_FOUND), Why::Changed),
        (failed(error_code::BAD_GPG_SIGNATURE), Why::Unsigned),
        (failed(error_code::MISSING_GPG_SIGNATURE), Why::Unsigned),
        (
            failed(error_code::CANNOT_INSTALL_REPO_UNSIGNED),
            Why::Unsigned,
        ),
        (
            failed(error_code::CANNOT_UPDATE_REPO_UNSIGNED),
            Why::Unsigned,
        ),
        (failed(13), Why::Broken),
        (
            PackageKitError::TimedOut {
                waited: Duration::from_secs(1),
                waiting: false,
            },
            Why::Unanswered,
        ),
        (PackageKitError::Vanished, Why::Unanswered),
        (PackageKitError::Absent, Why::Broken),
    ] {
        assert_eq!(why_of_packagekit(&error), why, "{error}");
    }
}

fn said(info: u32, name: &str) -> Updating {
    Updating::Package {
        info,
        id: format!("{name};2.0;amd64;x"),
    }
}

/// Folds `updates` and gives the last progress shown.
fn tracked(simulated: &Simulated, updates: Vec<Updating>) -> (Tracker, Progress) {
    let mut tracker = Tracker::new(simulated);
    let mut last = Progress::preparing();
    for updating in updates {
        if let Some(progress) = tracker.take(updating, false) {
            last = progress;
        }
    }
    (tracker, last)
}

#[test]
fn the_progress_follows_the_download_then_dpkg_counting_only_what_dpkg_ends() {
    // Catches the download's `Finished` counted as packages installed, an
    // unpacking taken for an end, and dpkg's phase never seen without a
    // `Commit` status.
    let simulated = Simulated {
        updated: vec!["bash".into(), "curl".into()],
        ..Simulated::default()
    };
    let (_, progress) = tracked(
        &simulated,
        vec![
            Updating::Status(status::WAITING_FOR_AUTH),
            Updating::Status(status::DOWNLOAD),
            Updating::Cancellable(true),
            Updating::Percentage(30),
            said(10, "bash"),
            said(info::FINISHED, "bash"),
            said(10, "curl"),
            said(info::FINISHED, "curl"),
        ],
    );
    assert_eq!(progress.phase, Phase::Downloading);
    assert_eq!(progress.package.as_deref(), Some("curl"));
    assert_eq!(progress.done, 0, "the download's ends are not dpkg's");
    assert_eq!(progress.percentage, Some(30));
    assert!(progress.cancellable);

    let (_, progress) = tracked(
        &simulated,
        vec![
            Updating::Status(status::DOWNLOAD),
            said(10, "bash"),
            said(info::FINISHED, "bash"),
            Updating::Cancellable(false),
            Updating::Percentage(packagekit::PERCENTAGE_UNKNOWN),
            Updating::Status(3),
            said(info::PREPARING, "bash"),
            said(info::DECOMPRESSING, "bash"),
            // Unpacked, dpkg moves on: not an end.
            said(info::FINISHED, "bash"),
            said(info::DECOMPRESSING, "curl"),
            said(info::FINISHED, "curl"),
            said(info::INSTALLING, "bash"),
            said(info::FINISHED, "bash"),
            said(info::INSTALLING, "curl"),
        ],
    );
    assert_eq!(progress.phase, Phase::Installing);
    assert_eq!((progress.done, progress.total), (1, 2));
    assert_eq!(progress.package.as_deref(), Some("curl"));
    assert_eq!(progress.percentage, None);
    assert!(!progress.cancellable);

    // Packages already downloaded: no download at all.
    let (tracker, progress) = tracked(
        &simulated,
        vec![Updating::Status(3), said(info::PREPARING, "bash")],
    );
    assert!(tracker.in_dpkg);
    assert_eq!(progress.phase, Phase::Installing);
    let (tracker, progress) = tracked(&simulated, vec![Updating::Status(3)]);
    assert!(!tracker.in_dpkg);
    assert_eq!(progress.phase, Phase::Preparing);
    // Waiting and cancelling are said over the rest.
    let (_, progress) = tracked(
        &simulated,
        vec![
            Updating::Status(status::DOWNLOAD),
            Updating::Status(status::WAITING_FOR_LOCK),
        ],
    );
    assert_eq!(progress.phase, Phase::Waiting);
    let (_, progress) = tracked(&simulated, vec![Updating::Status(status::CANCEL)]);
    assert_eq!(progress.phase, Phase::Cancelling);
}

#[test]
fn a_removal_never_shown_is_caught_as_dpkg_says_it() {
    // Catches a removal the real transaction resolved since the simulation
    // going unseen; and a package shown, updated or added taken for one.
    let simulated = Simulated {
        updated: vec!["bash".into()],
        added: vec![Affected {
            name: "linux-image".into(),
            architecture: "amd64".into(),
            summary: String::new(),
        }],
        removed: vec![Affected {
            name: "libold1".into(),
            architecture: "amd64".into(),
            summary: String::new(),
        }],
    };
    let (tracker, _) = tracked(
        &simulated,
        vec![
            said(info::REMOVING, "libold1"),
            said(info::REMOVING, "bash"),
            said(info::REMOVING, "linux-image"),
            said(info::REMOVING, "ubuntu-desktop"),
            said(info::REMOVING, "ubuntu-desktop"),
        ],
    );
    assert_eq!(tracker.unshown, ["ubuntu-desktop"]);

    let fake = Fake {
        said: vec![said(info::REMOVING, "ubuntu-desktop")],
        ..Fake::simulating(vec![vec![package(info::UPDATING, "bash")]])
    };
    let ended = ended(install(&fake, &[]));
    assert_eq!(ended.unshown_removals, ["ubuntu-desktop"]);
    assert!(ended.reached_dpkg);
}

#[test]
fn a_snap_change_shows_its_downloads_then_its_installation() {
    let downloading = snap_progress(&change(
        "Doing",
        vec![
            task(snapd::TASK_DOWNLOAD, "Done", 300, 300),
            task(snapd::TASK_DOWNLOAD, "Doing", 100, 700),
            task("link-snap", "Do", 0, 1),
        ],
    ));
    assert_eq!(downloading.phase, Phase::Downloading);
    assert_eq!(downloading.bytes, Some((400, 1000)));
    assert_eq!(downloading.percentage, Some(40));
    let installing = snap_progress(&change(
        "Doing",
        vec![
            task(snapd::TASK_DOWNLOAD, "Done", 700, 700),
            task("link-snap", "Doing", 0, 1),
        ],
    ));
    assert_eq!(installing.phase, Phase::Installing);
    assert_eq!(installing.bytes, None);
}

#[test]
fn each_end_of_a_snap_refresh_is_told() {
    let ending = |refresh: fn() -> Result<String, SnapdError>, followed: fn() -> Followed| {
        install_snaps(
            &Fake {
                refresh,
                followed,
                ..Fake::default()
            },
            &["snap-store".into()],
            &|_| {},
        )
        .end
    };
    let done = || Followed::Ready(change(CHANGE_DONE, Vec::new()));
    assert_eq!(ending(|| Ok("7".into()), done), End::Done);
    assert_eq!(
        ending(
            || Ok("7".into()),
            || Followed::Ready(change("Error", Vec::new()))
        ),
        End::Failed(Why::Broken)
    );
    assert_eq!(
        ending(|| Ok("7".into()), || Followed::Unanswered),
        End::Failed(Why::Unanswered)
    );
    assert_eq!(
        ending(
            || {
                Err(SnapdError::Refused {
                    status: 403,
                    kind: snapd::KIND_AUTH_CANCELLED.into(),
                    message: String::new(),
                })
            },
            done
        ),
        End::Failed(Why::Refused)
    );
    assert_eq!(
        ending(
            || {
                Err(SnapdError::Refused {
                    status: 409,
                    kind: snapd::KIND_CHANGE_CONFLICT.into(),
                    message: String::new(),
                })
            },
            done
        ),
        End::Failed(Why::Busy)
    );
    // As snapd 2.78 said it, with no kind.
    assert_eq!(
        ending(
            || {
                Err(SnapdError::Refused {
                    status: 400,
                    kind: String::new(),
                    message: "cannot refresh \"snap-store\": snap \"snap-store\" has running apps (snap-store), pids: 1221553".into(),
                })
            },
            done
        ),
        End::Failed(Why::AppsOpen)
    );
    // Its progress is heard.
    let heard = RefCell::new(Vec::new());
    install_snaps(
        &Fake {
            changes: vec![change(
                "Doing",
                vec![task(snapd::TASK_DOWNLOAD, "Doing", 1, 4)],
            )],
            ..Fake::default()
        },
        &["snap-store".into()],
        &|progress| heard.borrow_mut().push(progress.phase),
    );
    assert_eq!(*heard.borrow(), [Phase::Preparing, Phase::Downloading]);
}

fn update(name: &str, arch: &str, version: &str, kind: Kind) -> SystemUpdate {
    SystemUpdate {
        id: format!("{name};{version};{arch};x"),
        name: name.into(),
        version: version.into(),
        installed: None,
        summary: String::new(),
        kind,
    }
}

#[test]
fn what_is_left_is_known_by_name_and_architecture_never_by_its_shown_name() {
    // Catches a comparison on the shown name (`libfoo` or `libfoo:i386`,
    // as each reading says it) or on the version.
    let asked = [
        SystemUpdate {
            name: "libfoo:amd64".into(),
            ..update("libfoo", "amd64", "2", Kind::Other)
        },
        SystemUpdate {
            name: "libfoo:i386".into(),
            ..update("libfoo", "i386", "2", Kind::Other)
        },
        update("bash", "amd64", "5", Kind::Security),
        update("curl", "amd64", "8", Kind::Other),
    ];
    let read = [
        // Alone now, named without its architecture.
        update("libfoo", "i386", "2", Kind::Other),
        // A newer version come since: still waiting.
        update("bash", "amd64", "6", Kind::Security),
        // Held back now: still waiting.
        update("curl", "amd64", "8", Kind::Blocked),
        // Never asked.
        update("zsh", "amd64", "5", Kind::Other),
    ];
    assert_eq!(system_left(&asked, &read), ["libfoo:i386", "bash", "curl"]);
}

#[test]
fn removals_finished_count_and_the_count_never_passes_the_total() {
    // Catches a removal never counted as finished, and dpkg finishing more
    // packages than the update handles (its triggers) shown past the total.
    let simulated = Simulated {
        updated: vec!["bash".into()],
        removed: vec![Affected {
            name: "libold1".into(),
            architecture: "amd64".into(),
            summary: String::new(),
        }],
        ..Simulated::default()
    };
    let (_, progress) = tracked(
        &simulated,
        vec![
            said(info::REMOVING, "libold1"),
            said(info::FINISHED, "libold1"),
            said(info::INSTALLING, "bash"),
            said(info::FINISHED, "bash"),
            said(info::INSTALLING, "man-db"),
            said(info::FINISHED, "man-db"),
        ],
    );
    assert_eq!((progress.done, progress.total), (2, 2));
}

#[test]
fn a_package_of_the_update_unreadable_leaves_the_removals_unchecked() {
    // Catches a removal never shown that dpkg said unreadably, gone unseen.
    let (tracker, _) = tracked(&Simulated::default(), vec![Updating::Unreadable]);
    assert!(tracker.unchecked);
    let (tracker, _) = tracked(
        &Simulated::default(),
        vec![Updating::Package {
            info: info::REMOVING,
            id: "broken".into(),
        }],
    );
    assert!(tracker.unchecked);
    let (tracker, _) = tracked(&Simulated::default(), vec![said(info::INSTALLING, "bash")]);
    assert!(!tracker.unchecked);
}

#[test]
fn authorizing_is_said_until_something_else_is() {
    let (_, progress) = tracked(
        &Simulated::default(),
        vec![Updating::Status(status::WAITING_FOR_AUTH)],
    );
    assert_eq!(progress.phase, Phase::Authorizing);
}

#[test]
fn a_download_that_has_not_started_says_no_bytes() {
    // Catches a division by a total of nothing, and a task over (held,
    // undone) taken for a download under way.
    let starting = snap_progress(&change(
        "Doing",
        vec![task(snapd::TASK_DOWNLOAD, "Doing", 0, 0)],
    ));
    assert_eq!(starting.phase, Phase::Downloading);
    assert_eq!((starting.bytes, starting.percentage), (None, None));
    let held = snap_progress(&change(
        "Doing",
        vec![task(snapd::TASK_DOWNLOAD, "Hold", 1, 4)],
    ));
    assert_eq!(held.phase, Phase::Installing);
}

#[test]
fn dpkg_is_known_started_by_its_status_or_by_cancelling_let_go() {
    // Catches a failure once dpkg ran said « nothing installed » because no
    // package of dpkg could be read.
    let (tracker, _) = tracked(
        &Simulated::default(),
        vec![Updating::Status(status::COMMIT)],
    );
    assert!(tracker.in_dpkg);
    let (tracker, _) = tracked(
        &Simulated::default(),
        vec![Updating::Cancellable(true), Updating::Cancellable(false)],
    );
    assert!(tracker.in_dpkg);
    // Never cancellable, or still: no dpkg.
    for said in [
        vec![Updating::Cancellable(false)],
        vec![Updating::Cancellable(true), Updating::Cancellable(true)],
    ] {
        let (tracker, _) = tracked(&Simulated::default(), said);
        assert!(!tracker.in_dpkg);
    }
    // A cancel asked and refused lets go too: no dpkg.
    let mut tracker = Tracker::new(&Simulated::default());
    tracker.take(Updating::Cancellable(true), false);
    tracker.take(Updating::Status(status::DOWNLOAD), true);
    tracker.take(Updating::Cancellable(false), true);
    assert!(!tracker.in_dpkg);
    // The cancel asked lets go too: no dpkg.
    let (tracker, _) = tracked(
        &Simulated::default(),
        vec![
            Updating::Cancellable(true),
            Updating::Status(status::CANCEL),
            Updating::Cancellable(false),
        ],
    );
    assert!(!tracker.in_dpkg);
}

#[test]
fn an_update_given_up_while_polkit_asked_is_the_rights_not_given() {
    let fake = Fake {
        said: vec![Updating::Status(status::WAITING_FOR_AUTH)],
        updated: || {
            Err(PackageKitError::TimedOut {
                waited: Duration::from_secs(1),
                waiting: false,
            })
        },
        ..Fake::simulating(vec![vec![package(info::UPDATING, "bash")]])
    };
    assert_eq!(ended(install(&fake, &[])).end, End::Failed(Why::Refused));
}

#[test]
fn removals_unchecked_reach_the_end() {
    let fake = Fake {
        said: vec![Updating::Unreadable],
        ..Fake::simulating(vec![vec![package(info::UPDATING, "bash")]])
    };
    assert!(ended(install(&fake, &[])).removals_unchecked);
}
