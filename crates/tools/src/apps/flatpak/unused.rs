//! The runtimes and extensions of flatpak's system installation that no
//! application uses any more, as `flatpak uninstall --unused` tells them
//! (`flatpak_dir_list_unused_refs`), but stricter where flatpak broke
//! applications: every installation read counts, not only the caller's; an
//! extension any reference reaches is used, `autoprune-unless` or not; a
//! runtime that says what it extends is kept while that is used; and when a
//! part of any installation cannot be read, none is told unused. Each
//! comes with what its removal would give back, about: its files are hard
//! links to the repository's objects, shared with the other deployments.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    Active, FlatpakRef, Installation, KeyGroup, METADATA, METADATA_LIMIT, REPOSITORY, RefKind,
    active, extension_points, key_file_groups, kind_ids, presence, subfolders,
};

/// The groups of a metadata that name what a reference uses.
const APPLICATION_GROUP: &str = "Application";
const RUNTIME_GROUP: &str = "Runtime";
const EXTRA_DATA_GROUP: &str = "Extra Data";
const EXTENSION_OF_GROUP: &str = "ExtensionOf";
const KEY_RUNTIME: &str = "runtime";
const KEY_SDK: &str = "sdk";
const KEY_REF: &str = "ref";
const KEY_NO_RUNTIME: &str = "NoRuntime";
/// Where an installation's repository keeps its settings, and the runtimes
/// pinned there (`flatpak pin`): `xa.pinned` of `[core]`, patterns split by
/// `;`.
const REPOSITORY_CONFIG: &str = "config";
const CORE_GROUP: &str = "core";
const KEY_PINNED: &str = "xa.pinned";
const PATTERN_SEPARATOR: char = ';';
/// What a pattern may start with: it names runtimes only.
const RUNTIME_PREFIX: &str = "runtime/";
const APP_PREFIX: &str = "app/";
const PATTERN_PARTS: usize = 3;
const WILDCARD: char = '*';
/// The most of a repository's settings read.
const CONFIG_LIMIT: usize = 64 * 1024;
/// The most entries and depth a deployment is walked to tell its room: a
/// runtime holds some tens of thousands.
const MOST_ENTRIES: usize = 1_000_000;
const MOST_DEPTH: usize = 64;
/// A block, as `st_blocks` counts them.
const BLOCK_BYTES: u64 = 512;

/// A runtime of the system's installation no application uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusedRuntime {
    pub reference: FlatpakRef,
    /// Its deployment in use.
    pub deployment: PathBuf,
}

/// What the installations read gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unused {
    /// The system's runtimes no application uses, in the order of their
    /// references.
    pub runtimes: Vec<UnusedRuntime>,
    /// The runtimes of the user's installation no application uses: never
    /// proposed (no service changes it), only counted.
    pub for_account_only: usize,
}

/// What a deployment's metadata says it uses or extends.
#[derive(Debug, Default)]
struct Uses {
    /// `runtime=`, `sdk=`, and the runtime an extension of extra data
    /// needs.
    refs: Vec<FlatpakRef>,
    /// Its extension points, by name and branches.
    extensions: Vec<(String, Vec<String>, bool)>,
    /// What its `[ExtensionOf] ref=` names.
    extends: Extends,
}

/// What a deployment says it extends.
#[derive(Debug, Default)]
enum Extends {
    #[default]
    Nothing,
    Ref(FlatpakRef),
    /// A reference that cannot be read: it is never told unused.
    Unreadable,
}

/// A deployment in use, where it is.
#[derive(Debug)]
struct Deployed {
    reference: FlatpakRef,
    installation: usize,
    deployment: PathBuf,
}

/// Every deployment of the installations, and what each uses.
struct Read {
    deployed: Vec<Deployed>,
    uses: Vec<Uses>,
    /// Each installation's pinned patterns.
    pinned: Vec<Vec<Pattern>>,
}

/// The runtimes of the system's installation no application uses, as
/// `installations` hold them.
///
/// # Errors
///
/// Why a part of an installation could not be read: then none is told
/// unused (traced).
pub fn unused_runtimes(installations: &[Installation]) -> Result<Unused, String> {
    let _span = tracing::info_span!("flatpak_unused").entered();
    let read = read_all(installations).inspect_err(|why| {
        tracing::warn!(why, "a flatpak installation not read whole: no runtime told unused");
    })?;
    let is_app = |deployed: &Deployed| deployed.reference.kind == RefKind::App;
    let pinned = |deployed: &Deployed| {
        deployed.reference.kind == RefKind::Runtime
            && read.pinned[deployed.installation]
                .iter()
                .any(|pattern| pattern.matches(&deployed.reference))
    };
    let system = |deployed: &Deployed| installations[deployed.installation].system;
    // Every runtime of another installation is a root of the system's, as
    // flatpak takes the user's: what it uses there stays.
    let used_by_all = used(&read, |deployed| {
        is_app(deployed) || pinned(deployed) || !system(deployed)
    });
    let used_by_apps = used(&read, |deployed| is_app(deployed) || pinned(deployed));
    let unused_in = |used: &BTreeSet<FlatpakRef>, index: usize| {
        let deployed = &read.deployed[index];
        deployed.reference.kind == RefKind::Runtime
            && !used.contains(&deployed.reference)
            && match &read.uses[index].extends {
                Extends::Nothing => true,
                Extends::Ref(extended) => !used.contains(extended),
                Extends::Unreadable => false,
            }
    };
    let runtimes: Vec<UnusedRuntime> = (0..read.deployed.len())
        .filter(|index| system(&read.deployed[*index]) && unused_in(&used_by_all, *index))
        .map(|index| UnusedRuntime {
            reference: read.deployed[index].reference.clone(),
            deployment: read.deployed[index].deployment.clone(),
        })
        .collect();
    let for_account_only = (0..read.deployed.len())
        .filter(|index| {
            let installation = &installations[read.deployed[*index].installation];
            !installation.system && !installation.declared && unused_in(&used_by_apps, *index)
        })
        .count();
    for runtime in &runtimes {
        tracing::info!(reference = %runtime.reference, "a runtime no application uses");
    }
    tracing::info!(
        deployed = read.deployed.len(),
        unused = runtimes.len(),
        for_account_only,
        "flatpak's runtimes judged"
    );
    Ok(Unused {
        runtimes,
        for_account_only,
    })
}

/// Every reference reached from the deployments `root` takes, through what
/// their metadata says they use and the extensions their extension points
/// name (any architecture: an i386 one kept is no harm).
fn used(read: &Read, root: impl Fn(&Deployed) -> bool) -> BTreeSet<FlatpakRef> {
    let mut reached = BTreeSet::new();
    let mut queue: VecDeque<FlatpakRef> = read
        .deployed
        .iter()
        .filter(|deployed| root(deployed))
        .map(|deployed| deployed.reference.clone())
        .collect();
    while let Some(reference) = queue.pop_front() {
        if !reached.insert(reference.clone()) {
            continue;
        }
        // The same reference may be deployed in several installations:
        // what each says it uses counts.
        for (index, deployed) in read.deployed.iter().enumerate() {
            if deployed.reference != reference {
                continue;
            }
            let said = &read.uses[index];
            queue.extend(said.refs.iter().cloned());
            for (name, branches, subdirectories) in &said.extensions {
                let prefix = format!("{name}.");
                queue.extend(
                    read.deployed
                        .iter()
                        .map(|other| &other.reference)
                        .filter(|other| {
                            other.kind == RefKind::Runtime
                                && branches.contains(&other.branch)
                                && (other.id == *name
                                    || (*subdirectories && other.id.starts_with(&prefix)))
                        })
                        .cloned(),
                );
            }
        }
    }
    reached
}

/// Reads every deployment of `installations`, each one's metadata, and
/// each installation's pins.
///
/// # Errors
///
/// Why a part could not be read.
fn read_all(installations: &[Installation]) -> Result<Read, String> {
    let mut read = Read {
        deployed: Vec::new(),
        uses: Vec::new(),
        pinned: Vec::new(),
    };
    for (index, installation) in installations.iter().enumerate() {
        if !presence(installation)? {
            read.pinned.push(Vec::new());
            continue;
        }
        read.pinned.push(pins(&installation.path)?);
        for kind in [RefKind::App, RefKind::Runtime] {
            for (reference, deployment) in deployments_of(&installation.path, kind)? {
                read.uses.push(uses_of(&reference, &deployment)?);
                read.deployed.push(Deployed {
                    reference,
                    installation: index,
                    deployment,
                });
            }
        }
    }
    Ok(read)
}

/// The names in `folder`, listed whole, none refused; none when it is gone.
///
/// # Errors
///
/// It, or an entry of it, could not be read, or a name was refused.
fn names_in(folder: &Path) -> Result<Vec<String>, String> {
    match subfolders(folder) {
        Ok(listed) if listed.whole && !listed.refused => Ok(listed.names),
        Ok(_) => Err(format!("{} not listed whole", folder.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("{}: {error}", folder.display())),
    }
}

/// Every deployment in use of `kind` in the installation at `installation`.
///
/// # Errors
///
/// A folder, or an `active`, that could not be read, a name refused, or an
/// `active` that is not a folder beside it.
fn deployments_of(
    installation: &Path,
    kind: RefKind,
) -> Result<Vec<(FlatpakRef, PathBuf)>, String> {
    let listed = kind_ids(installation, kind)?;
    if listed.refused {
        return Err(format!(
            "a name refused in {}",
            installation.join(kind.folder()).display()
        ));
    }
    let mut found = Vec::new();
    for id in listed.names {
        let folder = installation.join(kind.folder()).join(&id);
        for arch in names_in(&folder)? {
            for branch in names_in(&folder.join(&arch))? {
                let branch_folder = folder.join(&arch).join(&branch);
                let reference = FlatpakRef::new(kind, &id, &arch, &branch)
                    .ok_or_else(|| format!("{} names no reference", branch_folder.display()))?;
                match active(&branch_folder) {
                    Ok(Active::At(deployment)) => found.push((reference, deployment)),
                    Ok(Active::None) => {}
                    Ok(Active::Refused) => {
                        return Err(format!("{} refused", branch_folder.display()));
                    }
                    Err(error) => return Err(format!("{}: {error}", branch_folder.display())),
                }
            }
        }
    }
    Ok(found)
}

/// The values of `key` in every group `name` of `groups`: a group written
/// twice is not merged by [`key_file_groups`].
fn values<'a>(
    groups: &'a [KeyGroup],
    name: &'a str,
    key: &'a str,
) -> impl Iterator<Item = &'a str> + 'a {
    groups
        .iter()
        .filter(move |group| group.name == name)
        .flat_map(move |group| group.values(key))
}

/// What the deployment of `reference` at `deployment` uses, as its metadata
/// says.
///
/// # Errors
///
/// Its metadata absent, unreadable or not UTF-8, or a runtime it names that
/// cannot be read.
fn uses_of(reference: &FlatpakRef, deployment: &Path) -> Result<Uses, String> {
    let path = deployment.join(METADATA);
    let bytes = okapi_platform::read_regular_capped(&path, METADATA_LIMIT)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let text =
        String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
    let groups = key_file_groups(&text);
    let runtime = |value: &str| {
        FlatpakRef::runtime_of(value)
            .ok_or_else(|| format!("{}: a runtime unreadable: {value:?}", path.display()))
    };
    let mut uses = Uses::default();
    let mut named: Vec<&str> = Vec::new();
    if reference.kind == RefKind::App {
        named.extend(values(&groups, APPLICATION_GROUP, KEY_RUNTIME));
    }
    named.extend(values(&groups, APPLICATION_GROUP, KEY_SDK));
    named.extend(values(&groups, RUNTIME_GROUP, KEY_SDK));
    let extra_data = groups
        .iter()
        .filter(|group| group.name == EXTRA_DATA_GROUP)
        .any(|group| !matches!(group.value(KEY_NO_RUNTIME), Some("true" | "1")));
    if extra_data {
        named.extend(values(&groups, EXTENSION_OF_GROUP, KEY_RUNTIME));
    }
    for value in named {
        uses.refs.push(runtime(value)?);
    }
    uses.extensions = extension_points(&text, &reference.branch)
        .into_iter()
        .map(|extension| (extension.name, extension.branches, extension.subdirectories))
        .collect();
    uses.extends = match values(&groups, EXTENSION_OF_GROUP, KEY_REF).last() {
        None => Extends::Nothing,
        Some(extended) => FlatpakRef::parse(extended).map_or_else(
            || {
                tracing::warn!(path = %path.display(), extended, "what an extension extends unreadable: kept");
                Extends::Unreadable
            },
            Extends::Ref,
        ),
    };
    Ok(uses)
}

/// One pattern of `xa.pinned`, as flatpak reads it for runtimes: its id,
/// arch and branch, each a glob where `*` stands for any name's characters
/// and a part left out for any.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    parts: Vec<String>,
}

impl Pattern {
    /// The pattern `text` writes, if flatpak reads it so.
    fn parse(text: &str) -> Option<Self> {
        if text.starts_with(APP_PREFIX) {
            return None;
        }
        let text = text.strip_prefix(RUNTIME_PREFIX).unwrap_or(text);
        let parts: Vec<String> = text.split('/').map(str::to_owned).collect();
        let allowed = |character: char| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | WILDCARD)
        };
        (parts.len() <= PATTERN_PARTS && parts.iter().all(|part| part.chars().all(allowed)))
            .then_some(Self { parts })
    }

    fn matches(&self, reference: &FlatpakRef) -> bool {
        [&reference.id, &reference.arch, &reference.branch]
            .into_iter()
            .enumerate()
            .all(|(index, value)| {
                self.parts
                    .get(index)
                    .is_none_or(|part| part.is_empty() || glob_matches(part, value))
            })
    }
}

/// Whether `value` matches `glob`, whose `*` stands for any run of a
/// name's characters (letters, digits, `-`, `_`, `.`).
fn glob_matches(glob: &str, value: &str) -> bool {
    let Some((head, rest)) = glob.split_once(WILDCARD) else {
        return glob == value;
    };
    let Some(after) = value.strip_prefix(head) else {
        return false;
    };
    let name_like = |character: char| {
        character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
    };
    // Each place the star may stop, as long as it ran over a name's
    // characters only.
    after
        .char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(after.len()))
        .take_while(|at| after[..*at].chars().all(name_like))
        .any(|at| glob_matches(rest, &after[at..]))
}

/// The runtimes pinned in the installation at `installation`.
///
/// # Errors
///
/// Its repository's settings unreadable or not UTF-8, or a pattern flatpak
/// would not read.
fn pins(installation: &Path) -> Result<Vec<Pattern>, String> {
    let path = installation.join(REPOSITORY).join(REPOSITORY_CONFIG);
    let bytes = match okapi_platform::read_regular_capped(&path, CONFIG_LIMIT) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let text =
        String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
    let groups = key_file_groups(&text);
    values(&groups, CORE_GROUP, KEY_PINNED)
        .flat_map(|value| value.split(PATTERN_SEPARATOR))
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(|pattern| {
            Pattern::parse(pattern)
                .ok_or_else(|| format!("{}: a pin unreadable: {pattern:?}", path.display()))
        })
        .collect()
}

/// About what removing the deployment at `deployment`, then pruning its
/// repository, gives back: the room of each file all of whose names are in
/// it, but one, the repository's object. A file another deployment shares
/// gives nothing. `None` when it could not be walked whole (a folder
/// unreadable, too large, stopped). Links are never followed, and another
/// device never entered.
#[must_use]
pub fn about_bytes(deployment: &Path, stop: &AtomicBool) -> Option<u64> {
    let root = std::fs::symlink_metadata(deployment)
        .ok()
        .filter(std::fs::Metadata::is_dir)?;
    // Each file met: how many of its names were seen, its links, its room.
    let mut files: HashMap<(u64, u64), (u64, u64, u64)> = HashMap::new();
    let mut folders = vec![(deployment.to_path_buf(), 0_usize)];
    let mut entries = 0_usize;
    while let Some((folder, depth)) = folders.pop() {
        if stop.load(Ordering::Relaxed) || depth > MOST_DEPTH {
            tracing::info!(path = %deployment.display(), depth, "a deployment not measured whole");
            return None;
        }
        let listing = std::fs::read_dir(&folder)
            .inspect_err(|error| tracing::info!(path = %folder.display(), %error, "a deployment's folder unreadable: its room unknown"))
            .ok()?;
        for entry in listing {
            entries += 1;
            if entries > MOST_ENTRIES {
                tracing::info!(path = %deployment.display(), "a deployment too large to measure");
                return None;
            }
            // Never followed: a link is told as such.
            let entry = entry.ok()?;
            let metadata = entry.metadata().ok()?;
            if metadata.dev() != root.dev() {
                continue;
            }
            if metadata.is_dir() {
                folders.push((entry.path(), depth + 1));
            } else if metadata.is_file() {
                let seen = files.entry((metadata.dev(), metadata.ino())).or_insert((
                    0,
                    metadata.nlink(),
                    metadata.blocks().saturating_mul(BLOCK_BYTES),
                ));
                seen.0 += 1;
            }
        }
    }
    Some(
        files
            .values()
            .filter(|(seen, links, _)| *links <= seen.saturating_add(1))
            .map(|(_, _, bytes)| *bytes)
            .fold(0, u64::saturating_add),
    )
}

#[cfg(test)]
mod tests;
