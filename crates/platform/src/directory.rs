use std::time::SystemTime;

use crate::FileIdentity;

#[cfg(target_os = "linux")]
#[path = "directory/linux.rs"]
mod system;
#[cfg(not(target_os = "linux"))]
#[path = "directory/unsupported.rs"]
mod system;

pub use system::{Directory, mount_root, tells_mount_roots};

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
pub(crate) const ROOT_USER: u32 = 0;

/// Who owns an object, and for a directory whether it is sticky.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ownership {
    pub owner: u32,
    /// Its entries may only be removed by their owner, its own, or root
    /// (`/tmp`).
    pub sticky: bool,
}

impl EntryStatus {
    #[must_use]
    #[cfg_attr(
        not(target_os = "linux"),
        expect(
            dead_code,
            reason = "only the Linux directory removes through descriptors"
        )
    )]
    pub(crate) const fn ownership(&self) -> Ownership {
        Ownership {
            owner: self.owner,
            sticky: self.sticky,
        }
    }
}

/// Whether `user` may remove an entry owned by `entry_owner` from `directory`
/// as far as the sticky bit goes: in a sticky directory only the entry's
/// owner, the directory's or root may (root taken to hold `CAP_FOWNER`; a root
/// without it fails at the removal itself, which the receipt reports).
#[must_use]
pub const fn sticky_allows(user: u32, directory: Ownership, entry_owner: u32) -> bool {
    !directory.sticky || user == ROOT_USER || user == directory.owner || user == entry_owner
}

/// Whether `user` may remove `entry` from `directory` once allowed to write
/// in it: neither may be locked, and the sticky bit must allow it.
#[cfg_attr(
    not(target_os = "linux"),
    expect(
        dead_code,
        reason = "only the Linux directory removes through descriptors"
    )
)]
pub(crate) const fn removal_allowed(
    user: u32,
    directory: &EntryStatus,
    entry: &EntryStatus,
) -> bool {
    !directory.locked && !entry.locked && sticky_allows(user, directory.ownership(), entry.owner)
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
    fn the_sticky_rule_names_who_may_remove() {
        let sticky = Ownership {
            owner: 0,
            sticky: true,
        };
        assert!(!sticky_allows(1, sticky, 2), "a stranger's entry");
        assert!(sticky_allows(1, sticky, 1), "one's own entry");
        assert!(sticky_allows(0, sticky, 2), "root");
        let mine = Ownership {
            owner: 1,
            sticky: true,
        };
        assert!(sticky_allows(1, mine, 2), "one's own sticky directory");
        let plain = Ownership {
            owner: 0,
            sticky: false,
        };
        assert!(sticky_allows(1, plain, 2), "no sticky bit, no restriction");
    }

    #[test]
    fn nothing_removes_what_is_locked() {
        let open = status(1, false, false);
        let locked = status(1, false, true);
        assert!(!removal_allowed(0, &locked, &open));
        assert!(!removal_allowed(0, &open, &locked));
    }
}
