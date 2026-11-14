//! Persistent settings, history and journal: what the user set in
//! `$XDG_CONFIG_HOME/okapi/settings.json`, the layout of the dashboard
//! beside it (`dashboard.json`), one line per cleaning in
//! `$XDG_STATE_HOME/okapi/history.jsonl`, the summary of the last analysis
//! beside it (`last-analysis.json`), how the user last left the interface
//! (`interface.json`, there too), and every trace in the journal. A
//! cleaning's receipt becomes its history line here, the one form both
//! interfaces write; nothing here depends on an interface.

mod dashboard;
mod file;
mod history;
mod interface_state;
pub mod journal;
mod last_analysis;
mod settings;

use std::io;
use std::path::PathBuf;

use thiserror::Error;

pub use dashboard::{DashboardLayout, DashboardWidget, LoadedLayout, WidgetSize};
pub use history::{
    AdminRecord, History, HistoryEntry, HistoryOutcome, HistoryUnavailable, LastCleaning, Opened,
    seconds,
};
pub use interface_state::InterfaceState;
pub use last_analysis::LastAnalysis;
pub use settings::Settings;

/// A file Okapi keeps could not be used.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("`{}` cannot be read: {error}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        error: io::Error,
    },

    /// A link (dotfiles) that leads nowhere: the file exists but cannot be
    /// reached, which is never taken for none.
    #[error("`{}` is a link that leads nowhere", path.display())]
    BrokenLink { path: PathBuf },

    /// A directory, a pipe or a device where a file is expected, or, for a
    /// file to replace, a link (never written through).
    #[error("`{}` is not a regular file", path.display())]
    NotAFile { path: PathBuf },

    #[error("`{}` is larger than {limit} bytes", path.display())]
    TooLarge { path: PathBuf, limit: u64 },

    #[error("`{}` is invalid: {error}", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        error: serde_json::Error,
    },

    #[error("`{}` has version {version}, this Okapi reads version {supported}", path.display())]
    UnsupportedVersion {
        path: PathBuf,
        version: u32,
        supported: u32,
    },

    #[error("`{}` cannot be written: {error}", path.display())]
    Unwritable {
        path: PathBuf,
        #[source]
        error: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsupported_version_names_the_one_this_okapi_reads() {
        let error = StoreError::UnsupportedVersion {
            path: PathBuf::from("/okapi/dashboard.json"),
            version: 5,
            supported: 3,
        };
        assert_eq!(
            error.to_string(),
            "`/okapi/dashboard.json` has version 5, this Okapi reads version 3"
        );
    }
}
