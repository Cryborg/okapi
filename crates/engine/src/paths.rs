use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Whether a failed lookup only means the path is not there.
pub(crate) fn is_absent(error: &io::Error) -> bool {
    is_absent_kind(error.kind())
}

/// [`is_absent`], for an error already reduced to its kind.
pub(crate) const fn is_absent_kind(kind: io::ErrorKind) -> bool {
    matches!(kind, io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

/// The real location of `path`, resolving its longest existing prefix when the
/// rest does not exist (yet): an exclusion or a variable written through an
/// alias must still match the real directory it names.
///
/// # Errors
///
/// Any error but absence, and a dangling link in the path: what lies below it
/// cannot be located, so a caller that protects something must not guess.
pub(crate) fn canonicalize_longest_prefix(path: &Path) -> io::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match fs::canonicalize(current) {
            Ok(real) => {
                return Ok(missing
                    .iter()
                    .rev()
                    .fold(real, |real, segment| real.join(segment)));
            }
            Err(error) if is_absent(&error) => {
                if fs::symlink_metadata(current).is_ok() {
                    // It is there but does not resolve: a dangling link.
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(segment)) => {
                missing.push(segment.to_os_string());
                current = parent;
            }
            // Nothing of it exists: keep it as written.
            _ => return Ok(path.to_path_buf()),
        }
    }
}

/// Whether `name` is one a running session or service relies on, whatever
/// its age. A lock file anywhere (`lock`, `*.lck`); and, directly below a scan
/// root (`top_level`), where `tmpfiles.d` puts them and never cleans them: the
/// X server's `.X0-lock`, the X11 socket directories, and the private
/// temporary directories of systemd, snap and ssh. Deleting them breaks what
/// is running, even as root. A lock protects itself only, not its siblings.
pub(crate) fn is_session_name(name: &OsStr, is_directory: bool, top_level: bool) -> bool {
    const EXACT: [&str; 5] = [
        ".X11-unix",
        ".ICE-unix",
        ".XIM-unix",
        ".font-unix",
        "snap-private-tmp",
    ];
    const PREFIXES: [&str; 2] = ["systemd-private-", "ssh-"];
    const LOCK_SUFFIX: &[u8] = b".lck";
    let bytes = name.as_encoded_bytes();
    let lock_file = !is_directory
        && (bytes.eq_ignore_ascii_case(b"lock")
            || bytes
                .len()
                .checked_sub(LOCK_SUFFIX.len())
                .is_some_and(|start| bytes[start..].eq_ignore_ascii_case(LOCK_SUFFIX)));
    let Some(name) = name.to_str() else {
        return lock_file;
    };
    let session_entry = top_level
        && ((name.starts_with(".X") && name.ends_with("-lock"))
            || EXACT.contains(&name)
            || PREFIXES.iter().any(|prefix| name.starts_with(prefix)));
    lock_file || session_entry
}

/// `text` in lower case, one character at a time: `str::to_lowercase` writes a
/// final sigma differently at the end of a word, so a name and its suffix
/// would no longer compare.
pub(crate) fn lowercase(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// Whether `path` is `prefix` or below it, comparing whole components and
/// ignoring case. Used where matching too much is the safe side (exclusions):
/// the text conversion can only make two different names compare equal.
pub(crate) fn starts_with_ignoring_case(path: &Path, prefix: &Path) -> bool {
    let mut components = path.components();
    prefix.components().all(|expected| {
        components.next().is_some_and(|component| {
            lowercase(&component.as_os_str().to_string_lossy())
                == lowercase(&expected.as_os_str().to_string_lossy())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_compare_whole_components_ignoring_case() {
        assert!(starts_with_ignoring_case(
            Path::new("/Home/A/b"),
            Path::new("/home/a")
        ));
        assert!(starts_with_ignoring_case(
            Path::new("/home/a"),
            Path::new("/home/a")
        ));
        assert!(!starts_with_ignoring_case(
            Path::new("/home/ab"),
            Path::new("/home/a")
        ));
        assert!(!starts_with_ignoring_case(
            Path::new("/home"),
            Path::new("/home/a")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn session_names_are_recognised_where_they_live() {
        use std::os::unix::ffi::OsStrExt;

        let session = |name: &str, is_directory, top_level| {
            is_session_name(OsStr::new(name), is_directory, top_level)
        };
        assert!(is_session_name(
            std::ffi::OsStr::from_bytes(b"db\xff.lck"),
            false,
            false
        ));
        for name in ["lock", "LOCK", "kotlin-daemon.log.lck", "db.LCK", ".lck"] {
            assert!(session(name, false, false), "{name}");
            assert!(!session(name, true, true), "a directory named {name}");
        }
        for (name, is_directory) in [
            (".X0-lock", false),
            (".X11-unix", true),
            (".ICE-unix", true),
            (".XIM-unix", true),
            (".font-unix", true),
            ("snap-private-tmp", true),
            ("systemd-private-0a1b-colord.service-xyz", true),
            ("ssh-XXXXabcd", true),
        ] {
            assert!(session(name, is_directory, true), "{name}");
            assert!(!session(name, is_directory, false), "{name} deeper down");
        }
        for name in [
            "Cargo.lock",
            "locks",
            "block",
            ".X0",
            "X0-lock",
            "lck",
            "systemd",
        ] {
            assert!(!session(name, false, true), "{name}");
        }
    }

    #[test]
    fn canonicalizes_what_exists_and_keeps_the_rest() {
        let directory = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(directory.path()).unwrap();
        assert_eq!(canonicalize_longest_prefix(directory.path()).unwrap(), real);
        assert_eq!(
            canonicalize_longest_prefix(&directory.path().join("missing/deeper")).unwrap(),
            real.join("missing").join("deeper")
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_an_alias_in_the_existing_part() {
        let directory = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(directory.path()).unwrap();
        fs::create_dir(real.join("target")).unwrap();
        std::os::unix::fs::symlink(real.join("target"), real.join("alias")).unwrap();
        assert_eq!(
            canonicalize_longest_prefix(&real.join("alias/missing")).unwrap(),
            real.join("target").join("missing")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(directory.path()).unwrap();
        std::os::unix::fs::symlink(real.join("nowhere"), real.join("dangling")).unwrap();
        assert!(canonicalize_longest_prefix(&real.join("dangling/below")).is_err());
    }
}
