//! Cleaning what the user checked in the results: a confirmation, the
//! cleaning on its own thread, which writes the history line itself before
//! it reports, and the receipt.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime};

use okapi_engine::{
    BrowserLock, Cancel, CleanPass, CleanReceipt, CleanResult, ItemId, KeepReason, Progress,
    ScanItem, ScanOptions, ScanReport, Stop,
};
use okapi_helper::{Operation, OutcomeAnswer, Reply, ReplyError};
use okapi_rules::{RuleSet, TemplateVariables};
use okapi_store::{
    AdminRecord, History, HistoryEntry, HistoryOutcome, Opened, OperationEnd, seconds,
};
use slint::ComponentHandle as _;

use super::docker::{self, Pruning, operation_shown};
use super::{
    CleanerEvent, CleanerScreen, Inputs, Problem, Running, Task, options, problem, rights,
    settings_problem, stop_shown,
};
use crate::elevation::{ElevationFailure, ElevationStep};
use crate::receipt::{Ending, Kept, LineKind, OutcomeFacts, Receipt, ReceiptLine};
use crate::shown::{show, shown, size};
use crate::ui;

/// What a cleaning takes: identifiers of the report kept, and only them
/// (invariant 1), with the rules, variables and options built as it starts;
/// and the request of root's part, which names the rules' targets of the
/// groups that need the administrator, never a path.
#[derive(Debug)]
pub struct CleanJob {
    pub report: Arc<ScanReport>,
    pub ids: Vec<ItemId>,
    pub rules: Arc<RuleSet>,
    pub variables: TemplateVariables,
    pub options: ScanOptions,
    pub admin: Option<Vec<u8>>,
}

/// What root's part of a cleaning gave.
pub type AdminDone = Result<Reply, ElevationFailure>;

/// What became of the history line of a cleaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryWrite {
    Recorded,
    Failed,
    /// No history could be opened (without a deletion log asked for).
    Unavailable,
}

/// A cleaning's receipt, root's part, Docker's prune, and its history line.
#[derive(Debug)]
pub struct Cleaned {
    receipt: CleanReceipt,
    admin: Option<AdminDone>,
    docker: Option<Pruning>,
    history: HistoryWrite,
}

/// The cleaning under way, as its progress tells it.
#[derive(Debug, Default)]
struct CleanView {
    /// Root's part under way: its question, then its work.
    rights: Option<ElevationStep>,
    /// Root's part over: whether it was done to its end.
    admin_done: Option<bool>,
    /// The cleaning asks Docker to prune, once the user's part is over.
    had_docker: bool,
    /// Docker was asked to prune.
    docker_asked: bool,
    /// How far each pass of the user's part went: entries done, of all.
    check: Option<(usize, usize)>,
    remove: Option<(usize, usize)>,
    freed: u64,
    /// The user cancelled: the passes stay where they were then, the
    /// entries left after it never counted as gone through.
    cancel_asked: bool,
}

impl CleanView {
    /// The view of a cleaning yet to start, which asks Docker to prune
    /// when `had_docker`.
    fn new(had_docker: bool) -> Self {
        Self {
            had_docker,
            ..Self::default()
        }
    }

    /// Every entry went through the check.
    fn check_over(&self) -> bool {
        self.remove.is_some() || matches!(self.check, Some((done, total)) if done == total)
    }

    /// The steps as they stand: root's part if the cleaning has one, the
    /// two passes over the user's `entries` if any, Docker's prune if asked
    /// for, then the history line.
    fn steps(&self, had_admin: bool, entries: usize) -> Vec<ui::CleanStep> {
        let pass = |kind, reached: Option<(usize, usize)>, over: bool| {
            let (done, total) = reached.unwrap_or((0, entries));
            let state = match reached {
                None => ui::RowState::Waiting,
                Some(_) if over || done == total => ui::RowState::Done,
                Some(_) => ui::RowState::Running,
            };
            clean_step(kind, state, done, total)
        };
        let mut steps = Vec::new();
        if had_admin {
            let state = match self.admin_done {
                None => ui::RowState::Running,
                Some(true) => ui::RowState::Done,
                Some(false) => ui::RowState::NotDone,
            };
            steps.push(clean_step(ui::CleanStepKind::Admin, state, 0, 0));
        }
        if entries > 0 {
            steps.push(pass(
                ui::CleanStepKind::Check,
                self.check,
                self.check_over(),
            ));
            steps.push(pass(ui::CleanStepKind::Remove, self.remove, false));
        }
        if self.had_docker {
            let state = if self.docker_asked {
                ui::RowState::Running
            } else {
                ui::RowState::Waiting
            };
            steps.push(clean_step(ui::CleanStepKind::Docker, state, 0, 0));
        }
        steps.push(clean_step(
            ui::CleanStepKind::History,
            ui::RowState::Waiting,
            0,
            0,
        ));
        steps
    }

    /// The steps once the cleaning is over (`ended`, none when its thread
    /// died): each done to its end, or not done.
    fn ended_steps(
        &self,
        had_admin: bool,
        entries: usize,
        ended: Option<Ended>,
    ) -> Vec<ui::CleanStep> {
        let mut steps = self.steps(had_admin, entries);
        let Some(ended) = ended else {
            // What it had not finished is not done.
            for step in &mut steps {
                if step.state != ui::RowState::Done {
                    step.state = ui::RowState::NotDone;
                }
            }
            return steps;
        };
        let Ended {
            admin_done,
            user,
            docker_done,
            recorded,
        } = ended;
        for step in &mut steps {
            let done = match step.kind {
                ui::CleanStepKind::Admin => admin_done,
                ui::CleanStepKind::Check => match user {
                    UserEnd::Whole => true,
                    UserEnd::Cancelled => self.cancel_asked && self.check_over(),
                    UserEnd::Stopped => false,
                },
                ui::CleanStepKind::Remove => user == UserEnd::Whole,
                ui::CleanStepKind::Docker => docker_done,
                ui::CleanStepKind::History => recorded,
            };
            if done {
                // Done to its end, whatever the view held when cancelled
                // too late.
                step.done = step.total;
            }
            step.state = if done {
                ui::RowState::Done
            } else {
                ui::RowState::NotDone
            };
        }
        steps
    }
}

/// What a cleaning that ended says of its steps.
#[derive(Debug, Clone, Copy)]
struct Ended {
    /// Root's part was done to its end.
    admin_done: bool,
    user: UserEnd,
    /// Docker's prune ran to its end.
    docker_done: bool,
    /// The history line is written.
    recorded: bool,
}

/// How the user's part of a cleaning ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserEnd {
    /// Every entry went through both passes.
    Whole,
    /// Entries were left by a cancellation.
    Cancelled,
    /// It stopped before it touched anything.
    Stopped,
}

impl Ended {
    fn of(cleaned: &Cleaned) -> Self {
        let receipt = &cleaned.receipt;
        let user = if !receipt.stops().is_empty() {
            UserEnd::Stopped
        } else if receipt
            .outcomes()
            .iter()
            .any(|outcome| outcome.result() == CleanResult::Kept(KeepReason::Cancelled))
        {
            UserEnd::Cancelled
        } else {
            UserEnd::Whole
        };
        Self {
            admin_done: cleaned.admin.as_ref().is_some_and(admin_done),
            user,
            docker_done: cleaned.docker.as_ref().is_some_and(Pruning::is_done),
            recorded: cleaned.history == HistoryWrite::Recorded,
        }
    }
}

fn clean_step(
    kind: ui::CleanStepKind,
    state: ui::RowState,
    done: usize,
    total: usize,
) -> ui::CleanStep {
    ui::CleanStep {
        kind,
        state,
        done: shown(done),
        total: shown(total),
    }
}

/// A cleaning under way. The screen never gives it up: a removal cut short
/// would leave no history line (only the window closing with the user
/// insisting, or the event loop ending, leaves it).
pub(super) struct Cleaning {
    pub(super) running: Running<Result<Cleaned, Problem>>,
    steps: Receiver<ElevationStep>,
    /// Root's part over: whether it was done to its end.
    admin_ended: Receiver<bool>,
    /// Docker was asked to prune.
    docker_asked: Receiver<()>,
    view: CleanView,
    report: Arc<ScanReport>,
    /// It had a part of root's.
    had_admin: bool,
    /// The entries of the user's part.
    entries: usize,
    /// The user asked for every entry in the history.
    deletion_log: bool,
    /// The window asked to close: it hides once the receipt is in.
    close_when_done: bool,
    /// The exclusions of the settings it started with, which its thread
    /// resolves.
    exclusions: Vec<PathBuf>,
}

impl Cleaning {
    /// Takes in what the cleaning's thread said since the last look.
    fn take_progress(&mut self) {
        let view = &mut self.view;
        if let Some(step) = self.steps.try_iter().last()
            && view.admin_done.is_none()
        {
            view.rights = Some(step);
        }
        if self.docker_asked.try_iter().last().is_some() {
            view.docker_asked = true;
        }
        if let Some(done) = self.admin_ended.try_iter().last() {
            view.admin_done = Some(done);
            view.rights = None;
        }
        for progress in self.running.progress.try_iter() {
            if let Progress::Cleaning {
                pass,
                done,
                total,
                removed_bytes,
            } = progress
            {
                // The user's part: root's is over.
                view.rights = None;
                if view.cancel_asked || self.running.cancel.is_cancelled() {
                    // Cancelled, by the user or by a "no" to the question:
                    // the entries left run through as cancelled.
                    continue;
                }
                match pass {
                    CleanPass::Check => view.check = Some((done, total)),
                    CleanPass::Remove => {
                        view.remove = Some((done, total));
                        view.freed = removed_bytes;
                    }
                }
            }
        }
    }

    /// Cancels the cleaning, its passes held where they stand now. Whether
    /// it was the first time.
    fn cancel(&mut self) -> bool {
        let first = !self.running.cancel.is_cancelled();
        if first {
            self.take_progress();
            self.view.cancel_asked = true;
            tracing::info!(check = ?self.view.check, remove = ?self.view.remove, admin_done = ?self.view.admin_done, "where the cleaning stood when cancelled");
        }
        self.running.cancel.cancel();
        first
    }
}

/// The confirmation open: what it shows, of which report. The user's part
/// (`groups`, `entries`) and root's (`admin`, its targets and entries) apart:
/// root's entries are never identifiers of this report to remove.
pub(super) struct Confirming {
    report: Arc<ScanReport>,
    groups: BTreeSet<usize>,
    entries: usize,
    admin: BTreeSet<usize>,
    admin_targets: BTreeSet<String>,
    admin_entries: usize,
    /// Docker's socket, when its prune is confirmed.
    docker: Option<PathBuf>,
}

impl Confirming {
    pub(super) fn report(&self) -> &ScanReport {
        &self.report
    }

    /// The user's groups confirmed, by their index in the report.
    pub(super) const fn groups(&self) -> &BTreeSet<usize> {
        &self.groups
    }

    /// The identifiers of the entries confirmed.
    pub(super) fn ids(&self) -> Vec<ItemId> {
        let groups = self.report.groups();
        self.groups
            .iter()
            .filter_map(|index| {
                let group = groups.get(*index);
                if group.is_none() {
                    // A line of the results that is no group of the report
                    // (a daemon's operation) never reaches the engine
                    // (invariant 1).
                    tracing::error!(index, "a group confirmed outside the report: left out");
                }
                group
            })
            .flat_map(|group| group.items().iter().map(ScanItem::id))
            .collect()
    }

    /// Whether a group confirmed still waits for `lock`.
    pub(super) fn carries(&self, lock: &BrowserLock) -> bool {
        let groups = self.report.groups();
        self.groups.iter().any(|index| {
            groups
                .get(*index)
                .is_some_and(|group| group.locks().contains(lock))
        })
    }

    /// Leaves out of this cleaning the groups that empty a cache of the
    /// browser of `lock`: what is confirmed is then what is left. The entries
    /// left, root's part included, Docker's prune counted as one (a browser's
    /// cache is never one of theirs).
    pub(super) fn leave_out(&mut self, lock: &BrowserLock) -> usize {
        let groups = self.report.groups();
        self.groups.retain(|index| {
            groups
                .get(*index)
                .is_some_and(|group| !group.locks().contains(lock))
        });
        self.entries = self
            .groups
            .iter()
            .filter_map(|index| groups.get(*index))
            .map(|group| group.items().len())
            .sum();
        self.entries + self.admin_entries + usize::from(self.docker.is_some())
    }
}

/// What a cleaning's job takes but its options, built from the settings
/// `inputs` on its thread: resolving the exclusions may wait on a mount that
/// stopped answering.
struct PendingJob {
    report: Arc<ScanReport>,
    ids: Vec<ItemId>,
    rules: Arc<RuleSet>,
    admin: Option<RootPart>,
    inputs: Inputs,
}

/// Root's part of a cleaning, as confirmed: its request is built on the
/// cleaning's thread, which reads which exclusions may touch its targets.
struct RootPart {
    /// The age and exclusions root's scan unlocked the groups with.
    granted: rights::Granted,
    /// Every target of the results held (those only root may read
    /// included), as root's scan was asked for them.
    targets: BTreeSet<String>,
    /// The targets of the groups confirmed.
    confirmed: BTreeSet<String>,
}

impl RootPart {
    /// Its request line, if the settings `inputs` read now still give the
    /// age and exclusions its scan unlocked with (and every exclusion can
    /// still be located): root never removes what the settings came to
    /// protect since.
    fn line(&self, inputs: &Inputs) -> Result<Vec<u8>, Problem> {
        let granted = &self.granted;
        let now = rights::Granted::of(inputs, &self.targets)?;
        if &now != granted {
            tracing::warn!(
                ?granted,
                ?now,
                "settings changed since the rights were obtained: nothing removed"
            );
            return Err(Problem::RightsChanged);
        }
        let line = granted
            .line(Operation::Clean, &self.confirmed)
            .ok_or(Problem::Start)?;
        tracing::info!(targets = ?self.confirmed, ?granted, "root's part of the cleaning");
        Ok(line)
    }
}

impl PendingJob {
    /// The job, its options and root's request built now: on the cleaning's
    /// thread, before anything is asked of root or removed. An exclusion
    /// refused is said before settings changed since the rights.
    fn build(self) -> Result<CleanJob, Problem> {
        let options = options(&self.inputs, SystemTime::now(), BTreeSet::new())?;
        tracing::info!(?options, "cleaning options built");
        let admin = self.admin.map(|part| part.line(&self.inputs)).transpose()?;
        Ok(CleanJob {
            report: self.report,
            ids: self.ids,
            rules: self.rules,
            variables: self.inputs.variables,
            options,
            admin,
        })
    }
}

/// A cleaning ready to start: its job, the history it writes to, and whether
/// the user asked for a deletion log.
struct Prepared {
    job: PendingJob,
    history: Option<History>,
    /// Read from the settings before they go into `job`, whose `build`
    /// consumes them: the thread still needs it for the history.
    deletion_log: bool,
    /// Docker's socket, when its prune is confirmed.
    docker: Option<PathBuf>,
}

/// Asks Docker on `socket` to prune its build cache, never under
/// `min_age`, once the user's part of the cleaning is over: never once it
/// is cancelled; `asked` told when Docker is.
fn prune_last(
    socket: &Path,
    prune: &docker::Prune,
    min_age: Duration,
    cancel: &Cancel,
    asked: &mpsc::Sender<()>,
) -> Pruning {
    if cancel.is_cancelled() {
        tracing::info!("cleaning cancelled before Docker's prune: not asked");
        return Pruning::NotAsked;
    }
    // The history line, and the deletion log asked for, wait for Docker's
    // answer: said, in case the process dies meanwhile.
    tracing::info!(
        ?socket,
        ?min_age,
        "Docker asked to prune: the history line waits for it"
    );
    if asked.send(()).is_err() {
        tracing::debug!("Docker's prune not shown: the screen is gone");
    }
    match prune(socket, min_age, &|| cancel.is_cancelled()) {
        Ok(pruned) => Pruning::Pruned(pruned),
        Err(error) => Pruning::Failed(error),
    }
}

/// What root's part gave the cleaning: what became of each of its entries,
/// and why it was not done to its end, if it was not (`RightsProblem::None`
/// when it was). A cancellation leaves entries: never done to its end, what
/// it removed before is in its outcomes. A run whose end is unknown may have
/// removed entries: said so. Root is only asked for groups its scan offered
/// without a word: one it says something about now (withheld, or walked
/// only in part) changed since, and was not cleaned whole.
fn admin_part(admin: &AdminDone) -> (&[OutcomeAnswer], ui::RightsProblem) {
    match admin {
        Ok(Reply::Answer(answer)) => {
            let outcomes = answer.outcomes().unwrap_or_default();
            let stops = answer.stops();
            let problem = if stops.iter().any(|stop| stop != Stop::Cancelled.key()) {
                ui::RightsProblem::Stopped
            } else if !stops.is_empty() {
                ui::RightsProblem::Cancelled
            } else if answer
                .issues()
                .iter()
                .any(|issue| !issue.targets().is_empty())
            {
                // A stop is an issue of no target, said by the stops.
                ui::RightsProblem::Changed
            } else {
                ui::RightsProblem::None
            };
            (outcomes, problem)
        }
        Ok(Reply::Refused(refusal)) => (&[], rights::refusal_problem(refusal)),
        Err(failure) if rights::ran_to_an_unknown_end(failure) => (&[], ui::RightsProblem::Unknown),
        Err(failure) => (&[], rights::problem_of(failure)),
    }
}

/// Whether root's part was done to its end: the one rule of the history
/// line, the steps and the receipt.
fn admin_done(admin: &AdminDone) -> bool {
    admin_part(admin).1 == ui::RightsProblem::None
}

/// What a cleaning gave, its line added to `history` first
/// ([`write_history`]).
fn recorded(
    history: Option<History>,
    receipt: CleanReceipt,
    admin: Option<AdminDone>,
    docker: Option<Pruning>,
    deletion_log: bool,
) -> Cleaned {
    let history = write_history(
        history,
        &receipt,
        admin.as_ref(),
        docker.as_ref(),
        deletion_log,
    );
    Cleaned {
        receipt,
        admin,
        docker,
        history,
    }
}

/// Adds the line of the cleaning to `history`, from the cleaning's thread:
/// the line is written before the screen stops waiting for it, unless the
/// user insists on closing. One line for both parts: root's entries first.
fn write_history(
    history: Option<History>,
    receipt: &CleanReceipt,
    admin: Option<&AdminDone>,
    docker: Option<&Pruning>,
    deletion_log: bool,
) -> HistoryWrite {
    // Said even when no history is kept.
    let admin = admin.map(|admin| {
        let (outcomes, problem) = admin_part(admin);
        if problem != ui::RightsProblem::None {
            tracing::warn!(
                ?problem,
                outcomes = outcomes.len(),
                "root's part of the cleaning not done to its end"
            );
        }
        (outcomes, problem)
    });
    let Some(mut history) = history else {
        return HistoryWrite::Unavailable;
    };
    let (admin_outcomes, admin_record) = admin.map_or((&[][..], None), |(outcomes, problem)| {
        let done = problem == ui::RightsProblem::None;
        let record = AdminRecord {
            done,
            reason: (!done).then(|| rights::problem_key(problem).to_owned()),
        };
        (outcomes, Some(record))
    });
    let outcomes = admin_outcomes
        .iter()
        .map(|answered| {
            let outcome = answered.outcome();
            (outcome.clean_result_or_refused(), outcome.clone())
        })
        .chain(
            receipt
                .outcomes()
                .iter()
                .map(|outcome| (outcome.result(), HistoryOutcome::of(outcome))),
        )
        .collect();
    let stops = receipt
        .stops()
        .iter()
        .map(|stop| stop.key().to_owned())
        .collect();
    let entry = HistoryEntry::of_outcomes(
        stops,
        outcomes,
        admin_record,
        SystemTime::now(),
        deletion_log,
    )
    .with_operations(docker.map(Pruning::record).into_iter().collect());
    match history.record(&entry) {
        Ok(()) => HistoryWrite::Recorded,
        Err(error) => {
            tracing::error!(%error, deletion_log, "history not recorded");
            HistoryWrite::Failed
        }
    }
}

impl CleanerScreen {
    /// Opens the confirmation of the groups checked: what it shows is what a
    /// confirmed cleaning takes, of this very report.
    pub(super) fn clean(&self) {
        if self.is_settling("cleaning") || self.results_frozen("cleaning") {
            return;
        }
        let opened = {
            let mut state = self.0.borrow_mut();
            if state.task.is_some() {
                tracing::warn!("cleaning asked for while a task runs");
                return;
            }
            let Some(held) = &state.held else {
                tracing::warn!("cleaning asked for without results");
                return;
            };
            let results = &held.results;
            let groups = results.user_checked();
            let (admin, admin_targets) = results.admin_checked();
            let (entries, bytes) = results.selected();
            let docker = results.docker_checked();
            if entries == 0 && docker.is_none() {
                tracing::warn!("cleaning asked for without an entry");
                return;
            }
            tracing::info!(groups = groups.len(), admin = ?admin_targets, entries, bytes, ?docker, "cleaning to confirm");
            let confirming = Confirming {
                report: Arc::clone(&held.report),
                entries: results.entries_of(&groups),
                groups,
                admin_entries: results.entries_of(&admin),
                admin,
                admin_targets,
                docker,
            };
            let opened = (
                entries,
                bytes,
                !confirming.admin.is_empty(),
                confirming.docker.is_some(),
            );
            state.confirming = Some(confirming);
            opened
        };
        self.settle();
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_understood(false);
            cleaner.set_confirm_entries(shown(opened.0));
            cleaner.set_confirm_size(size(opened.1));
            cleaner.set_confirm_admin(opened.2);
            cleaner.set_confirm_docker(opened.3);
            cleaner.set_confirming(true);
        });
    }

    /// Closes the confirmation; the second click of a double click on its
    /// buttons never reaches the results below.
    fn close_confirmation(&self) {
        self.settle();
        self.with_window(|window, _| window.global::<ui::Cleaner>().set_confirming(false));
    }

    pub(super) fn cancel_confirmation(&self) {
        if self.is_settling("cancellation of the confirmation") {
            return;
        }
        if self.0.borrow_mut().confirming.take().is_some() {
            tracing::info!("cleaning not confirmed");
        }
        self.close_confirmation();
    }

    /// Whether a confirmation, or the question about an open browser, is
    /// open: the results are then frozen.
    pub(super) fn is_confirming(&self) -> bool {
        self.0.borrow().confirming.is_some() || self.is_asking()
    }

    /// The cleaning of `confirmed`, from the settings read now (its options
    /// are built from them on its thread), its history opened before
    /// anything is removed.
    fn prepare_cleaning(&self, confirmed: &Confirming) -> Result<Prepared, Problem> {
        let state = self.0.borrow();
        let rules = state.rules.as_ref().ok_or(Problem::Rules)?;
        let inputs = (state.seams.read)()?;
        // Only what needs no I/O is checked here: an exclusion is met, and
        // said, on the task's thread.
        if let Some(problem) = settings_problem(&inputs, SystemTime::now()) {
            return Err(problem);
        }
        let history = match History::open_for_cleaning(
            inputs.state_directory.as_deref(),
            inputs.deletion_log,
        ) {
            Ok(Opened::Open(history)) => Some(history),
            Ok(Opened::Unavailable(_)) => None,
            Err(_) => return Err(Problem::History),
        };
        let ids = confirmed.ids();
        if ids.len() != confirmed.entries {
            tracing::error!(
                ids = ids.len(),
                confirmed = confirmed.entries,
                "the cleaning would not take what was confirmed: nothing removed"
            );
            return Err(Problem::Start);
        }
        // Root's part, with the age and exclusions its scan unlocked with,
        // checked on the cleaning's thread against the settings read now.
        let admin = if confirmed.admin_targets.is_empty() {
            None
        } else {
            let Some((held, granted)) = state
                .held
                .as_ref()
                .and_then(|held| Some((held, held.granted.clone()?)))
            else {
                tracing::error!(
                    "groups that need the administrator confirmed without the rights: nothing removed"
                );
                return Err(Problem::Start);
            };
            Some(RootPart {
                granted,
                targets: held.results.helper_targets(),
                confirmed: confirmed.admin_targets.clone(),
            })
        };
        let deletion_log = inputs.deletion_log;
        Ok(Prepared {
            job: PendingJob {
                report: Arc::clone(&confirmed.report),
                ids,
                rules: Arc::clone(rules),
                admin,
                inputs,
            },
            history,
            deletion_log,
            docker: confirmed.docker.clone(),
        })
    }

    /// Starts the cleaning confirmed, of the report the confirmation showed.
    pub(super) fn confirm_delete(&self) {
        if self.is_settling("deletion") {
            return;
        }
        let understood = self
            .0
            .borrow()
            .window
            .upgrade()
            .is_some_and(|window| window.global::<ui::Cleaner>().get_understood());
        if !understood {
            tracing::warn!("deletion asked for before it is said understood");
            return;
        }
        let Some(confirmed) = self.0.borrow_mut().confirming.take() else {
            return;
        };
        self.close_confirmation();
        self.ask_about_browsers(confirmed);
    }

    /// Starts the cleaning `confirmed`, if its report is still the one held
    /// and nothing runs.
    pub(super) fn launch(&self, confirmed: &Confirming) {
        let fit = {
            let state = self.0.borrow();
            state.task.is_none()
                && state
                    .held
                    .as_ref()
                    .is_some_and(|held| Arc::ptr_eq(&held.report, &confirmed.report))
        };
        if !fit {
            tracing::error!(
                "confirmation of another report, or while a task runs: nothing removed"
            );
            return;
        }
        let started = self
            .prepare_cleaning(confirmed)
            .and_then(|prepared| self.start_cleaning(prepared));
        if let Err(found) = started {
            tracing::warn!(problem = ?found, "cleaning cannot start: nothing removed");
            self.with_window(|window, _| {
                window
                    .global::<ui::Cleaner>()
                    .set_problem(problem(Some(found)));
            });
        }
    }

    /// Runs the cleaning `prepared` on its thread, which writes the history
    /// line before it reports.
    fn start_cleaning(&self, prepared: Prepared) -> Result<(), Problem> {
        let Prepared {
            job,
            history,
            deletion_log,
            docker,
        } = prepared;
        tracing::info!(
            ids = job.ids.len(),
            admin = job.admin.is_some(),
            history = history.is_some(),
            deletion_log,
            ?docker,
            "cleaning started"
        );
        let report = Arc::clone(&job.report);
        let had_admin = job.admin.is_some();
        let clean = Arc::clone(&self.0.borrow().seams.clean);
        let elevate = Arc::clone(&self.0.borrow().seams.elevate);
        let prune = Arc::clone(&self.0.borrow().seams.docker.prune);
        let had_docker = docker.is_some();
        let entries = job.ids.len();
        let exclusions = job.inputs.exclusions.clone();
        let (step_sender, steps) = mpsc::channel();
        let (admin_sender, admin_ended) = mpsc::channel();
        let (docker_sender, docker_asked) = mpsc::channel();
        let running = Running::start(
            "okapi-cleaning",
            tracing::info_span!("cleaning"),
            move |cancel, progress| {
                // Its options first: refused, nothing is asked of root nor
                // removed, and no history line is written.
                let job = job.build()?;
                // Root's part first: the system's question, if it comes,
                // comes before anything is removed; never asked once
                // cancelled.
                let admin = job.admin.clone().map(|line| {
                    if cancel.is_cancelled() {
                        tracing::info!("cleaning cancelled before root's part: not asked");
                        return Err(ElevationFailure::Cancelled);
                    }
                    elevate(Operation::Clean, line, &cancel, &step_sender)
                });
                if let Some(admin) = &admin {
                    let done = admin_done(admin);
                    if admin_sender.send(done).is_err() {
                        tracing::debug!("end of root's part not shown: the screen is gone");
                    }
                }
                if matches!(
                    admin,
                    Some(Err(ElevationFailure::Reply(ReplyError::Dismissed)))
                ) {
                    // A "no" to the question: nothing of the user's either.
                    tracing::info!("authentication dismissed: the whole cleaning is cancelled");
                    cancel.cancel();
                }
                let receipt = clean(&job, cancel.clone(), progress);
                // Docker's prune last: what the user's part removes is
                // known before the daemon, which may take minutes, works.
                let docker = docker.map(|socket| {
                    let min_age = job.options.min_age();
                    prune_last(&socket, &prune, min_age, &cancel, &docker_sender)
                });
                Ok(recorded(history, receipt, admin, docker, deletion_log))
            },
        )
        .map_err(|error| {
            tracing::error!(%error, "cleaning thread not started: nothing removed");
            Problem::Start
        })?;
        self.forget_refused_exclusions();
        self.with_window(|window, _| window.global::<ui::Cleaner>().set_problem(problem(None)));
        self.0.borrow_mut().task = Some(Task::Cleaning(Cleaning {
            running,
            steps,
            admin_ended,
            docker_asked,
            view: CleanView::new(had_docker),
            report,
            deletion_log,
            close_when_done: false,
            had_admin,
            entries,
            exclusions,
        }));
        self.settle();
        self.with_window(|window, state| {
            show(
                &state.step_model,
                CleanView::new(had_docker).steps(had_admin, entries),
            );
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_clean_cancel_asked(false);
            cleaner.set_freed(size(0));
            cleaner.set_freed_unknown(false);
            cleaner.set_pruning(false);
            cleaner.set_step(ui::CleanerStep::Cleaning);
        });
        self.watch_task();
        Ok(())
    }

    /// Takes in the progress of the cleaning, then looks for its receipt.
    pub(super) fn watch_cleaning(&self) {
        let (result, steps, freed, rights, pruning) = {
            let mut state = self.0.borrow_mut();
            let Some(Task::Cleaning(cleaning)) = state.task.as_mut() else {
                return;
            };
            cleaning.take_progress();
            let view = &cleaning.view;
            (
                cleaning.running.result.try_recv(),
                view.steps(cleaning.had_admin, cleaning.entries),
                view.freed,
                view.rights,
                view.docker_asked,
            )
        };
        self.with_window(|window, state| {
            show(&state.step_model, steps);
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_clean_rights(match rights {
                None => ui::RightsStep::Idle,
                Some(ElevationStep::Launched) => ui::RightsStep::Authenticating,
                Some(ElevationStep::Authenticated) => ui::RightsStep::Scanning,
            });
            cleaner.set_freed(size(freed));
            cleaner.set_pruning(pruning);
        });
        match result {
            Ok(Ok(cleaned)) => self.end_cleaning(Some(&cleaned)),
            Ok(Err(found)) => self.cleaning_refused(found),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                let (had_admin, docker_asked) = match &self.0.borrow().task {
                    Some(Task::Cleaning(cleaning)) => {
                        (cleaning.had_admin, cleaning.view.docker_asked)
                    }
                    _ => (false, false),
                };
                tracing::error!(
                    had_admin,
                    docker_asked,
                    "cleaning thread ended without a receipt: its history line is not written, and root's part or Docker, if any, may have removed entries"
                );
                self.end_cleaning(None);
            }
        }
    }

    /// The cleaning refused on its thread for `found`, before root was asked
    /// or anything removed: back to the results, still held, the refusal
    /// said. A closing asked meanwhile goes on: nothing was done.
    fn cleaning_refused(&self, found: Problem) {
        if !matches!(self.0.borrow().task, Some(Task::Cleaning(_))) {
            tracing::error!("a cleaning refused that was not under way");
            return;
        }
        let Some(Task::Cleaning(cleaning)) = self.0.borrow_mut().task.take() else {
            return;
        };
        self.0.borrow().timer.stop();
        let close_when_done = cleaning.close_when_done;
        // Its cause is traced where it was met, on the thread.
        tracing::info!(problem = ?found, close_when_done, "cleaning refused on its thread: nothing removed");
        let rights_changed = found == Problem::RightsChanged;
        if rights_changed {
            // The groups root unlocked were measured with other settings:
            // they must be unlocked again before they can be checked.
            let mut state = self.0.borrow_mut();
            if let Some(held) = state
                .held
                .as_mut()
                .filter(|held| Arc::ptr_eq(&held.report, &cleaning.report))
            {
                held.results.relock();
                held.granted = None;
            } else {
                tracing::error!("rights changed for a report no longer held: nothing locked again");
            }
        }
        self.settle();
        self.set_step(ui::CleanerStep::Results);
        self.show_results();
        if rights_changed {
            // Said whatever was said before: root was locked again.
            self.with_window(|window, _| {
                let cleaner = window.global::<ui::Cleaner>();
                let shown = cleaner.get_problem();
                if shown != ui::SetupProblem::None {
                    tracing::warn!(replaced = ?shown, "rights changed said over another problem");
                }
                cleaner.set_problem(problem(Some(found)));
            });
        } else {
            self.say_refusal(found, cleaning.exclusions);
        }
        if close_when_done {
            self.close_as_asked();
        }
    }

    /// Shows the receipt of the cleaning, or that it was interrupted; the
    /// report it took its identifiers from goes.
    fn end_cleaning(&self, done: Option<&Cleaned>) {
        if !matches!(self.0.borrow().task, Some(Task::Cleaning(_))) {
            tracing::error!("a cleaning ended that was not under way");
            return;
        }
        let Some(Task::Cleaning(mut cleaning)) = self.0.borrow_mut().task.take() else {
            return;
        };
        {
            let mut state = self.0.borrow_mut();
            state.timer.stop();
            state.held = None;
        }
        self.settle();
        cleaning.take_progress();
        let steps =
            cleaning
                .view
                .ended_steps(cleaning.had_admin, cleaning.entries, done.map(Ended::of));
        tracing::info!(steps = ?steps.iter().map(|step| (step.kind, step.state)).collect::<Vec<_>>(), "steps of the cleaning");
        let receipt = done.map(|done| {
            (
                done,
                receipt_of(
                    &done.receipt,
                    &cleaning.report,
                    done.admin.as_ref(),
                    done.docker.as_ref(),
                ),
            )
        });
        if let Some((done, receipt)) = &receipt {
            tracing::info!(
                ending = ?receipt.ending,
                freed = ?receipt.freed,
                removed = receipt.removed,
                partial = receipt.partial,
                gone = receipt.gone,
                kept = receipt.kept,
                history = ?done.history,
                docker = ?done.docker.as_ref().map(|docker| docker.record().end),
                "cleaning ended"
            );
        }
        let recorded = receipt
            .as_ref()
            .is_some_and(|(done, _)| done.history == HistoryWrite::Recorded);
        let log_lost = cleaning.deletion_log && !recorded;
        if log_lost {
            tracing::error!("the deletion log asked for is not written");
        }
        let mut close = false;
        self.with_window(|window, state| {
            show(&state.step_model, steps);
            let shown_to = window.global::<ui::Cleaner>();
            shown_to.set_receipt_docker(docker_shown(
                receipt.as_ref().map(|(done, _)| *done),
                cleaning.view.docker_asked,
            ));
            if let Some((done, receipt)) = &receipt {
                show(
                    &state.stop_model,
                    done.receipt.stops().iter().filter_map(stop_shown).collect(),
                );
                show_receipt(&shown_to, &state.receipt_model, receipt);
                shown_to.set_receipt_rights(
                    done.admin
                        .as_ref()
                        .map_or(ui::RightsProblem::None, |admin| admin_part(admin).1),
                );
                shown_to.set_history(match done.history {
                    HistoryWrite::Recorded => ui::HistoryState::Recorded,
                    HistoryWrite::Failed => ui::HistoryState::Failed,
                    HistoryWrite::Unavailable => ui::HistoryState::Unavailable,
                });
            } else {
                show(&state.receipt_model, Vec::new());
                show(&state.stop_model, Vec::new());
                // Its thread died: root's part, if any, may have removed entries.
                shown_to.set_receipt_rights(if cleaning.had_admin {
                    ui::RightsProblem::Unknown
                } else {
                    ui::RightsProblem::None
                });
                shown_to.set_ending(ui::CleanEnding::Interrupted);
                shown_to.set_history(ui::HistoryState::NotWritten);
            }
            shown_to.set_deletion_log_lost(log_lost);
            shown_to.set_step(ui::CleanerStep::Receipt);
            if cleaning.close_when_done && log_lost {
                // What is lost must be read before the window goes.
                tracing::warn!("closing held back: the receipt says the deletion log is lost");
            } else if cleaning.close_when_done {
                close = true;
            }
        });
        if close {
            self.close_as_asked();
        }
        // Even when it freed nothing it knows of: root's part, or a thread
        // that died, may have removed entries.
        self.emit(CleanerEvent::CleaningDone {
            freed: receipt.as_ref().and_then(|(_, receipt)| receipt.freed),
            at: seconds(SystemTime::now()),
        });
    }

    /// The closing the user asked during the cleaning, asked again as the
    /// window's own button asks: every screen may still hold it (an
    /// uninstallation running), never hidden past them.
    fn close_as_asked(&self) {
        let window = self.0.borrow().window.upgrade();
        let Some(window) = window else {
            return;
        };
        match window
            .window()
            .dispatch_event_with_result(slint::platform::WindowEvent::CloseRequested)
        {
            Ok(slint::platform::WindowEventDispatchResult::Accepted) => {
                tracing::info!("receipt in: the window closes as asked");
            }
            Ok(held) => {
                tracing::info!(
                    ?held,
                    "receipt in: the closing asked is held by another screen"
                );
            }
            Err(error) => tracing::error!(%error, "window not closed"),
        }
    }

    /// Cancels the cleaning: what is not yet removed stays; the entry under
    /// way finishes.
    pub(super) fn cancel_cleaning(&self) {
        if self.is_settling("cancellation of the cleaning") {
            return;
        }
        let asked = {
            let mut state = self.0.borrow_mut();
            let Some(Task::Cleaning(cleaning)) = state.task.as_mut() else {
                return;
            };
            cleaning.cancel()
        };
        if asked {
            tracing::info!("cleaning cancelled by the user");
        }
        self.with_window(|window, _| {
            window.global::<ui::Cleaner>().set_clean_cancel_asked(true);
        });
    }

    /// Whether the window may close now: during a cleaning, it cancels,
    /// shows the cleaning and waits for the receipt, so that the history
    /// line is written; asked again, it closes.
    pub fn close_requested(&self) -> slint::CloseRequestResponse {
        // The second click of a double click on the close button is no
        // insistence.
        let settling = self.0.borrow().settling.running();
        if let Some(asking) = &self.0.borrow().asking {
            tracing::info!(
                closing = asking.is_closing(),
                "window closed at the question about a browser: nothing cleaned"
            );
        }
        let insisted = {
            let mut state = self.0.borrow_mut();
            let Some(Task::Cleaning(cleaning)) = state.task.as_mut() else {
                return slint::CloseRequestResponse::HideWindow;
            };
            let insisted = cleaning.close_when_done && !settling;
            cleaning.close_when_done = true;
            cleaning.cancel();
            insisted
        };
        if insisted {
            // Closing waits a little more: `shut_down` says whether the line
            // is written.
            tracing::warn!("closing insisted during a cleaning");
            return slint::CloseRequestResponse::HideWindow;
        }
        tracing::info!("closing asked during a cleaning: cancelled, waiting for its receipt");
        self.settle();
        self.with_window(|window, _| {
            window.set_page(ui::Page::Cleaner);
            window.global::<ui::Cleaner>().set_clean_cancel_asked(true);
        });
        slint::CloseRequestResponse::KeepWindowShown
    }

    /// Leaves the receipt: back to choosing.
    pub(super) fn finish(&self) {
        if self.is_settling("finish") {
            return;
        }
        self.back_to_choosing();
    }
}

/// The receipt of a cleaning: `receipt` of the user's part, cleaned from
/// `report`, root's part and Docker's prune, if any. It ends cancelled when
/// the system's question was dismissed or the prune cancelled, stopped when
/// no part did anything.
fn receipt_of(
    receipt: &CleanReceipt,
    report: &ScanReport,
    admin: Option<&AdminDone>,
    docker: Option<&Pruning>,
) -> Receipt {
    let users = OutcomeFacts::all(receipt, report);
    if users
        .iter()
        .any(|fact| fact.result == CleanResult::Kept(KeepReason::UnknownItem))
    {
        tracing::error!("an identifier of another report reached the cleaning");
    }
    let (admin_outcomes, problem) = admin.map_or((&[][..], ui::RightsProblem::None), admin_part);
    let facts: Vec<OutcomeFacts> = admin_outcomes
        .iter()
        .map(OutcomeFacts::of_helper)
        .chain(users)
        .collect();
    // Stopped when nothing could be done: a part stopped or failed, and no
    // part touched anything. Root's part whose end is unknown may have
    // removed entries: never "nothing was touched".
    let unknown = problem == ui::RightsProblem::Unknown;
    // What the prune did, as the history line keeps it: one that ended
    // unknown may have removed caches, and leaves what was freed unknown.
    let prune_end = docker.map(|docker| docker.record().end);
    let prune_unknown = prune_end.is_some_and(OperationEnd::leaves_freed_unknown);
    let may_have_pruned = prune_end.is_some_and(OperationEnd::may_have_removed);
    let failed = !receipt.stops().is_empty()
        || problem != ui::RightsProblem::None
        || docker.is_some_and(|docker| !docker.is_done());
    let mut shown = Receipt::new(
        failed && !unknown && !may_have_pruned && facts.is_empty(),
        &facts,
    );
    shown.freed = shown
        .freed
        .map(|freed| freed.saturating_add(docker.map_or(0, Pruning::freed)))
        .filter(|_| !prune_unknown);
    // Root's part whose end is unknown: its outcomes unknown, whatever
    // Docker said.
    if unknown && facts.is_empty() {
        shown.ending = Ending::Interrupted;
    } else if !receipt.stops().is_empty() && facts.is_empty() {
        // The user's part stopped before it touched anything, as the
        // history line says, whatever Docker did after.
        shown.ending = Ending::Stopped;
    } else if matches!(
        problem,
        ui::RightsProblem::Dismissed | ui::RightsProblem::Cancelled
    ) || matches!(
        docker,
        Some(Pruning::NotAsked | Pruning::Failed(okapi_tools::docker::DockerError::Cancelled))
    ) {
        shown.ending = Ending::Cancelled;
    }
    shown
}

/// What the receipt says of Docker's prune: what `done` recorded of it; a
/// cleaning whose thread died (none done) may have pruned once Docker was
/// `asked`.
fn docker_shown(done: Option<&Cleaned>, asked: bool) -> ui::OperationShown {
    match done {
        Some(done) => done
            .docker
            .as_ref()
            .and_then(|docker| operation_shown(&docker.record()))
            .unwrap_or_default(),
        None => ui::OperationShown {
            end: if asked {
                ui::OperationEnd::Unknown
            } else {
                ui::OperationEnd::None
            },
            ..ui::OperationShown::default()
        },
    }
}

/// Shows `receipt` in the window.
fn show_receipt(
    cleaner: &ui::Cleaner<'_>,
    model: &slint::VecModel<ui::ReceiptLineShown>,
    receipt: &Receipt,
) {
    cleaner.set_ending(match receipt.ending {
        Ending::Done => ui::CleanEnding::Done,
        Ending::Cancelled => ui::CleanEnding::Cancelled,
        Ending::Stopped => ui::CleanEnding::Stopped,
        Ending::Interrupted => ui::CleanEnding::Interrupted,
    });
    cleaner.set_freed(size(receipt.freed.unwrap_or_default()));
    cleaner.set_freed_unknown(receipt.freed.is_none());
    cleaner.set_removed(shown(receipt.removed));
    cleaner.set_partial(shown(receipt.partial));
    cleaner.set_gone(shown(receipt.gone));
    cleaner.set_kept(shown(receipt.kept));
    show(model, receipt.lines.iter().map(line_shown).collect());
}

/// A line of a receipt as the window shows it, the cleaner's or the
/// history's.
pub(crate) fn line_shown(line: &ReceiptLine) -> ui::ReceiptLineShown {
    ui::ReceiptLineShown {
        kind: match line.kind {
            LineKind::Removed => ui::ReceiptKind::Removed,
            LineKind::Partial => ui::ReceiptKind::Partial,
            LineKind::Gone => ui::ReceiptKind::Gone,
            LineKind::Kept(Kept::Recent) => ui::ReceiptKind::Recent,
            LineKind::Kept(Kept::Changed) => ui::ReceiptKind::Changed,
            LineKind::Kept(Kept::Replaced) => ui::ReceiptKind::Replaced,
            LineKind::Kept(Kept::Excluded) => ui::ReceiptKind::Excluded,
            LineKind::Kept(Kept::Protected) => ui::ReceiptKind::Protected,
            LineKind::Kept(Kept::Denied) => ui::ReceiptKind::Denied,
            LineKind::Kept(Kept::Refused) => ui::ReceiptKind::Refused,
            LineKind::Kept(Kept::Cancelled) => ui::ReceiptKind::Cancelled,
            LineKind::Kept(Kept::BrowserOpen) => ui::ReceiptKind::BrowserOpen,
            LineKind::Kept(Kept::NoLongerChosen) => ui::ReceiptKind::NoLongerChosen,
            LineKind::Kept(Kept::Unknown) => ui::ReceiptKind::Unknown,
        },
        count: shown(line.count),
        sized: line.bytes.is_some(),
        size: size(line.bytes.unwrap_or_default()),
        paths: slint::ModelRc::new(slint::VecModel::from(
            line.paths
                .iter()
                .map(|(path, blocked)| ui::PathShown {
                    path: path.as_str().into(),
                    blocked: blocked.as_deref().unwrap_or_default().into(),
                })
                .collect::<Vec<_>>(),
        )),
        more: shown(line.more),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::Duration;

    use i_slint_backend_testing::{AccessibleRole, ElementHandle};
    use okapi_rules::{Os, RuleSet, TemplateVariables};
    use slint::ComponentHandle as _;

    use std::sync::Arc;

    use super::super::tests::{
        EXCLUSION_REFUSED, SIDEBAR_STOPS, Sandbox, button, checkbox, let_the_screen_settle,
        on_cleaner, press, refused_for_an_exclusion, refused_when, resize, seams_never_scanning,
        settle_files, shows, shows_a_text_starting, start_of, step, watch_until,
    };
    use super::super::{Inputs, Seams};
    use crate::ui;

    /// The window on the results of an analysis of Applications alone.
    fn on_results(seams: Seams) -> (ui::AppWindow, super::super::CleanerScreen) {
        let (window, screen) = on_cleaner(seams);
        for category in ["Système", "Jeux"] {
            checkbox(&window, category).invoke_accessible_default_action();
        }
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        let_the_screen_settle();
        (window, screen)
    }

    fn found(window: &ui::AppWindow, label: &str) -> ElementHandle {
        ElementHandle::find_by_accessible_label(window, label)
            .find(|element| element.accessible_role() == Some(AccessibleRole::Checkbox))
            .unwrap_or_else(|| panic!("no line {label} in the results"))
    }

    /// A press and release of the pointer at the middle of `element`: through
    /// the touch areas, as a mouse goes, never its accessible action.
    fn click(window: &ui::AppWindow, element: &ElementHandle) {
        let corner = element.absolute_position();
        let size = element.size();
        click_at(
            window,
            slint::LogicalPosition::new(corner.x + size.width / 2.0, corner.y + size.height / 2.0),
        );
    }

    /// A little to the right of `element`, at its middle height.
    fn right_of(element: &ElementHandle) -> slint::LogicalPosition {
        let corner = element.absolute_position();
        let size = element.size();
        slint::LogicalPosition::new(
            corner.x + size.width + size.height,
            corner.y + size.height / 2.0,
        )
    }

    /// A press and release of the pointer at `position`.
    fn click_at(window: &ui::AppWindow, position: slint::LogicalPosition) {
        use slint::platform::{PointerEventButton, WindowEvent};
        let button = PointerEventButton::Left;
        window
            .window()
            .dispatch_event(WindowEvent::PointerPressed { position, button });
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased { position, button });
        slint::platform::update_timers_and_animations();
    }

    /// The button that unfolds the description of the group `group`, its
    /// words `words`.
    fn why_button(window: &ui::AppWindow, words: &str, group: &str) -> ElementHandle {
        ElementHandle::find_by_accessible_label(window, words)
            .find(|button| button.accessible_description().as_deref() == Some(group))
            .unwrap_or_else(|| panic!("no button {words} for {group}"))
    }

    /// The description of the group of pip, in French.
    const PIP_SAID: &str = "Paquets wheel et archives de sources téléchargés par pip.";

    #[test]
    fn a_description_unfolds_without_checking_its_group() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        let pip = "Cache de pip";
        let said = PIP_SAID;
        let why = || why_button(&window, "Pourquoi c'est sans risque", pip);
        let checked = || found(&window, pip).accessible_checked();
        assert_eq!(checked(), Some(true));
        let name = found(&window, pip);
        assert!(
            why().absolute_position().y >= name.absolute_position().y + name.size().height,
            "below the name, which keeps its room"
        );
        assert_eq!(why().accessible_expanded(), Some(false));
        assert!(!shows(&window, said));
        click(&window, &why());
        assert_eq!(why().accessible_expanded(), Some(true));
        assert!(shows(&window, said), "its description, in French");
        assert_eq!(
            checked(),
            Some(true),
            "a click on the button checks nothing"
        );
        let text = ElementHandle::find_by_accessible_label(&window, said)
            .next()
            .unwrap();
        click(&window, &text);
        assert_eq!(checked(), Some(true), "nor one on the description");
        click(&window, &found(&window, pip));
        assert_eq!(checked(), Some(false), "the checkbox itself does");
        click_at(&window, right_of(&found(&window, pip)));
        assert_eq!(checked(), Some(true), "so does the rest of its line");
        // Under the confirmation, the results are frozen, but a description
        // still folds.
        button(&window, "Nettoyer").invoke_accessible_default_action();
        let cleaner = window.global::<ui::Cleaner>();
        assert!(cleaner.get_confirming());
        let_the_screen_settle();
        cleaner.invoke_explain(1);
        slint::platform::update_timers_and_animations();
        assert!(!shows(&window, said), "folded");
        assert!(cleaner.get_confirming());
    }

    #[test]
    fn a_group_left_to_choose_says_what_goes_and_stays_unchecked() {
        let sandbox = Sandbox::new();
        // A performance cache: left to the user's choice (invariant 5).
        sandbox.file("home/.cache/fontconfig/old");
        settle_files();
        let (window, _screen) = on_cleaner(sandbox.read(&sandbox.root.join("state")));
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        let_the_screen_settle();
        let fonts = "Cache des polices";
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Pourquoi c'est sans risque")
                .all(|button| button.accessible_description().as_deref() != Some(fonts)),
            "never called safe"
        );
        let what = why_button(&window, "Ce qui part", fonts);
        click(&window, &what);
        assert_eq!(what.accessible_expanded(), Some(true));
        assert!(shows(
            &window,
            "Cache de rendu des polices de Fontconfig : recréé tout seul quand il manque."
        ));
        assert_eq!(found(&window, fonts).accessible_checked(), Some(false));
    }

    #[test]
    fn the_description_is_english_in_an_english_window() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen) = super::super::tests::on_cleaner_in(
            sandbox.read(&sandbox.root.join("state")),
            okapi_rules::Language::English,
        );
        for category in ["System", "Games"] {
            checkbox(&window, category).invoke_accessible_default_action();
        }
        let_the_screen_settle();
        button(&window, "Analyze").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        let_the_screen_settle();
        why_button(&window, "Why it's safe", "pip Cache").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(shows(
            &window,
            "pip downloaded wheel and source distribution cache"
        ));
    }

    /// Opens the confirmation, says it is understood, and deletes.
    fn confirm(window: &ui::AppWindow) {
        button(window, "Nettoyer").invoke_accessible_default_action();
        assert!(window.global::<ui::Cleaner>().get_confirming());
        slint::platform::update_timers_and_animations();
        let_the_screen_settle();
        found(window, "J'ai compris que c'est définitif").invoke_accessible_default_action();
        button(window, "Supprimer définitivement").invoke_accessible_default_action();
    }

    #[test]
    fn what_is_checked_is_cleaned_and_recorded_with_every_entry() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        assert_eq!(
            found(&window, "Applications").accessible_checked(),
            Some(true)
        );
        assert_eq!(
            found(&window, "Cache de pip").accessible_checked(),
            Some(true)
        );
        assert!(shows(&window, "Éléments : 1 sur 1"));
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!old.exists(), "removed");
        assert!(shows(&window, "Nettoyage terminé"));
        assert!(shows(&window, "Le reçu est enregistré dans l'historique."));
        assert_eq!(
            steps_shown(&window),
            [
                (ui::CleanStepKind::Check, ui::RowState::Done),
                (ui::CleanStepKind::Remove, ui::RowState::Done),
                (ui::CleanStepKind::History, ui::RowState::Done),
            ],
            "no part of root's"
        );
        assert!(shows(&window, "Vérification de chaque élément"));
        assert!(shows(&window, "Suppression des éléments vérifiés"));
        let history = sandbox.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["removed"], 1);
        assert_eq!(
            sandbox.deletion_logs()[0][0]["path"],
            old.to_string_lossy().as_ref(),
            "the deletion log asked for"
        );
        button(&window, "Terminer").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Receipt, "a double click");
        let_the_screen_settle();
        button(&window, "Terminer").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Idle);
    }

    #[test]
    fn nothing_is_removed_without_the_confirmation() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        button(&window, "Nettoyer").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        // Enter twice on "Nettoyer": the second lands on "Annuler", which
        // keeps the confirmation open.
        press(&window, slint::platform::Key::Return);
        assert!(window.global::<ui::Cleaner>().get_confirming());
        let_the_screen_settle();
        assert_eq!(
            button(&window, "Supprimer définitivement").accessible_enabled(),
            Some(false),
            "not before it is understood"
        );
        // Rust deletes nothing either, whoever asks.
        window.global::<ui::Cleaner>().invoke_confirm_delete();
        assert_eq!(step(&window), ui::CleanerStep::Results);
        assert!(window.global::<ui::Cleaner>().get_confirming());
        // Under the confirmation, the results never change.
        found(&window, "Cache de pip").invoke_accessible_default_action();
        assert_eq!(
            found(&window, "Cache de pip").accessible_checked(),
            Some(true)
        );
        button(&window, "Nouvelle analyse").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Results);
        button(&window, "Annuler").invoke_accessible_default_action();
        assert!(!window.global::<ui::Cleaner>().get_confirming());
        assert!(old.exists());
        // The second click of a double click on "Annuler" lands on the
        // results below: they do not change.
        found(&window, "Cache de pip").invoke_accessible_default_action();
        assert_eq!(
            found(&window, "Cache de pip").accessible_checked(),
            Some(true)
        );
        let_the_screen_settle();
        // A double click on "Analyser" lands on "Nettoyer", in its place.
        screen.settle();
        button(&window, "Nettoyer").invoke_accessible_default_action();
        assert!(!window.global::<ui::Cleaner>().get_confirming());
        // Nothing checked: nothing to clean.
        let_the_screen_settle();
        button(&window, "Tout désélectionner").invoke_accessible_default_action();
        assert_eq!(
            button(&window, "Nettoyer").accessible_enabled(),
            Some(false)
        );
        window.global::<ui::Cleaner>().invoke_clean();
        assert!(!window.global::<ui::Cleaner>().get_confirming());
    }

    #[test]
    fn a_deletion_log_that_cannot_be_kept_removes_nothing() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        let not_a_directory = sandbox.file("state-is-a-file");
        settle_files();
        let (window, _screen) = on_results(sandbox.read(&not_a_directory));
        confirm(&window);
        assert_eq!(step(&window), ui::CleanerStep::Results);
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::History
        );
        assert!(shows_a_text_starting(
            &window,
            "L'historique ne peut pas être tenu"
        ));
        assert!(old.exists());
        // An analysis may start: the refusal is then over.
        let_the_screen_settle();
        button(&window, "Nouvelle analyse").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::None
        );
    }

    #[test]
    fn a_confirmation_open_stays_open_when_its_page_comes_back() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        button(&window, "Nettoyer").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        window.set_page(ui::Page::About);
        slint::platform::update_timers_and_animations();
        window.set_page(ui::Page::Cleaner);
        slint::platform::update_timers_and_animations();
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Supprimer définitivement")
                .next()
                .is_some(),
            "never a frozen screen without its confirmation"
        );
    }

    /// A cleaning that waits for the test's go before cleaning for real.
    fn held_back(seams: Seams) -> (Seams, std::sync::mpsc::Sender<()>) {
        let (go, gone) = std::sync::mpsc::channel::<()>();
        let gone = std::sync::Mutex::new(gone);
        let real = Arc::clone(&seams.clean);
        let clean: super::super::Clean = Arc::new(move |job, cancel, progress| {
            // A test that ends without its go drops it, which releases this
            // at once; the bound only spares a test stuck holding it.
            if gone
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .is_err()
            {
                tracing::warn!("held back cleaning released by its bound");
            }
            real(job, cancel, progress)
        });
        (Seams { clean, ..seams }, go)
    }

    #[test]
    fn closing_during_a_cleaning_waits_for_its_receipt_and_history() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let (seams, go) = held_back(sandbox.read(&sandbox.root.join("state")));
        let (window, screen) = on_results(seams);
        confirm(&window);
        assert_eq!(step(&window), ui::CleanerStep::Cleaning);
        window.window().show().unwrap();
        window.set_page(ui::Page::About);
        assert_eq!(
            screen.close_requested(),
            slint::CloseRequestResponse::KeepWindowShown,
            "the history line first"
        );
        assert!(shows(&window, "Annulation du nettoyage"));
        assert_eq!(
            window.get_page(),
            ui::Page::Cleaner,
            "shown, wherever the user was"
        );
        go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(
            !window.window().is_visible(),
            "closed once the receipt is in"
        );
        assert!(old.exists(), "cancelled before its turn");
        assert!(shows(&window, "Nettoyage annulé"));
        let history = sandbox.history();
        assert_eq!(history.len(), 1, "recorded before the window went");
        assert_eq!(history[0]["kept"], 1);
    }

    #[test]
    fn closing_again_during_a_cleaning_closes() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (seams, go) = held_back(sandbox.read(&sandbox.root.join("state")));
        let (window, screen) = on_results(seams);
        confirm(&window);
        assert_eq!(
            screen.close_requested(),
            slint::CloseRequestResponse::KeepWindowShown
        );
        let_the_screen_settle();
        assert_eq!(
            screen.close_requested(),
            slint::CloseRequestResponse::HideWindow,
            "the user insisted"
        );
        go.send(()).unwrap();
        screen.shut_down();
        assert_eq!(
            sandbox.history().len(),
            1,
            "the cancelled cleaning finished in time: its line is written"
        );
    }

    #[test]
    fn a_double_click_on_close_is_no_insistence() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (seams, go) = held_back(sandbox.read(&sandbox.root.join("state")));
        let (window, screen) = on_results(seams);
        confirm(&window);
        for _ in 0..2 {
            assert_eq!(
                screen.close_requested(),
                slint::CloseRequestResponse::KeepWindowShown
            );
        }
        go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert_eq!(sandbox.history().len(), 1);
    }

    #[test]
    fn cancelling_while_removing_keeps_the_check_done_and_the_removal_where_it_was() {
        let sandbox = Sandbox::new();
        for name in ["a", "b", "c"] {
            sandbox.file(&format!("home/.cache/pip/{name}"));
        }
        settle_files();
        let seams = sandbox.read(&sandbox.root.join("state"));
        let real = Arc::clone(&seams.clean);
        let (emitted, reached) = std::sync::mpsc::channel::<()>();
        let (go, gone) = std::sync::mpsc::channel::<()>();
        let gone = std::sync::Mutex::new(gone);
        // The check over and one entry removed, as the engine says them;
        // then the real cleaning, cancelled by then.
        let clean: super::super::Clean = Arc::new(move |job, cancel, progress| {
            let total = job.ids.len();
            for (pass, done) in [
                (super::CleanPass::Check, total),
                (super::CleanPass::Remove, 1),
            ] {
                progress
                    .send(super::Progress::Cleaning {
                        pass,
                        done,
                        total,
                        removed_bytes: 0,
                    })
                    .unwrap();
            }
            emitted.send(()).unwrap();
            gone.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
            real(job, cancel, progress)
        });
        let (window, _screen) = on_results(Seams { clean, ..seams });
        confirm(&window);
        reached.recv_timeout(Duration::from_secs(30)).unwrap();
        let_the_screen_settle();
        button(&window, "Annuler le nettoyage").invoke_accessible_default_action();
        go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        let counted: Vec<_> = {
            use slint::Model as _;
            window
                .global::<ui::Cleaner>()
                .get_clean_steps()
                .iter()
                .map(|step| (step.kind, step.state, step.done, step.total))
                .collect()
        };
        assert_eq!(
            counted,
            [
                (ui::CleanStepKind::Check, ui::RowState::Done, 3, 3),
                (ui::CleanStepKind::Remove, ui::RowState::NotDone, 1, 3),
                (ui::CleanStepKind::History, ui::RowState::Done, 0, 0),
            ],
            "the entries left after the cancel never counted as gone through"
        );
    }

    #[test]
    fn a_cancelled_cleaning_is_never_given_up() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let (seams, go) = held_back(sandbox.read(&sandbox.root.join("state")));
        let (window, _screen) = on_results(seams);
        confirm(&window);
        let_the_screen_settle();
        assert_eq!(
            steps_shown(&window),
            [
                (ui::CleanStepKind::Check, ui::RowState::Waiting),
                (ui::CleanStepKind::Remove, ui::RowState::Waiting),
                (ui::CleanStepKind::History, ui::RowState::Waiting),
            ],
            "every step shown from the start"
        );
        button(&window, "Annuler le nettoyage").invoke_accessible_default_action();
        assert_eq!(
            button(&window, "Annuler le nettoyage").accessible_enabled(),
            Some(false)
        );
        // Far beyond the time an analysis is given up after.
        for _ in 0..200 {
            i_slint_backend_testing::mock_elapsed_time(super::super::WATCH_EVERY);
            slint::platform::update_timers_and_animations();
        }
        assert_eq!(step(&window), ui::CleanerStep::Cleaning, "still waiting");
        go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(shows(&window, "Nettoyage annulé"));
        assert!(old.exists());
        assert_eq!(sandbox.history().len(), 1);
        assert_eq!(
            steps_shown(&window),
            [
                (ui::CleanStepKind::Check, ui::RowState::NotDone),
                (ui::CleanStepKind::Remove, ui::RowState::NotDone),
                (ui::CleanStepKind::History, ui::RowState::Done),
            ],
            "cancelled before its check"
        );
    }

    #[test]
    fn each_step_keeps_its_bar_as_the_passes_go() {
        use ui::RowState::{Done, NotDone, Running, Waiting};
        let states = |view: &super::CleanView| -> Vec<_> {
            view.steps(true, 3)
                .iter()
                .map(|step| (step.state, step.done, step.total))
                .collect()
        };
        let mut view = super::CleanView::default();
        assert_eq!(
            states(&view),
            [
                (Running, 0, 0),
                (Waiting, 0, 3),
                (Waiting, 0, 3),
                (Waiting, 0, 0)
            ],
            "root first, its bar sweeping"
        );
        view.admin_done = Some(true);
        view.check = Some((2, 3));
        assert_eq!(
            states(&view),
            [
                (Done, 0, 0),
                (Running, 2, 3),
                (Waiting, 0, 3),
                (Waiting, 0, 0)
            ]
        );
        view.check = Some((3, 3));
        view.remove = Some((1, 3));
        assert_eq!(
            states(&view),
            [(Done, 0, 0), (Done, 3, 3), (Running, 1, 3), (Waiting, 0, 0)],
            "the check stays shown, full"
        );
        view.admin_done = Some(false);
        assert_eq!(states(&view)[0], (NotDone, 0, 0), "said as soon as known");
        let kinds =
            |steps: Vec<ui::CleanStep>| -> Vec<_> { steps.iter().map(|step| step.kind).collect() };
        assert_eq!(
            kinds(view.steps(true, 0)),
            [ui::CleanStepKind::Admin, ui::CleanStepKind::History],
            "root's part alone: no pass of the user's"
        );
        assert_eq!(kinds(view.steps(false, 0)), [ui::CleanStepKind::History]);
    }

    /// What each step came to (root's, the check, the removal, the history),
    /// and the entries it shows done.
    fn ended(view: &super::CleanView, ended: Option<super::Ended>) -> Vec<(ui::RowState, i32)> {
        view.ended_steps(true, 3, ended)
            .iter()
            .map(|step| (step.state, step.done))
            .collect()
    }

    const ALL_DONE: super::Ended = super::Ended {
        admin_done: true,
        user: super::UserEnd::Whole,
        docker_done: true,
        recorded: true,
    };

    #[test]
    fn a_step_is_done_only_to_its_end() {
        use ui::RowState::{Done, NotDone};
        let removing = super::CleanView {
            admin_done: Some(true),
            check: Some((3, 3)),
            remove: Some((1, 3)),
            ..super::CleanView::default()
        };
        assert_eq!(
            ended(&removing, Some(ALL_DONE)),
            [(Done, 0), (Done, 3), (Done, 3), (Done, 0)],
            "a pass done to its end counts every entry"
        );
        let cancelled = super::Ended {
            user: super::UserEnd::Cancelled,
            ..ALL_DONE
        };
        let cancelled_while_removing = super::CleanView {
            cancel_asked: true,
            ..removing
        };
        assert_eq!(
            ended(&cancelled_while_removing, Some(cancelled)),
            [(Done, 0), (Done, 3), (NotDone, 1), (Done, 0)],
            "cancelled during the removal: the check was over"
        );
        let checking = super::CleanView {
            check: Some((1, 3)),
            remove: None,
            ..cancelled_while_removing
        };
        assert_eq!(
            ended(&checking, Some(cancelled)),
            [(Done, 0), (NotDone, 1), (NotDone, 0), (Done, 0)],
            "cancelled during the check"
        );
        let between = super::CleanView {
            check: Some((3, 3)),
            ..checking
        };
        assert_eq!(
            ended(&between, Some(cancelled))[1],
            (Done, 3),
            "cancelled once the check was over"
        );
        let removing_only = super::CleanView {
            check: None,
            remove: Some((1, 3)),
            ..between
        };
        assert_eq!(
            ended(&removing_only, Some(cancelled))[1],
            (Done, 3),
            "the removal begun says the check was over"
        );
        let too_late = super::CleanView {
            check: Some((2, 3)),
            remove: None,
            ..removing_only
        };
        assert_eq!(
            ended(&too_late, Some(ALL_DONE)),
            [(Done, 0), (Done, 3), (Done, 3), (Done, 0)],
            "cancelled once everything was done: done, every entry counted"
        );
        let stopped = super::Ended {
            admin_done: false,
            user: super::UserEnd::Stopped,
            docker_done: false,
            recorded: false,
        };
        assert_eq!(
            ended(&super::CleanView::default(), Some(stopped)),
            [(NotDone, 0), (NotDone, 0), (NotDone, 0), (NotDone, 0)],
            "nothing of a stopped cleaning is done"
        );
    }

    #[test]
    fn a_cleaning_whose_thread_dies_keeps_only_what_it_had_finished() {
        use ui::RowState::{Done, NotDone};
        let removing = super::CleanView {
            admin_done: Some(true),
            check: Some((3, 3)),
            remove: Some((1, 3)),
            ..super::CleanView::default()
        };
        assert_eq!(
            ended(&removing, None),
            [(Done, 0), (Done, 3), (NotDone, 1), (NotDone, 0)]
        );
    }

    /// Each step shown, by kind, with its state.
    fn steps_shown(window: &ui::AppWindow) -> Vec<(ui::CleanStepKind, ui::RowState)> {
        use slint::Model as _;
        window
            .global::<ui::Cleaner>()
            .get_clean_steps()
            .iter()
            .map(|step| (step.kind, step.state))
            .collect()
    }

    #[test]
    fn a_cleaning_whose_thread_dies_says_it_was_interrupted() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let seams = Seams {
            clean: Arc::new(|_, _, _| panic!("the cleaning thread dies, as a test asks")),
            ..sandbox.read(&sandbox.root.join("state"))
        };
        let (window, screen) = on_results(seams);
        confirm(&window);
        window.window().show().unwrap();
        assert_eq!(
            screen.close_requested(),
            slint::CloseRequestResponse::KeepWindowShown
        );
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(
            window.window().is_visible(),
            "the lost deletion log is read before the window goes"
        );
        assert!(shows(&window, "Nettoyage interrompu"));
        assert!(shows(&window, "L'historique n'a pas été écrit."));
        assert!(
            steps_shown(&window)
                .iter()
                .all(|(_, state)| *state == ui::RowState::NotDone),
            "nothing it had not finished is said done"
        );
        assert!(window.global::<ui::Cleaner>().get_deletion_log_lost());
        assert!(shows_a_text_starting(
            &window,
            "Le journal des suppressions que vous demandez n'a pas été écrit"
        ));
    }

    #[test]
    fn the_results_answer_the_keyboard() {
        use slint::platform::Key;

        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        // The sidebar, "Choisir d'autres catégories", "Nouvelle analyse",
        // then the fold of Applications.
        for _ in 0..SIDEBAR_STOPS + 3 {
            press(&window, Key::Tab);
        }
        let fold = || {
            ElementHandle::find_by_accessible_label(&window, "Replier Applications")
                .chain(ElementHandle::find_by_accessible_label(
                    &window,
                    "Déplier Applications",
                ))
                .next()
                .unwrap()
        };
        assert_eq!(fold().accessible_expanded(), Some(true));
        press(&window, Key::Return);
        assert_eq!(fold().accessible_expanded(), Some(false));
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Cache de pip")
                .next()
                .is_none(),
            "folded away"
        );
        press(&window, Key::Return);
        assert_eq!(fold().accessible_expanded(), Some(true), "the focus stayed");
        // The category's checkbox, then the group's.
        press(&window, Key::Tab);
        press(&window, Key::Tab);
        press(&window, Key::Space);
        assert_eq!(
            found(&window, "Cache de pip").accessible_checked(),
            Some(false)
        );
    }

    #[test]
    fn only_the_groups_checked_are_cleaned() {
        let sandbox = Sandbox::new();
        // Yarn, pip, Pipenv: the rules' order; the one in the middle stays.
        let yarn = sandbox.file("home/.cache/yarn/old");
        let pip = sandbox.file("home/.cache/pip/old");
        let pipenv = sandbox.file("home/.cache/pipenv/old");
        settle_files();
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        found(&window, "Cache de pip").invoke_accessible_default_action();
        assert_eq!(
            found(&window, "Cache de pip").accessible_checked(),
            Some(false)
        );
        assert!(shows(&window, "Éléments : 2 sur 3"));
        button(&window, "Nettoyer").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(shows_a_text_starting(
            &window,
            "Taille : 14 o. Éléments : 2."
        ));
        let_the_screen_settle();
        found(&window, "J'ai compris que c'est définitif").invoke_accessible_default_action();
        button(&window, "Supprimer définitivement").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!yarn.exists());
        assert!(pip.exists(), "unchecked, never cleaned (invariant 1)");
        assert!(!pipenv.exists());
        let mut paths: Vec<String> = sandbox.deletion_logs()[0]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap().to_owned())
            .collect();
        paths.sort();
        let mut expected = [pipenv, yarn].map(|path| path.to_string_lossy().into_owned());
        expected.sort();
        assert_eq!(paths, expected);
    }

    #[test]
    fn the_confirmation_starts_on_cancel_and_closes_with_escape() {
        use slint::platform::Key;

        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        button(&window, "Nettoyer").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        let_the_screen_settle();
        press(&window, Key::Return);
        assert!(
            !window.global::<ui::Cleaner>().get_confirming(),
            "Enter on the focus, \"Annuler\""
        );
        let_the_screen_settle();
        button(&window, "Nettoyer").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(
            found(&window, "J'ai compris que c'est définitif").accessible_checked(),
            Some(false),
            "never left checked from before"
        );
        found(&window, "J'ai compris que c'est définitif").invoke_accessible_default_action();
        let_the_screen_settle();
        // Tab goes round the confirmation, never below it.
        // "Supprimer", the checkbox, then "Annuler" again.
        for _ in 0..3 {
            press(&window, Key::Tab);
        }
        press(&window, Key::Return);
        assert!(
            !window.global::<ui::Cleaner>().get_confirming(),
            "Enter on \"Annuler\" again, never on what lies below"
        );
        assert!(old.exists());
        button(&window, "Nettoyer").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        let_the_screen_settle();
        press(&window, Key::Escape);
        assert!(!window.global::<ui::Cleaner>().get_confirming());
        assert!(!screen.is_confirming(), "closed for Rust too");
        let_the_screen_settle();
        found(&window, "Cache de pip").invoke_accessible_default_action();
        assert_eq!(
            found(&window, "Cache de pip").accessible_checked(),
            Some(false),
            "the results answer again"
        );
        found(&window, "Cache de pip").invoke_accessible_default_action();
        let_the_screen_settle();
        button(&window, "Nettoyer").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(
            found(&window, "J'ai compris que c'est définitif").accessible_checked(),
            Some(false),
            "a new confirmation, unchecked"
        );
        assert!(old.exists());
    }

    #[test]
    fn a_double_click_never_opens_the_confirmation_as_the_results_arrive() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen) = on_cleaner(sandbox.read(&sandbox.root.join("state")));
        for category in ["Système", "Jeux"] {
            checkbox(&window, category).invoke_accessible_default_action();
        }
        let_the_screen_settle();
        button(&window, "Analyser").invoke_accessible_default_action();
        let_the_screen_settle();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        // "Nettoyer" in place of "Annuler l'analyse", clicked twice.
        button(&window, "Nettoyer").invoke_accessible_default_action();
        assert!(!window.global::<ui::Cleaner>().get_confirming());
    }

    #[test]
    fn the_results_the_cleaning_and_the_receipt_narrow_their_margins_in_a_narrow_window() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (seams, go) = held_back(sandbox.read(&sandbox.root.join("state")));
        let (window, _screen) = on_results(seams);
        assert!((start_of(&window, "Résultats") - (248.0 + 32.0)).abs() < 1.0);
        resize(&window, 1000);
        // The sidebar folded, the narrow margin: the title and the footer
        // on the left, the footer's button on the right.
        assert!((start_of(&window, "Résultats") - (64.0 + 24.0)).abs() < 1.0);
        assert!((start_of(&window, "Sélectionné") - (64.0 + 24.0)).abs() < 1.0);
        let clean = button(&window, "Nettoyer");
        let right = clean.absolute_position().x + clean.size().width;
        assert!((right - (1000.0 - 24.0)).abs() < 1.0, "{right}");
        confirm(&window);
        let_the_screen_settle();
        assert!((start_of(&window, "Nettoyage en cours") - (64.0 + 24.0)).abs() < 1.0);
        go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!((start_of(&window, "Nettoyage terminé") - (64.0 + 24.0)).abs() < 1.0);
    }

    #[test]
    fn a_receipt_says_when_the_history_is_unavailable() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let inputs = Inputs {
            deletion_log: false,
            ..sandbox.inputs(None)
        };
        let (window, _screen) = on_results(Seams {
            read: Rc::new(move || Ok(inputs.clone())),
            ..seams_never_scanning()
        });
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!old.exists(), "without a deletion log, it goes on");
        assert!(shows(
            &window,
            "L'historique est indisponible : ce reçu n'y est pas ajouté. Le détail est dans le journal."
        ));
        assert!(!window.global::<ui::Cleaner>().get_deletion_log_lost());
    }

    #[test]
    fn a_cleaning_stopped_before_reading_says_why() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let deleting = Rc::new(std::cell::Cell::new(false));
        let lacking = Rc::clone(&deleting);
        let inputs = sandbox.inputs(Some(&sandbox.root.join("state")));
        let (window, _screen) = on_results(Seams {
            read: Rc::new(move || {
                let mut read = inputs.clone();
                // The settings read as the deletion starts lack a variable.
                if lacking.get() {
                    read.variables = TemplateVariables::new();
                }
                Ok(read)
            }),
            ..seams_never_scanning()
        });
        deleting.set(true);
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(old.exists());
        assert!(shows(&window, "Nettoyage arrêté"));
        assert!(shows(&window, "Rien n'a été touché."));
        assert!(shows(
            &window,
            "La variable HOME n'est pas définie : ce qu'elle protège ne peut pas être garanti."
        ));
        let line = &sandbox.history()[0];
        assert_eq!(line["removed"], 0);
        assert!(
            line["stops"]
                .as_array()
                .unwrap()
                .iter()
                .all(|stop| stop == "missingVariable"),
            "{line}"
        );
    }

    /// The checkbox of the line `label`: where it sits and how high it is.
    fn place(window: &ui::AppWindow, label: &str) -> (f32, f32, f32) {
        let line = found(window, label);
        let (position, size) = (line.absolute_position(), line.size());
        (position.x, position.y, size.height)
    }

    #[test]
    fn the_lines_of_the_results_never_overlap_and_fold_away() {
        let sandbox = Sandbox::new();
        // Many groups: more lines than the list shows.
        for rule in ["yarn", "pip", "pipenv", "npm/_cacache", "go-build", "deno"] {
            let relative = if rule.contains('/') {
                format!("home/.{rule}/old")
            } else {
                format!("home/.cache/{rule}/old")
            };
            sandbox.file(&relative);
        }
        sandbox.file("home/.cache/thumbnails/normal/old");
        // Left to choose: its note makes it a line of two.
        sandbox.file("home/.cache/fontconfig/old");
        settle_files();
        let (window, _screen) = on_cleaner(sandbox.read(&sandbox.root.join("state")));
        // Too short for every line, high enough to show Applications: what
        // the window does not show, the tests cannot find.
        window
            .window()
            .set_size(slint::PhysicalSize::new(1280, 600));
        checkbox(&window, "Jeux").invoke_accessible_default_action();
        let_the_screen_settle();
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        // Lines at the top of a list too short for all: never squeezed.
        let (_, system_top, system_height) = place(&window, "Système");
        let (_, thumbnails_top, thumbnails_height) = place(&window, "Cache des miniatures");
        assert!(
            thumbnails_top >= system_top + system_height,
            "a line below the one before: {system_top} + {system_height} > {thumbnails_top}"
        );
        let (_, applications_top, _) = place(&window, "Applications");
        assert!(
            applications_top >= thumbnails_top + thumbnails_height,
            "{thumbnails_top} + {thumbnails_height} > {applications_top}"
        );
        // A checkbox covers its name only: a line runs from it to the next.
        let (_, font_top, _) = place(&window, "Cache des polices");
        assert!(
            applications_top - font_top > font_top - thumbnails_top,
            "a line with its note is higher: {thumbnails_top} {font_top} {applications_top}"
        );
        let (category_x, ..) = place(&window, "Système");
        let (group_x, ..) = place(&window, "Cache des miniatures");
        assert!(group_x > category_x, "a group further in than its category");
        // Folded, Système leaves no room: Applications comes right below,
        // nearer than the second group was.
        let_the_screen_settle();
        window.global::<ui::Cleaner>().invoke_fold(0);
        let (_, folded_top, _) = place(&window, "Applications");
        assert!(
            folded_top - system_top < font_top - system_top,
            "folded away: {system_top} {font_top} {folded_top}"
        );
    }

    /// The rules' labels and Slint's own texts in one language: `words` are
    /// two categories to leave out, "Analyser", "Résultats" and pip's label.
    fn labels_follow(language: okapi_rules::Language, words: [&str; 5]) {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen) =
            super::super::tests::on_cleaner_in(sandbox.read(&sandbox.root.join("state")), language);
        let [system, games, analyze, results, pip] = words;
        for category in [system, games] {
            checkbox(&window, category).invoke_accessible_default_action();
        }
        let_the_screen_settle();
        button(&window, analyze).invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        assert!(shows(&window, results), "{language:?}: Slint's own text");
        assert_eq!(
            found(&window, pip).accessible_checked(),
            Some(true),
            "{language:?}: the rule's label"
        );
    }

    #[test]
    fn the_labels_are_french_in_a_french_window() {
        labels_follow(
            okapi_rules::Language::French,
            ["Système", "Jeux", "Analyser", "Résultats", "Cache de pip"],
        );
    }

    #[test]
    fn the_labels_are_english_in_an_english_window() {
        labels_follow(
            okapi_rules::Language::English,
            ["System", "Games", "Analyze", "Results", "pip Cache"],
        );
    }

    /// Chrome's cache and pip's, Chrome open: its lock names this machine
    /// and this very process (live, and never signalled by Okapi).
    fn chrome_open(sandbox: &Sandbox, with_pip: bool) -> (PathBuf, Option<PathBuf>, PathBuf) {
        let chrome = sandbox.file("home/.config/google-chrome/Default/Cache/old");
        let pip = with_pip.then(|| sandbox.file("home/.cache/pip/old"));
        let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        let lock = sandbox
            .root
            .join("home/.config/google-chrome/SingletonLock");
        std::os::unix::fs::symlink(format!("{}-{}", host.trim(), std::process::id()), &lock)
            .unwrap();
        settle_files();
        (chrome, pip, lock)
    }

    /// The lock there, though it points nowhere (`exists` follows it).
    fn there(lock: &Path) -> bool {
        fs::symlink_metadata(lock).is_ok()
    }

    /// The cleaning confirmed, and the question shown.
    fn confirm_and_ask(window: &ui::AppWindow) {
        confirm(window);
        slint::platform::update_timers_and_animations();
    }

    fn asking(window: &ui::AppWindow) -> bool {
        window.global::<ui::Cleaner>().get_browser_asking()
    }

    fn no_button(window: &ui::AppWindow, label: &str) -> bool {
        ElementHandle::find_by_accessible_label(window, label)
            .all(|element| element.accessible_role() != Some(AccessibleRole::Button))
    }

    #[test]
    fn an_open_browser_okapi_cannot_vouch_for_can_be_left_out() {
        let sandbox = Sandbox::new();
        let (chrome, pip, _) = chrome_open(&sandbox, true);
        let pip = pip.unwrap();
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        confirm_and_ask(&window);
        assert!(asking(&window));
        assert!(shows(&window, "Chrome est ouvert"));
        assert!(shows(&window, "Chrome : Cache"));
        assert!(
            no_button(&window, "Fermer Chrome et continuer"),
            "never signalled"
        );
        assert!(shows_a_text_starting(
            &window,
            "Okapi ne peut pas fermer Chrome"
        ));
        let_the_screen_settle();
        found(&window, "Cache de pip").invoke_accessible_default_action();
        assert_eq!(
            found(&window, "Cache de pip").accessible_checked(),
            Some(true),
            "the results frozen under the question"
        );
        button(&window, "Ignorer Chrome").invoke_accessible_default_action();
        assert!(!asking(&window));
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!pip.exists());
        assert!(chrome.exists(), "left out this time");
        assert_eq!(sandbox.history()[0]["removed"], 1);
    }

    #[test]
    fn okapi_closes_an_open_browser_then_cleans() {
        let sandbox = Sandbox::new();
        let (chrome, _, lock) = chrome_open(&sandbox, false);
        let seams = Seams {
            // A browser that closes when asked: its lock goes.
            browser: Rc::new(|lock| {
                let lock = lock.lock().to_path_buf();
                super::super::browsers::Asked::Closable {
                    pid: 1,
                    close: Box::new(move || fs::remove_file(lock)),
                }
            }),
            ..sandbox.read(&sandbox.root.join("state"))
        };
        let (window, _screen) = on_results(seams);
        confirm_and_ask(&window);
        assert!(asking(&window));
        button(&window, "Fermer Chrome et continuer").invoke_accessible_default_action();
        assert!(
            there(&lock),
            "the second click of a double click on Supprimer"
        );
        let_the_screen_settle();
        button(&window, "Fermer Chrome et continuer").invoke_accessible_default_action();
        assert!(!there(&lock));
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!asking(&window));
        assert!(!chrome.exists(), "cleaned once closed");
    }

    #[test]
    fn a_browser_that_does_not_close_is_said_and_can_be_left_out() {
        let sandbox = Sandbox::new();
        let (chrome, _, lock) = chrome_open(&sandbox, false);
        let seams = Seams {
            browser: Rc::new(|_| super::super::browsers::Asked::Closable {
                pid: 1,
                close: Box::new(|| Ok(())),
            }),
            ..sandbox.read(&sandbox.root.join("state"))
        };
        let (window, _screen) = on_results(seams);
        confirm_and_ask(&window);
        let_the_screen_settle();
        button(&window, "Fermer Chrome et continuer").invoke_accessible_default_action();
        assert!(shows(&window, "Fermeture de Chrome…"));
        for _ in 0..super::super::browsers::CLOSE_LOOKS {
            i_slint_backend_testing::mock_elapsed_time(super::super::browsers::CLOSE_LOOK_EVERY);
            slint::platform::update_timers_and_animations();
        }
        assert!(shows(
            &window,
            "Chrome ne s'est pas fermé : fermez-le, puis relancez le nettoyage."
        ));
        assert!(
            no_button(&window, "Fermer Chrome et continuer"),
            "asked once only"
        );
        assert!(there(&lock));
        let_the_screen_settle();
        button(&window, "Ignorer Chrome").invoke_accessible_default_action();
        assert!(!asking(&window));
        assert_eq!(
            step(&window),
            ui::CleanerStep::Results,
            "nothing left to clean"
        );
        assert!(chrome.exists());
        assert_eq!(
            sandbox.history(),
            Vec::<serde_json::Value>::new(),
            "no history line"
        );
    }

    #[test]
    fn escape_at_an_open_browser_cancels_the_cleaning() {
        let sandbox = Sandbox::new();
        let (chrome, pip, _) = chrome_open(&sandbox, true);
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        confirm_and_ask(&window);
        let_the_screen_settle();
        window.global::<ui::Cleaner>().invoke_cancel_browser();
        assert!(!asking(&window));
        assert_eq!(step(&window), ui::CleanerStep::Results);
        assert!(chrome.exists());
        assert!(pip.unwrap().exists(), "nothing removed");
        assert_eq!(sandbox.history(), Vec::<serde_json::Value>::new());
    }

    /// A browser's cache in the sandbox, the browser open (see
    /// [`chrome_open`]).
    fn open_browser(sandbox: &Sandbox, base: &str, files: usize) -> PathBuf {
        for file in 0..files {
            sandbox.file(&format!("home/.config/{base}/Default/Cache/{file}"));
        }
        let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        let lock = sandbox
            .root
            .join(format!("home/.config/{base}/SingletonLock"));
        std::os::unix::fs::symlink(format!("{}-{}", host.trim(), std::process::id()), &lock)
            .unwrap();
        lock
    }

    #[test]
    fn two_open_browsers_are_asked_about_one_after_the_other() {
        let sandbox = Sandbox::new();
        // Of sizes apart: what is left is counted by entry, not by group.
        open_browser(&sandbox, "google-chrome", 3);
        let edge = open_browser(&sandbox, "microsoft-edge", 2);
        let pip = sandbox.file("home/.cache/pip/old");
        sandbox.file("home/.cache/pip/second");
        sandbox.file("home/.cache/pip/third");
        settle_files();
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        confirm_and_ask(&window);
        assert!(shows(&window, "Chrome est ouvert"));
        // The second click of a double click on Supprimer leaves nothing out.
        window.global::<ui::Cleaner>().invoke_ignore_browser();
        assert!(asking(&window));
        assert!(shows(&window, "Chrome est ouvert"));
        let_the_screen_settle();
        // No cleaning starts under the question.
        window.global::<ui::Cleaner>().invoke_clean();
        assert!(!window.global::<ui::Cleaner>().get_confirming());
        assert!(asking(&window));
        button(&window, "Ignorer Chrome").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(shows(&window, "Edge est ouvert"), "then the next one");
        let_the_screen_settle();
        button(&window, "Ignorer Edge").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!pip.exists());
        assert!(there(&edge));
        assert_eq!(sandbox.history()[0]["removed"], 3, "pip alone");
    }

    #[test]
    fn firefox_with_two_profiles_open_is_asked_about_once() {
        let sandbox = Sandbox::new();
        let mut held = Vec::new();
        let mut caches = Vec::new();
        for profile in ["a.default-release", "b.default"] {
            let lock = sandbox.file(&format!(
                "home/.config/mozilla/firefox/{profile}/.parentlock"
            ));
            let file = fs::File::open(&lock).unwrap();
            file.lock().unwrap();
            held.push(file);
            caches.push(sandbox.file(&format!(
                "home/.cache/mozilla/firefox/{profile}/cache2/entry"
            )));
        }
        let pip = sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        confirm_and_ask(&window);
        assert!(shows(&window, "Firefox est ouvert"));
        assert!(shows(&window, "Firefox : Cache"));
        assert!(
            no_button(&window, "Fermer Firefox et continuer"),
            "a lock Okapi holds is never Okapi to close"
        );
        let_the_screen_settle();
        button(&window, "Ignorer Firefox").invoke_accessible_default_action();
        assert!(
            !asking(&window),
            "its other profile's caches are left out already"
        );
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!pip.exists());
        assert!(caches.iter().all(|cache| cache.exists()));
        assert_eq!(sandbox.history()[0]["removed"], 1, "pip alone");
    }

    /// The rules as shipped: the system's targets, the snaps' among them,
    /// are not analysed here (`/tmp` is the machine's), their reservation of a
    /// browser's caches is the engine's to test.
    #[test]
    fn firefox_in_a_snap_is_asked_about_as_any_firefox() {
        let sandbox = Sandbox::new();
        let lock = sandbox.file("home/snap/firefox/common/.mozilla/firefox/a.default/.parentlock");
        let held = fs::File::open(&lock).unwrap();
        held.lock().unwrap();
        let cache =
            sandbox.file("home/snap/firefox/common/.cache/mozilla/firefox/a.default/cache2/entry");
        let pip = sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        confirm_and_ask(&window);
        assert!(shows(&window, "Firefox est ouvert"));
        assert!(shows(&window, "Firefox : Cache"));
        let_the_screen_settle();
        button(&window, "Ignorer Firefox").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!pip.exists());
        assert!(cache.exists());
        assert_eq!(sandbox.history()[0]["removed"], 1, "pip alone");
        drop(held);
    }

    #[test]
    fn a_browser_closed_meanwhile_is_not_asked_about() {
        let sandbox = Sandbox::new();
        open_browser(&sandbox, "google-chrome", 3);
        let edge = open_browser(&sandbox, "microsoft-edge", 1);
        settle_files();
        let (window, _screen) = on_results(sandbox.read(&sandbox.root.join("state")));
        confirm_and_ask(&window);
        fs::remove_file(&edge).unwrap();
        let_the_screen_settle();
        button(&window, "Ignorer Chrome").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!asking(&window));
        assert_eq!(sandbox.history()[0]["removed"], 1, "Edge's cache, closed");
        assert!(
            sandbox
                .root
                .join("home/.config/google-chrome/Default/Cache/0")
                .exists()
        );
        assert!(
            !sandbox
                .root
                .join("home/.config/microsoft-edge/Default/Cache/0")
                .exists()
        );
    }

    #[test]
    fn the_holder_is_asked_again_at_the_click() {
        use std::cell::Cell;

        let sandbox = Sandbox::new();
        open_browser(&sandbox, "google-chrome", 1);
        settle_files();
        let asked = Rc::new(Cell::new(0));
        let signalled = Rc::new(Cell::new(0));
        let (asked_by, signalled_by) = (Rc::clone(&asked), Rc::clone(&signalled));
        let seams = Seams {
            // Vouched for when shown, no more at the click.
            browser: Rc::new(move |_| {
                asked_by.set(asked_by.get() + 1);
                if asked_by.get() == 1 {
                    let signalled = Rc::clone(&signalled_by);
                    super::super::browsers::Asked::Closable {
                        pid: 1,
                        close: Box::new(move || {
                            signalled.set(signalled.get() + 1);
                            Ok(())
                        }),
                    }
                } else {
                    super::super::browsers::Asked::Unverified
                }
            }),
            ..sandbox.read(&sandbox.root.join("state"))
        };
        let (window, _screen) = on_results(seams);
        confirm_and_ask(&window);
        let_the_screen_settle();
        button(&window, "Fermer Chrome et continuer").invoke_accessible_default_action();
        assert_eq!(asked.get(), 2, "asked again at the click");
        assert_eq!(signalled.get(), 0, "never signalled");
        assert!(shows_a_text_starting(
            &window,
            "Okapi ne peut pas fermer Chrome"
        ));
    }

    #[test]
    fn a_refused_request_to_close_is_said_at_once() {
        let sandbox = Sandbox::new();
        open_browser(&sandbox, "google-chrome", 1);
        settle_files();
        let seams = Seams {
            browser: Rc::new(|_| super::super::browsers::Asked::Closable {
                pid: 1,
                close: Box::new(|| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))),
            }),
            ..sandbox.read(&sandbox.root.join("state"))
        };
        let (window, _screen) = on_results(seams);
        confirm_and_ask(&window);
        let_the_screen_settle();
        button(&window, "Fermer Chrome et continuer").invoke_accessible_default_action();
        assert!(shows(
            &window,
            "Le système a refusé de demander à Chrome de se fermer : fermez-le, puis relancez le nettoyage."
        ));
        assert!(asking(&window));
        assert!(no_button(&window, "Fermer Chrome et continuer"));
    }

    #[test]
    fn a_browser_has_its_whole_wait_to_close() {
        use super::super::browsers::{CLOSE_LOOK_EVERY, CLOSE_LOOKS};

        assert_eq!(
            CLOSE_LOOK_EVERY * CLOSE_LOOKS,
            Duration::from_secs(15),
            "time for a browser to save its session"
        );
        let sandbox = Sandbox::new();
        let lock = open_browser(&sandbox, "google-chrome", 1);
        settle_files();
        let seams = Seams {
            browser: Rc::new(|_| super::super::browsers::Asked::Closable {
                pid: 1,
                close: Box::new(|| Ok(())),
            }),
            ..sandbox.read(&sandbox.root.join("state"))
        };
        let (window, _screen) = on_results(seams);
        confirm_and_ask(&window);
        let_the_screen_settle();
        button(&window, "Fermer Chrome et continuer").invoke_accessible_default_action();
        let one_look = || {
            i_slint_backend_testing::mock_elapsed_time(CLOSE_LOOK_EVERY);
            slint::platform::update_timers_and_animations();
        };
        for _ in 1..CLOSE_LOOKS {
            one_look();
        }
        assert!(shows(&window, "Fermeture de Chrome…"), "still waiting");
        // Closed at the last look: the cleaning goes on.
        fs::remove_file(&lock).unwrap();
        one_look();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert_eq!(sandbox.history()[0]["removed"], 1);
    }

    #[test]
    fn escape_right_after_fermer_still_cancels() {
        let sandbox = Sandbox::new();
        let lock = open_browser(&sandbox, "google-chrome", 1);
        let pip = sandbox.file("home/.cache/pip/old");
        settle_files();
        let seams = Seams {
            browser: Rc::new(|lock| {
                let lock = lock.lock().to_path_buf();
                super::super::browsers::Asked::Closable {
                    pid: 1,
                    close: Box::new(move || fs::remove_file(lock)),
                }
            }),
            ..sandbox.read(&sandbox.root.join("state"))
        };
        let (window, _screen) = on_results(seams);
        confirm_and_ask(&window);
        let_the_screen_settle();
        button(&window, "Fermer Chrome et continuer").invoke_accessible_default_action();
        assert!(!there(&lock), "asked to close");
        window.global::<ui::Cleaner>().invoke_cancel_browser();
        assert!(!asking(&window));
        for _ in 0..super::super::browsers::CLOSE_LOOKS {
            i_slint_backend_testing::mock_elapsed_time(super::super::browsers::CLOSE_LOOK_EVERY);
            slint::platform::update_timers_and_animations();
        }
        assert_eq!(step(&window), ui::CleanerStep::Results, "nothing started");
        assert!(pip.exists());
        assert_eq!(sandbox.history(), Vec::<serde_json::Value>::new());
    }

    #[test]
    fn a_browser_found_closed_when_asked_about_is_skipped() {
        let sandbox = Sandbox::new();
        let chrome = sandbox
            .root
            .join("home/.config/google-chrome/Default/Cache/0");
        open_browser(&sandbox, "google-chrome", 1);
        settle_files();
        let seams = Seams {
            browser: Rc::new(|_| super::super::browsers::Asked::Closed),
            ..sandbox.read(&sandbox.root.join("state"))
        };
        let (window, _screen) = on_results(seams);
        confirm(&window);
        slint::platform::update_timers_and_animations();
        assert!(!asking(&window), "no question");
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(
            chrome.exists(),
            "its lock still read as held at removal: kept"
        );
    }

    #[test]
    fn the_focus_goes_back_to_ignorer_when_the_buttons_change() {
        use slint::platform::Key;

        let sandbox = Sandbox::new();
        let chrome = sandbox
            .root
            .join("home/.config/google-chrome/Default/Cache/0");
        open_browser(&sandbox, "google-chrome", 1);
        settle_files();
        let shown = Rc::new(std::cell::Cell::new(false));
        let seams = Seams {
            // Vouched for when shown, no more at the click.
            browser: Rc::new(move |_| {
                if shown.replace(true) {
                    super::super::browsers::Asked::Unverified
                } else {
                    super::super::browsers::Asked::Closable {
                        pid: 1,
                        close: Box::new(|| Ok(())),
                    }
                }
            }),
            ..sandbox.read(&sandbox.root.join("state"))
        };
        let (window, _screen) = on_results(seams);
        confirm_and_ask(&window);
        let_the_screen_settle();
        // From Ignorer to Fermer, pressed: Fermer goes away.
        press(&window, Key::Tab);
        press(&window, Key::Return);
        assert!(asking(&window));
        assert!(no_button(&window, "Fermer Chrome et continuer"));
        slint::platform::update_timers_and_animations();
        let_the_screen_settle();
        press(&window, Key::Return);
        assert!(!asking(&window), "Ignorer took the key");
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        assert!(chrome.exists());
    }

    const CHROME: okapi_rules::BrowserKind =
        okapi_rules::BrowserKind::Chromium(okapi_rules::ChromiumBrowserKind::Chrome);

    /// Chrome and Edge open, the second shown after the first; a seam whose
    /// answer for each browser `answer` gives, from how many times it was
    /// asked about it, and that counts the requests to close.
    fn two_browsers(
        answer: fn(okapi_rules::BrowserKind, u32) -> bool,
    ) -> (Sandbox, Seams, Rc<std::cell::Cell<u32>>) {
        let sandbox = Sandbox::new();
        open_browser(&sandbox, "google-chrome", 1);
        open_browser(&sandbox, "microsoft-edge", 1);
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let signalled = Rc::new(std::cell::Cell::new(0));
        let asked = Rc::new(std::cell::RefCell::new(std::collections::BTreeMap::new()));
        let counted = Rc::clone(&signalled);
        let seams = Seams {
            browser: Rc::new(move |lock| {
                let times = {
                    let mut asked = asked.borrow_mut();
                    let times = asked.entry(lock.browser()).or_insert(0);
                    *times += 1;
                    *times
                };
                if answer(lock.browser(), times) {
                    let counted = Rc::clone(&counted);
                    super::super::browsers::Asked::Closable {
                        pid: if lock.browser() == CHROME { 1 } else { 2 },
                        close: Box::new(move || {
                            counted.set(counted.get() + 1);
                            Ok(())
                        }),
                    }
                } else {
                    super::super::browsers::Asked::Closed
                }
            }),
            ..sandbox.read(&sandbox.root.join("state"))
        };
        (sandbox, seams, signalled)
    }

    #[test]
    fn a_browser_closed_between_the_question_and_the_click_is_skipped() {
        use slint::platform::Key;

        // Chrome open when shown, closed at the click; Edge open.
        let (sandbox, seams, signalled) =
            two_browsers(|browser, times| browser != CHROME || times == 1);
        let (window, _screen) = on_results(seams);
        confirm_and_ask(&window);
        assert!(shows(&window, "Chrome est ouvert"));
        let_the_screen_settle();
        press(&window, Key::Tab);
        press(&window, Key::Return);
        slint::platform::update_timers_and_animations();
        assert!(shows(&window, "Edge est ouvert"), "Chrome skipped");
        assert_eq!(signalled.get(), 0);
        let_the_screen_settle();
        // The focus went back to Ignorer: Return never closes Edge unseen.
        press(&window, Key::Return);
        assert_eq!(signalled.get(), 0, "Edge never asked to close");
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert_eq!(sandbox.history()[0]["removed"], 1, "pip alone");
    }

    #[test]
    fn ignoring_a_browser_while_it_closes_leaves_the_next_one_whole() {
        let (_sandbox, seams, signalled) = two_browsers(|_, _| true);
        let (window, _screen) = on_results(seams);
        confirm_and_ask(&window);
        let_the_screen_settle();
        button(&window, "Fermer Chrome et continuer").invoke_accessible_default_action();
        assert!(shows(&window, "Fermeture de Chrome…"));
        let_the_screen_settle();
        button(&window, "Ignorer Chrome").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(shows(&window, "Edge est ouvert"));
        let_the_screen_settle();
        button(&window, "Fermer Edge et continuer").invoke_accessible_default_action();
        assert!(shows(&window, "Fermeture de Edge…"));
        assert_eq!(signalled.get(), 2);
        for _ in 0..super::super::browsers::CLOSE_LOOKS {
            i_slint_backend_testing::mock_elapsed_time(super::super::browsers::CLOSE_LOOK_EVERY);
            slint::platform::update_timers_and_animations();
        }
        assert!(shows(
            &window,
            "Edge ne s'est pas fermé : fermez-le, puis relancez le nettoyage."
        ));
        // Never a second request to the same browser, in this question or
        // the next.
        let_the_screen_settle();
        window.global::<ui::Cleaner>().invoke_close_browser();
        assert_eq!(signalled.get(), 2);
        window.global::<ui::Cleaner>().invoke_cancel_browser();
        let_the_screen_settle();
        confirm_and_ask(&window);
        assert!(shows(
            &window,
            "Chrome ne s'est pas fermé : fermez-le, puis relancez le nettoyage."
        ));
        assert!(no_button(&window, "Fermer Chrome et continuer"));
        assert_eq!(signalled.get(), 2);
    }

    /// What the fake helper was asked: each operation and its request.
    type Asked = Arc<std::sync::Mutex<Vec<(okapi_helper::Operation, serde_json::Value)>>>;

    /// The embedded rules, the system's targets two that need the
    /// administrator in the sandbox: crash reports and core dumps.
    fn admin_rules(sandbox: &Sandbox) -> RuleSet {
        admin_rules_with(sandbox, &[])
    }

    /// [`admin_rules`], and the targets `more`.
    fn admin_rules_with(sandbox: &Sandbox, more: &[serde_json::Value]) -> RuleSet {
        let target = |relative: &str, subcategory: &str| {
            serde_json::json!({
                "path": sandbox.root.join(relative),
                "subcategory": subcategory,
                "needsAdmin": true,
            })
        };
        let mut targets = vec![
            target("var/crash", "Crash Reports"),
            target("var/dumps", "Core Dumps"),
        ];
        targets.extend_from_slice(more);
        let system = serde_json::json!({
            "type": "system",
            "cleanTargets": targets,
        });
        RuleSet::embedded_with(
            Os::Linux,
            okapi_rules::RuleFileKind::System,
            &system.to_string(),
        )
        .unwrap()
    }

    /// The sandbox's settings, its analysis on `rules`, and a helper that
    /// answers `answer(operation, request)` as the helper writes it, after
    /// saying it is authenticated.
    fn admin_seams(
        sandbox: &Sandbox,
        rules: &RuleSet,
        asked: &Asked,
        answer: impl Fn(okapi_helper::Operation, &serde_json::Value) -> (i32, String)
        + Send
        + Sync
        + 'static,
    ) -> Seams {
        let rules = Arc::new(rules.clone());
        let asked = Arc::clone(asked);
        Seams {
            work: Arc::new(move |job, cancel, progress| {
                okapi_engine::scan_watched(
                    &rules,
                    &job.variables,
                    &job.options,
                    &okapi_engine::Watch::new(cancel, Some(progress)),
                )
            }),
            elevate: Arc::new(move |operation, line, _cancel, steps| {
                let request: serde_json::Value = serde_json::from_slice(&line).unwrap();
                let (code, output) = answer(operation, &request);
                asked.lock().unwrap().push((operation, request));
                steps
                    .send(crate::elevation::ElevationStep::Authenticated)
                    .unwrap();
                okapi_helper::read_reply(operation, Some(code), output.as_bytes())
                    .map_err(crate::elevation::ElevationFailure::Reply)
            }),
            ..sandbox.read(&sandbox.root.join("state"))
        }
    }

    /// The window on the results of an analysis of the system and the
    /// applications, the helper's targets those of `rules`.
    fn on_admin_results(
        seams: Seams,
        rules: &RuleSet,
    ) -> (ui::AppWindow, super::super::CleanerScreen) {
        let (window, screen) = on_cleaner(seams);
        screen.use_helper_targets_of(rules);
        checkbox(&window, "Jeux").invoke_accessible_default_action();
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        let_the_screen_settle();
        (window, screen)
    }

    /// Asks for the rights and waits for root's scan.
    fn obtain_rights(window: &ui::AppWindow) {
        let cleaner = window.global::<ui::Cleaner>();
        cleaner.invoke_obtain_rights();
        assert_ne!(cleaner.get_rights_step(), ui::RightsStep::Idle);
        watch_until(window, |window| {
            window.global::<ui::Cleaner>().get_rights_step() == ui::RightsStep::Idle
        });
        let_the_screen_settle();
    }

    /// Root's scan: the crash reports offered, nothing of the core dumps.
    fn scan_answer(sandbox: &Sandbox) -> String {
        serde_json::json!({
            "version": okapi_helper::PROTOCOL_VERSION,
            "operation": "scan",
            "stops": [],
            "groups": [{ "targets": [sandbox.root.join("var/crash")], "entries": 3, "bytes": 3_000 }],
            "issues": [],
        })
        .to_string()
    }

    #[test]
    fn the_rights_unlock_what_roots_scan_offers_and_root_cleans_only_what_is_checked() {
        let sandbox = Sandbox::new();
        let pip = sandbox.file("home/.cache/pip/old");
        let crash = sandbox.file("var/crash/core");
        sandbox.file("var/dumps/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let crash_name = sandbox.root.join("var/crash").display().to_string();
        let answer = {
            let scan = scan_answer(&sandbox);
            let crash = crash.display().to_string();
            move |operation, _: &serde_json::Value| {
                match operation {
                okapi_helper::Operation::Scan => (0, scan.clone()),
                okapi_helper::Operation::Clean => (
                    0,
                    serde_json::json!({
                        "version": okapi_helper::PROTOCOL_VERSION,
                        "operation": "clean",
                        "stops": [],
                        "groups": [],
                        "issues": [],
                        "outcomes": [{ "outcome": { "path": crash, "result": "removed", "bytes": 7 }, "scannedBytes": 7 }],
                    })
                    .to_string(),
                ),
            }
            }
        };
        let seams = admin_seams(&sandbox, &rules, &asked, answer);
        let (window, _screen) = on_admin_results(seams, &rules);
        let cleaner = window.global::<ui::Cleaner>();
        assert_eq!(cleaner.get_rights_waiting(), 2);
        assert_eq!(
            found(&window, "Rapports de plantage").accessible_enabled(),
            Some(false)
        );

        obtain_rights(&window);
        let (operation, request) = asked.lock().unwrap()[0].clone();
        assert_eq!(operation, okapi_helper::Operation::Scan);
        assert_eq!(
            request["minAgeMinutes"], 60,
            "never below the helper's floor"
        );
        assert_eq!(cleaner.get_rights_waiting(), 0);
        let unlocked = found(&window, "Rapports de plantage");
        assert_eq!(unlocked.accessible_enabled(), Some(true));
        assert_eq!(
            unlocked.accessible_checked(),
            Some(false),
            "never checked for the user (invariant 5)"
        );
        assert_eq!(
            found(&window, "Vidages mémoire").accessible_enabled(),
            Some(false),
            "nothing for root to clean"
        );

        unlocked.invoke_accessible_default_action();
        assert!(
            shows(&window, "Éléments : 4 sur 4"),
            "root's numbers counted"
        );
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        let (operation, request) = asked.lock().unwrap()[1].clone();
        assert_eq!(operation, okapi_helper::Operation::Clean);
        assert_eq!(
            request["targets"],
            serde_json::json!([crash_name]),
            "the targets checked, no more"
        );
        assert_eq!(request["minAgeMinutes"], 60, "as root's scan was asked");
        assert!(!pip.exists(), "the user's part removed");
        assert!(
            crash.exists(),
            "root's part is root's: never the user's own cleaning"
        );
        assert!(shows(&window, "Nettoyage terminé"));
        let history = sandbox.history();
        assert_eq!(history.len(), 1, "one line for both parts");
        assert_eq!(history[0]["removed"], 2);
        assert_eq!(history[0]["admin"], serde_json::json!({ "done": true }));
        assert_eq!(
            steps_shown(&window),
            [
                (ui::CleanStepKind::Admin, ui::RowState::Done),
                (ui::CleanStepKind::Check, ui::RowState::Done),
                (ui::CleanStepKind::Remove, ui::RowState::Done),
                (ui::CleanStepKind::History, ui::RowState::Done),
            ],
            "root's part first"
        );
        assert_eq!(
            sandbox.deletion_logs()[0][0]["path"],
            crash.display().to_string(),
            "root's first"
        );
    }

    /// A directory closed to the user running the tests, as
    /// `/var/lib/snapd/cache` is; opened again when dropped, a test failing
    /// or not, for the sandbox to be removed.
    struct Closed(PathBuf);

    impl Closed {
        /// `directory` closed; none when the tests run as root, whom
        /// nothing closes it to.
        fn new(directory: &Path) -> Option<Self> {
            use std::os::unix::fs::PermissionsExt as _;
            if okapi_platform::running_as_root() {
                return None;
            }
            fs::set_permissions(directory, fs::Permissions::from_mode(0o000)).unwrap();
            Some(Self(directory.to_path_buf()))
        }
    }

    impl Drop for Closed {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;
            // A failure here leaves the sandbox behind, which its removal
            // says; a panic while a test panics would abort them all.
            fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755)).ok();
        }
    }

    /// A target whose root the user cannot read waits for the rights as a
    /// group of its own, what it holds unknown; root's scan sets it, and
    /// root's cleaning is asked for its target alone, never the user's.
    #[test]
    fn a_target_only_root_may_read_waits_for_the_rights() {
        let sandbox = Sandbox::new();
        let pip = sandbox.file("home/.cache/pip/old");
        sandbox.file("var/crash/core");
        let dumps = sandbox.root.join("var/dumps");
        sandbox.file("var/dumps/core");
        settle_files();
        let Some(_closed) = Closed::new(&dumps) else {
            return;
        };
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let dumps_name = dumps.display().to_string();
        let answer = {
            let scan = serde_json::json!({
                "version": okapi_helper::PROTOCOL_VERSION,
                "operation": "scan",
                "stops": [],
                "groups": [{ "targets": [dumps_name], "entries": 2, "bytes": 5_000 }],
                "issues": [],
            })
            .to_string();
            let clean = serde_json::json!({
                "version": okapi_helper::PROTOCOL_VERSION,
                "operation": "clean",
                "stops": [],
                "groups": [],
                "issues": [],
                "outcomes": [{ "outcome": { "path": dumps.join("core"), "result": "removed", "bytes": 5_000 }, "scannedBytes": 5_000 }],
            })
            .to_string();
            move |operation, _: &serde_json::Value| match operation {
                okapi_helper::Operation::Scan => (0, scan.clone()),
                okapi_helper::Operation::Clean => (0, clean.clone()),
            }
        };
        let seams = admin_seams(&sandbox, &rules, &asked, answer);
        let (window, _screen) = on_admin_results(seams, &rules);
        let cleaner = window.global::<ui::Cleaner>();
        assert_eq!(cleaner.get_rights_waiting(), 2, "the crash reports and it");
        assert_eq!(
            found(&window, "Vidages mémoire").accessible_enabled(),
            Some(false)
        );
        assert!(shows(
            &window,
            "Illisible sans les droits administrateur : « Obtenir les droits » ci-dessus."
        ));
        // The notes are folded: their model says what they would show.
        let notes = cleaner.get_notes();
        assert!(
            (0..slint::Model::row_count(&notes)).all(|row| {
                slint::Model::row_data(&notes, row)
                    .is_none_or(|note| note.path != dumps.display().to_string())
            }),
            "its group says it, not a note"
        );

        obtain_rights(&window);
        let (_, request) = asked.lock().unwrap()[0].clone();
        assert!(
            request["targets"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(dumps_name)),
            "root is asked for it"
        );
        let unlocked = found(&window, "Vidages mémoire");
        assert_eq!(unlocked.accessible_enabled(), Some(true));
        assert_eq!(
            unlocked.accessible_checked(),
            Some(false),
            "never checked for the user (invariant 5)"
        );
        assert!(
            shows(&window, "Trouvé : 5,0 ko. Éléments : 3."),
            "root's numbers counted with the user's file, the crash reports \
             nothing to clean for root"
        );
        unlocked.invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        let (operation, request) = asked.lock().unwrap()[1].clone();
        assert_eq!(operation, okapi_helper::Operation::Clean);
        assert_eq!(
            request["targets"],
            serde_json::json!([dumps_name]),
            "its target alone"
        );
        assert!(!pip.exists(), "the user's part removed");
        let history = sandbox.history();
        assert_eq!(
            (&history[0]["removed"], &history[0]["kept"]),
            (&serde_json::json!(2), &serde_json::json!(0)),
            "the user's file and root's answer, merged in one line"
        );
    }

    /// The description of the line of the results labelled `label`.
    fn description_of(window: &ui::AppWindow, label: &str) -> slint::SharedString {
        let results = window.global::<ui::Cleaner>().get_results();
        (0..slint::Model::row_count(&results))
            .filter_map(|row| slint::Model::row_data(&results, row))
            .find(|row| row.label == label)
            .unwrap()
            .description
    }

    /// [`admin_rules`], and the journal's vacuum at `journal`, described as
    /// the embedded rule.
    fn journal_rules(sandbox: &Sandbox, journal: &Path) -> RuleSet {
        let embedded = RuleSet::embedded(Os::Linux).unwrap();
        let description = embedded
            .system()
            .clean_targets()
            .iter()
            .find(|target| target.subcategory().as_str() == "Journal Logs")
            .and_then(|target| target.description())
            .unwrap()
            .to_string();
        admin_rules_with(
            sandbox,
            &[serde_json::json!({
                "path": journal,
                "subcategory": "Journal Logs",
                "needsAdmin": true,
                "cleanupAction": "journal-vacuum",
                "description": description,
            })],
        )
    }

    /// A journal behind a link, which root never follows: no line waits for
    /// rights that would change nothing.
    #[test]
    fn a_journal_root_would_refuse_is_no_line() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        let real = sandbox.root.join("srv/journal");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(sandbox.root.join("var")).unwrap();
        std::os::unix::fs::symlink(sandbox.root.join("srv"), sandbox.root.join("var/log")).unwrap();
        settle_files();
        let rules = journal_rules(&sandbox, &sandbox.root.join("var/log/journal"));
        let seams = admin_seams(
            &sandbox,
            &rules,
            &Asked::default(),
            |_, _: &serde_json::Value| panic!("no rights asked for"),
        );
        let (window, _screen) = on_admin_results(seams, &rules);
        assert!(!shows(&window, "Journaux de systemd"));
    }

    /// The journal's vacuum: a line locked until root's scan measures it,
    /// never checked for the user; checked, only its path goes to the helper
    /// beside the user's own cleaning (invariants 1 and 5), and the receipt
    /// and the history count what root removed.
    #[test]
    fn the_journal_is_measured_and_vacuumed_by_root_alone() {
        let sandbox = Sandbox::new();
        let pip = sandbox.file("home/.cache/pip/old");
        let journal = sandbox.root.join("var/log/journal");
        fs::create_dir_all(&journal).unwrap();
        settle_files();
        let rules = journal_rules(&sandbox, &journal);
        let asked = Asked::default();
        let journal_name = journal.display().to_string();
        let archive = journal.join("machine/system@old.journal");
        let answer = {
            let scan = serde_json::json!({
                "version": okapi_helper::PROTOCOL_VERSION,
                "operation": "scan",
                "stops": [],
                "groups": [{ "targets": [journal_name], "entries": 3, "bytes": 7_000 }],
                "issues": [],
            })
            .to_string();
            let clean = serde_json::json!({
                "version": okapi_helper::PROTOCOL_VERSION,
                "operation": "clean",
                "stops": [],
                "groups": [],
                "issues": [],
                "outcomes": [{ "outcome": { "path": archive, "result": "removed", "bytes": 7_000 }, "scannedBytes": 7_000 }],
            })
            .to_string();
            move |operation, _: &serde_json::Value| match operation {
                okapi_helper::Operation::Scan => (0, scan.clone()),
                okapi_helper::Operation::Clean => (0, clean.clone()),
            }
        };
        let seams = admin_seams(&sandbox, &rules, &asked, answer);
        let (window, _screen) = on_admin_results(seams, &rules);
        let line = found(&window, "Journaux de systemd");
        assert_eq!(line.accessible_enabled(), Some(false), "locked");
        // Docker's and uv's places (the home folder) are there: neither is a
        // maintenance root runs, so no line offers them.
        for other in ["Cache de construction Docker", "Paquets uv inutilisés"] {
            assert!(!shows(&window, other), "{other}");
        }
        let description = description_of(&window, "Journaux de systemd");
        assert!(
            description.contains("plus de 30 jours")
                && description.contains("plus de 1 Go")
                && !description.contains('{'),
            "the helper's limits shown: {description}"
        );
        assert!(shows(
            &window,
            "Se mesure avec les droits administrateur : « Obtenir les droits » ci-dessus."
        ));

        obtain_rights(&window);
        let (_, request) = asked.lock().unwrap()[0].clone();
        assert!(
            request["targets"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(journal_name)),
            "root is asked to measure it"
        );
        let unlocked = found(&window, "Journaux de systemd");
        assert_eq!(unlocked.accessible_enabled(), Some(true));
        assert_eq!(
            unlocked.accessible_checked(),
            Some(false),
            "never checked for the user (invariant 5)"
        );
        assert!(shows(&window, "Opération du système : à cocher vous-même."));
        unlocked.invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        let (operation, request) = asked.lock().unwrap()[1].clone();
        assert_eq!(operation, okapi_helper::Operation::Clean);
        assert_eq!(
            request["targets"],
            serde_json::json!([journal_name]),
            "its path alone, never a file to remove"
        );
        assert!(!pip.exists(), "the user's part removed");
        let history = sandbox.history();
        assert_eq!(
            (&history[0]["removed"], &history[0]["kept"]),
            (&serde_json::json!(2), &serde_json::json!(0)),
            "the user's file and root's archive, in one line"
        );
    }

    /// What only root may read is found too, though the user's own scan
    /// found nothing; never for a category the user left out, whose rules
    /// are planned all the same: an ancestor the user cannot cross is noted
    /// before any root is chosen.
    #[test]
    fn a_target_only_root_may_read_is_found_only_in_a_category_chosen() {
        let sandbox = Sandbox::new();
        let var = sandbox.root.join("var");
        sandbox.file("var/dumps/core");
        settle_files();
        let Some(_closed) = Closed::new(&var) else {
            return;
        };
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let seams = admin_seams(&sandbox, &rules, &asked, |_, _: &serde_json::Value| {
            panic!("no rights asked for")
        });
        let (window, screen) = on_cleaner(seams);
        screen.use_helper_targets_of(&rules);
        let ended = |window: &ui::AppWindow| {
            matches!(
                step(window),
                ui::CleanerStep::Results | ui::CleanerStep::Outcome
            )
        };
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, ended);
        assert_eq!(step(&window), ui::CleanerStep::Results);
        assert_eq!(
            window.global::<ui::Cleaner>().get_rights_waiting(),
            2,
            "the crash reports and the core dumps wait for the rights"
        );
        let_the_screen_settle();
        button(&window, "Choisir d'autres catégories").invoke_accessible_default_action();
        let_the_screen_settle();
        checkbox(&window, "Système").invoke_accessible_default_action();
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, ended);
        assert_eq!(
            window.global::<ui::Cleaner>().get_outcome(),
            ui::OutcomeKind::NothingToClean,
            "the system left out: nothing to clean"
        );
        assert!(asked.lock().unwrap().is_empty());
    }

    /// A permission the system refused is never forced (D-19): its line
    /// says so; the system's other refusals keep theirs, without it.
    #[test]
    fn the_receipt_says_a_refused_permission_is_never_forced() {
        use std::io::ErrorKind;

        let (window, _screen) = on_cleaner(seams_never_scanning());
        let cleaner = window.global::<ui::Cleaner>();
        let model = Rc::new(slint::VecModel::default());
        cleaner.set_receipt_lines(slint::ModelRc::from(Rc::clone(&model)));
        cleaner.set_step(ui::CleanerStep::Receipt);
        let refused = |kind| crate::receipt::OutcomeFacts {
            result: okapi_engine::CleanResult::Kept(okapi_engine::KeepReason::Failed {
                kind,
                code: None,
            }),
            path: Some(PathBuf::from("/tmp/kept")),
            blocked: None,
            scanned_bytes: Some(1),
        };
        let note = "Elles appartiennent à un autre compte ou au système : Okapi ne les force pas.";
        let show = |kinds: &[ErrorKind]| {
            let outcomes: Vec<_> = kinds.iter().map(|kind| refused(*kind)).collect();
            super::show_receipt(&cleaner, &model, &super::Receipt::new(false, &outcomes));
            slint::platform::update_timers_and_animations();
        };
        let notes = || ElementHandle::find_by_accessible_label(&window, note).count();
        show(&[ErrorKind::PermissionDenied]);
        assert!(shows_a_text_starting(
            &window,
            "Gardées : permission refusée par le système"
        ));
        assert_eq!(notes(), 1);
        for other in [
            ErrorKind::ResourceBusy,
            ErrorKind::NotFound,
            ErrorKind::Other,
        ] {
            show(&[other]);
            assert!(shows_a_text_starting(&window, "Gardées : refus du système"));
            assert_eq!(notes(), 0, "{other:?}: said of a refused permission alone");
        }
        show(&[
            ErrorKind::ResourceBusy,
            ErrorKind::PermissionDenied,
            ErrorKind::PermissionDenied,
        ]);
        assert!(shows_a_text_starting(&window, "Gardées : refus du système"));
        assert!(shows_a_text_starting(
            &window,
            "Gardées : permission refusée par le système"
        ));
        assert_eq!(notes(), 1, "once, under its own line");
    }

    #[test]
    fn a_dismissed_question_cancels_the_whole_cleaning() {
        let sandbox = Sandbox::new();
        let pip = sandbox.file("home/.cache/pip/old");
        sandbox.file("var/crash/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let answer = {
            let scan = scan_answer(&sandbox);
            move |operation, _: &serde_json::Value| match operation {
                okapi_helper::Operation::Scan => (0, scan.clone()),
                okapi_helper::Operation::Clean => (okapi_helper::PKEXEC_DISMISSED, String::new()),
            }
        };
        let seams = admin_seams(&sandbox, &rules, &asked, answer);
        let (window, _screen) = on_admin_results(seams, &rules);
        obtain_rights(&window);
        found(&window, "Rapports de plantage").invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(
            pip.exists(),
            "a no to the question: nothing of the user's either"
        );
        assert!(shows(&window, "Nettoyage annulé"));
        assert!(shows_a_text_starting(
            &window,
            "La partie administrateur ne s'est pas terminée"
        ));
        let history = sandbox.history();
        assert_eq!(
            history[0]["admin"],
            serde_json::json!({ "done": false, "reason": "authDismissed" })
        );
        assert_eq!(history[0]["complete"], false);
        assert_eq!(
            steps_shown(&window),
            [
                (ui::CleanStepKind::Admin, ui::RowState::NotDone),
                (ui::CleanStepKind::Check, ui::RowState::NotDone),
                (ui::CleanStepKind::Remove, ui::RowState::NotDone),
                (ui::CleanStepKind::History, ui::RowState::Done),
            ],
            "a no to the question: nothing done but the history line"
        );
        let counted: Vec<_> = {
            use slint::Model as _;
            window
                .global::<ui::Cleaner>()
                .get_clean_steps()
                .iter()
                .map(|step| step.done)
                .collect()
        };
        assert_eq!(
            counted,
            [0, 0, 0, 0],
            "the entries ran through as cancelled: none counted"
        );
    }

    #[test]
    fn a_failed_question_leaves_the_rights_locked_and_says_why() {
        let sandbox = Sandbox::new();
        let pip = sandbox.file("home/.cache/pip/old");
        sandbox.file("var/crash/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let seams = admin_seams(&sandbox, &rules, &asked, |_, _| {
            (okapi_helper::PKEXEC_DISMISSED, String::new())
        });
        let (window, _screen) = on_admin_results(seams, &rules);
        obtain_rights(&window);
        let cleaner = window.global::<ui::Cleaner>();
        assert_eq!(cleaner.get_rights_problem(), ui::RightsProblem::Dismissed);
        assert_eq!(cleaner.get_rights_waiting(), 1, "nothing unlocked");
        assert_eq!(
            found(&window, "Rapports de plantage").accessible_enabled(),
            Some(false)
        );
        // A cleaning of the user's alone never asks the helper.
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!pip.exists());
        assert_eq!(asked.lock().unwrap().len(), 1, "the scan alone");
        assert_eq!(sandbox.history()[0].get("admin"), None, "no root's part");
    }

    /// An answer of root's cleaning, as the helper writes it.
    fn clean_answer(stops: &[&str], outcomes: &serde_json::Value) -> String {
        serde_json::json!({
            "version": okapi_helper::PROTOCOL_VERSION,
            "operation": "clean",
            "stops": stops,
            "groups": [],
            "issues": [],
            "outcomes": outcomes,
        })
        .to_string()
    }

    #[test]
    fn root_cleans_the_one_unlocked_group_checked_with_the_settings_of_its_scan() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        sandbox.file("var/crash/core");
        let dumps = sandbox.file("var/dumps/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let scan = serde_json::json!({
            "version": okapi_helper::PROTOCOL_VERSION,
            "operation": "scan",
            "stops": [],
            "groups": [
                { "targets": [sandbox.root.join("var/crash")], "entries": 1, "bytes": 7 },
                { "targets": [sandbox.root.join("var/dumps")], "entries": 1, "bytes": 7 },
            ],
            "issues": [],
        })
        .to_string();
        let answer = move |operation, _: &serde_json::Value| match operation {
            okapi_helper::Operation::Scan => (0, scan.clone()),
            okapi_helper::Operation::Clean => (0, clean_answer(&[], &serde_json::json!([]))),
        };
        let keep = sandbox.root.join("var/crash/keep");
        let seams = {
            let mut inputs = sandbox.inputs(Some(&sandbox.root.join("state")));
            inputs.min_age_minutes = Some(0);
            inputs
                .exclusions
                .extend([keep.clone(), "*.tmp".into(), "/srv/elsewhere".into()]);
            Seams {
                read: Rc::new(move || Ok(inputs.clone())),
                ..admin_seams(&sandbox, &rules, &asked, answer)
            }
        };
        let (window, _screen) = on_admin_results(seams, &rules);
        obtain_rights(&window);
        found(&window, "Rapports de plantage").invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        let asked = asked.lock().unwrap();
        let (scan, clean) = (&asked[0].1, &asked[1].1);
        assert_eq!(
            clean["targets"],
            serde_json::json!([sandbox.root.join("var/crash")]),
            "the group checked, never the other unlocked one"
        );
        assert_eq!(
            clean["minAgeMinutes"], 60,
            "the age of the scan that unlocked it"
        );
        assert_eq!(
            clean["exclusions"], scan["exclusions"],
            "its exclusions too"
        );
        assert_eq!(
            clean["exclusions"],
            serde_json::json!([keep, "*.tmp"]),
            "below a target or an extension; never one elsewhere"
        );
        assert!(dumps.exists());
    }

    #[test]
    fn settings_changed_since_the_rights_remove_nothing_and_lock_root_again() {
        let sandbox = Sandbox::new();
        let pip = sandbox.file("home/.cache/pip/old");
        let crash = sandbox.file("var/crash/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let answer = {
            let scan = scan_answer(&sandbox);
            move |operation, _: &serde_json::Value| match operation {
                okapi_helper::Operation::Scan => (0, scan.clone()),
                okapi_helper::Operation::Clean => (0, clean_answer(&[], &serde_json::json!([]))),
            }
        };
        let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seams = {
            let first = sandbox.inputs(Some(&sandbox.root.join("state")));
            let mut second = first.clone();
            second.min_age_minutes = Some(500);
            let changed = Arc::clone(&changed);
            Seams {
                read: Rc::new(move || {
                    Ok(if changed.load(std::sync::atomic::Ordering::Relaxed) {
                        second.clone()
                    } else {
                        first.clone()
                    })
                }),
                ..admin_seams(&sandbox, &rules, &asked, answer)
            }
        };
        let (window, _screen) = on_admin_results(seams, &rules);
        obtain_rights(&window);
        found(&window, "Rapports de plantage").invoke_accessible_default_action();
        changed.store(true, std::sync::atomic::Ordering::Relaxed);
        confirm(&window);
        let cleaner = window.global::<ui::Cleaner>();
        // Root's exclusions are read on the cleaning's thread.
        assert_eq!(step(&window), ui::CleanerStep::Cleaning);
        assert_eq!(cleaner.get_problem(), ui::SetupProblem::None);
        // A problem of the settings said meanwhile never hides it: root is
        // locked again.
        cleaner.set_problem(ui::SetupProblem::SettingsSaving);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        assert_eq!(cleaner.get_problem(), ui::SetupProblem::RightsChanged);
        assert_eq!(asked.lock().unwrap().len(), 1, "root never asked to clean");
        assert!(pip.exists(), "nothing of the user's either");
        assert!(crash.exists());
        assert_eq!(cleaner.get_rights_waiting(), 1, "root's group locked again");
        assert_eq!(
            found(&window, "Rapports de plantage").accessible_enabled(),
            Some(false)
        );
        assert!(sandbox.history().is_empty(), "nothing done, nothing kept");
    }

    #[test]
    fn an_exclusion_refused_is_said_before_the_rights_changed() {
        let sandbox = Sandbox::new();
        let pip = sandbox.file("home/.cache/pip/old");
        let crash = sandbox.file("var/crash/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let answer = {
            let scan = scan_answer(&sandbox);
            move |operation, _: &serde_json::Value| match operation {
                okapi_helper::Operation::Scan => (0, scan.clone()),
                okapi_helper::Operation::Clean => (0, clean_answer(&[], &serde_json::json!([]))),
            }
        };
        let refused = Rc::new(Cell::new(false));
        let seams = {
            let fine = sandbox.inputs(Some(&sandbox.root.join("state")));
            // Refused by the engine, and one root would be sent: an
            // extension may touch every target.
            let mut wild = fine.clone();
            wild.exclusions.push(PathBuf::from("*.t*"));
            let refused = Rc::clone(&refused);
            Seams {
                read: Rc::new(move || {
                    Ok(if refused.get() {
                        wild.clone()
                    } else {
                        fine.clone()
                    })
                }),
                ..admin_seams(&sandbox, &rules, &asked, answer)
            }
        };
        let (window, _screen) = on_admin_results(seams, &rules);
        obtain_rights(&window);
        found(&window, "Rapports de plantage").invoke_accessible_default_action();
        refused.set(true);
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        let cleaner = window.global::<ui::Cleaner>();
        assert_eq!(cleaner.get_problem(), ui::SetupProblem::Exclusion);
        assert_eq!(asked.lock().unwrap().len(), 1, "root never asked to clean");
        assert!(pip.exists() && crash.exists());
        assert_eq!(
            cleaner.get_rights_waiting(),
            0,
            "the rights kept: nothing changed but an exclusion refused"
        );
        assert!(sandbox.history().is_empty(), "nothing done, nothing kept");
        refused.set(false);
        let_the_screen_settle();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[1].0, okapi_helper::Operation::Clean);
        assert!(!pip.exists());
    }

    /// An exclusion that cannot be located protects something unknown: root
    /// is asked for nothing, neither its scan nor its cleaning.
    #[test]
    fn an_exclusion_that_cannot_be_located_asks_root_for_nothing() {
        let sandbox = Sandbox::new();
        sandbox.file("var/crash/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        let dangling = sandbox.root.join("dangling");
        std::os::unix::fs::symlink(sandbox.root.join("nowhere"), &dangling).unwrap();
        let read = Rc::new(std::cell::RefCell::new(
            sandbox.inputs(Some(&sandbox.root.join("state"))),
        ));
        let asked = Asked::default();
        let seams = {
            let read = Rc::clone(&read);
            Seams {
                read: Rc::new(move || Ok(read.borrow().clone())),
                ..admin_seams(&sandbox, &rules, &asked, |_, _| (0, String::new()))
            }
        };
        let (window, _screen) = on_admin_results(seams, &rules);
        // Lost since the user's own scan, which would have refused it.
        read.borrow_mut().exclusions.push(dangling.join("keep"));
        let inputs = read.borrow().clone();
        obtain_rights(&window);
        let cleaner = window.global::<ui::Cleaner>();
        assert_eq!(cleaner.get_rights_problem(), ui::RightsProblem::Failed);
        assert!(asked.lock().unwrap().is_empty(), "root's scan never asked");
        let crash = std::collections::BTreeSet::from([sandbox
            .root
            .join("var/crash")
            .display()
            .to_string()]);
        let part = super::RootPart {
            granted: crate::cleaner::rights::Granted {
                minutes: 60,
                exclusions: Vec::new(),
            },
            targets: crash.clone(),
            confirmed: crash,
        };
        assert_eq!(
            part.line(&inputs),
            Err(crate::cleaner::Problem::Exclusion),
            "nor its cleaning"
        );
    }

    #[test]
    fn a_request_root_would_refuse_is_never_sent() {
        use std::os::unix::ffi::OsStrExt as _;

        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        sandbox.file("var/crash/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let seams = {
            let mut inputs = sandbox.inputs(Some(&sandbox.root.join("state")));
            // Below a target, so sent; not text, so not written.
            inputs.exclusions.push(
                sandbox
                    .root
                    .join("var/crash")
                    .join(std::ffi::OsStr::from_bytes(b"keep\xff")),
            );
            Seams {
                read: Rc::new(move || Ok(inputs.clone())),
                // Sent, it would be noted, whatever it answers.
                ..admin_seams(&sandbox, &rules, &asked, |_, _| (0, String::new()))
            }
        };
        let (window, _screen) = on_admin_results(seams, &rules);
        obtain_rights(&window);
        let cleaner = window.global::<ui::Cleaner>();
        assert_eq!(cleaner.get_rights_problem(), ui::RightsProblem::Failed);
        assert!(asked.lock().unwrap().is_empty());
        assert_eq!(cleaner.get_rights_waiting(), 1, "nothing unlocked");
    }

    #[test]
    fn rights_cancelled_before_the_helper_is_launched_are_given_up() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        sandbox.file("var/crash/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        // Held before anything is launched, as reading where the
        // exclusions lie on a mount that stopped answering would.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (told, cancelled) = std::sync::mpsc::channel::<bool>();
        let held = std::sync::Mutex::new((held, told));
        let seams = Seams {
            elevate: Arc::new(move |_, _, cancel, _| {
                let held = held.lock().unwrap();
                let _ = held.0.recv();
                // What `Launcher::run` looks at before it launches anything.
                held.1.send(cancel.is_cancelled()).unwrap();
                Err(crate::elevation::ElevationFailure::Cancelled)
            }),
            ..admin_seams(&sandbox, &rules, &Asked::default(), |_, _| {
                unreachable!("never launched")
            })
        };
        let (window, _screen) = on_admin_results(seams, &rules);
        let cleaner = window.global::<ui::Cleaner>();
        cleaner.invoke_obtain_rights();
        assert_eq!(cleaner.get_rights_step(), ui::RightsStep::Preparing);
        assert!(shows(&window, "Préparation de la demande des droits…"));
        let_the_screen_settle();
        button(&window, "Annuler").invoke_accessible_default_action();
        assert_eq!(cleaner.get_rights_step(), ui::RightsStep::Withdrawing);
        assert!(shows(&window, "Annulation de la demande des droits…"));
        for _ in 1..looks_until_given_up() {
            look_once();
        }
        assert_eq!(
            cleaner.get_rights_step(),
            ui::RightsStep::Withdrawing,
            "waited for until then"
        );
        look_once();
        assert_eq!(cleaner.get_rights_step(), ui::RightsStep::Idle);
        assert_eq!(cleaner.get_rights_problem(), ui::RightsProblem::Cancelled);
        assert_eq!(cleaner.get_rights_waiting(), 1, "nothing unlocked");
        // The screen free again: a cleaning of the user's starts.
        let_the_screen_settle();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        drop(release);
        assert!(
            cancelled
                .recv_timeout(Duration::from_secs(10))
                .expect("the helper asked"),
            "the thread given up is cancelled: it launches nothing"
        );
    }

    /// The looks at a task until rights cancelled before the helper was
    /// launched are given up.
    fn looks_until_given_up() -> u128 {
        super::super::CANCEL_WAIT.as_millis() / super::super::WATCH_EVERY.as_millis()
    }

    /// One look at the task under way.
    fn look_once() {
        i_slint_backend_testing::mock_elapsed_time(super::super::WATCH_EVERY);
        slint::platform::update_timers_and_animations();
    }

    #[test]
    fn rights_launched_after_they_are_cancelled_wait_for_the_question() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        sandbox.file("var/crash/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        // Launched once told to (a launch already under way as the user
        // cancels), then answering once `end` goes.
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let (end, ended) = std::sync::mpsc::channel::<()>();
        let (said, launched) = std::sync::mpsc::channel::<()>();
        let gates = std::sync::Mutex::new((gate, ended, said));
        let seams = Seams {
            elevate: Arc::new(move |_, _, _, steps| {
                let gates = gates.lock().unwrap();
                let _ = gates.0.recv();
                steps
                    .send(crate::elevation::ElevationStep::Launched)
                    .unwrap();
                gates.2.send(()).unwrap();
                let _ = gates.1.recv();
                Err(crate::elevation::ElevationFailure::Cancelled)
            }),
            ..admin_seams(&sandbox, &rules, &Asked::default(), |_, _| {
                unreachable!("never answered")
            })
        };
        let (window, _screen) = on_admin_results(seams, &rules);
        let cleaner = window.global::<ui::Cleaner>();
        cleaner.invoke_obtain_rights();
        let_the_screen_settle();
        button(&window, "Annuler").invoke_accessible_default_action();
        assert_eq!(cleaner.get_rights_step(), ui::RightsStep::Withdrawing);
        go.send(()).unwrap();
        // Launched before the next look, however slow the machine.
        launched
            .recv_timeout(Duration::from_secs(10))
            .expect("the helper launched");
        watch_until(&window, |window| {
            window.global::<ui::Cleaner>().get_rights_step() == ui::RightsStep::Cancelling
        });
        assert!(shows(
            &window,
            "Annulation de la demande des droits : elle prend effet une fois la fenêtre du système fermée."
        ));
        for _ in 0..looks_until_given_up() * 2 {
            look_once();
        }
        assert_eq!(
            cleaner.get_rights_step(),
            ui::RightsStep::Cancelling,
            "launched: never given up, pkexec asks"
        );
        drop(end);
        watch_until(&window, |window| {
            window.global::<ui::Cleaner>().get_rights_step() == ui::RightsStep::Idle
        });
        assert_eq!(cleaner.get_rights_problem(), ui::RightsProblem::Cancelled);
    }

    /// The window on results whose settings refuse an exclusion while
    /// `refused` says so.
    fn on_results_refused_when(
        sandbox: &Sandbox,
        refused: &Rc<Cell<bool>>,
    ) -> (ui::AppWindow, super::super::CleanerScreen) {
        let state = sandbox.root.join("state");
        on_results(Seams {
            read: refused_when(sandbox.inputs(Some(&state)), refused),
            ..sandbox.read(&state)
        })
    }

    #[test]
    fn an_exclusion_refused_at_the_cleaning_removes_nothing() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let refused = Rc::new(Cell::new(false));
        let (window, _screen) = on_results_refused_when(&sandbox, &refused);
        refused.set(true);
        confirm(&window);
        let cleaner = window.global::<ui::Cleaner>();
        // Nothing resolved on the window's thread: the cleaning starts, and
        // its own thread meets the refusal.
        assert_eq!(step(&window), ui::CleanerStep::Cleaning);
        assert_eq!(cleaner.get_problem(), ui::SetupProblem::None);
        watch_until(&window, refused_for_an_exclusion);
        assert_eq!(step(&window), ui::CleanerStep::Results);
        assert!(shows(&window, EXCLUSION_REFUSED));
        assert!(old.exists());
        assert!(sandbox.history().is_empty(), "nothing done, nothing kept");
        // The results held still clean, once the settings are corrected.
        refused.set(false);
        let_the_screen_settle();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!old.exists());
        assert_eq!(sandbox.history().len(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn an_exclusion_that_resolves_again_is_no_more_said_once_a_cleaning_starts() {
        use std::os::unix::fs::PermissionsExt as _;
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let locked = sandbox.root.join("locked");
        fs::create_dir(&locked).unwrap();
        let state = sandbox.root.join("state");
        let mut inputs = sandbox.inputs(Some(&state));
        inputs.exclusions.push(locked.join("keep"));
        let (window, _screen) = on_results(Seams {
            read: Rc::new(move || Ok(inputs.clone())),
            ..sandbox.read(&state)
        });
        // Unresolvable, as a mount that stopped answering would leave it.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        confirm(&window);
        watch_until(&window, refused_for_an_exclusion);
        // Readable again with the same settings: the cleaning started leaves
        // the refusal behind.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let_the_screen_settle();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!old.exists());
        let_the_screen_settle();
        button(&window, "Terminer").invoke_accessible_default_action();
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::None
        );
    }

    #[test]
    fn a_minimum_age_beyond_the_clock_is_refused_at_the_confirmation() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let state = sandbox.root.join("state");
        let minutes = Rc::new(Cell::new(Some(0)));
        let read_minutes = Rc::clone(&minutes);
        let inputs = sandbox.inputs(Some(&state));
        let (window, _screen) = on_results(Seams {
            read: Rc::new(move || {
                Ok(Inputs {
                    min_age_minutes: read_minutes.get(),
                    ..inputs.clone()
                })
            }),
            ..sandbox.read(&state)
        });
        minutes.set(Some(u64::MAX));
        confirm(&window);
        // Said at the click: no cleaning started.
        assert_eq!(step(&window), ui::CleanerStep::Results);
        assert_eq!(
            window.global::<ui::Cleaner>().get_problem(),
            ui::SetupProblem::MinAge
        );
        assert!(old.exists());
    }

    #[test]
    fn closing_asked_during_a_refused_cleaning_closes() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let refused = Rc::new(Cell::new(false));
        let (window, screen) = on_results_refused_when(&sandbox, &refused);
        refused.set(true);
        confirm(&window);
        window.window().show().unwrap();
        assert_eq!(
            screen.close_requested(),
            slint::CloseRequestResponse::KeepWindowShown
        );
        watch_until(&window, refused_for_an_exclusion);
        assert!(!window.window().is_visible(), "nothing to read: it closes");
        assert!(old.exists());
    }

    #[test]
    fn a_refusal_after_an_analysis_that_found_nothing_goes_back_to_choosing() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let refused = Rc::new(Cell::new(false));
        let (window, _screen) = on_results_refused_when(&sandbox, &refused);
        // The analysis after finds nothing: the results before are no more
        // the last ones, and nothing may clean them.
        std::fs::remove_file(&old).unwrap();
        button(&window, "Nouvelle analyse").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        let_the_screen_settle();
        refused.set(true);
        button(&window, "Nouvelle analyse").invoke_accessible_default_action();
        watch_until(&window, refused_for_an_exclusion);
        assert_eq!(step(&window), ui::CleanerStep::Idle);
    }

    /// Root's scan offers the crash reports, the window holds them unlocked
    /// and checked; the settings refuse an exclusion while `refused` says so.
    fn on_unlocked_results_refused_when(
        sandbox: &Sandbox,
        asked: &Asked,
        refused: &Rc<Cell<bool>>,
    ) -> ui::AppWindow {
        let rules = admin_rules(sandbox);
        let answer = {
            let scan = scan_answer(sandbox);
            move |operation, _: &serde_json::Value| match operation {
                okapi_helper::Operation::Scan => (0, scan.clone()),
                okapi_helper::Operation::Clean => (0, clean_answer(&[], &serde_json::json!([]))),
            }
        };
        let seams = Seams {
            read: refused_when(sandbox.inputs(Some(&sandbox.root.join("state"))), refused),
            ..admin_seams(sandbox, &rules, asked, answer)
        };
        let (window, _screen) = on_admin_results(seams, &rules);
        obtain_rights(&window);
        found(&window, "Rapports de plantage").invoke_accessible_default_action();
        window
    }

    #[test]
    fn an_exclusion_refused_never_asks_root_to_clean() {
        let sandbox = Sandbox::new();
        let pip = sandbox.file("home/.cache/pip/old");
        let crash = sandbox.file("var/crash/core");
        settle_files();
        let asked = Asked::default();
        let refused = Rc::new(Cell::new(false));
        let window = on_unlocked_results_refused_when(&sandbox, &asked, &refused);
        refused.set(true);
        confirm(&window);
        watch_until(&window, refused_for_an_exclusion);
        assert_eq!(step(&window), ui::CleanerStep::Results);
        assert_eq!(
            asked.lock().unwrap().len(),
            1,
            "root asked for its scan alone"
        );
        assert!(pip.exists());
        assert!(crash.exists());
        assert!(sandbox.history().is_empty(), "nothing done, nothing kept");
    }

    #[test]
    fn a_new_analysis_refused_from_the_results_keeps_them_and_the_rights() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        sandbox.file("var/crash/core");
        settle_files();
        let asked = Asked::default();
        let refused = Rc::new(Cell::new(false));
        let window = on_unlocked_results_refused_when(&sandbox, &asked, &refused);
        let cleaner = window.global::<ui::Cleaner>();
        let found_before = cleaner.get_found();
        assert_ne!(found_before, crate::shown::size(0));
        refused.set(true);
        button(&window, "Nouvelle analyse").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Planning);
        watch_until(&window, refused_for_an_exclusion);
        assert_eq!(step(&window), ui::CleanerStep::Results);
        assert!(shows(&window, EXCLUSION_REFUSED));
        assert_eq!(cleaner.get_found(), found_before, "what the results found");
        assert_eq!(
            cleaner.get_rights_waiting(),
            0,
            "root's group still unlocked"
        );
        assert_eq!(
            button(&window, "Nouvelle analyse").accessible_enabled(),
            Some(true),
            "a mount may answer again: it may be asked for again"
        );
        let crash = found(&window, "Rapports de plantage");
        assert_eq!(crash.accessible_enabled(), Some(true));
        assert_eq!(crash.accessible_checked(), Some(true));
        // The same report cleans, root's part with it.
        refused.set(false);
        let_the_screen_settle();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[1].0, okapi_helper::Operation::Clean);
    }

    /// Root's part of a cleaning ends with `code` and `output`: the user's
    /// part goes, the receipt says root's was not done, the history why.
    fn roots_part_fails(code: i32, output: String, reason: &str) {
        let sandbox = Sandbox::new();
        let pip = sandbox.file("home/.cache/pip/old");
        sandbox.file("var/crash/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let answer = {
            let scan = scan_answer(&sandbox);
            move |operation, _: &serde_json::Value| match operation {
                okapi_helper::Operation::Scan => (0, scan.clone()),
                okapi_helper::Operation::Clean => (code, output.clone()),
            }
        };
        let seams = admin_seams(&sandbox, &rules, &asked, answer);
        let (window, _screen) = on_admin_results(seams, &rules);
        obtain_rights(&window);
        found(&window, "Rapports de plantage").invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!pip.exists(), "{reason}: the user's part goes");
        assert_eq!(
            steps_shown(&window),
            [
                (ui::CleanStepKind::Admin, ui::RowState::NotDone),
                (ui::CleanStepKind::Check, ui::RowState::Done),
                (ui::CleanStepKind::Remove, ui::RowState::Done),
                (ui::CleanStepKind::History, ui::RowState::Done),
            ],
            "{reason}"
        );
        assert!(
            shows_a_text_starting(&window, "La partie administrateur ne s'est pas terminée."),
            "{reason}"
        );
        let history = sandbox.history();
        assert_eq!(
            history[0]["admin"],
            serde_json::json!({ "done": false, "reason": reason })
        );
        assert_eq!(history[0]["complete"], false);
    }

    #[test]
    fn a_refused_part_of_roots_lets_the_users_part_go() {
        roots_part_fails(
            okapi_helper::PKEXEC_NOT_AUTHORIZED,
            String::new(),
            "notAuthorized",
        );
    }

    #[test]
    fn a_stopped_part_of_roots_lets_the_users_part_go() {
        roots_part_fails(
            1,
            clean_answer(&["unsupportedSystem"], &serde_json::json!([])),
            "stopped",
        );
    }

    #[test]
    fn a_group_changed_since_roots_scan_is_said_not_cleaned_whole() {
        // Walked only in part: what root removed of it is said all the same.
        let removed = serde_json::json!([{
            "outcome": { "path": "/var/crash/old", "result": "removed", "bytes": 7 },
            "scannedBytes": 7,
        }]);
        let mut answer: serde_json::Value =
            serde_json::from_str(&clean_answer(&[], &removed)).unwrap();
        answer["issues"] = serde_json::json!([{
            "kind": "limitReached",
            "targets": ["/var/crash"],
            "path": "/var/crash",
            "detail": "Entries",
        }]);
        roots_part_fails(0, answer.to_string(), "changed");
    }

    #[test]
    fn roots_part_cancelled_on_the_way_is_not_done() {
        let removed_then_cancelled = serde_json::json!([
            { "outcome": { "path": "/var/crash/old", "result": "removed", "bytes": 7 }, "scannedBytes": 7 },
            { "outcome": { "path": "/var/crash/new", "result": "cancelled", "bytes": 0 }, "scannedBytes": 7 },
        ]);
        roots_part_fails(
            1,
            clean_answer(&["cancelled"], &removed_then_cancelled),
            "cancelled",
        );
    }

    #[test]
    fn roots_part_cancelled_before_anything_is_not_done() {
        roots_part_fails(
            1,
            clean_answer(&["cancelled"], &serde_json::json!([])),
            "cancelled",
        );
    }

    #[test]
    fn an_unreadable_part_of_roots_may_have_removed_entries() {
        roots_part_fails(0, "{\"version\": 2".to_owned(), "unknown");
    }

    /// Root's part, checked alone, ends with `code` and `output`: the
    /// receipt is titled `title`.
    fn roots_part_alone_ends(code: i32, output: String, title: &str) {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        sandbox.file("var/crash/core");
        settle_files();
        let rules = admin_rules(&sandbox);
        let asked = Asked::default();
        let answer = {
            let scan = scan_answer(&sandbox);
            move |operation, _: &serde_json::Value| match operation {
                okapi_helper::Operation::Scan => (0, scan.clone()),
                okapi_helper::Operation::Clean => (code, output.clone()),
            }
        };
        let seams = admin_seams(&sandbox, &rules, &asked, answer);
        let (window, _screen) = on_admin_results(seams, &rules);
        obtain_rights(&window);
        button(&window, "Tout désélectionner").invoke_accessible_default_action();
        found(&window, "Rapports de plantage").invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(shows(&window, title));
    }

    #[test]
    fn roots_part_alone_that_fails_ends_stopped() {
        roots_part_alone_ends(
            okapi_helper::PKEXEC_NOT_AUTHORIZED,
            String::new(),
            "Nettoyage arrêté",
        );
    }

    #[test]
    fn roots_part_alone_whose_end_is_unknown_never_says_nothing_was_touched() {
        roots_part_alone_ends(0, "{\"version\": 2".to_owned(), "Nettoyage interrompu");
    }

    /// The line of Docker's build cache, in French.
    const DOCKER: &str = "Cache de construction Docker";

    type AgesAsked = Arc<std::sync::Mutex<Vec<Duration>>>;
    /// What the screen told the window each cleaning freed.
    type FreedTold = Rc<std::cell::RefCell<Vec<Option<u64>>>>;

    /// The results of `seams` with a Docker that accepts the user or not
    /// (`access`), its prune answering what `answer` makes; the minimum ages
    /// asked for land in what is returned last, and what the screen told
    /// the window of the cleaning's end (its `freed`) in the one before.
    fn on_results_with_docker(
        seams: Seams,
        access: okapi_tools::docker::Access,
        answer: impl Fn(
            &dyn Fn() -> bool,
        )
            -> Result<okapi_tools::docker::Pruned, okapi_tools::docker::DockerError>
        + Send
        + Sync
        + 'static,
    ) -> (
        ui::AppWindow,
        super::super::CleanerScreen,
        FreedTold,
        AgesAsked,
    ) {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (window, screen) = on_results(Seams {
            docker: super::super::docker::tests::fake(access, Arc::clone(&asked), answer),
            ..seams
        });
        let ended = Rc::new(std::cell::RefCell::new(Vec::new()));
        let told = Rc::clone(&ended);
        screen.listen(Rc::new(move |event| {
            if let super::super::CleanerEvent::CleaningDone { freed, .. } = event {
                told.borrow_mut().push(freed);
            }
        }));
        (window, screen, ended, asked)
    }

    fn pruned(caches: usize, bytes: u64) -> okapi_tools::docker::Pruned {
        okapi_tools::docker::Pruned { caches, bytes }
    }

    fn sandbox_seams(sandbox: &Sandbox) -> Seams {
        sandbox.read(&sandbox.root.join("state"))
    }

    #[test]
    fn docker_prunes_alone_once_checked_and_confirmed_and_both_receipt_and_history_say_it() {
        let sandbox = Sandbox::new();
        settle_files();
        let (window, _screen, ended, asked) = on_results_with_docker(
            sandbox_seams(&sandbox),
            okapi_tools::docker::Access::Allowed,
            |_| Ok(pruned(3, 3_000)),
        );
        let line = found(&window, DOCKER);
        assert_eq!(
            line.accessible_checked(),
            Some(false),
            "never checked before choosing (invariant 5)"
        );
        let description = {
            use slint::Model as _;
            window
                .global::<ui::Cleaner>()
                .get_results()
                .iter()
                .find(|row| row.label == DOCKER)
                .unwrap()
                .description
        };
        assert!(
            description.contains("7 jours") && !description.contains('{'),
            "its limits filled: {description}"
        );
        assert!(shows(&window, "Élagué par Docker : à cocher vous-même."));
        let cleaner = window.global::<ui::Cleaner>();
        assert_eq!(cleaner.get_selected_operations(), 0);
        assert_eq!(
            button(&window, "Nettoyer").accessible_enabled(),
            Some(false),
            "nothing checked"
        );
        line.invoke_accessible_default_action();
        assert_eq!(cleaner.get_selected_operations(), 1);
        assert_eq!(cleaner.get_selected_entries(), 0, "no entry of its");
        button(&window, "Nettoyer").invoke_accessible_default_action();
        assert!(cleaner.get_confirming(), "a prune alone is a cleaning");
        assert!(
            asked.lock().unwrap().is_empty(),
            "nothing before the confirmation"
        );
        slint::platform::update_timers_and_animations();
        let_the_screen_settle();
        assert!(shows(
            &window,
            "Docker élaguera son cache de construction : ce qui part n'est connu qu'après."
        ));
        found(&window, "J'ai compris que c'est définitif").invoke_accessible_default_action();
        button(&window, "Supprimer définitivement").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert_eq!(
            *asked.lock().unwrap(),
            [Duration::ZERO],
            "once, with the settings' minimum age"
        );
        assert_eq!(
            steps_shown(&window),
            [
                (ui::CleanStepKind::Docker, ui::RowState::Done),
                (ui::CleanStepKind::History, ui::RowState::Done),
            ]
        );
        assert!(shows(&window, "Nettoyage terminé"));
        assert!(shows_a_text_starting(
            &window,
            "Docker : 3 caches retirés, "
        ));
        assert!(shows(
            &window,
            "Aucun élément n'a été retiré ; ce que Docker a fait est dit ci-dessous."
        ));
        assert_eq!(*ended.borrow(), [Some(3_000)], "its bytes freed, known");
        let history = sandbox.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["removedBytes"], 3_000, "its bytes counted");
        assert_eq!(
            history[0]["operations"],
            serde_json::json!([{
                "action": "docker-build-prune",
                "end": "done",
                "entries": 3,
                "bytes": 3_000,
            }])
        );
    }

    /// The user's part first: what it removes is gone when Docker is
    /// asked; both counted.
    #[test]
    fn docker_prunes_once_the_users_part_is_over() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let seen_gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (window, _screen, ended, _) = {
            let old = old.clone();
            let seen_gone = Arc::clone(&seen_gone);
            on_results_with_docker(
                sandbox_seams(&sandbox),
                okapi_tools::docker::Access::Allowed,
                move |_| {
                    seen_gone.store(!old.exists(), std::sync::atomic::Ordering::SeqCst);
                    Ok(pruned(1, 1_000))
                },
            )
        };
        found(&window, DOCKER).invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(
            seen_gone.load(std::sync::atomic::Ordering::SeqCst),
            "pip's cache removed before Docker was asked"
        );
        assert_eq!(
            steps_shown(&window),
            [
                (ui::CleanStepKind::Check, ui::RowState::Done),
                (ui::CleanStepKind::Remove, ui::RowState::Done),
                (ui::CleanStepKind::Docker, ui::RowState::Done),
                (ui::CleanStepKind::History, ui::RowState::Done),
            ]
        );
        let removed = sandbox.history()[0]["removedBytes"].as_u64().unwrap();
        assert_eq!(removed, 1_007, "pip's bytes and Docker's");
        assert_eq!(*ended.borrow(), [Some(removed)]);
    }

    /// Docker never takes younger entries than the settings allow.
    #[test]
    fn docker_is_asked_for_the_settings_minimum_age() {
        let sandbox = Sandbox::new();
        settle_files();
        let thirty_days_minutes = 30 * 24 * 60;
        let inputs = Inputs {
            min_age_minutes: Some(thirty_days_minutes),
            ..sandbox.inputs(Some(&sandbox.root.join("state")))
        };
        let (window, _screen, _, asked) = on_results_with_docker(
            Seams {
                read: Rc::new(move || Ok(inputs.clone())),
                ..sandbox_seams(&sandbox)
            },
            okapi_tools::docker::Access::Allowed,
            |_| Ok(pruned(0, 0)),
        );
        found(&window, DOCKER).invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert_eq!(
            *asked.lock().unwrap(),
            [Duration::from_mins(thirty_days_minutes)]
        );
        assert!(shows(&window, "Docker : rien à retirer."));
    }

    #[test]
    fn docker_absent_at_the_cleaning_ends_it_stopped_and_freed_nothing() {
        let sandbox = Sandbox::new();
        settle_files();
        let (window, _screen, ended, _) = on_results_with_docker(
            sandbox_seams(&sandbox),
            okapi_tools::docker::Access::Allowed,
            |_| {
                Err(okapi_tools::docker::DockerError::Absent {
                    socket: PathBuf::from(super::super::docker::tests::FAKE_SOCKET),
                    source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
                })
            },
        );
        found(&window, DOCKER).invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(shows(&window, "Nettoyage arrêté"));
        assert!(shows(
            &window,
            "Docker : rien de retiré, le moteur ne tourne pas."
        ));
        assert_eq!(
            steps_shown(&window)[0],
            (ui::CleanStepKind::Docker, ui::RowState::NotDone)
        );
        assert_eq!(*ended.borrow(), [Some(0)], "nothing went: known");
        let history = sandbox.history();
        assert_eq!(history[0]["removedBytes"], 0);
        assert_eq!(history[0]["complete"], false);
        assert_eq!(
            history[0]["operations"][0],
            serde_json::json!({
                "action": "docker-build-prune",
                "end": "notDone",
                "reason": "absent",
            })
        );
    }

    /// Docker asked, then silent: it may have pruned, so what was freed is
    /// unknown, in the receipt as in the history and on the Home page.
    #[test]
    fn docker_silent_once_asked_leaves_what_was_freed_unknown() {
        let sandbox = Sandbox::new();
        settle_files();
        let (window, _screen, ended, _) = on_results_with_docker(
            sandbox_seams(&sandbox),
            okapi_tools::docker::Access::Allowed,
            |_| {
                Err(okapi_tools::docker::DockerError::Io(std::io::Error::from(
                    std::io::ErrorKind::TimedOut,
                )))
            },
        );
        found(&window, DOCKER).invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(
            shows(&window, "Nettoyage terminé"),
            "never \"nothing was touched\""
        );
        assert!(shows(&window, "Espace libéré : Inconnu"), "what was freed");
        assert!(window.global::<ui::Cleaner>().get_freed_unknown());
        assert!(shows(
            &window,
            "Docker : sa réponse n'a pas dit ce qui est parti, la place libérée est inconnue. Le détail est dans le journal."
        ));
        assert_eq!(*ended.borrow(), [None]);
        assert_eq!(
            sandbox.history()[0]["operations"][0],
            serde_json::json!({
                "action": "docker-build-prune",
                "end": "unknown",
                "reason": "unanswered",
            })
        );
    }

    /// Cancelled while Docker works: the wait ends, what went is unknown.
    #[test]
    fn a_cleaning_cancelled_while_docker_prunes_ends_cancelled_and_unknown() {
        let sandbox = Sandbox::new();
        settle_files();
        let (window, _screen, ended, asked) = on_results_with_docker(
            sandbox_seams(&sandbox),
            okapi_tools::docker::Access::Allowed,
            |cancelled| {
                while !cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(okapi_tools::docker::DockerError::Cancelled)
            },
        );
        found(&window, DOCKER).invoke_accessible_default_action();
        confirm(&window);
        watch_until(&window, |window| {
            window.global::<ui::Cleaner>().get_pruning()
        });
        assert!(shows(
            &window,
            "Docker élague son cache de construction ; ce qui part n'est connu qu'à la fin."
        ));
        assert_eq!(
            steps_shown(&window)[0],
            (ui::CleanStepKind::Docker, ui::RowState::Running)
        );
        let_the_screen_settle();
        button(&window, "Annuler le nettoyage").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert_eq!(asked.lock().unwrap().len(), 1);
        assert!(shows(&window, "Nettoyage annulé"));
        assert!(window.global::<ui::Cleaner>().get_freed_unknown());
        assert!(shows(
            &window,
            "Docker : élagage annulé en cours, ce qui est parti est inconnu."
        ));
        assert_eq!(*ended.borrow(), [None]);
        assert_eq!(
            sandbox.history()[0]["operations"][0],
            serde_json::json!({
                "action": "docker-build-prune",
                "end": "unknown",
                "reason": "cancelled",
            })
        );
    }

    /// Cancelled during the user's part: Docker is never asked.
    #[test]
    fn a_cleaning_cancelled_before_docker_never_asks_it() {
        let sandbox = Sandbox::new();
        settle_files();
        let seams = sandbox_seams(&sandbox);
        let real = Arc::clone(&seams.clean);
        let (reached_sender, reached) = std::sync::mpsc::channel::<()>();
        let (go, gone) = std::sync::mpsc::channel::<()>();
        let gone = std::sync::Mutex::new(gone);
        let clean: super::super::Clean = Arc::new(move |job, cancel, progress| {
            reached_sender.send(()).unwrap();
            gone.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
            real(job, cancel, progress)
        });
        let (window, _screen, ended, asked) = on_results_with_docker(
            Seams { clean, ..seams },
            okapi_tools::docker::Access::Allowed,
            |_| Ok(pruned(1, 1)),
        );
        found(&window, DOCKER).invoke_accessible_default_action();
        confirm(&window);
        reached.recv_timeout(Duration::from_secs(30)).unwrap();
        let_the_screen_settle();
        button(&window, "Annuler le nettoyage").invoke_accessible_default_action();
        go.send(()).unwrap();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(asked.lock().unwrap().is_empty(), "never asked");
        assert!(shows(&window, "Nettoyage annulé"));
        assert!(shows(
            &window,
            "Docker : pas demandé, le nettoyage a été annulé avant."
        ));
        assert_eq!(*ended.borrow(), [Some(0)], "nothing went: known");
        assert_eq!(
            sandbox.history()[0]["operations"][0],
            serde_json::json!({
                "action": "docker-build-prune",
                "end": "notDone",
                "reason": "cancelled",
            })
        );
    }

    /// The browser's cache left out, Docker's prune is still what the
    /// cleaning takes.
    #[test]
    fn ignoring_the_only_browser_still_prunes_docker() {
        let sandbox = Sandbox::new();
        let (chrome, _, _) = chrome_open(&sandbox, false);
        let (window, _screen, _, asked) = on_results_with_docker(
            sandbox_seams(&sandbox),
            okapi_tools::docker::Access::Allowed,
            |_| Ok(pruned(1, 1)),
        );
        found(&window, DOCKER).invoke_accessible_default_action();
        confirm_and_ask(&window);
        assert!(asking(&window));
        let_the_screen_settle();
        button(&window, "Ignorer Chrome").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(chrome.exists(), "left out");
        assert_eq!(asked.lock().unwrap().len(), 1);
    }

    #[test]
    fn docker_that_refuses_the_user_shows_its_line_that_cannot_be_checked() {
        let sandbox = Sandbox::new();
        settle_files();
        let (window, _screen, _, asked) = on_results_with_docker(
            sandbox_seams(&sandbox),
            okapi_tools::docker::Access::NotAllowed,
            |_| unreachable!("never asked"),
        );
        let line = found(&window, DOCKER);
        assert_eq!(line.accessible_checked(), Some(false));
        assert!(shows(
            &window,
            "Docker refuse votre compte : il faut être dans le groupe docker, puis rouvrir la session."
        ));
        line.invoke_accessible_default_action();
        assert_eq!(
            line.accessible_checked(),
            Some(false),
            "it cannot be checked"
        );
        found(&window, "Applications").invoke_accessible_default_action();
        found(&window, "Applications").invoke_accessible_default_action();
        assert_eq!(
            line.accessible_checked(),
            Some(false),
            "nor by its category's box"
        );
        assert_eq!(window.global::<ui::Cleaner>().get_selected_operations(), 0);
        assert!(asked.lock().unwrap().is_empty());
    }

    #[test]
    fn without_a_socket_of_docker_there_is_no_line() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen) = on_results(sandbox_seams(&sandbox));
        assert!(
            ElementHandle::find_by_accessible_label(&window, DOCKER)
                .next()
                .is_none()
        );
    }

    /// Docker present but left unchecked (`access` whatever it is): the
    /// cleaning never asks it (invariant 5).
    fn docker_left_unchecked_is_never_asked(access: okapi_tools::docker::Access) {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let (window, _screen, _, asked) =
            on_results_with_docker(sandbox_seams(&sandbox), access, |_| {
                unreachable!("never asked")
            });
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(asked.lock().unwrap().is_empty());
        assert!(
            steps_shown(&window)
                .iter()
                .all(|(kind, _)| *kind != ui::CleanStepKind::Docker)
        );
        assert!(sandbox.history()[0].get("operations").is_none());
    }

    #[test]
    fn docker_accepting_the_user_but_left_unchecked_is_never_asked() {
        docker_left_unchecked_is_never_asked(okapi_tools::docker::Access::Allowed);
    }

    #[test]
    fn docker_refusing_the_user_is_never_asked() {
        docker_left_unchecked_is_never_asked(okapi_tools::docker::Access::NotAllowed);
    }

    /// A cleaning whose thread died may have pruned once Docker was asked.
    #[test]
    fn a_cleaning_whose_thread_died_says_docker_may_have_pruned_once_asked() {
        assert_eq!(
            super::docker_shown(None, true).end,
            ui::OperationEnd::Unknown
        );
        assert_eq!(super::docker_shown(None, false).end, ui::OperationEnd::None);
    }

    /// uv's cache and pnpm's store found: a line each, its box disabled (the
    /// rule that it is never checked is `results.rs`'s; here, nothing to
    /// clean comes of it), its command shown exactly (no space nor line feed
    /// after it, which would run it on a paste), its button pressed, nothing
    /// run.
    #[test]
    fn uv_and_pnpm_show_the_command_the_user_runs_by_hand() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/uv/archive-v0/old");
        sandbox.file("home/.local/share/pnpm/store/v10/files/old");
        settle_files();
        let (window, _screen) = on_results(sandbox_seams(&sandbox));
        let of_line = |label: &str, role: &str| {
            ElementHandle::find_by_accessible_label(&window, role)
                .find(|element| {
                    element
                        .accessible_description()
                        .is_some_and(|description| description == label)
                })
                .unwrap_or_else(|| panic!("no {role} for {label}"))
        };
        for (label, command) in [
            ("Paquets uv inutilisés", "uv cache prune"),
            ("Paquets pnpm inutilisés", "pnpm store prune"),
        ] {
            let line = found(&window, label);
            assert_eq!(line.accessible_checked(), Some(false), "{label}");
            line.invoke_accessible_default_action();
            assert_eq!(line.accessible_checked(), Some(false), "a disabled box");
            let field = of_line(label, "Commande à copier");
            assert_eq!(field.accessible_value().as_deref(), Some(command));
            assert_eq!(field.accessible_read_only(), Some(true), "{label}");
            of_line(label, "Copier").invoke_accessible_default_action();
        }
        assert!(shows(
            &window,
            "À faire vous-même : la commande ci-dessous, dans un terminal."
        ));
        let cleaner = window.global::<ui::Cleaner>();
        assert_eq!(cleaner.get_selected_operations(), 0);
        assert_eq!(cleaner.get_selected_entries(), 0);
        assert_eq!(
            button(&window, "Nettoyer").accessible_enabled(),
            Some(false),
            "nothing to clean"
        );
    }

    /// No cache of uv, and a folder of pnpm without its versioned store: no
    /// line of theirs.
    #[test]
    fn without_their_cache_uv_and_pnpm_have_no_line() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        sandbox.file("home/.local/share/pnpm/store/other/old");
        settle_files();
        let (window, _screen) = on_results(sandbox_seams(&sandbox));
        for label in ["Paquets uv inutilisés", "Paquets pnpm inutilisés"] {
            assert!(
                ElementHandle::find_by_accessible_label(&window, label)
                    .next()
                    .is_none(),
                "{label}"
            );
        }
    }

    /// pnpm alone, its store of pnpm 9 and before (`v3`): its line alone,
    /// with its own command.
    #[test]
    fn pnpm_alone_shows_its_line_alone() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.local/share/pnpm/store/v3/files/old");
        settle_files();
        let (window, _screen) = on_results(sandbox_seams(&sandbox));
        assert_eq!(
            found(&window, "Paquets pnpm inutilisés").accessible_checked(),
            Some(false)
        );
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Paquets uv inutilisés")
                .next()
                .is_none()
        );
        let commands: Vec<_> =
            ElementHandle::find_by_accessible_label(&window, "Commande à copier")
                .filter_map(|field| field.accessible_value())
                .collect();
        assert_eq!(commands, ["pnpm store prune"]);
    }
}
