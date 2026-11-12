use serde::Deserialize;

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
        for (field, directories) in [("profile", &raw.profile[..]), ("shared", &raw.shared[..])] {
            if let Some(duplicate) =
                first_duplicate(directories.iter().map(ChromiumCacheDirectory::directory))
            {
                return Err(ValidationError::DuplicateEntry {
                    field,
                    value: duplicate.to_string(),
                });
            }
        }
        Ok(Self {
            profile: raw.profile,
            shared: raw.shared,
        })
    }
}

/// A Chromium-based browser that rules know about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
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

/// The content of `browsers.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserRules {
    #[serde(rename = "type", deserialize_with = "browsers_kind")]
    pub(crate) kind: RuleFileKind,
    #[serde(rename = "chromiumCacheDirs")]
    chromium_cache_directories: ChromiumCacheDirectories,
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
        for (field, json) in [
            (
                "profile",
                r#"{"profile": [{"dir": "Cache", "label": "A"}, {"dir": "Cache", "label": "B"}]}"#,
            ),
            (
                "shared",
                r#"{"profile": [{"dir": "Cache", "label": "A"}],
                    "shared": [{"dir": "ShaderCache", "label": "B"}, {"dir": "ShaderCache", "label": "C"}]}"#,
            ),
        ] {
            let error = error_of::<ChromiumCacheDirectories>(json);
            assert!(error.starts_with("duplicate value"), "{error}");
            assert!(error.contains(&format!("in `{field}`")), "{error}");
        }
        let both: ChromiumCacheDirectories = serde_json::from_str(
            r#"{"profile": [{"dir": "Cache", "label": "A"}], "shared": [{"dir": "Cache", "label": "B"}]}"#,
        )
        .unwrap();
        assert_eq!(
            both.profile().len() + both.shared().len(),
            2,
            "the two lists have different bases"
        );
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
