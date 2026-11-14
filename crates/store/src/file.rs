//! The small files Okapi keeps: read whole within a limit, written whole by
//! replacing them, never through a link.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Instant;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::StoreError;

/// Far beyond any file Okapi keeps here: a larger file is a mistake (a link
/// to a device, a log), never read whole.
pub(crate) const MAX_FILE_SIZE: u64 = 1024 * 1024;

/// The regular file at `path`, open for reading (a link followed); `None`
/// when there is none.
///
/// # Errors
///
/// [`StoreError::BrokenLink`] when a link on the way leads nowhere (never
/// taken for none), [`StoreError::Unreadable`], or [`StoreError::NotAFile`]
/// for a directory, a pipe or a device.
pub(crate) fn open_regular(path: &Path) -> Result<Option<File>, StoreError> {
    let file = match okapi_platform::open_for_reading(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(link) = dangling(path) {
                // The error names the file only: which link is broken is said here.
                tracing::warn!(path = %path.display(), link = %link.display(), "a link on the way leads nowhere");
                return Err(StoreError::BrokenLink {
                    path: path.to_owned(),
                });
            }
            return Ok(None);
        }
        Err(error) => {
            return Err(StoreError::Unreadable {
                path: path.to_owned(),
                error,
            });
        }
    };
    let metadata = file.metadata().map_err(|error| StoreError::Unreadable {
        path: path.to_owned(),
        error,
    })?;
    if !metadata.is_file() {
        return Err(StoreError::NotAFile {
            path: path.to_owned(),
        });
    }
    Ok(Some(file))
}

/// The content of the file at `path`, if it is a regular file of at most
/// [`MAX_FILE_SIZE`] bytes: a pipe would block forever, a device or a huge
/// file (or one that claims no size, under `/proc`) exhaust the memory.
/// `None` when there is none.
///
/// # Errors
///
/// Those of [`open_regular`], and [`StoreError::TooLarge`].
pub(crate) fn read_small(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    let Some(file) = open_regular(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| StoreError::Unreadable {
            path: path.to_owned(),
            error,
        })?;
    if u64::try_from(bytes.len()).map_or(true, |size| size > MAX_FILE_SIZE) {
        return Err(StoreError::TooLarge {
            path: path.to_owned(),
            limit: MAX_FILE_SIZE,
        });
    }
    Ok(Some(bytes))
}

#[derive(Deserialize)]
struct Versioned {
    version: u32,
}

/// The JSON file at `path`, of version `supported`, read as `T` (as strictly
/// as `T` says); `None` when there is none. Without a version, the error is
/// the one reading `T` meets first: a mistyped field is named before the
/// missing version.
///
/// # Errors
///
/// Those of [`read_small`], [`StoreError::UnsupportedVersion`], and
/// [`StoreError::Invalid`] (not JSON, no version, or not a `T`).
pub(crate) fn read_versioned<T: DeserializeOwned>(
    path: &Path,
    supported: u32,
) -> Result<Option<T>, StoreError> {
    let Some(bytes) = read_small(path)? else {
        return Ok(None);
    };
    if let Err(error) = check_version(bytes.as_slice(), path, supported) {
        if let StoreError::Invalid { .. } = error
            && let Err(strict) = serde_json::from_slice::<T>(&bytes)
        {
            return Err(StoreError::Invalid {
                path: path.to_owned(),
                error: strict,
            });
        }
        return Err(error);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| StoreError::Invalid {
            path: path.to_owned(),
            error,
        })
}

/// What a file holds, after its version.
#[derive(Serialize)]
struct Enveloped<'a, T> {
    version: u32,
    #[serde(flatten)]
    value: &'a T,
}

/// Replaces the file `name` of `directory` by `value` with its `version`
/// first, in JSON a person can read, as [`write_atomically`] does. `value`
/// must serialize as a map (a struct) with no `version` of its own.
///
/// # Errors
///
/// Those of [`write_atomically`].
pub(crate) fn write_versioned<T: Serialize>(
    directory: &Path,
    name: &str,
    version: u32,
    value: &T,
) -> Result<(), StoreError> {
    let mut bytes = serde_json::to_vec_pretty(&Enveloped { version, value }).map_err(|error| {
        let path = directory.join(name);
        tracing::error!(path = %path.display(), %error, "file not written: not serializable");
        StoreError::Unwritable {
            path,
            error: io::Error::other(error),
        }
    })?;
    bytes.push(b'\n');
    write_atomically(directory, name, &bytes)
}

/// Whether the file whose reading is `read` may be replaced: one that cannot
/// be read never is (D-10: replacing it would lose what the user had set,
/// theirs to repair), which is traced. Judged just before the write: one
/// damaged in that very instant may still be replaced.
///
/// # Errors
///
/// That of `read`.
pub(crate) fn replaceable<T>(read: Result<T, StoreError>) -> Result<(), StoreError> {
    read.map(drop).inspect_err(|error| {
        tracing::warn!(%error, "file unreadable: left as it is, not replaced");
    })
}

/// Checks the version of the JSON `reader` gives, read from `path`, before
/// its content: a later Okapi may add fields, which must be told as a later
/// version, not as a mistake.
///
/// # Errors
///
/// [`StoreError::Invalid`] (no version, or not JSON),
/// [`StoreError::UnsupportedVersion`], or [`StoreError::Unreadable`] when
/// `reader` fails.
pub(crate) fn check_version(
    reader: impl io::Read,
    path: &Path,
    supported: u32,
) -> Result<(), StoreError> {
    let Versioned { version } =
        serde_json::from_reader(reader).map_err(|error| json_error(path, error))?;
    if version == supported {
        Ok(())
    } else {
        Err(StoreError::UnsupportedVersion {
            path: path.to_owned(),
            version,
            supported,
        })
    }
}

/// What a failure to read JSON from `path` is: the reading's, or the
/// content's.
pub(crate) fn json_error(path: &Path, error: serde_json::Error) -> StoreError {
    let path = path.to_owned();
    if error.is_io() {
        StoreError::Unreadable {
            path,
            error: error.into(),
        }
    } else {
        StoreError::Invalid { path, error }
    }
}

/// Replaces the file `name` of `directory` by `bytes`, whole or not at all:
/// they are written to a private temporary file beside it, synced, then
/// renamed over it. `directory` is created private if needed, and made so
/// (through a link, its target: the directory is Okapi's even when kept with
/// the user's dotfiles). The file itself is never written through a link: a
/// file there that is a link, a directory, a pipe or a device is refused,
/// never followed nor replaced, as a link may lead to a file that is not
/// Okapi's. A link there that leads nowhere is [`StoreError::BrokenLink`].
/// When `directory` is created now, its own name is not synced: a power cut
/// may still lose the first file written.
///
/// # Errors
///
/// [`StoreError::BrokenLink`] when the file, or a link on the way to it,
/// leads nowhere, [`StoreError::NotAFile`], or [`StoreError::Unwritable`] (traced
/// with the step that failed). A failure to make the rename itself durable
/// is no error: the file is replaced, which a power cut may still undo, and
/// that is traced.
pub(crate) fn write_atomically(
    directory: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let path = directory.join(name);
    let started = Instant::now();
    let unwritable = |step: &'static str, error: io::Error| {
        tracing::error!(
            path = %path.display(),
            step,
            %error,
            code = error.raw_os_error(),
            "file not written"
        );
        StoreError::Unwritable {
            path: path.clone(),
            error,
        }
    };
    if let Some(link) = dangling(&path) {
        tracing::error!(
            path = %path.display(),
            link = %link.display(),
            "file not written: a link on the way leads nowhere"
        );
        return Err(StoreError::BrokenLink { path: path.clone() });
    }
    okapi_platform::create_private_directory(directory)
        .map_err(|error| unwritable("directory", error))?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => {
            tracing::error!(path = %path.display(), file_type = ?metadata.file_type(), "file not written: not a regular file");
            return Err(StoreError::NotAFile { path: path.clone() });
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(unwritable("status", error)),
    }
    // Recognisable if a crash leaves it behind (nothing removes it then).
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .tempfile_in(directory)
        .map_err(|error| unwritable("temporary", error))?;
    temporary
        .write_all(bytes)
        .map_err(|error| unwritable("write", error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| unwritable("sync", error))?;
    temporary
        .persist(&path)
        .map_err(|error| unwritable("rename", error.error))?;
    if let Err(error) = okapi_platform::sync_directory(directory) {
        tracing::warn!(
            path = %path.display(),
            %error,
            code = error.raw_os_error(),
            "file replaced, but the rename may not survive a power cut"
        );
    }
    tracing::info!(
        path = %path.display(),
        bytes = bytes.len(),
        duration_ms = started.elapsed().as_millis(),
        "file written"
    );
    Ok(())
}

/// The first of `path` and its ancestors that is a link whose target is
/// absent (a link refused or looping is not: its error is the system's).
fn dangling(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| {
        fs::symlink_metadata(ancestor).is_ok()
            && fs::metadata(ancestor).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reading that fails, as a failing disk does (EIO).
    struct Failing;

    impl io::Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(5))
        }
    }

    #[test]
    fn a_reading_that_fails_is_unreadable_never_invalid() {
        let Err(StoreError::Unreadable { error, .. }) =
            check_version(Failing, Path::new("/okapi/x.json"), 1)
        else {
            panic!("unreadable");
        };
        assert_eq!(error.raw_os_error(), Some(5), "the system's code kept");
    }

    const NAME: &str = "kept.json";

    /// The names in `directory`.
    fn names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_file_is_written_then_replaced_whole() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("okapi");
        write_atomically(&directory, NAME, b"a longer first content").unwrap();
        assert_eq!(
            read_small(&directory.join(NAME)).unwrap().unwrap(),
            b"a longer first content"
        );
        write_atomically(&directory, NAME, b"short").unwrap();
        assert_eq!(
            read_small(&directory.join(NAME)).unwrap().unwrap(),
            b"short",
            "nothing of the longer content is left"
        );
        assert_eq!(names(&directory), [NAME], "no temporary file is left");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_and_its_directory_are_private_even_if_they_were_not() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("okapi");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(directory.join(NAME), b"open").unwrap();
        fs::set_permissions(directory.join(NAME), fs::Permissions::from_mode(0o644)).unwrap();
        write_atomically(&directory, NAME, b"private").unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&directory.join(NAME)), 0o600);
        assert_eq!(mode(&directory), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_never_written_through_nor_replaced() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dotfiles = root.path().join("dotfiles");
        fs::create_dir(&dotfiles).unwrap();
        fs::set_permissions(&dotfiles, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dotfiles.join(NAME), b"theirs").unwrap();
        let directory = root.path().join("okapi");
        fs::create_dir(&directory).unwrap();
        std::os::unix::fs::symlink(dotfiles.join(NAME), directory.join(NAME)).unwrap();
        assert!(matches!(
            write_atomically(&directory, NAME, b"ours"),
            Err(StoreError::NotAFile { .. })
        ));
        assert_eq!(fs::read(dotfiles.join(NAME)).unwrap(), b"theirs");
        assert!(
            fs::symlink_metadata(directory.join(NAME))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            fs::metadata(&dotfiles).unwrap().permissions().mode() & 0o777,
            0o755,
            "the directory of the link's target is never touched"
        );
        assert_eq!(names(&directory), [NAME]);
        assert_eq!(names(&dotfiles), [NAME]);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_leads_nowhere_is_a_broken_link_and_nothing_is_created() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("okapi");
        std::os::unix::fs::symlink(root.path().join("unmounted"), &directory).unwrap();
        assert!(matches!(
            write_atomically(&directory, NAME, b"x"),
            Err(StoreError::BrokenLink { .. })
        ));
        assert!(!root.path().join("unmounted").exists());
        let file_link = root.path().join("file-link");
        fs::create_dir(&file_link).unwrap();
        std::os::unix::fs::symlink(root.path().join("gone"), file_link.join(NAME)).unwrap();
        assert!(matches!(
            write_atomically(&file_link, NAME, b"x"),
            Err(StoreError::BrokenLink { .. })
        ));
        assert!(!root.path().join("gone").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_loops_is_never_taken_for_one_that_leads_nowhere() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(NAME);
        std::os::unix::fs::symlink(&path, &path).unwrap();
        assert!(matches!(
            read_small(&path),
            Err(StoreError::Unreadable { .. })
        ));
        assert!(matches!(
            write_atomically(root.path(), NAME, b"x"),
            Err(StoreError::NotAFile { .. })
        ));
    }

    #[test]
    fn a_directory_in_the_way_is_refused_and_nothing_is_left() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(NAME)).unwrap();
        assert!(matches!(
            write_atomically(root.path(), NAME, b"x"),
            Err(StoreError::NotAFile { .. })
        ));
        assert_eq!(names(root.path()), [NAME]);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_is_a_link_is_okapi_s_and_written_in_its_target() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("dotfiles/okapi");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let directory = root.path().join("okapi");
        std::os::unix::fs::symlink(&target, &directory).unwrap();
        write_atomically(&directory, NAME, b"kept").unwrap();
        assert_eq!(fs::read(target.join(NAME)).unwrap(), b"kept");
        assert!(fs::symlink_metadata(&directory).unwrap().is_symlink());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn a_directory_that_cannot_be_made_is_unwritable_by_the_file_s_path() {
        let root = tempfile::tempdir().unwrap();
        let not_a_directory = root.path().join("okapi");
        fs::write(&not_a_directory, b"").unwrap();
        match write_atomically(&not_a_directory, NAME, b"x") {
            Err(StoreError::Unwritable { path, .. }) => {
                assert_eq!(path, not_a_directory.join(NAME));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_versioned_file_is_readable_json_with_its_version_first() {
        #[derive(Serialize)]
        struct Content {
            widgets: u8,
            alpha: u8,
        }

        let root = tempfile::tempdir().unwrap();
        write_versioned(
            root.path(),
            NAME,
            7,
            &Content {
                widgets: 2,
                alpha: 1,
            },
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join(NAME)).unwrap(),
            "{\n  \"version\": 7,\n  \"widgets\": 2,\n  \"alpha\": 1\n}\n",
            "in the order of the fields, never sorted"
        );
    }

    #[test]
    fn the_version_is_judged_before_the_content() {
        let path = Path::new("/okapi/kept.json");
        check_version(br#"{"version": 3, "anything": [1]}"#.as_slice(), path, 3).unwrap();
        assert!(matches!(
            check_version(br#"{"version": 2}"#.as_slice(), path, 3),
            Err(StoreError::UnsupportedVersion {
                version: 2,
                supported: 3,
                ..
            })
        ));
        assert!(matches!(
            check_version(
                br#"{"version": 4, "fieldOfLater": true}"#.as_slice(),
                path,
                3
            ),
            Err(StoreError::UnsupportedVersion {
                version: 4,
                supported: 3,
                ..
            })
        ));
        assert!(matches!(
            check_version(br#"{"widgets": []}"#.as_slice(), path, 3),
            Err(StoreError::Invalid { .. })
        ));
        assert!(matches!(
            check_version(b"not json".as_slice(), path, 3),
            Err(StoreError::Invalid { .. })
        ));
    }
}
