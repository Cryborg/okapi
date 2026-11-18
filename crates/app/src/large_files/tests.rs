//! The large files screen in a window: a sandbox's home folder searched by
//! the real engine, its files checked, filtered, ordered and moved to a
//! sandbox's trash, the file manager and the trash through stand-ins.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::time::Instant;

use jiff::tz::TimeZone;
use okapi_platform::{PersonalFolder, PersonalKind, Trash};
use slint::Model as _;
use slint::platform::Key;

use super::*;
use crate::cleaner::tests::{defaults, press, seams_never_scanning, shows};
use crate::cleaner::{Problem, Seams};
use crate::dashboard::tests::inert;

/// How long a test waits for the screen: a fault fails, it never hangs.
const WAIT: Duration = Duration::from_secs(20);
/// The smallest file listed in the tests: a mebibyte.
const FLOOR: u64 = 1_048_576;
const DAY: u64 = 86_400;

/// A home folder with its personal folders.
struct Sandbox {
    home: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        for folder in ["Documents", "Vidéos", "Téléchargements"] {
            fs::create_dir(home.path().join(folder)).unwrap();
        }
        Self { home }
    }

    fn root(&self) -> PathBuf {
        self.home.path().canonicalize().unwrap()
    }

    /// Writes `mebibytes` of a content of its own, by `seed`, at `path`,
    /// below the home folder, its change date `days` after the clock's
    /// origin.
    fn write(&self, path: &str, mebibytes: u64, seed: u8, days: u64) -> PathBuf {
        let path = self.root().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            content(seed).repeat(usize::try_from(mebibytes).unwrap()),
        )
        .unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(days * DAY))
            .unwrap();
        path
    }

    /// Where the sandbox's trash lies: in a hidden folder, never searched.
    fn data(&self) -> PathBuf {
        self.root().join(".local/share")
    }

    fn trashed(&self, name: &str) -> bool {
        self.data().join("Trash/files").join(name).exists()
    }
}

/// A mebibyte of `seed`'s own, never one byte repeated: a compressing
/// filesystem would count it smaller on disk.
fn content(seed: u8) -> Vec<u8> {
    let mut state = u64::from(seed).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..FLOOR)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

/// The file manager's answers, held until the test gives them.
type HeldAnswers = Rc<RefCell<Vec<Box<dyn FnOnce(bool)>>>>;
/// What the file manager was asked.
type Opened = Rc<RefCell<Vec<OpenRequest>>>;
/// The settings' exclusions the screen reads, or a problem.
type SettingsExclusions = Rc<RefCell<Result<Vec<PathBuf>, Problem>>>;

/// Seams of a screen on `sandbox`, its trash held until `release` says when
/// it is given.
fn seams(
    sandbox: &Sandbox,
    opened: &Opened,
    exclusions: &SettingsExclusions,
    release: Option<Arc<Mutex<std::sync::mpsc::Receiver<()>>>>,
) -> Seams {
    let data = sandbox.data();
    fs::create_dir_all(&data).unwrap();
    let opened = Rc::clone(opened);
    let read = Rc::clone(exclusions);
    Seams {
        read: Rc::new(move || {
            let exclusions = read.borrow().clone()?;
            Ok(crate::cleaner::Inputs {
                exclusions,
                ..defaults()
            })
        }),
        large_files: LargeFileSeams {
            search: SearchSeams {
                home: Some(sandbox.root()),
                folders: Arc::new(|home| {
                    Ok([
                        (PersonalKind::Documents, "Documents"),
                        (PersonalKind::Videos, "Vidéos"),
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
                    opened.borrow_mut().push(request.clone());
                    done(true);
                }),
                now: Timestamp::now,
                zone: TimeZone::UTC,
            },
            min_size: FLOOR,
        },
        ..seams_never_scanning()
    }
}

fn no_exclusion() -> SettingsExclusions {
    Rc::new(RefCell::new(Ok(Vec::new())))
}

fn large(window: &ui::AppWindow) -> ui::LargeFiles<'_> {
    window.global::<ui::LargeFiles>()
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
            large(window).get_step(),
            large(window).get_phase()
        );
        read_once();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The window on the large files screen, its folders read, and the
/// screen.
fn on_large_files(seams: Seams) -> (ui::AppWindow, LargeFilesScreen) {
    let crate::Screens {
        window,
        large_files,
        ..
    } = crate::wide_window(seams, inert(), Language::French);
    window.set_page(ui::Page::LargeFiles);
    slint::platform::update_timers_and_animations();
    read_until(&window, |window| {
        large(window).get_folders_state() != ui::FoldersState::Reading
    });
    (window, large_files)
}

/// Searched, the result shown.
fn searched(window: &ui::AppWindow) {
    large(window).invoke_search();
    read_until(window, |window| {
        large(window).get_step() != ui::ToolStep::Searching
    });
    assert_eq!(large(window).get_step(), ui::ToolStep::Result);
}

fn rows(window: &ui::AppWindow) -> Vec<ui::LargeFileRow> {
    large(window).get_rows().iter().collect()
}

fn names(window: &ui::AppWindow) -> Vec<String> {
    rows(window)
        .iter()
        .map(|row| row.name.to_string())
        .collect()
}

/// The line of the file named `name`.
fn line_of(window: &ui::AppWindow, name: &str) -> i32 {
    let at = rows(window)
        .iter()
        .position(|row| row.name == name)
        .unwrap_or_else(|| panic!("no line {name}"));
    i32::try_from(at).unwrap()
}

/// The rank of `kind` in the kinds' filter.
fn kind_rank(kind: FileKind) -> i32 {
    i32::try_from(FileKind::ALL.iter().position(|each| *each == kind).unwrap()).unwrap()
}

/// Where to search, the box of folder `name`.
fn folder_box(window: &ui::AppWindow, name: &str) -> i_slint_backend_testing::ElementHandle {
    i_slint_backend_testing::ElementHandle::find_by_accessible_label(window, name)
        .find(|element| {
            element.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Checkbox)
        })
        .unwrap_or_else(|| panic!("no folder line {name}"))
}

/// Where to search, the line of folder `name`: whether its box is checked,
/// and enabled.
fn folder_line(window: &ui::AppWindow, name: &str) -> (bool, bool) {
    let line = folder_box(window, name);
    (
        line.accessible_checked().unwrap(),
        line.accessible_enabled().unwrap(),
    )
}

/// Whether the home folder's box is said partly checked.
fn home_said_partly(window: &ui::AppWindow) -> bool {
    folder_box(window, "Dossier personnel")
        .accessible_description()
        .unwrap()
        .ends_with("certains de ses dossiers cochés")
}

/// The personal folders the seams name, made: each is offered.
fn with_every_folder(sandbox: &Sandbox) {
    for name in ["Documents", "Vidéos", "Musique", "Téléchargements"] {
        fs::create_dir_all(sandbox.root().join(name)).unwrap();
    }
}

#[test]
fn the_home_folder_holds_its_folders_and_gives_back_their_choice() {
    let sandbox = Sandbox::new();
    with_every_folder(&sandbox);
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    // One folder left out by hand: the home folder's box partly checked.
    large(&window).invoke_toggle_folder(2);
    assert_eq!(folder_line(&window, "Dossier personnel"), (false, true));
    assert!(home_said_partly(&window));
    assert_eq!(folder_line(&window, "Musique"), (false, true));
    assert_eq!(folder_line(&window, "Documents"), (true, true));
    // Checked, the home folder holds them all: none to choose apart.
    folder_line_toggled(&window, "Dossier personnel");
    assert!(large(&window).get_whole_home());
    assert!(!home_said_partly(&window));
    assert_eq!(folder_line(&window, "Musique"), (true, false));
    assert_eq!(folder_line(&window, "Documents"), (true, false));
    // A folder held cannot change its own choice.
    folder_line_toggled(&window, "Musique");
    // Unchecked, each has its own choice back.
    folder_line_toggled(&window, "Dossier personnel");
    assert!(!large(&window).get_whole_home());
    assert_eq!(folder_line(&window, "Musique"), (false, true));
    assert_eq!(folder_line(&window, "Documents"), (true, true));
    // None chosen: the box is plainly unchecked.
    for name in ["Documents", "Vidéos", "Téléchargements"] {
        folder_line_toggled(&window, name);
    }
    assert_eq!(folder_line(&window, "Dossier personnel"), (false, true));
    assert!(!home_said_partly(&window));
}

/// Where to search, the box of folder `name` pressed.
fn folder_line_toggled(window: &ui::AppWindow, name: &str) {
    folder_box(window, name).invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
}

#[test]
fn a_result_lists_the_largest_first_and_checks_nothing() {
    let sandbox = Sandbox::new();
    sandbox.write("Documents/rapport.pdf", 2, 1, 10);
    sandbox.write("Vidéos/film.mkv", 4, 2, 20);
    sandbox.write("Téléchargements/ubuntu.iso", 3, 3, 30);
    // Below the floor: never listed.
    fs::write(sandbox.root().join("Documents/note.txt"), "court").unwrap();
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    assert_eq!(large(&window).get_chosen_folders(), 3);
    searched(&window);
    assert_eq!(names(&window), ["film.mkv", "ubuntu.iso", "rapport.pdf"]);
    assert_eq!(large(&window).get_files(), 3);
    assert_eq!(large(&window).get_chosen_files(), 0);
    let first = &rows(&window)[0];
    assert!(rows(&window).iter().all(|row| !row.checked));
    assert_eq!(first.kind, ui::FileKindShown::Video);
    assert_eq!(first.full_folder, "~/Vidéos");
    assert!((first.bar - 1.0).abs() < f32::EPSILON);
    assert!(!first.outside);
    assert_eq!(large(&window).get_all_kinds(), 3);
    assert!(shows(&window, "Gros fichiers"));
}

#[test]
fn the_boxes_follow_their_files_whatever_the_filters_and_the_order() {
    let sandbox = Sandbox::new();
    // By size: a, b, c, d. By date: c, a, d, b.
    let a = sandbox.write("Vidéos/a.mkv", 5, 1, 20);
    let b = sandbox.write("Documents/b.pdf", 4, 2, 40);
    let c = sandbox.write("Vidéos/c.mkv", 3, 3, 10);
    let d = sandbox.write("Téléchargements/d.zip", 2, 4, 30);
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    large(&window).invoke_toggle(line_of(&window, "a.mkv"));
    large(&window).invoke_toggle(line_of(&window, "d.zip"));
    assert_eq!(large(&window).get_chosen_files(), 2);
    // The focus on d, then the lines move under it.
    large(&window).set_current(line_of(&window, "d.zip"));
    large(&window).invoke_sort_by(ui::LargeSort::Modified);
    assert_eq!(names(&window), ["c.mkv", "a.mkv", "d.zip", "b.pdf"]);
    assert_eq!(large(&window).get_current(), line_of(&window, "d.zip"));
    assert!(rows(&window)[1].checked && rows(&window)[2].checked);
    assert!(!rows(&window)[0].checked && !rows(&window)[3].checked);
    large(&window).invoke_sort_by(ui::LargeSort::Size);
    assert_eq!(names(&window), ["a.mkv", "b.pdf", "c.mkv", "d.zip"]);
    assert_eq!(large(&window).get_current(), 3);
    assert_eq!(
        (large(&window).get_sort(), large(&window).get_descending()),
        (ui::LargeSort::Size, true)
    );
    // Again: the smallest first, the boxes and the focus with their files.
    large(&window).invoke_sort_by(ui::LargeSort::Size);
    assert_eq!(names(&window), ["d.zip", "c.mkv", "b.pdf", "a.mkv"]);
    assert!(!large(&window).get_descending());
    assert_eq!(large(&window).get_current(), 0);
    assert!(rows(&window)[0].checked && rows(&window)[3].checked);
    assert!(!rows(&window)[1].checked && !rows(&window)[2].checked);
    // The date, then again: the newest first.
    large(&window).invoke_sort_by(ui::LargeSort::Modified);
    assert!(!large(&window).get_descending());
    large(&window).invoke_sort_by(ui::LargeSort::Modified);
    assert_eq!(names(&window), ["b.pdf", "d.zip", "a.mkv", "c.mkv"]);
    assert!(large(&window).get_descending());
    large(&window).invoke_sort_by(ui::LargeSort::Size);
    large(&window).invoke_sort_by(ui::LargeSort::Modified);
    // Videos only: d is hidden, still chosen; the focus goes to the first.
    large(&window).invoke_filter_kind(kind_rank(FileKind::Video));
    assert_eq!(names(&window), ["c.mkv", "a.mkv"]);
    assert_eq!(large(&window).get_current(), 0);
    assert_eq!(large(&window).get_chosen_files(), 2);
    assert_eq!(large(&window).get_chosen_hidden(), 1);
    // A line checked now is the file it shows.
    large(&window).invoke_toggle(0);
    assert!(rows(&window)[0].checked);
    large(&window).invoke_toggle(0);
    large(&window).invoke_trash();
    let confirmed: Vec<ui::ChosenFile> = large(&window).get_confirmed().iter().collect();
    assert_eq!(
        confirmed
            .iter()
            .map(|file| (file.name.to_string(), file.hidden))
            .collect::<Vec<_>>(),
        [("a.mkv".to_owned(), false), ("d.zip".to_owned(), true)]
    );
    large(&window).invoke_confirm();
    read_until(&window, |window| {
        large(window).get_step() == ui::ToolStep::Receipt
    });
    assert!(!a.exists() && !d.exists());
    assert!(b.exists() && c.exists());
    assert!(sandbox.trashed("a.mkv") && sandbox.trashed("d.zip"));
    assert_eq!(large(&window).get_moved_files(), 2);
}

#[test]
fn each_filter_leaves_its_own_out_and_clearing_shows_them_all() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/old.mkv", 3, 1, 10);
    // Above the first floor offered (50 MB).
    sandbox.write("Téléchargements/big.iso", 48, 2, 10);
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    // Now: four years after the files.
    seams.large_files.search.now = || Timestamp::from_second(1_500 * 86_400).unwrap();
    sandbox.write("Vidéos/new.mkv", 3, 3, 1_490);
    let (window, _screen) = on_large_files(seams);
    searched(&window);
    assert_eq!(large(&window).get_files(), 3);
    // Modified more than three months ago.
    large(&window).invoke_filter_age(0);
    assert_eq!(large(&window).get_age(), 0);
    assert_eq!(names(&window), ["big.iso", "old.mkv"]);
    // The kinds count under the age: one video, one disk image.
    let kinds: Vec<ui::KindCount> = large(&window).get_kinds().iter().collect();
    let count = |kind| {
        kinds
            .iter()
            .find(|counted| counted.kind == kind)
            .unwrap()
            .count
    };
    assert_eq!(count(ui::FileKindShown::Video), 1);
    assert_eq!(count(ui::FileKindShown::DiskImage), 1);
    assert_eq!(large(&window).get_all_kinds(), 2);
    // The floors offered start above the smallest file listed.
    assert_eq!(
        large(&window).get_floors().row_count(),
        okapi_tools::large_files::FLOORS.len()
    );
    large(&window).invoke_filter_size(0);
    assert_eq!(large(&window).get_floor(), 0);
    assert_eq!(names(&window), ["big.iso"]);
    large(&window).invoke_filter_kind(kind_rank(FileKind::Video));
    assert_eq!(large(&window).get_kind(), kind_rank(FileKind::Video));
    assert_eq!(rows(&window), []);
    assert!(shows(&window, "Aucun fichier ne correspond à ces filtres"));
    large(&window).invoke_clear_filters();
    assert_eq!(names(&window).len(), 3);
    assert_eq!(
        (
            large(&window).get_floor(),
            large(&window).get_kind(),
            large(&window).get_age()
        ),
        (-1, -1, -1)
    );
}

#[test]
fn a_sparse_file_is_measured_on_disk_and_its_size_said_apart() {
    let sandbox = Sandbox::new();
    let path = sandbox.root().join("Documents/disk.img");
    let file = fs::File::create(&path).unwrap();
    file.set_len(8 * FLOOR).unwrap();
    {
        use std::os::unix::fs::FileExt as _;
        file.write_all_at(&content(1).repeat(2), FLOOR).unwrap();
    }
    drop(file);
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    let row = &rows(&window)[0];
    assert!(row.differs);
    assert_eq!(row.file_size, size(8 * FLOOR));
    assert_ne!(row.size, row.file_size);
}

/// The filter of `label`, as a screen reader reaches it.
fn filter(window: &ui::AppWindow, label: &str) -> i_slint_backend_testing::ElementHandle {
    i_slint_backend_testing::ElementHandle::find_by_accessible_label(window, label)
        .find(|element| {
            element.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Combobox)
        })
        .unwrap_or_else(|| panic!("no filter {label}"))
}

/// Opens the menu of the filter `label`.
fn open_menu(window: &ui::AppWindow, label: &str) {
    filter(window, label).invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
}

#[test]
fn a_filter_s_menu_answers_the_keyboard() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/a.mkv", 3, 1, 10);
    sandbox.write("Documents/b.pdf", 2, 2, 10);
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    // Opened on « Tous les types »: one down is the first kind.
    open_menu(&window, "Type");
    press(&window, Key::DownArrow);
    press(&window, Key::Return);
    slint::platform::update_timers_and_animations();
    assert_eq!(large(&window).get_kind(), kind_rank(FileKind::ALL[0]));
    assert_eq!(names(&window), ["a.mkv"]);
    // Escape closes it: nothing chosen.
    open_menu(&window, "Type");
    press(&window, Key::DownArrow);
    press(&window, Key::Escape);
    slint::platform::update_timers_and_animations();
    assert_eq!(large(&window).get_kind(), kind_rank(FileKind::ALL[0]));
    // Opened on the kind applied: one up is every kind.
    open_menu(&window, "Type");
    press(&window, Key::UpArrow);
    press(&window, Key::Return);
    slint::platform::update_timers_and_animations();
    assert_eq!(large(&window).get_kind(), -1);
    assert_eq!(names(&window).len(), 2);
    // The last floor, from the end.
    open_menu(&window, "Taille minimale");
    press(&window, Key::End);
    press(&window, Key::Space);
    slint::platform::update_timers_and_animations();
    assert_eq!(
        large(&window).get_floor(),
        i32::try_from(okapi_tools::large_files::FLOORS.len() - 1).unwrap()
    );
}

#[test]
fn a_file_that_cannot_go_is_shown_with_its_reason_and_never_checked() {
    let sandbox = Sandbox::new();
    let named = sandbox.write("Documents/twice.tar", 2, 1, 10);
    fs::hard_link(&named, sandbox.root().join("Documents/again.tar")).unwrap();
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    assert_eq!(rows(&window).len(), 1);
    assert_eq!(rows(&window)[0].why, ui::LargeWhy::OtherNames);
    large(&window).invoke_toggle(0);
    assert!(!rows(&window)[0].checked);
    assert_eq!(large(&window).get_chosen_files(), 0);
    assert!(shows(
        &window,
        "A d'autres noms : le retirer ne libérerait rien"
    ));
}

#[test]
fn the_whole_home_offers_a_file_outside_the_personal_folders_with_its_note() {
    let sandbox = Sandbox::new();
    sandbox.write("VirtualBox VMs/win.vdi", 2, 1, 10);
    sandbox.write("Vidéos/film.mkv", 3, 4, 10);
    sandbox.write("snap/app/data.bin", 2, 2, 10);
    sandbox.write("Excluded/big.bin", 2, 3, 10);
    let exclusions = Rc::new(RefCell::new(Ok(vec![sandbox.root().join("Excluded")])));
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &exclusions, None));
    large(&window).set_whole_home(true);
    searched(&window);
    assert_eq!(names(&window), ["film.mkv", "win.vdi"]);
    assert!(!rows(&window)[0].outside);
    let row = &rows(&window)[1];
    assert!(row.outside);
    assert_eq!(row.why, ui::LargeWhy::None);
    large(&window).set_current(1);
    large(&window).invoke_toggle(1);
    assert!(rows(&window)[1].checked);
    // The folders named as the disk names them, those of the seams that
    // exist.
    assert!(shows(
        &window,
        "Ce fichier n'est dans aucun de ces dossiers : Documents, Vidéos, Téléchargements. Il appartient peut-être à une application."
    ));
}

#[test]
fn a_file_written_since_the_search_stays_and_the_receipt_says_why() {
    let sandbox = Sandbox::new();
    let kept = sandbox.write("Vidéos/rendu.mkv", 2, 1, 10);
    let gone = sandbox.write("Vidéos/brut.mkv", 3, 2, 10);
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    large(&window).invoke_toggle(0);
    large(&window).invoke_toggle(1);
    std::thread::sleep(Duration::from_millis(30));
    fs::write(&kept, vec![9; usize::try_from(2 * FLOOR).unwrap()]).unwrap();
    large(&window).invoke_trash();
    large(&window).invoke_confirm();
    read_until(&window, |window| {
        large(window).get_step() == ui::ToolStep::Receipt
    });
    assert!(kept.exists() && !gone.exists());
    assert!(sandbox.trashed("brut.mkv") && !sandbox.trashed("rendu.mkv"));
    assert_eq!(large(&window).get_trashed_files(), 1);
    assert_eq!(large(&window).get_moved(), size(3 * FLOOR));
    let stayed: Vec<ui::KeptShown> = large(&window).get_kept().iter().collect();
    assert_eq!(stayed.len(), 1);
    assert_eq!(stayed[0].name, "rendu.mkv");
    assert_eq!(stayed[0].folder, "~/Vidéos");
    assert_eq!(stayed[0].why, ui::ReceiptWhy::Changed);
    assert!(shows(&window, "rendu.mkv, A changé depuis la recherche"));
}

#[test]
fn the_confirmation_waits_on_cancel_and_escape_closes_it() {
    let sandbox = Sandbox::new();
    let file = sandbox.write("Téléchargements/c.iso", 2, 1, 10);
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    // Nothing chosen: nothing to confirm.
    large(&window).invoke_trash();
    assert!(!large(&window).get_confirming());
    large(&window).invoke_toggle(0);
    large(&window).invoke_trash();
    slint::platform::update_timers_and_animations();
    assert!(large(&window).get_confirming());
    assert!(shows(&window, "Mettre 1 fichier à la corbeille ?"));
    // Two Enter in a row never move a file: the focus is on « Annuler ».
    press(&window, Key::Return);
    slint::platform::update_timers_and_animations();
    assert!(!large(&window).get_confirming());
    assert!(!large(&window).get_trashing());
    large(&window).invoke_trash();
    slint::platform::update_timers_and_animations();
    press(&window, Key::Escape);
    slint::platform::update_timers_and_animations();
    assert!(!large(&window).get_confirming());
    assert!(!large(&window).get_trashing());
    for _ in 0..5 {
        read_once();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(file.exists() && !sandbox.trashed("c.iso"));
    assert_eq!(large(&window).get_step(), ui::ToolStep::Result);
    // Cancelled: the boxes may change again.
    large(&window).invoke_toggle(0);
    assert_eq!(large(&window).get_chosen_files(), 0);
}

#[test]
fn what_the_confirmation_names_is_what_goes_in_the_report_s_order() {
    let sandbox = Sandbox::new();
    // By size: a, b, c. By date: c, a, b.
    sandbox.write("Vidéos/a.mkv", 4, 1, 20);
    sandbox.write("Vidéos/b.mkv", 3, 2, 30);
    sandbox.write("Vidéos/c.mkv", 2, 3, 10);
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    large(&window).invoke_sort_by(ui::LargeSort::Modified);
    large(&window).invoke_toggle(line_of(&window, "a.mkv"));
    large(&window).invoke_toggle(line_of(&window, "c.mkv"));
    large(&window).invoke_trash();
    let confirmed: Vec<String> = large(&window)
        .get_confirmed()
        .iter()
        .map(|file| file.name.to_string())
        .collect();
    assert_eq!(confirmed, ["a.mkv", "c.mkv"]);
    // A line behind the confirmation (a screen reader reaches it) changes
    // nothing of what goes.
    large(&window).invoke_toggle(line_of(&window, "b.mkv"));
    large(&window).invoke_deselect_all();
    large(&window).invoke_filter_kind(kind_rank(FileKind::Archive));
    assert_eq!(large(&window).get_chosen_files(), 2);
    assert_eq!(large(&window).get_kind(), -1);
    large(&window).invoke_confirm();
    read_until(&window, |window| {
        large(window).get_step() == ui::ToolStep::Receipt
    });
    assert!(sandbox.trashed("a.mkv") && sandbox.trashed("c.mkv"));
    assert!(!sandbox.trashed("b.mkv"));
}

#[test]
fn every_box_unchecked_hidden_or_not() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/a.mkv", 3, 1, 10);
    sandbox.write("Documents/b.pdf", 2, 2, 10);
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    large(&window).invoke_toggle(0);
    large(&window).invoke_toggle(1);
    large(&window).invoke_filter_kind(kind_rank(FileKind::Video));
    assert_eq!(large(&window).get_chosen_hidden(), 1);
    large(&window).invoke_deselect_all();
    assert_eq!(large(&window).get_chosen_files(), 0);
    assert_eq!(large(&window).get_chosen_hidden(), 0);
    large(&window).invoke_clear_filters();
    assert!(rows(&window).iter().all(|row| !row.checked));
}

#[test]
fn the_file_shown_in_its_folder_is_the_one_on_the_line_whatever_the_order() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/a.mkv", 3, 1, 20);
    let oldest = sandbox.write("Vidéos/b.mkv", 2, 2, 10);
    let opened: Opened = Rc::default();
    let (window, _screen) = on_large_files(seams(&sandbox, &opened, &no_exclusion(), None));
    searched(&window);
    large(&window).invoke_sort_by(ui::LargeSort::Modified);
    large(&window).invoke_open(0);
    assert_eq!(opened.borrow()[0].path, oldest);
}

#[test]
fn a_refusal_of_the_file_manager_is_said_on_its_file_only() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/a.mkv", 3, 1, 20);
    sandbox.write("Vidéos/b.mkv", 2, 2, 10);
    let answers: HeldAnswers = Rc::default();
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    let held = Rc::clone(&answers);
    seams.large_files.search.open = Rc::new(move |_, done| held.borrow_mut().push(done));
    let (window, _screen) = on_large_files(seams);
    searched(&window);
    // Refused late, once another file took the line: not said.
    large(&window).set_current(0);
    large(&window).invoke_open(0);
    large(&window).invoke_sort_by(ui::LargeSort::Modified);
    assert_eq!(names(&window), ["b.mkv", "a.mkv"]);
    large(&window).set_current(0);
    answers.borrow_mut().pop().unwrap()(false);
    assert!(!large(&window).get_open_refused());
    assert!(!large(&window).get_opening());
    // Refused on the file with the focus: said.
    large(&window).invoke_open(0);
    answers.borrow_mut().pop().unwrap()(false);
    assert!(large(&window).get_open_refused());
}

#[test]
fn a_refusal_of_the_file_manager_stays_with_its_file_and_goes_with_the_focus() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/a.mkv", 3, 1, 20);
    sandbox.write("Vidéos/b.mkv", 2, 2, 10);
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    seams.large_files.search.open = Rc::new(|_, done| done(false));
    let (window, _screen) = on_large_files(seams);
    searched(&window);
    large(&window).set_current(1);
    large(&window).invoke_open(1);
    assert!(large(&window).get_open_refused());
    // b moves to the first line, the focus with it: still said.
    large(&window).invoke_sort_by(ui::LargeSort::Modified);
    assert_eq!(large(&window).get_current(), 0);
    assert!(large(&window).get_open_refused());
    // The user moves the focus: no more.
    press(&window, Key::DownArrow);
    assert_eq!(large(&window).get_current(), 1);
    assert!(!large(&window).get_open_refused());
}

#[test]
fn a_refusal_of_the_file_manager_goes_with_its_file_off_the_line() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/a.mkv", 3, 1, 10);
    sandbox.write("Vidéos/b.mkv", 2, 2, 30);
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    seams.large_files.search.open = Rc::new(|_, done| done(false));
    // 1970-04-23: three months before is January 23rd (day 22), between
    // the two files.
    seams.large_files.search.now = || Timestamp::from_second(112 * 86_400).unwrap();
    let (window, _screen) = on_large_files(seams);
    searched(&window);
    assert_eq!(names(&window), ["a.mkv", "b.mkv"]);
    large(&window).set_current(1);
    large(&window).invoke_open(1);
    assert!(large(&window).get_open_refused());
    // b hidden: a has the focus on its line now, the refusal is no more.
    large(&window).invoke_filter_age(0);
    assert_eq!(names(&window), ["a.mkv"]);
    assert!(!large(&window).get_open_refused());
}

#[test]
fn the_confirmation_names_five_files_and_counts_the_others() {
    let sandbox = Sandbox::new();
    for rank in 0..7 {
        sandbox.write(&format!("Vidéos/{rank}.mkv"), 2, rank, 10);
    }
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    for line in 0..7 {
        large(&window).invoke_toggle(line);
    }
    large(&window).invoke_trash();
    slint::platform::update_timers_and_animations();
    assert_eq!(large(&window).get_confirmed().row_count(), 5);
    assert_eq!(large(&window).get_confirmed_more(), 2);
    assert!(shows(&window, "Mettre 7 fichiers à la corbeille ?"));
    assert!(shows(&window, "et 2 autres"));
}

#[test]
fn the_list_answers_the_keyboard() {
    let sandbox = Sandbox::new();
    let first = sandbox.write("Vidéos/a.mkv", 3, 1, 10);
    sandbox.write("Vidéos/b.mkv", 2, 2, 10);
    let opened: Opened = Rc::default();
    let (window, _screen) = on_large_files(seams(&sandbox, &opened, &no_exclusion(), None));
    searched(&window);
    // The list has the focus at the result.
    press(&window, Key::DownArrow);
    assert_eq!(large(&window).get_current(), 1);
    press(&window, Key::Space);
    assert!(rows(&window)[1].checked, "Space checks a file");
    press(&window, Key::Home);
    assert_eq!(large(&window).get_current(), 0);
    press(&window, Key::Return);
    let asked = opened.borrow();
    assert_eq!(asked.len(), 1, "Enter shows the file in its folder");
    assert_eq!(asked[0].path, first);
    assert!(!asked[0].folder);
    let identity = okapi_platform::file_identity(&fs::symlink_metadata(&first).unwrap());
    assert_eq!(asked[0].expected, identity);
    drop(asked);
    press(&window, Key::End);
    assert_eq!(large(&window).get_current(), 1);
}

#[test]
fn a_line_is_read_as_its_file_and_what_becomes_of_it() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/Montage/rendu.mkv", 2, 1, 10);
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    large(&window).invoke_toggle(0);
    assert!(shows(
        &window,
        "rendu.mkv, ~/Vidéos/Montage, modifié le 11 janvier 1970 à 0 h, 2,0 Mo, Vidéo, coché, vers la corbeille"
    ));
}

#[test]
fn a_search_stopped_while_opening_keeps_nothing() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/a.mkv", 2, 1, 10);
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    large(&window).invoke_search();
    assert_eq!(large(&window).get_phase(), ui::SearchPhase::Opening);
    large(&window).invoke_stop();
    for _ in 0..5 {
        read_once();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(large(&window).get_phase(), ui::SearchPhase::Stopped);
    assert_eq!(large(&window).get_step(), ui::ToolStep::Searching);
    assert_eq!(rows(&window), []);
}

#[test]
fn settings_that_cannot_be_read_start_nothing() {
    let sandbox = Sandbox::new();
    let exclusions = Rc::new(RefCell::new(Err(Problem::SettingsUnsaved)));
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &exclusions, None));
    large(&window).invoke_search();
    assert_eq!(large(&window).get_step(), ui::ToolStep::Start);
    assert_eq!(large(&window).get_refused(), ui::ToolRefusal::Unsaved);
}

#[test]
fn a_folder_excluded_since_the_search_keeps_its_files() {
    let sandbox = Sandbox::new();
    let file = sandbox.write("Vidéos/a.mkv", 2, 1, 10);
    let exclusions = no_exclusion();
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &exclusions, None));
    searched(&window);
    large(&window).invoke_toggle(0);
    *exclusions.borrow_mut() = Ok(vec![sandbox.root().join("Vidéos")]);
    large(&window).invoke_trash();
    large(&window).invoke_confirm();
    read_until(&window, |window| {
        large(window).get_step() == ui::ToolStep::Receipt
    });
    assert!(file.exists() && !sandbox.trashed("a.mkv"));
    assert_eq!(large(&window).get_trashed_files(), 0);
    let stayed: Vec<ui::KeptShown> = large(&window).get_kept().iter().collect();
    assert_eq!(stayed[0].why, ui::ReceiptWhy::Excluded);
}

#[test]
fn settings_refused_at_the_click_move_nothing_and_keep_the_result() {
    let sandbox = Sandbox::new();
    let file = sandbox.write("Vidéos/a.mkv", 2, 1, 10);
    let exclusions = no_exclusion();
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &exclusions, None));
    searched(&window);
    large(&window).invoke_toggle(0);
    *exclusions.borrow_mut() = Err(Problem::SettingsUnsaved);
    large(&window).invoke_trash();
    large(&window).invoke_confirm();
    assert_eq!(large(&window).get_refused(), ui::ToolRefusal::Unsaved);
    assert!(!large(&window).get_trashing());
    assert_eq!(large(&window).get_step(), ui::ToolStep::Result);
    // Searching again is refused over the result, which stays.
    large(&window).invoke_again();
    assert_eq!(large(&window).get_step(), ui::ToolStep::Result);
    assert_eq!(large(&window).get_chosen_files(), 1);
    assert!(file.exists() && !sandbox.trashed("a.mkv"));
}

#[test]
fn a_large_file_set_aside_is_never_said_small() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/excluded/a.mkv", 2, 1, 10);
    let exclusions = Rc::new(RefCell::new(Ok(vec![
        sandbox.root().join("Vidéos/excluded/a.mkv"),
    ])));
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &exclusions, None));
    searched(&window);
    assert_eq!(large(&window).get_files(), 0);
    assert!(!large(&window).get_all_small());
    assert!(shows(
        &window,
        "Aucun gros fichier à proposer dans ces dossiers"
    ));
}

#[test]
fn nothing_found_says_so_and_a_new_result_clears_the_filters() {
    let sandbox = Sandbox::new();
    let (window, _screen) = on_large_files(seams(&sandbox, &Rc::default(), &no_exclusion(), None));
    searched(&window);
    assert_eq!(large(&window).get_files(), 0);
    assert!(large(&window).get_all_small());
    assert!(shows(
        &window,
        "Aucun fichier de plus de 1,0 Mo dans ces dossiers"
    ));
    sandbox.write("Vidéos/a.mkv", 2, 1, 10);
    large(&window).invoke_again();
    read_until(&window, |window| {
        large(window).get_step() == ui::ToolStep::Result && large(window).get_files() == 1
    });
    large(&window).invoke_toggle(0);
    large(&window).invoke_filter_size(1);
    large(&window).invoke_filter_kind(kind_rank(FileKind::Archive));
    large(&window).invoke_filter_age(2);
    large(&window).invoke_sort_by(ui::LargeSort::Modified);
    assert_eq!(
        (large(&window).get_sort(), large(&window).get_descending()),
        (ui::LargeSort::Modified, false)
    );
    large(&window).invoke_again();
    read_until(&window, |window| {
        large(window).get_step() == ui::ToolStep::Result && large(window).get_kind() == -1
    });
    assert_eq!(
        (large(&window).get_floor(), large(&window).get_age()),
        (-1, -1)
    );
    assert_eq!(large(&window).get_sort(), ui::LargeSort::Size);
    assert!(large(&window).get_descending());
    assert_eq!(names(&window), ["a.mkv"]);
    assert_eq!(large(&window).get_chosen_files(), 0);
    assert!(!rows(&window)[0].checked);
}

#[test]
fn while_files_move_nothing_else_can_be_done_and_closing_waits() {
    let sandbox = Sandbox::new();
    sandbox.write("Vidéos/a.mkv", 2, 1, 10);
    sandbox.write("Vidéos/b.mkv", 3, 2, 10);
    let (release, held): (Sender<()>, _) = mpsc::channel();
    // The start opens the trash too: let it.
    release.send(()).unwrap();
    let (window, screen) = on_large_files(seams(
        &sandbox,
        &Rc::default(),
        &no_exclusion(),
        Some(Arc::new(Mutex::new(held))),
    ));
    searched(&window);
    large(&window).invoke_toggle(0);
    large(&window).invoke_trash();
    large(&window).invoke_confirm();
    assert!(large(&window).get_trashing());
    // What goes is taken: nothing else changes.
    large(&window).invoke_toggle(1);
    large(&window).invoke_deselect_all();
    large(&window).invoke_filter_kind(kind_rank(FileKind::Archive));
    large(&window).invoke_filter_size(0);
    large(&window).invoke_filter_age(0);
    large(&window).invoke_clear_filters();
    large(&window).invoke_sort_by(ui::LargeSort::Size);
    large(&window).invoke_trash();
    large(&window).invoke_again();
    large(&window).invoke_change();
    large(&window).invoke_search();
    assert_eq!(large(&window).get_step(), ui::ToolStep::Result);
    assert_eq!(large(&window).get_chosen_files(), 1);
    assert_eq!(large(&window).get_kind(), -1);
    assert_eq!(large(&window).get_sort(), ui::LargeSort::Size);
    assert!(large(&window).get_descending());
    assert!(!large(&window).get_confirming());
    let later = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        release.send(()).unwrap();
    });
    let started = Instant::now();
    screen.shut_down();
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(sandbox.trashed("b.mkv") && !sandbox.trashed("a.mkv"));
    later.join().unwrap();
}

#[test]
fn a_trash_that_does_not_open_is_said_and_moves_nothing() {
    let sandbox = Sandbox::new();
    let file = sandbox.write("Vidéos/a.mkv", 2, 1, 10);
    let mut seams = seams(&sandbox, &Rc::default(), &no_exclusion(), None);
    seams.large_files.search.trash = Arc::new(|| Err(io::Error::other("no trash")));
    let (window, _screen) = on_large_files(seams);
    searched(&window);
    // The trash's disk unknown: any file may be checked, the move says why.
    large(&window).invoke_toggle(0);
    large(&window).invoke_trash();
    large(&window).invoke_confirm();
    read_until(&window, |window| !large(window).get_trashing());
    assert_eq!(large(&window).get_refused(), ui::ToolRefusal::Trash);
    assert!(file.exists());
    assert_eq!(large(&window).get_step(), ui::ToolStep::Result);
}
