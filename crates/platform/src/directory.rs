use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
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

/// Where every absolute path starts.
const FILESYSTEM_ROOT: &str = "/";

/// Why a folder was not reached ([`reach`], [`reach_with`], [`reach_path`]).
#[derive(Debug, thiserror::Error)]
pub enum Unreached {
    /// Something on the way is no longer what it was: a link or a file in a
    /// folder's place, another folder, another filesystem; for [`reach`] and
    /// [`gone_or_failed`], also a folder gone ([`reach_with`] and
    /// [`reach_path`] tell it apart).
    #[error("a folder on the way changed")]
    Changed,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What [`reach_with`] reached.
#[derive(Debug)]
pub struct Reached {
    /// The folder asked for.
    pub directory: Directory,
    /// The root's status, read once opened; its identity is the one asked
    /// for.
    pub root: EntryStatus,
}

/// Opens the folder `below` names under the folder `root` (an absolute,
/// canonical path), never through a link: `root` is reached from the
/// filesystem root one name at a time and must still be `root_identity`;
/// each folder below it must be a plain folder of its mount
/// ([`EntryStatus::other_mount`]). Each folder opened is checked to be the
/// one read before opening.
///
/// # Errors
///
/// [`Unreached::Changed`] when anything on the way is no longer what it was,
/// gone included; [`Unreached::Io`] for the system's refusal.
pub fn reach(
    root: &Path,
    root_identity: FileIdentity,
    below: &[&OsStr],
) -> Result<Directory, Unreached> {
    reach_with(root, root_identity, below, |_, _| true)?
        .map(|reached| reached.directory)
        .ok_or(Unreached::Changed)
}

/// [`reach`], each folder below the root also accepted by `accepts`, given
/// its place in `below` and its status read before it is opened: the caller's
/// own rule, on top of [`reach`]'s. `Ok(None)` when a folder on the way, the
/// root included, is gone.
///
/// # Errors
///
/// [`Unreached::Changed`] when anything on the way is no longer what it was
/// or `accepts` refuses it, never for a folder gone; [`Unreached::Io`] for the
/// system's refusal.
pub fn reach_with(
    root: &Path,
    root_identity: FileIdentity,
    below: &[&OsStr],
    accepts: impl Fn(usize, &EntryStatus) -> bool,
) -> Result<Option<Reached>, Unreached> {
    let Some(mut directory) = reach_path(root)? else {
        return Ok(None);
    };
    let root_status = directory.status().map_err(|error| failed(root, error))?;
    if root_status.identity != root_identity {
        tracing::debug!(
            path = ?root,
            expected = ?root_identity,
            found = ?root_status.identity,
            "the root on the way is another folder"
        );
        return Err(Unreached::Changed);
    }
    let device = root_identity.device();
    let mut path = root.to_path_buf();
    for (index, name) in below.iter().enumerate() {
        path.push(name);
        let allowed = |status: &EntryStatus| !status.other_mount(device) && accepts(index, status);
        let Some(entered) = enter(&directory, name, &path, allowed)? else {
            return Ok(None);
        };
        directory = entered;
    }
    Ok(Some(Reached {
        directory,
        root: root_status,
    }))
}

/// Opens the folder at `path` (absolute, canonical) from the filesystem root,
/// one name at a time, never through a link, each folder checked to be the
/// one read before opening; `Ok(None)` when one is gone. No identity vouches
/// for the folder reached: the caller checks what it opens in it.
///
/// # Errors
///
/// [`Unreached::Changed`] when `path` is not absolute or holds `..`, or
/// anything on the way is no longer a folder, never for a folder gone;
/// [`Unreached::Io`] for the system's refusal.
pub fn reach_path(path: &Path) -> Result<Option<Directory>, Unreached> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        tracing::debug!(path = ?path, "not an absolute path: not reached");
        return Err(Unreached::Changed);
    }
    let filesystem_root = Path::new(FILESYSTEM_ROOT);
    let mut directory =
        Directory::open(filesystem_root).map_err(|error| failed(filesystem_root, error))?;
    let mut walked = PathBuf::from(FILESYSTEM_ROOT);
    for component in components {
        // A canonical path holds no `..` (`components` drops a `.`).
        let Component::Normal(name) = component else {
            tracing::debug!(path = ?path, "not a canonical path: not reached");
            return Err(Unreached::Changed);
        };
        walked.push(name);
        let Some(entered) = enter(&directory, name, &walked, |_| true)? else {
            return Ok(None);
        };
        directory = entered;
    }
    Ok(Some(directory))
}

/// Opens the folder `name` of `parent`, at `path`, read first and accepted by
/// `allowed`, and checked to be the one read once opened; `None` when it is
/// gone.
fn enter(
    parent: &Directory,
    name: &OsStr,
    path: &Path,
    allowed: impl Fn(&EntryStatus) -> bool,
) -> Result<Option<Directory>, Unreached> {
    let status = match parent.status_of(name) {
        Ok(status) => status,
        Err(error) => return unreached(path, error),
    };
    if status.kind != EntryType::Directory || !allowed(&status) {
        tracing::debug!(
            path = ?path,
            kind = ?status.kind,
            identity = ?status.identity,
            mount_root = ?status.mount_root,
            "a folder on the way refused"
        );
        return Err(Unreached::Changed);
    }
    // Swapped for a link meanwhile, it fails to open.
    let opened = match parent.open_child(name) {
        Ok(opened) => opened,
        Err(error) => return unreached(path, error),
    };
    let identity = opened
        .status()
        .map_err(|error| failed(path, error))?
        .identity;
    if identity == status.identity {
        Ok(Some(opened))
    } else {
        tracing::debug!(
            path = ?path,
            read = ?status.identity,
            opened = ?identity,
            "a folder on the way changed once opened"
        );
        Err(Unreached::Changed)
    }
}

/// What failing to read or open the folder at `path` on the way means: gone
/// (`Ok(None)`), or as [`gone_or_failed`] says; traced.
fn unreached(path: &Path, error: io::Error) -> Result<Option<Directory>, Unreached> {
    if error.kind() == io::ErrorKind::NotFound {
        tracing::debug!(path = ?path, "a folder on the way is gone");
        return Ok(None);
    }
    let kind = error.kind();
    let code = error.raw_os_error();
    let outcome = gone_or_failed(error);
    match &outcome {
        Unreached::Changed => tracing::debug!(
            path = ?path,
            ?kind,
            ?code,
            "a folder on the way is no longer one"
        ),
        Unreached::Io(error) => tracing::debug!(
            path = ?path,
            ?kind,
            ?code,
            %error,
            "a folder on the way refused by the system"
        ),
    }
    Err(outcome)
}

/// The system's refusal to open or read the folder at `path`, on the way;
/// traced.
fn failed(path: &Path, error: io::Error) -> Unreached {
    tracing::debug!(
        path = ?path,
        kind = ?error.kind(),
        code = ?error.raw_os_error(),
        %error,
        "a folder on the way not read"
    );
    Unreached::Io(error)
}

/// What an error reading an entry inside a folder means: an entry gone, or
/// no longer a folder (a link fails to open as one), has changed; any other
/// error is the system's.
#[must_use]
pub fn gone_or_failed(error: io::Error) -> Unreached {
    use io::ErrorKind;

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

    /// The bytes it takes on disk, what removing it frees: its size stands
    /// for them when the system does not say.
    #[must_use]
    pub fn on_disk(&self) -> u64 {
        self.allocated.unwrap_or(self.size)
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
    fn what_an_entry_takes_on_disk_is_its_size_when_the_system_does_not_say() {
        let status = |allocated| EntryStatus {
            size: 1000,
            allocated,
            ..status(0, false, false)
        };
        assert_eq!(status(None).on_disk(), 1000);
        assert_eq!(status(Some(4096)).on_disk(), 4096);
        assert_eq!(status(Some(0)).on_disk(), 0, "a sparse file");
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

        #[test]
        fn a_folder_gone_on_the_way_is_told_apart() {
            let temporary = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(temporary.path()).unwrap();
            let root = base.join("root");
            fs::create_dir_all(root.join("a/b")).unwrap();
            let root_identity = identity(&root);
            let names = [OsStr::new("a"), OsStr::new("b")];
            let anything = |_: usize, _: &EntryStatus| true;
            fs::remove_dir(root.join("a/b")).unwrap();
            assert!(matches!(
                reach_with(&root, root_identity, &names, anything),
                Ok(None)
            ));
            fs::write(root.join("a/b"), b"x").unwrap();
            assert!(matches!(
                reach_with(&root, root_identity, &names, anything),
                Err(Unreached::Changed)
            ));
            fs::remove_dir_all(&root).unwrap();
            assert!(matches!(
                reach_with(&root, root_identity, &names, anything),
                Ok(None)
            ));
        }

        #[test]
        fn what_the_caller_refuses_below_the_root_is_a_change() {
            let temporary = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(temporary.path()).unwrap();
            fs::create_dir_all(root.join("a/b")).unwrap();
            let a = identity(&root.join("a"));
            let b = identity(&root.join("a/b"));
            let names = [OsStr::new("a"), OsStr::new("b")];
            let listed =
                |index: usize, status: &EntryStatus| [a, b].get(index) == Some(&status.identity);
            assert!(matches!(
                reach_with(&root, identity(&root), &names, listed),
                Ok(Some(_))
            ));
            let swapped =
                |index: usize, status: &EntryStatus| [b, a].get(index) == Some(&status.identity);
            assert!(matches!(
                reach_with(&root, identity(&root), &names, swapped),
                Err(Unreached::Changed)
            ));
            // Refused at the second folder only: reached up to the first.
            let not_second = |index: usize, _: &EntryStatus| index != 1;
            assert!(matches!(
                reach_with(&root, identity(&root), &names, not_second),
                Err(Unreached::Changed)
            ));
            assert!(matches!(
                reach_with(&root, identity(&root), &names[..1], not_second),
                Ok(Some(_))
            ));
        }

        #[test]
        fn the_root_status_is_the_one_read_on_the_way() {
            let temporary = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(temporary.path()).unwrap();
            fs::create_dir_all(root.join("a/b")).unwrap();
            let names = [OsStr::new("a"), OsStr::new("b")];
            let reached = reach_with(&root, identity(&root), &names, |_, _| true)
                .unwrap()
                .unwrap();
            assert_eq!(reached.root.identity, identity(&root));
            assert_eq!(
                reached.directory.status().unwrap().identity,
                identity(&root.join("a/b"))
            );
        }

        #[test]
        fn a_path_is_reached_without_a_link() {
            let temporary = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(temporary.path()).unwrap();
            fs::create_dir_all(base.join("real/inner")).unwrap();
            let reached = reach_path(&base.join("real/inner")).unwrap().unwrap();
            assert_eq!(
                reached.status().unwrap().identity,
                identity(&base.join("real/inner"))
            );
            symlink(base.join("real"), base.join("alias")).unwrap();
            assert!(matches!(
                reach_path(&base.join("alias/inner")),
                Err(Unreached::Changed)
            ));
            assert!(matches!(reach_path(&base.join("absent/inner")), Ok(None)));
            let relative = base.strip_prefix("/").unwrap().join("real");
            assert!(matches!(reach_path(&relative), Err(Unreached::Changed)));
            assert!(matches!(
                reach_path(&base.join("real/../real")),
                Err(Unreached::Changed)
            ));
        }

        #[test]
        fn a_mount_below_the_root_is_never_entered() {
            let dev = Path::new("/dev");
            let Some(mounted) = ["shm", "pts", "mqueue", "hugepages"]
                .into_iter()
                .find(|name| crate::mount_root(&dev.join(name)).ok().flatten() == Some(true))
            else {
                eprintln!("skipped: no mount under /dev");
                return;
            };
            // The root itself may be a mount: only what is below it is refused.
            assert!(reach(dev, identity(dev), &[]).is_ok());
            let names = [OsStr::new(mounted)];
            assert!(matches!(
                reach(dev, identity(dev), &names),
                Err(Unreached::Changed)
            ));
            assert!(matches!(
                reach_with(dev, identity(dev), &names, |_, _| true),
                Err(Unreached::Changed)
            ));
        }
    }
}
