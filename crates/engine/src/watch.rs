//! What a caller watching a scan or a cleaning gets: its progress on a
//! channel, and a token to cancel it, both kept apart from the work itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use crate::clean::CleanPass;
use crate::report::Category;

/// How many entries a walk visits between two [`Progress::Walking`].
pub(crate) const PROGRESS_EVERY: u64 = 1024;

/// A token that cancels a scan or a cleaning, from any thread. Cancelling is
/// final for this token.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// How far a scan or a cleaning went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// The scan planned its roots: those it walks or offers (of a group its
    /// options choose), by category and in all (each root once: as many
    /// as [`Progress::RootDone`] in a scan that ends). By category, a root
    /// counts for every category of its group (the rules sharing it), chosen
    /// or not. Not sent when the scan stops before planning.
    Planned {
        roots: BTreeMap<Category, usize>,
        total: usize,
    },
    /// A walk is under way: the entry it reached, the entries seen and the
    /// size offered so far in the whole scan, and its issues so far.
    Walking {
        path: PathBuf,
        entries_seen: u64,
        found_bytes: u64,
        issues: usize,
    },
    /// A root planned is done (walked, offered, or found unreadable): what it
    /// offers, and the scan's issues so far. The scan's total found is the
    /// sum over roots, never over categories (a root may count for two).
    RootDone {
        categories: BTreeSet<Category>,
        entries: usize,
        bytes: u64,
        issues: usize,
    },
    /// An entry asked for went through a pass of the cleaning (`done` of
    /// `total`, the identifiers asked for, each once).
    Cleaning {
        pass: CleanPass,
        done: usize,
        total: usize,
        removed_bytes: u64,
    },
}

/// A test's look at each progress, with the token to cancel at that point.
#[cfg(test)]
pub(crate) type Hook = Box<dyn Fn(&Progress, &Cancel) + Send + Sync>;

/// The progress channel and the cancel token of one scan or cleaning.
pub struct Watch {
    cancel: Cancel,
    progress: Option<Sender<Progress>>,
    /// A receiver that went away is said once per `Watch` (one per scan or
    /// cleaning).
    send_failed: AtomicBool,
    /// Called on each progress, synchronously: lets a test cancel at an exact
    /// point.
    #[cfg(test)]
    pub(crate) hook: Option<Hook>,
}

impl fmt::Debug for Watch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Watch")
            .field("cancel", &self.cancel)
            .field("progress", &self.progress.is_some())
            .finish_non_exhaustive()
    }
}

impl Watch {
    /// Progress sent to `progress`, if any; cancelled by `cancel`.
    #[must_use]
    pub const fn new(cancel: Cancel, progress: Option<Sender<Progress>>) -> Self {
        Self {
            cancel,
            progress,
            send_failed: AtomicBool::new(false),
            #[cfg(test)]
            hook: None,
        }
    }

    /// Nobody watches: no progress, never cancelled.
    #[must_use]
    pub fn none() -> Self {
        Self::new(Cancel::new(), None)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Sends `progress`; a receiver gone away stops nothing (the caller
    /// cancels with the token).
    pub(crate) fn emit(&self, progress: Progress) {
        #[cfg(test)]
        if let Some(hook) = &self.hook {
            hook(&progress, &self.cancel);
        }
        if let Some(sender) = &self.progress
            && sender.send(progress).is_err()
            && !self.send_failed.swap(true, Ordering::Relaxed)
        {
            tracing::debug!("progress receiver gone, progress no longer sent");
        }
    }
}
