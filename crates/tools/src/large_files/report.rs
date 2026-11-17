//! What a search found: the files above its floor, the largest on disk
//! first, each known by an identifier valid in this report only
//! (invariant 1).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use okapi_engine::report_nonce;
use okapi_platform::{FileIdentity, Trash};

use super::kind::FileKind;
use crate::listing::{Found, Root, SearchStats, Seen};

/// The identifier of a file, valid only in the report that gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId {
    report: u64,
    index: usize,
}

/// A large file, as the search read it.
#[derive(Debug, PartialEq, Eq)]
pub struct LargeFile {
    id: FileId,
    path: PathBuf,
    /// The search root it lies below, by its rank in the report's roots.
    root: usize,
    status: Seen,
    kind: FileKind,
}

impl LargeFile {
    #[must_use]
    pub const fn id(&self) -> FileId {
        self.id
    }

    /// Its canonical path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What it was when the search read it: its disk, and what showing it
    /// in its folder checks it still is.
    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.status.identity
    }

    /// Its size in bytes.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.status.size
    }

    /// The bytes its blocks take on disk, the most the trash frees once
    /// emptied: less than its size for a sparse file.
    #[must_use]
    pub const fn on_disk(&self) -> u64 {
        self.status.on_disk
    }

    /// When its content last changed.
    #[must_use]
    pub const fn modified(&self) -> SystemTime {
        self.status.modified
    }

    #[must_use]
    pub const fn kind(&self) -> FileKind {
        self.kind
    }

    /// How many names it has: one with others, outside the search or not,
    /// frees nothing when it goes.
    #[must_use]
    pub const fn links(&self) -> u64 {
        self.status.links
    }

    /// Whether moving it away frees what it takes: it has no other name.
    #[must_use]
    pub const fn frees(&self) -> bool {
        self.status.links == 1
    }

    /// Whether it lies on the device of `bin`: the trash takes only what
    /// lies on its own.
    #[must_use]
    pub fn lies_on(&self, bin: &Trash) -> bool {
        self.status.identity.device() == bin.device()
    }

    pub(crate) const fn root(&self) -> usize {
        self.root
    }

    pub(crate) const fn seen(&self) -> &Seen {
        &self.status
    }
}

/// What a search found. It cannot be built outside this crate nor
/// serialized: moving files away takes the report and identifiers of its
/// files (invariant 1).
#[derive(Debug)]
pub struct LargeFileReport {
    id: u64,
    files: Vec<LargeFile>,
    roots: Vec<Root>,
    stats: SearchStats,
}

impl LargeFileReport {
    /// Numbers the files `found`, the largest on disk first, then by path.
    pub(crate) fn new(found: Vec<Found>, roots: Vec<Root>, stats: SearchStats) -> Self {
        let id = report_nonce();
        let mut found = found;
        found.sort_by(|left, right| {
            right
                .status
                .on_disk
                .cmp(&left.status.on_disk)
                .then_with(|| left.path.cmp(&right.path))
        });
        let files = found
            .into_iter()
            .enumerate()
            .map(|(index, found)| LargeFile {
                id: FileId { report: id, index },
                kind: FileKind::of(&found.path),
                path: found.path,
                root: found.root,
                status: found.status,
            })
            .collect();
        Self {
            id,
            files,
            roots,
            stats,
        }
    }

    /// The files, the largest on disk first.
    #[must_use]
    pub fn files(&self) -> &[LargeFile] {
        &self.files
    }

    /// The folders searched, where they really are: one given twice is
    /// listed once.
    #[must_use = "the roots are only read"]
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().map(|root| root.path.as_path())
    }

    #[must_use]
    pub const fn stats(&self) -> &SearchStats {
        &self.stats
    }

    /// What the files take on disk, all together.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.files
            .iter()
            .map(LargeFile::on_disk)
            .fold(0, u64::saturating_add)
    }

    /// The file with this identifier, if it comes from this report.
    #[must_use]
    pub fn find(&self, id: FileId) -> Option<&LargeFile> {
        if id.report != self.id {
            return None;
        }
        self.files.get(id.index)
    }

    pub(crate) fn root(&self, index: usize) -> Option<&Root> {
        self.roots.get(index)
    }
}
