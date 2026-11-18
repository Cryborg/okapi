//! The applications installed (deb, snap, flatpak) and the space each takes:
//! the application itself, its data, the old versions it keeps. Read from
//! files and from the services that install them, never by running a
//! command; their icons turned into pixels with bounds ([`icons`]); their
//! folders measured without following a link ([`measure`]).

pub mod dpkg;
pub mod flatpak;
pub mod icons;
pub mod leftovers;
pub mod measure;
#[cfg(target_os = "linux")]
pub mod old_versions;
pub mod snap;
#[cfg(target_os = "linux")]
pub mod snapshots;
#[cfg(target_os = "linux")]
pub mod system;
pub mod uninstall;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use okapi_rules::Language;

use icons::{IconPixels, IconRoots};
use measure::Measured;

/// Who installed an application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

/// What an application itself takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppSize {
    /// As its source says.
    Known(u64),
    /// Its source does not say.
    Unknown,
    /// Measured from its deployment (a flatpak's).
    ToMeasure(PathBuf),
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
    /// The other names its launcher goes by: its file's name, its windows'
    /// class, its name in no language.
    pub aliases: Vec<String>,
    pub version: String,
    pub publisher: Option<String>,
    /// How a deb came; `None` for another source, or when apt cannot say.
    pub origin: Option<Origin>,
    /// The file of its icon, found in the system's folders.
    pub icon: Option<PathBuf>,
    pub size: AppSize,
    /// What the versions kept besides the one in use take, and how many.
    pub old_bytes: u64,
    pub old_versions: usize,
    /// The folder of its data in the home folder, when one is surely its.
    pub data: Option<PathBuf>,
    /// The system needs it: never offered to uninstall.
    pub system: bool,
    /// How it is uninstalled, when Okapi can (never a flatpak of the user's
    /// installation).
    pub target: Option<uninstall::Target>,
}

impl App {
    /// What names it, whatever is shown.
    #[must_use]
    pub fn key(&self) -> AppKey {
        AppKey {
            source: self.source,
            id: self.id.clone(),
        }
    }
}

/// One application, by its source and its identifier there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppKey {
    pub source: Source,
    pub id: String,
}

/// Which folder of an application was measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Its deployment (a flatpak's).
    App,
    Data,
}

/// What the reading sends as it goes: a source's applications first, then
/// their icons, then what their folders take. The reading is over when
/// every sender is gone.
#[derive(Debug)]
pub enum Arrival {
    Source(Source, SourceReading),
    Icon(AppKey, IconPixels),
    Measured(AppKey, Part, Measured),
}

/// How a source is read.
pub type Reader = Box<dyn FnOnce() -> SourceReading + Send>;

/// Reads each source on a thread of its own, so that a slow one (snapd
/// starting) never holds the others, and sends what each finds to `sink`
/// as it comes, until `stop` is set or nobody listens any more.
pub fn read_all(readers: Vec<(Source, Reader)>, sink: &Sender<Arrival>, stop: &Arc<AtomicBool>) {
    for (source, reader) in readers {
        let thread_sink = sink.clone();
        let thread_stop = Arc::clone(stop);
        let spawned = std::thread::Builder::new()
            .name(format!("okapi-apps-{source:?}"))
            .spawn(move || read_one(source, reader, &thread_sink, &thread_stop));
        if let Err(error) = spawned {
            tracing::error!(?source, %error, "no thread to read a source");
            let _ = sink.send(Arrival::Source(
                source,
                SourceReading::Failed(error.to_string()),
            ));
        }
    }
}

fn read_one(source: Source, reader: Reader, sink: &Sender<Arrival>, stop: &AtomicBool) {
    let started = std::time::Instant::now();
    let reading = reader();
    let apps = match &reading {
        SourceReading::Read { apps, .. } => apps.clone(),
        _ => Vec::new(),
    };
    // A send refused: nobody listens any more, the page went away.
    if sink.send(Arrival::Source(source, reading)).is_err() {
        return;
    }
    let (mut icons, mut no_icons, mut folders_measured) = (0_usize, 0_usize, 0_usize);
    for app in &apps {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(path) = app.icon.as_deref() else {
            continue;
        };
        match icons::load(path) {
            Ok(pixels) => {
                icons += 1;
                if sink.send(Arrival::Icon(app.key(), pixels)).is_err() {
                    return;
                }
            }
            Err(_) => no_icons += 1,
        }
    }
    let folders = apps.iter().flat_map(|app| {
        let deployment = match &app.size {
            AppSize::ToMeasure(folder) => Some(folder),
            AppSize::Known(_) | AppSize::Unknown => None,
        };
        [(Part::App, deployment), (Part::Data, app.data.as_ref())]
            .into_iter()
            .filter_map(move |(part, folder)| folder.map(|folder| (app.key(), part, folder)))
    });
    for (key, part, folder) in folders {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let measured = measure::measure(folder, stop);
        folders_measured += 1;
        if sink.send(Arrival::Measured(key, part, measured)).is_err() {
            return;
        }
    }
    tracing::info!(?source, apps = apps.len(), icons, no_icons, folders_measured, stopped = stop.load(Ordering::Relaxed), elapsed = ?started.elapsed(), "source read");
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
        /// A part of it could not be read (a flatpak installation): some
        /// applications may be missing.
        partial: bool,
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

    /// The launcher at `path`, if the menu shows it: its name, its icon
    /// looked for in `roots`, and the other names it goes by.
    fn launcher(&self, path: &Path, roots: &IconRoots) -> Option<Launcher> {
        let entry = okapi_platform::desktop_entry(path, self.language)
            .inspect_err(
                |error| tracing::debug!(path = %path.display(), %error, "launcher unreadable"),
            )
            .ok()?;
        if !entry.shown_in(&self.desktops) {
            tracing::debug!(path = %path.display(), "launcher kept from the menu");
            return None;
        }
        let Some(name) = entry.name else {
            tracing::debug!(path = %path.display(), "launcher without a name");
            return None;
        };
        let icon = entry
            .icon
            .as_deref()
            .and_then(|icon| icons::find(icon, roots));
        let mut aliases: Vec<String> = Vec::new();
        let file = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned);
        for alias in [file, entry.wm_class, entry.plain_name]
            .into_iter()
            .flatten()
        {
            if alias != name && !aliases.contains(&alias) {
                aliases.push(alias);
            }
        }
        Some(Launcher {
            name,
            icon,
            aliases,
        })
    }
}

/// A launcher the menu shows.
struct Launcher {
    name: String,
    icon: Option<PathBuf>,
    /// Its file's name, its windows' class, its name in no language.
    aliases: Vec<String>,
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
            dpkg: PathBuf::from(crate::packages::dpkg::DPKG_DIRECTORY),
            extended_states: PathBuf::from(dpkg::EXTENDED_STATES),
            root: PathBuf::from("/"),
        }
    }
}

/// What dpkg and apt say now.
struct DebState {
    status: dpkg::Status,
    /// Which came as dependencies: `None` when apt cannot say.
    automatic: Option<dpkg::Automatic>,
    /// The launchers of each package, by its list's name.
    launchers: Vec<(String, PathBuf)>,
    unreadable_lists: usize,
}

/// What dpkg and apt say now, or what the source is when they cannot be
/// read.
fn deb_state(places: &DebPlaces) -> Result<DebState, SourceReading> {
    match crate::packages::dpkg::pending_updates(&places.dpkg) {
        Ok(0) => {}
        Ok(pending) => {
            tracing::info!(pending, "dpkg is writing: packages read later");
            return Err(SourceReading::Busy);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("no dpkg");
            return Err(SourceReading::Absent);
        }
        Err(error) => {
            tracing::warn!(%error, "dpkg updates unreadable");
            return Err(SourceReading::Failed(error.to_string()));
        }
    }
    let status = match std::fs::read_to_string(places.dpkg.join("status")) {
        Ok(text) => dpkg::parse_status(&text),
        Err(error) => {
            tracing::warn!(%error, "dpkg status unreadable");
            return Err(SourceReading::Failed(error.to_string()));
        }
    };
    // Unknown, the origin is never said; absent, nothing came as a
    // dependency.
    let automatic = match std::fs::read_to_string(&places.extended_states) {
        Ok(text) => Some(dpkg::parse_automatic(&text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("no apt extended states: every package asked for");
            Some(dpkg::Automatic::default())
        }
        Err(error) => {
            tracing::warn!(%error, "apt's extended states unreadable: origins unknown");
            None
        }
    };
    let (launchers, unreadable_lists) = dpkg::launchers(&places.dpkg).map_err(|error| {
        tracing::warn!(%error, "dpkg lists unreadable");
        SourceReading::Failed(error.to_string())
    })?;
    Ok(DebState {
        status,
        automatic,
        launchers,
        unreadable_lists,
    })
}

/// The deb applications of the menu, and the packages besides.
#[must_use]
pub fn read_debs(places: &DebPlaces, reading: &Reading) -> SourceReading {
    let _span = tracing::info_span!("debs", dpkg = %places.dpkg.display()).entered();
    let DebState {
        status,
        automatic,
        launchers,
        unreadable_lists,
    } = match deb_state(places) {
        Ok(state) => state,
        Err(reading) => return reading,
    };
    let mut by_name: HashMap<&str, Vec<(&str, &Path)>> = HashMap::new();
    for (key, path) in &launchers {
        let name = key.split_once(':').map_or(key.as_str(), |(name, _)| name);
        by_name.entry(name).or_default().push((key, path));
    }
    let mut apps = Vec::new();
    let mut rest = Rest::default();
    let mut without_size = 0_usize;
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
            reading.launcher(
                &places.root.join(path.strip_prefix("/").unwrap_or(path)),
                &reading.icons,
            )
        });
        let Some(Launcher {
            name,
            icon,
            aliases,
        }) = shown
        else {
            rest.count += 1;
            match package.installed_bytes {
                Some(bytes) => rest.bytes = rest.bytes.saturating_add(bytes),
                None => without_size += 1,
            }
            continue;
        };
        let origin = if package.system {
            Some(Origin::WithSystem)
        } else {
            automatic.as_ref().map(|automatic| {
                if automatic.brought(package) {
                    Origin::AsDependency
                } else {
                    Origin::ByUser
                }
            })
        };
        apps.push(App {
            source: Source::Deb,
            id: format!("{}:{}", package.name, package.architecture),
            package: package.name.clone(),
            name,
            aliases,
            version: package.version.clone(),
            publisher: None,
            origin,
            icon,
            size: package
                .installed_bytes
                .map_or(AppSize::Unknown, AppSize::Known),
            old_bytes: 0,
            old_versions: 0,
            data: None,
            system: package.system,
            target: Some(uninstall::Target::Deb {
                package: package.name.clone(),
                architecture: package.architecture.clone(),
            }),
        });
    }
    tracing::info!(
        installed = status.installed.len(),
        unreadable = status.unreadable,
        unreadable_lists,
        launchers = launchers.len(),
        apps = apps.len(),
        rest = rest.count,
        rest_without_size = without_size,
        "debs read"
    );
    SourceReading::Read {
        apps,
        rest,
        partial: false,
    }
}

/// Whether `name` may name a file of a folder: no path, no `.` nor `..`,
/// only the characters an identifier of a package manager uses.
fn names_a_folder(name: &str) -> bool {
    okapi_platform::names_a_file(name)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
}

#[cfg(test)]
mod tests;
