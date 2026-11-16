//! A search on a thread of its own, as every tool that searches the user's
//! folders runs one: it ends once, complete with its report, cancelled or
//! failed; a cancellation that comes first keeps its outcome and drops the
//! report; a panic fails it; a search no one looks at any more is stopped.

use std::fmt;
use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use crate::panic_message;

/// How a search ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    /// Stopped: nothing found is kept.
    Cancelled,
    /// It broke on a fault of its own (traced): nothing found is kept.
    Failed,
}

/// What the search's thread and its owner share.
struct State<R, C> {
    cancelled: AtomicBool,
    /// How it ended, set once under `ended`: a cancellation after the end
    /// changes nothing.
    outcome: OnceLock<Outcome>,
    ended: Mutex<()>,
    changed: Condvar,
    report: OnceLock<R>,
    /// What the tool counts as it goes, for its progress.
    counts: C,
    span: tracing::Span,
}

fn lock(mutex: &Mutex<()>) -> MutexGuard<'_, ()> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<R, C> State<R, C> {
    /// Sets the outcome, if none is, and tells the waiters.
    fn end(&self, outcome: Outcome) -> bool {
        let set = {
            let _ended = lock(&self.ended);
            self.outcome.set(outcome).is_ok()
        };
        self.changed.notify_all();
        set
    }

    /// Sets the report and the outcome together: a cancellation that came
    /// first keeps its outcome, and the report is dropped.
    fn complete(&self, report: R) {
        let ended = {
            let _ended = lock(&self.ended);
            self.outcome.set(Outcome::Complete).is_ok() && self.report.set(report).is_ok()
        };
        self.changed.notify_all();
        if !ended {
            tracing::debug!("search cancelled at its end: its report is dropped");
        }
    }
}

/// A search running on a thread of its own, giving a report `R` once
/// complete and counting `C` as it goes.
pub(crate) struct Searching<R, C> {
    state: Arc<State<R, C>>,
}

impl<R, C> fmt::Debug for Searching<R, C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Searching")
            .field("outcome", &self.state.outcome.get())
            .finish_non_exhaustive()
    }
}

impl<R: Send + Sync + 'static, C: Send + Sync + 'static> Searching<R, C> {
    /// Runs `search` on a thread named `name`, in `span`: it is given the
    /// cancellation to look at and the counts, and gives its report, or
    /// `None` once it saw the cancellation.
    pub(crate) fn spawn(
        name: &str,
        span: tracing::Span,
        counts: C,
        search: impl FnOnce(&AtomicBool, &C) -> Option<R> + Send + 'static,
    ) -> io::Result<Self> {
        let state = Arc::new(State {
            cancelled: AtomicBool::new(false),
            outcome: OnceLock::new(),
            ended: Mutex::new(()),
            changed: Condvar::new(),
            report: OnceLock::new(),
            counts,
            span,
        });
        let worker = Arc::clone(&state);
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                worker.span.in_scope(|| {
                    let searched = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        search(&worker.cancelled, &worker.counts)
                    }));
                    match searched {
                        Ok(Some(report)) => worker.complete(report),
                        // `cancel` set the outcome and traced it.
                        Ok(None) if worker.cancelled.load(Ordering::Acquire) => {}
                        Ok(None) => {
                            tracing::error!("search ended without a report, not cancelled");
                            worker.end(Outcome::Failed);
                        }
                        Err(panic) => {
                            tracing::error!(message = panic_message(&*panic), "search panicked");
                            worker.end(Outcome::Failed);
                        }
                    }
                });
            })?;
        Ok(Self { state })
    }
}

impl<R, C> Searching<R, C> {
    /// Stops the search, if it runs: nothing found is kept. A thread held in
    /// a system call ends on its own.
    pub(crate) fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
        if self.state.end(Outcome::Cancelled) {
            tracing::info!(parent: &self.state.span, "search cancelled");
        }
    }

    /// Waits at most `timeout` for the search to end: its outcome, `None`
    /// while it runs.
    pub(crate) fn wait(&self, timeout: Duration) -> Option<Outcome> {
        let ended = lock(&self.state.ended);
        let _ended = self
            .state
            .changed
            .wait_timeout_while(ended, timeout, |()| self.state.outcome.get().is_none())
            .unwrap_or_else(PoisonError::into_inner);
        self.state.outcome.get().copied()
    }

    /// What the search counted so far.
    pub(crate) fn counts(&self) -> &C {
        &self.state.counts
    }

    /// What the search found, once it is complete.
    pub(crate) fn report(&self) -> Option<&R> {
        self.state.report.get()
    }
}

impl<R, C> Drop for Searching<R, C> {
    /// A search no one looks at any more is stopped.
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    /// How long a test waits: a deadlock fails, it never hangs.
    const WAIT: Duration = Duration::from_secs(20);

    /// A report that says when it is dropped.
    struct Dropped(mpsc::Sender<()>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    fn spawned<R: Send + Sync + 'static>(
        search: impl FnOnce(&AtomicBool, &()) -> Option<R> + Send + 'static,
    ) -> Searching<R, ()> {
        Searching::spawn("okapi-test", tracing::Span::none(), (), search).unwrap()
    }

    #[test]
    fn a_search_complete_gives_its_report() {
        let searching = spawned(|_, ()| Some(7));
        assert_eq!(searching.wait(WAIT), Some(Outcome::Complete));
        assert_eq!(searching.report(), Some(&7));
        // A cancellation after the end changes nothing.
        searching.cancel();
        assert_eq!(searching.wait(WAIT), Some(Outcome::Complete));
        assert_eq!(searching.report(), Some(&7));
    }

    #[test]
    fn a_report_given_after_a_cancellation_is_dropped() {
        let (go, gone) = mpsc::channel();
        let (dropped, told) = mpsc::channel();
        let searching = spawned(move |_, ()| {
            gone.recv().unwrap();
            Some(Dropped(dropped))
        });
        searching.cancel();
        go.send(()).unwrap();
        told.recv_timeout(WAIT).unwrap();
        assert_eq!(searching.wait(WAIT), Some(Outcome::Cancelled));
        assert!(searching.report().is_none());
    }

    #[test]
    fn a_search_dropped_is_cancelled() {
        let (stopped, told) = mpsc::channel();
        let searching = spawned(move |cancelled, ()| {
            while !cancelled.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            stopped.send(()).unwrap();
            None::<()>
        });
        drop(searching);
        told.recv_timeout(WAIT).unwrap();
    }

    #[test]
    fn a_search_that_panics_or_ends_without_a_report_fails() {
        let panicking = spawned(|_, ()| -> Option<()> { panic!("broken") });
        assert_eq!(panicking.wait(WAIT), Some(Outcome::Failed));
        assert!(panicking.report().is_none());
        let empty = spawned(|_, ()| None::<()>);
        assert_eq!(empty.wait(WAIT), Some(Outcome::Failed));
    }
}
