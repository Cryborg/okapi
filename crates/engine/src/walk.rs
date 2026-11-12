use std::collections::BTreeSet;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use okapi_platform::FileIdentity;
use okapi_rules::MiscRules;

use crate::options::Exclusions;
use crate::paths::{is_absent, is_absent_kind};
use crate::protection::ProtectedRoots;
use crate::report::{EntryKind, Limit, ScanStats};

/// Reads the identity of a file from its metadata.
pub(crate) type IdentityReader = fn(&Metadata) -> Option<FileIdentity>;

/// What the scan reads from metadata beyond its type: the platform's readers in
/// production, stand-ins in tests (a test cannot set a file's status change
/// time back, nor fake a mount point).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Readers {
    pub(crate) identity: IdentityReader,
    /// When the object last changed, content or inode.
    pub(crate) last_change: fn(&Metadata) -> Option<SystemTime>,
}

impl Readers {
    pub(crate) const SYSTEM: Self = Self {
        identity: okapi_platform::file_identity,
        last_change: okapi_platform::last_change,
    };
}

/// How far a walk may go below one root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// Directory levels below the root.
    pub(crate) depth: usize,
    /// Entries visited below the root, offered or not.
    pub(crate) entries: usize,
}

impl Limits {
    pub(crate) const DEFAULT: Self = Self {
        depth: 8,
        entries: 250_000,
    };
}

/// An entry the walk can offer: settled, and safe to remove as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    pub(crate) last_change: SystemTime,
    pub(crate) kind: EntryKind,
    pub(crate) identity: FileIdentity,
}

/// What a walk honours, besides its root.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WalkContext<'a> {
    /// Entries modified after this instant are too recent to offer.
    pub(crate) cutoff: SystemTime,
    pub(crate) exclusions: &'a Exclusions,
    pub(crate) misc: &'a MiscRules,
    pub(crate) protected: &'a ProtectedRoots,
    /// The identities of every scan root: a root met below another one keeps
    /// its own entries and is never entered by the outer walk.
    pub(crate) roots: &'a BTreeSet<FileIdentity>,
    pub(crate) readers: Readers,
    pub(crate) limits: Limits,
}

/// Why a walk could not even list its root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootProblem {
    Unreadable(io::ErrorKind),
    /// It is no longer the directory that was checked.
    Replaced,
}

/// What a walk found below its root.
#[derive(Debug, Default)]
pub(crate) struct Walk {
    pub(crate) found: Vec<Found>,
    pub(crate) stats: ScanStats,
    pub(crate) limits_reached: Vec<Limit>,
    pub(crate) root_problem: Option<RootProblem>,
}

/// What resolving one entry gives: the entries it offers, and whether nothing
/// below it was held back, which is what makes removing it whole safe.
struct Resolved {
    found: Vec<Found>,
    complete: bool,
    size: u64,
}

impl Resolved {
    const fn held_back() -> Self {
        Self {
            found: Vec::new(),
            complete: false,
            size: 0,
        }
    }
}

struct Walker<'a> {
    context: WalkContext<'a>,
    device: u64,
    visited: usize,
    walk: Walk,
}

/// Offers what lies below `root`, never `root` itself, which must be a
/// canonical directory with identity `root_identity`.
///
/// An entry is offered when it is a regular file last changed at or before the
/// cutoff (content or inode, see [`Readers::last_change`]), or a directory last
/// changed at or before the cutoff whose whole contents are offered (it is then one entry). Anything else is held back and keeps its
/// parent open: a link, a special file, another device, a variable's directory
/// or one of its ancestors, another scan root, an unreadable or recent entry,
/// an excluded or protected name, or a limit reached.
///
/// No link is followed: every entry is judged by metadata read without
/// following links, and a directory is listed only if, once opened, it is
/// still the directory that was judged. A swap between the two reads cannot be
/// excluded by paths alone: cleaning checks every entry again.
pub(crate) fn walk(root: &Path, root_identity: FileIdentity, context: WalkContext<'_>) -> Walk {
    let mut walker = Walker {
        context,
        device: root_identity.device(),
        visited: 0,
        walk: Walk::default(),
    };
    match walker.open_directory(root, root_identity) {
        Ok(entries) => {
            let resolved = walker.children(entries, context.limits.depth);
            walker.walk.found = resolved.found;
        }
        Err(problem) => walker.walk.root_problem = Some(problem),
    }
    walker.walk
}

impl Walker<'_> {
    /// Lists `directory`, after checking that what was opened is still the
    /// directory of `identity` (not a link swapped in since it was judged).
    fn open_directory(
        &self,
        directory: &Path,
        identity: FileIdentity,
    ) -> Result<fs::ReadDir, RootProblem> {
        let entries =
            fs::read_dir(directory).map_err(|error| RootProblem::Unreadable(error.kind()))?;
        let opened = fs::symlink_metadata(directory)
            .map_err(|error| RootProblem::Unreadable(error.kind()))?;
        let unchanged =
            opened.is_dir() && (self.context.readers.identity)(&opened) == Some(identity);
        if unchanged {
            Ok(entries)
        } else {
            Err(RootProblem::Replaced)
        }
    }

    fn children(&mut self, entries: fs::ReadDir, depth_left: usize) -> Resolved {
        let mut resolved = Resolved {
            found: Vec::new(),
            complete: true,
            size: 0,
        };
        for entry in entries {
            let child = if let Ok(entry) = entry {
                self.entry(&entry.path(), depth_left)
            } else {
                self.walk.stats.unreadable += 1;
                Resolved::held_back()
            };
            resolved.complete &= child.complete;
            resolved.size = resolved.size.saturating_add(child.size);
            resolved.found.extend(child.found);
        }
        resolved
    }

    fn limit(&mut self, limit: Limit) -> Resolved {
        if !self.walk.limits_reached.contains(&limit) {
            self.walk.limits_reached.push(limit);
        }
        Resolved::held_back()
    }

    fn entry(&mut self, path: &Path, depth_left: usize) -> Resolved {
        self.visited += 1;
        if self.visited > self.context.limits.entries {
            return self.limit(Limit::Entries);
        }
        if self.context.exclusions.excludes(path) {
            self.walk.stats.excluded += 1;
            return Resolved::held_back();
        }
        if self.context.misc.protects(path) {
            self.walk.stats.protected_names += 1;
            return Resolved::held_back();
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            // Gone since it was listed: nothing to count, the parent stays open.
            Err(error) if is_absent(&error) => return Resolved::held_back(),
            Err(_) => {
                self.walk.stats.unreadable += 1;
                return Resolved::held_back();
            }
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            self.walk.stats.links += 1;
            return Resolved::held_back();
        }
        if !file_type.is_file() && !file_type.is_dir() {
            self.walk.stats.special_files += 1;
            return Resolved::held_back();
        }
        let (Some(modified), Some(identity)) = (
            (self.context.readers.last_change)(&metadata),
            (self.context.readers.identity)(&metadata),
        ) else {
            self.walk.stats.unreadable += 1;
            return Resolved::held_back();
        };
        if identity.device() != self.device {
            self.walk.stats.other_filesystems += 1;
            return Resolved::held_back();
        }
        // Another scan root met here (a file target below a directory root, a
        // nested root) keeps its own entries.
        if self.context.roots.contains(&identity) {
            self.walk.stats.reserved_directories += 1;
            return Resolved::held_back();
        }
        if file_type.is_file() {
            return self.file(path, &metadata, modified, identity);
        }
        if self.context.protected.is_protected_directory(identity) {
            self.walk.stats.reserved_directories += 1;
            return Resolved::held_back();
        }
        let Some(depth_left) = depth_left.checked_sub(1) else {
            return self.limit(Limit::Depth);
        };
        self.directory(path, modified, identity, depth_left)
    }

    fn file(
        &mut self,
        path: &Path,
        metadata: &Metadata,
        modified: SystemTime,
        identity: FileIdentity,
    ) -> Resolved {
        if modified > self.context.cutoff {
            self.walk.stats.recent += 1;
            return Resolved::held_back();
        }
        Resolved {
            found: vec![Found {
                path: path.to_path_buf(),
                size: metadata.len(),
                last_change: modified,
                kind: EntryKind::File,
                identity,
            }],
            complete: true,
            size: metadata.len(),
        }
    }

    fn directory(
        &mut self,
        path: &Path,
        modified: SystemTime,
        identity: FileIdentity,
        depth_left: usize,
    ) -> Resolved {
        let entries = match self.open_directory(path, identity) {
            Ok(entries) => entries,
            Err(RootProblem::Replaced) => {
                self.walk.stats.replaced += 1;
                return Resolved::held_back();
            }
            Err(RootProblem::Unreadable(error)) => {
                if !is_absent_kind(error) {
                    self.walk.stats.unreadable += 1;
                }
                return Resolved::held_back();
            }
        };
        let children = self.children(entries, depth_left);
        if !children.complete {
            return children;
        }
        // A directory changed recently is in use (an extraction in progress, a
        // lock taken with `mkdir`), even when everything in it is old: its old
        // entries are still offered one by one, the directory is not.
        if modified > self.context.cutoff {
            self.walk.stats.recent += 1;
            return Resolved {
                complete: false,
                ..children
            };
        }
        // Shown as old as its most recent descendant, which is still before
        // the cutoff.
        let last_change = children
            .found
            .iter()
            .map(|found| found.last_change)
            .fold(modified, SystemTime::max);
        Resolved {
            found: vec![Found {
                path: path.to_path_buf(),
                size: children.size,
                last_change,
                kind: EntryKind::Directory,
                identity,
            }],
            complete: true,
            size: children.size,
        }
    }
}
