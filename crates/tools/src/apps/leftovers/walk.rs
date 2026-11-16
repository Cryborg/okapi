//! What a folder holds, to its deepest entry, read through descriptors from
//! the folder opened (never a link, never another mount): how many entries,
//! what they take, and a digest of each one's inode, inode change and size.
//! Read at the analysis and again just before the move, the two must agree
//! (invariant 7). A modification date is no part of it: one in the future
//! would hide a write, and every write moves the inode's change anyway.

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::{Hash as _, Hasher as _};
use std::io;
use std::rc::Rc;

use okapi_engine::Exclusions;
use okapi_platform::{Directory, EntryType, FileIdentity, Unreached, gone_or_failed};

/// The most entries walked: past it, the folder is too large to be checked.
pub const MOST_ENTRIES: u64 = 100_000;

/// What a folder holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Print {
    pub(crate) entries: u64,
    pub(crate) digest: u64,
    /// What its entries take on disk: shown, never compared (the blocks of
    /// a file may change with no write, compressed or trimmed later).
    pub(crate) bytes: u64,
}

impl Print {
    /// Whether `other` read the same entries.
    pub(crate) fn same_as(&self, other: &Self) -> bool {
        (self.entries, self.digest) == (other.entries, other.digest)
    }
}

/// Why a folder cannot be printed: then it is never moved.
#[derive(Debug)]
pub(crate) enum Unprinted {
    /// More entries than [`MOST_ENTRIES`].
    TooLarge,
    /// A mount inside it: moving it would cross filesystems.
    Mount,
    /// An entry the user does not own: once in the trash, the user could
    /// not remove it from there.
    Foreign,
    /// An entry the user excludes: moving the folder would take it along.
    Excluded,
    /// Something on the way changed while walking: an entry gone, swapped
    /// for a link, a folder for another.
    Changed,
    /// The system refused to read an entry.
    Io(io::Error),
}

impl From<Unreached> for Unprinted {
    fn from(unreached: Unreached) -> Self {
        match unreached {
            Unreached::Changed => Self::Changed,
            Unreached::Io(error) => Self::Io(error),
        }
    }
}

/// What every entry walked must be.
pub(crate) struct Walk<'a> {
    /// Who owns each one.
    pub(crate) owner: u32,
    /// The device each one is on.
    pub(crate) device: u64,
    /// The most entries walked.
    pub(crate) most: u64,
    pub(crate) exclusions: &'a Exclusions,
}

/// A folder to walk, opened only when its turn comes: the descriptors held
/// are those of the folders on the way, never one per folder waiting.
struct Waiting {
    parent: Rc<Directory>,
    name: OsString,
    identity: FileIdentity,
}

impl Waiting {
    /// The folder it names, if it is still the one read: a link put in its
    /// place fails to open as a folder.
    fn opened(&self) -> Result<Rc<Directory>, Unprinted> {
        let child = self.parent.open_child(&self.name).map_err(gone_or_failed)?;
        if child.status().map_err(Unprinted::Io)?.identity != self.identity {
            return Err(Unprinted::Changed);
        }
        Ok(Rc::new(child))
    }
}

impl Walk<'_> {
    /// The print of `folder`, walked by descriptors.
    pub(crate) fn print(&self, folder: Directory) -> Result<Print, Unprinted> {
        let mut print = Print {
            entries: 0,
            digest: 0,
            bytes: 0,
        };
        let mut waiting = Vec::new();
        let mut next = Some(Rc::new(folder));
        while let Some(directory) = next.take() {
            self.read(&directory, &mut print, &mut waiting)?;
            next = waiting.pop().as_ref().map(Waiting::opened).transpose()?;
        }
        Ok(print)
    }

    /// Adds the entries of `directory` to `print`, its folders to `waiting`.
    fn read(
        &self,
        directory: &Rc<Directory>,
        print: &mut Print,
        waiting: &mut Vec<Waiting>,
    ) -> Result<(), Unprinted> {
        let names = directory.names().map_err(Unprinted::Io)?;
        // Each name is an entry: past the bound, none is read.
        let listed = u64::try_from(names.len()).unwrap_or(u64::MAX);
        if print.entries.saturating_add(listed) > self.most {
            return Err(Unprinted::TooLarge);
        }
        let mut statuses = Vec::with_capacity(names.len());
        // Every exclusion first: one is told as such whatever entry comes
        // before it.
        for name in names {
            let status = directory.status_of(&name).map_err(gone_or_failed)?;
            if self.exclusions.excludes_name(&name)
                || self.exclusions.excludes_identity(status.identity)
            {
                return Err(Unprinted::Excluded);
            }
            statuses.push((name, status));
        }
        for (name, status) in statuses {
            if status.identity.device() != self.device || status.mount_root == Some(true) {
                return Err(Unprinted::Mount);
            }
            if status.owner != self.owner {
                return Err(Unprinted::Foreign);
            }
            print.entries += 1;
            let mut hasher = DefaultHasher::new();
            (status.identity, status.changed, status.size).hash(&mut hasher);
            // The entries come in no set order: summed, they need none.
            print.digest = print.digest.wrapping_add(hasher.finish());
            print.bytes = print.bytes.saturating_add(status.on_disk());
            if status.kind == EntryType::Directory {
                waiting.push(Waiting {
                    parent: Rc::clone(directory),
                    name,
                    identity: status.identity,
                });
            }
        }
        Ok(())
    }
}
