//! The duplicates screen in a window: a sandbox's home folder searched by
//! the real engine, its copies checked and moved to a sandbox's trash, the
//! file manager and the trash through stand-ins.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use jiff::tz::TimeZone;
use okapi_engine::Exclusions;
use okapi_platform::{PersonalFolder, Trash};

use okapi_platform::PersonalKind;
use slint::Model as _;

use super::*;
use crate::cleaner::tests::{button, defaults, press, seams_never_scanning, shows};
use crate::cleaner::{Problem, Seams};
use crate::dashboard::tests::inert;
use crate::user_files::ReadFolders;

/// How long a test waits for the screen: a fault fails, it never hangs.
const WAIT: Duration = Duration::from_secs(20);
/// A size above the screen's smallest file.
const LARGE: usize = 1_048_576 + 4096;

/// `LARGE` bytes of a content of its own, by `seed`.
fn content(seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..LARGE)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

/// A home folder with its personal folders.
struct Sandbox {
    home: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        for folder in ["Documents", "Images", "Téléchargements"] {
            fs::create_dir(home.path().join(folder)).unwrap();
        }
        Self { home }
    }

    fn root(&self) -> PathBuf {
        self.home.path().canonicalize().unwrap()
    }

    /// Writes the content `seed` at `path`, below the home folder, its
    /// change date `days` after the clock's origin.
    fn write(&self, path: &str, seed: u64, days: u64) -> PathBuf {
        let path = self.root().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content(seed)).unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(days * 86_400))
            .unwrap();
        path
    }

    /// The trash of the sandbox: in a hidden folder, never searched.
    fn data(&self) -> PathBuf {
        self.root().join(".local/share")
    }
}

/// What the stand-ins were asked.
#[derive(Default)]
struct Asked {
    opened: RefCell<Vec<OpenRequest>>,
}

/// How the file manager answers, held until the test gives it.
type Answer = Box<dyn FnOnce(bool)>;

/// The settings' exclusions the screen reads, or a problem.
type SettingsExclusions = Rc<RefCell<Result<Vec<PathBuf>, Problem>>>;

/// Seams of a screen on `sandbox`, its trash held until `release` says when
/// it is given.
fn seams(
    sandbox: &Sandbox,
    asked: &Rc<Asked>,
    exclusions: &SettingsExclusions,
    release: Option<Arc<Mutex<std::sync::mpsc::Receiver<()>>>>,
) -> Seams {
    let data = sandbox.data();
    fs::create_dir_all(&data).unwrap();
    let asked = Rc::clone(asked);
    let read = Rc::clone(exclusions);
    Seams {
        read: Rc::new(move || {
            let exclusions = read.borrow().clone()?;
            Ok(crate::cleaner::Inputs {
                exclusions,
                ..defaults()
            })
        }),
        duplicates: DuplicateSeams {
            search: SearchSeams {
                home: Some(sandbox.root()),
                folders: Arc::new(|home| {
                    Ok([
                        (PersonalKind::Documents, "Documents"),
                        (PersonalKind::Pictures, "Images"),
                        (PersonalKind::Music, "Musique"),
                        (PersonalKind::Download, "Téléchargements"),
                    ]
                    .into_iter()
                    .map(|(kind, name)| PersonalFolder {
                        kind,
                        path: home.join(name),
                    })
                    .collect())
                }),
                trash: Arc::new(move || {
                    if let Some(release) = &release {
                        release.lock().unwrap().recv().ok();
                    }
                    Trash::in_folder(&data)
                }),
                open: Rc::new(move |request, done| {
                    asked.opened.borrow_mut().push(request.clone());
                    done(true);
                }),
                now: Timestamp::now,
                zone: TimeZone::UTC,
            },
            threads: Arc::new(|_| NonZeroUsize::MIN),
        },
        ..seams_never_scanning()
    }
}

fn no_exclusion() -> SettingsExclusions {
    Rc::new(RefCell::new(Ok(Vec::new())))
}

fn duplicates(window: &ui::AppWindow) -> ui::Duplicates<'_> {
    window.global::<ui::Duplicates>()
}

fn read_once() {
    i_slint_backend_testing::mock_elapsed_time(READ_EVERY);
    slint::platform::update_timers_and_animations();
}

/// The screen's readings, until `done`.
fn read_until(window: &ui::AppWindow, done: impl Fn(&ui::AppWindow) -> bool) {
    let started = Instant::now();
    while !done(window) {
        assert!(
            started.elapsed() < WAIT,
            "never done: {:?}, {:?}",
            duplicates(window).get_step(),
            duplicates(window).get_phase()
        );
        read_once();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The window on the duplicates screen, its folders read.
fn on_duplicates(seams: Seams) -> ui::AppWindow {
    on_duplicates_screen(seams).0
}

/// The same, with the screen.
fn on_duplicates_screen(seams: Seams) -> (ui::AppWindow, DuplicatesScreen) {
    let crate::Screens {
        window,
        duplicates: screen,
        ..
    } = crate::wide_window(seams, inert(), Language::French);
    button(&window, "Doublons").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    read_until(&window, |window| {
        duplicates(window).get_folders_state() != ui::FoldersState::Reading
    });
    (window, screen)
}

/// Searched, the result shown.
fn searched(window: &ui::AppWindow) {
    duplicates(window).invoke_search();
    read_until(window, |window| {
        duplicates(window).get_step() != ui::ToolStep::Searching
    });
    assert_eq!(duplicates(window).get_step(), ui::ToolStep::Result);
}

/// The confirmation asked, then confirmed.
fn confirmed(window: &ui::AppWindow) {
    duplicates(window).invoke_trash();
    duplicates(window).invoke_confirm();
}

fn rows(window: &ui::AppWindow) -> Vec<ui::DuplicateRow> {
    duplicates(window).get_rows().iter().collect()
}

/// The copy whose path ends with `end`, and its line.
fn copy_row(window: &ui::AppWindow, end: &str) -> (usize, ui::DuplicateRow) {
    rows(window)
        .into_iter()
        .enumerate()
        .find(|(_, row)| !row.is_head && row.full_path.ends_with(end))
        .unwrap_or_else(|| panic!("no copy {end}"))
}

fn index(at: usize) -> i32 {
    i32::try_from(at).unwrap()
}

#[test]
fn the_start_offers_the_personal_folders_that_exist_and_the_whole_home() {
    let sandbox = Sandbox::new();
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    let names: Vec<String> = duplicates(&window)
        .get_folders()
        .iter()
        .map(|folder| folder.name.to_string())
        .collect();
    // Musique does not exist: not offered.
    assert_eq!(names, ["Documents", "Images", "Téléchargements"]);
    assert_eq!(
        duplicates(&window).get_folders_state(),
        ui::FoldersState::Read
    );
    assert_eq!(duplicates(&window).get_chosen_folders(), 3);
    duplicates(&window).invoke_toggle_folder(1);
    assert_eq!(duplicates(&window).get_chosen_folders(), 2);
    assert!(
        !duplicates(&window)
            .get_folders()
            .row_data(1)
            .unwrap()
            .checked
    );
    assert!(shows(&window, "Tout le dossier personnel"));
}

#[test]
fn a_result_is_checked_by_the_rule_and_its_last_copy_never_is() {
    let sandbox = Sandbox::new();
    sandbox.write("Téléchargements/photo.jpg", 1, 1);
    sandbox.write("Documents/photo.jpg", 1, 5);
    sandbox.write("Documents/unique.jpg", 2, 5);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    assert_eq!(duplicates(&window).get_groups(), 1);
    let (_, downloaded) = copy_row(&window, "Téléchargements/photo.jpg");
    let (kept_at, kept) = copy_row(&window, "Documents/photo.jpg");
    assert!(downloaded.checked);
    assert_eq!(downloaded.full_path, "~/Téléchargements/photo.jpg");
    assert!(!kept.checked && kept.last && !kept.checkable);
    assert_eq!(kept.why, ui::KeptWhy::Outside);
    assert_eq!(kept.why_first, "Téléchargements");
    assert_eq!(duplicates(&window).get_chosen_files(), 1);
    // The last copy left unchecked cannot be checked.
    duplicates(&window).invoke_toggle(index(kept_at));
    assert!(!copy_row(&window, "Documents/photo.jpg").1.checked);
    let (downloaded_at, _) = copy_row(&window, "Téléchargements/photo.jpg");
    duplicates(&window).invoke_toggle(index(downloaded_at));
    assert_eq!(duplicates(&window).get_chosen_files(), 0);
    // A quick selection keeps the oldest: Downloads' copy, then.
    duplicates(&window).invoke_quick(ui::QuickRule::Oldest, -1);
    assert!(copy_row(&window, "Documents/photo.jpg").1.checked);
    let (_, oldest) = copy_row(&window, "Téléchargements/photo.jpg");
    assert!(!oldest.checked);
    assert_eq!(oldest.why, ui::KeptWhy::Oldest);
}

#[test]
fn the_whole_home_checks_nothing_outside_the_personal_folders_and_never_offers_what_is_excluded() {
    let sandbox = Sandbox::new();
    sandbox.write("Android/Sdk/a/lib.so", 3, 1);
    sandbox.write("Android/Sdk/b/lib.so", 3, 2);
    sandbox.write("Documents/note.pdf", 4, 1);
    sandbox.write("Android/note.pdf", 4, 1);
    sandbox.write("snap/app/data.bin", 5, 1);
    sandbox.write("Images/data.bin", 5, 2);
    sandbox.write("Excluded/data.bin", 5, 3);
    let exclusions = Rc::new(RefCell::new(Ok(vec![sandbox.root().join("Excluded")])));
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &exclusions, None));
    duplicates(&window).set_whole_home(true);
    searched(&window);
    assert!(
        rows(&window)
            .iter()
            .all(|row| !row.full_path.contains("snap/") && !row.full_path.contains("Excluded/"))
    );
    for end in ["Sdk/a/lib.so", "Sdk/b/lib.so", "Android/note.pdf"] {
        let (_, row) = copy_row(&window, end);
        assert!(!row.checked, "{end}");
        assert_eq!(row.why, ui::KeptWhy::OutsidePersonal, "{end}");
    }
    // The copy of Documents is the only one of personal folders: kept.
    assert!(!copy_row(&window, "Documents/note.pdf").1.checked);
    assert_eq!(duplicates(&window).get_chosen_files(), 0);
}

#[test]
fn a_search_stopped_while_opening_keeps_nothing() {
    let sandbox = Sandbox::new();
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    duplicates(&window).invoke_search();
    assert_eq!(duplicates(&window).get_phase(), ui::SearchPhase::Opening);
    duplicates(&window).invoke_stop();
    assert_eq!(duplicates(&window).get_phase(), ui::SearchPhase::Stopped);
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Searching);
    assert_eq!(rows(&window), []);
}

#[test]
fn settings_that_cannot_be_read_start_nothing() {
    let sandbox = Sandbox::new();
    let exclusions = Rc::new(RefCell::new(Err(Problem::SettingsUnsaved)));
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &exclusions, None));
    duplicates(&window).invoke_search();
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Start);
    assert_eq!(duplicates(&window).get_refused(), ui::ToolRefusal::Unsaved);
}

#[test]
fn confirmed_copies_go_to_the_trash_but_one_changed_since() {
    let sandbox = Sandbox::new();
    let first = sandbox.write("Téléchargements/a.mov", 6, 1);
    sandbox.write("Documents/a.mov", 6, 5);
    let changed = sandbox.write("Téléchargements/b.mov", 7, 1);
    sandbox.write("Documents/b.mov", 7, 5);
    let exclusions = no_exclusion();
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &exclusions, None));
    searched(&window);
    assert_eq!(duplicates(&window).get_chosen_files(), 2);
    // Asked, then cancelled: nothing moves.
    duplicates(&window).invoke_trash();
    assert!(duplicates(&window).get_confirming());
    duplicates(&window).invoke_cancel_confirmation();
    assert!(!duplicates(&window).get_confirming());
    assert!(first.exists());
    // Written again since the search: it stays.
    std::thread::sleep(Duration::from_millis(30));
    fs::write(&changed, content(7)).unwrap();
    duplicates(&window).invoke_trash();
    duplicates(&window).invoke_confirm();
    read_until(&window, |window| {
        duplicates(window).get_step() == ui::ToolStep::Receipt
    });
    assert!(!first.exists());
    assert!(sandbox.data().join("Trash/files/a.mov").exists());
    assert_eq!(duplicates(&window).get_moved_files(), 1);
    assert_eq!(
        duplicates(&window).get_moved(),
        size(u64::try_from(LARGE).unwrap())
    );
    assert!(changed.exists());
    assert!(sandbox.root().join("Documents/a.mov").exists());
    assert_eq!(duplicates(&window).get_trashed_files(), 1);
    let kept: Vec<ui::KeptShown> = duplicates(&window).get_kept().iter().collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].name, "b.mov");
    assert_eq!(kept[0].why, ui::ReceiptWhy::Changed);
}

#[test]
fn while_copies_move_nothing_else_can_be_done() {
    let sandbox = Sandbox::new();
    sandbox.write("Téléchargements/a.iso", 8, 1);
    sandbox.write("Documents/a.iso", 8, 5);
    let (release, held): (Sender<()>, _) = mpsc::channel();
    // The start opens the trash too: let it.
    release.send(()).unwrap();
    let window = on_duplicates(seams(
        &sandbox,
        &Rc::default(),
        &no_exclusion(),
        Some(Arc::new(Mutex::new(held))),
    ));
    searched(&window);
    confirmed(&window);
    assert!(duplicates(&window).get_trashing());
    duplicates(&window).invoke_again();
    duplicates(&window).invoke_change();
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Result);
    // What goes is taken: the choices stay as they were.
    let (at, _) = copy_row(&window, "Téléchargements/a.iso");
    duplicates(&window).invoke_toggle(index(at));
    duplicates(&window).invoke_deselect_all();
    duplicates(&window).invoke_quick(ui::QuickRule::Newest, -1);
    assert!(copy_row(&window, "Téléchargements/a.iso").1.checked);
    assert_eq!(duplicates(&window).get_chosen_files(), 1);
    duplicates(&window).invoke_trash();
    assert!(!duplicates(&window).get_confirming());
    release.send(()).unwrap();
    read_until(&window, |window| {
        duplicates(window).get_step() == ui::ToolStep::Receipt
    });
    assert!(!duplicates(&window).get_trashing());
}

#[test]
fn a_copy_is_shown_in_its_folder_as_the_search_read_it() {
    let sandbox = Sandbox::new();
    let path = sandbox.write("Téléchargements/c.png", 9, 1);
    sandbox.write("Documents/c.png", 9, 5);
    let asked = Rc::new(Asked::default());
    let window = on_duplicates(seams(&sandbox, &asked, &no_exclusion(), None));
    searched(&window);
    let (at, _) = copy_row(&window, "Téléchargements/c.png");
    duplicates(&window).invoke_open(index(at));
    let opened = asked.opened.borrow();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].path, path);
    assert!(!opened[0].folder);
    let identity = okapi_platform::file_identity(&fs::symlink_metadata(&path).unwrap());
    assert_eq!(opened[0].expected, identity);
    // A group's header shows nothing.
    duplicates(&window).invoke_open(0);
    assert_eq!(opened.len(), 1);
}

#[test]
fn a_file_manager_refusing_at_once_is_said() {
    let sandbox = Sandbox::new();
    sandbox.write("Téléchargements/e.png", 11, 1);
    sandbox.write("Documents/e.png", 11, 5);
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    seams.duplicates.search.open = Rc::new(|_, done| done(false));
    let window = on_duplicates(seams);
    searched(&window);
    let (at, _) = copy_row(&window, "Documents/e.png");
    duplicates(&window).set_current(index(at));
    duplicates(&window).invoke_open(index(at));
    assert!(!duplicates(&window).get_opening());
    assert!(duplicates(&window).get_open_refused());
}

#[test]
fn the_whole_home_reaches_a_personal_folder_the_home_walk_leaves() {
    let sandbox = Sandbox::new();
    // A project: the walk of the home folder never enters it.
    fs::create_dir_all(sandbox.root().join("Documents/.git")).unwrap();
    sandbox.write("Documents/f.ogg", 12, 1);
    sandbox.write("Images/f.ogg", 12, 5);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    duplicates(&window).set_whole_home(true);
    searched(&window);
    assert_eq!(duplicates(&window).get_groups(), 1);
    assert!(copy_row(&window, "Images/f.ogg").1.checked);
}

#[test]
fn a_group_folds_and_unfolds() {
    let sandbox = Sandbox::new();
    sandbox.write("Téléchargements/d.zip", 10, 1);
    sandbox.write("Documents/d.zip", 10, 5);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    assert_eq!(rows(&window).len(), 3);
    duplicates(&window).invoke_fold(0, false);
    assert_eq!(rows(&window).len(), 1);
    assert!(!rows(&window)[0].open);
    duplicates(&window).invoke_fold(0, true);
    assert_eq!(rows(&window).len(), 3);
    assert_eq!(rows(&window)[2].head, 0);
}

#[test]
fn a_copy_s_path_is_said_from_the_home_folder_and_cut_at_its_start() {
    let sandbox = Sandbox::new();
    sandbox.write("Téléchargements/2026/Vacances/plage.jpg", 13, 1);
    sandbox.write("Documents/plage.jpg", 13, 5);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    let (_, row) = copy_row(&window, "Vacances/plage.jpg");
    assert_eq!(row.full_path, "~/Téléchargements/2026/Vacances/plage.jpg");
    assert_eq!(row.folder, "~/Téléchargements/2026/Vacances");
    duplicates(&window).invoke_path_room(25);
    let (_, row) = copy_row(&window, "Vacances/plage.jpg");
    assert_eq!(row.path, "…/2026/Vacances/plage.jpg");
    assert_eq!(row.full_path, "~/Téléchargements/2026/Vacances/plage.jpg");
}

#[test]
fn unchecked_folders_are_never_searched() {
    let sandbox = Sandbox::new();
    sandbox.write("Documents/x.bin", 14, 1);
    sandbox.write("Images/x.bin", 14, 2);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    duplicates(&window).invoke_toggle_folder(1);
    searched(&window);
    assert_eq!(duplicates(&window).get_groups(), 0);
    assert_eq!(
        duplicates(&window).get_searched_names(),
        "Documents, Téléchargements"
    );
    duplicates(&window).invoke_change();
    read_until(&window, |window| {
        duplicates(window).get_folders_state() != ui::FoldersState::Reading
    });
    for index in [0, 2] {
        duplicates(&window).invoke_toggle_folder(index);
    }
    assert_eq!(duplicates(&window).get_chosen_folders(), 0);
    duplicates(&window).invoke_search();
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Start);
    assert_eq!(duplicates(&window).get_refused(), ui::ToolRefusal::Folder);
}

#[test]
fn keeping_the_copies_of_a_folder_takes_it_from_the_folders_offered() {
    let sandbox = Sandbox::new();
    sandbox.write("Images/q.png", 15, 1);
    sandbox.write("Téléchargements/q.png", 15, 2);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    let offered: Vec<String> = duplicates(&window)
        .get_quick_folders()
        .iter()
        .map(|name| name.to_string())
        .collect();
    // Documents holds no copy: not offered, and the second is Downloads.
    assert_eq!(offered, ["Images", "Téléchargements"]);
    duplicates(&window).invoke_quick(ui::QuickRule::Folder, 1);
    assert!(copy_row(&window, "Images/q.png").1.checked);
    let (_, kept) = copy_row(&window, "Téléchargements/q.png");
    assert!(!kept.checked);
    assert_eq!(kept.why, ui::KeptWhy::InFolder);
    assert_eq!(kept.why_first, "Téléchargements");
    // None offered there: nothing changes.
    duplicates(&window).invoke_quick(ui::QuickRule::Folder, 9);
    assert!(!copy_row(&window, "Téléchargements/q.png").1.checked);
    duplicates(&window).invoke_deselect_all();
    assert_eq!(duplicates(&window).get_chosen_files(), 0);
    assert_eq!(
        copy_row(&window, "Téléchargements/q.png").1.why,
        ui::KeptWhy::InFolder
    );
}

#[test]
fn a_group_of_three_keeps_its_last_copy_and_one_of_other_names_never_goes() {
    let sandbox = Sandbox::new();
    sandbox.write("Documents/t.wav", 16, 1);
    sandbox.write("Images/t.wav", 16, 2);
    sandbox.write("Téléchargements/t.wav", 16, 3);
    let named = sandbox.write("Images/h.bin", 17, 1);
    sandbox.write("Téléchargements/h.bin", 17, 2);
    // Another name, outside the search: the copy of Images frees nothing.
    fs::create_dir_all(sandbox.root().join("Android")).unwrap();
    fs::hard_link(&named, sandbox.root().join("Android/h.bin")).unwrap();
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    let (_, linked) = copy_row(&window, "Images/h.bin");
    assert!(linked.other_names && !linked.checked && !linked.checkable);
    assert_eq!(linked.why, ui::KeptWhy::OtherNames);
    assert!(copy_row(&window, "Téléchargements/h.bin").1.checked);
    let (images_at, images) = copy_row(&window, "Images/t.wav");
    assert!(images.checked);
    assert!(
        copy_row(&window, "Documents/t.wav").1.last,
        "the keeper, alone unchecked"
    );
    duplicates(&window).invoke_toggle(index(images_at));
    assert!(!copy_row(&window, "Documents/t.wav").1.last, "two left");
    assert!(!copy_row(&window, "Images/t.wav").1.last);
    duplicates(&window).invoke_toggle(index(images_at));
    let (_, documents) = copy_row(&window, "Documents/t.wav");
    assert!(documents.last && !documents.checkable);
}

#[test]
fn the_lines_of_two_groups_know_where_they_sit() {
    let sandbox = Sandbox::new();
    sandbox.write("Documents/g1", 18, 1);
    sandbox.write("Images/g1", 18, 2);
    sandbox.write("Documents/g2", 19, 1);
    sandbox.write("Images/g2", 19, 2);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    assert_eq!(rows(&window).len(), 6);
    duplicates(&window).invoke_fold(0, false);
    let folded = rows(&window);
    assert_eq!(folded.len(), 4);
    assert!(folded[1].is_head);
    assert_eq!(folded[2].head, 1);
    assert!(rows(&window)[1].open, "the other group unfolded");
}

#[test]
fn the_tree_answers_the_keyboard() {
    use slint::platform::Key;

    let sandbox = Sandbox::new();
    sandbox.write("Documents/k.bin", 20, 1);
    sandbox.write("Images/k.bin", 20, 2);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    // The tree has the focus at the result.
    press(&window, Key::DownArrow);
    press(&window, Key::DownArrow);
    assert_eq!(duplicates(&window).get_current(), 2);
    let checked = rows(&window)[2].checked;
    press(&window, Key::Space);
    assert_ne!(rows(&window)[2].checked, checked, "Space checks a copy");
    press(&window, Key::LeftArrow);
    assert_eq!(duplicates(&window).get_current(), 0, "back to its group");
    press(&window, Key::LeftArrow);
    assert_eq!(rows(&window).len(), 1, "folded");
    press(&window, Key::Return);
    assert_eq!(rows(&window).len(), 3, "unfolded");
    press(&window, Key::End);
    assert_eq!(duplicates(&window).get_current(), 2);
    press(&window, Key::Home);
    assert_eq!(duplicates(&window).get_current(), 0);
}

#[test]
fn the_confirmation_waits_on_cancel_and_escape_closes_it() {
    use slint::platform::Key;

    let sandbox = Sandbox::new();
    let downloaded = sandbox.write("Téléchargements/c.iso", 21, 1);
    sandbox.write("Documents/c.iso", 21, 5);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    duplicates(&window).invoke_trash();
    slint::platform::update_timers_and_animations();
    assert!(duplicates(&window).get_confirming());
    // Two Enter in a row never move a file: the focus is on « Annuler ».
    press(&window, Key::Return);
    slint::platform::update_timers_and_animations();
    assert!(!duplicates(&window).get_confirming());
    assert!(downloaded.exists());
    duplicates(&window).invoke_trash();
    slint::platform::update_timers_and_animations();
    press(&window, Key::Escape);
    slint::platform::update_timers_and_animations();
    assert!(!duplicates(&window).get_confirming());
    assert!(downloaded.exists());
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Result);
    // Closed, the choices are free again. Nothing chosen: nothing to
    // confirm.
    duplicates(&window).invoke_deselect_all();
    assert_eq!(duplicates(&window).get_chosen_files(), 0);
    duplicates(&window).invoke_trash();
    assert!(!duplicates(&window).get_confirming());
}

#[test]
fn a_receipt_where_nothing_went_says_so() {
    let sandbox = Sandbox::new();
    let downloaded = sandbox.write("Téléchargements/z.mkv", 22, 1);
    sandbox.write("Documents/z.mkv", 22, 5);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    std::thread::sleep(Duration::from_millis(30));
    fs::write(&downloaded, content(22)).unwrap();
    confirmed(&window);
    read_until(&window, |window| {
        duplicates(window).get_step() == ui::ToolStep::Receipt
    });
    assert_eq!(duplicates(&window).get_trashed_files(), 0);
    assert_eq!(duplicates(&window).get_moved_files(), 0);
    assert!(shows(&window, "Rien n'a été mis à la corbeille"));
    assert!(!shows(
        &window,
        "La place sera libérée quand vous viderez la corbeille. D'ici là, vous pouvez y reprendre n'importe quel fichier."
    ));
    let kept: Vec<ui::KeptShown> = duplicates(&window).get_kept().iter().collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].folder, "~/Téléchargements");
    // Searched again: the pair is still there.
    duplicates(&window).invoke_again();
    read_until(&window, |window| {
        duplicates(window).get_step() == ui::ToolStep::Result
    });
    assert_eq!(duplicates(&window).get_groups(), 1);
}

#[test]
fn closing_waits_a_little_for_a_move_under_way() {
    let sandbox = Sandbox::new();
    sandbox.write("Téléchargements/w.iso", 23, 1);
    sandbox.write("Documents/w.iso", 23, 5);
    let (release, held): (Sender<()>, _) = mpsc::channel();
    release.send(()).unwrap();
    let (window, screen) = on_duplicates_screen(seams(
        &sandbox,
        &Rc::default(),
        &no_exclusion(),
        Some(Arc::new(Mutex::new(held))),
    ));
    searched(&window);
    confirmed(&window);
    assert!(duplicates(&window).get_trashing());
    let later = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        release.send(()).unwrap();
    });
    let started = Instant::now();
    screen.shut_down();
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(sandbox.data().join("Trash/files/w.iso").exists());
    later.join().unwrap();
}

#[test]
fn the_whole_home_without_a_home_folder_searches_nothing_and_says_why() {
    let sandbox = Sandbox::new();
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    seams.duplicates.search.home = None;
    let window = on_duplicates(seams);
    assert_eq!(
        duplicates(&window).get_folders_state(),
        ui::FoldersState::Homeless
    );
    duplicates(&window).set_whole_home(true);
    duplicates(&window).invoke_search();
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Start);
    assert_eq!(duplicates(&window).get_refused(), ui::ToolRefusal::Home);
}

/// The state of the start's folders, the personal folders read by `folders`.
fn folders_state(folders: ReadFolders) -> ui::FoldersState {
    let sandbox = Sandbox::new();
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    seams.duplicates.search.folders = folders;
    duplicates(&on_duplicates(seams)).get_folders_state()
}

#[test]
fn personal_folders_unread_are_said_on_the_start() {
    assert_eq!(
        folders_state(Arc::new(|_| Err(io::Error::other("unreadable")))),
        ui::FoldersState::Unread
    );
}

#[test]
fn no_personal_folder_named_is_said_on_the_start() {
    assert_eq!(
        folders_state(Arc::new(|_| Ok(Vec::new()))),
        ui::FoldersState::None
    );
}

#[test]
fn a_result_stays_when_the_settings_are_refused_and_a_receipt_goes_back_to_the_start() {
    let sandbox = Sandbox::new();
    let downloaded = sandbox.write("Téléchargements/s.iso", 24, 1);
    sandbox.write("Documents/s.iso", 24, 5);
    let exclusions = no_exclusion();
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &exclusions, None));
    searched(&window);
    *exclusions.borrow_mut() = Err(Problem::SettingsUnsaved);
    duplicates(&window).invoke_again();
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Result);
    assert_eq!(duplicates(&window).get_refused(), ui::ToolRefusal::Unsaved);
    assert_eq!(duplicates(&window).get_chosen_files(), 1);
    // Said again by the confirmation itself.
    duplicates(&window).set_refused(ui::ToolRefusal::None);
    confirmed(&window);
    assert!(!duplicates(&window).get_trashing());
    assert_eq!(duplicates(&window).get_refused(), ui::ToolRefusal::Unsaved);
    assert!(downloaded.exists());
    *exclusions.borrow_mut() = Ok(Vec::new());
    confirmed(&window);
    read_until(&window, |window| {
        duplicates(window).get_step() == ui::ToolStep::Receipt
    });
    // From the receipt, a refusal is said on the start.
    *exclusions.borrow_mut() = Err(Problem::SettingsSaving);
    duplicates(&window).invoke_again();
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Start);
    assert_eq!(duplicates(&window).get_refused(), ui::ToolRefusal::Saving);
}

const EXCLUSION_REFUSED: &str =
    "Une exclusion de vos réglages est refusée : rien n'a été fait. Corrigez-la dans les Réglages.";

/// An exclusion the engine refuses.
fn refused_exclusion() -> Vec<PathBuf> {
    vec![PathBuf::from("relative")]
}

#[test]
fn an_exclusion_refused_starts_no_search() {
    let sandbox = Sandbox::new();
    sandbox.write("Téléchargements/r.iso", 26, 1);
    sandbox.write("Documents/r.iso", 26, 5);
    let exclusions = Rc::new(RefCell::new(Ok(refused_exclusion())));
    let threads_asked = Arc::new(AtomicUsize::new(0));
    let mut seams = seams(&sandbox, &Rc::default(), &exclusions, None);
    let asking = Arc::clone(&threads_asked);
    seams.duplicates.threads = Arc::new(move |_| {
        asking.fetch_add(1, Ordering::SeqCst);
        NonZeroUsize::MIN
    });
    let window = on_duplicates(seams);
    duplicates(&window).invoke_search();
    // Nothing resolved on the window's thread: the search starts, and its
    // own thread meets the refusal.
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Searching);
    assert_eq!(duplicates(&window).get_refused(), ui::ToolRefusal::None);
    read_until(&window, |window| {
        duplicates(window).get_refused() == ui::ToolRefusal::Exclusion
    });
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Start);
    assert!(shows(&window, EXCLUSION_REFUSED));
    assert_eq!(threads_asked.load(Ordering::SeqCst), 0, "nothing read");
    // Corrected, it searches.
    *exclusions.borrow_mut() = Ok(Vec::new());
    searched(&window);
    assert_eq!(threads_asked.load(Ordering::SeqCst), 1);
}

#[test]
fn an_exclusion_refused_moves_nothing_to_the_trash() {
    let sandbox = Sandbox::new();
    let downloaded = sandbox.write("Téléchargements/t.iso", 27, 1);
    sandbox.write("Documents/t.iso", 27, 5);
    let exclusions = no_exclusion();
    let opened = Arc::new(AtomicUsize::new(0));
    let mut seams = seams(&sandbox, &Rc::default(), &exclusions, None);
    let counter = Arc::clone(&opened);
    let data = sandbox.data();
    seams.duplicates.search.trash = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Trash::in_folder(&data)
    });
    let window = on_duplicates(seams);
    searched(&window);
    let opened_before = opened.load(Ordering::SeqCst);
    *exclusions.borrow_mut() = Ok(refused_exclusion());
    confirmed(&window);
    assert!(duplicates(&window).get_trashing());
    assert_eq!(duplicates(&window).get_refused(), ui::ToolRefusal::None);
    read_until(&window, |window| !duplicates(window).get_trashing());
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Result);
    assert_eq!(
        duplicates(&window).get_refused(),
        ui::ToolRefusal::Exclusion
    );
    assert!(shows(&window, EXCLUSION_REFUSED));
    assert_eq!(
        opened.load(Ordering::SeqCst),
        opened_before,
        "no trash opened"
    );
    assert!(downloaded.exists());
    // Searched again, the refusal is met on the search's thread: said on the
    // start, the result let go as any new search does.
    duplicates(&window).invoke_again();
    read_until(&window, |window| {
        duplicates(window).get_step() == ui::ToolStep::Start
    });
    assert_eq!(
        duplicates(&window).get_refused(),
        ui::ToolRefusal::Exclusion
    );
    // Corrected, the search and the move go.
    *exclusions.borrow_mut() = Ok(Vec::new());
    searched(&window);
    confirmed(&window);
    read_until(&window, |window| {
        duplicates(window).get_step() == ui::ToolStep::Receipt
    });
    assert!(!downloaded.exists());
}

#[test]
fn a_trash_that_does_not_open_is_said_and_moves_nothing() {
    let sandbox = Sandbox::new();
    let downloaded = sandbox.write("Téléchargements/u.iso", 28, 1);
    sandbox.write("Documents/u.iso", 28, 5);
    let opens = Arc::new(AtomicBool::new(true));
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    let opening = Arc::clone(&opens);
    let data = sandbox.data();
    seams.duplicates.search.trash = Arc::new(move || {
        if opening.load(Ordering::SeqCst) {
            Trash::in_folder(&data)
        } else {
            Err(io::Error::other("no trash, as a test asks"))
        }
    });
    let window = on_duplicates(seams);
    searched(&window);
    opens.store(false, Ordering::SeqCst);
    confirmed(&window);
    read_until(&window, |window| !duplicates(window).get_trashing());
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Result);
    assert_eq!(duplicates(&window).get_refused(), ui::ToolRefusal::Trash);
    assert!(downloaded.exists());
}

#[test]
fn a_move_cut_short_says_so_and_shows_nothing_stale() {
    let sandbox = Sandbox::new();
    let downloaded = sandbox.write("Téléchargements/p.iso", 25, 1);
    sandbox.write("Documents/p.iso", 25, 5);
    let opened = Arc::new(AtomicUsize::new(0));
    let data = sandbox.data();
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    let count = Arc::clone(&opened);
    seams.duplicates.search.trash = Arc::new(move || {
        // The start opens it; the move dies.
        assert_eq!(
            count.fetch_add(1, Ordering::SeqCst),
            0,
            "the move's thread dies"
        );
        Trash::in_folder(&data)
    });
    let window = on_duplicates(seams);
    searched(&window);
    confirmed(&window);
    read_until(&window, |window| {
        duplicates(window).get_step() == ui::ToolStep::Start
    });
    assert_eq!(
        duplicates(&window).get_refused(),
        ui::ToolRefusal::Interrupted
    );
    assert!(!duplicates(&window).get_trashing());
    assert_eq!(rows(&window), []);
    assert!(downloaded.exists());
}

#[test]
fn a_second_confirmation_while_copies_move_moves_nothing_more() {
    let sandbox = Sandbox::new();
    sandbox.write("Téléchargements/d.iso", 26, 1);
    sandbox.write("Documents/d.iso", 26, 5);
    let (release, held): (Sender<()>, _) = mpsc::channel();
    release.send(()).unwrap();
    let opened = Arc::new(AtomicUsize::new(0));
    let mut seams = seams(
        &sandbox,
        &Rc::default(),
        &no_exclusion(),
        Some(Arc::new(Mutex::new(held))),
    );
    let trash = Arc::clone(&seams.duplicates.search.trash);
    let count = Arc::clone(&opened);
    seams.duplicates.search.trash = Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        trash()
    });
    let window = on_duplicates(seams);
    searched(&window);
    confirmed(&window);
    // Copies moving: no confirmation asked, and one never asked is refused.
    confirmed(&window);
    assert!(!duplicates(&window).get_confirming());
    release.send(()).unwrap();
    read_until(&window, |window| {
        duplicates(window).get_step() == ui::ToolStep::Receipt
    });
    // The start's, then one move's.
    assert_eq!(opened.load(Ordering::SeqCst), 2);
    assert_eq!(duplicates(&window).get_trashed_files(), 1);
}

#[test]
fn a_copy_of_another_report_is_said_in_the_receipt_never_dropped() {
    use okapi_tools::duplicates::{DuplicateSearch, SearchOptions, trash};

    let searched_in = |sandbox: &Sandbox| {
        let search = DuplicateSearch::start(
            &[
                sandbox.root().join("Documents"),
                sandbox.root().join("Images"),
            ],
            SearchOptions {
                min_size: MIN_SIZE,
                threads: NonZeroUsize::MIN,
                exclusions: Exclusions::none(),
            },
        )
        .unwrap();
        assert_eq!(search.wait(WAIT), Some(Outcome::Complete));
        search
    };
    let first = Sandbox::new();
    first.write("Documents/u.bin", 27, 1);
    first.write("Images/u.bin", 27, 2);
    let second = Sandbox::new();
    second.write("Documents/v.bin", 28, 1);
    second.write("Images/v.bin", 28, 2);
    let (one, other) = (searched_in(&first), searched_in(&second));
    let ids: Vec<CopyId> = one.report().unwrap().groups()[0]
        .copies()
        .iter()
        .take(1)
        .map(okapi_tools::duplicates::FileCopy::id)
        .collect();
    fs::create_dir_all(second.data()).unwrap();
    let bin = Trash::in_folder(&second.data()).unwrap();
    let report = other.report().unwrap();
    let receipt = trash(report, &ids, &bin, &Exclusions::none());
    let said = rows::receipt(report, &receipt, None);
    assert_eq!(said.trashed, 0);
    assert_eq!(said.kept.len(), 1);
    assert_eq!(said.kept[0].why, ui::ReceiptWhy::Okapi);
    assert_eq!(said.kept[0].name, "");
}

#[test]
fn a_late_refusal_of_the_file_manager_is_never_said_on_another_copy() {
    let sandbox = Sandbox::new();
    sandbox.write("Documents/l1", 29, 1);
    sandbox.write("Images/l1", 29, 2);
    sandbox.write("Documents/l2", 30, 1);
    sandbox.write("Images/l2", 30, 2);
    let held: Rc<RefCell<Option<Answer>>> = Rc::default();
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    let holding = Rc::clone(&held);
    seams.duplicates.search.open = Rc::new(move |_, done| {
        *holding.borrow_mut() = Some(done);
    });
    let window = on_duplicates(seams);
    searched(&window);
    // The second group's last copy, at line 5.
    duplicates(&window).set_current(5);
    duplicates(&window).invoke_open(5);
    // The first group folded: the copy asked for is at line 3, line 2 is
    // the other copy of its group.
    duplicates(&window).set_current(0);
    duplicates(&window).invoke_fold(0, false);
    duplicates(&window).set_current(2);
    let answer = held.borrow_mut().take().unwrap();
    answer(false);
    assert!(!duplicates(&window).get_open_refused());
    assert!(!duplicates(&window).get_opening());
    // Asked and refused on the copy with the focus: said.
    duplicates(&window).invoke_open(2);
    let answer = held.borrow_mut().take().unwrap();
    answer(false);
    assert!(duplicates(&window).get_open_refused());
}

#[test]
fn what_the_confirmation_counts_is_what_goes() {
    let sandbox = Sandbox::new();
    let first = sandbox.write("Téléchargements/m1.iso", 31, 1);
    let first_kept = sandbox.write("Documents/m1.iso", 31, 5);
    let second = sandbox.write("Téléchargements/m2.iso", 32, 1);
    let second_kept = sandbox.write("Documents/m2.iso", 32, 5);
    let window = on_duplicates(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    assert_eq!(duplicates(&window).get_chosen_files(), 2);
    // Never asked: nothing goes.
    duplicates(&window).invoke_confirm();
    assert!(!duplicates(&window).get_trashing());
    // Cancelled: the choices are free again.
    duplicates(&window).invoke_trash();
    duplicates(&window).invoke_cancel_confirmation();
    let (at, _) = copy_row(&window, "Téléchargements/m1.iso");
    duplicates(&window).invoke_toggle(index(at));
    assert_eq!(duplicates(&window).get_chosen_files(), 1);
    duplicates(&window).invoke_toggle(index(at));
    duplicates(&window).invoke_trash();
    assert!(duplicates(&window).get_confirming());
    // Behind the confirmation (a screen reader reaches the lines), nothing
    // changes what goes: unfrozen, « keep the oldest » keeps Downloads'
    // copies and Documents' would go instead.
    duplicates(&window).invoke_toggle(index(at));
    duplicates(&window).invoke_deselect_all();
    duplicates(&window).invoke_quick(ui::QuickRule::Oldest, -1);
    duplicates(&window).invoke_again();
    duplicates(&window).invoke_change();
    assert_eq!(duplicates(&window).get_step(), ui::ToolStep::Result);
    assert_eq!(duplicates(&window).get_chosen_files(), 2);
    assert!(copy_row(&window, "Téléchargements/m1.iso").1.checked);
    duplicates(&window).invoke_confirm();
    read_until(&window, |window| {
        duplicates(window).get_step() == ui::ToolStep::Receipt
    });
    assert_eq!(duplicates(&window).get_trashed_files(), 2);
    assert!(!first.exists() && !second.exists());
    assert!(first_kept.exists() && second_kept.exists());
}

#[test]
fn a_refusal_of_the_file_manager_goes_as_soon_as_the_focus_moves() {
    use slint::platform::Key;

    let sandbox = Sandbox::new();
    sandbox.write("Documents/o1", 33, 1);
    sandbox.write("Images/o1", 33, 2);
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    seams.duplicates.search.open = Rc::new(|_, done| done(false));
    let window = on_duplicates(seams);
    searched(&window);
    // The tree has the focus at the result.
    press(&window, Key::DownArrow);
    duplicates(&window).invoke_open(1);
    assert!(duplicates(&window).get_open_refused());
    // Gone with the key, before the page is drawn again: never said under
    // the other copy.
    press(&window, Key::DownArrow);
    assert_eq!(duplicates(&window).get_current(), 2);
    assert!(!duplicates(&window).get_open_refused());
}
