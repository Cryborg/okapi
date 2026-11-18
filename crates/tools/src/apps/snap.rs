//! The snaps installed, as snapd lists them (`GET /v2/snaps?select=all`:
//! every revision kept, the one in use `active`): an application when it is
//! of type `app` with a launcher the menu shows, its data in `~/snap/<name>`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use super::uninstall::Target;
use super::{App, AppSize, Launcher, Reading, Rest, Source, SourceReading};
use crate::packages::snapd::is_snap_name;

/// What snapd says of one revision.
#[derive(Debug, Deserialize)]
pub(crate) struct Revision {
    name: String,
    /// Its number, as snapd writes it.
    #[serde(rename = "revision", default)]
    pub(crate) number: String,
    #[serde(default)]
    pub(crate) version: String,
    /// `app`, `base`, `snapd`, `os`, `kernel`, `gadget`...
    #[serde(rename = "type", default)]
    pub(crate) kind: String,
    /// `active` for the one in use; `installed` for every other.
    #[serde(default)]
    pub(crate) status: String,
    /// Absent when snapd does not say it.
    #[serde(rename = "installed-size", default)]
    pub(crate) installed_size: Option<u64>,
    /// When the one in use was put in use, in RFC 3339: snapd says it of
    /// that one only.
    #[serde(rename = "install-date", default)]
    pub(crate) install_date: Option<String>,
    /// Why it cannot be used, when it cannot.
    #[serde(default)]
    pub(crate) broken: Option<String>,
    #[serde(default)]
    publisher: Option<Publisher>,
    #[serde(default)]
    apps: Vec<SnapApp>,
}

#[derive(Debug, Deserialize)]
struct Publisher {
    #[serde(rename = "display-name")]
    display_name: String,
}

#[derive(Debug, Deserialize)]
struct SnapApp {
    #[serde(rename = "desktop-file", default)]
    desktop_file: Option<String>,
}

/// Where snaps keep each one's data, under the home folder.
pub const SNAP_DATA: &str = "snap";

/// The first launcher of the application `active` the menu shows, its files
/// read under `root`; none for a snap that is no application.
fn shown_launcher(active: &Revision, root: &Path, reading: &Reading) -> Option<Launcher> {
    if active.kind != "app" {
        return None;
    }
    let mut files: Vec<&str> = active
        .apps
        .iter()
        .filter_map(|app| app.desktop_file.as_deref())
        .collect();
    files.sort_unstable();
    files.into_iter().find_map(|file| {
        let path = Path::new(file);
        reading.launcher(
            &root.join(path.strip_prefix("/").unwrap_or(path)),
            &reading.icons,
        )
    })
}

/// The revisions of `result` (snapd's list of every revision), in its order,
/// by their snap's name; a revision of a name that is no snap's passed
/// over, traced.
///
/// # Errors
///
/// `result` is no list of revisions.
pub(crate) fn revisions_by_name(
    result: &serde_json::Value,
) -> Result<BTreeMap<String, Vec<Revision>>, serde_json::Error> {
    let mut by_name: BTreeMap<String, Vec<Revision>> = BTreeMap::new();
    for revision in Vec::<Revision>::deserialize(result)? {
        if is_snap_name(&revision.name) {
            by_name
                .entry(revision.name.clone())
                .or_default()
                .push(revision);
        } else {
            tracing::warn!(name = %revision.name, "a snap's name refused");
        }
    }
    Ok(by_name)
}

/// The snaps of `result` (snapd's list of every revision), their launchers
/// read under `root` (`/`, or a test's folder), their data under `home`.
#[must_use]
pub fn read_snaps(
    result: &serde_json::Value,
    root: &Path,
    home: Option<&Path>,
    reading: &Reading,
) -> SourceReading {
    let _span = tracing::info_span!("snaps").entered();
    let by_name = match revisions_by_name(result) {
        Ok(by_name) => by_name,
        Err(error) => {
            tracing::warn!(%error, "snapd's list unreadable");
            return SourceReading::Failed(error.to_string());
        }
    };
    let mut apps = Vec::new();
    let mut rest = Rest::default();
    let mut without_size = 0_usize;
    for (name, revisions) in by_name {
        let (active, old): (Vec<Revision>, Vec<Revision>) = revisions
            .into_iter()
            .partition(|revision| revision.status == "active");
        // A revision of no size said counts nothing, and is told.
        without_size += old
            .iter()
            .filter(|revision| revision.installed_size.is_none())
            .count();
        let old_bytes = old
            .iter()
            .filter_map(|revision| revision.installed_size)
            .fold(0, u64::saturating_add);
        let Some(active) = active.into_iter().next() else {
            // Every revision disabled: it takes space, and runs nothing.
            rest.count += 1;
            rest.bytes = rest.bytes.saturating_add(old_bytes);
            continue;
        };
        let launcher = shown_launcher(&active, root, reading);
        without_size += usize::from(active.installed_size.is_none());
        let Some(Launcher {
            name: shown_name,
            icon,
            aliases,
        }) = launcher
        else {
            rest.count += 1;
            rest.bytes = rest
                .bytes
                .saturating_add(active.installed_size.unwrap_or(0))
                .saturating_add(old_bytes);
            continue;
        };
        apps.push(App {
            source: Source::Snap,
            id: name.clone(),
            package: name.clone(),
            name: shown_name,
            aliases,
            version: active.version,
            publisher: active.publisher.map(|publisher| publisher.display_name),
            origin: None,
            icon,
            size: active
                .installed_size
                .map_or(AppSize::Unknown, AppSize::Known),
            old_bytes,
            old_versions: old.len(),
            data: home.map(|home| home.join(SNAP_DATA).join(&name)),
            system: false,
            target: Some(Target::Snap {
                name: name.clone(),
                purge: false,
            }),
        });
    }
    tracing::info!(
        apps = apps.len(),
        rest = rest.count,
        without_size,
        "snaps read"
    );
    SourceReading::Read {
        apps,
        rest,
        partial: false,
    }
}
