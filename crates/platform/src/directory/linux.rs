use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{
    Access, AtFlags, CWD, Dir, FileType, Mode, OFlags, Statx, StatxAttributes, StatxFlags,
    StatxTimestamp, accessat, makedev, openat, statx, unlinkat,
};
use rustix::io::Errno;

use super::{EntryStatus, EntryType, removal_allowed};
use crate::FileIdentity;
use crate::identity::{instant, later_of};

/// A directory is opened for listing only, and never through a link.
const OPEN_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// How an entry is read: never through its link, never mounting an automount
/// point, and as fresh as `stat` gives it. Never `AT_STATX_DONT_SYNC`: the
/// cleaning judges ages on these reads (invariant 2). On a network filesystem,
/// `stat` itself may answer from its attribute cache for a few seconds.
const ENTRY_FLAGS: AtFlags = AtFlags::SYMLINK_NOFOLLOW.union(AtFlags::NO_AUTOMOUNT);

/// What [`EntryStatus`] is built from: an entry missing one is unreadable.
const NEEDED: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::MODE)
    .union(StatxFlags::NLINK)
    .union(StatxFlags::UID)
    .union(StatxFlags::INO)
    .union(StatxFlags::MTIME)
    .union(StatxFlags::CTIME)
    .union(StatxFlags::SIZE);

/// What is asked beside [`NEEDED`]: an entry without it is still read.
const WANTED: StatxFlags = NEEDED.union(StatxFlags::BLOCKS);

/// The unit of `stx_blocks`, whatever the filesystem's own block.
const BLOCK_SIZE: u64 = 512;

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
        status(&statx(&self.fd, "", AtFlags::EMPTY_PATH, WANTED)?)
    }

    /// Whether the process may remove `entry`, read through
    /// [`Self::status_of`], from this directory: write and search permission
    /// on it, neither of them locked, and in a sticky directory the entry or
    /// the directory owned by the process, or the process root.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::PermissionDenied`] when it may not, the system's error
    /// while reading the directory, or [`io::ErrorKind::Unsupported`] were the
    /// effective user unknown (never on Linux).
    pub fn ensure_removable(&self, entry: &EntryStatus) -> io::Result<()> {
        self.ensure_modifiable()?;
        let user = crate::effective_user().ok_or(io::ErrorKind::Unsupported)?;
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
        status(&statx(&self.fd, single_name(name)?, ENTRY_FLAGS, WANTED)?)
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

/// A descriptor of the object at `path` that only names it (`O_PATH`: its
/// content is never opened, a pipe never blocks), and what it is: what the
/// desktop portal takes to show an object in the file manager. A link in
/// the last component is followed, as the portal refuses a descriptor that
/// does not; with `directory`, anything but a directory is refused.
///
/// # Errors
///
/// The system's error: absent, not a directory when one is asked...
pub fn open_path(path: &Path, directory: bool) -> io::Result<(OwnedFd, EntryStatus)> {
    let mut flags = OFlags::PATH | OFlags::CLOEXEC;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    let fd = openat(CWD, path, flags, Mode::empty())?;
    let status = status(&statx(&fd, "", AtFlags::EMPTY_PATH, WANTED)?)?;
    Ok((fd, status))
}

/// Whether `error` says the process, or the system, ran out of open file
/// descriptors (`EMFILE`, `ENFILE`): no refusal of the file itself.
#[must_use]
pub fn out_of_descriptors(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        code == Errno::MFILE.raw_os_error() || code == Errno::NFILE.raw_os_error()
    })
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

/// Whether the entry at `path`, its last component never followed, is the root
/// of a mount, which catches a bind mount of the same device; `Ok(None)` when
/// the system cannot tell (Linux before 5.8, or no `statx` at all).
///
/// # Errors
///
/// The system's error: absent, unreadable...
pub fn mount_root(path: &Path) -> io::Result<Option<bool>> {
    match statx(CWD, path, ENTRY_FLAGS, StatxFlags::empty()) {
        Ok(stat) => Ok(mount_root_of(&stat)),
        // A kernel or a sandbox without `statx`: the scan works as it did,
        // the cleaning refuses such a system (`tells_mount_roots`).
        Err(Errno::NOSYS) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Whether `stat` is a mount root, if the system says.
fn mount_root_of(stat: &Statx) -> Option<bool> {
    stat.stx_attributes_mask
        .contains(StatxAttributes::MOUNT_ROOT)
        .then(|| stat.stx_attributes.contains(StatxAttributes::MOUNT_ROOT))
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
    let mount_root = mount_root_of(stat);
    let locked = stat
        .stx_attributes
        .intersects(StatxAttributes::IMMUTABLE | StatxAttributes::APPEND);
    let mode = Mode::from_raw_mode(u32::from(stat.stx_mode));
    let allocated = StatxFlags::from_bits_retain(stat.stx_mask)
        .contains(StatxFlags::BLOCKS)
        .then(|| stat.stx_blocks.saturating_mul(BLOCK_SIZE));
    Ok(EntryStatus {
        kind,
        identity: FileIdentity::new(
            makedev(stat.stx_dev_major, stat.stx_dev_minor),
            stat.stx_ino,
        ),
        last_change: later_of(modified, changed),
        modified,
        size: stat.stx_size,
        allocated,
        links: u64::from(stat.stx_nlink),
        owner: stat.stx_uid,
        sticky: kind == EntryType::Directory && mode.contains(Mode::SVTX),
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
        assert_eq!(
            status.allocated,
            Some(std::os::unix::fs::MetadataExt::blocks(&by_path) * 512)
        );
        let own = fs::symlink_metadata(temporary.path()).unwrap();
        assert_eq!(
            Some(directory.status().unwrap().identity),
            crate::file_identity(&own)
        );
    }

    #[test]
    fn what_an_entry_takes_on_disk_is_read() {
        let (temporary, directory) = directory();
        // Data that no filesystem keeps inline nor compresses.
        let mut state: u32 = 0x2545_F491;
        let data: Vec<u8> = (0..1 << 20)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect();
        fs::write(temporary.path().join("data"), &data).unwrap();
        let status = directory.status_of(OsStr::new("data")).unwrap();
        let by_path = fs::symlink_metadata(temporary.path().join("data")).unwrap();
        assert_eq!(
            status.allocated,
            Some(std::os::unix::fs::MetadataExt::blocks(&by_path) * 512)
        );
        assert!(
            status
                .allocated
                .is_some_and(|allocated| allocated >= 1 << 20)
        );
        assert!(
            directory.status().unwrap().allocated.is_some(),
            "a folder's own"
        );
    }

    #[test]
    fn a_path_is_named_by_a_descriptor_that_never_opens_it() {
        let (temporary, _) = directory();
        let folder = temporary.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let file = temporary.path().join("file");
        fs::write(&file, b"x").unwrap();
        let (_, status) = open_path(&folder, true).unwrap();
        assert_eq!(status.kind, EntryType::Directory);
        assert_eq!(
            Some(status.identity),
            crate::file_identity(&fs::symlink_metadata(&folder).unwrap())
        );
        assert_eq!(
            open_path(&file, true).unwrap_err().kind(),
            io::ErrorKind::NotADirectory,
            "a file is no folder"
        );
        let (descriptor, file_status) = open_path(&file, false).unwrap();
        assert_eq!(file_status.kind, EntryType::File);
        assert_eq!(
            Some(file_status.identity),
            crate::file_identity(&fs::symlink_metadata(&file).unwrap())
        );
        // It names the file, nothing more: never read, never inherited.
        assert_eq!(
            rustix::io::read(&descriptor, &mut [0_u8; 1]),
            Err(Errno::BADF)
        );
        assert!(
            rustix::io::fcntl_getfd(&descriptor)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        // A pipe with no writer would block an opening that reads.
        let pipe = temporary.path().join("pipe");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &pipe,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        let (_, pipe_status) = open_path(&pipe, false).unwrap();
        assert_eq!(pipe_status.kind, EntryType::Other);
        let link = temporary.path().join("link");
        symlink(&folder, &link).unwrap();
        let (_, followed) = open_path(&link, true).unwrap();
        assert_eq!(followed.identity, status.identity, "followed to its target");
    }

    #[test]
    fn running_out_of_descriptors_is_no_refusal() {
        let raw = io::Error::from_raw_os_error;
        assert!(out_of_descriptors(&raw(Errno::MFILE.raw_os_error())));
        assert!(out_of_descriptors(&raw(Errno::NFILE.raw_os_error())));
        assert!(!out_of_descriptors(&raw(Errno::ACCESS.raw_os_error())));
        assert!(!out_of_descriptors(&io::ErrorKind::Other.into()));
    }

    #[test]
    fn a_sparse_file_takes_less_than_its_size() {
        let (temporary, directory) = directory();
        let file = fs::File::create(temporary.path().join("sparse")).unwrap();
        file.set_len(10 << 20).unwrap();
        let status = directory.status_of(OsStr::new("sparse")).unwrap();
        assert_eq!(status.size, 10 << 20);
        assert!(
            status
                .allocated
                .is_some_and(|allocated| allocated < 1 << 20),
            "{status:?}"
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
        fs::set_permissions(
            temporary.path().join("one"),
            fs::Permissions::from_mode(0o1644),
        )
        .unwrap();
        let one = directory.status_of(OsStr::new("one")).unwrap();
        assert_eq!(one.links, 2);
        assert!(!one.sticky, "only a directory is sticky");
        assert!(!one.locked);
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

    #[test]
    fn mount_roots_are_read_by_path_never_through_a_link() {
        assert_eq!(mount_root(Path::new("/")).unwrap(), Some(true));
        let temporary = tempfile::tempdir().unwrap();
        let plain = temporary.path().join("plain");
        fs::create_dir(&plain).unwrap();
        fs::write(temporary.path().join("file"), b"x").unwrap();
        assert_eq!(mount_root(&plain).unwrap(), Some(false));
        assert_eq!(
            mount_root(&temporary.path().join("file")).unwrap(),
            Some(false)
        );
        let link = temporary.path().join("root");
        symlink("/", &link).unwrap();
        assert_eq!(mount_root(&link).unwrap(), Some(false), "the link itself");
        assert_eq!(
            mount_root(&temporary.path().join("absent"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
