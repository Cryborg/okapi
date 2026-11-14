//! The files of one directory, read again when the screen opens it: the tree
//! keeps directories only.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;

use okapi_platform::EntryType;

use super::scan::{FilesError, UsageScan, file_size};
use super::tree::NodeId;

/// What a file is, as listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    /// A symbolic link, never followed: its own size only.
    Link,
    /// A socket, a pipe or a device.
    Other,
}

/// One file of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: OsString,
    pub kind: FileKind,
    /// On disk.
    pub allocated: u64,
    /// As its length says.
    pub apparent: u64,
    /// The directory where another of its names was counted (its path by
    /// [`UsageScan::path`]): it adds nothing here.
    pub counted_in: Option<NodeId>,
}

impl UsageScan {
    /// The files of the directory `of` (its subdirectories are in
    /// [`UsageScan::view`]), the largest first, then by name, read now: an
    /// entry gone since is left out, one that cannot be read is counted in
    /// the second value. A `statx` per entry: never called on the
    /// interface's thread. No subdirectory the analysis met is read again,
    /// nor any mount point it listed: only a mount made since may be. A file
    /// that took the name of a subdirectory since is left out with it.
    ///
    /// # Errors
    ///
    /// [`FilesError::NotRead`] when the analysis did not read `of`,
    /// [`FilesError::Changed`] when it is no longer, from the root and never
    /// through a link, the directory it read; the system's error when it
    /// cannot be opened or listed.
    pub fn files(&self, of: NodeId) -> Result<(Vec<FileEntry>, usize), FilesError> {
        let path = self.path(of).unwrap_or_default();
        let span = tracing::debug_span!(parent: self.span(), "files", path = %path.display());
        let _entered = span.enter();
        let started = std::time::Instant::now();
        let listed = self.read_files(of, &path);
        match &listed {
            Ok((files, unreadable)) => tracing::debug!(
                files = files.len(),
                unreadable,
                elapsed = ?started.elapsed(),
                "files of a folder read"
            ),
            // Asked of a folder still waiting, or removed since: no fault.
            Err(error @ (FilesError::NotRead | FilesError::Gone)) => {
                tracing::debug!(%error, "files of a folder not read");
            }
            Err(error) => tracing::warn!(%error, "files of a folder not read"),
        }
        listed
    }

    fn read_files(
        &self,
        of: NodeId,
        path: &std::path::Path,
    ) -> Result<(Vec<FileEntry>, usize), FilesError> {
        let directory = self.open(of)?;
        let met: BTreeSet<OsString> = self.child_names(of).into_iter().collect();
        let mut seen_here = BTreeSet::new();
        let mut files = Vec::new();
        let mut unreadable = 0;
        for name in directory.names()? {
            let entry = path.join(&name);
            // A folder met is no file; a mount point is never read: one that
            // stopped answering would hold the caller.
            if met.contains(&name) || self.is_mount(&entry) {
                continue;
            }
            let status = match directory.status_of(&name) {
                Ok(status) => status,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    tracing::debug!(path = %entry.display(), %error, "file unreadable");
                    unreadable += 1;
                    continue;
                }
            };
            let kind = match status.kind {
                EntryType::Directory => continue,
                EntryType::File => FileKind::File,
                EntryType::Link => FileKind::Link,
                EntryType::Other => FileKind::Other,
            };
            // Counted where the analysis counted it, under the first of its
            // names there.
            let counted_in = if status.links > 1 {
                match self.counted_by(status.identity) {
                    Some(counter) if counter == of && seen_here.insert(status.identity) => None,
                    counter => counter,
                }
            } else {
                None
            };
            let size = file_size(&status);
            files.push(FileEntry {
                name,
                kind,
                allocated: size.allocated,
                apparent: size.apparent,
                counted_in,
            });
        }
        files.sort_by(|first, second| {
            second
                .allocated
                .cmp(&first.allocated)
                .then_with(|| first.name.cmp(&second.name))
        });
        Ok((files, unreadable))
    }
}
