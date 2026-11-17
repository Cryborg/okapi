//! The History screen: every cleaning, uninstallation and installation of
//! updates the history keeps (`okapi_store::History::read_all`), newest
//! first, and the one chosen in detail, with its entries by result when its
//! deletion log was kept. The history is read on a thread of its own each
//! time the page is shown, and when a cleaning ends while it is; the window
//! looks for what it read every [`READ_EVERY`]. A reading asked for while
//! one runs is done once it ends, never two at once. Also how the other
//! screens add their lines ([`open_to_record`]).

use std::cell::RefCell;
use std::cmp::Reverse;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use okapi_store::{
    Deletions, History, HistoryEntry, HistoryOutcome, HistoryRead, HistoryRecord,
    HistoryUnavailable, PackageSource, StoreError, UninstallEntry, UpdateEntry,
};
use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use crate::cleaner::{line_shown, problem_of_key};
use crate::disk_usage::READ_EVERY;
use crate::receipt::{OutcomeFacts, ReceiptLines};
use crate::shown::{show, shown, size};
use crate::ui;
use crate::updates::record;

/// The thread that reads the history.
const READER_NAME: &str = "okapi-history-read";

/// Writes the history line of an operation (an uninstallation, updates
/// installed); a failure is traced where it is met.
pub type Record<T> = Arc<dyn Fn(&T) -> Result<(), HistoryUnavailable> + Send + Sync>;

/// The user's history, opened to add the line of `what` (`uninstallation`,
/// `update`): its failure traced.
///
/// # Errors
///
/// [`HistoryUnavailable`]: no state directory known, or the history not
/// opened.
pub fn open_to_record(what: &'static str) -> Result<History, HistoryUnavailable> {
    let Some(directory) = okapi_platform::state_directory() else {
        tracing::error!(what, "no state directory: not recorded");
        return Err(HistoryUnavailable::NoDirectory);
    };
    History::open(&directory)
        .inspect_err(|error| tracing::error!(%error, what, "history not opened: not recorded"))
        .map_err(HistoryUnavailable::from)
}

/// Where the screen reads the history, and its clock.
#[derive(Debug, Clone)]
pub struct HistorySeams {
    /// The state directory; none when none is known.
    pub directory: Option<PathBuf>,
    pub now: fn() -> Timestamp,
    pub zone: TimeZone,
}

impl HistorySeams {
    /// This user's state directory, and the system's clock and time zone.
    pub fn system() -> Self {
        Self {
            directory: okapi_platform::state_directory(),
            now: Timestamp::now,
            zone: crate::dashboard::system_zone(),
        }
    }
}

/// What the screen keeps of a deletion log: its entries by result, a few
/// paths each, and how many could not be read back.
#[derive(Debug, Default)]
pub struct KeptLog {
    lines: ReceiptLines,
    unreadable: usize,
}

impl Extend<HistoryOutcome> for KeptLog {
    fn extend<I: IntoIterator<Item = HistoryOutcome>>(&mut self, entries: I) {
        for entry in entries {
            let facts = OutcomeFacts::of_history(&entry).unwrap_or_else(|unreadable| {
                self.unreadable += 1;
                unreadable
            });
            self.lines.add(&facts);
        }
    }
}

/// What a reading gives.
type ReadOutcome = Result<HistoryRead<KeptLog>, StoreError>;

/// One operation as the screen keeps it: its row, and the lines of its
/// deletion log, shown once it is chosen.
struct Operation {
    row: ui::HistoryRow,
    time: i64,
    log: Option<ReceiptLines>,
}

struct State {
    window: slint::Weak<ui::AppWindow>,
    seams: HistorySeams,
    on_page: bool,
    reading: Option<Receiver<ReadOutcome>>,
    /// A reading was asked for while one ran.
    again: bool,
    /// Newest first, as the rows.
    operations: Vec<Operation>,
    rows: Rc<VecModel<ui::HistoryRow>>,
    lines: Rc<VecModel<ui::ReceiptLineShown>>,
    timer: slint::Timer,
}

/// The History screen.
#[derive(Clone)]
pub struct HistoryScreen(Rc<RefCell<State>>);

impl std::fmt::Debug for HistoryScreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HistoryScreen")
            .finish_non_exhaustive()
    }
}

impl HistoryScreen {
    /// Binds the screen to `window`, reading through `seams`.
    pub fn attach(window: &ui::AppWindow, seams: HistorySeams) -> Self {
        let state = State {
            window: window.as_weak(),
            seams,
            on_page: false,
            reading: None,
            again: false,
            operations: Vec::new(),
            rows: Rc::new(VecModel::default()),
            lines: Rc::new(VecModel::default()),
            timer: slint::Timer::default(),
        };
        let history = window.global::<ui::History>();
        history.set_rows(ModelRc::from(Rc::clone(&state.rows)));
        history.set_lines(ModelRc::from(Rc::clone(&state.lines)));
        let screen = Self(Rc::new(RefCell::new(state)));
        let weak = Rc::downgrade(&screen.0);
        history.on_choose(move |index| {
            if let Some(state) = weak.upgrade() {
                Self(state).choose(index);
            }
        });
        screen
    }

    /// Reads the history again each time the page shows.
    pub fn page_shown(&self, page: ui::Page) {
        let on_page = page == ui::Page::History;
        self.0.borrow_mut().on_page = on_page;
        if on_page {
            self.read();
        }
    }

    /// A cleaning ended: its line is shown if the page is.
    pub fn cleaning_done(&self) {
        if self.0.borrow().on_page {
            self.read();
        }
    }

    /// Reads the history on its own thread, or once the reading under way
    /// ends.
    fn read(&self) {
        let mut state = self.0.borrow_mut();
        if state.reading.is_some() {
            tracing::debug!("history asked again while read: read once this one ends");
            state.again = true;
            return;
        }
        let Some(window) = state.window.upgrade() else {
            return;
        };
        let history = window.global::<ui::History>();
        let Some(directory) = state.seams.directory.clone() else {
            tracing::warn!("no state directory: no history to show");
            history.set_reading(ui::HistoryReading::Unreadable);
            return;
        };
        let (sender, receiver) = mpsc::channel();
        let span = tracing::info_span!("history_screen");
        let started = thread::Builder::new()
            .name(READER_NAME.to_owned())
            .spawn(move || {
                let _span = span.entered();
                if sender.send(History::read_all(&directory)).is_err() {
                    tracing::debug!("history read for a screen gone");
                }
            });
        if let Err(error) = started {
            tracing::error!(%error, "history reader not started");
            history.set_reading(ui::HistoryReading::Unreadable);
            return;
        }
        tracing::debug!("history reading started");
        history.set_reading(ui::HistoryReading::Reading);
        state.reading = Some(receiver);
        let weak = Rc::downgrade(&self.0);
        state
            .timer
            .start(slint::TimerMode::Repeated, READ_EVERY, move || {
                if let Some(state) = weak.upgrade() {
                    Self(state).take();
                }
            });
    }

    /// Shows what the reading gave, once it has; a reader gone without a
    /// word failed.
    fn take(&self) {
        let read = {
            let state = self.0.borrow();
            let Some(receiver) = &state.reading else {
                return;
            };
            match receiver.try_recv() {
                Ok(Ok(read)) => Some(read),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "history unreadable: said");
                    None
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    tracing::warn!("history reader gone without a word: said unreadable");
                    None
                }
            }
        };
        let again = {
            let mut state = self.0.borrow_mut();
            state.timer.stop();
            state.reading = None;
            std::mem::take(&mut state.again)
        };
        self.show(read);
        if again {
            self.read();
        }
    }

    /// Shows what a reading `read`, or that it read nothing.
    fn show(&self, read: Option<HistoryRead<KeptLog>>) {
        let mut state = self.0.borrow_mut();
        let Some(window) = state.window.upgrade() else {
            return;
        };
        let history = window.global::<ui::History>();
        let Some(read) = read else {
            state.operations.clear();
            show(&state.rows, Vec::new());
            show(&state.lines, Vec::new());
            history.set_skipped(0);
            history.set_reading(ui::HistoryReading::Unreadable);
            return;
        };
        let chosen = usize::try_from(history.get_chosen())
            .ok()
            .and_then(|index| state.operations.get(index))
            .map(|operation| (operation.row.kind, operation.time));
        let now = (state.seams.now)();
        let (operations, undated) = operations(read.records, now, &state.seams.zone);
        let skipped = read.skipped + undated;
        let index = chosen
            .and_then(|(kind, time)| {
                operations
                    .iter()
                    .position(|operation| operation.row.kind == kind && operation.time == time)
            })
            .unwrap_or(0);
        tracing::debug!(operations = operations.len(), skipped, "history shown");
        show(
            &state.rows,
            operations
                .iter()
                .map(|operation| operation.row.clone())
                .collect(),
        );
        state.operations = operations;
        history.set_skipped(shown(skipped));
        history.set_reading(ui::HistoryReading::Read);
        drop(state);
        self.choose(shown(index));
    }

    /// Shows the operation at `index` in detail.
    fn choose(&self, index: i32) {
        let state = self.0.borrow();
        let Some(window) = state.window.upgrade() else {
            return;
        };
        let Some(operation) = usize::try_from(index)
            .ok()
            .and_then(|index| state.operations.get(index))
        else {
            show(&state.lines, Vec::new());
            return;
        };
        window.global::<ui::History>().set_chosen(index);
        show(
            &state.lines,
            operation
                .log
                .as_ref()
                .map(|log| log.lines().iter().map(line_shown).collect())
                .unwrap_or_default(),
        );
    }
}

/// The operations `records` say, newest first (of two at the same time, the
/// later in the file first), each first of its day marked, and how many were
/// left out for a time no date holds (`dashboard::moment` traces each).
fn operations(
    records: Vec<HistoryRecord<KeptLog>>,
    now: Timestamp,
    zone: &TimeZone,
) -> (Vec<Operation>, usize) {
    let count = records.len();
    let mut operations: Vec<Operation> = records
        .into_iter()
        .rev()
        .filter_map(|record| {
            let (time, row, log) = match record {
                HistoryRecord::Cleaning { line, deletions } => {
                    let (row, log) = cleaning_row(&line, deletions);
                    (line.time, row, log)
                }
                HistoryRecord::Uninstall(line) => (line.time, uninstall_row(&line), None),
                HistoryRecord::Update(line) => (line.time, update_row(&line), None),
            };
            let at = crate::dashboard::moment(time, now, zone)?;
            Some(Operation {
                row: ui::HistoryRow { at, ..row },
                time,
                log,
            })
        })
        .collect();
    let undated = count - operations.len();
    operations.sort_by_key(|operation| Reverse(operation.time));
    let mut days = 0;
    let mut previous: Option<ui::Moment> = None;
    for operation in &mut operations {
        let at = &operation.row.at;
        let first = previous.is_none_or(|previous| !same_day(&previous, at));
        if first {
            days += 1;
        }
        previous = Some(at.clone());
        operation.row.day_first = first;
        operation.row.days_above = days;
    }
    (operations, undated)
}

fn same_day(one: &ui::Moment, other: &ui::Moment) -> bool {
    (one.day, one.day_of_month, one.month, one.year)
        == (other.day, other.day_of_month, other.month, other.year)
}

/// A cleaning's row, and the lines of its deletion log when read.
fn cleaning_row(
    line: &HistoryEntry,
    deletions: Deletions<KeptLog>,
) -> (ui::HistoryRow, Option<ReceiptLines>) {
    let freed = line.freed();
    let (deletion_log, log, unreadable) = match deletions {
        Deletions::NotKept => (ui::DeletionLog::NotKept, None, 0),
        Deletions::Unreadable => (ui::DeletionLog::Unreadable, None, 0),
        Deletions::Read(kept) => (ui::DeletionLog::Read, Some(kept.lines), kept.unreadable),
    };
    if unreadable > 0 {
        tracing::warn!(
            time = line.time,
            unreadable,
            "entries of a deletion log unreadable: counted kept"
        );
    }
    // A reason this version does not know is said as a failure.
    let (admin, admin_problem) = match &line.admin {
        None => (ui::HistoryAdmin::None, ui::RightsProblem::None),
        Some(admin) if admin.done => (ui::HistoryAdmin::Done, ui::RightsProblem::None),
        Some(admin) => (
            ui::HistoryAdmin::NotDone,
            admin
                .reason
                .as_deref()
                .and_then(problem_of_key)
                .unwrap_or(ui::RightsProblem::Failed),
        ),
    };
    let row = ui::HistoryRow {
        kind: ui::HistoryKind::Cleaning,
        freed_known: freed.is_some(),
        freed: size(freed.unwrap_or_default()),
        removed: shown(line.removed),
        partial: shown(line.partial),
        gone: shown(line.gone),
        kept: shown(line.kept),
        stops: model(line.stops.iter().map(|key| stop_of_key(key)).collect()),
        admin,
        admin_problem,
        deletion_log,
        unreadable_entries: shown(unreadable),
        touched_by_operations: line
            .operations
            .iter()
            .any(|operation| operation.end.may_have_removed()),
        operations: model(
            line.operations
                .iter()
                .filter_map(crate::cleaner::docker::operation_shown)
                .collect(),
        ),
        ..ui::HistoryRow::default()
    };
    (row, log)
}

fn uninstall_row(line: &UninstallEntry) -> ui::HistoryRow {
    ui::HistoryRow {
        kind: ui::HistoryKind::Uninstall,
        freed_known: line.freed_bytes.is_some(),
        freed: size(line.freed_bytes.unwrap_or_default()),
        name: line.name.as_str().into(),
        source: source_shown(line.source),
        packages: model(
            line.packages
                .iter()
                .map(|package| SharedString::from(package.as_str()))
                .collect(),
        ),
        data_removed: line.data_removed,
        ..ui::HistoryRow::default()
    }
}

const fn source_shown(source: PackageSource) -> ui::AppSource {
    match source {
        PackageSource::Deb => ui::AppSource::Deb,
        PackageSource::Snap => ui::AppSource::Snap,
        PackageSource::Flatpak => ui::AppSource::Flatpak,
    }
}

/// An installation of updates' row: how it ended, by what became of each
/// update asked (one done by its daemon may have left some waiting), then
/// by its line.
fn update_row(line: &UpdateEntry) -> ui::HistoryRow {
    let installed = line.packages.iter().filter(|package| package.done).count();
    let unknown = line
        .packages
        .iter()
        .any(|package| package.reason.as_deref() == Some(record::UNKNOWN));
    let end = if unknown {
        ui::UpdateEnd::Unknown
    } else if installed == line.packages.len() {
        ui::UpdateEnd::Done
    } else if line.reason.as_deref() == Some(record::CANCELLED) {
        ui::UpdateEnd::Cancelled
    } else if installed > 0 {
        ui::UpdateEnd::Partly
    } else {
        ui::UpdateEnd::Failed
    };
    let updated = line
        .packages
        .iter()
        .map(|package| {
            let state = match package.reason.as_deref() {
                _ if package.done => ui::UpdateState::Installed,
                Some(record::WAITING) => ui::UpdateState::Waiting,
                Some(record::UNKNOWN) => ui::UpdateState::Unknown,
                _ => ui::UpdateState::NotInstalled,
            };
            ui::UpdatedShown {
                name: package.name.as_str().into(),
                version: package.version.as_str().into(),
                state,
                why: why_said(package.reason.as_deref()),
            }
        })
        .collect();
    ui::HistoryRow {
        kind: ui::HistoryKind::Update,
        source: source_shown(line.source),
        update_end: end,
        update_why: why_said(line.reason.as_deref()),
        update_installed: shown(installed),
        update_left: shown(line.packages.len() - installed),
        updated: model(updated),
        restart_required: line.restart_required,
        ..ui::HistoryRow::default()
    }
}

/// The cause a reason of an update's line names, as the History says it;
/// none for a reason that is no cause (cancelled, waiting, unknown), or one
/// this version does not know (traced).
fn why_said(reason: Option<&str>) -> ui::InstallWhy {
    match reason {
        None | Some(record::CANCELLED | record::WAITING | record::UNKNOWN) => ui::InstallWhy::None,
        Some(key) => record::why_of_key(key).map_or_else(
            || {
                tracing::warn!(key, "a reason of an update this version does not know");
                ui::InstallWhy::None
            },
            crate::updates::why_shown,
        ),
    }
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

/// What stopped a cleaning, by the name its line keeps
/// (`okapi_engine::Stop::key`); one this version does not know is said so.
fn stop_of_key(key: &str) -> ui::HistoryStop {
    match key {
        "unsupportedSystem" => ui::HistoryStop::UnsupportedSystem,
        "missingVariable" => ui::HistoryStop::MissingVariable,
        "unreadableVariable" => ui::HistoryStop::UnreadableVariable,
        "minAgeBeyondClock" => ui::HistoryStop::MinAgeBeyondClock,
        "cancelled" => ui::HistoryStop::Cancelled,
        _ => {
            tracing::warn!(key, "a stop this version does not know");
            ui::HistoryStop::Unknown
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
