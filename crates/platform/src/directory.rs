use std::time::SystemTime;

use crate::FileIdentity;

#[cfg(target_os = "linux")]
#[path = "directory/linux.rs"]
mod system;
#[cfg(not(target_os = "linux"))]
#[path = "directory/unsupported.rs"]
mod system;

#[cfg(target_os = "linux")]
pub use system::open_path;
pub use system::{Directory, mount_root, out_of_descriptors, tells_mount_roots};

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
    /// When its inode last changed (`ctime`): a write moves it, even one
    /// that sets `modified` back.
    pub changed: SystemTime,
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
    /// The bytes it takes on disk (blocks of 512 bytes), less than its size
    /// for a sparse file; `None` when the system does not say.
    pub allocated: Option<u64>,
    /// Whether it is the root of a mount, which catches a bind mount of the
    /// same device; `None` when the system cannot tell.
    pub mount_root: Option<bool>,
}

/// Why [`reach`] did not reach a folder.
#[derive(Debug, thiserror::Error)]
pub enum Unreached {
    /// Something on the way is no longer what it was: gone, a link or a
    /// file in a folder's place, another folder, another filesystem.
    #[error("a folder on the way changed")]
    Changed,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Opens the folder `below` names under the folder `root` (an absolute,
/// canonical path), never through a link: `root` is reached from the
/// filesystem root one name at a time and must still be `root_identity`;
/// each folder below it must be a plain folder of its mount
/// ([`EntryStatus::other_mount`]). Each folder opened is checked to be the one read before opening.
///
/// # Errors
///
/// [`Unreached::Changed`] when anything on the way is no longer what it was,
/// [`Unreached::Io`] for the system's refusal.
pub fn reach(
    root: &std::path::Path,
    root_identity: FileIdentity,
    below: &[&std::ffi::OsStr],
) -> Result<Directory, Unreached> {
    use std::path::Component;

    let mut directory = Directory::open(std::path::Path::new("/"))?;
    for component in root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => directory = enter(&directory, name, |_| true)?,
            // A canonical path holds none of these.
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                return Err(Unreached::Changed);
            }
        }
    }
    if directory.status()?.identity != root_identity {
        return Err(Unreached::Changed);
    }
    let device = root_identity.device();
    for name in below {
        directory = enter(&directory, name, |status| !status.other_mount(device))?;
    }
    Ok(directory)
}

/// Opens the folder `name` of `parent`, read first and accepted by `allowed`,
/// and checked to be the one read once opened.
fn enter(
    parent: &Directory,
    name: &std::ffi::OsStr,
    allowed: impl Fn(&EntryStatus) -> bool,
) -> Result<Directory, Unreached> {
    let status = parent.status_of(name).map_err(gone_or_failed)?;
    if status.kind != EntryType::Directory || !allowed(&status) {
        return Err(Unreached::Changed);
    }
    // Swapped for a link meanwhile, it fails to open.
    let opened = parent.open_child(name).map_err(gone_or_failed)?;
    if opened.status()?.identity == status.identity {
        Ok(opened)
    } else {
        Err(Unreached::Changed)
    }
}

/// What an error reading an entry inside a folder means: an entry gone, or
/// no longer a folder (a link fails to open as one), has changed; any other
/// error is the system's.
#[must_use]
pub fn gone_or_failed(error: std::io::Error) -> Unreached {
    use std::io::ErrorKind;

    match error.kind() {
        ErrorKind::NotFound | ErrorKind::NotADirectory => Unreached::Changed,
        _ if system::link_refused(&error) => Unreached::Changed,
        _ => Unreached::Io(error),
    }
}

/// The administrator: the user that may remove anything in a sticky
/// directory, and the only one the privileged helper runs as.
pub const ROOT_USER: u32 = 0;

/// Who owns an object, and for a directory whether it is sticky.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ownership {
    pub owner: u32,
    /// Its entries may only be removed by their owner, its own, or root
    /// (`/tmp`).
    pub sticky: bool,
}

impl EntryStatus {
    /// Whether this directory is the root of another mount than that of the
    /// device `root_device` a walk started on: as the system says, or, when
    /// it cannot, by its device (a btrfs subvolume then counts as one). One
    /// rule for every walk and every way back down.
    #[must_use]
    pub const fn other_mount(&self, root_device: u64) -> bool {
        match self.mount_root {
            Some(mount_root) => mount_root,
            None => self.identity.device() != root_device,
        }
    }

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
            changed: SystemTime::UNIX_EPOCH,
            size: 0,
            allocated: None,
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
    fn a_mount_root_or_another_device_is_another_mount() {
        let status = |device: u64, mount_root: Option<bool>| EntryStatus {
            identity: FileIdentity::new(device, 1),
            mount_root,
            ..status(0, false, false)
        };
        assert!(status(7, Some(true)).other_mount(7), "a bind mount");
        assert!(status(8, Some(true)).other_mount(7));
        assert!(!status(7, Some(false)).other_mount(7));
        assert!(!status(8, Some(false)).other_mount(7), "a btrfs subvolume");
        assert!(
            status(8, None).other_mount(7),
            "unknown: another device is a mount"
        );
        assert!(!status(7, None).other_mount(7));
    }

    #[test]
    fn nothing_removes_what_is_locked() {
        let open = status(1, false, false);
        let locked = status(1, false, true);
        assert!(!removal_allowed(0, &locked, &open));
        assert!(!removal_allowed(0, &open, &locked));
    }

    #[cfg(target_os = "linux")]
    mod reaching {
        use std::ffi::OsStr;
        use std::fs;
        use std::os::unix::fs::symlink;
        use std::path::Path;

        use super::super::*;

        fn identity(path: &Path) -> FileIdentity {
            crate::file_identity(&fs::symlink_metadata(path).unwrap()).unwrap()
        }

        #[test]
        fn a_folder_is_reached_below_its_root() {
            let temporary = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(temporary.path()).unwrap();
            fs::create_dir_all(root.join("a/b")).unwrap();
            fs::write(root.join("a/b/file"), b"x").unwrap();
            let names = [OsStr::new("a"), OsStr::new("b")];
            let reached = reach(&root, identity(&root), &names).unwrap();
            assert_eq!(
                reached.status().unwrap().identity,
                identity(&root.join("a/b"))
            );
        }

        #[test]
        fn a_link_on_the_way_or_another_root_is_a_change() {
            let temporary = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(temporary.path()).unwrap();
            let root = base.join("root");
            fs::create_dir_all(root.join("a")).unwrap();
            fs::create_dir_all(base.join("elsewhere/a")).unwrap();
            let names = [OsStr::new("a")];
            // The root reached through a link: refused, though it leads there.
            symlink(&root, base.join("alias")).unwrap();
            let through = reach(&base.join("alias"), identity(&root), &names);
            assert!(matches!(through, Err(Unreached::Changed)));
            // Another folder at the root's path.
            let other = reach(&base.join("elsewhere"), identity(&root), &names);
            assert!(matches!(other, Err(Unreached::Changed)));
            // A folder below swapped for a link to a folder of that name.
            fs::rename(root.join("a"), base.join("a moved")).unwrap();
            symlink(base.join("elsewhere/a"), root.join("a")).unwrap();
            assert!(matches!(
                reach(&root, identity(&root), &names),
                Err(Unreached::Changed)
            ));
            // Gone, or a file in its place.
            fs::remove_file(root.join("a")).unwrap();
            assert!(matches!(
                reach(&root, identity(&root), &names),
                Err(Unreached::Changed)
            ));
            fs::write(root.join("a"), b"x").unwrap();
            assert!(matches!(
                reach(&root, identity(&root), &names),
                Err(Unreached::Changed)
            ));
        }

        #[test]
        fn a_folder_refused_is_a_refusal_not_a_change() {
            use std::os::unix::fs::PermissionsExt;

            let temporary = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(temporary.path()).unwrap();
            fs::create_dir_all(root.join("closed/inner")).unwrap();
            fs::set_permissions(root.join("closed"), fs::Permissions::from_mode(0o000)).unwrap();
            let names = [OsStr::new("closed"), OsStr::new("inner")];
            let reached = reach(&root, identity(&root), &names);
            fs::set_permissions(root.join("closed"), fs::Permissions::from_mode(0o700)).unwrap();
            if reached.is_ok() {
                eprintln!("skipped: root enters every folder");
                return;
            }
            assert!(
                matches!(
                    reached,
                    Err(Unreached::Io(ref error))
                        if error.kind() == std::io::ErrorKind::PermissionDenied
                ),
                "{reached:?}"
            );
        }
    }
}
