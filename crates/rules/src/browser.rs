use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::name::RelativePath;
use crate::rule_set::{RuleFileKind, browsers_kind};
use crate::template::{BoundedPath, TemplatePath};
use crate::values::{Label, NonEmpty, first_duplicate};

/// A cache directory of a Chromium-based browser.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChromiumCacheDirectory {
    #[serde(rename = "dir")]
    directory: RelativePath,
    label: Label,
    /// A cache of performance, rebuilt on demand: never selected by default
    /// (D-4, invariant 5).
    #[serde(default, rename = "cacheReset")]
    cache_reset: bool,
    description: Option<String>,
}

impl ChromiumCacheDirectory {
    /// Relative to a profile (profile caches) or to the user-data base (shared caches).
    #[must_use]
    pub const fn directory(&self) -> &RelativePath {
        &self.directory
    }

    #[must_use]
    pub const fn label(&self) -> &Label {
        &self.label
    }

    #[must_use]
    pub const fn is_cache_reset(&self) -> bool {
        self.cache_reset
    }

    /// Whether it is selected before the user chooses: never a performance
    /// cache (invariant 5).
    #[must_use]
    pub const fn is_selected_by_default(&self) -> bool {
        !self.cache_reset
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
}

/// The cache directories common to every Chromium-based browser.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawChromiumCacheDirectories")]
pub struct ChromiumCacheDirectories {
    profile: NonEmpty<ChromiumCacheDirectory>,
    shared: Vec<ChromiumCacheDirectory>,
}

impl ChromiumCacheDirectories {
    /// Directories inside each profile (`Default`, `Profile 1`...).
    #[must_use]
    pub fn profile(&self) -> &[ChromiumCacheDirectory] {
        &self.profile
    }

    /// Directories shared by every profile, directly under the user-data base.
    #[must_use]
    pub fn shared(&self) -> &[ChromiumCacheDirectory] {
        &self.shared
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChromiumCacheDirectories {
    profile: NonEmpty<ChromiumCacheDirectory>,
    #[serde(default)]
    shared: Vec<ChromiumCacheDirectory>,
}

impl TryFrom<RawChromiumCacheDirectories> for ChromiumCacheDirectories {
    type Error = ValidationError;

    fn try_from(raw: RawChromiumCacheDirectories) -> Result<Self, Self::Error> {
        // A directory names a group of every browser: listed once, in one
        // list or the other.
        if let Some(duplicate) = first_duplicate(
            raw.profile
                .iter()
                .chain(&raw.shared)
                .map(ChromiumCacheDirectory::directory),
        ) {
            return Err(ValidationError::DuplicateEntry {
                field: "chromiumCacheDirs",
                value: duplicate.to_string(),
            });
        }
        Ok(Self {
            profile: raw.profile,
            shared: raw.shared,
        })
    }
}

/// A Chromium-based browser that rules know about; written by its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChromiumBrowserKind {
    Chrome,
    Edge,
    Brave,
    Opera,
    #[serde(rename = "operaGX")]
    OperaGx,
    Vivaldi,
    Arc,
    Chromium,
    Thorium,
    Supermium,
    Helium,
    Cromite,
    Catsxp,
}

impl ChromiumBrowserKind {
    pub const ALL: [Self; 13] = [
        Self::Chrome,
        Self::Edge,
        Self::Brave,
        Self::Opera,
        Self::OperaGx,
        Self::Vivaldi,
        Self::Arc,
        Self::Chromium,
        Self::Thorium,
        Self::Supermium,
        Self::Helium,
        Self::Cromite,
        Self::Catsxp,
    ];

    /// Its name as shown: a product's name, never translated.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Edge => "Edge",
            Self::Brave => "Brave",
            Self::Opera => "Opera",
            Self::OperaGx => "Opera GX",
            Self::Vivaldi => "Vivaldi",
            Self::Arc => "Arc",
            Self::Chromium => "Chromium",
            Self::Thorium => "Thorium",
            Self::Supermium => "Supermium",
            Self::Helium => "Helium",
            Self::Cromite => "Cromite",
            Self::Catsxp => "CatsXP",
        }
    }

    /// Its stable key, as the rules write it (`operaGX`).
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Edge => "edge",
            Self::Brave => "brave",
            Self::Opera => "opera",
            Self::OperaGx => "operaGX",
            Self::Vivaldi => "vivaldi",
            Self::Arc => "arc",
            Self::Chromium => "chromium",
            Self::Thorium => "thorium",
            Self::Supermium => "supermium",
            Self::Helium => "helium",
            Self::Cromite => "cromite",
            Self::Catsxp => "catsxp",
        }
    }
}

/// Where one Chromium-based browser keeps its data.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChromiumBrowser {
    key: ChromiumBrowserKind,
    base: TemplatePath,
    cache: Option<TemplatePath>,
}

impl ChromiumBrowser {
    #[must_use]
    pub const fn kind(&self) -> ChromiumBrowserKind {
        self.key
    }

    /// The user-data directory that holds the profiles.
    #[must_use]
    pub const fn base(&self) -> &TemplatePath {
        &self.base
    }

    /// The root that mirrors [`Self::base`] for disk caches, where the OS keeps
    /// caches apart (`~/.cache` on Linux, `~/Library/Caches` on macOS).
    #[must_use]
    pub const fn cache(&self) -> Option<&TemplatePath> {
        self.cache.as_ref()
    }
}

/// Where Firefox keeps its profiles and their caches.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirefoxPaths {
    base: TemplatePath,
    cache: BoundedPath,
}

impl FirefoxPaths {
    /// The directory that holds the profiles.
    #[must_use]
    pub const fn base(&self) -> &TemplatePath {
        &self.base
    }

    /// The root that holds each profile's disk cache, apart from the profiles
    /// themselves; never a variable alone other than `${TMPDIR}`.
    #[must_use]
    pub fn cache(&self) -> &TemplatePath {
        &self.cache
    }
}

/// A Firefox fork that rules know about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FirefoxForkKind {
    Librewolf,
    Waterfox,
    Floorp,
    Zen,
}

/// Where one Firefox fork keeps its data.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirefoxFork {
    key: FirefoxForkKind,
    base: TemplatePath,
    cache: BoundedPath,
}

impl FirefoxFork {
    #[must_use]
    pub const fn kind(&self) -> FirefoxForkKind {
        self.key
    }

    /// The directory that holds the profiles.
    #[must_use]
    pub const fn base(&self) -> &TemplatePath {
        &self.base
    }

    /// The root that holds each profile's disk cache; never a variable alone
    /// other than `${TMPDIR}`.
    #[must_use]
    pub fn cache(&self) -> &TemplatePath {
        &self.cache
    }
}

/// Where Safari keeps its cache.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafariPaths {
    cache: BoundedPath,
}

impl SafariPaths {
    /// Safari's cache directory, emptied as a whole; never a variable alone
    /// other than `${TMPDIR}`.
    #[must_use]
    pub fn cache(&self) -> &TemplatePath {
        &self.cache
    }
}

/// The browsers of `chromium`, each listed once: one key names one browser.
fn browsers_once<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ChromiumBrowser>, D::Error> {
    let browsers = Vec::<ChromiumBrowser>::deserialize(deserializer)?;
    match first_duplicate(browsers.iter().map(ChromiumBrowser::kind)) {
        Some(duplicate) => Err(serde::de::Error::custom(ValidationError::DuplicateEntry {
            field: "chromium",
            value: duplicate.key().to_owned(),
        })),
        None => Ok(browsers),
    }
}

/// The content of `browsers.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserRules {
    #[serde(rename = "type", deserialize_with = "browsers_kind")]
    pub(crate) kind: RuleFileKind,
    #[serde(rename = "chromiumCacheDirs")]
    chromium_cache_directories: ChromiumCacheDirectories,
    #[serde(deserialize_with = "browsers_once")]
    chromium: Vec<ChromiumBrowser>,
    firefox: FirefoxPaths,
    #[serde(default)]
    firefox_forks: Vec<FirefoxFork>,
    safari: Option<SafariPaths>,
}

impl BrowserRules {
    #[must_use]
    pub const fn chromium_cache_directories(&self) -> &ChromiumCacheDirectories {
        &self.chromium_cache_directories
    }

    #[must_use]
    pub fn chromium(&self) -> &[ChromiumBrowser] {
        &self.chromium
    }

    #[must_use]
    pub const fn firefox(&self) -> &FirefoxPaths {
        &self.firefox
    }

    #[must_use]
    pub fn firefox_forks(&self) -> &[FirefoxFork] {
        &self.firefox_forks
    }

    #[must_use]
    pub const fn safari(&self) -> Option<&SafariPaths> {
        self.safari.as_ref()
    }

    pub(crate) fn template_paths(&self) -> impl Iterator<Item = &TemplatePath> {
        let chromium = self
            .chromium
            .iter()
            .flat_map(|browser| std::iter::once(browser.base()).chain(browser.cache()));
        let forks = self
            .firefox_forks
            .iter()
            .flat_map(|fork| [fork.base(), fork.cache()]);
        chromium
            .chain([self.firefox.base(), self.firefox.cache()])
            .chain(forks)
            .chain(self.safari.iter().map(SafariPaths::cache))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_of<T: for<'de> Deserialize<'de> + std::fmt::Debug>(json: &str) -> String {
        serde_json::from_str::<T>(json).unwrap_err().to_string()
    }

    #[test]
    fn reads_the_opera_gx_key_as_written_in_the_rules() {
        let browser: ChromiumBrowser =
            serde_json::from_str(r#"{"key": "operaGX", "base": "${CONFIG}/opera-gx"}"#).unwrap();
        assert_eq!(browser.kind(), ChromiumBrowserKind::OperaGx);
        assert_eq!(browser.cache(), None);
    }

    #[test]
    fn keeps_each_base_apart_from_its_cache() {
        let chromium: ChromiumBrowser = serde_json::from_str(
            r#"{"key": "chrome", "base": "${CONFIG}/c", "cache": "${CACHE}/c"}"#,
        )
        .unwrap();
        assert_eq!(chromium.base().to_string(), "${CONFIG}/c");
        assert_eq!(chromium.cache().unwrap().to_string(), "${CACHE}/c");
        let firefox: FirefoxPaths = serde_json::from_str(
            r#"{"base": "${HOME}/.mozilla/firefox", "cache": "${CACHE}/mozilla"}"#,
        )
        .unwrap();
        assert_eq!(firefox.base().to_string(), "${HOME}/.mozilla/firefox");
        assert_eq!(firefox.cache().to_string(), "${CACHE}/mozilla");
        let fork: FirefoxFork = serde_json::from_str(
            r#"{"key": "zen", "base": "${HOME}/.zen", "cache": "${CACHE}/zen"}"#,
        )
        .unwrap();
        assert_eq!(fork.base().to_string(), "${HOME}/.zen");
        assert_eq!(fork.cache().to_string(), "${CACHE}/zen");
        let safari: SafariPaths =
            serde_json::from_str(r#"{"cache": "${CACHES}/com.apple.Safari"}"#).unwrap();
        assert_eq!(safari.cache().to_string(), "${CACHES}/com.apple.Safari");
    }

    #[test]
    fn each_browser_has_its_own_name_and_the_key_of_the_rules() {
        let names: std::collections::BTreeSet<&str> = ChromiumBrowserKind::ALL
            .map(ChromiumBrowserKind::name)
            .into();
        assert_eq!(names.len(), ChromiumBrowserKind::ALL.len());
        for browser in ChromiumBrowserKind::ALL {
            let read: ChromiumBrowserKind =
                serde_json::from_value(serde_json::Value::from(browser.key())).unwrap();
            assert_eq!(read, browser);
            assert_eq!(serde_json::to_value(browser).unwrap(), browser.key());
        }
    }

    #[test]
    fn a_browser_is_listed_once() {
        let error = error_of::<BrowserRules>(
            r#"{"type": "browsers",
                "chromiumCacheDirs": {"profile": [{"dir": "Cache", "label": "Cache"}]},
                "chromium": [{"key": "chrome", "base": "${CONFIG}/a"}, {"key": "chrome", "base": "${CONFIG}/b"}],
                "firefox": {"base": "${HOME}/f", "cache": "${CACHE}/f"},
                "safari": null}"#,
        );
        assert!(error.contains("duplicate value"), "{error}");
        assert!(error.contains("chrome"), "{error}");
    }

    #[test]
    fn a_cache_directory_is_a_cache_reset_only_when_said() {
        let plain: ChromiumCacheDirectory =
            serde_json::from_str(r#"{"dir": "Cache", "label": "Cache"}"#).unwrap();
        assert!(!plain.is_cache_reset());
        let reset: ChromiumCacheDirectory = serde_json::from_str(
            r#"{"dir": "Code Cache", "label": "Code Cache", "cacheReset": true}"#,
        )
        .unwrap();
        assert!(reset.is_cache_reset());
    }

    #[test]
    fn rejects_an_unknown_browser() {
        let error = error_of::<ChromiumBrowser>(r#"{"key": "netscape", "base": "${CONFIG}/x"}"#);
        assert!(error.starts_with("unknown variant `netscape`"), "{error}");
    }

    #[test]
    fn a_fork_needs_both_paths() {
        let fork: FirefoxFork = serde_json::from_str(
            r#"{"key": "zen", "base": "${HOME}/.zen", "cache": "${CACHE}/zen"}"#,
        )
        .unwrap();
        assert_eq!(fork.kind(), FirefoxForkKind::Zen);
        let error = error_of::<FirefoxFork>(r#"{"key": "zen", "base": "${HOME}/.zen"}"#);
        assert!(error.starts_with("missing field `cache`"), "{error}");
    }

    #[test]
    fn cache_directories_stay_below_their_base_and_are_listed_once() {
        let escape =
            error_of::<ChromiumCacheDirectories>(r#"{"profile": [{"dir": "../x", "label": "X"}]}"#);
        assert!(escape.contains("not a relative path"), "{escape}");
        let empty = error_of::<ChromiumCacheDirectories>(r#"{"profile": []}"#);
        assert!(empty.starts_with("empty list"), "{empty}");
        for json in [
            r#"{"profile": [{"dir": "Cache", "label": "A"}, {"dir": "Cache", "label": "B"}]}"#,
            r#"{"profile": [{"dir": "Cache", "label": "A"}],
                "shared": [{"dir": "ShaderCache", "label": "B"}, {"dir": "ShaderCache", "label": "C"}]}"#,
            r#"{"profile": [{"dir": "Cache", "label": "A"}],
                "shared": [{"dir": "Cache", "label": "B"}]}"#,
        ] {
            let error = error_of::<ChromiumCacheDirectories>(json);
            assert!(error.starts_with("duplicate value"), "{error}");
            assert!(error.contains("in `chromiumCacheDirs`"), "{error}");
        }
    }

    #[test]
    fn a_cache_root_is_never_a_variable_alone() {
        for error in [
            error_of::<FirefoxPaths>(
                r#"{"base": "${HOME}/.mozilla/firefox", "cache": "${CACHE}"}"#,
            ),
            error_of::<FirefoxFork>(
                r#"{"key": "zen", "base": "${HOME}/.zen", "cache": "${HOME}"}"#,
            ),
            error_of::<SafariPaths>(r#"{"cache": "${CACHES}"}"#),
        ] {
            assert!(error.contains("alone would empty"), "{error}");
        }
    }

    #[test]
    fn rejects_unknown_fields_in_every_browser_type() {
        for error in [
            error_of::<ChromiumCacheDirectory>(r#"{"dir": "Cache", "label": "C", "command": "x"}"#),
            error_of::<ChromiumCacheDirectories>(
                r#"{"profile": [{"dir": "C", "label": "C"}], "command": "x"}"#,
            ),
            error_of::<ChromiumBrowser>(
                r#"{"key": "chrome", "base": "${CONFIG}/c", "command": "x"}"#,
            ),
            error_of::<FirefoxPaths>(
                r#"{"base": "${HOME}/f", "cache": "${CACHE}/f", "command": "x"}"#,
            ),
            error_of::<FirefoxFork>(
                r#"{"key": "zen", "base": "${HOME}/z", "cache": "${CACHE}/z", "command": "x"}"#,
            ),
            error_of::<SafariPaths>(r#"{"cache": "${CACHES}/s", "command": "x"}"#),
        ] {
            assert!(error.starts_with("unknown field `command`"), "{error}");
        }
    }
}
