//! The cleaner screen: the categories chosen, the analysis run on a thread
//! and watched, and how it ended. What it shows is worked out in plain Rust
//! ([`Choices`], [`ScanView`], [`ending`]), tested without a window;
//! [`CleanerScreen`] binds it to the window.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use okapi_engine::{
    ByteSize, Cancel, Category, DEFAULT_MIN_AGE, DEFAULT_MIN_AGE_MINUTES, DefaultChoice,
    Exclusions, Progress, ScanIssue, ScanOptions, ScanReport, SizeUnit, Stop, Watch, scan_watched,
    settings_min_age,
};
use okapi_rules::{Os, RuleSet, TemplateVariables};
use okapi_store::Settings;
use slint::{ComponentHandle as _, Model as _, ModelRc, VecModel};

use crate::ui;

/// How often the window looks at the analysis under way.
const WATCH_EVERY: Duration = Duration::from_millis(50);
/// How long closing the window waits for an analysis to stop, and how often
/// it looks again.
const STOP_WAIT: Duration = Duration::from_secs(1);
const STOP_POLL: Duration = Duration::from_millis(10);
/// How long, in looks at the analysis, the screen waits for a cancelled one
/// before giving it up: a read stuck on a dead mount would otherwise keep it
/// waiting for good.
const CANCEL_WAIT: Duration = Duration::from_secs(5);
/// How long after the screen changes a click is taken for the second click
/// of a double click on what it showed before: "Analyser" and "Annuler
/// l'analyse" share a place, so a double click would start and cancel.
const SECOND_CLICK: Duration = Duration::from_millis(500);

/// What a scan is prepared from, read from the machine each time it is
/// needed: the settings may change between two analyses.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub variables: TemplateVariables,
    pub min_age_minutes: Option<u64>,
    pub exclusions: Vec<PathBuf>,
}

/// Why the analysis cannot start, its detail traced where it is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    Variables,
    Rules,
    Settings,
    MinAge,
    Exclusion,
}

/// One analysis to run.
#[derive(Debug)]
pub struct Job {
    pub rules: Arc<RuleSet>,
    pub variables: TemplateVariables,
    pub options: ScanOptions,
}

pub type Read = Rc<dyn Fn() -> Result<Inputs, Problem>>;
pub type Work = Arc<dyn Fn(Job, Cancel, Sender<Progress>) -> ScanReport + Send + Sync>;

/// What the screen reads the machine and scans with: the system's, or a
/// test's, which never touches the machine.
#[derive(Clone)]
pub struct Seams {
    pub read: Read,
    pub work: Work,
}

impl std::fmt::Debug for Seams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Seams").finish_non_exhaustive()
    }
}

impl Seams {
    /// The session's variables, the user's settings, and the real scan.
    pub fn system() -> Self {
        Self {
            read: Rc::new(read_system),
            work: Arc::new(|job, cancel, progress| {
                scan_watched(
                    &job.rules,
                    &job.variables,
                    &job.options,
                    &Watch::new(cancel, Some(progress)),
                )
            }),
        }
    }
}

fn read_system() -> Result<Inputs, Problem> {
    let variables = okapi_platform::template_variables().map_err(|error| {
        tracing::error!(?error, "variables unavailable");
        Problem::Variables
    })?;
    let directory = okapi_platform::config_directory().ok_or_else(|| {
        tracing::error!("no settings directory");
        Problem::Settings
    })?;
    let settings = Settings::load(&directory).map_err(|error| {
        tracing::error!(%error, "settings unusable");
        Problem::Settings
    })?;
    Ok(Inputs {
        variables,
        min_age_minutes: settings.min_age_minutes,
        exclusions: settings.exclusions.into_iter().map(PathBuf::from).collect(),
    })
}

/// The options of an analysis started at `now` over `chosen`: the
/// exclusions resolved at this instant, as each scan needs.
fn options(
    inputs: &Inputs,
    now: SystemTime,
    chosen: BTreeSet<Category>,
) -> Result<ScanOptions, Problem> {
    let min_age = match inputs.min_age_minutes {
        None => DEFAULT_MIN_AGE,
        Some(minutes) => settings_min_age(minutes, now).ok_or_else(|| {
            tracing::warn!(minutes, "settings minimum age beyond the clock");
            Problem::MinAge
        })?,
    };
    let exclusions = Exclusions::new(inputs.exclusions.iter().cloned()).map_err(|error| {
        tracing::warn!(?error, "settings exclusion refused");
        Problem::Exclusion
    })?;
    Ok(ScanOptions::new(now, min_age, exclusions).with_categories(chosen))
}

/// One category offered before the analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Choice {
    category: Category,
    checked: bool,
    to_choose: bool,
}

/// The categories offered, in the order of the rules, and which are checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choices(Vec<Choice>);

impl Choices {
    /// Checked as the rules say (invariant 5); without rules, none is.
    #[must_use]
    pub fn of(rules: Option<&RuleSet>) -> Self {
        Self(
            Category::ALL
                .iter()
                .map(|&category| {
                    let to_choose = rules.is_none_or(|rules| {
                        category.default_choice(rules) == DefaultChoice::ToChoose
                    });
                    Choice {
                        category,
                        checked: !to_choose,
                        to_choose,
                    }
                })
                .collect(),
        )
    }

    pub fn toggle(&mut self, index: usize) {
        if let Some(choice) = self.0.get_mut(index) {
            choice.checked = !choice.checked;
        }
    }

    /// Checks those not left to the user, and only those.
    pub fn check_recommended(&mut self) {
        for choice in &mut self.0 {
            choice.checked = !choice.to_choose;
        }
    }

    #[must_use]
    pub fn is_recommended(&self) -> bool {
        self.0
            .iter()
            .all(|choice| choice.checked != choice.to_choose)
    }

    #[must_use]
    pub fn chosen(&self) -> BTreeSet<Category> {
        self.0
            .iter()
            .filter(|choice| choice.checked)
            .map(|choice| choice.category)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    category: Category,
    /// Its roots planned, and done: a root shared by two categories counts
    /// for both.
    planned: usize,
    done: usize,
    entries: usize,
    bytes: u64,
}

/// The analysis under way, as the progress of the scan tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanView {
    rows: Vec<Row>,
    planned: bool,
    /// The roots of the scan, each once.
    total: usize,
    roots_done: usize,
    found_bytes: u64,
    issues: usize,
    path: Option<PathBuf>,
}

impl ScanView {
    /// The categories `chosen`, in the order of the rules, all waiting.
    #[must_use]
    pub fn new(chosen: &BTreeSet<Category>) -> Self {
        Self {
            rows: Category::ALL
                .iter()
                .filter(|category| chosen.contains(category))
                .map(|&category| Row {
                    category,
                    planned: 0,
                    done: 0,
                    entries: 0,
                    bytes: 0,
                })
                .collect(),
            planned: false,
            total: 0,
            roots_done: 0,
            found_bytes: 0,
            issues: 0,
            path: None,
        }
    }

    pub fn apply(&mut self, progress: Progress) {
        match progress {
            Progress::Planned { roots, total } => {
                self.planned = true;
                self.total = total;
                for row in &mut self.rows {
                    row.planned = roots.get(&row.category).copied().unwrap_or_default();
                }
            }
            Progress::Walking { path, issues, .. } => {
                self.path = Some(path);
                self.issues = issues;
            }
            Progress::RootDone {
                categories,
                entries,
                bytes,
                issues,
            } => {
                self.roots_done += 1;
                // Reached in a root now done: never shown under the next one.
                self.path = None;
                self.found_bytes = self.found_bytes.saturating_add(bytes);
                self.issues = issues;
                for row in self
                    .rows
                    .iter_mut()
                    .filter(|row| categories.contains(&row.category))
                {
                    row.done += 1;
                    row.entries += entries;
                    row.bytes = row.bytes.saturating_add(bytes);
                }
            }
            // A cleaning's, never a scan's.
            Progress::Cleaning { .. } => {}
        }
    }

    /// Whether the scan planned its roots.
    #[must_use]
    pub const fn is_planned(&self) -> bool {
        self.planned
    }

    fn is_done(&self, row: &Row) -> bool {
        self.planned && row.done >= row.planned
    }

    /// The category taken as under way: the first chosen one not done, an
    /// estimate, since a root shared with a category left out is walked
    /// once for both.
    fn running(&self) -> Option<usize> {
        if !self.planned {
            return None;
        }
        self.rows.iter().position(|row| !self.is_done(row))
    }

    #[must_use]
    pub fn state(&self, index: usize) -> ui::RowState {
        match self.rows.get(index) {
            Some(row) if self.is_done(row) => ui::RowState::Done,
            Some(_) if self.running() == Some(index) => ui::RowState::Running,
            _ => ui::RowState::Waiting,
        }
    }

    #[must_use]
    pub fn rows_done(&self) -> usize {
        self.rows.iter().filter(|row| self.is_done(row)).count()
    }

    /// The part of the roots done, from 0 to 1.
    #[must_use]
    pub fn progress(&self) -> f32 {
        ratio(self.roots_done, self.total, self.planned)
    }
}

/// `done` of `total`, from 0 to 1; nothing to do is done once `planned`.
#[expect(
    clippy::cast_precision_loss,
    reason = "a ratio for a progress bar: precision beyond f32 is never shown"
)]
fn ratio(done: usize, total: usize, planned: bool) -> f32 {
    if total == 0 {
        return if planned { 1.0 } else { 0.0 };
    }
    done.min(total) as f32 / total as f32
}

/// How an analysis ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Stopped before reading anything: never "nothing to clean".
    NothingLooked(Vec<Stop>),
    /// Nothing offered, with the scan's notes (roots it could not read...):
    /// never silent about what it could not look at.
    NothingToClean { issues: usize },
    Found {
        groups: usize,
        bytes: u64,
        issues: usize,
    },
    /// No report: the analysis thread died, or never started.
    Failed,
}

/// How a scan that noted `issues` and offers `groups` of `bytes` ended: a
/// stop means nothing was looked at, the other notes are counted; none when
/// it was cancelled, which is no ending: the user goes back to choosing.
#[must_use]
pub fn ending(issues: &[ScanIssue], groups: usize, bytes: u64) -> Option<Outcome> {
    let stops: Vec<Stop> = issues
        .iter()
        .filter_map(|issue| match issue {
            ScanIssue::Stopped(stop) => Some(stop.clone()),
            _ => None,
        })
        .collect();
    if stops.contains(&Stop::Cancelled) {
        return None;
    }
    if !stops.is_empty() {
        return Some(Outcome::NothingLooked(stops));
    }
    // No stop: every issue is a note.
    let issues = issues.len();
    if groups == 0 {
        return Some(Outcome::NothingToClean { issues });
    }
    Some(Outcome::Found {
        groups,
        bytes,
        issues,
    })
}

fn ending_of(report: &ScanReport) -> Option<Outcome> {
    ending(report.issues(), report.groups().len(), report.total_size())
}

/// A count as the window shows it: beyond what it holds, the most it holds,
/// traced.
fn shown<T: Copy + TryInto<i32> + std::fmt::Debug>(value: T) -> i32 {
    value.try_into().unwrap_or_else(|_| {
        tracing::warn!(?value, "beyond what the window shows");
        i32::MAX
    })
}

fn size(bytes: u64) -> ui::Size {
    let size = ByteSize::of(bytes);
    ui::Size {
        whole: shown(size.whole()),
        tenths: size.tenths().map_or(0, shown),
        has_tenths: size.tenths().is_some(),
        unit: match size.unit() {
            SizeUnit::Byte => ui::SizeUnit::Byte,
            SizeUnit::Kilobyte => ui::SizeUnit::Kilobyte,
            SizeUnit::Megabyte => ui::SizeUnit::Megabyte,
            SizeUnit::Gigabyte => ui::SizeUnit::Gigabyte,
            SizeUnit::Terabyte => ui::SizeUnit::Terabyte,
        },
    }
}

const fn kind(category: Category) -> ui::CategoryKind {
    match category {
        Category::System => ui::CategoryKind::System,
        Category::Apps => ui::CategoryKind::Apps,
        Category::Gaming => ui::CategoryKind::Gaming,
        Category::Gpu => ui::CategoryKind::Gpu,
    }
}

const fn problem(problem: Option<Problem>) -> ui::SetupProblem {
    match problem {
        None => ui::SetupProblem::None,
        Some(Problem::Variables) => ui::SetupProblem::Variables,
        Some(Problem::Rules) => ui::SetupProblem::Rules,
        Some(Problem::Settings) => ui::SetupProblem::Settings,
        Some(Problem::MinAge) => ui::SetupProblem::MinAge,
        Some(Problem::Exclusion) => ui::SetupProblem::Exclusion,
    }
}

/// A stop as the window says it; a cancellation is never shown (no ending).
fn stop_shown(stop: &Stop) -> Option<ui::StopShown> {
    let (reason, variable) = match stop {
        Stop::UnsupportedSystem => (ui::StopReason::UnsupportedSystem, ""),
        Stop::MissingVariable { variable } => (ui::StopReason::MissingVariable, variable.name()),
        Stop::UnreadableVariable { variable, .. } => {
            (ui::StopReason::UnreadableVariable, variable.name())
        }
        Stop::MinAgeBeyondClock => (ui::StopReason::MinAgeBeyondClock, ""),
        Stop::Cancelled => return None,
    };
    Some(ui::StopShown {
        reason,
        variable: variable.into(),
    })
}

/// Shows `rows` in `model`, row by row when their number holds.
fn show<T: Clone + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() == rows.len() {
        for (index, row) in rows.into_iter().enumerate() {
            model.set_row_data(index, row);
        }
    } else {
        model.set_vec(rows);
    }
}

/// An analysis under way.
struct Running {
    cancel: Cancel,
    progress: Receiver<Progress>,
    report: Receiver<ScanReport>,
    thread: JoinHandle<()>,
    view: ScanView,
    /// How long it has been waited for since it was cancelled, counted in
    /// looks at it.
    cancelled_for: Option<Duration>,
}

impl Running {
    /// Runs `job` on its own thread, its progress and its report sent on
    /// channels made for it.
    fn start(work: &Work, job: Job, view: ScanView) -> std::io::Result<Self> {
        let cancel = Cancel::new();
        let (progress_sender, progress) = mpsc::channel();
        let (report_sender, report) = mpsc::channel();
        let work = Arc::clone(work);
        let span = tracing::info_span!("analysis");
        let token = cancel.clone();
        let thread = thread::Builder::new()
            .name("okapi-analysis".to_owned())
            .spawn(move || {
                span.in_scope(|| {
                    let report = work(job, token, progress_sender);
                    if report_sender.send(report).is_err() {
                        tracing::info!("report of an analysis given up or closed: unread");
                    }
                });
            })?;
        Ok(Self {
            cancel,
            progress,
            report,
            thread,
            view,
            cancelled_for: None,
        })
    }
}

struct State {
    window: slint::Weak<ui::AppWindow>,
    seams: Seams,
    rules: Option<Arc<RuleSet>>,
    choices: Choices,
    choice_model: Rc<VecModel<ui::CategoryChoice>>,
    row_model: Rc<VecModel<ui::ScanRow>>,
    stop_model: Rc<VecModel<ui::StopShown>>,
    running: Option<Running>,
    timer: Rc<slint::Timer>,
    /// Runs for [`SECOND_CLICK`] after the screen changes.
    settling: slint::Timer,
}

/// The cleaner screen bound to its window.
#[derive(Clone)]
pub struct CleanerScreen(Rc<RefCell<State>>);

impl std::fmt::Debug for CleanerScreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CleanerScreen")
            .finish_non_exhaustive()
    }
}

impl CleanerScreen {
    /// Binds the screen to `window`: the categories as the rules offer them,
    /// the settings read.
    pub fn attach(window: &ui::AppWindow, seams: Seams) -> Self {
        let rules = RuleSet::embedded(Os::Linux)
            .inspect_err(|error| tracing::error!(?error, "embedded rules unreadable"))
            .ok()
            .map(Arc::new);
        let choices = Choices::of(rules.as_deref());
        let state = State {
            window: window.as_weak(),
            seams,
            rules,
            choices,
            choice_model: Rc::new(VecModel::default()),
            row_model: Rc::new(VecModel::default()),
            stop_model: Rc::new(VecModel::default()),
            running: None,
            timer: Rc::new(slint::Timer::default()),
            settling: slint::Timer::default(),
        };
        let cleaner = window.global::<ui::Cleaner>();
        cleaner.set_choices(ModelRc::from(Rc::clone(&state.choice_model)));
        cleaner.set_rows(ModelRc::from(Rc::clone(&state.row_model)));
        cleaner.set_stops(ModelRc::from(Rc::clone(&state.stop_model)));
        let screen = Self(Rc::new(RefCell::new(state)));
        screen.show_choices();
        screen.refresh();

        let weak = Rc::downgrade(&screen.0);
        let on = move |action: fn(&Self)| {
            let weak = weak.clone();
            move || {
                if let Some(state) = weak.upgrade() {
                    action(&Self(state));
                }
            }
        };
        let weak = Rc::downgrade(&screen.0);
        cleaner.on_toggle(move |index| {
            if let Some(state) = weak.upgrade() {
                Self(state).toggle(index);
            }
        });
        cleaner.on_check_recommended(on(Self::check_recommended));
        cleaner.on_analyze(on(Self::analyze));
        cleaner.on_cancel(on(Self::cancel));
        cleaner.on_choose_again(on(Self::back_to_choosing));
        screen
    }

    fn with_window(&self, show: impl FnOnce(&ui::AppWindow, &State)) {
        let state = self.0.borrow();
        if let Some(window) = state.window.upgrade() {
            show(&window, &state);
        }
    }

    fn show_choices(&self) {
        self.with_window(|window, state| {
            let rows = state
                .choices
                .0
                .iter()
                .map(|choice| ui::CategoryChoice {
                    kind: kind(choice.category),
                    checked: choice.checked,
                    to_choose: choice.to_choose,
                })
                .collect();
            show(&state.choice_model, rows);
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_chosen_count(shown(state.choices.chosen().len()));
            cleaner.set_recommended_only(state.choices.is_recommended());
        });
    }

    fn set_step(&self, step: ui::CleanerStep) {
        self.with_window(|window, _| window.global::<ui::Cleaner>().set_step(step));
    }

    /// Reads the settings again, and says whether an analysis could start
    /// with them: on showing the screen, and back to choosing.
    pub fn refresh(&self) {
        let inputs = {
            let state = self.0.borrow();
            if state.rules.is_none() {
                Err(Problem::Rules)
            } else {
                (state.seams.read)()
            }
        };
        let found = inputs.as_ref().err().copied().or_else(|| {
            inputs
                .as_ref()
                .ok()
                .and_then(|inputs| options(inputs, SystemTime::now(), BTreeSet::new()).err())
        });
        let (minutes, exclusions) =
            inputs
                .as_ref()
                .map_or((DEFAULT_MIN_AGE_MINUTES, 0), |inputs| {
                    (
                        inputs.min_age_minutes.unwrap_or(DEFAULT_MIN_AGE_MINUTES),
                        inputs.exclusions.len(),
                    )
                });
        tracing::debug!(problem = ?found, minutes, exclusions, "cleaner settings read");
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_problem(problem(found));
            cleaner.set_min_age_minutes(shown(minutes));
            cleaner.set_exclusion_count(shown(exclusions));
        });
    }

    fn toggle(&self, index: i32) {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        self.0.borrow_mut().choices.toggle(index);
        self.show_choices();
    }

    fn check_recommended(&self) {
        self.0.borrow_mut().choices.check_recommended();
        self.show_choices();
    }

    /// The analysis of `chosen`, its options built now from the settings
    /// read now: they may have changed since the screen was shown.
    fn prepare(&self, chosen: &BTreeSet<Category>) -> Result<Job, Problem> {
        let state = self.0.borrow();
        let rules = state.rules.as_ref().ok_or(Problem::Rules)?;
        let inputs = (state.seams.read)()?;
        Ok(Job {
            rules: Arc::clone(rules),
            options: options(&inputs, SystemTime::now(), chosen.clone())?,
            variables: inputs.variables,
        })
    }

    /// Starts an analysis of the categories checked.
    fn analyze(&self) {
        if self.is_settling("analysis") {
            return;
        }
        let chosen = self.0.borrow().choices.chosen();
        if self.0.borrow().running.is_some() {
            tracing::warn!("analysis asked for while one runs");
            return;
        }
        if chosen.is_empty() {
            tracing::warn!("analysis asked for without a category");
            return;
        }
        let job = match self.prepare(&chosen) {
            Ok(job) => job,
            Err(found) => {
                tracing::warn!(problem = ?found, "analysis cannot start");
                self.with_window(|window, _| {
                    window
                        .global::<ui::Cleaner>()
                        .set_problem(problem(Some(found)));
                });
                return;
            }
        };
        tracing::info!(options = ?job.options, categories = ?chosen, "analysis started");
        let work = Arc::clone(&self.0.borrow().seams.work);
        self.settle();
        let running = match Running::start(&work, job, ScanView::new(&chosen)) {
            Ok(running) => running,
            Err(error) => {
                tracing::error!(%error, "analysis thread not started");
                self.end(Some(Outcome::Failed));
                return;
            }
        };
        self.0.borrow_mut().running = Some(running);
        self.show_view();
        self.set_step(ui::CleanerStep::Planning);
        let timer = Rc::clone(&self.0.borrow().timer);
        let weak = Rc::downgrade(&self.0);
        timer.start(slint::TimerMode::Repeated, WATCH_EVERY, move || {
            if let Some(state) = weak.upgrade() {
                Self(state).watch();
            }
        });
    }

    /// Takes in the progress sent so far, then looks for the report (one
    /// reader: what is sent after this look waits for the next one, and the
    /// ending shows the report's own counts).
    fn watch(&self) {
        let (received, given_up, changed) = {
            let mut state = self.0.borrow_mut();
            let Some(running) = state.running.as_mut() else {
                return;
            };
            let mut changed = false;
            for progress in running.progress.try_iter() {
                running.view.apply(progress);
                changed = true;
            }
            if let Some(waited) = &mut running.cancelled_for {
                *waited += WATCH_EVERY;
            }
            let view = &running.view;
            let given_up = running
                .cancelled_for
                .is_some_and(|waited| waited >= CANCEL_WAIT)
                .then(|| (view.path.clone(), view.roots_done, view.total));
            (running.report.try_recv(), given_up, changed)
        };
        if changed {
            self.show_view();
        }
        match received {
            Ok(report) => {
                tracing::info!(
                    groups = report.groups().len(),
                    bytes = report.total_size(),
                    issues = report.issues().len(),
                    "analysis report received"
                );
                self.end(ending_of(&report));
            }
            Err(TryRecvError::Empty) => {
                if let Some((path, roots_done, roots)) = given_up {
                    // The thread is left to finish alone: its report has no
                    // reader any more.
                    tracing::warn!(
                        ?path,
                        roots_done,
                        roots,
                        looks_for = ?CANCEL_WAIT,
                        "cancelled analysis given up"
                    );
                    self.end(None);
                }
            }
            Err(TryRecvError::Disconnected) => {
                tracing::error!("analysis thread ended without a report");
                self.end(Some(Outcome::Failed));
            }
        }
    }

    fn show_view(&self) {
        self.with_window(|window, state| {
            let Some(running) = &state.running else {
                return;
            };
            let view = &running.view;
            let rows = view
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    let row_state = view.state(index);
                    ui::ScanRow {
                        kind: kind(row.category),
                        state: row_state,
                        progress: ratio(row.done, row.planned, true),
                        entries: shown(row.entries),
                        size: size(row.bytes),
                        path: match (&view.path, row_state) {
                            (Some(path), ui::RowState::Running) => {
                                path.display().to_string().into()
                            }
                            _ => slint::SharedString::default(),
                        },
                    }
                })
                .collect();
            show(&state.row_model, rows);
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_rows_done(shown(view.rows_done()));
            cleaner.set_progress(view.progress());
            cleaner.set_found(size(view.found_bytes));
            cleaner.set_issues(shown(view.issues));
            if view.is_planned() && cleaner.get_step() == ui::CleanerStep::Planning {
                cleaner.set_step(ui::CleanerStep::Scanning);
            }
        });
    }

    /// Ends the analysis: shows how it ended, or goes back to choosing.
    fn end(&self, outcome: Option<Outcome>) {
        let cancel_asked = {
            let mut state = self.0.borrow_mut();
            state.timer.stop();
            state
                .running
                .take()
                .is_some_and(|running| running.cancel.is_cancelled())
        };
        let Some(outcome) = outcome else {
            tracing::info!("analysis cancelled: back to choosing");
            self.back_to_choosing();
            return;
        };
        // A cancellation that came after the scan ended shows its report.
        tracing::info!(?outcome, cancel_asked, "analysis ended");
        self.with_window(|window, state| {
            let cleaner = window.global::<ui::Cleaner>();
            let shown_kind = match &outcome {
                Outcome::NothingLooked(stops) => {
                    show(
                        &state.stop_model,
                        stops.iter().filter_map(stop_shown).collect(),
                    );
                    ui::OutcomeKind::NothingLooked
                }
                Outcome::NothingToClean { issues } => {
                    cleaner.set_issues(shown(*issues));
                    ui::OutcomeKind::NothingToClean
                }
                Outcome::Found {
                    groups,
                    bytes,
                    issues,
                } => {
                    cleaner.set_groups(shown(*groups));
                    cleaner.set_found(size(*bytes));
                    cleaner.set_issues(shown(*issues));
                    ui::OutcomeKind::Found
                }
                Outcome::Failed => ui::OutcomeKind::Failed,
            };
            cleaner.set_outcome(shown_kind);
            cleaner.set_step(ui::CleanerStep::Outcome);
        });
    }

    fn cancel(&self) {
        if self.is_settling("cancellation") {
            return;
        }
        {
            let mut state = self.0.borrow_mut();
            let Some(running) = state.running.as_mut() else {
                return;
            };
            if running.cancelled_for.is_some() {
                return;
            }
            tracing::info!("analysis cancelled by the user");
            running.cancel.cancel();
            running.cancelled_for = Some(Duration::ZERO);
        }
        self.set_step(ui::CleanerStep::Cancelling);
    }

    /// Back to the categories, the settings read again.
    /// Starts the time during which a click belongs to the screen before.
    fn settle(&self) {
        self.0
            .borrow()
            .settling
            .start(slint::TimerMode::SingleShot, SECOND_CLICK, || {});
    }

    /// Whether the screen just changed: `asked` for is then the second click
    /// of a double click, and ignored.
    fn is_settling(&self, asked: &str) -> bool {
        let settling = self.0.borrow().settling.running();
        if settling {
            tracing::debug!(asked, "second click of a double click ignored");
        }
        settling
    }

    fn back_to_choosing(&self) {
        self.settle();
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_back_from_analysis(true);
            cleaner.set_step(ui::CleanerStep::Idle);
        });
        self.refresh();
    }

    /// Cancels an analysis under way and waits for it a little: the window
    /// is closing.
    pub fn shut_down(&self) {
        let Some(running) = self.0.borrow_mut().running.take() else {
            return;
        };
        running.cancel.cancel();
        let started = Instant::now();
        while !running.thread.is_finished() && started.elapsed() < STOP_WAIT {
            thread::sleep(STOP_POLL);
        }
        if running.thread.is_finished() {
            tracing::info!(waited = ?started.elapsed(), "analysis stopped on closing");
        } else {
            tracing::warn!(
                waited = ?started.elapsed(),
                path = ?running.view.path,
                "analysis left running on closing"
            );
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use i_slint_backend_testing::{AccessibleRole, ElementHandle};
    use okapi_rules::Variable;

    use super::*;

    fn defaults() -> Inputs {
        Inputs {
            variables: TemplateVariables::new(),
            min_age_minutes: None,
            exclusions: Vec::new(),
        }
    }

    /// Seams reading the defaults, whose analysis, if asked for, stops
    /// before reading (no variable): nothing of the machine is read.
    pub fn seams_never_scanning() -> Seams {
        Seams {
            read: Rc::new(|| Ok(defaults())),
            work: Arc::new(|job, cancel, _| {
                scan_watched(
                    &job.rules,
                    &job.variables,
                    &job.options,
                    &Watch::new(cancel, None),
                )
            }),
        }
    }

    /// Every variable set, to paths nobody reads: a scan cancelled
    /// beforehand stops before any.
    fn every_variable() -> TemplateVariables {
        let mut variables = TemplateVariables::new();
        for variable in Variable::ALL {
            variables
                .insert(variable, PathBuf::from("/okapi-test").join(variable.name()))
                .unwrap();
        }
        variables
    }

    fn embedded() -> RuleSet {
        RuleSet::embedded(Os::Linux).unwrap()
    }

    #[test]
    fn choices_follow_the_rules_and_leave_resets_to_the_user() {
        let rules = embedded();
        let mut choices = Choices::of(Some(&rules));
        let all_but_gpu = BTreeSet::from([Category::System, Category::Apps, Category::Gaming]);
        assert_eq!(choices.chosen(), all_but_gpu);
        assert!(choices.is_recommended());
        choices.toggle(0);
        assert!(!choices.is_recommended(), "a recommended one left out");
        choices.toggle(3);
        assert!(choices.chosen().contains(&Category::Gpu));
        assert!(!choices.is_recommended());
        choices.check_recommended();
        assert_eq!(
            choices.chosen(),
            all_but_gpu,
            "the GPU caches left out again"
        );
        choices.toggle(Category::ALL.len());
        assert_eq!(choices.chosen(), all_but_gpu, "beyond the list: nothing");
        assert!(
            Choices::of(None).chosen().is_empty(),
            "no rules, nothing checked"
        );
    }

    #[test]
    fn the_view_follows_the_roots_of_the_categories_chosen() {
        let mut view = ScanView::new(&BTreeSet::from([
            Category::Gaming,
            Category::System,
            Category::Apps,
        ]));
        assert_eq!(view.state(0), ui::RowState::Waiting, "nothing planned yet");
        assert!((view.progress() - 0.0).abs() < f32::EPSILON);
        view.apply(Progress::Planned {
            roots: BTreeMap::from([
                (Category::System, 2),
                (Category::Apps, 1),
                (Category::Gpu, 1),
            ]),
            total: 3,
        });
        let states = |view: &ScanView| (0..3).map(|index| view.state(index)).collect::<Vec<_>>();
        assert_eq!(
            states(&view),
            [
                ui::RowState::Running,
                ui::RowState::Waiting,
                ui::RowState::Done
            ],
            "in the order of the rules; a category without root is done at once"
        );
        view.apply(Progress::Walking {
            path: PathBuf::from("/data/x"),
            entries_seen: 1024,
            found_bytes: 1,
            issues: 2,
        });
        assert_eq!(view.path.as_deref(), Some(std::path::Path::new("/data/x")));
        assert_eq!(view.issues, 2);
        let done = |categories: &[Category], entries, bytes| Progress::RootDone {
            categories: categories.iter().copied().collect(),
            entries,
            bytes,
            issues: 3,
        };
        view.apply(done(&[Category::System], 2, 10));
        assert_eq!(view.path, None, "reached in a root now done");
        assert_eq!(states(&view)[0], ui::RowState::Running, "one root of two");
        assert!(
            (view.progress() - 1.0 / 3.0).abs() < f32::EPSILON,
            "a third"
        );
        // A root shared with a category left out counts once in all.
        view.apply(done(&[Category::Apps, Category::Gpu], 1, 5));
        assert_eq!(
            states(&view),
            [
                ui::RowState::Running,
                ui::RowState::Done,
                ui::RowState::Done
            ]
        );
        assert_eq!(view.rows_done(), 2);
        view.apply(done(&[Category::System], 3, 7));
        assert_eq!(view.rows_done(), 3);
        assert!((view.progress() - 1.0).abs() < f32::EPSILON);
        assert_eq!(view.found_bytes, 22, "the sum over roots");
        assert_eq!((view.rows[0].entries, view.rows[0].bytes), (5, 17));
        assert_eq!((view.rows[1].entries, view.rows[1].bytes), (1, 5));
        assert_eq!(view.issues, 3);
    }

    #[test]
    fn the_category_under_way_is_the_first_not_done() {
        let mut view = ScanView::new(&BTreeSet::from([
            Category::System,
            Category::Apps,
            Category::Gaming,
        ]));
        view.apply(Progress::Planned {
            roots: BTreeMap::from([
                (Category::System, 1),
                (Category::Apps, 3),
                (Category::Gaming, 1),
            ]),
            total: 5,
        });
        view.apply(Progress::RootDone {
            categories: BTreeSet::from([Category::System]),
            entries: 1,
            bytes: 1,
            issues: 0,
        });
        assert_eq!(
            (0..3).map(|index| view.state(index)).collect::<Vec<_>>(),
            [
                ui::RowState::Done,
                ui::RowState::Running,
                ui::RowState::Waiting
            ]
        );
        assert!((view.progress() - 0.2).abs() < f32::EPSILON);
    }

    #[test]
    fn every_stop_is_said_with_its_variable() {
        let home = Variable::ALL[0];
        let other = Variable::ALL[1];
        let said = [
            Stop::UnsupportedSystem,
            Stop::MissingVariable { variable: home },
            Stop::UnreadableVariable {
                variable: other,
                path: PathBuf::from("/x"),
                error: std::io::ErrorKind::PermissionDenied,
            },
            Stop::MinAgeBeyondClock,
        ]
        .iter()
        .map(|stop| stop_shown(stop).map(|shown| (shown.reason, shown.variable.to_string())))
        .collect::<Vec<_>>();
        assert_eq!(
            said,
            [
                Some((ui::StopReason::UnsupportedSystem, String::new())),
                Some((ui::StopReason::MissingVariable, home.name().to_owned())),
                Some((ui::StopReason::UnreadableVariable, other.name().to_owned())),
                Some((ui::StopReason::MinAgeBeyondClock, String::new())),
            ]
        );
        assert_eq!(stop_shown(&Stop::Cancelled), None, "never an ending");
    }

    #[test]
    fn the_options_take_the_settings_read() {
        let now = SystemTime::now();
        let exclusion = PathBuf::from("/okapi-test/keep");
        let inputs = Inputs {
            min_age_minutes: Some(7),
            exclusions: vec![exclusion.clone()],
            ..defaults()
        };
        let chosen = BTreeSet::from([Category::Gaming]);
        assert_eq!(
            options(&inputs, now, chosen.clone()),
            Ok(ScanOptions::new(
                now,
                Duration::from_mins(7),
                Exclusions::new([exclusion]).unwrap()
            )
            .with_categories(chosen.clone()))
        );
        let refused = Inputs {
            exclusions: vec![PathBuf::from("relative")],
            ..defaults()
        };
        assert_eq!(options(&refused, now, chosen), Err(Problem::Exclusion));
    }

    #[test]
    fn a_scan_that_looked_at_nothing_never_ends_as_nothing_to_clean() {
        let missing = Stop::MissingVariable {
            variable: Variable::ALL[0],
        };
        let excluded = |path: &str| ScanIssue::Excluded {
            origins: Vec::new(),
            path: PathBuf::from(path),
        };
        assert_eq!(
            ending(
                &[
                    ScanIssue::Stopped(missing.clone()),
                    ScanIssue::Stopped(Stop::Cancelled)
                ],
                0,
                0
            ),
            None,
            "cancelled: no ending"
        );
        assert_eq!(
            ending(
                &[
                    ScanIssue::Stopped(Stop::Cancelled),
                    ScanIssue::Stopped(missing.clone())
                ],
                0,
                0
            ),
            None,
            "cancelled, wherever it is said"
        );
        assert_eq!(
            ending(&[excluded("/a"), ScanIssue::Stopped(missing.clone())], 0, 0),
            Some(Outcome::NothingLooked(vec![missing]))
        );
        assert_eq!(
            ending(&[excluded("/a"), excluded("/b")], 0, 0),
            Some(Outcome::NothingToClean { issues: 2 }),
            "with what it could not look at"
        );
        assert_eq!(
            ending(&[excluded("/a")], 2, 7),
            Some(Outcome::Found {
                groups: 2,
                bytes: 7,
                issues: 1
            })
        );
    }

    #[test]
    fn sizes_and_counts_reach_the_window_within_its_bounds() {
        let shown_size = size(1_999_999);
        assert_eq!(
            (shown_size.whole, shown_size.tenths, shown_size.has_tenths),
            (1, 9, true)
        );
        assert_eq!(shown_size.unit, ui::SizeUnit::Megabyte);
        assert!(!size(999).has_tenths);
        assert_eq!(shown(usize::MAX), i32::MAX);
        assert_eq!(shown(7_usize), 7);
    }

    fn checkbox(window: &ui::AppWindow, label: &str) -> ElementHandle {
        ElementHandle::find_by_accessible_label(window, label)
            .find(|element| element.accessible_role() == Some(AccessibleRole::Checkbox))
            .unwrap_or_else(|| panic!("no checkbox {label}"))
    }

    pub fn button(window: &ui::AppWindow, label: &str) -> ElementHandle {
        ElementHandle::find_by_accessible_label(window, label)
            .find(|element| element.accessible_role() == Some(AccessibleRole::Button))
            .unwrap_or_else(|| panic!("no button {label}"))
    }

    /// Images a screen reader would read without a name.
    pub fn unnamed_images(window: &ui::AppWindow) -> usize {
        ElementHandle::find_by_element_type_name(window, "Image")
            .filter(|image| {
                image.accessible_role() == Some(AccessibleRole::Image)
                    && image
                        .accessible_label()
                        .is_none_or(|label| label.is_empty())
            })
            .count()
    }

    pub fn shows(window: &ui::AppWindow, text: &str) -> bool {
        ElementHandle::find_by_accessible_label(window, text)
            .next()
            .is_some()
    }

    fn shows_a_text_starting(window: &ui::AppWindow, start: &str) -> bool {
        ElementHandle::find_by_element_type_name(window, "Text").any(|text| {
            text.accessible_label()
                .is_some_and(|label| label.starts_with(start))
        })
    }

    /// The window on the cleaner, in French, reading and scanning with
    /// `seams`.
    fn on_cleaner(seams: Seams) -> (ui::AppWindow, CleanerScreen) {
        i_slint_backend_testing::init_no_event_loop();
        let (window, screen) = crate::window(seams).unwrap();
        slint::select_bundled_translation("").unwrap();
        button(&window, "Nettoyeur").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        (window, screen)
    }

    fn step(window: &ui::AppWindow) -> ui::CleanerStep {
        window.global::<ui::Cleaner>().get_step()
    }

    #[test]
    fn the_categories_offered_leave_cache_resets_unchecked() {
        let (window, _screen) = on_cleaner(seams_never_scanning());
        for label in ["Système", "Applications", "Jeux"] {
            assert_eq!(
                checkbox(&window, label).accessible_checked(),
                Some(true),
                "{label}"
            );
        }
        let gpu = checkbox(&window, "Caches GPU");
        assert_eq!(gpu.accessible_checked(), Some(false), "invariant 5");
        assert!(
            gpu.accessible_description()
                .is_some_and(|description| description.starts_with("À choisir")),
            "the badge is read"
        );
        assert!(shows(&window, "Catégories sélectionnées : 3 sur 4"));
        assert!(shows(
            &window,
            "Toujours épargnés : fichiers modifiés depuis moins de 60 min. Exclusions : 0."
        ));
        let recommended = || button(&window, "Cocher les recommandées").accessible_enabled();
        assert_eq!(recommended(), Some(false), "already the case");
        gpu.invoke_accessible_default_action();
        assert_eq!(gpu.accessible_checked(), Some(true));
        assert_eq!(recommended(), Some(true));
        button(&window, "Cocher les recommandées").invoke_accessible_default_action();
        assert_eq!(
            gpu.accessible_checked(),
            Some(false),
            "never checked for the user"
        );
        assert_eq!(
            checkbox(&window, "Système").accessible_checked(),
            Some(true)
        );

        for label in ["Système", "Applications", "Jeux"] {
            checkbox(&window, label).invoke_accessible_default_action();
        }
        assert!(shows(&window, "Aucune catégorie sélectionnée"));
        assert_eq!(
            button(&window, "Analyser").accessible_enabled(),
            Some(false)
        );
        // Rust refuses too: no analysis without a category.
        window.global::<ui::Cleaner>().invoke_analyze();
        assert_eq!(step(&window), ui::CleanerStep::Idle);

        assert_eq!(unnamed_images(&window), 0);
    }

    #[test]
    fn unreadable_settings_keep_the_analysis_from_starting() {
        let (window, _screen) = on_cleaner(Seams {
            read: Rc::new(|| Err(Problem::Settings)),
            ..seams_never_scanning()
        });
        assert!(shows(
            &window,
            "Les réglages sont illisibles : l'analyse ne peut pas démarrer. Le détail est dans le journal."
        ));
        assert_eq!(
            button(&window, "Analyser").accessible_enabled(),
            Some(false)
        );
        assert!(!shows_a_text_starting(&window, "Toujours épargnés"));
        window.global::<ui::Cleaner>().invoke_analyze();
        assert_eq!(step(&window), ui::CleanerStep::Idle);
    }

    #[test]
    fn the_settings_are_read_again_on_coming_back() {
        let minutes = Rc::new(Cell::new(Some(5)));
        let read_minutes = Rc::clone(&minutes);
        let (window, _screen) = on_cleaner(Seams {
            read: Rc::new(move || {
                Ok(Inputs {
                    min_age_minutes: read_minutes.get(),
                    exclusions: vec![PathBuf::from("/okapi-test/a"), PathBuf::from("*.log")],
                    ..defaults()
                })
            }),
            ..seams_never_scanning()
        });
        assert!(shows(
            &window,
            "Toujours épargnés : fichiers modifiés depuis moins de 5 min. Exclusions : 2."
        ));
        minutes.set(Some(u64::MAX));
        button(&window, "Accueil").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        button(&window, "Nettoyeur").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::MinAge,
            "an age beyond the clock is the settings' fault"
        );
        assert!(
            !shows_a_text_starting(&window, "Toujours épargnés"),
            "nothing said of settings that cannot be used"
        );
    }

    /// Lets the window look at the analysis, until `done` or a bound: the
    /// thread sends its report when it likes.
    fn watch_until(window: &ui::AppWindow, done: impl Fn(&ui::AppWindow) -> bool) {
        for _ in 0..200 {
            i_slint_backend_testing::mock_elapsed_time(WATCH_EVERY);
            slint::platform::update_timers_and_animations();
            if done(window) {
                return;
            }
            thread::sleep(STOP_POLL);
        }
        panic!("the window never got there: still at {:?}", step(window));
    }

    /// Lets the time of a double click pass after the screen changed.
    fn let_the_screen_settle() {
        i_slint_backend_testing::mock_elapsed_time(SECOND_CLICK);
        slint::platform::update_timers_and_animations();
    }

    #[test]
    fn a_double_click_never_crosses_a_change_of_screen() {
        let (seams, scan) = scripted(Vec::new());
        let (window, _screen) = on_cleaner(Seams {
            read: Rc::new(|| {
                Ok(Inputs {
                    variables: every_variable(),
                    ..defaults()
                })
            }),
            ..seams
        });
        // On "Analyser": its second click lands on "Annuler l'analyse".
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        button(&window, "Annuler l'analyse").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Planning, "not cancelled");
        let_the_screen_settle();
        // On "Annuler l'analyse": its second click lands on "Analyser".
        button(&window, "Annuler l'analyse").invoke_accessible_default_action();
        scan.go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Idle);
        button(&window, "Analyser").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Idle, "not started again");
        let_the_screen_settle();
        button(&window, "Analyser").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Planning);
        scan.go.send(()).unwrap();
    }

    /// A scan run by the test: it says it started (with its options), then
    /// waits for each go to send the next progress, and at the last one ends
    /// as the real scan does with the variables read.
    struct Scripted {
        go: Sender<()>,
        started: Receiver<ScanOptions>,
        sent: Receiver<()>,
    }

    fn scripted(script: Vec<Progress>) -> (Seams, Scripted) {
        let (go, gone) = mpsc::channel();
        let (start, started) = mpsc::channel();
        let (said, sent) = mpsc::channel();
        let shared = Mutex::new((gone, start, said));
        let work: Work = Arc::new(move |job, cancel, progress| {
            let channels = shared.lock().unwrap();
            let (gone, start, said) = &*channels;
            start.send(job.options.clone()).unwrap();
            for step in script.clone() {
                gone.recv().unwrap();
                progress.send(step).unwrap();
                said.send(()).unwrap();
            }
            gone.recv().unwrap();
            let report = scan_watched(
                &job.rules,
                &job.variables,
                &job.options,
                &Watch::new(cancel, None),
            );
            // Its report is next; a test that gave the scan up may be gone.
            if said.send(()).is_err() {
                tracing::debug!("scripted scan ended after its test");
            }
            report
        });
        (
            Seams {
                read: Rc::new(|| Ok(defaults())),
                work,
            },
            Scripted { go, started, sent },
        )
    }

    /// `seams`, its reads of the settings counted.
    fn counting_reads(seams: Seams) -> (Seams, Rc<Cell<usize>>) {
        let reads = Rc::new(Cell::new(0));
        let counted = Rc::clone(&reads);
        let read = Rc::clone(&seams.read);
        let seams = Seams {
            read: Rc::new(move || {
                counted.set(counted.get() + 1);
                read()
            }),
            ..seams
        };
        (seams, reads)
    }

    /// The entry each row shows as reached.
    fn paths(window: &ui::AppWindow) -> Vec<String> {
        window
            .global::<ui::Cleaner>()
            .get_rows()
            .iter()
            .map(|row| row.path.to_string())
            .collect()
    }

    fn rows(window: &ui::AppWindow) -> Vec<(ui::CategoryKind, ui::RowState)> {
        window
            .global::<ui::Cleaner>()
            .get_rows()
            .iter()
            .map(|row| (row.kind, row.state))
            .collect()
    }

    #[test]
    fn an_analysis_shows_its_categories_then_how_it_ended() {
        let (seams, scan) = scripted(vec![
            Progress::Planned {
                roots: BTreeMap::from([(Category::System, 1), (Category::Gpu, 1)]),
                total: 1,
            },
            Progress::Walking {
                path: PathBuf::from("/okapi-test/walked"),
                entries_seen: 1024,
                found_bytes: 0,
                issues: 0,
            },
            Progress::RootDone {
                categories: BTreeSet::from([Category::System]),
                entries: 4,
                bytes: 1_500,
                issues: 1,
            },
        ]);
        let (seams, reads) = counting_reads(seams);
        let (window, _screen) = on_cleaner(seams);
        let before = reads.get();
        button(&window, "Analyser").invoke_accessible_default_action();
        let options = scan.started.recv().unwrap();
        assert_eq!(reads.get(), before + 1, "the settings read at the click");
        assert_eq!(
            *options.categories(),
            BTreeSet::from([Category::System, Category::Apps, Category::Gaming])
        );
        assert_eq!(step(&window), ui::CleanerStep::Planning);
        assert_eq!(
            rows(&window),
            [
                (ui::CategoryKind::System, ui::RowState::Waiting),
                (ui::CategoryKind::Apps, ui::RowState::Waiting),
                (ui::CategoryKind::Gaming, ui::RowState::Waiting),
            ]
        );

        scan.go.send(()).unwrap();
        scan.sent.recv().unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Scanning);
        assert_eq!(
            rows(&window),
            [
                (ui::CategoryKind::System, ui::RowState::Running),
                (ui::CategoryKind::Apps, ui::RowState::Done),
                (ui::CategoryKind::Gaming, ui::RowState::Done),
            ]
        );
        assert!(shows(
            &window,
            "Catégories terminées : 2 sur 3. Rien n'est supprimé pendant l'analyse."
        ));
        assert!(window.global::<ui::Cleaner>().get_progress().abs() < f32::EPSILON);

        scan.go.send(()).unwrap();
        scan.sent.recv().unwrap();
        watch_until(&window, |window| {
            !window
                .global::<ui::Cleaner>()
                .get_rows()
                .row_data(0)
                .unwrap()
                .path
                .is_empty()
        });
        assert_eq!(
            paths(&window),
            ["/okapi-test/walked", "", ""],
            "on the category under way only"
        );

        scan.go.send(()).unwrap();
        scan.sent.recv().unwrap();
        watch_until(&window, |window| {
            window.global::<ui::Cleaner>().get_rows_done() == 3
        });
        assert!(shows(&window, "Remarques de l'analyse : 1"));
        assert!(shows(&window, "1,5 ko"), "found so far");
        let system = ElementHandle::find_by_accessible_label(&window, "Système")
            .find(|element| element.accessible_role() == Some(AccessibleRole::ListItem))
            .unwrap();
        assert_eq!(
            system.accessible_description().as_deref(),
            Some("Terminé : éléments : 4, 1,5 ko")
        );

        // The variables read are empty: the real scan stops before reading.
        scan.go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        assert_eq!(
            window.global::<ui::Cleaner>().get_outcome(),
            ui::OutcomeKind::NothingLooked
        );
        assert!(shows(&window, "Rien n'a été regardé"));
        assert!(shows(
            &window,
            "La variable HOME n'est pas définie : ce qu'elle protège ne peut pas être garanti."
        ));
        button(&window, "Choisir d'autres catégories").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Idle);
    }

    #[test]
    fn a_cancelled_analysis_goes_back_to_choosing() {
        let (seams, scan) = scripted(Vec::new());
        let (window, _screen) = on_cleaner(Seams {
            read: Rc::new(|| {
                Ok(Inputs {
                    variables: every_variable(),
                    ..defaults()
                })
            }),
            ..seams
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        // The focus went to the cancel button: Enter cancels.
        let_the_screen_settle();
        press(&window, slint::platform::Key::Return);
        assert_eq!(step(&window), ui::CleanerStep::Cancelling);
        assert_eq!(
            button(&window, "Annuler l'analyse").accessible_enabled(),
            Some(false),
            "asked once"
        );
        assert!(shows(&window, "Annulation de l'analyse"));
        // The token reached the scan: cancelled before reading anything.
        scan.go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Idle);
        assert_eq!(
            checkbox(&window, "Système").accessible_checked(),
            Some(true)
        );
        // The focus is back on the first category.
        press(&window, slint::platform::Key::Space);
        assert_eq!(
            checkbox(&window, "Système").accessible_checked(),
            Some(false)
        );
        // A held key repeats: it toggles once.
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressRepeated {
                text: slint::platform::Key::Space.into(),
            });
        assert_eq!(
            checkbox(&window, "Système").accessible_checked(),
            Some(false)
        );
        // Once only: coming back from the sidebar leaves the focus alone.
        button(&window, "Accueil").invoke_accessible_default_action();
        button(&window, "Nettoyeur").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(!window.global::<ui::Cleaner>().get_back_from_analysis());
    }

    #[test]
    fn a_cancellation_after_the_scan_ended_shows_its_report() {
        // The variables read are empty: the scan stops before it could see
        // the token.
        let (seams, scan) = scripted(Vec::new());
        let (window, _screen) = on_cleaner(seams);
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        let_the_screen_settle();
        button(&window, "Annuler l'analyse").invoke_accessible_default_action();
        scan.go.send(()).unwrap();
        scan.sent.recv().unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        assert!(shows(&window, "Rien n'a été regardé"));
    }

    #[test]
    fn a_cancelled_analysis_that_never_stops_is_given_up() {
        let (seams, scan) = scripted(Vec::new());
        let (window, _screen) = on_cleaner(seams);
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        let_the_screen_settle();
        button(&window, "Annuler l'analyse").invoke_accessible_default_action();
        // The scan waits for a go that only comes after.
        let looks = usize::try_from(CANCEL_WAIT.as_millis() / WATCH_EVERY.as_millis()).unwrap();
        for _ in 1..looks {
            i_slint_backend_testing::mock_elapsed_time(WATCH_EVERY);
            slint::platform::update_timers_and_animations();
        }
        assert_eq!(step(&window), ui::CleanerStep::Cancelling, "not before");
        watch_until(&window, |window| step(window) == ui::CleanerStep::Idle);
        scan.go.send(()).unwrap();
    }

    #[test]
    fn the_endings_say_what_was_found_and_noted() {
        let (window, screen) = on_cleaner(seams_never_scanning());
        screen.end(Some(Outcome::Found {
            groups: 2,
            bytes: 2_340_000,
            issues: 3,
        }));
        assert_eq!(step(&window), ui::CleanerStep::Outcome);
        assert!(shows(&window, "Analyse terminée"));
        assert!(shows(
            &window,
            "Groupes trouvés : 2. Taille récupérable : 2,3 Mo."
        ));
        assert!(shows(
            &window,
            "Remarques de l'analyse : 3. Le détail est dans le journal."
        ));
        screen.end(Some(Outcome::NothingToClean { issues: 2 }));
        assert!(shows(&window, "Rien à nettoyer"));
        assert!(shows(
            &window,
            "Remarques de l'analyse : 2. Le détail est dans le journal."
        ));
        assert!(shows_a_text_starting(
            &window,
            "Okapi n'a rien trouvé de récupérable dans ce qu'il a pu lire."
        ));
        screen.end(Some(Outcome::Found {
            groups: 1,
            bytes: 1,
            issues: 0,
        }));
        assert!(!shows_a_text_starting(&window, "Remarques de l'analyse"));
    }

    #[test]
    fn a_new_analysis_refused_says_why_on_the_ending() {
        let refused = Rc::new(Cell::new(false));
        let read_refused = Rc::clone(&refused);
        let (window, screen) = on_cleaner(Seams {
            read: Rc::new(move || {
                Ok(Inputs {
                    exclusions: if read_refused.get() {
                        vec![PathBuf::from("relative")]
                    } else {
                        Vec::new()
                    },
                    ..defaults()
                })
            }),
            ..seams_never_scanning()
        });
        screen.end(Some(Outcome::NothingToClean { issues: 0 }));
        refused.set(true);
        button(&window, "Nouvelle analyse").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Outcome);
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::Exclusion
        );
        assert!(shows(
            &window,
            "Une exclusion des réglages est refusée : l'analyse ne peut pas démarrer. Le détail est dans le journal."
        ));
        // Shown again with the analysis refused, the focus goes to the
        // button that still works.
        button(&window, "Accueil").invoke_accessible_default_action();
        button(&window, "Nettoyeur").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        press(&window, slint::platform::Key::Return);
        assert_eq!(step(&window), ui::CleanerStep::Idle);
        assert!(shows(
            &window,
            "Une exclusion des réglages est refusée : l'analyse ne peut pas démarrer. Le détail est dans le journal."
        ));
    }

    #[test]
    fn an_analysis_whose_thread_dies_says_it_failed() {
        let (window, _screen) = on_cleaner(Seams {
            work: Arc::new(|_, _, _| panic!("the analysis thread dies, as a test asks")),
            ..seams_never_scanning()
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        assert_eq!(
            window.global::<ui::Cleaner>().get_outcome(),
            ui::OutcomeKind::Failed
        );
        assert!(shows(&window, "L'analyse a échoué"));
        // The focus went to a new analysis: Enter starts one.
        let_the_screen_settle();
        press(&window, slint::platform::Key::Return);
        assert_eq!(step(&window), ui::CleanerStep::Planning);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
    }

    #[test]
    fn closing_during_an_analysis_cancels_it() {
        let (saw, seen) = mpsc::channel();
        let saw = Mutex::new(saw);
        let (window, screen) = on_cleaner(Seams {
            work: Arc::new(move |job, cancel, _| {
                let started = Instant::now();
                while !cancel.is_cancelled() && started.elapsed() < STOP_WAIT * 5 {
                    thread::sleep(STOP_POLL);
                }
                saw.lock().unwrap().send(cancel.is_cancelled()).unwrap();
                scan_watched(
                    &job.rules,
                    &job.variables,
                    &job.options,
                    &Watch::new(cancel, None),
                )
            }),
            ..seams_never_scanning()
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        screen.shut_down();
        assert_eq!(seen.try_recv(), Ok(true), "cancelled, and waited for");
    }

    #[test]
    fn closing_waits_a_bounded_time_for_an_analysis_that_never_stops() {
        let (seams, scan) = scripted(Vec::new());
        let (window, screen) = on_cleaner(seams);
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        let started = Instant::now();
        screen.shut_down();
        assert!(started.elapsed() < STOP_WAIT * 3, "{:?}", started.elapsed());
        scan.go.send(()).unwrap();
    }

    pub fn press(window: &ui::AppWindow, key: slint::platform::Key) {
        let text = slint::SharedString::from(key);
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
    }

    #[test]
    fn the_categories_are_chosen_with_the_keyboard() {
        use slint::platform::Key;

        let (window, _screen) = on_cleaner(seams_never_scanning());
        // The three sidebar entries, then the first category.
        for _ in 0..4 {
            press(&window, Key::Tab);
        }
        press(&window, Key::Space);
        assert_eq!(
            checkbox(&window, "Système").accessible_checked(),
            Some(false)
        );
        press(&window, Key::Return);
        assert_eq!(
            checkbox(&window, "Système").accessible_checked(),
            Some(false),
            "a checkbox takes Space only"
        );
        // The other three categories, then the recommended ones.
        for _ in 0..4 {
            press(&window, Key::Tab);
        }
        press(&window, Key::Return);
        assert_eq!(
            checkbox(&window, "Système").accessible_checked(),
            Some(true)
        );
    }

    #[test]
    fn a_disabled_button_is_skipped_by_the_keyboard() {
        use slint::platform::Key;

        let (seams, scan) = scripted(Vec::new());
        let (window, _screen) = on_cleaner(seams);
        // The sidebar, the four categories, then past the disabled
        // "Cocher les recommandées" to "Analyser".
        for _ in 0..8 {
            press(&window, Key::Tab);
        }
        press(&window, Key::Return);
        assert_eq!(step(&window), ui::CleanerStep::Planning);
        let_the_screen_settle();
        // Enter held: its repeats reach the cancel button, now focused.
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressRepeated {
                text: Key::Return.into(),
            });
        assert_eq!(step(&window), ui::CleanerStep::Planning, "not cancelled");
        scan.go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
    }

    #[test]
    fn the_progress_is_a_named_live_region() {
        let (seams, scan) = scripted(Vec::new());
        let (window, _screen) = on_cleaner(seams);
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        let bar = ElementHandle::find_by_accessible_label(&window, "Progression de l'analyse")
            .next()
            .unwrap();
        assert_eq!(
            bar.accessible_role(),
            Some(AccessibleRole::ProgressIndicator)
        );
        let live = ElementHandle::find_by_element_type_name(&window, "Text").any(|text| {
            text.accessible_live_region().is_some_and(|liveness| {
                liveness != i_slint_backend_testing::AccessibleLiveness::Off
            }) && text
                .accessible_label()
                .is_some_and(|label| label.starts_with("Catégories terminées"))
        });
        assert!(live, "the categories done are said as they change");
        scan.go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
    }
}
