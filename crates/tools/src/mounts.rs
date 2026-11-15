//! The mount points below a folder, read in the table of mounts once: a
//! walk never even reads one (`EntryStatus::other_mount` judges the rest);
//! and the mount a path lies on.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use okapi_platform::Mount;

/// The mount `path` lies on: the deepest whose mount point holds it.
pub(crate) fn mount_of<'a>(path: &Path, mounts: &'a [Mount]) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.mount_point))
        .max_by_key(|mount| mount.mount_point.components().count())
}

/// The mount points strictly below `root`, and the filesystem `root` is on.
pub(crate) fn mounts_below(root: &Path) -> (BTreeSet<PathBuf>, Option<String>) {
    match okapi_platform::mounts() {
        Ok(list) => {
            if list.malformed > 0 {
                tracing::warn!(
                    lines = list.malformed,
                    "mounts partly unreadable: a mount missed is read for being one"
                );
            }
            let filesystem = mount_of(root, &list.mounts).map(|mount| mount.filesystem.clone());
            let below = list
                .mounts
                .into_iter()
                .map(|mount| mount.mount_point)
                .filter(|point| point.starts_with(root) && point != root)
                .collect();
            (below, filesystem)
        }
        Err(error) => {
            // Each directory is still read for being a mount root.
            tracing::warn!(%error, "mounts unreadable: a mount that stopped answering may hold the walk");
            (BTreeSet::new(), None)
        }
    }
}
