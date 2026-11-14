//! The applications installed (deb, snap, flatpak) and the space each takes:
//! the application itself, its data, the old versions it keeps. Read from
//! files and from the services that install them, never by running a
//! command; their icons turned into pixels with bounds ([`icons`]); their
//! folders measured without following a link ([`measure`]).

pub mod dpkg;
pub mod flatpak;
pub mod icons;
pub mod measure;
pub mod snap;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use okapi_rules::Language;

use icons::IconRoots;

/// Who installed an application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Deb,
    Snap,
    Flatpak,
}

/// How a deb package came.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Asked for by the user.
    ByUser,
    /// Brought by another package.
    AsDependency,
    /// One the system needs.
    WithSystem,
}

/// One application of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub source: Source,
    /// Unique within its source: `name:arch` for a deb, the snap's name,
    /// the flatpak's id.
    pub id: String,
    /// The name its source knows it by (package, snap, flatpak id).
    pub package: String,
    /// As the menu shows it.
    pub name: String,
    pub version: String,
    pub publisher: Option<String>,
    pub origin: Option<Origin>,
    /// The file of its icon, found in the system's folders.
    pub icon: Option<PathBuf>,
    /// What it takes, when its source says; otherwise `deployment` is
    /// measured.
    pub app_bytes: Option<u64>,
    pub deployment: Option<PathBuf>,
    /// What the versions kept besides the one in use take, and how many.
    pub old_bytes: u64,
    pub old_versions: usize,
    /// The folder of its data in the home folder, when one is surely its.
    pub data: Option<PathBuf>,
    /// The system needs it: never offered to uninstall.
    pub system: bool,
}

/// What a source has besides the applications of the menu: libraries,
/// runtimes, tools without a launcher.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rest {
    pub count: usize,
    pub bytes: u64,
}

/// What one source gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceReading {
    Read {
        apps: Vec<App>,
        rest: Rest,
    },
    /// Not on this system.
    Absent,
    /// Its manager is writing (dpkg mid-way): read again later.
    Busy,
    Failed(String),
}

/// How the launchers are read: in the session's language, shown on its
/// desktops, their icons in the system's folders.
#[derive(Debug, Clone)]
pub struct Reading {
    pub language: Language,
    /// `XDG_CURRENT_DESKTOP`, most specific first.
    pub desktops: Vec<String>,
    pub icons: IconRoots,
}

impl Reading {
    /// This session's.
    #[must_use]
    pub fn session(language: Language) -> Self {
        Self {
            language,
            desktops: okapi_platform::desktop_session().desktops,
            icons: IconRoots::system(),
        }
    }

    /// The launcher at `path`, if the menu shows it: its name and icon.
    fn launcher(&self, path: &Path) -> Option<(String, Option<PathBuf>)> {
        let entry = okapi_platform::desktop_entry(path, self.language)
            .inspect_err(
                |error| tracing::debug!(path = %path.display(), %error, "launcher unreadable"),
            )
            .ok()?;
        if !entry.shown_in(&self.desktops) {
            return None;
        }
        let name = entry.name?;
        let icon = entry
            .icon
            .as_deref()
            .and_then(|icon| icons::find(icon, &self.icons));
        Some((name, icon))
    }
}

/// Where dpkg and apt keep their state, and the root launchers are read
/// under (`/`, or a test's folder).
#[derive(Debug, Clone)]
pub struct DebPlaces {
    pub dpkg: PathBuf,
    pub extended_states: PathBuf,
    pub root: PathBuf,
}

impl DebPlaces {
    /// The system's.
    #[must_use]
    pub fn system() -> Self {
        Self {
            dpkg: PathBuf::from(dpkg::DPKG_DIRECTORY),
            extended_states: PathBuf::from(dpkg::EXTENDED_STATES),
            root: PathBuf::from("/"),
        }
    }
}

/// The deb applications of the menu, and the packages besides.
#[must_use]
pub fn read_debs(places: &DebPlaces, reading: &Reading) -> SourceReading {
    let _span = tracing::info_span!("debs", dpkg = %places.dpkg.display()).entered();
    match dpkg::pending_updates(&places.dpkg) {
        Ok(0) => {}
        Ok(pending) => {
            tracing::info!(pending, "dpkg is writing: packages read later");
            return SourceReading::Busy;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("no dpkg");
            return SourceReading::Absent;
        }
        Err(error) => {
            tracing::warn!(%error, "dpkg updates unreadable");
            return SourceReading::Failed(error.to_string());
        }
    }
    let status = match std::fs::read_to_string(places.dpkg.join("status")) {
        Ok(text) => dpkg::parse_status(&text),
        Err(error) => {
            tracing::warn!(%error, "dpkg status unreadable");
            return SourceReading::Failed(error.to_string());
        }
    };
    let automatic = std::fs::read_to_string(&places.extended_states)
        .map(|text| dpkg::parse_automatic(&text))
        .inspect_err(|error| tracing::info!(%error, "apt's extended states unread: every package said asked for"))
        .unwrap_or_default();
    let (launchers, unreadable_lists) = match dpkg::launchers(&places.dpkg) {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(%error, "dpkg lists unreadable");
            return SourceReading::Failed(error.to_string());
        }
    };
    let mut by_name: HashMap<&str, Vec<(&str, &Path)>> = HashMap::new();
    for (key, path) in &launchers {
        let name = key.split_once(':').map_or(key.as_str(), |(name, _)| name);
        by_name.entry(name).or_default().push((key, path));
    }
    let mut apps = Vec::new();
    let mut rest = Rest::default();
    for package in &status.installed {
        let mut own: Vec<&Path> = by_name
            .get(package.name.as_str())
            .into_iter()
            .flatten()
            .filter(|(key, _)| package.is_listed_as(key))
            .map(|(_, path)| *path)
            .collect();
        // The launcher named as the package first, then by path.
        own.sort_by_key(|path| (path.file_stem() != Some(package.name.as_ref()), *path));
        let shown = own.iter().find_map(|path| {
            reading.launcher(&places.root.join(path.strip_prefix("/").unwrap_or(path)))
        });
        let Some((name, icon)) = shown else {
            rest.count += 1;
            rest.bytes = rest
                .bytes
                .saturating_add(package.installed_bytes.unwrap_or(0));
            continue;
        };
        let origin = if package.system {
            Origin::WithSystem
        } else if automatic.contains(&(package.name.clone(), package.architecture.clone())) {
            Origin::AsDependency
        } else {
            Origin::ByUser
        };
        apps.push(App {
            source: Source::Deb,
            id: format!("{}:{}", package.name, package.architecture),
            package: package.name.clone(),
            name,
            version: package.version.clone(),
            publisher: None,
            origin: Some(origin),
            icon,
            app_bytes: Some(package.installed_bytes.unwrap_or(0)),
            deployment: None,
            old_bytes: 0,
            old_versions: 0,
            data: None,
            system: package.system,
        });
    }
    tracing::info!(
        installed = status.installed.len(),
        unreadable = status.unreadable,
        unreadable_lists,
        launchers = launchers.len(),
        apps = apps.len(),
        rest = rest.count,
        "debs read"
    );
    SourceReading::Read { apps, rest }
}

/// Whether `name` may name a file of a folder: no path, no `.` nor `..`,
/// only the characters an identifier of a package manager uses.
fn names_a_folder(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
}

#[cfg(test)]
mod tests;
