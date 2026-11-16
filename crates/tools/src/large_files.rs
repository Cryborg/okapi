//! The largest files in the user's folders: a search on a thread of its own
//! ([`LargeFileSearch`]) lists the plain files of the user's that take at
//! least a floor on disk. It never follows a link, never enters another
//! mount, a hidden folder nor a project (but one the user chose), lists a
//! file of several names once, and never a file of another user. What it
//! found ([`LargeFileReport`]) names each file by an identifier valid in it
//! only; files go to the trash only by those identifiers, each read again
//! just before ([`trash`]).

mod kind;
mod report;
mod search;
mod trash;

pub use crate::listing::{SearchError, SearchStats};
pub use crate::searching::Outcome;
pub use kind::FileKind;
pub use report::{FileId, LargeFile, LargeFileReport};
pub use search::{FLOORS, LargeFileSearch, MIN_SIZE, Progress, SearchOptions};
pub use trash::{KeepReason, Refusal, TrashOutcome, TrashReceipt, TrashResult, trash};

// Real folders, read through descriptors: Linux only.
#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;
