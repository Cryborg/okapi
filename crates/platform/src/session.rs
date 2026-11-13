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

/// The user who started the process (its real user), whom the system's
/// access checks (`accessat`) judge, and the cleaning's with them.
#[cfg(target_os = "linux")]
#[must_use]
pub fn real_user() -> Option<u32> {
    Some(rustix::process::getuid().as_raw())
}

/// The user who started the process: unknown here yet.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub const fn real_user() -> Option<u32> {
    None
}

/// Whether the process runs as root: the command line refuses to run then,
/// removals that need the administrator go through the privileged helper.
#[must_use]
pub fn running_as_root() -> bool {
    effective_user() == Some(crate::directory::ROOT_USER)
}

/// Creates `directory` and its missing parents, `directory` itself readable
/// by its owner only (it holds the journal and the history, which list file
/// names), and makes it so if it already existed.
///
/// # Errors
///
/// The system's error.
pub fn create_private_directory(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        /// Only its owner may read or enter it.
        const PRIVATE: u32 = 0o700;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE)
            .create(directory)?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(PRIVATE))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(directory)
    }
}

/// Opens the journal at `path` for appending (and reading), creating it readable by its
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
    // Readable too: the history checks how its last line ends through the
    // descriptor it writes to, never by its path again.
    let flags = OFlags::RDWR
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

/// Opens the file at `path` for reading, a link followed, without ever
/// waiting: a pipe opens at once (and is then told apart by its type).
///
/// # Errors
///
/// The system's error.
#[cfg(target_os = "linux")]
pub fn open_for_reading(path: &Path) -> io::Result<File> {
    use rustix::fs::{CWD, Mode, OFlags, openat};

    let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    Ok(File::from(openat(CWD, path, flags, Mode::empty())?))
}

/// Opens the file at `path` for reading.
///
/// # Errors
///
/// The system's error.
#[cfg(not(target_os = "linux"))]
pub fn open_for_reading(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Opens the journal at `path` for appending (and reading).
///
/// # Errors
///
/// The system's error.
#[cfg(not(target_os = "linux"))]
pub fn open_journal(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
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
    fn a_private_directory_is_made_private_even_if_it_existed() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state/okapi");
        create_private_directory(&state).unwrap();
        let mode = std::fs::metadata(&state).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_directory(&state).unwrap();
        let mode = std::fs::metadata(&state).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
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
