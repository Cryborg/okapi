use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{
    Access, AtFlags, CWD, Dir, FileType, Mode, OFlags, Statx, StatxAttributes, StatxFlags,
    StatxTimestamp, accessat, makedev, openat, statx, unlinkat,
};

use super::{EntryStatus, EntryType, removal_allowed};
use crate::FileIdentity;
use crate::identity::{instant, later_of};

/// A directory is opened for listing only, and never through a link.
const OPEN_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// What [`EntryStatus`] is built from: an entry missing one is unreadable.
const NEEDED: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::MODE)
    .union(StatxFlags::NLINK)
    .union(StatxFlags::UID)
    .union(StatxFlags::INO)
    .union(StatxFlags::MTIME)
    .union(StatxFlags::CTIME)
    .union(StatxFlags::SIZE);

/// An open directory, whose entries are read and removed relative to it: a
/// directory swapped for a link after being opened is never followed.
#[derive(Debug)]
pub struct Directory {
    fd: OwnedFd,
}

impl Directory {
    /// Opens the directory at `path`, refusing a link in its last component.
    ///
    /// # Errors
    ///
    /// The system's error: `path` is a link, not a directory, absent...
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            fd: openat(CWD, path, OPEN_FLAGS, Mode::empty())?,
        })
    }

    /// The directory itself, as opened.
    ///
    /// # Errors
    ///
    /// The system's error, or [`io::ErrorKind::Unsupported`] when it cannot
    /// give a type, an identity and dates.
    pub fn status(&self) -> io::Result<EntryStatus> {
        status(&statx(&self.fd, "", AtFlags::EMPTY_PATH, NEEDED)?)
    }

    /// Whether the process may remove `entry`, read through
    /// [`Self::status_of`], from this directory: write and search permission
    /// on it, neither of them locked, and in a sticky directory the entry or
    /// the directory owned by the process, or the process root.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::PermissionDenied`] when it may not, or the system's
    /// error while reading the directory.
    pub fn ensure_removable(&self, entry: &EntryStatus) -> io::Result<()> {
        self.ensure_modifiable()?;
        let user = rustix::process::geteuid().as_raw();
        if removal_allowed(user, &self.status()?, entry) {
            Ok(())
        } else {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }

    /// Whether the process may remove entries of this directory (write and
    /// search permission).
    ///
    /// # Errors
    ///
    /// The system's refusal: [`io::ErrorKind::PermissionDenied`],
    /// [`io::ErrorKind::ReadOnlyFilesystem`]...
    fn ensure_modifiable(&self) -> io::Result<()> {
        // Checked for the real user, as `faccessat` does: the process is never
        // set-user-ID, and asking for the effective one needs `faccessat2`,
        // which old seccomp filters refuse with `EPERM`.
        Ok(accessat(
            &self.fd,
            ".",
            Access::WRITE_OK | Access::EXEC_OK,
            AtFlags::empty(),
        )?)
    }

    /// The names of its entries, `.` and `..` left out.
    ///
    /// # Errors
    ///
    /// The system's error while listing.
    pub fn names(&self) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in Dir::read_from(&self.fd)? {
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name != "." && name != ".." {
                names.push(name.to_os_string());
            }
        }
        Ok(names)
    }

    /// The entry `name`, without following it if it is a link.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] when `name` is not a single name, or
    /// the errors of [`Self::status`].
    pub fn status_of(&self, name: &OsStr) -> io::Result<EntryStatus> {
        status(&statx(
            &self.fd,
            single_name(name)?,
            // Reading an automount point must not mount it.
            AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT,
            NEEDED,
        )?)
    }

    /// Opens the subdirectory `name`, refusing a link.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] when `name` is not a single name, or the
    /// system's error: a link, not a directory, absent...
    pub fn open_child(&self, name: &OsStr) -> io::Result<Self> {
        Ok(Self {
            fd: openat(&self.fd, single_name(name)?, OPEN_FLAGS, Mode::empty())?,
        })
    }

    /// Removes the entry `name` that is not a directory (a link is removed,
    /// never what it points to).
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] when `name` is not a single name, or the
    /// system's error.
    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        Ok(unlinkat(&self.fd, single_name(name)?, AtFlags::empty())?)
    }

    /// Removes the empty subdirectory `name`.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] when `name` is not a single name, or the
    /// system's error ([`io::ErrorKind::DirectoryNotEmpty`] when it is not
    /// empty).
    pub fn remove_directory(&self, name: &OsStr) -> io::Result<()> {
        Ok(unlinkat(&self.fd, single_name(name)?, AtFlags::REMOVEDIR)?)
    }
}

/// Whether this system tells a mount root from a plain directory (Linux 5.8
/// and later), which cleaning needs to never enter a bind mount; `false` also
/// when the filesystem root cannot be read.
#[must_use]
pub fn tells_mount_roots() -> bool {
    Directory::open(Path::new("/"))
        .and_then(|root| root.status())
        .is_ok_and(|status| status.mount_root.is_some())
}

/// `name`, if it names an entry of the directory itself: a path such as
/// `../x` or `a/b` would reach outside it.
fn single_name(name: &OsStr) -> io::Result<&OsStr> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || name == "." || name == ".." {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(name)
}

fn status(stat: &Statx) -> io::Result<EntryStatus> {
    if !StatxFlags::from_bits_retain(stat.stx_mask).contains(NEEDED) {
        return Err(io::ErrorKind::Unsupported.into());
    }
    let kind = match FileType::from_raw_mode(u32::from(stat.stx_mode)) {
        FileType::RegularFile => EntryType::File,
        FileType::Directory => EntryType::Directory,
        FileType::Symlink => EntryType::Link,
        _ => EntryType::Other,
    };
    let date = |timestamp: StatxTimestamp| {
        instant(timestamp.tv_sec, timestamp.tv_nsec).ok_or(io::ErrorKind::InvalidData)
    };
    let modified = date(stat.stx_mtime)?;
    let changed = date(stat.stx_ctime)?;
    let mount_root = stat
        .stx_attributes_mask
        .contains(StatxAttributes::MOUNT_ROOT)
        .then(|| stat.stx_attributes.contains(StatxAttributes::MOUNT_ROOT));
    let locked = stat
        .stx_attributes
        .intersects(StatxAttributes::IMMUTABLE | StatxAttributes::APPEND);
    let mode = Mode::from_raw_mode(u32::from(stat.stx_mode));
    Ok(EntryStatus {
        kind,
        identity: FileIdentity::new(
            makedev(stat.stx_dev_major, stat.stx_dev_minor),
            stat.stx_ino,
        ),
        last_change: later_of(modified, changed),
        modified,
        size: stat.stx_size,
        links: u64::from(stat.stx_nlink),
        owner: stat.stx_uid,
        sticky: mode.contains(Mode::SVTX),
        locked,
        mount_root,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    fn directory() -> (tempfile::TempDir, Directory) {
        let temporary = tempfile::tempdir().unwrap();
        let opened = Directory::open(temporary.path()).unwrap();
        (temporary, opened)
    }

    #[test]
    fn a_status_agrees_with_what_is_read_by_path() {
        let (temporary, directory) = directory();
        let path = temporary.path().join("file");
        fs::write(&path, b"12345").unwrap();
        let by_path = fs::symlink_metadata(&path).unwrap();
        let status = directory.status_of(OsStr::new("file")).unwrap();
        assert_eq!(status.kind, EntryType::File);
        assert_eq!(Some(status.identity), crate::file_identity(&by_path));
        assert_eq!(Some(status.last_change), crate::last_change(&by_path));
        assert_eq!(status.size, 5);
        let own = fs::symlink_metadata(temporary.path()).unwrap();
        assert_eq!(
            Some(directory.status().unwrap().identity),
            crate::file_identity(&own)
        );
    }

    #[test]
    fn a_status_dates_the_inode_change_and_the_content_change() {
        let (temporary, directory) = directory();
        let path = temporary.path().join("extracted");
        fs::write(&path, b"x").unwrap();
        let long_ago =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::open(&path)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let status = directory.status_of(OsStr::new("extracted")).unwrap();
        assert_eq!(status.modified, long_ago);
        assert!(status.last_change > long_ago, "the inode changed just now");
        let by_path = fs::symlink_metadata(&path).unwrap();
        assert_eq!(Some(status.last_change), crate::last_change(&by_path));
        let ahead = std::time::SystemTime::now() + std::time::Duration::from_hours(24);
        fs::File::open(&path).unwrap().set_modified(ahead).unwrap();
        assert_eq!(
            directory
                .status_of(OsStr::new("extracted"))
                .unwrap()
                .last_change,
            ahead
        );
    }

    #[test]
    fn the_sticky_bit_and_names_are_read() {
        use std::os::unix::fs::PermissionsExt;

        let (temporary, directory) = directory();
        let shared = temporary.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(directory.status_of(OsStr::new("shared")).unwrap().sticky);
        fs::write(temporary.path().join("one"), b"x").unwrap();
        fs::hard_link(temporary.path().join("one"), temporary.path().join("two")).unwrap();
        let one = directory.status_of(OsStr::new("one")).unwrap();
        assert_eq!(one.links, 2);
        assert!(!one.sticky && !one.locked);
        directory.ensure_removable(&one).unwrap();
    }

    #[test]
    fn links_are_read_and_removed_never_followed() {
        let (temporary, directory) = directory();
        fs::create_dir(temporary.path().join("target")).unwrap();
        fs::write(temporary.path().join("target/kept"), b"x").unwrap();
        symlink(
            temporary.path().join("target"),
            temporary.path().join("link"),
        )
        .unwrap();
        let link = OsStr::new("link");
        assert_eq!(directory.status_of(link).unwrap().kind, EntryType::Link);
        assert!(directory.open_child(link).is_err());
        assert!(Directory::open(&temporary.path().join("link")).is_err());
        directory.remove_file(link).unwrap();
        assert!(temporary.path().join("target/kept").exists());
    }

    #[test]
    fn lists_names_and_removes_files_and_empty_directories() {
        let (temporary, directory) = directory();
        fs::create_dir(temporary.path().join("full")).unwrap();
        fs::write(temporary.path().join("full/inner"), b"x").unwrap();
        fs::write(temporary.path().join("file"), b"x").unwrap();
        let mut names = directory.names().unwrap();
        names.sort();
        assert_eq!(names, [OsString::from("file"), OsString::from("full")]);
        let full = OsStr::new("full");
        assert_eq!(
            directory.remove_directory(full).unwrap_err().kind(),
            io::ErrorKind::DirectoryNotEmpty
        );
        let child = directory.open_child(full).unwrap();
        assert_eq!(child.status().unwrap().kind, EntryType::Directory);
        child.remove_file(OsStr::new("inner")).unwrap();
        directory.remove_directory(full).unwrap();
        directory.remove_file(OsStr::new("file")).unwrap();
        assert_eq!(directory.names().unwrap(), Vec::<OsString>::new());
    }

    #[test]
    fn a_name_never_reaches_outside_the_directory() {
        let (temporary, _) = directory();
        fs::create_dir(temporary.path().join("inner")).unwrap();
        fs::write(temporary.path().join("outside"), b"x").unwrap();
        let inner = Directory::open(&temporary.path().join("inner")).unwrap();
        for name in ["", ".", "..", "../outside", "a/b"] {
            let name = OsStr::new(name);
            assert_eq!(
                inner.remove_file(name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{name:?}"
            );
            assert!(inner.status_of(name).is_err());
            assert!(inner.open_child(name).is_err());
            assert!(inner.remove_directory(name).is_err());
        }
        assert!(temporary.path().join("outside").exists());
    }

    #[test]
    fn a_read_only_directory_is_not_modifiable() {
        use std::os::unix::fs::PermissionsExt;

        let (temporary, directory) = directory();
        directory.ensure_modifiable().unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let refused = directory.ensure_modifiable();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o666)).unwrap();
        let unsearchable = directory.ensure_modifiable();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes through permissions: nothing to observe then.
        let root = fs::write(temporary.path().join("probe"), b"").is_ok();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o755)).unwrap();
        if !root {
            assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(
                unsearchable.unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn the_filesystem_root_is_a_mount_root_and_a_subdirectory_is_not() {
        // Linux 5.8 and later tell mount roots; the cleaning refuses to run
        // otherwise.
        let root = Directory::open(Path::new("/")).unwrap();
        assert_eq!(root.status().unwrap().mount_root, Some(true));
        let (temporary, directory) = directory();
        fs::create_dir(temporary.path().join("plain")).unwrap();
        assert_eq!(
            directory.status_of(OsStr::new("plain")).unwrap().mount_root,
            Some(false)
        );
        assert!(tells_mount_roots());
    }
}
