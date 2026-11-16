//! The settings screen in a window: a sandbox's `settings.json` read,
//! changed and written through the screen, the folder chooser and the
//! desktop's colour scheme asked through stand-ins.

use std::cell::{Cell, RefCell};
use std::fs;
use std::rc::Rc;

use std::time::{Duration, Instant};

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use okapi_engine::DEFAULT_MIN_AGE_MINUTES;
use slint::Model as _;

use super::*;
use crate::Screens;
use crate::cleaner::Seams;
use crate::cleaner::tests::{button, defaults, seams_never_scanning, shows};
use crate::saving::LAST_WRITE_WAIT;

/// No settings directory, no folder chooser, a desktop that says nothing:
/// nothing of the machine is read nor written.
pub fn nowhere() -> SettingsSeams {
    SettingsSeams {
        directory: None,
        home: None,
        state_directory: None,
        save: |_, _, _| Written::Failed,
        choose_folder: Rc::new(|_, done| done(Chosen::Unavailable)),
        desktop_theme: Rc::new(|done| done(None)),
        examine: Arc::new(examine),
    }
}

/// A write that takes a while, then is done.
fn slow(settings: &Settings, over: &Settings, directory: &Path) -> Written {
    std::thread::sleep(Duration::from_millis(150));
    stored(settings, over, directory)
}

/// A write stuck beyond what the screen waits for.
fn stuck(settings: &Settings, over: &Settings, directory: &Path) -> Written {
    std::thread::sleep(LAST_WRITE_WAIT * 2);
    stored(settings, over, directory)
}

/// A write that takes longer than the first look, then fails.
fn slow_failing(_: &Settings, _: &Settings, _: &Path) -> Written {
    std::thread::sleep(Duration::from_millis(500));
    Written::Failed
}

/// A write that fails at once (a full disk).
fn failing(_: &Settings, _: &Settings, _: &Path) -> Written {
    Written::Failed
}

/// A write whose thread dies.
fn dying(_: &Settings, _: &Settings, _: &Path) -> Written {
    panic!("the settings writer dies");
}

/// What the desktop asked for its theme answers, once told to.
type Answer = Box<dyn FnOnce(Option<Theme>)>;

/// A home folder with Okapi's settings directory in it, and what the
/// stand-ins were asked.
struct Sandbox {
    home: tempfile::TempDir,
    /// What the folder chooser answers next.
    chosen: Rc<RefCell<Option<Chosen>>>,
    /// What the desktop says of its theme, and how often it was asked; its
    /// answer kept until told when `defer` is set.
    desktop_theme: Rc<Cell<Option<Theme>>>,
    desktop_asked: Rc<Cell<u32>>,
    defer: Rc<Cell<bool>>,
    deferred: Rc<RefCell<Option<Answer>>>,
    save: Save,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".config/okapi")).unwrap();
        Self {
            home,
            chosen: Rc::new(RefCell::new(None)),
            desktop_theme: Rc::new(Cell::new(None)),
            desktop_asked: Rc::new(Cell::new(0)),
            defer: Rc::new(Cell::new(false)),
            deferred: Rc::new(RefCell::new(None)),
            save: stored,
        }
    }

    fn writing_with(save: Save) -> Self {
        Self {
            save,
            ..Self::new()
        }
    }

    fn directory(&self) -> PathBuf {
        self.home.path().join(".config/okapi")
    }

    fn file(&self) -> PathBuf {
        Settings::path(&self.directory())
    }

    fn write(&self, text: &str) {
        fs::write(self.file(), text).unwrap();
    }

    fn text(&self) -> String {
        fs::read_to_string(self.file()).unwrap()
    }

    fn settings(&self) -> Settings {
        Settings::load(&self.directory()).unwrap()
    }

    fn seams(&self) -> SettingsSeams {
        let chosen = Rc::clone(&self.chosen);
        let theme = Rc::clone(&self.desktop_theme);
        let asked = Rc::clone(&self.desktop_asked);
        let defer = Rc::clone(&self.defer);
        let deferred = Rc::clone(&self.deferred);
        SettingsSeams {
            directory: Some(self.directory()),
            home: Some(self.home.path().to_path_buf()),
            state_directory: Some(self.home.path().join(".local/state/okapi")),
            save: self.save,
            choose_folder: Rc::new(move |_, done| {
                done(
                    chosen
                        .borrow_mut()
                        .take()
                        .expect("a folder chooser asked once"),
                );
            }),
            desktop_theme: Rc::new(move |done| {
                asked.set(asked.get() + 1);
                if defer.get() {
                    deferred.replace(Some(done));
                } else {
                    done(theme.get());
                }
            }),
            examine: Arc::new(examine),
        }
    }

    /// The window, on the settings screen, its cleaner reading through
    /// `read`.
    fn window_reading(&self, read: crate::cleaner::Read) -> (ui::AppWindow, Screens) {
        Self::window_from(read, self.seams())
    }

    /// The window, on the settings screen, reading and asking through
    /// `settings`, its cleaner reading through `read`.
    fn window_from(
        read: crate::cleaner::Read,
        settings: SettingsSeams,
    ) -> (ui::AppWindow, Screens) {
        let seams = Seams {
            read,
            settings,
            ..seams_never_scanning()
        };
        let screens = crate::wide_window(seams, crate::dashboard::tests::inert(), Language::French);
        let window = screens.0.clone_strong();
        button(&window, "Réglages").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        // The page is made, and reads, as the window is looked at, as the
        // app draws it at once.
        assert!(shows(
            &window,
            "Chaque changement est enregistré tout de suite."
        ));
        (window, screens)
    }

    fn window(&self) -> (ui::AppWindow, Screens) {
        self.window_reading(Rc::new(|| Ok(defaults())))
    }
}

fn preferences(window: &ui::AppWindow) -> ui::Preferences<'_> {
    window.global::<ui::Preferences>()
}

/// Whether the screen says `refusal` of the last change.
fn save_refused(window: &ui::AppWindow, refusal: ui::SaveRefusal) -> bool {
    preferences(window).get_save_refused() == refusal
}

/// Types `text` in the field, and presses "Ajouter".
fn type_and_add(window: &ui::AppWindow, text: &str) {
    preferences(window).set_entry(text.into());
    button(window, "Ajouter").invoke_accessible_default_action();
}

/// The exclusions shown, and what each is.
fn rows(window: &ui::AppWindow) -> Vec<(String, ui::ExclusionKind)> {
    preferences(window)
        .get_exclusions()
        .iter()
        .map(|row| (row.text.to_string(), row.kind))
        .collect()
}

/// Lets the lines shown be read, as the screen looks at them.
fn examined(screens: &Screens) {
    let begun = Instant::now();
    while screens.3.reads_lines() {
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "lines never read"
        );
        std::thread::sleep(Duration::from_millis(5));
        i_slint_backend_testing::mock_elapsed_time(EXAMINE_EVERY);
        slint::platform::update_timers_and_animations();
    }
}

/// The badge of the line `text`, as it is read with it.
fn badge(window: &ui::AppWindow, text: &str) -> String {
    ElementHandle::find_by_accessible_label(window, text)
        .find(|element| element.accessible_role() == Some(AccessibleRole::ListItem))
        .and_then(|line| line.accessible_description())
        .unwrap_or_else(|| panic!("no line {text}"))
        .to_string()
}

/// Lets the changes asked for be written, as what reads the settings next
/// does.
fn written(screens: &Screens) {
    // What it says is the cleaner's to hear: the tests look at the screen.
    let _ = screens.3.wait_for_writes();
}

/// The segment `label` of the group `group`.
fn segment(window: &ui::AppWindow, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .find(|element| element.accessible_role() == Some(AccessibleRole::RadioButton))
        .unwrap_or_else(|| panic!("no segment {label}"))
}

fn dark(window: &ui::AppWindow) -> bool {
    window.global::<ui::Theme>().get_dark()
}

#[test]
fn an_exclusion_typed_is_written_absolute_and_shown_from_home() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::Editable
    );
    assert!(shows(&window, "Aucune exclusion."));
    type_and_add(&window, "  ~/Projets ");
    written(&screens);
    let projets = sandbox.home.path().join("Projets");
    assert_eq!(
        sandbox.settings().exclusions,
        [projets.to_str().unwrap()],
        "absolute, as the engine takes it"
    );
    assert_eq!(
        rows(&window),
        [("~/Projets".to_owned(), ui::ExclusionKind::Absent)]
    );
    assert_eq!(preferences(&window).get_entry(), "", "the field emptied");
    assert_eq!(preferences(&window).get_entry_error(), ui::EntryError::None);
    // Made since: a folder, once read again.
    fs::create_dir(&projets).unwrap();
    preferences(&window).invoke_reload();
    examined(&screens);
    assert_eq!(
        rows(&window),
        [("~/Projets".to_owned(), ui::ExclusionKind::Folder)]
    );
    // `~` alone is the home folder; a folder whose name only starts like
    // it is not below it.
    type_and_add(&window, "~");
    let beside = format!("{}2/x", sandbox.home.path().display());
    type_and_add(&window, &beside);
    type_and_add(&window, "*.blend");
    assert_eq!(
        rows(&window)[1..],
        [
            ("~".to_owned(), ui::ExclusionKind::Folder),
            (beside, ui::ExclusionKind::Absent),
            ("*.blend".to_owned(), ui::ExclusionKind::Extension),
        ]
    );
}

#[test]
fn an_exclusion_the_engine_refuses_is_never_written_and_stays_typed() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    for refused in ["relatif/x", "*.", "*.a*b", "/data/../etc", "~user/x"] {
        type_and_add(&window, refused);
        assert_eq!(
            preferences(&window).get_entry_error(),
            ui::EntryError::Invalid,
            "{refused}"
        );
        assert_eq!(preferences(&window).get_entry(), refused, "kept to fix");
        assert!(shows(
            &window,
            "Un chemin absolu (/… ou ~/…) ou une extension *.ext."
        ));
    }
    written(&screens);
    assert!(!sandbox.file().exists(), "nothing written");
    // Typing again clears the message.
    type_and_add(&window, "*.log");
    assert_eq!(preferences(&window).get_entry_error(), ui::EntryError::None);
}

#[test]
fn an_exclusion_already_there_is_said_and_not_added_twice() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    type_and_add(&window, "~/x");
    let absolute = sandbox.home.path().join("x");
    type_and_add(&window, absolute.to_str().unwrap());
    assert_eq!(
        preferences(&window).get_entry_error(),
        ui::EntryError::Duplicate
    );
    assert!(shows(&window, "Déjà dans la liste."));
    written(&screens);
    assert_eq!(sandbox.settings().exclusions.len(), 1);
}

#[test]
fn the_exclusion_removed_is_the_one_chosen() {
    let sandbox = Sandbox::new();
    sandbox.write(r#"{"version": 1, "exclusions": ["/a", "/b", "/c"]}"#);
    let (window, screens) = sandbox.window();
    button(&window, "Retirer /b des exclusions").invoke_accessible_default_action();
    written(&screens);
    assert_eq!(sandbox.settings().exclusions, ["/a", "/c"]);
    assert_eq!(
        rows(&window)
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>(),
        ["/a", "/c"]
    );
}

#[test]
fn an_exclusion_written_by_hand_is_shown_as_the_engine_takes_it() {
    let sandbox = Sandbox::new();
    let file = sandbox.home.path().join("notes.txt");
    fs::write(&file, "").unwrap();
    sandbox.write(&format!(
        r#"{{"version": 1, "exclusions": ["relatif", "*.", "*.a*b", "*.log", "{}"]}}"#,
        file.display()
    ));
    let (window, screens) = sandbox.window();
    examined(&screens);
    assert_eq!(
        rows(&window)
            .into_iter()
            .map(|(_, kind)| kind)
            .collect::<Vec<_>>(),
        [
            ui::ExclusionKind::Invalid,
            ui::ExclusionKind::Invalid,
            ui::ExclusionKind::Invalid,
            ui::ExclusionKind::Extension,
            ui::ExclusionKind::File,
        ]
    );
    // Read with its line, as its badge shows it.
    assert_eq!(badge(&window, "relatif"), "Invalide");
    assert_eq!(badge(&window, "*.log"), "Extension");
    // Another setting changed: shown as read, never read again.
    preferences(&window).invoke_toggle_deletion_log();
    assert!(!screens.3.reads_lines(), "nothing read again");
    assert_eq!(badge(&window, "*.log"), "Extension");
    // The one that stops every analysis can be removed here.
    button(&window, "Retirer relatif des exclusions").invoke_accessible_default_action();
    assert_eq!(rows(&window).len(), 4);
}

#[test]
fn a_place_that_never_answers_never_holds_the_window() {
    let sandbox = Sandbox::new();
    sandbox.write(r#"{"version": 1, "exclusions": ["/mort/a", "*.log"]}"#);
    // Below `/mort`, held until `release` goes, as a mount that stopped
    // answering holds what reads where it lies.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);
    let seams = SettingsSeams {
        examine: Arc::new(move |entry| {
            if entry.starts_with("/mort") {
                // Bounded: waited for on the window's thread, it would fail
                // the test rather than hold it.
                let _ = held.lock().unwrap().recv_timeout(EXAMINE_WAIT * 10);
            }
            examine(entry)
        }),
        ..sandbox.seams()
    };
    let (window, screens) = Sandbox::window_from(Rc::new(|| Ok(defaults())), seams);
    let begun = Instant::now();
    while badge(&window, "*.log") != "Extension" {
        assert!(begun.elapsed() < Duration::from_secs(10), "never read");
        std::thread::sleep(Duration::from_millis(5));
        i_slint_backend_testing::mock_elapsed_time(EXAMINE_EVERY);
        slint::platform::update_timers_and_animations();
    }
    assert_eq!(badge(&window, "/mort/a"), "Lecture…", "the other one read");
    // Added below it: waited for a little, then said unreadable.
    let begun = Instant::now();
    type_and_add(&window, "/mort/b");
    let waited = begun.elapsed();
    assert!(
        waited >= EXAMINE_WAIT && waited < EXAMINE_WAIT * 3,
        "{waited:?}"
    );
    assert_eq!(
        preferences(&window).get_entry_error(),
        ui::EntryError::Unreadable
    );
    assert_eq!(preferences(&window).get_entry(), "/mort/b", "kept to fix");
    // One already there is said so at once, never read.
    let begun = Instant::now();
    type_and_add(&window, "/mort/a");
    assert!(begun.elapsed() < EXAMINE_WAIT);
    assert_eq!(
        preferences(&window).get_entry_error(),
        ui::EntryError::Duplicate
    );
    // Another place is added at once.
    type_and_add(&window, "/ici");
    assert_eq!(preferences(&window).get_entry_error(), ui::EntryError::None);
    written(&screens);
    assert_eq!(sandbox.settings().exclusions, ["/mort/a", "*.log", "/ici"]);
    // The mount answers again: the line is read once the file is.
    drop(release);
    preferences(&window).invoke_reload();
    examined(&screens);
    assert_eq!(badge(&window, "/mort/a"), "Absent");
}

#[test]
fn a_line_whose_reader_dies_is_shown_invalid() {
    let sandbox = Sandbox::new();
    sandbox.write(r#"{"version": 1, "exclusions": ["/boom", "*.log"]}"#);
    let seams = SettingsSeams {
        examine: Arc::new(|entry| {
            assert_ne!(entry, "/boom", "the reader dies");
            examine(entry)
        }),
        ..sandbox.seams()
    };
    let (window, screens) = Sandbox::window_from(Rc::new(|| Ok(defaults())), seams);
    examined(&screens);
    assert_eq!(badge(&window, "/boom"), "Invalide");
    assert_eq!(badge(&window, "*.log"), "Extension");
}

#[test]
fn the_minimum_age_is_one_of_four_the_default_among_them() {
    assert!(MIN_AGE_CHOICES.contains(&DEFAULT_MIN_AGE_MINUTES));
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    let options: Vec<String> = preferences(&window)
        .get_min_age_options()
        .iter()
        .map(|option| option.to_string())
        .collect();
    assert_eq!(options, ["30 min", "1 h", "2 h", "24 h"]);
    assert_eq!(
        segment(&window, "1 h").accessible_checked(),
        Some(true),
        "the default"
    );
    assert!(shows(
        &window,
        "Un fichier modifié plus récemment n'est jamais retiré. Par défaut : 1 h."
    ));
    segment(&window, "2 h").invoke_accessible_default_action();
    written(&screens);
    assert_eq!(sandbox.settings().min_age_minutes, Some(120));
    assert_eq!(segment(&window, "2 h").accessible_checked(), Some(true));
    assert_eq!(segment(&window, "1 h").accessible_checked(), Some(false));

    // An age written by hand, out of the choices: none chosen, it is said.
    sandbox.write(r#"{"version": 1, "minAgeMinutes": 90}"#);
    preferences(&window).invoke_reload();
    assert_eq!(preferences(&window).get_min_age_chosen(), -1);
    assert!(shows(
        &window,
        "Un fichier modifié plus récemment n'est jamais retiré. Réglé à 90 min dans le fichier."
    ));
    segment(&window, "24 h").invoke_accessible_default_action();
    written(&screens);
    assert_eq!(sandbox.settings().min_age_minutes, Some(1440));
}

#[test]
fn the_deletion_log_is_switched_and_says_where_it_goes() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    assert!(shows(
        &window,
        "Garde, pour chaque nettoyage, la liste des chemins retirés dans ~/.local/state/okapi."
    ));
    let switch = || {
        ElementHandle::find_by_accessible_label(&window, "Journal des suppressions")
            .find(|element| element.accessible_role() == Some(AccessibleRole::Switch))
            .unwrap()
    };
    assert_eq!(switch().accessible_checked(), Some(false));
    switch().invoke_accessible_default_action();
    written(&screens);
    assert!(sandbox.settings().deletion_log);
    assert_eq!(switch().accessible_checked(), Some(true));
    switch().invoke_accessible_default_action();
    written(&screens);
    assert!(!sandbox.settings().deletion_log);
}

#[test]
fn the_language_is_written_for_the_next_start_only() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    assert_eq!(segment(&window, "Système").accessible_checked(), Some(true));
    segment(&window, "English").invoke_accessible_default_action();
    written(&screens);
    assert_eq!(sandbox.settings().language, Some(Language::English));
    assert_eq!(segment(&window, "English").accessible_checked(), Some(true));
    assert_eq!(
        segment(&window, "Français").accessible_checked(),
        Some(false)
    );
    assert!(
        shows(&window, "Réglages"),
        "still in French until the next start"
    );
    segment(&window, "Français").invoke_accessible_default_action();
    written(&screens);
    assert_eq!(sandbox.settings().language, Some(Language::French));
    // Both groups have a "Système": the language's is the first.
    ElementHandle::find_by_accessible_label(&window, "Système")
        .find(|element| element.accessible_role() == Some(AccessibleRole::RadioButton))
        .unwrap()
        .invoke_accessible_default_action();
    written(&screens);
    assert_eq!(sandbox.settings().language, None);
}

#[test]
fn a_theme_forced_shows_at_once_and_the_desktop_s_is_asked_back() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    assert_eq!(sandbox.desktop_asked.get(), 0, "followed from the start");
    assert!(!dark(&window));
    let theme = |label| {
        ElementHandle::find_by_accessible_label(&window, label)
            .filter(|element| element.accessible_role() == Some(AccessibleRole::RadioButton))
            .last()
            .unwrap()
    };
    theme("Sombre").invoke_accessible_default_action();
    assert!(dark(&window));
    assert_eq!(theme("Sombre").accessible_checked(), Some(true));
    written(&screens);
    assert_eq!(sandbox.settings().theme, Some(Theme::Dark));
    theme("Clair").invoke_accessible_default_action();
    assert!(!dark(&window));
    // Back to the desktop's: asked, as the palette no longer follows it.
    sandbox.desktop_theme.set(Some(Theme::Dark));
    theme("Système").invoke_accessible_default_action();
    assert_eq!(sandbox.desktop_asked.get(), 1);
    assert!(dark(&window));
    written(&screens);
    assert_eq!(sandbox.settings().theme, None);
    // Read again unchanged: not asked again.
    preferences(&window).invoke_reload();
    assert_eq!(sandbox.desktop_asked.get(), 1);
}

#[test]
fn a_theme_in_the_file_shows_from_the_start() {
    let sandbox = Sandbox::new();
    sandbox.write(r#"{"version": 1, "theme": "dark"}"#);
    let (window, _screens) = sandbox.window();
    assert!(dark(&window));
    assert_eq!(sandbox.desktop_asked.get(), 0);
}

#[cfg(unix)]
#[test]
fn settings_that_are_a_link_are_shown_and_never_written() {
    let sandbox = Sandbox::new();
    let dotfiles = sandbox.home.path().join("dotfiles.json");
    fs::write(&dotfiles, r#"{"version": 1, "deletionLog": true}"#).unwrap();
    std::os::unix::fs::symlink(&dotfiles, sandbox.file()).unwrap();
    let (window, screens) = sandbox.window();
    assert_eq!(preferences(&window).get_state(), ui::SettingsState::Linked);
    assert!(shows(
        &window,
        "Vos réglages sont un lien : modifiez-les à la main."
    ));
    assert!(preferences(&window).get_deletion_log(), "its values shown");
    assert_eq!(button(&window, "Ajouter").accessible_enabled(), Some(false));
    // Even asked directly, nothing changes.
    preferences(&window).invoke_toggle_deletion_log();
    preferences(&window).invoke_add("/x".into());
    preferences(&window).invoke_browse();
    written(&screens);
    assert!(
        save_refused(&window, ui::SaveRefusal::None),
        "never even tried: nothing failed"
    );
    assert!(preferences(&window).get_deletion_log());
    assert_eq!(rows(&window), []);
    assert!(fs::symlink_metadata(sandbox.file()).unwrap().is_symlink());
    assert_eq!(
        fs::read_to_string(&dotfiles).unwrap(),
        r#"{"version": 1, "deletionLog": true}"#
    );
}

#[cfg(unix)]
#[test]
fn settings_behind_a_link_that_leads_nowhere_are_unreadable() {
    let sandbox = Sandbox::new();
    std::os::unix::fs::symlink(sandbox.home.path().join("gone.json"), sandbox.file()).unwrap();
    let (window, _screens) = sandbox.window();
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::Unreadable
    );
}

#[test]
fn unreadable_settings_are_said_never_replaced_and_read_again_once_fixed() {
    let sandbox = Sandbox::new();
    sandbox.write("not json");
    let (window, screens) = sandbox.window();
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::Unreadable
    );
    assert!(shows(
        &window,
        "~/.config/okapi/settings.json ne se lit pas : corrigez-le ou supprimez-le. Okapi ne le remplace pas."
    ));
    preferences(&window).invoke_toggle_deletion_log();
    written(&screens);
    assert_eq!(sandbox.text(), "not json");
    sandbox.write(r#"{"version": 1}"#);
    button(&window, "Relire").invoke_accessible_default_action();
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::Editable
    );
}

#[test]
fn settings_broken_behind_the_screen_are_not_replaced_and_said() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    sandbox.write(r#"{"version": 1, "exclusion": []}"#);
    preferences(&window).invoke_toggle_deletion_log();
    written(&screens);
    assert!(save_refused(&window, ui::SaveRefusal::Failed));
    assert!(shows(
        &window,
        "Ce réglage n'a pas pu être enregistré (le détail est dans le journal)."
    ));
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::Unreadable
    );
    assert_eq!(sandbox.text(), r#"{"version": 1, "exclusion": []}"#);
}

#[test]
fn a_folder_chosen_is_added_as_it_is() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    let folder = sandbox.home.path().join(" espacé ");
    fs::create_dir(&folder).unwrap();
    sandbox.chosen.replace(Some(Chosen::Folder(folder.clone())));
    button(&window, "Parcourir").invoke_accessible_default_action();
    written(&screens);
    assert_eq!(
        sandbox.settings().exclusions,
        [folder.to_str().unwrap()],
        "never trimmed"
    );
    assert!(!preferences(&window).get_browsing());

    sandbox.chosen.replace(Some(Chosen::Cancelled));
    button(&window, "Parcourir").invoke_accessible_default_action();
    assert_eq!(preferences(&window).get_entry_error(), ui::EntryError::None);
    sandbox.chosen.replace(Some(Chosen::Unavailable));
    button(&window, "Parcourir").invoke_accessible_default_action();
    assert_eq!(
        preferences(&window).get_entry_error(),
        ui::EntryError::Chooser
    );
    assert!(shows(
        &window,
        "Le sélecteur de dossiers n'a pas pu s'ouvrir : écrivez le chemin."
    ));
    written(&screens);
    assert_eq!(sandbox.settings().exclusions.len(), 1);
}

#[cfg(unix)]
#[test]
fn a_folder_chosen_whose_name_is_not_text_is_refused() {
    use std::os::unix::ffi::OsStringExt as _;

    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    let folder = PathBuf::from(std::ffi::OsString::from_vec(b"/data/\xff".to_vec()));
    sandbox.chosen.replace(Some(Chosen::Folder(folder)));
    button(&window, "Parcourir").invoke_accessible_default_action();
    assert_eq!(
        preferences(&window).get_entry_error(),
        ui::EntryError::NotText
    );
    written(&screens);
    assert!(!sandbox.file().exists());
}

#[test]
fn a_change_keeps_every_other_setting() {
    let sandbox = Sandbox::new();
    sandbox.write(
        r#"{"version": 1, "exclusions": ["/keep", "*.log"], "minAgeMinutes": 90,
            "deletionLog": true, "language": "en", "theme": "dark"}"#,
    );
    let before = sandbox.settings();
    let (window, screens) = sandbox.window();
    type_and_add(&window, "/data");
    written(&screens);
    let mut expected = before;
    expected.exclusions.push("/data".to_owned());
    assert_eq!(sandbox.settings(), expected);
}

/// The window of `sandbox`, its cleaner noting the exclusions it reads
/// (none until it reads any).
fn noting_reads(sandbox: &Sandbox) -> (ui::AppWindow, Screens, Rc<RefCell<Option<Vec<String>>>>) {
    let directory = sandbox.directory();
    let read_exclusions = Rc::new(RefCell::new(None));
    let noted = Rc::clone(&read_exclusions);
    let read: crate::cleaner::Read = Rc::new(move || {
        let settings = Settings::load(&directory).unwrap();
        noted.replace(Some(settings.exclusions));
        Ok(defaults())
    });
    let (window, screens) = sandbox.window_reading(read);
    // What the cleaner read as the window opened.
    read_exclusions.replace(None);
    (window, screens, read_exclusions)
}

#[test]
fn the_cleaner_reads_the_exclusion_just_added() {
    // Slower than going straight to the cleaner: read too early, the file
    // would not hold it yet.
    let sandbox = Sandbox::writing_with(slow);
    let (window, screens, read_exclusions) = noting_reads(&sandbox);
    type_and_add(&window, "/data");
    screens.1.refresh();
    assert_eq!(*read_exclusions.borrow(), Some(vec!["/data".to_owned()]));
}

#[test]
fn the_cleaner_reads_nothing_while_a_change_is_stuck() {
    let sandbox = Sandbox::writing_with(stuck);
    let (window, screens, read_exclusions) = noting_reads(&sandbox);
    type_and_add(&window, "/data");
    let begun = Instant::now();
    screens.1.refresh();
    assert!(begun.elapsed() >= LAST_WRITE_WAIT, "it waited first");
    assert_eq!(*read_exclusions.borrow(), None, "never the file without it");
    assert_eq!(
        window.global::<ui::Cleaner>().get_problem(),
        ui::SetupProblem::SettingsSaving
    );
    // Written at last: the cleaner reads it, and may start.
    let begun = Instant::now();
    while window.global::<ui::Cleaner>().get_problem() != ui::SetupProblem::None {
        assert!(begun.elapsed() < LAST_WRITE_WAIT * 10, "never lifted");
        std::thread::sleep(Duration::from_millis(50));
        i_slint_backend_testing::mock_elapsed_time(WRITE_CHECK);
        slint::platform::update_timers_and_animations();
    }
    assert_eq!(*read_exclusions.borrow(), Some(vec!["/data".to_owned()]));
}

#[test]
fn a_failure_seen_first_by_the_screen_is_still_told_to_the_cleaner() {
    let sandbox = Sandbox::writing_with(failing);
    let (window, screens, read_exclusions) = noting_reads(&sandbox);
    type_and_add(&window, "/data");
    // The screen looks first, while the user is elsewhere already.
    button(&window, "Accueil").invoke_accessible_default_action();
    std::thread::sleep(Duration::from_millis(50));
    i_slint_backend_testing::mock_elapsed_time(WRITE_CHECK);
    slint::platform::update_timers_and_animations();
    assert_eq!(*read_exclusions.borrow(), None, "never the file without it");
    assert_eq!(
        window.global::<ui::Cleaner>().get_problem(),
        ui::SetupProblem::SettingsUnsaved
    );
    // Told once: the next reading reads the file as it is.
    screens.1.refresh();
    assert_eq!(*read_exclusions.borrow(), Some(Vec::new()));
}

#[test]
fn the_cleaner_never_reads_past_a_change_that_failed() {
    let sandbox = Sandbox::writing_with(failing);
    let (window, screens, read_exclusions) = noting_reads(&sandbox);
    type_and_add(&window, "/data");
    screens.1.refresh();
    assert_eq!(*read_exclusions.borrow(), None, "refused once, and said");
    assert_eq!(
        window.global::<ui::Cleaner>().get_problem(),
        ui::SetupProblem::SettingsUnsaved
    );
    // Back on the settings: the failure is still said, the file shown.
    button(&window, "Accueil").invoke_accessible_default_action();
    button(&window, "Réglages").invoke_accessible_default_action();
    assert!(shows(
        &window,
        "Ce réglage n'a pas pu être enregistré (le détail est dans le journal)."
    ));
    assert_eq!(rows(&window), []);
    // Said once: the next reading reads the file as it is.
    screens.1.refresh();
    assert_eq!(*read_exclusions.borrow(), Some(Vec::new()));
}

#[test]
fn a_change_still_being_written_is_never_read_over() {
    let sandbox = Sandbox::writing_with(stuck);
    let (window, screens) = sandbox.window();
    type_and_add(&window, "/data");
    // Shown again while the write is stuck: what is held stays.
    preferences(&window).invoke_reload();
    assert_eq!(rows(&window).len(), 1);
    // Once written, a change keeps it.
    let written_at_last = || {
        let begun = Instant::now();
        while screens.3.wait_for_writes().is_err() {
            assert!(begun.elapsed() < Duration::from_secs(20), "never written");
        }
    };
    written_at_last();
    preferences(&window).invoke_toggle_deletion_log();
    written_at_last();
    assert_eq!(sandbox.settings().exclusions, ["/data"]);
    assert!(sandbox.settings().deletion_log);
}

#[test]
fn a_writer_gone_is_said_and_nothing_can_be_changed_any_more() {
    let sandbox = Sandbox::writing_with(dying);
    let (window, screens) = sandbox.window();
    preferences(&window).invoke_toggle_deletion_log();
    // Said once, read again once: never a loop. The reader then is told
    // the change is lost; the next one has nothing to wait for.
    assert_eq!(screens.3.wait_for_writes(), Err(Unsaved::Failed));
    assert!(save_refused(&window, ui::SaveRefusal::Failed));
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::NoWriter
    );
    assert!(shows(
        &window,
        "Okapi ne peut pas enregistrer ses réglages : le détail est dans le journal."
    ));
    assert_eq!(
        screens.3.wait_for_writes(),
        Ok(()),
        "said once, nothing to wait for"
    );
    assert!(!preferences(&window).get_deletion_log(), "the file shown");
}

#[test]
fn a_failure_looked_at_is_still_told_to_the_next_reader() {
    // Catches the leftovers, read in the background, taking the one telling
    // of a lost exclusion from the cleaner: it would then clean without it.
    let sandbox = Sandbox::writing_with(failing);
    let (window, screens) = sandbox.window();
    preferences(&window).invoke_toggle_deletion_log();
    assert_eq!(screens.3.look_at_writes(), Err(Unsaved::Failed));
    assert_eq!(screens.3.look_at_writes(), Err(Unsaved::Failed));
    assert_eq!(screens.3.wait_for_writes(), Err(Unsaved::Failed));
    assert_eq!(screens.3.wait_for_writes(), Ok(()), "told once");
    assert_eq!(screens.3.look_at_writes(), Ok(()));
}

#[test]
fn a_failed_write_is_said_by_the_screen_alone() {
    // Slower than the first look: the screen looks again until it is done.
    let sandbox = Sandbox::writing_with(slow_failing);
    let (window, _screens) = sandbox.window();
    preferences(&window).invoke_toggle_deletion_log();
    i_slint_backend_testing::mock_elapsed_time(WRITE_CHECK);
    slint::platform::update_timers_and_animations();
    assert!(
        !save_refused(&window, ui::SaveRefusal::Failed),
        "not done yet"
    );
    let mut looked = 0;
    while !save_refused(&window, ui::SaveRefusal::Failed) {
        std::thread::sleep(Duration::from_millis(10));
        i_slint_backend_testing::mock_elapsed_time(WRITE_CHECK);
        slint::platform::update_timers_and_animations();
        looked += 1;
        assert!(looked < 1000, "the failure is never said");
    }
    assert!(!preferences(&window).get_deletion_log(), "the file shown");
}

#[test]
fn a_failure_is_said_until_a_write_keeps_the_settings_or_they_are_read_again() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    sandbox.write(r#"{"version": 1, "exclusion": []}"#);
    preferences(&window).invoke_toggle_deletion_log();
    written(&screens);
    assert!(save_refused(&window, ui::SaveRefusal::Failed));
    // The page shown again says it still.
    preferences(&window).invoke_reload();
    assert!(save_refused(&window, ui::SaveRefusal::Failed));
    // Fixed, then read again on asking.
    assert!(shows(
        &window,
        "~/.config/okapi/settings.json ne se lit pas : corrigez-le ou supprimez-le. Okapi ne le remplace pas."
    ));
    sandbox.write(r#"{"version": 1}"#);
    button(&window, "Relire").invoke_accessible_default_action();
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::Editable
    );
    assert!(
        !save_refused(&window, ui::SaveRefusal::Failed),
        "said no longer"
    );
    // A write that keeps them clears it too, read again or not.
    sandbox.write(r#"{"version": 1, "exclusion": []}"#);
    preferences(&window).invoke_toggle_deletion_log();
    written(&screens);
    assert!(save_refused(&window, ui::SaveRefusal::Failed));
    sandbox.write(r#"{"version": 1}"#);
    preferences(&window).invoke_reload();
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::Editable
    );
    assert!(save_refused(&window, ui::SaveRefusal::Failed), "said still");
    preferences(&window).invoke_toggle_deletion_log();
    written(&screens);
    assert!(sandbox.settings().deletion_log);
    assert!(!save_refused(&window, ui::SaveRefusal::Failed));
}

#[test]
fn reading_again_lets_go_of_a_chooser_that_never_answered() {
    let sandbox = Sandbox::new();
    let asked = Rc::new(Cell::new(0));
    let counted = Rc::clone(&asked);
    let seams = SettingsSeams {
        // Asked, never answering.
        choose_folder: Rc::new(move |_, _| counted.set(counted.get() + 1)),
        ..sandbox.seams()
    };
    let (window, _screens) = Sandbox::window_from(Rc::new(|| Ok(defaults())), seams);
    button(&window, "Parcourir").invoke_accessible_default_action();
    assert!(preferences(&window).get_browsing());
    // Asked again, past the button the window greys out.
    preferences(&window).invoke_browse();
    assert_eq!(asked.get(), 1, "one at a time");
    preferences(&window).invoke_reload();
    assert!(!preferences(&window).get_browsing());
    button(&window, "Parcourir").invoke_accessible_default_action();
    assert_eq!(asked.get(), 2);
}

#[test]
fn a_late_desktop_answer_never_overrides_a_theme_forced_since() {
    let sandbox = Sandbox::new();
    let (window, _screens) = sandbox.window();
    let theme = |label| {
        ElementHandle::find_by_accessible_label(&window, label)
            .filter(|element| element.accessible_role() == Some(AccessibleRole::RadioButton))
            .last()
            .unwrap()
    };
    theme("Sombre").invoke_accessible_default_action();
    sandbox.defer.set(true);
    theme("Système").invoke_accessible_default_action();
    theme("Clair").invoke_accessible_default_action();
    let answer = sandbox.deferred.borrow_mut().take().unwrap();
    answer(Some(Theme::Dark));
    assert!(!dark(&window), "the theme forced since stays");
    // A desktop that cannot say: light.
    theme("Sombre").invoke_accessible_default_action();
    theme("Système").invoke_accessible_default_action();
    let answer = sandbox.deferred.borrow_mut().take().unwrap();
    answer(None);
    assert!(!dark(&window));
}

#[test]
fn the_theme_shown_stays_while_the_settings_cannot_be_read() {
    let sandbox = Sandbox::new();
    sandbox.write(r#"{"version": 1, "theme": "dark"}"#);
    let (window, _screens) = sandbox.window();
    sandbox.write("not json");
    preferences(&window).invoke_reload();
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::Unreadable
    );
    assert!(dark(&window));
    assert_eq!(sandbox.desktop_asked.get(), 0);
}

#[test]
fn without_a_settings_directory_nothing_can_be_changed() {
    let seams = Seams {
        settings: nowhere(),
        ..seams_never_scanning()
    };
    let (window, ..) =
        crate::wide_window(seams, crate::dashboard::tests::inert(), Language::French);
    button(&window, "Réglages").invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::NoDirectory
    );
    preferences(&window).invoke_add("/x".into());
    preferences(&window).invoke_toggle_deletion_log();
    assert_eq!(rows(&window), []);
    assert!(!preferences(&window).get_deletion_log());
    assert!(!save_refused(&window, ui::SaveRefusal::Failed));
}

#[cfg(unix)]
#[test]
fn an_exclusion_whose_place_cannot_be_read_is_said_so() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    let looping = sandbox.home.path().join("loop");
    std::os::unix::fs::symlink(&looping, &looping).unwrap();
    let typed = format!("{}/x", looping.display());
    type_and_add(&window, &typed);
    assert_eq!(
        preferences(&window).get_entry_error(),
        ui::EntryError::Unreadable
    );
    assert!(shows(&window, "Cet emplacement ne se lit pas."));
    assert_eq!(preferences(&window).get_entry(), typed.as_str());
    written(&screens);
    assert!(!sandbox.file().exists());
}

#[test]
fn a_folder_chosen_leaves_what_is_typed() {
    let sandbox = Sandbox::new();
    let (window, _screens) = sandbox.window();
    preferences(&window).set_entry("/data/a".into());
    sandbox
        .chosen
        .replace(Some(Chosen::Folder(PathBuf::from("/mnt/x"))));
    button(&window, "Parcourir").invoke_accessible_default_action();
    assert_eq!(rows(&window).len(), 1);
    assert_eq!(preferences(&window).get_entry(), "/data/a");
}

#[test]
fn settings_changed_by_hand_are_read_when_the_screen_shows() {
    let sandbox = Sandbox::new();
    let (window, _screens) = sandbox.window();
    assert!(!preferences(&window).get_deletion_log());
    button(&window, "Accueil").invoke_accessible_default_action();
    sandbox.write(r#"{"version": 1, "deletionLog": true}"#);
    button(&window, "Réglages").invoke_accessible_default_action();
    // The page is made when the window is next looked at, and reads then.
    assert!(shows(
        &window,
        "Chaque changement est enregistré tout de suite."
    ));
    assert!(preferences(&window).get_deletion_log());
}

#[test]
fn an_exclusion_added_by_hand_is_never_written_over_and_said() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    let by_hand = r#"{"version": 1, "exclusions": ["/data/keep"]}"#;
    sandbox.write(by_hand);
    segment(&window, "Sombre").invoke_accessible_default_action();
    // Told to the next reader once, as a change that failed is.
    assert_eq!(screens.3.wait_for_writes(), Err(Unsaved::Failed));
    assert_eq!(sandbox.text(), by_hand, "never written over");
    assert!(save_refused(&window, ui::SaveRefusal::ChangedElsewhere));
    assert!(shows(
        &window,
        "Vos réglages ont été modifiés hors de cet écran : ils viennent d'être relus, et vos derniers changements n'ont pas été enregistrés."
    ));
    assert_eq!(rows(&window).len(), 1, "the file read again is shown");
    // The next change comes from that reading: written, and said no longer.
    segment(&window, "Sombre").invoke_accessible_default_action();
    written(&screens);
    assert_eq!(sandbox.settings().exclusions, ["/data/keep"]);
    assert_eq!(sandbox.settings().theme, Some(Theme::Dark));
    assert!(save_refused(&window, ui::SaveRefusal::None));
}

#[test]
fn changes_passed_by_a_newer_one_are_never_taken_for_a_change_elsewhere() {
    let sandbox = Sandbox::writing_with(slow);
    let (window, screens) = sandbox.window();
    // The first is being written while the second waits, passed by the third.
    preferences(&window).invoke_toggle_deletion_log();
    std::thread::sleep(Duration::from_millis(50));
    preferences(&window).invoke_toggle_deletion_log();
    preferences(&window).invoke_toggle_deletion_log();
    assert_eq!(screens.3.wait_for_writes(), Ok(()));
    assert!(sandbox.settings().deletion_log);
    assert!(save_refused(&window, ui::SaveRefusal::None));
}

/// Fails its first write (after a while, so that the next change waits),
/// then writes as the store does.
fn failing_once(settings: &Settings, over: &Settings, directory: &Path) -> Written {
    static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !FAILED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(150));
        return Written::Failed;
    }
    stored(settings, over, directory)
}

#[test]
fn a_write_that_failed_never_makes_the_next_one_refused() {
    let sandbox = Sandbox::writing_with(failing_once);
    let (window, screens) = sandbox.window();
    preferences(&window).invoke_toggle_deletion_log();
    // Handed while the first is being written.
    std::thread::sleep(Duration::from_millis(50));
    segment(&window, "Sombre").invoke_accessible_default_action();
    // The second holds the first: written over what was read, the file
    // untouched by the failure.
    written(&screens);
    assert!(sandbox.settings().deletion_log);
    assert_eq!(sandbox.settings().theme, Some(Theme::Dark));
}

#[test]
fn the_settings_entry_leads_to_the_screen() {
    let sandbox = Sandbox::new();
    let (window, _screens) = sandbox.window();
    assert_eq!(window.get_page(), ui::Page::Settings);
    assert_eq!(
        button(&window, "Réglages")
            .accessible_description()
            .as_deref(),
        Some("page affichée")
    );
    assert!(shows(
        &window,
        "Chaque changement est enregistré tout de suite."
    ));
}

#[test]
fn a_path_typed_from_home_is_expanded() {
    let home = Path::new("/home/ana");
    assert_eq!(expanded("~/x", Some(home)), "/home/ana/x");
    assert_eq!(expanded("~", Some(home)), "/home/ana");
    assert_eq!(expanded("~x", Some(home)), "~x");
    assert_eq!(expanded("~/x", None), "~/x");
    assert_eq!(
        expanded("~//x", Some(home)),
        "/home/ana/x",
        "never the root's"
    );
}

#[test]
fn an_exclusion_whose_write_cannot_start_stays_typed() {
    let sandbox = Sandbox::writing_with(dying);
    let (window, _screens) = sandbox.window();
    preferences(&window).invoke_toggle_deletion_log();
    // The writer dies at its first write, unseen yet by the screen.
    std::thread::sleep(Duration::from_millis(200));
    type_and_add(&window, "/x");
    assert_eq!(
        preferences(&window).get_entry(),
        "/x",
        "kept, as nothing is"
    );
    assert_eq!(
        preferences(&window).get_state(),
        ui::SettingsState::NoWriter
    );
    assert!(save_refused(&window, ui::SaveRefusal::Failed));
}

#[test]
fn an_exclusion_the_engine_takes_as_one_already_there_is_said() {
    let sandbox = Sandbox::new();
    let (window, screens) = sandbox.window();
    type_and_add(&window, "/data");
    type_and_add(&window, "/Data/");
    assert_eq!(
        preferences(&window).get_entry_error(),
        ui::EntryError::Duplicate
    );
    type_and_add(&window, "*.log");
    type_and_add(&window, "*.LOG");
    assert_eq!(
        preferences(&window).get_entry_error(),
        ui::EntryError::Duplicate
    );
    written(&screens);
    assert_eq!(sandbox.settings().exclusions, ["/data", "*.log"]);
}
