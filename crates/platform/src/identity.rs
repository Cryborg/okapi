use std::fs::Metadata;
use std::time::SystemTime;

/// What makes a file or directory itself, whatever path reaches it: its device
/// and its inode. Two paths with the same identity are the same object, which
/// is how mount points and bind mounts are told apart from ordinary directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    #[must_use]
    pub const fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }

    /// The filesystem the object lives on.
    #[must_use]
    pub const fn device(self) -> u64 {
        self.device
    }

    #[must_use]
    pub const fn inode(self) -> u64 {
        self.inode
    }
}

/// When the object `metadata` was read for last changed, content or inode: the
/// later of its modification time and its status change time. Extractors and
/// copies set the modification time back to the source's; the status change
/// time cannot be set from user space, so a freshly extracted file looks new.
#[cfg(unix)]
#[must_use]
pub fn last_change(metadata: &Metadata) -> Option<SystemTime> {
    use std::os::unix::fs::MetadataExt;

    let modified = metadata.modified().ok()?;
    let nanoseconds = u32::try_from(metadata.ctime_nsec()).ok()?;
    let changed = instant(metadata.ctime(), nanoseconds)?;
    Some(later_of(modified, changed))
}

/// The instant `seconds` after the clock's origin (before it when negative),
/// plus `nanoseconds`, as the system writes file dates.
/// The last change of an object, content or inode: the later of its content
/// date and its status change date.
#[cfg(unix)]
pub(crate) fn later_of(modified: SystemTime, changed: SystemTime) -> SystemTime {
    modified.max(changed)
}

#[cfg(unix)]
pub(crate) fn instant(seconds: i64, nanoseconds: u32) -> Option<SystemTime> {
    let whole = std::time::Duration::from_secs(seconds.unsigned_abs());
    let seconds = if seconds >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(whole)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(whole)
    };
    seconds?.checked_add(std::time::Duration::from_nanos(u64::from(nanoseconds)))
}

/// When the object `metadata` was read for last changed: its modification
/// time, until the Windows port reads the change time too.
#[cfg(not(unix))]
#[must_use]
pub fn last_change(metadata: &Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
}

/// Who owns the object `metadata` was read for, and whether it is a sticky
/// directory.
#[cfg(unix)]
#[must_use]
pub fn ownership(metadata: &Metadata) -> Option<crate::Ownership> {
    use std::os::unix::fs::MetadataExt;

    /// The sticky bit of a file mode.
    const STICKY: u32 = 0o1000;
    Some(crate::Ownership {
        owner: metadata.uid(),
        sticky: metadata.is_dir() && metadata.mode() & STICKY != 0,
    })
}

/// Who owns the object `metadata` was read for: unknown here.
#[cfg(not(unix))]
#[must_use]
pub fn ownership(_metadata: &Metadata) -> Option<crate::Ownership> {
    None
}

/// Whether [`file_identity`] knows the identity of files on this system.
pub const IDENTITY_SUPPORTED: bool = cfg!(unix);

/// The identity of the object `metadata` was read for.
#[cfg(unix)]
#[must_use]
pub fn file_identity(metadata: &Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;

    Some(FileIdentity::new(metadata.dev(), metadata.ino()))
}

/// The identity of the object `metadata` was read for: unknown here, until the
/// Windows port reads the volume serial number and the file index.
#[cfg(not(unix))]
#[must_use]
pub fn file_identity(_metadata: &Metadata) -> Option<FileIdentity> {
    None
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    fn identity(path: &std::path::Path) -> FileIdentity {
        file_identity(&std::fs::symlink_metadata(path).unwrap()).unwrap()
    }

    #[test]
    fn identity_tells_objects_apart_whatever_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir(root.join("one")).unwrap();
        std::fs::create_dir(root.join("two")).unwrap();
        std::fs::write(root.join("file"), b"x").unwrap();
        std::fs::hard_link(root.join("file"), root.join("same-file")).unwrap();
        assert_ne!(identity(&root.join("one")), identity(&root.join("two")));
        assert_eq!(
            identity(&root.join("file")),
            identity(&root.join("same-file"))
        );
        assert_eq!(identity(&root.join("one")), identity(&root.join("one/.")));
        let device = identity(&root.join("one")).device();
        assert_eq!(identity(&root.join("two")).device(), device);
    }

    #[cfg(unix)]
    #[test]
    fn ownership_reads_the_owner_and_the_sticky_bit() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let shared = directory.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let metadata = std::fs::symlink_metadata(&shared).unwrap();
        let read = ownership(&metadata).unwrap();
        assert!(read.sticky);
        let creator = std::fs::symlink_metadata(directory.path()).unwrap().uid();
        assert_eq!(read.owner, creator, "the process created both");
        // Linux lets anyone set the sticky bit on a file; it means nothing there.
        if cfg!(target_os = "linux") {
            let file = directory.path().join("file");
            std::fs::write(&file, b"x").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o1644)).unwrap();
            let file = ownership(&std::fs::symlink_metadata(&file).unwrap()).unwrap();
            assert!(!file.sticky, "only a directory is sticky");
        }
        let plain = ownership(&std::fs::symlink_metadata(directory.path()).unwrap()).unwrap();
        assert!(!plain.sticky);
    }

    #[cfg(unix)]
    #[test]
    fn dates_before_the_clock_origin_are_kept() {
        assert_eq!(
            instant(-2, 500_000_000),
            Some(SystemTime::UNIX_EPOCH - std::time::Duration::from_millis(1_500))
        );
        assert_eq!(
            instant(3, 1),
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(3, 1))
        );
    }

    #[test]
    fn a_file_set_back_in_time_still_changed_now() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extracted");
        std::fs::write(&path, b"x").unwrap();
        let long_ago = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.modified().unwrap(), long_ago);
        let changed = last_change(&metadata).unwrap();
        assert!(
            changed > long_ago,
            "the status change time keeps the extraction recent"
        );
    }

    #[test]
    fn a_modification_in_the_future_is_the_last_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ahead");
        std::fs::write(&path, b"x").unwrap();
        let ahead = SystemTime::now() + std::time::Duration::from_hours(24);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(ahead)
            .unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(last_change(&metadata), Some(ahead));
    }
}
