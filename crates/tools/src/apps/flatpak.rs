//! The flatpaks installed, read from their installations' folders (no
//! public interface lists them): `app/<id>/<arch>/<branch>/active` names
//! the deployment in use, beside it, whose launcher says the name. Its size
//! is measured, its data is `~/.var/app/<id>`.

use std::path::{Path, PathBuf};

use super::{App, Reading, Rest, Source, SourceReading, names_a_folder};

/// The system's installation.
pub const SYSTEM_INSTALLATION: &str = "/var/lib/flatpak";
/// The user's, under the home folder.
pub const USER_INSTALLATION: &str = ".local/share/flatpak";

/// The installations of this system: the system's and `home`'s.
#[must_use]
pub fn installations(home: Option<&Path>) -> Vec<PathBuf> {
    std::iter::once(PathBuf::from(SYSTEM_INSTALLATION))
        .chain(home.map(|home| home.join(USER_INSTALLATION)))
        .collect()
}

/// The names of the folders in `folder` that may be names, sorted; none
/// when it cannot be listed (traced).
fn subfolders(folder: &Path) -> Vec<String> {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(path = %folder.display(), %error, "flatpak folder not listed");
            return Vec::new();
        }
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            let safe = names_a_folder(name);
            if !safe {
                tracing::warn!(name, "a flatpak folder's name refused");
            }
            safe
        })
        .collect();
    names.sort_unstable();
    names
}

/// The deployment `active` names in `branch`, if it is a folder beside it
/// and not a link: never followed elsewhere.
fn deployment(branch: &Path) -> Option<PathBuf> {
    let active = branch.join("active");
    let target = std::fs::read_link(&active)
        .inspect_err(
            |error| tracing::debug!(path = %active.display(), %error, "no active deployment"),
        )
        .ok()?;
    let name = target.to_str().filter(|name| names_a_folder(name))?;
    let deployment = branch.join(name);
    let is_folder = std::fs::symlink_metadata(&deployment).is_ok_and(|metadata| metadata.is_dir());
    if !is_folder {
        tracing::warn!(path = %active.display(), "the active deployment is not a folder beside it");
        return None;
    }
    Some(deployment)
}

/// The flatpaks of `installations`, their data under `home`.
#[must_use]
pub fn read_flatpaks(
    installations: &[PathBuf],
    home: Option<&Path>,
    reading: &Reading,
) -> SourceReading {
    let _span = tracing::info_span!("flatpaks").entered();
    let present: Vec<&PathBuf> = installations
        .iter()
        .filter(|installation| installation.is_dir())
        .collect();
    if present.is_empty() {
        tracing::info!("no flatpak installation");
        return SourceReading::Absent;
    }
    let mut apps = Vec::new();
    let mut rest = Rest::default();
    for installation in present {
        let app_folder = installation.join("app");
        for id in subfolders(&app_folder) {
            for arch in subfolders(&app_folder.join(&id)) {
                for branch in subfolders(&app_folder.join(&id).join(&arch)) {
                    let branch = app_folder.join(&id).join(&arch).join(&branch);
                    let Some(deployment) = deployment(&branch) else {
                        continue;
                    };
                    let launcher = deployment
                        .join("export/share/applications")
                        .join(format!("{id}.desktop"));
                    let Some((name, icon)) = reading.launcher(&launcher) else {
                        rest.count += 1;
                        continue;
                    };
                    apps.push(App {
                        source: Source::Flatpak,
                        id: id.clone(),
                        package: id.clone(),
                        name,
                        version: String::new(),
                        publisher: None,
                        origin: None,
                        icon,
                        app_bytes: None,
                        deployment: Some(deployment),
                        old_bytes: 0,
                        old_versions: 0,
                        data: home.map(|home| home.join(".var/app").join(&id)),
                        system: false,
                    });
                }
            }
        }
    }
    tracing::info!(apps = apps.len(), rest = rest.count, "flatpaks read");
    SourceReading::Read { apps, rest }
}
