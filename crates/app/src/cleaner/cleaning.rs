//! Cleaning what the user checked in the results: a confirmation, the
//! cleaning on its own thread, which writes the history line itself before
//! it reports, and the receipt.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::SystemTime;

use okapi_engine::{
    BrowserLock, CleanPass, CleanReceipt, CleanResult, ItemId, KeepReason, Progress, ScanItem,
    ScanOptions, ScanReport, Stop,
};
use okapi_helper::{Operation, OutcomeAnswer, Reply, ReplyError};
use okapi_rules::{RuleSet, TemplateVariables};
use okapi_store::{AdminRecord, History, HistoryEntry, HistoryOutcome, Opened, seconds};
use slint::ComponentHandle as _;

use super::{
    CleanerEvent, CleanerScreen, Problem, Running, Task, options, problem, rights, stop_shown,
};
use crate::elevation::{ElevationFailure, ElevationStep};
use crate::receipt::{Ending, Kept, LineKind, OutcomeFacts, Receipt};
use crate::shown::{show, shown, size};
use crate::ui;

/// What a cleaning takes: identifiers of the report kept, and only them
/// (invariant 1), with the rules, variables and options read as it starts;
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

/// A cleaning's receipt, root's part, and its history line.
#[derive(Debug)]
pub struct Cleaned {
    receipt: CleanReceipt,
    admin: Option<AdminDone>,
    history: HistoryWrite,
}

/// The cleaning under way, as its progress tells it.
#[derive(Debug, Default)]
struct CleanView {
    /// Root's part under way: its question, then its work.
    rights: Option<ElevationStep>,
    /// Root's part over: whether it was done to its end.
    admin_done: Option<bool>,
    /// How far each pass of the user's part went: entries done, of all.
    check: Option<(usize, usize)>,
    remove: Option<(usize, usize)>,
    freed: u64,
    /// The user cancelled: the passes stay where they were then, the
    /// entries left after it never counted as gone through.
    cancel_asked: bool,
}

impl CleanView {
    /// Every entry went through the check.
    fn check_over(&self) -> bool {
        self.remove.is_some() || matches!(self.check, Some((done, total)) if done == total)
    }

    /// The steps as they stand: root's part if the cleaning has one, the
    /// two passes over the user's `entries` if any, then the history line.
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
    pub(super) running: Running<Cleaned>,
    steps: Receiver<ElevationStep>,
    /// Root's part over: whether it was done to its end.
    admin_ended: Receiver<bool>,
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
            .filter_map(|index| groups.get(*index))
            .flat_map(|group| group.items().iter().map(ScanItem::id))
            .collect()
    }

    /// Leaves out of this cleaning the groups that empty a cache of the
    /// browser of `lock`: what is confirmed is then what is left. The entries
    /// left, root's part included (a browser's cache is never one of its).
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
        self.entries + self.admin_entries
    }
}

/// A cleaning ready to start: its job, the history it writes to, and whether
/// the user asked for a deletion log.
struct Prepared {
    job: CleanJob,
    history: Option<History>,
    deletion_log: bool,
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

/// Adds the line of the cleaning to `history`, from the cleaning's thread:
/// the line is written before the screen stops waiting for it, unless the
/// user insists on closing. One line for both parts: root's entries first.
fn record(
    history: Option<History>,
    receipt: &CleanReceipt,
    admin: Option<&AdminDone>,
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
    );
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
            if entries == 0 {
                tracing::warn!("cleaning asked for without an entry");
                return;
            }
            tracing::info!(groups = groups.len(), admin = ?admin_targets, entries, bytes, "cleaning to confirm");
            state.confirming = Some(Confirming {
                report: Arc::clone(&held.report),
                entries: results.entries_of(&groups),
                groups,
                admin_entries: results.entries_of(&admin),
                admin,
                admin_targets,
            });
            (
                entries,
                bytes,
                state
                    .confirming
                    .as_ref()
                    .is_some_and(|confirming| !confirming.admin.is_empty()),
            )
        };
        self.settle();
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_understood(false);
            cleaner.set_confirm_entries(shown(opened.0));
            cleaner.set_confirm_size(size(opened.1));
            cleaner.set_confirm_admin(opened.2);
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

    /// The cleaning of `confirmed`, its options built now from the settings
    /// read now, its history opened before anything is removed.
    fn prepare_cleaning(&self, confirmed: &Confirming) -> Result<Prepared, Problem> {
        let state = self.0.borrow();
        let rules = state.rules.as_ref().ok_or(Problem::Rules)?;
        let inputs = (state.seams.read)()?;
        let options = options(&inputs, SystemTime::now(), BTreeSet::new())?;
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
        // which must still be those of the settings read now: root never
        // removes what the settings came to protect since.
        let admin = if confirmed.admin_targets.is_empty() {
            None
        } else {
            let held = state.held.as_ref();
            let Some(granted) = held.and_then(|held| held.granted.as_ref()) else {
                tracing::error!(
                    "groups that need the administrator confirmed without the rights: nothing removed"
                );
                return Err(Problem::Start);
            };
            let now = rights::Granted::of(
                &inputs,
                &held
                    .map(|held| held.results.helper_targets())
                    .unwrap_or_default(),
            );
            if &now != granted {
                tracing::warn!(
                    ?granted,
                    ?now,
                    "settings changed since the rights were obtained: nothing removed"
                );
                return Err(Problem::RightsChanged);
            }
            let Some(line) = granted.line(Operation::Clean, &confirmed.admin_targets) else {
                return Err(Problem::Start);
            };
            tracing::info!(targets = ?confirmed.admin_targets, ?granted, "root's part of the cleaning");
            Some(line)
        };
        Ok(Prepared {
            job: CleanJob {
                report: Arc::clone(&confirmed.report),
                ids,
                rules: Arc::clone(rules),
                variables: inputs.variables,
                options,
                admin,
            },
            history,
            deletion_log: inputs.deletion_log,
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
            if found == Problem::RightsChanged {
                // The groups root unlocked were measured with other settings:
                // they must be unlocked again before they can be checked.
                if let Some(held) = self.0.borrow_mut().held.as_mut() {
                    held.results.relock();
                    held.granted = None;
                }
                self.show_results();
            }
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
        } = prepared;
        tracing::info!(ids = job.ids.len(), admin = job.admin.is_some(), options = ?job.options, history = history.is_some(), deletion_log, "cleaning started");
        let report = Arc::clone(&job.report);
        let had_admin = job.admin.is_some();
        let clean = Arc::clone(&self.0.borrow().seams.clean);
        let elevate = Arc::clone(&self.0.borrow().seams.elevate);
        let entries = job.ids.len();
        let (step_sender, steps) = mpsc::channel();
        let (admin_sender, admin_ended) = mpsc::channel();
        let running = Running::start(
            "okapi-cleaning",
            tracing::info_span!("cleaning"),
            move |cancel, progress| {
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
                let receipt = clean(&job, cancel, progress);
                let history = record(history, &receipt, admin.as_ref(), deletion_log);
                Cleaned {
                    receipt,
                    admin,
                    history,
                }
            },
        )
        .map_err(|error| {
            tracing::error!(%error, "cleaning thread not started: nothing removed");
            Problem::Start
        })?;
        self.with_window(|window, _| window.global::<ui::Cleaner>().set_problem(problem(None)));
        self.0.borrow_mut().task = Some(Task::Cleaning(Cleaning {
            running,
            steps,
            admin_ended,
            view: CleanView::default(),
            report,
            deletion_log,
            close_when_done: false,
            had_admin,
            entries,
        }));
        self.settle();
        self.with_window(|window, state| {
            show(
                &state.step_model,
                CleanView::default().steps(had_admin, entries),
            );
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_clean_cancel_asked(false);
            cleaner.set_freed(size(0));
            cleaner.set_step(ui::CleanerStep::Cleaning);
        });
        self.watch_task();
        Ok(())
    }

    /// Takes in the progress of the cleaning, then looks for its receipt.
    pub(super) fn watch_cleaning(&self) {
        let (result, steps, freed, rights) = {
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
        });
        match result {
            Ok(cleaned) => self.end_cleaning(Some(&cleaned)),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                let had_admin = matches!(
                    &self.0.borrow().task,
                    Some(Task::Cleaning(cleaning)) if cleaning.had_admin
                );
                tracing::error!(
                    had_admin,
                    "cleaning thread ended without a receipt: its history line is not written, and root's part, if any, may have removed entries"
                );
                self.end_cleaning(None);
            }
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
                receipt_of(&done.receipt, &cleaning.report, done.admin.as_ref()),
            )
        });
        if let Some((done, receipt)) = &receipt {
            tracing::info!(
                ending = ?receipt.ending,
                freed = receipt.freed,
                removed = receipt.removed,
                partial = receipt.partial,
                gone = receipt.gone,
                kept = receipt.kept,
                history = ?done.history,
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
        self.with_window(|window, state| {
            show(&state.step_model, steps);
            let shown_to = window.global::<ui::Cleaner>();
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
                tracing::info!("receipt in: the window closes as asked");
                if let Err(error) = window.hide() {
                    tracing::error!(%error, "window not hidden");
                }
            }
        });
        // Even when it freed nothing it knows of: root's part, or a thread
        // that died, may have removed entries.
        self.emit(CleanerEvent::CleaningDone {
            freed: receipt.as_ref().map(|(_, receipt)| receipt.freed),
            at: seconds(SystemTime::now()),
        });
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
/// `report`, and root's part, if any. It ends cancelled when the system's
/// question was dismissed, stopped when neither part did anything.
fn receipt_of(receipt: &CleanReceipt, report: &ScanReport, admin: Option<&AdminDone>) -> Receipt {
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
    let failed = !receipt.stops().is_empty() || problem != ui::RightsProblem::None;
    let mut shown = Receipt::new(failed && !unknown && facts.is_empty(), &facts);
    if unknown && facts.is_empty() {
        shown.ending = Ending::Interrupted;
    } else if matches!(
        problem,
        ui::RightsProblem::Dismissed | ui::RightsProblem::Cancelled
    ) {
        shown.ending = Ending::Cancelled;
    }
    shown
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
    cleaner.set_freed(size(receipt.freed));
    cleaner.set_removed(shown(receipt.removed));
    cleaner.set_partial(shown(receipt.partial));
    cleaner.set_gone(shown(receipt.gone));
    cleaner.set_kept(shown(receipt.kept));
    let lines = receipt
        .lines
        .iter()
        .map(|line| ui::ReceiptLineShown {
            kind: match line.kind {
                LineKind::Removed => ui::ReceiptKind::Removed,
                LineKind::Partial => ui::ReceiptKind::Partial,
                LineKind::Gone => ui::ReceiptKind::Gone,
                LineKind::Kept(Kept::Recent) => ui::ReceiptKind::Recent,
                LineKind::Kept(Kept::Changed) => ui::ReceiptKind::Changed,
                LineKind::Kept(Kept::Replaced) => ui::ReceiptKind::Replaced,
                LineKind::Kept(Kept::Excluded) => ui::ReceiptKind::Excluded,
                LineKind::Kept(Kept::Protected) => ui::ReceiptKind::Protected,
                LineKind::Kept(Kept::Refused) => ui::ReceiptKind::Refused,
                LineKind::Kept(Kept::Cancelled) => ui::ReceiptKind::Cancelled,
                LineKind::Kept(Kept::BrowserOpen) => ui::ReceiptKind::BrowserOpen,
                LineKind::Kept(Kept::Unknown) => ui::ReceiptKind::Unknown,
            },
            count: shown(line.count),
            size: size(line.bytes),
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
        })
        .collect();
    show(model, lines);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::Duration;

    use i_slint_backend_testing::{AccessibleRole, ElementHandle};
    use okapi_rules::{Os, RuleSet, TemplateVariables};
    use slint::ComponentHandle as _;

    use std::sync::Arc;

    use super::super::tests::{
        SIDEBAR_STOPS, Sandbox, button, checkbox, let_the_screen_settle, on_cleaner, press,
        settle_files, shows, shows_a_text_starting, step, watch_until,
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
            history[0]["entries"][0]["path"],
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
        let mut paths: Vec<String> = sandbox.history()[0]["entries"]
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
            ..Seams::system()
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
            ..Seams::system()
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
        window
            .window()
            .set_size(slint::PhysicalSize::new(1280, 480));
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
        let (_, _, font_height) = place(&window, "Cache des polices");
        assert!(
            font_height > thumbnails_height,
            "a line with its note is higher: {font_height} {thumbnails_height}"
        );
        let (category_x, ..) = place(&window, "Système");
        let (group_x, ..) = place(&window, "Cache des miniatures");
        assert!(group_x > category_x, "a group further in than its category");
        // Folded, Système leaves no room: Applications comes right below.
        let_the_screen_settle();
        window.global::<ui::Cleaner>().invoke_fold(0);
        let (_, applications_top, _) = place(&window, "Applications");
        assert!(
            applications_top - (system_top + system_height) < system_height,
            "folded away: {system_top} {system_height} {applications_top}"
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

    /// Chrome and Edge open, the second shown after the first; a seam whose
    /// answer for each browser `answer` gives, from how many times it was
    /// asked about it, and that counts the requests to close.
    fn two_browsers(
        answer: fn(okapi_rules::ChromiumBrowserKind, u32) -> bool,
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
                        pid: if lock.browser() == okapi_rules::ChromiumBrowserKind::Chrome {
                            1
                        } else {
                            2
                        },
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
        use okapi_rules::ChromiumBrowserKind::Chrome;
        use slint::platform::Key;

        // Chrome open when shown, closed at the click; Edge open.
        let (sandbox, seams, signalled) =
            two_browsers(|browser, times| browser != Chrome || times == 1);
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
        let target = |relative: &str, subcategory: &str| {
            serde_json::json!({
                "path": sandbox.root.join(relative),
                "subcategory": subcategory,
                "needsAdmin": true,
            })
        };
        let system = serde_json::json!({
            "type": "system",
            "cleanTargets": [target("var/crash", "Crash Reports"), target("var/dumps", "Core Dumps")],
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
            history[0]["entries"][0]["path"],
            crash.display().to_string(),
            "root's first"
        );
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
}
