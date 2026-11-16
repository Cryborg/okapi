//! The folders searched: opened where they really are, each once, then
//! listed one after the other, the mount points below each read once and
//! its filesystem judged.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use okapi_platform::{Directory, filesystem_may_alias, filesystem_undated};

use super::walk::{Cancelled, Listing, Scope, walk};
use super::{Found, Root, SearchError, SearchStats};

/// How the mount points below a root are read, and the filesystem it is on.
pub(crate) type Mounts = fn(&Path) -> (BTreeSet<PathBuf>, Option<String>);

/// The roots opened, and their folders.
pub(crate) struct Opened {
    pub(crate) roots: Vec<Root>,
    pub(crate) directories: Vec<Directory>,
}

/// Opens each root where it really is, leaving out one given twice.
pub(crate) fn open_roots(paths: &[PathBuf]) -> Result<Opened, SearchError> {
    let mut opened: Vec<(Root, Directory)> = Vec::new();
    for path in paths {
        let root = std::fs::canonicalize(path).and_then(|canonical| {
            let directory = Directory::open(&canonical)?;
            let status = directory.status()?;
            Ok((
                Root {
                    path: canonical,
                    identity: status.identity,
                    // Both known once its filesystem is read (`list`).
                    may_alias: true,
                    undated: true,
                },
                directory,
            ))
        });
        let root = root.map_err(|error| {
            tracing::warn!(path = %path.display(), %error, "folder to search not opened");
            SearchError::Root {
                path: path.clone(),
                error,
            }
        })?;
        opened.push(root);
    }
    // One folder given twice (or by two paths) is searched once. A root
    // inside another is searched as its own: the walk of the outer one
    // leaves it, as it may lie below a folder that walk skips.
    let mut kept: Vec<(Root, Directory)> = Vec::new();
    for (root, directory) in opened {
        if kept
            .iter()
            .any(|(earlier, _)| earlier.identity == root.identity)
        {
            tracing::info!(path = %root.path.display(), "folder given twice: searched once");
        } else {
            kept.push((root, directory));
        }
    }
    if kept.is_empty() {
        return Err(SearchError::NoRoot);
    }
    let (roots, directories) = kept.into_iter().unzip();
    Ok(Opened { roots, directories })
}

/// Lists every root, the mount points below each read once by `mounts`,
/// each root's filesystem judged.
pub(crate) fn list(
    listing: &Listing<'_>,
    roots: &mut [Root],
    directories: Vec<Directory>,
    mounts: Mounts,
    stats: &mut SearchStats,
) -> Result<Vec<Found>, Cancelled> {
    let mut below = BTreeSet::new();
    for root in roots.iter_mut() {
        let (points, filesystem) = mounts(&root.path);
        below.extend(points);
        // An unknown filesystem (the table of mounts unreadable) may be
        // either: no copy found there keeps another.
        root.may_alias = filesystem.as_deref().is_none_or(filesystem_may_alias);
        root.undated = filesystem.as_deref().is_none_or(filesystem_undated);
        tracing::info!(
            path = %root.path.display(),
            filesystem = filesystem.as_deref(),
            may_alias = root.may_alias,
            undated = root.undated,
            "filesystem of a root"
        );
    }
    let roots: &[Root] = roots;
    let scope = Scope {
        listing,
        roots: &roots.iter().map(|root| root.identity).collect(),
        mounts: &below,
    };
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for (index, (root, directory)) in roots.iter().zip(directories).enumerate() {
        if listing.exclusions.excludes(&root.path)
            || listing.exclusions.excludes_identity(root.identity)
        {
            stats.excluded += 1;
            tracing::info!(path = %root.path.display(), "folder searched is excluded: not searched");
            continue;
        }
        walk(
            &scope, index, directory, &root.path, stats, &mut seen, &mut found,
        )?;
    }
    Ok(found)
}
