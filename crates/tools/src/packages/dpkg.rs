//! What dpkg's own files say of dpkg (`/var/lib/dpkg`), read and never
//! written: whether it is writing.

use std::fs;
use std::io;
use std::path::Path;

/// Where dpkg keeps its state.
pub const DPKG_DIRECTORY: &str = "/var/lib/dpkg";

/// How many updates dpkg has not yet folded into `status`: some means it is
/// writing, or was cut short, and `status` alone is not the truth.
///
/// # Errors
///
/// The system's error reading `updates/`.
pub fn pending_updates(dpkg: &Path) -> io::Result<usize> {
    let mut pending = 0;
    for entry in fs::read_dir(dpkg.join("updates"))? {
        // dpkg names its updates by number; `tmp.i` is its work file.
        if entry?
            .file_name()
            .to_str()
            .is_some_and(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
        {
            pending += 1;
        }
    }
    Ok(pending)
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
}
