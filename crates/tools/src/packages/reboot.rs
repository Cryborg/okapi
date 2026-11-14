//! Whether a restart is awaited: package maintainer scripts
//! (`notify-reboot-required`, of update-notifier-common) leave
//! `reboot-required` in `/run`, and the packages that asked for it in
//! `reboot-required.pkgs`.

use std::fs;
use std::io;
use std::path::Path;

/// Where the files are on this system.
pub const DIRECTORY: &str = "/run";
const FLAG: &str = "reboot-required";
const PACKAGES: &str = "reboot-required.pkgs";

/// A restart awaited, and the packages that asked for it, if known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebootRequired {
    /// Each once, in the order written; none when the list could not be
    /// read (the restart is awaited all the same).
    pub packages: Option<Vec<String>>,
}

/// The restart awaited in `directory`, if one is.
///
/// # Errors
/// The flag that could not be checked.
pub fn reboot_required(directory: &Path) -> io::Result<Option<RebootRequired>> {
    match fs::symlink_metadata(directory.join(FLAG)) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let packages = match fs::read_to_string(directory.join(PACKAGES)) {
        Ok(text) => Some(listed(&text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(Vec::new()),
        Err(error) => {
            tracing::warn!(%error, "the packages awaiting a restart are unreadable");
            None
        }
    };
    Ok(Some(RebootRequired { packages }))
}

fn listed(text: &str) -> Vec<String> {
    let mut packages: Vec<String> = Vec::new();
    for name in text.lines().map(str::trim).filter(|name| !name.is_empty()) {
        if !packages.iter().any(|known| known == name) {
            packages.push(name.to_owned());
        }
    }
    packages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restart_is_awaited_only_when_flagged_with_its_packages_once() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(reboot_required(directory.path()).unwrap(), None);

        fs::write(
            directory.path().join(FLAG),
            "*** System restart required ***\n",
        )
        .unwrap();
        assert_eq!(
            reboot_required(directory.path()).unwrap(),
            Some(RebootRequired {
                packages: Some(Vec::new())
            }),
            "flagged, no list"
        );

        fs::write(
            directory.path().join(PACKAGES),
            "linux-image-7.0.0-43-generic\n\nlibc6\nlinux-image-7.0.0-43-generic\n  \n",
        )
        .unwrap();
        assert_eq!(
            reboot_required(directory.path()).unwrap(),
            Some(RebootRequired {
                packages: Some(vec!["linux-image-7.0.0-43-generic".into(), "libc6".into()])
            })
        );

        // A list that cannot be read still says a restart is awaited.
        fs::remove_file(directory.path().join(PACKAGES)).unwrap();
        fs::create_dir(directory.path().join(PACKAGES)).unwrap();
        let awaited = reboot_required(directory.path()).unwrap().unwrap();
        assert_eq!(awaited.packages, None);
    }
}
