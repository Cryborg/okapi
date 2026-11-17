use std::path::Path;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use okapi_rules::Language;
use okapi_store::{
    AdminRecord, HistoryEntry, HistoryOutcome, UninstallEntry, UpdateEntry, UpdatedPackage,
};
use slint::Model as _;

use super::*;
use crate::cleaner::tests::{
    button, seams_never_scanning, shows, shows_a_text_starting, watch_until,
};

/// Seams that read no history: no state directory.
pub fn nowhere() -> HistorySeams {
    HistorySeams {
        directory: None,
        now: noon,
        zone: TimeZone::UTC,
    }
}

/// 2026-11-13 at noon, UTC.
fn noon() -> Timestamp {
    "2026-11-13T12:00:00Z".parse().unwrap()
}

/// `hours` before [`noon`], in seconds.
fn before(hours: i64) -> i64 {
    noon().as_second() - hours * 3600
}

fn seams(directory: &Path) -> HistorySeams {
    HistorySeams {
        directory: Some(directory.to_owned()),
        ..nowhere()
    }
}

fn outcome(path: &str, result: &str, bytes: Option<u64>, reason: Option<&str>) -> HistoryOutcome {
    HistoryOutcome {
        path: Some(path.to_owned()),
        result: result.to_owned(),
        bytes,
        reason: reason.map(str::to_owned),
        error: None,
        code: None,
        blocked: None,
        inexact: false,
    }
}

fn cleaning(time: i64, removed_bytes: u64, entries: Option<Vec<HistoryOutcome>>) -> HistoryEntry {
    HistoryEntry {
        time,
        stops: Vec::new(),
        removed_bytes,
        removed: 2,
        partial: 3,
        gone: 5,
        kept: 1,
        complete: false,
        operations: Vec::new(),
        admin: None,
        entries,
    }
}

fn record(directory: &Path, entry: &HistoryEntry) {
    History::open(directory).unwrap().record(entry).unwrap();
}

fn uninstall(directory: &Path, time: i64) {
    History::open(directory)
        .unwrap()
        .record_uninstall(&UninstallEntry {
            time,
            source: PackageSource::Flatpak,
            name: "GIMP".to_owned(),
            packages: vec!["org.gimp.GIMP".to_owned()],
            freed_bytes: Some(300_000_000),
            data_removed: true,
        })
        .unwrap();
}

fn append(directory: &Path, bytes: &[u8]) {
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(History::path(directory))
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// A window at the mockup's size, its History screen alone bound, on its
/// page, read once.
fn shown_history(seams: HistorySeams) -> (ui::AppWindow, HistoryScreen) {
    let window = ui::AppWindow::new().unwrap();
    window
        .window()
        .set_size(slint::PhysicalSize::new(1280, 800));
    let screen = HistoryScreen::attach(&window, seams);
    window.set_page(ui::Page::History);
    screen.page_shown(ui::Page::History);
    watch_until(&window, |window| {
        window.global::<ui::History>().get_reading() != ui::HistoryReading::Reading
    });
    (window, screen)
}

fn rows(window: &ui::AppWindow) -> Vec<ui::HistoryRow> {
    window.global::<ui::History>().get_rows().iter().collect()
}

/// The history the tests read: a cleaning with its deletion log three
/// days ago, an uninstallation and a cleaning without a log today, a line
/// that cannot be read between them.
fn kept_history(directory: &Path) {
    record(
        directory,
        &cleaning(
            before(72),
            12,
            Some(vec![
                outcome("/var/tmp/a", "removed", Some(5), None),
                outcome("/var/tmp/b", "removed", Some(7), None),
                outcome("/var/tmp/c", "kept", None, Some("recent")),
            ]),
        ),
    );
    append(directory, b"{\"version\":1,\"tim\n");
    uninstall(directory, before(3));
    record(directory, &cleaning(before(2), 2_000_000_000, None));
}

#[test]
fn every_operation_shows_newest_first_with_the_chosen_one_in_detail() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    kept_history(directory.path());
    let (window, _screen) = shown_history(seams(directory.path()));
    let history = window.global::<ui::History>();
    assert_eq!(history.get_reading(), ui::HistoryReading::Read);
    let rows = rows(&window);
    let kinds: Vec<_> = rows.iter().map(|row| row.kind).collect();
    assert_eq!(
        kinds,
        [
            ui::HistoryKind::Cleaning,
            ui::HistoryKind::Uninstall,
            ui::HistoryKind::Cleaning
        ]
    );
    // Today's two under one day, the older one under its own (the days are
    // drawn, never read: each line says its own).
    let days: Vec<_> = rows
        .iter()
        .map(|row| (row.day_first, row.days_above))
        .collect();
    assert_eq!(days, [(true, 1), (false, 1), (true, 2)]);
    assert_eq!(history.get_skipped(), 1);
    assert!(shows(
        &window,
        "1 ligne de l'historique, illisible ou incomplète, est ignorée."
    ));

    // The newest chosen: no deletion log, said so.
    assert_eq!(history.get_chosen(), 0);
    assert!(rows[0].freed_known);
    assert_eq!(
        (rows[0].removed, rows[0].partial, rows[0].gone, rows[0].kept),
        (2, 3, 5, 1)
    );
    assert!(shows(
        &window,
        "Le détail par élément n'est gardé que si le journal des suppressions est activé dans Réglages."
    ));
    assert_eq!(history.get_lines().row_count(), 0);

    // Chosen by its line: the uninstallation, then the oldest cleaning and
    // its entries by result.
    let line = |label: &str| {
        ElementHandle::find_by_accessible_label(&window, label)
            .find(|element| element.accessible_role() == Some(AccessibleRole::ListItem))
            .unwrap_or_else(|| panic!("no line {label}"))
    };
    line("Désinstallation, GIMP · 300,0 Mo libérés, aujourd'hui à 9 h")
        .invoke_accessible_default_action();
    assert_eq!(history.get_chosen(), 1);
    assert!(shows(&window, "Ses données ont été supprimées avec elle."));
    assert!(shows(&window, "org.gimp.GIMP"));
    line("Nettoyage, 12 o libérés · gardées : 1, le 10 novembre à 12 h")
        .invoke_accessible_default_action();
    assert_eq!(history.get_chosen(), 2);
    let lines: Vec<_> = history
        .get_lines()
        .iter()
        .map(|line| (line.kind, line.count, line.sized))
        .collect();
    assert_eq!(
        lines,
        [
            (ui::ReceiptKind::Removed, 2, true),
            (ui::ReceiptKind::Recent, 1, false)
        ],
        "the size of the kept unknown to the history"
    );
    assert!(
        button(&window, "Déplier Retirées")
            .accessible_description()
            .is_some()
    );
}

#[test]
fn the_chosen_operation_stays_chosen_when_the_history_is_read_again() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    kept_history(directory.path());
    let (window, screen) = shown_history(seams(directory.path()));
    let history = window.global::<ui::History>();
    history.invoke_choose(1);
    record(directory.path(), &cleaning(before(1), 1, None));
    screen.cleaning_done();
    watch_until(&window, |window| rows(window).len() == 4);
    assert_eq!(
        history.get_chosen(),
        2,
        "the uninstallation, one line lower"
    );
    // Not on the page: never read.
    screen.page_shown(ui::Page::Home);
    record(directory.path(), &cleaning(before(0), 1, None));
    screen.cleaning_done();
    assert_eq!(
        window.global::<ui::History>().get_reading(),
        ui::HistoryReading::Read
    );
    assert_eq!(rows(&window).len(), 4);
}

#[test]
fn a_cleaning_says_what_stopped_it_and_its_administrator_s_part() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    record(
        directory.path(),
        &HistoryEntry {
            admin: Some(AdminRecord {
                done: false,
                reason: Some("authDismissed".to_owned()),
            }),
            ..cleaning(before(30), 5, None)
        },
    );
    record(
        directory.path(),
        &HistoryEntry {
            stops: vec!["missingVariable".to_owned(), "someday".to_owned()],
            ..cleaning(before(26), 0, None)
        },
    );
    let (window, _screen) = shown_history(seams(directory.path()));
    let rows = rows(&window);
    assert!(shows(&window, "Nettoyage arrêté"));
    assert_eq!(
        rows[0].stops.iter().collect::<Vec<_>>(),
        [ui::HistoryStop::MissingVariable, ui::HistoryStop::Unknown]
    );
    assert!(shows(
        &window,
        "Un dossier de la session n'était pas défini : ce qu'il protège ne pouvait pas être garanti."
    ));
    assert_eq!(rows[1].admin, ui::HistoryAdmin::NotDone);
    assert_eq!(rows[1].admin_problem, ui::RightsProblem::Dismissed);
    assert!(!rows[1].freed_known, "root's part unfinished: unknown");
    window.global::<ui::History>().invoke_choose(1);
    assert!(shows(
        &window,
        "La partie administrateur ne s'est pas terminée. L'authentification a été annulée."
    ));
    assert!(shows(
        &window,
        "Nettoyage, partie administrateur, Libéré : inconnu · gardées : 1, hier à 6 h"
    ));
}

#[test]
fn an_empty_history_and_one_that_cannot_be_read_are_said() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    let (window, _screen) = shown_history(seams(directory.path()));
    assert!(shows(&window, "Aucune opération pour l'instant"));

    std::fs::create_dir(History::path(directory.path())).unwrap();
    let (window, _screen) = shown_history(seams(directory.path()));
    assert_eq!(
        window.global::<ui::History>().get_reading(),
        ui::HistoryReading::Unreadable
    );
    assert!(shows(
        &window,
        "L'historique ne peut pas être lu. Le détail est dans le journal."
    ));
    let (window, _screen) = shown_history(nowhere());
    assert!(shows(
        &window,
        "L'historique ne peut pas être lu. Le détail est dans le journal."
    ));
}

#[test]
fn the_arrows_choose_the_next_and_previous_operation() {
    use slint::platform::Key;

    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    kept_history(directory.path());
    let (window, _screen) = shown_history(seams(directory.path()));
    let history = window.global::<ui::History>();
    let press = |key| crate::cleaner::tests::press(&window, key);
    press(Key::DownArrow);
    assert_eq!(history.get_chosen(), 1);
    press(Key::End);
    assert_eq!(history.get_chosen(), 2);
    press(Key::DownArrow);
    assert_eq!(history.get_chosen(), 2, "never past the last");
    press(Key::Home);
    assert_eq!(history.get_chosen(), 0);
    press(Key::UpArrow);
    assert_eq!(history.get_chosen(), 0);
}

fn package(name: &str, version: &str, done: bool, reason: Option<&str>) -> UpdatedPackage {
    UpdatedPackage {
        name: name.to_owned(),
        version: version.to_owned(),
        done,
        reason: reason.map(str::to_owned),
    }
}

/// An installation of updates `hours` before noon.
fn update(
    hours: i64,
    source: PackageSource,
    packages: Vec<UpdatedPackage>,
    reason: Option<&str>,
) -> UpdateEntry {
    UpdateEntry {
        time: before(hours),
        source,
        done: !packages.is_empty() && packages.iter().all(|package| package.done),
        packages,
        reason: reason.map(str::to_owned),
        restart_required: false,
    }
}

/// The system all done, a restart asked; snaps, one refused while its app
/// was open and one waiting still; the system stopped in dpkg; snaps none
/// of which went, for a reason this version does not know; the system
/// cancelled before anything; an empty line, as by hand.
fn kept_updates(directory: &Path) {
    let lines = [
        UpdateEntry {
            restart_required: true,
            ..update(
                7,
                PackageSource::Deb,
                vec![
                    package("openssl", "3.5.1", true, None),
                    package("bash", "5.2.37", true, None),
                ],
                None,
            )
        },
        update(
            6,
            PackageSource::Snap,
            vec![
                package("spotify", "1.2.74", false, Some("appsOpen")),
                package("firefox", "145.0", true, None),
                package("obs-studio", "32.0.2", false, Some("waiting")),
            ],
            None,
        ),
        update(
            5,
            PackageSource::Deb,
            vec![package("zsh", "5.9", false, Some("unknown"))],
            Some("unanswered"),
        ),
        update(
            4,
            PackageSource::Snap,
            vec![package("vlc", "3.0.21", false, None)],
            Some("aLaterReason"),
        ),
        update(
            3,
            PackageSource::Deb,
            vec![package("mesa", "25.2", false, None)],
            Some("cancelled"),
        ),
        update(2, PackageSource::Snap, Vec::new(), Some("cancelled")),
        // As written when snapd said done and every snap waits still.
        update(
            1,
            PackageSource::Snap,
            vec![package("deja", "1.0", false, None)],
            Some("waiting"),
        ),
    ];
    let mut history = History::open(directory).unwrap();
    for line in &lines {
        history.record_update(line).unwrap();
    }
}

#[test]
fn each_update_shows_how_it_ended_and_each_package_asked() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    kept_updates(directory.path());
    let (window, _screen) = shown_history(seams(directory.path()));
    let ends: Vec<_> = rows(&window)
        .iter()
        .map(|row| (row.update_end, row.update_why))
        .collect();
    assert_eq!(
        ends,
        [
            (ui::UpdateEnd::Failed, ui::InstallWhy::None),
            (ui::UpdateEnd::Cancelled, ui::InstallWhy::None),
            (ui::UpdateEnd::Cancelled, ui::InstallWhy::None),
            (ui::UpdateEnd::Unknown, ui::InstallWhy::None),
            (ui::UpdateEnd::Unknown, ui::InstallWhy::Unanswered),
            (ui::UpdateEnd::Partly, ui::InstallWhy::None),
            (ui::UpdateEnd::Done, ui::InstallWhy::None),
        ],
        "an empty line never said done, a reason unknown never said known"
    );
    let line = |label: &str| {
        ElementHandle::find_by_accessible_label(&window, label)
            .find(|element| element.accessible_role() == Some(AccessibleRole::ListItem))
            .unwrap_or_else(|| panic!("no line {label}"))
    };
    for (label, said) in [
        (
            "Mise à jour, Système · 2 installées, aujourd'hui à 5 h",
            &[
                "Toutes les mises à jour demandées ont été installées.",
                "Le système a demandé un redémarrage pour la terminer.",
                "openssl 3.5.1 : installée",
                "bash 5.2.37 : installée",
            ][..],
        ),
        (
            "Mise à jour, Applis · installées : 1 sur 3, aujourd'hui à 6 h",
            &[
                "Une partie des mises à jour demandées n'a pas été installée.",
                "spotify 1.2.74 : non installée. Une appli à mettre à jour était ouverte.",
                "obs-studio 32.0.2 : encore en attente",
            ],
        ),
        (
            "Mise à jour, Système · issue inconnue, aujourd'hui à 7 h",
            &[
                "Ce qui a été installé n'est pas connu.",
                "Le gestionnaire de mises à jour ne répondait plus.",
                "zsh 5.9 : état inconnu",
            ],
        ),
        (
            "Mise à jour, Applis · issue inconnue, aujourd'hui à 8 h",
            &["vlc 3.0.21 : état inconnu"],
        ),
        (
            "Mise à jour, Système · annulée, aujourd'hui à 9 h",
            &[
                "Annulée avant toute installation.",
                "mesa 25.2 : non installée",
            ],
        ),
        (
            "Mise à jour, Applis · aucune installée, aujourd'hui à 11 h",
            &[
                "Aucune mise à jour n'a été installée.",
                "deja 1.0 : encore en attente",
            ],
        ),
    ] {
        line(label).invoke_accessible_default_action();
        for text in said {
            assert!(shows(&window, text), "{label}: {text}");
        }
    }
}

#[test]
fn the_sidebar_leads_to_the_history() {
    let crate::Screens { window, .. } = crate::wide_window(
        crate::cleaner::Seams {
            history: nowhere(),
            ..seams_never_scanning()
        },
        crate::dashboard::tests::inert(),
        Language::French,
    );
    button(&window, "Historique").invoke_accessible_default_action();
    assert_eq!(window.get_page(), ui::Page::History);
    slint::platform::update_timers_and_animations();
    assert!(shows(
        &window,
        "Ce qu'Okapi a nettoyé, désinstallé et mis à jour sur cet ordinateur, le plus récent d'abord. Les plus anciennes opérations partent au-delà de la taille gardée."
    ));
}

#[test]
fn every_stop_the_engine_names_is_known() {
    use okapi_engine::Stop;
    use okapi_rules::Variable;

    for stop in [
        Stop::UnsupportedSystem,
        Stop::MissingVariable {
            variable: Variable::Home,
        },
        Stop::UnreadableVariable {
            variable: Variable::Home,
            path: "/home".into(),
            error: std::io::ErrorKind::PermissionDenied,
        },
        Stop::MinAgeBeyondClock,
        Stop::Cancelled,
    ] {
        // A stop added to the engine fails to compile here until it is
        // listed above.
        match &stop {
            Stop::UnsupportedSystem
            | Stop::MissingVariable { .. }
            | Stop::UnreadableVariable { .. }
            | Stop::MinAgeBeyondClock
            | Stop::Cancelled => {}
        }
        assert_ne!(
            stop_of_key(stop.key()),
            ui::HistoryStop::Unknown,
            "{stop:?}"
        );
    }
}

/// A reading asked for while one runs is done once it ends: a cleaning that
/// ends while the page reads is shown.
#[test]
fn a_reading_asked_for_while_one_runs_is_done_when_it_ends() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    kept_history(directory.path());
    let window = ui::AppWindow::new().unwrap();
    let screen = HistoryScreen::attach(&window, seams(directory.path()));
    screen.page_shown(ui::Page::History);
    assert!(screen.0.borrow().reading.is_some(), "a reading under way");
    record(directory.path(), &cleaning(before(1), 1, None));
    screen.cleaning_done();
    assert!(screen.0.borrow().again, "asked again, not started");
    watch_until(&window, |window| rows(window).len() == 4);
    watch_until(&window, |_| screen.0.borrow().reading.is_none());
    assert!(!screen.0.borrow().again);
}

/// What the history keeps of each: a log that cannot be read is never
/// said turned off, an entry that cannot be read is counted kept and said,
/// an uninstallation without a size nor its data says so.
#[test]
fn what_cannot_be_read_back_is_said() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        History::path(directory.path()),
        format!(
            "{{\"version\":1,\"kind\":\"deletions\",\"time\":{t},\"entries\":[{{\"path\":3}}]}}\n",
            t = before(5)
        ),
    )
    .unwrap();
    record(directory.path(), &cleaning(before(5), 1, None));
    record(
        directory.path(),
        &cleaning(
            before(4),
            1,
            Some(vec![
                outcome("/var/tmp/a", "removed", Some(1), None),
                outcome("/var/tmp/b", "vanished", None, None),
            ]),
        ),
    );
    History::open(directory.path())
        .unwrap()
        .record_uninstall(&UninstallEntry {
            time: before(3),
            source: PackageSource::Snap,
            name: "Spotify".to_owned(),
            packages: vec!["spotify".to_owned()],
            freed_bytes: None,
            data_removed: false,
        })
        .unwrap();
    let (window, _screen) = shown_history(seams(directory.path()));
    let rows = rows(&window);
    let history = window.global::<ui::History>();
    // The uninstallation, newest.
    assert_eq!(rows[0].source, ui::AppSource::Snap);
    assert!(!rows[0].freed_known && !rows[0].data_removed);
    assert!(shows(&window, "Espace libéré inconnu."));
    assert!(shows(&window, "Ses données ont été gardées."));
    history.invoke_choose(1);
    assert_eq!(rows[1].deletion_log, ui::DeletionLog::Read);
    assert_eq!(rows[1].unreadable_entries, 1);
    assert!(shows(
        &window,
        "1 élément du journal des suppressions ne se lit plus : compté parmi les gardés."
    ));
    history.invoke_choose(2);
    assert_eq!(rows[2].deletion_log, ui::DeletionLog::Unreadable);
    assert!(shows(
        &window,
        "Le journal des suppressions de ce nettoyage ne se lit plus : le détail par élément est perdu."
    ));
    assert!(!shows(
        &window,
        "Le détail par élément n'est gardé que si le journal des suppressions est activé dans Réglages."
    ));
    assert!(
        !shows_a_text_starting(&window, "1 ligne de l'historique"),
        "nothing skipped: no banner"
    );
}

/// Newest first by their time, whatever the file's order (a clock set
/// back); a day is told by its month and year too.
#[test]
fn the_operations_are_sorted_by_time_and_grouped_by_whole_day() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    let month = 31 * 24;
    for hours in [72 + month + 1, 73, 72 + month + 2, 74] {
        record(directory.path(), &cleaning(before(hours), 1, None));
    }
    let (window, _screen) = shown_history(seams(directory.path()));
    let read: Vec<_> = rows(&window)
        .iter()
        .map(|row| (row.at.hour, row.day_first))
        .collect();
    // 11 h and 10 h on the 10th of November, then of October.
    assert_eq!(read, [(11, true), (10, false), (11, true), (10, false)]);
}

#[test]
fn a_finished_administrator_s_part_is_said() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    record(
        directory.path(),
        &HistoryEntry {
            admin: Some(AdminRecord {
                done: true,
                reason: None,
            }),
            ..cleaning(before(1), 1, None)
        },
    );
    let (window, _screen) = shown_history(seams(directory.path()));
    assert_eq!(rows(&window)[0].admin, ui::HistoryAdmin::Done);
    assert!(rows(&window)[0].freed_known);
    assert!(shows(&window, "La partie administrateur s'est terminée."));
}

/// Stopped after root's part removed entries: never « Rien n'a été
/// touché ». An operation at a time no date holds is left out, counted.
#[test]
fn a_stopped_cleaning_says_what_root_s_part_did_and_an_undated_one_is_left_out() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    record(
        directory.path(),
        &HistoryEntry {
            stops: vec!["unreadableVariable".to_owned()],
            admin: Some(AdminRecord {
                done: true,
                reason: None,
            }),
            ..cleaning(before(1), 2_000_000_000, None)
        },
    );
    record(directory.path(), &cleaning(i64::MAX, 1, None));
    let (window, _screen) = shown_history(seams(directory.path()));
    assert_eq!(rows(&window).len(), 1);
    assert_eq!(window.global::<ui::History>().get_skipped(), 1);
    assert!(shows(
        &window,
        "Nettoyage arrêté, partie administrateur, 2,0 Go libérés · gardées : 1, aujourd'hui à 11 h"
    ));
}

/// Stopped before touching anything: said so. Read again and found
/// unreadable, nothing of the reading before is left.
#[test]
fn a_cleaning_that_touched_nothing_says_so_and_an_unreadable_history_shows_nothing() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    kept_history(directory.path());
    record(
        directory.path(),
        &HistoryEntry {
            stops: vec!["minAgeBeyondClock".to_owned()],
            removed: 0,
            partial: 0,
            removed_bytes: 0,
            ..cleaning(before(1), 0, None)
        },
    );
    let (window, screen) = shown_history(seams(directory.path()));
    assert!(shows(
        &window,
        "Nettoyage arrêté, Rien n'a été touché, aujourd'hui à 11 h"
    ));
    let history = window.global::<ui::History>();
    assert_eq!(history.get_skipped(), 1);
    std::fs::remove_file(History::path(directory.path())).unwrap();
    std::fs::create_dir(History::path(directory.path())).unwrap();
    screen.page_shown(ui::Page::History);
    watch_until(&window, |window| {
        window.global::<ui::History>().get_reading() == ui::HistoryReading::Unreadable
    });
    assert_eq!(history.get_skipped(), 0);
    assert_eq!(rows(&window).len(), 0);
}

/// Docker's prune, done or not, is said in the detail of its cleaning; one
/// whose end is unknown leaves what was freed unknown.
#[test]
fn a_cleaning_says_what_docker_pruned() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    let prune = |end| okapi_store::OperationRecord {
        action: okapi_rules::CleanupAction::DockerBuildPrune,
        end,
    };
    record(
        directory.path(),
        &cleaning(before(2), 1, None).with_operations(vec![prune(
            okapi_store::OperationEnd::NotDone {
                reason: okapi_store::OperationReason::NotAllowed,
            },
        )]),
    );
    record(
        directory.path(),
        &cleaning(before(1), 1, None).with_operations(vec![prune(
            okapi_store::OperationEnd::Unknown {
                reason: okapi_store::OperationReason::Unanswered,
            },
        )]),
    );
    let (window, _screen) = shown_history(seams(directory.path()));
    assert!(!rows(&window)[0].freed_known, "it may have pruned");
    assert!(shows(
        &window,
        "Docker : sa réponse n'a pas dit ce qui est parti, la place libérée est inconnue. Le détail est dans le journal."
    ));
    assert!(rows(&window)[1].freed_known);
    assert_eq!(
        rows(&window)[1]
            .operations
            .row_data(0)
            .map(|shown| shown.end),
        Some(ui::OperationEnd::NotAllowed)
    );
}

/// The user's part stopped before it touched anything, then Docker: the
/// line never says nothing was touched once Docker freed room or may have,
/// and does when Docker had nothing to remove.
#[test]
fn a_cleaning_stopped_before_docker_pruned_is_not_one_that_touched_nothing() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = tempfile::tempdir().unwrap();
    let stopped_then = |hours, end| {
        let stopped = HistoryEntry {
            stops: vec!["missingVariable".to_owned()],
            removed: 0,
            partial: 0,
            gone: 0,
            kept: 0,
            ..cleaning(before(hours), 0, None)
        };
        record(
            directory.path(),
            &stopped.with_operations(vec![okapi_store::OperationRecord {
                action: okapi_rules::CleanupAction::DockerBuildPrune,
                end,
            }]),
        );
    };
    stopped_then(
        3,
        okapi_store::OperationEnd::Done {
            entries: 0,
            bytes: 2_000,
        },
    );
    stopped_then(
        2,
        okapi_store::OperationEnd::Done {
            entries: 0,
            bytes: 0,
        },
    );
    stopped_then(
        1,
        okapi_store::OperationEnd::Unknown {
            reason: okapi_store::OperationReason::Unanswered,
        },
    );
    let (window, _screen) = shown_history(seams(directory.path()));
    let touched: Vec<bool> = rows(&window)
        .iter()
        .map(|row| row.touched_by_operations)
        .collect();
    assert_eq!(touched, [true, false, true], "newest first");
    let nothing_touched_at = |hour: u8| {
        shows(
            &window,
            &format!("Nettoyage arrêté, Rien n'a été touché, aujourd'hui à {hour} h"),
        )
    };
    assert!(nothing_touched_at(10), "Docker had nothing to remove");
    assert!(!nothing_touched_at(9), "Docker freed room");
    assert!(!nothing_touched_at(11), "Docker may have freed room");
}
