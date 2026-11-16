//! What a filesystem's type, as `mountinfo` names it, says of the files it
//! holds: whether it may show one under another identity, whether it keeps
//! a change date of its own. One source for every tool that judges a folder
//! by them.

use std::io;
use std::path::Path;

use crate::{Mount, MountList, mounts};

/// What every FUSE filesystem's type starts with (`fuse.bindfs`).
const FUSE_PREFIX: &str = "fuse";
/// Filesystems reached over the network.
const NETWORK_FILESYSTEMS: &[&str] =
    &["nfs", "nfs4", "cifs", "smb3", "smbfs", "ceph", "fuse.sshfs"];
/// Filesystems a virtual machine shares with its host (WSL's drives
/// included), whose files the host shows again.
const HOST_SHARED_FILESYSTEMS: &[&str] = &["9p", "virtiofs", "vboxsf"];
/// Filesystems stacked over another folder, whose files they show again.
const STACKED_FILESYSTEMS: &[&str] = &["overlay", "aufs"];
/// Filesystems that keep no change date of their own, the modification
/// date standing for it (Linux, `fs/fat`: `fat_fill_inode`,
/// `fat_truncate_time`; `fs/exfat`: `exfat_fill_inode` sets it from the
/// modification date each time the file is read from the disk): a file
/// rewritten then dated back looks unchanged, at least once remounted.
const UNDATED_FILESYSTEMS: &[&str] = &["vfat", "msdos", "exfat"];

/// Whether the filesystem `filesystem` may show a file under another
/// identity than its own: FUSE (a bind of a folder, a desktop's share),
/// network, shared by a virtual machine's host, and stacked filesystems.
#[must_use]
pub fn filesystem_may_alias(filesystem: &str) -> bool {
    filesystem.starts_with(FUSE_PREFIX)
        || filesystem_is_network(filesystem)
        || HOST_SHARED_FILESYSTEMS.contains(&filesystem)
        || STACKED_FILESYSTEMS.contains(&filesystem)
}

/// Whether the filesystem `filesystem` is reached over the network.
#[must_use]
pub fn filesystem_is_network(filesystem: &str) -> bool {
    NETWORK_FILESYSTEMS.contains(&filesystem)
}

/// Whether the filesystem `filesystem` keeps no change date of its own: a
/// file's age cannot be told there.
#[must_use]
pub fn filesystem_undated(filesystem: &str) -> bool {
    UNDATED_FILESYSTEMS.contains(&filesystem)
}

/// The mount `path` lies on: the deepest whose mount point holds it.
#[must_use]
pub fn mount_of<'a>(path: &Path, mounts: &'a [Mount]) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.mount_point))
        // The last of equals: mounted over the others, it is the one seen.
        .max_by_key(|mount| mount.mount_point.components().count())
}

/// The type of the filesystem `path`, a real path, lies on; none when no
/// mount holds it.
///
/// # Errors
///
/// Those of [`mounts`]: the table of mounts cannot be read (outside Linux,
/// it is never known); [`io::ErrorKind::InvalidData`] when a line of it
/// cannot be understood, which may be the mount `path` lies on.
pub fn filesystem_of(path: &Path) -> io::Result<Option<String>> {
    filesystem_in(&mounts()?, path)
}

/// [`filesystem_of`], in the table of mounts `list`.
fn filesystem_in(list: &MountList, path: &Path) -> io::Result<Option<String>> {
    if list.malformed > 0 {
        tracing::warn!(path = %path.display(), lines = list.malformed, "mounts partly unreadable: the filesystem of a path cannot be told");
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "lines of the table of mounts not understood",
        ));
    }
    Ok(mount_of(path, &list.mounts).map(|mount| mount.filesystem.clone()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{Device, MountClass};

    #[test]
    fn a_filesystem_that_may_show_a_file_twice_is_known() {
        for filesystem in [
            "fuse.bindfs",
            "fuse.gvfsd-fuse",
            "fuseblk",
            "nfs4",
            "cifs",
            "overlay",
            "aufs",
            "smbfs",
            "smb3",
            "ceph",
            "fuse.sshfs",
            "vboxsf",
            "9p",
            "virtiofs",
        ] {
            assert!(filesystem_may_alias(filesystem), "{filesystem}");
        }
        for filesystem in ["ext4", "btrfs", "xfs", "vfat", "zfs"] {
            assert!(!filesystem_may_alias(filesystem), "{filesystem}");
        }
    }

    #[test]
    fn a_virtual_machine_s_share_is_no_network_one_for_the_volumes_shown() {
        for filesystem in ["nfs4", "cifs", "smbfs", "ceph", "fuse.sshfs"] {
            assert!(filesystem_is_network(filesystem), "{filesystem}");
        }
        for filesystem in ["9p", "virtiofs", "vboxsf", "ext4"] {
            assert!(!filesystem_is_network(filesystem), "{filesystem}");
        }
    }

    #[test]
    fn a_filesystem_without_a_change_date_of_its_own_is_known() {
        for filesystem in ["vfat", "msdos", "exfat"] {
            assert!(filesystem_undated(filesystem), "{filesystem}");
        }
        for filesystem in ["ext4", "btrfs", "xfs", "ntfs3", "fuseblk"] {
            assert!(!filesystem_undated(filesystem), "{filesystem}");
        }
    }

    #[test]
    fn a_path_lies_on_the_deepest_mount_that_holds_it() {
        let mount = |point: &str, filesystem: &str| Mount {
            id: 1,
            device: Device { major: 0, minor: 1 },
            mount_point: PathBuf::from(point),
            filesystem: filesystem.to_owned(),
            source: String::new(),
            class: MountClass::Block,
        };
        let mounts = [
            mount("/", "ext4"),
            mount("/media/ana/KEY", "vfat"),
            mount("/media", "tmpfs"),
            // A key mounted over a folder: the mount on top, met last, holds it.
            mount("/srv/drop", "ext4"),
            mount("/srv/drop", "vfat"),
        ];
        let on =
            |path: &str| mount_of(Path::new(path), &mounts).map(|mount| mount.filesystem.as_str());
        assert_eq!(on("/media/ana/KEY/tmp"), Some("vfat"));
        assert_eq!(on("/media/ana/KEYS"), Some("tmpfs"), "whole components");
        assert_eq!(on("/home/ana"), Some("ext4"));
        assert_eq!(on("/srv/drop/tmp"), Some("vfat"));
        assert_eq!(mount_of(Path::new("/x"), &[]), None);
        let list = |malformed| MountList {
            mounts: mounts.to_vec(),
            malformed,
        };
        let key = Path::new("/media/ana/KEY/tmp");
        assert_eq!(
            filesystem_in(&list(0), key).unwrap().as_deref(),
            Some("vfat")
        );
        assert_eq!(
            filesystem_in(&list(1), key).map_err(|error| error.kind()),
            Err(io::ErrorKind::InvalidData),
            "the line not understood may be its own mount"
        );
    }
}
