//! Maintenance tools beyond the cleaner, one module per tool.

#[cfg(feature = "disk-usage")]
pub mod disk_usage;
#[cfg(feature = "duplicates")]
pub mod duplicates;
#[cfg(feature = "monitor")]
pub mod monitor;
#[cfg(any(feature = "disk-usage", feature = "duplicates"))]
mod mounts;

/// What a panic caught says, when it says it in words.
#[cfg(any(feature = "disk-usage", feature = "duplicates"))]
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}
