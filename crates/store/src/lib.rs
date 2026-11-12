//! Persistent settings and history: what the user set in
//! `$XDG_CONFIG_HOME/okapi/settings.json`, and one line per cleaning in
//! `$XDG_STATE_HOME/okapi/history.jsonl`. Nothing here depends on the engine
//! nor on the interface: callers turn their results into these plain types.

mod history;
mod settings;

use std::io;
use std::path::PathBuf;

use thiserror::Error;

pub use history::{History, HistoryEntry, HistoryOutcome};
pub use settings::Settings;

/// A settings or history file could not be used.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("`{}` cannot be read: {error}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        error: io::Error,
    },

    /// A link (dotfiles) that leads nowhere: the settings exist but cannot be
    /// reached, which is never taken for none.
    #[error("`{}` is a link that leads nowhere", path.display())]
    BrokenLink { path: PathBuf },

    /// A directory, a pipe or a device where a file is expected.
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

    #[error("`{}` has version {version}, this Okapi reads version {}", path.display(), settings::SETTINGS_VERSION)]
    UnsupportedVersion { path: PathBuf, version: u32 },

    #[error("`{}` cannot be written: {error}", path.display())]
    Unwritable {
        path: PathBuf,
        #[source]
        error: io::Error,
    },
}
