use std::io::{self, Read};
use std::path::Path;

/// The text of the file at `path`, of at most `limit` bytes: a file of the
/// kernel tells no size, so a longer one is cut, which is said
/// ([`io::ErrorKind::InvalidData`]), never taken for the whole.
///
/// # Errors
///
/// The system's error, or [`io::ErrorKind::InvalidData`] when the file is
/// longer than `limit` or not UTF-8.
pub(crate) fn read_capped(path: &Path, limit: usize) -> io::Result<String> {
    String::from_utf8(read_capped_bytes(path, limit)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// The bytes of the file at `path`, as [`read_capped`] reads them, UTF-8 or
/// not (a mount point or a process name may be any bytes).
///
/// # Errors
///
/// The system's error, or [`io::ErrorKind::InvalidData`] when the file is
/// longer than `limit`.
pub(crate) fn read_capped_bytes(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    read_up_to(crate::open_for_reading(path)?, limit)
}

/// The bytes of the regular file at `path`, of at most `limit`: never opened
/// when the file itself is a link (one in a folder above is followed), never
/// waited on (a pipe or a device is refused), never read past the limit.
///
/// # Errors
///
/// The system's error ([`io::ErrorKind::InvalidInput`] for what is not a
/// regular file, the system's own for a link), or
/// [`io::ErrorKind::InvalidData`] when it is longer than `limit`.
pub fn read_regular_capped(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let file = open_no_link(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    read_up_to(file, limit)
}

#[cfg(target_os = "linux")]
fn open_no_link(path: &Path) -> io::Result<std::fs::File> {
    use rustix::fs::{CWD, Mode, OFlags, openat};

    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    Ok(std::fs::File::from(openat(
        CWD,
        path,
        flags,
        Mode::empty(),
    )?))
}

#[cfg(not(target_os = "linux"))]
fn open_no_link(path: &Path) -> io::Result<std::fs::File> {
    if std::fs::symlink_metadata(path)?.is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a link"));
    }
    std::fs::File::open(path)
}

pub(crate) fn read_up_to(file: std::fs::File, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    file.take(cap).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("longer than {limit} bytes"),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_within_the_limit_is_read_whole_and_a_longer_one_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("status");
        std::fs::write(&path, "12345").unwrap();
        assert_eq!(read_capped(&path, 5).unwrap(), "12345");
        assert_eq!(
            read_capped(&path, 4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_regular_file_alone_is_read_never_a_link_a_pipe_or_a_device() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("icon");
        std::fs::write(&path, "12345").unwrap();
        assert_eq!(read_regular_capped(&path, 5).unwrap(), b"12345");
        assert_eq!(
            read_regular_capped(&path, 4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_regular_capped(&link, 5).is_err(), "a link");
        assert_eq!(
            read_regular_capped(directory.path(), 5).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "a folder"
        );
        // Never waited on: a pipe nobody writes to is refused at once.
        let pipe = directory.path().join("pipe");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &pipe,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        assert_eq!(
            read_regular_capped(&pipe, 5).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            read_regular_capped(Path::new("/dev/zero"), 5)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput,
            "a device, never read to the limit"
        );
    }
}
