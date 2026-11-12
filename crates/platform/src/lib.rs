//! Operating system abstractions, one trait per domain and one module per OS.

mod directory;
mod identity;
#[cfg(target_os = "linux")]
mod variables;

pub use directory::{
    DESCRIPTOR_REMOVAL_SUPPORTED, Directory, EntryStatus, EntryType, tells_mount_roots,
};
pub use identity::{FileIdentity, IDENTITY_SUPPORTED, file_identity, last_change};
#[cfg(target_os = "linux")]
pub use variables::{VariablesError, template_variables};
