//! What a search found: groups of copies of one content, each copy known by
//! an identifier valid in this report only (invariant 1).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use okapi_engine::report_nonce;
use okapi_platform::FileIdentity;

use crate::listing::{Found, Root, SearchStats, Seen};

/// The identifier of a copy, valid only in the report that gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CopyId {
    report: u64,
    index: usize,
}

/// A file of a group, as the search read it.
#[derive(Debug, PartialEq, Eq)]
pub struct FileCopy {
    id: CopyId,
    path: PathBuf,
    /// The search root it lies below, by its rank in the report's roots.
    root: usize,
    status: Seen,
}

impl FileCopy {
    #[must_use]
    pub const fn id(&self) -> CopyId {
        self.id
    }

    /// Its canonical path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.status.size
    }

    /// When its content last changed.
    #[must_use]
    pub const fn modified(&self) -> SystemTime {
        self.status.modified
    }

    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.status.identity
    }

    /// How many names the file has: one with others, outside the search or
    /// not, frees nothing when it goes.
    #[must_use]
    pub const fn links(&self) -> u64 {
        self.status.links
    }

    /// Whether moving it away frees its size: it has no other name.
    #[must_use]
    pub const fn frees(&self) -> bool {
        self.status.links == 1
    }

    pub(crate) const fn root(&self) -> usize {
        self.root
    }

    pub(crate) const fn seen(&self) -> &Seen {
        &self.status
    }
}

/// Copies of one content: two at least, each a different file.
#[derive(Debug, PartialEq, Eq)]
pub struct Group {
    size: u64,
    copies: Vec<FileCopy>,
}

impl Group {
    /// The size of each copy.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Its copies, by path.
    #[must_use]
    pub fn copies(&self) -> &[FileCopy] {
        &self.copies
    }

    /// What moving all its copies but one away frees at most: one is always
    /// kept, a copy of several names frees nothing.
    #[must_use]
    pub fn gain(&self) -> u64 {
        let freeing = self.copies.iter().filter(|copy| copy.frees()).count();
        let removable = if freeing == self.copies.len() {
            freeing - 1
        } else {
            freeing
        };
        self.size
            .saturating_mul(u64::try_from(removable).unwrap_or(u64::MAX))
    }
}

/// What a search found. It cannot be built outside this crate nor
/// serialized: moving copies away takes the report and identifiers of its
/// copies (invariant 1).
#[derive(Debug)]
pub struct DuplicateReport {
    id: u64,
    groups: Vec<Group>,
    /// Where each copy is, by the index of its identifier.
    locations: Vec<(usize, usize)>,
    roots: Vec<Root>,
    stats: SearchStats,
}

impl DuplicateReport {
    /// Numbers the copies of `groups`, each of two copies at least, and
    /// orders them: the groups by what they free, most first, then by size;
    /// the copies by path.
    pub(crate) fn new(groups: Vec<Vec<Found>>, roots: Vec<Root>, stats: SearchStats) -> Self {
        let id = report_nonce();
        let mut groups: Vec<Group> = groups
            .into_iter()
            .filter(|found| found.len() > 1)
            .map(|mut found| {
                found.sort_by(|left, right| left.path.cmp(&right.path));
                Group {
                    size: found.first().map_or(0, |copy| copy.status.size),
                    copies: found
                        .into_iter()
                        .map(|found| FileCopy {
                            id: CopyId {
                                report: id,
                                index: 0,
                            },
                            path: found.path,
                            root: found.root,
                            status: found.status,
                        })
                        .collect(),
                }
            })
            .collect();
        // The last key keeps the order from one search to the next: the
        // groups come out of a hash map.
        groups.sort_by(|left, right| {
            right
                .gain()
                .cmp(&left.gain())
                .then_with(|| right.size.cmp(&left.size))
                .then_with(|| {
                    left.copies
                        .first()
                        .map(|copy| &copy.path)
                        .cmp(&right.copies.first().map(|copy| &copy.path))
                })
        });
        let mut locations = Vec::new();
        for (group_index, group) in groups.iter_mut().enumerate() {
            for (copy_index, copy) in group.copies.iter_mut().enumerate() {
                copy.id.index = locations.len();
                locations.push((group_index, copy_index));
            }
        }
        Self {
            id,
            groups,
            locations,
            roots,
            stats,
        }
    }

    /// The groups, what they free most first.
    #[must_use]
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// The rank of the group of the copy `id`, if it is of this report.
    pub(crate) fn group_of(&self, id: CopyId) -> Option<usize> {
        self.find(id)?;
        self.locations.get(id.index).map(|&(group, _)| group)
    }

    pub(crate) fn root(&self, index: usize) -> Option<&Root> {
        self.roots.get(index)
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

    /// What moving away all copies but one of each group frees at most.
    #[must_use]
    pub fn gain(&self) -> u64 {
        self.groups
            .iter()
            .map(Group::gain)
            .fold(0, u64::saturating_add)
    }

    /// The copy with this identifier and its group, if the identifier comes
    /// from this report.
    #[must_use]
    pub fn find(&self, id: CopyId) -> Option<(&Group, &FileCopy)> {
        if id.report != self.id {
            return None;
        }
        let &(group, copy) = self.locations.get(id.index)?;
        let group = self.groups.get(group)?;
        Some((group, group.copies.get(copy)?))
    }
}
