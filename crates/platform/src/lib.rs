//! Operating system abstractions, one trait per domain and one module per OS.

mod identity;

pub use identity::{FileIdentity, IDENTITY_SUPPORTED, file_identity, last_change};
