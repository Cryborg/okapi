//! The settings screen: what the user set in `settings.json`
//! (`okapi_store::Settings`), shown and changed there, each change written
//! at once off the interface's thread. Read again each time the screen shows
//! (the page asks, `Preferences.reload`): they may have been changed by hand.
//! Never written when the file is a link (D-9) or cannot be read (D-10),
//! nor over a file changed elsewhere since the screen read it (by hand, by
//! another window): it is read again then, and that is said.
//! What reads the settings elsewhere waits for the writes under way first
//! (`wait_for_writes`), and is refused while one is stuck, or once after
//! one failed: an exclusion just added, or a folder to clean just taken
//! out, is never missed by the next cleaning without a word.
//!
//! Two lists of paths are edited alike: the exclusions and the folders to
//! clean. Where a line of either lies is read on a thread of its own (a
//! mount that stopped answering would hold the window): waited for a little
//! as it is added, in the background for the lines shown.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use okapi_engine::{
    CustomFolder, CustomFolderRefusal, DEFAULT_MIN_AGE_MINUTES, ExclusionError, Exclusions,
};
use okapi_rules::Language;
use okapi_store::{Replaced, Settings, Theme};
use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use crate::saving::{LastWrite, Saver, WRITE_CHECK};
use crate::shown::shown as count;
use crate::shown::{HOME_MARK, from_home};
use crate::ui;

/// The thread that writes the settings.
const SAVER_NAME: &str = "okapi-settings-save";
/// The minimum ages offered, in minutes.
const MIN_AGE_CHOICES: [u64; 4] = [30, 60, 120, 1440];
/// The thread that reads where one exclusion lies.
const EXAMINER_NAME: &str = "okapi-path-read";
/// How long an exclusion added is waited for: one whose place does not
/// answer by then cannot be read.
const EXAMINE_WAIT: Duration = Duration::from_secs(1);
/// How often the lines being read are looked at.
const EXAMINE_EVERY: Duration = Duration::from_millis(100);
/// How long the lines being read are looked at every [`EXAMINE_EVERY`]:
/// beyond it (a mount slow to wake, or one that stopped answering), they
/// are looked at every `EXAMINE_PATIENCE`.
const EXAMINE_PATIENCE: Duration = Duration::from_secs(5);

/// What a line of a list is, read where it lies; or why it cannot be added.
pub type Examined = Result<ui::PathKind, ui::EntryError>;
/// Reads what a line of a list is, off the interface's thread: the engine
/// and the disk, or a test's stand-in.
pub type Examine = Arc<dyn Fn(&str) -> Examined + Send + Sync>;

/// Why what reads the settings would miss the user's last change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsaved {
    /// Still being written, beyond what is waited for.
    Pending,
    /// It could not be written: the file holds what it held.
    Failed,
}

/// What the folder chooser answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chosen {
    Folder(PathBuf),
    Cancelled,
    /// It could not be asked, or gave no folder (traced where it is known).
    Unavailable,
}

/// Asks the user for a folder, in a window of the title given; answers once.
pub type ChooseFolder = Rc<dyn Fn(String, Box<dyn FnOnce(Chosen)>)>;
/// Asks the desktop for its theme; answers once, none when it cannot say.
pub type DesktopTheme = Rc<dyn Fn(Box<dyn FnOnce(Option<Theme>)>)>;
/// Writes the settings (first) in their directory (last), off the
/// interface's thread, if the file still holds those given second (the
/// store traces why not).
pub type Save = fn(&Settings, &Settings, &Path) -> Written;

/// What became of a write of the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Kept,
    /// Not written (unreadable, a link, a failing disk): the file is as it
    /// was, a write never failing once it is replaced.
    Failed,
    /// The file no longer held what the screen read or wrote last (changed
    /// by hand, or by another window): left as it is.
    ChangedElsewhere,
}

impl Written {
    /// Whether the change was not kept.
    const fn refused(self) -> bool {
        !matches!(self, Self::Kept)
    }
}

/// A change handed to the writer: the settings wanted, and the reading of
/// the file they come from (its number, and what it read).
#[derive(Debug, Clone)]
struct Change {
    reading: u64,
    read: Settings,
    settings: Settings,
}

/// What the settings screen reads and asks of the machine.
#[derive(Clone)]
pub struct SettingsSeams {
    /// Where `settings.json` lives.
    pub directory: Option<PathBuf>,
    /// The home folder, which `~` stands for.
    pub home: Option<PathBuf>,
    /// Where the history (and its deletion log) lives.
    pub state_directory: Option<PathBuf>,
    pub save: Save,
    pub choose_folder: ChooseFolder,
    pub desktop_theme: DesktopTheme,
    /// What an exclusion is.
    pub examine: Examine,
    /// What a folder to clean is.
    pub examine_folder: Examine,
}

impl std::fmt::Debug for SettingsSeams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettingsSeams")
            .field("directory", &self.directory)
            .field("home", &self.home)
            .field("state_directory", &self.state_directory)
            .finish_non_exhaustive()
    }
}

impl SettingsSeams {
    /// The user's directories, `home` the home folder, and the portal.
    pub fn system(home: Option<PathBuf>) -> Self {
        Self {
            directory: okapi_platform::config_directory(),
            home,
            state_directory: okapi_platform::state_directory(),
            save: stored,
            choose_folder: Rc::new(portal::choose_folder),
            desktop_theme: Rc::new(portal::desktop_theme),
            examine: Arc::new(examine),
            examine_folder: Arc::new(examine_folder),
        }
    }
}

/// Writes `settings` in `directory` through the store, over `over`.
fn stored(settings: &Settings, over: &Settings, directory: &Path) -> Written {
    match settings.save(directory, over) {
        Ok(Replaced::Written) => Written::Kept,
        Ok(Replaced::ChangedElsewhere) => Written::ChangedElsewhere,
        // Traced by the store, with the step that failed.
        Err(_) => Written::Failed,
    }
}

/// The system's folder chooser and theme: the desktop portal.
#[cfg(target_os = "linux")]
mod portal {
    use std::cell::Cell;
    use std::rc::Rc;

    use okapi_store::Theme;

    use super::Chosen;
    use crate::portal;

    /// Runs `asked` on the event loop, and gives what it answers to
    /// `done`: `unavailable` when it cannot run.
    fn answer<T: 'static>(
        asked: impl Future<Output = T> + 'static,
        done: Box<dyn FnOnce(T)>,
        unavailable: T,
    ) {
        // Answered once: by the portal, or at once when it cannot be asked.
        let done = Rc::new(Cell::new(Some(done)));
        let answer_to = Rc::clone(&done);
        let spawned = slint::spawn_local(async move {
            let answer = asked.await;
            if let Some(done) = answer_to.take() {
                done(answer);
            }
        });
        if let Err(error) = spawned {
            tracing::error!(%error, "portal not asked: no event loop");
            if let Some(done) = done.take() {
                done(unavailable);
            }
        }
    }

    pub(super) fn choose_folder(title: String, done: Box<dyn FnOnce(Chosen)>) {
        let asked = async move {
            match portal::choose_folder(&title).await {
                Ok(folder) => {
                    tracing::info!(folder = %folder.display(), "folder chosen");
                    Chosen::Folder(folder)
                }
                Err(portal::Refusal::Cancelled) => {
                    tracing::info!("folder chooser closed by the user");
                    Chosen::Cancelled
                }
                Err(refusal) => {
                    tracing::warn!(%refusal, "folder chooser gave no folder");
                    Chosen::Unavailable
                }
            }
        };
        answer(asked, done, Chosen::Unavailable);
    }

    pub(super) fn desktop_theme(done: Box<dyn FnOnce(Option<Theme>)>) {
        let asked = async {
            portal::desktop_prefers_dark()
                .await
                .inspect(|dark| tracing::info!(dark, "desktop's colour scheme read"))
                .inspect_err(|refusal| {
                    tracing::warn!(%refusal, "desktop's colour scheme unknown: light");
                })
                .ok()
                .map(|dark| if dark { Theme::Dark } else { Theme::Light })
        };
        answer(asked, done, None);
    }
}

/// Elsewhere, no portal yet: paths are typed, and the theme light.
#[cfg(not(target_os = "linux"))]
mod portal {
    use okapi_store::Theme;

    use super::Chosen;

    pub(super) fn choose_folder(_title: String, done: Box<dyn FnOnce(Chosen)>) {
        tracing::warn!("no folder chooser on this system");
        done(Chosen::Unavailable);
    }

    pub(super) fn desktop_theme(done: Box<dyn FnOnce(Option<Theme>)>) {
        tracing::warn!("desktop's colour scheme unknown on this system: light");
        done(None);
    }
}

struct State {
    window: slint::Weak<ui::AppWindow>,
    seams: SettingsSeams,
    /// As shown: as last read, with the changes made since.
    settings: Settings,
    /// The number of the last reading of the file, and what it read: what
    /// the changes made since are written over (see the writer, in
    /// `attach`).
    reading: u64,
    read: Settings,
    /// Whether the settings can be changed here, and why not.
    status: ui::SettingsState,
    /// None when it did not start, or is gone: nothing can be kept then.
    saver: Option<Saver<Change, Written>>,
    check: slint::Timer,
    exclusions: ListState,
    folders: ListState,
    /// The theme the interface shows: none while it follows the desktop,
    /// as it does from the start until a theme is forced.
    theme_shown: Option<Theme>,
    /// A change could not be written, and no reader of the settings has
    /// been told yet.
    untold: bool,
    /// Told once the writes under way are done, whatever they said.
    when_written: Option<Rc<dyn Fn()>>,
    /// Looks at the lines being read, of either list.
    examine_check: slint::Timer,
}

/// What the screen holds of one list of paths, besides the settings.
struct ListState {
    rows: Rc<VecModel<ui::PathRow>>,
    /// What each line was last read to be: one not read yet shows as being
    /// read.
    kinds: HashMap<String, ui::PathKind>,
    /// The lines being read, each on its thread: since when, and its answer.
    examining: HashMap<String, (Instant, Receiver<Examined>)>,
}

impl ListState {
    fn new() -> Self {
        Self {
            rows: Rc::new(VecModel::default()),
            kinds: HashMap::new(),
            examining: HashMap::new(),
        }
    }
}

impl State {
    fn list(&self, list: ui::PathList) -> &ListState {
        match list {
            ui::PathList::Exclusions => &self.exclusions,
            ui::PathList::Folders => &self.folders,
        }
    }

    fn list_mut(&mut self, list: ui::PathList) -> &mut ListState {
        match list {
            ui::PathList::Exclusions => &mut self.exclusions,
            ui::PathList::Folders => &mut self.folders,
        }
    }

    /// What reads a line of `list`.
    fn examine(&self, list: ui::PathList) -> Examine {
        Arc::clone(match list {
            ui::PathList::Exclusions => &self.seams.examine,
            ui::PathList::Folders => &self.seams.examine_folder,
        })
    }
}

/// The two lists, in the order the screen shows them.
const LISTS: [ui::PathList; 2] = [ui::PathList::Exclusions, ui::PathList::Folders];

/// The lines of `list` the settings hold, as written.
fn entries(settings: &Settings, list: ui::PathList) -> &Vec<String> {
    match list {
        ui::PathList::Exclusions => &settings.exclusions,
        ui::PathList::Folders => &settings.custom_folders,
    }
}

fn entries_mut(settings: &mut Settings, list: ui::PathList) -> &mut Vec<String> {
    match list {
        ui::PathList::Exclusions => &mut settings.exclusions,
        ui::PathList::Folders => &mut settings.custom_folders,
    }
}

/// The settings screen.
#[derive(Clone)]
pub struct SettingsScreen(Rc<RefCell<State>>);

impl std::fmt::Debug for SettingsScreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettingsScreen")
            .finish_non_exhaustive()
    }
}

impl SettingsScreen {
    /// Binds the screen to `window` and shows the settings as they are,
    /// their theme applied at once.
    pub fn attach(window: &ui::AppWindow, seams: SettingsSeams) -> Self {
        let saver = seams.directory.clone().and_then(|directory| {
            let save = seams.save;
            // What this writer wrote last, and the reading it followed: the
            // next change of that reading is written over it, not over what
            // was read (the changes passed by a newer one were never
            // written). Only a write kept moves it.
            let written: RefCell<Option<(u64, Settings)>> = RefCell::new(None);
            let write = move |change: &Change| {
                let mut written = written.borrow_mut();
                let over = match &*written {
                    Some((reading, settings)) if *reading == change.reading => settings,
                    _ => &change.read,
                };
                let result = save(&change.settings, over, &directory);
                if result == Written::Kept {
                    *written = Some((change.reading, change.settings.clone()));
                }
                result
            };
            Saver::start(SAVER_NAME, Box::new(write))
                .inspect_err(|error| {
                    tracing::error!(%error, "settings writer not started: nothing can be changed");
                })
                .ok()
        });
        let (exclusions, folders) = (ListState::new(), ListState::new());
        let preferences = window.global::<ui::Preferences>();
        preferences.set_exclusions(ModelRc::from(Rc::clone(&exclusions.rows)));
        preferences.set_folders(ModelRc::from(Rc::clone(&folders.rows)));
        let options: Vec<SharedString> = MIN_AGE_CHOICES
            .iter()
            .map(|&minutes| preferences.invoke_age(count(minutes)))
            .collect();
        preferences.set_min_age_options(ModelRc::from(Rc::new(VecModel::from(options))));
        preferences.set_default_min_age(count(DEFAULT_MIN_AGE_MINUTES));
        let home = seams.home.as_deref();
        let path_shown = |path: Option<PathBuf>| {
            path.as_deref()
                .map(|path| from_home(path, home))
                .unwrap_or_default()
                .into()
        };
        preferences.set_state_directory(path_shown(seams.state_directory.clone()));
        preferences.set_settings_file(path_shown(seams.directory.as_deref().map(Settings::path)));
        let screen = Self(Rc::new(RefCell::new(State {
            window: window.as_weak(),
            seams,
            settings: Settings::default(),
            reading: 0,
            read: Settings::default(),
            status: ui::SettingsState::NoDirectory,
            saver,
            check: slint::Timer::default(),
            exclusions,
            folders,
            theme_shown: None,
            untold: false,
            when_written: None,
            examine_check: slint::Timer::default(),
        })));
        screen.bind(&preferences);
        screen.reload();
        screen
    }

    fn bind(&self, preferences: &ui::Preferences<'_>) {
        let screen = self.clone();
        preferences.on_add(move |list, typed| screen.add(list, &typed));
        let screen = self.clone();
        preferences.on_remove(move |list, index| screen.remove(list, index));
        let screen = self.clone();
        preferences.on_browse(move |list| screen.browse(list));
        let screen = self.clone();
        preferences.on_choose_min_age(move |index| {
            let Some(&minutes) = usize::try_from(index)
                .ok()
                .and_then(|index| MIN_AGE_CHOICES.get(index))
            else {
                tracing::error!(index, "minimum age chosen out of the choices");
                return;
            };
            screen.change("minimum age", |settings| {
                settings.min_age_minutes = Some(minutes);
            });
        });
        let screen = self.clone();
        preferences.on_toggle_deletion_log(move || {
            screen.change("deletion log", |settings| {
                settings.deletion_log = !settings.deletion_log;
            });
        });
        let screen = self.clone();
        preferences.on_choose_language(move |choice| {
            screen.change("language", |settings| {
                settings.language = language_of(choice);
            });
        });
        let screen = self.clone();
        preferences.on_choose_theme(move |choice| {
            if screen.change("theme", |settings| settings.theme = theme_of(choice)) {
                screen.apply_theme();
            }
        });
        let screen = self.clone();
        preferences.on_reload(move || screen.reload());
        let screen = self.clone();
        preferences.on_read_again(move || {
            screen.reload();
            if screen.editable("read again") {
                // The user saw the failure, and reads the file again.
                screen.0.borrow_mut().untold = false;
                screen.set_save_refused(ui::SaveRefusal::None);
            }
        });
    }

    /// Waits a little for the changes under way to be written; why what
    /// reads the settings now would miss the user's last change: one still
    /// being written, or one that failed (said once to a reader: the next
    /// reads the file as it is, which the screen shows).
    ///
    /// # Errors
    ///
    /// [`Unsaved`].
    pub fn wait_for_writes(&self) -> Result<(), Unsaved> {
        self.settle_writes(true)
    }

    /// [`Self::wait_for_writes`] for a reader that must not take the one
    /// telling of a failure from the cleaner (the leftovers): a failure is
    /// refused all the same, and still said to the next reader.
    ///
    /// # Errors
    ///
    /// [`Unsaved`].
    pub fn look_at_writes(&self) -> Result<(), Unsaved> {
        self.settle_writes(false)
    }

    fn settle_writes(&self, tell: bool) -> Result<(), Unsaved> {
        let (last, _) = self.settled();
        self.said(last);
        // Looked at after: a writer gone has nothing under way any more.
        let writing = self.0.borrow().saver.as_ref().is_some_and(Saver::writing);
        if writing {
            return Err(Unsaved::Pending);
        }
        let untold = &mut self.0.borrow_mut().untold;
        if *untold {
            *untold = !tell;
            return Err(Unsaved::Failed);
        }
        Ok(())
    }

    /// Calls `told` each time the writes under way are done (looked at by
    /// the screen itself), whatever they said.
    pub fn when_written(&self, told: Rc<dyn Fn()>) {
        self.0.borrow_mut().when_written = Some(told);
    }

    /// What the writes said once those under way are done, waiting a little;
    /// and whether one still is.
    fn settled(&self) -> (LastWrite<Written>, bool) {
        match &self.0.borrow().saver {
            Some(saver) => (saver.settle(), saver.writing()),
            None => (LastWrite::Nothing, false),
        }
    }

    /// Lets the last change be written, and waits for it a little.
    pub fn shut_down(&self) {
        if let Some(saver) = &mut self.0.borrow_mut().saver {
            saver.shut_down();
        }
    }

    /// Reads the file, and shows what it holds and whether it can be
    /// changed here; once the changes under way are written. Never while
    /// one is stuck: the file would not hold it, and the next change would
    /// write that older file; the changes held stay shown.
    fn reload(&self) {
        let (last, writing) = self.settled();
        // A failure reads the file itself, when nothing else is under way.
        let failed = match last {
            LastWrite::Said(written) => written.refused(),
            LastWrite::Gone => true,
            LastWrite::Nothing => false,
        };
        self.said(last);
        if writing {
            tracing::warn!(
                writer = SAVER_NAME,
                "a change still being written: the settings are not read again"
            );
        } else if !failed {
            self.read_file();
        }
    }

    /// Reads the file now, and shows what it holds and whether it can be
    /// changed here.
    fn read_file(&self) {
        let mut state = self.0.borrow_mut();
        let (settings, status, readable) = match &state.seams.directory {
            None => {
                tracing::warn!("no settings directory: the settings cannot be changed");
                (Settings::default(), ui::SettingsState::NoDirectory, false)
            }
            Some(directory) => match Settings::load(directory) {
                // First: a link that leads nowhere has nothing to show.
                Err(error) => {
                    tracing::warn!(%error, "settings unreadable: shown as such, never replaced");
                    (Settings::default(), ui::SettingsState::Unreadable, false)
                }
                Ok(settings) if Settings::is_link(directory) => {
                    tracing::info!(path = %Settings::path(directory).display(), "settings are a link: shown, never written");
                    (settings, ui::SettingsState::Linked, true)
                }
                Ok(settings) if state.saver.is_none() => {
                    tracing::warn!("no settings writer: the settings cannot be changed");
                    (settings, ui::SettingsState::NoWriter, true)
                }
                Ok(settings) => (settings, ui::SettingsState::Editable, true),
            },
        };
        // Every reading of the file goes through here: a change made after
        // it is written over what it read.
        state.reading += 1;
        state.read.clone_from(&settings);
        state.settings = settings;
        state.status = status;
        if let Some(window) = state.window.upgrade() {
            let preferences = window.global::<ui::Preferences>();
            preferences.set_state(status);
            // A chooser that never answered no longer keeps "Parcourir" off.
            preferences.set_browsing(false);
        }
        let lines = LISTS.map(|list| (list, entries(&state.settings, list).clone()));
        // As the file is read, only its lines are known or waited for: one
        // written again is read again.
        for (list, lines) in &lines {
            let held = state.list_mut(*list);
            held.kinds.retain(|entry, _| lines.contains(entry));
            held.examining.retain(|entry, _| lines.contains(entry));
        }
        state.show();
        drop(state);
        // What each line is, read again: answers come in first, then what is
        // not being read any more is.
        self.take_examined();
        for (list, lines) in lines {
            for entry in lines {
                self.examine_line(list, entry);
            }
        }
        if self.reads_lines() {
            self.watch_examined(EXAMINE_EVERY);
        }
        // Settings that cannot be read say nothing of the theme: the one
        // shown stays.
        if readable {
            self.apply_theme();
        }
    }

    /// Reads what the line `entry` of `list` is on a thread of its own,
    /// unless it is being read already.
    fn examine_line(&self, list: ui::PathList, entry: String) {
        if self.0.borrow().list(list).examining.contains_key(&entry) {
            return;
        }
        let examine = self.0.borrow().examine(list);
        match examine_off(&examine, list, entry.clone()) {
            Ok(answer) => {
                self.0
                    .borrow_mut()
                    .list_mut(list)
                    .examining
                    .insert(entry, (Instant::now(), answer));
            }
            Err(error) => {
                tracing::error!(%entry, ?list, %error, "line not read: no thread, shown as unreadable");
                self.0
                    .borrow_mut()
                    .list_mut(list)
                    .kinds
                    .insert(entry, kind_shown(list, Err(ui::EntryError::Unreadable)));
            }
        }
    }

    /// Whether a line shown, of either list, is still being read.
    fn reads_lines(&self) -> bool {
        let state = self.0.borrow();
        LISTS
            .iter()
            .any(|list| !state.list(*list).examining.is_empty())
    }

    /// Looks at the lines being read `every` so often.
    fn watch_examined(&self, every: Duration) {
        let screen = Rc::downgrade(&self.0);
        self.0
            .borrow()
            .examine_check
            .start(slint::TimerMode::Repeated, every, move || {
                if let Some(screen) = Weak::upgrade(&screen).map(Self) {
                    screen.take_examined();
                }
            });
    }

    /// Shows what the lines read since the last look are. The looks slow
    /// down once every line still being read has waited longer than
    /// [`EXAMINE_PATIENCE`], and stop once none is.
    fn take_examined(&self) {
        let mut state = self.0.borrow_mut();
        let mut any_answered = false;
        for list in LISTS {
            let held = state.list_mut(list);
            let mut answered = Vec::new();
            held.examining.retain(|entry, (_, answer)| {
                let examined = match answer.try_recv() {
                    Ok(examined) => examined,
                    Err(TryRecvError::Empty) => return true,
                    Err(TryRecvError::Disconnected) => {
                        tracing::error!(%entry, ?list, "line reader ended without word: shown as unreadable");
                        Err(ui::EntryError::Unreadable)
                    }
                };
                answered.push((entry.clone(), examined));
                false
            });
            any_answered |= !answered.is_empty();
            for (entry, examined) in answered {
                held.kinds.insert(entry, kind_shown(list, examined));
            }
        }
        if any_answered {
            state.show();
        }
        let waited = || {
            LISTS
                .iter()
                .flat_map(|list| state.list(*list).examining.values())
                .map(|(since, _)| since.elapsed())
        };
        let Some(youngest) = waited().min() else {
            state.examine_check.stop();
            return;
        };
        let every = if youngest < EXAMINE_PATIENCE {
            EXAMINE_EVERY
        } else {
            EXAMINE_PATIENCE
        };
        if state.examine_check.interval() != every {
            if every == EXAMINE_PATIENCE {
                let entries: Vec<&String> = LISTS
                    .iter()
                    .flat_map(|list| state.list(*list).examining.keys())
                    .collect();
                tracing::info!(?entries, oldest = ?waited().max(), "lines still being read: shown as such, looked at less often");
            }
            drop(state);
            self.watch_examined(every);
        }
    }

    /// Shows the theme the settings hold, if it is not the one shown: a
    /// theme forced, or the desktop's, asked again.
    fn apply_theme(&self) {
        let mut state = self.0.borrow_mut();
        let wanted = state.settings.theme;
        if wanted == state.theme_shown {
            return;
        }
        state.theme_shown = wanted;
        let window = state.window.clone();
        let desktop_theme = Rc::clone(&state.seams.desktop_theme);
        drop(state);
        if let Some(theme) = wanted {
            tracing::info!(?theme, "theme shown");
            show_theme(&window, theme);
            return;
        }
        // Forced since the start: the palette no longer follows the
        // desktop, which is asked how it is now.
        let screen = self.clone();
        desktop_theme(Box::new(move |theme| {
            // A theme forced again in the meantime wins.
            if screen.0.borrow().theme_shown.is_none() {
                tracing::info!(?theme, "desktop's theme shown");
                show_theme(&window, theme.unwrap_or(Theme::Light));
            }
        }));
    }

    /// Changes `what` in the settings, by `change`, if they can be changed,
    /// shows it and has it written; whether it is handed to the writer
    /// (or changed nothing).
    fn change(&self, what: &'static str, change: impl FnOnce(&mut Settings)) -> bool {
        let mut state = self.0.borrow_mut();
        if state.status != ui::SettingsState::Editable {
            tracing::warn!(what, state = ?state.status, "setting not changed: the settings cannot be written");
            return false;
        }
        let before = state.settings.clone();
        change(&mut state.settings);
        if state.settings == before {
            return true;
        }
        tracing::info!(what, ?before, after = ?state.settings, "setting changed");
        state.show();
        let change = Change {
            reading: state.reading,
            read: state.read.clone(),
            settings: state.settings.clone(),
        };
        let handed = state.saver.as_ref().is_some_and(|saver| saver.save(change));
        drop(state);
        if handed {
            self.check_write();
        } else {
            self.said(LastWrite::Gone);
        }
        handed
    }

    /// Looks, a little later, at whether the writes asked for are done,
    /// and tells `when_written` once they are.
    fn check_write(&self) {
        let screen = Rc::downgrade(&self.0);
        self.0
            .borrow()
            .check
            .start(slint::TimerMode::SingleShot, WRITE_CHECK, move || {
                let Some(screen) = Weak::upgrade(&screen).map(Self) else {
                    return;
                };
                let (last, writing) = match &screen.0.borrow().saver {
                    Some(saver) => (saver.last_write(), saver.writing()),
                    None => (LastWrite::Nothing, false),
                };
                screen.said(last);
                if writing {
                    screen.check_write();
                    return;
                }
                let told = screen.0.borrow().when_written.clone();
                if let Some(told) = told {
                    told();
                }
            });
    }

    /// Says what the writes done said. One that did not keep the settings
    /// (failed, or refused over a file changed elsewhere) is said on the
    /// screen, and to the next reader (`untold`); the file is shown as it is
    /// unless another write is under way. A writer gone keeps nothing any
    /// more, which the screen says from then on.
    fn said(&self, last: LastWrite<Written>) {
        match last {
            LastWrite::Nothing => {}
            LastWrite::Said(Written::Kept) => {
                // Each write holds every change made: none is missing now.
                self.0.borrow_mut().untold = false;
                self.set_save_refused(refusal_shown(Written::Kept));
            }
            LastWrite::Said(refused) => {
                tracing::warn!(writer = SAVER_NAME, ?refused, "settings not written: said");
                self.0.borrow_mut().untold = true;
                self.set_save_refused(refusal_shown(refused));
                let writing = self.0.borrow().saver.as_ref().is_some_and(Saver::writing);
                if !writing {
                    self.read_file();
                }
            }
            LastWrite::Gone => {
                tracing::error!(
                    writer = SAVER_NAME,
                    "settings writer gone: nothing can be changed any more"
                );
                {
                    let mut state = self.0.borrow_mut();
                    state.saver = None;
                    state.untold = true;
                }
                self.set_save_refused(ui::SaveRefusal::Failed);
                self.read_file();
            }
        }
    }

    fn set_save_refused(&self, refusal: ui::SaveRefusal) {
        if let Some(window) = self.0.borrow().window.upgrade() {
            window.global::<ui::Preferences>().set_save_refused(refusal);
        }
    }

    fn editable(&self, what: &'static str) -> bool {
        let status = self.0.borrow().status;
        let editable = status == ui::SettingsState::Editable;
        if !editable {
            tracing::warn!(what, state = ?status, "not done: the settings cannot be written");
        }
        editable
    }

    /// Adds to `list` what the user typed (spaces around it dropped, `~`
    /// standing for the home folder), and empties the field once it is
    /// added.
    fn add(&self, list: ui::PathList, typed: &str) {
        let entry = expanded(typed.trim(), self.0.borrow().seams.home.as_deref());
        if !self.add_entry(list, entry) {
            return;
        }
        self.set_entry(list, SharedString::new(), ui::EntryError::None);
    }

    /// Adds `entry` to `list`, as it is, if what reads its lines takes it
    /// and it is not there yet; says why not otherwise. Whether it is added.
    /// Where it lies is read on a thread of its own, waited for
    /// [`EXAMINE_WAIT`] at most: a place that does not answer by then
    /// cannot be read (its thread is left to end alone, if it ever does).
    fn add_entry(&self, list: ui::PathList, entry: String) -> bool {
        if entry.is_empty() || !self.editable("line added") {
            return false;
        }
        let duplicate = entries(&self.0.borrow().settings, list)
            .iter()
            .any(|kept| same(list, kept, &entry));
        let refused = if duplicate {
            Some(ui::EntryError::Duplicate)
        } else {
            match self
                .examine_added(list, &entry)
                .and_then(|kind| admitted(list, kind))
            {
                Err(refused) => Some(refused),
                Ok(kind) => {
                    // Never shown as being read: it was, now. An older
                    // reading of it (removed, then added again) is stale.
                    let mut state = self.0.borrow_mut();
                    let held = state.list_mut(list);
                    held.kinds.insert(entry.clone(), kind);
                    held.examining.remove(&entry);
                    None
                }
            }
        };
        if let Some(refused) = refused {
            tracing::warn!(%entry, ?list, ?refused, "line not added");
            self.refuse(list, refused);
            return false;
        }
        self.change(added(list), |settings| {
            entries_mut(settings, list).push(entry);
        })
    }

    /// What `entry`, being added to `list`, is: read on a thread of its
    /// own, waited for [`EXAMINE_WAIT`] at most.
    fn examine_added(&self, list: ui::PathList, entry: &str) -> Examined {
        let examine = self.0.borrow().examine(list);
        let answer = examine_off(&examine, list, entry.to_owned()).map_err(|error| {
            tracing::error!(%entry, ?list, %error, "line not read: no thread");
            ui::EntryError::Unreadable
        })?;
        answer
            .recv_timeout(EXAMINE_WAIT)
            .unwrap_or_else(|error| {
                match error {
                    RecvTimeoutError::Timeout => tracing::warn!(%entry, ?list, waited = ?EXAMINE_WAIT, "line not read in time: its place does not answer"),
                    RecvTimeoutError::Disconnected => tracing::error!(%entry, ?list, "line reader ended without word"),
                }
                Err(ui::EntryError::Unreadable)
            })
    }

    /// Says why what was offered to `list` is not added.
    fn refuse(&self, list: ui::PathList, error: ui::EntryError) {
        if let Some(window) = self.0.borrow().window.upgrade() {
            let preferences = window.global::<ui::Preferences>();
            match list {
                ui::PathList::Exclusions => preferences.set_entry_error(error),
                ui::PathList::Folders => preferences.set_folder_entry_error(error),
            }
        }
    }

    /// Sets the field of `list` and what it says of it.
    fn set_entry(&self, list: ui::PathList, text: SharedString, error: ui::EntryError) {
        if let Some(window) = self.0.borrow().window.upgrade() {
            let preferences = window.global::<ui::Preferences>();
            match list {
                ui::PathList::Exclusions => preferences.set_entry(text),
                ui::PathList::Folders => preferences.set_folder_entry(text),
            }
        }
        self.refuse(list, error);
    }

    fn remove(&self, list: ui::PathList, index: i32) {
        let Some(index) = usize::try_from(index)
            .ok()
            .filter(|&index| index < entries(&self.0.borrow().settings, list).len())
        else {
            tracing::error!(index, ?list, "line to remove out of the list");
            return;
        };
        self.change(removed(list), |settings| {
            entries_mut(settings, list).remove(index);
        });
    }

    /// Asks the folder chooser for a folder to add to `list`, one at a
    /// time whatever the list.
    fn browse(&self, list: ui::PathList) {
        if !self.editable("folder chooser") {
            return;
        }
        let state = self.0.borrow();
        let Some(window) = state.window.upgrade() else {
            return;
        };
        let preferences = window.global::<ui::Preferences>();
        if preferences.get_browsing() {
            tracing::debug!("folder chooser already open");
            return;
        }
        preferences.set_browsing(true);
        let title = match list {
            ui::PathList::Exclusions => preferences.get_chooser_title(),
            ui::PathList::Folders => preferences.get_folder_chooser_title(),
        }
        .to_string();
        let choose = Rc::clone(&state.seams.choose_folder);
        drop(state);
        self.refuse(list, ui::EntryError::None);
        let screen = self.clone();
        choose(
            title,
            Box::new(move |chosen| {
                if let Some(window) = screen.0.borrow().window.upgrade() {
                    window.global::<ui::Preferences>().set_browsing(false);
                }
                match chosen {
                    // As chosen: never trimmed nor expanded, and what is
                    // typed in the field stays there.
                    Chosen::Folder(folder) => match folder.into_os_string().into_string() {
                        Ok(folder) => {
                            screen.add_entry(list, folder);
                        }
                        Err(folder) => {
                            tracing::warn!(folder = %folder.display(), "folder chosen not UTF-8: the settings cannot hold it");
                            screen.refuse(list, ui::EntryError::NotText);
                        }
                    },
                    Chosen::Cancelled => {}
                    Chosen::Unavailable => screen.refuse(list, ui::EntryError::Chooser),
                }
            }),
        );
    }
}

impl State {
    /// Shows the settings held.
    fn show(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let preferences = window.global::<ui::Preferences>();
        let home = self.seams.home.as_deref();
        for list in LISTS {
            let held = self.list(list);
            let rows: Vec<ui::PathRow> = entries(&self.settings, list)
                .iter()
                .map(|entry| ui::PathRow {
                    text: from_home(Path::new(entry), home).into(),
                    kind: held
                        .kinds
                        .get(entry)
                        .copied()
                        .unwrap_or(ui::PathKind::Reading),
                })
                .collect();
            // In place: the line that keeps the focus after a removal stays.
            crate::shown::show(&held.rows, rows);
        }
        let minutes = self
            .settings
            .min_age_minutes
            .unwrap_or(DEFAULT_MIN_AGE_MINUTES);
        let chosen = MIN_AGE_CHOICES
            .iter()
            .position(|&choice| choice == minutes)
            .map_or(-1, count);
        preferences.set_min_age_chosen(chosen);
        preferences.set_min_age_written(minutes.to_string().into());
        preferences.set_deletion_log(self.settings.deletion_log);
        preferences.set_language(match self.settings.language {
            None => ui::LanguageChoice::System,
            Some(Language::French) => ui::LanguageChoice::French,
            Some(Language::English) => ui::LanguageChoice::English,
        });
        preferences.set_theme(match self.settings.theme {
            None => ui::ThemeChoice::System,
            Some(Theme::Light) => ui::ThemeChoice::Light,
            Some(Theme::Dark) => ui::ThemeChoice::Dark,
        });
    }
}

fn show_theme(window: &slint::Weak<ui::AppWindow>, theme: Theme) {
    if let Some(window) = window.upgrade() {
        window
            .global::<ui::Preferences>()
            .invoke_show_theme(theme == Theme::Dark);
    }
}

/// What the screen says of a write.
const fn refusal_shown(written: Written) -> ui::SaveRefusal {
    match written {
        Written::Kept => ui::SaveRefusal::None,
        Written::Failed => ui::SaveRefusal::Failed,
        Written::ChangedElsewhere => ui::SaveRefusal::ChangedElsewhere,
    }
}

const fn language_of(choice: ui::LanguageChoice) -> Option<Language> {
    match choice {
        ui::LanguageChoice::System => None,
        ui::LanguageChoice::French => Some(Language::French),
        ui::LanguageChoice::English => Some(Language::English),
    }
}

const fn theme_of(choice: ui::ThemeChoice) -> Option<Theme> {
    match choice {
        ui::ThemeChoice::System => None,
        ui::ThemeChoice::Light => Some(Theme::Light),
        ui::ThemeChoice::Dark => Some(Theme::Dark),
    }
}

/// What the user typed, `~` (alone, or leading a path) standing for `home`:
/// the engine takes absolute paths only. Left as typed with no home.
fn expanded(typed: &str, home: Option<&Path>) -> String {
    let below = if typed == HOME_MARK {
        Some("")
    } else {
        typed
            .strip_prefix(HOME_MARK)
            .and_then(|rest| rest.strip_prefix('/'))
            // `~//x` is below home too: an absolute part would replace it.
            .map(|rest| rest.trim_start_matches('/'))
    };
    match (below, home.and_then(Path::to_str)) {
        (Some(""), Some(home)) => home.to_owned(),
        (Some(below), Some(home)) => Path::new(home).join(below).to_string_lossy().into_owned(),
        _ => typed.to_owned(),
    }
}

/// Whether two lines of `list` are the same to the engine. It matches
/// exclusions, paths and extensions, ignoring case: `/data/` is `/data`,
/// `*.LOG` is `*.log`; a folder to clean is the path it names: `/data/` is
/// `/data`, `/Data` is another.
fn same(list: ui::PathList, one: &str, other: &str) -> bool {
    match list {
        ui::PathList::Exclusions => {
            Path::new(&one.to_lowercase()) == Path::new(&other.to_lowercase())
        }
        ui::PathList::Folders => Path::new(one) == Path::new(other),
    }
}

/// What the screen traces when a line is added to `list`.
const fn added(list: ui::PathList) -> &'static str {
    match list {
        ui::PathList::Exclusions => "exclusion added",
        ui::PathList::Folders => "folder to clean added",
    }
}

/// What the screen traces when a line is taken out of `list`.
const fn removed(list: ui::PathList) -> &'static str {
    match list {
        ui::PathList::Exclusions => "exclusion removed",
        ui::PathList::Folders => "folder to clean removed",
    }
}

/// What an exclusion is: refused by the engine (its place unreadable, or
/// invalid), an extension, or a path read on the disk now, through a link
/// as the engine compares it. Reading where it lies waits on a mount that
/// stopped answering: called on a thread of its own ([`examine_off`]).
fn examine(entry: &str) -> Examined {
    let path = Path::new(entry);
    match Exclusions::new([path.to_path_buf()]) {
        Err(error @ ExclusionError::Unresolvable { .. }) => {
            tracing::debug!(%entry, %error, "exclusion whose place cannot be read");
            return Err(ui::EntryError::Unreadable);
        }
        Err(error) => {
            tracing::debug!(%entry, %error, "exclusion the engine refuses");
            return Err(ui::EntryError::Invalid);
        }
        Ok(_) => {}
    }
    // Taken by the engine and not absolute: an extension (`*.log`).
    if !path.is_absolute() {
        return Ok(ui::PathKind::Extension);
    }
    Ok(match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => ui::PathKind::Folder,
        Ok(_) => ui::PathKind::File,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(%entry, %error, "exclusion not read: shown as absent");
            }
            ui::PathKind::Absent
        }
    })
}

/// What a folder to clean is, as the engine judges it for a scan
/// ([`CustomFolder::check`]): a folder it walks, or why it would leave it
/// alone; invalid as written (it stops every analysis). Reading it waits on
/// a mount that stopped answering: called on a thread of its own
/// ([`examine_off`]).
fn examine_folder(entry: &str) -> Examined {
    let folder = CustomFolder::new(PathBuf::from(entry)).map_err(|error| {
        tracing::debug!(%entry, %error, "folder to clean the engine refuses");
        ui::EntryError::Invalid
    })?;
    let variables = okapi_platform::template_variables().map_err(|error| {
        tracing::warn!(%entry, ?error, "variables unavailable: folder to clean not judged");
        ui::EntryError::Unreadable
    })?;
    Ok(match folder.check(&variables) {
        Ok(()) => ui::PathKind::Folder,
        Err(refusal) => {
            tracing::debug!(%entry, ?refusal, "folder to clean an analysis would leave alone");
            match refusal {
                CustomFolderRefusal::Absent => ui::PathKind::Absent,
                CustomFolderRefusal::Link => ui::PathKind::Link,
                CustomFolderRefusal::NotADirectory => ui::PathKind::NotFolder,
                CustomFolderRefusal::Replaced | CustomFolderRefusal::Unreadable(_) => {
                    ui::PathKind::Unreadable
                }
                CustomFolderRefusal::PersonalUnknown(_) => ui::PathKind::Unverifiable,
                CustomFolderRefusal::NotOwned => ui::PathKind::NotOwned,
                CustomFolderRefusal::Protected(_) => ui::PathKind::Protected,
                CustomFolderRefusal::Personal => ui::PathKind::Personal,
                CustomFolderRefusal::Trash => ui::PathKind::Trash,
                CustomFolderRefusal::TrashUnknown(_) => ui::PathKind::TrashUnknown,
                CustomFolderRefusal::UnsureFilesystem => ui::PathKind::Unsure,
            }
        }
    })
}

/// Whether a line of `list` read to be `kind` may be added: any exclusion
/// the engine takes (an absent path still protects what will come there),
/// only a folder to clean an analysis would walk. Why not otherwise.
fn admitted(list: ui::PathList, kind: ui::PathKind) -> Examined {
    match (list, kind) {
        (ui::PathList::Exclusions, _) | (ui::PathList::Folders, ui::PathKind::Folder) => Ok(kind),
        (ui::PathList::Folders, refused) => Err(match refused {
            ui::PathKind::Absent => ui::EntryError::Absent,
            ui::PathKind::Link => ui::EntryError::Link,
            ui::PathKind::NotFolder | ui::PathKind::File => ui::EntryError::NotFolder,
            ui::PathKind::Protected => ui::EntryError::Protected,
            ui::PathKind::Personal => ui::EntryError::Personal,
            ui::PathKind::Trash => ui::EntryError::Trash,
            ui::PathKind::TrashUnknown => ui::EntryError::TrashUnknown,
            ui::PathKind::NotOwned => ui::EntryError::NotOwned,
            ui::PathKind::Unsure => ui::EntryError::Unsure,
            ui::PathKind::Unverifiable => ui::EntryError::Unverifiable,
            ui::PathKind::Unreadable | ui::PathKind::Reading => ui::EntryError::Unreadable,
            ui::PathKind::Folder | ui::PathKind::Extension | ui::PathKind::Invalid => {
                ui::EntryError::Invalid
            }
        }),
    }
}

/// Runs `examine` on `entry` on a thread of its own; its answer comes on
/// the receiver returned, or never when the place does not answer.
fn examine_off(
    examine: &Examine,
    list: ui::PathList,
    entry: String,
) -> std::io::Result<Receiver<Examined>> {
    let (sender, answer) = mpsc::channel();
    let examine = Arc::clone(examine);
    let span = tracing::Span::current();
    std::thread::Builder::new()
        .name(EXAMINER_NAME.to_owned())
        .spawn(move || {
            span.in_scope(|| {
                tracing::debug!(%entry, ?list, "line being read");
                let started = Instant::now();
                let examined = examine(&entry);
                tracing::debug!(%entry, ?list, ?examined, elapsed = ?started.elapsed(), "line read");
                if sender.send(examined).is_err() {
                    tracing::info!(%entry, ?list, elapsed = ?started.elapsed(), "line read too late: no one waits for it");
                }
            });
        })?;
    Ok(answer)
}

/// How a line of `list` read is shown. An exclusion the engine refuses,
/// whatever why, stops every analysis, which says why; a folder to clean
/// does so only when it is invalid as written, and is otherwise left alone,
/// as unreadable.
const fn kind_shown(list: ui::PathList, examined: Examined) -> ui::PathKind {
    match (list, examined) {
        (_, Ok(kind)) => kind,
        (ui::PathList::Exclusions, Err(_))
        | (ui::PathList::Folders, Err(ui::EntryError::Invalid)) => ui::PathKind::Invalid,
        (ui::PathList::Folders, Err(_)) => ui::PathKind::Unreadable,
    }
}

#[cfg(test)]
pub mod tests;
