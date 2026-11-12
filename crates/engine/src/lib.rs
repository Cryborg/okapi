//! Scan and cleaning engine: finds what the cleaning rules can remove, names
//! it by identifier, and removes what is asked for after checking it again.
//!
//! [`scan`] walks the directories the cleaning rules name and reports what can
//! be removed, as groups of entries identified by [`ItemId`]. [`clean`] takes
//! the report and the identifiers of the entries to remove, never a path
//! (invariant 1), and checks again, before removing an entry, that it is the
//! same object and still old enough, descendants included (invariant 2). The
//! [`CleanReceipt`] says what became of each one.

mod clean;
mod options;
mod paths;
mod preconditions;
mod protection;
mod report;
mod roots;
mod scan;
mod size;
mod walk;
mod watch;

pub use clean::{
    CleanOutcome, CleanPass, CleanReceipt, CleanResult, KeepReason, clean, clean_watched,
};
pub use options::{
    DEFAULT_MIN_AGE, DEFAULT_MIN_AGE_MINUTES, ExclusionError, Exclusions, ScanOptions,
    duration_of_minutes, settings_min_age,
};
pub use preconditions::Stop;
pub use report::{
    AppFile, BrowserLock, Category, EntryKind, ItemId, Limit, Origin, ScanGroup, ScanIssue,
    ScanItem, ScanReport, ScanStats, UnsafeReason, checked_by_default,
};
pub use scan::{DefaultChoice, scan, scan_watched};
pub use size::{ByteSize, SizeUnit};
pub use watch::{Cancel, Progress, Watch};

// The end-to-end tests write rule paths into JSON and create links: Unix only.
#[cfg(test)]
#[cfg(unix)]
mod tests;
