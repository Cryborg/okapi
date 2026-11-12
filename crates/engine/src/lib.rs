//! Scan engine: finds what the cleaning rules can remove and names it by
//! identifier. Cleaning and receipts are still to come.
//!
//! [`scan`] walks the directories the cleaning rules name and reports what can
//! be removed, as groups of entries identified by [`ItemId`]. Cleaning will
//! take those identifiers, never a path (invariant 1), and check again, before
//! removing an entry, that it is the same object and still old enough,
//! descendants included (invariant 2).

mod options;
mod paths;
mod protection;
mod report;
mod roots;
mod scan;
mod walk;

pub use options::{DEFAULT_MIN_AGE, ExclusionError, Exclusions, ScanOptions};
pub use report::{
    AppFile, EntryKind, ItemId, Limit, Origin, ScanGroup, ScanIssue, ScanItem, ScanReport,
    ScanStats, UnsafeReason,
};
pub use scan::scan;

// The end-to-end tests write rule paths into JSON and create links: Unix only.
#[cfg(test)]
#[cfg(unix)]
mod tests;
