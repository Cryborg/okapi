//! Installing from the updates screen (`okapi_tools::updates::install`):
//! the system's updates or some snaps, on a thread of their own whose
//! progress the window reads at each tick; what the system's update
//! would remove is shown and accepted first. The window only ever names a
//! line it showed (all the system, a snap's line): what is asked is
//! resolved here from the last reading, never sent as text. Once it ends,
//! the list is read again, and the outcome told from it.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use okapi_tools::updates::install::{
    self, End, Ended, Phase, Progress, Simulated, SystemInstall, Why,
};
use okapi_tools::updates::{Kind, SnapUpdate, SystemUpdate};
use slint::{ComponentHandle as _, SharedString};

use super::{UpdatesScreen, spawn, told};
use crate::disk_usage::READ_EVERY;
use crate::shown::{show, shown, size};
use crate::ui;

/// How many names still waiting the outcome says before « et N autres ».
const LEFT_NAMES_FIRST: usize = 6;
/// How many packages removed the confirmation lists before « et N
/// autres »: it must fit the window.
const REMOVED_LINES_FIRST: usize = 8;

/// What an installation updates: what was asked, as it was read.
#[derive(Debug, Clone)]
pub(super) enum Target {
    System(Vec<SystemUpdate>),
    Snaps(Vec<SnapUpdate>),
}

impl Target {
    const fn shown(&self) -> ui::InstallTarget {
        match self {
            Self::System(_) => ui::InstallTarget::System,
            Self::Snaps(_) => ui::InstallTarget::Snaps,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::System(updates) => updates.len(),
            Self::Snaps(snaps) => snaps.len(),
        }
    }
}

/// Where an installation stands.
#[derive(Default)]
pub(super) enum Install {
    #[default]
    Idle,
    Running(Running),
    /// What the system's update would remove, shown, awaits the user.
    Confirm {
        asked: Vec<SystemUpdate>,
        simulated: Simulated,
    },
    /// It ended; the list is read again (the one search it lets through)
    /// before its outcome is told.
    Verifying {
        target: Target,
        ended: Ended,
    },
}

impl Install {
    /// Whether one is under way, awaits the user or is read again: no
    /// other may start.
    pub(super) const fn active(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// Whether it keeps a search from starting: only under way or awaiting
    /// the user (the reading again that ends it is a search).
    pub(super) const fn blocks_search(&self) -> bool {
        matches!(self, Self::Running(_) | Self::Confirm { .. })
    }
}

pub(super) struct Running {
    target: Target,
    events: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    /// It could be cancelled once: no longer, it is too late.
    was_cancellable: bool,
    /// It follows a confirmation: what it removes may have changed since.
    confirmed: bool,
    /// dpkg was said to run: what it did is on the disk, whatever comes.
    in_dpkg: bool,
}

/// What an installation's thread tells.
#[derive(Debug)]
enum Event {
    Progress(Progress),
    System(SystemInstall),
    Snaps(Ended),
}

/// What was installed and what still waits, by the list read again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) installed: usize,
    /// Asked and still waiting, as shown (a package's name, a snap's
    /// title).
    pub(super) left: Vec<String>,
}

/// What the outcome tells.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Outcome {
    pub(super) end: ui::InstallEnd,
    /// Why it failed, when it did.
    pub(super) why: Option<Why>,
    /// None when they cannot be known.
    pub(super) counts: Option<Counts>,
}

/// What the outcome tells, from what was asked, how it ended and the list
/// read again (`None`: not read). Once dpkg ran, a failure or a
/// cancellation says no count: what it unpacked is no longer listed, yet
/// not configured. Before it, nothing was installed. PackageKit done says
/// every update asked installed, the list read again only finds the ones
/// still waiting. Snaps are counted by the list read again, which decides:
/// snapd undoes a snap that failed, which waits still.
pub(super) fn outcome(
    target: &Target,
    ended: &Ended,
    system: Option<&[SystemUpdate]>,
    snaps: Option<&[SnapUpdate]>,
) -> Outcome {
    let why = match ended.end {
        End::Failed(why) => Some(why),
        End::Done | End::Cancelled => None,
    };
    let told = |end, counts| Outcome { end, why, counts };
    let counted = |asked: usize, left: Vec<String>| Counts {
        installed: asked - left.len(),
        left,
    };
    match (target, ended.end) {
        (Target::System(_), End::Failed(_) | End::Cancelled) if ended.reached_dpkg => {
            told(ui::InstallEnd::Stopped, None)
        }
        (Target::System(_), End::Cancelled) => told(ui::InstallEnd::Cancelled, None),
        (Target::System(_), End::Failed(_)) => told(ui::InstallEnd::Failed, None),
        // Done: every update asked installed; one listed again waits in a
        // newer version come since, or held back.
        (Target::System(asked), End::Done) => told(
            ui::InstallEnd::Done,
            Some(Counts {
                installed: asked.len(),
                left: system.map_or_else(Vec::new, |read| install::system_left(asked, read)),
            }),
        ),
        (Target::Snaps(asked), end) => match snaps {
            Some(read) => {
                let left: Vec<String> = asked
                    .iter()
                    .filter(|snap| read.iter().any(|waiting| waiting.name == snap.name))
                    .map(|snap| snap.title.clone())
                    .collect();
                // Done by snapd with some still waiting: done, the rest said
                // waiting, never failed without a cause.
                let end = if left.is_empty() || end == End::Done {
                    ui::InstallEnd::Done
                } else {
                    ui::InstallEnd::Failed
                };
                if end == ui::InstallEnd::Done && !left.is_empty() {
                    tracing::warn!(?left, "snapd said done, yet these still wait");
                }
                // Done by the list read again: its failure said nothing true.
                let why = why.filter(|_| end == ui::InstallEnd::Failed);
                Outcome {
                    end,
                    why,
                    counts: Some(counted(asked.len(), left)),
                }
            }
            None if end == End::Done => told(ui::InstallEnd::Done, None),
            None => told(ui::InstallEnd::Failed, None),
        },
    }
}

impl UpdatesScreen {
    /// Installs every update of the system the last reading shows, held
    /// back aside.
    pub(super) fn update_system(&self) {
        let asked: Vec<SystemUpdate> = self
            .0
            .borrow()
            .system
            .iter()
            .filter(|update| update.kind != Kind::Blocked)
            .cloned()
            .collect();
        if asked.is_empty() {
            tracing::info!("no update of the system waits: nothing asked");
            return;
        }
        if self.may_install() {
            self.start_system(asked, BTreeSet::new(), false);
        }
    }

    /// Refreshes the snap of line `index` of the last reading.
    pub(super) fn update_snap(&self, index: i32) {
        let snap = usize::try_from(index).ok().and_then(|index| {
            self.0
                .borrow()
                .snaps
                .as_ref()
                .and_then(|snaps| snaps.get(index).cloned())
        });
        let Some(snap) = snap else {
            tracing::error!(index, "no snap at this line: nothing asked");
            return;
        };
        if self.may_install() {
            self.start_snaps(vec![snap]);
        }
    }

    /// Refreshes every snap of the last reading.
    pub(super) fn update_snaps(&self) {
        let snaps = self.0.borrow().snaps.clone().unwrap_or_default();
        if snaps.is_empty() {
            tracing::info!("no snap waits: nothing asked");
            return;
        }
        if self.may_install() {
            self.start_snaps(snaps);
        }
    }

    /// What the update removes, shown, is accepted: simulated again, and
    /// installed if it removes nothing more.
    pub(super) fn confirm_update(&self) {
        let install = std::mem::take(&mut self.0.borrow_mut().install);
        let Install::Confirm { asked, simulated } = install else {
            tracing::warn!("a confirmation with nothing to confirm: ignored");
            self.0.borrow_mut().install = install;
            return;
        };
        tracing::info!(removed = ?simulated.removed_keys(), "what the update removes accepted");
        self.start_system(asked, simulated.removed_keys(), true);
    }

    /// Closes the confirmation, or asks the installation to stop if it
    /// still can.
    pub(super) fn cancel_update(&self) {
        let mut state = self.0.borrow_mut();
        match &state.install {
            Install::Confirm { .. } => {
                tracing::info!("what the update removes refused: nothing installed");
                state.install = Install::Idle;
                drop(state);
                self.with_window(|window| {
                    window
                        .global::<ui::Updates>()
                        .set_install_step(ui::InstallStep::Idle);
                });
            }
            Install::Running(running) => {
                tracing::info!("the update's cancel asked");
                running.cancel.store(true, Ordering::Relaxed);
                drop(state);
                self.with_window(|window| {
                    window
                        .global::<ui::Updates>()
                        .set_install_cancel_asked(true);
                });
            }
            Install::Idle | Install::Verifying { .. } => {
                tracing::warn!("a cancel with nothing to cancel: ignored");
            }
        }
    }

    pub(super) fn close_result(&self) {
        self.with_window(|window| window.global::<ui::Updates>().set_result_shown(false));
    }

    /// The last installation again, on what still waits.
    pub(super) fn retry_update(&self) {
        let retry = self.0.borrow().retry.clone();
        match retry {
            Some(Target::System(_)) => self.update_system(),
            Some(Target::Snaps(asked)) => {
                let waiting: Vec<SnapUpdate> = self
                    .0
                    .borrow()
                    .snaps
                    .iter()
                    .flatten()
                    .filter(|snap| asked.iter().any(|tried| tried.name == snap.name))
                    .cloned()
                    .collect();
                if waiting.is_empty() {
                    tracing::info!("none of the snaps tried waits still: nothing asked");
                } else if self.may_install() {
                    self.start_snaps(waiting);
                }
            }
            None => tracing::warn!("nothing installed yet to try again"),
        }
    }

    /// Whether an installation may start: no other, no search.
    fn may_install(&self) -> bool {
        let state = self.0.borrow();
        let free = state.search.is_none() && !state.install.active();
        if !free {
            tracing::warn!("an installation asked while another or a search runs: refused");
        }
        free
    }

    fn start_system(&self, asked: Vec<SystemUpdate>, accepted: BTreeSet<String>, confirmed: bool) {
        let ids: Vec<String> = asked.iter().map(|update| update.id.clone()).collect();
        let installer = Arc::clone(&self.0.borrow().seams.installer);
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, events) = mpsc::channel();
        let started = spawn("updates-install", {
            let cancel = Arc::clone(&cancel);
            move || {
                let heard = |progress| told(&sender, Event::Progress(progress));
                let outcome =
                    install::install_system(&*installer, &ids, &accepted, &heard, &cancel);
                told(&sender, Event::System(outcome));
            }
        });
        self.started(Target::System(asked), events, cancel, started, confirmed);
    }

    fn start_snaps(&self, asked: Vec<SnapUpdate>) {
        let names: Vec<String> = asked.iter().map(|snap| snap.name.clone()).collect();
        let installer = Arc::clone(&self.0.borrow().seams.installer);
        let (sender, events) = mpsc::channel();
        let started = spawn("updates-install", move || {
            let heard = |progress| told(&sender, Event::Progress(progress));
            told(
                &sender,
                Event::Snaps(install::install_snaps(&*installer, &names, &heard)),
            );
        });
        self.started(
            Target::Snaps(asked),
            events,
            Arc::new(AtomicBool::new(false)),
            started,
            false,
        );
    }

    fn started(
        &self,
        target: Target,
        events: Receiver<Event>,
        cancel: Arc<AtomicBool>,
        started: bool,
        confirmed: bool,
    ) {
        let name: SharedString = match &target {
            Target::Snaps(snaps) if snaps.len() == 1 => snaps[0].title.as_str().into(),
            Target::System(_) | Target::Snaps(_) => SharedString::new(),
        };
        let shown_target = target.shown();
        self.0.borrow_mut().retry = Some(target.clone());
        if !started {
            self.ended(target, Ended::before(End::Failed(Why::Broken)));
            return;
        }
        self.0.borrow_mut().install = Install::Running(Running {
            target,
            events,
            cancel,
            was_cancellable: false,
            confirmed,
            in_dpkg: false,
        });
        self.with_window(|window| {
            let updates = window.global::<ui::Updates>();
            updates.set_result_shown(false);
            updates.set_install_target(shown_target);
            updates.set_install_name(name);
            updates.set_install_cancel_asked(false);
            updates.set_install_too_late(false);
            // The confirmation, if any, closes: what it accepted runs.
            updates.set_install_step(ui::InstallStep::Running);
        });
        self.show_progress(&Progress::preparing());
        self.watch_install();
    }

    fn watch_install(&self) {
        let weak = std::rc::Rc::downgrade(&self.0);
        self.0
            .borrow()
            .install_timer
            .start(slint::TimerMode::Repeated, READ_EVERY, move || {
                if let Some(state) = weak.upgrade() {
                    UpdatesScreen(state).take_install();
                }
            });
    }

    /// Shows what the installation's thread told; a thread gone without
    /// its end failed.
    fn take_install(&self) {
        loop {
            let event = {
                let state = self.0.borrow();
                let Install::Running(running) = &state.install else {
                    state.install_timer.stop();
                    return;
                };
                running.events.try_recv()
            };
            match event {
                Ok(Event::Progress(progress)) => self.show_progress(&progress),
                Ok(Event::System(SystemInstall::Confirm(simulated))) => {
                    if let Some(running) = self.stop_running() {
                        self.confirm(running, simulated);
                    }
                    return;
                }
                Ok(Event::System(SystemInstall::Ended(ended)) | Event::Snaps(ended)) => {
                    if let Some(running) = self.stop_running() {
                        self.ended(running.target, ended);
                    }
                    return;
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    if let Some(running) = self.stop_running() {
                        tracing::error!(
                            target = ?running.target.shown(),
                            asked = running.target.len(),
                            in_dpkg = running.in_dpkg,
                            "the installation's thread ended without its end"
                        );
                        // Once dpkg ran, it may be finishing without Okapi.
                        let ended = if running.in_dpkg {
                            Ended {
                                reached_dpkg: true,
                                ..Ended::before(End::Failed(Why::Unanswered))
                            }
                        } else {
                            Ended::before(End::Failed(Why::Broken))
                        };
                        self.ended(running.target, ended);
                    }
                    return;
                }
            }
        }
    }

    /// The installation running, followed no longer.
    fn stop_running(&self) -> Option<Running> {
        let mut state = self.0.borrow_mut();
        state.install_timer.stop();
        match std::mem::take(&mut state.install) {
            Install::Running(running) => Some(running),
            other => {
                state.install = other;
                None
            }
        }
    }

    /// What the update would remove is shown, to be accepted or not.
    fn confirm(&self, running: Running, simulated: Simulated) {
        let Target::System(asked) = running.target else {
            tracing::error!("a confirmation for snaps: none shown");
            self.ended(running.target, Ended::before(End::Failed(Why::Broken)));
            return;
        };
        let removed_more = shown(simulated.removed.len().saturating_sub(REMOVED_LINES_FIRST));
        let removed_count = shown(simulated.removed.len());
        let removed: Vec<ui::RemovedLine> = simulated
            .removed
            .iter()
            .take(REMOVED_LINES_FIRST)
            .map(|affected| ui::RemovedLine {
                name: affected.name.as_str().into(),
                summary: affected.summary.as_str().into(),
            })
            .collect();
        let added = shown(simulated.added.len());
        self.0.borrow_mut().install = Install::Confirm { asked, simulated };
        let model = std::rc::Rc::clone(&self.0.borrow().removed_model);
        show(&model, removed);
        self.with_window(|window| {
            let updates = window.global::<ui::Updates>();
            updates.set_added(added);
            updates.set_removed_count(removed_count);
            updates.set_removed_more(removed_more);
            updates.set_removals_changed(running.confirmed);
            updates.set_install_step(ui::InstallStep::Confirm);
        });
    }

    fn show_progress(&self, progress: &Progress) {
        let too_late = {
            let mut state = self.0.borrow_mut();
            match &mut state.install {
                Install::Running(running) => {
                    running.was_cancellable |= progress.cancellable;
                    running.in_dpkg |= progress.in_dpkg;
                    // A cancel taken says « annulation… » first; one refused
                    // or late never does.
                    running.was_cancellable
                        && !progress.cancellable
                        && progress.phase != Phase::Cancelling
                }
                _ => false,
            }
        };
        self.with_window(|window| {
            let updates = window.global::<ui::Updates>();
            updates.set_install_phase(match progress.phase {
                Phase::Preparing => ui::InstallPhase::Preparing,
                Phase::Waiting => ui::InstallPhase::Waiting,
                Phase::Authorizing => ui::InstallPhase::Authorizing,
                Phase::Downloading => ui::InstallPhase::Downloading,
                Phase::Installing => ui::InstallPhase::Installing,
                Phase::Cancelling => ui::InstallPhase::Cancelling,
            });
            updates.set_install_has_percentage(progress.percentage.is_some());
            updates.set_install_percentage(shown(
                progress
                    .percentage
                    .and_then(|percentage| usize::try_from(percentage).ok())
                    .unwrap_or_default(),
            ));
            updates.set_install_package(progress.package.clone().unwrap_or_default().into());
            updates.set_install_done(shown(progress.done));
            updates.set_install_total(shown(progress.total));
            updates.set_install_cancellable(progress.cancellable);
            updates.set_install_too_late(too_late);
            updates.set_install_has_bytes(progress.bytes.is_some());
            if let Some((done, total)) = progress.bytes {
                updates.set_install_bytes_done(size(done));
                updates.set_install_bytes_total(size(total));
            }
        });
    }

    /// The installation ended: the list is read again, then the outcome
    /// told.
    fn ended(&self, target: Target, ended: Ended) {
        tracing::info!(?ended, "installation ended: the list read again");
        self.0.borrow_mut().install = Install::Verifying { target, ended };
        self.with_window(|window| {
            window
                .global::<ui::Updates>()
                .set_install_step(ui::InstallStep::Verifying);
        });
        self.search(false);
    }

    /// Tells the outcome of the installation whose list was just read
    /// again.
    pub(super) fn tell_outcome(&self) {
        let install = std::mem::take(&mut self.0.borrow_mut().install);
        let Install::Verifying { target, ended } = install else {
            self.0.borrow_mut().install = install;
            return;
        };
        let told = {
            let state = self.0.borrow();
            outcome(
                &target,
                &ended,
                (state.system_reading == ui::SourceState::Read).then_some(state.system.as_slice()),
                state.snaps.as_deref(),
            )
        };
        let retriable = retriable(
            &target,
            &told,
            &self.0.borrow().system,
            self.0.borrow().snaps.as_deref(),
        );
        tracing::info!(
            ?told,
            verified = told.counts.is_some(),
            retriable,
            "installation's outcome"
        );
        let (counted, installed, left) = told.counts.map_or((false, 0, Vec::new()), |counts| {
            (true, counts.installed, counts.left)
        });
        let first_names = left
            .iter()
            .take(LEFT_NAMES_FIRST)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        self.with_window(|window| {
            let updates = window.global::<ui::Updates>();
            updates.set_install_step(ui::InstallStep::Idle);
            updates.set_result_end(told.end);
            updates.set_result_target(target.shown());
            updates.set_result_why(told.why.map_or(ui::InstallWhy::None, why_shown));
            updates.set_result_retriable(retriable);
            updates.set_result_counted(counted);
            updates.set_result_installed(shown(installed));
            updates.set_result_left(shown(left.len()));
            updates.set_result_left_names(first_names.into());
            updates.set_result_left_more(shown(left.len().saturating_sub(LEFT_NAMES_FIRST)));
            updates.set_result_restart(ended.system_restart);
            updates.set_result_unshown(ended.unshown_removals.join(", ").into());
            updates.set_result_unchecked(ended.removals_unchecked);
            updates.set_result_shown(true);
        });
    }
}

/// Whether trying again has a purpose: something of `target` still waits in
/// the last reading, and its failure is not one that only the user can
/// mend (dpkg cut short, an unsigned package).
pub(super) fn retriable(
    target: &Target,
    told: &Outcome,
    system: &[SystemUpdate],
    snaps: Option<&[SnapUpdate]>,
) -> bool {
    if matches!(told.why, Some(Why::Interrupted | Why::Unsigned))
        || told.end == ui::InstallEnd::Stopped
    {
        return false;
    }
    // Done: only what was asked and still waits is worth trying again.
    if told.end == ui::InstallEnd::Done {
        return told
            .counts
            .as_ref()
            .is_some_and(|counts| !counts.left.is_empty());
    }
    match target {
        Target::System(_) => system.iter().any(|update| update.kind != Kind::Blocked),
        Target::Snaps(asked) => snaps.is_some_and(|read| {
            read.iter()
                .any(|waiting| asked.iter().any(|tried| tried.name == waiting.name))
        }),
    }
}

pub(super) const fn why_shown(why: Why) -> ui::InstallWhy {
    match why {
        Why::Busy => ui::InstallWhy::Busy,
        Why::Interrupted => ui::InstallWhy::Interrupted,
        Why::Offline => ui::InstallWhy::Offline,
        Why::NoSpace => ui::InstallWhy::NoSpace,
        Why::Refused => ui::InstallWhy::Refused,
        Why::Changed => ui::InstallWhy::Changed,
        Why::AppsOpen => ui::InstallWhy::AppsOpen,
        Why::Unsigned => ui::InstallWhy::Unsigned,
        Why::Unanswered => ui::InstallWhy::Unanswered,
        Why::Broken => ui::InstallWhy::Broken,
    }
}
