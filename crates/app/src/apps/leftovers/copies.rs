//! The copies snapd keeps of snaps removed, in the leftovers' tab
//! (`okapi_tools::apps::snapshots`): shown with the leftovers, each
//! forgotten by snapd on a thread once confirmed, by an identifier of the
//! report shown only (invariant 1); the tab read again after (nothing is
//! asked while folders are checked, which would hold the reading back).

use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use okapi_store::SeenApps;
use okapi_tools::apps::Source;
use okapi_tools::apps::snapshots::{Copies, CopyId, CopyReport, Forgot};
use okapi_tools::apps::uninstall::Why;
use slint::VecModel;

use super::{AppsScreen, shown_name};
use crate::shown::size;
use crate::ui;

/// A copy being forgotten, and what the outcome will name.
pub(super) struct Forgetting {
    receiver: Receiver<Forgot>,
    name: String,
    bytes: u64,
}

/// A copy asked to be forgotten, kept as the window names it: whatever
/// becomes of the report shown, the yes forgets this one.
pub(super) struct Asked {
    report: Arc<CopyReport>,
    id: CopyId,
    name: String,
    bytes: u64,
}

/// What the last forgetting did, until closed.
struct Told {
    outcome: ui::ForgetOutcome,
    name: String,
    bytes: u64,
}

/// The copies' part of the tab's state.
#[derive(Default)]
pub(super) struct CopiesState {
    report: Option<Arc<CopyReport>>,
    /// They could not be read: none is proposed.
    unread: bool,
    /// The copy asked to be forgotten, waiting for the yes.
    pub(super) asked: Option<Asked>,
    pub(super) forgetting: Option<Forgetting>,
    told: Option<Told>,
    pub(super) model: Rc<VecModel<ui::CopyLine>>,
}

impl CopiesState {
    /// What an analysis read of them.
    pub(super) fn analysed(&mut self, copies: Copies) {
        (self.report, self.unread) = match copies {
            Copies::None => (None, false),
            Copies::Found(report) => (Some(Arc::new(report)), false),
            Copies::Unread(why) => {
                tracing::info!(why, "snapd's copies not shown");
                (None, true)
            }
        };
    }

    /// Shows them on `leftovers`, named as `seen` knows them, dated from
    /// `now` in `zone`.
    pub(super) fn show(
        &self,
        leftovers: &ui::Leftovers<'_>,
        seen: &SeenApps,
        now: Timestamp,
        zone: &TimeZone,
    ) {
        let lines: Vec<ui::CopyLine> = self
            .report
            .iter()
            .flat_map(|report| report.copies())
            .map(|copy| ui::CopyLine {
                name: shown_name(seen, Source::Snap, copy.snap()).0.into(),
                made: crate::dashboard::moment(copy.made().as_second(), now, zone)
                    .unwrap_or_default(),
                size: size(copy.bytes()),
            })
            .collect();
        crate::shown::show(&self.model, lines);
        leftovers.set_copies_unread(self.unread);
        leftovers.set_forget_asking(self.asked.is_some());
        if let Some(asked) = &self.asked {
            leftovers.set_forget_name(asked.name.as_str().into());
            leftovers.set_forget_size(size(asked.bytes));
        }
        leftovers.set_forgetting(self.forgetting.is_some());
        match &self.told {
            Some(told) => {
                leftovers.set_forgot(told.outcome);
                leftovers.set_forgot_name(told.name.as_str().into());
                leftovers.set_forgot_size(size(told.bytes));
            }
            None => leftovers.set_forgot(ui::ForgetOutcome::None),
        }
    }
}

/// What the screen says of `forgot`.
fn outcome(forgot: &Forgot) -> ui::ForgetOutcome {
    match forgot {
        Forgot::Forgotten => ui::ForgetOutcome::Forgotten,
        Forgot::Gone => ui::ForgetOutcome::Gone,
        Forgot::Reinstalled => ui::ForgetOutcome::Reinstalled,
        Forgot::Failed(Why::NotAuthorized) => ui::ForgetOutcome::Refused,
        Forgot::Failed(Why::Busy) => ui::ForgetOutcome::Busy,
        Forgot::Failed(Why::TimedOut | Why::Unanswered) => ui::ForgetOutcome::Unknown,
        Forgot::Failed(
            Why::Absent | Why::NotFound | Why::Unclear | Why::StillOpen | Why::Service(_),
        ) => ui::ForgetOutcome::Failed,
    }
}

impl AppsScreen {
    /// Asks the user whether to forget the copy `index` shown.
    pub(super) fn ask_forget(&self, index: usize) {
        {
            let mut state = self.0.borrow_mut();
            let retiring = state.old_versions.retiring();
            let leftovers = &mut state.leftovers;
            let shown = leftovers.copies.report.as_ref().and_then(|report| {
                let copy = report.copies().get(index)?;
                Some(Asked {
                    report: Arc::clone(report),
                    id: copy.id(),
                    name: shown_name(&leftovers.seen, Source::Snap, copy.snap()).0,
                    bytes: copy.bytes(),
                })
            });
            if shown.is_none() {
                tracing::info!(index, "a copy out of the list refused");
            } else if leftovers.busy()
                || retiring
                || leftovers.confirming
                || leftovers.copies.asked.is_some()
                || !leftovers.checked.is_empty()
            {
                tracing::info!(index, "a copy to forget while another action runs: refused");
            } else {
                leftovers.copies.asked = shown;
            }
        }
        self.show_leftovers();
    }

    pub(super) fn cancel_forget(&self) {
        self.0.borrow_mut().leftovers.copies.asked = None;
        self.show_leftovers();
        // An analysis owed meanwhile is made once free.
    }

    /// Asks snapd, on a thread, to forget the copy asked: read again there
    /// first.
    pub(super) fn confirm_forget(&self) {
        let asked = {
            let mut state = self.0.borrow_mut();
            let leftovers = &mut state.leftovers;
            let asked = leftovers.copies.asked.take();
            asked.filter(|_| !leftovers.busy())
        };
        let Some(Asked {
            report,
            id,
            name,
            bytes,
        }) = asked
        else {
            tracing::warn!(
                "a copy to forget confirmed with none asked, or while another action runs: refused"
            );
            self.show_leftovers();
            return;
        };
        tracing::info!(?id, "forgetting a copy of snapd confirmed");
        let forget = Arc::clone(&self.0.borrow().seams.leftovers.forget);
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("okapi-snap-copy".to_owned())
            .spawn(move || {
                // The screen gone: snapd ends it all the same, traced.
                let _ = sender.send(forget(&report, id));
            });
        {
            let mut state = self.0.borrow_mut();
            let copies = &mut state.leftovers.copies;
            copies.told = None;
            match spawned {
                Ok(_) => {
                    copies.forgetting = Some(Forgetting {
                        receiver,
                        name,
                        bytes,
                    });
                }
                Err(error) => {
                    tracing::error!(%error, "no thread to forget a copy: nothing asked");
                    copies.told = Some(Told {
                        outcome: ui::ForgetOutcome::Failed,
                        name,
                        bytes,
                    });
                }
            }
        }
        self.show_leftovers();
        self.watch();
    }

    /// What the forgetting said, if it ended: told, then the tab read again.
    pub(super) fn take_forgotten(&self) {
        let ended = {
            let state = self.0.borrow();
            match state
                .leftovers
                .copies
                .forgetting
                .as_ref()
                .map(|forgetting| forgetting.receiver.try_recv())
            {
                Some(Ok(forgot)) => Some(outcome(&forgot)),
                Some(Err(TryRecvError::Disconnected)) => {
                    tracing::error!("the thread forgetting a copy ended without a word");
                    Some(ui::ForgetOutcome::Failed)
                }
                Some(Err(TryRecvError::Empty)) | None => None,
            }
        };
        let Some(said) = ended else { return };
        {
            let mut state = self.0.borrow_mut();
            let leftovers = &mut state.leftovers;
            if let Some(forgetting) = leftovers.copies.forgetting.take() {
                tracing::info!(outcome = ?said, "a copy's forgetting told");
                leftovers.copies.told = Some(Told {
                    outcome: said,
                    name: forgetting.name,
                    bytes: forgetting.bytes,
                });
            }
            // What snapd keeps changed: read again.
            leftovers.again = true;
        }
        self.show_leftovers();
    }

    pub(super) fn close_forgot(&self) {
        self.0.borrow_mut().leftovers.copies.told = None;
        self.show_leftovers();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_outcome_of_a_forgetting_is_told_in_its_own_words() {
        // Catches a refusal told as a failure, snapd's silence told as a
        // refusal, and a busy snapd told as broken.
        for (forgot, told) in [
            (Forgot::Forgotten, ui::ForgetOutcome::Forgotten),
            (Forgot::Gone, ui::ForgetOutcome::Gone),
            (Forgot::Reinstalled, ui::ForgetOutcome::Reinstalled),
            (
                Forgot::Failed(Why::NotAuthorized),
                ui::ForgetOutcome::Refused,
            ),
            (Forgot::Failed(Why::Busy), ui::ForgetOutcome::Busy),
            (Forgot::Failed(Why::TimedOut), ui::ForgetOutcome::Unknown),
            (Forgot::Failed(Why::Unanswered), ui::ForgetOutcome::Unknown),
            (
                Forgot::Failed(Why::Service("cannot".to_owned())),
                ui::ForgetOutcome::Failed,
            ),
        ] {
            assert_eq!(outcome(&forgot), told, "{forgot:?}");
        }
    }
}
