//! The cleaner screen: the categories chosen, the analysis run on a thread
//! and watched, its results, and the cleaning of what is checked
//! ([`cleaning`]). What it shows is worked out in plain Rust ([`Choices`],
//! [`ScanView`], [`ending`], `Results`, `Receipt`), tested without a window;
//! [`CleanerScreen`] binds it to the window.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use okapi_engine::{
    Cancel, Category, CleanReceipt, CustomFolder, DEFAULT_MIN_AGE, DEFAULT_MIN_AGE_MINUTES,
    DefaultChoice, Exclusions, Limit, Origin, Progress, ScanIssue, ScanOptions, ScanReport, Stop,
    UnsafeReason, Watch, clean_watched, scan_watched, settings_min_age,
};
use okapi_rules::{CleanupAction, Language, Os, RuleSet, TemplateVariables};
use okapi_store::{LastAnalysis, Settings};
use okapi_tools::docker::Access;
use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use okapi_helper::{Operation, Reply};

use crate::elevation::{ElevationFailure, ElevationStep, Launcher};
use crate::kept_selections::{KeptSelections, Unkept};
use crate::results::{
    AdminState, CheckState, DaemonOperation, GroupFacts, Line, Results, RunBy, Seen, SortColumn,
};
use crate::shown::{show, shown, size};
use crate::ui;
use cleaning::CleanJob;

/// How often the window looks at the task under way.
const WATCH_EVERY: Duration = Duration::from_millis(50);
/// How long closing the window waits for a task to stop, and how often
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
    /// The folders the user chose to have emptied, as written.
    pub custom_folders: Vec<PathBuf>,
    /// Every entry of a cleaning is kept in the history.
    pub deletion_log: bool,
    /// Where the history lives; none when no state directory is known.
    pub state_directory: Option<PathBuf>,
}

/// Why an analysis or a cleaning cannot start, its detail traced where it
/// is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    Variables,
    Rules,
    Settings,
    MinAge,
    Exclusion,
    /// A folder to clean of the settings is not written as one.
    CustomFolder,
    /// The deletion log asked for cannot be kept: nothing is removed.
    History,
    /// A task could not start: its thread, or what it would take.
    Start,
    /// The settings changed since the rights were obtained: root's part would
    /// not honour them, so nothing is removed and the rights are to obtain again.
    RightsChanged,
    /// The last change of the settings is still being written: what is
    /// read would miss it, so nothing starts until it is.
    SettingsSaving,
    /// The last change of the settings could not be written: said once,
    /// nothing starts then.
    SettingsUnsaved,
}

mod browsers;
mod cleaning;
pub mod docker;
mod rights;

pub(crate) use cleaning::line_shown;
pub(crate) use rights::problem_of_key;

/// One analysis to run.
#[derive(Debug)]
pub struct Job {
    pub rules: Arc<RuleSet>,
    pub variables: TemplateVariables,
    pub options: ScanOptions,
}

pub type Read = Rc<dyn Fn() -> Result<Inputs, Problem>>;
pub type Work = Arc<dyn Fn(Job, Cancel, Sender<Progress>) -> ScanReport + Send + Sync>;
pub type Clean = Arc<dyn Fn(&CleanJob, Cancel, Sender<Progress>) -> CleanReceipt + Send + Sync>;
/// Runs the privileged helper on a request line: pkexec, or a test's fake.
pub type Elevate = Arc<
    dyn Fn(Operation, Vec<u8>, &Cancel, &Sender<ElevationStep>) -> Result<Reply, ElevationFailure>
        + Send
        + Sync,
>;

/// What the screen reads the machine, scans and cleans with: the system's,
/// or a test's, which never touches the machine.
#[derive(Clone)]
pub struct Seams {
    pub read: Read,
    pub work: Work,
    pub clean: Clean,
    /// Who holds the lock of an open browser, and how to close it.
    pub browser: browsers::HolderSeam,
    /// The privileged helper, for what needs the administrator.
    pub elevate: Elevate,
    /// What the disk usage screen reads and asks of the machine.
    pub disk: crate::disk_usage::DiskSeams,
    /// What the duplicates screen reads and asks of the machine.
    pub duplicates: crate::duplicates::DuplicateSeams,
    /// What the large files screen reads and asks of the machine.
    pub large_files: crate::large_files::LargeFileSeams,
    /// What the settings screen reads and asks of the machine.
    pub settings: crate::settings::SettingsSeams,
    /// Where the updates screen reads what waits.
    pub updates: crate::updates::UpdatesSeams,
    /// What the applications screen reads and asks of the machine.
    pub apps: crate::apps::AppsSeams,
    /// Where the History screen reads the history.
    pub history: crate::history::HistorySeams,
    /// Docker's daemon, for the prune of its build cache.
    pub docker: docker::DockerSeam,
}

impl std::fmt::Debug for Seams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Seams").finish_non_exhaustive()
    }
}

impl Seams {
    /// The session's variables, the user's settings, and the real scan.
    pub fn system() -> Self {
        let disk = crate::disk_usage::DiskSeams::system();
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
            clean: Arc::new(|job, cancel, progress| {
                clean_watched(
                    &job.report,
                    &job.ids,
                    &job.rules,
                    &job.variables,
                    &job.options,
                    &Watch::new(cancel, Some(progress)),
                )
            }),
            browser: browsers::system_holder(),
            elevate: {
                let launcher = Launcher::pkexec();
                Arc::new(move |operation, line, cancel, steps| {
                    launcher.run(operation, line, cancel, steps)
                })
            },
            settings: crate::settings::SettingsSeams::system(disk.home.clone()),
            duplicates: crate::duplicates::DuplicateSeams::system(disk.home.clone()),
            large_files: crate::large_files::LargeFileSeams::system(disk.home.clone()),
            apps: crate::apps::AppsSeams::system(disk.home.clone()),
            disk,
            updates: crate::updates::UpdatesSeams::system(),
            history: crate::history::HistorySeams::system(),
            docker: docker::DockerSeam::system(),
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
        custom_folders: settings
            .custom_folders
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        deletion_log: settings.deletion_log,
        state_directory: okapi_platform::state_directory(),
    })
}

/// The minimum age the settings `inputs` ask for at `now`. It does no I/O,
/// so the window may check it.
fn min_age(inputs: &Inputs, now: SystemTime) -> Result<Duration, Problem> {
    match inputs.min_age_minutes {
        None => Ok(DEFAULT_MIN_AGE),
        Some(minutes) => settings_min_age(minutes, now).ok_or_else(|| {
            tracing::warn!(minutes, "settings minimum age beyond the clock");
            Problem::MinAge
        }),
    }
}

/// The folders to clean the settings `inputs` name, as the engine takes
/// them. It does no I/O, so the window may check it.
fn custom_folders(inputs: &Inputs) -> Result<Vec<CustomFolder>, Problem> {
    inputs
        .custom_folders
        .iter()
        .map(|folder| {
            CustomFolder::new(folder.clone()).map_err(|error| {
                tracing::warn!(%error, "settings folder to clean refused");
                Problem::CustomFolder
            })
        })
        .collect()
}

/// What the window checks of the settings `inputs` at `now` before an
/// analysis: what needs no I/O.
fn settings_problem(inputs: &Inputs, now: SystemTime) -> Option<Problem> {
    min_age(inputs, now)
        .err()
        .or_else(|| custom_folders(inputs).err())
}

/// The options of an analysis started at `now` over `chosen`: the
/// exclusions resolved at this instant, as each scan needs. Resolving reads
/// where each one really lies, which waits on a mount that stopped
/// answering: call it from a task's thread, not the window's.
fn options(
    inputs: &Inputs,
    now: SystemTime,
    chosen: BTreeSet<Category>,
) -> Result<ScanOptions, Problem> {
    let min_age = min_age(inputs, now)?;
    let custom_folders = custom_folders(inputs)?;
    tracing::debug!(exclusions = inputs.exclusions.len(), "resolving exclusions");
    let started = Instant::now();
    let exclusions = Exclusions::new(inputs.exclusions.iter().cloned()).map_err(|error| {
        tracing::warn!(?error, elapsed = ?started.elapsed(), "settings exclusion refused");
        Problem::Exclusion
    })?;
    tracing::info!(elapsed = ?started.elapsed(), "exclusions resolved");
    Ok(ScanOptions::new(now, min_age, exclusions)
        .with_categories(chosen)
        .with_custom_folders(custom_folders))
}

/// One category offered before the analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Choice {
    category: Category,
    checked: bool,
}

/// The categories offered, in the order of the rules, and which are checked.
/// The user's own folders are offered only when the settings name some.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choices(Vec<Choice>);

impl Choices {
    /// Checked as the rules say (invariant 5); without rules, none is. The
    /// user's folders are not offered until [`Self::offer_custom_folders`].
    #[must_use]
    pub fn of(rules: Option<&RuleSet>) -> Self {
        Self(
            Category::ALL
                .iter()
                .filter(|&&category| category != Category::Custom)
                .map(|&category| Choice {
                    category,
                    checked: rules.is_some_and(|rules| {
                        category.default_choice(rules) != DefaultChoice::ToChoose
                    }),
                })
                .collect(),
        )
    }

    /// Offers the user's folders when the settings name some (`any`),
    /// unchecked as they come (never chosen beforehand); withdraws them
    /// otherwise. Last, as in the rules' order: the other indices hold.
    pub fn offer_custom_folders(&mut self, any: bool) {
        let offered = self
            .0
            .last()
            .is_some_and(|choice| choice.category == Category::Custom);
        if any && !offered {
            self.0.push(Choice {
                category: Category::Custom,
                checked: false,
            });
        } else if !any && offered {
            let withdrawn = self.0.pop();
            tracing::debug!(
                checked = withdrawn.is_some_and(|choice| choice.checked),
                "no folder to clean: their card withdrawn"
            );
        }
    }

    pub fn toggle(&mut self, index: usize) {
        if let Some(choice) = self.0.get_mut(index) {
            choice.checked = !choice.checked;
        }
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

    /// The categories the analysis looks at.
    #[must_use]
    pub fn chosen(&self) -> BTreeSet<Category> {
        self.rows.iter().map(|row| row.category).collect()
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

/// How a scan that noted `issues` and shows `groups` lines (every line of
/// its results) of `bytes` ended: a
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

/// How the analysis that gave `report` ended for its summary: what the
/// user's own scan found (a target only root may read is none of it).
fn ending_of(report: &ScanReport) -> Option<Outcome> {
    ending(report.issues(), report.groups().len(), report.total_size())
}

/// What the analysis thread hands back.
struct Analyzed {
    report: ScanReport,
    /// The summary made for the Home page (kept when it could be), when
    /// the analysis is one to sum up.
    summary: Option<LastAnalysis>,
}

/// Where an analysis of at least the categories checked by default keeps
/// its summary: nowhere when no state directory is known (the Home page is
/// told it all the same).
#[derive(Debug)]
struct SummaryTarget {
    directory: Option<PathBuf>,
}

/// An analysis as the window prepares it: the settings read, its options
/// still to build on its thread.
struct Planned {
    rules: Arc<RuleSet>,
    inputs: Inputs,
    chosen: BTreeSet<Category>,
    target: Option<SummaryTarget>,
}

impl Planned {
    /// Builds its options, then scans with `work`: on the analysis's
    /// thread. Its options refused, nothing is scanned.
    fn run(
        self,
        work: &Work,
        cancel: Cancel,
        progress: Sender<Progress>,
    ) -> Result<Analyzed, Problem> {
        let options = options(&self.inputs, SystemTime::now(), self.chosen)?;
        tracing::info!(?options, "analysis started");
        let job = Job {
            rules: self.rules,
            variables: self.inputs.variables,
            options: options.clone(),
        };
        let report = work(job, cancel, progress);
        let summary = self
            .target
            .as_ref()
            .and_then(|target| keep_summary(&report, &options, target));
        Ok(Analyzed { report, summary })
    }
}

/// The summary of `report`, scanned with `options`, when the analysis came
/// to an end the window shows: something found, or nothing to clean (at 0
/// bytes); kept in `target`'s directory, its writing traced by the store.
/// Cancelled or stopped before reading, none: it replaces nothing.
fn keep_summary(
    report: &ScanReport,
    options: &ScanOptions,
    target: &SummaryTarget,
) -> Option<LastAnalysis> {
    let ending = ending_of(report);
    if !matches!(
        ending,
        Some(Outcome::Found { .. } | Outcome::NothingToClean { .. })
    ) {
        tracing::debug!(?ending, "analysis summary not kept: no ending to keep");
        return None;
    }
    let summary = LastAnalysis::of(report, options);
    if let Some(directory) = &target.directory
        && summary.save(directory).is_ok()
    {
        tracing::info!(?summary, "analysis summary kept");
    }
    Some(summary)
}

/// What the cleaner tells the rest of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanerEvent {
    /// An analysis of at least the categories checked by default came to
    /// an end the window shows: its summary (kept when it could be).
    AnalysisDone(LastAnalysis),
    /// A cleaning ended, `freed` bytes when its receipt says so (none when
    /// its thread died, or Docker's prune ended unknown), at `at` seconds
    /// since the clock's origin.
    CleaningDone { freed: Option<u64>, at: i64 },
}

/// Who hears the cleaner's events.
pub type Listener = Rc<dyn Fn(CleanerEvent)>;

const fn kind(category: Category) -> ui::CategoryKind {
    match category {
        Category::System => ui::CategoryKind::System,
        Category::Browsers => ui::CategoryKind::Browsers,
        Category::Apps => ui::CategoryKind::Apps,
        Category::Gaming => ui::CategoryKind::Gaming,
        Category::Gpu => ui::CategoryKind::Gpu,
        Category::Custom => ui::CategoryKind::Custom,
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
        Some(Problem::CustomFolder) => ui::SetupProblem::CustomFolder,
        Some(Problem::History) => ui::SetupProblem::History,
        Some(Problem::Start) => ui::SetupProblem::Start,
        Some(Problem::RightsChanged) => ui::SetupProblem::RightsChanged,
        Some(Problem::SettingsSaving) => ui::SetupProblem::SettingsSaving,
        Some(Problem::SettingsUnsaved) => ui::SetupProblem::SettingsUnsaved,
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

/// The column of the results a header names.
const fn column_of(column: ui::ResultsSort) -> SortColumn {
    match column {
        ui::ResultsSort::Name => SortColumn::Name,
        ui::ResultsSort::Entries => SortColumn::Entries,
        ui::ResultsSort::Size => SortColumn::Size,
    }
}

/// The header of a column of the results: [`column_of`] the other way.
const fn column_shown(column: SortColumn) -> ui::ResultsSort {
    match column {
        SortColumn::Name => ui::ResultsSort::Name,
        SortColumn::Entries => ui::ResultsSort::Entries,
        SortColumn::Size => ui::ResultsSort::Size,
    }
}

/// A line of the results as the window shows it.
fn result_row(results: &Results, row: &Line) -> ui::ResultRow {
    let state = |state: CheckState| match state {
        CheckState::Checked => ui::CheckState::Checked,
        CheckState::Unchecked => ui::CheckState::Unchecked,
        CheckState::Partial => ui::CheckState::Partial,
    };
    match row {
        Line::Category {
            category,
            state: check,
            enabled,
            folded,
            entries,
            bytes,
        } => ui::ResultRow {
            kind: ui::RowKind::Category,
            nested: false,
            category: kind(*category),
            explained: false,
            checked_by_default: false,
            state: state(*check),
            enabled: *enabled,
            folded: *folded,
            hidden: false,
            badge: ui::ResultBadge::Preselected,
            note: ui::RowNote::None,
            entries: shown(*entries),
            size: size(*bytes),
            ..ui::ResultRow::default()
        },
        Line::Browser {
            browser,
            state: check,
            enabled,
            folded,
            hidden,
            entries,
            bytes,
        } => ui::ResultRow {
            kind: ui::RowKind::Browser,
            nested: false,
            category: ui::CategoryKind::Browsers,
            label: browser.name().into(),
            explained: false,
            checked_by_default: false,
            state: state(*check),
            enabled: *enabled,
            folded: *folded,
            hidden: *hidden,
            badge: ui::ResultBadge::Preselected,
            note: ui::RowNote::None,
            entries: shown(*entries),
            size: size(*bytes),
            ..ui::ResultRow::default()
        },
        Line::Group {
            index,
            checked,
            hidden,
            nested,
            explained,
        } => {
            let Some(group) = results.group(*index) else {
                tracing::error!(index, "a line of the results names no group");
                return ui::ResultRow::default();
            };
            ui::ResultRow {
                kind: ui::RowKind::Group,
                nested: *nested,
                category: results.shown_under(*index).map_or_else(
                    || {
                        tracing::error!(index, "a group of the results under no category chosen");
                        ui::CategoryKind::System
                    },
                    kind,
                ),
                label: if *nested {
                    group.label.as_str().into()
                } else {
                    group.label_alone.as_str().into()
                },
                description: group.description.as_str().into(),
                explained: *explained,
                checked_by_default: group.checked_by_default,
                state: state(if *checked {
                    CheckState::Checked
                } else {
                    CheckState::Unchecked
                }),
                enabled: group.is_checkable(),
                folded: false,
                hidden: *hidden,
                badge: badge_of(group),
                note: note_of(group),
                command: command_of(group),
                entries: shown(group.numbers().0),
                size: size(group.numbers().1),
            }
        }
    }
}

/// The command a group's line shows to copy: the one the user runs by hand
/// for it, none for any other.
fn command_of(group: &GroupFacts) -> SharedString {
    group.command().map(SharedString::from).unwrap_or_default()
}

/// The badge of a group: "Administrator" while locked; unlocked, an
/// administrator's group is to choose, never preselected (invariant 5), as
/// is a maintenance Okapi does not run (Docker's, whatever Docker says of
/// the user, or the user's own).
fn badge_of(group: &GroupFacts) -> ui::ResultBadge {
    if group.run_by.is_none() && !group.is_checkable() {
        ui::ResultBadge::Admin
    } else if !group.checked_by_default || !group.is_checkable() {
        ui::ResultBadge::ToChoose
    } else {
        ui::ResultBadge::Preselected
    }
}

/// What the note of a group says: who runs a maintenance Okapi does not
/// (Docker, the user), where a group stands while out of reach, or that it
/// is unlocked; nothing for any other, preselected or left to the user.
fn note_of(group: &GroupFacts) -> ui::RowNote {
    match &group.run_by {
        Some(RunBy::Docker(daemon)) => {
            return match daemon.access {
                Access::Allowed => ui::RowNote::Daemon,
                Access::NotAllowed => ui::RowNote::DaemonRefused,
            };
        }
        Some(RunBy::User { .. }) => return ui::RowNote::ByHand,
        None => {}
    }
    match badge_of(group) {
        ui::ResultBadge::ToChoose if group.needs_admin => ui::RowNote::RightsObtained,
        // Left to the user, its unchecked box says it; the confirmation
        // warns of a cache reset or the trash.
        ui::ResultBadge::Preselected | ui::ResultBadge::ToChoose => ui::RowNote::None,
        ui::ResultBadge::Admin => match (group.admin, &group.targets, group.seen) {
            (AdminState::NothingToClean, ..) => ui::RowNote::Nothing,
            (_, None, _) => ui::RowNote::Unreachable,
            (_, Some(_), Seen::Unreadable) => ui::RowNote::Unseen,
            (_, Some(_), Seen::Operation) => ui::RowNote::Unmeasured,
            (_, Some(_), Seen::Counted { .. }) => ui::RowNote::Locked,
        },
    }
}

/// A note of the analysis as the window says it; a stop is no note.
fn note_shown(issue: &ScanIssue) -> Option<ui::IssueShown> {
    let path = |path: &std::path::Path| SharedString::from(path.display().to_string());
    let (kind, path) = match issue {
        ScanIssue::Stopped(_) => return None,
        ScanIssue::AgeBeyondClock { .. } => {
            (ui::IssueKind::AgeBeyondClock, SharedString::default())
        }
        ScanIssue::Unsupported { .. } => (ui::IssueKind::Unsupported, SharedString::default()),
        ScanIssue::Unresolvable { .. } => (ui::IssueKind::Unresolvable, SharedString::default()),
        ScanIssue::UnreadableRoot { path: at, .. } => (ui::IssueKind::UnreadableRoot, path(at)),
        ScanIssue::UnsafeRoot {
            path: at,
            reason: UnsafeReason::BrowserData,
            ..
        } => (ui::IssueKind::BrowserData, path(at)),
        ScanIssue::UnsafeRoot { path: at, .. } => (ui::IssueKind::UnsafeRoot, path(at)),
        ScanIssue::ProtectedRoot { path: at, .. } => (ui::IssueKind::ProtectedRoot, path(at)),
        ScanIssue::Excluded { path: at, .. } => (ui::IssueKind::Excluded, path(at)),
        // Its path as written in the settings, where it is said why.
        ScanIssue::CustomFolderRefused { origin, .. } => {
            let Origin::Custom { folder } = origin else {
                tracing::error!(?origin, "a custom folder refused names another rule");
                return None;
            };
            (ui::IssueKind::CustomFolderRefused, path(folder.path()))
        }
        ScanIssue::LimitReached { root, limit, .. } => (
            match limit {
                Limit::Depth => ui::IssueKind::DepthLimit,
                Limit::Entries => ui::IssueKind::EntriesLimit,
            },
            path(root),
        ),
    };
    Some(ui::IssueShown { kind, path })
}

/// A thread at work, its progress and its result sent on channels made for
/// it, and the token that cancels it.
struct Running<T> {
    cancel: Cancel,
    progress: Receiver<Progress>,
    result: Receiver<T>,
    thread: JoinHandle<()>,
}

impl<T: Send + 'static> Running<T> {
    /// Runs `work` on a thread named `name`, in `span`.
    fn start(
        name: &str,
        span: tracing::Span,
        work: impl FnOnce(Cancel, Sender<Progress>) -> T + Send + 'static,
    ) -> std::io::Result<Self> {
        let cancel = Cancel::new();
        let (progress_sender, progress) = mpsc::channel();
        let (result_sender, result) = mpsc::channel();
        let token = cancel.clone();
        let thread = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                span.in_scope(|| {
                    if result_sender.send(work(token, progress_sender)).is_err() {
                        tracing::info!("result of a task given up or closed: unread");
                    }
                });
            })?;
        Ok(Self {
            cancel,
            progress,
            result,
            thread,
        })
    }
}

/// An analysis under way.
struct Analysis {
    running: Running<Result<Analyzed, Problem>>,
    view: ScanView,
    /// How long it has been waited for since it was cancelled, counted in
    /// looks at it.
    cancelled_for: Option<Duration>,
    /// The exclusions of the settings it started with, which its thread
    /// resolves.
    exclusions: Vec<PathBuf>,
}

/// The one thing the screen follows at a time: an analysis and a cleaning
/// are never both under way (the thread of an analysis given up may still
/// read, its report unread).
enum Task {
    Analysis(Analysis),
    /// Root's scan of what waits for the rights.
    Elevating(rights::Elevating),
    Cleaning(cleaning::Cleaning),
}

impl Task {
    const fn cancel(&self) -> &Cancel {
        match self {
            Self::Analysis(analysis) => &analysis.running.cancel,
            Self::Elevating(elevating) => &elevating.running.cancel,
            Self::Cleaning(cleaning) => &cleaning.running.cancel,
        }
    }

    fn is_finished(&self) -> bool {
        match self {
            Self::Analysis(analysis) => analysis.running.thread.is_finished(),
            Self::Elevating(elevating) => elevating.running.thread.is_finished(),
            Self::Cleaning(cleaning) => cleaning.running.thread.is_finished(),
        }
    }
}

/// A report kept for its cleaning, and the results shown of it.
struct Held {
    report: Arc<ScanReport>,
    results: Results,
    /// The age and exclusions root's scan unlocked the groups with: sent as
    /// they are to root's cleaning, which measures its groups again
    /// (invariant 1) and so never removes younger or excluded entries than
    /// the scan shown would have.
    granted: Option<rights::Granted>,
}

struct State {
    window: slint::Weak<ui::AppWindow>,
    /// The language of the window, which the rules' labels follow.
    language: Language,
    seams: Seams,
    rules: Option<Arc<RuleSet>>,
    /// The path each rule the privileged helper may be asked for is asked
    /// by.
    helper_targets: BTreeMap<Origin, String>,
    choices: Choices,
    choice_model: Rc<VecModel<ui::CategoryChoice>>,
    row_model: Rc<VecModel<ui::ScanRow>>,
    stop_model: Rc<VecModel<ui::StopShown>>,
    task: Option<Task>,
    /// The last analysis that found something, and the user's choice in it:
    /// what a cleaning takes its identifiers from (invariant 1).
    held: Option<Held>,
    /// The user's choices among the results, kept from one analysis to the
    /// next; none when they are not kept.
    kept: Option<KeptSelections>,
    /// The confirmation of a cleaning, open.
    confirming: Option<cleaning::Confirming>,
    /// The question about an open browser, before the cleaning confirmed.
    asking: Option<browsers::Asking>,
    /// The processes of the browsers asked to close, never asked twice.
    asked_to_close: BTreeSet<u32>,
    result_model: Rc<VecModel<ui::ResultRow>>,
    receipt_model: Rc<VecModel<ui::ReceiptLineShown>>,
    step_model: Rc<VecModel<ui::CleanStep>>,
    note_model: Rc<VecModel<ui::IssueShown>>,
    timer: Rc<slint::Timer>,
    /// Runs for [`SECOND_CLICK`] after the screen changes.
    settling: slint::Timer,
    /// Looks, a little after a choice, at whether it was kept.
    write_check: slint::Timer,
    /// The last choice among the results could not be kept.
    choices_unsaved: bool,
    /// The exclusions read when a task's thread refused one of them: said
    /// again by each reading of the settings that holds the same ones,
    /// whatever page was shown when it came; forgotten once an analysis or
    /// a cleaning starts, or other exclusions are read.
    refused_exclusions: Option<Vec<PathBuf>>,
    listener: Option<Listener>,
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
    /// the settings read, the rules' labels in `language`.
    pub fn attach(window: &ui::AppWindow, seams: Seams, language: Language) -> Self {
        let rules = RuleSet::embedded(Os::Linux)
            .inspect_err(|error| tracing::error!(?error, "embedded rules unreadable"))
            .ok()
            .map(Arc::new);
        let choices = Choices::of(rules.as_deref());
        let helper_targets = rules
            .as_deref()
            .map(rights::helper_targets)
            .unwrap_or_default();
        let state = State {
            window: window.as_weak(),
            language,
            seams,
            rules,
            helper_targets,
            choices,
            choice_model: Rc::new(VecModel::default()),
            row_model: Rc::new(VecModel::default()),
            stop_model: Rc::new(VecModel::default()),
            task: None,
            held: None,
            kept: None,
            confirming: None,
            asking: None,
            asked_to_close: BTreeSet::new(),
            result_model: Rc::new(VecModel::default()),
            receipt_model: Rc::new(VecModel::default()),
            step_model: Rc::new(VecModel::default()),
            note_model: Rc::new(VecModel::default()),
            timer: Rc::new(slint::Timer::default()),
            settling: slint::Timer::default(),
            write_check: slint::Timer::default(),
            choices_unsaved: false,
            refused_exclusions: None,
            listener: None,
        };
        let cleaner = window.global::<ui::Cleaner>();
        cleaner.set_choices(ModelRc::from(Rc::clone(&state.choice_model)));
        cleaner.set_rows(ModelRc::from(Rc::clone(&state.row_model)));
        cleaner.set_stops(ModelRc::from(Rc::clone(&state.stop_model)));
        cleaner.set_results(ModelRc::from(Rc::clone(&state.result_model)));
        cleaner.set_notes(ModelRc::from(Rc::clone(&state.note_model)));
        cleaner.set_receipt_lines(ModelRc::from(Rc::clone(&state.receipt_model)));
        cleaner.set_clean_steps(ModelRc::from(Rc::clone(&state.step_model)));
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
        let on_row = move |action: fn(&Self, i32)| {
            let weak = weak.clone();
            move |row: i32| {
                if let Some(state) = weak.upgrade() {
                    action(&Self(state), row);
                }
            }
        };
        cleaner.on_toggle(on_row(Self::toggle));
        cleaner.on_analyze(on(Self::analyze));
        cleaner.on_cancel(on(Self::cancel));
        cleaner.on_choose_again(on(Self::back_to_choosing));
        cleaner.on_toggle_result(on_row(|screen, row| {
            screen.change_results(row, Results::toggle_row);
        }));
        cleaner.on_fold(on_row(|screen, row| {
            screen.change_results(row, Results::fold_row);
        }));
        cleaner.on_explain(on_row(Self::explain));
        let weak = Rc::downgrade(&screen.0);
        cleaner.on_sort_by(move |column| {
            if let Some(state) = weak.upgrade() {
                Self(state).sort_by(column);
            }
        });
        cleaner.on_command_copied(on_row(Self::command_copied));
        cleaner.on_deselect_all(on(Self::deselect_all));
        cleaner.on_clean(on(Self::clean));
        cleaner.on_confirm_delete(on(Self::confirm_delete));
        cleaner.on_cancel_confirmation(on(Self::cancel_confirmation));
        cleaner.on_ignore_browser(on(Self::ignore_browser));
        cleaner.on_close_browser(on(Self::close_browser));
        cleaner.on_cancel_browser(on(Self::cancel_browser_question));
        cleaner.on_cancel_cleaning(on(Self::cancel_cleaning));
        cleaner.on_finish(on(Self::finish));
        cleaner.on_obtain_rights(on(Self::obtain_rights));
        cleaner.on_cancel_rights(on(Self::cancel_rights));
        screen
    }

    /// Tells `listener` the events from now on.
    pub fn listen(&self, listener: Listener) {
        self.0.borrow_mut().listener = Some(listener);
    }

    /// Tells the listener `event`, out of any borrow: it may look at the
    /// screen.
    fn emit(&self, event: CleanerEvent) {
        tracing::debug!(?event, "cleaner event");
        let listener = self.0.borrow().listener.clone();
        if let Some(listener) = listener {
            listener(event);
        }
    }

    /// The analysis the Home page asks for: the categories checked by
    /// default, on the cleaner's page, refused as the cleaner's own button
    /// would be.
    pub fn analyze_default(&self) {
        if self.is_settling("analysis from home") || self.results_frozen("analysis from home") {
            return;
        }
        if self.0.borrow().task.is_some() {
            tracing::warn!("analysis from home asked for while a task runs");
            return;
        }
        {
            let mut state = self.0.borrow_mut();
            state.choices = Choices::of(state.rules.as_deref());
        }
        self.show_choices();
        self.with_window(|window, _| window.set_page(ui::Page::Cleaner));
        self.analyze();
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
                })
                .collect();
            show(&state.choice_model, rows);
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_chosen_count(shown(state.choices.chosen().len()));
        });
    }

    fn set_step(&self, step: ui::CleanerStep) {
        self.with_window(|window, _| window.global::<ui::Cleaner>().set_step(step));
    }

    /// Says `found`, a refusal a task's thread met with the settings'
    /// `exclusions`; an exclusion refused is kept to be said again. A
    /// problem of the settings said meanwhile stays: it may be said once
    /// only, and the refusal comes back with the next reading of them.
    fn say_refusal(&self, found: Problem, exclusions: Vec<PathBuf>) {
        if found == Problem::Exclusion {
            self.0.borrow_mut().refused_exclusions = Some(exclusions);
        }
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            let shown = cleaner.get_problem();
            if shown == ui::SetupProblem::None {
                cleaner.set_problem(problem(Some(found)));
            } else {
                tracing::debug!(?shown, problem = ?found, "refusal kept, not shown over a problem of the settings");
            }
        });
    }

    /// Forgets the exclusions a task's thread refused: an analysis or a
    /// cleaning starts, whose thread meets them afresh.
    fn forget_refused_exclusions(&self) {
        self.0.borrow_mut().refused_exclusions = None;
    }

    /// Whether the settings read, `inputs`, still hold the exclusions a
    /// task's thread refused: the refusal is then said again. Reading cannot
    /// resolve them (a dead mount would hold the window): other exclusions
    /// read, it is forgotten.
    fn still_refused(&self, inputs: &Result<Inputs, Problem>) -> bool {
        let mut state = self.0.borrow_mut();
        let Some(refused) = &state.refused_exclusions else {
            return false;
        };
        match inputs {
            Ok(inputs) if inputs.exclusions != *refused => {
                tracing::info!(
                    "exclusions changed since one was refused: the refusal is no more said"
                );
                state.refused_exclusions = None;
                false
            }
            // Unread now, they may be the same: kept for the next reading.
            Ok(_) | Err(_) => true,
        }
    }

    /// Reads the settings again, and says whether they let an analysis
    /// start: on showing the screen, and back to choosing. The exclusions
    /// are not resolved here, as a dead mount would hold the window: one
    /// refused is said when a task's thread meets it.
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
                .and_then(|inputs| settings_problem(inputs, SystemTime::now()))
        });
        // A problem of the settings read now comes first.
        let found = found.or(self.still_refused(&inputs).then_some(Problem::Exclusion));
        let (minutes, exclusions) =
            inputs
                .as_ref()
                .map_or((DEFAULT_MIN_AGE_MINUTES, 0), |inputs| {
                    (
                        inputs.min_age_minutes.unwrap_or(DEFAULT_MIN_AGE_MINUTES),
                        inputs.exclusions.len(),
                    )
                });
        let custom_folders = inputs
            .as_ref()
            .map_or(0, |inputs| inputs.custom_folders.len());
        tracing::debug!(problem = ?found, minutes, exclusions, custom_folders, "cleaner settings read");
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_problem(problem(found));
            cleaner.set_min_age_minutes(shown(minutes));
            cleaner.set_exclusion_count(shown(exclusions));
        });
        self.0
            .borrow_mut()
            .choices
            .offer_custom_folders(custom_folders > 0);
        self.show_choices();
    }

    fn toggle(&self, index: i32) {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        self.0.borrow_mut().choices.toggle(index);
        self.show_choices();
    }

    /// The analysis of `chosen`: the settings read now (they may have
    /// changed since the screen was shown), its options to be built from
    /// them on its thread, and where its summary is kept: none when it
    /// covers less than the categories checked by default, whose estimate a
    /// partial analysis never replaces.
    fn prepare(&self, chosen: &BTreeSet<Category>) -> Result<Planned, Problem> {
        let state = self.0.borrow();
        let rules = state.rules.as_ref().ok_or(Problem::Rules)?;
        let inputs = (state.seams.read)()?;
        // Only what needs no I/O is checked here: an exclusion is met, and
        // said, on the task's thread.
        if let Some(problem) = settings_problem(&inputs, SystemTime::now()) {
            return Err(problem);
        }
        let by_default = Choices::of(Some(rules)).chosen();
        let target = if chosen.contains(&Category::Custom) {
            // What the user's folders hold, and the room they take from the
            // rules' roots above them, would make it no estimate to compare.
            tracing::info!("analysis of the user's folders: its summary is not kept");
            None
        } else if by_default.is_subset(chosen) {
            if inputs.state_directory.is_none() {
                tracing::warn!("no state directory: the summary of the analysis is not written");
            }
            Some(SummaryTarget {
                directory: inputs.state_directory.clone(),
            })
        } else {
            let missing: Vec<_> = by_default.difference(chosen).collect();
            tracing::info!(?missing, "partial analysis: its summary is not kept");
            None
        };
        Ok(Planned {
            rules: Arc::clone(rules),
            inputs,
            chosen: chosen.clone(),
            target,
        })
    }

    /// Starts an analysis of the categories checked.
    fn analyze(&self) {
        if self.is_settling("analysis") || self.results_frozen("analysis") {
            return;
        }
        let chosen = self.0.borrow().choices.chosen();
        if self.0.borrow().task.is_some() {
            tracing::warn!("analysis asked for while one runs");
            return;
        }
        if chosen.is_empty() {
            tracing::warn!("analysis asked for without a category");
            return;
        }
        let planned = match self.prepare(&chosen) {
            Ok(planned) => planned,
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
        // It starts: a refusal said before is over.
        self.with_window(|window, _| window.global::<ui::Cleaner>().set_problem(problem(None)));
        tracing::info!(categories = ?chosen, "analysis asked for");
        let replaces_estimate = planned.target.is_some();
        self.with_window(|window, _| {
            window
                .global::<ui::Cleaner>()
                .set_replaces_estimate(replaces_estimate);
        });
        let work = Arc::clone(&self.0.borrow().seams.work);
        let exclusions = planned.inputs.exclusions.clone();
        let started = Running::start(
            "okapi-analysis",
            tracing::info_span!("analysis"),
            move |cancel, progress| planned.run(&work, cancel, progress),
        );
        // The results of the analysis before stay until this one ends:
        // refused on its thread, the analysis leaves them as they were.
        self.settle();
        let running = match started {
            Ok(running) => running,
            Err(error) => {
                tracing::error!(%error, "analysis thread not started");
                self.end(Some(Outcome::Failed));
                return;
            }
        };
        // Its thread meets the exclusions afresh.
        self.forget_refused_exclusions();
        self.0.borrow_mut().task = Some(Task::Analysis(Analysis {
            running,
            view: ScanView::new(&chosen),
            cancelled_for: None,
            exclusions,
        }));
        self.show_view();
        self.set_step(ui::CleanerStep::Planning);
        self.watch_task();
    }

    /// Looks at the task under way every [`WATCH_EVERY`].
    fn watch_task(&self) {
        let timer = Rc::clone(&self.0.borrow().timer);
        let weak = Rc::downgrade(&self.0);
        timer.start(slint::TimerMode::Repeated, WATCH_EVERY, move || {
            if let Some(state) = weak.upgrade() {
                let screen = Self(state);
                let watched = match screen.0.borrow().task {
                    Some(Task::Analysis(_)) => Self::watch,
                    Some(Task::Elevating(_)) => Self::watch_elevation,
                    Some(Task::Cleaning(_)) => Self::watch_cleaning,
                    None => return,
                };
                watched(&screen);
            }
        });
    }

    /// Takes in the progress sent so far, then looks for the report (one
    /// reader: what is sent after this look waits for the next one, and the
    /// ending shows the report's own counts).
    fn watch(&self) {
        let (received, given_up, changed) = {
            let mut state = self.0.borrow_mut();
            let Some(Task::Analysis(analysis)) = state.task.as_mut() else {
                return;
            };
            let mut changed = false;
            for progress in analysis.running.progress.try_iter() {
                analysis.view.apply(progress);
                changed = true;
            }
            if let Some(waited) = &mut analysis.cancelled_for {
                *waited += WATCH_EVERY;
            }
            let view = &analysis.view;
            let given_up = analysis
                .cancelled_for
                .is_some_and(|waited| waited >= CANCEL_WAIT)
                .then(|| (view.path.clone(), view.roots_done, view.total));
            (analysis.running.result.try_recv(), given_up, changed)
        };
        if changed {
            self.show_view();
        }
        match received {
            Ok(Err(found)) => self.analysis_refused(found),
            Ok(Ok(Analyzed { report, summary })) => {
                tracing::info!(
                    groups = report.groups().len(),
                    bytes = report.total_size(),
                    issues = report.issues().len(),
                    "analysis report received"
                );
                // Built here, where the helper's targets are known: what
                // only root may read, and the maintenances root runs, are
                // found too, though no group of the report holds them.
                let mut lines = self.unseen_of(&report);
                lines.extend(self.maintenances_of(&report));
                let outcome = ending(
                    report.issues(),
                    report.groups().len() + lines.len(),
                    report.total_size(),
                );
                if matches!(outcome, Some(Outcome::Found { .. })) {
                    self.hold(report, lines);
                }
                self.end(outcome);
                if let Some(summary) = summary {
                    self.emit(CleanerEvent::AnalysisDone(summary));
                }
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
            let Some(Task::Analysis(analysis)) = &state.task else {
                return;
            };
            let view = &analysis.view;
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

    /// The analysis refused on its thread for `found`, before anything was
    /// scanned: back to the results it would have replaced, or to choosing,
    /// the refusal said. The settings are not read again (`refresh`): a
    /// problem they give that is said once only is left to the next reading.
    fn analysis_refused(&self, found: Problem) {
        if !matches!(self.0.borrow().task, Some(Task::Analysis(_))) {
            tracing::error!("an analysis refused that was not under way");
            return;
        }
        let Some(Task::Analysis(analysis)) = self.0.borrow_mut().task.take() else {
            return;
        };
        self.0.borrow().timer.stop();
        let held = self.0.borrow().held.is_some();
        // Its cause is traced where it was met, on the thread.
        tracing::info!(problem = ?found, held, "analysis refused on its thread: nothing scanned");
        self.settle();
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            if held {
                cleaner.set_step(ui::CleanerStep::Results);
            } else {
                cleaner.set_back_from_analysis(true);
                cleaner.set_step(ui::CleanerStep::Idle);
            }
        });
        if held {
            self.show_results();
        }
        self.say_refusal(found, analysis.exclusions);
    }

    /// Ends the analysis: shows how it ended, or goes back to choosing.
    fn end(&self, outcome: Option<Outcome>) {
        let cancel_asked = {
            let mut state = self.0.borrow_mut();
            state.timer.stop();
            state
                .task
                .take()
                .is_some_and(|task| task.cancel().is_cancelled())
        };
        let Some(outcome) = outcome else {
            tracing::info!("analysis cancelled: back to choosing");
            self.back_to_choosing();
            return;
        };
        // A cancellation that came after the scan ended shows its report.
        tracing::info!(?outcome, cancel_asked, "analysis ended");
        if !matches!(outcome, Outcome::Found { .. }) {
            // The analysis is the last one: the results of the one before go
            // (found, its report took their place in `hold`).
            self.0.borrow_mut().held = None;
        }
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
                // What was found is said by the results (`show_results`).
                Outcome::Found { .. } => {
                    // "Nettoyer" takes the place of "Annuler l'analyse".
                    self.settle();
                    cleaner.set_step(ui::CleanerStep::Results);
                    return;
                }
                Outcome::Failed => ui::OutcomeKind::Failed,
            };
            cleaner.set_outcome(shown_kind);
            cleaner.set_step(ui::CleanerStep::Outcome);
        });
        self.show_results();
    }

    /// The groups only root may read of the analysis under way, which gave
    /// `report` ([`rights::unseen_groups`]).
    fn unseen_of(&self, report: &ScanReport) -> Vec<GroupFacts> {
        let state = self.0.borrow();
        let (Some(Task::Analysis(analysis)), Some(rules)) = (&state.task, &state.rules) else {
            tracing::warn!(
                analysis = matches!(state.task, Some(Task::Analysis(_))),
                rules = state.rules.is_some(),
                "what only root may read not looked for: its unreadable roots stay notes"
            );
            return Vec::new();
        };
        // The analysis read them already, refusing what it could not use:
        // compared as written, never read again. Unusable, nothing is shown
        // that one of them may cover: its unreadable roots stay notes.
        let excluded = match Exclusions::as_written(analysis.exclusions.iter().cloned()) {
            Ok(excluded) => excluded,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "exclusions of the analysis unusable: its unreadable roots stay notes"
                );
                return Vec::new();
            }
        };
        rights::unseen_groups(
            report.issues(),
            report
                .groups()
                .iter()
                .flat_map(okapi_engine::ScanGroup::origins)
                .collect(),
            rules,
            state.language,
            &state.helper_targets,
            &analysis.view.chosen(),
            &excluded,
        )
    }

    /// The lines of the maintenances `report` found that root runs
    /// ([`GroupFacts::maintenance`]), or Docker for the session, when its
    /// socket is there ([`GroupFacts::daemon_operation`]), or the user by
    /// hand ([`GroupFacts::by_hand`]).
    /// Never one whose operation would refuse whatever the rights: its line
    /// would wait for them for ever.
    fn maintenances_of(&self, report: &ScanReport) -> Vec<GroupFacts> {
        let state = self.0.borrow();
        report
            .maintenances()
            .iter()
            .filter(|found| {
                let unfit = (found.action() == CleanupAction::JournalVacuum)
                    .then(|| found.paths().iter().find_map(|path| okapi_helper::unfit(path)))
                    .flatten();
                if let Some(unfit) = unfit {
                    tracing::info!(origin = ?found.origin(), ?unfit, "a maintenance root would refuse: no line");
                }
                unfit.is_none()
            })
            .filter_map(|found| {
                if found.action() == CleanupAction::DockerBuildPrune {
                    // No socket: no Docker for this session, no line.
                    let (socket, access) = (state.seams.docker.find)()?;
                    return Some(GroupFacts::daemon_operation(
                        found,
                        state.language,
                        DaemonOperation {
                            socket,
                            access,
                        },
                    ));
                }
                GroupFacts::by_hand(found, state.language).or_else(|| {
                    GroupFacts::maintenance(found, state.language, &state.helper_targets)
                })
            })
            .collect()
    }

    /// Keeps the report of an analysis that found something, with `lines`
    /// that the report does not hold (the groups only root may read, the
    /// maintenances root runs, Docker's and the user's by hand); its results
    /// checked as the engine says, less the groups the user unchecked before
    /// (choices kept only ever uncheck).
    fn hold(&self, report: ScanReport, lines: Vec<GroupFacts>) {
        let mut state = self.0.borrow_mut();
        let chosen = state
            .task
            .as_ref()
            .and_then(|task| match task {
                Task::Analysis(analysis) => Some(analysis.view.chosen()),
                Task::Elevating(_) | Task::Cleaning(_) => None,
            })
            .unwrap_or_default();
        let language = state.language;
        // After the report's own: an index of the report never names one
        // of those (invariant 1).
        let groups: Vec<GroupFacts> = report
            .groups()
            .iter()
            .map(|group| {
                GroupFacts::of(
                    group,
                    language,
                    &state.helper_targets,
                    state.seams.disk.home.as_deref(),
                )
            })
            .chain(lines)
            .collect();
        let notes: Vec<_> = report
            .issues()
            .iter()
            .filter(|issue| !rights::said_by(issue, &groups))
            .filter_map(note_shown)
            .collect();
        show(&state.note_model, notes);
        let mut results = Results::new(groups, chosen);
        if let Some(kept) = &state.kept {
            results = results.unchecking(kept.unchecked());
        }
        // A new analysis: what waits for the rights is locked again, and
        // what was said of the rights before is over.
        state.held = Some(Held {
            results,
            report: Arc::new(report),
            granted: None,
        });
        if let Some(window) = state.window.upgrade() {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_rights_step(ui::RightsStep::Idle);
            cleaner.set_rights_problem(ui::RightsProblem::None);
        }
    }

    /// Changes the results at the line `row` shows, by `change`.
    fn change_results(&self, row: i32, change: fn(&mut Results, usize)) {
        // The second click of a double click on what was in its place (the
        // confirmation's buttons) changes nothing.
        if self.is_settling("change of the results") || self.results_frozen("change of the results")
        {
            return;
        }
        {
            let mut guard = self.0.borrow_mut();
            let state = &mut *guard;
            let Some(held) = state.held.as_mut() else {
                return;
            };
            let Ok(row) = usize::try_from(row) else {
                return;
            };
            let before = held.results.checked().clone();
            change(&mut held.results, row);
            if let Some(kept) = state.kept.as_mut() {
                kept.update(&held.results, &before);
            }
        }
        self.check_write();
    }

    /// Traces that the command of the line at `row` was copied: asked
    /// only, Slint does not say whether the system's clipboard got it.
    fn command_copied(&self, row: i32) {
        let state = self.0.borrow();
        let copied = usize::try_from(row)
            .ok()
            .and_then(|row| state.held.as_ref()?.results.command_at(row));
        if let Some(command) = copied {
            tracing::info!(row, command, "copy of a command asked");
        } else {
            tracing::warn!(row, "a command copied from no line that shows one");
        }
    }

    /// Unfolds or folds the description of the group the line `row` shows.
    /// Allowed while the results are frozen: it changes neither what is
    /// checked nor what a cleaning would take.
    fn explain(&self, row: i32) {
        if self.is_settling("description of a group") {
            return;
        }
        {
            let mut state = self.0.borrow_mut();
            let Some(held) = state.held.as_mut() else {
                tracing::warn!(row, "no results held to explain a group of");
                return;
            };
            let Ok(row) = usize::try_from(row) else {
                tracing::warn!(row, "no line of the results to explain");
                return;
            };
            held.results.explain_row(row);
        }
        self.show_results();
    }

    /// Sorts the groups by `column` within their category, or the other
    /// way. Allowed while the results are frozen, as a description is.
    fn sort_by(&self, column: ui::ResultsSort) {
        if self.is_settling("sort of the results") {
            return;
        }
        let column = column_of(column);
        {
            let mut state = self.0.borrow_mut();
            let Some(held) = state.held.as_mut() else {
                tracing::warn!(?column, "no results held to sort");
                return;
            };
            held.results.sort_by(column);
            tracing::info!(
                sorting = ?held.results.sorting(),
                entries = held.results.entries(),
                "results sorted"
            );
        }
        self.show_results();
    }

    /// Shows the results, and whether the last choice among them was kept:
    /// looked at again a little later while its write is not done.
    fn check_write(&self) {
        self.show_results();
        let state = self.0.borrow();
        if state.kept.as_ref().is_some_and(KeptSelections::writing) {
            let weak = Rc::downgrade(&self.0);
            state.write_check.start(
                slint::TimerMode::SingleShot,
                crate::saving::WRITE_CHECK,
                move || {
                    if let Some(state) = weak.upgrade() {
                        Self(state).check_write();
                    }
                },
            );
        }
    }

    /// Whether the results may change: never under an open confirmation,
    /// which shows what a cleaning takes, nor while root's scan, which
    /// changes them, is under way.
    fn results_frozen(&self, asked: &str) -> bool {
        let elevating = matches!(self.0.borrow().task, Some(Task::Elevating(_)));
        let frozen = self.is_confirming() || elevating;
        if frozen {
            tracing::warn!(
                asked,
                elevating,
                "refused while a cleaning waits for its confirmation, an answer about a browser, or the rights"
            );
        }
        frozen
    }

    /// Unchecks every group, for this analysis only: one click to clean
    /// one thing this time is no lasting choice (the choices kept are left).
    fn deselect_all(&self) {
        if self.is_settling("deselection") || self.results_frozen("deselection") {
            return;
        }
        if let Some(held) = self.0.borrow_mut().held.as_mut() {
            held.results.deselect_all();
        }
        self.show_results();
    }

    /// Shows the results held, if any.
    fn show_results(&self) {
        {
            let mut state = self.0.borrow_mut();
            if let Some(kept) = state.kept.as_mut() {
                state.choices_unsaved = !kept.all_kept();
            }
        }
        self.with_window(|window, state| {
            window
                .global::<ui::Cleaner>()
                .set_choices_unsaved(state.choices_unsaved);
            let Some(held) = &state.held else {
                return;
            };
            let results = &held.results;
            let rows = results
                .rows()
                .iter()
                .map(|row| result_row(results, row))
                .collect();
            show(&state.result_model, rows);
            let (entries, bytes) = results.selected();
            let cleaner = window.global::<ui::Cleaner>();
            let sorting = results.sorting();
            cleaner.set_sorted(sorting.is_some());
            if let Some(sorting) = sorting {
                cleaner.set_sort(column_shown(sorting.column));
                cleaner.set_descending(sorting.descending);
            }
            cleaner.set_selected_entries(shown(entries));
            cleaner.set_selected_operations(shown(usize::from(results.docker_checked().is_some())));
            cleaner.set_selected_size(size(bytes));
            cleaner.set_result_entries(shown(results.entries()));
            // From the results held, as shown (root's numbers once its scan
            // set a group): an analysis refused since showed its own count
            // in its place. While one runs, its count is shown.
            if !matches!(state.task, Some(Task::Analysis(_))) {
                cleaner.set_found(size(results.bytes()));
            }
            cleaner.set_rights_waiting(shown(results.waiting_for_rights()));
        });
    }

    fn cancel(&self) {
        if self.is_settling("cancellation") {
            return;
        }
        {
            let mut state = self.0.borrow_mut();
            let Some(Task::Analysis(analysis)) = state.task.as_mut() else {
                return;
            };
            if analysis.cancelled_for.is_some() {
                return;
            }
            tracing::info!("analysis cancelled by the user");
            analysis.running.cancel.cancel();
            analysis.cancelled_for = Some(Duration::ZERO);
        }
        self.set_step(ui::CleanerStep::Cancelling);
    }

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

    /// Back to the categories, the settings read again.
    fn back_to_choosing(&self) {
        if self.results_frozen("choosing again") {
            return;
        }
        self.0.borrow_mut().held = None;
        self.settle();
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_back_from_analysis(true);
            cleaner.set_step(ui::CleanerStep::Idle);
        });
        self.refresh();
    }

    /// Keeps the user's choices among the results from now on; when they
    /// could not be read, says so on the results, which then start as by
    /// default.
    pub fn keep_selections(&self, kept: Result<KeptSelections, Unkept>) {
        match kept {
            Ok(kept) => self.0.borrow_mut().kept = Some(kept),
            Err(Unkept::Unreadable(path)) => {
                if let Some(window) = self.0.borrow().window.upgrade() {
                    window
                        .global::<ui::Cleaner>()
                        .set_choices_unreadable(path.display().to_string().into());
                } else {
                    tracing::warn!("no window to say the choices are unreadable");
                }
            }
            // Rare, and traced: without a state directory the history is
            // not kept either.
            Err(Unkept::NoDirectory | Unkept::NoWriter) => {}
        }
    }

    /// Cancels a task under way, lets the last choice among the results be
    /// written meanwhile, then waits for the task a little: the window is
    /// closing. A cleaning is only left here when the user insisted on
    /// closing ([`Self::close_requested`]) or the event loop ended: its
    /// history line is then lost unless it finishes meanwhile.
    pub fn shut_down(&self) {
        let task = self.0.borrow_mut().task.take();
        if let Some(task) = &task {
            task.cancel().cancel();
        }
        // The last choice among the results is written whatever runs, while
        // the task stops.
        let mut state = self.0.borrow_mut();
        state.write_check.stop();
        if let Some(kept) = state.kept.as_mut() {
            kept.shut_down();
        }
        drop(state);
        let Some(task) = task else {
            return;
        };
        let started = Instant::now();
        while !task.is_finished() && started.elapsed() < STOP_WAIT {
            thread::sleep(STOP_POLL);
        }
        match (&task, task.is_finished()) {
            (_, true) => tracing::info!(waited = ?started.elapsed(), "task stopped on closing"),
            (Task::Analysis(analysis), false) => tracing::warn!(
                waited = ?started.elapsed(),
                path = ?analysis.view.path,
                "analysis left running on closing"
            ),
            // Launched, its input closes with the window: the helper cancels
            // itself once the question of pkexec is answered. Not launched
            // as last seen, the thread cancelled no longer launches it,
            // short of a launch already under way.
            (Task::Elevating(elevating), false) => tracing::warn!(
                waited = ?started.elapsed(),
                launched = elevating.launched,
                "root's scan left running on closing: the helper, if launched, cancels itself once pkexec's question is answered"
            ),
            (Task::Cleaning(_), false) => tracing::error!(
                waited = ?started.elapsed(),
                "cleaning left running on closing: its history line is not written"
            ),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use i_slint_backend_testing::{AccessibleRole, ElementHandle};
    use okapi_rules::Variable;
    use slint::Model as _;

    use super::*;
    use crate::results::Preselection;

    /// A group of the administrator's, locked, its rule preselected, which
    /// the user's own scan counted.
    fn locked() -> GroupFacts {
        GroupFacts {
            label: String::new(),
            label_alone: String::new(),
            description: String::new(),
            browser: None,
            categories: BTreeSet::new(),
            preselection: Preselection::Preselected,
            needs_admin: true,
            targets: Some(BTreeSet::from(["/var/crash".to_owned()])),
            admin: AdminState::Locked,
            checked_by_default: false,
            keys: BTreeSet::new(),
            seen: Seen::Counted {
                entries: 1,
                bytes: 1,
            },
            run_by: None,
        }
    }

    /// The journal's vacuum, locked.
    fn journal() -> GroupFacts {
        GroupFacts {
            preselection: Preselection::ToChoose,
            targets: Some(BTreeSet::from(["/var/log/journal".to_owned()])),
            seen: Seen::Operation,
            ..locked()
        }
    }

    fn with(base: GroupFacts, change: fn(&mut GroupFacts)) -> GroupFacts {
        let mut group = base;
        change(&mut group);
        group
    }

    fn assert_notes(cases: Vec<(GroupFacts, ui::ResultBadge, ui::RowNote)>) {
        for (group, badge, note) in cases {
            assert_eq!(
                (badge_of(&group), note_of(&group)),
                (badge, note),
                "{group:?}"
            );
        }
    }

    /// What a group left to the user says, unlocked ones included: never
    /// preselected (invariant 5); a cache reset or the trash is said at the
    /// confirmation alone.
    #[test]
    fn a_note_says_why_a_group_is_left_to_the_user() {
        let ordinary = GroupFacts {
            needs_admin: false,
            targets: None,
            checked_by_default: true,
            ..locked()
        };
        let unlocked = |group: &mut GroupFacts| {
            group.admin = AdminState::Unlocked {
                entries: 1,
                bytes: 1,
            };
        };
        assert_notes(vec![
            (
                ordinary.clone(),
                ui::ResultBadge::Preselected,
                ui::RowNote::None,
            ),
            (
                with(ordinary.clone(), |group| {
                    group.preselection = Preselection::CacheReset;
                    group.checked_by_default = false;
                }),
                ui::ResultBadge::ToChoose,
                ui::RowNote::None,
            ),
            (
                with(ordinary.clone(), |group| {
                    group.preselection = Preselection::ToChoose;
                    group.checked_by_default = false;
                }),
                ui::ResultBadge::ToChoose,
                ui::RowNote::None,
            ),
            // The trash: the confirmation, not the line, says it goes for good.
            (
                with(ordinary, |group| {
                    group.checked_by_default = false;
                    group.preselection = Preselection::GoneForGood;
                }),
                ui::ResultBadge::ToChoose,
                ui::RowNote::None,
            ),
            (
                with(locked(), unlocked),
                ui::ResultBadge::ToChoose,
                ui::RowNote::RightsObtained,
            ),
            (
                with(with(locked(), unlocked), |group| {
                    group.preselection = Preselection::CacheReset;
                }),
                ui::ResultBadge::ToChoose,
                ui::RowNote::RightsObtained,
            ),
            // The journal's vacuum, as any group root's scan unlocked.
            (
                with(journal(), unlocked),
                ui::ResultBadge::ToChoose,
                ui::RowNote::RightsObtained,
            ),
        ]);
    }

    /// Where a group out of reach stands until root's scan.
    #[test]
    fn a_note_says_where_a_group_out_of_reach_stands() {
        assert_notes(vec![
            (locked(), ui::ResultBadge::Admin, ui::RowNote::Locked),
            (
                with(locked(), |group| group.seen = Seen::Unreadable),
                ui::ResultBadge::Admin,
                ui::RowNote::Unseen,
            ),
            (journal(), ui::ResultBadge::Admin, ui::RowNote::Unmeasured),
            (
                with(locked(), |group| group.targets = None),
                ui::ResultBadge::Admin,
                ui::RowNote::Unreachable,
            ),
            (
                with(journal(), |group| group.admin = AdminState::NothingToClean),
                ui::ResultBadge::Admin,
                ui::RowNote::Nothing,
            ),
        ]);
    }

    /// A maintenance Okapi does not run: never "Administrator", whether it
    /// may be checked or not; its note says who runs it.
    #[test]
    fn a_note_says_who_runs_a_maintenance_okapi_does_not() {
        let not_okapis = |run_by| GroupFacts {
            needs_admin: false,
            targets: None,
            preselection: Preselection::ToChoose,
            seen: Seen::Operation,
            run_by: Some(run_by),
            ..locked()
        };
        let docker = |access| {
            not_okapis(RunBy::Docker(DaemonOperation {
                socket: PathBuf::from("/run/docker.sock"),
                access,
            }))
        };
        assert_notes(vec![
            (
                not_okapis(RunBy::User {
                    command: "uv cache prune",
                }),
                ui::ResultBadge::ToChoose,
                ui::RowNote::ByHand,
            ),
            (
                docker(Access::Allowed),
                ui::ResultBadge::ToChoose,
                ui::RowNote::Daemon,
            ),
            (
                docker(Access::NotAllowed),
                ui::ResultBadge::ToChoose,
                ui::RowNote::DaemonRefused,
            ),
        ]);
    }

    pub fn defaults() -> Inputs {
        Inputs {
            variables: TemplateVariables::new(),
            min_age_minutes: None,
            exclusions: Vec::new(),
            custom_folders: Vec::new(),
            deletion_log: false,
            state_directory: None,
        }
    }

    /// Seams reading the defaults, whose analysis, if asked for, stops
    /// before reading (no variable): nothing of the machine is read.
    pub fn seams_never_scanning() -> Seams {
        Seams {
            read: Rc::new(|| Ok(defaults())),
            // No home folder to analyze, no file manager to open.
            disk: crate::disk_usage::DiskSeams {
                home: None,
                cache: None,
                mounts: Rc::new(|| None),
                open: Rc::new(|_, done| done(false)),
                files: std::sync::Arc::new(okapi_tools::disk_usage::UsageScan::files),
                now: jiff::Timestamp::now,
                threads: std::num::NonZeroUsize::MIN,
            },
            // No settings read nor written.
            settings: crate::settings::tests::nowhere(),
            // No folder read, no trash opened.
            duplicates: crate::duplicates::inert(),
            large_files: crate::large_files::inert(),
            // No daemon asked: nothing on this system.
            updates: crate::updates::tests::nothing(),
            // Nothing read, nothing uninstalled.
            apps: crate::apps::tests::inert(),
            // No history read.
            history: crate::history::tests::nowhere(),
            // No Docker asked.
            docker: docker::tests::none(),
            ..Seams::system()
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
        let all_but_gpu = BTreeSet::from([
            Category::System,
            Category::Browsers,
            Category::Apps,
            Category::Gaming,
        ]);
        assert_eq!(choices.chosen(), all_but_gpu, "never a reset (invariant 5)");
        choices.toggle(4);
        assert!(choices.chosen().contains(&Category::Gpu));
        choices.toggle(4);
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

    pub fn checkbox(window: &ui::AppWindow, label: &str) -> ElementHandle {
        ElementHandle::find_by_accessible_label(window, label)
            .find(|element| element.accessible_role() == Some(AccessibleRole::Checkbox))
            .unwrap_or_else(|| panic!("no checkbox {label}"))
    }

    pub fn button(window: &ui::AppWindow, label: &str) -> ElementHandle {
        ElementHandle::find_by_accessible_label(window, label)
            .find(|element| element.accessible_role() == Some(AccessibleRole::Button))
            .unwrap_or_else(|| panic!("no button {label}"))
    }

    /// The buttons labelled `label` shown.
    pub fn buttons(window: &ui::AppWindow, label: &str) -> usize {
        ElementHandle::find_by_accessible_label(window, label)
            .filter(|element| element.accessible_role() == Some(AccessibleRole::Button))
            .count()
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

    /// Where `text` starts: on a page, after the sidebar and the page's
    /// margin.
    pub fn start_of(window: &ui::AppWindow, text: &str) -> f32 {
        ElementHandle::find_by_accessible_label(window, text)
            .find(|element| element.accessible_role() == Some(AccessibleRole::Text))
            .unwrap_or_else(|| panic!("no text {text}"))
            .absolute_position()
            .x
    }

    /// Sets the window's width, its height kept, and lets its `changed`
    /// handlers run.
    pub fn resize(window: &ui::AppWindow, width: u32) {
        let height = window.window().size().height;
        window
            .window()
            .set_size(slint::PhysicalSize::new(width, height));
        slint::platform::update_timers_and_animations();
    }

    pub fn shows(window: &ui::AppWindow, text: &str) -> bool {
        ElementHandle::find_by_accessible_label(window, text)
            .next()
            .is_some()
    }

    /// Every label read in the window, whatever reads it.
    pub fn labels_read(window: &ui::AppWindow) -> Vec<String> {
        use i_slint_backend_testing::ElementRoot as _;
        window
            .root_element()
            .query_descendants()
            .find_all()
            .into_iter()
            .filter_map(|element| element.accessible_label().map(String::from))
            .collect()
    }

    pub fn shows_a_text_starting(window: &ui::AppWindow, start: &str) -> bool {
        ElementHandle::find_by_element_type_name(window, "Text").any(|text| {
            text.accessible_label()
                .is_some_and(|label| label.starts_with(start))
        })
    }

    /// `seams`, the instant each analysis started from sent.
    fn noting_starts(seams: Seams) -> (Seams, Receiver<SystemTime>) {
        let (start, starts) = mpsc::channel();
        let work = Arc::clone(&seams.work);
        let seams = Seams {
            work: Arc::new(move |job, cancel, progress| {
                start.send(job.options.now()).unwrap();
                work(job, cancel, progress)
            }),
            ..seams
        };
        (seams, starts)
    }

    /// A summary no analysis of a sandbox can make.
    const EARLIER: LastAnalysis = LastAnalysis {
        time: 1,
        reclaimable_bytes: 2,
        entries: 3,
    };

    fn summary(sandbox: &Sandbox) -> Option<LastAnalysis> {
        LastAnalysis::load(&sandbox.root.join("state")).unwrap()
    }

    #[test]
    fn an_analysis_of_the_default_categories_keeps_its_summary() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        sandbox.file("home/.cache/pip/older");
        settle_files();
        EARLIER.save(&sandbox.root.join("state")).unwrap();
        let (seams, starts) = noting_starts(sandbox.read(&sandbox.root.join("state")));
        let (window, _screen) = on_cleaner(seams);
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        let kept = summary(&sandbox).unwrap();
        assert_eq!(kept.time, okapi_store::seconds(starts.recv().unwrap()));
        assert_eq!(
            (kept.reclaimable_bytes, kept.entries),
            (14, 2),
            "two files of 7 bytes"
        );
    }

    #[test]
    fn an_analysis_of_more_categories_than_the_default_ones_keeps_its_summary() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        EARLIER.save(&sandbox.root.join("state")).unwrap();
        let (window, _screen) = on_cleaner(sandbox.read(&sandbox.root.join("state")));
        checkbox(&window, "Caches GPU").invoke_accessible_default_action();
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        assert_eq!(summary(&sandbox).map(|kept| kept.entries), Some(1));
    }

    #[test]
    fn an_analysis_stopped_before_reading_keeps_the_summary_before() {
        let sandbox = Sandbox::new();
        EARLIER.save(&sandbox.root.join("state")).unwrap();
        let inputs = Inputs {
            variables: TemplateVariables::new(),
            ..sandbox.inputs(Some(&sandbox.root.join("state")))
        };
        let (window, _screen) = on_cleaner(Seams {
            read: Rc::new(move || Ok(inputs.clone())),
            ..seams_never_scanning()
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        assert_eq!(
            window.global::<ui::Cleaner>().get_outcome(),
            ui::OutcomeKind::NothingLooked
        );
        assert_eq!(summary(&sandbox), Some(EARLIER));
    }

    #[test]
    fn an_analysis_without_a_state_directory_shows_its_results() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let inputs = sandbox.inputs(None);
        let (window, _screen) = on_cleaner(Seams {
            read: Rc::new(move || Ok(inputs.clone())),
            ..seams_never_scanning()
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
    }

    /// The entries of the groups found something in, in their order.
    fn entries_shown(window: &ui::AppWindow) -> Vec<i32> {
        window
            .global::<ui::Cleaner>()
            .get_results()
            .iter()
            .filter(|row| row.kind == ui::RowKind::Group && row.entries > 0)
            .map(|row| row.entries)
            .collect()
    }

    #[test]
    fn a_header_sorts_the_groups_and_a_line_checks_the_group_it_shows() {
        // Yarn's two files, pip's one: « Cache de pip » before « Cache de
        // Yarn » by name, after it by entries and by size.
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/yarn/old");
        sandbox.file("home/.cache/yarn/older");
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let inputs = sandbox.inputs(None);
        let (window, _screen) = on_cleaner(Seams {
            read: Rc::new(move || Ok(inputs.clone())),
            ..seams_never_scanning()
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        let cleaner = window.global::<ui::Cleaner>();
        assert!(!cleaner.get_sorted(), "in the order of the rules");
        let_the_screen_settle();
        let sorted = |column| {
            cleaner.invoke_sort_by(column);
            (
                cleaner.get_sort(),
                cleaner.get_descending(),
                entries_shown(&window),
            )
        };
        assert_eq!(
            sorted(ui::ResultsSort::Name),
            (ui::ResultsSort::Name, false, vec![1, 2])
        );
        assert!(cleaner.get_sorted());
        assert_eq!(
            sorted(ui::ResultsSort::Name),
            (ui::ResultsSort::Name, true, vec![2, 1])
        );
        assert_eq!(
            sorted(ui::ResultsSort::Entries),
            (ui::ResultsSort::Entries, true, vec![2, 1])
        );
        assert_eq!(
            sorted(ui::ResultsSort::Size),
            (ui::ResultsSort::Size, true, vec![2, 1])
        );
        assert_eq!(
            sorted(ui::ResultsSort::Size),
            (ui::ResultsSort::Size, false, vec![1, 2])
        );
        // The line checked is the group it shows, sorted.
        let pip = cleaner
            .get_results()
            .iter()
            .position(|row| row.kind == ui::RowKind::Group && row.entries == 1)
            .unwrap();
        let before = cleaner.get_results().row_data(pip).unwrap().state;
        cleaner.invoke_toggle_result(i32::try_from(pip).unwrap());
        let after = cleaner.get_results().row_data(pip).unwrap();
        assert_eq!(after.entries, 1);
        assert_ne!(after.state, before);
        // A new analysis: in the order of the rules again.
        cleaner.invoke_analyze();
        watch_until(&window, |window| {
            step(window) == ui::CleanerStep::Results && !window.global::<ui::Cleaner>().get_sorted()
        });
    }

    #[test]
    fn an_analysis_with_nothing_to_clean_keeps_a_summary_at_zero() {
        let sandbox = Sandbox::new();
        EARLIER.save(&sandbox.root.join("state")).unwrap();
        let (window, _screen) = on_cleaner(sandbox.read(&sandbox.root.join("state")));
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        assert_eq!(
            window.global::<ui::Cleaner>().get_outcome(),
            ui::OutcomeKind::NothingToClean
        );
        let kept = summary(&sandbox).unwrap();
        assert_eq!((kept.reclaimable_bytes, kept.entries), (0, 0));
    }

    #[test]
    fn an_analysis_of_fewer_categories_than_the_default_ones_keeps_the_summary_before() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        EARLIER.save(&sandbox.root.join("state")).unwrap();
        let (window, _screen) = on_cleaner(sandbox.read(&sandbox.root.join("state")));
        checkbox(&window, "Jeux").invoke_accessible_default_action();
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        assert_eq!(summary(&sandbox), Some(EARLIER));
    }

    #[test]
    fn a_cancelled_analysis_keeps_the_summary_before() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        EARLIER.save(&sandbox.root.join("state")).unwrap();
        let (seams, scan) = scripted(Vec::new());
        let (window, _screen) = on_cleaner(Seams {
            read: sandbox.read(&sandbox.root.join("state")).read,
            ..seams
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        let_the_screen_settle();
        button(&window, "Annuler l'analyse").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Cancelling);
        scan.go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Idle);
        assert_eq!(summary(&sandbox), Some(EARLIER));
    }

    #[test]
    fn a_summary_that_cannot_be_kept_leaves_the_results_shown() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        let not_a_directory = sandbox.file("state-is-a-file");
        settle_files();
        let (window, _screen) = on_cleaner(sandbox.read(&not_a_directory));
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        assert_eq!(
            fs::read(&not_a_directory).unwrap(),
            b"content",
            "nothing written over it"
        );
    }

    /// A home, a temporary directory and a state directory below the build
    /// directory, never below a target of the rules (`/tmp` is one), with
    /// every absolute target of the rules excluded: the analysis and the
    /// cleaning reach nothing of the machine.
    pub struct Sandbox {
        _directory: tempfile::TempDir,
        pub root: PathBuf,
    }

    impl Sandbox {
        pub fn new() -> Self {
            let directory = tempfile::TempDir::new_in(env!("OUT_DIR")).unwrap();
            let root = fs::canonicalize(directory.path()).unwrap();
            for target in Self::excluded() {
                assert!(
                    !root.starts_with(&target),
                    "the sandbox {} lies below {}",
                    root.display(),
                    target.display()
                );
            }
            for directory in [
                "home/.cache",
                "home/.config",
                "home/.local/share",
                "tmp",
                "state",
            ] {
                fs::create_dir_all(root.join(directory)).unwrap();
            }
            Self {
                _directory: directory,
                root,
            }
        }

        pub fn excluded() -> Vec<PathBuf> {
            RuleSet::embedded(Os::Linux)
                .unwrap()
                .absolute_targets(false)
        }

        pub fn file(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"content").unwrap();
            path
        }

        pub fn variables(&self) -> TemplateVariables {
            let mut variables = TemplateVariables::new();
            for (variable, relative) in [
                (Variable::Home, "home"),
                (Variable::TempDirectory, "tmp"),
                (Variable::Config, "home/.config"),
                (Variable::Cache, "home/.cache"),
                (Variable::LocalShare, "home/.local/share"),
            ] {
                variables
                    .insert(variable, self.root.join(relative))
                    .unwrap();
            }
            variables
        }

        /// The settings: no minimum age (the files are fresh), every
        /// absolute target excluded, a deletion log, and the history here.
        pub fn inputs(&self, state_directory: Option<&Path>) -> Inputs {
            Inputs {
                variables: self.variables(),
                min_age_minutes: Some(0),
                exclusions: Self::excluded(),
                custom_folders: Vec::new(),
                deletion_log: true,
                state_directory: state_directory.map(Path::to_path_buf),
            }
        }

        pub fn read(&self, state_directory: &Path) -> Seams {
            let inputs = self.inputs(Some(state_directory));
            Seams {
                read: Rc::new(move || Ok(inputs.clone())),
                ..seams_never_scanning()
            }
        }

        /// The cleaning lines of the history (their deletion logs, on the
        /// lines before them, are [`Self::deletion_logs`]).
        pub fn history(&self) -> Vec<serde_json::Value> {
            self.history_lines()
                .into_iter()
                .filter(|line| line.get("kind").is_none())
                .collect()
        }

        /// The entries of the deletion log of each cleaning, in order.
        pub fn deletion_logs(&self) -> Vec<serde_json::Value> {
            let lines = self.history_lines();
            assert!(
                lines.last().is_none_or(|line| line["kind"] != "deletions"),
                "a deletion log without its cleaning line"
            );
            lines
                .iter()
                .zip(lines.iter().skip(1))
                .filter(|(line, _)| line["kind"] == "deletions")
                .map(|(log, cleaning)| {
                    assert_eq!(log["time"], cleaning["time"], "its cleaning's, next");
                    assert_eq!(cleaning.get("kind"), None);
                    log["entries"].clone()
                })
                .collect()
        }

        fn history_lines(&self) -> Vec<serde_json::Value> {
            fs::read_to_string(self.root.join("state/history.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    /// File dates follow a coarse clock: a pause puts what was just written
    /// before the analysis's own `now`.
    pub fn settle_files() {
        std::thread::sleep(Duration::from_millis(50));
    }

    /// The window on the cleaner, in French, reading and scanning with
    /// `seams`.
    pub fn on_cleaner(seams: Seams) -> (ui::AppWindow, CleanerScreen) {
        on_cleaner_in(seams, Language::French)
    }

    /// The window on the cleaner, in `language`.
    pub fn on_cleaner_in(seams: Seams, language: Language) -> (ui::AppWindow, CleanerScreen) {
        let crate::Screens {
            window,
            cleaner: screen,
            ..
        } = crate::wide_window(seams, crate::dashboard::tests::inert(), language);
        let cleaner = match language {
            Language::French => "Nettoyeur",
            Language::English => "Cleaner",
        };
        button(&window, cleaner).invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        (window, screen)
    }

    pub fn step(window: &ui::AppWindow) -> ui::CleanerStep {
        window.global::<ui::Cleaner>().get_step()
    }

    #[test]
    fn the_categories_offered_leave_cache_resets_unchecked() {
        let (window, _screen) = on_cleaner(seams_never_scanning());
        for label in ["Système", "Navigateurs", "Applications", "Jeux"] {
            assert_eq!(
                checkbox(&window, label).accessible_checked(),
                Some(true),
                "{label}"
            );
        }
        let gpu = checkbox(&window, "Caches GPU");
        assert_eq!(gpu.accessible_checked(), Some(false), "invariant 5");
        // Its consequence is said with the results, never beside its box.
        assert_eq!(
            gpu.accessible_description().as_deref(),
            Some("Shaders Mesa et NVIDIA.")
        );
        // Said once, by its box: written below its name, silent.
        assert!(!shows(&window, "Shaders Mesa et NVIDIA."));
        assert!(shows(
            &window,
            "Toujours épargnés : fichiers modifiés depuis moins de 60 min. Exclusions : 0."
        ));
        gpu.invoke_accessible_default_action();
        assert_eq!(gpu.accessible_checked(), Some(true), "the user's to check");
        gpu.invoke_accessible_default_action();

        for label in ["Système", "Navigateurs", "Applications", "Jeux"] {
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
            "Les réglages sont illisibles : rien ne peut démarrer. Le détail est dans le journal."
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
    pub fn watch_until(window: &ui::AppWindow, done: impl Fn(&ui::AppWindow) -> bool) {
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
    pub fn let_the_screen_settle() {
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
    pub struct Scripted {
        pub go: Sender<()>,
        pub started: Receiver<ScanOptions>,
        sent: Receiver<()>,
    }

    pub fn scripted(script: Vec<Progress>) -> (Seams, Scripted) {
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
                ..seams_never_scanning()
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
        // Neither a reset nor the user's folders, none of which is named.
        let by_default = Category::ALL
            .into_iter()
            .filter(|category| ![Category::Gpu, Category::Custom].contains(category));
        assert_eq!(*options.categories(), by_default.collect());
        assert_eq!(step(&window), ui::CleanerStep::Planning);
        assert_eq!(
            rows(&window),
            [
                (ui::CategoryKind::System, ui::RowState::Waiting),
                (ui::CategoryKind::Browsers, ui::RowState::Waiting),
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
                (ui::CategoryKind::Browsers, ui::RowState::Done),
                (ui::CategoryKind::Apps, ui::RowState::Done),
                (ui::CategoryKind::Gaming, ui::RowState::Done),
            ]
        );
        assert!(shows(&window, "Catégories terminées : 3 sur 4."));
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
            ["/okapi-test/walked", "", "", ""],
            "on the category under way only"
        );

        scan.go.send(()).unwrap();
        scan.sent.recv().unwrap();
        watch_until(&window, |window| {
            window.global::<ui::Cleaner>().get_rows_done() == 4
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
        // Said by its line, its written count silent.
        assert!(!shows(&window, "Éléments : 4"));

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
    fn the_analysis_and_its_end_narrow_their_margins_in_a_narrow_window() {
        let (seams, scan) = scripted(Vec::new());
        let (window, _screen) = on_cleaner(seams);
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        let_the_screen_settle();
        let wide = start_of(&window, "Analyse en cours");
        assert!((wide - (248.0 + 32.0)).abs() < 1.0, "{wide}");
        // Narrowed while it is shown: the sidebar folded, the narrow margin.
        resize(&window, 1000);
        let narrow = start_of(&window, "Analyse en cours");
        assert!((narrow - (64.0 + 24.0)).abs() < 1.0, "{narrow}");
        scan.go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        assert!((start_of(&window, "Nettoyeur") - (64.0 + 24.0)).abs() < 1.0);
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
    fn the_endings_say_what_was_noted() {
        let (window, screen) = on_cleaner(seams_never_scanning());
        screen.end(Some(Outcome::NothingToClean { issues: 2 }));
        assert_eq!(step(&window), ui::CleanerStep::Outcome);
        assert!(shows(&window, "Rien à nettoyer"));
        assert!(shows(
            &window,
            "Remarques de l'analyse : 2. Le détail est dans le journal."
        ));
        assert!(shows_a_text_starting(
            &window,
            "Okapi n'a rien trouvé de récupérable dans ce qu'il a pu lire."
        ));
        screen.end(Some(Outcome::NothingToClean { issues: 0 }));
        assert!(shows_a_text_starting(
            &window,
            "Okapi a regardé les catégories choisies"
        ));
        assert!(!shows_a_text_starting(&window, "Remarques de l'analyse"));
    }

    pub const EXCLUSION_REFUSED: &str =
        "Une exclusion des réglages est refusée : rien n'a démarré. Le détail est dans le journal.";

    /// The settings `inputs`, an exclusion the engine refuses added while
    /// `refused` says so.
    pub fn refused_when(inputs: Inputs, refused: &Rc<Cell<bool>>) -> Read {
        let refused = Rc::clone(refused);
        Rc::new(move || {
            let mut read = inputs.clone();
            if refused.get() {
                read.exclusions.push(PathBuf::from("relative"));
            }
            Ok(read)
        })
    }

    /// A scan that only counts how often it is run.
    fn counted_work(runs: &Arc<AtomicUsize>) -> Work {
        let runs = Arc::clone(runs);
        Arc::new(move |job, cancel, progress| {
            runs.fetch_add(1, Ordering::SeqCst);
            scan_watched(
                &job.rules,
                &job.variables,
                &job.options,
                &Watch::new(cancel, Some(progress)),
            )
        })
    }

    #[test]
    fn a_new_analysis_refused_says_why_on_the_ending() {
        let refused = Rc::new(Cell::new(false));
        let minutes = Rc::new(Cell::new(None));
        let read_minutes = Rc::clone(&minutes);
        let read_refused = refused_when(defaults(), &refused);
        let (window, screen) = on_cleaner(Seams {
            read: Rc::new(move || {
                read_refused().map(|inputs| Inputs {
                    min_age_minutes: read_minutes.get(),
                    ..inputs
                })
            }),
            ..seams_never_scanning()
        });
        screen.end(Some(Outcome::NothingToClean { issues: 0 }));
        minutes.set(Some(u64::MAX));
        screen.refresh();
        show_page(&window, "Accueil");
        show_page(&window, "Nettoyeur");
        assert_eq!(step(&window), ui::CleanerStep::Outcome);
        // "Nouvelle analyse" greyed (a minimum age beyond the clock), the
        // focus goes to the button that still works.
        press(&window, slint::platform::Key::Return);
        assert_eq!(step(&window), ui::CleanerStep::Idle);
        assert!(shows(
            &window,
            "L'âge minimal des réglages remonte avant l'horloge : rien ne serait assez ancien."
        ));
        // An exclusion refused is only met on the analysis's thread, which
        // goes back to choosing to say it.
        minutes.set(None);
        refused.set(true);
        show_page(&window, "Accueil");
        show_page(&window, "Nettoyeur");
        screen.end(Some(Outcome::NothingToClean { issues: 0 }));
        let_the_screen_settle();
        button(&window, "Nouvelle analyse").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Planning);
        watch_until(&window, refused_for_an_exclusion);
        assert_eq!(step(&window), ui::CleanerStep::Idle);
        assert!(shows(&window, EXCLUSION_REFUSED));
    }

    /// Shows the page of the sidebar's entry `label`, and lets the window
    /// hear it (a change of page is told on the next turn).
    pub fn show_page(window: &ui::AppWindow, label: &str) {
        button(window, label).invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
    }

    /// Whether the window says an exclusion was refused.
    pub fn refused_for_an_exclusion(window: &ui::AppWindow) -> bool {
        window.global::<ui::Cleaner>().get_problem() == ui::SetupProblem::Exclusion
    }

    #[test]
    fn a_refusal_met_on_a_thread_stays_said_while_the_exclusions_are_the_same() {
        let refused = Rc::new(Cell::new(true));
        let (window, screen) = on_cleaner(Seams {
            read: refused_when(defaults(), &refused),
            ..seams_never_scanning()
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        show_page(&window, "Accueil");
        watch_until(&window, refused_for_an_exclusion);
        // Each reading of the settings (the page shown, a setting written)
        // says it again while they hold the same exclusions, whatever page
        // was shown when it came.
        for _ in 0..2 {
            show_page(&window, "Nettoyeur");
            assert!(shows(&window, EXCLUSION_REFUSED));
            show_page(&window, "Accueil");
        }
        screen.refresh();
        assert!(refused_for_an_exclusion(&window));
        // Other exclusions read: it is no more said.
        refused.set(false);
        screen.refresh();
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::None
        );
    }

    #[test]
    fn a_refusal_met_on_a_thread_outlives_a_reading_that_fails() {
        let refused = Rc::new(Cell::new(true));
        let saving = Rc::new(Cell::new(false));
        let read_saving = Rc::clone(&saving);
        let read_refused = refused_when(defaults(), &refused);
        let (window, screen) = on_cleaner(Seams {
            read: Rc::new(move || {
                if read_saving.get() {
                    Err(Problem::SettingsSaving)
                } else {
                    read_refused()
                }
            }),
            ..seams_never_scanning()
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, refused_for_an_exclusion);
        // Unread, the exclusions may be the same: the refusal waits.
        saving.set(true);
        screen.refresh();
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::SettingsSaving
        );
        saving.set(false);
        screen.refresh();
        assert!(refused_for_an_exclusion(&window));
    }

    #[test]
    fn a_problem_of_the_settings_comes_before_a_refusal_met_on_a_thread() {
        let refused = Rc::new(Cell::new(true));
        let minutes = Rc::new(Cell::new(None));
        let read_minutes = Rc::clone(&minutes);
        let read_refused = refused_when(defaults(), &refused);
        let (window, screen) = on_cleaner(Seams {
            read: Rc::new(move || {
                read_refused().map(|inputs| Inputs {
                    min_age_minutes: read_minutes.get(),
                    ..inputs
                })
            }),
            ..seams_never_scanning()
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        // The settings read again while the thread resolves: their own
        // problem is said, and the refusal does not cover it.
        minutes.set(Some(u64::MAX));
        screen.refresh();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Idle);
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::MinAge
        );
        // Read again with both standing: the settings' own problem first.
        screen.refresh();
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::MinAge
        );
        // Kept, the refusal is said once the settings' own problem is over.
        minutes.set(None);
        screen.refresh();
        assert!(refused_for_an_exclusion(&window));
    }

    #[test]
    #[cfg(unix)]
    fn an_exclusion_that_resolves_again_is_no_more_said_once_an_analysis_starts() {
        use std::os::unix::fs::PermissionsExt as _;
        let folder = tempfile::tempdir().unwrap();
        let locked = folder.path().join("locked");
        fs::create_dir(&locked).unwrap();
        // Unresolvable, as a mount that stopped answering would leave it.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let exclusion = locked.join("keep");
        let (window, _screen) = on_cleaner(Seams {
            read: Rc::new(move || {
                Ok(Inputs {
                    exclusions: vec![exclusion.clone()],
                    ..defaults()
                })
            }),
            ..seams_never_scanning()
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, refused_for_an_exclusion);
        // Readable again with the same settings: the analysis started leaves
        // the refusal behind.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let_the_screen_settle();
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        show_page(&window, "Accueil");
        show_page(&window, "Nettoyeur");
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::None
        );
    }

    #[test]
    fn an_exclusion_refused_starts_no_analysis() {
        let refused = Rc::new(Cell::new(true));
        let runs = Arc::new(AtomicUsize::new(0));
        let (window, _screen) = on_cleaner(Seams {
            read: refused_when(defaults(), &refused),
            work: counted_work(&runs),
            ..seams_never_scanning()
        });
        let cleaner = window.global::<ui::Cleaner>();
        // Nothing resolved on the window's thread: the analysis starts, and
        // its own thread meets the refusal.
        assert_eq!(cleaner.get_problem(), ui::SetupProblem::None);
        button(&window, "Analyser").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Planning);
        assert_eq!(cleaner.get_problem(), ui::SetupProblem::None);
        watch_until(&window, refused_for_an_exclusion);
        assert_eq!(step(&window), ui::CleanerStep::Idle);
        assert!(shows(&window, EXCLUSION_REFUSED));
        assert_eq!(runs.load(Ordering::SeqCst), 0, "nothing scanned");
        // A mount may answer again: the analysis may be asked for at once.
        refused.set(false);
        let_the_screen_settle();
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        // Started, the analysis left the refusal behind.
        show_page(&window, "Accueil");
        show_page(&window, "Nettoyeur");
        assert_eq!(cleaner.get_problem(), ui::SetupProblem::None);
    }

    #[test]
    fn an_exclusion_refused_after_cancelling_is_said_back_at_choosing() {
        let refused = Rc::new(Cell::new(true));
        let (window, screen) = on_cleaner(Seams {
            read: refused_when(defaults(), &refused),
            ..seams_never_scanning()
        });
        button(&window, "Analyser").invoke_accessible_default_action();
        // Cancelled before its thread is looked at: the double click guard
        // lifted by hand, as no time may pass.
        screen.0.borrow().settling.stop();
        screen.cancel();
        assert_eq!(step(&window), ui::CleanerStep::Cancelling);
        watch_until(&window, refused_for_an_exclusion);
        assert_eq!(step(&window), ui::CleanerStep::Idle);
    }

    #[test]
    fn a_minimum_age_beyond_the_clock_is_refused_at_the_click() {
        let minutes = Rc::new(Cell::new(None));
        let read_minutes = Rc::clone(&minutes);
        let runs = Arc::new(AtomicUsize::new(0));
        let (window, _screen) = on_cleaner(Seams {
            read: Rc::new(move || {
                Ok(Inputs {
                    min_age_minutes: read_minutes.get(),
                    ..defaults()
                })
            }),
            work: counted_work(&runs),
            ..seams_never_scanning()
        });
        // Changed since the page was shown: said at the click, nothing run.
        minutes.set(Some(u64::MAX));
        button(&window, "Analyser").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Idle);
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::MinAge
        );
        assert_eq!(runs.load(Ordering::SeqCst), 0);
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

    /// The screens the sidebar leads to, each a stop of Tab; in a narrow
    /// window, its only stops.
    pub const SIDEBAR_ENTRIES: usize = 10;
    /// The groups' titles of the unfolded sidebar, each a stop of Tab.
    pub const SIDEBAR_GROUPS: usize = okapi_store::SidebarGroup::ALL.len();
    /// The stops of Tab in the sidebar of a wide window, every group open:
    /// its fold button, then its entries and its groups' titles.
    pub const SIDEBAR_STOPS: usize = 1 + SIDEBAR_ENTRIES + SIDEBAR_GROUPS;

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
        // The sidebar, then the first category.
        for _ in 0..=SIDEBAR_STOPS {
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
        press(&window, Key::Space);
        assert_eq!(
            checkbox(&window, "Système").accessible_checked(),
            Some(true)
        );
    }

    #[test]
    fn the_keyboard_reaches_analyser_and_a_held_enter_never_cancels() {
        use slint::platform::Key;

        let (seams, scan) = scripted(Vec::new());
        let (window, _screen) = on_cleaner(seams);
        // The sidebar, then a stop per category (five) and "Analyser".
        for _ in 0..SIDEBAR_STOPS + 6 {
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

    #[test]
    fn the_user_s_folders_are_offered_unchecked_only_when_the_settings_name_some() {
        let rules = embedded();
        let mut choices = Choices::of(Some(&rules));
        let by_default = choices.chosen();
        assert_eq!(choices.0.len(), Category::ALL.len() - 1, "none named");
        choices.offer_custom_folders(true);
        choices.offer_custom_folders(true);
        assert_eq!(choices.0.len(), Category::ALL.len(), "offered once");
        assert_eq!(choices.chosen(), by_default, "never chosen beforehand");
        choices.toggle(Category::ALL.len() - 1);
        assert!(choices.chosen().contains(&Category::Custom));
        choices.offer_custom_folders(false);
        assert_eq!(choices.chosen(), by_default, "withdrawn, not chosen");
    }

    #[test]
    fn a_folder_to_clean_not_written_absolute_stops_everything() {
        let inputs = |folders: &[&str]| Inputs {
            custom_folders: folders.iter().map(PathBuf::from).collect(),
            ..defaults()
        };
        let now = SystemTime::now();
        assert_eq!(settings_problem(&inputs(&["/data/build"]), now), None);
        for written in ["relatif", "/data/../etc", ""] {
            let refused = inputs(&["/data/build", written]);
            assert_eq!(
                settings_problem(&refused, now),
                Some(Problem::CustomFolder),
                "{written}"
            );
            assert_eq!(
                options(&refused, now, BTreeSet::new()),
                Err(Problem::CustomFolder)
            );
        }
        let (window, screen) = on_cleaner(Seams {
            read: Rc::new(move || Ok(inputs(&["relatif"]))),
            ..seams_never_scanning()
        });
        screen.refresh();
        let cleaner = window.global::<ui::Cleaner>();
        assert_eq!(cleaner.get_problem(), ui::SetupProblem::CustomFolder);
        assert!(!cleaner.get_analysis_allowed());
        assert_eq!(
            cleaner.get_choices().row_count(),
            Category::ALL.len(),
            "its card offered, to be seen with the problem"
        );
    }

    #[test]
    fn a_folder_to_clean_shows_by_its_path_unchecked_and_outside_the_estimate() {
        let sandbox = Sandbox::new();
        sandbox.file("work/build/old.o");
        let build = sandbox.root.join("work/build");
        settle_files();
        let inputs = Inputs {
            custom_folders: vec![build],
            ..sandbox.inputs(Some(&sandbox.root.join("state")))
        };
        let never = seams_never_scanning();
        let (window, screen) = on_cleaner(Seams {
            read: Rc::new(move || Ok(inputs.clone())),
            // `~` is the folder above it.
            disk: crate::disk_usage::DiskSeams {
                home: Some(sandbox.root.join("work")),
                ..never.disk.clone()
            },
            ..never
        });
        screen.refresh();
        let cleaner = window.global::<ui::Cleaner>();
        let custom = Category::ALL.len() - 1;
        assert_eq!(
            cleaner
                .get_choices()
                .row_data(custom)
                .map(|choice| choice.kind),
            Some(ui::CategoryKind::Custom)
        );
        cleaner.invoke_toggle(i32::try_from(custom).unwrap());
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        let row = cleaner
            .get_results()
            .iter()
            .find(|row| row.kind == ui::RowKind::Group && row.category == ui::CategoryKind::Custom)
            .expect("the folder's group");
        assert_eq!(row.label, "~/build", "its path, never translated");
        assert_eq!(row.badge, ui::ResultBadge::ToChoose);
        assert_eq!(row.note, ui::RowNote::None, "nothing said under its line");
        assert_eq!(
            row.state,
            ui::CheckState::Unchecked,
            "nothing checked beforehand"
        );
        assert!(row.enabled, "the user's to check");
        // No estimate kept from an analysis of them.
        assert_eq!(summary(&sandbox), None);
    }
}
