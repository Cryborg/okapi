//! The applications screen: what is installed (`okapi_tools::apps`), read
//! on threads of its own and shown as it comes (each source, then the
//! icons, then what the folders take), sorted by the space each takes;
//! the detail of one; its uninstallation by the system's own services, on
//! a thread that writes its history line itself. Nothing here runs a
//! command nor removes a file.

pub mod leftovers;
pub mod old_versions;
mod rows;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use jiff::Timestamp;
use okapi_rules::Language;
use okapi_store::{HistoryUnavailable, PackageSource, UninstallEntry, UninstallPart};
use okapi_tools::apps::icons::IconPixels;
use okapi_tools::apps::uninstall::{
    Checked, Ended, Going, Gone, Removal, Step, Target, Uninstaller, Why, check, remove_as_shown,
};
use okapi_tools::apps::{AppKey, Arrival, Part, Rest, Source, SourceReading};
use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use crate::dashboard::TOAST_SHOWN_FOR;
use crate::dates::MINUTES_EVERY;
use crate::shown::{shown, size};
use crate::ui;
use rows::Entry;

/// How often the screen reads what comes.
const READ_EVERY: Duration = Duration::from_millis(500);
/// How long sizes that come in wait before they move the lines again (a
/// search, a filter, a new source sort at once).
const SORT_EVERY: Duration = Duration::from_secs(2);
/// A list older than this is read again when the page is shown.
const READ_AGAIN_AFTER: jiff::SignedDuration = jiff::SignedDuration::from_mins(5);

/// Reads the sources in `language`: what each finds comes on the sender;
/// the flag stops measuring.
pub type Read = Rc<dyn Fn(Language, &Sender<Arrival>, &Arc<AtomicBool>)>;
/// Writes the history line of an uninstallation.
pub type Record = crate::history::Record<UninstallEntry>;

/// What the screen reads and asks of the machine: the system's, or a test's.
#[derive(Clone)]
pub struct AppsSeams {
    pub read: Read,
    pub uninstaller: Arc<dyn Uninstaller>,
    pub record: Record,
    pub now: fn() -> Timestamp,
    pub leftovers: leftovers::LeftoverSeams,
    pub old_versions: old_versions::OldVersionSeams,
    /// The home folder, as a leftover's path is shown under it.
    pub home: Option<PathBuf>,
    /// Where a copy's date is said.
    pub zone: jiff::tz::TimeZone,
}

/// How the applications seen name `source`: written and read by this key.
const fn source_key(source: Source) -> &'static str {
    match source {
        Source::Deb => "deb",
        Source::Snap => "snap",
        Source::Flatpak => "flatpak",
    }
}

/// What a day is, in seconds: an application seen is dated by its day.
const SECONDS_PER_DAY: i64 = 86_400;

/// What Okapi keeps of `app`, seen installed on `day`: the names a folder it
/// left may bear.
fn seen_app(app: &okapi_tools::apps::App, day: i64) -> okapi_store::SeenApp {
    let source = source_key(app.source);
    // A flatpak's id ends with the application's own name (`org.gimp.GIMP`).
    let last = (app.source == Source::Flatpak)
        .then(|| app.id.rsplit('.').next().map(str::to_owned))
        .flatten();
    let names = std::iter::once(app.package.clone())
        .chain(last)
        .chain(std::iter::once(app.name.clone()))
        .chain(app.aliases.iter().cloned())
        .collect();
    okapi_store::SeenApp {
        source: source.to_owned(),
        id: app.id.clone(),
        shown: app.name.clone(),
        names,
        day,
    }
}

pub use crate::user_files::ReadExclusions;

impl std::fmt::Debug for AppsSeams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AppsSeams").finish_non_exhaustive()
    }
}

impl AppsSeams {
    /// The system's sources, services and history, the home folder `home`
    /// (canonical, as the leftovers' paths are).
    pub fn system(home: Option<PathBuf>) -> Self {
        let home = home.map(|home| home.canonicalize().unwrap_or(home));
        Self {
            read: Rc::new(read_system),
            uninstaller: system_uninstaller(),
            record: Arc::new(record_system),
            now: Timestamp::now,
            leftovers: leftovers::LeftoverSeams::system(),
            old_versions: old_versions::OldVersionSeams::system(),
            home,
            zone: crate::dashboard::system_zone(),
        }
    }
}

/// Writes `entry` in the user's history (its write traces its own failure).
fn record_system(entry: &UninstallEntry) -> Result<(), HistoryUnavailable> {
    crate::history::open_to_record("uninstallation")?.record_uninstall(entry)?;
    Ok(())
}

/// The system's sources, each on a thread of its own.
fn read_system(language: Language, sink: &Sender<Arrival>, stop: &Arc<AtomicBool>) {
    let reading = okapi_tools::apps::Reading::session(language);
    let variables = okapi_platform::template_variables()
        .inspect_err(|error| {
            tracing::warn!(
                ?error,
                "no home folder: the data of snaps and flatpaks not read"
            );
        })
        .ok();
    let folder = |wanted| {
        variables
            .as_ref()
            .and_then(|variables| variables.value_of(wanted))
            .map(Path::to_path_buf)
    };
    let home = folder(okapi_rules::Variable::Home);
    let data_home = folder(okapi_rules::Variable::LocalShare);
    let debs = {
        let reading = reading.clone();
        Box::new(move || {
            okapi_tools::apps::read_debs(&okapi_tools::apps::DebPlaces::system(), &reading)
        })
    };
    let flatpaks = {
        let home = home.clone();
        let reading = reading.clone();
        Box::new(move || {
            okapi_tools::apps::flatpak::read_flatpaks(
                &okapi_tools::apps::flatpak::installations(data_home.as_deref()),
                home.as_deref(),
                &reading,
            )
        })
    };
    let snaps = Box::new(move || system_snaps(home.as_deref(), &reading));
    okapi_tools::apps::read_all(
        vec![
            (Source::Deb, debs),
            (Source::Snap, snaps),
            (Source::Flatpak, flatpaks),
        ],
        sink,
        stop,
    );
}

/// The snaps snapd lists.
#[cfg(target_os = "linux")]
fn system_snaps(
    home: Option<&std::path::Path>,
    reading: &okapi_tools::apps::Reading,
) -> SourceReading {
    okapi_tools::apps::system::snaps(home, reading)
}

#[cfg(not(target_os = "linux"))]
fn system_snaps(
    _home: Option<&std::path::Path>,
    _reading: &okapi_tools::apps::Reading,
) -> SourceReading {
    SourceReading::Absent
}

#[cfg(target_os = "linux")]
fn system_uninstaller() -> Arc<dyn Uninstaller> {
    Arc::new(okapi_tools::apps::system::Services::session())
}

/// No service to uninstall with elsewhere yet.
#[cfg(not(target_os = "linux"))]
fn system_uninstaller() -> Arc<dyn Uninstaller> {
    struct Nowhere;
    impl Uninstaller for Nowhere {
        fn simulate(&self, _target: &Target) -> Result<Vec<Going>, Why> {
            Err(Why::Absent)
        }
        fn status(&self) -> Result<okapi_tools::apps::dpkg::Status, Why> {
            Err(Why::Absent)
        }
        fn remove(
            &self,
            _target: &Target,
            _going: &[Going],
            _steps: &Sender<Step>,
            _cancel: &AtomicBool,
        ) -> Ended {
            Ended::Failed(Why::Absent)
        }
    }
    Arc::new(Nowhere)
}

/// What a source is, as shown.
fn source_shown(reading: &SourceReading) -> ui::SourceShown {
    match reading {
        SourceReading::Read { partial: false, .. } => ui::SourceShown::Read,
        SourceReading::Read { partial: true, .. } => ui::SourceShown::Partial,
        SourceReading::Absent => ui::SourceShown::Absent,
        SourceReading::Busy => ui::SourceShown::Busy,
        SourceReading::Failed(_) => ui::SourceShown::Failed,
    }
}

/// A reading of the sources under way.
struct Reading {
    receiver: Receiver<Arrival>,
    stop: Arc<AtomicBool>,
}

/// What an uninstallation's threads say.
#[derive(Debug)]
enum Said {
    Checked(Result<Checked, Why>),
    Step(Step),
    Ended { ended: Ended, recorded: bool },
}

/// The window of an uninstallation running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Window {
    #[default]
    Shown,
    /// Closed: the uninstallation goes on.
    Hidden,
    /// Shown again because Okapi was asked to close.
    CloseAsked,
}

/// Whether an uninstallation running may be cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Cancelling {
    /// Not yet (the rights are asked, or the service never says).
    #[default]
    NotYet,
    Possible,
    /// It could be, no longer is.
    TooLate,
}

/// An uninstallation running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Run {
    window: Window,
    cancelling: Cancelling,
    /// Another installation holds the package manager.
    waiting: bool,
}

/// Where an uninstallation stands. Its window shows in every phase but a
/// run whose window was closed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Checking,
    Confirm(Vec<Going>),
    Refused(Vec<String>),
    Running(Run),
    Failed(Why),
}

/// An uninstallation, asked for from its detail.
struct Operation {
    key: AppKey,
    name: String,
    target: Target,
    /// What it frees whatever is chosen (the application, its old
    /// versions; a deb's packages or a flatpak's extensions once checked),
    /// when every size is known; its data are added when they go.
    freed: Option<u64>,
    /// The row's estimate of the application and its old versions: what a
    /// check that sizes nothing falls back on, and a retry starts from.
    estimated: Option<u64>,
    data_bytes: u64,
    phase: Phase,
    receiver: Receiver<Said>,
    cancel: Arc<AtomicBool>,
}

impl Operation {
    const fn purges(&self) -> bool {
        matches!(self.target, Target::Snap { purge: true, .. })
    }

    /// What it frees, its data with it when they go; unknown when a size
    /// is.
    fn freed_now(&self) -> Option<u64> {
        if self.purges() {
            self.freed
                .map(|freed| freed.saturating_add(self.data_bytes))
        } else {
            self.freed
        }
    }

    const fn running(&self) -> bool {
        matches!(self.phase, Phase::Running(_))
    }

    const fn shown(&self) -> bool {
        match self.phase {
            Phase::Running(run) => !matches!(run.window, Window::Hidden),
            _ => true,
        }
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts of the screen: a sort owed, the list focused, the page shown, the note held"
)]
struct State {
    window: slint::Weak<ui::AppWindow>,
    seams: AppsSeams,
    language: Language,
    entries: HashMap<AppKey, Entry>,
    icons: HashMap<AppKey, slint::Image>,
    sources: HashMap<Source, ui::SourceShown>,
    rests: HashMap<Source, Rest>,
    /// The keys in their order, sorted again when owed (`show_lines`).
    order: Vec<AppKey>,
    sort_owed: bool,
    readings_since_sort: u32,
    /// The lines shown, in their order.
    shown: Vec<AppKey>,
    pointed: Option<usize>,
    list_focused: bool,
    /// The line of the keyboard, and the one whose detail is open.
    current: Option<AppKey>,
    chosen: Option<AppKey>,
    reading: Option<Reading>,
    /// When the list was last read whole; `None` when it must be read again.
    read_at: Option<Timestamp>,
    on_page: bool,
    operation: Option<Operation>,
    toast_held: bool,
    line_model: Rc<VecModel<ui::AppLine>>,
    going_model: Rc<VecModel<SharedString>>,
    timer: slint::Timer,
    minutes_timer: slint::Timer,
    toast_timer: slint::Timer,
    /// The exclusions of the settings: never a leftover there.
    exclusions: ReadExclusions,
    leftovers: leftovers::LeftoversState,
    old_versions: old_versions::OldVersionsState,
}

/// The applications screen.
#[derive(Clone)]
pub struct AppsScreen(Rc<RefCell<State>>);

impl std::fmt::Debug for AppsScreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AppsScreen").finish_non_exhaustive()
    }
}

impl AppsScreen {
    pub fn attach(
        window: &ui::AppWindow,
        seams: AppsSeams,
        exclusions: ReadExclusions,
        language: Language,
    ) -> Self {
        let state = State {
            exclusions,
            leftovers: leftovers::LeftoversState::default(),
            old_versions: old_versions::OldVersionsState::default(),
            window: window.as_weak(),
            seams,
            language,
            entries: HashMap::new(),
            icons: HashMap::new(),
            sources: HashMap::new(),
            rests: HashMap::new(),
            order: Vec::new(),
            sort_owed: false,
            readings_since_sort: 0,
            shown: Vec::new(),
            pointed: None,
            list_focused: false,
            current: None,
            chosen: None,
            reading: None,
            read_at: None,
            on_page: false,
            operation: None,
            toast_held: false,
            line_model: Rc::new(VecModel::default()),
            going_model: Rc::new(VecModel::default()),
            timer: slint::Timer::default(),
            minutes_timer: slint::Timer::default(),
            toast_timer: slint::Timer::default(),
        };
        let apps = window.global::<ui::Apps>();
        apps.set_lines(ModelRc::from(Rc::clone(&state.line_model)));
        apps.set_going(ModelRc::from(Rc::clone(&state.going_model)));
        apps.set_debs(ui::SourceShown::Reading);
        apps.set_snaps(ui::SourceShown::Reading);
        apps.set_flatpaks(ui::SourceShown::Reading);
        let screen = Self(Rc::new(RefCell::new(state)));
        screen.attach_leftovers(window);
        screen.attach_old_versions(window);
        let weak = Rc::downgrade(&screen.0);
        let on = move |action: fn(&Self)| {
            let weak = weak.clone();
            move || {
                if let Some(state) = weak.upgrade() {
                    action(&Self(state));
                }
            }
        };
        apps.on_refresh(on(Self::refresh));
        apps.on_searched(on(Self::show_lines_now));
        apps.on_filtered(on(Self::show_lines_now));
        apps.on_close_detail(on(Self::close_detail));
        apps.on_uninstall(on(Self::uninstall));
        apps.on_confirm(on(Self::confirm));
        apps.on_cancel(on(Self::cancel));
        apps.on_close_modal(on(Self::close_modal));
        apps.on_retry(on(Self::retry));
        apps.on_close_toast(on(Self::close_toast));
        let weak = Rc::downgrade(&screen.0);
        let with_index = move |action: fn(&Self, usize)| {
            let weak = weak.clone();
            move |index: i32| {
                if let Some(state) = weak.upgrade() {
                    if let Ok(index) = usize::try_from(index) {
                        action(&Self(state), index);
                    } else {
                        tracing::warn!(index, "a line out of the list refused");
                    }
                }
            }
        };
        apps.on_choose(with_index(Self::choose));
        apps.on_move_to(with_index(Self::move_to));
        let weak = Rc::downgrade(&screen.0);
        apps.on_point(move |index| {
            if let Some(state) = weak.upgrade() {
                state.borrow_mut().pointed = usize::try_from(index).ok();
            }
        });
        let weak = Rc::downgrade(&screen.0);
        apps.on_list_focus(move |focused| {
            if let Some(state) = weak.upgrade() {
                state.borrow_mut().list_focused = focused;
            }
        });
        let weak = Rc::downgrade(&screen.0);
        apps.on_toast_held(move |held| {
            if let Some(state) = weak.upgrade() {
                Self(state).hold_toast(held);
            }
        });
        screen
    }

    fn with_window(&self, show: impl FnOnce(&ui::AppWindow)) {
        let window = self.0.borrow().window.upgrade();
        if let Some(window) = window {
            show(&window);
        }
    }

    /// Follows the page shown: the list is read when it is first shown, and
    /// again when it is old or a source could not be read; leaving the page
    /// stops measuring (the list is then read again on return).
    pub fn page_shown(&self, page: ui::Page) {
        let on_page = page == ui::Page::Apps;
        {
            let mut state = self.0.borrow_mut();
            state.on_page = on_page;
            // What held the lines went with the page.
            state.pointed = None;
            state.list_focused = false;
        }
        if !on_page {
            let state = self.0.borrow();
            state.minutes_timer.stop();
            if let Some(reading) = &state.reading {
                tracing::info!("page left: measuring stopped");
                reading.stop.store(true, Ordering::Relaxed);
            }
            drop(state);
            self.stop_old_versions();
            return;
        }
        let (reading, stale, running) = {
            let state = self.0.borrow();
            let now = (state.seams.now)();
            let failed = state.sources.values().any(|source| {
                matches!(
                    source,
                    ui::SourceShown::Busy | ui::SourceShown::Failed | ui::SourceShown::Partial
                )
            });
            let old = state
                .read_at
                .is_none_or(|read_at| now.duration_since(read_at) >= READ_AGAIN_AFTER);
            (
                state.reading.is_some(),
                old || failed,
                state.operation.as_ref().is_some_and(Operation::running),
            )
        };
        if !reading && !running && stale {
            self.refresh();
        }
        // A note arrived meanwhile is seen now: it goes after a while.
        let toast = self
            .0
            .borrow()
            .window
            .upgrade()
            .is_some_and(|window| window.global::<ui::Apps>().get_toast());
        if toast {
            let held = self.0.borrow().toast_held;
            self.hold_toast(held);
        }
        self.show_operation();
        self.watch();
    }

    fn watch(&self) {
        let weak = Rc::downgrade(&self.0);
        self.0
            .borrow()
            .timer
            .start(slint::TimerMode::Repeated, READ_EVERY, move || {
                if let Some(state) = weak.upgrade() {
                    Self(state).read();
                }
            });
        let weak = Rc::downgrade(&self.0);
        self.0
            .borrow()
            .minutes_timer
            .start(slint::TimerMode::Repeated, MINUTES_EVERY, move || {
                if let Some(state) = weak.upgrade() {
                    Self(state).show_minutes();
                }
            });
    }

    /// Reads the sources again, from nothing.
    fn refresh(&self) {
        if self
            .0
            .borrow()
            .operation
            .as_ref()
            .is_some_and(Operation::running)
        {
            // dpkg's own updates would say « an installation runs ».
            tracing::info!("not read again during an uninstallation");
            return;
        }
        if let Some(reading) = self.0.borrow_mut().reading.take() {
            reading.stop.store(true, Ordering::Relaxed);
        }
        let (sink, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let mut state = self.0.borrow_mut();
            state.entries.clear();
            state.icons.clear();
            state.sources.clear();
            state.rests.clear();
            state.order.clear();
            state.read_at = None;
            state.reading = Some(Reading {
                receiver,
                stop: Arc::clone(&stop),
            });
        }
        tracing::info!("applications read");
        let (read, language) = {
            let state = self.0.borrow();
            (Rc::clone(&state.seams.read), state.language)
        };
        // The screen keeps no sender: the reading ends when its threads do.
        read(language, &sink, &stop);
        drop(sink);
        // They read their own sources: never waiting for the list's measures.
        self.analyse_leftovers();
        self.find_old_versions();
        self.show_lines_now();
        self.watch();
    }

    /// One reading: what came from the sources, from an uninstallation; the
    /// sort owed, every so often.
    fn read(&self) {
        self.take_arrivals();
        self.take_said();
        self.take_leftovers();
        self.take_old_versions();
        let (owed, on_page, busy) = {
            let mut state = self.0.borrow_mut();
            state.readings_since_sort = state.readings_since_sort.saturating_add(1);
            (
                state.sort_owed,
                state.on_page,
                state.reading.is_some()
                    || state.operation.is_some()
                    || state.leftovers.busy()
                    || state.old_versions.busy(),
            )
        };
        if owed {
            self.show_lines(false);
        }
        if !on_page && !busy {
            self.0.borrow().timer.stop();
        }
    }

    fn take_arrivals(&self) {
        let mut ended = false;
        loop {
            let next = {
                let state = self.0.borrow();
                let Some(reading) = &state.reading else { break };
                reading.receiver.try_recv()
            };
            match next {
                Ok(arrival) => self.take(arrival),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    ended = true;
                    break;
                }
            }
        }
        if ended {
            self.reading_ended();
        }
    }

    fn take(&self, arrival: Arrival) {
        let mut state = self.0.borrow_mut();
        match arrival {
            Arrival::Source(source, reading) => {
                state.sources.insert(source, source_shown(&reading));
                if let SourceReading::Read { apps, rest, .. } = reading {
                    state.rests.insert(source, rest);
                    for app in apps {
                        state.order.push(app.key());
                        state.entries.insert(app.key(), Entry::new(app));
                    }
                }
                // A new source is sorted in at once, unless held.
                state.sort_owed = true;
                state.readings_since_sort = u32::MAX;
            }
            Arrival::Icon(key, pixels) => {
                state.icons.insert(key, image(&pixels));
                state.sort_owed = true;
            }
            Arrival::Measured(key, part, measured) => {
                if let Some(entry) = state.entries.get_mut(&key) {
                    match part {
                        Part::App => entry.deployment = Some(measured),
                        Part::Data => entry.data = Some(measured),
                    }
                    state.sort_owed = true;
                }
            }
        }
    }

    /// Every thread of the reading is gone: stopped, the list is read again
    /// on return; otherwise it is whole, and what never came is said.
    fn reading_ended(&self) {
        let now = (self.0.borrow().seams.now)();
        let mut remembered = Vec::new();
        let mut state = self.0.borrow_mut();
        let Some(reading) = state.reading.take() else {
            return;
        };
        if reading.stop.load(Ordering::Relaxed) {
            tracing::info!(
                apps = state.entries.len(),
                "reading stopped: read again on return"
            );
            state.read_at = None;
        } else {
            for source in [Source::Deb, Source::Snap, Source::Flatpak] {
                state.sources.entry(source).or_insert_with(|| {
                    tracing::error!(?source, "a source's thread ended without a word");
                    ui::SourceShown::Failed
                });
            }
            let mut unmeasured = 0_usize;
            for entry in state.entries.values_mut() {
                unmeasured += entry.never_measured();
            }
            if unmeasured > 0 {
                tracing::error!(
                    unmeasured,
                    "folders never measured: their thread ended early"
                );
            }
            state.read_at = Some(now);
            let partial = state
                .sources
                .values()
                .any(|source| *source == ui::SourceShown::Partial);
            tracing::info!(
                apps = state.entries.len(),
                partial,
                "applications read to their end"
            );
            // Only a whole reading says which are installed.
            let whole = state
                .sources
                .values()
                .all(|source| matches!(source, ui::SourceShown::Read | ui::SourceShown::Absent));
            if whole {
                let day = now.as_second().div_euclid(SECONDS_PER_DAY);
                remembered = state
                    .entries
                    .values()
                    .map(|entry| seen_app(&entry.app, day))
                    .collect();
            }
        }
        state.sort_owed = true;
        state.readings_since_sort = u32::MAX;
        drop(state);
        if !remembered.is_empty() {
            self.remember_seen(remembered);
        }
        self.show_lines(false);
    }

    /// Shows the lines now, sorted again (a search, a filter, a new list).
    fn show_lines_now(&self) {
        {
            let mut state = self.0.borrow_mut();
            state.sort_owed = true;
            state.pointed = None;
        }
        self.show_lines(true);
    }

    /// Sorts again when owed: now if `now`, else when nothing is held under
    /// the pointer or the keyboard and the last sort is old enough.
    fn sort_if_owed(state: &mut State, now: bool) {
        let held = state.pointed.is_some() || state.list_focused;
        let every =
            u32::try_from(SORT_EVERY.as_millis() / READ_EVERY.as_millis()).unwrap_or(u32::MAX);
        if state.sort_owed && (now || (!held && state.readings_since_sort >= every)) {
            state.order = rows::ordered(state.entries.values());
            state.sort_owed = false;
            state.readings_since_sort = 0;
        }
    }

    /// Shows the lines, the sources' states, the line of the keyboard and
    /// the detail open.
    fn show_lines(&self, now: bool) {
        let window = self.0.borrow().window.upgrade();
        let Some(window) = window else { return };
        let apps = window.global::<ui::Apps>();
        let (search, filter) = (apps.get_search().to_string(), apps.get_filter());
        let mut state = self.0.borrow_mut();
        Self::sort_if_owed(&mut state, now);
        let visible: Vec<AppKey> = state
            .order
            .iter()
            .filter(|key| {
                state
                    .entries
                    .get(*key)
                    .is_some_and(|entry| entry.matches(&search, filter))
            })
            .cloned()
            .collect();
        let largest = visible
            .iter()
            .filter_map(|key| state.entries.get(key))
            .map(Entry::total)
            .max()
            .unwrap_or(0);
        let uninstalling = state
            .operation
            .as_ref()
            .filter(|operation| operation.running())
            .map(|operation| operation.key.clone());
        let lines: Vec<ui::AppLine> = visible
            .iter()
            .filter_map(|key| {
                let entry = state.entries.get(key)?;
                Some(rows::line(
                    entry,
                    state.icons.get(key),
                    largest,
                    uninstalling.as_ref() == Some(key),
                ))
            })
            .collect();
        crate::shown::show(&state.line_model, lines);
        state.shown = visible;
        Self::show_sources(&state, &apps);
        let position = |key: Option<&AppKey>| {
            key.and_then(|key| state.shown.iter().position(|shown| shown == key))
        };
        apps.set_chosen(position(state.chosen.as_ref()).map_or(-1, shown));
        apps.set_current(position(state.current.as_ref()).map_or(0, shown));
        if let Some(entry) = state
            .chosen
            .as_ref()
            .and_then(|chosen| state.entries.get(chosen))
        {
            let key = entry.app.key();
            apps.set_detail(detail(
                entry,
                state.icons.get(&key),
                uninstalling.as_ref() == Some(&key),
            ));
        }
        apps.set_busy(uninstalling.is_some() || state.old_versions.retiring());
        apps.set_reading(state.reading.is_some());
        drop(state);
        self.show_minutes();
    }

    fn show_sources(state: &State, apps: &ui::Apps<'_>) {
        let rest = |source| state.rests.get(&source).copied().unwrap_or_default();
        let source = |source| {
            state
                .sources
                .get(&source)
                .copied()
                .unwrap_or(ui::SourceShown::Reading)
        };
        apps.set_app_count(shown(state.entries.len()));
        apps.set_has_flatpak(
            state
                .entries
                .keys()
                .any(|key| key.source == Source::Flatpak),
        );
        apps.set_debs(source(Source::Deb));
        apps.set_snaps(source(Source::Snap));
        apps.set_flatpaks(source(Source::Flatpak));
        apps.set_rest_packages(shown(rest(Source::Deb).count));
        apps.set_rest_packages_size(size(rest(Source::Deb).bytes));
        apps.set_rest_snaps(shown(rest(Source::Snap).count));
        apps.set_rest_snaps_size(size(rest(Source::Snap).bytes));
    }

    fn show_minutes(&self) {
        let (read_at, now) = {
            let state = self.0.borrow();
            (state.read_at, (state.seams.now)())
        };
        let minutes = read_at.map_or(-1, |read_at| {
            shown(now.duration_since(read_at).as_mins().max(0))
        });
        self.with_window(|window| window.global::<ui::Apps>().set_minutes_ago(minutes));
    }

    fn choose(&self, index: usize) {
        let key = self.0.borrow().shown.get(index).cloned();
        let Some(key) = key else {
            tracing::warn!(index, "a line out of the list refused");
            return;
        };
        tracing::debug!(?key, "application chosen");
        {
            let mut state = self.0.borrow_mut();
            state.current = Some(key.clone());
            state.chosen = Some(key);
        }
        self.show_lines(false);
    }

    /// The keyboard moved to line `index`: it stays on its application
    /// whatever the sort.
    fn move_to(&self, index: usize) {
        let key = self.0.borrow().shown.get(index).cloned();
        self.0.borrow_mut().current = key;
    }

    fn close_detail(&self) {
        self.0.borrow_mut().chosen = None;
        self.show_lines(false);
    }

    /// Opens the uninstallation of the application chosen, or shows the one
    /// running.
    fn uninstall(&self) {
        if self.0.borrow().old_versions.retiring() {
            tracing::info!("an uninstallation refused: old versions are being removed");
            return;
        }
        let running = {
            let mut state = self.0.borrow_mut();
            let chosen = state.chosen.clone();
            match state.operation.as_mut() {
                Some(operation) => {
                    if chosen.as_ref() != Some(&operation.key) {
                        tracing::info!(asked = ?chosen, running = ?operation.key, "another uninstallation refused: one runs");
                    }
                    if let Phase::Running(run) = &mut operation.phase
                        && run.window == Window::Hidden
                    {
                        run.window = Window::Shown;
                    }
                    true
                }
                None => false,
            }
        };
        if running {
            self.show_operation();
            return;
        }
        let entry = {
            let state = self.0.borrow();
            state
                .chosen
                .as_ref()
                .and_then(|chosen| state.entries.get(chosen))
                .cloned()
        };
        let Some(entry) = entry else { return };
        let app = &entry.app;
        let target = match (&app.target, app.system) {
            (Some(target), false) => target.clone(),
            (_, true) => {
                tracing::warn!(id = %app.id, "the system's application never uninstalled");
                return;
            }
            (None, false) => {
                tracing::warn!(id = %app.id, "an application Okapi does not uninstall");
                return;
            }
        };
        let freed = entry
            .app_bytes()
            .map(|bytes| bytes.saturating_add(app.old_bytes));
        tracing::info!(?target, "uninstallation asked");
        self.with_window(|window| {
            let apps = window.global::<ui::Apps>();
            apps.set_purge(false);
            apps.set_has_data(entry.data_bytes() > 0);
            apps.set_data_size(size(entry.data_bytes()));
        });
        self.check(
            app.key(),
            app.name.clone(),
            target,
            freed,
            entry.data_bytes(),
        );
    }

    /// Checks what `target` would take with it, on a thread.
    fn check(
        &self,
        key: AppKey,
        name: String,
        target: Target,
        freed: Option<u64>,
        data_bytes: u64,
    ) {
        let (said, receiver) = mpsc::channel();
        let uninstaller = Arc::clone(&self.0.borrow().seams.uninstaller);
        let checked = target.clone();
        let spawned = std::thread::Builder::new()
            .name("okapi-uninstall-check".to_owned())
            .spawn(move || {
                let _ = said.send(Said::Checked(check(&*uninstaller, &checked)));
            });
        let phase = match spawned {
            Ok(_) => Phase::Checking,
            Err(error) => {
                tracing::error!(%error, "no thread to check an uninstallation");
                Phase::Failed(Why::Unanswered)
            }
        };
        self.0.borrow_mut().operation = Some(Operation {
            key,
            name,
            target,
            freed,
            estimated: freed,
            data_bytes,
            phase,
            receiver,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        self.show_operation();
        self.watch();
    }

    /// Removes what the user confirmed on a thread, which checks a deb once
    /// more (what goes is what was shown) and writes the history line of
    /// what went.
    fn confirm(&self) {
        let purge = self
            .0
            .borrow()
            .window
            .upgrade()
            .is_some_and(|window| window.global::<ui::Apps>().get_purge());
        let started = {
            let mut state = self.0.borrow_mut();
            let uninstaller = Arc::clone(&state.seams.uninstaller);
            let record = Arc::clone(&state.seams.record);
            let now = (state.seams.now)();
            let Some(operation) = state.operation.as_mut() else {
                tracing::warn!("a confirmation without an uninstallation refused");
                return;
            };
            let Phase::Confirm(going) = &operation.phase else {
                tracing::warn!(phase = ?operation.phase, "a confirmation out of its time refused");
                return;
            };
            let going = going.clone();
            if let Target::Snap { purge: asked, .. } = &mut operation.target {
                *asked = purge;
            }
            let (said, receiver) = mpsc::channel();
            operation.receiver = receiver;
            operation.phase = Phase::Running(Run::default());
            let target = operation.target.clone();
            tracing::info!(?target, packages = going.len(), "uninstallation confirmed");
            let entry = UninstallEntry {
                time: now.as_second(),
                source: match target.source() {
                    Source::Deb => PackageSource::Deb,
                    Source::Snap => PackageSource::Snap,
                    Source::Flatpak => PackageSource::Flatpak,
                },
                name: operation.name.clone(),
                // What went is known once removed: `went` writes it.
                packages: Vec::new(),
                freed_bytes: operation.freed_now(),
                data_removed: operation.purges(),
                part: UninstallPart::App,
                later_bytes: None,
            };
            let cancel = Arc::clone(&operation.cancel);
            std::thread::Builder::new()
                .name("okapi-uninstall".to_owned())
                .spawn(move || {
                    remove(
                        &*uninstaller,
                        &target,
                        &going,
                        &said,
                        &cancel,
                        &record,
                        &entry,
                    );
                })
        };
        if let Err(error) = started {
            tracing::error!(%error, "no thread to uninstall");
            if let Some(operation) = self.0.borrow_mut().operation.as_mut() {
                operation.phase = Phase::Failed(Why::Unanswered);
            }
        }
        self.show_operation();
        self.show_lines(false);
    }

    fn cancel(&self) {
        if let Some(operation) = &self.0.borrow().operation {
            tracing::info!(key = ?operation.key, "uninstallation cancel asked");
            operation.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Closes the window of the uninstallation: one running goes on.
    fn close_modal(&self) {
        {
            let mut state = self.0.borrow_mut();
            match state
                .operation
                .as_mut()
                .map(|operation| &mut operation.phase)
            {
                Some(Phase::Running(run)) => run.window = Window::Hidden,
                Some(_) => state.operation = None,
                None => {}
            }
        }
        self.show_operation();
        self.show_lines(false);
    }

    /// Tries a failed uninstallation again, from its check.
    fn retry(&self) {
        let operation = {
            let mut state = self.0.borrow_mut();
            if !matches!(
                state.operation.as_ref().map(|operation| &operation.phase),
                Some(Phase::Failed(_))
            ) {
                tracing::warn!("a retry with nothing failed refused");
                return;
            }
            state.operation.take()
        };
        if let Some(operation) = operation {
            let target = match operation.target {
                Target::Snap { name, .. } => Target::Snap { name, purge: false },
                other @ (Target::Deb { .. } | Target::Flatpak(_)) => other,
            };
            self.with_window(|window| window.global::<ui::Apps>().set_purge(false));
            self.check(
                operation.key,
                operation.name,
                target,
                operation.estimated,
                operation.data_bytes,
            );
        }
    }

    /// The toast held stays; let go, it goes after a while.
    fn hold_toast(&self, held: bool) {
        self.0.borrow_mut().toast_held = held;
        let state = self.0.borrow();
        if held {
            state.toast_timer.stop();
            return;
        }
        let weak = Rc::downgrade(&self.0);
        state
            .toast_timer
            .start(slint::TimerMode::SingleShot, TOAST_SHOWN_FOR, move || {
                if let Some(state) = weak.upgrade() {
                    Self(state).close_toast();
                }
            });
    }

    fn close_toast(&self) {
        {
            let mut state = self.0.borrow_mut();
            state.toast_timer.stop();
            state.toast_held = false;
        }
        self.with_window(|window| window.global::<ui::Apps>().set_toast(false));
    }

    /// What the uninstallation's threads said.
    fn take_said(&self) {
        loop {
            let next = {
                let state = self.0.borrow();
                let Some(operation) = &state.operation else {
                    return;
                };
                operation.receiver.try_recv()
            };
            match next {
                Ok(said) => self.hear(said),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    let silent = {
                        let mut state = self.0.borrow_mut();
                        let Some(operation) = state.operation.as_mut() else {
                            return;
                        };
                        let silent = matches!(operation.phase, Phase::Checking | Phase::Running(_));
                        if silent {
                            tracing::error!(key = ?operation.key, "the uninstallation's thread ended without a word");
                            operation.phase = Phase::Failed(Why::Unanswered);
                        }
                        silent
                    };
                    self.show_operation();
                    if silent {
                        self.read_again();
                    }
                    return;
                }
            }
        }
    }

    fn hear(&self, said: Said) {
        let mut ending = None;
        let mut changed = false;
        {
            let mut state = self.0.borrow_mut();
            let Some(operation) = state.operation.as_mut() else {
                return;
            };
            match said {
                Said::Checked(checked) => {
                    changed = operation.running();
                    if changed {
                        tracing::warn!(key = ?operation.key, "what would go changed: asked again");
                    }
                    operation.phase = match checked {
                        Ok(Checked::Going(going)) => {
                            // A snap's size is the line's, old versions with it.
                            if !matches!(operation.target, Target::Snap { .. }) {
                                let sizes: Option<Vec<u64>> =
                                    going.iter().map(|package| package.bytes).collect();
                                operation.freed = match sizes {
                                    Some(sizes) => {
                                        Some(sizes.into_iter().fold(0, u64::saturating_add))
                                    }
                                    // The application alone: the row's estimate
                                    // of it stays.
                                    None if going.len() == 1 => operation.estimated,
                                    // Another package unknown: the sum would
                                    // say less than goes.
                                    None => None,
                                };
                            }
                            Phase::Confirm(going)
                        }
                        Ok(Checked::Refused(names)) => Phase::Refused(names),
                        Err(why) => {
                            tracing::warn!(?why, "uninstallation not checked");
                            Phase::Failed(why)
                        }
                    };
                }
                Said::Step(step) => {
                    if let Phase::Running(run) = &mut operation.phase {
                        match step {
                            Step::Cancellable(true) => run.cancelling = Cancelling::Possible,
                            Step::Cancellable(false) if run.cancelling == Cancelling::Possible => {
                                run.cancelling = Cancelling::TooLate;
                            }
                            Step::Cancellable(false) => {}
                            Step::Waiting(waiting) => run.waiting = waiting,
                        }
                    }
                }
                Said::Ended { ended, recorded } => {
                    tracing::info!(key = ?operation.key, ?ended, recorded, "uninstallation ended");
                    if let Ended::Failed(why) = &ended {
                        operation.phase = Phase::Failed(why.clone());
                    }
                    ending = Some((ended, recorded));
                }
            }
        }
        match &ending {
            Some((Ended::Done(gone), recorded)) => {
                let operation = self.0.borrow_mut().operation.take();
                if let Some(operation) = operation {
                    let freed = operation.freed_now().filter(|_| !gone.space_unknown);
                    self.show_toast(&operation.name, freed, !gone.left.is_empty(), *recorded);
                }
            }
            Some((Ended::Cancelled, _)) => self.0.borrow_mut().operation = None,
            Some((Ended::Failed(_), _)) | None => {}
        }
        self.show_operation();
        let gone_or_unknown = matches!(
            self.0
                .borrow()
                .operation
                .as_ref()
                .map(|operation| &operation.phase),
            Some(Phase::Failed(Why::NotFound | Why::Unanswered))
        );
        if ending.is_some() || gone_or_unknown || changed {
            // The system may have changed.
            self.read_again();
        } else {
            self.show_lines(false);
        }
    }

    /// Reads the list again now on the page, on return otherwise.
    fn read_again(&self) {
        let on_page = {
            let mut state = self.0.borrow_mut();
            state.read_at = None;
            state.on_page
        };
        if on_page {
            self.refresh();
        } else {
            self.show_lines(false);
        }
    }

    fn show_toast(&self, name: &str, freed: Option<u64>, partly: bool, recorded: bool) {
        self.with_window(|window| {
            let apps = window.global::<ui::Apps>();
            apps.set_toast_name(name.into());
            apps.set_toast_partly(partly);
            apps.set_toast_freed_known(freed.is_some());
            apps.set_toast_freed(size(freed.unwrap_or(0)));
            apps.set_toast_recorded(recorded);
            apps.set_toast(true);
        });
        // Arrived away from the page, it waits to be seen there: the time of
        // an earlier one never takes it away unseen.
        let (held, on_page) = {
            let state = self.0.borrow();
            (state.toast_held, state.on_page)
        };
        if on_page {
            self.hold_toast(held);
        } else {
            self.0.borrow().toast_timer.stop();
        }
    }

    /// Shows the uninstallation as it stands.
    fn show_operation(&self) {
        // An uninstallation holds the old versions' removal back.
        self.show_old_versions();
        let window = self.0.borrow().window.upgrade();
        let Some(window) = window else { return };
        let apps = window.global::<ui::Apps>();
        let state = self.0.borrow();
        let Some(operation) = state
            .operation
            .as_ref()
            .filter(|operation| operation.shown())
        else {
            apps.set_step(ui::UninstallStep::Closed);
            return;
        };
        apps.set_target(operation.name.as_str().into());
        apps.set_target_source(rows::source(operation.target.source()));
        apps.set_freed_known(operation.freed.is_some());
        apps.set_freed(size(operation.freed.unwrap_or(0)));
        let going: Vec<SharedString> = match &operation.phase {
            Phase::Confirm(going) => going
                .iter()
                .map(|package| package.name.as_str().into())
                .collect(),
            Phase::Refused(names) => names.iter().map(|name| name.as_str().into()).collect(),
            Phase::Checking | Phase::Running(_) | Phase::Failed(_) => Vec::new(),
        };
        crate::shown::show(&state.going_model, going);
        let step = match &operation.phase {
            Phase::Checking => ui::UninstallStep::Checking,
            Phase::Confirm(_) => ui::UninstallStep::Confirm,
            Phase::Refused(_) => ui::UninstallStep::Refused,
            Phase::Running(run) => {
                apps.set_cancellable(run.cancelling == Cancelling::Possible);
                apps.set_too_late(run.cancelling == Cancelling::TooLate);
                apps.set_waiting(run.waiting);
                apps.set_closing(run.window == Window::CloseAsked);
                ui::UninstallStep::Running
            }
            Phase::Failed(why) => {
                let (shown_why, detail) = why_shown(why);
                apps.set_why(shown_why);
                apps.set_why_detail(detail.into());
                ui::UninstallStep::Failed
            }
        };
        apps.set_step(step);
    }

    /// Whether the window may close now: during an uninstallation it is
    /// held once, the uninstallation shown; asked again, it closes, the
    /// uninstallation left to the service.
    pub fn close_requested(&self) -> slint::CloseRequestResponse {
        let insisted = {
            let mut state = self.0.borrow_mut();
            let Some(Phase::Running(run)) = state
                .operation
                .as_mut()
                .map(|operation| &mut operation.phase)
            else {
                return slint::CloseRequestResponse::HideWindow;
            };
            let insisted = run.window == Window::CloseAsked;
            run.window = Window::CloseAsked;
            insisted
        };
        if insisted {
            tracing::warn!(
                "closing insisted during an uninstallation: left to the service, its history line lost"
            );
            return slint::CloseRequestResponse::HideWindow;
        }
        tracing::info!("closing asked during an uninstallation: held once");
        self.with_window(|window| window.set_page(ui::Page::Apps));
        self.show_operation();
        slint::CloseRequestResponse::KeepWindowShown
    }

    /// The window closes: measuring stops; an uninstallation running is
    /// left to its service, never waited for.
    pub fn shut_down(&self) {
        self.wait_for_leftovers();
        self.leave_old_versions();
        let state = self.0.borrow();
        if let Some(reading) = &state.reading {
            reading.stop.store(true, Ordering::Relaxed);
        }
        if let Some(operation) = state
            .operation
            .as_ref()
            .filter(|operation| operation.running())
        {
            let source = operation.target.source();
            if source == Source::Flatpak {
                tracing::warn!(key = ?operation.key, "flatpak's uninstallation left at closing: its extensions and clean-up may not be done");
            } else {
                tracing::warn!(key = ?operation.key, ?source, "uninstallation left running at closing");
            }
        }
    }
}

/// The uninstallation's thread: removed as shown (`remove_as_shown`), the
/// history line written if it went.
fn remove(
    uninstaller: &dyn Uninstaller,
    target: &Target,
    going: &[Going],
    said: &Sender<Said>,
    cancel: &AtomicBool,
    record: &Record,
    entry: &UninstallEntry,
) {
    let (steps, heard) = mpsc::channel();
    let forward = said.clone();
    let relay = std::thread::Builder::new()
        .name("okapi-uninstall-steps".to_owned())
        .spawn(move || {
            for step in heard {
                let _ = forward.send(Said::Step(step));
            }
        });
    if let Err(error) = &relay {
        tracing::warn!(%error, "no thread to relay the steps: they go unsaid");
    }
    let removal = remove_as_shown(uninstaller, target, going, &steps, cancel);
    drop(steps);
    if let Ok(relay) = relay
        && relay.join().is_err()
    {
        tracing::error!("the steps' relay panicked");
    }
    let said_now = match removal {
        Removal::Changed(checked) => Said::Checked(checked),
        Removal::Ended(ended) => {
            let recorded = match &ended {
                Ended::Done(gone) => record(&went(entry, going, gone)).is_ok(),
                Ended::Cancelled | Ended::Failed(_) => false,
            };
            Said::Ended { ended, recorded }
        }
    };
    let _ = said.send(said_now);
}

/// The history line of what went: `entry`, with what of `going` did not
/// stay, by name; its space unknown when it is not known free.
fn went(entry: &UninstallEntry, going: &[Going], gone: &Gone) -> UninstallEntry {
    UninstallEntry {
        packages: going
            .iter()
            .filter(|package| !gone.left.contains(&package.id))
            .map(|package| package.name.clone())
            .collect(),
        freed_bytes: entry.freed_bytes.filter(|_| !gone.space_unknown),
        ..entry.clone()
    }
}

fn why_shown(why: &Why) -> (ui::UninstallWhy, String) {
    match why {
        Why::Absent => (ui::UninstallWhy::Absent, String::new()),
        Why::NotAuthorized => (ui::UninstallWhy::NotAuthorized, String::new()),
        Why::Busy => (ui::UninstallWhy::Busy, String::new()),
        Why::Unanswered => (ui::UninstallWhy::Unanswered, String::new()),
        Why::TimedOut => (ui::UninstallWhy::TimedOut, String::new()),
        Why::NotFound => (ui::UninstallWhy::NotFound, String::new()),
        Why::Unclear => (ui::UninstallWhy::Unclear, String::new()),
        Why::Service(words) => (ui::UninstallWhy::Service, words.clone()),
    }
}

/// An icon's pixels, as the window draws them.
fn image(pixels: &IconPixels) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &pixels.rgba,
        pixels.width,
        pixels.height,
    );
    if pixels.premultiplied {
        slint::Image::from_rgba8_premultiplied(buffer)
    } else {
        slint::Image::from_rgba8(buffer)
    }
}

fn detail(entry: &Entry, icon: Option<&slint::Image>, uninstalling: bool) -> ui::AppDetail {
    let app = &entry.app;
    let app_bytes = entry.app_bytes();
    ui::AppDetail {
        name: app.name.as_str().into(),
        source: rows::source(app.source),
        icon: icon.cloned().unwrap_or_default(),
        has_icon: icon.is_some(),
        version: app.version.as_str().into(),
        publisher: app.publisher.as_deref().unwrap_or_default().into(),
        origin: match app.origin {
            None => ui::AppOrigin::None,
            Some(okapi_tools::apps::Origin::ByUser) => ui::AppOrigin::ByUser,
            Some(okapi_tools::apps::Origin::AsDependency) => ui::AppOrigin::AsDependency,
            Some(okapi_tools::apps::Origin::WithSystem) => ui::AppOrigin::WithSystem,
        },
        package: app.package.as_str().into(),
        app_known: app_bytes.is_some(),
        app_size: size(app_bytes.unwrap_or(0)),
        data: entry.data(),
        data_size: size(entry.data_bytes()),
        has_old: app.old_bytes > 0,
        old_size: size(app.old_bytes),
        old_versions: shown(app.old_versions),
        total: size(entry.total()),
        system: app.system,
        uninstallable: !app.system && app.target.is_some() && !uninstalling,
        uninstalling,
        for_account_only: app.source == Source::Flatpak && app.target.is_none(),
    }
}

#[cfg(test)]
pub(crate) mod tests;
