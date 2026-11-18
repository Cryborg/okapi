//! The disk usage screen in a window: a sandbox's home folder analyzed, its
//! folders browsed with the mouse and the keyboard, the file manager asked
//! through a stand-in.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use i_slint_backend_testing::ElementHandle;
use okapi_platform::{Device, Mount, MountClass};
use okapi_tools::monitor::{Command, Sample, Source, Space, Unavailable, VolumeName, VolumeSpace};
use slint::Model as _;

use super::*;
use crate::cleaner::Seams;
use crate::cleaner::tests::{button, seams_never_scanning, shows};
use crate::dashboard::tests::{GB, Sampler, known, noting, silent, take_in};

/// How long a test waits for an analysis: a fault fails, it never hangs.
const WAIT: Duration = Duration::from_secs(20);

/// `length` bytes no filesystem compresses: sizes on disk stay apart.
fn noise(length: usize) -> Vec<u8> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

/// A home folder with a large folder, a small one, and a file.
fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("large/inner")).unwrap();
    fs::write(home.path().join("large/inner/data"), noise(1_000_000)).unwrap();
    fs::create_dir(home.path().join("small")).unwrap();
    fs::write(home.path().join("small/data"), noise(10_000)).unwrap();
    fs::write(home.path().join("note"), noise(50_000)).unwrap();
    home
}

fn root(home: &tempfile::TempDir) -> PathBuf {
    home.path().canonicalize().unwrap()
}

/// How the file manager answers: opened or not.
type Answer = Box<dyn FnOnce(bool)>;

/// What the file manager stand-in is asked, and its answers held until the
/// test gives them.
#[derive(Default)]
struct FileManager {
    asked: RefCell<Vec<OpenRequest>>,
    answers: RefCell<Vec<Answer>>,
}

impl FileManager {
    /// Answers the last call.
    fn answer(&self, opened: bool) {
        let answer = self.answers.borrow_mut().pop().unwrap();
        answer(opened);
    }

    /// Answers the first call not answered yet.
    fn answer_oldest(&self, opened: bool) {
        let answer = self.answers.borrow_mut().remove(0);
        answer(opened);
    }
}

/// Seams of a screen analyzing `home`, its caches in `large`, the file
/// manager `manager` (answering at once when `at_once`), the mounts
/// `mounts`.
fn seams_with(
    home: &tempfile::TempDir,
    manager: &Rc<FileManager>,
    at_once: bool,
    mounts: &Rc<RefCell<Option<MountList>>>,
) -> Seams {
    let manager = Rc::clone(manager);
    let mounts = Rc::clone(mounts);
    Seams {
        disk: DiskSeams {
            home: Some(root(home)),
            cache: Some(root(home).join("large")),
            mounts: Rc::new(move || mounts.borrow().clone()),
            open: Rc::new(move |request, done| {
                manager.asked.borrow_mut().push(request.clone());
                if at_once {
                    done(true);
                } else {
                    manager.answers.borrow_mut().push(done);
                }
            }),
            files: Arc::new(UsageScan::files),
            now: Timestamp::now,
            threads: NonZeroUsize::MIN,
        },
        ..seams_never_scanning()
    }
}

/// Seams of a screen analyzing `home`, the file manager answering at once.
fn seams(home: &tempfile::TempDir, manager: &Rc<FileManager>) -> Seams {
    seams_with(home, manager, true, &Rc::new(RefCell::new(None)))
}

/// The window on the disk usage screen.
fn on_disk(seams: Seams) -> (ui::AppWindow, Sampler) {
    let (home, sampler) = noting(None);
    let crate::Screens { window, .. } = crate::wide_window(seams, home, Language::French);
    button(&window, "Espace disque").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    (window, sampler)
}

fn disk(window: &ui::AppWindow) -> ui::DiskUsage<'_> {
    window.global::<ui::DiskUsage>()
}

fn step(window: &ui::AppWindow) -> ui::DiskStep {
    disk(window).get_step()
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
            step(window),
            names(window)
        );
        read_once();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn lines(window: &ui::AppWindow) -> Vec<ui::DiskLine> {
    disk(window).get_lines().iter().collect()
}

/// The names of the lines shown, in their order.
fn names(window: &ui::AppWindow) -> Vec<String> {
    lines(window)
        .iter()
        .map(|line| line.name.to_string())
        .collect()
}

fn index_of(window: &ui::AppWindow, name: &str) -> i32 {
    let index = names(window)
        .iter()
        .position(|shown| shown == name)
        .unwrap_or_else(|| panic!("no line {name}"));
    shown(index)
}

/// Analyzed whole, its files read.
fn analyzed(window: &ui::AppWindow) {
    button(window, "Analyser").invoke_accessible_default_action();
    read_until(window, |window| {
        step(window) == ui::DiskStep::Result && names(window).contains(&"note".to_owned())
    });
}

fn send_volumes(sampler: &Sampler, volumes: Result<Vec<Volume>, Unavailable>) {
    sampler
        .sink
        .as_ref()
        .unwrap()
        .send(Sample::Volumes(volumes))
        .unwrap();
    take_in();
}

fn volume(name: &str, kind: VolumeKind, space: VolumeSpace) -> Volume {
    Volume {
        name: VolumeName::Source(name.to_owned()),
        mount_point: PathBuf::from(format!("/media/{name}")),
        filesystem: "ext4".to_owned(),
        kind,
        space,
    }
}

const SPACE: VolumeSpace = VolumeSpace::Known(Space {
    total: 64 * GB,
    available: 32 * GB,
    used: 32 * GB,
});

#[test]
fn the_sidebar_leads_to_the_start_and_the_volumes_read() {
    let home = home();
    let manager = Rc::default();
    let (window, sampler) = on_disk(seams(&home, &manager));
    assert!(shows(&window, "Dossier personnel"));
    // Its description read by its choice, written below its name silent.
    let description = "Vos documents, médias, projets et les données de vos applications.";
    assert!(
        ElementHandle::find_by_accessible_label(&window, "Dossier personnel")
            .any(|choice| choice.accessible_description().as_deref() == Some(description))
    );
    assert!(!shows(&window, description));
    assert_eq!(disk(&window).get_volumes_state(), ui::DiskVolumes::Reading);
    assert!(
        sampler.commands.borrow().ends_with(&[
            Command::Watch(BTreeSet::from([Source::Volumes])),
            Command::Resume
        ]),
        "the volumes read alone, at once: {:?}",
        sampler.commands.borrow()
    );
    let failed = Unavailable {
        kind: std::io::ErrorKind::PermissionDenied,
        code: None,
    };
    send_volumes(
        &sampler,
        Ok(vec![
            volume("share", VolumeKind::Network, SPACE),
            volume("lost", VolumeKind::Local, VolumeSpace::NotResponding),
            volume("broken", VolumeKind::Local, VolumeSpace::Failed(failed)),
            volume("usb", VolumeKind::Removable, SPACE),
            known(VolumeName::System, 39 * GB, 198 * GB),
            silent("nas:/photos", VolumeSpace::NotResponding),
        ]),
    );
    read_until(&window, |window| {
        disk(window).get_volumes_state() == ui::DiskVolumes::Ready
    });
    let offered: Vec<String> = disk(&window)
        .get_volumes()
        .iter()
        .map(|volume| volume.mount_point.to_string())
        .collect();
    assert_eq!(
        offered,
        ["/media/usb", "/okapi-test"],
        "a share, or a disk of unknown space, never offered"
    );
    assert!(shows(&window, "Système"));
    send_volumes(&sampler, Err(failed));
    read_once();
    assert_eq!(
        disk(&window).get_volumes_state(),
        ui::DiskVolumes::Unavailable
    );
    assert_eq!(disk(&window).get_volumes().row_count(), 0);
}

#[test]
fn volumes_that_never_come_are_said_unreadable() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    for _ in 1..VOLUMES_READINGS - 1 {
        read_once();
    }
    assert_eq!(disk(&window).get_volumes_state(), ui::DiskVolumes::Reading);
    read_once();
    assert_eq!(
        disk(&window).get_volumes_state(),
        ui::DiskVolumes::Unavailable
    );
}

#[test]
fn the_home_folder_is_analyzed_its_lines_largest_first() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    assert_eq!(names(&window), ["large", "note", "small"]);
    assert_eq!(disk(&window).get_phase(), ui::DiskPhase::Complete);
    assert_eq!(disk(&window).get_trail().row_count(), 1);
    assert!(!shows(&window, "Que voulez-vous analyser ?"));
    let shown_lines = lines(&window);
    let first = &shown_lines[0];
    assert!(
        first.percent > 50 && first.percent < 100,
        "of the folder, not of the largest: {}",
        first.percent
    );
    assert!((first.bar - 1.0).abs() < f32::EPSILON, "the largest fills");
    assert!(shown_lines[1].bar < 1.0 && shown_lines[1].bar > shown_lines[2].bar);
    // What a screen reader says of a line: its name, size, share, kind.
    let spoken = ElementHandle::find_by_element_type_name(&window, "Row")
        .filter_map(|row| row.accessible_label())
        .map(|label| label.to_string())
        .find(|label| label.starts_with("large, "))
        .unwrap();
    let parts: Vec<&str> = spoken.split(", ").collect();
    assert_eq!(parts.len(), 4, "{spoken}");
    assert_eq!(parts[2], format!("{} %", first.percent), "{spoken}");
    assert_eq!(parts[3], "dossier", "{spoken}");
    // Nothing here deletes, the panel open.
    disk(&window).invoke_choose(0);
    let buttons: Vec<String> = ElementHandle::find_by_element_type_name(&window, "ActionButton")
        .chain(ElementHandle::find_by_element_type_name(
            &window,
            "IconButton",
        ))
        .filter_map(|element| element.accessible_label())
        .map(|label| label.to_lowercase())
        .collect();
    assert!(buttons.contains(&"ouvrir dans le gestionnaire de fichiers".to_owned()));
    for word in ["supprim", "effac", "vider", "corbeille", "nettoyer"] {
        assert!(
            buttons.iter().all(|label| !label.contains(word)),
            "{word}: {buttons:?}"
        );
    }
}

#[test]
fn folders_are_entered_and_left_with_their_files_and_the_trail() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    disk(&window).invoke_enter(index_of(&window, "small"));
    read_until(&window, |window| names(window) == ["data"]);
    let trail: Vec<String> = disk(&window)
        .get_trail()
        .iter()
        .map(|name| name.to_string())
        .collect();
    assert_eq!(trail, ["", "small"]);
    disk(&window).invoke_up();
    read_until(&window, |window| names(window).len() == 3);
    let current = usize::try_from(disk(&window).get_current()).unwrap();
    assert_eq!(names(&window)[current], "small", "back on the folder left");
    disk(&window).invoke_enter(index_of(&window, "large"));
    read_until(&window, |window| names(window) == ["inner"]);
    disk(&window).invoke_enter(0);
    read_until(&window, |window| names(window) == ["data"]);
    assert_eq!(disk(&window).get_trail().row_count(), 3);
    disk(&window).invoke_go_to(1);
    read_until(&window, |window| names(window) == ["inner"]);
    assert_eq!(disk(&window).get_trail().row_count(), 2);
    disk(&window).invoke_go_to(0);
    read_until(&window, |window| names(window).len() == 3);
    assert_eq!(disk(&window).get_trail().row_count(), 1);
}

#[test]
fn a_folder_changed_since_the_analysis_says_so() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    fs::remove_dir_all(home.path().join("small")).unwrap();
    disk(&window).invoke_enter(index_of(&window, "small"));
    read_until(&window, |window| {
        disk(window).get_files_state() == ui::DiskFiles::Changed
    });
    disk(&window).invoke_up();
    assert_eq!(disk(&window).get_files_state(), ui::DiskFiles::Listed);
}

/// Analyzed, then back at the top from `small`, the list without the focus:
/// its folders shown at once, its files on their way.
fn back_at_the_top_its_files_coming(window: &ui::AppWindow) {
    analyzed(window);
    disk(window).invoke_enter(index_of(window, "small"));
    read_until(window, |window| names(window) == ["data"]);
    disk(window).invoke_list_focus(false);
    disk(window).invoke_up();
    disk(window).set_current(0);
    assert_eq!(names(window), ["large", "small"]);
}

#[test]
fn a_line_held_keeps_its_rank_when_the_files_come() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    back_at_the_top_its_files_coming(&window);
    disk(&window).invoke_point(1);
    read_until(&window, |window| names(window).len() == 3);
    assert_eq!(
        names(&window),
        ["large", "small", "note"],
        "small pointed at"
    );
}

#[test]
fn the_sort_put_off_by_the_pointer_is_made_once_it_leaves() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    back_at_the_top_its_files_coming(&window);
    disk(&window).invoke_point(1);
    read_until(&window, |window| names(window).len() == 3);
    assert_eq!(names(&window), ["large", "small", "note"]);
    disk(&window).invoke_point(-1);
    read_once();
    assert_eq!(
        names(&window),
        ["large", "note", "small"],
        "sorted at the next reading, not two seconds later"
    );
}

#[test]
fn a_line_pointed_at_that_cannot_be_held_leaves_the_focus_line_held() {
    let home = home();
    let closed = home.path().join("closed");
    fs::create_dir(&closed).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&closed).is_ok() {
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipped: root reads every folder");
        return;
    }
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(names(&window), ["large", "note", "small", "closed"]);
    disk(&window).invoke_list_focus(true);
    disk(&window).invoke_enter(index_of(&window, "small"));
    read_until(&window, |window| names(window) == ["data"]);
    disk(&window).invoke_up();
    assert_eq!(names(&window), ["large", "small", "closed"]);
    // Unreadable, without a size: never held, the focus holds `small`.
    disk(&window).invoke_point(index_of(&window, "closed"));
    read_until(&window, |window| names(window).len() == 4);
    assert_eq!(names(&window), ["large", "small", "note", "closed"]);
}

/// The screen alone on a window, kept: what it holds can be looked at.
fn screen_alone(seams: DiskSeams) -> (ui::AppWindow, DiskUsageScreen) {
    i_slint_backend_testing::init_no_event_loop();
    let window = ui::AppWindow::new().unwrap();
    let screen = DiskUsageScreen::attach(&window, seams, Language::French);
    (window, screen)
}

#[test]
fn a_refusal_at_the_start_lets_the_analysis_running_go() {
    let home = home();
    let manager = Rc::default();
    let (window, screen) = screen_alone(seams(&home, &manager).disk);
    disk(&window).invoke_analyze_home();
    read_until(&window, |window| step(window) != ui::DiskStep::Opening);
    assert!(screen.0.borrow().scan.is_some());
    // « Analyser ce disque » on a line that is no disk offered.
    disk(&window).invoke_analyze_mount(0);
    assert_eq!(step(&window), ui::DiskStep::Start);
    assert_eq!(disk(&window).get_start_refused(), ui::DiskRefusal::Volume);
    assert!(
        screen.0.borrow().scan.is_none(),
        "the analysis given up, stopped"
    );
}

#[test]
fn files_that_never_come_are_given_up_after_their_wait_not_before() {
    let home = home();
    let mut seams = seams(&home, &Rc::default());
    // The files of `small` come only once the test is over.
    let (_over, stuck) = mpsc::channel::<()>();
    let stuck = Mutex::new(stuck);
    seams.disk.files = Arc::new(move |scan: &UsageScan, folder: NodeId| {
        if scan
            .path(folder)
            .is_some_and(|path| path.ends_with("small"))
        {
            let _ = stuck.lock().unwrap().recv();
        }
        scan.files(folder)
    });
    let (window, _sampler) = on_disk(seams);
    analyzed(&window);
    disk(&window).invoke_enter(index_of(&window, "small"));
    for _ in 1..FILES_WAIT.as_millis() / READ_EVERY.as_millis() {
        read_once();
    }
    assert_eq!(
        disk(&window).get_files_state(),
        ui::DiskFiles::Listed,
        "still awaited"
    );
    read_once();
    assert_eq!(
        disk(&window).get_files_state(),
        ui::DiskFiles::Unreadable,
        "given up"
    );
}

#[test]
fn the_line_with_the_focus_keeps_its_rank_and_the_focus_follows_it() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    disk(&window).invoke_list_focus(true);
    disk(&window).invoke_enter(index_of(&window, "small"));
    read_until(&window, |window| names(window) == ["data"]);
    disk(&window).invoke_up();
    assert_eq!(disk(&window).get_current(), 1, "on small, left");
    read_until(&window, |window| names(window).len() == 3);
    assert_eq!(
        names(&window),
        ["large", "small", "note"],
        "small has the focus"
    );
    // Unfocused, the list is sorted again, the focus line followed.
    disk(&window).invoke_list_focus(false);
    disk(&window).invoke_enter(index_of(&window, "small"));
    read_until(&window, |window| names(window) == ["data"]);
    disk(&window).invoke_up();
    read_until(&window, |window| names(window).len() == 3);
    assert_eq!(names(&window), ["large", "note", "small"]);
    assert_eq!(disk(&window).get_current(), 2, "the focus followed small");
}

#[test]
fn the_small_lines_are_gathered_in_others_unfolded_on_enter() {
    let home = home();
    fs::write(home.path().join("tiny1"), b"1").unwrap();
    fs::write(home.path().join("tiny2"), b"2").unwrap();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    let others = |window: &ui::AppWindow| {
        lines(window)
            .iter()
            .position(|line| line.kind == ui::DiskLineKind::Others)
    };
    read_until(&window, |window| others(window).is_some());
    let at = others(&window).unwrap();
    assert_eq!(lines(&window)[at].count, 2);
    assert_eq!(lines(&window).len(), 4);
    disk(&window).invoke_enter(shown(at));
    let unfolded = lines(&window);
    assert_eq!(unfolded.len(), 6);
    assert!(unfolded[at].expanded);
    assert!(unfolded[at + 1].indented && unfolded[at + 2].indented);
    let mut tiny = vec![
        unfolded[at + 1].name.to_string(),
        unfolded[at + 2].name.to_string(),
    ];
    tiny.sort();
    assert_eq!(tiny, ["tiny1", "tiny2"]);
    disk(&window).invoke_enter(shown(at));
    assert_eq!(lines(&window).len(), 4);
}

/// A mount of `point` on the device `minor`.
fn mount(minor: u32, point: PathBuf) -> Mount {
    Mount {
        id: minor,
        device: Device { major: 8, minor },
        mount_point: point,
        filesystem: "ext4".to_owned(),
        source: "/dev/sda".to_owned(),
        class: MountClass::Block,
    }
}

#[test]
fn a_volume_mounted_once_tells_the_space_unexplained_at_its_root() {
    let home = home();
    let manager = Rc::default();
    let mounts = Rc::new(RefCell::new(Some(MountList {
        mounts: vec![mount(1, root(&home))],
        malformed: 0,
    })));
    let (window, sampler) = on_disk(seams_with(&home, &manager, true, &mounts));
    // The filesystem's reserve is taken, never used nor available.
    let (taken, used) = (700 * GB, 600 * GB);
    send_volumes(
        &sampler,
        Ok(vec![Volume {
            mount_point: root(&home),
            kind: VolumeKind::Local,
            space: VolumeSpace::Known(Space {
                total: 1000 * GB,
                available: 300 * GB,
                used,
            }),
            ..volume("home", VolumeKind::Local, SPACE)
        }]),
    );
    read_until(&window, |window| {
        disk(window).get_volumes_state() == ui::DiskVolumes::Ready
    });
    disk(&window).invoke_analyze_volume(0);
    read_until(&window, |window| {
        step(window) == ui::DiskStep::Result && names(window).contains(&"note".to_owned())
    });
    let last = lines(&window).pop().unwrap();
    assert_eq!(last.kind, ui::DiskLineKind::Unexplained);
    // What the analysis saw is a few megabytes: under a tenth of a gigabyte.
    assert_eq!(last.size, size(taken - 1));
    assert_ne!(last.size, size(used));
    assert!(!disk(&window).get_unexplained_untold());
    disk(&window).invoke_enter(index_of(&window, "large"));
    read_until(&window, |window| names(window) == ["inner"]);
    assert!(
        lines(&window)
            .iter()
            .all(|line| line.kind != ui::DiskLineKind::Unexplained),
        "said at the root only"
    );
    // The same disk mounted twice: what it holds elsewhere is not seen.
    *mounts.borrow_mut() = Some(MountList {
        mounts: vec![mount(1, root(&home)), mount(1, PathBuf::from("/elsewhere"))],
        malformed: 0,
    });
    disk(&window).invoke_change();
    disk(&window).invoke_analyze_volume(0);
    read_until(&window, |window| {
        step(window) == ui::DiskStep::Result && names(window).contains(&"note".to_owned())
    });
    assert!(
        lines(&window)
            .iter()
            .all(|line| line.kind != ui::DiskLineKind::Unexplained)
    );
    assert!(disk(&window).get_unexplained_untold());
}

#[test]
fn stopping_an_analysis_that_opens_waits_for_nothing() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    button(&window, "Analyser").invoke_accessible_default_action();
    assert_eq!(step(&window), ui::DiskStep::Opening);
    disk(&window).invoke_stop();
    assert_eq!(step(&window), ui::DiskStep::Start, "nothing waited for");
    // The analysis given up never shows, however late it opens.
    std::thread::sleep(Duration::from_millis(100));
    read_once();
    assert_eq!(step(&window), ui::DiskStep::Start);
}

#[test]
fn the_line_chosen_opens_in_the_file_manager_never_twice_at_once() {
    let home = home();
    std::os::unix::fs::symlink(home.path().join("small"), home.path().join("link")).unwrap();
    let manager: Rc<FileManager> = Rc::default();
    let mounts = Rc::new(RefCell::new(None));
    let (window, _sampler) = on_disk(seams_with(&home, &manager, false, &mounts));
    analyzed(&window);
    read_until(&window, |window| names(window).contains(&"link".to_owned()));
    let identity = |path: &Path| okapi_platform::open_path(path, true).unwrap().1.identity;
    disk(&window).invoke_choose(index_of(&window, "large"));
    let detail = disk(&window).get_detail();
    assert_eq!(detail.name, "large");
    assert!(detail.cache, "a folder of caches is sent to the Cleaner");
    assert!(detail.openable);
    disk(&window).invoke_open_chosen();
    disk(&window).invoke_open_chosen();
    assert_eq!(manager.asked.borrow().len(), 1, "one call at a time");
    assert!(disk(&window).get_opening());
    let request = manager.asked.borrow()[0].clone();
    assert_eq!(request.path, root(&home).join("large"));
    assert!(request.folder);
    assert_eq!(
        request.expected,
        Some(identity(&root(&home).join("large"))),
        "the folder read, checked again"
    );
    manager.answer(true);
    assert!(!disk(&window).get_opening());
    assert!(!disk(&window).get_open_refused());
    // A file is chosen in its folder.
    disk(&window).invoke_choose(index_of(&window, "note"));
    assert!(!disk(&window).get_detail().cache);
    disk(&window).invoke_open_chosen();
    let request = manager.asked.borrow()[1].clone();
    assert!(!request.folder);
    assert_eq!(request.path, root(&home).join("note"));
    assert_eq!(request.expected, None);
    manager.answer(false);
    assert!(disk(&window).get_open_refused(), "a refusal is said");
    disk(&window).invoke_choose(index_of(&window, "small"));
    assert!(!disk(&window).get_open_refused(), "of the line chosen only");
    // A link is never followed: its folder is opened.
    disk(&window).invoke_choose(index_of(&window, "link"));
    disk(&window).invoke_open_chosen();
    let request = manager.asked.borrow()[2].clone();
    assert_eq!(request.path, root(&home));
    assert!(request.folder);
    assert_eq!(request.expected, Some(identity(&root(&home))));
    // A file manager that never answers frees the button, saying nothing
    // it does not know.
    i_slint_backend_testing::mock_elapsed_time(crate::file_manager::OPEN_WAIT);
    slint::platform::update_timers_and_animations();
    assert!(!disk(&window).get_opening());
    assert!(!disk(&window).get_open_refused());
    disk(&window).invoke_open_chosen();
    assert_eq!(manager.asked.borrow().len(), 4, "asked again");
    manager.answer_oldest(false);
    assert!(
        disk(&window).get_opening() && !disk(&window).get_open_refused(),
        "the answer of a call no longer awaited is dropped"
    );
    manager.answer(true);
    assert!(!disk(&window).get_opening());
}

#[test]
fn a_file_manager_refusing_at_once_is_said() {
    let home = home();
    let mut seams = seams(&home, &Rc::default());
    // Refused before any call: a folder replaced, a portal not reached.
    seams.disk.open = Rc::new(|_, done| done(false));
    let (window, _sampler) = on_disk(seams);
    analyzed(&window);
    disk(&window).invoke_choose(index_of(&window, "large"));
    disk(&window).invoke_open_chosen();
    assert!(!disk(&window).get_opening());
    assert!(disk(&window).get_open_refused());
}

#[test]
fn a_folder_that_cannot_be_opened_is_said_on_the_start() {
    let home = home();
    let manager = Rc::default();
    let gone = home.path().join("gone");
    let seams = Seams {
        disk: DiskSeams {
            home: Some(gone),
            ..seams(&home, &manager).disk
        },
        ..seams_never_scanning()
    };
    let (window, _sampler) = on_disk(seams);
    button(&window, "Analyser").invoke_accessible_default_action();
    read_until(&window, |window| step(window) == ui::DiskStep::Start);
    assert_eq!(disk(&window).get_start_refused(), ui::DiskRefusal::Folder);
    assert!(shows(
        &window,
        "Ce dossier n'a pas pu être ouvert : rien n'a été analysé."
    ));
}

#[test]
fn analyzed_again_the_home_folder_shows_what_it_holds_now() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    fs::write(home.path().join("new"), noise(4_000_000)).unwrap();
    disk(&window).invoke_again();
    read_until(&window, |window| {
        step(window) == ui::DiskStep::Result && names(window).contains(&"new".to_owned())
    });
    // The focus the screen gave the list holds nothing.
    assert_eq!(names(&window), ["new", "large", "note", "small"]);
    assert_eq!(disk(&window).get_start_refused(), ui::DiskRefusal::None);
}

#[test]
fn a_volume_no_longer_read_is_not_analyzed_again() {
    let home = home();
    let manager = Rc::default();
    let (window, sampler) = on_disk(seams(&home, &manager));
    let mounted = Volume {
        mount_point: root(&home),
        ..volume("home", VolumeKind::Local, SPACE)
    };
    send_volumes(&sampler, Ok(vec![mounted]));
    read_until(&window, |window| {
        disk(window).get_volumes_state() == ui::DiskVolumes::Ready
    });
    disk(&window).invoke_analyze_volume(0);
    read_until(&window, |window| {
        step(window) == ui::DiskStep::Result && names(window).contains(&"note".to_owned())
    });
    // Failing since: still mounted, its space unknown.
    send_volumes(
        &sampler,
        Ok(vec![Volume {
            mount_point: root(&home),
            ..volume("home", VolumeKind::Local, VolumeSpace::NotResponding)
        }]),
    );
    disk(&window).invoke_again();
    assert_eq!(step(&window), ui::DiskStep::Start);
    assert_eq!(disk(&window).get_start_refused(), ui::DiskRefusal::Volume);
    assert_eq!(
        disk(&window).get_volumes().row_count(),
        0,
        "the volumes read again"
    );
}

fn press(window: &ui::AppWindow, key: slint::platform::Key) {
    let text = SharedString::from(key);
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
}

#[test]
fn the_pages_move_the_focus_by_what_the_list_shows() {
    let home = home();
    fs::create_dir(home.path().join("many")).unwrap();
    for index in 0..60 {
        fs::write(
            home.path().join(format!("many/file{index:02}")),
            noise(8_000),
        )
        .unwrap();
    }
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    disk(&window).invoke_enter(index_of(&window, "many"));
    read_until(&window, |window| names(window).len() == 60);
    press(&window, slint::platform::Key::PageDown);
    let page = disk(&window).get_current();
    assert!(page > 1 && page < 30, "a page of lines: {page}");
    press(&window, slint::platform::Key::PageDown);
    assert_eq!(disk(&window).get_current(), 2 * page);
    press(&window, slint::platform::Key::PageUp);
    assert_eq!(disk(&window).get_current(), page);
    press(&window, slint::platform::Key::PageUp);
    press(&window, slint::platform::Key::PageUp);
    assert_eq!(disk(&window).get_current(), 0, "never above the first");
}

#[test]
fn a_name_cut_short_is_said_whole_under_the_focus() {
    let home = home();
    let long = "a".repeat(200);
    fs::write(home.path().join(&long), b"1").unwrap();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    read_until(&window, |window| names(window).contains(&long));
    let tips = || ElementHandle::find_by_element_type_name(&window, "NameTip").count();
    assert_eq!(tips(), 0);
    press(&window, slint::platform::Key::End);
    assert_eq!(names(&window).last(), Some(&long));
    // `changed` handlers run on the next turn of the event loop.
    slint::platform::update_timers_and_animations();
    assert_eq!(tips(), 1, "its name said whole");
    press(&window, slint::platform::Key::Home);
    slint::platform::update_timers_and_animations();
    assert_eq!(tips(), 0, "a name shown whole needs none");
}

#[test]
fn others_says_its_count_in_the_singular_and_the_plural() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    disk(&window).set_step(ui::DiskStep::Result);
    let others = |count| ui::DiskLine {
        kind: ui::DiskLineKind::Others,
        has_size: true,
        tall: true,
        count,
        ..ui::DiskLine::default()
    };
    disk(&window).set_lines(ModelRc::new(VecModel::from(vec![others(1), others(2)])));
    slint::platform::update_timers_and_animations();
    let says = |text: &str| {
        ElementHandle::find_by_element_type_name(&window, "Row")
            .filter_map(|row| row.accessible_label())
            .any(|label| label.contains(text))
    };
    assert!(says("1 élément, moins de 1 % chacun"));
    assert!(says("2 éléments, moins de 1 % chacun"));
    slint::select_bundled_translation("en").unwrap();
    assert!(says("1 item, under 1% each"));
    assert!(says("2 items, under 1% each"));
}

#[test]
fn a_partial_size_is_said_at_least_once_the_analysis_stopped() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    disk(&window).set_step(ui::DiskStep::Result);
    let partial = ui::DiskLine {
        kind: ui::DiskLineKind::Folder,
        state: ui::DiskLineState::Partial,
        name: "partiel".into(),
        has_size: true,
        size: ui::Size {
            whole: 1,
            tenths: 0,
            has_tenths: true,
            unit: ui::SizeUnit::Gigabyte,
        },
        ..ui::DiskLine::default()
    };
    disk(&window).set_lines(ModelRc::new(VecModel::from(vec![partial])));
    disk(&window).set_phase(ui::DiskPhase::Running);
    slint::platform::update_timers_and_animations();
    let stopped = "Analyse arrêtée : tailles partielles. Les dossiers pas encore parcourus ne sont pas comptés.";
    // Running: its icon says it grows, no banner.
    assert!(shows(&window, "En cours, la taille va encore grandir"));
    assert!(!shows(&window, stopped));
    disk(&window).set_phase(ui::DiskPhase::Stopped);
    slint::platform::update_timers_and_animations();
    // Stopped: no icon, the banner says it for the screen and « au moins »
    // by its size, read aloud with its line (the size column, read with its
    // line, is no element a test reaches: the snapshot shows it).
    assert!(!shows(&window, "En cours, la taille va encore grandir"));
    assert!(!shows(
        &window,
        "Taille partielle : l'analyse a été arrêtée"
    ));
    assert!(shows(&window, stopped));
    assert!(
        ElementHandle::find_by_element_type_name(&window, "Row")
            .filter_map(|row| row.accessible_label())
            .any(|label| label.starts_with("partiel, au moins 1,0 Go, ")
                && label.contains("Taille partielle : l'analyse a été arrêtée"))
    );
}

#[test]
fn of_the_states_a_hard_link_alone_is_said_below_its_name() {
    let home = home();
    fs::hard_link(home.path().join("note"), home.path().join("twin")).unwrap();
    let closed = home.path().join("closed");
    fs::create_dir(&closed).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&closed).is_ok() {
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipped: root reads every folder");
        return;
    }
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    let shown_lines = lines(&window);
    for state in [
        ui::DiskLineState::CountedElsewhere,
        ui::DiskLineState::Unreadable,
    ] {
        assert!(shown_lines.iter().any(|line| line.state == state));
    }
    // Every other state is an icon beside the name: the line stays low
    // (« Autres », tall for its count, none here).
    for line in &shown_lines {
        assert_ne!(line.kind, ui::DiskLineKind::Others);
        assert_eq!(
            line.tall,
            line.state == ui::DiskLineState::CountedElsewhere,
            "{}",
            line.name
        );
    }
}

#[test]
fn folders_are_entered_and_left_with_the_keyboard() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    analyzed(&window);
    // The list has the focus once the analysis ends.
    press(&window, slint::platform::Key::DownArrow);
    press(&window, slint::platform::Key::DownArrow);
    assert_eq!(names(&window)[2], "small");
    assert_eq!(disk(&window).get_current(), 2);
    press(&window, slint::platform::Key::Return);
    read_until(&window, |window| names(window) == ["data"]);
    press(&window, slint::platform::Key::Backspace);
    read_until(&window, |window| names(window).len() == 3);
    assert_eq!(
        names(&window),
        ["large", "small", "note"],
        "the line the user is on keeps its place"
    );
    assert_eq!(disk(&window).get_current(), 1, "back on the folder left");
    press(&window, slint::platform::Key::Space);
    assert_eq!(
        disk(&window).get_detail().name,
        "small",
        "chosen for the panel"
    );
}

#[test]
fn escape_stops_the_analysis_its_stop_button_focused() {
    let home = home();
    let manager = Rc::default();
    let (window, _sampler) = on_disk(seams(&home, &manager));
    button(&window, "Analyser").invoke_accessible_default_action();
    assert_eq!(step(&window), ui::DiskStep::Opening);
    // Shown, « Arrêter » takes the focus: Escape reaches the page.
    button(&window, "Arrêter");
    press(&window, slint::platform::Key::Escape);
    assert_eq!(step(&window), ui::DiskStep::Start);
}

#[test]
fn the_volume_chosen_stays_chosen_across_a_gap_in_the_readings() {
    let home = home();
    let manager = Rc::default();
    let (window, sampler) = on_disk(seams(&home, &manager));
    let usb = |name: &str| volume(name, VolumeKind::Removable, SPACE);
    send_volumes(&sampler, Ok(vec![usb("a"), usb("b")]));
    read_once();
    disk(&window).set_choice(1);
    // Away and back: the list is read again, not there for a while.
    button(&window, "Accueil").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    button(&window, "Espace disque").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    assert_eq!(disk(&window).get_volumes_state(), ui::DiskVolumes::Reading);
    assert_eq!(disk(&window).get_choice(), 1, "nothing known to change it");
    send_volumes(&sampler, Ok(vec![usb("b")]));
    read_once();
    assert_eq!(disk(&window).get_choice(), 0, "b, wherever it now is");
    send_volumes(&sampler, Ok(vec![usb("c")]));
    read_once();
    assert_eq!(disk(&window).get_choice(), -1, "gone: the home folder");
}

#[test]
fn a_volume_analyzed_again_is_read_again() {
    let home = home();
    let manager: Rc<FileManager> = Rc::default();
    let mounts = Rc::new(RefCell::new(None));
    let (window, sampler) = on_disk(seams_with(&home, &manager, false, &mounts));
    let mounted = |total| Volume {
        mount_point: root(&home),
        space: VolumeSpace::Known(Space {
            total,
            available: total / 2,
            used: total / 2,
        }),
        ..volume("home", VolumeKind::Local, SPACE)
    };
    send_volumes(
        &sampler,
        Ok(vec![
            volume("a", VolumeKind::Removable, SPACE),
            mounted(64 * GB),
        ]),
    );
    read_once();
    disk(&window).invoke_analyze_volume(1);
    read_until(&window, |window| {
        step(window) == ui::DiskStep::Result && names(window).contains(&"note".to_owned())
    });
    assert_eq!(
        disk(&window).get_target_path(),
        root(&home).display().to_string()
    );
    assert_eq!(disk(&window).get_total(), size(64 * GB));
    // A call to the file manager awaited: given up with the analysis.
    disk(&window).invoke_choose(index_of(&window, "large"));
    disk(&window).invoke_open_chosen();
    assert!(disk(&window).get_opening());
    send_volumes(&sampler, Ok(vec![mounted(128 * GB)]));
    disk(&window).invoke_again();
    assert!(!disk(&window).get_opening(), "no longer awaited");
    read_until(&window, |window| {
        step(window) == ui::DiskStep::Result && names(window).contains(&"note".to_owned())
    });
    assert_eq!(
        disk(&window).get_total(),
        size(128 * GB),
        "its space read again"
    );
    manager.answer(false);
    assert!(
        !disk(&window).get_open_refused(),
        "the answer of the analysis before"
    );
    disk(&window).invoke_choose(index_of(&window, "large"));
    disk(&window).invoke_open_chosen();
    assert_eq!(manager.asked.borrow().len(), 2);
}
