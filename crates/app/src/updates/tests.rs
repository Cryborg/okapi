//! The updates screen in a window, its sources stand-ins: what it reads when
//! it is shown, what it says of each source, and what it installs.

use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Instant, SystemTime};

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use okapi_tools::packages::packagekit::{
    Details, Package, PackageKitError, Updated, Updating, error_code, info,
};
use okapi_tools::packages::snapd::{
    CHANGE_DONE, Change, Followed, Publisher, Refresh, Schedule, SnapdError,
};
use okapi_tools::updates::install::{End, Ended, Why};
use slint::Model as _;

use super::install::{Outcome, Target, outcome, retriable, why_shown};

use super::*;
use crate::cleaner::Seams;
use crate::cleaner::tests::{button, seams_never_scanning, shows};

/// How long a test waits for a search: a fault fails, it never hangs.
const WAIT: Duration = Duration::from_secs(20);
/// 2026-11-14 at 22:00 UTC: the tests' « now », moved by `later`.
const NOW: i64 = 1_794_693_600;
/// The same day at 3:56 UTC: apt's lists downloaded that morning.
const STAMP_TODAY: u64 = 1_794_628_567;

thread_local! {
    static SHIFT: Cell<i64> = const { Cell::new(0) };
}

fn now() -> Timestamp {
    Timestamp::from_second(NOW + SHIFT.with(Cell::get)).unwrap()
}

/// The clock moves `seconds` on.
fn later(seconds: i64) {
    SHIFT.with(|shift| shift.set(shift.get() + seconds));
}

/// Sources that answer what they are given, count what they are asked,
/// and hold the system's reading until let go when told to.
struct Fake {
    system: fn() -> Result<Vec<Package>, PackageKitError>,
    refresh: fn() -> Result<(), PackageKitError>,
    snaps: fn() -> Result<Vec<Refresh>, SnapdError>,
    schedule: fn() -> Result<Schedule, SnapdError>,
    reboot: fn() -> io::Result<Option<RebootRequired>>,
    stamp: fn() -> io::Result<Option<SystemTime>>,
    /// When set, the next system's reading waits for another program until
    /// a word comes.
    hold: Mutex<Option<mpsc::Receiver<()>>>,
    /// When set, the next reading of the snaps waits until a word comes.
    hold_snaps: Mutex<Option<mpsc::Receiver<()>>>,
    readings: AtomicU32,
    refreshes: AtomicU32,
    /// While set, downloading the lists finds no network.
    offline: AtomicBool,
    /// The download size of each update, by its identifier.
    size: fn(&str) -> Option<u64>,
    /// What each simulation removes beyond the updates, in turn (none once
    /// all said).
    removals: Mutex<Vec<Vec<&'static str>>>,
    simulations: AtomicU32,
    /// The identifiers of each simulation asked.
    simulated_ids: Mutex<Vec<Vec<String>>>,
    /// What the update says as it goes.
    said: Vec<Updating>,
    /// When set, the update waits for a word, then ends cancelled if asked.
    hold_update: Mutex<Option<mpsc::Receiver<()>>>,
    updated: fn() -> Result<Updated, PackageKitError>,
    /// The identifiers of each update asked, the names of each refresh.
    updates_asked: Mutex<Vec<Vec<String>>>,
    refreshes_asked: Mutex<Vec<Vec<String>>>,
    followed: fn() -> Followed,
    /// What the system and the snaps say once an installation ran.
    system_after: fn() -> Result<Vec<Package>, PackageKitError>,
    snaps_after: fn() -> Result<Vec<Refresh>, SnapdError>,
    installed: AtomicBool,
    /// The update's thread dies without its end.
    dies: bool,
    /// What the update says once let go.
    said_after_hold: Vec<Updating>,
}

impl Default for Fake {
    fn default() -> Self {
        Self {
            system: || Ok(Vec::new()),
            refresh: || Ok(()),
            snaps: || Ok(Vec::new()),
            schedule: || Ok(Schedule::default()),
            reboot: || Ok(None),
            // Downloaded this very morning: the lists are fresh.
            stamp: || {
                Ok(Some(
                    SystemTime::UNIX_EPOCH + Duration::from_secs(STAMP_TODAY),
                ))
            },
            hold: Mutex::new(None),
            hold_snaps: Mutex::new(None),
            readings: AtomicU32::new(0),
            refreshes: AtomicU32::new(0),
            offline: AtomicBool::new(false),
            size: |_| None,
            removals: Mutex::new(Vec::new()),
            simulations: AtomicU32::new(0),
            simulated_ids: Mutex::new(Vec::new()),
            said: Vec::new(),
            hold_update: Mutex::new(None),
            updated: || {
                Ok(Updated {
                    system_restart: false,
                })
            },
            updates_asked: Mutex::new(Vec::new()),
            refreshes_asked: Mutex::new(Vec::new()),
            followed: || {
                Followed::Ready(Change {
                    status: CHANGE_DONE.into(),
                    ready: true,
                    err: None,
                    tasks: Vec::new(),
                })
            },
            system_after: || Ok(Vec::new()),
            snaps_after: || Ok(Vec::new()),
            installed: AtomicBool::new(false),
            dies: false,
            said_after_hold: Vec::new(),
        }
    }
}

impl Sources for Fake {
    fn refresh_system(&self, _waiting: &dyn Fn(bool)) -> Result<(), PackageKitError> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        if self.offline.load(Ordering::SeqCst) {
            return Err(PackageKitError::Failed {
                code: error_code::NO_NETWORK,
                details: "offline".into(),
            });
        }
        (self.refresh)()
    }
    fn system(&self, waiting: &dyn Fn(bool)) -> Result<Vec<Package>, PackageKitError> {
        self.readings.fetch_add(1, Ordering::SeqCst);
        // Held once: the next reading goes through.
        let hold = self.hold.lock().unwrap().take();
        if let Some(hold) = hold {
            waiting(true);
            hold.recv().unwrap();
            waiting(false);
        }
        if self.installed.load(Ordering::SeqCst) {
            return (self.system_after)();
        }
        (self.system)()
    }
    fn details(
        &self,
        ids: &[String],
        _waiting: &dyn Fn(bool),
    ) -> Result<Vec<Details>, PackageKitError> {
        Ok(ids
            .iter()
            .map(|id| Details {
                id: id.clone(),
                download_size: (self.size)(id),
            })
            .collect())
    }
    fn lists_downloaded_at(&self) -> io::Result<Option<SystemTime>> {
        (self.stamp)()
    }
    fn snaps(&self) -> Result<Vec<Refresh>, SnapdError> {
        let hold = self.hold_snaps.lock().unwrap().take();
        if let Some(hold) = hold {
            hold.recv().unwrap();
        }
        if self.installed.load(Ordering::SeqCst) {
            return (self.snaps_after)();
        }
        (self.snaps)()
    }
    fn schedule(&self) -> Result<Schedule, SnapdError> {
        (self.schedule)()
    }
    fn reboot(&self) -> io::Result<Option<RebootRequired>> {
        (self.reboot)()
    }
}

impl Installer for Fake {
    fn simulate_system(
        &self,
        ids: &[String],
        _waiting: &dyn Fn(bool),
    ) -> Result<Vec<Package>, PackageKitError> {
        self.simulations.fetch_add(1, Ordering::SeqCst);
        self.simulated_ids.lock().unwrap().push(ids.to_vec());
        let mut packages: Vec<Package> = ids
            .iter()
            .map(|id| Package {
                info: info::UPDATING,
                id: id.clone(),
                summary: String::new(),
            })
            .collect();
        let mut removals = self.removals.lock().unwrap();
        if !removals.is_empty() {
            // A name starting with `+` is brought in, any other removed.
            packages.extend(removals.remove(0).into_iter().map(|name| {
                let (info, name) = name
                    .strip_prefix('+')
                    .map_or((info::REMOVING, name), |added| (info::INSTALLING, added));
                Package {
                    info,
                    id: format!("{name};1;amd64;installed"),
                    summary: format!("{name}, said briefly"),
                }
            }));
        }
        Ok(packages)
    }

    fn update_system(
        &self,
        ids: &[String],
        heard: &dyn Fn(Updating),
        cancel: &AtomicBool,
    ) -> Result<Updated, PackageKitError> {
        self.updates_asked.lock().unwrap().push(ids.to_vec());
        for updating in &self.said {
            heard(updating.clone());
        }
        assert!(!self.dies, "the update's thread dies");
        let hold = self.hold_update.lock().unwrap().take();
        if let Some(hold) = hold {
            hold.recv().unwrap();
            for updating in &self.said_after_hold {
                heard(updating.clone());
            }
            if cancel.load(Ordering::SeqCst) {
                return Err(PackageKitError::Failed {
                    code: error_code::TRANSACTION_CANCELLED,
                    details: String::new(),
                });
            }
        }
        let updated = (self.updated)();
        self.installed.store(updated.is_ok(), Ordering::SeqCst);
        updated
    }

    fn dpkg_interrupted(&self) -> io::Result<bool> {
        Ok(false)
    }

    fn refresh_snaps(&self, names: &[String]) -> Result<String, SnapdError> {
        self.refreshes_asked.lock().unwrap().push(names.to_vec());
        self.installed.store(true, Ordering::SeqCst);
        Ok("7".into())
    }

    fn follow_snaps(&self, _id: &str, _heard: &dyn Fn(&Change)) -> Followed {
        (self.followed)()
    }
}

fn seams_of(fake: &Arc<Fake>) -> UpdatesSeams {
    UpdatesSeams {
        sources: Arc::clone(fake) as Arc<dyn Sources>,
        installer: Arc::clone(fake) as Arc<dyn Installer>,
        distribution: Some("Ubuntu 26.04".into()),
        now,
        zone: TimeZone::UTC,
    }
}

/// No daemon, no file: nothing on this system.
pub(crate) fn nothing() -> UpdatesSeams {
    let fake = Fake {
        system: || Err(PackageKitError::Absent),
        snaps: || {
            Err(SnapdError::Absent {
                socket: PathBuf::from("/run/snapd.socket"),
                source: io::ErrorKind::NotFound.into(),
            })
        },
        ..Fake::default()
    };
    seams_of(&Arc::new(fake))
}

fn package(info: u32, name: &str, version: &str) -> Package {
    Package {
        info,
        id: format!("{name};{version};amd64;resolute-security"),
        summary: format!("{name}, said briefly"),
    }
}

/// Three of security, one held back, four others: eight, more than the
/// first lines.
#[expect(clippy::unnecessary_wraps, reason = "the shape of a source")]
fn eight() -> Result<Vec<Package>, PackageKitError> {
    Ok(vec![
        package(5, "zsh", "5.9-6ubuntu3"),
        package(info::SECURITY, "openssl", "3.5.1-1ubuntu2"),
        package(info::BLOCKED, "aardvark", "1.0-1"),
        package(6, "bash", "5.2.37-2"),
        package(info::SECURITY, "libssl3t64", "3.5.1-1ubuntu2"),
        package(5, "libreoffice-core", "4:25.8.4-0ubuntu1"),
        package(info::SECURITY, "libxml2", "2.14.3+dfsg-1ubuntu0.1"),
        package(7, "mesa", "25.2.1-1"),
    ])
}

#[expect(clippy::unnecessary_wraps, reason = "the shape of a source")]
fn two_snaps() -> Result<Vec<Refresh>, SnapdError> {
    Ok(vec![
        Refresh {
            name: "snap-store".into(),
            title: Some("App Center".into()),
            version: "1.0.4".into(),
            publisher: Some(Publisher {
                display_name: "Canonical".into(),
                validation: Some("verified".into()),
            }),
            download_size: Some(12_283_904),
        },
        Refresh {
            name: "obs-studio".into(),
            title: Some("OBS Studio".into()),
            version: "32.0.2".into(),
            publisher: Some(Publisher {
                display_name: "Snapcrafters".into(),
                validation: None,
            }),
            download_size: Some(74_100_000),
        },
    ])
}

/// The window, its updates read through `fake`, on the updates page.
fn on_updates(fake: &Arc<Fake>) -> ui::AppWindow {
    let seams = Seams {
        updates: seams_of(fake),
        ..seams_never_scanning()
    };
    let crate::Screens { window, .. } =
        crate::wide_window(seams, crate::dashboard::tests::inert(), Language::French);
    button(&window, "Mises à jour").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    window
}

fn updates(window: &ui::AppWindow) -> ui::Updates<'_> {
    window.global::<ui::Updates>()
}

fn read_once() {
    i_slint_backend_testing::mock_elapsed_time(READ_EVERY);
    slint::platform::update_timers_and_animations();
}

fn read_until(window: &ui::AppWindow, done: impl Fn(&ui::AppWindow) -> bool) {
    let started = Instant::now();
    while !done(window) {
        assert!(
            started.elapsed() < WAIT,
            "never done: {:?}",
            updates(window).get_step()
        );
        read_once();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn read(window: &ui::AppWindow) {
    read_until(window, |window| {
        updates(window).get_step() == ui::UpdatesStep::Read
    });
}

/// What a screen reader is told of each line of a list named `name`.
fn list_items(window: &ui::AppWindow) -> Vec<String> {
    ElementHandle::find_by_element_type_name(window, "SystemRow")
        .chain(ElementHandle::find_by_element_type_name(window, "SnapRow"))
        .filter(|row| row.accessible_role() == Some(AccessibleRole::ListItem))
        .filter_map(|row| row.accessible_label().map(String::from))
        .collect()
}

#[test]
fn showing_the_page_reads_from_the_cache_and_again_only_once_old() {
    let fake = Arc::new(Fake::default());
    let window = on_updates(&fake);
    read(&window);
    assert_eq!(fake.readings.load(Ordering::SeqCst), 1);
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 0, "no download");

    let leave_and_come_back = |window: &ui::AppWindow| {
        button(window, "Accueil").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        button(window, "Mises à jour").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        read(window);
    };
    later(4 * 60);
    leave_and_come_back(&window);
    assert_eq!(fake.readings.load(Ordering::SeqCst), 1, "still fresh");
    assert_eq!(updates(&window).get_minutes(), 4);
    later(60);
    leave_and_come_back(&window);
    assert_eq!(fake.readings.load(Ordering::SeqCst), 2, "read again");
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 0);
}

#[test]
fn searching_now_downloads_the_lists_and_retrying_asks_the_same() {
    let fake = Arc::new(Fake::default());
    let window = on_updates(&fake);
    read(&window);
    let (go, hold) = mpsc::channel();
    *fake.hold.lock().unwrap() = Some(hold);
    button(&window, "Rechercher maintenant").invoke_accessible_default_action();
    assert_eq!(updates(&window).get_step(), ui::UpdatesStep::Searching);
    assert!(updates(&window).get_refreshing());
    assert_eq!(
        button(&window, "Recherche…").accessible_enabled(),
        Some(false),
        "no second search while one runs"
    );
    // A second ask while it runs starts nothing.
    updates(&window).invoke_search_now();
    go.send(()).unwrap();
    read(&window);
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(fake.readings.load(Ordering::SeqCst), 2);
    updates(&window).invoke_retry();
    read(&window);
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 2, "the same search");
}

#[test]
fn the_system_is_one_line_that_unfolds_security_first_and_says_each_package() {
    let fake = Arc::new(Fake {
        system: eight,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(shows(
        &window,
        "7 mises à jour, dont 3 de sécurité, recherché à l'instant"
    ));
    let head = button(
        &window,
        "Système Ubuntu 26.04, 7 mises à jour, dont 3 de sécurité, replié",
    );
    assert_eq!(head.accessible_expanded(), Some(false));
    assert_eq!(list_items(&window), Vec::<String>::new(), "folded");
    head.invoke_accessible_default_action();
    assert_eq!(
        list_items(&window),
        [
            "libssl3t64, mise à jour de sécurité, version 3.5.1",
            "libxml2, mise à jour de sécurité, version 2.14.3",
            "openssl, mise à jour de sécurité, version 3.5.1",
            "bash, mise à jour, version 5.2.37",
            "libreoffice-core, mise à jour, version 25.8.4",
            "mesa, mise à jour, version 25.2.1",
        ],
        "the first six"
    );
    button(&window, "Afficher les 2 autres").invoke_accessible_default_action();
    let all = list_items(&window);
    assert_eq!(all.len(), 8);
    assert_eq!(all[6], "zsh, mise à jour, version 5.9");
    assert_eq!(all[7], "aardvark, retenue par le système, version 1.0");
}

#[test]
fn the_snaps_say_who_publishes_them_their_size_and_snapd_s_next_pass() {
    let fake = Arc::new(Fake {
        snaps: two_snaps,
        schedule: || {
            Ok(Schedule {
                // An hour from the tests' now.
                next: Some("2026-11-14T23:00:00Z".into()),
                hold: None,
            })
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert_eq!(
        list_items(&window),
        [
            "App Center, de Canonical, éditeur vérifié, version 1.0.4, 12,2 Mo à télécharger",
            "OBS Studio, de Snapcrafters, version 32.0.2, 74,1 Mo à télécharger",
        ]
    );
    assert!(shows(&window, "2 mises à jour, 86,3 Mo"));
    assert!(shows(
        &window,
        "Les applis snap se mettent à jour seules. Prochain passage aujourd'hui à 23 h."
    ));
}

#[test]
fn a_pass_of_snapd_already_gone_is_not_said() {
    let fake = Arc::new(Fake {
        snaps: two_snaps,
        schedule: || {
            Ok(Schedule {
                next: Some("2026-11-14T21:00:00Z".into()),
                hold: None,
            })
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(!updates(&window).get_has_next_refresh());
    assert!(shows(&window, "Les applis snap se mettent à jour seules."));
}

#[test]
fn the_snaps_are_shown_while_the_system_waits_for_another_program() {
    // Catches a screen that waits for the slowest source, or says nothing
    // while the package manager is held.
    let (go, hold) = mpsc::channel();
    let fake = Arc::new(Fake {
        snaps: two_snaps,
        system: eight,
        hold: Mutex::new(Some(hold)),
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read_until(&window, |window| {
        updates(window).get_waiting() && updates(window).get_snap_lines().row_count() == 2
    });
    assert_eq!(updates(&window).get_step(), ui::UpdatesStep::Searching);
    assert!(shows(
        &window,
        "Une autre installation est en cours : la recherche attend qu'elle finisse."
    ));
    go.send(()).unwrap();
    read(&window);
    assert!(!updates(&window).get_waiting());
    assert_eq!(updates(&window).get_system_count(), 7);
}

#[test]
fn everything_up_to_date_is_said_once_with_each_source() {
    let fake = Arc::new(Fake::default());
    let window = on_updates(&fake);
    read(&window);
    for text in [
        "Tout est à jour",
        "Système Ubuntu 26.04 : aucune mise à jour",
        "Applis : aucune mise à jour",
    ] {
        assert!(shows(&window, text), "{text}");
    }
}

#[test]
fn without_packagekit_nothing_claims_the_system_up_to_date() {
    // No apt stamp either, as on such a system.
    let fake = Arc::new(Fake {
        system: || Err(PackageKitError::Absent),
        stamp: || Ok(None),
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(!shows(&window, "Tout est à jour"));
    assert!(shows(&window, "Les applis sont à jour"));
    assert!(shows(
        &window,
        "Paquets du système : non vérifiés, PackageKit n'est pas installé"
    ));
}

#[test]
fn no_source_at_all_is_said() {
    // No apt stamp either: an unknown age of no list is nothing old.
    let window = on_updates(&Arc::new(Fake {
        system: || Err(PackageKitError::Absent),
        stamp: || Ok(None),
        snaps: || {
            Err(SnapdError::Absent {
                socket: PathBuf::from("/run/snapd.socket"),
                source: io::ErrorKind::NotFound.into(),
            })
        },
        ..Fake::default()
    }));
    read(&window);
    assert!(shows(
        &window,
        "Okapi ne trouve pas de gestionnaire de mises à jour sur ce système."
    ));
}

#[test]
fn a_held_system_keeps_the_snaps_and_offers_to_retry() {
    let fake = Arc::new(Fake {
        system: || {
            Err(PackageKitError::Failed {
                code: error_code::CANNOT_GET_LOCK,
                details: "held".into(),
            })
        },
        snaps: two_snaps,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(shows(
        &window,
        "Une autre installation est en cours. Réessayez dans un moment."
    ));
    assert!(shows(
        &window,
        "2 mises à jour d'applis, le système n'a pas pu être vérifié, recherché à l'instant"
    ));
    assert_eq!(list_items(&window).len(), 2);
    button(&window, "Réessayer").invoke_accessible_default_action();
    read(&window);
    assert_eq!(fake.readings.load(Ordering::SeqCst), 2);
    // The same search again: from the cache, as the page's was.
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 0);
}

#[test]
fn a_system_not_read_never_lets_the_screen_say_up_to_date() {
    // Catches « Tout est à jour » said while the system could not be read,
    // nothing else waiting.
    let window = on_updates(&Arc::new(Fake {
        system: || Err(PackageKitError::Vanished),
        ..Fake::default()
    }));
    read(&window);
    assert!(!shows(&window, "Tout est à jour"));
    assert_eq!(updates(&window).get_system_state(), ui::SourceState::Failed);
}

#[test]
fn snaps_waiting_without_packagekit_say_the_system_is_not_checked() {
    let window = on_updates(&Arc::new(Fake {
        system: || Err(PackageKitError::Absent),
        snaps: two_snaps,
        ..Fake::default()
    }));
    read(&window);
    assert!(shows(
        &window,
        "2 mises à jour d'applis, les paquets du système ne sont pas vérifiés, recherché à l'instant"
    ));
}

#[test]
fn snapd_failing_hides_nothing_of_the_system() {
    let window = on_updates(&Arc::new(Fake {
        system: eight,
        snaps: || Err(SnapdError::Io(io::ErrorKind::TimedOut.into())),
        ..Fake::default()
    }));
    read(&window);
    assert_eq!(updates(&window).get_system_count(), 7);
    assert_eq!(updates(&window).get_snaps_state(), ui::SourceState::Failed);
    // The count says it is the system's alone.
    assert!(shows(
        &window,
        "7 mises à jour du système, les applis n'ont pas pu être vérifiées, recherché à l'instant"
    ));
    assert!(shows(
        &window,
        "Le gestionnaire de mises à jour ne répond pas."
    ));
}

#[test]
fn each_failure_reaches_the_window_as_itself() {
    // Catches two failures swapped on their way to the window.
    for (failure, said) in [
        (Failure::Busy, ui::SourceFailure::Busy),
        (Failure::Offline, ui::SourceFailure::Offline),
        (Failure::Refused, ui::SourceFailure::Refused),
        (Failure::Unanswered, ui::SourceFailure::Unanswered),
        (Failure::Broken, ui::SourceFailure::Broken),
    ] {
        assert_eq!(failure_shown(failure), said, "{failure:?}");
    }
    // And in the window, by its words.
    let window = on_updates(&Arc::new(Fake {
        system: || {
            Err(PackageKitError::Failed {
                code: error_code::NOT_AUTHORIZED,
                details: String::new(),
            })
        },
        ..Fake::default()
    }));
    read(&window);
    assert!(shows(
        &window,
        "Le système a refusé la recherche : elle demande une session ouverte sur cet écran."
    ));
}

/// Lists dated by `stamp`, nothing waiting: never « à jour » until
/// downloaded again.
fn old_lists_with_nothing_waiting(stamp: fn() -> io::Result<Option<SystemTime>>, badge: &str) {
    let fake = Arc::new(Fake {
        stamp,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(updates(&window).get_lists_old());
    assert!(!shows(&window, "Tout est à jour"));
    assert!(shows(&window, badge), "{badge}");
    assert!(shows(
        &window,
        "Aucune mise à jour dans cette liste, qui n'est peut-être plus à jour : « Rechercher maintenant » la télécharge à nouveau."
    ));
    // Downloaded again: fresh, whatever the stamp says.
    button(&window, "Rechercher maintenant").invoke_accessible_default_action();
    read(&window);
    assert!(!updates(&window).get_lists_old());
    assert!(shows(&window, "Tout est à jour"));
}

#[test]
fn lists_two_weeks_old_with_nothing_waiting_are_never_up_to_date() {
    // Catches the error the spec forbids: an old cache shown as fresh.
    old_lists_with_nothing_waiting(
        || {
            Ok(Some(
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_527_207),
            ))
        },
        "Liste du 1er novembre",
    );
}

#[test]
fn undated_lists_with_nothing_waiting_are_never_up_to_date() {
    old_lists_with_nothing_waiting(|| Ok(None), "Liste d'âge inconnu");
}

#[test]
fn lists_are_old_from_a_week_on() {
    let now = Timestamp::from_second(NOW).unwrap();
    let before = |seconds: i64| Timestamp::from_second(NOW - seconds).unwrap();
    let week = i64::try_from(LISTS_OLD_AFTER.as_secs()).unwrap();
    assert!(!older_than(before(week - 1), now, LISTS_OLD_AFTER));
    assert!(older_than(before(week), now, LISTS_OLD_AFTER));
    assert!(
        !older_than(before(-5), now, LISTS_OLD_AFTER),
        "a clock set back"
    );
}

#[test]
fn lists_not_downloaded_again_are_dated_and_kept() {
    let fake = Arc::new(Fake {
        refresh: || {
            Err(PackageKitError::Failed {
                code: error_code::NO_NETWORK,
                details: "offline".into(),
            })
        },
        system: eight,
        // 11 November, 9:12 UTC.
        stamp: || {
            Ok(Some(
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_388_327),
            ))
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(!updates(&window).get_refresh_failed(), "not asked yet");
    assert!(!shows(&window, "Liste du 11 novembre"), "three days: fresh");
    button(&window, "Rechercher maintenant").invoke_accessible_default_action();
    read(&window);
    assert!(shows(
        &window,
        "La recherche n'a pas pu joindre Internet. La liste date du dernier téléchargement réussi."
    ));
    assert!(shows(&window, "Liste du 11 novembre"));
    assert_eq!(updates(&window).get_system_count(), 7, "the old list kept");
}

#[test]
fn lists_a_week_old_are_dated_without_a_search() {
    let fake = Arc::new(Fake {
        system: eight,
        // 1 November.
        stamp: || {
            Ok(Some(
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_527_207),
            ))
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(updates(&window).get_lists_old());
    assert!(shows(&window, "Liste du 1er novembre"));
}

#[test]
fn a_restart_awaited_is_said_with_its_packages() {
    let fake = Arc::new(Fake {
        reboot: || {
            Ok(Some(RebootRequired {
                packages: Some(vec!["linux-image-7.0.0-43-generic".into(), "libc6".into()]),
            }))
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(shows(
        &window,
        "Un redémarrage termine des mises à jour déjà installées. Redémarrez quand cela vous arrange."
    ));
    let fold = button(&window, "2 paquets");
    assert!(!shows(&window, "libc6"), "folded");
    fold.invoke_accessible_default_action();
    assert!(shows(&window, "libc6"));
    assert!(shows(&window, "Okapi ne redémarre rien lui-même."));
}

#[test]
fn a_reading_thread_that_dies_ends_the_search_with_a_failure() {
    let fake = Arc::new(Fake {
        system: || panic!("a reading that dies"),
        snaps: two_snaps,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert_eq!(updates(&window).get_system_state(), ui::SourceState::Failed);
    assert_eq!(
        updates(&window).get_system_failure(),
        ui::SourceFailure::Broken
    );
    assert_eq!(updates(&window).get_snap_lines().row_count(), 2);
    assert_eq!(
        button(&window, "Rechercher maintenant").accessible_enabled(),
        Some(true)
    );
}

#[test]
fn a_snaps_thread_that_dies_ends_the_search_too() {
    let fake = Arc::new(Fake {
        system: eight,
        snaps: || panic!("a reading that dies"),
        reboot: || {
            Ok(Some(RebootRequired {
                packages: Some(Vec::new()),
            }))
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert_eq!(updates(&window).get_snaps_state(), ui::SourceState::Failed);
    assert_eq!(updates(&window).get_system_count(), 7);
    assert!(!updates(&window).get_reboot(), "never read");
}

#[test]
fn a_system_thread_that_dies_says_so_without_waiting_for_the_snaps() {
    // Catches a failure said only once the other thread, which holds the
    // channel open, ends.
    let (go, hold) = mpsc::channel();
    let fake = Arc::new(Fake {
        system: || panic!("a reading that dies"),
        snaps: two_snaps,
        hold_snaps: Mutex::new(Some(hold)),
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read_until(&window, |window| {
        updates(window).get_system_state() == ui::SourceState::Failed
    });
    assert_eq!(updates(&window).get_step(), ui::UpdatesStep::Searching);
    assert_eq!(
        updates(&window).get_system_failure(),
        ui::SourceFailure::Broken
    );
    go.send(()).unwrap();
    read(&window);
    assert_eq!(updates(&window).get_snap_lines().row_count(), 2);
}

#[test]
fn a_snaps_thread_that_dies_says_so_without_waiting_for_the_system() {
    let (go, hold) = mpsc::channel();
    let fake = Arc::new(Fake {
        system: eight,
        snaps: || panic!("a reading that dies"),
        reboot: || {
            Ok(Some(RebootRequired {
                packages: Some(Vec::new()),
            }))
        },
        hold: Mutex::new(Some(hold)),
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read_until(&window, |window| {
        updates(window).get_snaps_state() == ui::SourceState::Failed
    });
    assert_eq!(updates(&window).get_step(), ui::UpdatesStep::Searching);
    assert!(!updates(&window).get_reboot(), "owed, never read");
    go.send(()).unwrap();
    read(&window);
    assert_eq!(updates(&window).get_system_count(), 7);
}

#[test]
fn each_button_that_installs_names_what_it_updates() {
    let fake = Arc::new(Fake {
        system: eight,
        snaps: two_snaps,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    button(
        &window,
        "Système Ubuntu 26.04, 7 mises à jour, dont 3 de sécurité, replié",
    )
    .invoke_accessible_default_action();
    // Every button of the page, whatever its words.
    let mut labels: Vec<String> = ElementHandle::find_by_element_type_name(&window, "UpdatesPage")
        .next()
        .unwrap()
        .query_descendants()
        .match_accessible_role(AccessibleRole::Button)
        .find_all()
        .into_iter()
        .filter_map(|element| element.accessible_label().map(String::from))
        .collect();
    labels.sort();
    assert_eq!(
        labels,
        [
            "Afficher les 2 autres",
            "Mettre à jour App Center",
            "Mettre à jour OBS Studio",
            "Mettre à jour Système Ubuntu 26.04",
            "Mettre à jour toutes les applis",
            "Rechercher maintenant",
            "Système Ubuntu 26.04, 7 mises à jour, dont 3 de sécurité, déplié",
        ]
    );
    assert!(
        fake.updates_asked.lock().unwrap().is_empty(),
        "nothing asked"
    );
}

/// Waits for the outcome of an installation, the list read again.
fn installed(window: &ui::AppWindow) {
    read_until(window, |window| updates(window).get_result_shown());
}

fn names_asked(fake: &Fake) -> Vec<Vec<String>> {
    fake.updates_asked
        .lock()
        .unwrap()
        .iter()
        .map(|ids| {
            ids.iter()
                .map(|id| id.split(';').next().unwrap().to_owned())
                .collect()
        })
        .collect()
}

#[test]
fn updating_the_system_asks_the_updates_read_never_those_held_back() {
    // Catches a held back update sent, an update of the reading left out,
    // and an outcome told before the list is read again.
    let fake = Arc::new(Fake {
        system: eight,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    installed(&window);
    assert_eq!(
        names_asked(&fake),
        [[
            "libssl3t64",
            "libxml2",
            "openssl",
            "bash",
            "libreoffice-core",
            "mesa",
            "zsh"
        ]]
    );
    assert_eq!(fake.readings.load(Ordering::SeqCst), 2, "read again");
    assert_eq!(
        fake.refreshes.load(Ordering::SeqCst),
        0,
        "read again, never downloaded"
    );
    assert_eq!(
        *fake.simulated_ids.lock().unwrap(),
        *fake.updates_asked.lock().unwrap(),
        "simulated what is installed"
    );
    let shown = updates(&window);
    assert_eq!(shown.get_result_end(), ui::InstallEnd::Done);
    assert!(shown.get_result_counted());
    assert_eq!(shown.get_result_installed(), 7);
    assert_eq!(shown.get_result_left(), 0);
    assert_eq!(shown.get_install_step(), ui::InstallStep::Idle);
    assert_eq!(shown.get_system_count(), 0, "the list read again");
}

#[test]
fn what_is_still_waiting_after_the_update_is_named() {
    let fake = Arc::new(Fake {
        system: eight,
        system_after: || {
            Ok(vec![
                package(5, "mesa", "25.2.2-1"),
                package(info::SECURITY, "openssl", "3.5.1-1ubuntu2"),
            ])
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    installed(&window);
    let shown = updates(&window);
    // All installed (PackageKit done); two wait anew, in newer versions.
    assert_eq!(shown.get_result_installed(), 7);
    assert_eq!(shown.get_result_left(), 2);
    assert_eq!(shown.get_result_left_names(), "openssl, mesa");
}

fn removing(removals: Vec<Vec<&'static str>>) -> Arc<Fake> {
    Arc::new(Fake {
        system: eight,
        removals: Mutex::new(removals),
        ..Fake::default()
    })
}

fn removed_names(window: &ui::AppWindow) -> Vec<String> {
    updates(window)
        .get_removed()
        .iter()
        .map(|line| line.name.to_string())
        .collect()
}

#[test]
fn what_the_update_removes_is_asked_first_and_refused_installs_nothing() {
    let fake = removing(vec![vec!["libold1"]]);
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    read_until(&window, |window| {
        updates(window).get_install_step() == ui::InstallStep::Confirm
    });
    assert_eq!(removed_names(&window), ["libold1"]);
    assert!(!updates(&window).get_removals_changed());
    updates(&window).invoke_cancel_update();
    assert_eq!(updates(&window).get_install_step(), ui::InstallStep::Idle);
    assert_eq!(updates(&window).get_install_step(), ui::InstallStep::Idle);
    assert!(
        fake.updates_asked.lock().unwrap().is_empty(),
        "nothing installed"
    );
}

#[test]
fn removals_accepted_install_but_a_change_since_asks_again() {
    // Catches a removal accepted then installed as it changed.
    let fake = removing(vec![
        vec!["libold1"],
        vec!["libold1", "libold2"],
        vec!["libold2", "libold1"],
    ]);
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    read_until(&window, |window| {
        updates(window).get_install_step() == ui::InstallStep::Confirm
    });
    updates(&window).invoke_confirm_update();
    read_until(&window, |window| {
        updates(window).get_install_step() == ui::InstallStep::Confirm
            && updates(window).get_removed().row_count() == 2
    });
    assert!(updates(&window).get_removals_changed());
    assert!(fake.updates_asked.lock().unwrap().is_empty(), "not yet");
    updates(&window).invoke_confirm_update();
    // The confirmation closes at once: what it accepted runs, followed.
    assert_eq!(
        updates(&window).get_install_step(),
        ui::InstallStep::Running
    );
    installed(&window);
    assert_eq!(fake.updates_asked.lock().unwrap().len(), 1);
    assert_eq!(fake.simulations.load(Ordering::SeqCst), 3);
}

#[test]
fn the_progress_is_shown_and_a_cancel_reaches_the_update() {
    let (go, hold) = mpsc::channel();
    let fake = Arc::new(Fake {
        system: eight,
        said: vec![
            Updating::Status(8),
            Updating::Cancellable(true),
            Updating::Percentage(38),
            Updating::Package {
                info: 10,
                id: "libgtk-4-1;4.20;amd64;x".into(),
            },
        ],
        hold_update: Mutex::new(Some(hold)),
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    read_until(&window, |window| {
        updates(window).get_install_package() == "libgtk-4-1"
    });
    assert!(updates(&window).get_install_has_percentage());
    assert_eq!(updates(&window).get_install_percentage(), 38);
    let shown = updates(&window);
    assert_eq!(shown.get_install_step(), ui::InstallStep::Running);
    assert_eq!(shown.get_install_phase(), ui::InstallPhase::Downloading);
    assert_eq!(shown.get_install_package(), "libgtk-4-1");
    assert!(shown.get_install_cancellable());
    assert_eq!(shown.get_install_step(), ui::InstallStep::Running);
    shown.invoke_cancel_update();
    assert!(shown.get_install_cancel_asked());
    go.send(()).unwrap();
    installed(&window);
    assert_eq!(updates(&window).get_result_end(), ui::InstallEnd::Cancelled);
    assert!(!updates(&window).get_result_counted());
}

#[test]
fn nothing_else_starts_while_an_installation_runs() {
    // Catches a search read in the middle of dpkg (its list then taken for
    // the outcome), and a second installation at once.
    let (go, hold) = mpsc::channel();
    let fake = Arc::new(Fake {
        system: eight,
        snaps: two_snaps,
        said: vec![Updating::Status(8)],
        hold_update: Mutex::new(Some(hold)),
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    read_until(&window, |window| {
        updates(window).get_install_phase() == ui::InstallPhase::Downloading
    });
    updates(&window).invoke_search_now();
    updates(&window).invoke_retry();
    later(10 * 60);
    button(&window, "Accueil").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    button(&window, "Mises à jour").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    updates(&window).invoke_update_snaps();
    updates(&window).invoke_update_system();
    assert_eq!(fake.readings.load(Ordering::SeqCst), 1, "no search");
    assert!(fake.refreshes_asked.lock().unwrap().is_empty());
    go.send(()).unwrap();
    installed(&window);
    assert_eq!(fake.updates_asked.lock().unwrap().len(), 1);
    assert_eq!(fake.readings.load(Ordering::SeqCst), 2, "read again once");
}

#[test]
fn a_restart_asked_by_the_update_is_said() {
    let fake = Arc::new(Fake {
        system: eight,
        updated: || {
            Ok(Updated {
                system_restart: true,
            })
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    installed(&window);
    assert!(updates(&window).get_result_restart());
}

#[test]
fn a_refusal_installs_nothing_and_retrying_asks_again() {
    let fake = Arc::new(Fake {
        system: eight,
        updated: || {
            Err(PackageKitError::Failed {
                code: error_code::NOT_AUTHORIZED,
                details: "refused".into(),
            })
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    installed(&window);
    let shown = updates(&window);
    assert_eq!(shown.get_result_end(), ui::InstallEnd::Failed);
    assert_eq!(shown.get_result_why(), ui::InstallWhy::Refused);
    assert!(!shown.get_result_counted());
    shown.invoke_retry_update();
    read_until(&window, |window| {
        fake.updates_asked.lock().unwrap().len() == 2 && updates(window).get_result_shown()
    });
}

#[test]
fn a_snap_s_button_refreshes_that_snap_only() {
    let fake = Arc::new(Fake {
        snaps: two_snaps,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    // Lines by title: App Center, then OBS Studio.
    updates(&window).invoke_update_snap(1);
    installed(&window);
    assert_eq!(*fake.refreshes_asked.lock().unwrap(), [["obs-studio"]]);
    assert_eq!(updates(&window).get_result_installed(), 1);
    assert_eq!(
        updates(&window).get_result_target(),
        ui::InstallTarget::Snaps
    );
}

#[test]
fn all_the_snaps_are_refreshed_together() {
    let fake = Arc::new(Fake {
        snaps: two_snaps,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_snaps();
    installed(&window);
    assert_eq!(
        *fake.refreshes_asked.lock().unwrap(),
        [["snap-store", "obs-studio"]]
    );
}

#[test]
fn a_line_never_shown_asks_nothing() {
    // Catches an index taken on trust from the window.
    let fake = Arc::new(Fake {
        snaps: two_snaps,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    for index in [2, -1] {
        updates(&window).invoke_update_snap(index);
    }
    assert_eq!(updates(&window).get_install_step(), ui::InstallStep::Idle);
    assert!(fake.refreshes_asked.lock().unwrap().is_empty());
}

#[test]
fn snaps_still_waiting_after_a_failed_refresh_are_in_failure() {
    let fake = Arc::new(Fake {
        snaps: two_snaps,
        snaps_after: two_snaps,
        followed: || {
            Followed::Ready(Change {
                status: "Error".into(),
                ready: true,
                err: Some("cannot refresh".into()),
                tasks: Vec::new(),
            })
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_snaps();
    installed(&window);
    let shown = updates(&window);
    assert_eq!(shown.get_result_end(), ui::InstallEnd::Failed);
    assert!(shown.get_result_counted());
    assert_eq!(shown.get_result_installed(), 0);
    assert_eq!(shown.get_result_left_names(), "App Center, OBS Studio");
}

#[test]
fn the_download_is_said_beside_the_count() {
    let fake = Arc::new(Fake {
        system: eight,
        size: |_| Some(1_000_000),
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(updates(&window).get_has_system_download());
    assert_eq!(updates(&window).get_system_download(), size(7_000_000));
    assert!(
        ElementHandle::find_by_element_type_name(&window, "FoldHead")
            .filter_map(|head| head.accessible_label())
            .any(|label| label.contains("à télécharger")),
    );
}

#[test]
fn the_version_said_is_the_authors_one() {
    for (version, said) in [
        ("3.5.1-1ubuntu2", "3.5.1"),
        ("4:25.8.4-0ubuntu1", "25.8.4"),
        ("2.14.3+dfsg-1ubuntu0.1", "2.14.3"),
        ("20260410-0ubuntu3", "20260410"),
        ("1.2~rc1-1", "1.2"),
        ("7", "7"),
        // An upstream version with its own dash, a backport's marks.
        ("1.0-beta-2", "1.0-beta"),
        ("1.2.3-1~bpo12+1", "1.2.3"),
        ("1:-weird", "1:-weird"),
    ] {
        assert_eq!(upstream_version(version), said, "{version}");
    }
}

#[test]
fn snaps_and_no_packagekit_say_the_system_is_unchecked_in_the_list_too() {
    // Catches the system left out in silence when the snaps are listed.
    let window = on_updates(&Arc::new(Fake {
        system: || Err(PackageKitError::Absent),
        snaps: || Err(SnapdError::Io(io::ErrorKind::TimedOut.into())),
        stamp: || Ok(None),
        ..Fake::default()
    }));
    read(&window);
    assert!(shows(
        &window,
        "Paquets du système : non vérifiés, PackageKit n'est pas installé"
    ));
}

#[test]
fn a_total_of_the_snaps_is_said_only_when_every_size_is_known() {
    let window = on_updates(&Arc::new(Fake {
        snaps: || {
            let mut snaps = two_snaps()?;
            snaps[1].download_size = None;
            Ok(snaps)
        },
        ..Fake::default()
    }));
    read(&window);
    assert!(!updates(&window).get_has_snaps_size());
    assert!(shows(&window, "2 mises à jour"), "a count, no size");
    assert!(shows(
        &window,
        "OBS Studio, de Snapcrafters, version 32.0.2"
    ));
}

#[test]
fn no_snap_waiting_has_no_total() {
    let window = on_updates(&Arc::new(Fake::default()));
    read(&window);
    assert!(!updates(&window).get_has_snaps_size());
}

#[test]
fn updates_held_back_never_wait() {
    // Catches a held-back update keeping « Tout est à jour » away: it will
    // not be installed (the system's own updater says up to date too).
    let window = on_updates(&Arc::new(Fake {
        system: || Ok(vec![package(info::BLOCKED, "mesa", "25.2.1-1")]),
        ..Fake::default()
    }));
    read(&window);
    assert_eq!(updates(&window).get_system_count(), 0);
    assert_eq!(updates(&window).get_system_held(), 1);
    assert!(shows(&window, "Tout est à jour"));
}

#[test]
fn updates_held_back_are_listed_beside_those_waiting() {
    let window = on_updates(&Arc::new(Fake {
        system: eight,
        ..Fake::default()
    }));
    read(&window);
    assert_eq!(updates(&window).get_system_held(), 1);
    button(
        &window,
        "Système Ubuntu 26.04, 7 mises à jour, dont 3 de sécurité, replié",
    )
    .invoke_accessible_default_action();
    button(&window, "Afficher les 2 autres").invoke_accessible_default_action();
    assert_eq!(
        list_items(&window).last().map(String::as_str),
        Some("aardvark, retenue par le système, version 1.0")
    );
}

#[test]
fn seven_packages_show_six_then_the_other_one_and_a_new_search_folds_back() {
    let window = on_updates(&Arc::new(Fake {
        system: || {
            let mut seven = eight()?;
            seven.pop();
            Ok(seven)
        },
        ..Fake::default()
    }));
    read(&window);
    button(
        &window,
        "Système Ubuntu 26.04, 6 mises à jour, dont 3 de sécurité, replié",
    )
    .invoke_accessible_default_action();
    button(&window, "Afficher l'autre").invoke_accessible_default_action();
    assert_eq!(list_items(&window).len(), 7);
    button(&window, "Rechercher maintenant").invoke_accessible_default_action();
    read(&window);
    assert_eq!(list_items(&window).len(), 6, "folded back to the first");
}

#[test]
fn a_download_that_works_again_lifts_its_banner() {
    let fake = Arc::new(Fake {
        system: eight,
        ..Fake::default()
    });
    fake.offline.store(true, Ordering::SeqCst);
    let window = on_updates(&fake);
    read(&window);
    button(&window, "Rechercher maintenant").invoke_accessible_default_action();
    read(&window);
    assert!(updates(&window).get_refresh_failed());
    // Downloaded this morning: said as today, never as a date of nothing.
    assert!(shows(&window, "Liste d'aujourd'hui"));
    fake.offline.store(false, Ordering::SeqCst);
    button(&window, "Rechercher maintenant").invoke_accessible_default_action();
    read(&window);
    assert!(!updates(&window).get_refresh_failed());
}

#[test]
fn the_minutes_go_on_while_the_page_stays() {
    let window = on_updates(&Arc::new(Fake::default()));
    read(&window);
    assert_eq!(updates(&window).get_minutes(), 0);
    later(3 * 60);
    i_slint_backend_testing::mock_elapsed_time(MINUTES_EVERY);
    slint::platform::update_timers_and_animations();
    assert_eq!(updates(&window).get_minutes(), 3);
}

#[test]
fn a_pass_of_snapd_gone_by_while_the_page_stays_is_no_longer_said() {
    let window = on_updates(&Arc::new(Fake {
        snaps: two_snaps,
        schedule: || {
            Ok(Schedule {
                // Ten minutes from the tests' now.
                next: Some("2026-11-14T22:10:00Z".into()),
                hold: None,
            })
        },
        ..Fake::default()
    }));
    read(&window);
    assert!(updates(&window).get_has_next_refresh());
    later(11 * 60);
    i_slint_backend_testing::mock_elapsed_time(MINUTES_EVERY);
    slint::platform::update_timers_and_animations();
    assert!(!updates(&window).get_has_next_refresh());
}

#[test]
fn a_download_of_okapi_s_own_is_remembered_without_a_stamp() {
    // Catches lists downloaded by Okapi called of an unknown age at the next
    // reading, apt having left no stamp.
    let fake = Arc::new(Fake {
        stamp: || Ok(None),
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(updates(&window).get_lists_old());
    button(&window, "Rechercher maintenant").invoke_accessible_default_action();
    read(&window);
    assert!(!updates(&window).get_lists_old());
    // Shown again later: read from the cache, the download remembered.
    later(6 * 60);
    button(&window, "Accueil").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    button(&window, "Mises à jour").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    read(&window);
    assert_eq!(fake.readings.load(Ordering::SeqCst), 3);
    assert!(!updates(&window).get_lists_old());
    assert!(shows(&window, "Tout est à jour"));
}

#[test]
fn no_installation_starts_while_a_search_runs() {
    // Catches an installation started during a search, its outcome then
    // read from a list of before.
    let (go, hold) = mpsc::channel();
    let fake = Arc::new(Fake {
        system: eight,
        snaps: two_snaps,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    *fake.hold.lock().unwrap() = Some(hold);
    updates(&window).invoke_search_now();
    read_until(&window, |window| updates(window).get_waiting());
    updates(&window).invoke_update_system();
    updates(&window).invoke_update_snaps();
    updates(&window).invoke_update_snap(0);
    assert_eq!(updates(&window).get_install_step(), ui::InstallStep::Idle);
    go.send(()).unwrap();
    read(&window);
    assert!(fake.updates_asked.lock().unwrap().is_empty());
    assert!(fake.refreshes_asked.lock().unwrap().is_empty());
}

#[test]
fn a_failure_once_dpkg_ran_says_it_stopped_halfway_without_counts() {
    // Catches « Aucune mise à jour installée » said of a dpkg that already
    // unpacked packages.
    let fake = Arc::new(Fake {
        system: eight,
        said: vec![Updating::Package {
            info: 21,
            id: "bash;5;amd64;x".into(),
        }],
        updated: || {
            Err(PackageKitError::Failed {
                code: 58,
                details: "dpkg failed".into(),
            })
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    installed(&window);
    assert_eq!(updates(&window).get_result_end(), ui::InstallEnd::Stopped);
    assert!(!updates(&window).get_result_counted());
}

#[test]
fn too_late_to_cancel_is_said_once_it_could_and_no_longer_can() {
    let (go, hold) = mpsc::channel();
    let fake = Arc::new(Fake {
        system: eight,
        said: vec![
            Updating::Cancellable(true),
            Updating::Cancellable(false),
            Updating::Status(16),
        ],
        hold_update: Mutex::new(Some(hold)),
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    read_until(&window, |window| updates(window).get_install_too_late());
    assert!(!updates(&window).get_install_cancellable());
    go.send(()).unwrap();
    installed(&window);
}

#[test]
fn an_installation_thread_that_dies_ends_in_failure() {
    // Catches a screen stuck on « installation » for ever.
    let fake = Arc::new(Fake {
        system: eight,
        dies: true,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    installed(&window);
    assert_eq!(updates(&window).get_result_end(), ui::InstallEnd::Failed);
    assert_eq!(updates(&window).get_install_step(), ui::InstallStep::Idle);
}

#[test]
fn a_removal_never_shown_is_said_in_the_outcome() {
    let fake = Arc::new(Fake {
        system: eight,
        said: vec![Updating::Package {
            info: info::REMOVING,
            id: "ubuntu-desktop;1;amd64;installed".into(),
        }],
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    installed(&window);
    assert_eq!(updates(&window).get_result_unshown(), "ubuntu-desktop");
}

#[test]
fn no_download_is_said_when_a_size_is_unknown() {
    let fake = Arc::new(Fake {
        system: eight,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    assert!(!updates(&window).get_has_system_download());
}

#[test]
fn retrying_snaps_asks_only_those_still_waiting() {
    // Catches a retry that asks again a snap already refreshed.
    let fake = Arc::new(Fake {
        snaps: two_snaps,
        snaps_after: || {
            two_snaps().map(|snaps| {
                snaps
                    .into_iter()
                    .filter(|snap| snap.name == "obs-studio")
                    .collect()
            })
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_snaps();
    installed(&window);
    assert_eq!(updates(&window).get_result_left_names(), "OBS Studio");
    updates(&window).invoke_retry_update();
    read_until(&window, |_| fake.refreshes_asked.lock().unwrap().len() == 2);
    assert_eq!(
        fake.refreshes_asked.lock().unwrap()[1],
        ["obs-studio".to_owned()]
    );
}

fn asked_system(names: &[&str]) -> Vec<SystemUpdate> {
    names
        .iter()
        .map(|name| SystemUpdate {
            id: format!("{name};2;amd64;x"),
            name: (*name).to_owned(),
            version: "2".into(),
            summary: String::new(),
            kind: Kind::Other,
        })
        .collect()
}

fn ending(end: End, reached_dpkg: bool) -> Ended {
    Ended {
        reached_dpkg,
        ..Ended::before(end)
    }
}

#[test]
fn each_case_of_the_outcome_is_told_by_its_end_and_the_list_read_again() {
    let asked = Target::System(asked_system(&["bash", "curl", "zsh"]));
    let read = asked_system(&["zsh"]);
    let done = outcome(&asked, &ending(End::Done, true), Some(&read), None);
    assert_eq!(done.end, ui::InstallEnd::Done);
    let counts = done.counts.unwrap();
    // PackageKit done: all asked installed; one listed again waits anew.
    assert_eq!((counts.installed, counts.left), (3, vec!["zsh".to_owned()]));
    // Done, the list not read again: PackageKit installed all it was asked.
    let unread = outcome(&asked, &ending(End::Done, true), None, None);
    assert_eq!(unread.counts.unwrap().installed, 3);
    for (end, reached, shown) in [
        (End::Cancelled, false, ui::InstallEnd::Cancelled),
        (End::Cancelled, true, ui::InstallEnd::Stopped),
        (End::Failed(Why::Refused), false, ui::InstallEnd::Failed),
        (End::Failed(Why::Broken), true, ui::InstallEnd::Stopped),
    ] {
        let told = outcome(&asked, &ending(end, reached), Some(&read), None);
        assert_eq!(told.end, shown, "{end:?} {reached}");
        assert_eq!(told.counts, None, "{end:?} {reached}: no count");
    }
    // Snaps: the list read again decides.
    let snaps = |names: &[&str]| -> Vec<okapi_tools::updates::SnapUpdate> {
        names
            .iter()
            .map(|name| okapi_tools::updates::SnapUpdate {
                name: (*name).to_owned(),
                title: name.to_uppercase(),
                publisher: None,
                verified: false,
                version: "1".into(),
                download_size: None,
            })
            .collect()
    };
    let asked = Target::Snaps(snaps(&["a", "b"]));
    let all_gone = outcome(
        &asked,
        &ending(End::Failed(Why::Unanswered), false),
        None,
        Some(&[]),
    );
    assert_eq!(all_gone.end, ui::InstallEnd::Done, "gone despite no answer");
    assert_eq!(all_gone.why, None, "no failure said of what is done");
    let one_left = outcome(
        &asked,
        &ending(End::Done, false),
        None,
        Some(&snaps(&["b"])),
    );
    // Done by snapd, one still waiting: done, the rest said waiting.
    assert_eq!((one_left.end, one_left.why), (ui::InstallEnd::Done, None));
    assert_eq!(one_left.counts.unwrap().left, ["B"]);
    let one_failed = outcome(
        &asked,
        &ending(End::Failed(Why::Broken), false),
        None,
        Some(&snaps(&["b"])),
    );
    assert_eq!(one_failed.end, ui::InstallEnd::Failed);
    assert_eq!(one_failed.counts.unwrap().installed, 1);
    let unread = outcome(&asked, &ending(End::Failed(Why::Busy), false), None, None);
    assert_eq!((unread.end, unread.counts), (ui::InstallEnd::Failed, None));
}

#[test]
fn a_cancel_refused_or_late_says_it_came_too_late() {
    // Catches « Trop tard pour annuler » hidden once Annuler is pressed,
    // when PackageKit lets go of cancelling without taking it.
    let (go, hold) = mpsc::channel();
    let fake = Arc::new(Fake {
        system: eight,
        said: vec![Updating::Cancellable(true)],
        hold_update: Mutex::new(Some(hold)),
        said_after_hold: vec![Updating::Cancellable(false), Updating::Status(16)],
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    read_until(&window, |window| updates(window).get_install_cancellable());
    updates(&window).invoke_cancel_update();
    go.send(()).unwrap();
    read_until(&window, |window| updates(window).get_install_too_late());
}

#[test]
fn a_thread_that_dies_once_dpkg_ran_says_it_may_be_finishing() {
    // Catches « Aucune mise à jour installée » said of a dpkg at work.
    let fake = Arc::new(Fake {
        system: eight,
        said: vec![Updating::Package {
            info: 21,
            id: "bash;5;amd64;x".into(),
        }],
        dies: true,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    installed(&window);
    assert_eq!(updates(&window).get_result_end(), ui::InstallEnd::Stopped);
    assert_eq!(
        updates(&window).get_result_why(),
        ui::InstallWhy::Unanswered
    );
}

#[test]
fn trying_again_is_offered_only_when_something_still_waits() {
    // Catches « Réessayer » shown with nothing left to try.
    let fake = Arc::new(Fake {
        system: eight,
        updated: || {
            Err(PackageKitError::Failed {
                code: error_code::UPDATE_NOT_FOUND,
                details: "changed".into(),
            })
        },
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    installed(&window);
    assert_eq!(updates(&window).get_result_why(), ui::InstallWhy::Changed);
    assert!(
        updates(&window).get_result_retriable(),
        "the list still waits"
    );
}

#[test]
fn trying_again_is_not_offered_when_nothing_waits() {
    let fake = Arc::new(Fake {
        snaps: two_snaps,
        followed: || Followed::Unanswered,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_snaps();
    installed(&window);
    assert!(!updates(&window).get_result_retriable(), "none still waits");
    assert_eq!(updates(&window).get_result_end(), ui::InstallEnd::Done);
}

#[test]
fn trying_again_needs_an_update_still_waiting_and_a_cause_it_can_mend() {
    // Catches « Réessayer » offered on a list with nothing left, all held
    // back, or a failure only the user can mend.
    let asked = Target::System(asked_system(&["bash"]));
    let failed = |why| Outcome {
        end: ui::InstallEnd::Failed,
        why: Some(why),
        counts: None,
    };
    let held = vec![SystemUpdate {
        kind: Kind::Blocked,
        ..asked_system(&["aardvark"]).remove(0)
    }];
    assert!(retriable(
        &asked,
        &failed(Why::Busy),
        &asked_system(&["bash"]),
        None
    ));
    assert!(
        !retriable(&asked, &failed(Why::Busy), &[], None),
        "nothing left"
    );
    assert!(
        !retriable(&asked, &failed(Why::Busy), &held, None),
        "only held back"
    );
    for why in [Why::Interrupted, Why::Unsigned] {
        assert!(
            !retriable(&asked, &failed(why), &asked_system(&["bash"]), None),
            "{why:?}"
        );
    }
    let stopped = Outcome {
        end: ui::InstallEnd::Stopped,
        why: Some(Why::Broken),
        counts: None,
    };
    assert!(!retriable(&asked, &stopped, &asked_system(&["bash"]), None));
}

#[test]
fn the_buttons_clicked_ask_what_they_name() {
    // Catches a line's button asking another snap, and the system's button
    // wired to the snaps.
    let fake = Arc::new(Fake {
        system: eight,
        snaps: two_snaps,
        system_after: eight,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    button(&window, "Mettre à jour OBS Studio").invoke_accessible_default_action();
    installed(&window);
    assert_eq!(*fake.refreshes_asked.lock().unwrap(), [["obs-studio"]]);
    assert!(fake.updates_asked.lock().unwrap().is_empty());
    button(&window, "Mettre à jour Système Ubuntu 26.04").invoke_accessible_default_action();
    read_until(&window, |window| {
        updates(window).get_result_target() == ui::InstallTarget::System
            && updates(window).get_result_shown()
    });
    assert_eq!(fake.updates_asked.lock().unwrap().len(), 1);
}

#[test]
fn the_confirmation_s_buttons_refuse_or_accept() {
    // Catches « Annuler » wired to installing what the update removes.
    let fake = removing(vec![vec!["libold1"], vec!["libold1"], vec!["libold1"]]);
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    read_until(&window, |window| {
        updates(window).get_install_step() == ui::InstallStep::Confirm
    });
    button(&window, "Annuler").invoke_accessible_default_action();
    assert_eq!(updates(&window).get_install_step(), ui::InstallStep::Idle);
    assert!(fake.updates_asked.lock().unwrap().is_empty());
    updates(&window).invoke_update_system();
    read_until(&window, |window| {
        updates(window).get_install_step() == ui::InstallStep::Confirm
    });
    button(&window, "Mettre à jour quand même").invoke_accessible_default_action();
    installed(&window);
    assert_eq!(fake.updates_asked.lock().unwrap().len(), 1);
}

#[test]
fn the_confirmation_counts_what_it_removes_and_brings_and_fits() {
    // Catches a list of removals unbounded, its count lost, or what the
    // update brings in left unsaid.
    let fake = removing(vec![vec![
        "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "+new1", "+new2",
    ]]);
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    read_until(&window, |window| {
        updates(window).get_install_step() == ui::InstallStep::Confirm
    });
    let shown = updates(&window);
    assert_eq!(shown.get_removed().row_count(), 8);
    assert_eq!(shown.get_removed_count(), 10);
    assert_eq!(shown.get_removed_more(), 2);
    assert_eq!(shown.get_added(), 2);
}

#[test]
fn no_search_while_the_confirmation_waits() {
    let fake = removing(vec![vec!["libold1"]]);
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    read_until(&window, |window| {
        updates(window).get_install_step() == ui::InstallStep::Confirm
    });
    updates(&window).invoke_search_now();
    updates(&window).invoke_retry();
    read_once();
    assert_eq!(fake.readings.load(Ordering::SeqCst), 1);
    assert_eq!(updates(&window).get_step(), ui::UpdatesStep::Read);
}

#[test]
fn a_cancel_taken_is_never_said_too_late() {
    let (go, hold) = mpsc::channel();
    let fake = Arc::new(Fake {
        system: eight,
        said: vec![Updating::Cancellable(true)],
        hold_update: Mutex::new(Some(hold)),
        said_after_hold: vec![Updating::Status(19), Updating::Cancellable(false)],
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    read_until(&window, |window| updates(window).get_install_cancellable());
    updates(&window).invoke_cancel_update();
    go.send(()).unwrap();
    installed(&window);
    assert!(!updates(&window).get_install_too_late());
    assert_eq!(updates(&window).get_result_end(), ui::InstallEnd::Cancelled);
}

#[test]
fn many_still_waiting_are_named_six_then_counted() {
    let fake = Arc::new(Fake {
        system: eight,
        system_after: eight,
        ..Fake::default()
    });
    let window = on_updates(&fake);
    read(&window);
    updates(&window).invoke_update_system();
    installed(&window);
    let shown = updates(&window);
    assert_eq!(shown.get_result_left(), 7);
    assert_eq!(shown.get_result_left_names().split(", ").count(), 6);
    assert_eq!(shown.get_result_left_more(), 1);
}

#[test]
fn each_cause_is_shown_as_itself() {
    // Catches two causes swapped on their way to the window.
    use std::collections::HashSet;
    let shown: HashSet<String> = [
        Why::Busy,
        Why::Interrupted,
        Why::Offline,
        Why::NoSpace,
        Why::Refused,
        Why::Changed,
        Why::AppsOpen,
        Why::Unsigned,
        Why::Unanswered,
        Why::Broken,
    ]
    .into_iter()
    .map(|why| format!("{:?}", why_shown(why)))
    .collect();
    assert_eq!(shown.len(), 10);
    assert!(!shown.contains("None"));
    assert_eq!(why_shown(Why::NoSpace), ui::InstallWhy::NoSpace);
    assert_eq!(why_shown(Why::Busy), ui::InstallWhy::Busy);
    assert_eq!(why_shown(Why::Offline), ui::InstallWhy::Offline);
    assert_eq!(why_shown(Why::Unsigned), ui::InstallWhy::Unsigned);
    assert_eq!(why_shown(Why::Interrupted), ui::InstallWhy::Interrupted);
    assert_eq!(why_shown(Why::Broken), ui::InstallWhy::Broken);
    assert_eq!(why_shown(Why::AppsOpen), ui::InstallWhy::AppsOpen);
}
