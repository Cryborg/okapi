//! The snaps installed, as snapd lists them (`GET /v2/snaps?select=all`:
//! every revision kept, the one in use `active`): an application when it is
//! of type `app` with a launcher the menu shows, its data in `~/snap/<name>`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use super::{App, Reading, Rest, Source, SourceReading};

/// What snapd says of one revision.
#[derive(Debug, Deserialize)]
struct Revision {
    name: String,
    #[serde(default)]
    version: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    status: String,
    #[serde(rename = "installed-size", default)]
    installed_size: u64,
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

/// Whether `name` is a snap's name (an instance key after `_`): it is then
/// safe in a path and in a request.
#[must_use]
pub fn is_snap_name(name: &str) -> bool {
    let (base, instance) = name.split_once('_').unwrap_or((name, ""));
    let lower = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    base.bytes().next().is_some_and(lower)
        && base.bytes().all(|byte| lower(byte) || byte == b'-')
        && (!name.contains('_') || (!instance.is_empty() && instance.bytes().all(lower)))
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
    let revisions: Vec<Revision> = match serde_json::from_value(result.clone()) {
        Ok(revisions) => revisions,
        Err(error) => {
            tracing::warn!(%error, "snapd's list unreadable");
            return SourceReading::Failed(error.to_string());
        }
    };
    let mut by_name: BTreeMap<String, Vec<Revision>> = BTreeMap::new();
    for revision in revisions {
        if is_snap_name(&revision.name) {
            by_name
                .entry(revision.name.clone())
                .or_default()
                .push(revision);
        } else {
            tracing::warn!(name = %revision.name, "a snap's name refused");
        }
    }
    let mut apps = Vec::new();
    let mut rest = Rest::default();
    for (name, revisions) in by_name {
        let (active, old): (Vec<Revision>, Vec<Revision>) = revisions
            .into_iter()
            .partition(|revision| revision.status == "active");
        let old_bytes = old
            .iter()
            .map(|revision| revision.installed_size)
            .fold(0, u64::saturating_add);
        let Some(active) = active.into_iter().next() else {
            // Every revision disabled: it takes space, and runs nothing.
            rest.count += 1;
            rest.bytes = rest.bytes.saturating_add(old_bytes);
            continue;
        };
        let launcher = (active.kind == "app")
            .then(|| {
                let mut files: Vec<&str> = active
                    .apps
                    .iter()
                    .filter_map(|app| app.desktop_file.as_deref())
                    .collect();
                files.sort_unstable();
                files.into_iter().find_map(|file| {
                    let path = Path::new(file);
                    reading.launcher(&root.join(path.strip_prefix("/").unwrap_or(path)))
                })
            })
            .flatten();
        let Some((shown_name, icon)) = launcher else {
            rest.count += 1;
            rest.bytes = rest
                .bytes
                .saturating_add(active.installed_size)
                .saturating_add(old_bytes);
            continue;
        };
        apps.push(App {
            source: Source::Snap,
            id: name.clone(),
            package: name.clone(),
            name: shown_name,
            version: active.version,
            publisher: active.publisher.map(|publisher| publisher.display_name),
            origin: None,
            icon,
            app_bytes: Some(active.installed_size),
            deployment: None,
            old_bytes,
            old_versions: old.len(),
            data: home.map(|home| home.join("snap").join(&name)),
            system: false,
        });
    }
    tracing::info!(apps = apps.len(), rest = rest.count, "snaps read");
    SourceReading::Read { apps, rest }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snap_name_is_safe_in_a_path_and_a_request() {
        for name in [
            "firefox",
            "gnome-46-2404",
            "core24",
            "android-studio",
            "lxd_test2",
        ] {
            assert!(is_snap_name(name), "{name}");
        }
        for name in [
            "", "-x", "Firefox", "a/b", "..", "a b", "a_", "a_B", "a%2Fb", "a_b_c",
        ] {
            assert!(!is_snap_name(name), "{name}");
        }
    }
}
