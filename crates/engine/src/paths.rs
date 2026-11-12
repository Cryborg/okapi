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

/// Whether `path` is `prefix` or below it, comparing whole components and
/// ignoring case. Used where matching too much is the safe side (exclusions):
/// the text conversion can only make two different names compare equal.
pub(crate) fn starts_with_ignoring_case(path: &Path, prefix: &Path) -> bool {
    let mut components = path.components();
    prefix.components().all(|expected| {
        components.next().is_some_and(|component| {
            component.as_os_str().to_string_lossy().to_lowercase()
                == expected.as_os_str().to_string_lossy().to_lowercase()
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
