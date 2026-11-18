//! The user's trash, as freedesktop specifies it (Trash 1.0): a file goes to
//! `$XDG_DATA_HOME/Trash/files`, and what restores it to
//! `Trash/info/<name>.trashinfo`. A file is moved there from its own folder,
//! by descriptors, never copied: one of another filesystem stays where it
//! is. Nothing is written through a link. When an entry went there is read
//! back from its `.trashinfo` ([`read_deletion`]); the `.trashinfo` of an
//! entry gone is removed by [`remove_information`]. Neither goes through a
//! link.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::directory::link_refused;
use crate::probe::read_up_to;
use crate::{Directory, EntryStatus};

const TRASH: &str = "Trash";
const FILES: &str = "files";
const INFO: &str = "info";
const INFO_EXTENSION: &str = ".trashinfo";
/// The first line of a `.trashinfo`, and the group its keys belong to.
const INFO_GROUP: &str = "[Trash Info]";
const PATH_KEY: &str = "Path";
const DELETION_DATE_KEY: &str = "DeletionDate";
/// How long a `.trashinfo` may be: the longest path Linux takes (4096
/// bytes), every byte escaped, fits in it twice over.
const INFO_MOST_BYTES: usize = 32 * 1024;
const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_HOUR: u32 = 3_600;
const SECONDS_PER_MINUTE: u32 = 60;
const MINUTES_PER_HOUR: u32 = 60;
const HOURS_PER_DAY: u32 = 24;
const MONTHS_PER_YEAR: u32 = 12;
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
#[expect(
    clippy::disallowed_methods,
    reason = "a folder of the trash, maybe created here: no identity read before to check"
)]
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
    format!("{INFO_GROUP}\n{PATH_KEY}={path}\n{DELETION_DATE_KEY}={deleted_at}\n")
}

/// The trash of the user's home, below their data folder `data`
/// (`$XDG_DATA_HOME`).
#[must_use]
pub fn home_trash(data: &Path) -> PathBuf {
    data.join(TRASH)
}

/// Where the trash whose entries are in `files` keeps what restores them:
/// its `info` folder, beside `files`; `None` when `files` is not the
/// `files` folder of a trash.
#[must_use]
pub fn information_folder(files: &Path) -> Option<PathBuf> {
    if files.file_name()? != FILES {
        return None;
    }
    Some(files.parent()?.join(INFO))
}

/// When an entry of the trash went there, as its `.trashinfo` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deletion {
    /// Its `DeletionDate`, read as if it were universal time: the trash
    /// writes local time without its zone, which is up to 14 hours off.
    pub date: SystemTime,
    /// The `.trashinfo` as it was read: what [`remove_information`] removes.
    pub information: EntryStatus,
}

/// Why the `.trashinfo` of an entry of the trash does not say when it went.
#[derive(Debug, thiserror::Error)]
pub enum InformationRefusal {
    /// No `.trashinfo` of its name.
    #[error("the entry has no information")]
    Absent,
    /// A link, a folder, a pipe...: never followed nor read.
    #[error("the information is not a plain file")]
    NotAFile,
    /// Longer than a `.trashinfo` of the longest path.
    #[error("the information is longer than {INFO_MOST_BYTES} bytes")]
    TooLong,
    /// Its first line is not `[Trash Info]`, or its first `DeletionDate` is
    /// missing, or not a date.
    #[error("the information does not say when the entry went")]
    Malformed,
    #[error(transparent)]
    Io(io::Error),
}

/// When the entry `trashed` of a trash went there, read from its
/// `.trashinfo` in the trash's `info` folder `information`, never through a
/// link.
///
/// # Errors
///
/// Why it cannot be told ([`InformationRefusal`]).
pub fn read_deletion(
    information: &Directory,
    trashed: &OsStr,
) -> Result<Deletion, InformationRefusal> {
    let (file, status) = information
        .open_file(&information_name(trashed))
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => InformationRefusal::Absent,
            // Not a plain file, or a link (`O_NOFOLLOW`).
            io::ErrorKind::InvalidInput => InformationRefusal::NotAFile,
            _ if link_refused(&error) => InformationRefusal::NotAFile,
            _ => InformationRefusal::Io(error),
        })?;
    let bytes = read_up_to(file, INFO_MOST_BYTES).map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidData {
            InformationRefusal::TooLong
        } else {
            InformationRefusal::Io(error)
        }
    })?;
    let date =
        deletion_date(&String::from_utf8_lossy(&bytes)).ok_or(InformationRefusal::Malformed)?;
    Ok(Deletion {
        date,
        information: status,
    })
}

/// What became of a `.trashinfo` to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InformationRemoval {
    /// Removed, as it was read.
    Removed,
    /// Already gone.
    Gone,
    /// Another file has its name now, or it was written since it was read:
    /// left as it is.
    Replaced,
}

/// Removes the `.trashinfo` of the entry `trashed` from the trash's `info`
/// folder `information`, if it is still the one read as `read`.
///
/// # Errors
///
/// The system's error.
pub fn remove_information(
    information: &Directory,
    trashed: &OsStr,
    read: &EntryStatus,
) -> io::Result<InformationRemoval> {
    let name = information_name(trashed);
    match information.status_of(&name) {
        Ok(now) if now.identity == read.identity && now.changed == read.changed => {}
        Ok(_) => return Ok(InformationRemoval::Replaced),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(InformationRemoval::Gone);
        }
        Err(error) => return Err(error),
    }
    match information.remove_file(&name) {
        Ok(()) => Ok(InformationRemoval::Removed),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(InformationRemoval::Gone),
        Err(error) => Err(error),
    }
}

/// The name of the `.trashinfo` of the entry `trashed`.
fn information_name(trashed: &OsStr) -> OsString {
    let mut name = trashed.to_os_string();
    name.push(INFO_EXTENSION);
    name
}

/// The first `DeletionDate` of the `.trashinfo` `text`, if its first line
/// is `[Trash Info]` and the date is one, written `YYYY-MM-DDThh:mm:ss` or,
/// as the specification's own example, `YYYYMMDDThh:mm:ss`; read as
/// universal time.
fn deletion_date(text: &str) -> Option<SystemTime> {
    let mut lines = text.lines();
    if lines.next()? != INFO_GROUP {
        return None;
    }
    for line in lines {
        // The keys of another group are not this one's.
        if line.starts_with('[') {
            return None;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim() == DELETION_DATE_KEY
        {
            // The first one counts, whatever follows.
            return civil_time(value.trim());
        }
    }
    None
}

/// The instant of the civil date and time `value`, as universal time.
fn civil_time(value: &str) -> Option<SystemTime> {
    let (date, time) = value.split_once('T')?;
    let (year, month, day) = match date.as_bytes() {
        [_, _, _, _, b'-', _, _, b'-', _, _] => (date.get(..4)?, date.get(5..7)?, date.get(8..)?),
        [_, _, _, _, _, _, _, _] => (date.get(..4)?, date.get(4..6)?, date.get(6..)?),
        _ => return None,
    };
    let [hours, minutes, seconds] = match time.as_bytes() {
        [_, _, b':', _, _, b':', _, _] => [time.get(..2)?, time.get(3..5)?, time.get(6..)?],
        _ => return None,
    };
    let [year, month, day, hours, minutes, seconds] =
        [year, month, day, hours, minutes, seconds].map(digits);
    let (year, month, day) = (year?, month?, day?);
    let (hours, minutes, seconds) = (hours?, minutes?, seconds?);
    if !(1..=MONTHS_PER_YEAR).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hours >= HOURS_PER_DAY
        || minutes >= MINUTES_PER_HOUR
        || seconds >= SECONDS_PER_MINUTE
    {
        return None;
    }
    let since_epoch = days_from_civil(year, month, day)
        .checked_mul(SECONDS_PER_DAY)?
        .checked_add(i64::from(
            hours * SECONDS_PER_HOUR + minutes * SECONDS_PER_MINUTE + seconds,
        ))?;
    let offset = Duration::from_secs(since_epoch.unsigned_abs());
    if since_epoch >= 0 {
        UNIX_EPOCH.checked_add(offset)
    } else {
        UNIX_EPOCH.checked_sub(offset)
    }
}

/// The number `text` writes in decimal digits alone (no sign, no space).
fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

const fn is_leap(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The days from 1970-01-01 to the date of the proleptic Gregorian calendar
/// (Howard Hinnant's `days_from_civil`), negative before.
fn days_from_civil(year: u32, month: u32, day: u32) -> i64 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let march_based_month = i64::from((month + 9) % 12);
    let day_of_year = (153 * march_based_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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

    /// The instant `seconds` after 1970, as [`deletion_date`] gives it.
    fn at(seconds: u64) -> Option<SystemTime> {
        UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
    }

    #[test]
    fn the_deletion_date_is_read_as_written_either_way() {
        // The specification's example, written both ways.
        for date in ["2004-08-31T22:32:08", "20040831T22:32:08"] {
            let text = format!("[Trash Info]\nPath=foo/bar/meow.bow-wow\nDeletionDate={date}\n");
            assert_eq!(deletion_date(&text), at(1_093_991_528), "{date}");
        }
        // Line ends of another system, spaces around the sign, no last line end.
        assert_eq!(
            deletion_date("[Trash Info]\r\nPath=/a\r\nDeletionDate = 1970-01-02T00:00:01"),
            at(86_401)
        );
        // A leap day, and the instants around the epoch.
        assert_eq!(
            deletion_date("[Trash Info]\nDeletionDate=2024-02-29T00:00:00\n"),
            at(1_709_164_800)
        );
        assert_eq!(
            deletion_date("[Trash Info]\nDeletionDate=1969-12-31T23:59:59\n"),
            UNIX_EPOCH.checked_sub(Duration::from_secs(1))
        );
    }

    #[test]
    fn only_the_first_deletion_date_of_the_group_counts() {
        assert_eq!(
            deletion_date(
                "[Trash Info]\nDeletionDate=2004-08-31T22:32:08\nDeletionDate=1970-01-01T00:00:00\n"
            ),
            at(1_093_991_528)
        );
        // A first one that is no date is not made up for by the next.
        assert_eq!(
            deletion_date(
                "[Trash Info]\nDeletionDate=yesterday\nDeletionDate=1970-01-01T00:00:00\n"
            ),
            None
        );
        // Another group's key is not this one's.
        assert_eq!(
            deletion_date("[Trash Info]\nPath=/a\n[Other]\nDeletionDate=1970-01-01T00:00:00\n"),
            None
        );
    }

    #[test]
    fn what_is_not_a_trash_information_or_not_a_date_says_nothing() {
        for text in [
            "",
            "Path=/a\nDeletionDate=2004-08-31T22:32:08\n",
            "\n[Trash Info]\nDeletionDate=2004-08-31T22:32:08\n",
            "[Trash Info] \nDeletionDate=2004-08-31T22:32:08\n",
            "[Trash Info]\nPath=/a\n",
            "[Trash Info]\nDeletionDate=\n",
            "[Trash Info]\nDeletionDate=2004-13-01T00:00:00\n",
            "[Trash Info]\nDeletionDate=2004-00-01T00:00:00\n",
            "[Trash Info]\nDeletionDate=2023-02-29T00:00:00\n",
            "[Trash Info]\nDeletionDate=1900-02-29T00:00:00\n",
            "[Trash Info]\nDeletionDate=2004-04-31T00:00:00\n",
            "[Trash Info]\nDeletionDate=2004-08-00T00:00:00\n",
            "[Trash Info]\nDeletionDate=2004-08-31T24:00:00\n",
            "[Trash Info]\nDeletionDate=2004-08-31T23:60:00\n",
            "[Trash Info]\nDeletionDate=2004-08-31T23:59:60\n",
            "[Trash Info]\nDeletionDate=2004-08-31 22:32:08\n",
            "[Trash Info]\nDeletionDate=2004-08-31T22:32:08Z\n",
            "[Trash Info]\nDeletionDate=2004-08-31T22:32:08.5\n",
            "[Trash Info]\nDeletionDate=2004-08-31T22:32\n",
            "[Trash Info]\nDeletionDate=+004-08-31T22:32:08\n",
            "[Trash Info]\nDeletionDate=2004/08/31T22:32:08\n",
            "[Trash Info]\nDeletionDate=2004-8-031T22:32:08\n",
        ] {
            assert_eq!(deletion_date(text), None, "{text:?}");
        }
        // A leap day of a year divisible by 400 is one.
        assert!(deletion_date("[Trash Info]\nDeletionDate=2000-02-29T00:00:00\n").is_some());
    }

    #[test]
    fn what_the_trash_writes_reads_back() {
        let text = restoring(Path::new("/home/a b/photo.jpg"), DATE);
        assert_eq!(deletion_date(&text), at(1_794_696_425));
    }

    #[test]
    fn the_information_folder_is_beside_the_files_folder_only() {
        assert_eq!(
            information_folder(Path::new("/home/a/.local/share/Trash/files")),
            Some(PathBuf::from("/home/a/.local/share/Trash/info"))
        );
        for other in ["/home/a/.local/share/Trash", "/home/a/files/x", "/"] {
            assert_eq!(information_folder(Path::new(other)), None, "{other}");
        }
    }

    /// A trash under `data` holding `name` and its `.trashinfo` `content`;
    /// its `info` folder, opened.
    fn trash_holding(data: &Path, name: &str, content: &[u8]) -> Directory {
        fs::create_dir_all(data.join("Trash/files")).unwrap();
        fs::create_dir_all(data.join("Trash/info")).unwrap();
        fs::write(data.join("Trash/files").join(name), b"x").unwrap();
        fs::write(
            data.join("Trash/info").join(format!("{name}.trashinfo")),
            content,
        )
        .unwrap();
        Directory::open(&data.join("Trash/info")).unwrap()
    }

    #[test]
    fn the_deletion_of_an_entry_is_read_from_its_information() {
        let temporary = tempfile::tempdir().unwrap();
        let text = format!("[Trash Info]\nPath=/a\nDeletionDate={DATE}\n");
        let information = trash_holding(temporary.path(), "photo.jpg", text.as_bytes());
        let deletion = read_deletion(&information, OsStr::new("photo.jpg")).unwrap();
        assert_eq!(Some(deletion.date), at(1_794_696_425));
        assert_eq!(
            Some(deletion.information.identity),
            crate::file_identity(
                &fs::symlink_metadata(temporary.path().join("Trash/info/photo.jpg.trashinfo"))
                    .unwrap()
            )
        );
        assert!(matches!(
            read_deletion(&information, OsStr::new("absent")),
            Err(InformationRefusal::Absent)
        ));
    }

    #[test]
    fn an_information_that_is_a_link_a_folder_too_long_or_undated_is_refused() {
        let temporary = tempfile::tempdir().unwrap();
        let good = format!("[Trash Info]\nDeletionDate={DATE}\n");
        let information = trash_holding(temporary.path(), "kept", good.as_bytes());
        let info = temporary.path().join("Trash/info");
        // A link to a good one is never followed.
        symlink(info.join("kept.trashinfo"), info.join("linked.trashinfo")).unwrap();
        assert!(matches!(
            read_deletion(&information, OsStr::new("linked")),
            Err(InformationRefusal::NotAFile)
        ));
        fs::create_dir(info.join("folder.trashinfo")).unwrap();
        assert!(matches!(
            read_deletion(&information, OsStr::new("folder")),
            Err(InformationRefusal::NotAFile)
        ));
        let mut long = good.clone();
        long.push_str(&"#".repeat(INFO_MOST_BYTES));
        fs::write(info.join("long.trashinfo"), long).unwrap();
        assert!(matches!(
            read_deletion(&information, OsStr::new("long")),
            Err(InformationRefusal::TooLong)
        ));
        fs::write(info.join("undated.trashinfo"), "[Trash Info]\nPath=/a\n").unwrap();
        assert!(matches!(
            read_deletion(&information, OsStr::new("undated")),
            Err(InformationRefusal::Malformed)
        ));
        // Within the limit, however long its path.
        let mut within = good;
        within.push_str(&"#".repeat(INFO_MOST_BYTES / 2));
        fs::write(info.join("within.trashinfo"), within).unwrap();
        assert!(read_deletion(&information, OsStr::new("within")).is_ok());
    }

    #[test]
    fn an_information_rewritten_in_place_since_it_was_read_is_left() {
        let temporary = tempfile::tempdir().unwrap();
        let text = format!("[Trash Info]\nDeletionDate={DATE}\n");
        let information = trash_holding(temporary.path(), "entry", text.as_bytes());
        let path = temporary.path().join("Trash/info/entry.trashinfo");
        let read = read_deletion(&information, OsStr::new("entry")).unwrap();
        // The clock of inode dates is coarse: past one tick of it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .write_all(b"[Trash Info]\nDeletionDate=2999-01-01T00:00:00\n")
            .unwrap();
        assert_eq!(
            remove_information(&information, OsStr::new("entry"), &read.information).unwrap(),
            InformationRemoval::Replaced
        );
        assert!(path.exists());
    }

    #[test]
    fn an_information_is_removed_only_when_it_is_still_the_one_read() {
        let temporary = tempfile::tempdir().unwrap();
        let text = format!("[Trash Info]\nDeletionDate={DATE}\n");
        let information = trash_holding(temporary.path(), "entry", text.as_bytes());
        let path = temporary.path().join("Trash/info/entry.trashinfo");
        let read = read_deletion(&information, OsStr::new("entry")).unwrap();
        // Replaced since it was read: left. Kept open meanwhile, the first
        // one's inode cannot be reused by the second.
        let _first = fs::File::open(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, &text).unwrap();
        assert_eq!(
            remove_information(&information, OsStr::new("entry"), &read.information).unwrap(),
            InformationRemoval::Replaced
        );
        assert!(path.exists());
        let read = read_deletion(&information, OsStr::new("entry")).unwrap();
        assert_eq!(
            remove_information(&information, OsStr::new("entry"), &read.information).unwrap(),
            InformationRemoval::Removed
        );
        assert!(!path.exists());
        assert!(temporary.path().join("Trash/files/entry").exists());
        assert_eq!(
            remove_information(&information, OsStr::new("entry"), &read.information).unwrap(),
            InformationRemoval::Gone
        );
    }
}
