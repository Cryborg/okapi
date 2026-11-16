//! The administrator's rights: root's scan of the groups that need them,
//! run by the privileged helper, which unlocks those it offers. The helper
//! is asked for the targets of the rules, never a path to remove
//! (invariant 1). Every scan asks for every target and sets every group
//! again: the age and exclusions kept for root's cleaning are always those
//! of the one scan that unlocked what is checked.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use okapi_engine::{DEFAULT_MIN_AGE_MINUTES, Exclusions, Origin, ScanReport};
use okapi_helper::{
    HELPER_MIN_AGE_FLOOR, Operation, RefusalAnswer, Reply, ReplyError, admin_targets, request_line,
};
use okapi_rules::RuleSet;
use slint::ComponentHandle as _;

use super::{CANCEL_WAIT, CleanerScreen, Inputs, Running, Task, WATCH_EVERY};
use crate::elevation::{ElevationFailure, ElevationStep};
use crate::ui;

/// The age and exclusions root's scan unlocked the groups with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Granted {
    pub minutes: u64,
    pub exclusions: Vec<PathBuf>,
}

impl Granted {
    /// What the helper is sent for `targets`, from the settings `inputs`:
    /// the settings' age, never below the helper's floor; and the exclusions
    /// that may touch a target ([`Exclusions::may_touch`]), so that one
    /// elsewhere (a mount root cannot read) never makes it refuse. Telling
    /// which may touch one reads where each lies, which waits on a mount that
    /// stopped answering: call it from a task's thread, not the window's.
    pub fn of(inputs: &Inputs, targets: &BTreeSet<String>) -> Self {
        let floor = HELPER_MIN_AGE_FLOOR
            .as_secs()
            .div_ceil(Duration::from_mins(1).as_secs());
        let minutes = inputs
            .min_age_minutes
            .unwrap_or(DEFAULT_MIN_AGE_MINUTES)
            .max(floor);
        tracing::debug!(
            exclusions = inputs.exclusions.len(),
            "reading which exclusions may touch root's targets"
        );
        let started = Instant::now();
        // In order: settings that only reorder them are the same.
        let exclusions: Vec<PathBuf> = inputs
            .exclusions
            .iter()
            .filter(|exclusion| {
                targets
                    .iter()
                    .any(|target| Exclusions::may_touch(exclusion, Path::new(target)))
            })
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        tracing::info!(elapsed = ?started.elapsed(), kept = exclusions.len(), "exclusions for root read");
        Self {
            minutes,
            exclusions,
        }
    }

    /// The request line of `operation` on `targets`.
    pub fn line(&self, operation: Operation, targets: &BTreeSet<String>) -> Option<Vec<u8>> {
        let targets: Vec<String> = targets.iter().cloned().collect();
        request_line(operation, &targets, self.minutes, &self.exclusions)
            .inspect_err(
                |refusal| tracing::error!(%refusal, "a request the helper would refuse: not sent"),
            )
            .ok()
    }
}

/// The path each rule of `rules` the privileged helper may be asked for is
/// asked by.
pub(super) fn helper_targets(rules: &RuleSet) -> BTreeMap<Origin, String> {
    admin_targets(rules)
        .into_iter()
        .map(|(name, origin)| (origin, name))
        .collect()
}

#[cfg(test)]
impl CleanerScreen {
    /// The helper's targets of `rules` in the place of the embedded rules':
    /// a test's targets lie in its sandbox.
    pub(super) fn use_helper_targets_of(&self, rules: &RuleSet) {
        self.0.borrow_mut().helper_targets = helper_targets(rules);
    }
}

/// What root's scan gave, with the age and exclusions it was asked with.
pub(super) struct RootScan {
    granted: Granted,
    reply: Result<Reply, ElevationFailure>,
}

/// Root's scan under way: its request built on its thread, then the helper.
pub(super) struct Elevating {
    /// None when the request was not sent (traced where it was built).
    pub(super) running: Running<Option<RootScan>>,
    steps: Receiver<ElevationStep>,
    /// The report whose groups it unlocks.
    report: Arc<ScanReport>,
    /// The helper was launched, as its steps said: pkexec asks, or has
    /// asked. Never given up then.
    pub(super) launched: bool,
    /// None until it is cancelled before the helper was launched; then how
    /// long it has been waited for, counted in looks at it, up to
    /// [`CANCEL_WAIT`] (unless it is launched meanwhile).
    cancelled_for: Option<Duration>,
}

/// How the rights cancelled are shown: their question open (`launched`)
/// closes first; before it, the request is only withdrawn.
const fn cancelling(launched: bool) -> ui::RightsStep {
    if launched {
        ui::RightsStep::Cancelling
    } else {
        ui::RightsStep::Withdrawing
    }
}

/// Why the rights, or the administrator's part of a cleaning, gave nothing,
/// as the window says it.
pub(super) fn problem_of(failure: &ElevationFailure) -> ui::RightsProblem {
    match failure {
        ElevationFailure::Cancelled => ui::RightsProblem::Cancelled,
        ElevationFailure::NotInstalled => ui::RightsProblem::NotInstalled,
        ElevationFailure::Reply(ReplyError::NotAuthorized) => ui::RightsProblem::NotAuthorized,
        ElevationFailure::Reply(ReplyError::Dismissed) => ui::RightsProblem::Dismissed,
        ElevationFailure::Reply(ReplyError::OtherVersion { .. }) => ui::RightsProblem::Outdated,
        ElevationFailure::Unlaunchable { .. }
        | ElevationFailure::Unwatched { .. }
        | ElevationFailure::Reply(
            ReplyError::NotRoot
            | ReplyError::Panicked
            | ReplyError::Killed
            | ReplyError::UnexpectedCode { .. }
            | ReplyError::Unreadable { .. },
        ) => ui::RightsProblem::Failed,
    }
}

/// Whether the helper ran as root and ended without saying what it did in a
/// form this interface reads (another version's answer included): a
/// cleaning may then have removed entries nobody can list.
pub(super) const fn ran_to_an_unknown_end(failure: &ElevationFailure) -> bool {
    matches!(
        failure,
        ElevationFailure::Unwatched { .. }
            | ElevationFailure::Reply(
                ReplyError::Panicked
                    | ReplyError::Killed
                    | ReplyError::UnexpectedCode { .. }
                    | ReplyError::Unreadable { .. }
                    | ReplyError::OtherVersion { .. }
            )
    )
}

/// Why a refusal of the helper gave nothing: a helper of another version
/// does not know the request, or the targets of these rules.
pub(super) fn refusal_problem(refusal: &RefusalAnswer) -> ui::RightsProblem {
    if refusal.asks_to_reinstall() {
        ui::RightsProblem::Outdated
    } else {
        ui::RightsProblem::Failed
    }
}

/// The stable name of why the administrator's part was not done, for the
/// history.
pub(super) const fn problem_key(problem: ui::RightsProblem) -> &'static str {
    match problem {
        ui::RightsProblem::None => "none",
        ui::RightsProblem::Cancelled => "cancelled",
        ui::RightsProblem::Dismissed => "authDismissed",
        ui::RightsProblem::NotAuthorized => "notAuthorized",
        ui::RightsProblem::NotInstalled => "notInstalled",
        ui::RightsProblem::Outdated => "outdated",
        ui::RightsProblem::Failed => "failed",
        ui::RightsProblem::Stopped => "stopped",
        ui::RightsProblem::Changed => "changed",
        ui::RightsProblem::Unknown => "unknown",
    }
}

impl CleanerScreen {
    /// Asks for the rights: root's scan of the groups waiting for them.
    pub(super) fn obtain_rights(&self) {
        if self.is_settling("rights") || self.results_frozen("rights") {
            return;
        }
        let prepared = {
            let state = self.0.borrow();
            if state.task.is_some() {
                tracing::warn!("rights asked for while a task runs");
                return;
            }
            let Some(held) = &state.held else {
                tracing::warn!("rights asked for without results");
                return;
            };
            if held.results.waiting_for_rights() == 0 {
                tracing::warn!("rights asked for without a group waiting for them");
                return;
            }
            // Every target, unlocked ones too: the answer sets every group.
            let targets = held.results.helper_targets();
            (state.seams.read)().map(|inputs| (Arc::clone(&held.report), inputs, targets))
        };
        let (report, inputs, targets) = match prepared {
            Ok(prepared) => prepared,
            Err(found) => {
                tracing::warn!(problem = ?found, "rights cannot be asked for");
                self.with_window(|window, _| {
                    window
                        .global::<ui::Cleaner>()
                        .set_problem(super::problem(Some(found)));
                });
                return;
            }
        };
        let elevate = Arc::clone(&self.0.borrow().seams.elevate);
        let (step_sender, steps) = mpsc::channel();
        let started = Running::start(
            "okapi-rights",
            tracing::info_span!("rights"),
            move |cancel, _progress| {
                let granted = Granted::of(&inputs, &targets);
                let line = granted.line(Operation::Scan, &targets)?;
                tracing::info!(?targets, ?granted, "rights asked for: root's scan");
                let reply = elevate(Operation::Scan, line, &cancel, &step_sender);
                Some(RootScan { granted, reply })
            },
        );
        let running = match started {
            Ok(running) => running,
            Err(error) => {
                tracing::error!(%error, "rights thread not started");
                self.show_rights(ui::RightsStep::Idle, ui::RightsProblem::Failed);
                return;
            }
        };
        self.0.borrow_mut().task = Some(Task::Elevating(Elevating {
            running,
            steps,
            report,
            launched: false,
            cancelled_for: None,
        }));
        self.settle();
        self.show_rights(ui::RightsStep::Preparing, ui::RightsProblem::None);
        self.watch_task();
    }

    /// Cancels root's scan: its input closes once pkexec's question is
    /// answered. Not launched yet, the request is withdrawn: `Launcher::run`
    /// launches nothing once cancelled, and the thread is given up after a
    /// while if what it reads holds it. A launch already under way still
    /// shows its question (see `watch_elevation`).
    pub(super) fn cancel_rights(&self) {
        if self.is_settling("cancellation of the rights") {
            return;
        }
        let launched = {
            let mut state = self.0.borrow_mut();
            let Some(Task::Elevating(elevating)) = state.task.as_mut() else {
                return;
            };
            if !elevating.launched && elevating.cancelled_for.is_none() {
                elevating.cancelled_for = Some(Duration::ZERO);
            }
            elevating.running.cancel.cancel();
            elevating.launched
        };
        tracing::info!(launched, "rights cancelled by the user");
        self.show_rights(cancelling(launched), ui::RightsProblem::None);
    }

    /// Takes in where root's scan stands, then looks for its reply.
    pub(super) fn watch_elevation(&self) {
        let (step, received, given_up) = {
            let mut state = self.0.borrow_mut();
            let Some(Task::Elevating(elevating)) = state.task.as_mut() else {
                return;
            };
            // In the order sent: the last says where it stands.
            let step = elevating.steps.try_iter().last();
            elevating.launched |= step.is_some();
            if let Some(waited) = &mut elevating.cancelled_for {
                *waited += WATCH_EVERY;
            }
            let given_up = !elevating.launched
                && elevating
                    .cancelled_for
                    .is_some_and(|waited| waited >= CANCEL_WAIT);
            let cancelled = elevating.running.cancel.is_cancelled();
            (
                step.map(|step| (step, cancelled)),
                elevating.running.result.try_recv(),
                given_up,
            )
        };
        let shown = match step {
            // Launched after it was cancelled: pkexec asks all the same.
            Some((_, true)) => Some(cancelling(true)),
            Some((ElevationStep::Launched, false)) => Some(ui::RightsStep::Authenticating),
            Some((ElevationStep::Authenticated, false)) => Some(ui::RightsStep::Scanning),
            None => None,
        };
        if let Some(shown) = shown {
            self.show_rights(shown, ui::RightsProblem::None);
        }
        match received {
            Ok(scan) => self.end_elevation(scan),
            Err(TryRecvError::Empty) if given_up => {
                // Left to end alone: not launched in all that time, and
                // cancelled, `Launcher::run` no longer launches the helper.
                // Its input closes, should a launch already under way get
                // through: the helper cancels itself once pkexec's question
                // is answered.
                tracing::warn!(
                    looks_for = ?CANCEL_WAIT,
                    "rights cancelled before the helper was launched: given up"
                );
                if self.take_elevating().is_some() {
                    self.close_rights(problem_of(&ElevationFailure::Cancelled));
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                tracing::error!("rights thread ended without a reply");
                self.end_elevation(None);
            }
        }
    }

    /// Root's scan under way, taken: the screen no longer follows it.
    fn take_elevating(&self) -> Option<Elevating> {
        if !matches!(self.0.borrow().task, Some(Task::Elevating(_))) {
            tracing::error!("rights ended that were not asked for");
            return None;
        }
        let Some(Task::Elevating(elevating)) = self.0.borrow_mut().task.take() else {
            return None;
        };
        self.0.borrow().timer.stop();
        Some(elevating)
    }

    /// Takes in root's `scan`: the groups it offers are unlocked, if the
    /// report it was asked for is still the one held.
    fn end_elevation(&self, scan: Option<RootScan>) {
        let Some(elevating) = self.take_elevating() else {
            return;
        };
        let cancelled = elevating.running.cancel.is_cancelled();
        let problem = {
            let mut state = self.0.borrow_mut();
            let held = state
                .held
                .as_mut()
                .filter(|held| Arc::ptr_eq(&held.report, &elevating.report));
            match held {
                None => {
                    tracing::error!("root's scan of a report no longer held: nothing unlocked");
                    ui::RightsProblem::Failed
                }
                Some(held) => apply(held, scan, cancelled),
            }
        };
        self.close_rights(problem);
    }

    /// Shows the rights over, saying `problem`, and the results as they are.
    fn close_rights(&self, problem: ui::RightsProblem) {
        self.settle();
        self.show_rights(ui::RightsStep::Idle, problem);
        self.show_results();
    }

    /// Shows where the rights stand.
    fn show_rights(&self, step: ui::RightsStep, problem: ui::RightsProblem) {
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_rights_step(step);
            cleaner.set_rights_problem(problem);
        });
    }
}

/// Takes root's `scan` into `held`: a complete answer sets every group that
/// needs the administrator again, those it offers unlocked, and keeps the
/// age and exclusions it was asked with for root's cleaning; anything else
/// changes nothing. None: the thread died, or the request was not sent.
/// Gives what to say.
fn apply(held: &mut super::Held, scan: Option<RootScan>, cancelled: bool) -> ui::RightsProblem {
    let Some(RootScan { granted, reply }) = scan else {
        return ui::RightsProblem::Failed;
    };
    match reply {
        Err(failure) => {
            tracing::warn!(?failure, "rights not obtained");
            problem_of(&failure)
        }
        Ok(Reply::Refused(refusal)) => {
            tracing::warn!(refused = refusal.refused(), helper = refusal.helper_version(), target = ?refusal.target(), "root's scan refused");
            refusal_problem(&refusal)
        }
        Ok(Reply::Answer(answer)) if !answer.stops().is_empty() => {
            tracing::warn!(stops = ?answer.stops(), cancelled, "root's scan stopped: nothing unlocked");
            if cancelled {
                ui::RightsProblem::None
            } else {
                ui::RightsProblem::Stopped
            }
        }
        Ok(Reply::Answer(answer)) => {
            let offered: Vec<_> = answer
                .groups()
                .iter()
                .map(|group| {
                    (
                        group.targets().iter().cloned().collect::<BTreeSet<_>>(),
                        group.entries(),
                        group.bytes(),
                    )
                })
                .collect();
            let troubled = answer
                .issues()
                .iter()
                .flat_map(|issue| issue.targets().iter().cloned())
                .collect();
            held.results.relock();
            let unlocked = held.results.unlock(&offered, &troubled);
            tracing::info!(
                unlocked,
                offered = offered.len(),
                ?granted,
                "rights obtained"
            );
            held.granted = Some(granted);
            ui::RightsProblem::None
        }
    }
}

#[cfg(test)]
mod tests {
    use okapi_helper::Refusal;

    use super::*;

    fn inputs(minutes: Option<u64>, exclusions: &[&str]) -> Inputs {
        Inputs {
            variables: okapi_rules::TemplateVariables::new(),
            min_age_minutes: minutes,
            exclusions: exclusions.iter().map(PathBuf::from).collect(),
            deletion_log: false,
            state_directory: None,
        }
    }

    fn targets(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_helper_is_sent_the_settings_age_never_below_its_floor() {
        let crash = targets(&["/var/crash"]);
        assert_eq!(Granted::of(&inputs(Some(10), &[]), &crash).minutes, 60);
        assert_eq!(Granted::of(&inputs(Some(90), &[]), &crash).minutes, 90);
        assert_eq!(Granted::of(&inputs(None, &[]), &crash).minutes, 60);
    }

    #[test]
    fn the_helper_is_sent_only_the_exclusions_that_may_touch_its_targets() {
        let granted = Granted::of(
            &inputs(
                None,
                &[
                    "*.log",
                    "/var/crash/keep",
                    "/var",
                    "/run/user/1000/gvfs",
                    "/var/crashes",
                    "/home/ana/keep",
                ],
            ),
            &targets(&["/var/crash", "/var/cache/apt/archives"]),
        );
        assert_eq!(
            granted.exclusions,
            ["/var", "/var/crash/keep", "*.log"].map(PathBuf::from),
            "above a target, below one, an extension, in order; never a path beside them"
        );
    }

    #[test]
    fn a_refusal_of_another_helper_asks_to_install_it_again() {
        let refused = |refusal| refusal_problem(&RefusalAnswer::of(&refusal));
        assert_eq!(
            refused(Refusal::UnsupportedVersion { version: 2 }),
            ui::RightsProblem::Outdated
        );
        assert_eq!(
            refused(Refusal::UnknownTarget {
                target: "/var/crash".to_owned()
            }),
            ui::RightsProblem::Outdated
        );
        assert_eq!(refused(Refusal::NoTarget), ui::RightsProblem::Failed);
        assert_eq!(
            problem_of(&ElevationFailure::Reply(ReplyError::Dismissed)),
            ui::RightsProblem::Dismissed
        );
        assert_eq!(
            problem_of(&ElevationFailure::Reply(ReplyError::OtherVersion {
                version: 1
            })),
            ui::RightsProblem::Outdated
        );
    }
}
