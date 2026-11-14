//! The flatpaks installed, read from their installations' folders (no
//! public interface lists them): `app/<id>/<arch>/<branch>/active` names
//! the deployment in use, beside it, whose launcher says the name. Its size
//! is measured, its data is `~/.var/app/<id>`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::icons::IconRoots;
use super::{App, AppSize, Reading, Rest, Source, SourceReading, names_a_folder};

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

/// The names of the folders in `folder` that may be names, sorted.
///
/// # Errors
///
/// The system's error listing it.
fn subfolders(folder: &Path) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let name = entry?.file_name();
        match name.to_str() {
            Some(name) if names_a_folder(name) => names.push(name.to_owned()),
            _ => {
                tracing::warn!(?name, folder = %folder.display(), "a flatpak folder's name refused");
            }
        }
    }
    names.sort_unstable();
    Ok(names)
}

/// [`subfolders`], none when it cannot be listed (traced).
fn subfolders_or_none(folder: &Path) -> Vec<String> {
    subfolders(folder)
        .inspect_err(
            |error| tracing::warn!(path = %folder.display(), %error, "flatpak folder not listed"),
        )
        .unwrap_or_default()
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
    let Some(name) = target.to_str().filter(|name| names_a_folder(name)) else {
        tracing::warn!(path = %active.display(), target = %target.display(), "the active deployment is not beside it: refused");
        return None;
    };
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
    let mut seen = HashSet::new();
    for installation in present {
        let app_folder = installation.join("app");
        let ids = match subfolders(&app_folder) {
            Ok(ids) => ids,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                tracing::warn!(path = %app_folder.display(), %error, "flatpak installation unreadable");
                return SourceReading::Failed(error.to_string());
            }
        };
        for id in ids {
            for arch in subfolders_or_none(&app_folder.join(&id)) {
                for branch in subfolders_or_none(&app_folder.join(&id).join(&arch)) {
                    let branch = app_folder.join(&id).join(&arch).join(&branch);
                    let Some(deployment) = deployment(&branch) else {
                        continue;
                    };
                    // The system's installation first, then the user's; a
                    // branch, then another: one line per application.
                    if !seen.insert(id.clone()) {
                        tracing::warn!(id, branch = %branch.display(), "a flatpak installed twice: shown once");
                        continue;
                    }
                    let export = deployment.join("export/share");
                    // Its exported icons, beside its launcher: those under
                    // the installation's exports are links to them.
                    let roots = IconRoots {
                        themes: vec![export.join("icons")],
                        pixmaps: Vec::new(),
                        absolute: vec![deployment.clone()],
                    };
                    let launcher = export.join("applications").join(format!("{id}.desktop"));
                    let Some((name, icon)) = reading.launcher(&launcher, &roots) else {
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
                        size: AppSize::ToMeasure(deployment),
                        old_bytes: 0,
                        old_versions: 0,
                        data: home.map(|home| home.join(".var/app").join(&id)),
                        system: false,
                        target: None,
                    });
                }
            }
        }
    }
    tracing::info!(apps = apps.len(), rest = rest.count, "flatpaks read");
    SourceReading::Read { apps, rest }
}
