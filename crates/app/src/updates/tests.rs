//! The updates screen in a window, its sources stand-ins: what it reads when
//! it is shown, what it says of each source, and that it installs nothing.

use std::cell::Cell;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Instant, SystemTime};

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use okapi_tools::packages::packagekit::{Package, PackageKitError, error_code, info};
use okapi_tools::packages::snapd::{Publisher, Refresh, Schedule, SnapdError};
use slint::Model as _;

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
    readings: AtomicU32,
    refreshes: AtomicU32,
    /// While set, downloading the lists finds no network.
    offline: AtomicBool,
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
            readings: AtomicU32::new(0),
            refreshes: AtomicU32::new(0),
            offline: AtomicBool::new(false),
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

fn seams_of(fake: &Arc<Fake>) -> UpdatesSeams {
    UpdatesSeams {
        sources: Arc::clone(fake) as Arc<dyn Sources>,
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
    let (window, ..) =
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
fn the_page_offers_no_button_that_installs() {
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
            "Rechercher maintenant",
            "Système Ubuntu 26.04, 7 mises à jour, dont 3 de sécurité, déplié",
        ]
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
