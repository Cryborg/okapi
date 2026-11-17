//! What the screens that look through the user's files and move some to
//! the trash share: where they search (the personal folders, read on a
//! thread of their own, or the whole home folder), the exclusions of the
//! settings, which disk the trash takes, and the wait for a move at
//! closing.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use okapi_engine::Exclusions;
use okapi_platform::{PersonalFolder, PersonalKind, Trash};

use crate::cleaner::Problem;
use crate::file_manager::{Open, open_in_file_manager};
use crate::shown::{from_home, name_of};
use crate::ui;

/// Never searched: what snapd installs is none of the user's own files.
const SNAP_FOLDER: &str = okapi_tools::apps::snap::SNAP_DATA;
/// How long the window's closing waits for a move to the trash to end:
/// hundreds of renames take less than a second.
const TRASH_WAIT: Duration = Duration::from_secs(5);

/// Reads the exclusions of the settings, once the writes under way are
/// done (the cleaner's own reading).
pub type ReadExclusions = Rc<dyn Fn() -> Result<Vec<PathBuf>, Problem>>;
/// The personal folders named below a home folder.
pub type ReadFolders = Arc<dyn Fn(&Path) -> io::Result<Vec<PersonalFolder>> + Send + Sync>;
/// The trash files go to.
pub type OpenTrash = Arc<dyn Fn() -> io::Result<Trash> + Send + Sync>;

/// What a screen that searches the user's folders reads and asks of the
/// machine: the system's, or a test's.
#[derive(Clone)]
pub struct SearchSeams {
    pub home: Option<PathBuf>,
    pub folders: ReadFolders,
    pub trash: OpenTrash,
    pub open: Open,
    pub now: fn() -> Timestamp,
    pub zone: TimeZone,
}

impl std::fmt::Debug for SearchSeams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SearchSeams")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl SearchSeams {
    /// The session's home folder `home`, its personal folders, its trash,
    /// the portal.
    pub fn system(home: Option<PathBuf>) -> Self {
        Self {
            home,
            folders: Arc::new(okapi_platform::personal_folders),
            trash: Arc::new(Trash::home),
            open: Rc::new(open_in_file_manager),
            now: Timestamp::now,
            zone: crate::dashboard::system_zone(),
        }
    }
}

/// Seams reading no folder and opening no trash, for the other screens'
/// tests.
#[cfg(test)]
pub fn inert() -> SearchSeams {
    SearchSeams {
        home: None,
        folders: Arc::new(|_| Ok(Vec::new())),
        trash: Arc::new(|| Err(io::Error::from(io::ErrorKind::Unsupported))),
        open: Rc::new(|_, done| done(false)),
        now: Timestamp::now,
        zone: TimeZone::UTC,
    }
}

/// A personal folder, as the screens know it: its kind, its real path (a
/// link on its way resolved, as the paths a search finds are).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Folder {
    pub kind: PersonalKind,
    pub path: PathBuf,
}

/// The rank of the personal folder `path` lies in: the deepest that holds
/// it.
pub(crate) fn folder_of(path: &Path, folders: &[Folder]) -> Option<usize> {
    folders
        .iter()
        .enumerate()
        .filter(|(_, folder)| path.starts_with(&folder.path))
        .max_by_key(|(_, folder)| folder.path.components().count())
        .map(|(rank, _)| rank)
}

/// Whether the trash takes a file on the filesystem `device`: on its own
/// filesystem; any, when the trash could not be opened (the move will say
/// why).
pub(crate) fn trashable(trash_device: Option<u64>, device: u64) -> bool {
    trash_device.is_none_or(|trash| trash == device)
}

/// What the start knows of the folders, read away from the interface's
/// thread (a folder on a mount that stopped answering would hold it): the
/// home folder and the personal folders where they really are, the
/// filesystem of the trash, and whether the personal folders could be read.
#[derive(Debug, Clone, Default)]
pub(crate) struct Known {
    pub home: Option<PathBuf>,
    pub folders: Vec<Folder>,
    pub trash_device: Option<u64>,
    pub unread: bool,
}

impl Known {
    /// Nothing known but the home folder: the folders unread.
    fn unread(home: Option<PathBuf>) -> Self {
        Self {
            home,
            unread: true,
            ..Self::default()
        }
    }

    /// The names of the personal folders, by rank.
    pub(crate) fn names(&self) -> Vec<String> {
        self.folders
            .iter()
            .map(|folder| name_of(&folder.path))
            .collect()
    }
}

/// Reads what the start knows.
fn read_known(seams_home: Option<&Path>, folders: &ReadFolders, trash: &OpenTrash) -> Known {
    let home = seams_home.and_then(|home| {
        home.canonicalize()
            .inspect_err(
                |error| tracing::warn!(home = %home.display(), %error, "home folder unreadable"),
            )
            .ok()
    });
    let (named, unread) = match home.as_deref().map(|home| folders(home)) {
        Some(Ok(named)) => (named, false),
        Some(Err(error)) => {
            tracing::warn!(%error, "personal folders unreadable");
            (Vec::new(), true)
        }
        None => (Vec::new(), true),
    };
    let mut known: Vec<Folder> = Vec::new();
    for folder in named {
        match folder.path.canonicalize() {
            // The home folder itself, or above it: no personal folder.
            Ok(path) if home.as_deref().is_some_and(|home| home.starts_with(&path)) => {
                tracing::warn!(path = %path.display(), kind = ?folder.kind, "personal folder holding the home folder: not offered");
            }
            Ok(path) if path.is_dir() => {
                // The first kind in the screen's order names it.
                if known.iter().any(|other| other.path == path) {
                    tracing::info!(path = %path.display(), kind = ?folder.kind, "personal folder named twice: once offered");
                } else {
                    known.push(Folder {
                        kind: folder.kind,
                        path,
                    });
                }
            }
            Ok(path) => {
                tracing::info!(path = %path.display(), "personal folder not a folder: not offered");
            }
            Err(error) => {
                tracing::info!(path = %folder.path.display(), %error, "personal folder absent: not offered");
            }
        }
    }
    let trash_device = trash()
        .inspect_err(|error| tracing::warn!(%error, "trash not opened: its disk unknown"))
        .ok()
        .map(|trash| trash.device());
    Known {
        home,
        folders: known,
        trash_device,
        unread,
    }
}

/// A search asked: where, how it is said, and what the start knew then.
#[derive(Debug, Clone)]
pub(crate) struct Asked {
    pub roots: Vec<PathBuf>,
    pub whole_home: bool,
    pub names: String,
    pub known: Known,
}

/// The start as the window shows it.
#[derive(Debug)]
pub(crate) struct StartShown {
    pub rows: Vec<ui::PersonalShown>,
    pub home: String,
    pub state: ui::FoldersState,
    pub chosen: usize,
}

/// Where a search may look: the personal folders known, being read on a
/// thread of their own, and those left unchecked, by where they are.
pub(crate) struct Folders {
    home: Option<PathBuf>,
    read: ReadFolders,
    trash: OpenTrash,
    known: Option<Known>,
    reading: Option<Receiver<Known>>,
    unchecked: BTreeSet<PathBuf>,
}

impl std::fmt::Debug for Folders {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Folders")
            .field("known", &self.known)
            .field("reading", &self.reading.is_some())
            .finish_non_exhaustive()
    }
}

impl Folders {
    pub(crate) fn new(seams: &SearchSeams) -> Self {
        Self {
            home: seams.home.clone(),
            read: Arc::clone(&seams.folders),
            trash: Arc::clone(&seams.trash),
            known: None,
            reading: None,
            unchecked: BTreeSet::new(),
        }
    }

    /// Reads the personal folders on a thread of its own: whether their
    /// reading is to be waited for (without a thread, they are unread).
    pub(crate) fn read(&mut self) -> bool {
        let (sender, receiver) = mpsc::channel();
        let (home, folders, trash) = (
            self.home.clone(),
            Arc::clone(&self.read),
            Arc::clone(&self.trash),
        );
        let spawned = std::thread::Builder::new()
            .name("okapi-personal-folders".to_owned())
            .spawn(move || {
                // The screen gone: nobody waits.
                let _ = sender.send(read_known(home.as_deref(), &folders, &trash));
            });
        match spawned {
            Ok(_) => {
                self.reading = Some(receiver);
                true
            }
            Err(error) => {
                tracing::error!(%error, "personal folders not read: no thread");
                self.known = Some(Known::unread(self.home.clone()));
                false
            }
        }
    }

    /// Whether they are being read.
    pub(crate) const fn reading(&self) -> bool {
        self.reading.is_some()
    }

    /// Takes what their reading found, if it ended: whether it did.
    pub(crate) fn take(&mut self) -> bool {
        let taken = match self.reading.as_ref().map(Receiver::try_recv) {
            Some(Ok(known)) => known,
            Some(Err(TryRecvError::Disconnected)) => {
                tracing::error!("personal folders thread ended without word");
                Known::unread(self.home.clone())
            }
            _ => return false,
        };
        tracing::info!(
            home = ?taken.home,
            folders = taken.folders.len(),
            trash_device = ?taken.trash_device,
            unread = taken.unread,
            "personal folders known"
        );
        self.reading = None;
        self.known = Some(taken);
        true
    }

    /// The start as it stands: the folders, which are checked.
    pub(crate) fn shown(&self) -> StartShown {
        let known = self.known.clone().unwrap_or_default();
        let rows: Vec<ui::PersonalShown> = known
            .folders
            .iter()
            .map(|folder| ui::PersonalShown {
                name: name_of(&folder.path).into(),
                path: from_home(&folder.path, known.home.as_deref()).into(),
                checked: !self.unchecked.contains(&folder.path),
            })
            .collect();
        let state = if self.known.is_none() || self.reading.is_some() {
            ui::FoldersState::Reading
        } else if known.home.is_none() {
            ui::FoldersState::Homeless
        } else if known.unread {
            ui::FoldersState::Unread
        } else if rows.is_empty() {
            ui::FoldersState::None
        } else {
            ui::FoldersState::Read
        };
        let chosen = rows.iter().filter(|row| row.checked).count();
        StartShown {
            rows,
            home: known
                .home
                .map(|home| home.display().to_string())
                .unwrap_or_default(),
            state,
            chosen,
        }
    }

    /// Checks or unchecks the folder at `index`: whether there is one.
    pub(crate) fn toggle(&mut self, index: usize) -> bool {
        let Some(path) = self
            .known
            .as_ref()
            .and_then(|known| known.folders.get(index))
            .map(|folder| folder.path.clone())
        else {
            return false;
        };
        if !self.unchecked.remove(&path) {
            self.unchecked.insert(path);
        }
        true
    }

    /// The search the start asks for, the whole home folder or not: `None`
    /// when it cannot be said yet (the folders not read), or why it is
    /// refused.
    pub(crate) fn asked(&self, whole_home: bool) -> Result<Option<Asked>, ui::ToolRefusal> {
        let known = match &self.known {
            Some(known) => known.clone(),
            // The folders not read yet (one on a mount that stopped
            // answering holds them): the whole home folder can be
            // searched, nothing checked beforehand.
            None if whole_home => {
                tracing::warn!("personal folders not read yet: the home folder searched alone");
                Known::unread(self.home.clone())
            }
            None => {
                tracing::warn!("search asked before the folders were read: nothing searched");
                return Ok(None);
            }
        };
        if whole_home {
            let Some(home) = known.home.clone() else {
                tracing::warn!("home folder unknown: nothing searched");
                return Err(ui::ToolRefusal::Home);
            };
            // Each personal folder is a root too: one elsewhere (a link, an
            // absolute path), mounted apart or a project would not be
            // reached from the home folder; the engine reads a folder given
            // twice once.
            let folders = known.folders.iter().map(|folder| folder.path.clone());
            return Ok(Some(Asked {
                roots: std::iter::once(home).chain(folders).collect(),
                whole_home: true,
                names: String::new(),
                known,
            }));
        }
        let chosen: Vec<&Folder> = known
            .folders
            .iter()
            .filter(|folder| !self.unchecked.contains(&folder.path))
            .collect();
        if chosen.is_empty() {
            tracing::warn!("no folder chosen: nothing searched");
            return Err(ui::ToolRefusal::Folder);
        }
        Ok(Some(Asked {
            roots: chosen.iter().map(|folder| folder.path.clone()).collect(),
            whole_home: false,
            // Names of folders, as on the disk: the same in both
            // languages, as the separator.
            names: chosen
                .iter()
                .map(|folder| name_of(&folder.path))
                .collect::<Vec<_>>()
                .join(", "),
            known: known.clone(),
        }))
    }
}

/// The exclusions `paths`, resolved where each really lies: on the thread
/// of a search or a move, as a mount that stopped answering may hold it.
pub(crate) fn resolve(paths: Vec<PathBuf>) -> Result<Exclusions, ui::ToolRefusal> {
    tracing::debug!(exclusions = paths.len(), "resolving exclusions");
    let started = Instant::now();
    let resolved = Exclusions::new(paths).map_err(|error| {
        tracing::warn!(?error, elapsed = ?started.elapsed(), "an exclusion refused: nothing done");
        ui::ToolRefusal::Exclusion
    })?;
    tracing::info!(elapsed = ?started.elapsed(), "exclusions resolved");
    Ok(resolved)
}

/// The exclusions of the settings read now by `read`, `~/snap` with them,
/// as they are written: resolved on the thread that uses them
/// ([`resolve`]); why none can be had.
pub(crate) fn exclusion_paths(
    read: &ReadExclusions,
    home: Option<&Path>,
) -> Result<Vec<PathBuf>, ui::ToolRefusal> {
    let mut paths = read().map_err(|problem| {
        tracing::warn!(?problem, "settings not read: nothing done");
        match problem {
            Problem::SettingsSaving => ui::ToolRefusal::Saving,
            Problem::SettingsUnsaved => ui::ToolRefusal::Unsaved,
            Problem::Exclusion => ui::ToolRefusal::Exclusion,
            _ => ui::ToolRefusal::Settings,
        }
    })?;
    tracing::info!(settings = paths.len(), "exclusions read");
    paths.extend(home.map(|home| home.join(SNAP_FOLDER)));
    Ok(paths)
}

/// The window closes: the move of `what` heard on `moving` is waited for a
/// little (each file is one rename; a cut one leaves at most a
/// `.trashinfo` without its file, or a file in the trash not read again).
pub(crate) fn wait_at_closing<T>(moving: &Receiver<T>, what: &str) {
    let started = Instant::now();
    match moving.recv_timeout(TRASH_WAIT) {
        Ok(_) => {
            tracing::info!(waited = ?started.elapsed(), what, "move to the trash ended before closing");
        }
        Err(RecvTimeoutError::Disconnected) => {
            tracing::error!(
                waited = ?started.elapsed(),
                what,
                "the move to the trash ended without word: some may be in the trash"
            );
        }
        Err(RecvTimeoutError::Timeout) => {
            tracing::warn!(waited = ?started.elapsed(), what, "move to the trash left running at closing");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_lies_in_the_deepest_personal_folder_that_holds_it() {
        let folder = |kind, path: &str| Folder {
            kind,
            path: PathBuf::from(path),
        };
        // In either order: the deeper is neither the first nor the last that
        // holds the file.
        let mut folders = vec![
            folder(PersonalKind::Documents, "/home/ana/Documents"),
            folder(PersonalKind::Pictures, "/home/ana/Images"),
            folder(PersonalKind::Music, "/home/ana/Documents/Musique"),
        ];
        for _ in 0..2 {
            let rank = |path| {
                folders
                    .iter()
                    .position(|folder| folder.path == Path::new(path))
            };
            assert_eq!(
                folder_of(Path::new("/home/ana/Documents/Musique/a"), &folders),
                rank("/home/ana/Documents/Musique")
            );
            assert_eq!(
                folder_of(Path::new("/home/ana/Documents/a"), &folders),
                rank("/home/ana/Documents")
            );
            folders.reverse();
        }
        assert_eq!(
            folder_of(Path::new("/home/ana/Documentsx/a"), &folders),
            None
        );
    }

    #[test]
    fn the_folders_offered_are_where_they_really_are_and_never_hold_the_home() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().canonicalize().unwrap();
        for folder in ["Documents", "Images"] {
            std::fs::create_dir(root.join(folder)).unwrap();
        }
        std::os::unix::fs::symlink(root.join("Images"), root.join("Photos")).unwrap();
        std::fs::write(root.join("a-file"), "").unwrap();
        let named = [
            (PersonalKind::Documents, root.join("Documents")),
            // Through a link: offered where it is, once.
            (PersonalKind::Pictures, root.join("Photos")),
            (PersonalKind::Videos, root.join("Images")),
            (PersonalKind::Music, root.clone()),
            (PersonalKind::Desktop, PathBuf::from("/")),
            (PersonalKind::Download, root.join("absent")),
            // A file, not a folder.
            (PersonalKind::Download, root.join("a-file")),
        ];
        let folders: ReadFolders = Arc::new(move |_| {
            Ok(named
                .iter()
                .map(|(kind, path)| PersonalFolder {
                    kind: *kind,
                    path: path.clone(),
                })
                .collect())
        });
        let trash: OpenTrash = Arc::new(|| Err(io::Error::other("no trash")));
        let known = read_known(Some(&root), &folders, &trash);
        assert_eq!(known.home.as_deref(), Some(root.as_path()));
        let offered: Vec<(PersonalKind, PathBuf)> = known
            .folders
            .into_iter()
            .map(|folder| (folder.kind, folder.path))
            .collect();
        assert_eq!(
            offered,
            [
                (PersonalKind::Documents, root.join("Documents")),
                (PersonalKind::Pictures, root.join("Images")),
            ]
        );
        assert_eq!(known.trash_device, None);
        // A trash that opens says its disk.
        let data = root.join("data");
        std::fs::create_dir(&data).unwrap();
        let trash: OpenTrash = Arc::new(move || Trash::in_folder(&data));
        let known = read_known(Some(&root), &folders, &trash);
        let device = okapi_platform::file_identity(&std::fs::metadata(&root).unwrap())
            .map(okapi_platform::FileIdentity::device);
        assert_eq!(known.trash_device, device);
        assert!(known.trash_device.is_some());
    }

    #[test]
    fn the_trash_takes_a_file_of_its_own_disk_or_any_when_unknown() {
        assert!(trashable(Some(1), 1));
        assert!(!trashable(Some(1), 2));
        assert!(trashable(None, 2));
    }
}
