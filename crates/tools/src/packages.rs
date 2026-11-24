//! The package managers' daemons, asked and never run (invariant 4):
//! PackageKit on the system bus ([`packagekit`]), flatpak's system helper
//! there too ([`flatpak`]), snapd on its socket ([`snapd`]), the session's
//! systemd on the session bus ([`systemd_session`]); when apt last
//! downloaded its lists ([`apt`]), whether dpkg is writing ([`dpkg`]) and
//! whether a restart is awaited ([`reboot`]), from the files they leave.
//! Linux only: elsewhere the module is empty, and a tool built on it has no
//! source.

#[cfg(target_os = "linux")]
pub mod apt;
#[cfg(target_os = "linux")]
pub mod bus;
#[cfg(target_os = "linux")]
pub mod dpkg;
#[cfg(target_os = "linux")]
pub mod flatpak;
#[cfg(target_os = "linux")]
pub mod packagekit;
#[cfg(target_os = "linux")]
pub mod reboot;
#[cfg(target_os = "linux")]
pub mod snapd;
#[cfg(target_os = "linux")]
pub mod systemd_session;

#[cfg(test)]
#[cfg(target_os = "linux")]
pub(crate) mod fake_bus;
