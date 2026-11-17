use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use okapi_platform::{FileIdentity, file_identity};
use thiserror::Error;

use crate::custom::CustomFolder;
use crate::paths::{
    canonicalize_longest_prefix, is_plain_absolute, lowercase, starts_with_ignoring_case,
};
use crate::report::{Category, Origin};
use crate::walk::IdentityReader;

/// How old an entry must be to be offered when the user has not chosen, in
/// minutes, as ages are written.
pub const DEFAULT_MIN_AGE_MINUTES: u64 = 60;
/// [`DEFAULT_MIN_AGE_MINUTES`] as a duration.
pub const DEFAULT_MIN_AGE: Duration = Duration::from_mins(DEFAULT_MIN_AGE_MINUTES);

const SECONDS_PER_MINUTE: u64 = 60;

/// `minutes` as a duration, if a duration can hold it: the one conversion of
/// an age written in minutes.
#[must_use]
pub fn duration_of_minutes(minutes: u64) -> Option<Duration> {
    Some(Duration::from_secs(
        minutes.checked_mul(SECONDS_PER_MINUTE)?,
    ))
}

/// The minimum age of `minutes` set in the settings, if the clock can reach
/// back that far from `now`: beyond it, nothing could be old enough, which
/// the caller says is the settings' fault, where they are known.
#[must_use]
pub fn settings_min_age(minutes: u64, now: SystemTime) -> Option<Duration> {
    duration_of_minutes(minutes).filter(|age| now.checked_sub(*age).is_some())
}

/// The prefix of an exclusion that matches file names by their extension.
const EXTENSION_PATTERN_PREFIX: &str = "*.";

/// An exclusion the user wrote cannot be used.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExclusionError {
    #[error("an exclusion cannot be empty")]
    Empty,

    #[error("`{}` is neither an absolute path without `..` nor a `*.ext` pattern", path.display())]
    Invalid { path: PathBuf },

    #[error("`{}` cannot be located: {error}", path.display())]
    Unresolvable { path: PathBuf, error: io::ErrorKind },
}

/// Whether the exclusion `entry` is written as an extension pattern
/// (`*.log`), whatever follows its prefix.
fn is_extension_pattern(entry: &Path) -> bool {
    entry
        .to_str()
        .is_some_and(|text| text.starts_with(EXTENSION_PATTERN_PREFIX))
}

/// An exclusion as written, its form checked.
enum Form {
    /// The suffix it matches names by, lower case, its dot included.
    Extension(String),
    /// An absolute path without `..`.
    Path(PathBuf),
}

impl Form {
    /// The form of `entry`.
    fn of(entry: PathBuf) -> Result<Self, ExclusionError> {
        if entry.as_os_str().is_empty() {
            return Err(ExclusionError::Empty);
        }
        let pattern = entry
            .to_str()
            .and_then(|text| text.strip_prefix(EXTENSION_PATTERN_PREFIX));
        let plain_path = is_plain_absolute(&entry);
        match pattern {
            Some(extension) if !extension.is_empty() && !extension.contains(['/', '\\', '*']) => {
                Ok(Self::Extension(format!(".{}", lowercase(extension))))
            }
            None if plain_path => Ok(Self::Path(entry)),
            _ => Err(ExclusionError::Invalid { path: entry }),
        }
    }
}

/// What the user never wants cleaned: paths (and everything below them), and
/// extensions written `*.log` (files, and directories with everything below
/// them, such as a `*.default-release` browser profile). Matching ignores case,
/// which can only exclude more; Unicode normalization forms are not folded.
/// Read by [`Exclusions::new`], an excluded directory that exists is also
/// known by its identity, which
/// sees through a bind mount that shows it at another path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exclusions {
    prefixes: Vec<PathBuf>,
    extensions: Vec<String>,
    identities: BTreeSet<FileIdentity>,
}

impl Exclusions {
    /// No exclusion.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether the exclusion `entry`, as written, may cover what lies at or
    /// below `root`, without reading anything: an extension always; a path
    /// at, above or below `root`, ignoring case.
    #[must_use]
    pub fn touches_lexically(entry: &Path, root: &Path) -> bool {
        is_extension_pattern(entry)
            || starts_with_ignoring_case(entry, root)
            || starts_with_ignoring_case(root, entry)
    }

    /// Where the exclusion `entry` really lies, as [`Self::new`] reads it:
    /// through its links, the part that does not exist yet kept as written;
    /// an extension pattern as written (never a path of the current
    /// directory).
    ///
    /// # Errors
    ///
    /// Any error but absence, and a dangling link in the path.
    pub fn real_location(entry: &Path) -> io::Result<PathBuf> {
        if is_extension_pattern(entry) {
            return Ok(entry.to_path_buf());
        }
        canonicalize_longest_prefix(entry)
    }

    /// Reads the exclusions the user wrote. A path is kept as written and as
    /// its real location at this instant, so an alias still protects what it
    /// points to: build the exclusions afresh for each scan.
    ///
    /// # Errors
    ///
    /// [`ExclusionError`] for an entry that is empty, relative, holds `..` or a
    /// wildcard in its extension (each would exclude more, or less, than
    /// intended), or whose real location cannot be read.
    pub fn new(entries: impl IntoIterator<Item = PathBuf>) -> Result<Self, ExclusionError> {
        let mut exclusions = Self::none();
        for entry in entries {
            match Form::of(entry)? {
                Form::Extension(extension) => exclusions.extensions.push(extension),
                Form::Path(entry) => {
                    let real = canonicalize_longest_prefix(&entry).map_err(|error| {
                        ExclusionError::Unresolvable {
                            path: entry.clone(),
                            error: error.kind(),
                        }
                    })?;
                    if let Some(identity) = fs::symlink_metadata(&real)
                        .ok()
                        .and_then(|metadata| file_identity(&metadata))
                    {
                        exclusions.identities.insert(identity);
                    }
                    exclusions.prefixes.push(real);
                    exclusions.prefixes.push(entry);
                }
            }
        }
        Ok(exclusions)
    }

    /// The exclusions as received, resolved already by whoever sent them:
    /// nothing is read, no identity known. For the privileged helper, which
    /// never reads a path its caller names: the interface sends it the real
    /// locations of the user's exclusions, and the engine compares only real
    /// locations. Without identities, another name of an excluded file (a
    /// hard link, whose data stays all the same) and an excluded directory
    /// shown below a root by a bind mount are not excluded.
    ///
    /// # Errors
    ///
    /// [`ExclusionError`] for an entry of a form [`Self::new`] refuses.
    pub fn as_written(entries: impl IntoIterator<Item = PathBuf>) -> Result<Self, ExclusionError> {
        let mut exclusions = Self::none();
        for entry in entries {
            match Form::of(entry)? {
                Form::Extension(extension) => exclusions.extensions.push(extension),
                Form::Path(entry) => exclusions.prefixes.push(entry),
            }
        }
        Ok(exclusions)
    }

    /// Whether the object of `identity` is an excluded directory.
    #[must_use]
    pub fn excludes_identity(&self, identity: FileIdentity) -> bool {
        self.identities.contains(&identity)
    }

    /// [`Self::excludes_by_identity`], identities read by the system.
    #[must_use]
    pub fn excludes_through_identity(&self, path: &Path) -> bool {
        self.excludes_by_identity(path, file_identity)
    }

    /// Whether `path` or one of its ancestors is an excluded directory by
    /// identity: the same directory shown at another path by a bind mount.
    pub(crate) fn excludes_by_identity(&self, path: &Path, identity_of: IdentityReader) -> bool {
        !self.identities.is_empty()
            && path.ancestors().any(|ancestor| {
                fs::symlink_metadata(ancestor)
                    .ok()
                    .and_then(|metadata| identity_of(&metadata))
                    .is_some_and(|identity| self.identities.contains(&identity))
            })
    }

    /// Whether `path` is excluded, by a prefix or by the extension of any of
    /// its components: an excluded directory covers what lies below it.
    #[must_use]
    pub fn excludes(&self, path: &Path) -> bool {
        let by_extension = path.components().any(|component| match component {
            Component::Normal(name) => self.excludes_name(name),
            _ => false,
        });
        by_extension
            || self
                .prefixes
                .iter()
                .any(|prefix| starts_with_ignoring_case(path, prefix))
    }

    /// Whether an entry named `name` is excluded by its extension.
    #[must_use]
    pub fn excludes_name(&self, name: &OsStr) -> bool {
        let name = lowercase(&name.to_string_lossy());
        self.extensions
            .iter()
            .any(|extension| name.ends_with(extension.as_str()))
    }

    /// Whether an excluded path lies below `root`: moving `root` whole would
    /// take it along.
    #[must_use]
    pub fn excludes_below(&self, root: &Path) -> bool {
        self.prefixes
            .iter()
            .any(|prefix| starts_with_ignoring_case(prefix, root))
    }
}

/// What a scan honours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    now: SystemTime,
    min_age: Duration,
    exclusions: Exclusions,
    /// The categories walked; none means all.
    categories: BTreeSet<Category>,
    /// The rules walked, when they are chosen one by one: an empty set walks
    /// nothing, never all.
    origins: Option<BTreeSet<Origin>>,
    /// The folders the user chose to have emptied, planned after the rules.
    custom_folders: Vec<CustomFolder>,
}

impl ScanOptions {
    /// `now` is the instant ages are measured from; `min_age` applies to every
    /// rule, which may ask for more.
    #[must_use]
    pub const fn new(now: SystemTime, min_age: Duration, exclusions: Exclusions) -> Self {
        Self {
            now,
            min_age,
            exclusions,
            categories: BTreeSet::new(),
            origins: None,
            custom_folders: Vec::new(),
        }
    }

    /// Plans `folders` too, after the rules: each one's contents a group of
    /// the [`Category::Custom`], never checked by default, walked as the
    /// categories chosen say ([`Self::with_categories`]).
    #[must_use]
    pub fn with_custom_folders(mut self, folders: Vec<CustomFolder>) -> Self {
        self.custom_folders = folders;
        self
    }

    /// The folders the user chose, as they stand now: what a cleaning
    /// still empties.
    pub(crate) fn custom_folders(&self) -> &[CustomFolder] {
        &self.custom_folders
    }

    /// The folders the user chose that the categories and rules chosen
    /// choose ([`Self::chooses`]): what a scan plans.
    pub(crate) fn chosen_custom_folders(&self) -> Vec<&CustomFolder> {
        self.custom_folders
            .iter()
            .filter(|folder| {
                self.chooses(&[Origin::Custom {
                    folder: (*folder).clone(),
                }])
            })
            .collect()
    }

    /// Walks only the roots of a group with one of `categories` (none: all).
    /// The groups are formed over every rule first, so a root shared with a
    /// category left out keeps the attributes of all its rules (invariant 5),
    /// and every root still reserves its place below another one.
    #[must_use]
    pub fn with_categories(mut self, categories: BTreeSet<Category>) -> Self {
        self.categories = categories;
        self
    }

    /// The instant given to [`ScanOptions::new`], ages are measured from: for
    /// the options a scan was made with, when it started.
    #[must_use]
    pub const fn now(&self) -> SystemTime {
        self.now
    }

    /// The minimum age given to [`ScanOptions::new`], every rule's at least:
    /// what an operation a daemon runs (Docker's prune) never goes under.
    #[must_use]
    pub const fn min_age(&self) -> Duration {
        self.min_age
    }

    /// The categories walked; none means all.
    #[must_use]
    pub const fn categories(&self) -> &BTreeSet<Category> {
        &self.categories
    }

    /// Walks only the roots of a group with one of `origins`, as
    /// [`Self::with_categories`] does for categories, and on top of them; but
    /// an empty set walks nothing.
    #[must_use]
    pub fn with_origins(mut self, origins: BTreeSet<Origin>) -> Self {
        self.origins = Some(origins);
        self
    }

    /// Whether a group of rules `origins` is walked: one of them is itself
    /// chosen, by its category and, when rules are chosen one by one, as a
    /// rule. Two rules sharing a root, one chosen by its category and the
    /// other as a rule, choose nothing.
    pub(crate) fn chooses(&self, origins: &[Origin]) -> bool {
        origins.iter().any(|origin| {
            Category::is_chosen(&BTreeSet::from([origin.category()]), &self.categories)
                && self
                    .origins
                    .as_ref()
                    .is_none_or(|chosen| chosen.contains(origin))
        })
    }

    /// The exclusions honoured.
    #[must_use]
    pub const fn exclusions(&self) -> &Exclusions {
        &self.exclusions
    }

    /// The instant after which an entry is too recent to offer, for a rule that
    /// asks for `rule_age`: the stricter of the two ages. `None` when the age
    /// reaches before the clock's origin, so nothing is old enough.
    #[must_use]
    pub fn cutoff(&self, rule_age: Option<Duration>) -> Option<SystemTime> {
        let age = rule_age.map_or(self.min_age, |rule_age| rule_age.max(self.min_age));
        self.now.checked_sub(age)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_become_an_age_the_clock_can_reach() {
        assert_eq!(duration_of_minutes(2), Some(Duration::from_secs(120)));
        assert_eq!(duration_of_minutes(u64::MAX), None);
        let now = SystemTime::now();
        assert_eq!(settings_min_age(90, now), Some(Duration::from_mins(90)));
        // The most minutes a duration holds: more seconds than any system's
        // clock counts back (at most a signed 64-bit count of seconds).
        let beyond = u64::MAX / SECONDS_PER_MINUTE;
        assert!(duration_of_minutes(beyond).is_some());
        assert_eq!(
            settings_min_age(beyond, now),
            None,
            "fits in a duration, beyond what the clock can go back to"
        );
        assert_eq!(settings_min_age(u64::MAX, now), None);
    }

    fn exclusions(entries: &[&str]) -> Result<Exclusions, ExclusionError> {
        Exclusions::new(entries.iter().map(PathBuf::from))
    }

    #[test]
    fn rejects_entries_that_would_exclude_too_much() {
        assert_eq!(exclusions(&[""]), Err(ExclusionError::Empty));
        for entry in [
            "relative/path",
            "*.",
            "*./x",
            "*.a\\b",
            "*.*",
            "*.t*",
            "/data/../etc",
        ] {
            assert_eq!(
                exclusions(&[entry]),
                Err(ExclusionError::Invalid {
                    path: PathBuf::from(entry)
                }),
                "{entry}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn excludes_by_prefix_and_extension_ignoring_case() {
        let exclusions = exclusions(&["/data/Keep", "*.log"]).unwrap();
        assert!(exclusions.excludes(Path::new("/data/keep")));
        assert!(exclusions.excludes(Path::new("/data/keep/x/y")));
        assert!(!exclusions.excludes(Path::new("/data/keeper")));
        assert!(exclusions.excludes(Path::new("/tmp/build.LOG")));
        assert!(
            exclusions.excludes(Path::new("/tmp/run.log/part")),
            "below an excluded directory"
        );
        assert!(!exclusions.excludes(Path::new("/tmp/catalog")));
        assert!(!exclusions.excludes(Path::new("/tmp/catalog/x")));
        assert!(!Exclusions::none().excludes(Path::new("/data/keep")));
    }

    #[cfg(unix)]
    #[test]
    fn an_exclusion_below_a_root_or_an_entry_by_its_name_is_seen() {
        // Catches a folder moved whole with what the user excludes inside.
        let exclusions = exclusions(&["/data/Keep/vault", "*.kdbx"]).unwrap();
        assert!(exclusions.excludes_below(Path::new("/data/keep")));
        assert!(exclusions.excludes_below(Path::new("/data")));
        assert!(!exclusions.excludes_below(Path::new("/data/other")));
        assert!(exclusions.excludes_name(OsStr::new("Secrets.KDBX")));
        assert!(!exclusions.excludes_name(OsStr::new("kdbx.txt")));
        assert!(!exclusions.excludes_name(OsStr::new("x.kdbx.bak")));
    }

    #[cfg(unix)]
    #[test]
    fn an_exclusion_written_through_an_alias_protects_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(directory.path()).unwrap();
        std::fs::create_dir(real.join("target")).unwrap();
        std::os::unix::fs::symlink(real.join("target"), real.join("alias")).unwrap();
        let exclusions = Exclusions::new([real.join("alias")]).unwrap();
        assert!(exclusions.excludes(&real.join("target/file")));
    }

    #[cfg(unix)]
    #[test]
    fn an_exclusion_through_a_dangling_link_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(directory.path()).unwrap();
        std::os::unix::fs::symlink(real.join("nowhere"), real.join("dangling")).unwrap();
        let entry = real.join("dangling/keep");
        assert!(matches!(
            Exclusions::new([entry.clone()]),
            Err(ExclusionError::Unresolvable { path, .. }) if path == entry
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_excluded_directory_is_known_by_identity_below_it() {
        let directory = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(directory.path()).unwrap();
        std::fs::create_dir_all(real.join("keep/inner")).unwrap();
        std::fs::create_dir(real.join("other")).unwrap();
        let exclusions = Exclusions::new([real.join("keep")]).unwrap();
        let identity_of_keep =
            file_identity(&std::fs::symlink_metadata(real.join("keep")).unwrap()).unwrap();
        assert!(exclusions.excludes_identity(identity_of_keep));
        assert!(exclusions.excludes_by_identity(&real.join("keep/inner"), file_identity));
        assert!(!exclusions.excludes_by_identity(&real.join("other"), file_identity));
        assert!(!Exclusions::none().excludes_by_identity(&real.join("keep"), file_identity));
    }

    #[cfg(unix)]
    #[test]
    fn an_exclusion_touches_a_root_at_above_or_below_it_as_written() {
        let root = Path::new("/var/crash");
        let touches = |entry: &str| Exclusions::touches_lexically(Path::new(entry), root);
        assert!(touches("*.log"));
        assert!(touches("/var/crash/keep"));
        assert!(touches("/var"), "above it: excludes it whole");
        assert!(touches("/VAR"), "above it, case ignored");
        assert!(!touches("/va"), "whole components");
        assert!(touches("/"));
        assert!(touches("/VAR/Crash/keep"), "case ignored, as matching does");
        assert!(touches("/var//crash/./keep/"), "spelled otherwise");
        assert!(!touches("/var/crashes"), "a name beside it");
        assert!(!touches("/home/ana/keep"));
        assert!(!touches("/root/x"));
    }

    /// Received resolved, an exclusion is never read again: a dangling link
    /// is no error, a live one protects only its own path, never where it
    /// leads nor that place's identity.
    #[cfg(unix)]
    #[test]
    fn exclusions_as_written_read_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(directory.path()).unwrap();
        std::fs::create_dir(real.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(real.join("elsewhere"), real.join("live")).unwrap();
        std::os::unix::fs::symlink(real.join("nowhere"), real.join("dangling")).unwrap();
        let exclusions =
            Exclusions::as_written([real.join("live"), real.join("dangling/x"), "*.LOG".into()])
                .unwrap();
        assert!(exclusions.excludes(&real.join("live/x")));
        assert!(exclusions.excludes(&real.join("a.log")));
        assert!(
            !exclusions.excludes(&real.join("elsewhere/x")),
            "never where it leads"
        );
        let identity =
            file_identity(&std::fs::symlink_metadata(real.join("elsewhere")).unwrap()).unwrap();
        assert!(!exclusions.excludes_identity(identity));
        for refused in ["", "relative", "/a/../b", "*."] {
            assert!(
                Exclusions::as_written([PathBuf::from(refused)]).is_err(),
                "{refused}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_real_location_of_an_exclusion_is_read_through_its_links() {
        let directory = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(directory.path()).unwrap();
        let target = real.join("var/crash");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, real.join("alias")).unwrap();
        std::os::unix::fs::symlink(real.join("nowhere"), real.join("dangling")).unwrap();
        assert_eq!(
            Exclusions::real_location(&real.join("alias/keep")).unwrap(),
            target.join("keep"),
            "through a link, the rest as written"
        );
        assert!(Exclusions::real_location(&real.join("dangling/keep")).is_err());
    }

    #[test]
    fn a_final_sigma_extension_still_matches() {
        let exclusions = exclusions(&["*.Σ"]).unwrap();
        assert!(exclusions.excludes(Path::new("/cache/foo.Σ")));
    }

    #[test]
    fn the_user_s_folders_are_planned_only_when_chosen() {
        let build = CustomFolder::new(PathBuf::from("/data/build")).unwrap();
        let other = CustomFolder::new(PathBuf::from("/data/other")).unwrap();
        let options = ScanOptions::new(SystemTime::now(), DEFAULT_MIN_AGE, Exclusions::none())
            .with_custom_folders(vec![build.clone(), other.clone()]);
        assert_eq!(
            options.chosen_custom_folders(),
            [&build, &other],
            "no category: all"
        );
        let without = options
            .clone()
            .with_categories(BTreeSet::from([Category::System, Category::Browsers]));
        assert_eq!(without.chosen_custom_folders(), Vec::<&CustomFolder>::new());
        assert_eq!(
            without.custom_folders(),
            [build.clone(), other.clone()],
            "still in force for a cleaning"
        );
        let with = options
            .clone()
            .with_categories(BTreeSet::from([Category::System, Category::Custom]));
        assert_eq!(with.chosen_custom_folders(), [&build, &other]);
        let one = options.with_origins(BTreeSet::from([Origin::Custom {
            folder: other.clone(),
        }]));
        assert_eq!(
            one.chosen_custom_folders(),
            [&other],
            "rules chosen one by one"
        );
    }

    #[test]
    fn the_cutoff_takes_the_stricter_age() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let options = ScanOptions::new(now, Duration::from_secs(100), Exclusions::none());
        assert_eq!(options.cutoff(None), Some(now - Duration::from_secs(100)));
        assert_eq!(
            options.cutoff(Some(Duration::from_secs(500))),
            Some(now - Duration::from_secs(500))
        );
        assert_eq!(
            options.cutoff(Some(Duration::from_secs(10))),
            Some(now - Duration::from_secs(100))
        );
        assert_eq!(options.cutoff(Some(Duration::from_secs(u64::MAX))), None);
    }
}
