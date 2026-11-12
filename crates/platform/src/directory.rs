use std::time::SystemTime;

use crate::FileIdentity;

#[cfg(target_os = "linux")]
#[path = "directory/linux.rs"]
mod system;
#[cfg(not(target_os = "linux"))]
#[path = "directory/unsupported.rs"]
mod system;

pub use system::{Directory, tells_mount_roots};

/// Whether [`Directory`] works on this system. Elsewhere it cannot be opened
/// ([`std::io::ErrorKind::Unsupported`]): on macOS the scan works but cleaning
/// does not, until a port of this module.
pub const DESCRIPTOR_REMOVAL_SUPPORTED: bool = cfg!(target_os = "linux");

/// What an entry is, read without following links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    File,
    Directory,
    Link,
    /// A socket, a pipe or a device.
    Other,
}

/// An entry as read through a [`Directory`], comparable with what
/// [`crate::file_identity`] and [`crate::last_change`] read by path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryStatus {
    pub kind: EntryType,
    pub identity: FileIdentity,
    /// When it last changed, content or inode.
    pub last_change: SystemTime,
    /// When its content last changed.
    pub modified: SystemTime,
    /// How many names it has (hard links).
    pub links: u64,
    /// The user that owns it.
    pub owner: u32,
    /// Whether it is a directory whose entries only their owner, or its own,
    /// may remove (the sticky bit of `/tmp`).
    pub sticky: bool,
    /// Whether it is immutable or append-only: it cannot be removed, nor, for
    /// a directory, can its entries.
    pub locked: bool,
    /// Its size in bytes.
    pub size: u64,
    /// Whether it is the root of a mount, which catches a bind mount of the
    /// same device; `None` when the system cannot tell.
    pub mount_root: Option<bool>,
}

/// The user that may remove anything in a sticky directory.
const ROOT_USER: u32 = 0;

/// Whether `user` may remove `entry` from `directory` once allowed to write
/// in it: neither may be locked, and in a sticky directory `user` must own one
/// of them or be root.
pub(crate) const fn removal_allowed(
    user: u32,
    directory: &EntryStatus,
    entry: &EntryStatus,
) -> bool {
    if directory.locked || entry.locked {
        return false;
    }
    !directory.sticky || user == ROOT_USER || user == directory.owner || user == entry.owner
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(owner: u32, sticky: bool, locked: bool) -> EntryStatus {
        EntryStatus {
            kind: EntryType::Directory,
            identity: FileIdentity::new(1, 1),
            last_change: SystemTime::UNIX_EPOCH,
            modified: SystemTime::UNIX_EPOCH,
            size: 0,
            links: 1,
            owner,
            sticky,
            locked,
            mount_root: Some(false),
        }
    }

    #[test]
    fn a_sticky_directory_lets_only_owners_and_root_remove() {
        let sticky = status(0, true, false);
        let others = status(2, false, false);
        let mine = status(1, false, false);
        assert!(!removal_allowed(1, &sticky, &others));
        assert!(removal_allowed(1, &sticky, &mine));
        assert!(removal_allowed(0, &sticky, &others));
        assert!(removal_allowed(2, &status(2, true, false), &mine));
        assert!(removal_allowed(1, &status(0, false, false), &others));
    }

    #[test]
    fn nothing_removes_what_is_locked() {
        let open = status(1, false, false);
        let locked = status(1, false, true);
        assert!(!removal_allowed(0, &locked, &open));
        assert!(!removal_allowed(0, &open, &locked));
    }
}
