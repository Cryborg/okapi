//! The packages dpkg keeps (`/var/lib/dpkg`), read as files: which are
//! installed, their size, whether the system needs them, which installed
//! by hand, and which install a launcher. Never read while dpkg writes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead as _, BufReader};
use std::path::{Path, PathBuf};

/// Where dpkg keeps its state.
pub const DPKG_DIRECTORY: &str = "/var/lib/dpkg";
/// Where apt says which packages came in as dependencies.
pub const EXTENDED_STATES: &str = "/var/lib/apt/extended_states";
/// Where the launchers of the menu are installed.
const LAUNCHERS: &str = "/usr/share/applications/";
const LAUNCHER_SUFFIX: &str = ".desktop";
/// What dpkg says of a size: kibibytes.
const KIBIBYTE: u64 = 1024;
/// The states of a package whose files are all there.
const INSTALLED: [&str; 3] = ["installed", "triggers-pending", "triggers-awaited"];

/// One package installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebPackage {
    pub name: String,
    pub architecture: String,
    pub version: String,
    /// Its `Installed-Size`, an estimate, in bytes.
    pub installed_bytes: Option<u64>,
    /// Essential, protected or required: the system needs it.
    pub system: bool,
    /// A package that only gathers others (`Section: metapackages`).
    pub metapackage: bool,
}

impl DebPackage {
    /// Whether `key` (a `.list` file's name: `name` or `name:arch`) is its.
    #[must_use]
    pub fn is_listed_as(&self, key: &str) -> bool {
        match key.split_once(':') {
            Some((name, architecture)) => name == self.name && architecture == self.architecture,
            None => key == self.name,
        }
    }
}

/// What a `status` file says: the packages installed, and how many of its
/// paragraphs could not be read.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub installed: Vec<DebPackage>,
    pub unreadable: usize,
}

/// The fields of one paragraph, a field continued on the next lines kept to
/// its first.
fn fields(paragraph: &str) -> HashMap<&str, &str> {
    paragraph
        .lines()
        .filter(|line| !line.starts_with([' ', '\t']))
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect()
}

fn paragraphs(text: &str) -> impl Iterator<Item = &str> {
    text.split("\n\n")
        .filter(|paragraph| !paragraph.trim().is_empty())
}

/// The packages `text` (a dpkg `status` file) says are installed.
#[must_use]
pub fn parse_status(text: &str) -> Status {
    let mut status = Status::default();
    for (index, paragraph) in paragraphs(text).enumerate() {
        let fields = fields(paragraph);
        let (Some(name), Some(state)) = (fields.get("Package"), fields.get("Status")) else {
            tracing::debug!(
                paragraph = index,
                "dpkg paragraph without its package or state"
            );
            status.unreadable += 1;
            continue;
        };
        // `want flag state`: only the state says it is there, its triggers
        // run or not.
        if !state
            .split_whitespace()
            .nth(2)
            .is_some_and(|state| INSTALLED.contains(&state))
        {
            continue;
        }
        let installed_bytes = fields.get("Installed-Size").and_then(|size| {
            let bytes = size
                .parse::<u64>()
                .ok()
                .map(|kib| kib.saturating_mul(KIBIBYTE));
            if bytes.is_none() {
                tracing::debug!(package = name, size, "dpkg size unreadable");
            }
            bytes
        });
        let yes = |field| fields.get(field) == Some(&"yes");
        status.installed.push(DebPackage {
            name: (*name).to_owned(),
            architecture: fields
                .get("Architecture")
                .copied()
                .unwrap_or_default()
                .to_owned(),
            version: fields
                .get("Version")
                .copied()
                .unwrap_or_default()
                .to_owned(),
            installed_bytes,
            system: yes("Essential")
                || yes("Protected")
                || fields.get("Priority") == Some(&"required"),
            // An archive area may come first (`universe/metapackages`).
            metapackage: fields
                .get("Section")
                .is_some_and(|section| section.rsplit('/').next() == Some("metapackages")),
        });
    }
    status
}

/// The packages `text` (apt's `extended_states`) says came in as
/// dependencies, by name and architecture.
#[must_use]
pub fn parse_automatic(text: &str) -> HashSet<(String, String)> {
    paragraphs(text)
        .map(fields)
        .filter(|fields| fields.get("Auto-Installed") == Some(&"1"))
        .filter_map(|fields| {
            Some((
                (*fields.get("Package")?).to_owned(),
                (*fields.get("Architecture")?).to_owned(),
            ))
        })
        .collect()
}

/// How many updates dpkg has not yet folded into `status`: some means it is
/// writing, or was cut short, and `status` alone is not the truth.
///
/// # Errors
///
/// The system's error reading `updates/`.
pub fn pending_updates(dpkg: &Path) -> io::Result<usize> {
    let mut pending = 0;
    for entry in fs::read_dir(dpkg.join("updates"))? {
        // dpkg names its updates by number; `tmp.i` is its work file.
        if entry?
            .file_name()
            .to_str()
            .is_some_and(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
        {
            pending += 1;
        }
    }
    Ok(pending)
}

/// The launchers each package installs, by the name of its `.list` file
/// (`name` or `name:arch`), from the lists of `dpkg`'s `info/`; and how
/// many lists could not be read.
///
/// # Errors
///
/// The system's error listing `info/`.
pub fn launchers(dpkg: &Path) -> io::Result<(Vec<(String, PathBuf)>, usize)> {
    let mut found = Vec::new();
    let mut unreadable = 0;
    for entry in fs::read_dir(dpkg.join("info"))? {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                tracing::debug!(%error, "dpkg list not listed");
                unreadable += 1;
                continue;
            }
        };
        let Some(key) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".list"))
        else {
            continue;
        };
        match listed_launchers(&path) {
            Ok(paths) => found.extend(paths.into_iter().map(|launcher| (key.to_owned(), launcher))),
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "dpkg list unreadable");
                unreadable += 1;
            }
        }
    }
    Ok((found, unreadable))
}

/// The launchers of the menu among the files `list` names.
fn listed_launchers(list: &Path) -> io::Result<Vec<PathBuf>> {
    let mut launchers = Vec::new();
    for line in BufReader::new(fs::File::open(list)?).lines() {
        let line = line?;
        if line.starts_with(LAUNCHERS) && line.ends_with(LAUNCHER_SUFFIX) {
            launchers.push(PathBuf::from(line));
        }
    }
    Ok(launchers)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "\
Package: gimp
Status: install ok installed
Priority: optional
Section: graphics
Installed-Size: 3
Architecture: amd64
Version: 3.0.4-1
Description: GNU Image Manipulation Program
 A long description,
 Status: not a field.

Package: removed-app
Status: deinstall ok config-files
Architecture: amd64
Version: 1.0
Installed-Size: 50

Package: base-files
Essential: yes
Status: install ok installed
Priority: required
Architecture: amd64
Version: 13
Installed-Size: 1

Package: half
Status: install reinstreq half-configured
Architecture: amd64
Installed-Size: 7

Package: libfoo
Status: install ok triggers-pending
Architecture: i386
Version: 2
Section: libs

Package: ubuntu-desktop-minimal
Status: hold ok installed
Section: universe/metapackages
Architecture: amd64
Version: 1.5
Installed-Size: 9

Architecture: amd64
Status: install ok installed
";

    #[test]
    fn only_what_is_installed_is_read_with_its_size_in_bytes() {
        let status = parse_status(STATUS);
        let names: Vec<&str> = status
            .installed
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["gimp", "base-files", "libfoo", "ubuntu-desktop-minimal"]
        );
        assert_eq!(status.unreadable, 1, "the paragraph without its package");
        let gimp = &status.installed[0];
        assert_eq!(gimp.installed_bytes, Some(3 * 1024));
        assert_eq!(gimp.version, "3.0.4-1");
        assert!(!gimp.system && !gimp.metapackage);
        assert!(status.installed[1].system, "essential and required");
        let libfoo = &status.installed[2];
        assert_eq!(
            (libfoo.installed_bytes, libfoo.architecture.as_str()),
            (None, "i386")
        );
        assert!(status.installed[3].metapackage, "in an archive area");
    }

    #[test]
    fn a_package_is_known_by_its_list_file() {
        let status = parse_status(STATUS);
        let libfoo = &status.installed[2];
        assert!(libfoo.is_listed_as("libfoo"));
        assert!(libfoo.is_listed_as("libfoo:i386"));
        assert!(!libfoo.is_listed_as("libfoo:amd64"));
        assert!(!libfoo.is_listed_as("libfoo2"));
    }

    #[test]
    fn the_packages_installed_as_dependencies_are_known() {
        let automatic = parse_automatic(
            "Package: libfoo\nArchitecture: i386\nAuto-Installed: 1\n\n\
             Package: gimp\nArchitecture: amd64\nAuto-Installed: 0\n\n\
             Package: nameless-arch\nAuto-Installed: 1\n",
        );
        assert_eq!(
            automatic,
            HashSet::from([("libfoo".to_owned(), "i386".to_owned())])
        );
    }

    #[test]
    fn dpkg_writing_is_seen_in_its_updates() {
        let dpkg = tempfile::tempdir().unwrap();
        let updates = dpkg.path().join("updates");
        fs::create_dir(&updates).unwrap();
        fs::write(updates.join("tmp.i"), "").unwrap();
        assert_eq!(
            pending_updates(dpkg.path()).unwrap(),
            0,
            "its work file alone"
        );
        fs::write(updates.join("0003"), "").unwrap();
        assert_eq!(pending_updates(dpkg.path()).unwrap(), 1);
    }

    #[test]
    fn the_launchers_of_each_package_are_found_in_its_list() {
        let dpkg = tempfile::tempdir().unwrap();
        let info = dpkg.path().join("info");
        fs::create_dir(&info).unwrap();
        fs::write(
            info.join("gimp.list"),
            "/.\n/usr/bin/gimp\n/usr/share/applications\n/usr/share/applications/gimp.desktop\n\
             /usr/share/doc/gimp.desktop\n",
        )
        .unwrap();
        fs::write(
            info.join("libfoo:i386.list"),
            "/usr/share/applications/foo.desktop\n",
        )
        .unwrap();
        fs::write(
            info.join("gimp.md5sums"),
            "/usr/share/applications/x.desktop\n",
        )
        .unwrap();
        fs::create_dir(info.join("broken.list")).unwrap();
        let (mut found, unreadable) = launchers(dpkg.path()).unwrap();
        found.sort();
        assert_eq!(
            found,
            [
                (
                    "gimp".to_owned(),
                    PathBuf::from("/usr/share/applications/gimp.desktop")
                ),
                (
                    "libfoo:i386".to_owned(),
                    PathBuf::from("/usr/share/applications/foo.desktop")
                ),
            ]
        );
        assert_eq!(unreadable, 1);
    }
}
