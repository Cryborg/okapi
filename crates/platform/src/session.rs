use std::fs::File;
use std::io;
use std::path::Path;

/// The user the process acts as (its effective user), who removes what a
/// cleaning removes.
#[cfg(target_os = "linux")]
#[must_use]
pub fn effective_user() -> Option<u32> {
    Some(rustix::process::geteuid().as_raw())
}

/// The user the process acts as: unknown here yet.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub const fn effective_user() -> Option<u32> {
    None
}

/// Whether the process runs as root: the command line refuses to run then,
/// removals that need the administrator go through the privileged helper.
#[must_use]
pub fn running_as_root() -> bool {
    effective_user() == Some(crate::directory::ROOT_USER)
}

/// Opens the journal at `path` for appending, creating it readable by its
/// owner only. A link, a pipe or anything but a regular file is refused,
/// never followed nor waited on.
///
/// # Errors
///
/// The system's error, or [`io::ErrorKind::InvalidInput`] when `path` is not a
/// regular file.
#[cfg(target_os = "linux")]
pub fn open_journal(path: &Path) -> io::Result<File> {
    use rustix::fs::{CWD, Mode, OFlags, openat};

    /// Readable and writable by its owner only: it lists file names.
    const OWNER_ONLY: Mode = Mode::RUSR.union(Mode::WUSR);
    let flags = OFlags::WRONLY
        | OFlags::APPEND
        | OFlags::CREATE
        | OFlags::NOFOLLOW
        | OFlags::NONBLOCK
        | OFlags::CLOEXEC;
    let file = File::from(openat(CWD, path, flags, OWNER_ONLY)?);
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // A journal created earlier with looser rights is made private too.
    rustix::fs::fchmod(&file, OWNER_ONLY)?;
    Ok(file)
}

/// Opens the journal at `path` for appending.
///
/// # Errors
///
/// The system's error.
#[cfg(not(target_os = "linux"))]
pub fn open_journal(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    #[test]
    fn the_journal_is_appended_to_and_private() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("okapi.log");
        open_journal(&path).unwrap().write_all(b"one\n").unwrap();
        open_journal(&path).unwrap().write_all(b"two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        open_journal(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "made private again");
    }

    #[test]
    fn a_link_or_a_pipe_is_never_a_journal() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"kept").unwrap();
        let link = directory.path().join("link.log");
        symlink(&target, &link).unwrap();
        assert!(open_journal(&link).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
        let pipe = directory.path().join("pipe.log");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &pipe,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        // Without a reader, opening it fails at once (`ENXIO`): never a wait.
        assert!(open_journal(&pipe).is_err());
        assert!(open_journal(directory.path()).is_err());
    }
}
