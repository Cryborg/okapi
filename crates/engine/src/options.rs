use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use okapi_platform::{FileIdentity, file_identity};
use thiserror::Error;

use crate::paths::{canonicalize_longest_prefix, lowercase, starts_with_ignoring_case};
use crate::walk::IdentityReader;

/// How old an entry must be to be offered when the user has not chosen.
pub const DEFAULT_MIN_AGE: Duration = Duration::from_hours(1);

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

/// What the user never wants cleaned: paths (and everything below them), and
/// extensions written `*.log` (files, and directories with everything below
/// them, such as a `*.default-release` browser profile). Matching ignores case,
/// which can only exclude more; Unicode normalization forms are not folded.
/// An excluded directory that exists is also known by its identity, which
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
            if entry.as_os_str().is_empty() {
                return Err(ExclusionError::Empty);
            }
            let pattern = entry
                .to_str()
                .and_then(|text| text.strip_prefix(EXTENSION_PATTERN_PREFIX));
            let plain_path = entry.is_absolute()
                && !entry
                    .components()
                    .any(|component| component == Component::ParentDir);
            match pattern {
                Some(extension)
                    if !extension.is_empty() && !extension.contains(['/', '\\', '*']) =>
                {
                    exclusions
                        .extensions
                        .push(format!(".{}", lowercase(extension)));
                }
                None if plain_path => {
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
                _ => return Err(ExclusionError::Invalid { path: entry }),
            }
        }
        Ok(exclusions)
    }

    /// Whether the object of `identity` is an excluded directory.
    pub(crate) fn excludes_identity(&self, identity: FileIdentity) -> bool {
        self.identities.contains(&identity)
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
    pub(crate) fn excludes(&self, path: &Path) -> bool {
        let by_extension = path.components().any(|component| {
            let Component::Normal(name) = component else {
                return false;
            };
            let name = lowercase(&name.to_string_lossy());
            self.extensions
                .iter()
                .any(|extension| name.ends_with(extension.as_str()))
        });
        by_extension
            || self
                .prefixes
                .iter()
                .any(|prefix| starts_with_ignoring_case(path, prefix))
    }
}

/// What a scan honours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    now: SystemTime,
    min_age: Duration,
    exclusions: Exclusions,
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
        }
    }

    pub(crate) const fn exclusions(&self) -> &Exclusions {
        &self.exclusions
    }

    /// The instant after which an entry is too recent to offer, for a rule that
    /// asks for `rule_age`: the stricter of the two ages. `None` when the age
    /// reaches before the clock's origin, so nothing is old enough.
    pub(crate) fn cutoff(&self, rule_age: Option<Duration>) -> Option<SystemTime> {
        let age = rule_age.map_or(self.min_age, |rule_age| rule_age.max(self.min_age));
        self.now.checked_sub(age)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_final_sigma_extension_still_matches() {
        let exclusions = exclusions(&["*.Σ"]).unwrap();
        assert!(exclusions.excludes(Path::new("/cache/foo.Σ")));
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
