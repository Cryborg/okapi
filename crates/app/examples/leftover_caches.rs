//! What the leftovers by name (IDEE-8 c2, caches) would propose on this
//! machine, measured before any of it is coded: each folder of the cache
//! whose name is that of one deb package removed, whether something
//! installed resembles it, when it was last written and read, and what it
//! takes. `cargo run -p okapi-app --example leftover_caches`. A development
//! tool: it reads, it never moves nor removes.
#![expect(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "a development tool prints and fails by panicking"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use okapi_rules::Variable;
use okapi_store::SeenApps;
use okapi_tools::apps::dpkg;

/// How long a folder must have been left alone to be proposed, in days.
const LEFT_ALONE_DAYS: u64 = 90;
const HOURS_PER_DAY: u64 = 24;
const LEFT_ALONE: Duration = Duration::from_hours(LEFT_ALONE_DAYS * HOURS_PER_DAY);
/// The most entries walked in one folder.
const MOST_ENTRIES: u64 = 100_000;
/// The shortest name a resemblance by containing counts for.
const CONTAINED_FROM: usize = 4;
/// The most of a log read.
const LOG_LIMIT: usize = 16 * 1024 * 1024;
const DPKG_LOGS: [&str; 2] = ["/var/log/dpkg.log", "/var/log/dpkg.log.1"];
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// A name as compared: lower case, without separators.
fn normal(name: &str) -> String {
    name.chars()
        .filter(|character| !matches!(character, '-' | '_' | '.' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// The installed name `candidate` resembles, if one.
fn resembling<'a>(candidate: &str, installed: &'a BTreeSet<String>) -> Option<&'a str> {
    let candidate = normal(candidate);
    installed
        .iter()
        .find(|name| {
            let name = normal(name);
            let shortest = name.len().min(candidate.len());
            name == candidate
                || (shortest >= CONTAINED_FROM
                    && (name.contains(&candidate) || candidate.contains(&name)))
        })
        .map(String::as_str)
}

/// What a walk of a folder read, without following a link nor leaving its
/// device.
struct Walked {
    entries: u64,
    bytes: u64,
    newest_change: SystemTime,
    /// Of its plain files only; `None` without one.
    newest_access: Option<SystemTime>,
}

fn walk(folder: &Path) -> Option<Walked> {
    let root = fs::symlink_metadata(folder).ok()?;
    let mut walked = Walked {
        entries: 0,
        bytes: 0,
        newest_change: changed(&root),
        newest_access: None,
    };
    let mut waiting = vec![folder.to_path_buf()];
    while let Some(directory) = waiting.pop() {
        for entry in fs::read_dir(&directory).ok()? {
            let path = entry.ok()?.path();
            let metadata = fs::symlink_metadata(&path).ok()?;
            if metadata.dev() != root.dev() {
                return None;
            }
            walked.entries += 1;
            if walked.entries > MOST_ENTRIES {
                return None;
            }
            walked.bytes += metadata.blocks() * 512;
            walked.newest_change = walked.newest_change.max(changed(&metadata));
            if metadata.is_file() {
                let accessed = metadata.accessed().ok()?;
                walked.newest_access = Some(
                    walked
                        .newest_access
                        .map_or(accessed, |newest| newest.max(accessed)),
                );
            } else if metadata.is_dir() {
                waiting.push(path);
            }
        }
    }
    Some(walked)
}

/// When `metadata`'s inode last changed.
fn changed(metadata: &fs::Metadata) -> SystemTime {
    let seconds = u64::try_from(metadata.ctime()).unwrap_or(0);
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

fn days(since: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(since).unwrap_or_default().as_secs() / SECONDS_PER_DAY
}

/// Who left something, by the name it is known by (lower case): the deb
/// packages removed (dpkg's log, `rc`) and those Okapi saw.
fn former_owners(status: &dpkg::Status) -> BTreeMap<String, BTreeSet<String>> {
    let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut own = |name: &str, package: &str| {
        owners
            .entry(name.to_lowercase())
            .or_default()
            .insert(package.to_owned());
    };
    for log in DPKG_LOGS {
        match okapi_platform::read_regular_capped(Path::new(log), LOG_LIMIT) {
            Ok(bytes) => {
                for package in dpkg::parse_removals(&String::from_utf8_lossy(&bytes)) {
                    own(&package, &package);
                }
            }
            Err(error) => println!("{log}: {error}"),
        }
    }
    for package in &status.residual {
        own(package, package);
    }
    match okapi_platform::state_directory().map(|directory| SeenApps::load(&directory)) {
        Some(Ok(seen)) => {
            for card in seen.apps().iter().filter(|card| card.source == "deb") {
                let package = card.id.split(':').next().unwrap_or(&card.id);
                for name in card.names.iter().chain(std::iter::once(&card.shown)) {
                    own(name, package);
                }
            }
        }
        Some(Err(error)) => println!("applications seen unreadable: {error}"),
        None => println!("no state directory"),
    }
    owners
}

/// The names installed as c2 would weigh them: the packages, and the PATH.
fn installed_names(installed: &BTreeSet<String>) -> BTreeSet<String> {
    let mut names = installed.clone();
    for folder in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if let Ok(entries) = fs::read_dir(&folder) {
            names.extend(
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned()),
            );
        }
    }
    names
}

/// What c2 would say of the cache `folder`, owned by `packages`, and what
/// it would free when proposed.
fn verdict(
    folder: &Path,
    packages: &[&String],
    installed: &BTreeSet<String>,
    names: &BTreeSet<String>,
    now: SystemTime,
) -> (String, Option<u64>) {
    let name = folder.file_name().unwrap().to_string_lossy();
    if packages.len() > 1 {
        return ("ambiguous".to_owned(), None);
    }
    if installed.contains(packages[0]) {
        return ("owner installed".to_owned(), None);
    }
    if let Some(like) = resembling(&name, names) {
        return (format!("resembles {like}"), None);
    }
    let Some(walked) = walk(folder) else {
        return ("not walked whole".to_owned(), None);
    };
    let left = now.checked_sub(LEFT_ALONE).is_some_and(|limit| {
        walked.newest_change <= limit && walked.newest_access.is_some_and(|access| access <= limit)
    });
    let said = format!(
        "{}: written {} d ago, read {} d ago, {} entries, {} bytes",
        if left { "PROPOSED" } else { "recent" },
        days(walked.newest_change, now),
        walked.newest_access.map_or_else(
            || "never".to_owned(),
            |access| days(access, now).to_string()
        ),
        walked.entries,
        walked.bytes
    );
    (said, left.then_some(walked.bytes))
}

fn main() {
    let now = SystemTime::now();
    let variables = okapi_platform::template_variables().unwrap();
    let cache = variables.value_of(Variable::Cache).unwrap().to_path_buf();
    let status = dpkg::parse_status(
        &fs::read_to_string(Path::new(okapi_tools::packages::dpkg::DPKG_DIRECTORY).join("status"))
            .unwrap(),
    );
    let installed: BTreeSet<String> = status
        .installed
        .iter()
        .map(|package| package.name.clone())
        .chain(status.unsettled.iter().cloned())
        .collect();
    let owners = former_owners(&status);
    println!(
        "{} former owners' names, {} packages installed, cache {}",
        owners.len(),
        installed.len(),
        cache.display()
    );
    let names = installed_names(&installed);
    let mut folders: Vec<PathBuf> = fs::read_dir(&cache)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()))
        .collect();
    folders.sort();
    let (mut proposed, mut freed) = (0_usize, 0_u64);
    for folder in folders {
        let name = folder.file_name().unwrap().to_string_lossy().into_owned();
        let Some(packages) = owners
            .get(&name.to_lowercase())
            .filter(|_| !name.starts_with('.'))
        else {
            continue;
        };
        let packages: Vec<&String> = packages.iter().collect();
        let (said, bytes) = verdict(&folder, &packages, &installed, &names, now);
        if let Some(bytes) = bytes {
            proposed += 1;
            freed += bytes;
        }
        println!("~/.cache/{name} (owner {packages:?}): {said}");
    }
    println!("{proposed} folders proposed, {freed} bytes");
}
