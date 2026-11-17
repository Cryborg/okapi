//! The large files screen: where to search (the personal folders, or the
//! whole home folder), the search (`okapi_tools::large_files`) on a thread
//! of its own read on a timer, its files with their filters, order and
//! boxes (`rows`), the confirmation, the move to the trash on a thread of
//! its own, its receipt. A file goes only by its identifier in the report
//! kept (invariant 1), and a box follows its file, never its line;
//! `okapi_tools::large_files::trash` checks each file again just before
//! (invariant 7).

mod rows;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime};

use jiff::Timestamp;
use okapi_rules::Language;
use okapi_tools::large_files::{
    FileId, FileKind, LargeFile, LargeFileReport, LargeFileSearch, MIN_SIZE, Outcome, SearchError,
    SearchOptions, TrashReceipt,
};
use slint::{ComponentHandle as _, ModelRc, VecModel};

use crate::dates::MINUTES_EVERY;
use crate::disk_usage::READ_EVERY;
use crate::file_manager::{Calls, OpenRequest};
use crate::shown::{DEFAULT_ROOM, grouped, shown, size};
use crate::ui;
use crate::user_files::{
    Asked, Folders, OpenTrash, ReadExclusions, SearchSeams, exclusion_paths, resolve,
};
use rows::{AGES, Facts, Filters, Limits, Sort, View};

/// What the screen reads and asks of the machine: the system's, or a test's.
#[derive(Clone, Debug)]
pub struct LargeFileSeams {
    pub search: SearchSeams,
    /// The smallest file listed.
    pub min_size: u64,
}

impl LargeFileSeams {
    /// The session's home folder `home`, its personal folders, its trash,
    /// the portal; the engine's floor.
    pub fn system(home: Option<PathBuf>) -> Self {
        Self {
            search: SearchSeams::system(home),
            min_size: MIN_SIZE,
        }
    }
}

/// Opens the search of `roots` on the start's thread: its exclusions
/// resolved first, nothing read when one is refused.
fn open_search(
    roots: &[PathBuf],
    exclusions: Vec<PathBuf>,
    min_size: u64,
) -> Result<LargeFileSearch, ui::ToolRefusal> {
    let exclusions = resolve(exclusions)?;
    let options = SearchOptions {
        min_size,
        exclusions,
    };
    LargeFileSearch::start(roots, options).map_err(|error| {
        tracing::warn!(%error, "large file search not opened");
        match error {
            SearchError::Thread(_) => ui::ToolRefusal::Thread,
            SearchError::NoRoot | SearchError::Root { .. } => ui::ToolRefusal::Folder,
        }
    })
}

/// Moves the files `ids` of `search`'s report to the trash, on the move's
/// thread: its exclusions resolved first, then the report and the trash
/// opened; nothing moved when any of them fails.
fn move_to_trash(
    search: &LargeFileSearch,
    ids: &[FileId],
    exclusions: Vec<PathBuf>,
    trash: &OpenTrash,
) -> Result<TrashReceipt, ui::ToolRefusal> {
    let exclusions = resolve(exclusions)?;
    let Some(report) = search.report() else {
        tracing::error!("no report to move files of: nothing moved");
        return Err(ui::ToolRefusal::Trash);
    };
    let bin = trash().map_err(|error| {
        tracing::error!(%error, "trash not opened: nothing moved");
        ui::ToolRefusal::Trash
    })?;
    Ok(okapi_tools::large_files::trash(
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
    Opening(Receiver<Result<LargeFileSearch, ui::ToolRefusal>>),
    Running(Arc<LargeFileSearch>),
    /// Complete: its report shown.
    Shown(Arc<LargeFileSearch>),
}

/// A move to the trash on its way: its receipt to come, and the search
/// whose report names its files.
struct Moving {
    receiver: Receiver<Result<TrashReceipt, ui::ToolRefusal>>,
    search: Arc<LargeFileSearch>,
}

struct State {
    window: slint::Weak<ui::AppWindow>,
    seams: LargeFileSeams,
    exclusions: ReadExclusions,
    language: Language,
    folders: Folders,
    asked: Option<Asked>,
    search: Search,
    /// The floors a filter offers, above the smallest file listed.
    floors: Vec<u64>,
    /// What the screen reads of each file of the report shown, and whether
    /// its box is checked, by its rank in the report.
    files: Vec<Facts>,
    checked: Vec<bool>,
    filters: Filters,
    sort: Sort,
    /// The rank of the file each line shows.
    lines: Vec<usize>,
    /// The ranks the confirmation open names: what goes when it is
    /// confirmed, nothing else chosen meanwhile (a screen reader still
    /// reaches the lines behind it).
    confirming: Option<Vec<usize>>,
    room: usize,
    ended_at: Option<Timestamp>,
    moving: Option<Moving>,
    calls: Calls,
    folder_model: Rc<VecModel<ui::PersonalShown>>,
    row_model: Rc<VecModel<ui::LargeFileRow>>,
    kind_model: Rc<VecModel<ui::KindCount>>,
    confirmed_model: Rc<VecModel<ui::ChosenFile>>,
    kept_model: Rc<VecModel<ui::KeptShown>>,
    timer: slint::Timer,
    minutes_timer: slint::Timer,
}

impl State {
    /// The search whose report is shown, if one is complete.
    fn shown(&self) -> Option<Arc<LargeFileSearch>> {
        match &self.search {
            Search::Shown(search) => Some(Arc::clone(search)),
            _ => None,
        }
    }

    fn home(&self) -> Option<PathBuf> {
        self.asked
            .as_ref()
            .and_then(|asked| asked.known.home.clone())
    }
}

/// The large files screen.
#[derive(Clone)]
pub struct LargeFilesScreen(Rc<RefCell<State>>);

impl std::fmt::Debug for LargeFilesScreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LargeFilesScreen")
            .finish_non_exhaustive()
    }
}

impl LargeFilesScreen {
    pub fn attach(
        window: &ui::AppWindow,
        seams: LargeFileSeams,
        exclusions: ReadExclusions,
        language: Language,
    ) -> Self {
        let floors = rows::floors(seams.min_size);
        let state = State {
            window: window.as_weak(),
            folders: Folders::new(&seams.search),
            seams,
            exclusions,
            language,
            asked: None,
            search: Search::Idle,
            floors,
            files: Vec::new(),
            checked: Vec::new(),
            filters: Filters::default(),
            sort: Sort::default(),
            lines: Vec::new(),
            confirming: None,
            room: DEFAULT_ROOM,
            ended_at: None,
            moving: None,
            calls: Calls::default(),
            folder_model: Rc::new(VecModel::default()),
            row_model: Rc::new(VecModel::default()),
            kind_model: Rc::new(VecModel::default()),
            confirmed_model: Rc::new(VecModel::default()),
            kept_model: Rc::new(VecModel::default()),
            timer: slint::Timer::default(),
            minutes_timer: slint::Timer::default(),
        };
        let large = window.global::<ui::LargeFiles>();
        large.set_folders(ModelRc::from(Rc::clone(&state.folder_model)));
        large.set_rows(ModelRc::from(Rc::clone(&state.row_model)));
        large.set_kinds(ModelRc::from(Rc::clone(&state.kind_model)));
        large.set_confirmed(ModelRc::from(Rc::clone(&state.confirmed_model)));
        large.set_kept(ModelRc::from(Rc::clone(&state.kept_model)));
        let floors: Vec<ui::Size> = state.floors.iter().map(|&floor| size(floor)).collect();
        large.set_floors(ModelRc::new(VecModel::from(floors)));
        large.set_ages(ModelRc::new(VecModel::from(
            AGES.map(rows::age_shown).to_vec(),
        )));
        large.set_min_size(size(state.seams.min_size));
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
        large.on_search(on(Self::search));
        large.on_stop(on(Self::stop));
        large.on_change(on(Self::change));
        large.on_again(on(Self::again));
        large.on_deselect_all(on(Self::deselect_all));
        large.on_clear_filters(on(Self::clear_filters));
        large.on_trash(on(Self::ask_confirmation));
        large.on_cancel_confirmation(on(Self::cancel_confirmation));
        large.on_confirm(on(Self::confirm));
        let weak = Rc::downgrade(&screen.0);
        let with_index = move |action: fn(&Self, usize)| {
            let weak = weak.clone();
            move |index: i32| {
                if let (Some(state), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                    action(&Self(state), index);
                }
            }
        };
        large.on_toggle_folder(with_index(Self::toggle_folder));
        large.on_toggle(with_index(Self::toggle));
        large.on_open(with_index(Self::open));
        large.on_path_room(with_index(Self::path_room));
        // A filter's rank, or -1 for none.
        let weak = Rc::downgrade(&screen.0);
        let with_rank = move |action: fn(&Self, Option<usize>)| {
            let weak = weak.clone();
            move |rank: i32| {
                if let Some(state) = weak.upgrade() {
                    action(&Self(state), usize::try_from(rank).ok());
                }
            }
        };
        large.on_filter_size(with_rank(Self::filter_size));
        large.on_filter_kind(with_rank(Self::filter_kind));
        large.on_filter_age(with_rank(Self::filter_age));
        let weak = Rc::downgrade(&screen.0);
        large.on_sort_by(move |sort| {
            if let Some(state) = weak.upgrade() {
                Self(state).sort_by(sort);
            }
        });
        large.set_step(ui::ToolStep::Start);
        screen
    }

    fn with_window(&self, show: impl FnOnce(&ui::LargeFiles<'_>)) {
        let window = self.0.borrow().window.upgrade();
        if let Some(window) = window {
            show(&window.global::<ui::LargeFiles>());
        }
    }

    fn read_window<T>(&self, read: impl FnOnce(&ui::AppWindow) -> T) -> Option<T> {
        let window = self.0.borrow().window.upgrade();
        window.map(|window| read(&window))
    }

    fn step(&self) -> Option<ui::ToolStep> {
        self.read_window(|window| window.global::<ui::LargeFiles>().get_step())
    }

    /// Follows the page shown: the folders are read when the start shows.
    pub fn page_shown(&self, page: ui::Page) {
        if page == ui::Page::LargeFiles && self.step() == Some(ui::ToolStep::Start) {
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
        self.with_window(|large| {
            large.set_folders_state(start.state);
            large.set_home_path(start.home.into());
            large.set_chosen_folders(shown(start.chosen));
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
            .read_window(|window| window.global::<ui::LargeFiles>().get_whole_home())
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
            Err(refusal) if self.step() == Some(ui::ToolStep::Result) => {
                self.with_window(|large| large.set_refused(refusal));
                return;
            }
            Err(refusal) => {
                self.refuse_start(refusal);
                return;
            }
        };
        self.give_up();
        let min_size = self.0.borrow().seams.min_size;
        let roots = asked.roots.clone();
        tracing::info!(
            roots = roots.len(),
            whole_home = asked.whole_home,
            min_size,
            "large file search asked"
        );
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("okapi-large-files-start".to_owned())
            .spawn(move || {
                let opened = tracing::info_span!("large_file_search")
                    .in_scope(|| open_search(&roots, exclusions, min_size));
                // Given up, or the screen gone: the search refused stops as
                // it is dropped.
                let _ = sender.send(opened);
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "large file search not started: no thread");
            self.refuse_start(ui::ToolRefusal::Thread);
            return;
        }
        let (whole_home, names) = (asked.whole_home, asked.names.clone());
        {
            let mut state = self.0.borrow_mut();
            state.search = Search::Opening(receiver);
            state.asked = Some(asked);
        }
        self.with_window(|large| {
            large.set_refused(ui::ToolRefusal::None);
            large.set_searched_home(whole_home);
            large.set_searched_names(names.into());
            large.set_phase(ui::SearchPhase::Opening);
            large.set_files_listed("0".into());
            large.set_files_count(0);
            large.set_found("0".into());
            large.set_found_count(0);
            large.set_step(ui::ToolStep::Searching);
        });
        self.watch();
    }

    /// Nothing started, or nothing can go on, for `refusal`: said on the
    /// start.
    fn refuse_start(&self, refusal: ui::ToolRefusal) {
        self.give_up();
        self.with_window(|large| {
            large.set_refused(refusal);
            large.set_step(ui::ToolStep::Start);
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
                self.with_window(|large| large.set_phase(ui::SearchPhase::Running));
            }
            // Traced where it was met.
            Some(Err(refusal)) => self.refuse_start(refusal),
            None => {
                tracing::error!("large file search start thread ended without word");
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
        let progress = search.progress();
        let language = self.0.borrow().language;
        self.with_window(|large| {
            large.set_files_listed(grouped(progress.files, language).into());
            large.set_files_count(shown(progress.files));
            large.set_found(grouped(progress.found, language).into());
            large.set_found_count(shown(progress.found));
        });
        let Some(outcome) = search.wait(Duration::ZERO) else {
            return;
        };
        if outcome == Outcome::Failed {
            tracing::warn!(?outcome, "large file search ended");
        } else {
            tracing::info!(?outcome, "large file search ended");
        }
        let phase = match (outcome, search.report()) {
            (Outcome::Complete, Some(report)) => {
                self.0.borrow_mut().search = Search::Shown(Arc::clone(&search));
                self.show_result(report);
                return;
            }
            (Outcome::Complete, None) => {
                tracing::error!("large file search complete without a report");
                ui::SearchPhase::Failed
            }
            (Outcome::Cancelled, _) => ui::SearchPhase::Stopped,
            (Outcome::Failed, _) => ui::SearchPhase::Failed,
        };
        self.0.borrow_mut().search = Search::Idle;
        self.with_window(|large| large.set_phase(phase));
    }

    /// The report shown, nothing checked, the filters and the order as they
    /// are at first.
    fn show_result(&self, report: &LargeFileReport) {
        let (known, now, language) = {
            let state = self.0.borrow();
            let known = state
                .asked
                .as_ref()
                .map(|asked| asked.known.clone())
                .unwrap_or_default();
            (known, (state.seams.search.now)(), state.language)
        };
        let files: Vec<Facts> = report
            .files()
            .iter()
            .map(|file| Facts::of(file, &known.folders, known.trash_device))
            .collect();
        let other_names = files.iter().filter(|file| !file.frees).count();
        let other_device = files.iter().filter(|file| !file.trashable).count();
        // Said only when the personal folders are known, as the screen says
        // it.
        let personal_known = !known.folders.is_empty();
        let outside = files
            .iter()
            .filter(|file| personal_known && !file.personal)
            .count();
        let mut by_kind: Vec<(&'static str, usize)> = FileKind::ALL
            .iter()
            .map(|kind| {
                (
                    kind.key(),
                    files.iter().filter(|file| file.kind == *kind).count(),
                )
            })
            .collect();
        by_kind.retain(|(_, count)| *count > 0);
        tracing::info!(
            files = files.len(),
            bytes = report.total(),
            ?by_kind,
            personal_known,
            outside_personal = outside,
            other_names,
            other_device,
            "large files shown, nothing checked"
        );
        let walked = report.stats().files;
        let all_small = report.stats().below_minimum == walked;
        {
            let mut state = self.0.borrow_mut();
            state.checked = vec![false; files.len()];
            state.files = files;
            state.filters = Filters::default();
            state.sort = Sort::default();
            state.lines.clear();
            state.ended_at = Some(now);
        }
        let count = shown(report.files().len());
        self.with_window(|large| {
            large.set_files(count);
            large.set_total(size(report.total()));
            large.set_walked(grouped(walked, language).into());
            large.set_walked_count(shown(walked));
            large.set_all_small(all_small);
            large.set_minutes(0);
            large.set_current(0);
            large.set_refused(ui::ToolRefusal::None);
            large.set_step(ui::ToolStep::Result);
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
            self.with_window(|large| large.set_minutes(minutes));
        }
    }

    /// The lines as the filters and the order leave them, what is chosen,
    /// the kinds counted; the focus kept on its file when it is still shown.
    fn show_rows(&self) {
        let current = self
            .read_window(|window| window.global::<ui::LargeFiles>().get_current())
            .and_then(|current| usize::try_from(current).ok());
        let (rows, kinds, totals, filters, lines, focused) = {
            let state = self.0.borrow();
            let focused = current.and_then(|current| state.lines.get(current).copied());
            let now = (state.seams.search.now)();
            let zone = state.seams.search.zone.clone();
            let limits = Limits::at(state.filters, now, &zone);
            let visible = rows::visible(&state.files, &limits, state.sort);
            let home = state.home();
            // One instant for every line: none says today, another
            // yesterday.
            let moment = move |at: SystemTime| crate::dashboard::moment_of_time(at, now, &zone);
            let view = View {
                home: home.as_deref(),
                personal_known: state
                    .asked
                    .as_ref()
                    .is_some_and(|asked| !asked.known.folders.is_empty()),
                room: state.room,
                moment: &moment,
            };
            let rows = rows::lines(&state.files, &state.checked, &visible.ranks, &view);
            let kinds: Vec<ui::KindCount> = FileKind::ALL
                .iter()
                .zip(visible.kinds)
                .map(|(kind, count)| ui::KindCount {
                    kind: rows::kind_shown(*kind),
                    count: shown(count),
                })
                .collect();
            let totals = rows::totals(&state.files, &rows::chosen(&state.checked), &visible.ranks);
            (
                rows,
                (kinds, visible.all_kinds),
                totals,
                state.filters,
                visible.ranks,
                focused,
            )
        };
        let kept_focus = focused.and_then(|rank| lines.iter().position(|&line| line == rank));
        let at = kept_focus.unwrap_or(0);
        let (kind_model, row_model) = {
            let state = self.0.borrow();
            (Rc::clone(&state.kind_model), Rc::clone(&state.row_model))
        };
        crate::shown::show(&kind_model, kinds.0);
        crate::shown::show(&row_model, rows);
        let rank_of = |kind: Option<FileKind>| {
            kind.and_then(|kind| FileKind::ALL.iter().position(|each| *each == kind))
        };
        let floor = {
            let state = self.0.borrow();
            filters
                .floor
                .and_then(|floor| state.floors.iter().position(|&each| each == floor))
        };
        let rank = |rank: Option<usize>| rank.map_or(-1, shown);
        let sort = match self.0.borrow().sort {
            Sort::Size => ui::LargeSort::Size,
            Sort::Oldest => ui::LargeSort::Oldest,
        };
        self.0.borrow_mut().lines = lines;
        self.with_window(|large| {
            large.set_all_kinds(shown(kinds.1));
            large.set_floor(rank(floor));
            large.set_kind(rank(rank_of(filters.kind)));
            large.set_age(rank(filters.age));
            large.set_sort(sort);
            large.set_current(shown(at));
            // A refusal of the file manager speaks of the file it was asked
            // for: another on the line, it is no more.
            if kept_focus.is_none() {
                large.set_open_refused(false);
            }
            large.set_chosen_size(size(totals.bytes));
            large.set_chosen_files(shown(totals.files));
            large.set_chosen_hidden(shown(totals.hidden));
        });
    }

    /// The rank of the file line `index` shows.
    fn rank_at(&self, index: usize) -> Option<usize> {
        self.0.borrow().lines.get(index).copied()
    }

    /// Whether the choices may change: never while the confirmation is
    /// open (what it names goes) nor while files move (what goes is already
    /// taken).
    fn choosing(&self) -> bool {
        let state = self.0.borrow();
        let free = state.moving.is_none() && state.confirming.is_none();
        if !free {
            tracing::debug!(
                moving = state.moving.is_some(),
                confirming = state.confirming.is_some(),
                "choices frozen"
            );
        }
        free
    }

    fn toggle(&self, index: usize) {
        if !self.choosing() {
            return;
        }
        let Some(rank) = self.rank_at(index) else {
            tracing::warn!(index, "no file on this line: no box changes");
            return;
        };
        let refused = {
            let mut state = self.0.borrow_mut();
            let State { files, checked, .. } = &mut *state;
            (!rows::toggle(files, checked, rank))
                .then(|| files.get(rank).map(|file| (file.frees, file.trashable)))
        };
        if let Some(file) = refused {
            tracing::debug!(
                rank,
                frees = file.map(|(frees, _)| frees),
                trashable = file.map(|(_, trashable)| trashable),
                "file not checkable: of other names, or on another disk"
            );
            return;
        }
        self.show_rows();
    }

    fn deselect_all(&self) {
        if !self.choosing() {
            return;
        }
        self.0.borrow_mut().checked.fill(false);
        tracing::info!("every file unchecked");
        self.show_rows();
    }

    /// Sets the filters and the order as `change` does, and says what they
    /// show.
    fn filter(&self, change: impl FnOnce(&mut Filters, &mut Sort, &[u64])) {
        if !self.choosing() {
            return;
        }
        {
            let mut state = self.0.borrow_mut();
            let State {
                filters,
                sort,
                floors,
                ..
            } = &mut *state;
            change(filters, sort, floors);
        }
        self.show_rows();
        let state = self.0.borrow();
        tracing::info!(
            floor = ?state.filters.floor,
            kind = ?state.filters.kind,
            age_months = ?state.filters.age.and_then(|rank| AGES.get(rank)),
            sort = ?state.sort,
            shown = state.lines.len(),
            files = state.files.len(),
            "large file filters or order changed"
        );
    }

    fn filter_size(&self, rank: Option<usize>) {
        self.filter(|filters, _, floors| {
            filters.floor = rank.and_then(|rank| floors.get(rank).copied());
        });
    }

    fn filter_kind(&self, rank: Option<usize>) {
        self.filter(|filters, _, _| {
            filters.kind = rank.and_then(|rank| FileKind::ALL.get(rank).copied());
        });
    }

    fn filter_age(&self, rank: Option<usize>) {
        self.filter(|filters, _, _| {
            filters.age = rank.filter(|&rank| rank < AGES.len());
        });
    }

    fn clear_filters(&self) {
        self.filter(|filters, _, _| *filters = Filters::default());
    }

    fn sort_by(&self, sort: ui::LargeSort) {
        let sort = match sort {
            ui::LargeSort::Size => Sort::Size,
            ui::LargeSort::Oldest => Sort::Oldest,
        };
        self.filter(|_, order, _| *order = sort);
    }

    fn path_room(&self, room: usize) {
        if self.0.borrow().room == room {
            return;
        }
        self.0.borrow_mut().room = room;
        self.show_rows();
    }

    /// Shows the file of line `index` in its folder, if it is still the
    /// file the search read.
    fn open(&self, index: usize) {
        let Some(rank) = self.rank_at(index) else {
            tracing::warn!(index, "no file on this line: none shown in its folder");
            return;
        };
        let Some(search) = self.0.borrow().shown() else {
            tracing::warn!(index, "no result: no file shown in its folder");
            return;
        };
        let Some(found) = search.report().and_then(|report| report.files().get(rank)) else {
            tracing::warn!(
                rank,
                "a rank beyond the report: no file shown in its folder"
            );
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
        self.with_window(|large| {
            large.set_opening(true);
            large.set_open_refused(false);
        });
        let weak = Rc::downgrade(&self.0);
        calls.ask(&open, &request, move |opened| {
            if let Some(state) = weak.upgrade() {
                let screen = Self(state);
                // Said on the file it was asked for only, wherever filters
                // or the order put it since (one rule with the page, which
                // clears it when the user moves the focus).
                let current = screen
                    .read_window(|window| window.global::<ui::LargeFiles>().get_current())
                    .and_then(|current| usize::try_from(current).ok());
                let same_file = current.and_then(|current| screen.rank_at(current)) == Some(rank);
                screen.with_window(|large| {
                    large.set_opening(false);
                    large.set_open_refused(same_file && opened == Some(false));
                });
            }
        });
    }

    /// The confirmation, naming what is chosen: that, and nothing else,
    /// goes when it is confirmed.
    fn ask_confirmation(&self) {
        if !self.choosing() {
            return;
        }
        let said = {
            let mut state = self.0.borrow_mut();
            let ranks = rows::chosen(&state.checked);
            if ranks.is_empty() {
                tracing::debug!("nothing checked: no confirmation asked");
                return;
            }
            tracing::info!(files = ranks.len(), "confirmation asked");
            let home = state.home();
            let said = rows::confirmed(&state.files, &ranks, &state.lines, home.as_deref());
            state.confirming = Some(ranks);
            said
        };
        let model = Rc::clone(&self.0.borrow().confirmed_model);
        crate::shown::show(&model, said.named);
        self.with_window(|large| {
            large.set_confirmed_more(shown(said.more));
            large.set_confirmed_more_hidden(shown(said.more_hidden));
            large.set_confirming(true);
        });
    }

    fn cancel_confirmation(&self) {
        tracing::info!("confirmation cancelled");
        self.0.borrow_mut().confirming = None;
        self.with_window(|large| large.set_confirming(false));
    }

    /// Moves the files the confirmation named to the trash on a thread of
    /// its own: each is read again there (invariant 7).
    fn confirm(&self) {
        self.with_window(|large| large.set_confirming(false));
        let Some(ranks) = self.0.borrow_mut().confirming.take() else {
            tracing::warn!("a confirmation never asked refused");
            return;
        };
        if self.0.borrow().moving.is_some() {
            tracing::warn!("a confirmation while files move refused");
            return;
        }
        let Some(search) = self.0.borrow().shown() else {
            tracing::warn!("a confirmation without a result refused");
            return;
        };
        let home = self.0.borrow().home();
        let exclusions = match self.exclusion_paths(home.as_deref()) {
            Ok(paths) => paths,
            Err(refusal) => {
                self.with_window(|large| large.set_refused(refusal));
                return;
            }
        };
        let (ids, totals) = {
            let state = self.0.borrow();
            let Some(report) = search.report() else {
                tracing::error!("a confirmation without its report: nothing moved");
                return;
            };
            let ids: Vec<FileId> = ranks
                .iter()
                .filter_map(|&rank| report.files().get(rank))
                .map(LargeFile::id)
                .collect();
            (ids, rows::totals(&state.files, &ranks, &state.lines))
        };
        if ids.is_empty() {
            tracing::warn!("a confirmation naming no file of the report: nothing moved");
            return;
        }
        tracing::info!(
            files = ids.len(),
            bytes = totals.bytes,
            hidden_by_filters = totals.hidden,
            "move to the trash confirmed"
        );
        let trash = Arc::clone(&self.0.borrow().seams.search.trash);
        let (sender, receiver) = mpsc::channel();
        let moved = Arc::clone(&search);
        let spawned = std::thread::Builder::new()
            .name("okapi-large-files-trash".to_owned())
            .spawn(move || {
                let receipt = tracing::info_span!("large_file_trash")
                    .in_scope(|| move_to_trash(&moved, &ids, exclusions, &trash));
                // The screen gone: the move ended all the same, each file
                // traced where it was moved or kept.
                let _ = sender.send(receipt);
            });
        match spawned {
            Ok(_) => {
                self.0.borrow_mut().moving = Some(Moving { receiver, search });
                self.with_window(|large| {
                    large.set_refused(ui::ToolRefusal::None);
                    large.set_trashing(true);
                });
                self.watch();
            }
            Err(error) => {
                tracing::error!(%error, "no thread to move files to the trash");
                self.with_window(|large| large.set_refused(ui::ToolRefusal::Thread));
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
        self.with_window(|large| large.set_trashing(false));
        match receipt {
            Some(Ok(receipt)) => self.show_receipt(&moving.search, &receipt),
            // Traced where it was met: nothing moved.
            Some(Err(refusal)) => {
                self.with_window(|large| large.set_refused(refusal));
            }
            // Some files may be in the trash already: the report no longer
            // says what is where.
            None => {
                tracing::error!("the thread moving files ended without word");
                self.refuse_start(ui::ToolRefusal::Interrupted);
            }
        }
    }

    fn show_receipt(&self, search: &LargeFileSearch, receipt: &TrashReceipt) {
        let Some(report) = search.report() else {
            tracing::error!("a receipt without its report: nothing said");
            self.refuse_start(ui::ToolRefusal::Interrupted);
            return;
        };
        let home = self.0.borrow().home();
        let said = rows::receipt(report, receipt, home.as_deref());
        let at = {
            let state = self.0.borrow();
            let now = (state.seams.search.now)();
            crate::dashboard::moment(now.as_second(), now, &state.seams.search.zone)
                .unwrap_or_default()
        };
        crate::shown::show(&self.0.borrow().kept_model, said.kept);
        self.with_window(|large| {
            large.set_trashed_files(shown(said.trashed));
            large.set_moved_files(shown(said.trashed + said.unchecked));
            large.set_moved(size(said.moved_bytes));
            large.set_trashed_at(at);
            large.set_step(ui::ToolStep::Receipt);
        });
        // What it found is no more: a new search starts from scratch.
        self.give_up();
    }

    /// Lets the search go: it stops, its thread ends on its own.
    fn give_up(&self) {
        let mut state = self.0.borrow_mut();
        match std::mem::replace(&mut state.search, Search::Idle) {
            Search::Running(search) | Search::Shown(search) => search.cancel(),
            Search::Opening(_) | Search::Idle => {}
        }
        state.files.clear();
        state.checked.clear();
        state.lines.clear();
        state.confirming = None;
        state.minutes_timer.stop();
        let calls = state.calls.clone();
        drop(state);
        calls.forget();
        crate::shown::show(&self.0.borrow().row_model, Vec::new());
        self.with_window(|large| {
            large.set_opening(false);
            large.set_open_refused(false);
            large.set_confirming(false);
        });
    }

    fn stop(&self) {
        let search = match &self.0.borrow().search {
            Search::Opening(_) => None,
            Search::Running(search) => Some(Arc::clone(search)),
            Search::Idle | Search::Shown(_) => return,
        };
        if let Some(search) = search {
            tracing::info!("large file search stopped by the user");
            search.cancel();
            self.read();
        } else {
            tracing::info!("large file search stopped while opening");
            self.give_up();
            self.with_window(|large| large.set_phase(ui::SearchPhase::Stopped));
        }
    }

    fn change(&self) {
        if !self.choosing() {
            return;
        }
        self.give_up();
        self.with_window(|large| {
            large.set_refused(ui::ToolRefusal::None);
            large.set_step(ui::ToolStep::Start);
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
    /// for a little.
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
            crate::user_files::wait_at_closing(&moving.receiver, "large files");
        }
    }
}

/// Seams reading no folder and opening no trash, for the other screens'
/// tests.
#[cfg(test)]
pub fn inert() -> LargeFileSeams {
    LargeFileSeams {
        search: crate::user_files::inert(),
        min_size: MIN_SIZE,
    }
}

// Windows on real folders: Linux only, as the search's descriptors.
#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;
