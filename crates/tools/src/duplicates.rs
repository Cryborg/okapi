//! Copies of one content in the user's folders: a search on threads of its
//! own ([`DuplicateSearch`]) lists the plain files, then tells those of one
//! size apart by the blake3 key of their ends, then of all their content.
//! It never follows a link, never enters another mount, a hidden folder nor
//! a project (but one the user chose), and counts a file of several names
//! once. Copies go to the trash only by identifiers of that report, each
//! read again just before, never the last of a group ([`trash`]). What it found
//! ([`DuplicateReport`]) names each copy by an identifier valid in it only.

mod compare;
mod report;
mod search;
mod trash;

pub use crate::listing::{SearchError, SearchStats};
pub use report::{CopyId, DuplicateReport, FileCopy, Group};
pub use search::{
    DuplicateSearch, MIN_SIZE, Outcome, Progress, SearchOptions, default_threads, threads_for_roots,
};
pub use trash::{KeepReason, Refusal, TrashOutcome, TrashReceipt, TrashResult, trash};

// Real folders, read through descriptors: Linux only.
#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;
