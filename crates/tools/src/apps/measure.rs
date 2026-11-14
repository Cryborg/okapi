//! What a folder of an application takes on the disk (its data, a flatpak
//! deployment), measured by the disk usage analysis on one thread. Its root
//! is never a link: the analysis would follow one there.

use std::io;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use crate::disk_usage::{NodeId, Outcome, UsageScan};

/// How long a wait for the end lasts, between two looks at whether to stop.
const WAIT_STEP: Duration = Duration::from_millis(100);

/// What a measure found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measured {
    /// No such folder: the application kept nothing there.
    Absent,
    /// On the disk; `partial` when some of it could not be read, or the
    /// measure was stopped.
    Bytes { bytes: u64, partial: bool },
    /// Not measured: a link, not a folder, or one that cannot be opened
    /// (traced with its reason).
    NotMeasured,
}

/// A measure under way, stopped by [`Measuring::cancel`] from another
/// thread.
#[derive(Debug)]
pub struct Measuring {
    scan: UsageScan,
}

impl Measuring {
    /// Starts measuring `folder`. Never on the interface's thread: opening
    /// a folder may wait.
    ///
    /// # Errors
    ///
    /// What it is, when there is nothing to measure.
    pub fn start(folder: &Path) -> Result<Self, Measured> {
        match std::fs::symlink_metadata(folder) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(Measured::Absent),
            Err(error) => {
                tracing::debug!(path = %folder.display(), %error, "folder not measured");
                return Err(Measured::NotMeasured);
            }
            Ok(metadata) if !metadata.is_dir() => {
                tracing::debug!(path = %folder.display(), link = metadata.is_symlink(), "not a folder: not measured");
                return Err(Measured::NotMeasured);
            }
            Ok(_) => {}
        }
        UsageScan::start(folder, NonZeroUsize::MIN)
            .map(|scan| Self { scan })
            .map_err(|error| {
                tracing::debug!(path = %folder.display(), %error, "folder not measured");
                Measured::NotMeasured
            })
    }

    /// Stops the measure: what was read is kept, said partial.
    pub fn cancel(&self) {
        self.scan.cancel();
    }

    /// Waits for the measure to end, then says it.
    #[must_use]
    pub fn finish(self) -> Measured {
        let outcome = loop {
            if let Some(outcome) = self.scan.wait(WAIT_STEP) {
                break outcome;
            }
        };
        let Some(view) = self.scan.view(NodeId::ROOT) else {
            return Measured::NotMeasured;
        };
        let measured = Measured::Bytes {
            bytes: view.directory.total.allocated,
            partial: view.directory.partial || outcome == Outcome::Cancelled,
        };
        tracing::debug!(path = %self.scan.root().display(), ?measured, "folder measured");
        measured
    }
}

/// Measures `folder` to its end.
#[must_use]
pub fn measure(folder: &Path) -> Measured {
    Measuring::start(folder).map_or_else(|measured| measured, Measuring::finish)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_measured_and_a_link_never_followed() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("big"), vec![1_u8; 64 * 1024]).unwrap();
        let Measured::Bytes { bytes, partial } = measure(&data) else {
            panic!("measured");
        };
        assert!(bytes >= 64 * 1024, "{bytes}");
        assert!(!partial);

        let link = root.path().join("link");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        assert_eq!(
            measure(&link),
            Measured::NotMeasured,
            "a link, never followed"
        );
        assert_eq!(measure(&root.path().join("none")), Measured::Absent);
        assert_eq!(measure(&data.join("big")), Measured::NotMeasured, "a file");
    }
}
