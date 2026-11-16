//! What is installed, as the leftovers need it: every snap, every flatpak id
//! of every installation (with the ids a flatpak was renamed from, and those
//! running now), and the packages removed whose settings stay. A source not
//! read whole is unread: nothing it might own is ever proposed. Okapi
//! confined (a container, a sandbox) reads none: what it sees is not the
//! system that filled the home folder.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use okapi_rules::Language;

use crate::apps::flatpak::{Installation, app_ids, deployments};
use crate::apps::names_a_folder;

/// Where snapd keeps the files of the snaps installed.
pub const SNAP_FILES: &str = "/var/lib/snapd/snaps";
const SNAP_FILE_SUFFIX: &str = ".snap";
/// Where flatpak says, under `$XDG_RUNTIME_DIR`, which instances run.
const RUNNING: &str = ".flatpak";
const INSTANCE_INFO: &str = "info";
const INFO_LIMIT: usize = 64 * 1024;
const APPLICATION_GROUP: &str = "[Application]";
const KEY_NAME: &str = "name";
/// Where a deployment exports its launchers.
const EXPORTED_LAUNCHERS: &str = "export/share/applications";
const LAUNCHER_SUFFIX: &str = ".desktop";

/// The names a source has installed, or why it could not be read whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Known {
    Read(BTreeSet<String>),
    Unread(String),
}

impl Known {
    /// Whether `name` is installed; `None` when the source was not read.
    #[must_use]
    pub fn bears(&self, name: &str) -> Option<bool> {
        match self {
            Self::Read(names) => Some(names.contains(name)),
            Self::Unread(_) => None,
        }
    }
}

/// What is installed now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub snaps: Known,
    pub flatpaks: Known,
}

/// Where what is installed is read.
#[derive(Debug, Clone)]
pub struct InstalledPlaces {
    pub snap_files: PathBuf,
    pub installations: Vec<Installation>,
    /// Where running flatpaks are said: `$XDG_RUNTIME_DIR`, or the folder
    /// flatpak falls back on without it. Without either, flatpaks are
    /// unread.
    pub runtime: Option<PathBuf>,
    /// Why Okapi may not see the system that filled the home folder (a
    /// container, a sandbox): then nothing installed is known, never none.
    pub(crate) confined: Option<String>,
}

impl InstalledPlaces {
    /// The places given, Okapi's confinement read now
    /// ([`okapi_platform::confinement`]).
    #[must_use]
    pub fn new(
        snap_files: PathBuf,
        installations: Vec<Installation>,
        runtime: Option<PathBuf>,
    ) -> Self {
        Self {
            snap_files,
            installations,
            runtime,
            confined: okapi_platform::confinement(),
        }
    }
}

/// What is installed on this system now: the snaps as snapd lists them, the
/// flatpaks as `places` hold them.
#[must_use]
pub fn installed_now(places: &InstalledPlaces) -> Installed {
    installed_reading(places, || snaps_now(&places.snap_files))
}

/// The snaps installed on this system now, as snapd lists them; when it
/// cannot be asked, none only if no snap file is in `snap_files`.
#[must_use]
pub fn snaps_now(snap_files: &Path) -> Known {
    let listed = crate::packages::snapd::Snapd::system()
        .snaps()
        .map_err(|error| error.to_string())
        .and_then(|listed| snap_names(&listed));
    snaps(listed, snap_files)
}

/// What is installed, the snaps being `snaps` (read once by [`snaps_now`]
/// for whoever needs them), the flatpaks as `places` hold them.
#[must_use]
pub fn installed_with(places: &InstalledPlaces, snaps: Known) -> Installed {
    installed_reading(places, || snaps)
}

/// What is installed, the snaps read by `snaps`; nothing read, and both
/// unread, when Okapi runs confined: what it sees is not the system that
/// filled the home folder (none there would propose every application's
/// data).
fn installed_reading(places: &InstalledPlaces, snaps: impl FnOnce() -> Known) -> Installed {
    if let Some(mark) = &places.confined {
        tracing::warn!(mark, "Okapi runs confined: what is installed unread");
        let unread = Known::Unread(format!("Okapi runs confined ({mark})"));
        return Installed {
            snaps: unread.clone(),
            flatpaks: unread,
        };
    }
    Installed {
        snaps: snaps(),
        flatpaks: flatpaks(&places.installations, places.runtime.as_deref()),
    }
}

/// The names of the snaps `listed` (snapd's `/v2/snaps` result), every
/// type and revision.
///
/// # Errors
///
/// What `listed` is when it is no list of snaps.
pub(crate) fn snap_names(listed: &serde_json::Value) -> Result<Vec<String>, String> {
    let snaps = listed
        .as_array()
        .ok_or_else(|| "snapd's list is no list".to_owned())?;
    snaps
        .iter()
        .map(|snap| {
            snap.get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "a snap without its name".to_owned())
        })
        .collect()
}

/// The snaps installed: those snapd lists (`listed`); when it could not be
/// asked, none only if no snap file is there.
#[must_use]
pub(crate) fn snaps(listed: Result<Vec<String>, String>, snap_files: &Path) -> Known {
    let why = match listed {
        Ok(names) => {
            tracing::info!(snaps = names.len(), "snaps installed read");
            return Known::Read(names.into_iter().collect());
        }
        Err(why) => why,
    };
    match std::fs::read_dir(snap_files) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match crate::apps::flatpak::missing_for_sure(snap_files) {
                Ok(()) => {
                    tracing::info!(%why, "snapd not asked, no snap file: no snap installed");
                    Known::Read(BTreeSet::new())
                }
                Err(unsure) => {
                    tracing::warn!(%why, unsure, "snapd not asked, its files unsure: snaps unread");
                    Known::Unread(why)
                }
            }
        }
        Err(error) => {
            tracing::warn!(%why, %error, "snapd not asked, its files unreadable: snaps unread");
            Known::Unread(why)
        }
        Ok(entries) => {
            let mut any = false;
            for entry in entries {
                match entry {
                    Ok(entry)
                        if entry
                            .file_name()
                            .to_string_lossy()
                            .ends_with(SNAP_FILE_SUFFIX) =>
                    {
                        any = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%why, %error, "snapd not asked, its files unreadable: snaps unread");
                        return Known::Unread(why);
                    }
                }
            }
            if any {
                tracing::warn!(%why, "snapd not asked while snaps are there: snaps unread");
                Known::Unread(why)
            } else {
                Known::Read(BTreeSet::new())
            }
        }
    }
}

/// The flatpak ids installed in `installations`, with those renamed from and
/// those running under `runtime`.
#[must_use]
pub(crate) fn flatpaks(installations: &[Installation], runtime: Option<&Path>) -> Known {
    let mut ids = BTreeSet::new();
    for installation in installations {
        let found = match app_ids(installation) {
            Ok(found) => found,
            Err(why) => {
                tracing::warn!(path = %installation.path.display(), why, "a flatpak installation unread: flatpaks unread");
                return Known::Unread(why);
            }
        };
        for id in found {
            match renamed_from(&installation.path, &id) {
                Ok(former) => ids.extend(former),
                Err(error) => {
                    tracing::warn!(id, %error, "a flatpak's launchers unreadable: flatpaks unread");
                    return Known::Unread(error.to_string());
                }
            }
            ids.insert(id);
        }
    }
    match running(runtime) {
        Ok(running) => {
            tracing::info!(running = running.len(), "flatpaks running read");
            ids.extend(running);
        }
        Err(error) => {
            tracing::warn!(%error, "the flatpaks running unreadable: flatpaks unread");
            return Known::Unread(error.to_string());
        }
    }
    tracing::info!(
        installations = installations.len(),
        ids = ids.len(),
        "flatpaks installed read"
    );
    Known::Read(ids)
}

/// The ids the application `id` was renamed from, as its exported launchers
/// say (`X-Flatpak-RenamedFrom`, each `<former-id><suffix>`, the suffix being
/// the launcher's own after its id).
fn renamed_from(installation: &Path, id: &str) -> io::Result<Vec<String>> {
    let mut former = Vec::new();
    for deployment in deployments(installation, id)? {
        let folder = deployment.join(EXPORTED_LAUNCHERS);
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let file = entry.file_name();
            let Some(suffix) = file
                .to_str()
                .filter(|name| name.ends_with(LAUNCHER_SUFFIX))
                .and_then(|name| name.strip_prefix(id))
            else {
                continue;
            };
            let launcher = okapi_platform::desktop_entry(&entry.path(), Language::English)?;
            former.extend(
                launcher
                    .renamed_from
                    .iter()
                    .filter_map(|name| name.strip_suffix(suffix))
                    .filter(|name| names_a_folder(name))
                    .map(str::to_owned),
            );
        }
    }
    Ok(former)
}

/// The flatpaks running, as `runtime/.flatpak/<instance>/info` says; without
/// a runtime folder, where they would be said is unknown.
fn running(runtime: Option<&Path>) -> io::Result<Vec<String>> {
    let Some(runtime) = runtime else {
        return Err(io::Error::other("no runtime folder"));
    };
    let entries = match std::fs::read_dir(runtime.join(RUNNING)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut running = Vec::new();
    for entry in entries {
        let info = entry?.path().join(INSTANCE_INFO);
        let text = match okapi_platform::read_regular_capped(&info, INFO_LIMIT) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            // An instance ending meanwhile, or a file beside the instances.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut in_application = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_application = line == APPLICATION_GROUP;
            } else if in_application
                && let Some((key, value)) = line.split_once('=')
                && key.trim() == KEY_NAME
                && names_a_folder(value.trim())
            {
                running.push(value.trim().to_owned());
            }
        }
    }
    Ok(running)
}
