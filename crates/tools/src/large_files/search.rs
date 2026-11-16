//! The search itself, on a thread of its own: the listing is all of it.
//! Cancelled, it gives no report: a list half made does not tell which
//! files are the largest.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use okapi_engine::Exclusions;
use okapi_platform::effective_user;

use super::report::LargeFileReport;
use crate::listing::{
    self, Counts, Listing, Measure, Mounts, Opened, Owners, SearchError, SearchStats, open_roots,
};
use crate::mounts::mounts_below;
use crate::searching::{Outcome, Searching};

/// The smallest file the screen lists, in bytes on disk (50 MB in the
/// decimal units shown).
pub const MIN_SIZE: u64 = 50_000_000;

/// The floors the screen filters by, in bytes on disk, the smallest first.
pub const FLOORS: [u64; 5] = [
    MIN_SIZE,
    100_000_000,
    500_000_000,
    1_000_000_000,
    5_000_000_000,
];

/// What a search honours.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Files that take less on disk are not listed ([`MIN_SIZE`] on the
    /// screen).
    pub min_size: u64,
    /// What the user never wants touched.
    pub exclusions: Exclusions,
}

/// What a search has done so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Plain files listed, of any size.
    pub files: u64,
    /// Large files found among them.
    pub found: u64,
}

/// A search for the large files of folders, on a thread of its own.
#[derive(Debug)]
pub struct LargeFileSearch {
    searching: Searching<LargeFileReport, Counts>,
}

impl LargeFileSearch {
    /// Starts searching `roots` for the user's large files, and returns once
    /// they are opened, which happens on the caller's thread: never call it
    /// from the interface's (a root on a mount that stopped answering would
    /// hold it). Each is searched where it is (a link is followed to it,
    /// none below it); one given twice is searched once, one inside another
    /// as its own. A file of another user is never listed.
    ///
    /// # Errors
    ///
    /// [`SearchError::NoRoot`] without a root, [`SearchError::Root`] when
    /// one is not a folder that opens, [`SearchError::Thread`] when the
    /// system gives no thread.
    pub fn start(roots: &[PathBuf], options: SearchOptions) -> Result<Self, SearchError> {
        Self::start_with(roots, options, mounts_below, Owners::Only(effective_user()))
    }

    /// [`Self::start`], the mount points below each root read by `mounts`,
    /// the files of `owners` only listed.
    pub(crate) fn start_with(
        roots: &[PathBuf],
        options: SearchOptions,
        mounts: Mounts,
        owners: Owners,
    ) -> Result<Self, SearchError> {
        let span = tracing::info_span!(
            "large_files",
            roots = roots.len(),
            min_size = options.min_size
        );
        let _entered = span.enter();
        if owners == Owners::Only(None) {
            tracing::warn!("the user is unknown: no file is the user's, none is listed");
        }
        let opened = open_roots(roots)?;
        let counts = Counts::default();
        let searching = Searching::spawn(
            "okapi-large-files",
            span.clone(),
            counts,
            move |cancelled, counts| search(cancelled, counts, opened, &options, mounts, owners),
        )
        .map_err(|error| {
            tracing::warn!(%error, "search thread not started");
            SearchError::Thread(error)
        })?;
        Ok(Self { searching })
    }

    /// Stops the search, if it runs: nothing found is kept. A thread held in
    /// a system call ends on its own.
    pub fn cancel(&self) {
        self.searching.cancel();
    }

    /// Waits at most `timeout` for the search to end: its outcome, `None`
    /// while it runs.
    #[must_use]
    pub fn wait(&self, timeout: Duration) -> Option<Outcome> {
        self.searching.wait(timeout)
    }

    #[must_use]
    pub fn progress(&self) -> Progress {
        let counts = self.searching.counts();
        Progress {
            files: counts.files.load(Ordering::Relaxed),
            found: counts.kept.load(Ordering::Relaxed),
        }
    }

    /// What the search found, once it is complete.
    #[must_use]
    pub fn report(&self) -> Option<&LargeFileReport> {
        self.searching.report()
    }
}

/// The search, from the listing to the report; `None` once cancelled.
fn search(
    cancelled: &AtomicBool,
    counts: &Counts,
    opened: Opened,
    options: &SearchOptions,
    mounts: Mounts,
    owners: Owners,
) -> Option<LargeFileReport> {
    let started = Instant::now();
    let Opened {
        mut roots,
        directories,
    } = opened;
    let mut stats = SearchStats::default();
    let listing = Listing {
        min_size: options.min_size,
        measure: Measure::OnDisk,
        owners,
        exclusions: &options.exclusions,
        cancelled,
        counts,
    };
    let found = listing::list(&listing, &mut roots, directories, mounts, &mut stats).ok()?;
    let report = LargeFileReport::new(found, roots, stats);
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for file in report.files() {
        *kinds.entry(file.kind().key()).or_default() += 1;
    }
    tracing::info!(
        elapsed = ?started.elapsed(),
        ?stats,
        found = report.files().len(),
        bytes = report.total(),
        ?kinds,
        "large file search ended"
    );
    Some(report)
}
