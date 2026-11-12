use std::fmt;
use std::time::Duration;

use serde::Deserialize;

use crate::effect::{CleanupAction, Effect};
use crate::error::ValidationError;
use crate::name::{Name, RULE_SEPARATOR, RelativePath, WILDCARD};
use crate::rule_set::RuleFileKind;
use crate::template::TemplatePath;
use crate::values::{Label, NonEmpty, RuleId, ensure_unique};

/// A minimum age in days, between [`Self::MIN`] and [`Self::MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "u32")]
pub struct AgeDays(u16);

impl AgeDays {
    pub const MIN: u16 = 1;
    pub const MAX: u16 = 3650;

    #[must_use]
    pub const fn days(self) -> u16 {
        self.0
    }

    #[must_use]
    pub fn duration(self) -> Duration {
        Duration::from_hours(u64::from(self.0) * HOURS_PER_DAY)
    }
}

const HOURS_PER_DAY: u64 = 24;

impl TryFrom<u32> for AgeDays {
    type Error = ValidationError;

    fn try_from(days: u32) -> Result<Self, Self::Error> {
        u16::try_from(days)
            .ok()
            .filter(|days| (Self::MIN..=Self::MAX).contains(days))
            .map(Self)
            .ok_or(ValidationError::AgeOutOfRange {
                days,
                min: Self::MIN,
                max: Self::MAX,
            })
    }
}

/// How many directory levels a recursive match may go down below its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "u32")]
pub struct MaxDepth(u8);

impl MaxDepth {
    pub const MIN: u8 = 1;
    pub const MAX: u8 = 32;
    const DEFAULT_LEVELS: u8 = 12;

    #[must_use]
    pub const fn levels(self) -> u8 {
        self.0
    }
}

impl Default for MaxDepth {
    fn default() -> Self {
        Self(Self::DEFAULT_LEVELS)
    }
}

impl TryFrom<u32> for MaxDepth {
    type Error = ValidationError;

    fn try_from(depth: u32) -> Result<Self, Self::Error> {
        u8::try_from(depth)
            .ok()
            .filter(|depth| (Self::MIN..=Self::MAX).contains(depth))
            .map(Self)
            .ok_or(ValidationError::DepthOutOfRange {
                depth,
                min: Self::MIN,
                max: Self::MAX,
            })
    }
}

/// One segment of an [`AnchorPath`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AnchorSegment {
    /// `*`: exactly one directory, whatever its name.
    Wildcard,
    /// A directory with this exact name.
    Exact(Name),
}

/// A relative path to an anchor directory made of literal names and whole `*`
/// segments, such as `Packages/*/EBWebView`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct AnchorPath(Vec<AnchorSegment>);

impl AnchorPath {
    #[must_use]
    pub fn segments(&self) -> &[AnchorSegment] {
        &self.0
    }

    fn ends_with(&self, anchor: &Name) -> bool {
        matches!(self.0.last(), Some(AnchorSegment::Exact(last)) if last == anchor)
    }
}

impl TryFrom<String> for AnchorPath {
    type Error = ValidationError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        let segment = |text: &str| {
            if text.strip_prefix(WILDCARD) == Some("") {
                Some(AnchorSegment::Wildcard)
            } else {
                Name::try_from(text.to_owned())
                    .ok()
                    .map(AnchorSegment::Exact)
            }
        };
        match path.split(RULE_SEPARATOR).map(segment).collect() {
            Some(segments) => Ok(Self(segments)),
            None => Err(ValidationError::InvalidAnchorPath { path }),
        }
    }
}

impl fmt::Display for AnchorPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, segment) in self.0.iter().enumerate() {
            if index > 0 {
                write!(formatter, "{RULE_SEPARATOR}")?;
            }
            match segment {
                AnchorSegment::Wildcard => write!(formatter, "{WILDCARD}")?,
                AnchorSegment::Exact(name) => formatter.write_str(name.as_str())?,
            }
        }
        Ok(())
    }
}

/// Cache directories with a fixed name, found below an anchor directory (for
/// example every `Cache` below an `EBWebView`).
///
/// The search follows Kudu's format: it starts from each anchor found through
/// [`Self::anchor_paths`], or else from the rule's path itself, which counts as
/// an anchor when it bears the anchor's name. It goes down at most
/// [`Self::max_depth`] levels from where it starts, and only directories below
/// an anchor can be targets or excluded ancestors.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawRecursivePathMatch")]
pub struct RecursivePathMatch {
    anchor: Name,
    anchor_paths: Option<NonEmpty<AnchorPath>>,
    targets: NonEmpty<Name>,
    excluded_ancestors: Vec<Name>,
    max_depth: MaxDepth,
}

impl RecursivePathMatch {
    /// The exact name of the directory every target must be below.
    #[must_use]
    pub const fn anchor(&self) -> &Name {
        &self.anchor
    }

    /// Where to look for anchors below the base directory. `None` means the
    /// anchor is searched for recursively, down to [`Self::max_depth`].
    #[must_use]
    pub fn anchor_paths(&self) -> Option<&[AnchorPath]> {
        self.anchor_paths.as_deref()
    }

    /// The exact names of the cache directories to offer.
    #[must_use]
    pub fn targets(&self) -> &[Name] {
        &self.targets
    }

    /// Directory names, below an anchor, that a target must never be below,
    /// such as `IndexedDB`.
    #[must_use]
    pub fn excluded_ancestors(&self) -> &[Name] {
        &self.excluded_ancestors
    }

    #[must_use]
    pub const fn max_depth(&self) -> MaxDepth {
        self.max_depth
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawRecursivePathMatch {
    anchor: Name,
    anchor_paths: Option<NonEmpty<AnchorPath>>,
    targets: NonEmpty<Name>,
    #[serde(default)]
    excluded_ancestors: Vec<Name>,
    #[serde(default)]
    max_depth: MaxDepth,
}

impl TryFrom<RawRecursivePathMatch> for RecursivePathMatch {
    type Error = ValidationError;

    fn try_from(raw: RawRecursivePathMatch) -> Result<Self, Self::Error> {
        if let Some(anchor_paths) = &raw.anchor_paths {
            ensure_unique("anchorPaths", anchor_paths)?;
            if let Some(mismatch) = anchor_paths
                .iter()
                .find(|path| !path.ends_with(&raw.anchor))
            {
                return Err(ValidationError::AnchorPathMismatch {
                    anchor: raw.anchor.to_string(),
                    anchor_path: mismatch.to_string(),
                });
            }
        }
        ensure_unique("targets", &raw.targets)?;
        ensure_unique("excludedAncestors", &raw.excluded_ancestors)?;
        Ok(Self {
            anchor: raw.anchor,
            anchor_paths: raw.anchor_paths,
            targets: raw.targets,
            excluded_ancestors: raw.excluded_ancestors,
            max_depth: raw.max_depth,
        })
    }
}

/// A list of exact file names, offered only once they are old enough.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawDirectFileMatch")]
pub struct DirectFileMatch {
    names: NonEmpty<Name>,
    child_directory_suffix: Option<Name>,
    min_age: AgeDays,
    skip_if_child_exists: Vec<Name>,
}

impl DirectFileMatch {
    /// The exact names of the files to offer, directly in each candidate directory.
    #[must_use]
    pub fn names(&self) -> &[Name] {
        &self.names
    }

    /// If set, the candidate directories are the immediate children of the
    /// rule's path whose name ends with this suffix; otherwise the candidate is
    /// the rule's path itself.
    #[must_use]
    pub const fn child_directory_suffix(&self) -> Option<&Name> {
        self.child_directory_suffix.as_ref()
    }

    #[must_use]
    pub const fn min_age(&self) -> AgeDays {
        self.min_age
    }

    /// A candidate directory that directly holds one of these names is skipped.
    #[must_use]
    pub fn skip_if_child_exists(&self) -> &[Name] {
        &self.skip_if_child_exists
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawDirectFileMatch {
    names: NonEmpty<Name>,
    #[serde(rename = "childDirSuffix")]
    child_directory_suffix: Option<Name>,
    min_age_days: AgeDays,
    #[serde(default)]
    skip_if_child_exists: Vec<Name>,
}

impl TryFrom<RawDirectFileMatch> for DirectFileMatch {
    type Error = ValidationError;

    fn try_from(raw: RawDirectFileMatch) -> Result<Self, Self::Error> {
        ensure_unique("names", &raw.names)?;
        ensure_unique("skipIfChildExists", &raw.skip_if_child_exists)?;
        Ok(Self {
            names: raw.names,
            child_directory_suffix: raw.child_directory_suffix,
            min_age: raw.min_age_days,
            skip_if_child_exists: raw.skip_if_child_exists,
        })
    }
}

/// What an application rule offers below each of its paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathMatcher {
    /// The contents of the path itself.
    WholeDirectory,
    /// The contents of `<path>/*/<subdirectory>` (versioned layouts).
    ChildSubdirectory(RelativePath),
    /// Cache directories found below an anchor directory.
    Recursive(RecursivePathMatch),
    /// Exact files.
    DirectFiles(DirectFileMatch),
}

impl PathMatcher {
    /// The rule file field that selects this matcher, if any.
    const fn field(&self) -> Option<&'static str> {
        match self {
            Self::WholeDirectory => None,
            Self::ChildSubdirectory(_) => Some("childSubdir"),
            Self::Recursive(_) => Some("recursiveMatch"),
            Self::DirectFiles(_) => Some("fileMatch"),
        }
    }
}

/// How an application rule deletes: where, what it offers there, and how old
/// it must be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDeletion {
    paths: NonEmpty<TemplatePath>,
    matcher: PathMatcher,
    min_age: Option<AgeDays>,
    cache_reset: bool,
}

impl AppDeletion {
    /// The base directories the matcher searches.
    #[must_use]
    pub fn paths(&self) -> &[TemplatePath] {
        &self.paths
    }

    #[must_use]
    pub const fn matcher(&self) -> &PathMatcher {
        &self.matcher
    }

    /// Entries younger than this are kept. It never applies with
    /// [`PathMatcher::DirectFiles`], which carries its own minimum age.
    #[must_use]
    pub const fn min_age(&self) -> Option<AgeDays> {
        self.min_age
    }

    /// A performance cache: clearing it slows the next launch.
    #[must_use]
    pub const fn is_cache_reset(&self) -> bool {
        self.cache_reset
    }
}

/// The caches of one application, game launcher or GPU driver.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawAppCacheRule")]
pub struct AppCacheRule {
    id: RuleId,
    name: Label,
    group: Option<Label>,
    effect: Effect<AppDeletion, NonEmpty<TemplatePath>>,
    description: Option<String>,
}

impl AppCacheRule {
    #[must_use]
    pub const fn id(&self) -> &RuleId {
        &self.id
    }

    #[must_use]
    pub const fn name(&self) -> &Label {
        &self.name
    }

    /// The section that gathers related rules in the interface.
    #[must_use]
    pub const fn group(&self) -> Option<&Label> {
        self.group.as_ref()
    }

    #[must_use]
    pub const fn effect(&self) -> &Effect<AppDeletion, NonEmpty<TemplatePath>> {
        &self.effect
    }

    /// Whether the rule is selected before the user chooses: never for a
    /// performance cache nor a maintenance operation (invariant 5).
    #[must_use]
    pub const fn is_selected_by_default(&self) -> bool {
        match &self.effect {
            Effect::Delete(deletion) => !deletion.cache_reset,
            Effect::Maintenance { .. } => false,
        }
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Every path of the rule, deleted or only affected, for checks that do
    /// not delete anything.
    pub(crate) fn template_paths(&self) -> &[TemplatePath] {
        match &self.effect {
            Effect::Delete(deletion) => &deletion.paths,
            Effect::Maintenance { affected, .. } => affected,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawAppCacheRule {
    id: RuleId,
    name: Label,
    paths: NonEmpty<TemplatePath>,
    group: Option<Label>,
    min_age_days: Option<AgeDays>,
    child_subdir: Option<RelativePath>,
    recursive_match: Option<RecursivePathMatch>,
    file_match: Option<DirectFileMatch>,
    #[serde(default)]
    cache_reset: bool,
    cleanup_action: Option<CleanupAction>,
    description: Option<String>,
}

impl RawAppCacheRule {
    /// Checks that each path stays bounded by the matcher: a bare variable is
    /// never emptied whole, nor fanned out to each of its children, nor searched
    /// freely for an anchor. This only reads the rule; the engine still checks
    /// the resolved paths (invariant 6).
    fn ensure_bounded(&self, matcher: &PathMatcher) -> Result<(), ValidationError> {
        for path in self.paths.iter() {
            match matcher {
                PathMatcher::WholeDirectory | PathMatcher::ChildSubdirectory(_) => {
                    path.ensure_bounded_deletion()?;
                }
                PathMatcher::Recursive(recursive)
                    if path.is_bare_variable() && recursive.anchor_paths().is_none() =>
                {
                    return Err(ValidationError::UnboundedRecursiveSearch {
                        path: path.to_string(),
                    });
                }
                PathMatcher::Recursive(_) | PathMatcher::DirectFiles(_) => {}
            }
        }
        Ok(())
    }
}

impl TryFrom<RawAppCacheRule> for AppCacheRule {
    type Error = ValidationError;

    fn try_from(mut raw: RawAppCacheRule) -> Result<Self, Self::Error> {
        let rule = raw.id.to_string();
        let conflict = |first, second| ValidationError::ConflictingFields {
            rule: rule.clone(),
            first,
            second,
        };
        let matcher = match (
            raw.child_subdir.take(),
            raw.recursive_match.take(),
            raw.file_match.take(),
        ) {
            (None, None, None) => PathMatcher::WholeDirectory,
            (Some(subdirectory), None, None) => PathMatcher::ChildSubdirectory(subdirectory),
            (None, Some(recursive), None) => PathMatcher::Recursive(recursive),
            (None, None, Some(files)) => PathMatcher::DirectFiles(files),
            (Some(_), Some(_), _) => return Err(conflict("childSubdir", "recursiveMatch")),
            (Some(_), None, Some(_)) => return Err(conflict("childSubdir", "fileMatch")),
            (None, Some(_), Some(_)) => return Err(conflict("recursiveMatch", "fileMatch")),
        };
        ensure_unique("paths", &raw.paths)?;

        let effect = match (raw.cleanup_action, matcher.field()) {
            (Some(_), Some(matcher_field)) => return Err(conflict("cleanupAction", matcher_field)),
            (Some(_), None) if raw.min_age_days.is_some() => {
                return Err(conflict("cleanupAction", "minAgeDays"));
            }
            (Some(_), None) if raw.cache_reset => {
                return Err(conflict("cleanupAction", "cacheReset"));
            }
            (Some(action), None) => Effect::Maintenance {
                action,
                affected: raw.paths,
            },
            (None, _)
                if raw.min_age_days.is_some() && matches!(matcher, PathMatcher::DirectFiles(_)) =>
            {
                return Err(conflict("minAgeDays", "fileMatch"));
            }
            (None, _) => {
                raw.ensure_bounded(&matcher)?;
                Effect::Delete(AppDeletion {
                    paths: raw.paths,
                    matcher,
                    min_age: raw.min_age_days,
                    cache_reset: raw.cache_reset,
                })
            }
        };
        Ok(Self {
            id: raw.id,
            name: raw.name,
            group: raw.group,
            effect,
            description: raw.description,
        })
    }
}

/// The content of `apps.json`, `gaming.json` and `gpu-cache.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AppRules {
    #[serde(rename = "type")]
    pub(crate) kind: RuleFileKind,
    apps: Vec<AppCacheRule>,
}

impl AppRules {
    pub(crate) fn apps(&self) -> &[AppCacheRule] {
        &self.apps
    }

    pub(crate) fn template_paths(&self) -> impl Iterator<Item = &TemplatePath> {
        self.apps.iter().flat_map(AppCacheRule::template_paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(json: &str) -> Result<AppCacheRule, serde_json::Error> {
        serde_json::from_str(json)
    }

    fn error_of(json: &str) -> String {
        rule(json).unwrap_err().to_string()
    }

    fn deletion(rule: &AppCacheRule) -> &AppDeletion {
        match rule.effect() {
            Effect::Delete(deletion) => deletion,
            Effect::Maintenance { action, .. } => panic!("expected a deletion, got {action:?}"),
        }
    }

    #[test]
    fn a_minimal_rule_deletes_whole_directories_and_is_selected() {
        let rule =
            rule(r#"{"id": "discord", "name": "Discord", "paths": ["${CONFIG}/discord/Cache"]}"#)
                .unwrap();
        assert_eq!(rule.id().as_str(), "discord");
        assert_eq!(deletion(&rule).matcher(), &PathMatcher::WholeDirectory);
        assert_eq!(deletion(&rule).min_age(), None);
        assert!(!deletion(&rule).is_cache_reset());
        assert!(rule.is_selected_by_default());
    }

    #[test]
    fn reads_every_optional_field() {
        let rule = rule(
            r#"{"id": "shaders", "name": "Shaders", "paths": ["${CACHE}/a", "${CACHE}/b"], "group": "GPU",
                "minAgeDays": 7, "cacheReset": true, "description": "Rebuilt on demand"}"#,
        )
        .unwrap();
        assert_eq!(rule.name().as_str(), "Shaders");
        assert_eq!(deletion(&rule).paths().len(), 2);
        assert_eq!(rule.group().unwrap().as_str(), "GPU");
        assert_eq!(deletion(&rule).min_age().unwrap().days(), 7);
        assert!(deletion(&rule).is_cache_reset());
        assert!(
            !rule.is_selected_by_default(),
            "a cache reset is never preselected"
        );
        assert_eq!(rule.description(), Some("Rebuilt on demand"));
    }

    #[test]
    fn a_maintenance_rule_keeps_its_paths_apart_and_is_not_selected() {
        let rule = rule(
            r#"{"id": "uv", "name": "uv", "paths": ["${HOME}"], "cleanupAction": "uv-prune"}"#,
        )
        .unwrap();
        let Effect::Maintenance { action, affected } = rule.effect() else {
            panic!("expected a maintenance operation");
        };
        assert_eq!(*action, CleanupAction::UvPrune);
        assert_eq!(affected[0].to_string(), "${HOME}");
        assert!(!rule.is_selected_by_default());
    }

    #[test]
    fn a_maintenance_rule_cannot_also_delete() {
        for (extra, second) in [
            (r#""childSubdir": "cache""#, "childSubdir"),
            (
                r#""recursiveMatch": {"anchor": "A", "targets": ["Cache"]}"#,
                "recursiveMatch",
            ),
            (
                r#""fileMatch": {"names": ["x"], "minAgeDays": 1}"#,
                "fileMatch",
            ),
            (r#""minAgeDays": 3"#, "minAgeDays"),
            (r#""cacheReset": true"#, "cacheReset"),
        ] {
            let json = format!(
                r#"{{"id": "uv", "name": "uv", "paths": ["${{CACHE}}/uv"], "cleanupAction": "uv-prune", {extra}}}"#
            );
            let error = error_of(&json);
            assert!(
                error.contains(&format!("combines `cleanupAction` and `{second}`")),
                "{error}"
            );
        }
    }

    #[test]
    fn a_bare_variable_is_never_a_whole_directory_to_delete() {
        for paths in [
            r#"["${HOME}"]"#,
            r#"["${CACHE}/a", "${HOME}"]"#,
            r#"["${HOME}", "${CACHE}/a"]"#,
        ] {
            let error = error_of(&format!(
                r#"{{"id": "home", "name": "Home", "paths": {paths}}}"#
            ));
            assert!(
                error.starts_with("`${HOME}` alone would empty"),
                "{paths}: {error}"
            );
        }
    }

    #[test]
    fn a_bare_variable_is_never_fanned_out_to_its_children() {
        let error = error_of(
            r#"{"id": "d", "name": "D", "paths": ["${HOME}"], "childSubdir": "Documents"}"#,
        );
        assert!(error.starts_with("`${HOME}` alone would empty"), "{error}");
    }

    #[test]
    fn a_bare_variable_is_searched_only_through_anchor_paths() {
        let free = error_of(
            r#"{"id": "w", "name": "W", "paths": ["${CACHE}/a", "${HOME}"],
                "recursiveMatch": {"anchor": "EBWebView", "targets": ["Cache"]}}"#,
        );
        assert!(
            free.starts_with("`${HOME}` alone needs `anchorPaths`"),
            "{free}"
        );
        let bounded = rule(
            r#"{"id": "w", "name": "W", "paths": ["${HOME}"],
                "recursiveMatch": {"anchor": "EBWebView", "anchorPaths": ["*/EBWebView"], "targets": ["Cache"]}}"#,
        );
        assert!(bounded.is_ok(), "{bounded:?}");
        let below = rule(
            r#"{"id": "w", "name": "W", "paths": ["${CACHE}/app"],
                "recursiveMatch": {"anchor": "EBWebView", "targets": ["Cache"]}}"#,
        );
        assert!(below.is_ok(), "{below:?}");
    }

    #[test]
    fn reads_each_kind_of_matcher() {
        let subdirectory =
            rule(r#"{"id": "a", "name": "A", "paths": ["${CACHE}/a"], "childSubdir": "caches"}"#)
                .unwrap();
        assert!(
            matches!(deletion(&subdirectory).matcher(), PathMatcher::ChildSubdirectory(path) if path.to_string() == "caches")
        );

        let recursive = rule(
            r#"{"id": "b", "name": "B", "paths": ["${CACHE}/b"],
                "recursiveMatch": {"anchor": "EBWebView", "anchorPaths": ["*/EBWebView"],
                                   "targets": ["Cache", "GPUCache"], "excludedAncestors": ["IndexedDB"], "maxDepth": 4}}"#,
        )
        .unwrap();
        let PathMatcher::Recursive(recursive) = deletion(&recursive).matcher() else {
            panic!("expected a recursive matcher");
        };
        assert_eq!(recursive.anchor().as_str(), "EBWebView");
        assert_eq!(
            recursive.anchor_paths().unwrap()[0].to_string(),
            "*/EBWebView"
        );
        assert_eq!(recursive.targets().len(), 2);
        assert_eq!(recursive.excluded_ancestors()[0].as_str(), "IndexedDB");
        assert_eq!(recursive.max_depth().levels(), 4);

        let files = rule(
            r#"{"id": "c", "name": "C", "paths": ["${CACHE}/c"],
                "fileMatch": {"names": ["installer.exe"], "childDirSuffix": "-updater", "minAgeDays": 14,
                              "skipIfChildExists": ["pending"]}}"#,
        )
        .unwrap();
        let PathMatcher::DirectFiles(files) = deletion(&files).matcher() else {
            panic!("expected a file matcher");
        };
        assert_eq!(files.names()[0].as_str(), "installer.exe");
        assert_eq!(files.child_directory_suffix().unwrap().as_str(), "-updater");
        assert_eq!(files.min_age().days(), 14);
        assert_eq!(files.skip_if_child_exists()[0].as_str(), "pending");
    }

    #[test]
    fn a_recursive_match_defaults_to_twelve_levels_and_free_anchor_search() {
        let raw = r#"{"anchor": "EBWebView", "targets": ["Cache"]}"#;
        let recursive: RecursivePathMatch = serde_json::from_str(raw).unwrap();
        assert_eq!(recursive.max_depth().levels(), 12);
        assert_eq!(recursive.anchor_paths(), None);
    }

    #[test]
    fn rejects_every_pair_of_matchers() {
        let child = r#""childSubdir": "caches""#;
        let recursive = r#""recursiveMatch": {"anchor": "A", "targets": ["Cache"]}"#;
        let files = r#""fileMatch": {"names": ["x"], "minAgeDays": 1}"#;
        for (fields, first, second) in [
            (
                format!("{child}, {recursive}"),
                "childSubdir",
                "recursiveMatch",
            ),
            (format!("{child}, {files}"), "childSubdir", "fileMatch"),
            (
                format!("{recursive}, {files}"),
                "recursiveMatch",
                "fileMatch",
            ),
            (
                format!("{child}, {recursive}, {files}"),
                "childSubdir",
                "recursiveMatch",
            ),
        ] {
            let error = error_of(&format!(
                r#"{{"id": "a", "name": "A", "paths": ["${{CACHE}}/a"], {fields}}}"#
            ));
            assert!(
                error.contains(&format!("`a` combines `{first}` and `{second}`")),
                "{error}"
            );
        }
    }

    #[test]
    fn a_file_match_keeps_its_own_minimum_age() {
        let error = error_of(
            r#"{"id": "a", "name": "A", "paths": ["${CACHE}/a"], "minAgeDays": 3,
                "fileMatch": {"names": ["x"], "minAgeDays": 1}}"#,
        );
        assert!(
            error.contains("combines `minAgeDays` and `fileMatch`"),
            "{error}"
        );
        let missing = serde_json::from_str::<DirectFileMatch>(r#"{"names": ["a"]}"#).unwrap_err();
        assert!(
            missing.to_string().contains("missing field `minAgeDays`"),
            "{missing}"
        );
        let zero = serde_json::from_str::<DirectFileMatch>(r#"{"names": ["a"], "minAgeDays": 0}"#)
            .unwrap_err();
        assert!(zero.to_string().contains("0 days is outside"), "{zero}");
    }

    #[test]
    fn rejects_unknown_fields_in_every_rule_type() {
        let error =
            error_of(r#"{"id": "a", "name": "A", "paths": ["${CACHE}/a"], "command": "rm"}"#);
        assert!(error.contains("unknown field `command`"), "{error}");
        let recursive = serde_json::from_str::<RecursivePathMatch>(
            r#"{"anchor": "A", "targets": ["c"], "command": "x"}"#,
        )
        .unwrap_err();
        assert!(
            recursive.to_string().contains("unknown field `command`"),
            "{recursive}"
        );
        let files = serde_json::from_str::<DirectFileMatch>(
            r#"{"names": ["a"], "minAgeDays": 3, "command": "x"}"#,
        )
        .unwrap_err();
        assert!(
            files.to_string().contains("unknown field `command`"),
            "{files}"
        );
    }

    #[test]
    fn rejects_empty_and_repeated_lists() {
        let empty_paths = error_of(r#"{"id": "a", "name": "A", "paths": []}"#);
        assert!(empty_paths.starts_with("empty list"), "{empty_paths}");
        let repeated =
            error_of(r#"{"id": "a", "name": "A", "paths": ["${CACHE}/a", "${CACHE}/a"]}"#);
        assert!(
            repeated.contains("duplicate value `${CACHE}/a` in `paths`"),
            "{repeated}"
        );

        for (raw, expected) in [
            (r#"{"anchor": "A", "targets": []}"#, "empty list"),
            (
                r#"{"anchor": "A", "anchorPaths": [], "targets": ["Cache"]}"#,
                "empty list",
            ),
            (
                r#"{"anchor": "A", "anchorPaths": ["*/A", "*/A"], "targets": ["Cache"]}"#,
                "duplicate value `*/A` in `anchorPaths`",
            ),
            (
                r#"{"anchor": "A", "targets": ["Cache", "Cache"]}"#,
                "duplicate value `Cache` in `targets`",
            ),
            (
                r#"{"anchor": "A", "targets": ["Cache"], "excludedAncestors": ["IndexedDB", "IndexedDB"]}"#,
                "duplicate value `IndexedDB` in `excludedAncestors`",
            ),
        ] {
            let error = serde_json::from_str::<RecursivePathMatch>(raw)
                .unwrap_err()
                .to_string();
            assert!(error.starts_with(expected), "{raw}: {error}");
        }
        for (raw, expected) in [
            (r#"{"names": [], "minAgeDays": 3}"#, "empty list"),
            (
                r#"{"names": ["a.exe", "a.exe"], "minAgeDays": 3}"#,
                "duplicate value `a.exe` in `names`",
            ),
            (
                r#"{"names": ["a"], "minAgeDays": 3, "skipIfChildExists": ["x", "x"]}"#,
                "duplicate value `x` in `skipIfChildExists`",
            ),
        ] {
            let error = serde_json::from_str::<DirectFileMatch>(raw)
                .unwrap_err()
                .to_string();
            assert!(error.starts_with(expected), "{raw}: {error}");
        }
    }

    #[test]
    fn age_and_depth_stay_in_range_without_truncation() {
        assert_eq!(AgeDays::try_from(1).unwrap().days(), 1);
        assert_eq!(AgeDays::try_from(3650).unwrap().days(), 3650);
        assert_eq!(
            AgeDays::try_from(2).unwrap().duration(),
            Duration::from_hours(48)
        );
        // 65_537 truncates to 1 as a `u16`, which would be in range.
        for days in [0, 3651, 65_537] {
            assert_eq!(
                AgeDays::try_from(days),
                Err(ValidationError::AgeOutOfRange {
                    days,
                    min: 1,
                    max: 3650
                })
            );
        }
        assert_eq!(MaxDepth::try_from(1).unwrap().levels(), 1);
        assert_eq!(MaxDepth::try_from(32).unwrap().levels(), 32);
        // 257 truncates to 1 as a `u8`, which would be in range.
        for depth in [0, 33, 257] {
            assert_eq!(
                MaxDepth::try_from(depth),
                Err(ValidationError::DepthOutOfRange {
                    depth,
                    min: 1,
                    max: 32
                })
            );
        }
    }

    #[test]
    fn anchor_paths_allow_only_whole_segment_wildcards() {
        assert_eq!(
            AnchorPath::try_from("Packages/*/EBWebView".to_owned())
                .unwrap()
                .to_string(),
            "Packages/*/EBWebView"
        );
        for path in [
            "Pack*/EBWebView",
            "../EBWebView",
            "a//EBWebView",
            "",
            "C:/EBWebView",
        ] {
            assert_eq!(
                AnchorPath::try_from(path.to_owned()),
                Err(ValidationError::InvalidAnchorPath {
                    path: path.to_owned()
                })
            );
        }
    }

    #[test]
    fn every_anchor_path_must_end_with_the_anchor() {
        for (paths, faulty) in [
            (r#"["*/EBWebView", "*/Other"]"#, "*/Other"),
            (r#"["*/Other", "*/EBWebView"]"#, "*/Other"),
            (r#"["EBWebView/*"]"#, "EBWebView/*"),
        ] {
            let raw = format!(
                r#"{{"anchor": "EBWebView", "anchorPaths": {paths}, "targets": ["Cache"]}}"#
            );
            let error = serde_json::from_str::<RecursivePathMatch>(&raw)
                .unwrap_err()
                .to_string();
            let expected = ValidationError::AnchorPathMismatch {
                anchor: "EBWebView".to_owned(),
                anchor_path: faulty.to_owned(),
            };
            assert!(error.starts_with(&expected.to_string()), "{paths}: {error}");
        }
    }
}
