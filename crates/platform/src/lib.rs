//! Operating system abstractions, one trait per domain and one module per OS.

mod browser;
mod directory;
#[cfg(target_os = "linux")]
mod elevation;
mod identity;
mod session;
#[cfg(target_os = "linux")]
mod variables;

pub use browser::{CHROMIUM_LOCK, lock_is_live};
#[cfg(target_os = "linux")]
pub use browser::{Holder, RunningBrowser, UnknownHolder, holder};
pub use directory::{
    DESCRIPTOR_REMOVAL_SUPPORTED, Directory, EntryStatus, EntryType, Ownership, ROOT_USER,
    mount_root, sticky_allows, tells_mount_roots,
};
#[cfg(target_os = "linux")]
pub use elevation::{HELPER_PATH, PKEXEC_PATH};
pub use identity::{FileIdentity, IDENTITY_SUPPORTED, file_identity, last_change, ownership};
pub use session::{
    create_private_directory, effective_user, open_for_reading, open_journal, real_user,
    running_as_root,
};
#[cfg(target_os = "linux")]
pub use variables::{VariablesError, config_directory, state_directory, template_variables};
