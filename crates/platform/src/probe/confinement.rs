use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// What a container or a sandbox leaves in the system it shows: podman and
/// toolbox, docker, a flatpak's sandbox, systemd-nspawn (and what systemd
/// writes in any container it runs in).
const MARK_FILES: [&str; 4] = [
    "/run/.containerenv",
    "/.dockerenv",
    "/.flatpak-info",
    "/run/systemd/container",
];
/// What a container (systemd's convention, followed by podman, LXC,
/// toolbox) or schroot sets.
const MARK_VARIABLES: [&str; 2] = ["container", "SCHROOT_CHROOT_NAME"];
/// A snap's own folder, set in its confinement. A classic snap's terminal
/// passes it on to what is launched from it: it counts only when Okapi's
/// own program is in it.
const SNAP_VARIABLE: &str = "SNAP";

/// Why Okapi may not see the system that filled the home folder: it runs in
/// a container, a sandbox or a chroot that says so, the mark found named;
/// `None` when nothing says so. A mark that cannot be looked at counts.
/// A bare chroot says nothing a user can read: it is not told.
#[must_use]
pub fn confinement() -> Option<String> {
    let program = std::env::current_exe().ok();
    confinement_from(
        &|name| std::env::var_os(name),
        &|path| match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        },
        program.as_deref(),
    )
}

/// [`confinement`], the environment read through `variable`, the files
/// through `present`, Okapi's program being `program` (`None`: unknown).
pub(crate) fn confinement_from(
    variable: &dyn Fn(&str) -> Option<OsString>,
    present: &dyn Fn(&Path) -> io::Result<bool>,
    program: Option<&Path>,
) -> Option<String> {
    for file in MARK_FILES {
        match present(Path::new(file)) {
            Ok(false) => {}
            Ok(true) => return Some(file.to_owned()),
            Err(error) => return Some(format!("{file}: {error}")),
        }
    }
    let set = |name: &str| variable(name).filter(|value| !value.is_empty());
    if let Some(name) = MARK_VARIABLES.into_iter().find(|name| set(name).is_some()) {
        return Some(format!("variable {name}"));
    }
    let snap = PathBuf::from(set(SNAP_VARIABLE)?);
    match program {
        Some(program) if !program.starts_with(&snap) => None,
        _ => Some(format!("variable {SNAP_VARIABLE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marks of `files`, the variables `variables`, the program at
    /// `program`.
    fn confined(
        files: &[&str],
        variables: &[(&str, &str)],
        program: Option<&str>,
    ) -> Option<String> {
        confinement_from(
            &|name| {
                variables
                    .iter()
                    .find(|(wanted, _)| *wanted == name)
                    .map(|(_, value)| OsString::from(value))
            },
            &|path| Ok(files.iter().any(|file| Path::new(file) == path)),
            program.map(Path::new),
        )
    }

    #[test]
    fn each_mark_alone_says_okapi_runs_confined() {
        // Catches a container, a sandbox or a chroot missed: what is
        // installed would be read from a system that did not fill the home
        // folder, and its applications' data proposed as leftovers.
        let program = Some("/usr/bin/okapi-app");
        assert_eq!(confined(&[], &[], program), None);
        for file in MARK_FILES {
            assert_eq!(confined(&[file], &[], program).as_deref(), Some(file));
        }
        for name in MARK_VARIABLES {
            assert!(
                confined(&[], &[(name, "podman")], program).is_some(),
                "{name}"
            );
            assert_eq!(confined(&[], &[(name, "")], program), None, "{name} empty");
        }
    }

    #[test]
    fn a_snap_s_folder_counts_only_when_okapi_runs_from_it() {
        // Catches Okapi launched from a classic snap's terminal (it passes
        // `SNAP` on) taken for Okapi in a snap: it sees the system.
        let snap = [("SNAP", "/snap/okapi/12")];
        assert!(confined(&[], &snap, Some("/snap/okapi/12/bin/okapi-app")).is_some());
        assert_eq!(confined(&[], &snap, Some("/usr/bin/okapi-app")), None);
        assert_eq!(
            confined(&[], &snap, Some("/snap/okapi/123/okapi-app")),
            None
        );
        // Its program unknown: confined, the safe side.
        assert!(confined(&[], &snap, None).is_some());
    }

    #[test]
    fn a_mark_that_cannot_be_looked_at_counts() {
        let blocked = confinement_from(
            &|_| None,
            &|_| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            None,
        );
        assert!(blocked.is_some_and(|why| why.starts_with("/run/.containerenv")));
    }
}
