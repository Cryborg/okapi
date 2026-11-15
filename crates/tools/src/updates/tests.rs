use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use super::*;
use crate::packages::packagekit::error_code;
use crate::packages::snapd::Publisher;

/// `PkInfoEnum` values that are neither security nor held back.
const INFO_LOW: u32 = 3;
const INFO_ENHANCEMENT: u32 = 4;
const INFO_NORMAL: u32 = 5;
const INFO_BUGFIX: u32 = 6;
const INFO_IMPORTANT: u32 = 7;
/// `PkErrorEnum::NoCache`: no class of its own.
const NO_CACHE: u32 = 18;

/// Sources that answer what they are given, count the refreshes asked, and
/// say they wait (while listing, or while downloading) when told to.
struct Fake {
    refresh: fn() -> Result<(), PackageKitError>,
    system: fn() -> Result<Vec<Package>, PackageKitError>,
    stamp: fn() -> io::Result<Option<SystemTime>>,
    snaps: fn() -> Result<Vec<Refresh>, SnapdError>,
    schedule: fn() -> Result<Schedule, SnapdError>,
    reboot: fn() -> io::Result<Option<RebootRequired>>,
    waits: bool,
    refresh_waits: bool,
    refreshes: AtomicU32,
}

impl Default for Fake {
    fn default() -> Self {
        Self {
            refresh: || Ok(()),
            system: || Ok(Vec::new()),
            stamp: || Ok(None),
            snaps: || Ok(Vec::new()),
            schedule: || Ok(Schedule::default()),
            reboot: || Ok(None),
            waits: false,
            refresh_waits: false,
            refreshes: AtomicU32::new(0),
        }
    }
}

impl Sources for Fake {
    fn refresh_system(&self, waiting: &dyn Fn(bool)) -> Result<(), PackageKitError> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        if self.refresh_waits {
            waiting(true);
            waiting(false);
        }
        (self.refresh)()
    }
    fn system(&self, waiting: &dyn Fn(bool)) -> Result<Vec<Package>, PackageKitError> {
        if self.waits {
            waiting(true);
            waiting(false);
        }
        (self.system)()
    }
    fn lists_downloaded_at(&self) -> io::Result<Option<SystemTime>> {
        (self.stamp)()
    }
    fn snaps(&self) -> Result<Vec<Refresh>, SnapdError> {
        (self.snaps)()
    }
    fn schedule(&self) -> Result<Schedule, SnapdError> {
        (self.schedule)()
    }
    fn reboot(&self) -> io::Result<Option<RebootRequired>> {
        (self.reboot)()
    }
}

fn package(info: u32, name: &str) -> Package {
    Package {
        info,
        id: format!("{name};2.0-1ubuntu1;amd64;resolute-updates"),
        summary: format!("{name}, said briefly"),
    }
}

/// Six packages, out of order: two of security, one held back, three
/// others of every other kind.
#[expect(clippy::unnecessary_wraps, reason = "the shape of a source")]
fn mixed() -> Result<Vec<Package>, PackageKitError> {
    Ok(vec![
        package(info::BLOCKED, "aardvark"),
        package(INFO_BUGFIX, "zlib1g"),
        package(info::SECURITY, "openssl"),
        package(INFO_LOW, "bash"),
        package(info::SECURITY, "curl"),
        package(INFO_IMPORTANT, "mesa"),
    ])
}

fn quiet(_: bool) {}

fn snap(name: &str, title: Option<&str>, validation: Option<&str>) -> Refresh {
    Refresh {
        name: name.into(),
        title: title.map(Into::into),
        version: format!("{name} 1.0"),
        publisher: Some(Publisher {
            display_name: format!("{name} publisher"),
            validation: validation.map(Into::into),
        }),
        download_size: u64::try_from(name.len()).ok(),
    }
}

#[test]
fn security_comes_first_held_back_last_by_name() {
    // Catches a kind misread or an order lost.
    let fake = Fake {
        system: mixed,
        ..Fake::default()
    };
    let Outcome::Read(updates) = read_system(&fake, false, &quiet).updates else {
        panic!("read");
    };
    let shown: Vec<(&str, Kind)> = updates
        .iter()
        .map(|update| (update.name.as_str(), update.kind))
        .collect();
    assert_eq!(
        shown,
        [
            ("curl", Kind::Security),
            ("openssl", Kind::Security),
            ("bash", Kind::Other),
            ("mesa", Kind::Other),
            ("zlib1g", Kind::Other),
            ("aardvark", Kind::Blocked),
        ]
    );
    assert_eq!(updates[0].version, "2.0-1ubuntu1");
    assert_eq!(updates[0].summary, "curl, said briefly");
    // An unknown or lesser kind is no security.
    let fake = Fake {
        system: || {
            Ok(vec![
                package(99, "odd"),
                package(INFO_ENHANCEMENT, "new"),
                package(INFO_NORMAL, "plain"),
            ])
        },
        ..Fake::default()
    };
    let Outcome::Read(updates) = read_system(&fake, false, &quiet).updates else {
        panic!("read");
    };
    assert_eq!(updates.len(), 3);
    assert!(updates.iter().all(|update| update.kind == Kind::Other));
}

#[test]
fn what_upgrading_everything_would_also_do_is_no_update() {
    // Catches a package apt would install, remove, obsolete or downgrade
    // counted, shown and sent as an update; and one of them taken for the
    // other architecture of an update.
    let fake = Fake {
        system: || {
            Ok(vec![
                package(info::SECURITY, "openssl"),
                package(INFO_NORMAL, "bash"),
                package(info::BLOCKED, "aardvark"),
                package(info::INSTALL, "linux-image-7.1"),
                package(info::REMOVE, "libold1"),
                package(info::OBSOLETE, "python3-gone"),
                Package {
                    info: info::DOWNGRADE,
                    id: "bash;5.1-1;i386;resolute".into(),
                    summary: String::new(),
                },
            ])
        },
        ..Fake::default()
    };
    let Outcome::Read(updates) = read_system(&fake, false, &quiet).updates else {
        panic!("read");
    };
    let names: Vec<&str> = updates.iter().map(|update| update.name.as_str()).collect();
    assert_eq!(names, ["openssl", "bash", "aardvark"]);
}

#[test]
fn a_wait_for_another_program_is_passed_on() {
    let heard = RefCell::new(Vec::new());
    let fake = Fake {
        waits: true,
        ..Fake::default()
    };
    read_system(&fake, false, &|waiting| heard.borrow_mut().push(waiting));
    assert_eq!(*heard.borrow(), [true, false]);
    // While downloading the lists too: the longest wait.
    let heard = RefCell::new(Vec::new());
    let fake = Fake {
        refresh_waits: true,
        ..Fake::default()
    };
    read_system(&fake, true, &|waiting| heard.borrow_mut().push(waiting));
    assert_eq!(*heard.borrow(), [true, false]);
}

#[test]
fn a_package_whose_identifier_cannot_be_read_makes_the_list_fail() {
    // Catches a list short of a package passing for the whole one.
    let fake = Fake {
        system: || {
            Ok(vec![
                package(info::SECURITY, "openssl"),
                Package {
                    info: info::SECURITY,
                    id: "broken;id".into(),
                    summary: String::new(),
                },
            ])
        },
        ..Fake::default()
    };
    assert_eq!(
        read_system(&fake, false, &quiet).updates,
        Outcome::Failed(Failure::Broken)
    );
}

#[test]
fn a_name_of_two_architectures_says_each() {
    let fake = Fake {
        system: || {
            Ok(vec![
                Package {
                    info: INFO_NORMAL,
                    id: "libc6;2.42-1;i386;resolute-updates".into(),
                    summary: String::new(),
                },
                package(INFO_NORMAL, "bash"),
                package(INFO_NORMAL, "libc6"),
            ])
        },
        ..Fake::default()
    };
    let Outcome::Read(updates) = read_system(&fake, false, &quiet).updates else {
        panic!("read");
    };
    let names: Vec<&str> = updates.iter().map(|update| update.name.as_str()).collect();
    assert_eq!(names, ["bash", "libc6:amd64", "libc6:i386"]);
}

#[test]
fn the_lists_are_dated_by_apt_s_stamp_if_it_can_be_read() {
    let fake = Fake {
        stamp: || {
            Ok(Some(
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_763_100_007),
            ))
        },
        ..Fake::default()
    };
    assert_eq!(
        read_system(&fake, false, &quiet).listed_at,
        Some(Timestamp::from_second(1_763_100_007).unwrap())
    );
    let fake = Fake {
        stamp: || Err(io::ErrorKind::PermissionDenied.into()),
        ..Fake::default()
    };
    assert_eq!(read_system(&fake, false, &quiet).listed_at, None);
}

#[test]
fn snaps_are_titled_by_title_or_name_and_vouched_for_by_the_store() {
    let fake = Fake {
        snaps: || {
            Ok(vec![
                snap("zoom-client", Some("Zoom"), None),
                snap("snap-store", Some("App Center"), Some("verified")),
                snap("bare", Some("  "), Some("starred")),
                snap("core22", None, Some("unproven")),
            ])
        },
        schedule: || {
            Ok(Schedule {
                next: Some("2026-11-15T01:37:00+01:00".into()),
                hold: None,
            })
        },
        ..Fake::default()
    };
    let Outcome::Read(snaps) = read_snaps(&fake) else {
        panic!("read");
    };
    let shown: Vec<(&str, &str, bool)> = snaps
        .updates
        .iter()
        .map(|update| (update.title.as_str(), update.name.as_str(), update.verified))
        .collect();
    // Each field where it belongs.
    assert_eq!(
        snaps.updates[0],
        SnapUpdate {
            name: "snap-store".into(),
            title: "App Center".into(),
            publisher: Some("snap-store publisher".into()),
            verified: true,
            version: "snap-store 1.0".into(),
            download_size: Some(10),
        }
    );
    assert_eq!(
        shown,
        [
            ("App Center", "snap-store", true),
            ("bare", "bare", true),
            ("core22", "core22", false),
            ("Zoom", "zoom-client", false),
        ]
    );
    assert_eq!(
        snaps.next_refresh,
        Some("2026-11-15T00:37:00Z".parse().unwrap())
    );
}

#[test]
fn a_schedule_that_cannot_be_read_hides_nothing_else() {
    for schedule in [(|| Err(SnapdError::Malformed("test"))) as fn() -> _, || {
        Ok(Schedule {
            next: Some("tomorrow".into()),
            hold: None,
        })
    }] {
        let fake = Fake {
            snaps: || Ok(vec![snap("a", None, None)]),
            schedule,
            ..Fake::default()
        };
        let Outcome::Read(snaps) = read_snaps(&fake) else {
            panic!("read");
        };
        assert_eq!((snaps.updates.len(), snaps.next_refresh), (1, None));
    }
}

#[test]
fn each_source_says_its_own_failure_or_absence() {
    let fake = Fake {
        system: || {
            Err(PackageKitError::Failed {
                code: error_code::CANNOT_GET_LOCK,
                details: "held".into(),
            })
        },
        snaps: || Ok(vec![snap("a", None, None)]),
        ..Fake::default()
    };
    assert_eq!(
        read_system(&fake, false, &quiet).updates,
        Outcome::Failed(Failure::Busy)
    );
    assert!(matches!(read_snaps(&fake), Outcome::Read(snaps) if snaps.updates.len() == 1));

    let fake = Fake {
        refresh: || Err(PackageKitError::Absent),
        system: || Err(PackageKitError::Absent),
        snaps: || {
            Err(SnapdError::Absent {
                socket: PathBuf::from("/run/snapd.socket"),
                source: io::ErrorKind::NotFound.into(),
            })
        },
        ..Fake::default()
    };
    let reading = read_system(&fake, true, &quiet);
    // No PackageKit is no failed download either.
    assert_eq!(
        (reading.updates, reading.refresh),
        (Outcome::Absent, ListsRefresh::NotAsked)
    );
    assert_eq!(read_snaps(&fake), Outcome::Absent);

    // snapd fails, the system is read all the same.
    let fake = Fake {
        system: mixed,
        snaps: || Err(SnapdError::Io(io::ErrorKind::TimedOut.into())),
        ..Fake::default()
    };
    assert_eq!(read_snaps(&fake), Outcome::Failed(Failure::Unanswered));
    assert!(
        matches!(read_system(&fake, false, &quiet).updates, Outcome::Read(updates) if updates.len() == 6)
    );
}

#[test]
fn a_refresh_is_asked_only_when_told_and_its_failure_keeps_the_old_list() {
    let fake = Fake {
        system: mixed,
        ..Fake::default()
    };
    let reading = read_system(&fake, false, &quiet);
    assert_eq!(
        (fake.refreshes.load(Ordering::SeqCst), reading.refresh),
        (0, ListsRefresh::NotAsked)
    );
    let reading = read_system(&fake, true, &quiet);
    assert_eq!(
        (fake.refreshes.load(Ordering::SeqCst), reading.refresh),
        (1, ListsRefresh::Done)
    );

    let fake = Fake {
        refresh: || {
            Err(PackageKitError::Failed {
                code: error_code::NO_NETWORK,
                details: "offline".into(),
            })
        },
        system: mixed,
        ..Fake::default()
    };
    let reading = read_system(&fake, true, &quiet);
    assert_eq!(reading.refresh, ListsRefresh::Failed(Failure::Offline));
    assert!(matches!(&reading.updates, Outcome::Read(updates) if updates.len() == 6));
}

#[test]
fn each_error_is_told_in_the_words_of_its_cause() {
    let failed = |code| PackageKitError::Failed {
        code,
        details: String::new(),
    };
    let timed_out = |waiting| PackageKitError::TimedOut {
        waited: Duration::from_secs(1),
        waiting,
    };
    for (error, failure) in [
        (failed(error_code::CANNOT_GET_LOCK), Failure::Busy),
        (failed(error_code::LOCK_REQUIRED), Failure::Busy),
        (failed(error_code::NO_NETWORK), Failure::Offline),
        (failed(error_code::REPO_NOT_AVAILABLE), Failure::Offline),
        (failed(error_code::NO_MORE_MIRRORS_TO_TRY), Failure::Offline),
        (failed(error_code::CANNOT_FETCH_SOURCES), Failure::Offline),
        (failed(error_code::NOT_AUTHORIZED), Failure::Refused),
        (failed(NO_CACHE), Failure::Broken),
        (timed_out(true), Failure::Busy),
        (timed_out(false), Failure::Unanswered),
        (PackageKitError::Ended { exit: 2 }, Failure::Broken),
        (PackageKitError::Vanished, Failure::Broken),
        (PackageKitError::Transport("tcp:".into()), Failure::Broken),
        // A call to the bus with no reply in time, and one that broke.
        (
            PackageKitError::Bus(zbus::Error::InputOutput(std::sync::Arc::new(
                io::ErrorKind::TimedOut.into(),
            ))),
            Failure::Unanswered,
        ),
        (
            PackageKitError::Bus(zbus::Error::InputOutput(std::sync::Arc::new(
                io::ErrorKind::ConnectionRefused.into(),
            ))),
            Failure::Broken,
        ),
        (
            PackageKitError::Unreadable {
                member: "Package".into(),
            },
            Failure::Broken,
        ),
    ] {
        assert_eq!(failure_of_packagekit(&error), failure, "{error:?}");
    }
    let refused = |kind: &str| SnapdError::Refused {
        status: 500,
        kind: kind.into(),
        message: String::new(),
    };
    for (error, failure) in [
        (
            SnapdError::Io(io::ErrorKind::TimedOut.into()),
            Failure::Unanswered,
        ),
        (
            SnapdError::Io(io::ErrorKind::WouldBlock.into()),
            Failure::Unanswered,
        ),
        (refused(snapd::KIND_NETWORK_TIMEOUT), Failure::Offline),
        (refused(snapd::KIND_DNS_FAILURE), Failure::Offline),
        (refused("login-required"), Failure::Broken),
        (SnapdError::TooLarge, Failure::Broken),
    ] {
        assert_eq!(failure_of_snapd(&error), failure, "{error:?}");
    }
}

#[test]
fn a_restart_awaited_is_told_and_its_check_failing_is_none() {
    let fake = Fake {
        reboot: || {
            Ok(Some(RebootRequired {
                packages: Some(vec!["libc6".into()]),
            }))
        },
        ..Fake::default()
    };
    assert_eq!(
        read_reboot(&fake),
        Some(RebootRequired {
            packages: Some(vec!["libc6".into()])
        })
    );
    let fake = Fake {
        reboot: || Err(io::ErrorKind::PermissionDenied.into()),
        ..Fake::default()
    };
    assert_eq!(read_reboot(&fake), None);
}

#[test]
fn a_hold_puts_off_the_next_refresh() {
    // Catches a pass said for a time snapd will not refresh at.
    for (hold, next) in [
        (Some("2026-11-20T12:00:00Z"), "2026-11-20T12:00:00Z"),
        (Some("2026-11-01T12:00:00Z"), "2026-11-15T00:37:00Z"),
        (None, "2026-11-15T00:37:00Z"),
    ] {
        let schedule = Schedule {
            next: Some("2026-11-15T01:37:00+01:00".into()),
            hold: hold.map(Into::into),
        };
        assert_eq!(
            next_refresh(Ok(schedule)),
            Some(next.parse().unwrap()),
            "{hold:?}"
        );
    }
}
