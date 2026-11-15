//! The applications screen in a window, its sources and its uninstaller
//! doubled: nothing is read from the machine, nothing uninstalled.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use okapi_tools::apps::dpkg::{Status, parse_status};
use okapi_tools::apps::measure::Measured;
use okapi_tools::apps::{App, AppSize, Origin};
use slint::Model as _;

use super::*;
use crate::cleaner::tests::{button, checkbox, press, seams_never_scanning, shows};

/// How long a test waits for a thread of the screen: a fault fails, it
/// never hangs.
const WAIT: Duration = Duration::from_secs(20);

/// Seams that read nothing and uninstall nothing.
pub(crate) fn inert() -> AppsSeams {
    AppsSeams {
        read: Rc::new(|_, _, _| {}),
        uninstaller: Fake::new(Vec::new(), Ended::Done),
        record: Arc::new(|_| Ok(())),
        now: Timestamp::now,
    }
}

fn app(source: Source, id: &str, name: &str, bytes: u64) -> App {
    let package = id.split(':').next().unwrap_or(id).to_owned();
    App {
        source,
        id: id.to_owned(),
        package: package.clone(),
        name: name.to_owned(),
        version: "1.0".to_owned(),
        publisher: None,
        origin: (source == Source::Deb).then_some(Origin::ByUser),
        icon: None,
        size: AppSize::Known(bytes),
        old_bytes: 0,
        old_versions: 0,
        data: (source == Source::Snap).then(|| PathBuf::from(format!("/home/u/snap/{id}"))),
        system: false,
        target: match source {
            Source::Deb => Some(Target::Deb {
                package,
                architecture: "amd64".to_owned(),
            }),
            Source::Snap => Some(Target::Snap {
                name: id.to_owned(),
                purge: false,
            }),
            Source::Flatpak => None,
        },
    }
}

/// The machine of these tests: GIMP (a deb, 3 MB), Files (the system's),
/// Android Studio (a snap, 1 MB, its data 5 MB measured after, an old
/// version of 1 MB), Builder (a flatpak, 2 MB measured).
fn arrivals() -> Vec<Arrival> {
    let studio = App {
        old_bytes: 1_000_000,
        old_versions: 1,
        publisher: Some("Snapcrafters".to_owned()),
        ..app(Source::Snap, "android-studio", "Android Studio", 1_000_000)
    };
    let files = App {
        system: true,
        origin: Some(Origin::WithSystem),
        ..app(Source::Deb, "nautilus:amd64", "Fichiers", 500_000)
    };
    let builder = App {
        size: AppSize::ToMeasure(PathBuf::from("/var/lib/flatpak/app/b")),
        ..app(Source::Flatpak, "org.gnome.Builder", "Builder", 0)
    };
    vec![
        Arrival::Source(
            Source::Deb,
            SourceReading::Read {
                apps: vec![app(Source::Deb, "gimp:amd64", "GIMP", 3_000_000), files],
                rest: Rest {
                    count: 2026,
                    bytes: 9_400_000_000,
                },
            },
        ),
        Arrival::Source(
            Source::Snap,
            SourceReading::Read {
                apps: vec![studio.clone()],
                rest: Rest {
                    count: 6,
                    bytes: 1_800_000_000,
                },
            },
        ),
        Arrival::Measured(
            studio.key(),
            Part::Data,
            Measured::Bytes {
                bytes: 5_000_000,
                partial: false,
            },
        ),
        Arrival::Source(
            Source::Flatpak,
            SourceReading::Read {
                apps: vec![builder.clone()],
                rest: Rest::default(),
            },
        ),
        Arrival::Measured(
            builder.key(),
            Part::App,
            Measured::Bytes {
                bytes: 2_000_000,
                partial: false,
            },
        ),
    ]
}

const STATUS: &str = "Package: gimp\nStatus: install ok installed\nArchitecture: amd64\n\n\
     Package: gimp-help-fr\nStatus: install ok installed\nArchitecture: amd64\n\n\
     Package: gimp-plugin\nStatus: install ok installed\nArchitecture: amd64\n\n\
     Package: ubuntu-desktop-minimal\nStatus: install ok installed\nArchitecture: amd64\nSection: metapackages\n";

/// An uninstaller that simulates in turn what it is given, and keeps what
/// it was asked to remove.
pub(crate) struct Fake {
    simulations: Mutex<Vec<Result<Vec<Going>, Why>>>,
    ended: Ended,
    simulated: AtomicUsize,
    removed: Mutex<Vec<(Target, Vec<Going>)>>,
}

impl Fake {
    fn new(simulations: Vec<Result<Vec<Going>, Why>>, ended: Ended) -> Arc<Self> {
        Arc::new(Self {
            simulations: Mutex::new(simulations),
            ended,
            simulated: AtomicUsize::new(0),
            removed: Mutex::new(Vec::new()),
        })
    }

    fn removed(&self) -> Vec<(Target, Vec<Going>)> {
        self.removed.lock().unwrap().clone()
    }
}

impl Uninstaller for Fake {
    fn simulate(&self, _target: &Target) -> Result<Vec<Going>, Why> {
        self.simulated.fetch_add(1, Ordering::Relaxed);
        let mut simulations = self.simulations.lock().unwrap();
        if simulations.len() > 1 {
            simulations.remove(0)
        } else {
            simulations.first().cloned().unwrap_or(Err(Why::Absent))
        }
    }

    fn status(&self) -> Result<Status, Why> {
        Ok(parse_status(STATUS))
    }

    fn remove(
        &self,
        target: &Target,
        going: &[Going],
        steps: &Sender<Step>,
        _cancel: &AtomicBool,
    ) -> Ended {
        self.removed
            .lock()
            .unwrap()
            .push((target.clone(), going.to_vec()));
        let _ = steps.send(Step::Cancellable(true));
        let _ = steps.send(Step::Cancellable(false));
        self.ended.clone()
    }
}

fn going(name: &str, bytes: u64) -> Going {
    Going {
        id: format!("{name};1;amd64;installed"),
        name: name.to_owned(),
        architecture: "amd64".to_owned(),
        bytes: Some(bytes),
    }
}

/// The window on the applications screen, its screen, the history lines
/// written, and how many times the sources were read.
type OnApps = (
    ui::AppWindow,
    AppsScreen,
    Arc<Mutex<Vec<UninstallEntry>>>,
    Rc<std::cell::Cell<usize>>,
);

/// The window on the applications screen, reading `arrivals` (sent at
/// once) and uninstalling with `uninstaller`.
fn on_apps(uninstaller: Arc<dyn Uninstaller>) -> OnApps {
    on_apps_recording(uninstaller, true)
}

/// [`on_apps`], its history written when `writable`.
fn on_apps_recording(uninstaller: Arc<dyn Uninstaller>, writable: bool) -> OnApps {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&recorded);
    let reads = Rc::new(std::cell::Cell::new(0));
    let counted = Rc::clone(&reads);
    let seams = crate::cleaner::Seams {
        apps: AppsSeams {
            read: Rc::new(move |_, sink, _| {
                counted.set(counted.get() + 1);
                for arrival in arrivals() {
                    sink.send(arrival).unwrap();
                }
            }),
            uninstaller,
            record: Arc::new(move |entry| {
                if !writable {
                    return Err(HistoryUnavailable::NoDirectory);
                }
                kept.lock().unwrap().push(entry.clone());
                Ok(())
            }),
            now: Timestamp::now,
        },
        ..seams_never_scanning()
    };
    let (window, _, _, _, screen) =
        crate::wide_window(seams, crate::dashboard::tests::inert(), Language::French);
    button(&window, "Applications").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    read_once();
    (window, screen, recorded, reads)
}

fn read_once() {
    i_slint_backend_testing::mock_elapsed_time(READ_EVERY);
    slint::platform::update_timers_and_animations();
}

/// The screen's readings, until `done`.
fn read_until(window: &ui::AppWindow, done: impl Fn(&ui::AppWindow) -> bool) {
    let started = Instant::now();
    while !done(window) {
        assert!(started.elapsed() < WAIT, "never came");
        read_once();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn apps(window: &ui::AppWindow) -> ui::Apps<'_> {
    window.global::<ui::Apps>()
}

fn names(window: &ui::AppWindow) -> Vec<String> {
    apps(window)
        .get_lines()
        .iter()
        .map(|line| line.name.to_string())
        .collect()
}

fn step(window: &ui::AppWindow) -> ui::UninstallStep {
    apps(window).get_step()
}

/// The buttons labelled `label` shown.
fn buttons(window: &ui::AppWindow, label: &str) -> usize {
    ElementHandle::find_by_accessible_label(window, label)
        .filter(|element| element.accessible_role() == Some(AccessibleRole::Button))
        .count()
}

/// Opens the detail of the line named `name`.
fn open(window: &ui::AppWindow, name: &str) {
    let index = names(window)
        .iter()
        .position(|shown| shown == name)
        .unwrap();
    apps(window).invoke_choose(i32::try_from(index).unwrap());
}

/// Opens the uninstallation of `name`, until its check is said.
fn ask(window: &ui::AppWindow, name: &str) {
    open(window, name);
    apps(window).invoke_uninstall();
    read_until(window, |window| step(window) != ui::UninstallStep::Checking);
}

#[test]
fn the_list_is_read_when_shown_and_sorted_by_all_each_takes() {
    let (window, _, _, reads) = on_apps(inert().uninstaller);
    assert_eq!(reads.get(), 1);
    // Android Studio: 1 + 5 + 1 MB, above GIMP's 3 MB once its data came.
    assert_eq!(
        names(&window),
        ["Android Studio", "GIMP", "Builder", "Fichiers"]
    );
    assert_eq!(apps(&window).get_app_count(), 4);
    assert_eq!(apps(&window).get_rest_packages(), 2026);
    assert_eq!(apps(&window).get_rest_snaps(), 6);
    assert!(apps(&window).get_has_flatpak());
    assert!(!apps(&window).get_reading());
    let spoken: Vec<String> = ElementHandle::find_by_element_type_name(&window, "AppRow")
        .filter_map(|row| row.accessible_label().map(|label| label.to_string()))
        .collect();
    assert!(
        spoken.contains(
            &"Android Studio, Snap, 1,0 Mo, données 5,0 Mo, anciennes versions 1,0 Mo".to_owned()
        ),
        "{spoken:?}"
    );
    assert!(
        spoken.contains(&"Fichiers, Ubuntu, 500,0 ko, fait partie du système".to_owned()),
        "{spoken:?}"
    );

    // Shown again soon: not read again.
    button(&window, "Accueil").invoke_accessible_default_action();
    button(&window, "Applications").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    assert_eq!(reads.get(), 1);
}

#[test]
fn a_reading_stopped_by_leaving_the_page_is_read_again_on_return() {
    let stopped = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&stopped);
    let reads = Rc::new(std::cell::Cell::new(0));
    let counted = Rc::clone(&reads);
    // A reading whose thread goes on: its sender kept here.
    let senders = Rc::new(RefCell::new(Vec::new()));
    let kept = Rc::clone(&senders);
    let seams = crate::cleaner::Seams {
        apps: AppsSeams {
            read: Rc::new(move |_, sink: &Sender<Arrival>, stop: &Arc<AtomicBool>| {
                counted.set(counted.get() + 1);
                seen.borrow_mut().push(Arc::clone(stop));
                kept.borrow_mut().push(sink.clone());
            }),
            ..inert()
        },
        ..seams_never_scanning()
    };
    let (window, ..) =
        crate::wide_window(seams, crate::dashboard::tests::inert(), Language::French);
    button(&window, "Applications").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    button(&window, "Accueil").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    assert!(
        stopped.borrow()[0].load(Ordering::Relaxed),
        "measuring stopped"
    );
    // The thread ends, stopped.
    senders.borrow_mut().clear();
    read_once();
    button(&window, "Applications").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    assert_eq!(reads.get(), 2, "read again: it was stopped");
}

#[test]
fn the_search_reads_names_and_packages_and_the_filter_the_source() {
    let (window, ..) = on_apps(inert().uninstaller);
    apps(&window).set_search("NAUT".into());
    apps(&window).invoke_searched();
    assert_eq!(names(&window), ["Fichiers"], "by its package");
    apps(&window).set_search("photoshop".into());
    apps(&window).invoke_searched();
    assert_eq!(names(&window), Vec::<String>::new());
    assert!(shows(&window, "Aucune appli ne correspond à « photoshop »"));
    button(&window, "Effacer la recherche").invoke_accessible_default_action();
    assert_eq!(names(&window).len(), 4);
    button(&window, "Snap").invoke_accessible_default_action();
    assert_eq!(names(&window), ["Android Studio"]);
    button(&window, "Ubuntu").invoke_accessible_default_action();
    apps(&window).set_search("gi".into());
    apps(&window).invoke_searched();
    assert_eq!(
        names(&window),
        ["GIMP"],
        "the filter and the search together"
    );
}

#[test]
fn the_keyboard_stays_on_its_application_and_the_system_s_is_never_offered() {
    let (window, screen, ..) = on_apps(inert().uninstaller);
    press(&window, slint::platform::Key::DownArrow);
    // A size come since moves GIMP down: the keyboard follows it.
    screen
        .0
        .borrow_mut()
        .entries
        .get_mut(&AppKey {
            source: Source::Deb,
            id: "gimp:amd64".to_owned(),
        })
        .unwrap()
        .app
        .size = AppSize::Known(1);
    apps(&window).invoke_searched();
    assert_eq!(names(&window)[3], "GIMP");
    assert_eq!(apps(&window).get_current(), 3);
    press(&window, slint::platform::Key::Return);
    assert_eq!(apps(&window).get_detail().name, "GIMP");
    assert!(shows(&window, "Installée par vous"));
    assert_eq!(
        button(&window, "Désinstaller").accessible_enabled(),
        Some(true)
    );
    press(&window, slint::platform::Key::Escape);
    assert_eq!(apps(&window).get_chosen(), -1);

    for (name, words) in [
        (
            "Fichiers",
            "Le bureau a besoin de Fichiers pour fonctionner. Okapi ne propose donc pas de la désinstaller.",
        ),
        ("Builder", "Okapi ne désinstalle pas encore les flatpaks."),
    ] {
        open(&window, name);
        assert_eq!(buttons(&window, "Désinstaller"), 0, "{name}: no button");
        assert!(shows(&window, words), "{name}");
        apps(&window).invoke_uninstall();
        assert_eq!(
            step(&window),
            ui::UninstallStep::Closed,
            "{name}: refused in Rust too"
        );
    }
}

#[test]
fn a_deb_goes_as_shown_and_is_recorded_once_gone() {
    let fake = Fake::new(
        vec![Ok(vec![
            going("gimp", 3_000_000),
            going("gimp-help-fr", 1_000_000),
        ])],
        Ended::Done,
    );
    let (window, _, recorded, reads) = on_apps(fake.clone());
    ask(&window, "GIMP");
    assert_eq!(step(&window), ui::UninstallStep::Confirm);
    assert!(shows(
        &window,
        "GIMP et un paquet qui en dépend partiront. Place libérée : 4,0 Mo."
    ));
    assert!(shows(&window, "Ce qui partirait : gimp, gimp-help-fr"));
    assert!(shows(&window, "Votre mot de passe vous sera demandé."));
    assert_eq!(
        buttons(&window, "Désinstaller"),
        2,
        "the detail's and the window's"
    );
    apps(&window).invoke_confirm();
    read_until(&window, |window| apps(window).get_toast());
    assert_eq!(
        fake.removed(),
        [(
            Target::Deb {
                package: "gimp".to_owned(),
                architecture: "amd64".to_owned()
            },
            vec![going("gimp", 3_000_000), going("gimp-help-fr", 1_000_000)]
        )]
    );
    assert_eq!(step(&window), ui::UninstallStep::Closed);
    assert_eq!(apps(&window).get_toast_name(), "GIMP");
    assert!(apps(&window).get_toast_recorded());
    let lines = recorded.lock().unwrap().clone();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        (
            lines[0].source,
            lines[0].packages.clone(),
            lines[0].freed_bytes,
            lines[0].data_removed
        ),
        (
            UninstallSource::Deb,
            vec!["gimp".to_owned(), "gimp-help-fr".to_owned()],
            Some(4_000_000),
            false
        )
    );
    assert_eq!(reads.get(), 2, "read again after");
}

#[test]
fn what_would_take_the_system_s_is_refused_named_and_never_removed() {
    let fake = Fake::new(
        vec![Ok(vec![
            going("gimp", 1),
            going("ubuntu-desktop-minimal", 1),
        ])],
        Ended::Done,
    );
    let (window, _, recorded, _) = on_apps(fake.clone());
    ask(&window, "GIMP");
    assert_eq!(step(&window), ui::UninstallStep::Refused);
    assert!(shows(&window, "GIMP reste installée"));
    assert!(shows(&window, "Ce qui partirait : ubuntu-desktop-minimal"));
    assert_eq!(buttons(&window, "Désinstaller"), 1, "the detail's alone");
    // A confirmation out of its time does nothing, at once (no thread).
    apps(&window).invoke_confirm();
    assert_eq!(fake.removed(), []);
    assert!(recorded.lock().unwrap().is_empty());
    button(&window, "Fermer").invoke_accessible_default_action();
    assert_eq!(step(&window), ui::UninstallStep::Closed);
}

#[test]
fn a_list_changed_since_it_was_shown_is_shown_again_and_nothing_removed() {
    let fake = Fake::new(
        vec![
            Ok(vec![going("gimp", 3), going("gimp-help-fr", 1)]),
            // As many packages, another one among them.
            Ok(vec![going("gimp", 3), going("gimp-plugin", 1)]),
        ],
        Ended::Done,
    );
    let (window, _, recorded, _) = on_apps(fake.clone());
    ask(&window, "GIMP");
    apps(&window).invoke_confirm();
    read_until(&window, |window| {
        apps(window).get_going().row_data(1).as_deref() == Some("gimp-plugin")
    });
    assert_eq!(step(&window), ui::UninstallStep::Confirm, "asked again");
    assert_eq!(fake.removed(), []);
    assert!(recorded.lock().unwrap().is_empty());
}

/// An uninstallation of GIMP that ends `ended`: its words shown, no
/// history line, the list read again, and tried again from its check.
fn ending_says(ended: Ended, words: &str) {
    let fake = Fake::new(vec![Ok(vec![going("gimp", 1)])], ended);
    let (window, _, recorded, reads) = on_apps(fake);
    ask(&window, "GIMP");
    apps(&window).invoke_confirm();
    read_until(&window, |window| step(window) == ui::UninstallStep::Failed);
    assert!(shows(&window, "GIMP est toujours là"), "{words}");
    assert!(shows(&window, words), "{words}");
    assert!(recorded.lock().unwrap().is_empty(), "no line for a failure");
    assert_eq!(reads.get(), 2, "{words}: read again");
    button(&window, "Réessayer").invoke_accessible_default_action();
    read_until(&window, |window| step(window) == ui::UninstallStep::Confirm);
}

#[test]
fn rights_not_granted_are_said() {
    ending_says(
        Ended::Failed(Why::NotAuthorized),
        "Les droits n'ont pas été accordés.",
    );
}

#[test]
fn another_installation_running_is_said() {
    ending_says(
        Ended::Failed(Why::Busy),
        "Une autre installation est en cours : réessayez dans un moment.",
    );
}

#[test]
fn no_answer_in_time_says_it_may_still_happen() {
    ending_says(
        Ended::Failed(Why::TimedOut),
        "Pas de réponse à temps. Si vous avez accordé les droits, la désinstallation peut se faire quand même.",
    );
}

#[test]
fn the_service_s_own_refusal_is_said_in_its_words() {
    ending_says(
        Ended::Failed(Why::Service("no space".to_owned())),
        "Le système a refusé : no space",
    );
}

#[test]
fn a_cancelled_uninstallation_closes_and_says_nothing_went() {
    let fake = Fake::new(vec![Ok(vec![going("gimp", 1)])], Ended::Cancelled);
    let (window, _, recorded, reads) = on_apps(fake);
    ask(&window, "GIMP");
    apps(&window).invoke_confirm();
    read_until(&window, |_| reads.get() == 2);
    assert_eq!(step(&window), ui::UninstallStep::Closed);
    assert!(!apps(&window).get_toast(), "cancelled: nothing went");
    assert!(recorded.lock().unwrap().is_empty());
}

/// An uninstaller whose removal waits for the test, the package manager
/// held meanwhile.
struct Held {
    fake: Arc<Fake>,
    released: Mutex<Receiver<()>>,
}

impl Uninstaller for Held {
    fn simulate(&self, target: &Target) -> Result<Vec<Going>, Why> {
        self.fake.simulate(target)
    }

    fn status(&self) -> Result<Status, Why> {
        self.fake.status()
    }

    fn remove(
        &self,
        target: &Target,
        going: &[Going],
        steps: &Sender<Step>,
        cancel: &AtomicBool,
    ) -> Ended {
        // Waits for another program, then its turn comes: released twice.
        let _ = steps.send(Step::Waiting(true));
        let _ = self.released.lock().unwrap().recv();
        let _ = steps.send(Step::Waiting(false));
        let _ = self.released.lock().unwrap().recv();
        self.fake.remove(target, going, steps, cancel)
    }
}

#[test]
fn one_uninstallation_at_a_time_its_window_closed_or_not() {
    let (release, released) = mpsc::channel::<()>();
    let fake = Fake::new(vec![Ok(vec![going("gimp", 1)])], Ended::Done);
    let held = Arc::new(Held {
        fake: Arc::clone(&fake),
        released: Mutex::new(released),
    });
    let (window, _, _, reads) = on_apps(held);
    ask(&window, "GIMP");
    apps(&window).invoke_confirm();
    read_until(&window, |window| apps(window).get_waiting());
    assert!(shows(
        &window,
        "Une autre installation est en cours : Okapi attend son tour."
    ));
    button(&window, "Fermer").invoke_accessible_default_action();
    assert_eq!(step(&window), ui::UninstallStep::Closed, "it goes on");
    assert!(apps(&window).get_busy());
    assert_eq!(
        button(&window, "Actualiser").accessible_enabled(),
        Some(false)
    );
    // Another application: its button greyed, and refused in Rust too.
    open(&window, "Android Studio");
    assert_eq!(
        button(&window, "Désinstaller").accessible_enabled(),
        Some(false)
    );
    let checks = fake.simulated.load(Ordering::Relaxed);
    apps(&window).invoke_uninstall();
    assert_eq!(
        step(&window),
        ui::UninstallStep::Running,
        "the one running shown again"
    );
    apps(&window).invoke_refresh();
    assert_eq!(reads.get(), 1, "never read during it");
    release.send(()).unwrap();
    // Its turn come, it no longer says it waits.
    read_until(&window, |window| !apps(window).get_waiting());
    release.send(()).unwrap();
    read_until(&window, |window| apps(window).get_toast());
    assert_eq!(
        fake.simulated.load(Ordering::Relaxed),
        checks,
        "no second check"
    );
    assert_eq!(fake.removed().len(), 1);
}

/// A snap uninstalled, its data asked to go when `purge`.
fn snap_uninstalled(purge: bool) {
    let fake = Fake::new(vec![Ok(vec![going("android-studio", 1)])], Ended::Done);
    let (window, _, recorded, _) = on_apps(fake.clone());
    ask(&window, "Android Studio");
    let data = checkbox(&window, "Supprimer aussi ses données (5,0 Mo)");
    assert_eq!(data.accessible_checked(), Some(false), "unchecked");
    assert!(shows(
        &window,
        "Une copie de ses données (de tous les comptes et du système) est gardée 31 jours par le système."
    ));
    if purge {
        data.invoke_accessible_default_action();
        assert!(shows(
            &window,
            "Ses données partent tout de suite, pour tous les comptes et celles du système, sans copie gardée."
        ));
    }
    apps(&window).invoke_confirm();
    read_until(&window, |window| apps(window).get_toast());
    assert_eq!(
        fake.removed(),
        [(
            Target::Snap {
                name: "android-studio".to_owned(),
                purge
            },
            vec![going("android-studio", 1)]
        )]
    );
    let line = recorded.lock().unwrap()[0].clone();
    assert_eq!(line.data_removed, purge);
    assert_eq!(
        line.freed_bytes,
        Some(if purge { 7_000_000 } else { 2_000_000 }),
        "the snap and its old version, its data when they go"
    );
}

#[test]
fn a_snap_keeps_its_data_unless_asked() {
    snap_uninstalled(false);
}

#[test]
fn a_snap_s_data_go_when_asked() {
    snap_uninstalled(true);
}

#[test]
fn a_snap_tried_again_never_counts_its_data_twice() {
    let fake = Fake::new(
        vec![Ok(vec![going("android-studio", 1)])],
        Ended::Failed(Why::NotAuthorized),
    );
    let (window, ..) = on_apps(fake);
    ask(&window, "Android Studio");
    checkbox(&window, "Supprimer aussi ses données (5,0 Mo)").invoke_accessible_default_action();
    apps(&window).invoke_confirm();
    read_until(&window, |window| step(window) == ui::UninstallStep::Failed);
    button(&window, "Réessayer").invoke_accessible_default_action();
    read_until(&window, |window| step(window) == ui::UninstallStep::Confirm);
    assert!(shows(
        &window,
        "Place libérée : 2,0 Mo, anciennes versions comprises."
    ));
    assert!(!apps(&window).get_purge(), "asked again, unchecked");
}

#[test]
fn closing_the_window_during_an_uninstallation_is_held_once() {
    let (window, screen, ..) = on_apps(inert().uninstaller);
    assert_eq!(
        screen.close_requested(),
        slint::CloseRequestResponse::HideWindow,
        "nothing running"
    );
    let (said, receiver) = mpsc::channel();
    screen.0.borrow_mut().operation = Some(Operation {
        key: AppKey {
            source: Source::Deb,
            id: "gimp:amd64".to_owned(),
        },
        name: "GIMP".to_owned(),
        target: Target::Deb {
            package: "gimp".to_owned(),
            architecture: "amd64".to_owned(),
        },
        freed: Some(1),
        data_bytes: 0,
        phase: Phase::Running(Run::default()),
        receiver,
        cancel: Arc::new(AtomicBool::new(false)),
    });
    assert_eq!(
        screen.close_requested(),
        slint::CloseRequestResponse::KeepWindowShown
    );
    assert_eq!(step(&window), ui::UninstallStep::Running, "shown again");
    assert!(apps(&window).get_closing());
    assert_eq!(
        screen.close_requested(),
        slint::CloseRequestResponse::HideWindow,
        "insisted"
    );
    drop(said);
}

#[test]
fn a_history_that_could_not_be_written_is_said_with_what_went() {
    let fake = Fake::new(vec![Ok(vec![going("gimp", 3_000_000)])], Ended::Done);
    let (window, ..) = on_apps_recording(fake, false);
    ask(&window, "GIMP");
    apps(&window).invoke_confirm();
    read_until(&window, |window| apps(window).get_toast());
    assert!(!apps(&window).get_toast_recorded());
    assert!(shows(
        &window,
        "GIMP désinstallée : 3,0 Mo libérés. Elle n'a pas pu être inscrite à l'historique."
    ));
}

#[test]
fn a_size_unknown_is_said_unknown_never_nothing() {
    let without_size = Going {
        bytes: None,
        ..going("gimp", 0)
    };
    let fake = Fake::new(vec![Ok(vec![without_size])], Ended::Done);
    let (window, screen, recorded, _) = on_apps(fake);
    screen
        .0
        .borrow_mut()
        .entries
        .get_mut(&AppKey {
            source: Source::Deb,
            id: "gimp:amd64".to_owned(),
        })
        .unwrap()
        .app
        .size = AppSize::Unknown;
    ask(&window, "GIMP");
    assert!(shows(&window, "Place libérée : inconnue."));
    apps(&window).invoke_confirm();
    read_until(&window, |window| apps(window).get_toast());
    assert!(shows(&window, "GIMP désinstallée"));
    assert_eq!(recorded.lock().unwrap()[0].freed_bytes, None);
}

#[test]
fn a_package_going_without_its_size_makes_the_place_freed_unknown() {
    // Catches the application's size said for the whole, a package that
    // depends on it unknown: the screen and the history would say less
    // than goes.
    let dependent = Going {
        bytes: None,
        ..going("gimp-plugin", 0)
    };
    let fake = Fake::new(
        vec![Ok(vec![going("gimp", 3_000_000), dependent])],
        Ended::Done,
    );
    let (window, _, recorded, _) = on_apps(fake);
    ask(&window, "GIMP");
    assert!(shows(
        &window,
        "GIMP et un paquet qui en dépend partiront. Place libérée : inconnue."
    ));
    apps(&window).invoke_confirm();
    read_until(&window, |window| apps(window).get_toast());
    assert_eq!(recorded.lock().unwrap()[0].freed_bytes, None);
}

#[test]
fn an_ending_away_from_the_page_is_seen_on_return() {
    let (release, released) = mpsc::channel::<()>();
    let fake = Fake::new(vec![Ok(vec![going("gimp", 1)])], Ended::Done);
    let held = Arc::new(Held {
        fake,
        released: Mutex::new(released),
    });
    let (window, ..) = on_apps(held);
    ask(&window, "GIMP");
    apps(&window).invoke_confirm();
    button(&window, "Accueil").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    drop(release);
    read_until(&window, |window| apps(window).get_toast());
    i_slint_backend_testing::mock_elapsed_time(TOAST_SHOWN_FOR * 2);
    slint::platform::update_timers_and_animations();
    assert!(apps(&window).get_toast(), "not seen yet");
    button(&window, "Applications").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    assert!(apps(&window).get_toast());
    i_slint_backend_testing::mock_elapsed_time(TOAST_SHOWN_FOR);
    slint::platform::update_timers_and_animations();
    assert!(!apps(&window).get_toast(), "seen, then gone");
}

/// An uninstaller whose removal may be cancelled until it is asked to.
struct Cancellable;

impl Uninstaller for Cancellable {
    fn simulate(&self, _target: &Target) -> Result<Vec<Going>, Why> {
        Ok(vec![going("gimp", 1)])
    }

    fn status(&self) -> Result<Status, Why> {
        Ok(parse_status(STATUS))
    }

    fn remove(
        &self,
        _target: &Target,
        _going: &[Going],
        steps: &Sender<Step>,
        cancel: &AtomicBool,
    ) -> Ended {
        let _ = steps.send(Step::Cancellable(true));
        let started = Instant::now();
        while !cancel.load(Ordering::Relaxed) {
            if started.elapsed() > WAIT {
                return Ended::Done;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ended::Cancelled
    }
}

#[test]
fn an_uninstallation_asked_to_stop_stops_and_nothing_is_said_gone() {
    let (window, _, recorded, _) = on_apps(Arc::new(Cancellable));
    ask(&window, "GIMP");
    apps(&window).invoke_confirm();
    read_until(&window, |window| apps(window).get_cancellable());
    assert!(!apps(&window).get_too_late());
    button(&window, "Annuler la désinstallation").invoke_accessible_default_action();
    read_until(&window, |window| step(window) == ui::UninstallStep::Closed);
    assert!(!apps(&window).get_toast());
    assert!(recorded.lock().unwrap().is_empty());
}

#[test]
fn a_source_busy_or_silent_is_said_and_the_others_shown() {
    let seams = crate::cleaner::Seams {
        apps: AppsSeams {
            // dpkg writing; snapd's thread ends without a word.
            read: Rc::new(|_, sink, _| {
                sink.send(Arrival::Source(Source::Deb, SourceReading::Busy))
                    .unwrap();
                sink.send(Arrival::Source(Source::Flatpak, SourceReading::Absent))
                    .unwrap();
            }),
            ..inert()
        },
        ..seams_never_scanning()
    };
    let (window, ..) =
        crate::wide_window(seams, crate::dashboard::tests::inert(), Language::French);
    button(&window, "Applications").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    read_once();
    assert_eq!(apps(&window).get_debs(), ui::SourceShown::Busy);
    assert_eq!(apps(&window).get_snaps(), ui::SourceShown::Failed);
    assert!(shows(
        &window,
        "Les paquets Ubuntu ne sont pas lisibles pour l'instant : une installation est en cours."
    ));
    assert!(shows(
        &window,
        "Les snaps ne sont pas lisibles : le service snapd ne répond pas."
    ));
}

/// The check of GIMP fails `why`: said, and the list read again or not.
fn check_fails(why: Why, words: &str, read_again: bool) {
    let fake = Fake::new(vec![Err(why)], Ended::Done);
    let (window, _, _, reads) = on_apps(fake.clone());
    ask(&window, "GIMP");
    assert_eq!(step(&window), ui::UninstallStep::Failed);
    assert!(shows(&window, words), "{words}");
    assert_eq!(reads.get(), if read_again { 2 } else { 1 }, "{words}");
    assert_eq!(fake.removed(), []);
}

#[test]
fn an_application_gone_since_is_said_and_the_list_read_again() {
    check_fails(
        Why::NotFound,
        "Elle n'est plus installée. La liste est relue.",
        true,
    );
}

#[test]
fn a_check_held_by_another_installation_is_said() {
    check_fails(
        Why::Busy,
        "Une autre installation est en cours : réessayez dans un moment.",
        false,
    );
}

/// An uninstaller whose removal panics: its thread ends without a word.
struct Panicking;

impl Uninstaller for Panicking {
    fn simulate(&self, _target: &Target) -> Result<Vec<Going>, Why> {
        Ok(vec![going("gimp", 1)])
    }

    fn status(&self) -> Result<Status, Why> {
        Ok(parse_status(STATUS))
    }

    fn remove(
        &self,
        _target: &Target,
        _going: &[Going],
        _steps: &Sender<Step>,
        _cancel: &AtomicBool,
    ) -> Ended {
        panic!("the service's library failed");
    }
}

#[test]
fn a_removal_that_dies_says_its_outcome_unknown_and_reads_again() {
    let (window, _, recorded, reads) = on_apps(Arc::new(Panicking));
    ask(&window, "GIMP");
    apps(&window).invoke_confirm();
    read_until(&window, |window| step(window) == ui::UninstallStep::Failed);
    assert!(shows(
        &window,
        "Le service ne répond pas. La liste est relue."
    ));
    assert!(recorded.lock().unwrap().is_empty());
    assert_eq!(reads.get(), 2);
}

#[test]
fn the_window_s_close_button_is_held_once_during_an_uninstallation() {
    let (release, released) = mpsc::channel::<()>();
    let fake = Fake::new(vec![Ok(vec![going("gimp", 1)])], Ended::Done);
    let held = Arc::new(Held {
        fake,
        released: Mutex::new(released),
    });
    let (window, ..) = on_apps(held);
    ask(&window, "GIMP");
    apps(&window).invoke_confirm();
    read_until(&window, |window| apps(window).get_waiting());
    button(&window, "Fermer").invoke_accessible_default_action();
    button(&window, "Accueil").invoke_accessible_default_action();
    // As the window's own button asks: the cleaner, then this screen.
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::CloseRequested);
    assert_eq!(
        window.get_page(),
        ui::Page::Apps,
        "the uninstallation shown"
    );
    assert_eq!(step(&window), ui::UninstallStep::Running);
    assert!(apps(&window).get_closing());
    drop(release);
}
