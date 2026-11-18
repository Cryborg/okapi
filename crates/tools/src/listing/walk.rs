//! One root's walk: read down from the root one folder at a time, never
//! through a link nor into another mount, it keeps the plain files a tool
//! may offer.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use okapi_engine::Exclusions;
use okapi_platform::{Directory, EntryStatus, EntryType, FileIdentity, Unreached};

// A file in the trash is on its way out, never one to offer.
use super::{Found, SearchStats, Seen, trash_folder};

/// How many entries of a folder are read between two looks at the
/// cancellation.
const CANCEL_LOOK_EVERY: usize = 1024;
/// What makes a folder a project: its files are never offered (a
/// repository, its build, its dependencies), unless the user chose it as a
/// folder to search.
const PROJECT_MARK: &str = ".git";
/// The dependencies a project keeps beside its code, never offered even
/// outside a repository.
const DEPENDENCIES: &str = "node_modules";
const HIDDEN_PREFIX: u8 = b'.';

/// The search was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cancelled;

/// What a file's size is, as compared with the smallest kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    all(not(test), not(all(feature = "duplicates", feature = "large-files"))),
    expect(dead_code, reason = "each tool measures one way")
)]
pub(crate) enum Measure {
    /// Its length: what the duplicates compare.
    Apparent,
    /// What its blocks take on disk: the most the trash frees once emptied.
    OnDisk,
}

/// Whose files are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    all(not(test), not(all(feature = "duplicates", feature = "large-files"))),
    expect(dead_code, reason = "each tool keeps the files of one rule")
)]
pub(crate) enum Owners {
    All,
    /// This user's only, none when the user is unknown.
    Only(Option<u32>),
}

impl Owners {
    fn keep(self, owner: u32) -> bool {
        match self {
            Self::All => true,
            Self::Only(user) => user == Some(owner),
        }
    }
}

/// What the listing counts as it goes, for the progress.
#[derive(Debug, Default)]
pub(crate) struct Counts {
    /// Plain files listed, of any size.
    pub(crate) files: AtomicU64,
    /// Files kept.
    pub(crate) kept: AtomicU64,
}

/// What the listing honours, as the tool gives it.
pub(crate) struct Listing<'a> {
    pub(crate) min_size: u64,
    pub(crate) measure: Measure,
    pub(crate) owners: Owners,
    pub(crate) exclusions: &'a Exclusions,
    pub(crate) cancelled: &'a AtomicBool,
    pub(crate) counts: &'a Counts,
}

/// What every root's walk honours.
pub(super) struct Scope<'a> {
    pub(super) listing: &'a Listing<'a>,
    /// The folders searched: each is walked from its own root only.
    pub(super) roots: &'a HashSet<FileIdentity>,
    /// The mount points below the roots, read once for each: never touched,
    /// a mount that stopped answering would hold the search for good.
    pub(super) mounts: &'a BTreeSet<PathBuf>,
}

/// A folder to read: opened from its parent, it must still be the one
/// listed there.
struct Task {
    parent: Rc<Directory>,
    name: OsString,
    path: PathBuf,
    expected: FileIdentity,
}

/// One root's listing.
struct Walk<'a, 'b> {
    listing: &'b Listing<'a>,
    roots: &'b HashSet<FileIdentity>,
    mounts: &'b BTreeSet<PathBuf>,
    root: usize,
    device: u64,
    stats: &'b mut SearchStats,
    seen: &'b mut HashSet<FileIdentity>,
    found: &'b mut Vec<Found>,
}

/// Lists the root `root` (its rank among the roots), opened as `directory`
/// at `path`, into `found`. A file met again under another name, in this
/// root or an earlier one (`seen`), is listed once.
pub(super) fn walk(
    scope: &Scope<'_>,
    root: usize,
    directory: Directory,
    path: &Path,
    stats: &mut SearchStats,
    seen: &mut HashSet<FileIdentity>,
    found: &mut Vec<Found>,
) -> Result<(), Cancelled> {
    let device = match directory.status() {
        Ok(own) => own.identity.device(),
        Err(error) => {
            stats.unreadable += 1;
            tracing::warn!(path = %path.display(), %error, "folder searched unreadable");
            return Ok(());
        }
    };
    let mut walk = Walk {
        listing: scope.listing,
        roots: scope.roots,
        mounts: scope.mounts,
        root,
        device,
        stats,
        seen,
        found,
    };
    let mut stack = Vec::new();
    walk.read(&Rc::new(directory), path, true, &mut stack)?;
    while let Some(task) = stack.pop() {
        if scope.listing.cancelled.load(Ordering::Acquire) {
            return Err(Cancelled);
        }
        if let Some(directory) = walk.open(&task) {
            walk.read(&Rc::new(directory), &task.path, false, &mut stack)?;
        }
    }
    Ok(())
}

impl Walk<'_, '_> {
    /// Opens the task's folder, checked to be the one listed; `None`, and
    /// counted, when it cannot be.
    fn open(&mut self, task: &Task) -> Option<Directory> {
        match okapi_platform::enter(&task.parent, &task.name, task.expected) {
            Ok(Some((directory, _))) => Some(directory),
            // Removed meanwhile: nothing to search there.
            Ok(None) => {
                tracing::debug!(path = %task.path.display(), "folder gone since listed");
                None
            }
            Err(Unreached::Changed) => {
                self.stats.changed_directories += 1;
                tracing::debug!(path = %task.path.display(), "folder changed since listed: not read");
                None
            }
            Err(Unreached::Io(error)) => {
                self.stats.unreadable += 1;
                tracing::debug!(path = %task.path.display(), %error, "folder unreadable");
                None
            }
        }
    }

    /// Reads the folder `directory` at `path`: its files kept, its folders
    /// pushed on `stack`. A project's folder is left whole, unless it is the
    /// root the user chose.
    fn read(
        &mut self,
        directory: &Rc<Directory>,
        path: &Path,
        root: bool,
        stack: &mut Vec<Task>,
    ) -> Result<(), Cancelled> {
        let names = match directory.names() {
            Ok(names) => names,
            Err(error) => {
                self.stats.unreadable += 1;
                // A folder searched that cannot be read is no folder found
                // empty: it is told. One below it is counted.
                if root {
                    tracing::warn!(path = %path.display(), %error, "folder searched unreadable");
                } else {
                    tracing::debug!(path = %path.display(), %error, "folder unreadable");
                }
                return Ok(());
            }
        };
        self.stats.directories += 1;
        if !root && names.iter().any(|name| name == PROJECT_MARK) {
            self.stats.projects += 1;
            tracing::debug!(path = %path.display(), "project: not searched");
            return Ok(());
        }
        for (index, name) in names.into_iter().enumerate() {
            if index % CANCEL_LOOK_EVERY == 0 && self.listing.cancelled.load(Ordering::Acquire) {
                return Err(Cancelled);
            }
            self.entry(directory, path, name, stack);
        }
        Ok(())
    }

    /// Judges the entry `name` of `directory`, at `parent`.
    fn entry(
        &mut self,
        directory: &Rc<Directory>,
        parent: &Path,
        name: OsString,
        stack: &mut Vec<Task>,
    ) {
        let path = parent.join(&name);
        // A mount point is never even read: one that stopped answering would
        // hold the search for good.
        if self.mounts.contains(&path) {
            self.stats.other_mounts += 1;
            tracing::debug!(path = %path.display(), "mount not entered");
            return;
        }
        let status = match directory.status_of(&name) {
            Ok(status) => status,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                self.stats.unreadable += 1;
                tracing::debug!(path = %path.display(), %error, "entry unreadable");
                return;
            }
        };
        match status.kind {
            EntryType::Directory => {
                if let Some(expected) = self.folder(&name, &path, &status) {
                    stack.push(Task {
                        parent: Rc::clone(directory),
                        name,
                        path,
                        expected,
                    });
                }
            }
            EntryType::File => self.file(path, &status),
            // Never followed, never a copy: a link, a socket, a pipe.
            EntryType::Link | EntryType::Other => {}
        }
    }

    /// The identity a subfolder must still have when opened, `None` when it
    /// is not searched (and counted).
    fn folder(
        &mut self,
        name: &OsString,
        path: &Path,
        status: &EntryStatus,
    ) -> Option<FileIdentity> {
        let reason = if self.roots.contains(&status.identity) {
            "searched as its own root"
        } else if name.as_encoded_bytes().first() == Some(&HIDDEN_PREFIX) {
            self.stats.hidden_directories += 1;
            "hidden"
        } else if name == DEPENDENCIES {
            self.stats.dependency_directories += 1;
            "dependencies"
        } else if trash_folder(name) {
            self.stats.trash_directories += 1;
            "trash"
        } else if self.listing.exclusions.excludes(path)
            || self.listing.exclusions.excludes_identity(status.identity)
        {
            self.stats.excluded += 1;
            "excluded"
        } else if status.other_mount(self.device) {
            self.stats.other_mounts += 1;
            "mount"
        } else {
            return Some(status.identity);
        };
        tracing::debug!(path = %path.display(), reason, "folder not searched");
        None
    }

    /// Keeps the plain file at `path`, read as `status`, if the tool may
    /// offer it.
    fn file(&mut self, path: PathBuf, status: &EntryStatus) {
        // A file mounted on another (bind) may lie on another filesystem
        // than its root's, which judges every file found below it.
        if status.mount_root == Some(true) {
            self.stats.other_mounts += 1;
            tracing::debug!(path = %path.display(), "mounted file not kept");
            return;
        }
        self.stats.files += 1;
        self.listing.counts.files.fetch_add(1, Ordering::Relaxed);
        let seen = Seen::of(status);
        let size = match self.listing.measure {
            Measure::Apparent => seen.size,
            Measure::OnDisk => seen.on_disk,
        };
        // An empty file is never worth moving: it frees nothing, and an
        // empty file often marks something (`__init__.py`).
        if size < self.listing.min_size.max(1) {
            self.stats.below_minimum += 1;
        } else if self.listing.exclusions.excludes(&path) {
            self.stats.excluded += 1;
        } else if !self.listing.owners.keep(status.owner) {
            self.stats.other_owners += 1;
            tracing::debug!(path = %path.display(), owner = status.owner, "file of another user not kept");
        } else if !self.seen.insert(status.identity) {
            // Another name of a file already listed: removing one would free
            // nothing, and a file is never its own copy.
            self.stats.other_names += 1;
        } else {
            self.listing.counts.kept.fetch_add(1, Ordering::Relaxed);
            self.found.push(Found {
                path,
                root: self.root,
                status: seen,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    const SIZE: u64 = 4096;

    fn status(inode: u64, mount_root: Option<bool>) -> EntryStatus {
        EntryStatus {
            kind: EntryType::File,
            identity: FileIdentity::new(1, inode),
            last_change: SystemTime::UNIX_EPOCH,
            modified: SystemTime::UNIX_EPOCH,
            changed: SystemTime::UNIX_EPOCH,
            links: 1,
            owner: 0,
            sticky: false,
            locked: false,
            size: SIZE,
            allocated: None,
            mount_root,
        }
    }

    /// The roots and mounts of a walk that has none below its root.
    fn scope<'a>(listing: &'a Listing<'a>) -> Scope<'a> {
        static NO_ROOTS: std::sync::LazyLock<HashSet<FileIdentity>> =
            std::sync::LazyLock::new(HashSet::new);
        static NO_MOUNTS: BTreeSet<PathBuf> = BTreeSet::new();
        Scope {
            listing,
            roots: &NO_ROOTS,
            mounts: &NO_MOUNTS,
        }
    }

    /// What a walk keeping files by `measure` and `owners` keeps of `files`,
    /// what it counted, and its count of the files kept.
    fn walked(
        measure: Measure,
        owners: Owners,
        files: &[(&str, EntryStatus)],
    ) -> (Vec<PathBuf>, SearchStats, u64) {
        let exclusions = Exclusions::none();
        let cancelled = AtomicBool::new(false);
        let counts = Counts::default();
        let listing = Listing {
            min_size: SIZE,
            measure,
            owners,
            exclusions: &exclusions,
            cancelled: &cancelled,
            counts: &counts,
        };
        let scope = scope(&listing);
        let mut stats = SearchStats::default();
        let mut seen = HashSet::new();
        let mut found = Vec::new();
        let mut walk = Walk {
            listing: scope.listing,
            roots: scope.roots,
            mounts: scope.mounts,
            root: 0,
            device: 1,
            stats: &mut stats,
            seen: &mut seen,
            found: &mut found,
        };
        for (path, status) in files {
            walk.file(PathBuf::from(path), status);
        }
        // The progress counts what the stats count.
        assert_eq!(counts.files.load(Ordering::Relaxed), stats.files);
        let paths = found.into_iter().map(|file| file.path).collect();
        (paths, stats, counts.kept.load(Ordering::Relaxed))
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_walk_cancelled_reads_no_further() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(temporary.path().join("a"), "a").unwrap();
        let exclusions = Exclusions::none();
        let cancelled = AtomicBool::new(true);
        let counts = Counts::default();
        let listing = Listing {
            min_size: SIZE,
            measure: Measure::Apparent,
            owners: Owners::All,
            exclusions: &exclusions,
            cancelled: &cancelled,
            counts: &counts,
        };
        let mut stats = SearchStats::default();
        let mut found = Vec::new();
        let walked = walk(
            &scope(&listing),
            0,
            Directory::open(temporary.path()).unwrap(),
            temporary.path(),
            &mut stats,
            &mut HashSet::new(),
            &mut found,
        );
        assert_eq!(walked, Err(Cancelled));
        assert!(found.is_empty());
        assert_eq!(stats.files, 0);
    }

    #[test]
    fn a_folder_refused_is_unreadable_and_one_replaced_changed() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        std::fs::create_dir(root.join("closed")).unwrap();
        std::fs::create_dir(root.join("other")).unwrap();
        std::os::unix::fs::symlink(root.join("other"), root.join("link")).unwrap();
        std::fs::write(root.join("file"), b"x").unwrap();
        let identity = |name: &str| {
            okapi_platform::file_identity(&std::fs::symlink_metadata(root.join(name)).unwrap())
                .unwrap()
        };
        let parent = Rc::new(Directory::open(root).unwrap());
        let task = |name: &str, expected| Task {
            parent: Rc::clone(&parent),
            name: OsString::from(name),
            path: root.join(name),
            expected,
        };
        let refused = task("closed", identity("closed"));
        // Other folders than the ones listed at those names.
        let replaced = [
            task("closed", identity("other")),
            task("other", identity("closed")),
            // A link to the folder listed, and a file, in its place.
            task("link", identity("other")),
            task("file", identity("other")),
        ];
        let exclusions = Exclusions::none();
        let cancelled = AtomicBool::new(false);
        let counts = Counts::default();
        let listing = Listing {
            min_size: SIZE,
            measure: Measure::Apparent,
            owners: Owners::All,
            exclusions: &exclusions,
            cancelled: &cancelled,
            counts: &counts,
        };
        let scope = scope(&listing);
        let mut stats = SearchStats::default();
        let mut walk = Walk {
            listing: scope.listing,
            roots: scope.roots,
            mounts: scope.mounts,
            root: 0,
            device: 1,
            stats: &mut stats,
            seen: &mut HashSet::new(),
            found: &mut Vec::new(),
        };
        // Removed meanwhile: nothing counted.
        assert!(walk.open(&task("absent", identity("other"))).is_none());
        assert_eq!(
            (walk.stats.changed_directories, walk.stats.unreadable),
            (0, 0)
        );
        for task in &replaced {
            assert!(walk.open(task).is_none());
        }
        assert_eq!(
            (walk.stats.changed_directories, walk.stats.unreadable),
            (4, 0)
        );
        std::fs::set_permissions(root.join("closed"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let opened = walk.open(&refused);
        std::fs::set_permissions(root.join("closed"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        if opened.is_some() {
            eprintln!("skipped: root enters every folder");
            return;
        }
        assert_eq!((stats.changed_directories, stats.unreadable), (4, 1));
    }

    #[test]
    fn a_file_mounted_on_another_is_never_kept() {
        let (kept, stats, count) = walked(
            Measure::Apparent,
            Owners::All,
            &[
                ("/root/mounted", status(1, Some(true))),
                ("/root/plain", status(2, Some(false))),
                // A system that cannot tell keeps it: the table of mounts
                // is read.
                ("/root/untold", status(3, None)),
            ],
        );
        assert_eq!(kept, paths(&["/root/plain", "/root/untold"]));
        assert_eq!(stats.other_mounts, 1);
        assert_eq!(stats.files, 2);
        assert_eq!(count, 2);
    }

    #[test]
    fn only_the_files_of_the_owners_kept_are_and_the_others_are_counted() {
        const USER: u32 = 1000;
        let owned = |inode, owner| EntryStatus {
            owner,
            ..status(inode, Some(false))
        };
        let files = [
            ("/root/mine", owned(1, USER)),
            ("/root/theirs", owned(2, USER + 1)),
            ("/root/also_mine", owned(3, USER)),
        ];
        let (kept, stats, count) = walked(Measure::Apparent, Owners::Only(Some(USER)), &files);
        assert_eq!(kept, paths(&["/root/mine", "/root/also_mine"]));
        assert_eq!((stats.other_owners, stats.files, count), (1, 3, 2));
        let (kept, stats, _) = walked(Measure::Apparent, Owners::All, &files);
        assert_eq!(kept.len(), 3);
        assert_eq!(stats.other_owners, 0);
        // An unknown user keeps nothing.
        let (kept, stats, count) = walked(Measure::Apparent, Owners::Only(None), &files);
        assert_eq!(kept, Vec::<PathBuf>::new());
        assert_eq!((stats.other_owners, count), (3, 0));
    }

    #[test]
    fn the_smallest_kept_is_measured_as_the_tool_says() {
        let sized = |inode, size, allocated| EntryStatus {
            size,
            allocated: Some(allocated),
            ..status(inode, Some(false))
        };
        let files = [
            // Long, sparse: little on disk.
            ("/root/sparse", sized(1, 4 * SIZE, SIZE / 2)),
            // Short, its blocks rounded up.
            ("/root/full", sized(2, SIZE - 1, 2 * SIZE)),
        ];
        let (kept, stats, _) = walked(Measure::Apparent, Owners::All, &files);
        assert_eq!(kept, paths(&["/root/sparse"]));
        assert_eq!(stats.below_minimum, 1);
        let (kept, stats, _) = walked(Measure::OnDisk, Owners::All, &files);
        assert_eq!(kept, paths(&["/root/full"]));
        assert_eq!(stats.below_minimum, 1);
    }
}
