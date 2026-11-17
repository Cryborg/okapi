//! Maintenance tools beyond the cleaner, one module per tool.

#[cfg(feature = "apps")]
pub mod apps;
#[cfg(feature = "disk-usage")]
pub mod disk_usage;
// Its socket is judged by Linux's access check (`okapi-platform`).
#[cfg(all(target_os = "linux", feature = "docker"))]
pub mod docker;
#[cfg(feature = "duplicates")]
pub mod duplicates;
#[cfg(feature = "monitor")]
pub mod monitor;
#[cfg(any(feature = "disk-usage", feature = "duplicates"))]
mod mounts;
#[cfg(feature = "packages")]
pub mod packages;
#[cfg(any(feature = "duplicates", feature = "apps"))]
mod trashing;
// Its daemons (snapd, Docker) are asked on Linux.
#[cfg(all(target_os = "linux", any(feature = "packages", feature = "docker")))]
mod unix_http;
// Its sources are Linux's daemons.
#[cfg(all(feature = "updates", target_os = "linux"))]
pub mod updates;

/// What a panic caught says, when it says it in words.
#[cfg(any(feature = "disk-usage", feature = "duplicates"))]
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

/// The locale PackageKit gives its texts in, for `language`.
#[cfg(all(target_os = "linux", any(feature = "apps", feature = "updates")))]
pub(crate) const fn packagekit_locale(language: okapi_rules::Language) -> &'static str {
    match language {
        okapi_rules::Language::French => "fr_FR.UTF-8",
        okapi_rules::Language::English => "en_US.UTF-8",
    }
}
