use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::name::RelativePath;
use crate::rule_set::{RuleFileKind, browsers_kind};
use crate::template::{BoundedPath, TemplatePath};
use crate::values::{Label, NonEmpty, first_duplicate};

/// A cache directory of a browser, the same in each one of its family.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserCacheDirectory {
    #[serde(rename = "dir")]
    directory: RelativePath,
    label: Label,
    /// A cache of performance, rebuilt on demand: never selected by default
    /// (D-4, invariant 5).
    #[serde(default, rename = "cacheReset")]
    cache_reset: bool,
    description: Option<Label>,
}

impl BrowserCacheDirectory {
    /// Relative to a profile (profile caches) or, for Chromium, to the
    /// user-data base (shared caches).
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
    pub const fn description(&self) -> Option<&Label> {
        self.description.as_ref()
    }
}

/// The cache directories common to every Chromium-based browser.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawChromiumCacheDirectories")]
pub struct ChromiumCacheDirectories {
    profile: NonEmpty<BrowserCacheDirectory>,
    shared: Vec<BrowserCacheDirectory>,
}

impl ChromiumCacheDirectories {
    /// Directories inside each profile (`Default`, `Profile 1`...).
    #[must_use]
    pub fn profile(&self) -> &[BrowserCacheDirectory] {
        &self.profile
    }

    /// Directories shared by every profile, directly under the user-data base.
    #[must_use]
    pub fn shared(&self) -> &[BrowserCacheDirectory] {
        &self.shared
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChromiumCacheDirectories {
    profile: NonEmpty<BrowserCacheDirectory>,
    #[serde(default)]
    shared: Vec<BrowserCacheDirectory>,
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
                .map(BrowserCacheDirectory::directory),
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

/// Firefox, or a browser built on it, that rules know about; written by
/// its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FirefoxKind {
    Firefox,
    Librewolf,
    Waterfox,
    Floorp,
    Zen,
}

impl FirefoxKind {
    pub const ALL: [Self; 5] = [
        Self::Firefox,
        Self::Librewolf,
        Self::Waterfox,
        Self::Floorp,
        Self::Zen,
    ];

    /// Its name as shown: a product's name, never translated.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Firefox => "Firefox",
            Self::Librewolf => "LibreWolf",
            Self::Waterfox => "Waterfox",
            Self::Floorp => "Floorp",
            Self::Zen => "Zen",
        }
    }

    /// Its stable key, as the rules write it.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Firefox => "firefox",
            Self::Librewolf => "librewolf",
            Self::Waterfox => "waterfox",
            Self::Floorp => "floorp",
            Self::Zen => "zen",
        }
    }
}

/// Where Firefox, or a browser built on it, keeps its profiles and their
/// caches.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirefoxBrowser {
    key: FirefoxKind,
    bases: NonEmpty<TemplatePath>,
    cache: BoundedPath,
}

impl FirefoxBrowser {
    #[must_use]
    pub const fn kind(&self) -> FirefoxKind {
        self.key
    }

    /// The directories that may hold its profiles, each a place it has used
    /// (`~/.mozilla/firefox`, then `~/.config/mozilla/firefox`).
    #[must_use]
    pub fn bases(&self) -> &[TemplatePath] {
        &self.bases
    }

    /// The root that holds each profile's disk cache, apart from the profiles
    /// themselves; never a variable alone other than `${TMPDIR}`.
    #[must_use]
    pub fn cache(&self) -> &TemplatePath {
        &self.cache
    }
}

/// A browser whose caches rules know, of either family; written by its key,
/// which no two browsers share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(untagged)]
pub enum BrowserKind {
    Chromium(ChromiumBrowserKind),
    Firefox(FirefoxKind),
}

impl BrowserKind {
    /// Its name as shown: a product's name, never translated.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Chromium(browser) => browser.name(),
            Self::Firefox(browser) => browser.name(),
        }
    }

    /// Its stable key, as the rules write it.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Chromium(browser) => browser.key(),
            Self::Firefox(browser) => browser.key(),
        }
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

/// The items of a list of `field`, each once: two of the same `key` are
/// refused.
fn listed_once<T>(
    items: Vec<T>,
    field: &'static str,
    key: impl Fn(&T) -> String,
) -> Result<Vec<T>, ValidationError> {
    match first_duplicate(items.iter().map(key)) {
        Some(duplicate) => Err(ValidationError::DuplicateEntry {
            field,
            value: duplicate,
        }),
        None => Ok(items),
    }
}

/// The browsers of `chromium`, each listed once: one key names one browser.
fn chromium_once<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ChromiumBrowser>, D::Error> {
    let browsers = Vec::deserialize(deserializer)?;
    listed_once(browsers, "chromium", |browser: &ChromiumBrowser| {
        browser.kind().key().to_owned()
    })
    .map_err(serde::de::Error::custom)
}

/// The browsers of `firefox`, each listed once.
fn firefox_once<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<FirefoxBrowser>, D::Error> {
    let browsers = Vec::deserialize(deserializer)?;
    listed_once(browsers, "firefox", |browser: &FirefoxBrowser| {
        browser.kind().key().to_owned()
    })
    .map_err(serde::de::Error::custom)
}

/// The directories of `firefoxCacheDirs`, at least one, each listed once: a
/// directory names a group of every browser.
fn firefox_directories_once<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<NonEmpty<BrowserCacheDirectory>, D::Error> {
    let directories = Vec::deserialize(deserializer)?;
    listed_once(
        directories,
        "firefoxCacheDirs",
        |directory: &BrowserCacheDirectory| directory.directory().to_string(),
    )
    .and_then(NonEmpty::new)
    .map_err(serde::de::Error::custom)
}

/// The content of `browsers.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserRules {
    #[serde(rename = "type", deserialize_with = "browsers_kind")]
    pub(crate) kind: RuleFileKind,
    #[serde(rename = "chromiumCacheDirs")]
    chromium_cache_directories: ChromiumCacheDirectories,
    #[serde(deserialize_with = "chromium_once")]
    chromium: Vec<ChromiumBrowser>,
    #[serde(
        rename = "firefoxCacheDirs",
        deserialize_with = "firefox_directories_once"
    )]
    firefox_cache_directories: NonEmpty<BrowserCacheDirectory>,
    #[serde(deserialize_with = "firefox_once")]
    firefox: Vec<FirefoxBrowser>,
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

    /// The cache directories of every Firefox-based browser, inside each
    /// profile's cache.
    #[must_use]
    pub fn firefox_cache_directories(&self) -> &[BrowserCacheDirectory] {
        &self.firefox_cache_directories
    }

    #[must_use]
    pub fn firefox(&self) -> &[FirefoxBrowser] {
        &self.firefox
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
        let firefox = self
            .firefox
            .iter()
            .flat_map(|browser| browser.bases().iter().chain([browser.cache()]));
        chromium
            .chain(firefox)
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
        let firefox: FirefoxBrowser = serde_json::from_str(
            r#"{"key": "firefox", "bases": ["${HOME}/.mozilla/firefox", "${CONFIG}/mozilla/firefox"],
                "cache": "${CACHE}/mozilla/firefox"}"#,
        )
        .unwrap();
        assert_eq!(firefox.kind(), FirefoxKind::Firefox);
        let bases: Vec<String> = firefox.bases().iter().map(ToString::to_string).collect();
        assert_eq!(
            bases,
            ["${HOME}/.mozilla/firefox", "${CONFIG}/mozilla/firefox"]
        );
        assert_eq!(firefox.cache().to_string(), "${CACHE}/mozilla/firefox");
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
        for browser in FirefoxKind::ALL {
            let read: FirefoxKind =
                serde_json::from_value(serde_json::Value::from(browser.key())).unwrap();
            assert_eq!(read, browser);
        }
        let every: Vec<BrowserKind> = ChromiumBrowserKind::ALL
            .map(BrowserKind::Chromium)
            .into_iter()
            .chain(FirefoxKind::ALL.map(BrowserKind::Firefox))
            .collect();
        let keys: std::collections::BTreeSet<&str> =
            every.iter().map(|browser| browser.key()).collect();
        let names: std::collections::BTreeSet<&str> =
            every.iter().map(|browser| browser.name()).collect();
        assert_eq!(
            keys.len(),
            every.len(),
            "a key names one browser of either family"
        );
        assert_eq!(names.len(), every.len());
        for browser in every {
            assert_eq!(serde_json::to_value(browser).unwrap(), browser.key());
        }
    }

    /// `browsers.json` with these lists of browsers and Firefox directories.
    fn browsers(chromium: &str, firefox: &str, firefox_directories: &str) -> String {
        format!(
            r#"{{"type": "browsers",
                "chromiumCacheDirs": {{"profile": [{{"dir": "Cache", "label": "Cache"}}]}},
                "firefoxCacheDirs": [{firefox_directories}],
                "chromium": [{chromium}], "firefox": [{firefox}], "safari": null}}"#
        )
    }

    #[test]
    fn a_browser_is_listed_once() {
        let cache2 = r#"{"dir": "cache2", "label": "Cache"}"#;
        serde_json::from_str::<BrowserRules>(&browsers("", "", cache2)).unwrap();
        for (json, field, key) in [
            (
                browsers(
                    r#"{"key": "chrome", "base": "${CONFIG}/a"}, {"key": "chrome", "base": "${CONFIG}/b"}"#,
                    "",
                    cache2,
                ),
                "chromium",
                "chrome",
            ),
            (
                browsers(
                    "",
                    r#"{"key": "zen", "bases": ["${HOME}/a"], "cache": "${CACHE}/a"},
                       {"key": "zen", "bases": ["${HOME}/b"], "cache": "${CACHE}/b"}"#,
                    cache2,
                ),
                "firefox",
                "zen",
            ),
        ] {
            let error = error_of::<BrowserRules>(&json);
            assert!(error.contains("duplicate value"), "{error}");
            assert!(error.contains(&format!("`{field}`")), "{error}");
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn the_firefox_directories_are_some_and_each_once() {
        let empty = error_of::<BrowserRules>(&browsers("", "", ""));
        assert!(empty.starts_with("empty list"), "{empty}");
        let twice = error_of::<BrowserRules>(&browsers(
            "",
            "",
            r#"{"dir": "cache2", "label": "A"}, {"dir": "cache2", "label": "B"}"#,
        ));
        assert!(twice.starts_with("duplicate value"), "{twice}");
        assert!(twice.contains("in `firefoxCacheDirs`"), "{twice}");
        let escape =
            error_of::<BrowserRules>(&browsers("", "", r#"{"dir": "../x", "label": "X"}"#));
        assert!(escape.contains("not a relative path"), "{escape}");
    }

    #[test]
    fn a_cache_directory_is_a_cache_reset_only_when_said() {
        let plain: BrowserCacheDirectory =
            serde_json::from_str(r#"{"dir": "Cache", "label": "Cache"}"#).unwrap();
        assert!(!plain.is_cache_reset());
        let reset: BrowserCacheDirectory = serde_json::from_str(
            r#"{"dir": "Code Cache", "label": "Code Cache", "cacheReset": true}"#,
        )
        .unwrap();
        assert!(reset.is_cache_reset());
    }

    #[test]
    fn a_cache_directory_never_describes_itself_with_a_blank() {
        let error = error_of::<BrowserCacheDirectory>(
            r#"{"dir": "Cache", "label": "Cache", "description": " "}"#,
        );
        assert!(error.contains("empty text"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_browser() {
        let error = error_of::<ChromiumBrowser>(r#"{"key": "netscape", "base": "${CONFIG}/x"}"#);
        assert!(error.starts_with("unknown variant `netscape`"), "{error}");
    }

    #[test]
    fn a_firefox_browser_needs_a_base_and_its_cache() {
        let no_base =
            error_of::<FirefoxBrowser>(r#"{"key": "zen", "bases": [], "cache": "${CACHE}/zen"}"#);
        assert!(no_base.starts_with("empty list"), "{no_base}");
        let no_cache = error_of::<FirefoxBrowser>(r#"{"key": "zen", "bases": ["${HOME}/.zen"]}"#);
        assert!(no_cache.starts_with("missing field `cache`"), "{no_cache}");
        let old_form = error_of::<FirefoxBrowser>(
            r#"{"key": "zen", "base": "${HOME}/.zen", "cache": "${CACHE}/zen"}"#,
        );
        assert!(old_form.starts_with("unknown field `base`"), "{old_form}");
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
            error_of::<FirefoxBrowser>(
                r#"{"key": "firefox", "bases": ["${HOME}/.mozilla/firefox"], "cache": "${CACHE}"}"#,
            ),
            error_of::<SafariPaths>(r#"{"cache": "${CACHES}"}"#),
        ] {
            assert!(error.contains("alone would empty"), "{error}");
        }
    }

    #[test]
    fn rejects_unknown_fields_in_every_browser_type() {
        for error in [
            error_of::<BrowserCacheDirectory>(r#"{"dir": "Cache", "label": "C", "command": "x"}"#),
            error_of::<ChromiumCacheDirectories>(
                r#"{"profile": [{"dir": "C", "label": "C"}], "command": "x"}"#,
            ),
            error_of::<ChromiumBrowser>(
                r#"{"key": "chrome", "base": "${CONFIG}/c", "command": "x"}"#,
            ),
            error_of::<FirefoxBrowser>(
                r#"{"key": "zen", "bases": ["${HOME}/z"], "cache": "${CACHE}/z", "command": "x"}"#,
            ),
            error_of::<SafariPaths>(r#"{"cache": "${CACHES}/s", "command": "x"}"#),
        ] {
            assert!(error.starts_with("unknown field `command`"), "{error}");
        }
    }
}
