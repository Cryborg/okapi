//! The search itself, on a thread of its own: the listing, then the passes
//! of the comparison, then the report. Cancelled, it gives no report: a
//! group half compared is no sure group.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use okapi_engine::Exclusions;
use okapi_platform::Directory;

use super::compare::{self, Comparing, Pass};
use super::report::{DuplicateReport, Found, Root, SearchStats};
use super::walk::{self, Cancelled, Listing};
use crate::mounts::{mount_of, mounts_below};
use crate::panic_message;

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

/// How the mount points below a root are read, and the filesystem it is on.
pub(crate) type Mounts = fn(&std::path::Path) -> (BTreeSet<PathBuf>, Option<String>);

/// What a search honours.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Smaller files are not compared ([`MIN_SIZE`] on the screen).
    pub min_size: u64,
    pub threads: NonZeroUsize,
    /// What the user never wants touched.
    pub exclusions: Exclusions,
}

/// Why a search did not start.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("no folder to search")]
    NoRoot,
    #[error("the folder {} cannot be searched", path.display())]
    Root {
        path: PathBuf,
        #[source]
        error: io::Error,
    },
    #[error("the thread of the search could not be started")]
    Thread(#[source] io::Error),
}

/// How a search ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    /// Stopped: nothing found is kept.
    Cancelled,
    /// It broke on a fault of its own (traced): nothing found is kept.
    Failed,
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

/// What the search's thread and its owner share.
struct Shared {
    cancelled: AtomicBool,
    /// How it ended, set once under `ended`: a cancellation after the end
    /// changes nothing.
    outcome: OnceLock<Outcome>,
    ended: Mutex<()>,
    changed: Condvar,
    report: OnceLock<DuplicateReport>,
    step: Mutex<Step>,
    files: AtomicU64,
    ends_read: AtomicU64,
    bytes_read: AtomicU64,
    span: tracing::Span,
}

fn lock(mutex: &Mutex<()>) -> MutexGuard<'_, ()> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn enter(&self, step: Step) {
        *self.step.lock().unwrap_or_else(PoisonError::into_inner) = step;
    }

    /// Sets the outcome, if none is, and tells the waiters.
    fn end(&self, outcome: Outcome) -> bool {
        let set = {
            let _ended = lock(&self.ended);
            self.outcome.set(outcome).is_ok()
        };
        self.changed.notify_all();
        set
    }
}

/// A search for copies of one content in folders, on a thread of its own.
pub struct DuplicateSearch {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for DuplicateSearch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DuplicateSearch")
            .field("outcome", &self.shared.outcome.get())
            .finish_non_exhaustive()
    }
}

/// The roots opened, and their folders.
struct Opened {
    roots: Vec<Root>,
    directories: Vec<Directory>,
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
        let shared = Arc::new(Shared {
            cancelled: AtomicBool::new(false),
            outcome: OnceLock::new(),
            ended: Mutex::new(()),
            changed: Condvar::new(),
            report: OnceLock::new(),
            step: Mutex::new(Step::Listing),
            files: AtomicU64::new(0),
            ends_read: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            span: span.clone(),
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("okapi-duplicates".to_owned())
            .spawn(move || {
                let span = worker.span.clone();
                span.in_scope(|| {
                    let searched = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        search(
                            &worker,
                            opened,
                            &options,
                            mounts,
                            #[cfg(test)]
                            before_read,
                        );
                    }));
                    if let Err(panic) = searched {
                        tracing::error!(
                            message = panic_message(&*panic),
                            "duplicate search panicked"
                        );
                        worker.end(Outcome::Failed);
                    }
                });
            })
            .map_err(|error| {
                tracing::warn!(%error, "search thread not started");
                SearchError::Thread(error)
            })?;
        Ok(Self { shared })
    }

    /// Stops the search, if it runs: nothing found is kept. A thread held in
    /// a system call ends on its own.
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
        if self.shared.end(Outcome::Cancelled) {
            tracing::info!(parent: &self.shared.span, "duplicate search cancelled");
        }
    }

    /// Waits at most `timeout` for the search to end: its outcome, `None`
    /// while it runs.
    #[must_use]
    pub fn wait(&self, timeout: Duration) -> Option<Outcome> {
        let ended = lock(&self.shared.ended);
        let _ended = self
            .shared
            .changed
            .wait_timeout_while(ended, timeout, |()| self.shared.outcome.get().is_none())
            .unwrap_or_else(PoisonError::into_inner);
        self.shared.outcome.get().copied()
    }

    #[must_use]
    pub fn progress(&self) -> Progress {
        let shared = &self.shared;
        let step = *shared.step.lock().unwrap_or_else(PoisonError::into_inner);
        match step {
            Step::Listing => Progress::Listing {
                files: shared.files.load(Ordering::Relaxed),
            },
            Step::Ends { to_read } => Progress::Ends {
                read: shared.ends_read.load(Ordering::Relaxed),
                to_read,
            },
            Step::Contents { bytes_to_read } => Progress::Contents {
                bytes_read: shared.bytes_read.load(Ordering::Relaxed),
                bytes_to_read,
            },
        }
    }

    /// What the search found, once it is complete.
    #[must_use]
    pub fn report(&self) -> Option<&DuplicateReport> {
        self.shared.report.get()
    }
}

impl Drop for DuplicateSearch {
    /// A search no one looks at any more is stopped.
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Opens each root where it really is, leaving out one given twice.
fn open_roots(paths: &[PathBuf]) -> Result<Opened, SearchError> {
    let mut opened: Vec<(Root, Directory)> = Vec::new();
    for path in paths {
        let root = std::fs::canonicalize(path).and_then(|canonical| {
            let directory = Directory::open(&canonical)?;
            let status = directory.status()?;
            Ok((
                Root {
                    path: canonical,
                    identity: status.identity,
                    // Known once its filesystem is read (`list`).
                    may_alias: true,
                },
                directory,
            ))
        });
        let root = root.map_err(|error| {
            tracing::warn!(path = %path.display(), %error, "folder to search not opened");
            SearchError::Root {
                path: path.clone(),
                error,
            }
        })?;
        opened.push(root);
    }
    // One folder given twice (or by two paths) is searched once. A root
    // inside another is searched as its own: the walk of the outer one
    // leaves it, as it may lie below a folder that walk skips.
    let mut kept: Vec<(Root, Directory)> = Vec::new();
    for (root, directory) in opened {
        if kept
            .iter()
            .any(|(earlier, _)| earlier.identity == root.identity)
        {
            tracing::info!(path = %root.path.display(), "folder given twice: searched once");
        } else {
            kept.push((root, directory));
        }
    }
    if kept.is_empty() {
        return Err(SearchError::NoRoot);
    }
    let (roots, directories) = kept.into_iter().unzip();
    Ok(Opened { roots, directories })
}

/// The search, from the listing to the report; the outcome set at the end.
fn search(
    shared: &Shared,
    opened: Opened,
    options: &SearchOptions,
    mounts: Mounts,
    #[cfg(test)] before_read: Option<fn(&std::path::Path)>,
) {
    let started = Instant::now();
    let Opened { roots, directories } = opened;
    let mut stats = SearchStats::default();
    let mut roots = roots;
    let found = list(shared, &mut roots, directories, options, mounts, &mut stats);
    let listed = started.elapsed();
    let comparing = Comparing {
        roots: &roots,
        threads: options.threads,
        cancelled: &shared.cancelled,
        bytes_read: &shared.bytes_read,
        files_done: &shared.ends_read,
        #[cfg(test)]
        before_read,
    };
    let result = found.and_then(|found| compare(shared, found, &comparing, &mut stats));
    let Ok(groups) = result else {
        // `cancel` set the outcome and traced it.
        return;
    };
    let report = DuplicateReport::new(groups, roots, stats);
    tracing::info!(
        elapsed = ?started.elapsed(),
        listing = ?listed,
        ?stats,
        bytes_read = shared.bytes_read.load(Ordering::Relaxed),
        groups = report.groups().len(),
        gain = report.gain(),
        "duplicate search ended"
    );
    // The report and the outcome together: a cancellation that came first
    // keeps its outcome, and no report is given.
    let ended = {
        let _ended = lock(&shared.ended);
        shared.outcome.set(Outcome::Complete).is_ok() && shared.report.set(report).is_ok()
    };
    shared.changed.notify_all();
    if !ended {
        tracing::debug!("search cancelled at its end: its report is dropped");
    }
}

/// Lists every root, the mount points below each read once.
fn list(
    shared: &Shared,
    roots: &mut [Root],
    directories: Vec<Directory>,
    options: &SearchOptions,
    mounts: Mounts,
    stats: &mut SearchStats,
) -> Result<Vec<Found>, Cancelled> {
    let mut below = BTreeSet::new();
    for root in roots.iter_mut() {
        let (points, filesystem) = mounts(&root.path);
        below.extend(points);
        root.may_alias = filesystem.as_deref().is_none_or(may_alias);
    }
    let mounts = below;
    let roots: &[Root] = roots;
    let listing = Listing {
        roots: &roots.iter().map(|root| root.identity).collect(),
        min_size: options.min_size,
        exclusions: &options.exclusions,
        mounts: &mounts,
        cancelled: &shared.cancelled,
        files: &shared.files,
    };
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for (index, (root, directory)) in roots.iter().zip(directories).enumerate() {
        if options.exclusions.excludes(&root.path)
            || options.exclusions.excludes_identity(root.identity)
        {
            stats.excluded += 1;
            tracing::info!(path = %root.path.display(), "folder searched is excluded: not searched");
            continue;
        }
        walk::walk(
            &listing, index, directory, &root.path, stats, &mut seen, &mut found,
        )?;
    }
    Ok(found)
}

/// Whether the filesystem `filesystem` (as `mountinfo` names it) may show
/// a file under another identity than its own: FUSE (a bind of a folder, a
/// desktop's share), network and stacked filesystems.
pub(crate) fn may_alias(filesystem: &str) -> bool {
    filesystem.starts_with(FUSE_PREFIX)
        || NETWORK_FILESYSTEMS.contains(&filesystem)
        || STACKED_FILESYSTEMS.contains(&filesystem)
}

/// What every FUSE filesystem's type starts with (`fuse.bindfs`).
const FUSE_PREFIX: &str = "fuse";
/// Filesystems reached over the network.
const NETWORK_FILESYSTEMS: &[&str] = &["nfs", "nfs4", "cifs", "smb3", "smbfs", "9p", "ceph"];
/// Filesystems stacked over another folder, whose files they show again.
const STACKED_FILESYSTEMS: &[&str] = &["overlay", "aufs"];

/// Groups `found` by size, then by the key of their ends, then of their
/// whole content.
fn compare(
    shared: &Shared,
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
    shared.enter(Step::Ends {
        to_read: candidates as u64,
    });
    let same_ends = compare::split(same_size, Pass::Ends, comparing, &mut stats.dropped)?;
    let to_read = same_ends
        .iter()
        .flatten()
        .map(|file| file.status.size)
        .fold(0, u64::saturating_add);
    shared.enter(Step::Contents {
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
