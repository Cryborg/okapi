//! What a search found: groups of copies of one content, each copy known by
//! an identifier valid in this report only (invariant 1).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use okapi_engine::report_nonce;
use okapi_platform::{EntryStatus, FileIdentity};

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

/// What the search read of a file, and what it must still be to be moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) identity: FileIdentity,
    pub(crate) size: u64,
    pub(crate) modified: SystemTime,
    pub(crate) changed: SystemTime,
    pub(crate) links: u64,
    pub(crate) owner: u32,
}

impl Seen {
    pub(crate) const fn of(status: &EntryStatus) -> Self {
        Self {
            identity: status.identity,
            size: status.size,
            modified: status.modified,
            changed: status.changed,
            links: status.links,
            owner: status.owner,
        }
    }

    /// Whether `status` is still this file, unchanged: the same object, of
    /// the same size, dates and owner. A write moves the inode's change
    /// date, even one that sets the modification date back; only two writes
    /// within one tick of a coarse clock could go unseen. FAT keeps no change
    /// date of its own (a date set back sets it back too): the size is the
    /// little this catches there.
    pub(crate) fn unchanged(&self, status: &EntryStatus) -> bool {
        status.identity == self.identity
            && status.changed == self.changed
            && status.size == self.size
            && status.modified == self.modified
            && status.owner == self.owner
    }
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

/// A folder searched, as it was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Root {
    pub(crate) path: PathBuf,
    pub(crate) identity: FileIdentity,
    /// Whether its filesystem may show the files of another under other
    /// identities (FUSE, network, stacked, or unknown): a copy found there
    /// may be the very file it seems to duplicate.
    pub(crate) may_alias: bool,
    /// Whether its filesystem keeps no change date of its own (FAT, or
    /// unknown): a copy written there then dated back looks unchanged.
    pub(crate) undated: bool,
}

/// What the search met, beyond its groups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SearchStats {
    pub directories: u64,
    /// Plain files listed, of any size (but one mounted on another).
    pub files: u64,
    pub below_minimum: u64,
    /// Files met again under another name (hard links): one copy each.
    pub other_names: u64,
    /// Folders, or entries of one, that could not be read.
    pub unreadable: u64,
    /// Folders replaced, or turned into links, while listed: not read.
    pub changed_directories: u64,
    pub hidden_directories: u64,
    /// Folders holding a `.git` (but a folder the user chose): a project's
    /// files are never offered.
    pub projects: u64,
    pub dependency_directories: u64,
    /// Trash folders: what lies there is on its way out.
    pub trash_directories: u64,
    /// Mounts not entered: folders, or files mounted on another.
    pub other_mounts: u64,
    pub excluded: u64,
    /// Files that changed, went, or could not be read while compared: never
    /// in a group.
    pub dropped: u64,
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

/// A copy found, before it has its identifier.
#[derive(Debug)]
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    pub(crate) root: usize,
    pub(crate) status: Seen,
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use okapi_platform::EntryType;

    use super::*;

    fn status() -> EntryStatus {
        let date = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        EntryStatus {
            kind: EntryType::File,
            identity: FileIdentity::new(1, 2),
            last_change: date,
            modified: date,
            changed: date,
            links: 1,
            owner: 1000,
            sticky: false,
            locked: false,
            size: 10,
            allocated: None,
            mount_root: Some(false),
        }
    }

    #[test]
    fn a_file_is_unchanged_only_when_all_that_was_read_is_alike() {
        let seen = Seen::of(&status());
        assert!(seen.unchanged(&status()));
        let resized = EntryStatus {
            size: 11,
            ..status()
        };
        assert!(!seen.unchanged(&resized), "cut or grown");
        let redated = EntryStatus {
            modified: status().modified + Duration::from_secs(2),
            ..status()
        };
        // No filesystem known moves one date without the other: a guard.
        assert!(!seen.unchanged(&redated), "written, its change date kept");
        let replaced = EntryStatus {
            identity: FileIdentity::new(1, 3),
            ..status()
        };
        assert!(!seen.unchanged(&replaced), "another file");
        let changed = EntryStatus {
            changed: status().changed + Duration::from_secs(1),
            ..status()
        };
        assert!(!seen.unchanged(&changed), "written, its date set back");
        let given = EntryStatus {
            owner: 1001,
            ..status()
        };
        assert!(!seen.unchanged(&given), "given to another");
    }
}
