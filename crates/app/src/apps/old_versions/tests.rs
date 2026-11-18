//! The tab of old versions in a window: the search is the library's own on
//! snapd's list given by hand (its report cannot be built elsewhere), the
//! removal a fake that tells what the test says.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use okapi_rules::Language;
use okapi_store::HistoryUnavailable;
use okapi_tools::apps::leftovers::InstalledPlaces;
use okapi_tools::apps::old_versions::find;
use okapi_tools::packages::snapd::SnapdError;
use serde_json::Value;
use slint::Model as _;

use super::*;
use crate::apps::{AppsSeams, READ_EVERY};
use crate::cleaner::tests::{button, seams_never_scanning, shows, shows_a_text_starting};

const WAIT: Duration = Duration::from_secs(20);

/// No snapd, no flatpak: nothing found.
pub(crate) fn inert() -> OldVersionSeams {
    OldVersionSeams {
        find: Arc::new(|stop| {
            find(
                Err(SnapdError::Absent {
                    socket: PathBuf::from("/run/snapd.socket"),
                    source: std::io::ErrorKind::NotFound.into(),
                }),
                InstalledPlaces::new(PathBuf::from("/nonexistent"), Vec::new(), None),
                Path::new("/nonexistent"),
                stop,
            )
        }),
        retire: Arc::new(|_, _, _| false),
    }
}

/// snapd listing two old revisions of Android Studio, and one of core22.
fn listed() -> Value {
    let revision = |name: &str, number: &str, kind: &str, status: &str| {
        serde_json::json!({
            "name": name, "revision": number, "version": format!("v{number}"),
            "type": kind, "status": status, "installed-size": 4096,
        })
    };
    Value::from(vec![
        revision("android-studio", "248", "app", "installed"),
        revision("android-studio", "249", "app", "installed"),
        revision("android-studio", "250", "app", "active"),
        revision("core22", "2987", "base", "installed"),
        revision("core22", "3129", "base", "active"),
    ])
}

/// A search over [`listed`] and a removal that tells each asked as
/// `answer` says, all they were asked counted.
#[derive(Clone)]
struct Fake {
    files: PathBuf,
    answer: Retired,
    /// Each asked, by its rank in the removal, told this instead when set.
    by_rank: Vec<Retired>,
    /// What the removal returns: runtimes' room withheld.
    withheld: bool,
    /// Each removal asked, by its identifiers.
    asked: Arc<Mutex<Vec<Vec<OldVersionId>>>>,
    searches: Arc<AtomicUsize>,
    /// A removal waits for a word here before it ends, when set.
    held: Option<Arc<Mutex<mpsc::Receiver<()>>>>,
}

impl Fake {
    fn new(files: &Path, answer: Retired) -> Self {
        Self {
            files: files.to_path_buf(),
            answer,
            by_rank: Vec::new(),
            withheld: false,
            asked: Arc::default(),
            searches: Arc::default(),
            held: None,
        }
    }

    fn seams(&self) -> OldVersionSeams {
        let searched = self.clone();
        let removing = self.clone();
        OldVersionSeams {
            find: Arc::new(move |stop| {
                searched.searches.fetch_add(1, Ordering::Relaxed);
                find(
                    Ok(listed()),
                    InstalledPlaces::new(searched.files.clone(), Vec::new(), None),
                    Path::new("/nonexistent"),
                    stop,
                )
            }),
            retire: Arc::new(move |report, ids, told| {
                removing.asked.lock().unwrap().push(ids.to_vec());
                if let Some(held) = &removing.held {
                    let _ = held.lock().unwrap().recv();
                }
                for (rank, id) in ids.iter().enumerate() {
                    let answer = removing.by_rank.get(rank).unwrap_or(&removing.answer);
                    told(report.find(*id).unwrap(), answer);
                }
                removing.withheld
            }),
        }
    }

    fn searches(&self) -> usize {
        self.searches.load(Ordering::Relaxed)
    }
}

fn read_once() {
    i_slint_backend_testing::mock_elapsed_time(READ_EVERY);
    slint::platform::update_timers_and_animations();
}

fn tab(window: &ui::AppWindow) -> ui::OldVersions<'_> {
    window.global::<ui::OldVersions>()
}

fn read_until(window: &ui::AppWindow, done: impl Fn(&ui::AppWindow) -> bool) {
    let started = Instant::now();
    while !done(window) {
        assert!(started.elapsed() < WAIT, "never came");
        read_once();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn settled(window: &ui::AppWindow) {
    read_until(window, |window| {
        !tab(window).get_finding() && !tab(window).get_retiring()
    });
}

/// The history lines written; none when it cannot be written.
type Recorded = Arc<Mutex<Option<Vec<UninstallEntry>>>>;

fn writable() -> Recorded {
    Arc::new(Mutex::new(Some(Vec::new())))
}

/// The window on the applications page, on the old versions' tab; the
/// tab's seams `seams`, the history lines written kept in `recorded`.
fn on_page(seams: OldVersionSeams, recorded: Recorded) -> (ui::AppWindow, crate::apps::AppsScreen) {
    let cleaner = seams_never_scanning();
    let seams = crate::cleaner::Seams {
        apps: AppsSeams {
            read: Rc::new(|_, sink, _| {
                for arrival in crate::apps::tests::arrivals() {
                    sink.send(arrival).unwrap();
                }
            }),
            old_versions: seams,
            record: Arc::new(move |entry| {
                let mut recorded = recorded.lock().unwrap();
                let lines = recorded.as_mut().ok_or(HistoryUnavailable::NoDirectory)?;
                lines.push(entry.clone());
                Ok(())
            }),
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
    settled(&window);
    window
        .global::<ui::Apps>()
        .set_tab(ui::AppsTab::OldVersions);
    (window, screen)
}

/// The snaps' folder of a test: Android Studio's 248 alone, its 249 shared
/// with the download cache, twice as large; core22's not there.
fn snap_files() -> tempfile::TempDir {
    let files = tempfile::tempdir().unwrap();
    std::fs::write(files.path().join("android-studio_248.snap"), [1; 8192]).unwrap();
    let cached = files.path().join("android-studio_249.snap");
    std::fs::write(&cached, [1; 16384]).unwrap();
    std::fs::hard_link(&cached, files.path().join("cache-entry")).unwrap();
    files
}

/// Whether the tests cannot see the system as it is (a container): the
/// library then proposes nothing at all, and the tests that need a version
/// say so and pass over.
fn confined() -> bool {
    let confined = okapi_platform::confinement().is_some();
    if confined {
        eprintln!("confined: no old version can be found here, the test passes over");
    }
    confined
}

/// Removes what is checked, once confirmed, and waits for the end.
fn remove(window: &ui::AppWindow) {
    tab(window).invoke_retire();
    tab(window).invoke_confirm();
    settled(window);
}

#[test]
fn the_old_versions_are_found_with_the_list_none_checked_and_their_room_told() {
    // Catches a search that waits for the tab, a version checked
    // beforehand (invariant 5), and the room of a file the cache shares
    // told freed at once.
    if confined() {
        return;
    }
    let files = snap_files();
    let (window, _) = on_page(
        Fake::new(files.path(), Retired::Removed).seams(),
        writable(),
    );
    let lines: Vec<(String, String, ui::OldRoom, bool)> = tab(&window)
        .get_snaps()
        .iter()
        .map(|line| {
            (
                line.name.to_string(),
                line.revision.to_string(),
                line.room,
                line.checked,
            )
        })
        .collect();
    let line =
        |name: &str, revision: &str, room| (name.to_owned(), revision.to_owned(), room, false);
    assert_eq!(
        lines,
        [
            line("Android Studio", "248", ui::OldRoom::Now),
            line("Android Studio", "249", ui::OldRoom::Later),
            line("core22", "2987", ui::OldRoom::Unknown),
        ]
    );
    assert_eq!(tab(&window).get_flatpaks().row_count(), 0);
    assert_eq!(tab(&window).get_checked_count(), 0);
    assert!(tab(&window).get_has_total(), "the badge says the room");
    // Its revision read with its line, written below its name silent.
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&window, "OldRow")
            .filter_map(|line| line.accessible_label())
            .any(|label| label.contains("Révision 248"))
    );
    assert!(!shows_a_text_starting(&window, "Révision"));
}

#[test]
fn nothing_goes_unchecked_and_what_is_checked_goes_once_confirmed_each_in_the_history() {
    // Catches a removal without its yes, identifiers not those checked, the
    // two rooms swapped in the question, a version gone without its history
    // line or named by another's rank, boxes left checked after it, and the
    // tab not looked at again.
    if confined() {
        return;
    }
    let files = snap_files();
    let fake = Fake::new(files.path(), Retired::Removed);
    let recorded = writable();
    let (window, _) = on_page(fake.seams(), Arc::clone(&recorded));
    tab(&window).invoke_retire();
    assert!(
        !tab(&window).get_confirming(),
        "nothing checked: nothing asked"
    );
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_toggle(1);
    tab(&window).invoke_toggle(2);
    assert_eq!(tab(&window).get_checked_count(), 3);
    tab(&window).invoke_retire();
    assert!(tab(&window).get_confirming());
    let going: Vec<(String, String)> = tab(&window)
        .get_going()
        .iter()
        .map(|line| (line.name.to_string(), line.version.to_string()))
        .collect();
    let named = |name: &str, version: &str| (name.to_owned(), version.to_owned());
    assert_eq!(
        going,
        [
            named("Android Studio", "v248"),
            named("Android Studio", "v249"),
            named("core22", "v2987"),
        ],
        "each named"
    );
    assert_eq!(tab(&window).get_checked_now(), crate::shown::size(8192));
    assert_eq!(tab(&window).get_checked_later(), crate::shown::size(16384));
    tab(&window).invoke_cancel();
    assert!(!tab(&window).get_confirming());
    assert!(
        fake.asked.lock().unwrap().is_empty(),
        "cancelled: nothing asked"
    );
    let searches = fake.searches();
    remove(&window);
    let asked = fake.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].len(), 3);
    let recorded = recorded.lock().unwrap().clone().unwrap();
    let [alone, cached, unknown] = recorded.as_slice() else {
        panic!("one line each: {recorded:?}");
    };
    assert_eq!(alone.name, "Android Studio v248");
    assert_eq!(alone.packages, ["android-studio 248"]);
    assert_eq!(alone.part, UninstallPart::OldVersion);
    assert!(alone.data_removed, "its data of that version went");
    assert!(
        alone.freed_bytes.is_some_and(|bytes| bytes > 0) && alone.later_bytes.is_none(),
        "freed at once"
    );
    assert_eq!(
        cached.freed_bytes,
        Some(0),
        "the cache holds it: nothing freed yet"
    );
    assert!(cached.later_bytes.is_some(), "given back later");
    assert_eq!(unknown.name, "core22 v2987");
    assert_eq!(unknown.freed_bytes, None, "its room unknown");
    assert_eq!(tab(&window).get_outcome(), ui::OldOutcome::Done);
    assert_eq!(tab(&window).get_removed(), 3);
    assert!(shows(&window, "3 éléments retirés."));
    assert_eq!(tab(&window).get_checked_count(), 0, "nothing checked after");
    assert!(
        tab(&window).get_snaps().iter().all(|line| !line.checked),
        "no line checked after"
    );
    assert!(fake.searches() > searches, "the tab looked at again");
}

#[test]
fn a_search_never_comes_under_what_is_checked() {
    // Catches a new report put under the boxes the user ticked: they would
    // name other versions, or be lost.
    if confined() {
        return;
    }
    let files = snap_files();
    let fake = Fake::new(files.path(), Retired::Removed);
    let (window, _) = on_page(fake.seams(), writable());
    tab(&window).invoke_toggle(1);
    let searches = fake.searches();
    window.global::<ui::Apps>().invoke_refresh();
    for _ in 0..10 {
        read_once();
    }
    assert_eq!(fake.searches(), searches, "held while a box is checked");
    assert_eq!(tab(&window).get_checked_count(), 1);
    assert!(tab(&window).get_snaps().row_data(1).unwrap().checked);
    tab(&window).invoke_toggle(1);
    read_until(&window, |_| fake.searches() > searches);
}

#[test]
fn while_old_versions_go_nothing_else_on_the_screen_starts() {
    // Catches a removal of old versions beside a move of leftovers, a
    // forgetting or an uninstallation: two windows of polkit, two daemons
    // asked at once.
    if confined() {
        return;
    }
    let files = snap_files();
    let (release, waiting) = mpsc::channel();
    let fake = Fake {
        held: Some(Arc::new(Mutex::new(waiting))),
        ..Fake::new(files.path(), Retired::Removed)
    };
    let (window, _) = on_page(fake.seams(), writable());
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_retire();
    tab(&window).invoke_confirm();
    read_until(&window, |window| tab(window).get_retiring());
    assert!(window.global::<ui::Leftovers>().get_busy());
    assert!(window.global::<ui::Apps>().get_busy());
    assert!(tab(&window).get_busy());
    release.send(()).unwrap();
    settled(&window);
    assert!(!window.global::<ui::Leftovers>().get_busy());
}

#[test]
fn the_rights_refused_say_so_and_write_nothing() {
    // Catches the ones not asked after the refusal counted as failures of
    // their own: the outcome would no longer say the rights.
    if confined() {
        return;
    }
    let files = snap_files();
    let recorded = writable();
    let fake = Fake {
        by_rank: vec![Retired::Failed(Why::NotAuthorized), Retired::NotAsked],
        ..Fake::new(files.path(), Retired::Removed)
    };
    let (window, _) = on_page(fake.seams(), Arc::clone(&recorded));
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_toggle(1);
    remove(&window);
    assert_eq!(tab(&window).get_outcome(), ui::OldOutcome::Refused);
    assert_eq!(recorded.lock().unwrap().as_deref(), Some(&[][..]));
    assert!(shows(
        &window,
        "Rien n'a été retiré : les droits n'ont pas été donnés."
    ));
}

#[test]
fn a_history_line_lost_or_a_room_withheld_is_said() {
    // Catches a removal said whole when its history line could not be
    // written, and runtimes' room said given back when flatpak did not
    // prune its repository.
    if confined() {
        return;
    }
    let files = snap_files();
    let fake = Fake {
        withheld: true,
        ..Fake::new(files.path(), Retired::Removed)
    };
    let (window, _) = on_page(fake.seams(), Arc::new(Mutex::new(None)));
    tab(&window).invoke_toggle(0);
    remove(&window);
    assert!(tab(&window).get_unrecorded());
    assert!(tab(&window).get_unpruned());
    assert!(shows(
        &window,
        "Un élément retiré. Flatpak n'a pas encore rendu la place des environnements. L'historique n'a pas pu tout garder."
    ));
}

#[test]
fn nothing_found_says_so() {
    let (window, _) = on_page(inert(), writable());
    assert!(tab(&window).get_searched());
    assert!(shows(&window, "Aucune ancienne version à retirer"));
    assert!(!tab(&window).get_has_total());
}

#[test]
fn closing_waits_a_little_for_a_removal_under_way() {
    // Catches a window closed on a removal at once: the version in a
    // daemon's hands would leave no history line.
    if confined() {
        return;
    }
    let files = snap_files();
    let (release, waiting) = mpsc::channel();
    let fake = Fake {
        held: Some(Arc::new(Mutex::new(waiting))),
        ..Fake::new(files.path(), Retired::Removed)
    };
    let recorded = writable();
    let (window, screen) = on_page(fake.seams(), Arc::clone(&recorded));
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_retire();
    tab(&window).invoke_confirm();
    assert!(tab(&window).get_retiring());
    let later = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        release.send(()).unwrap();
    });
    let started = Instant::now();
    screen.shut_down();
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert_eq!(
        recorded.lock().unwrap().as_ref().map(Vec::len),
        Some(1),
        "its history line written"
    );
    later.join().unwrap();
}

/// The rank of `name` among the applications listed.
fn line_of_app(window: &ui::AppWindow, name: &str) -> i32 {
    let rank = window
        .global::<ui::Apps>()
        .get_lines()
        .iter()
        .position(|line| line.name == name)
        .unwrap();
    i32::try_from(rank).unwrap()
}

#[test]
fn an_uninstallation_and_a_removal_of_old_versions_never_run_together() {
    // Catches, both ways, two daemons asked at once and two windows of
    // polkit: an uninstallation started while old versions go, and old
    // versions asked while an uninstallation is under way.
    if confined() {
        return;
    }
    let files = snap_files();
    let (release, waiting) = mpsc::channel();
    let fake = Fake {
        held: Some(Arc::new(Mutex::new(waiting))),
        ..Fake::new(files.path(), Retired::Removed)
    };
    let (window, _) = on_page(fake.seams(), writable());
    let apps = window.global::<ui::Apps>();
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_retire();
    tab(&window).invoke_confirm();
    read_until(&window, |window| tab(window).get_retiring());
    apps.invoke_choose(line_of_app(&window, "Android Studio"));
    apps.invoke_uninstall();
    assert_eq!(
        apps.get_step(),
        ui::UninstallStep::Closed,
        "refused while old versions go"
    );
    release.send(()).unwrap();
    settled(&window);

    apps.invoke_choose(line_of_app(&window, "Android Studio"));
    apps.invoke_uninstall();
    assert_ne!(apps.get_step(), ui::UninstallStep::Closed);
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_retire();
    assert!(
        !tab(&window).get_confirming(),
        "refused while an uninstallation is open"
    );
}

#[test]
fn what_went_what_was_kept_and_what_was_gone_already_are_told_apart() {
    // Catches a version gone before the asking counted removed (and
    // written in the history), one not asked counted otherwise than for
    // the rights, and everything gone already told as a failure.
    if confined() {
        return;
    }
    let files = snap_files();
    let fake = Fake {
        by_rank: vec![Retired::Removed, Retired::Gone, Retired::NotAsked],
        ..Fake::new(files.path(), Retired::Removed)
    };
    let recorded = writable();
    let (window, _) = on_page(fake.seams(), Arc::clone(&recorded));
    for index in 0..3 {
        tab(&window).invoke_toggle(index);
    }
    remove(&window);
    assert_eq!(tab(&window).get_outcome(), ui::OldOutcome::Partly);
    assert_eq!(tab(&window).get_removed(), 1);
    assert_eq!(tab(&window).get_kept(), 1);
    assert_eq!(recorded.lock().unwrap().as_ref().map(Vec::len), Some(1));
}

#[test]
fn everything_gone_already_is_no_failure() {
    // Catches versions gone before the asking (snapd's own refresh) told as
    // a removal that failed.
    if confined() {
        return;
    }
    let files = snap_files();
    let fake = Fake::new(files.path(), Retired::Gone);
    let (window, _) = on_page(fake.seams(), writable());
    tab(&window).invoke_toggle(0);
    remove(&window);
    assert_eq!(
        tab(&window).get_outcome(),
        ui::OldOutcome::None,
        "gone already: nothing to say but the list read again"
    );
}
