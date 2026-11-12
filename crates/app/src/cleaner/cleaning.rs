//! Cleaning what the user checked in the results: a confirmation, the
//! cleaning on its own thread, which writes the history line itself before
//! it reports, and the receipt.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::mpsc::TryRecvError;
use std::time::SystemTime;

use okapi_engine::{
    CleanPass, CleanReceipt, CleanResult, ItemId, KeepReason, Progress, ScanItem, ScanOptions,
    ScanReport,
};
use okapi_rules::{RuleSet, TemplateVariables};
use okapi_store::{History, Opened};
use slint::ComponentHandle as _;

use super::{
    CleanerScreen, Problem, Running, Task, options, problem, show, shown, size, stop_shown,
};
use crate::receipt::{Ending, Kept, LineKind, OutcomeFacts, Receipt};
use crate::ui;

/// What a cleaning takes: identifiers of the report kept, and only them
/// (invariant 1), with the rules, variables and options read as it starts.
#[derive(Debug)]
pub struct CleanJob {
    pub report: Arc<ScanReport>,
    pub ids: Vec<ItemId>,
    pub rules: Arc<RuleSet>,
    pub variables: TemplateVariables,
    pub options: ScanOptions,
}

/// What became of the history line of a cleaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryWrite {
    Recorded,
    Failed,
    /// No history could be opened (without a deletion log asked for).
    Unavailable,
}

/// A cleaning's receipt, and its history line.
#[derive(Debug)]
pub struct Cleaned {
    receipt: CleanReceipt,
    history: HistoryWrite,
}

/// The cleaning under way, as its progress tells it.
#[derive(Debug, Default)]
struct CleanView {
    pass: Option<CleanPass>,
    done: usize,
    total: usize,
    freed: u64,
}

/// A cleaning under way. The screen never gives it up: a removal cut short
/// would leave no history line (only the window closing with the user
/// insisting, or the event loop ending, leaves it).
pub(super) struct Cleaning {
    pub(super) running: Running<Cleaned>,
    view: CleanView,
    report: Arc<ScanReport>,
    /// The user asked for every entry in the history.
    deletion_log: bool,
    /// The window asked to close: it hides once the receipt is in.
    close_when_done: bool,
}

/// The confirmation open: what it shows, of which report.
pub(super) struct Confirming {
    report: Arc<ScanReport>,
    groups: BTreeSet<usize>,
    entries: usize,
}

/// A cleaning ready to start: its job, the history it writes to, and whether
/// the user asked for a deletion log.
struct Prepared {
    job: CleanJob,
    history: Option<History>,
    deletion_log: bool,
}

/// Adds the line of `receipt` to `history`, from the cleaning's thread: the
/// line is written before the screen stops waiting for it, unless the user
/// insists on closing.
fn record(history: Option<History>, receipt: &CleanReceipt, deletion_log: bool) -> HistoryWrite {
    let Some(mut history) = history else {
        return HistoryWrite::Unavailable;
    };
    match history.record_cleaning(receipt, deletion_log) {
        Ok(()) => HistoryWrite::Recorded,
        Err(_) => HistoryWrite::Failed,
    }
}

impl CleanerScreen {
    /// Opens the confirmation of the groups checked: what it shows is what a
    /// confirmed cleaning takes, of this very report.
    pub(super) fn clean(&self) {
        if self.is_settling("cleaning") {
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
            let groups = held.results.checked().clone();
            let (entries, bytes) = held.results.selected();
            if entries == 0 {
                tracing::warn!("cleaning asked for without an entry");
                return;
            }
            tracing::info!(groups = groups.len(), entries, bytes, "cleaning to confirm");
            state.confirming = Some(Confirming {
                report: Arc::clone(&held.report),
                groups,
                entries,
            });
            (entries, bytes)
        };
        self.settle();
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_understood(false);
            cleaner.set_confirm_entries(shown(opened.0));
            cleaner.set_confirm_size(size(opened.1));
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

    /// Whether a confirmation is open: the results are then frozen.
    pub(super) fn is_confirming(&self) -> bool {
        self.0.borrow().confirming.is_some()
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
        let groups = confirmed.report.groups();
        let ids: Vec<ItemId> = confirmed
            .groups
            .iter()
            .filter_map(|index| groups.get(*index))
            .flat_map(|group| group.items().iter().map(ScanItem::id))
            .collect();
        if ids.len() != confirmed.entries {
            tracing::error!(
                ids = ids.len(),
                confirmed = confirmed.entries,
                "the cleaning would not take what was confirmed: nothing removed"
            );
            return Err(Problem::Start);
        }
        Ok(Prepared {
            job: CleanJob {
                report: Arc::clone(&confirmed.report),
                ids,
                rules: Arc::clone(rules),
                variables: inputs.variables,
                options,
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
            .prepare_cleaning(&confirmed)
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
        } = prepared;
        tracing::info!(ids = job.ids.len(), options = ?job.options, history = history.is_some(), deletion_log, "cleaning started");
        let report = Arc::clone(&job.report);
        let clean = Arc::clone(&self.0.borrow().seams.clean);
        let running = Running::start(
            "okapi-cleaning",
            tracing::info_span!("cleaning"),
            move |cancel, progress| {
                let receipt = clean(&job, cancel, progress);
                let history = record(history, &receipt, deletion_log);
                Cleaned { receipt, history }
            },
        )
        .map_err(|error| {
            tracing::error!(%error, "cleaning thread not started: nothing removed");
            Problem::Start
        })?;
        self.with_window(|window, _| window.global::<ui::Cleaner>().set_problem(problem(None)));
        self.0.borrow_mut().task = Some(Task::Cleaning(Cleaning {
            running,
            view: CleanView::default(),
            report,
            deletion_log,
            close_when_done: false,
        }));
        self.settle();
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_clean_cancel_asked(false);
            cleaner.set_clean_done(0);
            cleaner.set_clean_total(0);
            cleaner.set_freed(size(0));
            cleaner.set_clean_pass(ui::CleanPassKind::Check);
            cleaner.set_step(ui::CleanerStep::Cleaning);
        });
        self.watch_task();
        Ok(())
    }

    /// Takes in the progress of the cleaning, then looks for its receipt.
    pub(super) fn watch_cleaning(&self) {
        let (result, view) = {
            let mut state = self.0.borrow_mut();
            let Some(Task::Cleaning(cleaning)) = state.task.as_mut() else {
                return;
            };
            for progress in cleaning.running.progress.try_iter() {
                if let Progress::Cleaning {
                    pass,
                    done,
                    total,
                    removed_bytes,
                } = progress
                {
                    cleaning.view = CleanView {
                        pass: Some(pass),
                        done,
                        total,
                        freed: removed_bytes,
                    };
                }
            }
            let view = &cleaning.view;
            (
                cleaning.running.result.try_recv(),
                (view.pass, view.done, view.total, view.freed),
            )
        };
        let (pass, done, total, freed) = view;
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_clean_pass(match pass {
                Some(CleanPass::Remove) => ui::CleanPassKind::Remove,
                Some(CleanPass::Check) | None => ui::CleanPassKind::Check,
            });
            cleaner.set_clean_done(shown(done));
            cleaner.set_clean_total(shown(total));
            cleaner.set_freed(size(freed));
        });
        match result {
            Ok(cleaned) => self.end_cleaning(Some(&cleaned)),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                tracing::error!(
                    "cleaning thread ended without a receipt: its history line is not written"
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
        let Some(Task::Cleaning(cleaning)) = self.0.borrow_mut().task.take() else {
            return;
        };
        {
            let mut state = self.0.borrow_mut();
            state.timer.stop();
            state.held = None;
        }
        self.settle();
        let receipt = done.map(|done| (done, receipt_of(&done.receipt, &cleaning.report)));
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
            let shown_to = window.global::<ui::Cleaner>();
            if let Some((done, receipt)) = &receipt {
                show(
                    &state.stop_model,
                    done.receipt.stops().iter().filter_map(stop_shown).collect(),
                );
                show_receipt(&shown_to, &state.receipt_model, receipt);
                shown_to.set_history(match done.history {
                    HistoryWrite::Recorded => ui::HistoryState::Recorded,
                    HistoryWrite::Failed => ui::HistoryState::Failed,
                    HistoryWrite::Unavailable => ui::HistoryState::Unavailable,
                });
            } else {
                show(&state.receipt_model, Vec::new());
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
    }

    /// Cancels the cleaning: what is not yet removed stays; the entry under
    /// way finishes.
    pub(super) fn cancel_cleaning(&self) {
        if self.is_settling("cancellation of the cleaning") {
            return;
        }
        let asked = {
            let state = self.0.borrow();
            let Some(Task::Cleaning(cleaning)) = &state.task else {
                return;
            };
            let first = !cleaning.running.cancel.is_cancelled();
            cleaning.running.cancel.cancel();
            first
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
        let insisted = {
            let mut state = self.0.borrow_mut();
            let Some(Task::Cleaning(cleaning)) = state.task.as_mut() else {
                return slint::CloseRequestResponse::HideWindow;
            };
            let insisted = cleaning.close_when_done && !settling;
            cleaning.close_when_done = true;
            cleaning.running.cancel.cancel();
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

/// The receipt of `receipt`, cleaned from `report`.
fn receipt_of(receipt: &CleanReceipt, report: &ScanReport) -> Receipt {
    let facts = OutcomeFacts::all(receipt, report);
    if facts
        .iter()
        .any(|fact| fact.result == CleanResult::Kept(KeepReason::UnknownItem))
    {
        tracing::error!("an identifier of another report reached the cleaning");
    }
    Receipt::new(!receipt.stops().is_empty(), &facts)
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
    use okapi_rules::{Os, RuleSet, TemplateVariables, Variable};
    use slint::ComponentHandle as _;

    use std::sync::Arc;

    use super::super::tests::{
        button, checkbox, let_the_screen_settle, on_cleaner, press, shows, shows_a_text_starting,
        step, watch_until,
    };
    use super::super::{Inputs, Seams};
    use crate::ui;

    /// A home, a temporary directory and a state directory below the build
    /// directory, never below a target of the rules (`/tmp` is one), with
    /// every absolute target of the rules excluded: the analysis and the
    /// cleaning reach nothing of the machine.
    struct Sandbox {
        _directory: tempfile::TempDir,
        root: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
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

        fn excluded() -> Vec<PathBuf> {
            RuleSet::embedded(Os::Linux)
                .unwrap()
                .absolute_targets(false)
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"content").unwrap();
            path
        }

        fn variables(&self) -> TemplateVariables {
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
        fn inputs(&self, state_directory: Option<&Path>) -> Inputs {
            Inputs {
                variables: self.variables(),
                min_age_minutes: Some(0),
                exclusions: Self::excluded(),
                deletion_log: true,
                state_directory: state_directory.map(Path::to_path_buf),
            }
        }

        fn read(&self, state_directory: &Path) -> Seams {
            let inputs = self.inputs(Some(state_directory));
            Seams {
                read: Rc::new(move || Ok(inputs.clone())),
                ..Seams::system()
            }
        }

        fn history(&self) -> Vec<serde_json::Value> {
            fs::read_to_string(self.root.join("state/history.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    /// File dates follow a coarse clock: a pause puts what was just written
    /// before the analysis's own `now`.
    fn settle_files() {
        std::thread::sleep(Duration::from_millis(50));
    }

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
        assert_eq!(found(&window, "pip Cache").accessible_checked(), Some(true));
        assert!(shows(&window, "Éléments : 1 sur 1"));
        confirm(&window);
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(!old.exists(), "removed");
        assert!(shows(&window, "Nettoyage terminé"));
        assert!(shows(&window, "Le reçu est enregistré dans l'historique."));
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
        found(&window, "pip Cache").invoke_accessible_default_action();
        assert_eq!(found(&window, "pip Cache").accessible_checked(), Some(true));
        button(&window, "Nouvelle analyse").invoke_accessible_default_action();
        assert_eq!(step(&window), ui::CleanerStep::Results);
        button(&window, "Annuler").invoke_accessible_default_action();
        assert!(!window.global::<ui::Cleaner>().get_confirming());
        assert!(old.exists());
        // The second click of a double click on "Annuler" lands on the
        // results below: they do not change.
        found(&window, "pip Cache").invoke_accessible_default_action();
        assert_eq!(found(&window, "pip Cache").accessible_checked(), Some(true));
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
    fn a_cancelled_cleaning_is_never_given_up() {
        let sandbox = Sandbox::new();
        let old = sandbox.file("home/.cache/pip/old");
        settle_files();
        let (seams, go) = held_back(sandbox.read(&sandbox.root.join("state")));
        let (window, _screen) = on_results(seams);
        confirm(&window);
        let_the_screen_settle();
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
        for _ in 0..6 {
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
            ElementHandle::find_by_accessible_label(&window, "pip Cache")
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
            found(&window, "pip Cache").accessible_checked(),
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
        found(&window, "pip Cache").invoke_accessible_default_action();
        assert_eq!(
            found(&window, "pip Cache").accessible_checked(),
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
        found(&window, "pip Cache").invoke_accessible_default_action();
        assert_eq!(
            found(&window, "pip Cache").accessible_checked(),
            Some(false),
            "the results answer again"
        );
        found(&window, "pip Cache").invoke_accessible_default_action();
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
}
