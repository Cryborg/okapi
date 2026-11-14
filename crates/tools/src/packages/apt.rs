//! When apt last downloaded its package lists, from the stamps left in
//! `/var/lib/apt/periodic`: `update-success-stamp`, touched after each
//! successful `apt update` by update-notifier-common's hook
//! (`15update-stamp`, Ubuntu), says it; without it (Debian), `update-stamp`,
//! touched by `apt.systemd.daily` when its update exits with 0, which a
//! network error does not always prevent. Neither: the lists' age is
//! unknown. Files' dates, no command.

use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// Where apt leaves its stamps.
pub const PERIODIC: &str = "/var/lib/apt/periodic";
/// The stamps, the surest first.
const STAMPS: [&str; 2] = ["update-success-stamp", "update-stamp"];

/// When the lists were last downloaded, by the surest stamp in `periodic`.
///
/// # Errors
/// A stamp that could not be read.
pub fn lists_downloaded_at(periodic: &Path) -> io::Result<Option<SystemTime>> {
    for stamp in STAMPS {
        match fs::symlink_metadata(periodic.join(stamp)) {
            Ok(metadata) => return metadata.modified().map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_lists_are_dated_by_the_surest_stamp_or_not_at_all() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(lists_downloaded_at(directory.path()).unwrap(), None);
        let at = |seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        let stamp = |name, seconds| {
            fs::File::create(directory.path().join(name))
                .unwrap()
                .set_modified(at(seconds))
                .unwrap();
        };
        // Debian: apt's daily run alone.
        stamp("update-stamp", 1_763_100_007);
        assert_eq!(
            lists_downloaded_at(directory.path()).unwrap(),
            Some(at(1_763_100_007))
        );
        // Ubuntu: the success stamp says it, even older than the daily run
        // that may have failed since.
        stamp("update-success-stamp", 1_763_000_003);
        assert_eq!(
            lists_downloaded_at(directory.path()).unwrap(),
            Some(at(1_763_000_003))
        );
    }
}
