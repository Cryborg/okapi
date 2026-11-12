//! Operating system abstractions, one trait per domain and one module per OS.

mod identity;
#[cfg(target_os = "linux")]
mod variables;

pub use identity::{FileIdentity, IDENTITY_SUPPORTED, file_identity, last_change};
#[cfg(target_os = "linux")]
pub use variables::{VariablesError, template_variables};
