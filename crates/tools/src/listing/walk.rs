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
use okapi_platform::{Directory, EntryStatus, EntryType, FileIdentity};

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

/// What the listing honours, as the tool gives it.
pub(crate) struct Listing<'a> {
    pub(crate) min_size: u64,
    pub(crate) exclusions: &'a Exclusions,
    pub(crate) cancelled: &'a AtomicBool,
    /// The plain files listed so far, for the progress.
    pub(crate) files: &'a AtomicU64,
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
            tracing::debug!(path = %path.display(), %error, "folder searched unreadable");
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
        let opened = task.parent.open_child(&task.name).and_then(|directory| {
            let status = directory.status()?;
            Ok((directory, status))
        });
        match opened {
            Ok((_, status)) if status.mount_root == Some(true) => {
                // Mounted on since it was listed.
                self.stats.other_mounts += 1;
                tracing::debug!(path = %task.path.display(), "mount not entered");
                None
            }
            Ok((directory, status)) if status.identity == task.expected => Some(directory),
            Ok(_) => {
                self.stats.changed_directories += 1;
                tracing::debug!(path = %task.path.display(), "folder changed since listed: not read");
                None
            }
            // Removed meanwhile: nothing to search there.
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
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
                tracing::debug!(path = %path.display(), %error, "folder unreadable");
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

    /// Keeps the plain file at `path`, read as `status`, if it could have a
    /// copy.
    fn file(&mut self, path: PathBuf, status: &EntryStatus) {
        // A file mounted on another (bind) may lie on another filesystem
        // than its root's, which judges every copy found below it.
        if status.mount_root == Some(true) {
            self.stats.other_mounts += 1;
            tracing::debug!(path = %path.display(), "mounted file not kept");
            return;
        }
        self.stats.files += 1;
        self.listing.files.fetch_add(1, Ordering::Relaxed);
        // An empty file is never a copy worth moving: it frees nothing, and
        // an empty file often marks something (`__init__.py`).
        if status.size < self.listing.min_size.max(1) {
            self.stats.below_minimum += 1;
        } else if self.listing.exclusions.excludes(&path) {
            self.stats.excluded += 1;
        } else if !self.seen.insert(status.identity) {
            // Another name of a file already listed: removing one would free
            // nothing, and a file is never its own copy.
            self.stats.other_names += 1;
        } else {
            self.found.push(Found {
                path,
                root: self.root,
                status: Seen::of(status),
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

    #[test]
    fn a_walk_cancelled_reads_no_further() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(temporary.path().join("a"), "a").unwrap();
        let exclusions = Exclusions::none();
        let cancelled = AtomicBool::new(true);
        let files = AtomicU64::new(0);
        let listing = Listing {
            min_size: SIZE,
            exclusions: &exclusions,
            cancelled: &cancelled,
            files: &files,
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
    fn a_file_mounted_on_another_is_never_kept() {
        let exclusions = Exclusions::none();
        let cancelled = AtomicBool::new(false);
        let files = AtomicU64::new(0);
        let listing = Listing {
            min_size: SIZE,
            exclusions: &exclusions,
            cancelled: &cancelled,
            files: &files,
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
        walk.file(PathBuf::from("/root/mounted"), &status(1, Some(true)));
        walk.file(PathBuf::from("/root/plain"), &status(2, Some(false)));
        // A system that cannot tell keeps it: the table of mounts is read.
        walk.file(PathBuf::from("/root/untold"), &status(3, None));
        let kept: Vec<&Path> = found.iter().map(|file| file.path.as_path()).collect();
        assert_eq!(kept, [Path::new("/root/plain"), Path::new("/root/untold")]);
        assert_eq!(stats.other_mounts, 1);
        assert_eq!(stats.files, 2);
    }
}
