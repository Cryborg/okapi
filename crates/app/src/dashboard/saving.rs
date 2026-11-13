//! Keeps the dashboard's layout, off the interface's thread: a write syncs
//! the disk, which the window must never wait for. One thread writes them
//! in order, only the latest of those waiting (the others are passed by
//! it), and says whether each write kept it.

use std::io;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use okapi_store::DashboardLayout;

const THREAD_NAME: &str = "okapi-dashboard-save";
/// How long the window waits, as it closes, for the last write.
const LAST_WRITE_WAIT: Duration = Duration::from_secs(2);
/// How often it looks whether that write is done.
const LAST_WRITE_POLL: Duration = Duration::from_millis(10);

/// Writes a layout; whether it is kept (the store traces why not).
pub type Write = Box<dyn Fn(&DashboardLayout) -> bool + Send>;

/// What the writes done since the last look said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LastWrite {
    /// None was done.
    Nothing,
    /// The last one kept the layout, or not.
    Kept(bool),
    /// The thread is gone: no write will be done any more.
    Gone,
}

/// The thread that writes the layouts.
pub(crate) struct Saver {
    layouts: Option<Sender<DashboardLayout>>,
    kept: Receiver<bool>,
    thread: Option<JoinHandle<()>>,
}

impl Saver {
    /// Starts the thread, writing through `write`, in the span it is
    /// started in.
    pub(crate) fn start(write: Write) -> io::Result<Self> {
        let (layouts, waiting) = mpsc::channel();
        let (said, kept) = mpsc::channel();
        let span = tracing::Span::current();
        let thread = thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || span.in_scope(|| run(&waiting, &said, &write)))?;
        Ok(Self {
            layouts: Some(layouts),
            kept,
            thread: Some(thread),
        })
    }

    /// Hands `layout` to the thread; false when the thread is gone.
    pub(crate) fn save(&self, layout: DashboardLayout) -> bool {
        self.layouts
            .as_ref()
            .is_some_and(|layouts| layouts.send(layout).is_ok())
    }

    /// What the writes done since the last look said.
    pub(crate) fn last_write(&self) -> LastWrite {
        let mut last = LastWrite::Nothing;
        loop {
            match self.kept.try_recv() {
                Ok(kept) => last = LastWrite::Kept(kept),
                Err(TryRecvError::Empty) => return last,
                // What was said before it went comes first.
                Err(TryRecvError::Disconnected) if last != LastWrite::Nothing => return last,
                Err(TryRecvError::Disconnected) => return LastWrite::Gone,
            }
        }
    }

    /// Lets the thread write the last layout handed to it, and waits for it
    /// a little.
    pub(crate) fn shut_down(&mut self) {
        self.layouts = None;
        let Some(thread) = self.thread.take() else {
            return;
        };
        let started = Instant::now();
        while !thread.is_finished() && started.elapsed() < LAST_WRITE_WAIT {
            thread::sleep(LAST_WRITE_POLL);
        }
        if thread.is_finished() {
            if thread.join().is_err() {
                tracing::error!("dashboard layout writer panicked");
            }
        } else {
            tracing::warn!(waited = ?LAST_WRITE_WAIT, "dashboard layout writer still busy: left to finish");
        }
    }
}

/// Writes the latest of the layouts waiting, until no one can hand one
/// any more; the last one handed is written even then.
fn run(waiting: &Receiver<DashboardLayout>, said: &Sender<bool>, write: &Write) {
    while let Ok(mut layout) = waiting.recv() {
        let mut passed = 0_usize;
        while let Ok(newer) = waiting.try_recv() {
            layout = newer;
            passed += 1;
        }
        if passed > 0 {
            tracing::debug!(passed, "dashboard layouts passed by a newer one");
        }
        let kept = write(&layout);
        if kept {
            tracing::info!(widgets = layout.widgets.len(), "dashboard layout kept");
        }
        // The window gone, there is no one to tell.
        let _ = said.send(kept);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use okapi_store::{DashboardWidget, WidgetSize};

    use super::*;

    fn layout(widgets: usize) -> DashboardLayout {
        DashboardLayout {
            widgets: (0..widgets)
                .map(|_| DashboardWidget {
                    kind: "system".to_owned(),
                    size: WidgetSize::Small,
                })
                .collect(),
        }
    }

    /// A write that notes each layout by its length, says it started, and
    /// waits for `go` before each one.
    fn noting(go: Receiver<()>, started: Sender<()>) -> (Write, Arc<Mutex<Vec<usize>>>) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let noted = Arc::clone(&written);
        let go = Mutex::new(go);
        let started = Mutex::new(started);
        let write: Write = Box::new(move |layout| {
            started.lock().unwrap().send(()).unwrap();
            go.lock().unwrap().recv().unwrap();
            noted.lock().unwrap().push(layout.widgets.len());
            true
        });
        (write, written)
    }

    #[test]
    fn the_latest_waiting_layout_passes_those_before_it() {
        let (go, gone) = mpsc::channel();
        let (start, started) = mpsc::channel();
        let (write, written) = noting(gone, start);
        let mut saver = Saver::start(write).unwrap();
        assert!(saver.save(layout(1)));
        // The first is being written while two more wait.
        started.recv().unwrap();
        assert!(saver.save(layout(2)));
        assert!(saver.save(layout(3)));
        go.send(()).unwrap();
        go.send(()).unwrap();
        saver.shut_down();
        assert_eq!(*written.lock().unwrap(), [1, 3]);
        assert_eq!(saver.last_write(), LastWrite::Kept(true));
    }

    #[test]
    fn the_last_layout_handed_is_written_as_the_window_closes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_path_buf();
        let mut saver = Saver::start(Box::new(move |layout| layout.save(&path).is_ok())).unwrap();
        assert!(saver.save(layout(2)));
        saver.shut_down();
        let kept = DashboardLayout::load(directory.path()).unwrap().unwrap();
        assert_eq!(kept.layout, layout(2));
    }

    #[test]
    fn a_write_that_fails_is_said_and_the_next_one_too() {
        let directory = tempfile::tempdir().unwrap();
        let not_a_directory = directory.path().join("file");
        std::fs::write(&not_a_directory, "").unwrap();
        let target = Arc::new(Mutex::new(not_a_directory));
        let written_to = Arc::clone(&target);
        let saver = Saver::start(Box::new(move |layout| {
            layout.save(&written_to.lock().unwrap()).is_ok()
        }))
        .unwrap();
        assert!(saver.save(layout(1)));
        let said = |saver: &Saver| {
            for _ in 0..1000 {
                if let LastWrite::Kept(kept) = saver.last_write() {
                    return kept;
                }
                thread::sleep(LAST_WRITE_POLL);
            }
            panic!("no write said");
        };
        assert!(!said(&saver));
        *target.lock().unwrap() = directory.path().to_path_buf();
        assert!(saver.save(layout(1)));
        assert!(said(&saver));
    }

    #[test]
    fn a_thread_gone_is_said() {
        let saver = Saver::start(Box::new(|_| panic!("the writer dies"))).unwrap();
        assert!(saver.save(layout(1)));
        let gone = (0..1000).any(|_| {
            thread::sleep(LAST_WRITE_POLL);
            saver.last_write() == LastWrite::Gone
        });
        assert!(gone);
    }
}
