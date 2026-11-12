//! Operating system abstractions, one trait per domain and one module per OS.

mod directory;
mod identity;
mod session;
#[cfg(target_os = "linux")]
mod variables;

pub use directory::{
    DESCRIPTOR_REMOVAL_SUPPORTED, Directory, EntryStatus, EntryType, Ownership, mount_root,
    sticky_allows, tells_mount_roots,
};
pub use identity::{FileIdentity, IDENTITY_SUPPORTED, file_identity, last_change, ownership};
pub use session::{
    create_private_directory, effective_user, open_for_reading, open_journal, running_as_root,
};
#[cfg(target_os = "linux")]
pub use variables::{VariablesError, config_directory, state_directory, template_variables};
