//! The user's personal folders (Documents, Pictures, Downloads...) as the
//! freedesktop `xdg-user-dirs` specification names them, in
//! `$XDG_CONFIG_HOME/user-dirs.dirs`: a file read, never a command run.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use crate::read_regular_capped;
use crate::variables::{CONFIG, usable_home};

/// The file that names the personal folders, below the configuration home.
const USER_DIRS: &str = "user-dirs.dirs";
/// Far beyond a file of a dozen lines: a longer one is no such file.
const USER_DIRS_LIMIT: usize = 64 * 1024;
/// What a folder below the home folder is written from.
const HOME_PREFIX: &[u8] = b"$HOME";

/// A personal folder the screens offer: the others (`TEMPLATES`,
/// `PUBLICSHARE`) hold nothing the user keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PersonalKind {
    Documents,
    Pictures,
    Music,
    Videos,
    Download,
    Desktop,
}

impl PersonalKind {
    /// Every kind, in the order the screens offer them.
    pub const ALL: [Self; 6] = [
        Self::Documents,
        Self::Pictures,
        Self::Music,
        Self::Videos,
        Self::Download,
        Self::Desktop,
    ];

    /// Its name in the file: `XDG_<name>_DIR`.
    const fn key(self) -> &'static [u8] {
        match self {
            Self::Documents => b"XDG_DOCUMENTS_DIR",
            Self::Pictures => b"XDG_PICTURES_DIR",
            Self::Music => b"XDG_MUSIC_DIR",
            Self::Videos => b"XDG_VIDEOS_DIR",
            Self::Download => b"XDG_DOWNLOAD_DIR",
            Self::Desktop => b"XDG_DESKTOP_DIR",
        }
    }
}

/// A personal folder, where the file says it is (it may not exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalFolder {
    pub kind: PersonalKind,
    pub path: PathBuf,
}

/// The personal folders the file names below `home`, in the order of
/// [`PersonalKind::ALL`]; none when it does not exist (no desktop wrote it).
/// Never read through a link.
///
/// # Errors
///
/// The system's error when the file cannot be read, or
/// an error of its own when the configuration folder is unknown.
pub fn personal_folders(home: &Path) -> io::Result<Vec<PersonalFolder>> {
    let configuration = CONFIG
        .resolve(
            |name| std::env::var_os(name),
            usable_home(Some(home.to_path_buf())).as_deref(),
        )
        .ok_or_else(|| io::Error::other("configuration folder unknown"))?;
    read_personal_folders(&configuration.join(USER_DIRS), home)
}

/// [`personal_folders`], named by the file `file`, below `home`.
fn read_personal_folders(file: &Path, home: &Path) -> io::Result<Vec<PersonalFolder>> {
    let bytes = match read_regular_capped(file, USER_DIRS_LIMIT) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::info!(file = %file.display(), "no personal folders named");
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let (folders, ignored) = parse_user_dirs(&bytes, home);
    if ignored > 0 {
        tracing::warn!(file = %file.display(), folders = folders.len(), ignored, "personal folders read, lines ignored");
    } else {
        tracing::info!(file = %file.display(), folders = folders.len(), "personal folders read");
    }
    Ok(folders)
}

/// The folders `bytes`, a `user-dirs.dirs`, names below `home`, and how
/// many lines it holds that are neither blank, a comment, nor a folder:
/// `XDG_<NAME>_DIR="$HOME/<path>"` or `="/<path>"`, the path escaped as in
/// a shell's double quotes. A folder that is the home folder itself is
/// turned off, as the specification says; the last line of a name wins.
fn parse_user_dirs(bytes: &[u8], home: &Path) -> (Vec<PersonalFolder>, usize) {
    let mut found: Vec<(PersonalKind, Option<PathBuf>)> = Vec::new();
    let mut ignored = 0;
    for (number, line) in bytes.split(|&byte| byte == b'\n').enumerate() {
        let line = line.trim_ascii();
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        let Some((key, value)) = line
            .iter()
            .position(|&byte| byte == b'=')
            .map(|equals| (&line[..equals], &line[equals + 1..]))
        else {
            tracing::debug!(line = number + 1, "personal folders: not a setting");
            ignored += 1;
            continue;
        };
        let Some(kind) = PersonalKind::ALL
            .into_iter()
            .find(|kind| kind.key() == key.trim_ascii())
        else {
            // Another folder (`TEMPLATES`...), or none at all.
            continue;
        };
        let Some(path) = folder_of(value.trim_ascii(), home) else {
            // A folder offered no more: said, its path not (the user's).
            tracing::warn!(
                line = number + 1,
                ?kind,
                "personal folders: a path not understood"
            );
            ignored += 1;
            continue;
        };
        let path = (path != home).then_some(path);
        found.retain(|(other, _)| *other != kind);
        found.push((kind, path));
    }
    let folders = PersonalKind::ALL
        .into_iter()
        .filter_map(|kind| {
            found
                .iter()
                .find(|(other, _)| *other == kind)
                .and_then(|(_, path)| path.clone())
                .map(|path| PersonalFolder { kind, path })
        })
        .collect();
    (folders, ignored)
}

/// The folder a quoted value names: below `home`, or absolute.
fn folder_of(value: &[u8], home: &Path) -> Option<PathBuf> {
    let inner = value.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    let text = unescaped(inner)?;
    if let Some(rest) = text.strip_prefix(HOME_PREFIX) {
        return match rest {
            [] => Some(home.to_path_buf()),
            [b'/', below @ ..] => Some(home.join(OsStr::from_bytes(below))),
            _ => None,
        };
    }
    // A path written with a slash at its end is the same folder.
    text.starts_with(b"/")
        .then(|| Path::new(&OsString::from_vec(text)).components().collect())
}

/// `inner` with a shell's escapes inside double quotes undone (`\"`, `\\`,
/// `\$`, `` \` ``); `None` for an unescaped quote, which ends the value early.
fn unescaped(inner: &[u8]) -> Option<Vec<u8>> {
    let mut text = Vec::with_capacity(inner.len());
    let mut bytes = inner.iter().copied();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' => match bytes.next() {
                Some(escaped @ (b'"' | b'\\' | b'$' | b'`')) => text.push(escaped),
                Some(other) => text.extend([b'\\', other]),
                None => text.push(b'\\'),
            },
            b'"' => return None,
            other => text.push(other),
        }
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/ana";

    fn parsed(text: &str) -> (Vec<(PersonalKind, String)>, usize) {
        let (folders, ignored) = parse_user_dirs(text.as_bytes(), Path::new(HOME));
        (
            folders
                .into_iter()
                .map(|folder| (folder.kind, folder.path.display().to_string()))
                .collect(),
            ignored,
        )
    }

    #[test]
    fn folders_are_read_below_the_home_or_absolute_in_the_screens_order() {
        let (folders, ignored) = parsed(
            "# This file is written by xdg-user-dirs-update\n\
             XDG_DESKTOP_DIR=\"$HOME/Bureau\"\n\
             XDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n\
             XDG_TEMPLATES_DIR=\"$HOME/Modèles\"\n\
             XDG_PUBLICSHARE_DIR=\"$HOME/Public\"\n\
             XDG_DOCUMENTS_DIR=\"/data/Documents/\"\n\
             XDG_MUSIC_DIR=\"$HOME/Musique\"\n\
             XDG_PICTURES_DIR=\"$HOME/Images\"\n\
             XDG_VIDEOS_DIR=\"$HOME/Vidéos\"\n",
        );
        assert_eq!(ignored, 0);
        assert_eq!(
            folders,
            [
                (PersonalKind::Documents, "/data/Documents".to_owned()),
                (PersonalKind::Pictures, "/home/ana/Images".to_owned()),
                (PersonalKind::Music, "/home/ana/Musique".to_owned()),
                (PersonalKind::Videos, "/home/ana/Vidéos".to_owned()),
                (
                    PersonalKind::Download,
                    "/home/ana/Téléchargements".to_owned()
                ),
                (PersonalKind::Desktop, "/home/ana/Bureau".to_owned()),
            ]
        );
    }

    #[test]
    fn a_folder_that_is_the_home_is_turned_off_and_the_last_line_wins() {
        let (folders, _) = parsed(
            "XDG_DESKTOP_DIR=\"$HOME/Bureau\"\n\
             XDG_DESKTOP_DIR=\"$HOME\"\n\
             XDG_MUSIC_DIR=\"$HOME/\"\n\
             XDG_PICTURES_DIR=\"$HOME\"\n\
             XDG_PICTURES_DIR=\"$HOME/Photos\"\n",
        );
        assert_eq!(
            folders,
            [(PersonalKind::Pictures, "/home/ana/Photos".to_owned())]
        );
    }

    #[test]
    fn a_shells_escapes_are_undone() {
        let (folders, _) = parsed(r#"XDG_DOCUMENTS_DIR="$HOME/Mes \"docs\" \$ \\ à \`moi\`""#);
        assert_eq!(
            folders,
            [(
                PersonalKind::Documents,
                r#"/home/ana/Mes "docs" $ \ à `moi`"#.to_owned()
            )]
        );
    }

    #[test]
    fn what_is_not_understood_is_ignored_and_counted() {
        let (folders, ignored) = parsed(
            "   \n\
             # XDG_MUSIC_DIR=\"$HOME/Musique\"\n\
             nothing here\n\
             XDG_DOCUMENTS_DIR=Documents\n\
             XDG_PICTURES_DIR=\"Images\"\n\
             XDG_VIDEOS_DIR=\"$HOMEVidéos\"\n\
             XDG_DESKTOP_DIR=\"$HOME/a\"b\"\n\
             XDG_DOWNLOAD_DIR = \"$HOME/Downloads\"\n",
        );
        assert_eq!(ignored, 5);
        assert_eq!(
            folders,
            [(PersonalKind::Download, "/home/ana/Downloads".to_owned())]
        );
    }

    #[test]
    fn a_path_of_any_bytes_is_kept() {
        let (folders, _) = parse_user_dirs(b"XDG_MUSIC_DIR=\"$HOME/caf\xe9\"", Path::new(HOME));
        assert_eq!(
            folders,
            [PersonalFolder {
                kind: PersonalKind::Music,
                path: Path::new(HOME).join(OsStr::from_bytes(b"caf\xe9")),
            }]
        );
    }

    #[test]
    fn the_file_is_read_never_through_a_link() {
        let folder = tempfile::tempdir().unwrap();
        let home = folder.path();
        let file = home.join(USER_DIRS);
        assert_eq!(read_personal_folders(&file, home).unwrap(), []);
        let real = home.join("real");
        std::fs::write(&real, "XDG_MUSIC_DIR=\"$HOME/Musique\"\n").unwrap();
        std::os::unix::fs::symlink(&real, &file).unwrap();
        assert!(read_personal_folders(&file, home).is_err());
        std::fs::remove_file(&file).unwrap();
        std::fs::rename(&real, &file).unwrap();
        assert_eq!(
            read_personal_folders(&file, home).unwrap(),
            [PersonalFolder {
                kind: PersonalKind::Music,
                path: home.join("Musique"),
            }]
        );
    }
}
