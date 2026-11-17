//! The updates screen: what waits for an update (`okapi_tools::updates`).
//! A search reads the system's packages on one thread, the snaps and the
//! restart awaited on another, each told to the window as soon as it is
//! read; the window reads them every half second. It runs when the page is
//! shown and the last one is old, or when the user asks; never while an
//! installation ([`install`]) runs, which reads the list again once it
//! ends.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use okapi_rules::Language;
use okapi_store::{HistoryUnavailable, UpdateEntry};
use okapi_tools::packages::reboot::RebootRequired;
use okapi_tools::updates::install::Installer;
use okapi_tools::updates::{
    self, Failure, Kind, ListsRefresh, Outcome, SnapUpdate, Snaps, Sources, SystemReading,
    SystemSources, SystemUpdate,
};
use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use crate::dates::MINUTES_EVERY;
use crate::disk_usage::READ_EVERY;
use crate::history::Record;
use crate::shown::{show, shown, size};
use crate::ui;

/// Showing the page again reads again once the last search is this old.
const READ_AGAIN_AFTER: Duration = Duration::from_mins(5);
/// Package lists downloaded longer ago than this are said to be old.
const LISTS_OLD_AFTER: Duration = Duration::from_hours(7 * 24);
/// How many packages the unfolded list shows before « Afficher les N
/// autres ».
const SYSTEM_LINES_FIRST: usize = 6;

mod install;
pub(crate) mod record;

pub(crate) use install::why_shown;
use install::{Install, Target};

/// What the screen reads and installs with, where it records what it
/// installed, and its clock.
#[derive(Clone)]
pub struct UpdatesSeams {
    pub sources: Arc<dyn Sources>,
    pub installer: Arc<dyn Installer>,
    pub record: Record<UpdateEntry>,
    /// The distribution's name, said beside « Système ».
    pub distribution: Option<String>,
    pub now: fn() -> Timestamp,
    pub zone: TimeZone,
}

impl UpdatesSeams {
    /// This system's daemons and files, texts in the session's language.
    pub fn system() -> Self {
        let language = Language::of_session(|name| std::env::var_os(name));
        let system = Arc::new(SystemSources::new(language));
        Self {
            sources: Arc::clone(&system) as Arc<dyn Sources>,
            installer: system,
            record: Arc::new(record_system),
            distribution: okapi_platform::distribution()
                .inspect_err(|error| tracing::warn!(%error, "distribution unknown"))
                .ok()
                .flatten(),
            now: Timestamp::now,
            zone: crate::dashboard::system_zone(),
        }
    }
}

/// Writes `entry` in the user's history (its write traces its own failure).
fn record_system(entry: &UpdateEntry) -> Result<(), HistoryUnavailable> {
    crate::history::open_to_record("update")?.record_update(entry)?;
    Ok(())
}

/// What a search's threads tell.
#[derive(Debug)]
enum Event {
    /// The system's reading waits for another program, or no longer.
    Waiting(bool),
    System(SystemReading),
    Snaps(Outcome<Snaps>),
    Reboot(Option<RebootRequired>),
}

/// A search on its way: what it tells, and what is still awaited.
struct Search {
    events: Receiver<Event>,
    awaiting_system: bool,
    awaiting_snaps: bool,
    awaiting_reboot: bool,
}

impl Search {
    const fn done(&self) -> bool {
        !(self.awaiting_system || self.awaiting_snaps || self.awaiting_reboot)
    }
}

struct State {
    window: slint::Weak<ui::AppWindow>,
    seams: UpdatesSeams,
    search: Option<Search>,
    /// Whether the last search downloaded the lists: « Réessayer » asks the
    /// same.
    last_refresh: bool,
    read_at: Option<Timestamp>,
    /// snapd's next pass, said again as the clock goes, until it is past.
    next_refresh: Option<Timestamp>,
    /// When the lists were downloaded, by apt's stamps or by Okapi itself
    /// (a download of its own that left no stamp is remembered).
    listed_at: Option<Timestamp>,
    /// The page is shown: the moments are said again as the clock goes.
    on_page: bool,
    system: Vec<SystemUpdate>,
    /// How the last reading of the system went: read, what an installation
    /// left is known from it.
    system_reading: ui::SourceState,
    system_all: bool,
    system_model: Rc<VecModel<ui::SystemLine>>,
    /// The snaps shown, in their lines' order (what a line's button names);
    /// none when not read.
    snaps: Option<Vec<SnapUpdate>>,
    snap_model: Rc<VecModel<ui::SnapLine>>,
    reboot_model: Rc<VecModel<SharedString>>,
    removed_model: Rc<VecModel<ui::RemovedLine>>,
    install: Install,
    /// What the last installation asked: « Réessayer » asks it again.
    retry: Option<Target>,
    /// The line of the last installation being written: whether it was.
    recording: Option<Receiver<bool>>,
    timer: slint::Timer,
    minutes_timer: slint::Timer,
    install_timer: slint::Timer,
    record_timer: slint::Timer,
}

/// The updates screen.
#[derive(Clone)]
pub struct UpdatesScreen(Rc<RefCell<State>>);

impl std::fmt::Debug for UpdatesScreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpdatesScreen")
            .finish_non_exhaustive()
    }
}

impl UpdatesScreen {
    pub fn attach(window: &ui::AppWindow, seams: UpdatesSeams) -> Self {
        let state = State {
            window: window.as_weak(),
            search: None,
            last_refresh: false,
            read_at: None,
            next_refresh: None,
            listed_at: None,
            on_page: false,
            system: Vec::new(),
            system_reading: ui::SourceState::Absent,
            system_all: false,
            system_model: Rc::new(VecModel::default()),
            snaps: None,
            snap_model: Rc::new(VecModel::default()),
            reboot_model: Rc::new(VecModel::default()),
            removed_model: Rc::new(VecModel::default()),
            install: Install::Idle,
            retry: None,
            recording: None,
            timer: slint::Timer::default(),
            minutes_timer: slint::Timer::default(),
            install_timer: slint::Timer::default(),
            record_timer: slint::Timer::default(),
            seams,
        };
        let updates = window.global::<ui::Updates>();
        updates.set_system_lines(ModelRc::from(Rc::clone(&state.system_model)));
        updates.set_snap_lines(ModelRc::from(Rc::clone(&state.snap_model)));
        updates.set_reboot_packages(ModelRc::from(Rc::clone(&state.reboot_model)));
        updates.set_removed(ModelRc::from(Rc::clone(&state.removed_model)));
        updates.set_distribution(state.seams.distribution.clone().unwrap_or_default().into());
        let screen = Self(Rc::new(RefCell::new(state)));
        let weak = Rc::downgrade(&screen.0);
        let on = move |action: fn(&Self)| {
            let weak = weak.clone();
            move || {
                if let Some(state) = weak.upgrade() {
                    action(&Self(state));
                }
            }
        };
        updates.on_search_now(on(|screen| screen.search(true)));
        updates.on_retry(on(|screen| {
            let refresh = screen.0.borrow().last_refresh;
            screen.search(refresh);
        }));
        updates.on_show_all_system(on(Self::show_all_system));
        updates.on_update_system(on(Self::update_system));
        updates.on_update_snaps(on(Self::update_snaps));
        updates.on_confirm_update(on(Self::confirm_update));
        updates.on_cancel_update(on(Self::cancel_update));
        updates.on_close_result(on(Self::close_result));
        updates.on_retry_update(on(Self::retry_update));
        let weak = Rc::downgrade(&screen.0);
        updates.on_update_snap(move |index| {
            if let Some(state) = weak.upgrade() {
                Self(state).update_snap(index);
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

    /// Follows the page shown: on it, a search when none was made or the
    /// last is old.
    pub fn page_shown(&self, page: ui::Page) {
        let on_page = page == ui::Page::Updates;
        self.0.borrow_mut().on_page = on_page;
        if !on_page {
            self.0.borrow().minutes_timer.stop();
            return;
        }
        let due = {
            let state = self.0.borrow();
            state.search.is_none()
                && !state.install.active()
                && state.read_at.is_none_or(|read_at| {
                    older_than(read_at, (state.seams.now)(), READ_AGAIN_AFTER)
                })
        };
        if due {
            self.search(false);
        } else {
            self.show_moments();
            self.watch_minutes();
        }
    }

    /// Starts a search, unless one is on its way or an installation runs
    /// (the one it ends with reads the list again); `refresh` downloads the
    /// package lists first.
    fn search(&self, refresh: bool) {
        {
            let state = self.0.borrow();
            if state.search.is_some() {
                return;
            }
            if state.install.blocks_search() {
                tracing::info!("a search asked while an installation runs: refused");
                return;
            }
        }
        // The threads' traces belong to this search.
        let search = tracing::info_span!("updates", refresh);
        search.in_scope(|| tracing::info!("updates searched"));
        let (sender, events) = mpsc::channel();
        let sources = Arc::clone(&self.0.borrow().seams.sources);
        let system = spawn(SYSTEM_THREAD, {
            let sources = Arc::clone(&sources);
            let sender = sender.clone();
            let search = search.clone();
            move || {
                let _search = search.entered();
                let mut teller = Teller::new(sender, SYSTEM_THREAD, &[Owed::System]);
                let waiting = |waiting| told(&teller.sender, Event::Waiting(waiting));
                let reading = updates::read_system(&*sources, refresh, &waiting);
                teller.tell(Event::System(reading));
            }
        });
        let snaps = spawn(SNAPS_THREAD, {
            let sender = sender.clone();
            move || {
                let _search = search.entered();
                let mut teller = Teller::new(sender, SNAPS_THREAD, &[Owed::Snaps, Owed::Reboot]);
                teller.tell(Event::Snaps(updates::read_snaps(&*sources)));
                teller.tell(Event::Reboot(updates::read_reboot(&*sources)));
            }
        });
        drop(sender);
        {
            let mut state = self.0.borrow_mut();
            state.last_refresh = refresh;
            state.system_all = false;
            state.search = Some(Search {
                events,
                awaiting_system: system,
                awaiting_snaps: snaps,
                awaiting_reboot: snaps,
            });
        }
        let previous = self.0.borrow().read_at.is_some();
        self.with_window(|window| {
            let updates = window.global::<ui::Updates>();
            updates.set_step(ui::UpdatesStep::Searching);
            updates.set_refreshing(refresh);
            updates.set_waiting(false);
            updates.set_previous_shown(previous);
        });
        if !system {
            self.show_system(failed_reading());
        }
        if !snaps {
            self.show_snaps(Outcome::Failed(Failure::Broken));
            self.show_reboot(None);
        }
        self.watch();
        self.take();
    }

    fn watch(&self) {
        let weak = Rc::downgrade(&self.0);
        self.0
            .borrow()
            .timer
            .start(slint::TimerMode::Repeated, READ_EVERY, move || {
                if let Some(state) = weak.upgrade() {
                    Self(state).take();
                }
            });
    }

    /// Shows what the search's threads have told, and ends it once they
    /// have all told. A thread that dies tells its failure itself
    /// ([`Teller`]); all of them gone with something still awaited failed
    /// it too.
    fn take(&self) {
        loop {
            let event = {
                let state = self.0.borrow();
                let Some(search) = &state.search else {
                    return;
                };
                if search.done() {
                    None
                } else {
                    Some(search.events.try_recv())
                }
            };
            match event {
                None => break,
                Some(Ok(event)) => self.apply(event),
                Some(Err(TryRecvError::Empty)) => return,
                Some(Err(TryRecvError::Disconnected)) => {
                    let (system, snaps, reboot) = {
                        let state = self.0.borrow();
                        let search = state.search.as_ref();
                        (
                            search.is_some_and(|search| search.awaiting_system),
                            search.is_some_and(|search| search.awaiting_snaps),
                            search.is_some_and(|search| search.awaiting_reboot),
                        )
                    };
                    tracing::error!(
                        system,
                        snaps,
                        reboot,
                        "the updates threads gone with a reading still awaited"
                    );
                    if system {
                        self.show_system(failed_reading());
                    }
                    if snaps {
                        self.show_snaps(Outcome::Failed(Failure::Broken));
                    }
                    if reboot {
                        self.show_reboot(None);
                    }
                    // Nothing more can come.
                    break;
                }
            }
        }
        self.end_search();
    }

    fn apply(&self, event: Event) {
        match event {
            Event::Waiting(waiting) => self.with_window(|window| {
                window.global::<ui::Updates>().set_waiting(waiting);
            }),
            Event::System(reading) => self.show_system(reading),
            Event::Snaps(outcome) => self.show_snaps(outcome),
            Event::Reboot(reboot) => self.show_reboot(reboot.as_ref()),
        }
    }

    fn end_search(&self) {
        let now = {
            let mut state = self.0.borrow_mut();
            state.search = None;
            state.timer.stop();
            let now = (state.seams.now)();
            state.read_at = Some(now);
            now
        };
        let said = self.moment(now).is_some();
        self.with_window(|window| {
            let updates = window.global::<ui::Updates>();
            updates.set_step(ui::UpdatesStep::Read);
            updates.set_waiting(false);
            // A list is shown with its moment, or not at all.
            updates.set_has_read(said);
        });
        self.show_moments();
        if self.0.borrow().on_page {
            self.watch_minutes();
        }
        self.tell_outcome();
    }

    fn moment(&self, at: Timestamp) -> Option<ui::Moment> {
        let state = self.0.borrow();
        crate::dashboard::moment(at.as_second(), (state.seams.now)(), &state.seams.zone)
    }

    fn show_system(&self, reading: SystemReading) {
        let now = (self.0.borrow().seams.now)();
        let listed_at = {
            let mut state = self.0.borrow_mut();
            let downloaded_now = (reading.refresh == ListsRefresh::Done).then_some(now);
            state.listed_at = state.listed_at.max(reading.listed_at).max(downloaded_now);
            state.listed_at
        };
        // Read from lists dated long ago or not at all. Without lists read
        // (no PackageKit), nothing is old.
        let lists_old = matches!(reading.updates, Outcome::Read(_))
            && listed_at.is_none_or(|at| older_than(at, now, LISTS_OLD_AFTER));
        let (state_shown, failure, updates) = match reading.updates {
            Outcome::Absent => (ui::SourceState::Absent, None, Vec::new()),
            Outcome::Read(updates) => (ui::SourceState::Read, None, updates),
            Outcome::Failed(failure) => (ui::SourceState::Failed, Some(failure), Vec::new()),
        };
        let security = updates
            .iter()
            .filter(|update| update.kind == Kind::Security)
            .count();
        // Held back, it will not be installed: shown, never waiting.
        let held = updates
            .iter()
            .filter(|update| update.kind == Kind::Blocked)
            .count();
        let download = reading
            .download_size
            .filter(|_| state_shown == ui::SourceState::Read);
        {
            let mut state = self.0.borrow_mut();
            state.system = updates;
            state.system_reading = state_shown;
            if let Some(search) = &mut state.search {
                search.awaiting_system = false;
            }
        }
        let waiting = self.0.borrow().system.len() - held;
        self.show_system_lines();
        self.with_window(|window| {
            let shown_updates = window.global::<ui::Updates>();
            // Its reading is over, whatever it waited for; the list shown is
            // no longer the last one.
            shown_updates.set_waiting(false);
            shown_updates.set_previous_shown(false);
            shown_updates.set_system_state(state_shown);
            if let Some(failure) = failure {
                shown_updates.set_system_failure(failure_shown(failure));
            }
            shown_updates.set_system_count(shown(waiting));
            shown_updates.set_system_held(shown(held));
            shown_updates.set_system_security(shown(security));
            shown_updates.set_has_system_download(download.is_some());
            shown_updates.set_system_download(size(download.unwrap_or_default()));
            match reading.refresh {
                ListsRefresh::Failed(failure) => {
                    shown_updates.set_refresh_failed(true);
                    shown_updates.set_refresh_failure(failure_shown(failure));
                }
                ListsRefresh::NotAsked | ListsRefresh::Done => {
                    shown_updates.set_refresh_failed(false);
                }
            }
            shown_updates.set_lists_old(lists_old);
        });
        self.show_moments();
    }

    fn show_all_system(&self) {
        self.0.borrow_mut().system_all = true;
        self.show_system_lines();
    }

    /// The first packages, or all once asked.
    fn show_system_lines(&self) {
        let state = self.0.borrow();
        let shown_count = if state.system_all {
            state.system.len()
        } else {
            state.system.len().min(SYSTEM_LINES_FIRST)
        };
        let lines = state.system[..shown_count]
            .iter()
            .map(system_line)
            .collect();
        show(&state.system_model, lines);
        let more = shown(state.system.len() - shown_count);
        drop(state);
        self.with_window(|window| window.global::<ui::Updates>().set_system_more(more));
    }

    fn show_snaps(&self, outcome: Outcome<Snaps>) {
        let (state_shown, failure, snaps) = match outcome {
            Outcome::Absent => (ui::SourceState::Absent, None, None),
            Outcome::Read(snaps) => (ui::SourceState::Read, None, Some(snaps)),
            Outcome::Failed(failure) => (ui::SourceState::Failed, Some(failure), None),
        };
        let lines = snaps
            .as_ref()
            .map(|snaps| snaps.updates.iter().map(snap_line).collect())
            .unwrap_or_default();
        // A total only when every size is known: a part would pass for all.
        let total: Option<u64> = snaps.as_ref().and_then(|snaps| {
            snaps
                .updates
                .iter()
                .map(|update| update.download_size)
                .sum::<Option<u64>>()
                .filter(|_| !snaps.updates.is_empty())
        });
        {
            let mut state = self.0.borrow_mut();
            state.next_refresh = snaps.as_ref().and_then(|snaps| snaps.next_refresh);
            state.snaps = snaps.as_ref().map(|snaps| snaps.updates.clone());
            show(&state.snap_model, lines);
            if let Some(search) = &mut state.search {
                search.awaiting_snaps = false;
            }
        }
        self.with_window(|window| {
            let updates = window.global::<ui::Updates>();
            updates.set_snaps_state(state_shown);
            if let Some(failure) = failure {
                updates.set_snaps_failure(failure_shown(failure));
            }
            updates.set_has_snaps_size(total.is_some());
            updates.set_snaps_size(size(total.unwrap_or_default()));
            updates.set_previous_shown(false);
        });
        self.show_moments();
    }

    fn show_reboot(&self, reboot: Option<&RebootRequired>) {
        let packages: Vec<SharedString> = reboot
            .and_then(|reboot| reboot.packages.as_ref())
            .map(|packages| packages.iter().map(SharedString::from).collect())
            .unwrap_or_default();
        {
            let mut state = self.0.borrow_mut();
            show(&state.reboot_model, packages);
            if let Some(search) = &mut state.search {
                search.awaiting_reboot = false;
            }
        }
        self.with_window(|window| {
            window.global::<ui::Updates>().set_reboot(reboot.is_some());
        });
    }

    fn watch_minutes(&self) {
        let weak = Rc::downgrade(&self.0);
        self.0
            .borrow()
            .minutes_timer
            .start(slint::TimerMode::Repeated, MINUTES_EVERY, move || {
                if let Some(state) = weak.upgrade() {
                    Self(state).show_moments();
                }
            });
    }

    /// Says the moments again as the clock goes: since the last reading, the
    /// lists' date (« aujourd'hui » turns into « hier »), and snapd's next
    /// pass while it is still to come.
    fn show_moments(&self) {
        let (read_at, listed_at, next_refresh, now) = {
            let state = self.0.borrow();
            (
                state.read_at,
                state.listed_at,
                state.next_refresh,
                (state.seams.now)(),
            )
        };
        let listed_at = listed_at.and_then(|at| self.moment(at));
        let read = read_at.and_then(|at| {
            Some((
                shown(crate::dates::minutes_since(at, now)),
                self.moment(at)?,
            ))
        });
        let next = next_refresh
            .filter(|next| *next > now)
            .and_then(|next| self.moment(next));
        self.with_window(|window| {
            let updates = window.global::<ui::Updates>();
            if let Some((minutes, read_at)) = read {
                updates.set_minutes(minutes);
                updates.set_read_at(read_at);
            }
            updates.set_has_listed_at(listed_at.is_some());
            if let Some(listed_at) = listed_at {
                updates.set_listed_at(listed_at);
            }
            updates.set_has_next_refresh(next.is_some());
            if let Some(next) = next {
                updates.set_next_refresh(next);
            }
        });
    }
}

/// Whether `at` is at least `age` before `now`.
fn older_than(at: Timestamp, now: Timestamp, age: Duration) -> bool {
    now.as_second().saturating_sub(at.as_second())
        >= i64::try_from(age.as_secs()).unwrap_or(i64::MAX)
}

/// The threads of a search.
const SYSTEM_THREAD: &str = "updates-system";
const SNAPS_THREAD: &str = "updates-snaps";

/// What a thread of a search owes the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owed {
    System,
    Snaps,
    Reboot,
}

impl Owed {
    /// What `event` pays, if anything.
    const fn paid_by(event: &Event) -> Option<Self> {
        match event {
            Event::Waiting(_) => None,
            Event::System(_) => Some(Self::System),
            Event::Snaps(_) => Some(Self::Snaps),
            Event::Reboot(_) => Some(Self::Reboot),
        }
    }

    /// What is shown of it when it never came.
    const fn failed(self) -> Event {
        match self {
            Self::System => Event::System(failed_reading()),
            Self::Snaps => Event::Snaps(Outcome::Failed(Failure::Broken)),
            Self::Reboot => Event::Reboot(None),
        }
    }
}

/// A thread's end of the search: what it tells, and, once dropped (its
/// end, or a panic unwinding), the failure of what it still owed, at once:
/// the other thread, which holds the channel open, is never waited for.
struct Teller {
    sender: mpsc::Sender<Event>,
    thread: &'static str,
    owed: Vec<Owed>,
}

impl Teller {
    fn new(sender: mpsc::Sender<Event>, thread: &'static str, owed: &[Owed]) -> Self {
        Self {
            sender,
            thread,
            owed: owed.to_vec(),
        }
    }

    fn tell(&mut self, event: Event) {
        if let Some(paid) = Owed::paid_by(&event) {
            self.owed.retain(|owed| *owed != paid);
        }
        told(&self.sender, event);
    }
}

impl Drop for Teller {
    fn drop(&mut self) {
        for owed in std::mem::take(&mut self.owed) {
            tracing::error!(
                thread = self.thread,
                ?owed,
                "an updates thread ended without its reading: its failure told"
            );
            told(&self.sender, owed.failed());
        }
    }
}

/// What a thread tells the window; a window gone (the app closing) has
/// nothing left to show.
fn told<T>(sender: &mpsc::Sender<T>, event: T) {
    if sender.send(event).is_err() {
        tracing::debug!("the updates screen is gone");
    }
}

/// Runs `read` on a thread named `name`; false, traced, when it could not
/// start.
fn spawn(name: &str, read: impl FnOnce() + Send + 'static) -> bool {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(read)
        .inspect_err(|error| tracing::error!(name, %error, "updates thread not started"))
        .is_ok()
}

/// What is shown of a system that could not be read at all.
const fn failed_reading() -> SystemReading {
    SystemReading {
        updates: Outcome::Failed(Failure::Broken),
        refresh: ListsRefresh::NotAsked,
        listed_at: None,
        download_size: None,
    }
}

const fn failure_shown(failure: Failure) -> ui::SourceFailure {
    match failure {
        Failure::Busy => ui::SourceFailure::Busy,
        Failure::Offline => ui::SourceFailure::Offline,
        Failure::Refused => ui::SourceFailure::Refused,
        Failure::Unanswered => ui::SourceFailure::Unanswered,
        Failure::Broken => ui::SourceFailure::Broken,
    }
}

fn system_line(update: &SystemUpdate) -> ui::SystemLine {
    ui::SystemLine {
        name: update.name.as_str().into(),
        summary: update.summary.as_str().into(),
        version: update.version.as_str().into(),
        spoken_version: upstream_version(&update.version).into(),
        kind: match update.kind {
            Kind::Security => ui::UpdateKind::Security,
            Kind::Other => ui::UpdateKind::Other,
            Kind::Blocked => ui::UpdateKind::Blocked,
        },
    }
}

fn snap_line(update: &okapi_tools::updates::SnapUpdate) -> ui::SnapLine {
    ui::SnapLine {
        title: update.title.as_str().into(),
        publisher: update.publisher.clone().unwrap_or_default().into(),
        verified: update.verified,
        version: update.version.as_str().into(),
        size: size(update.download_size.unwrap_or_default()),
        has_size: update.download_size.is_some(),
    }
}

/// The version as its authors name it, without Debian's epoch, revision
/// and repacking marks: `4:25.8.4-0ubuntu1` is `25.8.4`, `2.14.3+dfsg-1`
/// is `2.14.3`. A version that would come to nothing is left whole.
fn upstream_version(version: &str) -> &str {
    let without_epoch = version.split_once(':').map_or(version, |(_, rest)| rest);
    let without_revision = without_epoch
        .rsplit_once('-')
        .map_or(without_epoch, |(upstream, _)| upstream);
    let upstream = without_revision
        .split(['+', '~'])
        .next()
        .unwrap_or(without_revision);
    if upstream.is_empty() {
        version
    } else {
        upstream
    }
}

#[cfg(test)]
pub(crate) mod tests;
