//! What dpkg's own files say of dpkg (`/var/lib/dpkg`), read and never
//! written: whether it is writing, or was cut short.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// Where dpkg keeps its state.
pub const DPKG_DIRECTORY: &str = "/var/lib/dpkg";
/// Where the kernel lists the file locks held.
pub const PROC_LOCKS: &str = "/proc/locks";
/// The locks dpkg and apt take: dpkg's own, apt's front end's, and those of
/// apt's lists and downloads (any one held is an installation at work).
pub const LOCK_FILES: [&str; 4] = [
    "/var/lib/dpkg/lock",
    "/var/lib/dpkg/lock-frontend",
    "/var/lib/apt/lists/lock",
    "/var/cache/apt/archives/lock",
];

/// How many updates dpkg has not yet folded into `status`: some means it is
/// writing, or was cut short, and `status` alone is not the truth.
///
/// # Errors
///
/// The system's error reading `updates/`.
pub fn pending_updates(dpkg: &Path) -> io::Result<usize> {
    let mut pending = 0;
    for entry in fs::read_dir(dpkg.join("updates"))? {
        if is_update(&entry?) {
            pending += 1;
        }
    }
    Ok(pending)
}

/// Whether dpkg was cut short: updates not folded into `status`, and
/// nobody holds any of `lock_files` by `locks` (the kernel's list,
/// `/proc/locks`). A full `updates/` alone says neither: it is dpkg's
/// journal while it works, and a dpkg at work, or an apt waiting on a
/// question, holds its locks however long it is quiet.
///
/// # Errors
///
/// The system's error reading `updates/` or a lock's identity.
pub fn interrupted(dpkg: &Path, lock_files: &[&Path], locks: &str) -> io::Result<bool> {
    if pending_updates(dpkg)? == 0 {
        return Ok(false);
    }
    for lock in lock_files {
        match fs::metadata(lock) {
            Ok(metadata) if held(&metadata, locks) => return Ok(false),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

/// Whether `locks` names a lock on the file of `metadata`, known by its
/// inode alone: the kernel names a file `major:minor:inode` by its file
/// system's device, which is not the one `stat` gives on btrfs. Another
/// file of the same inode elsewhere can only make it held: busy, never cut
/// short.
fn held(metadata: &fs::Metadata, locks: &str) -> bool {
    let inode = format!(":{}", metadata.ino());
    locks.lines().any(|line| {
        line.split_whitespace()
            .any(|field| field.ends_with(&inode) && field.matches(':').count() == 2)
    })
}

/// Whether `entry` is an update of dpkg: named by number (`tmp.i` is its
/// work file).
fn is_update(entry: &fs::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpkg_writing_is_seen_in_its_updates() {
        let dpkg = tempfile::tempdir().unwrap();
        let updates = dpkg.path().join("updates");
        fs::create_dir(&updates).unwrap();
        fs::write(updates.join("tmp.i"), "").unwrap();
        assert_eq!(
            pending_updates(dpkg.path()).unwrap(),
            0,
            "its work file alone"
        );
        fs::write(updates.join("0003"), "").unwrap();
        assert_eq!(pending_updates(dpkg.path()).unwrap(), 1);
    }

    #[test]
    fn dpkg_is_cut_short_only_with_updates_left_and_no_lock_held() {
        // Catches a dpkg at work, however quiet (a long script, a question
        // in a terminal), said cut short; and one cut short said busy.
        let dpkg = tempfile::tempdir().unwrap();
        let updates = dpkg.path().join("updates");
        fs::create_dir(&updates).unwrap();
        let lock = dpkg.path().join("lock");
        let other = dpkg.path().join("lock-frontend");
        fs::write(&lock, "").unwrap();
        fs::write(&other, "").unwrap();
        let inode = fs::metadata(&lock).unwrap().ino();
        let locks = [lock.as_path(), other.as_path()];
        // The device as btrfs names it, unlike `stat`: never compared.
        let held = format!("1: POSIX  ADVISORY  WRITE 4242 00:2a:{inode} 0 EOF\n");
        let free = format!("1: POSIX  ADVISORY  WRITE 4242 00:2a:{inode}9 0 EOF\n");
        assert!(
            !interrupted(dpkg.path(), &locks, &free).unwrap(),
            "nothing left"
        );
        fs::write(updates.join("0001"), "").unwrap();
        assert!(
            interrupted(dpkg.path(), &locks, &free).unwrap(),
            "left, no lock"
        );
        assert!(
            !interrupted(dpkg.path(), &locks, &held).unwrap(),
            "left, lock held"
        );
        assert!(
            interrupted(dpkg.path(), &[dpkg.path().join("none").as_path()], &held).unwrap(),
            "a lock file absent is no lock"
        );
        // Held on another of the locks than the first.
        let other_inode = fs::metadata(&other).unwrap().ino();
        let other_held = format!("1: POSIX  ADVISORY  WRITE 4242 00:2a:{other_inode} 0 EOF\n");
        assert!(
            !interrupted(dpkg.path(), &locks, &other_held).unwrap(),
            "second lock held"
        );
        // A lock whose identity cannot be read says neither.
        let unreadable = lock.join("below-a-file");
        assert!(interrupted(dpkg.path(), &[unreadable.as_path()], &free).is_err());
    }
}
