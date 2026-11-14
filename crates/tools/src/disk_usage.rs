//! What takes the space of a folder: an analysis on threads of its own
//! ([`UsageScan`]), read as it goes, directory by directory ([`View`]), the
//! files of one read again when it is opened ([`UsageScan::files`]). It
//! never follows a link, never enters another mount, counts a file of
//! several names once, and removes nothing.

mod files;
mod scan;
mod tree;

pub use files::{FileEntry, FileKind};
pub use scan::{FilesError, Outcome, Progress, ScanError, UsageScan, default_threads};
pub use tree::{Entry, NodeId, NodeState, Size, View};

// Real folders, read through descriptors: Linux only.
#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;
