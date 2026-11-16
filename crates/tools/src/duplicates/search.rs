//! The search itself, on a thread of its own: the listing, then the passes
//! of the comparison, then the report. Cancelled, it gives no report: a
//! group half compared is no sure group.

use std::collections::BTreeMap;
use std::io;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use okapi_engine::Exclusions;
use okapi_platform::mount_of;

use super::compare::{self, Comparing, Pass};
use super::report::DuplicateReport;
use crate::listing::{
    self, Cancelled, Found, Listing, Mounts, Opened, SearchError, SearchStats, open_roots,
};
use crate::mounts::mounts_below;
use crate::searching::{Outcome, Searching};

/// The smallest file the screen compares: below, many small groups for
/// little to gain.
pub const MIN_SIZE: u64 = 1024 * 1024;
/// The most threads [`default_threads`] gives: reading is what costs.
const MOST_THREADS: usize = 4;

/// The threads a comparison reads on: one on a rotating disk (or one not
/// known not to be), where reading several files at once seeks back and
/// forth; one per processor otherwise, at most [`MOST_THREADS`].
#[must_use]
pub fn default_threads(rotational: Option<bool>) -> NonZeroUsize {
    let processors = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    threads_for(rotational, processors)
}

/// The threads to read `roots` with: [`default_threads`], each root on the
/// disk of the mount that holds it, from the table of mounts.
#[must_use]
pub fn threads_for_roots(roots: &[PathBuf]) -> NonZeroUsize {
    let mounts = okapi_platform::mounts()
        .inspect_err(|error| tracing::warn!(%error, "mounts unreadable: one thread reads"))
        .ok();
    default_threads(rotational_of(
        roots,
        mounts.as_ref(),
        okapi_platform::rotational,
    ))
}

/// `Some(false)` when every one of `roots` lies on a disk known not to
/// rotate, else `None` (one rotating disk, or one not known not to, and a
/// single thread reads): `rotates` reads the source of the mount it lies on.
fn rotational_of(
    roots: &[PathBuf],
    mounts: Option<&okapi_platform::MountList>,
    rotates: impl Fn(&std::path::Path) -> io::Result<bool>,
) -> Option<bool> {
    let mounts = mounts?;
    let mut all_still = !roots.is_empty();
    for root in roots {
        let mount = mount_of(root, &mounts.mounts);
        let rotating = mount.map(|mount| rotates(std::path::Path::new(&mount.source)));
        if !matches!(rotating, Some(Ok(false))) {
            all_still = false;
        }
        tracing::info!(
            root = %root.display(),
            source = mount.map(|mount| mount.source.as_str()),
            rotating = ?rotating.as_ref().map(|answer| answer.as_ref().map_err(ToString::to_string)),
            "disk of a root"
        );
    }
    all_still.then_some(false)
}

fn threads_for(rotational: Option<bool>, processors: usize) -> NonZeroUsize {
    if rotational == Some(false) {
        NonZeroUsize::new(processors.min(MOST_THREADS)).unwrap_or(NonZeroUsize::MIN)
    } else {
        NonZeroUsize::MIN
    }
}

/// What a search honours.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Smaller files are not compared ([`MIN_SIZE`] on the screen).
    pub min_size: u64,
    pub threads: NonZeroUsize,
    /// What the user never wants touched.
    pub exclusions: Exclusions,
}

/// What a search has done so far, step by step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Its folders are being listed.
    Listing { files: u64 },
    /// The first and last bytes of the files of one size are being read.
    Ends { read: u64, to_read: u64 },
    /// The whole contents of the files alike so far are being read; a file
    /// left out counts as read.
    Contents { bytes_read: u64, bytes_to_read: u64 },
}

/// The step a search is at, with what it has to do.
#[derive(Debug, Clone, Copy)]
enum Step {
    Listing,
    Ends { to_read: u64 },
    Contents { bytes_to_read: u64 },
}

/// What a search counts as it goes.
struct Counts {
    step: Mutex<Step>,
    files: AtomicU64,
    ends_read: AtomicU64,
    bytes_read: AtomicU64,
}

impl Counts {
    fn enter(&self, step: Step) {
        *self.step.lock().unwrap_or_else(PoisonError::into_inner) = step;
    }
}

/// A search for copies of one content in folders, on a thread of its own.
#[derive(Debug)]
pub struct DuplicateSearch {
    searching: Searching<DuplicateReport, Counts>,
}

impl DuplicateSearch {
    /// Starts searching `roots` for copies, and returns once they are
    /// opened, which happens on the caller's thread: never call it from the
    /// interface's (a root on a mount that stopped answering would hold it).
    /// Each is searched where it is (a link is followed to it, none below
    /// it); one given twice is searched once, one inside another as its own.
    ///
    /// # Errors
    ///
    /// [`SearchError::NoRoot`] without a root, [`SearchError::Root`] when
    /// one is not a folder that opens, [`SearchError::Thread`] when the
    /// system gives no thread.
    pub fn start(roots: &[PathBuf], options: SearchOptions) -> Result<Self, SearchError> {
        Self::start_with(
            roots,
            options,
            mounts_below,
            #[cfg(test)]
            None,
        )
    }

    /// [`Self::start`], the mount points below each root read by `mounts`.
    pub(crate) fn start_with(
        roots: &[PathBuf],
        options: SearchOptions,
        mounts: Mounts,
        #[cfg(test)] before_read: Option<fn(&std::path::Path)>,
    ) -> Result<Self, SearchError> {
        let span = tracing::info_span!(
            "duplicates",
            roots = roots.len(),
            min_size = options.min_size,
            threads = options.threads.get()
        );
        let _entered = span.enter();
        let opened = open_roots(roots)?;
        let counts = Counts {
            step: Mutex::new(Step::Listing),
            files: AtomicU64::new(0),
            ends_read: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
        };
        let searching = Searching::spawn(
            "okapi-duplicates",
            span.clone(),
            counts,
            move |cancelled, counts| {
                search(
                    cancelled,
                    counts,
                    opened,
                    &options,
                    mounts,
                    #[cfg(test)]
                    before_read,
                )
            },
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
        let step = *counts.step.lock().unwrap_or_else(PoisonError::into_inner);
        match step {
            Step::Listing => Progress::Listing {
                files: counts.files.load(Ordering::Relaxed),
            },
            Step::Ends { to_read } => Progress::Ends {
                read: counts.ends_read.load(Ordering::Relaxed),
                to_read,
            },
            Step::Contents { bytes_to_read } => Progress::Contents {
                bytes_read: counts.bytes_read.load(Ordering::Relaxed),
                bytes_to_read,
            },
        }
    }

    /// What the search found, once it is complete.
    #[must_use]
    pub fn report(&self) -> Option<&DuplicateReport> {
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
    #[cfg(test)] before_read: Option<fn(&std::path::Path)>,
) -> Option<DuplicateReport> {
    let started = Instant::now();
    let Opened {
        mut roots,
        directories,
    } = opened;
    let mut stats = SearchStats::default();
    let listing = Listing {
        min_size: options.min_size,
        exclusions: &options.exclusions,
        cancelled,
        files: &counts.files,
    };
    let found = listing::list(&listing, &mut roots, directories, mounts, &mut stats);
    let listed = started.elapsed();
    let comparing = Comparing {
        roots: &roots,
        threads: options.threads,
        cancelled,
        bytes_read: &counts.bytes_read,
        files_done: &counts.ends_read,
        #[cfg(test)]
        before_read,
    };
    let groups = found
        .and_then(|found| compare(counts, found, &comparing, &mut stats))
        .ok()?;
    let report = DuplicateReport::new(groups, roots, stats);
    tracing::info!(
        elapsed = ?started.elapsed(),
        listing = ?listed,
        ?stats,
        bytes_read = counts.bytes_read.load(Ordering::Relaxed),
        groups = report.groups().len(),
        gain = report.gain(),
        "duplicate search ended"
    );
    Some(report)
}

/// Groups `found` by size, then by the key of their ends, then of their
/// whole content.
fn compare(
    counts: &Counts,
    found: Vec<Found>,
    comparing: &Comparing<'_>,
    stats: &mut SearchStats,
) -> Result<Vec<Vec<Found>>, Cancelled> {
    let mut by_size: BTreeMap<u64, Vec<Found>> = BTreeMap::new();
    for file in found {
        by_size.entry(file.status.size).or_default().push(file);
    }
    let same_size: Vec<Vec<Found>> = by_size
        .into_values()
        .filter(|files| files.len() > 1)
        .collect();
    let candidates = count(&same_size);
    counts.enter(Step::Ends {
        to_read: candidates as u64,
    });
    let same_ends = compare::split(same_size, Pass::Ends, comparing, &mut stats.dropped)?;
    let to_read = same_ends
        .iter()
        .flatten()
        .map(|file| file.status.size)
        .fold(0, u64::saturating_add);
    counts.enter(Step::Contents {
        bytes_to_read: to_read,
    });
    let after_ends = count(&same_ends);
    let groups = compare::split(same_ends, Pass::Content, comparing, &mut stats.dropped)?;
    tracing::debug!(
        candidates,
        after_ends,
        after_content = count(&groups),
        "files left after each pass"
    );
    Ok(groups)
}

fn count(groups: &[Vec<Found>]) -> usize {
    groups.iter().map(Vec::len).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rotating_disk_or_an_unknown_one_is_read_by_one_thread() {
        assert_eq!(threads_for(Some(true), 16).get(), 1);
        assert_eq!(threads_for(None, 16).get(), 1);
        assert_eq!(threads_for(Some(false), 16).get(), MOST_THREADS);
        assert_eq!(threads_for(Some(false), 2).get(), 2);
    }

    fn mount(point: &str, source: &str) -> okapi_platform::Mount {
        okapi_platform::Mount {
            id: 1,
            device: okapi_platform::Device { major: 8, minor: 1 },
            mount_point: PathBuf::from(point),
            filesystem: "ext4".to_owned(),
            source: source.to_owned(),
            class: okapi_platform::MountClass::Block,
        }
    }

    #[test]
    fn a_root_lies_on_the_disk_of_the_deepest_mount_that_holds_it() {
        let mounts = okapi_platform::MountList {
            mounts: vec![
                mount("/", "/dev/nvme0n1p2"),
                mount("/home/ana/Vidéos", "/dev/sda1"),
                mount("/home/ana/Images", "/dev/sdb1"),
            ],
            malformed: 0,
        };
        let rotates = |source: &std::path::Path| match source.to_str() {
            Some("/dev/sda1") => Ok(true),
            Some("/dev/sdb1") => Err(io::Error::other("no answer")),
            _ => Ok(false),
        };
        let roots = |paths: &[&str]| paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        let of = |paths: &[&str], mounts| rotational_of(&roots(paths), mounts, rotates);
        assert_eq!(of(&["/home/ana/Documents"], Some(&mounts)), Some(false));
        // The deepest mount, not the root's.
        assert_eq!(
            of(
                &["/home/ana/Documents", "/home/ana/Vidéos/x"],
                Some(&mounts)
            ),
            None
        );
        assert_eq!(of(&["/home/ana/Images"], Some(&mounts)), None);
        assert_eq!(of(&["/home/ana"], None), None);
        assert_eq!(of(&[], Some(&mounts)), None);
        assert_eq!(
            of(&["/home/ana"], Some(&okapi_platform::MountList::default())),
            None
        );
    }
}
