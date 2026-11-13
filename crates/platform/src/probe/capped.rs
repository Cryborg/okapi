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
    let file = crate::open_for_reading(path)?;
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
}
