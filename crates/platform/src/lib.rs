//! Operating system abstractions, one trait per domain and one module per OS.

mod browser;
mod directory;
mod identity;
mod probe;
mod session;
#[cfg(target_os = "linux")]
mod variables;

pub use browser::{CHROMIUM_LOCK, lock_is_live};
#[cfg(target_os = "linux")]
pub use browser::{Holder, RunningBrowser, UnknownHolder, holder};
pub use directory::{
    DESCRIPTOR_REMOVAL_SUPPORTED, Directory, EntryStatus, EntryType, Ownership, mount_root,
    sticky_allows, tells_mount_roots,
};
pub use identity::{FileIdentity, IDENTITY_SUPPORTED, file_identity, last_change, ownership};
pub use probe::{
    CpuTimes, DesktopSession, Device, MemoryStatus, Mount, MountClass, MountList, ProcessList,
    ProcessMemory, Space, application_name, cpu_times, desktop_id, desktop_session, device_labels,
    distribution, kernel_release, load_average, memory, mounts, processes, removable, space,
    uptime,
};
pub use session::sync_directory;
pub use session::{
    create_private_directory, effective_user, open_for_reading, open_journal, running_as_root,
};
#[cfg(target_os = "linux")]
pub use variables::{VariablesError, config_directory, state_directory, template_variables};
