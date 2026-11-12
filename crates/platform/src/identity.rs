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
    use std::time::Duration;

    let modified = metadata.modified().ok()?;
    let seconds = u64::try_from(metadata.ctime()).ok()?;
    let nanoseconds = u32::try_from(metadata.ctime_nsec()).ok()?;
    let changed = SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, nanoseconds))?;
    Some(modified.max(changed))
}

/// When the object `metadata` was read for last changed: its modification
/// time, until the Windows port reads the change time too.
#[cfg(not(unix))]
#[must_use]
pub fn last_change(metadata: &Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
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
