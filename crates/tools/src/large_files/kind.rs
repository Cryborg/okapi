//! What a file is, guessed from its extension alone, whatever its case:
//! never by reading it.

use std::path::Path;

/// What a large file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileKind {
    Video,
    Image,
    Music,
    Document,
    Archive,
    /// A disk or a virtual machine's disk (`.iso`, `.vdi`...).
    DiskImage,
    /// No extension known.
    Other,
}

/// The extensions of each kind but [`FileKind::Other`], in lower case, each
/// of one kind only.
const EXTENSIONS: &[(FileKind, &[&str])] = &[
    (
        FileKind::Video,
        &[
            "3gp", "avi", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "mts", "ogv",
            "vob", "webm", "wmv",
        ],
    ),
    (
        FileKind::Image,
        &[
            "arw", "avif", "bmp", "cr2", "cr3", "dng", "gif", "heic", "heif", "jpeg", "jpg", "nef",
            "orf", "png", "psd", "raw", "rw2", "svg", "tif", "tiff", "webp", "xcf",
        ],
    ),
    (
        FileKind::Music,
        &[
            "aac", "aif", "aiff", "ape", "flac", "m4a", "mp3", "oga", "ogg", "opus", "wav", "wma",
        ],
    ),
    (
        FileKind::Document,
        &[
            "csv", "doc", "docx", "epub", "md", "odp", "ods", "odt", "pdf", "ppt", "pptx", "rtf",
            "txt", "xls", "xlsx",
        ],
    ),
    (
        FileKind::Archive,
        &[
            "7z", "bz2", "cab", "deb", "gz", "jar", "lz", "lzma", "rar", "rpm", "tar", "tgz", "xz",
            "zip", "zst",
        ],
    ),
    (
        FileKind::DiskImage,
        &[
            "dmg", "img", "iso", "ova", "qcow2", "vdi", "vhd", "vhdx", "vmdk", "wim",
        ],
    ),
];

impl FileKind {
    /// Every kind, in the order the screen lists them.
    pub const ALL: [Self; 7] = [
        Self::Video,
        Self::Image,
        Self::Music,
        Self::Document,
        Self::Archive,
        Self::DiskImage,
        Self::Other,
    ];

    /// The kind of the file at `path`, by its last extension (`.tar.gz` is
    /// an archive by its `.gz`).
    #[must_use]
    pub fn of(path: &Path) -> Self {
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            return Self::Other;
        };
        EXTENSIONS
            .iter()
            .find(|(_, extensions)| {
                extensions
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(extension))
            })
            .map_or(Self::Other, |&(kind, _)| kind)
    }

    /// A stable key, for traces.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Image => "image",
            Self::Music => "music",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::DiskImage => "diskImage",
            Self::Other => "other",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn kind(name: &str) -> FileKind {
        FileKind::of(Path::new(name))
    }

    #[test]
    fn a_file_s_kind_is_its_last_extension_whatever_its_case() {
        assert_eq!(kind("/home/ana/Vidéos/film.mkv"), FileKind::Video);
        assert_eq!(kind("FILM.MKV"), FileKind::Video);
        assert_eq!(kind("Photo.JpEg"), FileKind::Image);
        assert_eq!(kind("album.flac"), FileKind::Music);
        assert_eq!(kind("thesis.pdf"), FileKind::Document);
        assert_eq!(kind("backup.tar.gz"), FileKind::Archive);
        assert_eq!(kind("ubuntu-26.04-desktop-amd64.iso"), FileKind::DiskImage);
        assert_eq!(kind("windows.vdi"), FileKind::DiskImage);
    }

    #[test]
    fn a_file_without_an_extension_known_is_other() {
        assert_eq!(kind("core"), FileKind::Other);
        assert_eq!(kind("data.bin"), FileKind::Other);
        assert_eq!(kind(".mkv"), FileKind::Other, "a hidden name, no extension");
        assert_eq!(
            kind("film.mkv.part"),
            FileKind::Other,
            "a download under way"
        );
    }

    #[test]
    fn each_extension_is_of_one_kind_in_lower_case() {
        let mut met = HashSet::new();
        for (kind, extensions) in EXTENSIONS {
            assert_ne!(*kind, FileKind::Other);
            for extension in *extensions {
                assert_eq!(*extension, extension.to_lowercase(), "{extension}");
                assert!(met.insert(*extension), "{extension} twice");
            }
        }
        let kinds: HashSet<FileKind> = EXTENSIONS.iter().map(|&(kind, _)| kind).collect();
        assert_eq!(kinds.len(), FileKind::ALL.len() - 1, "every kind but other");
    }
}
