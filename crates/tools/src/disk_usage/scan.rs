//! The analysis itself: a few detached threads share a stack of directories
//! to read, depth first. A directory is opened from its parent, never through
//! a link, and checked to be the one listed; a mount point is never entered.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use okapi_platform::{Directory, EntryStatus, EntryType, FileIdentity, Unreached, reach_with};

use super::tree::{Child, NodeId, NodeState, Size, Tree, View};
use crate::mounts::mounts_below;

/// The most threads [`default_threads`] gives: more only adds contention on
/// one disk.
const MOST_THREADS: usize = 8;
/// How many entries of a directory are read between two looks at the
/// cancellation.
const CANCEL_LOOK_EVERY: usize = 1024;

/// The threads an analysis uses by default: one per processor, at most
/// [`MOST_THREADS`].
#[must_use]
pub fn default_threads() -> NonZeroUsize {
    threads_for(std::thread::available_parallelism().map_or(1, NonZeroUsize::get))
}

/// The threads for `processors` processors.
fn threads_for(processors: usize) -> NonZeroUsize {
    NonZeroUsize::new(processors.min(MOST_THREADS)).unwrap_or(NonZeroUsize::MIN)
}

/// Why an analysis did not start.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("the folder to analyze cannot be opened")]
    Root(#[source] io::Error),
    #[error("a thread of the analysis could not be started")]
    Thread(#[source] io::Error),
}

/// Why the files of a directory were not read.
#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    #[error("not a folder the analysis read")]
    NotRead,
    #[error("the folder changed since the analysis")]
    Changed,
    #[error("the folder was removed since the analysis")]
    Gone,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// How an analysis ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    /// Stopped: what was read stays, the rest is pending.
    Cancelled,
}

/// What an analysis has read so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub directories: u64,
    pub files: u64,
    pub allocated: u64,
    /// Directories that could not be opened or listed: what they hold is
    /// unknown.
    pub unreadable: u64,
}

/// What the analysis met, counted for its last trace.
#[derive(Debug, Default)]
struct Counts {
    directories: AtomicU64,
    files: AtomicU64,
    allocated: AtomicU64,
    unreadable_directories: AtomicU64,
    unreadable_entries: AtomicU64,
    too_many_open: AtomicU64,
    gone: AtomicU64,
    changed: AtomicU64,
    panicked: AtomicU64,
    /// Entries whose blocks the system did not give: their length is taken
    /// for what they take on disk.
    allocated_unknown: AtomicU64,
    other_mounts: AtomicU64,
    counted_elsewhere: AtomicU64,
}

fn count(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Ordering::Relaxed);
}

fn read(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

/// Where a task's directory comes from.
enum Source {
    /// The root, opened by `start`.
    Root(Directory),
    /// A subdirectory, opened from its parent, never through a link: it must
    /// still be the directory `expected` listed there.
    Child {
        parent: Arc<Directory>,
        name: OsString,
        expected: FileIdentity,
    },
}

/// A directory to read.
struct Task {
    source: Source,
    node: NodeId,
    path: PathBuf,
}

/// The tasks not done yet: those waiting on the stack and those running.
struct Work {
    stack: Vec<Task>,
    in_flight: usize,
}

/// What the threads of one analysis share.
struct Shared {
    tree: Mutex<Tree>,
    work: Mutex<Work>,
    /// Told when a task ends or the analysis ends.
    changed: Condvar,
    cancelled: AtomicBool,
    /// How it ended, set once, under the lock of `work`: a cancellation
    /// after the end changes nothing.
    outcome: OnceLock<Outcome>,
    /// The files of several names met, and the directory that counted each.
    hard_links: Mutex<HashMap<FileIdentity, NodeId>>,
    /// The mount points below the root, read once: never touched, a mount
    /// that stopped answering would hold a thread for good.
    mounts: BTreeSet<PathBuf>,
    root_device: u64,
    counts: Counts,
    started: Instant,
    span: tracing::Span,
    /// Called with each directory's path before it is read, to test what
    /// happens meanwhile: `true` cancels the analysis there.
    #[cfg(test)]
    before_read: Option<fn(&Path) -> bool>,
}

/// Locks `mutex`; a thread that panicked holding it left consistent data
/// (each change is made whole under the lock).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An analysis of a folder, running on threads of its own: what it read is
/// viewed as it goes.
pub struct UsageScan {
    shared: Arc<Shared>,
    root: PathBuf,
    root_identity: FileIdentity,
}

impl std::fmt::Debug for UsageScan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsageScan")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl UsageScan {
    /// Starts analyzing `root` on `threads` threads, and returns once the
    /// root is opened: never on the interface's thread (a root on a mount
    /// that stopped answering would hold it).
    ///
    /// # Errors
    ///
    /// [`ScanError::Root`] when `root` is not a directory or cannot be
    /// opened (a link is analyzed where it leads; no link below it is
    /// followed); [`ScanError::Thread`] when the system gives no thread.
    pub fn start(root: &Path, threads: NonZeroUsize) -> Result<Self, ScanError> {
        Self::start_with(
            root,
            threads,
            mounts_below,
            #[cfg(test)]
            None,
        )
    }

    /// [`Self::start`], the mount points below the root read by `mounts`.
    /// The folder given is analyzed where it is (`/home` may be a link): no
    /// link below it is followed.
    pub(crate) fn start_with(
        root: &Path,
        threads: NonZeroUsize,
        mounts: fn(&Path) -> (BTreeSet<PathBuf>, Option<String>),
        #[cfg(test)] before_read: Option<fn(&Path) -> bool>,
    ) -> Result<Self, ScanError> {
        let span = tracing::info_span!("disk_usage", root = %root.display());
        let _entered = span.enter();
        let opened = std::fs::canonicalize(root).and_then(|canonical| {
            let directory = Directory::open(&canonical)?;
            let status = directory.status()?;
            Ok((canonical, directory, status))
        });
        let (canonical, directory, status) = opened.map_err(|error| {
            tracing::warn!(%error, "folder to analyze not opened");
            ScanError::Root(error)
        })?;
        let (mounts, filesystem) = mounts(&canonical);
        tracing::info!(
            root = %canonical.display(),
            filesystem = filesystem.as_deref().unwrap_or("unknown"),
            threads = threads.get(),
            mounts = mounts.len(),
            "disk usage analysis started"
        );
        let shared = Arc::new(Shared {
            tree: Mutex::new(Tree::new(
                canonical.clone().into_os_string(),
                status.identity,
            )),
            work: Mutex::new(Work {
                stack: vec![Task {
                    source: Source::Root(directory),
                    node: NodeId::ROOT,
                    path: canonical.clone(),
                }],
                in_flight: 1,
            }),
            changed: Condvar::new(),
            cancelled: AtomicBool::new(false),
            outcome: OnceLock::new(),
            hard_links: Mutex::new(HashMap::new()),
            mounts,
            root_device: status.identity.device(),
            counts: Counts::default(),
            started: Instant::now(),
            span: span.clone(),
            #[cfg(test)]
            before_read,
        });
        for index in 0..threads.get() {
            let worker = Arc::clone(&shared);
            let spawned = std::thread::Builder::new()
                .name(format!("okapi-disk-usage-{index}"))
                .spawn(move || {
                    let span = worker.span.clone();
                    span.in_scope(|| work(&worker));
                });
            if let Err(error) = spawned {
                tracing::warn!(%error, "disk usage thread not started");
                cancel(&shared);
                return Err(ScanError::Thread(error));
            }
        }
        Ok(Self {
            shared,
            root: canonical,
            root_identity: status.identity,
        })
    }

    /// Stops the analysis, if it runs: what was read stays. A thread held in
    /// a system call ends on its own.
    pub fn cancel(&self) {
        cancel(&self.shared);
    }

    /// Waits at most `timeout` for the analysis to end: its outcome, `None`
    /// while it runs.
    #[must_use]
    pub fn wait(&self, timeout: Duration) -> Option<Outcome> {
        let work = lock(&self.shared.work);
        let _work = self
            .shared
            .changed
            .wait_timeout_while(work, timeout, |_| self.shared.outcome.get().is_none())
            .unwrap_or_else(PoisonError::into_inner);
        self.shared.outcome.get().copied()
    }

    /// What was read so far.
    #[must_use]
    pub fn progress(&self) -> Progress {
        let counts = &self.shared.counts;
        Progress {
            directories: read(&counts.directories),
            files: read(&counts.files),
            allocated: read(&counts.allocated),
            unreadable: read(&counts.unreadable_directories),
        }
    }

    /// What the directory `of` was when the analysis listed it (device and
    /// inode): what one opened again must still be; `None` for a mount
    /// never read.
    #[must_use]
    pub fn identity(&self, of: NodeId) -> Option<FileIdentity> {
        lock(&self.shared.tree).identity(of)
    }

    /// Whether no thread reads any more: an analysis cancelled may still
    /// write the directories it was reading when it was.
    #[must_use]
    pub fn settled(&self) -> bool {
        lock(&self.shared.work).in_flight == 0
    }

    /// The directory `of` and its subdirectories, with what each holds so
    /// far.
    #[must_use]
    pub fn view(&self, of: NodeId) -> Option<View> {
        lock(&self.shared.tree).view(of)
    }

    /// The folder analyzed, canonical.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path of the directory `of`.
    #[must_use]
    pub fn path(&self, of: NodeId) -> Option<PathBuf> {
        let names = lock(&self.shared.tree).names(of)?;
        let mut path = self.root.clone();
        path.extend(names);
        Some(path)
    }

    /// Opens the directory `of` again, read by the analysis, from the
    /// filesystem root, name by name, never through a link ([`reach_with`]):
    /// the root and each directory below it must still be the one the
    /// analysis read there, none below the root a mount point;
    /// [`FilesError::Gone`] when one is gone.
    pub(crate) fn open(&self, of: NodeId) -> Result<Directory, FilesError> {
        let lineage = lock(&self.shared.tree)
            .read_lineage(of)
            .ok_or(FilesError::NotRead)?;
        let names: Vec<&OsStr> = lineage.iter().map(|(name, _)| name.as_os_str()).collect();
        let listed = |index: usize, status: &EntryStatus| {
            lineage
                .get(index)
                .is_some_and(|&(_, expected)| still_listed(status, expected))
        };
        match reach_with(&self.root, self.root_identity, &names, listed) {
            Ok(Some(reached)) => Ok(reached.directory),
            Ok(None) => Err(FilesError::Gone),
            Err(Unreached::Changed) => Err(FilesError::Changed),
            Err(Unreached::Io(error)) => Err(FilesError::Io(error)),
        }
    }

    /// Whether `path` is a mount point below the root, read at the start.
    pub(crate) fn is_mount(&self, path: &Path) -> bool {
        self.shared.mounts.contains(path)
    }

    /// The names of the subdirectories of `of` the analysis met.
    pub(crate) fn child_names(&self, of: NodeId) -> Vec<OsString> {
        lock(&self.shared.tree).child_names(of)
    }

    /// The analysis's span, for the reading of a directory's files.
    pub(crate) fn span(&self) -> &tracing::Span {
        &self.shared.span
    }

    /// The directory that counted the file `identity`, when it has several
    /// names.
    pub(crate) fn counted_by(&self, identity: FileIdentity) -> Option<NodeId> {
        lock(&self.shared.hard_links).get(&identity).copied()
    }

    #[cfg(test)]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::Acquire)
    }
}

impl Drop for UsageScan {
    /// An analysis no one looks at any more is stopped.
    fn drop(&mut self) {
        cancel(&self.shared);
    }
}

fn cancel(shared: &Shared) {
    if shared.outcome.get().is_some() || shared.cancelled.swap(true, Ordering::AcqRel) {
        return;
    }
    let left = {
        let mut work = lock(&shared.work);
        let left = work.stack.len();
        // Closes their descriptors; their nodes stay pending.
        work.stack.clear();
        work.in_flight -= left;
        let running = work.in_flight;
        shared
            .outcome
            .set(Outcome::Cancelled)
            .is_ok()
            .then_some((left, running))
    };
    shared.changed.notify_all();
    if let Some((left, running)) = left {
        finished(shared, Outcome::Cancelled, left, running);
    }
}

/// The last trace of an analysis. A cancelled one says what was counted
/// then: a thread still reading may add a directory after.
fn finished(shared: &Shared, outcome: Outcome, left: usize, running: usize) {
    let counts = &shared.counts;
    tracing::info!(
        parent: &shared.span,
        ?outcome,
        elapsed = ?shared.started.elapsed(),
        directories = read(&counts.directories),
        files = read(&counts.files),
        allocated = read(&counts.allocated),
        unreadable_directories = read(&counts.unreadable_directories),
        unreadable_entries = read(&counts.unreadable_entries),
        too_many_open = read(&counts.too_many_open),
        gone = read(&counts.gone),
        changed = read(&counts.changed),
        panicked = read(&counts.panicked),
        allocated_unknown = read(&counts.allocated_unknown),
        other_mounts = read(&counts.other_mounts),
        counted_elsewhere = read(&counts.counted_elsewhere),
        left,
        running,
        "disk usage analysis ended"
    );
}

/// A task taken off the stack: whatever becomes of it, it is counted done
/// once, with the subdirectories it found, and the others told.
struct Running<'a> {
    shared: &'a Shared,
    children: Vec<Task>,
}

impl Drop for Running<'_> {
    /// The one place a task ends: its subdirectories pushed and counted under
    /// the same lock, the end of the analysis set when none is left.
    fn drop(&mut self) {
        let (pushed, ended) = {
            let mut work = lock(&self.shared.work);
            work.in_flight -= 1;
            let pushed = !self.children.is_empty();
            if self.shared.cancelled.load(Ordering::Acquire) {
                // Closes their descriptors; their nodes stay pending.
                self.children.clear();
            } else {
                work.in_flight += self.children.len();
                work.stack.append(&mut self.children);
            }
            let ended = work.in_flight == 0
                && !self.shared.cancelled.load(Ordering::Acquire)
                && self.shared.outcome.set(Outcome::Complete).is_ok();
            (pushed, ended)
        };
        // Only new work or the end wakes the others.
        if pushed || ended {
            self.shared.changed.notify_all();
        }
        if ended {
            finished(self.shared, Outcome::Complete, 0, 0);
        }
    }
}

/// A thread's loop: a task at a time, until none is left or the analysis is
/// cancelled.
fn work(shared: &Shared) {
    loop {
        let task = {
            let mut work = lock(&shared.work);
            loop {
                if shared.cancelled.load(Ordering::Acquire) {
                    return;
                }
                if let Some(task) = work.stack.pop() {
                    break task;
                }
                if work.in_flight == 0 {
                    return;
                }
                work = shared
                    .changed
                    .wait(work)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let mut running = Running {
            shared,
            children: Vec::new(),
        };
        let node = task.node;
        let path = task.path.clone();
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| read_directory(shared, task)));
        match outcome {
            Ok(children) => running.children = children,
            Err(panic) => {
                // The thread lives on; the directory is said unreadable.
                let message = crate::panic_message(&*panic);
                count(&shared.counts.panicked, 1);
                tracing::error!(path = %path.display(), message, "reading a directory panicked: left unread");
                lock(&shared.tree).fail(node, NodeState::Unreadable(io::ErrorKind::Other));
            }
        }
    }
}

/// Why a directory was not read.
enum Failure {
    Io(io::Error),
    /// No longer the directory listed.
    Changed,
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Whether a directory below the root, read again as `status`, is still the
/// one listed as `expected`, and no mount point (the root may be one, as `/`
/// is: [`reach_with`] judges it by its identity alone).
fn still_listed(status: &EntryStatus, expected: FileIdentity) -> bool {
    status.identity == expected && status.mount_root != Some(true)
}

/// Opens the task's directory, checked to be the one listed, and reads it.
fn open(source: Source) -> Result<(Directory, EntryStatus), Failure> {
    let (directory, expected) = match source {
        Source::Root(directory) => {
            let status = directory.status()?;
            return Ok((directory, status));
        }
        Source::Child {
            parent,
            name,
            expected,
        } => (parent.open_child(&name)?, expected),
    };
    let status = directory.status()?;
    if !still_listed(&status, expected) {
        return Err(Failure::Changed);
    }
    Ok((directory, status))
}

/// What an entry that is not a directory takes.
pub(crate) fn file_size(status: &EntryStatus) -> Size {
    Size {
        allocated: status.on_disk(),
        apparent: status.size,
        files: 1,
    }
}

/// What reading a directory found so far.
struct Listed {
    /// What it holds itself: its files and its own blocks.
    own: Size,
    /// An entry could not be read.
    incomplete: bool,
    children: Vec<Child>,
    /// The subdirectories to read: their place among `children`, how to
    /// open them, their path.
    subdirectories: Vec<(usize, Source, PathBuf)>,
}

/// Reads the entry `name` of `directory`, at `parent`, the node `node`.
fn read_entry(
    shared: &Shared,
    parent: &Path,
    node: NodeId,
    directory: &Arc<Directory>,
    name: OsString,
    listed: &mut Listed,
) {
    let path = parent.join(&name);
    // A mount point is never even read: one that stopped answering would
    // hold this thread for good.
    if shared.mounts.contains(&path) {
        count(&shared.counts.other_mounts, 1);
        tracing::debug!(path = %path.display(), "mount not entered");
        listed.children.push(Child {
            name,
            identity: None,
            other_mount: true,
        });
        return;
    }
    let status = match directory.status_of(&name) {
        Ok(status) => status,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            count(&shared.counts.unreadable_entries, 1);
            tracing::debug!(path = %path.display(), %error, "entry unreadable");
            listed.incomplete = true;
            return;
        }
    };
    if status.kind == EntryType::Directory {
        let other_mount = status.other_mount(shared.root_device);
        if other_mount {
            count(&shared.counts.other_mounts, 1);
            tracing::debug!(path = %path.display(), "mount not entered");
        } else {
            listed.subdirectories.push((
                listed.children.len(),
                Source::Child {
                    parent: Arc::clone(directory),
                    name: name.clone(),
                    expected: status.identity,
                },
                path,
            ));
        }
        listed.children.push(Child {
            name,
            identity: Some(status.identity),
            other_mount,
        });
        return;
    }
    if status.allocated.is_none() {
        count(&shared.counts.allocated_unknown, 1);
    }
    let mut size = file_size(&status);
    if status.links > 1 {
        match lock(&shared.hard_links).entry(status.identity) {
            Entry::Occupied(_) => {
                count(&shared.counts.counted_elsewhere, 1);
                size.allocated = 0;
                size.apparent = 0;
            }
            Entry::Vacant(vacant) => {
                vacant.insert(node);
            }
        }
    }
    listed.own.add(size);
}

/// The state of a directory not read, counted and said.
fn failed_state(shared: &Shared, path: &Path, failure: Failure) -> NodeState {
    match failure {
        Failure::Io(error) if error.kind() == io::ErrorKind::NotFound => {
            count(&shared.counts.gone, 1);
            tracing::debug!(path = %path.display(), "directory gone since it was listed");
            NodeState::Gone
        }
        // No longer a directory: replaced by a file or a link.
        Failure::Io(error) if error.kind() == io::ErrorKind::NotADirectory => {
            count(&shared.counts.changed, 1);
            tracing::debug!(path = %path.display(), "directory replaced since it was listed");
            NodeState::Changed
        }
        Failure::Io(error) => {
            count(&shared.counts.unreadable_directories, 1);
            if okapi_platform::out_of_descriptors(&error) {
                // Warned once, each said below: the count is in the last trace.
                if shared.counts.too_many_open.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::warn!(path = %path.display(), %error, "too many open files: directories left unread");
                } else {
                    tracing::debug!(path = %path.display(), %error, "too many open files: directory left unread");
                }
            } else {
                tracing::debug!(path = %path.display(), %error, "directory unreadable");
            }
            NodeState::Unreadable(error.kind())
        }
        Failure::Changed => {
            count(&shared.counts.changed, 1);
            tracing::debug!(path = %path.display(), "directory changed since it was listed");
            NodeState::Changed
        }
    }
}

/// Reads the task's directory into the tree: its subdirectories, as tasks.
fn read_directory(shared: &Shared, task: Task) -> Vec<Task> {
    let node = task.node;
    let fail = |failure: Failure| {
        let state = failed_state(shared, &task.path, failure);
        lock(&shared.tree).fail(node, state);
        Vec::new()
    };
    if shared.cancelled.load(Ordering::Acquire) {
        return Vec::new();
    }
    #[cfg(test)]
    if shared
        .before_read
        .is_some_and(|before_read| before_read(&task.path))
    {
        cancel(shared);
    }
    let (directory, status) = match open(task.source) {
        Ok(opened) => opened,
        Err(failure) => return fail(failure),
    };
    let own = Size {
        files: 0,
        ..file_size(&status)
    };
    let names = match directory.names() {
        Ok(names) => names,
        Err(error) => return fail(error.into()),
    };
    let directory = Arc::new(directory);
    let mut listed = Listed {
        own,
        incomplete: false,
        children: Vec::new(),
        subdirectories: Vec::new(),
    };
    for (index, name) in names.into_iter().enumerate() {
        if index % CANCEL_LOOK_EVERY == 0 && shared.cancelled.load(Ordering::Acquire) {
            listed.incomplete = true;
            break;
        }
        read_entry(shared, &task.path, node, &directory, name, &mut listed);
    }
    let Listed {
        own,
        incomplete,
        children,
        subdirectories,
    } = listed;
    count(&shared.counts.directories, 1);
    count(&shared.counts.files, own.files);
    count(&shared.counts.allocated, own.allocated);
    let ids = lock(&shared.tree).record(node, own, incomplete, children);
    subdirectories
        .into_iter()
        .filter_map(|(index, source, path)| {
            ids.get(index).map(|node| Task {
                source,
                node: *node,
                path,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holds the reading of the folder `slow` a moment.
    fn slow(path: &Path) -> bool {
        if path.file_name() == Some(std::ffi::OsStr::new("slow")) {
            std::thread::sleep(Duration::from_millis(300));
        }
        false
    }

    /// Whether a thread is in the reading of `settling`, for its test only.
    static IN_SETTLING: AtomicBool = AtomicBool::new(false);

    /// Holds the reading of the folder `settling` a moment, said.
    fn settling(path: &Path) -> bool {
        if path.file_name() == Some(std::ffi::OsStr::new("settling")) {
            IN_SETTLING.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
        }
        false
    }

    fn no_mounts(_root: &Path) -> (BTreeSet<PathBuf>, Option<String>) {
        (BTreeSet::new(), None)
    }

    #[test]
    fn an_analysis_dropped_is_stopped() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::create_dir(temporary.path().join("slow")).unwrap();
        let scan =
            UsageScan::start_with(temporary.path(), NonZeroUsize::MIN, no_mounts, Some(slow))
                .unwrap();
        let shared = Arc::clone(&scan.shared);
        drop(scan);
        assert_eq!(shared.outcome.get(), Some(&Outcome::Cancelled));
    }

    #[test]
    fn a_cancelled_analysis_settles_once_its_last_reading_ends() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::create_dir(temporary.path().join("settling")).unwrap();
        let scan = UsageScan::start_with(
            temporary.path(),
            NonZeroUsize::MIN,
            no_mounts,
            Some(settling),
        )
        .unwrap();
        // `settling` held in its reading.
        let started = std::time::Instant::now();
        while !IN_SETTLING.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(20) {
            std::thread::yield_now();
        }
        scan.cancel();
        assert_eq!(scan.wait(Duration::ZERO), Some(Outcome::Cancelled));
        assert!(!scan.settled(), "`slow` still read");
        while !scan.settled() && started.elapsed() < Duration::from_secs(20) {
            std::thread::yield_now();
        }
        assert!(scan.settled());
    }

    #[test]
    fn one_thread_per_processor_at_most_eight() {
        let threads: Vec<usize> = [0, 1, 4, 8, 64]
            .map(|processors| threads_for(processors).get())
            .into();
        assert_eq!(threads, [1, 1, 4, 8, 8]);
    }

    #[test]
    fn an_analysis_running_has_no_outcome_yet() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::create_dir(temporary.path().join("slow")).unwrap();
        let scan =
            UsageScan::start_with(temporary.path(), NonZeroUsize::MIN, no_mounts, Some(slow))
                .unwrap();
        assert_eq!(scan.wait(Duration::from_millis(10)), None);
        assert_eq!(scan.wait(Duration::from_secs(20)), Some(Outcome::Complete));
    }

    #[test]
    fn what_a_file_takes_is_its_length_when_its_blocks_are_unknown() {
        let status = EntryStatus {
            kind: EntryType::File,
            identity: FileIdentity::new(1, 1),
            last_change: std::time::SystemTime::UNIX_EPOCH,
            modified: std::time::SystemTime::UNIX_EPOCH,
            changed: std::time::SystemTime::UNIX_EPOCH,
            size: 1000,
            allocated: None,
            links: 1,
            owner: 0,
            sticky: false,
            locked: false,
            mount_root: None,
        };
        assert_eq!(file_size(&status).allocated, 1000);
        let known = EntryStatus {
            allocated: Some(4096),
            ..status
        };
        assert_eq!(file_size(&known).allocated, 4096);
        assert_eq!(file_size(&known).apparent, 1000);
    }

    #[test]
    fn a_folder_opened_again_is_the_one_listed_and_no_mount() {
        let listed = FileIdentity::new(1, 1);
        let status = |identity, mount_root| EntryStatus {
            kind: EntryType::Directory,
            identity,
            last_change: std::time::SystemTime::UNIX_EPOCH,
            modified: std::time::SystemTime::UNIX_EPOCH,
            changed: std::time::SystemTime::UNIX_EPOCH,
            size: 0,
            allocated: None,
            links: 2,
            owner: 0,
            sticky: false,
            locked: false,
            mount_root,
        };
        assert!(still_listed(&status(listed, Some(false)), listed));
        assert!(!still_listed(&status(listed, Some(true)), listed));
        assert!(!still_listed(
            &status(FileIdentity::new(1, 2), Some(false)),
            listed
        ));
    }
}
