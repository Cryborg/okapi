//! The duplicates screen: where to search (the personal folders, or the
//! whole home folder), the search (`okapi_tools::duplicates`) on threads of
//! its own read on a timer, its groups and the copies the rules check
//! (`choice`, `rows`), the confirmation, the move to the trash on a thread
//! of its own, its receipt. A copy goes only by its identifier in the
//! report kept (invariant 1); `okapi_tools::duplicates::trash` checks it
//! again just before, and a copy that stays (invariant 7).

mod choice;
mod rows;

use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime};

use jiff::Timestamp;
use okapi_rules::Language;
use okapi_tools::duplicates::{
    CopyId, DuplicateReport, DuplicateSearch, MIN_SIZE, Outcome, Progress, SearchError,
    SearchOptions, TrashReceipt,
};
use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use crate::dates::MINUTES_EVERY;
use crate::disk_usage::READ_EVERY;
use crate::file_manager::{Calls, OpenRequest};
use crate::shown::{DEFAULT_ROOM, grouped, name_of, part, shown, size};
use crate::ui;
use crate::user_files::{
    Asked, Folders, Known, OpenTrash, ReadExclusions, SearchSeams, exclusion_paths, resolve,
};
use choice::Quick;
use rows::{GroupChoice, Line, View};

/// The threads that read the copies of these roots.
pub type Threads = Arc<dyn Fn(&[PathBuf]) -> NonZeroUsize + Send + Sync>;

/// What the screen reads and asks of the machine: the system's, or a test's.
#[derive(Clone)]
pub struct DuplicateSeams {
    pub search: SearchSeams,
    pub threads: Threads,
}

impl std::fmt::Debug for DuplicateSeams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DuplicateSeams")
            .field("search", &self.search)
            .finish_non_exhaustive()
    }
}

impl DuplicateSeams {
    /// The session's home folder `home`, its personal folders, its trash,
    /// the portal.
    pub fn system(home: Option<PathBuf>) -> Self {
        Self {
            search: SearchSeams::system(home),
            threads: Arc::new(okapi_tools::duplicates::threads_for_roots),
        }
    }
}

/// Opens the search of `roots` on the start's thread: its exclusions
/// resolved first, nothing read when one is refused.
fn open_search(
    roots: &[PathBuf],
    exclusions: Vec<PathBuf>,
    threads: &Threads,
) -> Result<DuplicateSearch, ui::ToolRefusal> {
    let exclusions = resolve(exclusions)?;
    let threads = threads(roots);
    tracing::info!(threads = threads.get(), "duplicate search threads");
    let options = SearchOptions {
        min_size: MIN_SIZE,
        threads,
        exclusions,
    };
    DuplicateSearch::start(roots, options).map_err(|error| {
        tracing::warn!(%error, "duplicate search not opened");
        match error {
            SearchError::Thread(_) => ui::ToolRefusal::Thread,
            SearchError::NoRoot | SearchError::Root { .. } => ui::ToolRefusal::Folder,
        }
    })
}

/// Moves the copies `ids` of `search`'s report to the trash, on the move's
/// thread: its exclusions resolved first, then the report and the trash
/// opened; nothing moved when any of them fails.
fn move_to_trash(
    search: &DuplicateSearch,
    ids: &[CopyId],
    exclusions: Vec<PathBuf>,
    trash: &OpenTrash,
) -> Result<TrashReceipt, ui::ToolRefusal> {
    let exclusions = resolve(exclusions)?;
    let Some(report) = search.report() else {
        tracing::error!("no report to move copies of: nothing moved");
        return Err(ui::ToolRefusal::Trash);
    };
    let bin = trash().map_err(|error| {
        tracing::error!(%error, "trash not opened: nothing moved");
        ui::ToolRefusal::Trash
    })?;
    Ok(okapi_tools::duplicates::trash(
        report,
        ids,
        &bin,
        &exclusions,
    ))
}

/// The search shown, and what it holds.
enum Search {
    /// None, or one stopped, failed or given up.
    Idle,
    /// Its exclusions being resolved and its roots opened on a thread: the
    /// search to come, or why it did not start, traced there.
    Opening(Receiver<Result<DuplicateSearch, ui::ToolRefusal>>),
    Running(Arc<DuplicateSearch>),
    /// Complete: its report shown.
    Shown(Arc<DuplicateSearch>),
}

/// A move to the trash on its way: its receipt to come, and the search
/// whose report names its copies.
struct Moving {
    receiver: Receiver<Result<TrashReceipt, ui::ToolRefusal>>,
    search: Arc<DuplicateSearch>,
}

struct State {
    window: slint::Weak<ui::AppWindow>,
    seams: DuplicateSeams,
    exclusions: ReadExclusions,
    language: Language,
    folders: Folders,
    asked: Option<Asked>,
    search: Search,
    listed: u64,
    groups: Vec<GroupChoice>,
    /// The personal folders holding a copy, offered to the quick selection.
    quick_folders: Vec<usize>,
    lines: Vec<Line>,
    room: usize,
    ended_at: Option<Timestamp>,
    moving: Option<Moving>,
    calls: Calls,
    folder_model: Rc<VecModel<ui::PersonalShown>>,
    row_model: Rc<VecModel<ui::DuplicateRow>>,
    quick_model: Rc<VecModel<SharedString>>,
    kept_model: Rc<VecModel<ui::KeptShown>>,
    timer: slint::Timer,
    minutes_timer: slint::Timer,
}

impl State {
    /// The report shown, if a search is complete.
    fn shown(&self) -> Option<Arc<DuplicateSearch>> {
        match &self.search {
            Search::Shown(search) => Some(Arc::clone(search)),
            _ => None,
        }
    }
}

/// The duplicates screen.
#[derive(Clone)]
pub struct DuplicatesScreen(Rc<RefCell<State>>);

impl std::fmt::Debug for DuplicatesScreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DuplicatesScreen")
            .finish_non_exhaustive()
    }
}

impl DuplicatesScreen {
    pub fn attach(
        window: &ui::AppWindow,
        seams: DuplicateSeams,
        exclusions: ReadExclusions,
        language: Language,
    ) -> Self {
        let state = State {
            window: window.as_weak(),
            folders: Folders::new(&seams.search),
            seams,
            exclusions,
            language,
            asked: None,
            search: Search::Idle,
            listed: 0,
            groups: Vec::new(),
            quick_folders: Vec::new(),
            lines: Vec::new(),
            room: DEFAULT_ROOM,
            ended_at: None,
            moving: None,
            calls: Calls::default(),
            folder_model: Rc::new(VecModel::default()),
            row_model: Rc::new(VecModel::default()),
            quick_model: Rc::new(VecModel::default()),
            kept_model: Rc::new(VecModel::default()),
            timer: slint::Timer::default(),
            minutes_timer: slint::Timer::default(),
        };
        let duplicates = window.global::<ui::Duplicates>();
        duplicates.set_folders(ModelRc::from(Rc::clone(&state.folder_model)));
        duplicates.set_rows(ModelRc::from(Rc::clone(&state.row_model)));
        duplicates.set_quick_folders(ModelRc::from(Rc::clone(&state.quick_model)));
        duplicates.set_kept(ModelRc::from(Rc::clone(&state.kept_model)));
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
        duplicates.on_search(on(Self::search));
        duplicates.on_stop(on(Self::stop));
        duplicates.on_change(on(Self::change));
        duplicates.on_again(on(Self::again));
        duplicates.on_deselect_all(on(Self::deselect_all));
        duplicates.on_trash(on(Self::ask_confirmation));
        duplicates.on_cancel_confirmation(on(Self::cancel_confirmation));
        duplicates.on_confirm(on(Self::confirm));
        let weak = Rc::downgrade(&screen.0);
        let with_index = move |action: fn(&Self, usize)| {
            let weak = weak.clone();
            move |index: i32| {
                if let (Some(state), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                    action(&Self(state), index);
                }
            }
        };
        duplicates.on_toggle_folder(with_index(Self::toggle_folder));
        duplicates.on_toggle(with_index(Self::toggle));
        duplicates.on_open(with_index(Self::open));
        duplicates.on_path_room(with_index(Self::path_room));
        let weak = Rc::downgrade(&screen.0);
        duplicates.on_fold(move |index, open| {
            if let (Some(state), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                Self(state).fold(index, open);
            }
        });
        let weak = Rc::downgrade(&screen.0);
        duplicates.on_quick(move |rule, index| {
            if let Some(state) = weak.upgrade() {
                Self(state).quick(rule, index);
            }
        });
        duplicates.set_min_size(size(MIN_SIZE));
        duplicates.set_step(ui::ToolStep::Start);
        screen
    }

    fn with_window(&self, show: impl FnOnce(&ui::Duplicates<'_>)) {
        let window = self.0.borrow().window.upgrade();
        if let Some(window) = window {
            show(&window.global::<ui::Duplicates>());
        }
    }

    fn read_window<T>(&self, read: impl FnOnce(&ui::AppWindow) -> T) -> Option<T> {
        let window = self.0.borrow().window.upgrade();
        window.map(|window| read(&window))
    }

    fn step(&self) -> Option<ui::ToolStep> {
        self.read_window(|window| window.global::<ui::Duplicates>().get_step())
    }

    /// Follows the page shown: the folders are read when the start shows.
    pub fn page_shown(&self, page: ui::Page) {
        if page == ui::Page::Duplicates && self.step() == Some(ui::ToolStep::Start) {
            self.read_folders();
        }
    }

    /// Reads the personal folders on a thread of its own.
    fn read_folders(&self) {
        let waiting = self.0.borrow_mut().folders.read();
        self.show_start();
        if waiting {
            self.watch();
        }
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
    }

    /// One reading: the folders, the search opened, its progress and end,
    /// the move to the trash. Nothing left to wait for, the timer rests.
    fn read(&self) {
        self.take_known();
        self.take_opening();
        self.follow_search();
        self.take_receipt();
        let state = self.0.borrow();
        let waiting = state.folders.reading()
            || matches!(state.search, Search::Opening(_) | Search::Running(_))
            || state.moving.is_some();
        if !waiting {
            state.timer.stop();
        }
    }

    fn take_known(&self) {
        let taken = self.0.borrow_mut().folders.take();
        if taken {
            self.show_start();
        }
    }

    /// The start as it stands: the folders, which are checked.
    fn show_start(&self) {
        let start = self.0.borrow().folders.shown();
        let model = Rc::clone(&self.0.borrow().folder_model);
        crate::shown::show(&model, start.rows);
        self.with_window(|duplicates| {
            duplicates.set_folders_state(start.state);
            duplicates.set_home_path(start.home.into());
            duplicates.set_chosen_folders(shown(start.chosen));
        });
    }

    fn toggle_folder(&self, index: usize) {
        let toggled = self.0.borrow_mut().folders.toggle(index);
        if toggled {
            self.show_start();
        }
    }

    /// Searches where the start says.
    fn search(&self) {
        if !self.choosing() {
            return;
        }
        let whole_home = self
            .read_window(|window| window.global::<ui::Duplicates>().get_whole_home())
            .unwrap_or(false);
        let asked = self.0.borrow().folders.asked(whole_home);
        match asked {
            Ok(Some(asked)) => self.start(asked),
            Ok(None) => {}
            Err(refusal) => self.refuse_start(refusal),
        }
    }

    /// The exclusions of the settings read now, `~/snap` with them; why
    /// none can be had.
    fn exclusion_paths(&self, home: Option<&Path>) -> Result<Vec<PathBuf>, ui::ToolRefusal> {
        let read = Rc::clone(&self.0.borrow().exclusions);
        exclusion_paths(&read, home)
    }

    /// Starts searching on a thread of its own: resolving the exclusions and
    /// opening the roots may wait on a filesystem that stopped answering.
    /// Settings that cannot be read are said over a result, which stays, or
    /// on the start.
    fn start(&self, asked: Asked) {
        let exclusions = match self.exclusion_paths(asked.known.home.as_deref()) {
            Ok(paths) => paths,
            // Settings not read: a result stays, the refusal said over it;
            // elsewhere (a receipt, a search stopped) the start says it.
            Err(refusal) if self.step() == Some(ui::ToolStep::Result) => {
                self.with_window(|duplicates| duplicates.set_refused(refusal));
                return;
            }
            Err(refusal) => {
                self.refuse_start(refusal);
                return;
            }
        };
        self.give_up();
        let threads = Arc::clone(&self.0.borrow().seams.threads);
        let roots = asked.roots.clone();
        tracing::info!(
            roots = roots.len(),
            whole_home = asked.whole_home,
            "duplicate search asked"
        );
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("okapi-duplicates-start".to_owned())
            .spawn(move || {
                let opened = tracing::info_span!("duplicate_search")
                    .in_scope(|| open_search(&roots, exclusions, &threads));
                // Given up, or the screen gone: the search refused stops as
                // it is dropped.
                let _ = sender.send(opened);
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "duplicate search not started: no thread");
            self.refuse_start(ui::ToolRefusal::Thread);
            return;
        }
        let (whole_home, names) = (asked.whole_home, asked.names.clone());
        {
            let mut state = self.0.borrow_mut();
            state.search = Search::Opening(receiver);
            state.listed = 0;
            state.asked = Some(asked);
        }
        self.with_window(|duplicates| {
            duplicates.set_refused(ui::ToolRefusal::None);
            duplicates.set_searched_home(whole_home);
            duplicates.set_searched_names(names.into());
            duplicates.set_phase(ui::SearchPhase::Opening);
            duplicates.set_stage(ui::SearchStage::Listing);
            duplicates.set_files_listed(SharedString::from("0"));
            duplicates.set_step(ui::ToolStep::Searching);
        });
        self.watch();
    }

    /// Nothing started, or nothing can go on, for `refusal`: said on the
    /// start.
    fn refuse_start(&self, refusal: ui::ToolRefusal) {
        self.give_up();
        self.with_window(|duplicates| {
            duplicates.set_refused(refusal);
            duplicates.set_step(ui::ToolStep::Start);
        });
        self.show_start();
    }

    fn take_opening(&self) {
        let taken = {
            let state = self.0.borrow();
            match &state.search {
                Search::Opening(receiver) => match receiver.try_recv() {
                    Ok(opened) => Some(Some(opened)),
                    Err(TryRecvError::Disconnected) => Some(None),
                    Err(TryRecvError::Empty) => None,
                },
                _ => None,
            }
        };
        let Some(opened) = taken else {
            return;
        };
        match opened {
            Some(Ok(search)) => {
                self.0.borrow_mut().search = Search::Running(Arc::new(search));
                self.with_window(|duplicates| duplicates.set_phase(ui::SearchPhase::Running));
            }
            // Traced where it was met.
            Some(Err(refusal)) => self.refuse_start(refusal),
            None => {
                tracing::error!("duplicate search start thread ended without word");
                self.refuse_start(ui::ToolRefusal::Thread);
            }
        }
    }

    /// The search's progress, then its end.
    fn follow_search(&self) {
        let search = match &self.0.borrow().search {
            Search::Running(search) => Arc::clone(search),
            _ => return,
        };
        self.show_progress(search.progress());
        let Some(outcome) = search.wait(Duration::ZERO) else {
            return;
        };
        tracing::info!(?outcome, "duplicate search ended");
        let phase = match (outcome, search.report()) {
            (Outcome::Complete, Some(report)) => {
                self.0.borrow_mut().search = Search::Shown(Arc::clone(&search));
                self.show_result(report);
                return;
            }
            (Outcome::Complete, None) => {
                tracing::error!("duplicate search complete without a report");
                ui::SearchPhase::Failed
            }
            (Outcome::Cancelled, _) => ui::SearchPhase::Stopped,
            (Outcome::Failed, _) => ui::SearchPhase::Failed,
        };
        self.0.borrow_mut().search = Search::Idle;
        self.with_window(|duplicates| duplicates.set_phase(phase));
    }

    fn show_progress(&self, progress: Progress) {
        let language = self.0.borrow().language;
        if let Progress::Listing { files } = progress {
            self.0.borrow_mut().listed = files;
        }
        let listed = grouped(self.0.borrow().listed, language);
        self.with_window(|duplicates| {
            duplicates.set_files_listed(listed.into());
            match progress {
                Progress::Listing { .. } => duplicates.set_stage(ui::SearchStage::Listing),
                Progress::Ends { read, to_read } => {
                    duplicates.set_stage(ui::SearchStage::Ends);
                    duplicates.set_ends_read(grouped(read, language).into());
                    duplicates.set_ends_to_read(grouped(to_read, language).into());
                    duplicates.set_done(part(read, to_read));
                }
                Progress::Contents {
                    bytes_read,
                    bytes_to_read,
                } => {
                    duplicates.set_stage(ui::SearchStage::Contents);
                    duplicates.set_bytes_read(size(bytes_read));
                    duplicates.set_bytes_to_read(size(bytes_to_read));
                    duplicates.set_done(part(bytes_read, bytes_to_read));
                }
            }
        });
    }

    /// The report shown, its copies checked by the rule.
    fn show_result(&self, report: &DuplicateReport) {
        let (known, now, language) = {
            let state = self.0.borrow();
            let known = state
                .asked
                .as_ref()
                .map(|asked| asked.known.clone())
                .unwrap_or_default();
            (known, (state.seams.search.now)(), state.language)
        };
        let groups: Vec<GroupChoice> = report
            .groups()
            .iter()
            .map(|group| GroupChoice::ruled(group, &known.folders, known.trash_device))
            .collect();
        let checked: usize = groups
            .iter()
            .map(|held| {
                held.chosen()
                    .iter()
                    .filter(|chosen| chosen.is_checked())
                    .count()
            })
            .sum();
        let outside: usize = groups.iter().map(GroupChoice::outside).sum();
        let gain = report
            .groups()
            .iter()
            .zip(&groups)
            .map(|(group, held)| held.gain(group))
            .fold(0, u64::saturating_add);
        let mut quick: Vec<usize> = groups.iter().flat_map(GroupChoice::folders).collect();
        quick.sort_unstable();
        quick.dedup();
        let copies: usize = report
            .groups()
            .iter()
            .map(|group| group.copies().len())
            .sum();
        tracing::info!(
            groups = groups.len(),
            gain,
            engine_gain = report.gain(),
            checked,
            kept = copies - checked,
            outside_personal = outside,
            "duplicates shown, checked by the rule"
        );
        let names: Vec<SharedString> = quick
            .iter()
            .filter_map(|&rank| known.folders.get(rank))
            .map(|folder| name_of(&folder.path).into())
            .collect();
        let searched = report.stats();
        let compared = grouped(
            searched.files.saturating_sub(searched.below_minimum),
            language,
        );
        {
            let mut state = self.0.borrow_mut();
            state.groups = groups;
            state.quick_folders = quick;
            state.ended_at = Some(now);
        }
        crate::shown::show(&self.0.borrow().quick_model, names);
        let count = shown(report.groups().len());
        let rule_said = !known.folders.is_empty();
        self.with_window(|duplicates| {
            duplicates.set_groups(count);
            duplicates.set_duplicated(size(gain));
            duplicates.set_compared(compared.into());
            duplicates.set_minutes(0);
            duplicates.set_current(0);
            duplicates.set_rule_said(rule_said);
            duplicates.set_refused(ui::ToolRefusal::None);
            duplicates.set_step(ui::ToolStep::Result);
        });
        self.show_rows();
        self.watch_minutes();
    }

    fn watch_minutes(&self) {
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

    fn show_minutes(&self) {
        let (ended_at, now) = {
            let state = self.0.borrow();
            (state.ended_at, (state.seams.search.now)())
        };
        if let Some(ended_at) = ended_at {
            let minutes = shown(crate::dates::minutes_since(ended_at, now));
            self.with_window(|duplicates| duplicates.set_minutes(minutes));
        }
    }

    /// The lines of the tree and what is chosen, as they stand.
    fn show_rows(&self) {
        let Some(search) = self.0.borrow().shown() else {
            return;
        };
        let Some(report) = search.report() else {
            return;
        };
        let (rows, lines, totals) = {
            let state = self.0.borrow();
            let known = state.asked.as_ref().map(|asked| &asked.known);
            let names: Vec<String> = known.map(Known::names).unwrap_or_default();
            // One instant for every line: none says today, another
            // yesterday.
            let now = (state.seams.search.now)();
            let zone = state.seams.search.zone.clone();
            let moment = move |at: SystemTime| crate::dashboard::moment_of_time(at, now, &zone);
            let view = View {
                home: known.and_then(|known| known.home.as_deref()),
                names: &names,
                room: state.room,
                moment: &moment,
            };
            rows::lines(report, &state.groups, &view)
        };
        let model = Rc::clone(&self.0.borrow().row_model);
        crate::shown::show(&model, rows);
        self.0.borrow_mut().lines = lines;
        self.with_window(|duplicates| {
            duplicates.set_chosen_size(size(totals.bytes));
            duplicates.set_chosen_files(shown(totals.files));
            duplicates.set_chosen_groups(shown(totals.groups));
        });
    }

    fn line(&self, index: usize) -> Option<Line> {
        self.0.borrow().lines.get(index).copied()
    }

    /// Whether the choices may change: never while copies move (what goes
    /// is already taken).
    fn choosing(&self) -> bool {
        self.0.borrow().moving.is_none()
    }

    fn toggle(&self, index: usize) {
        let Some(Line::Copy(group, copy)) = self.line(index) else {
            return;
        };
        let Some(search) = self.0.borrow().shown() else {
            return;
        };
        let Some(report_group) = search
            .report()
            .and_then(|report| report.groups().get(group))
        else {
            return;
        };
        if !self.choosing() {
            tracing::debug!(group, copy, "copies moving: no box changes");
            return;
        }
        let toggled = self
            .0
            .borrow_mut()
            .groups
            .get_mut(group)
            .is_some_and(|held| held.toggle(report_group, copy));
        if !toggled {
            tracing::debug!(
                group,
                copy,
                "copy not checkable: the last one left, of other names, or on another disk"
            );
            return;
        }
        self.show_rows();
    }

    fn fold(&self, index: usize, open: bool) {
        let Some(Line::Head(group)) = self.line(index) else {
            return;
        };
        if let Some(held) = self.0.borrow_mut().groups.get_mut(group) {
            held.fold(open);
        }
        self.show_rows();
    }

    fn quick(&self, rule: ui::QuickRule, folder: i32) {
        if !self.choosing() {
            return;
        }
        let quick = match rule {
            ui::QuickRule::Oldest => Quick::Oldest,
            ui::QuickRule::Newest => Quick::Newest,
            ui::QuickRule::Shortest => Quick::Shortest,
            ui::QuickRule::Folder => {
                let rank = usize::try_from(folder)
                    .ok()
                    .and_then(|index| self.0.borrow().quick_folders.get(index).copied());
                let Some(rank) = rank else {
                    tracing::warn!(folder, "no folder offered there: nothing chosen");
                    return;
                };
                Quick::InFolder(rank)
            }
        };
        let Some(search) = self.0.borrow().shown() else {
            return;
        };
        let Some(report) = search.report() else {
            return;
        };
        let applied = {
            let mut state = self.0.borrow_mut();
            let mut applied = 0;
            for (group, held) in report.groups().iter().zip(state.groups.iter_mut()) {
                if held.quick(group, quick) {
                    applied += 1;
                }
            }
            applied
        };
        tracing::info!(?quick, groups = applied, "quick selection applied");
        self.show_rows();
    }

    fn deselect_all(&self) {
        if !self.choosing() {
            return;
        }
        for held in &mut self.0.borrow_mut().groups {
            held.deselect();
        }
        tracing::info!("every copy unchecked");
        self.show_rows();
    }

    fn path_room(&self, room: usize) {
        if self.0.borrow().room == room {
            return;
        }
        self.0.borrow_mut().room = room;
        self.show_rows();
    }

    /// Shows the copy of line `index` in its folder, if it is still the
    /// file the search read.
    fn open(&self, index: usize) {
        let Some(Line::Copy(group, copy)) = self.line(index) else {
            return;
        };
        let Some(search) = self.0.borrow().shown() else {
            return;
        };
        let Some(found) = search
            .report()
            .and_then(|report| report.groups().get(group))
            .and_then(|group| group.copies().get(copy))
        else {
            return;
        };
        let request = OpenRequest {
            path: found.path().to_path_buf(),
            folder: false,
            expected: Some(found.identity()),
        };
        let (calls, open) = {
            let state = self.0.borrow();
            (state.calls.clone(), Rc::clone(&state.seams.search.open))
        };
        if calls.awaiting() {
            tracing::debug!(
                index,
                "a call to the file manager already awaited: none asked"
            );
            return;
        }
        // Said first: the answer may come at once, and say a refusal.
        self.with_window(|duplicates| {
            duplicates.set_opening(true);
            duplicates.set_open_refused(false);
        });
        let weak = Rc::downgrade(&self.0);
        let asked_for = Line::Copy(group, copy);
        calls.ask(&open, &request, move |opened| {
            if let Some(state) = weak.upgrade() {
                let screen = Self(state);
                // Said on the copy it was asked for only, wherever lines
                // folded since put it.
                let current = screen
                    .read_window(|window| window.global::<ui::Duplicates>().get_current())
                    .and_then(|current| usize::try_from(current).ok());
                let same_copy = current.and_then(|current| screen.line(current)) == Some(asked_for);
                screen.with_window(|duplicates| {
                    duplicates.set_opening(false);
                    duplicates.set_open_refused(same_copy && opened == Some(false));
                });
            }
        });
    }

    fn ask_confirmation(&self) {
        let chosen = self
            .read_window(|window| window.global::<ui::Duplicates>().get_chosen_files())
            .unwrap_or(0);
        if chosen > 0 && self.choosing() {
            self.with_window(|duplicates| duplicates.set_confirming(true));
        }
    }

    fn cancel_confirmation(&self) {
        self.with_window(|duplicates| duplicates.set_confirming(false));
    }

    /// Moves the copies checked to the trash on a thread of its own: each
    /// is read again there, and so is a copy that stays (invariant 7).
    fn confirm(&self) {
        self.with_window(|duplicates| duplicates.set_confirming(false));
        if !self.choosing() {
            return;
        }
        let Some(search) = self.0.borrow().shown() else {
            tracing::warn!("a confirmation without a result refused");
            return;
        };
        let home = self
            .0
            .borrow()
            .asked
            .as_ref()
            .and_then(|asked| asked.known.home.clone());
        let exclusions = match self.exclusion_paths(home.as_deref()) {
            Ok(paths) => paths,
            Err(refusal) => {
                self.with_window(|duplicates| duplicates.set_refused(refusal));
                return;
            }
        };
        let (ids, bytes): (Vec<CopyId>, u64) = {
            let state = self.0.borrow();
            let Some(report) = search.report() else {
                return;
            };
            let ids: Vec<CopyId> = report
                .groups()
                .iter()
                .zip(&state.groups)
                .flat_map(|(group, held)| held.checked(group))
                .collect();
            let bytes = ids
                .iter()
                .filter_map(|id| report.find(*id))
                .map(|(group, _)| group.size())
                .fold(0, u64::saturating_add);
            (ids, bytes)
        };
        if ids.is_empty() {
            return;
        }
        tracing::info!(copies = ids.len(), bytes, "move to the trash confirmed");
        let trash = Arc::clone(&self.0.borrow().seams.search.trash);
        let (sender, receiver) = mpsc::channel();
        let moved = Arc::clone(&search);
        let spawned = std::thread::Builder::new()
            .name("okapi-duplicates-trash".to_owned())
            .spawn(move || {
                let receipt = tracing::info_span!("duplicate_trash")
                    .in_scope(|| move_to_trash(&moved, &ids, exclusions, &trash));
                // The screen gone: the move ended all the same, each copy
                // traced where it was moved or kept.
                let _ = sender.send(receipt);
            });
        match spawned {
            Ok(_) => {
                self.0.borrow_mut().moving = Some(Moving { receiver, search });
                self.with_window(|duplicates| {
                    duplicates.set_refused(ui::ToolRefusal::None);
                    duplicates.set_trashing(true);
                });
                self.watch();
            }
            Err(error) => {
                tracing::error!(%error, "no thread to move copies to the trash");
                self.with_window(|duplicates| {
                    duplicates.set_refused(ui::ToolRefusal::Thread);
                });
            }
        }
    }

    fn take_receipt(&self) {
        let taken = {
            let state = self.0.borrow();
            match state
                .moving
                .as_ref()
                .map(|moving| moving.receiver.try_recv())
            {
                Some(Ok(receipt)) => Some(Some(receipt)),
                Some(Err(TryRecvError::Disconnected)) => Some(None),
                _ => None,
            }
        };
        let Some(receipt) = taken else {
            return;
        };
        let Some(moving) = self.0.borrow_mut().moving.take() else {
            return;
        };
        self.with_window(|duplicates| duplicates.set_trashing(false));
        match receipt {
            Some(Ok(receipt)) => self.show_receipt(&moving.search, &receipt),
            // Traced where it was met: nothing moved.
            Some(Err(refusal)) => {
                self.with_window(|duplicates| duplicates.set_refused(refusal));
            }
            // Some copies may be in the trash already: the report no longer
            // says what is where.
            None => {
                tracing::error!("the thread moving copies ended without word");
                self.refuse_start(ui::ToolRefusal::Interrupted);
            }
        }
    }

    fn show_receipt(&self, search: &DuplicateSearch, receipt: &TrashReceipt) {
        let Some(report) = search.report() else {
            tracing::error!("a receipt without its report: nothing said");
            self.refuse_start(ui::ToolRefusal::Interrupted);
            return;
        };
        let home = self
            .0
            .borrow()
            .asked
            .as_ref()
            .and_then(|asked| asked.known.home.clone());
        let said = rows::receipt(report, receipt, home.as_deref());
        let at = {
            let state = self.0.borrow();
            let now = (state.seams.search.now)();
            crate::dashboard::moment(now.as_second(), now, &state.seams.search.zone)
                .unwrap_or_default()
        };
        crate::shown::show(&self.0.borrow().kept_model, said.kept);
        self.with_window(|duplicates| {
            duplicates.set_trashed_files(shown(said.trashed));
            duplicates.set_moved_files(shown(said.trashed + said.unchecked));
            duplicates.set_moved(size(said.moved_bytes));
            duplicates.set_trashed_at(at);
            duplicates.set_step(ui::ToolStep::Receipt);
        });
        // What it found is no more: a new search starts from scratch.
        self.give_up();
    }

    /// Lets the search go: it stops, its threads end on their own.
    fn give_up(&self) {
        let mut state = self.0.borrow_mut();
        match std::mem::replace(&mut state.search, Search::Idle) {
            Search::Running(search) | Search::Shown(search) => search.cancel(),
            Search::Opening(_) | Search::Idle => {}
        }
        state.groups.clear();
        state.lines.clear();
        state.minutes_timer.stop();
        let calls = state.calls.clone();
        drop(state);
        calls.forget();
        crate::shown::show(&self.0.borrow().row_model, Vec::new());
        self.with_window(|duplicates| {
            duplicates.set_opening(false);
            duplicates.set_open_refused(false);
            duplicates.set_confirming(false);
        });
    }

    fn stop(&self) {
        let search = match &self.0.borrow().search {
            Search::Opening(_) => None,
            Search::Running(search) => Some(Arc::clone(search)),
            Search::Idle | Search::Shown(_) => return,
        };
        if let Some(search) = search {
            tracing::info!("duplicate search stopped by the user");
            search.cancel();
            self.read();
        } else {
            tracing::info!("duplicate search stopped while opening");
            self.give_up();
            self.with_window(|duplicates| duplicates.set_phase(ui::SearchPhase::Stopped));
        }
    }

    fn change(&self) {
        if !self.choosing() {
            return;
        }
        self.give_up();
        self.with_window(|duplicates| {
            duplicates.set_refused(ui::ToolRefusal::None);
            duplicates.set_step(ui::ToolStep::Start);
        });
        self.show_start();
        self.read_folders();
    }

    /// The same folders, searched again.
    fn again(&self) {
        if !self.choosing() {
            return;
        }
        let asked = self.0.borrow().asked.clone();
        if let Some(asked) = asked {
            self.start(asked);
        }
    }

    /// The window closes: the search stops; a move to the trash is waited
    /// for a little (each move is one rename, a cut one leaves at most a
    /// `.trashinfo` without its file, or a file in the trash not read
    /// again).
    pub fn shut_down(&self) {
        let moving = {
            let mut state = self.0.borrow_mut();
            if let Search::Running(search) | Search::Shown(search) =
                std::mem::replace(&mut state.search, Search::Idle)
            {
                search.cancel();
            }
            state.moving.take()
        };
        if let Some(moving) = moving {
            crate::user_files::wait_at_closing(&moving.receiver, "duplicates");
        }
    }
}

/// Seams reading no folder and opening no trash, for the other screens'
/// tests.
#[cfg(test)]
pub fn inert() -> DuplicateSeams {
    DuplicateSeams {
        search: crate::user_files::inert(),
        threads: Arc::new(|_| NonZeroUsize::MIN),
    }
}

// Windows on real folders: Linux only, as the search's descriptors.
#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;
