//! The flatpaks installed, read from their installations' folders (no
//! public interface lists them): `app/<id>/<arch>/<branch>/active` names
//! the deployment in use, beside it, whose launcher says the name. Its size
//! is measured, its data is `~/.var/app/<id>`. An application of the
//! system's installation is uninstalled by flatpak's system helper, with the
//! extensions that go with it ([`related`]); one of the user's installation
//! is not (no service changes it). The runtimes no application uses any
//! more are told by [`unused`].

pub mod unused;

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use super::icons::IconRoots;
use super::uninstall::{Target, Why};
use super::{App, AppSize, Launcher, Reading, Rest, Source, SourceReading, names_a_folder};

/// The system's installation, unless `FLATPAK_SYSTEM_DIR` says another.
pub const SYSTEM_INSTALLATION: &str = "/var/lib/flatpak";
/// The user's, under the data folder (`$XDG_DATA_HOME`), unless
/// `FLATPAK_USER_DIR` says another.
pub const USER_INSTALLATION: &str = "flatpak";
/// What flatpak reads to place its installations, as it does.
const SYSTEM_DIR_VARIABLE: &str = "FLATPAK_SYSTEM_DIR";
const USER_DIR_VARIABLE: &str = "FLATPAK_USER_DIR";
const CONFIG_DIR_VARIABLE: &str = "FLATPAK_CONFIG_DIR";
/// Where flatpak's configuration is, unless `FLATPAK_CONFIG_DIR` says.
const CONFIG_DIRECTORY: &str = "/etc/flatpak";
/// The folder of the configuration declaring other installations, each in
/// a `*.conf` key file whose groups say their `Path=`.
const DECLARATIONS: &str = "installations.d";
const DECLARATION_SUFFIX: &str = ".conf";
const KEY_PATH: &str = "Path";
/// The most of a declaration read.
const DECLARATION_LIMIT: usize = 64 * 1024;
/// Where flatpaks keep each one's data, under the home folder.
pub const FLATPAK_DATA: &str = ".var/app";
/// What flatpak makes with each installation: none there, it may be a disk
/// not mounted.
const REPOSITORY: &str = "repo";
/// What describes a deployment: its runtime, its extensions...
const METADATA: &str = "metadata";
/// The most of a deployment's metadata read: such files are small, the cap
/// bounds a hostile one.
pub(crate) const METADATA_LIMIT: usize = 64 * 1024;
/// The groups of the metadata that name an extension: `[Extension <name>]`,
/// the name maybe followed by `@<tag>`.
const EXTENSION_GROUP: &str = "Extension ";
const TAG: char = '@';
/// Extensions that always go with their application, whatever their keys.
const ALWAYS_DELETED: [&str; 2] = [".Locale", ".Debug"];
const KEY_AUTODELETE: &str = "autodelete";
const KEY_LOCALE_SUBSET: &str = "locale-subset";
const KEY_VERSION: &str = "version";
const KEY_VERSIONS: &str = "versions";
const KEY_SUBDIRECTORIES: &str = "subdirectories";
/// What separates the items of a list, in a key file.
const LIST_SEPARATOR: char = ';';
/// The group of an extension's metadata that says whose it is, by the
/// reference `ref` of what it extends.
const EXTENSION_OF_GROUP: &str = "ExtensionOf";
const KEY_REF: &str = "ref";
/// What flatpak keeps of a deployment beside it: a `GVariant` `(ssasta{sv})`
/// whose first member, the remote it came from, starts the file and ends at
/// the first NUL.
const DEPLOY_DATA: &str = "deploy";
/// The most of a deployment's data read: its `AppStream` texts make it some
/// kilobytes.
const DEPLOY_DATA_LIMIT: usize = 1024 * 1024;

/// One installation of flatpaks. Whether it is there is said by
/// [`presence`] alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    /// Where it is; for one unplaced, what could not be read.
    pub(crate) path: PathBuf,
    /// The system's default (no `FLATPAK_SYSTEM_DIR`): the only one its
    /// helper uninstalls from.
    pub(crate) system: bool,
    /// Declared in `installations.d`: when it cannot be found (a disk
    /// unplugged) it is unread, never none.
    pub(crate) declared: bool,
    /// Why it could not even be placed (a declaration unreadable, no data
    /// folder for the user's, a place written relative): it is then always
    /// unread, never none.
    pub(crate) unplaced: Option<String>,
}

impl Installation {
    pub(crate) fn at(path: PathBuf, system: bool, declared: bool) -> Self {
        Self {
            path,
            system,
            declared,
            unplaced: None,
        }
    }

    /// One that could not be placed, for `why`, `path` being what could not
    /// be read.
    pub(crate) fn unplaced(path: PathBuf, why: String) -> Self {
        Self {
            path,
            system: false,
            declared: false,
            unplaced: Some(why),
        }
    }
}

/// The installations of this system, placed as flatpak places them: the
/// system's, the user's under the data folder `data_home`, then those
/// declared in `installations.d`.
#[must_use]
pub fn installations(data_home: Option<&Path>) -> Vec<Installation> {
    installations_from(&|name| std::env::var_os(name), data_home)
}

/// [`installations`], the environment read through `variable`. A place a
/// variable writes relative is unplaced: read from Okapi's own folder, it
/// would be said absent.
pub(crate) fn installations_from(
    variable: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    data_home: Option<&Path>,
) -> Vec<Installation> {
    let placed = |name: &str| {
        variable(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let at = |path: PathBuf, system: bool| {
        if path.is_absolute() {
            Installation::at(path, system, false)
        } else {
            tracing::warn!(path = %path.display(), "a flatpak installation placed relative: unread");
            Installation::unplaced(path, "placed relative".to_owned())
        }
    };
    let user = match (placed(USER_DIR_VARIABLE), data_home) {
        (Some(path), _) => at(path, false),
        (None, Some(data)) => at(data.join(USER_INSTALLATION), false),
        (None, None) => {
            tracing::warn!("no data folder: the user's flatpak installation unread");
            Installation::unplaced(PathBuf::new(), "no data folder".to_owned())
        }
    };
    let config = placed(CONFIG_DIR_VARIABLE).unwrap_or_else(|| PathBuf::from(CONFIG_DIRECTORY));
    // The helper, root with its own environment, only changes its default.
    let system = match placed(SYSTEM_DIR_VARIABLE) {
        Some(path) => {
            let default = path == Path::new(SYSTEM_INSTALLATION);
            at(path, default)
        }
        None => at(PathBuf::from(SYSTEM_INSTALLATION), true),
    };
    let declarations = if config.is_absolute() {
        declared(&config.join(DECLARATIONS))
    } else {
        tracing::warn!(path = %config.display(), "flatpak's configuration placed relative: its installations unread");
        vec![Installation::unplaced(config, "placed relative".to_owned())]
    };
    let installations: Vec<Installation> = [system, user].into_iter().chain(declarations).collect();
    for installation in &installations {
        tracing::info!(
            path = %installation.path.display(),
            system = installation.system,
            declared = installation.declared,
            unplaced = installation.unplaced.as_deref(),
            "flatpak installation placed"
        );
    }
    installations
}

/// Whether `path`, found missing, is surely absent: the first of its
/// ancestors there is no link leading nowhere (one to a disk not mounted
/// would hide what is installed) and could be read.
///
/// # Errors
///
/// Why its absence is not sure.
fn surely_absent(path: &Path) -> Result<(), String> {
    for ancestor in path.ancestors().skip(1) {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata)
                if metadata.file_type().is_symlink() && std::fs::metadata(ancestor).is_err() =>
            {
                return Err(format!("{} leads nowhere", ancestor.display()));
            }
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", ancestor.display())),
        }
    }
    Ok(())
}

/// Whether `path`, found missing by a reading that follows links, is
/// surely absent: itself no link, then [`surely_absent`].
///
/// # Errors
///
/// Why its absence is not sure.
pub(crate) fn missing_for_sure(path: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(format!("{} is a link leading nowhere", path.display()));
    }
    surely_absent(path)
}

/// Whether `installation` is there: absent only when it is not declared
/// and surely absent; one unplaced, no folder or unreadable says why it is
/// unread.
///
/// # Errors
///
/// Why it is unread.
pub(crate) fn presence(installation: &Installation) -> Result<bool, String> {
    if let Some(why) = &installation.unplaced {
        return Err(why.clone());
    }
    match std::fs::metadata(&installation.path) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(format!("{} is not a folder", installation.path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !installation.declared => {
            missing_for_sure(&installation.path)?;
            tracing::debug!(path = %installation.path.display(), "no flatpak installation there");
            Ok(false)
        }
        Err(error) => Err(format!("{}: {error}", installation.path.display())),
    }
}

/// The installations `folder` declares, each `Path=` of its `*.conf`; a
/// declaration that cannot be read is an installation unplaced.
fn declared(folder: &Path) -> Vec<Installation> {
    let unreadable = |path: &Path, why: String| {
        tracing::warn!(path = %path.display(), why, "a flatpak declaration unreadable: flatpaks unread");
        Installation::unplaced(path.to_path_buf(), why)
    };
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => return vec![unreadable(folder, error.to_string())],
    };
    let mut found = Vec::new();
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry.path()),
            Err(error) => {
                found.push(unreadable(folder, error.to_string()));
                None
            }
        })
        // Told by the bytes of its name, as flatpak does: one not in UTF-8
        // is read all the same.
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                name.as_encoded_bytes()
                    .ends_with(DECLARATION_SUFFIX.as_bytes())
            })
        })
        .collect();
    files.sort();
    for file in files {
        let text = match okapi_platform::read_regular_capped(&file, DECLARATION_LIMIT) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                found.push(unreadable(&file, error.to_string()));
                continue;
            }
        };
        let paths: Vec<PathBuf> = text
            .lines()
            .filter_map(|line| line.trim().split_once('='))
            .filter(|(key, _)| key.trim() == KEY_PATH)
            .map(|(_, value)| PathBuf::from(value.trim()))
            .collect();
        if paths.is_empty() || paths.iter().any(|path| !path.is_absolute()) {
            found.push(unreadable(&file, "a path missing or relative".to_owned()));
        }
        found.extend(
            paths
                .into_iter()
                .filter(|path| path.is_absolute())
                .map(|path| Installation::at(path, false, true)),
        );
    }
    found
}

/// What a reference names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum RefKind {
    App,
    Runtime,
}

/// A flatpak, as flatpak names it: `app/<id>/<arch>/<branch>` or
/// `runtime/…`; each part a name read from a folder, never a path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FlatpakRef {
    kind: RefKind,
    id: String,
    arch: String,
    branch: String,
}

impl FlatpakRef {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn arch(&self) -> &str {
        &self.arch
    }

    /// An application's reference, if each part may name a folder.
    #[must_use]
    pub fn app(id: &str, arch: &str, branch: &str) -> Option<Self> {
        Self::new(RefKind::App, id, arch, branch)
    }

    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The runtime `text` names as a metadata writes it (`runtime=`,
    /// `sdk=`: `<id>/<arch>/<branch>`), if it is one.
    fn runtime_of(text: &str) -> Option<Self> {
        let mut parts = text.split('/');
        let (id, arch, branch) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        Self::new(RefKind::Runtime, id, arch, branch)
    }

    /// The reference `text` writes (`app/<id>/<arch>/<branch>`), if it is
    /// one.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('/');
        let kind = match parts.next()? {
            "app" => RefKind::App,
            "runtime" => RefKind::Runtime,
            _ => return None,
        };
        let (id, arch, branch) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        Self::new(kind, id, arch, branch)
    }

    fn new(kind: RefKind, id: &str, arch: &str, branch: &str) -> Option<Self> {
        [id, arch, branch]
            .iter()
            .all(|part| names_a_folder(part))
            .then(|| Self {
                kind,
                id: id.to_owned(),
                arch: arch.to_owned(),
                branch: branch.to_owned(),
            })
    }

    /// Its branch's folder in `installation`.
    fn folder(&self, installation: &Path) -> PathBuf {
        installation
            .join(self.kind.folder())
            .join(&self.id)
            .join(&self.arch)
            .join(&self.branch)
    }
}

impl RefKind {
    const fn folder(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Runtime => "runtime",
        }
    }
}

impl fmt::Display for FlatpakRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}/{}/{}",
            self.kind.folder(),
            self.id,
            self.arch,
            self.branch
        )
    }
}

/// The folders of a folder that may be names, sorted, and whether each of
/// its entries could be read.
struct Listed {
    names: Vec<String>,
    whole: bool,
    /// A folder's name was refused (not a name, not UTF-8), or a link other
    /// than `current` stands where folders are: what it holds is not known.
    refused: bool,
}

/// The one link flatpak makes among the folders it lists:
/// `app/<id>/current`, to a branch.
const CURRENT_LINK: &str = "current";

/// The names of the folders in `folder` that may be names: never a link
/// (`app/<id>/current` is one to a branch). An entry that cannot be read
/// (gone meanwhile) is passed over, the listing then not whole (what
/// follows it may be lost too).
///
/// # Errors
///
/// The system's error listing it.
fn subfolders(folder: &Path) -> io::Result<Listed> {
    let mut names = Vec::new();
    let mut whole = true;
    let mut refused = false;
    for entry in std::fs::read_dir(folder)? {
        let read = entry.and_then(|entry| Ok((entry.file_name(), entry.file_type()?)));
        let (name, kind) = match read {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(folder = %folder.display(), %error, "an entry of a flatpak folder unreadable");
                whole = false;
                continue;
            }
        };
        if kind.is_symlink() && name != CURRENT_LINK {
            tracing::warn!(?name, folder = %folder.display(), "a link where flatpak keeps folders: never followed");
            refused = true;
            continue;
        }
        if !kind.is_dir() {
            tracing::debug!(?name, folder = %folder.display(), "not a folder of flatpak: passed over");
            continue;
        }
        match name.to_str() {
            Some(name) if names_a_folder(name) => names.push(name.to_owned()),
            _ => {
                tracing::warn!(?name, folder = %folder.display(), "a flatpak folder's name refused");
                refused = true;
            }
        }
    }
    names.sort_unstable();
    Ok(Listed {
        names,
        whole,
        refused,
    })
}

/// [`subfolders`], none when it is gone; `partial` set when it, or an entry
/// of it, cannot be read or is refused (traced).
fn subfolders_or_none(folder: &Path, partial: &mut bool) -> Vec<String> {
    match subfolders(folder) {
        Ok(listed) => {
            *partial |= !listed.whole || listed.refused;
            listed.names
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::warn!(path = %folder.display(), %error, "flatpak folder not listed");
            *partial = true;
            Vec::new()
        }
    }
}

/// The deployment `active` names in `branch`, if it is a folder beside it
/// and not a link: never followed elsewhere. None when there is none, or one
/// refused (traced).
///
/// # Errors
///
/// `active` could not be read.
fn deployment(branch: &Path) -> io::Result<Option<PathBuf>> {
    Ok(match active(branch)? {
        Active::At(deployment) => Some(deployment),
        Active::None | Active::Refused => None,
    })
}

/// What a branch's `active` names.
pub(crate) enum Active {
    /// No `active`: nothing deployed.
    None,
    /// One that is not a folder beside it: never followed (traced).
    Refused,
    At(PathBuf),
}

/// The deployment `active` names in `branch`.
///
/// # Errors
///
/// `active` could not be read.
fn active(branch: &Path) -> io::Result<Active> {
    let active = branch.join("active");
    let target = match std::fs::read_link(&active) {
        Ok(target) => target,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::debug!(path = %active.display(), "no active deployment");
            return Ok(Active::None);
        }
        Err(error) => {
            tracing::warn!(path = %active.display(), %error, "the active deployment unreadable");
            return Err(error);
        }
    };
    let Some(name) = target.to_str().filter(|name| names_a_folder(name)) else {
        tracing::warn!(path = %active.display(), target = %target.display(), "the active deployment is not beside it: refused");
        return Ok(Active::Refused);
    };
    let deployment = branch.join(name);
    let is_folder = std::fs::symlink_metadata(&deployment).is_ok_and(|metadata| metadata.is_dir());
    if !is_folder {
        tracing::warn!(path = %active.display(), "the active deployment is not a folder beside it");
        return Ok(Active::Refused);
    }
    Ok(Active::At(deployment))
}

/// The ids of the applications `installation` holds, each `app/<id>`,
/// deployed or not, shown in the menu or not.
///
/// # Errors
///
/// Why the installation could not be read whole: one unplaced, one absent
/// that is declared, one unreadable, a part of its listing unreadable.
pub(crate) fn app_ids(installation: &Installation) -> Result<Vec<String>, String> {
    if !presence(installation)? {
        return Ok(Vec::new());
    }
    kind_ids(&installation.path, RefKind::App).map(|listed| listed.names)
}

/// The ids of what of `kind` the installation at `installation` (there)
/// holds, `app/<id>` or `runtime/<id>`, listed whole.
///
/// # Errors
///
/// Why they could not be listed whole: the folder unreadable, an entry of
/// it, a name refused or a link where folders are (what they hold is not
/// known), or the folder missing from an installation that holds nothing.
fn kind_ids(installation: &Path, kind: RefKind) -> Result<Listed, String> {
    let folder = installation.join(kind.folder());
    match subfolders(&folder) {
        Ok(listed) if listed.whole && !listed.refused => Ok(listed),
        Ok(_) => Err(format!("{} not listed whole", folder.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            missing_for_sure(&folder)?;
            if holds_something(installation)? {
                tracing::debug!(path = %folder.display(), "none there in an installation that holds something: none installed");
                Ok(Listed {
                    names: Vec::new(),
                    whole: true,
                    refused: false,
                })
            } else {
                Err(format!("{} holds nothing", installation.display()))
            }
        }
        Err(error) => Err(format!("{}: {error}", folder.display())),
    }
}

/// Whether the installation at `installation` holds what flatpak makes in
/// one: its repository, or anything (its permission store, `db`, is there
/// on a desktop where nothing was ever installed). An empty folder is a
/// disk not mounted there, maybe.
///
/// # Errors
///
/// It could not be listed.
fn holds_something(installation: &Path) -> Result<bool, String> {
    if std::fs::symlink_metadata(installation.join(REPOSITORY))
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return Ok(true);
    }
    let unread = |error: io::Error| format!("{}: {error}", installation.display());
    match std::fs::read_dir(installation).map_err(unread)?.next() {
        None => Ok(false),
        Some(Ok(_)) => Ok(true),
        Some(Err(error)) => Err(unread(error)),
    }
}

/// The deployments in use of the application `id` in `installation`, of
/// every architecture and branch.
///
/// # Errors
///
/// A folder of it, or an `active`, that cannot be read.
pub(crate) fn deployments(installation: &Path, id: &str) -> io::Result<Vec<PathBuf>> {
    let folder = installation.join(RefKind::App.folder()).join(id);
    let mut found = Vec::new();
    let listed = |folder: &Path| match subfolders(folder) {
        Ok(Listed {
            names, whole: true, ..
        }) => Ok(names),
        Ok(Listed { whole: false, .. }) => Err(io::Error::other("not listed whole")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    };
    for arch in listed(&folder)? {
        for branch in listed(&folder.join(&arch))? {
            if let Some(deployment) = deployment(&folder.join(&arch).join(&branch))? {
                found.push(deployment);
            }
        }
    }
    Ok(found)
}

/// The flatpaks of `installations`, their data under `home`. An
/// installation, or a folder of one, that cannot be read is passed over and
/// the reading said partial; it failed only when no installation there gave
/// its list.
#[must_use]
pub fn read_flatpaks(
    installations: &[Installation],
    home: Option<&Path>,
    reading: &Reading,
) -> SourceReading {
    let _span = tracing::info_span!("flatpaks").entered();
    let mut found = Found::default();
    let mut present = Vec::new();
    for installation in installations {
        match presence(installation) {
            Ok(true) => present.push(installation),
            Ok(false) => {}
            // There, maybe (a declared one on a disk unplugged): never said
            // absent.
            Err(why) => {
                tracing::warn!(path = %installation.path.display(), why, "flatpak installation unread");
                found.unread(why);
            }
        }
    }
    if present.is_empty() && found.failure.is_none() {
        tracing::info!("no flatpak installation");
        return SourceReading::Absent;
    }
    let mut listed = 0_usize;
    for installation in present {
        match read_installation(installation, home, reading, &mut found) {
            Ok(()) => listed += 1,
            Err(error) => found.unread(error.to_string()),
        }
    }
    let Found {
        apps,
        rest,
        partial,
        failure,
        ..
    } = found;
    if listed == 0 {
        let error = failure.unwrap_or_default();
        tracing::warn!(%error, "no flatpak installation read");
        return SourceReading::Failed(error);
    }
    tracing::info!(
        apps = apps.len(),
        rest = rest.count,
        partial,
        "flatpaks read"
    );
    SourceReading::Read {
        apps,
        rest,
        partial,
    }
}

/// What the installations read so far gave.
#[derive(Default)]
struct Found {
    apps: Vec<App>,
    rest: Rest,
    /// The ids of the applications shown: one line each.
    seen: HashSet<String>,
    partial: bool,
    /// Why the last installation not read was not.
    failure: Option<String>,
}

impl Found {
    /// An installation, or a part of one, not read, for `why`.
    fn unread(&mut self, why: String) {
        self.partial = true;
        self.failure = Some(why);
    }
}

/// Reads the flatpaks of `installation` into `found`.
///
/// # Errors
///
/// Its applications could not be listed (traced).
fn read_installation(
    installation: &Installation,
    home: Option<&Path>,
    reading: &Reading,
    found: &mut Found,
) -> io::Result<()> {
    let app_folder = installation.path.join(RefKind::App.folder());
    let ids = match subfolders(&app_folder) {
        Ok(listed) => {
            found.partial |= !listed.whole || listed.refused;
            listed.names
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::warn!(path = %app_folder.display(), %error, "flatpak installation unreadable");
            return Err(error);
        }
    };
    for id in ids {
        for arch in subfolders_or_none(&app_folder.join(&id), &mut found.partial) {
            for branch in subfolders_or_none(&app_folder.join(&id).join(&arch), &mut found.partial)
            {
                let Some(reference) = FlatpakRef::app(&id, &arch, &branch) else {
                    continue;
                };
                let deployment = match deployment(&reference.folder(&installation.path)) {
                    Ok(Some(deployment)) => deployment,
                    Ok(None) => continue,
                    Err(_) => {
                        found.partial = true;
                        continue;
                    }
                };
                if found.seen.contains(&id) {
                    tracing::warn!(%reference, installation = %installation.path.display(), "a flatpak installed twice: shown once");
                    continue;
                }
                let Some(app) = app_of(installation, reference, deployment, home, reading) else {
                    found.rest.count += 1;
                    continue;
                };
                // The system's installation first, then the user's; a
                // branch, then another: one line per application, the first
                // shown in the menu.
                found.seen.insert(id.clone());
                found.apps.push(app);
            }
        }
    }
    Ok(())
}

/// The application `reference` deployed at `deployment` in `installation`,
/// if its launcher is shown in the menu; its data under `home`.
fn app_of(
    installation: &Installation,
    reference: FlatpakRef,
    deployment: PathBuf,
    home: Option<&Path>,
    reading: &Reading,
) -> Option<App> {
    let export = deployment.join("export/share");
    // Its exported icons, beside its launcher: those under the
    // installation's exports are links to them.
    let roots = IconRoots {
        themes: vec![export.join("icons")],
        pixmaps: Vec::new(),
        absolute: vec![deployment.clone()],
    };
    let id = reference.id.clone();
    let launcher = export.join("applications").join(format!("{id}.desktop"));
    let Launcher {
        name,
        icon,
        aliases,
    } = reading.launcher(&launcher, &roots)?;
    Some(App {
        aliases,
        source: Source::Flatpak,
        package: id.clone(),
        name,
        version: String::new(),
        publisher: None,
        origin: None,
        icon,
        size: AppSize::ToMeasure(deployment),
        old_bytes: 0,
        old_versions: 0,
        data: home.map(|home| home.join(FLATPAK_DATA).join(&id)),
        system: false,
        target: installation.system.then_some(Target::Flatpak(reference)),
        id,
    })
}

/// What a metadata's extension point (`[Extension <name>]`) says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Extension {
    pub(crate) name: String,
    /// Its branches: its own, or those of what it extends.
    pub(crate) branches: Vec<String>,
    /// Its own subdirectories are extensions too (`<name>.<x>`).
    pub(crate) subdirectories: bool,
    /// It goes with what it extends, as libflatpak tells it: marked
    /// `autodelete`, a subset of locales, or translations or debugging
    /// symbols by their name.
    deleted: bool,
}

/// Whether a key file's `value` says true.
fn says_true(value: Option<&str>) -> bool {
    matches!(value, Some("true" | "1"))
}

/// A group of a key file (`[name]`) and its keys, in their order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyGroup {
    pub(crate) name: String,
    keys: Vec<(String, String)>,
}

impl KeyGroup {
    /// Every value `wanted` is given in this group, in their order.
    pub(crate) fn values<'a, 'b>(
        &'a self,
        wanted: &'b str,
    ) -> impl Iterator<Item = &'a str> + use<'a, 'b> {
        self.keys
            .iter()
            .filter(move |(key, _)| key == wanted)
            .map(|(_, value)| value.as_str())
    }

    /// The value of `wanted`, the last one when it is written twice (as
    /// `GKeyFile` reads it).
    pub(crate) fn value(&self, wanted: &str) -> Option<&str> {
        self.values(wanted).last()
    }
}

/// The groups of the key file `text`, in their order: comments, blank
/// lines and keys before any group are passed over. A group named twice is
/// two groups here, where `GKeyFile` would merge them: an extension named
/// twice is two candidates, each checked by its own metadata.
pub(crate) fn key_file_groups(text: &str) -> Vec<KeyGroup> {
    let mut groups: Vec<KeyGroup> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            groups.push(KeyGroup {
                name: name.to_owned(),
                keys: Vec::new(),
            });
        } else if let (Some(group), Some((key, value))) = (groups.last_mut(), line.split_once('='))
        {
            group
                .keys
                .push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    groups
}

/// The extensions of `metadata` that go with their application (whose
/// branch is `branch`). Each is only a candidate: whether it is the
/// application's is told by its own metadata ([`Owner::not_its_own`]).
fn extensions_deleted(metadata: &str, branch: &str) -> Vec<Extension> {
    extension_points(metadata, branch)
        .into_iter()
        .filter(|extension| extension.deleted)
        .collect()
}

/// Every extension point of `metadata`, of what is on the branch `branch`,
/// whose name may name a folder: the tag after `@` left out.
pub(crate) fn extension_points(metadata: &str, branch: &str) -> Vec<Extension> {
    key_file_groups(metadata)
        .into_iter()
        .filter_map(|group| {
            let tagged = group.name.strip_prefix(EXTENSION_GROUP)?;
            let name = tagged.split_once(TAG).map_or(tagged, |(name, _)| name);
            let value = |wanted: &str| group.value(wanted);
            let deleted = says_true(value(KEY_AUTODELETE))
                || says_true(value(KEY_LOCALE_SUBSET))
                || ALWAYS_DELETED.iter().any(|suffix| name.ends_with(suffix));
            if !names_a_folder(name) {
                return None;
            }
            let branches: Vec<String> = match (value(KEY_VERSIONS), value(KEY_VERSION)) {
                (Some(versions), _) => versions
                    .split(LIST_SEPARATOR)
                    .filter(|version| !version.is_empty())
                    .map(str::to_owned)
                    .collect(),
                (None, Some(version)) => vec![version.to_owned()],
                (None, None) => vec![branch.to_owned()],
            };
            Some(Extension {
                name: name.to_owned(),
                branches,
                subdirectories: says_true(value(KEY_SUBDIRECTORIES)),
                deleted,
            })
        })
        .collect()
}

/// The deployment of `reference` in `installation`, none when it has none.
///
/// # Errors
///
/// [`Why::Unclear`] when it cannot be read (traced).
fn deployed_or_unclear(
    installation: &Path,
    reference: &FlatpakRef,
) -> Result<Option<PathBuf>, Why> {
    deployment(&reference.folder(installation)).map_err(|_| Why::Unclear)
}

/// The ids of the runtimes of `installation`, listed whole, for the
/// extensions of `app` whose subdirectories are extensions too.
///
/// # Errors
///
/// [`Why::Unclear`] when they cannot be listed whole (traced).
fn runtime_ids(installation: &Path, app: &FlatpakRef) -> Result<Vec<String>, Why> {
    match subfolders(&installation.join(RefKind::Runtime.folder())) {
        Ok(Listed {
            names, whole: true, ..
        }) => Ok(names),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Ok(Listed { whole: false, .. }) => {
            tracing::warn!(%app, "the runtimes not listed whole: what goes unknown");
            Err(Why::Unclear)
        }
        Err(error) => {
            tracing::warn!(%app, %error, "the runtimes not listed: what goes unknown");
            Err(Why::Unclear)
        }
    }
}

/// The remote `deployment` came from, as its data says (what precedes the
/// first NUL of `deploy`); none when it has no data or none readable as a
/// name.
///
/// # Errors
///
/// Its data cannot be read (traced by the caller).
fn deployment_origin(deployment: &Path) -> io::Result<Option<String>> {
    let bytes =
        match okapi_platform::read_regular_capped(&deployment.join(DEPLOY_DATA), DEPLOY_DATA_LIMIT)
        {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
    Ok(bytes
        .iter()
        .position(|byte| *byte == 0)
        .and_then(|end| std::str::from_utf8(&bytes[..end]).ok())
        .filter(|origin| !origin.is_empty())
        .map(str::to_owned))
}

/// What a candidate needs to go with `app`: the application's origin, read
/// once, when first needed (an application with no candidate of its own
/// never needs it).
struct Owner<'a> {
    installation: &'a Path,
    app: &'a FlatpakRef,
    deployment: &'a Path,
    origin: Option<String>,
}

impl Owner<'_> {
    /// The application's origin.
    ///
    /// # Errors
    ///
    /// [`Why::Unclear`] when it cannot be told (traced): what goes with it
    /// cannot either, as for libflatpak.
    fn origin(&mut self) -> Result<&str, Why> {
        let origin = if let Some(origin) = self.origin.take() {
            origin
        } else {
            let app = self.app;
            match deployment_origin(self.deployment) {
                Ok(Some(origin)) => origin,
                Ok(None) => {
                    tracing::warn!(%app, "its origin unknown (no data, or none naming a remote): what goes unknown");
                    return Err(Why::Unclear);
                }
                Err(error) => {
                    tracing::warn!(%app, %error, "its origin unreadable: what goes unknown");
                    return Err(Why::Unclear);
                }
            }
        };
        Ok(self.origin.insert(origin))
    }

    /// Why the extension `reference` deployed at `deployment` is not the
    /// application's, so stays: its own metadata does not say it extends
    /// this application (`[ExtensionOf] ref=app/<id>/<arch>/…`), or it
    /// extends another branch of it still installed, or it came from
    /// another remote. `None` when it goes. The application's metadata
    /// only names candidates: it may name anyone's.
    ///
    /// # Errors
    ///
    /// [`Why::Unclear`] when what it is cannot be read (traced).
    fn not_its_own(
        &mut self,
        reference: &FlatpakRef,
        deployment: &Path,
    ) -> Result<Option<&'static str>, Why> {
        let app = self.app;
        let metadata = match okapi_platform::read_regular_capped(
            &deployment.join(METADATA),
            METADATA_LIMIT,
        ) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Some("no metadata"));
            }
            Err(error) => {
                tracing::warn!(%app, %reference, %error, "an extension's metadata unreadable: what goes unknown");
                return Err(Why::Unclear);
            }
        };
        let groups = key_file_groups(&metadata);
        let Some(extended) = groups
            .iter()
            .rev()
            .find(|group| group.name == EXTENSION_OF_GROUP)
            .and_then(|group| group.value(KEY_REF))
        else {
            return Ok(Some("no ExtensionOf"));
        };
        let Some(extended) = FlatpakRef::parse(extended).filter(|extended| {
            extended.kind == RefKind::App && extended.id == app.id && extended.arch == app.arch
        }) else {
            return Ok(Some("ExtensionOf names another"));
        };
        if extended.branch != app.branch
            && deployed_or_unclear(self.installation, &extended)?.is_some()
        {
            return Ok(Some("of another branch still installed"));
        }
        let own = match deployment_origin(deployment) {
            Ok(Some(own)) => own,
            Ok(None) => return Ok(Some("no origin")),
            Err(error) => {
                tracing::warn!(%app, %reference, %error, "an extension's origin unreadable: what goes unknown");
                return Err(Why::Unclear);
            }
        };
        if own != self.origin()? {
            return Ok(Some("another origin"));
        }
        Ok(None)
    }
}

/// An application deployed, and the extensions that go with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Related {
    /// The application's deployment.
    pub deployment: PathBuf,
    /// Each extension that goes with it, and its deployment.
    pub extensions: Vec<(FlatpakRef, PathBuf)>,
}

/// The application `app` deployed in `installation`, and the extensions
/// installed there that go with it: named by its deployment's metadata now
/// (none when it has no metadata), as libflatpak names them, and each, by
/// its own metadata and data, the application's and from its remote. What
/// goes is removed as root by flatpak's helper: an application's metadata never
/// makes another's go.
///
/// # Errors
///
/// [`Why::NotFound`] when the application is no longer deployed;
/// [`Why::Unclear`] when what goes cannot be told: its deployment, its
/// metadata or origin, an extension's folder, metadata or origin, or the
/// runtimes' folder unreadable.
pub fn related(installation: &Path, app: &FlatpakRef) -> Result<Related, Why> {
    let deployed = match deployment(&app.folder(installation)) {
        Ok(Some(deployed)) => deployed,
        Ok(None) => {
            tracing::info!(%app, "no longer deployed");
            return Err(Why::NotFound);
        }
        Err(_) => return Err(Why::Unclear),
    };
    let metadata =
        match okapi_platform::read_regular_capped(&deployed.join(METADATA), METADATA_LIMIT) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::info!(%app, "no metadata: no extension goes with it");
                String::new()
            }
            Err(error) => {
                tracing::warn!(%app, %error, "its metadata unreadable: what goes unknown");
                return Err(Why::Unclear);
            }
        };
    let extensions = extensions_deleted(&metadata, &app.branch);
    let runtimes = if extensions.iter().any(|extension| extension.subdirectories) {
        runtime_ids(installation, app)?
    } else {
        Vec::new()
    };
    let mut owner = Owner {
        installation,
        app,
        deployment: &deployed,
        origin: None,
    };
    let mut found: Vec<(FlatpakRef, PathBuf)> = Vec::new();
    for extension in extensions {
        for branch in &extension.branches {
            let Some(base) = FlatpakRef::new(RefKind::Runtime, &extension.name, &app.arch, branch)
            else {
                continue;
            };
            // As libflatpak: its subdirectories only when it is not itself
            // installed.
            let candidates = if deployed_or_unclear(installation, &base)?.is_some() {
                vec![base]
            } else if extension.subdirectories {
                let prefix = format!("{}.", extension.name);
                runtimes
                    .iter()
                    .filter(|id| id.starts_with(&prefix))
                    .filter_map(|id| FlatpakRef::new(RefKind::Runtime, id, &app.arch, branch))
                    .collect()
            } else {
                Vec::new()
            };
            for reference in candidates {
                if found.iter().any(|(known, _)| *known == reference) {
                    continue;
                }
                let Some(extension_deployed) = deployed_or_unclear(installation, &reference)?
                else {
                    continue;
                };
                if let Some(reason) = owner.not_its_own(&reference, &extension_deployed)? {
                    tracing::info!(%app, %reference, reason, "an extension kept: not the application's");
                    continue;
                }
                found.push((reference, extension_deployed));
            }
        }
    }
    let references: Vec<String> = found
        .iter()
        .map(|(reference, _)| reference.to_string())
        .collect();
    tracing::info!(%app, ?references, "the extensions that go with it");
    Ok(Related {
        deployment: deployed,
        extensions: found,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_is_its_kind_id_arch_and_branch_each_a_name() {
        let app = FlatpakRef::app("org.gimp.GIMP", "x86_64", "stable").unwrap();
        assert_eq!(app.to_string(), "app/org.gimp.GIMP/x86_64/stable");
        assert_eq!(
            FlatpakRef::new(RefKind::Runtime, "org.gimp.GIMP.Locale", "x86_64", "stable")
                .unwrap()
                .to_string(),
            "runtime/org.gimp.GIMP.Locale/x86_64/stable"
        );
        // Catches a part that would climb or add a level.
        for (id, arch, branch) in [("..", "x", "y"), ("a/b", "x", "y"), ("a", "", "y")] {
            assert_eq!(
                FlatpakRef::app(id, arch, branch),
                None,
                "{id} {arch} {branch}"
            );
        }
    }

    #[test]
    fn the_extensions_that_go_are_those_libflatpak_deletes_on_their_branches() {
        // Catches an extension kept that flatpak would remove, one removed
        // that it keeps, a branch taken from the application when the
        // extension names its own, a tag left in the name.
        let metadata = "[Application]\nname=org.gimp.GIMP\n\n\
            [Extension org.gimp.GIMP.Locale]\ndirectory=share/runtime/locale\nlocale-subset=true\n\n\
            [Extension org.gimp.GIMP.Debug]\ndirectory=lib/debug\n\n\
            [Extension org.gimp.GIMP.Plugin]\nautodelete=true\nversions=2.40;3\nsubdirectories=true\n\n\
            [Extension org.freedesktop.Platform.GL@gl]\nversion=24.08\n\n\
            [Extension org.gimp.GIMP.Help]\nautodelete=false\nversion=1\n\n\
            [Extension org.gimp.GIMP.Data@x]\nautodelete=1\n\n\
            [Extension org.gimp.GIMP.Fonts.Locale]\n\n\
            [Extension org.gimp.GIMP.Both]\nautodelete=true\nversion=9\nversions=1;2;\n\n\
            [Extension org.gimp.GIMP.Lang]\nlocale-subset=true\n\n\
            [Extension org.gimp.GIMP.Twice]\nautodelete=true\nautodelete=false\n";
        let names: Vec<(String, Vec<String>, bool)> = extensions_deleted(metadata, "stable")
            .into_iter()
            .map(|extension| (extension.name, extension.branches, extension.subdirectories))
            .collect();
        let owned = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
        assert_eq!(
            names,
            [
                ("org.gimp.GIMP.Locale".to_owned(), owned(&["stable"]), false),
                ("org.gimp.GIMP.Debug".to_owned(), owned(&["stable"]), false),
                (
                    "org.gimp.GIMP.Plugin".to_owned(),
                    owned(&["2.40", "3"]),
                    true
                ),
                ("org.gimp.GIMP.Data".to_owned(), owned(&["stable"]), false),
                // By its name alone, keys or none.
                (
                    "org.gimp.GIMP.Fonts.Locale".to_owned(),
                    owned(&["stable"]),
                    false
                ),
                // `versions` first, an empty item dropped.
                ("org.gimp.GIMP.Both".to_owned(), owned(&["1", "2"]), false),
                // A subset of locales goes, whatever its name.
                ("org.gimp.GIMP.Lang".to_owned(), owned(&["stable"]), false),
            ]
        );
    }
}
