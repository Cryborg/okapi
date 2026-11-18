//! The tab of old versions in a window: the search is the library's own on
//! snapd's list given by hand (its report cannot be built elsewhere), the
//! removal a fake that tells what the test says.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use okapi_rules::Language;
use okapi_tools::apps::leftovers::InstalledPlaces;
use okapi_tools::apps::old_versions::find;
use okapi_tools::packages::snapd::SnapdError;
use serde_json::Value;
use slint::Model as _;

use super::*;
use crate::apps::{AppsSeams, READ_EVERY};
use crate::cleaner::tests::{button, seams_never_scanning, shows};

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
        retire: Arc::new(|_, _, _| {}),
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

/// The removals asked, by their identifiers.
type Asked = Arc<Mutex<Vec<Vec<OldVersionId>>>>;

/// A search over [`listed`], the snaps' files in `files`; a removal that
/// tells each asked as `answer` says.
fn seams_of(files: PathBuf, asked: Asked, answer: Retired) -> OldVersionSeams {
    OldVersionSeams {
        find: Arc::new(move |stop| {
            find(
                Ok(listed()),
                InstalledPlaces::new(files.clone(), Vec::new(), None),
                Path::new("/nonexistent"),
                stop,
            )
        }),
        retire: Arc::new(move |report, ids, told| {
            asked.lock().unwrap().push(ids.to_vec());
            for id in ids {
                told(report.find(*id).unwrap(), &answer);
            }
        }),
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

/// The window on the applications page; the tab's seams `seams`, the
/// history lines written kept in `recorded`.
fn on_page(
    seams: OldVersionSeams,
    recorded: Arc<Mutex<Vec<UninstallEntry>>>,
) -> (ui::AppWindow, crate::apps::AppsScreen) {
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
                recorded.lock().unwrap().push(entry.clone());
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
/// with the download cache; core22's not there.
fn snap_files() -> tempfile::TempDir {
    let files = tempfile::tempdir().unwrap();
    std::fs::write(files.path().join("android-studio_248.snap"), [1; 8192]).unwrap();
    let cached = files.path().join("android-studio_249.snap");
    std::fs::write(&cached, [1; 8192]).unwrap();
    std::fs::hard_link(&cached, files.path().join("cache-entry")).unwrap();
    files
}

/// Whether the tests cannot see the system as it is (a container): the
/// library then proposes nothing at all.
fn confined() -> bool {
    okapi_platform::confinement().is_some()
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
        seams_of(files.path().to_path_buf(), Arc::default(), Retired::Removed),
        Arc::default(),
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
    assert!(shows(
        &window,
        "Rien de coché. Cochez ce que vous voulez retirer."
    ));
}

#[test]
fn nothing_goes_unchecked_and_what_is_checked_goes_once_confirmed_each_in_the_history() {
    // Catches a removal without its yes, identifiers not those checked, the
    // two rooms not told apart in the question, a version gone without its
    // history line, and the tab not looked at again.
    if confined() {
        return;
    }
    let files = snap_files();
    let asked = Asked::default();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let (window, _) = on_page(
        seams_of(
            files.path().to_path_buf(),
            Arc::clone(&asked),
            Retired::Removed,
        ),
        Arc::clone(&recorded),
    );
    tab(&window).invoke_retire();
    assert!(
        !tab(&window).get_confirming(),
        "nothing checked: nothing asked"
    );
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_toggle(1);
    assert_eq!(tab(&window).get_checked_count(), 2);
    tab(&window).invoke_retire();
    assert!(tab(&window).get_confirming());
    assert_eq!(tab(&window).get_going().row_count(), 2, "each named");
    assert!(tab(&window).get_checked_snaps());
    assert!(tab(&window).get_has_later());
    assert!(tab(&window).get_checked_now().whole > 0);
    tab(&window).invoke_cancel();
    assert!(!tab(&window).get_confirming());
    assert!(asked.lock().unwrap().is_empty(), "cancelled: nothing asked");
    tab(&window).invoke_retire();
    tab(&window).invoke_confirm();
    settled(&window);
    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].len(), 2);
    let recorded = recorded.lock().unwrap().clone();
    let [alone, cached] = recorded.as_slice() else {
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
    assert_eq!(tab(&window).get_outcome(), ui::OldOutcome::Done);
    assert_eq!(tab(&window).get_removed(), 2);
    assert!(shows(&window, "2 anciennes versions retirées."));
}

#[test]
fn the_rights_refused_say_so_and_write_nothing() {
    if confined() {
        return;
    }
    let files = snap_files();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let (window, _) = on_page(
        seams_of(
            files.path().to_path_buf(),
            Arc::default(),
            Retired::Failed(Why::NotAuthorized),
        ),
        Arc::clone(&recorded),
    );
    tab(&window).invoke_toggle(0);
    tab(&window).invoke_retire();
    tab(&window).invoke_confirm();
    settled(&window);
    assert_eq!(tab(&window).get_outcome(), ui::OldOutcome::Refused);
    assert!(recorded.lock().unwrap().is_empty());
    assert!(shows(
        &window,
        "Rien n'a été retiré : les droits n'ont pas été donnés."
    ));
}

#[test]
fn nothing_found_says_so() {
    let (window, _) = on_page(inert(), Arc::default());
    assert!(tab(&window).get_searched());
    assert!(shows(&window, "Aucune ancienne version à retirer"));
    assert!(!tab(&window).get_has_total());
}
