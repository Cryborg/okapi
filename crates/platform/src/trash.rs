//! The user's trash, as freedesktop specifies it (Trash 1.0): a file goes to
//! `$XDG_DATA_HOME/Trash/files`, and what restores it to
//! `Trash/info/<name>.trashinfo`. A file is moved there from its own folder,
//! by descriptors, never copied: one of another filesystem stays where it
//! is. Nothing is written through a link.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::Path;

use crate::{Directory, EntryStatus};

const TRASH: &str = "Trash";
const FILES: &str = "files";
const INFO: &str = "info";
const INFO_EXTENSION: &str = ".trashinfo";
/// How many names are tried for a file before giving up: `name`, then
/// `name.2`, `name.3`...
const MOST_NAMES: u32 = 1000;
/// What a path keeps as it is in a `.trashinfo`, beside letters and digits:
/// the unreserved characters of an address, and its separator.
const UNESCAPED: &[u8] = b"-._~/";

/// Why a file did not go to the trash.
#[derive(Debug, thiserror::Error)]
pub enum TrashError {
    /// It is on another filesystem than the trash: moving it would copy it.
    #[error("the file is on another filesystem than the trash")]
    OtherDevice,
    /// What was moved is not the file meant: it was put back.
    #[error("the file changed before it was moved")]
    Changed,
    /// What was moved could not be put back once found not to be the file
    /// as checked (another file, or the file written meanwhile), or not
    /// read again at all: it stays in the trash under this name, restorable.
    #[error("a file not known to be the one meant went to the trash and stays there")]
    Unchecked(OsString),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The user's trash, opened.
#[derive(Debug)]
pub struct Trash {
    files: Directory,
    info: Directory,
    device: u64,
}

impl Trash {
    /// The trash of the user's home, its folders created (for their owner
    /// only) when missing.
    ///
    /// # Errors
    ///
    /// The system's error: no data folder, a link where a folder of the
    /// trash should be, not allowed to create it... Elsewhere than on Linux,
    /// [`io::ErrorKind::Unsupported`].
    pub fn home() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let data = crate::variables::data_home().ok_or(io::ErrorKind::NotFound)?;
            Self::in_folder(&data)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    /// The device of the trash: a file elsewhere cannot be moved into it.
    #[must_use]
    pub const fn device(&self) -> u64 {
        self.device
    }

    /// The trash below the data folder `data`.
    ///
    /// # Errors
    ///
    /// As [`Self::home`].
    pub fn in_folder(data: &Path) -> io::Result<Self> {
        let trash = opened_or_created(&Directory::open(data)?, TRASH)?;
        let files = opened_or_created(&trash, FILES)?;
        let info = opened_or_created(&trash, INFO)?;
        let device = files.status()?.identity.device();
        Ok(Self {
            files,
            info,
            device,
        })
    }

    /// Moves the file `name` of `folder`, at `original`, to the trash, with
    /// what restores it, dated `deleted_at` (local time,
    /// `YYYY-MM-DDThh:mm:ss`); the name it has in the trash. What was moved
    /// is read again in the trash: one `still` refuses (a file put in its
    /// place by name between the caller's check and the move), or that
    /// cannot be read again, is put back where it was.
    ///
    /// # Errors
    ///
    /// [`TrashError::OtherDevice`] when `folder` is on another filesystem
    /// than the trash (nothing is written); [`TrashError::Changed`] when what
    /// was moved is not what `still` accepts (put back);
    /// [`TrashError::Unchecked`] when it could not be put back (its name was
    /// taken); or the system's error. The trash is then left as it was,
    /// but for a `.trashinfo` that could not be removed (traced).
    pub fn put(
        &self,
        folder: &Directory,
        name: &OsStr,
        original: &Path,
        deleted_at: &str,
        still: impl Fn(&EntryStatus) -> bool,
    ) -> Result<OsString, TrashError> {
        if folder.status()?.identity.device() != self.device {
            return Err(TrashError::OtherDevice);
        }
        let (trashed, info) = self.reserve(name, &restoring(original, deleted_at))?;
        if let Err(error) = folder.move_to(name, &self.files, &trashed) {
            self.forget(&info);
            return Err(if error.kind() == io::ErrorKind::CrossesDevices {
                TrashError::OtherDevice
            } else {
                error.into()
            });
        }
        let unread = match self.files.status_of(&trashed) {
            Ok(moved) if still(&moved) => return Ok(trashed),
            Ok(moved) => {
                tracing::warn!(
                    original = %original.display(),
                    found = ?moved.identity,
                    "not the file meant moved to the trash: put back"
                );
                None
            }
            Err(error) => Some(error),
        };
        // Not what was checked, or not read again: back where it was.
        match (self.files.move_to(&trashed, folder, name), unread) {
            (Ok(()), unread) => {
                self.forget(&info);
                Err(unread.map_or(TrashError::Changed, TrashError::Io))
            }
            // Not put back (its name was taken): it stays, restorable, not
            // known to be the file meant, whether read again or not.
            (Err(error), unread) => {
                tracing::warn!(
                    original = %original.display(),
                    trashed = ?trashed,
                    unread = ?unread,
                    %error,
                    "a file not known to be the one meant went to the trash and stays there"
                );
                Err(TrashError::Unchecked(trashed))
            }
        }
    }

    /// Removes the information `info` of a file that did not go.
    fn forget(&self, info: &OsStr) {
        if let Err(error) = self.info.remove_file(info) {
            tracing::warn!(info = ?info, %error, "trash information left without its file");
        }
    }

    /// Reserves a name for `name` in the trash, free in `files` and in
    /// `info`, by creating its `.trashinfo`, which holds `content`: the name,
    /// and the name of its information.
    fn reserve(&self, name: &OsStr, content: &str) -> io::Result<(OsString, OsString)> {
        for attempt in 1..=MOST_NAMES {
            let mut trashed = name.to_os_string();
            if attempt > 1 {
                trashed.push(format!(".{attempt}"));
            }
            // A name taken in `files` without its information (left by
            // another program) is not reused.
            match self.files.status_of(&trashed) {
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let mut info = trashed.clone();
            info.push(INFO_EXTENSION);
            match self.info.create_new_file(&info) {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(content.as_bytes()) {
                        self.forget(&info);
                        return Err(error);
                    }
                    return Ok((trashed, info));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        tracing::warn!(name = ?name, tried = MOST_NAMES, "no free name in the trash");
        Err(io::ErrorKind::AlreadyExists.into())
    }
}

/// The folder `name` of `parent`, created when missing; never a link.
fn opened_or_created(parent: &Directory, name: &str) -> io::Result<Directory> {
    let name = OsStr::new(name);
    match parent.open_child(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match parent.create_directory(name) {
                // Created meanwhile by another program: opened as it is (a
                // link is refused).
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            parent.open_child(name)
        }
        opened => opened,
    }
}

/// What a `.trashinfo` holds: where the file was, its path escaped as in an
/// address, and when it went.
fn restoring(original: &Path, deleted_at: &str) -> String {
    #[cfg(unix)]
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(original.as_os_str());
    #[cfg(not(unix))]
    let lossy = original.to_string_lossy();
    #[cfg(not(unix))]
    let bytes = lossy.as_bytes();
    let mut path = String::new();
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || UNESCAPED.contains(&byte) {
            path.push(char::from(byte));
        } else {
            // Writing to a `String` never fails.
            let _ = write!(path, "%{byte:02X}");
        }
    }
    format!("[Trash Info]\nPath={path}\nDeletionDate={deleted_at}\n")
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::fs::symlink;

    use super::*;

    const DATE: &str = "2026-11-14T22:47:05";

    /// What accepts any file moved.
    fn any(_: &EntryStatus) -> bool {
        true
    }

    fn trash(data: &Path) -> Trash {
        Trash::in_folder(data).unwrap()
    }

    #[test]
    fn a_file_goes_to_the_trash_with_what_restores_it() {
        let temporary = tempfile::tempdir().unwrap();
        let data = temporary.path().join("share");
        fs::create_dir(&data).unwrap();
        let folder = temporary.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("photo.jpg"), b"content").unwrap();
        let bin = trash(&data);
        let name = bin
            .put(
                &Directory::open(&folder).unwrap(),
                OsStr::new("photo.jpg"),
                &folder.join("photo.jpg"),
                DATE,
                any,
            )
            .unwrap();
        assert_eq!(name, "photo.jpg");
        assert!(!folder.join("photo.jpg").exists());
        assert_eq!(
            fs::read(data.join("Trash/files/photo.jpg")).unwrap(),
            b"content"
        );
        let info = fs::read_to_string(data.join("Trash/info/photo.jpg.trashinfo")).unwrap();
        assert_eq!(
            info,
            format!(
                "[Trash Info]\nPath={}/photo.jpg\nDeletionDate={DATE}\n",
                folder.display()
            )
        );
    }

    #[test]
    fn a_path_is_escaped_as_in_an_address() {
        let path = std::path::PathBuf::from(OsString::from_vec(
            b"/home/a b/100%/caf\xc3\xa9/\xff~x-y_z\nDeletionDate=1.txt".to_vec(),
        ));
        // A line break in a name never makes a line of its own.
        assert_eq!(
            restoring(&path, DATE),
            format!(
                "[Trash Info]\nPath=/home/a%20b/100%25/caf%C3%A9/%FF~x-y_z%0ADeletionDate%3D1.txt\nDeletionDate={DATE}\n"
            )
        );
    }

    #[test]
    fn two_files_of_one_name_never_take_each_other_s_place() {
        let temporary = tempfile::tempdir().unwrap();
        let data = temporary.path().to_path_buf();
        let bin = trash(&data);
        for (index, content) in [b"first", b"other"].iter().enumerate() {
            let folder = temporary.path().join(format!("folder {index}"));
            fs::create_dir(&folder).unwrap();
            fs::write(folder.join("same"), content).unwrap();
            bin.put(
                &Directory::open(&folder).unwrap(),
                OsStr::new("same"),
                &folder.join("same"),
                DATE,
                any,
            )
            .unwrap();
        }
        // Left by another program: a file without its information.
        fs::write(data.join("Trash/files/kept"), b"kept").unwrap();
        let folder = temporary.path().join("folder 0");
        fs::write(folder.join("kept"), b"third").unwrap();
        let name = bin
            .put(
                &Directory::open(&folder).unwrap(),
                OsStr::new("kept"),
                &folder.join("kept"),
                DATE,
                any,
            )
            .unwrap();
        assert_eq!(name, "kept.2");
        assert_eq!(fs::read(data.join("Trash/files/same")).unwrap(), b"first");
        assert_eq!(fs::read(data.join("Trash/files/same.2")).unwrap(), b"other");
        assert!(data.join("Trash/info/same.2.trashinfo").exists());
        assert_eq!(fs::read(data.join("Trash/files/kept")).unwrap(), b"kept");
    }

    #[test]
    fn a_trash_folder_that_is_a_link_is_refused_and_nothing_is_written() {
        for link in ["Trash", "Trash/files", "Trash/info"] {
            let temporary = tempfile::tempdir().unwrap();
            let data = temporary.path().join("share");
            let elsewhere = temporary.path().join("elsewhere");
            fs::create_dir_all(data.join("Trash")).unwrap();
            fs::create_dir(&elsewhere).unwrap();
            if link == "Trash" {
                fs::remove_dir(data.join("Trash")).unwrap();
            } else {
                fs::create_dir(data.join("Trash/files")).unwrap();
                fs::create_dir(data.join("Trash/info")).unwrap();
                fs::remove_dir(data.join(link)).unwrap();
            }
            symlink(&elsewhere, data.join(link)).unwrap();
            let error = Trash::in_folder(&data).unwrap_err();
            // A link opened as a folder without following it is refused.
            assert_eq!(error.kind(), io::ErrorKind::NotADirectory, "{link}");
            assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0, "{link}");
        }
    }

    #[test]
    fn a_move_that_fails_leaves_no_information_behind() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = trash(temporary.path());
        let folder = temporary.path().join("folder");
        fs::create_dir(&folder).unwrap();
        // No such file: the name is reserved, then the move fails.
        let error = bin
            .put(
                &Directory::open(&folder).unwrap(),
                OsStr::new("absent"),
                &folder.join("absent"),
                DATE,
                any,
            )
            .unwrap_err();
        assert!(
            matches!(error, TrashError::Io(ref error) if error.kind() == io::ErrorKind::NotFound)
        );
        assert_eq!(
            fs::read_dir(temporary.path().join("Trash/info"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn a_file_of_another_filesystem_stays_and_nothing_is_written() {
        let temporary = tempfile::tempdir().unwrap();
        let mut bin = trash(temporary.path());
        bin.device = bin.device.wrapping_add(1);
        let folder = temporary.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("file"), b"x").unwrap();
        let error = bin
            .put(
                &Directory::open(&folder).unwrap(),
                OsStr::new("file"),
                &folder.join("file"),
                DATE,
                any,
            )
            .unwrap_err();
        assert!(matches!(error, TrashError::OtherDevice));
        assert!(folder.join("file").exists());
        assert_eq!(
            fs::read_dir(temporary.path().join("Trash/info"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn a_file_swapped_in_before_the_move_is_put_back() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = trash(temporary.path());
        let folder = temporary.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("file"), b"swapped in").unwrap();
        let directory = Directory::open(&folder).unwrap();
        let error = bin
            .put(
                &directory,
                OsStr::new("file"),
                &folder.join("file"),
                DATE,
                |_| false,
            )
            .unwrap_err();
        assert!(matches!(error, TrashError::Changed));
        assert_eq!(fs::read(folder.join("file")).unwrap(), b"swapped in");
        assert_eq!(
            fs::read_dir(temporary.path().join("Trash/files"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(temporary.path().join("Trash/info"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn a_name_taken_again_is_never_overwritten_by_the_put_back() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = trash(temporary.path());
        let folder = temporary.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("file"), b"swapped in").unwrap();
        let directory = Directory::open(&folder).unwrap();
        // Another program writes a file of that name once the move is done.
        let newcomer = folder.join("file");
        let still = |_: &EntryStatus| {
            fs::write(&newcomer, b"newcomer").unwrap();
            false
        };
        let error = bin
            .put(
                &directory,
                OsStr::new("file"),
                &folder.join("file"),
                DATE,
                still,
            )
            .unwrap_err();
        assert!(matches!(error, TrashError::Unchecked(ref name) if name == "file"));
        assert_eq!(fs::read(folder.join("file")).unwrap(), b"newcomer");
        let trash = temporary.path().join("Trash");
        assert_eq!(fs::read(trash.join("files/file")).unwrap(), b"swapped in");
        assert!(trash.join("info/file.trashinfo").exists(), "restorable");
    }

    #[test]
    fn an_information_file_that_is_a_link_is_never_written_through() {
        let temporary = tempfile::tempdir().unwrap();
        let bin = trash(temporary.path());
        let target = temporary.path().join("target");
        fs::write(&target, b"untouched").unwrap();
        let info = temporary.path().join("Trash/info");
        symlink(&target, info.join("x.trashinfo")).unwrap();
        symlink(temporary.path().join("nowhere"), info.join("x.2.trashinfo")).unwrap();
        let folder = temporary.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("x"), b"x").unwrap();
        let name = bin
            .put(
                &Directory::open(&folder).unwrap(),
                OsStr::new("x"),
                &folder.join("x"),
                DATE,
                any,
            )
            .unwrap();
        assert_eq!(name, "x.3");
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        assert!(!temporary.path().join("nowhere").exists());
        assert!(info.join("x.3.trashinfo").exists());
    }
}
