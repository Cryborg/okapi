//! Operating system abstractions, one trait per domain and one module per OS.

mod browser;
mod directory;
#[cfg(target_os = "linux")]
mod elevation;
mod filesystems;
mod identity;
mod probe;
mod session;
mod trash;
#[cfg(target_os = "linux")]
mod user_dirs;
#[cfg(target_os = "linux")]
mod variables;

pub use browser::{CHROMIUM_LOCK, FIREFOX_LOCK, LockKind, lock_is_live};
#[cfg(target_os = "linux")]
pub use browser::{Holder, RunningBrowser, UnknownHolder, holder};
#[cfg(target_os = "linux")]
pub use directory::open_path;
pub use directory::{
    DESCRIPTOR_REMOVAL_SUPPORTED, Directory, EntryStatus, EntryType, Ownership, ROOT_USER, Reached,
    Unreached, enter, gone_or_failed, mount_root, out_of_descriptors, reach, reach_path,
    reach_with, sticky_allows, tells_mount_roots,
};
#[cfg(target_os = "linux")]
pub use elevation::{HELPER_PATH, PKEXEC_PATH};
pub use filesystems::{
    filesystem_is_network, filesystem_may_alias, filesystem_of, filesystem_undated, mount_of,
};
pub use identity::{
    FileIdentity, IDENTITY_SUPPORTED, file_identity, last_change, link_count, ownership,
};
pub use probe::{
    CpuTimes, DesktopEntry, DesktopSession, Device, MemoryStatus, Mount, MountClass, MountList,
    ProcessList, ProcessMemory, SCOPE_SUFFIX, Space, application_name, confinement, cpu_times,
    desktop_entry, desktop_entry_lossy, desktop_id, desktop_session, device_labels, distribution,
    kernel_release, lineage_cgroups, load_average, memory, mounts, names_a_file, processes,
    read_regular_capped, removable, rotational, scope_snap, space, uptime,
};
#[cfg(target_os = "linux")]
pub use session::may_write;
pub use session::sync_directory;
pub use session::{
    create_private_directory, effective_user, open_existing_journal, open_for_reading,
    open_journal, real_user, running_as_root, try_lock_journal_exclusive, try_lock_journal_shared,
};
pub use trash::{
    Deletion, InformationRefusal, InformationRemoval, Trash, TrashError, home_trash,
    information_folder, read_deletion, remove_information,
};
#[cfg(target_os = "linux")]
pub use user_dirs::guarded_personal_folders;
#[cfg(target_os = "linux")]
pub use user_dirs::{PersonalFolder, PersonalKind, personal_folders};
#[cfg(target_os = "linux")]
pub use variables::{VariablesError, config_directory, state_directory, template_variables};

/// The personal folders to guard: unknown outside Linux yet, so none can be
/// told apart (a guard refuses then).
///
/// # Errors
///
/// Always [`std::io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn guarded_personal_folders(
    _config: &std::path::Path,
    _home: &std::path::Path,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    Err(std::io::ErrorKind::Unsupported.into())
}
