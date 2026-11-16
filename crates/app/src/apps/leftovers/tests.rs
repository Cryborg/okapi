//! The tab of leftovers in a window, on a sandbox: a home folder and a trash
//! of its own, what is installed given by hand; the analysis and the move
//! are the library's own (its report cannot be built elsewhere).

use std::cell::Cell;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use okapi_engine::{Exclusions, ProtectedRoots};
use okapi_rules::Language;
use okapi_rules::TemplateVariables;
use okapi_store::{SeenApp, SeenApps};
use okapi_tools::apps::leftovers::{self, Installed, Known, LeftoverReport, MOST_ENTRIES, Places};
use slint::Model as _;

use super::*;
use crate::apps::{AppsSeams, READ_EVERY};
use crate::cleaner::tests::{button, seams_never_scanning, shows};

const WAIT: Duration = Duration::from_secs(20);

fn nothing_protected() -> ProtectedRoots {
    ProtectedRoots::of(&TemplateVariables::new()).unwrap()
}

fn read(names: &[&str]) -> Known {
    Known::Read(names.iter().map(|name| (*name).to_owned()).collect())
}

/// What finds nothing and moves nothing.
pub(crate) fn inert() -> LeftoverSeams {
    LeftoverSeams {
        analyse: Arc::new(|_| Analysis {
            report: Ok(leftovers::find(
                &Places {
                    home: PathBuf::from("/nonexistent"),
                    user: 0,
                    most_entries: MOST_ENTRIES,
                },
                &Installed {
                    snaps: read(&[]),
                    flatpaks: read(&[]),
                },
                &nothing_protected(),
                &Exclusions::none(),
            )),
            residual: None,
            seen: SeenApps::default(),
            copies: Copies::None,
        }),
        trash: Arc::new(|_, _, _| Err(io::Error::other("inert"))),
        remember: Arc::new(|_| {}),
        forget: Arc::new(|_, _| Forgot::Gone),
    }
}

/// A home folder holding `~/snap/spotify`, `~/snap/firefox` (installed) and
/// `~/.var/app/org.gimp.GIMP`, and a trash beside it.
struct Sandbox {
    _root: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
}

fn sandbox() -> Arc<Sandbox> {
    let root = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(root.path()).unwrap();
    let home = canonical.join("home");
    for folder in [
        "snap/spotify/deep",
        "snap/firefox",
        ".var/app/org.gimp.GIMP",
    ] {
        std::fs::create_dir_all(home.join(folder)).unwrap();
    }
    std::fs::write(home.join("snap/spotify/deep/file"), [0; 4096]).unwrap();
    let data = canonical.join("data");
    std::fs::create_dir_all(&data).unwrap();
    Arc::new(Sandbox {
        _root: root,
        home,
        data,
    })
}

fn installed(snaps: Known) -> Installed {
    Installed {
        snaps,
        flatpaks: read(&[]),
    }
}

/// The sandbox's analysis and trash; what the screen asked to remember.
fn seams_of(
    sandbox: &Arc<Sandbox>,
    snaps: Known,
    seen: SeenApps,
    remembered: Arc<Mutex<Vec<Vec<SeenApp>>>>,
) -> LeftoverSeams {
    let analysed = Arc::clone(sandbox);
    let moved = Arc::clone(sandbox);
    let now = installed(snaps.clone());
    LeftoverSeams {
        analyse: Arc::new(move |exclusions| Analysis {
            report: exclusions.map(|exclusions| {
                leftovers::find(
                    &Places {
                        home: analysed.home.clone(),
                        user: okapi_platform::effective_user().unwrap(),
                        most_entries: MOST_ENTRIES,
                    },
                    &installed(snaps.clone()),
                    &nothing_protected(),
                    &Exclusions::new(exclusions).unwrap(),
                )
            }),
            residual: Some(3),
            seen: seen.clone(),
            copies: Copies::None,
        }),
        trash: Arc::new(move |report: &LeftoverReport, ids, _| {
            let bin = okapi_platform::Trash::in_folder(&moved.data)?;
            let now = now.clone();
            Ok(leftovers::trash(
                report,
                ids,
                &bin,
                &move || now.clone(),
                &leftovers::TrashRules {
                    protected: &nothing_protected(),
                    exclusions: &Exclusions::none(),
                },
            ))
        }),
        remember: Arc::new(move |seen| remembered.lock().unwrap().push(seen)),
        forget: Arc::new(|_, _| Forgot::Gone),
    }
}

fn read_once() {
    i_slint_backend_testing::mock_elapsed_time(READ_EVERY);
    slint::platform::update_timers_and_animations();
}

fn tab(window: &ui::AppWindow) -> ui::Leftovers<'_> {
    window.global::<ui::Leftovers>()
}

fn read_until(window: &ui::AppWindow, done: impl Fn(&ui::AppWindow) -> bool) {
    let started = Instant::now();
    while !done(window) {
        assert!(started.elapsed() < WAIT, "never came");
        read_once();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn analysed(window: &ui::AppWindow) {
    read_until(window, |window| {
        !tab(window).get_analysing() && !tab(window).get_trashing()
    });
}

/// Whether the settings can be read: changed by a test as it goes.
fn settings(readable: bool) -> Rc<Cell<bool>> {
    Rc::new(Cell::new(readable))
}

/// The window on the applications page, reading `arrivals`; the tab's
/// seams `seams`, the settings read while `readable` says so.
fn on_page(
    seams: LeftoverSeams,
    arrivals: fn() -> Vec<okapi_tools::apps::Arrival>,
    readable: Rc<Cell<bool>>,
) -> (ui::AppWindow, crate::apps::AppsScreen) {
    on_page_at(seams, arrivals, readable, None)
}

/// [`on_page`], the home folder `home`.
fn on_page_at(
    seams: LeftoverSeams,
    arrivals: fn() -> Vec<okapi_tools::apps::Arrival>,
    readable: Rc<Cell<bool>>,
    home: Option<PathBuf>,
) -> (ui::AppWindow, crate::apps::AppsScreen) {
    let mut cleaner = seams_never_scanning();
    let read = Rc::clone(&cleaner.read);
    cleaner.read = Rc::new(move || {
        if readable.get() {
            read()
        } else {
            Err(crate::cleaner::Problem::Exclusion)
        }
    });
    let seams = crate::cleaner::Seams {
        apps: AppsSeams {
            read: Rc::new(move |_, sink, _| {
                for arrival in arrivals() {
                    sink.send(arrival).unwrap();
                }
            }),
            leftovers: seams,
            home,
            ..crate::apps::tests::inert()
        },
        ..cleaner
    };
    let crate::Screens {
        window,
        apps: screen,
        ..
    } = crate::wide_window(seams, crate::dashboard::tests::inert(), Language::French);
    button(&window, "Applications").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    analysed(&window);
    (window, screen)
}

fn lines(window: &ui::AppWindow) -> Vec<(String, bool, String)> {
    tab(window)
        .get_lines()
        .iter()
        .map(|line| (line.name.to_string(), line.known, line.path.to_string()))
        .collect()
}

#[test]
fn the_leftovers_are_found_with_the_list_their_badge_shown_before_the_tab_opens() {
    // Catches an analysis that waits for the tab, a leftover of an
    // application installed, and a name the user saw it by forgotten.
    let sandbox = sandbox();
    let mut seen = SeenApps::default();
    seen.remember([SeenApp {
        source: "snap".to_owned(),
        id: "spotify".to_owned(),
        shown: "Spotify".to_owned(),
        names: vec!["spotify".to_owned()],
        day: 1,
    }]);
    let seams = seams_of(&sandbox, read(&["firefox"]), seen, Arc::default());
    let (window, _) = on_page_at(
        seams,
        crate::apps::tests::arrivals,
        settings(true),
        Some(sandbox.home.clone()),
    );
    assert_eq!(
        window.global::<ui::Apps>().get_tab(),
        ui::AppsTab::Installed
    );
    let paths: Vec<(String, bool, String)> = lines(&window);
    assert_eq!(
        paths,
        [
            ("Spotify".to_owned(), true, "~/snap/spotify".to_owned()),
            (
                "org.gimp.GIMP".to_owned(),
                false,
                "~/.var/app/org.gimp.GIMP".to_owned()
            ),
        ]
    );
    assert!(
        tab(&window).get_total().whole > 0,
        "the badge says the space"
    );
    assert_eq!(tab(&window).get_residual(), 3);
    assert!(!tab(&window).get_snaps_unread());
}

#[test]
fn nothing_goes_unchecked_and_what_is_checked_goes_once_confirmed() {
    // Catches a move without a confirmation, identifiers of another report,
    // and a tab not analysed again after the move.
    let sandbox = sandbox();
    let seams = seams_of(
        &sandbox,
        read(&["firefox"]),
        SeenApps::default(),
        Arc::default(),
    );
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    window.global::<ui::Apps>().set_tab(ui::AppsTab::Leftovers);
    assert_eq!(tab(&window).get_checked_count(), 0);
    assert!(shows(
        &window,
        "Rien de coché. Cochez ce que vous voulez mettre à la corbeille."
    ));
    tab(&window).invoke_trash();
    assert!(
        !tab(&window).get_confirming(),
        "nothing checked: nothing asked"
    );
    tab(&window).invoke_toggle(0);
    assert_eq!(tab(&window).get_checked_count(), 1);
    tab(&window).invoke_trash();
    assert!(tab(&window).get_confirming());
    assert!(
        sandbox.home.join("snap/spotify").exists(),
        "not before the yes"
    );
    tab(&window).invoke_confirm();
    read_until(&window, |window| tab(window).get_receipt());
    assert_eq!(tab(&window).get_trashed(), 1);
    assert_eq!(tab(&window).get_kept(), 0);
    assert!(!sandbox.home.join("snap/spotify").exists());
    assert!(
        sandbox.data.join("Trash/files/spotify").exists(),
        "restorable"
    );
    analysed(&window);
    assert_eq!(
        lines(&window).len(),
        1,
        "analysed again: only org.gimp.GIMP left"
    );
}

#[test]
fn a_new_analysis_never_comes_under_the_boxes_checked() {
    // Catches the report replaced while the user checks: the identifiers
    // would be of another report, or the boxes jump.
    let sandbox = sandbox();
    let seams = seams_of(
        &sandbox,
        read(&["firefox"]),
        SeenApps::default(),
        Arc::default(),
    );
    let (window, screen) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    tab(&window).invoke_toggle(1);
    std::fs::create_dir_all(sandbox.home.join("snap/newcomer")).unwrap();
    screen.analyse_leftovers();
    analysed(&window);
    assert_eq!(lines(&window).len(), 2, "held while checked");
    tab(&window).invoke_toggle(1);
    read_once();
    analysed(&window);
    assert_eq!(lines(&window).len(), 3, "made once let go");
}

#[test]
fn a_source_not_read_whole_is_said_and_proposes_nothing() {
    let sandbox = sandbox();
    let unread = Known::Unread("snapd silent".to_owned());
    let seams = seams_of(&sandbox, unread, SeenApps::default(), Arc::default());
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    assert!(tab(&window).get_snaps_unread());
    assert_eq!(lines(&window).len(), 1, "the flatpak's only");
}

#[test]
fn settings_unread_propose_nothing_and_say_it() {
    let sandbox = sandbox();
    let seams = seams_of(&sandbox, read(&[]), SeenApps::default(), Arc::default());
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(false));
    assert_eq!(tab(&window).get_blocked(), ui::LeftoversBlock::Settings);
    assert_eq!(lines(&window).len(), 0);
}

#[test]
fn the_applications_of_a_whole_reading_are_remembered() {
    // Catches applications remembered from a reading that missed some (a
    // folder would later be told by too few names), and none remembered.
    let sandbox = sandbox();
    let remembered = Arc::new(Mutex::new(Vec::new()));
    let seams = seams_of(
        &sandbox,
        read(&[]),
        SeenApps::default(),
        Arc::clone(&remembered),
    );
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    read_until(&window, |_| !remembered.lock().unwrap().is_empty());
    let cards = remembered.lock().unwrap()[0].clone();
    let builder = cards
        .iter()
        .find(|card| card.id == "org.gnome.Builder")
        .unwrap();
    assert_eq!(builder.source, "flatpak");
    assert!(builder.names.contains(&"Builder".to_owned()), "{builder:?}");
}

#[test]
fn the_applications_of_a_partial_reading_are_never_remembered() {
    let sandbox = sandbox();
    let remembered = Arc::new(Mutex::new(Vec::new()));
    let seams = seams_of(
        &sandbox,
        read(&[]),
        SeenApps::default(),
        Arc::clone(&remembered),
    );
    let (window, _) = on_page(
        seams,
        || {
            let mut arrivals = crate::apps::tests::arrivals();
            arrivals.push(okapi_tools::apps::Arrival::Source(
                okapi_tools::apps::Source::Flatpak,
                okapi_tools::apps::SourceReading::Read {
                    apps: Vec::new(),
                    rest: okapi_tools::apps::Rest::default(),
                    partial: true,
                },
            ));
            arrivals
        },
        settings(true),
    );
    // The reading ended (what is remembered is sent then), then time for
    // a thread to write: a whole reading's is there by then.
    read_until(&window, |window| {
        window.global::<ui::Apps>().get_minutes_ago() >= 0
    });
    std::thread::sleep(Duration::from_millis(300));
    read_once();
    assert_eq!(remembered.lock().unwrap().len(), 0);
}

/// `seams` whose analyses after the first wait for a word on the sender.
fn gated(seams: LeftoverSeams) -> (LeftoverSeams, mpsc::Sender<()>) {
    let (sender, receiver) = mpsc::channel::<()>();
    let receiver = Mutex::new(receiver);
    let calls = AtomicUsize::new(0);
    let analyse = Arc::clone(&seams.analyse);
    let gated = LeftoverSeams {
        analyse: Arc::new(move |exclusions| {
            if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                let _ = receiver.lock().unwrap().recv();
            }
            analyse(exclusions)
        }),
        ..seams
    };
    (gated, sender)
}

#[test]
fn no_box_is_ticked_while_the_leftovers_are_read_again() {
    // Catches boxes ticked under an analysis in flight, then cleared by its
    // report without a word.
    let sandbox = sandbox();
    let (seams, gate) = gated(seams_of(
        &sandbox,
        read(&["firefox"]),
        SeenApps::default(),
        Arc::default(),
    ));
    let (window, screen) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    screen.analyse_leftovers();
    assert!(tab(&window).get_analysing());
    tab(&window).invoke_toggle(0);
    assert_eq!(tab(&window).get_checked_count(), 0, "refused while read");
    gate.send(()).unwrap();
    analysed(&window);
    tab(&window).invoke_toggle(0);
    assert_eq!(tab(&window).get_checked_count(), 1);
}

/// `seams` whose move counts its calls.
fn counted(seams: LeftoverSeams) -> (LeftoverSeams, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let trash = Arc::clone(&seams.trash);
    let seams = LeftoverSeams {
        trash: Arc::new(move |report, ids, exclusions| {
            counter.fetch_add(1, Ordering::SeqCst);
            trash(report, ids, exclusions)
        }),
        ..seams
    };
    (seams, calls)
}

#[test]
fn settings_unread_at_the_yes_or_a_second_yes_move_nothing() {
    // Catches a move made with the exclusions of nothing (the settings not
    // read), and two moves of one confirmation clicked twice.
    let sandbox = sandbox();
    let (seams, calls) = counted(seams_of(
        &sandbox,
        read(&["firefox"]),
        SeenApps::default(),
        Arc::default(),
    ));
    let readable = settings(true);
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, Rc::clone(&readable));
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_trash();
    readable.set(false);
    tab(&window).invoke_confirm();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(tab(&window).get_trash_failed());
    readable.set(true);
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_trash();
    tab(&window).invoke_confirm();
    tab(&window).invoke_confirm();
    read_until(&window, |window| {
        tab(window).get_receipt() && !tab(window).get_trashing()
    });
    analysed(&window);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_move_that_ends_without_a_word_says_to_look_in_the_trash() {
    let seams = LeftoverSeams {
        trash: Arc::new(|_, _, _| panic!("the move fails midway")),
        ..seams_of(
            &sandbox(),
            read(&["firefox"]),
            SeenApps::default(),
            Arc::default(),
        )
    };
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_trash();
    tab(&window).invoke_confirm();
    read_until(&window, |window| tab(window).get_receipt());
    assert!(tab(&window).get_trash_interrupted());
    assert!(!tab(&window).get_trash_failed(), "never said none moved");
}

#[test]
fn settings_written_look_again_for_the_leftovers_they_held_back() {
    // Catches a tab left saying « settings unread » once they are written,
    // and an analysis made at every write when nothing was held back.
    let sandbox = sandbox();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let base = seams_of(
        &sandbox,
        read(&["firefox"]),
        SeenApps::default(),
        Arc::default(),
    );
    let analyse = Arc::clone(&base.analyse);
    let seams = LeftoverSeams {
        analyse: Arc::new(move |exclusions| {
            counter.fetch_add(1, Ordering::SeqCst);
            analyse(exclusions)
        }),
        ..base
    };
    let readable = settings(false);
    let (window, screen) = on_page(seams, crate::apps::tests::arrivals, Rc::clone(&readable));
    assert_eq!(tab(&window).get_blocked(), ui::LeftoversBlock::Settings);
    readable.set(true);
    screen.settings_written();
    analysed(&window);
    assert_eq!(tab(&window).get_blocked(), ui::LeftoversBlock::None);
    assert_eq!(lines(&window).len(), 2);
    let before = calls.load(Ordering::SeqCst);
    screen.settings_written();
    assert_eq!(calls.load(Ordering::SeqCst), before, "nothing held back");
}

#[test]
fn settings_written_while_a_held_back_analysis_runs_look_again_after_it() {
    // Catches an analysis started without the settings, still running when
    // they are written: the tab would keep saying them unread.
    let sandbox = sandbox();
    let (seams, gate) = gated(seams_of(
        &sandbox,
        read(&["firefox"]),
        SeenApps::default(),
        Arc::default(),
    ));
    let readable = settings(true);
    let (window, screen) = on_page(seams, crate::apps::tests::arrivals, Rc::clone(&readable));
    readable.set(false);
    screen.analyse_leftovers();
    readable.set(true);
    screen.settings_written();
    gate.send(()).unwrap();
    gate.send(()).unwrap();
    read_until(&window, |window| {
        tab(window).get_blocked() == ui::LeftoversBlock::None && !tab(window).get_analysing()
    });
    assert_eq!(lines(&window).len(), 2);
}

#[test]
fn a_folder_withheld_for_safety_is_said_never_taken_for_none() {
    if okapi_platform::running_as_root() {
        return;
    }
    let sandbox = sandbox();
    let deep = sandbox.home.join("snap/spotify/deep");
    std::fs::set_permissions(&deep, std::fs::Permissions::from_mode(0o000)).unwrap();
    let seams = seams_of(
        &sandbox,
        read(&["firefox"]),
        SeenApps::default(),
        Arc::default(),
    );
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    std::fs::set_permissions(&deep, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(tab(&window).get_withheld(), 1);
    assert_eq!(lines(&window).len(), 1, "the flatpak's only");
}

#[test]
fn a_move_that_cannot_run_or_settings_unread_say_nothing_was_moved() {
    // Catches a failed move closing its window without a word, a refusal
    // of the settings turning the whole tab into « nothing proposed », and
    // a receipt that never closes.
    let sandbox = sandbox();
    let seams = LeftoverSeams {
        trash: Arc::new(|_, _, _| Err(io::Error::other("no trash"))),
        ..seams_of(
            &sandbox,
            read(&["firefox"]),
            SeenApps::default(),
            Arc::default(),
        )
    };
    let readable = settings(true);
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, Rc::clone(&readable));
    window.global::<ui::Apps>().set_tab(ui::AppsTab::Leftovers);
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_trash();
    tab(&window).invoke_confirm();
    read_until(&window, |window| tab(window).get_receipt());
    assert!(tab(&window).get_trash_failed());
    assert!(shows(
        &window,
        "Rien n'a été mis à la corbeille. Le détail est dans le journal."
    ));
    tab(&window).invoke_close_receipt();
    assert!(!tab(&window).get_receipt());
    analysed(&window);

    readable.set(false);
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_trash();
    tab(&window).invoke_confirm();
    assert!(tab(&window).get_receipt());
    assert!(tab(&window).get_trash_failed());
    assert_eq!(tab(&window).get_blocked(), ui::LeftoversBlock::None);
    assert!(sandbox.home.join("snap/spotify").exists());
}

#[test]
fn a_leftover_that_changed_since_is_kept_and_counted_apart() {
    // Catches the second line's identifier lost, and a receipt saying all
    // moved when one stayed.
    let sandbox = sandbox();
    let seams = seams_of(
        &sandbox,
        read(&["firefox"]),
        SeenApps::default(),
        Arc::default(),
    );
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_toggle(1);
    tab(&window).invoke_trash();
    std::fs::write(sandbox.home.join("snap/spotify/new"), b"x").unwrap();
    tab(&window).invoke_confirm();
    read_until(&window, |window| tab(window).get_receipt());
    assert!(!tab(&window).get_trash_failed());
    assert_eq!(tab(&window).get_trashed(), 1);
    assert_eq!(tab(&window).get_kept(), 1);
    assert_eq!(tab(&window).get_unchecked(), 0);
    assert!(!sandbox.home.join(".var/app/org.gimp.GIMP").exists());
    assert!(sandbox.home.join("snap/spotify").exists());
}

#[test]
fn an_analysis_that_ends_without_a_word_is_said_never_taken_for_none() {
    // Catches « no leftover found » shown when nothing was looked at.
    let seams = LeftoverSeams {
        analyse: Arc::new(|_| panic!("the analysis fails")),
        ..inert()
    };
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    assert!(tab(&window).get_analysis_failed());
    assert!(!tab(&window).get_searched());
}

#[test]
fn a_leftover_too_large_to_be_checked_is_never_ticked() {
    let sandbox = sandbox();
    let base = seams_of(
        &sandbox,
        read(&["firefox"]),
        SeenApps::default(),
        Arc::default(),
    );
    let analysed = Arc::clone(&sandbox);
    let seams = LeftoverSeams {
        analyse: Arc::new(move |_| Analysis {
            report: Ok(leftovers::find(
                &Places {
                    home: analysed.home.clone(),
                    user: okapi_platform::effective_user().unwrap(),
                    most_entries: 1,
                },
                &installed(read(&["firefox"])),
                &nothing_protected(),
                &Exclusions::none(),
            )),
            residual: None,
            seen: SeenApps::default(),
            copies: Copies::None,
        }),
        ..base
    };
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    assert!(!tab(&window).get_lines().row_data(0).unwrap().checkable);
    tab(&window).invoke_toggle(0);
    assert_eq!(tab(&window).get_checked_count(), 0);
}

/// snapd's answer listing firefox's and nordvpn's automatic copies.
const FIREFOX_COPY: &str = r#"[{"id":1,"snapshots":[{"set":1,"time":"2026-10-17T18:39:44+02:00","snap":"firefox","revision":"1","snap-id":"x","epoch":{"read":[0]},"summary":"","version":"1","sha3-384":{},"size":668852084,"auto":true}]},{"id":3,"snapshots":[{"set":3,"time":"2026-10-17T21:48:00+02:00","snap":"nordvpn","revision":"81","snap-id":"y","epoch":{"read":[0]},"summary":"","version":"5","sha3-384":{},"size":654252,"auto":true}]}]"#;

/// What snapd keeps: firefox's and nordvpn's copies, none of their snaps
/// installed.
fn firefox_copy() -> Copies {
    okapi_tools::apps::snapshots::find(Ok(serde_json::from_str(FIREFOX_COPY).unwrap()), &read(&[]))
}

/// `seams` whose analyses also give `copies` (made again each time), and
/// whose forgetting tells the snap asked for, then says `forgot`.
fn with_copies(
    seams: LeftoverSeams,
    copies: fn() -> Copies,
    forgot: Forgot,
) -> (LeftoverSeams, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let told = Arc::clone(&asked);
    let analyse = Arc::clone(&seams.analyse);
    let seams = LeftoverSeams {
        analyse: Arc::new(move |exclusions| Analysis {
            copies: copies(),
            ..analyse(exclusions)
        }),
        forget: Arc::new(move |report, id| {
            let snap = report
                .find(id)
                .map_or_else(String::new, |copy| copy.snap().to_owned());
            told.lock().unwrap().push(snap);
            forgot.clone()
        }),
        ..seams
    };
    (seams, asked)
}

#[test]
fn snapd_s_copies_are_shown_named_never_under_nothing_found() {
    // Catches a copy hidden under « nothing found », and the name Okapi saw
    // the snap by lost.
    let mut seen = SeenApps::default();
    seen.remember([SeenApp {
        source: "snap".to_owned(),
        id: "firefox".to_owned(),
        shown: "Firefox".to_owned(),
        names: vec!["firefox".to_owned()],
        day: 1,
    }]);
    let base = LeftoverSeams {
        analyse: Arc::new(move |exclusions| Analysis {
            seen: seen.clone(),
            ..(inert().analyse)(exclusions)
        }),
        ..inert()
    };
    let (seams, _) = with_copies(base, firefox_copy, Forgot::Gone);
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    window.global::<ui::Apps>().set_tab(ui::AppsTab::Leftovers);
    let copies = tab(&window).get_copies();
    assert_eq!(copies.row_count(), 2);
    let line = copies.row_data(0).unwrap();
    assert_eq!(line.name, "Firefox");
    assert!(line.size.whole > 0);
    assert_eq!(
        (line.made.day_of_month, line.made.month, line.made.hour),
        (17, 10, 16),
        "dated in the seams' zone"
    );
    assert_eq!(copies.row_data(1).unwrap().name, "nordvpn");
    assert!(
        !shows(&window, "Aucun reste trouvé"),
        "a copy is no nothing"
    );
    assert!(!tab(&window).get_copies_unread());
}

#[test]
fn snapd_s_copies_unread_are_said_never_taken_for_none() {
    let (seams, _) = with_copies(
        inert(),
        || Copies::Unread("silent".to_owned()),
        Forgot::Gone,
    );
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    window.global::<ui::Apps>().set_tab(ui::AppsTab::Leftovers);
    assert!(tab(&window).get_copies_unread());
    assert_eq!(tab(&window).get_copies().row_count(), 0);
    assert!(
        !shows(&window, "Aucun reste trouvé"),
        "not read is no nothing"
    );
}

#[test]
fn without_snapd_nothing_is_said_of_its_copies() {
    let (window, _) = on_page(inert(), crate::apps::tests::arrivals, settings(true));
    window.global::<ui::Apps>().set_tab(ui::AppsTab::Leftovers);
    assert!(
        !tab(&window).get_copies_unread(),
        "no snapd: nothing to say"
    );
    assert!(shows(&window, "Aucun reste trouvé"));
}

#[test]
fn a_copy_is_forgotten_only_once_confirmed_by_its_identifier_then_told_and_read_again() {
    // Catches a copy forgotten without the yes, another copy asked for,
    // the outcome unsaid, and the tab not read again.
    let (seams, asked) = with_copies(inert(), firefox_copy, Forgot::Forgotten);
    let analyses = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&analyses);
    let analyse = Arc::clone(&seams.analyse);
    let seams = LeftoverSeams {
        analyse: Arc::new(move |exclusions| {
            counted.fetch_add(1, Ordering::SeqCst);
            analyse(exclusions)
        }),
        ..seams
    };
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    tab(&window).invoke_forget(1);
    assert!(tab(&window).get_forget_asking());
    assert_eq!(tab(&window).get_forget_name(), "nordvpn");
    assert!(asked.lock().unwrap().is_empty(), "not before the yes");
    tab(&window).invoke_cancel_forget();
    assert!(!tab(&window).get_forget_asking());
    tab(&window).invoke_forget(1);
    let before = analyses.load(Ordering::SeqCst);
    tab(&window).invoke_confirm_forget();
    read_until(&window, |window| {
        tab(window).get_forgot() != ui::ForgetOutcome::None
    });
    assert_eq!(
        *asked.lock().unwrap(),
        ["nordvpn"],
        "the copy shown, no other"
    );
    assert_eq!(tab(&window).get_forgot(), ui::ForgetOutcome::Forgotten);
    assert_eq!(tab(&window).get_forgot_name(), "nordvpn");
    analysed(&window);
    assert!(analyses.load(Ordering::SeqCst) > before, "read again");
    tab(&window).invoke_close_forgot();
    assert_eq!(tab(&window).get_forgot(), ui::ForgetOutcome::None);
}

/// firefox's and nordvpn's copies, in the other order from the second
/// reading on: a rank shown before is another copy after.
fn copies_reordered() -> Copies {
    static READINGS: AtomicUsize = AtomicUsize::new(0);
    let listed: serde_json::Value = serde_json::from_str(FIREFOX_COPY).unwrap();
    let mut sets = listed.as_array().unwrap().clone();
    if READINGS.fetch_add(1, Ordering::SeqCst) > 0 {
        sets.reverse();
    }
    okapi_tools::apps::snapshots::find(Ok(serde_json::Value::Array(sets)), &read(&[]))
}

#[test]
fn the_copy_forgotten_is_the_one_the_window_names_whatever_is_read_meanwhile() {
    // Catches the copy asked kept by its rank: a reading come between the
    // question and the yes would forget another one.
    let (seams, asked) = with_copies(inert(), copies_reordered, Forgot::Forgotten);
    let (window, screen) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    tab(&window).invoke_forget(1);
    assert_eq!(tab(&window).get_forget_name(), "nordvpn");
    screen.analyse_leftovers();
    analysed(&window);
    tab(&window).invoke_confirm_forget();
    read_until(&window, |window| {
        tab(window).get_forgot() != ui::ForgetOutcome::None
    });
    assert_eq!(*asked.lock().unwrap(), ["nordvpn"]);
}

#[test]
fn nothing_else_runs_while_a_copy_is_forgotten_nor_over_boxes_checked() {
    // Catches a box ticked or a second copy asked while snapd forgets one
    // (polkit may wait a long while), and a copy asked over boxes checked,
    // which would hold back the reading that follows.
    let sandbox = sandbox();
    let (gate, waiting) = mpsc::channel::<()>();
    let waiting = Mutex::new(waiting);
    let (seams, _) = with_copies(
        seams_of(
            &sandbox,
            read(&["firefox"]),
            SeenApps::default(),
            Arc::default(),
        ),
        firefox_copy,
        Forgot::Forgotten,
    );
    let forget = Arc::clone(&seams.forget);
    let seams = LeftoverSeams {
        forget: Arc::new(move |report, id| {
            let _ = waiting.lock().unwrap().recv();
            forget(report, id)
        }),
        ..seams
    };
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(true));
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_forget(0);
    assert!(!tab(&window).get_forget_asking(), "boxes checked");
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_forget(0);
    tab(&window).invoke_confirm_forget();
    assert!(tab(&window).get_forgetting());
    assert!(tab(&window).get_busy());
    tab(&window).invoke_toggle(0);
    assert_eq!(tab(&window).get_checked_count(), 0);
    tab(&window).invoke_forget(0);
    assert!(!tab(&window).get_forget_asking());
    gate.send(()).unwrap();
    read_until(&window, |window| !tab(window).get_forgetting());
}

#[test]
fn snapd_s_copies_stay_shown_when_the_settings_block_the_leftovers() {
    let (seams, _) = with_copies(
        seams_of(&sandbox(), read(&[]), SeenApps::default(), Arc::default()),
        firefox_copy,
        Forgot::Gone,
    );
    let (window, _) = on_page(seams, crate::apps::tests::arrivals, settings(false));
    assert_eq!(tab(&window).get_blocked(), ui::LeftoversBlock::Settings);
    assert_eq!(tab(&window).get_copies().row_count(), 2);
}
