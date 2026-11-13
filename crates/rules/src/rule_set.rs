use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer};

use crate::app::{AppCacheRule, AppRules};
use crate::browser::BrowserRules;
use crate::database::{DatabaseRules, DatabaseTarget};
use crate::effect::Effect;
use crate::error::RulesError;
use crate::misc::MiscRules;
use crate::os::Os;
use crate::steam::SteamRules;
use crate::system::{CleanTarget, SystemRules};
use crate::template::TemplatePath;
use crate::values::first_duplicate;

/// One of the rule files of an operating system, named by its `type` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleFileKind {
    System,
    Browsers,
    Apps,
    Gaming,
    GpuCache,
    Steam,
    Databases,
    Misc,
}

impl RuleFileKind {
    /// The name of the file in `data/<os>/`.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::System => "system.json",
            Self::Browsers => "browsers.json",
            Self::Apps => "apps.json",
            Self::Gaming => "gaming.json",
            Self::GpuCache => "gpu-cache.json",
            Self::Steam => "steam.json",
            Self::Databases => "databases.json",
            Self::Misc => "misc.json",
        }
    }
}

impl fmt::Display for RuleFileKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.file_name())
    }
}

/// The content of a rule file, which declares its own kind in `type`.
///
/// Each content type reads `type` as an ordinary field rather than through a
/// serde-tagged enum: a tagged enum buffers the whole file, and its errors then
/// lose their line and column.
trait RuleFileContent: DeserializeOwned {
    fn kind(&self) -> RuleFileKind;
}

macro_rules! rule_file_content {
    ($($content:ty),+) => {
        $(impl RuleFileContent for $content {
            fn kind(&self) -> RuleFileKind {
                self.kind
            }
        })+
    };
}

rule_file_content!(
    SystemRules,
    BrowserRules,
    AppRules,
    SteamRules,
    DatabaseRules,
    MiscRules
);

/// Reads the `type` field of a file whose kind is fixed, so that the content
/// type rejects a file of another kind even when read on its own.
fn expect_kind<'de, D: Deserializer<'de>>(
    deserializer: D,
    expected: RuleFileKind,
) -> Result<RuleFileKind, D::Error> {
    let found = RuleFileKind::deserialize(deserializer)?;
    if found == expected {
        Ok(found)
    } else {
        Err(de::Error::custom(format!(
            "the `type` field announces the content of {found}, expected the content of {expected}"
        )))
    }
}

macro_rules! kind_readers {
    ($($reader:ident => $kind:ident),+) => {
        $(pub(crate) fn $reader<'de, D: Deserializer<'de>>(deserializer: D) -> Result<RuleFileKind, D::Error> {
            expect_kind(deserializer, RuleFileKind::$kind)
        })+
    };
}

kind_readers!(
    system_kind => System,
    browsers_kind => Browsers,
    steam_kind => Steam,
    databases_kind => Databases,
    misc_kind => Misc
);

/// Parses one file. The application files share one content type, so their
/// `type` is only checked here.
fn parse<T: RuleFileContent>(os: Os, file: RuleFileKind, text: &str) -> Result<T, RulesError> {
    let content: T =
        serde_json::from_str(text).map_err(|source| RulesError::Parse { os, file, source })?;
    if content.kind() == file {
        Ok(content)
    } else {
        Err(RulesError::UnexpectedFileType {
            os,
            file,
            found: content.kind(),
        })
    }
}

/// The JSON text of each rule file of one operating system.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RuleSources<'a> {
    pub(crate) system: &'a str,
    pub(crate) browsers: &'a str,
    pub(crate) apps: &'a str,
    pub(crate) gaming: &'a str,
    pub(crate) gpu_cache: &'a str,
    pub(crate) steam: &'a str,
    pub(crate) databases: &'a str,
    pub(crate) misc: &'a str,
}

/// The rule files of `data/<directory>/`, embedded in the binary.
macro_rules! embedded_sources {
    ($directory:literal) => {
        RuleSources {
            system: include_str!(concat!("../data/", $directory, "/system.json")),
            browsers: include_str!(concat!("../data/", $directory, "/browsers.json")),
            apps: include_str!(concat!("../data/", $directory, "/apps.json")),
            gaming: include_str!(concat!("../data/", $directory, "/gaming.json")),
            gpu_cache: include_str!(concat!("../data/", $directory, "/gpu-cache.json")),
            steam: include_str!(concat!("../data/", $directory, "/steam.json")),
            databases: include_str!(concat!("../data/", $directory, "/databases.json")),
            misc: include_str!(concat!("../data/", $directory, "/misc.json")),
        }
    };
}

impl RuleSources<'static> {
    /// The rule files embedded for `os`.
    const fn embedded(os: Os) -> Self {
        match os {
            Os::Linux => embedded_sources!("linux"),
            Os::MacOs => embedded_sources!("macos"),
            Os::Windows => embedded_sources!("windows"),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl<'a> RuleSources<'a> {
    /// The text of `file`, to replace.
    const fn of_kind(&mut self, file: RuleFileKind) -> &mut &'a str {
        match file {
            RuleFileKind::System => &mut self.system,
            RuleFileKind::Browsers => &mut self.browsers,
            RuleFileKind::Apps => &mut self.apps,
            RuleFileKind::Gaming => &mut self.gaming,
            RuleFileKind::GpuCache => &mut self.gpu_cache,
            RuleFileKind::Steam => &mut self.steam,
            RuleFileKind::Databases => &mut self.databases,
            RuleFileKind::Misc => &mut self.misc,
        }
    }
}

/// Every cleaning rule of one operating system, validated as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSet {
    os: Os,
    system: SystemRules,
    browsers: BrowserRules,
    apps: AppRules,
    gaming: AppRules,
    gpu_cache: AppRules,
    steam: SteamRules,
    databases: DatabaseRules,
    misc: MiscRules,
}

impl RuleSet {
    /// The rules shipped with Okapi for `os`.
    ///
    /// # Errors
    ///
    /// Any [`RulesError`] if the embedded files are invalid, which the tests of
    /// this crate rule out for every [`Os`].
    pub fn embedded(os: Os) -> Result<Self, RulesError> {
        Self::from_sources(os, &RuleSources::embedded(os))
    }

    /// The rules shipped with Okapi for `os`, `file` replaced by `text`: rules
    /// whose paths a test chooses, checked as a whole as any rule set is.
    ///
    /// # Errors
    ///
    /// Any [`RulesError`] that `text` causes.
    #[cfg(feature = "test-support")]
    pub fn embedded_with(os: Os, file: RuleFileKind, text: &str) -> Result<Self, RulesError> {
        let mut sources = RuleSources::embedded(os);
        *sources.of_kind(file) = text;
        Self::from_sources(os, &sources)
    }

    pub(crate) fn from_sources(os: Os, sources: &RuleSources<'_>) -> Result<Self, RulesError> {
        let rules = Self {
            os,
            system: parse(os, RuleFileKind::System, sources.system)?,
            browsers: parse(os, RuleFileKind::Browsers, sources.browsers)?,
            apps: parse(os, RuleFileKind::Apps, sources.apps)?,
            gaming: parse(os, RuleFileKind::Gaming, sources.gaming)?,
            gpu_cache: parse(os, RuleFileKind::GpuCache, sources.gpu_cache)?,
            steam: parse(os, RuleFileKind::Steam, sources.steam)?,
            databases: parse(os, RuleFileKind::Databases, sources.databases)?,
            misc: parse(os, RuleFileKind::Misc, sources.misc)?,
        };
        rules.check_paths_exist_on_os()?;
        rules.check_unique_rule_ids()?;
        rules.check_unique_database_labels()?;
        rules.check_unique_admin_targets()?;
        Ok(rules)
    }

    #[must_use]
    pub const fn os(&self) -> Os {
        self.os
    }

    #[must_use]
    pub const fn system(&self) -> &SystemRules {
        &self.system
    }

    #[must_use]
    pub const fn browsers(&self) -> &BrowserRules {
        &self.browsers
    }

    /// Application caches.
    #[must_use]
    pub fn apps(&self) -> &[AppCacheRule] {
        self.apps.apps()
    }

    /// Game launcher caches.
    #[must_use]
    pub fn gaming(&self) -> &[AppCacheRule] {
        self.gaming.apps()
    }

    /// GPU shader caches.
    #[must_use]
    pub fn gpu_cache(&self) -> &[AppCacheRule] {
        self.gpu_cache.apps()
    }

    #[must_use]
    pub const fn steam(&self) -> &SteamRules {
        &self.steam
    }

    #[must_use]
    pub const fn databases(&self) -> &DatabaseRules {
        &self.databases
    }

    #[must_use]
    pub const fn misc(&self) -> &MiscRules {
        &self.misc
    }

    /// The paths of the system targets and application rules that no
    /// variable places, maintenance operations included unless
    /// `deletions_only`: what a test must keep a scan or a cleaning away
    /// from, the real system.
    #[must_use]
    pub fn absolute_targets(&self, deletions_only: bool) -> Vec<PathBuf> {
        let system = self.system();
        let targets = system
            .clean_targets()
            .iter()
            .filter_map(|target| match target.effect() {
                Effect::Delete(deletion) => Some(deletion.path().to_string()),
                Effect::Maintenance { affected, .. } => {
                    (!deletions_only).then(|| affected.to_string())
                }
            });
        let files = system
            .single_file_targets()
            .iter()
            .map(|target| target.path().to_string());
        let applications = self
            .apps()
            .iter()
            .chain(self.gaming())
            .chain(self.gpu_cache())
            .flat_map(|rule| match rule.effect() {
                Effect::Delete(deletion) => deletion.paths().to_vec(),
                Effect::Maintenance { affected, .. } if !deletions_only => affected.to_vec(),
                Effect::Maintenance { .. } => Vec::new(),
            })
            .map(|path| path.to_string());
        targets
            .chain(files)
            .chain(applications)
            .filter(|path| path.starts_with('/'))
            .map(PathBuf::from)
            .collect()
    }

    fn app_files(&self) -> [(RuleFileKind, &AppRules); 3] {
        [
            (RuleFileKind::Apps, &self.apps),
            (RuleFileKind::Gaming, &self.gaming),
            (RuleFileKind::GpuCache, &self.gpu_cache),
        ]
    }

    fn check_paths_exist_on_os(&self) -> Result<(), RulesError> {
        self.check_available(RuleFileKind::System, self.system.template_paths())?;
        self.check_available(RuleFileKind::Browsers, self.browsers.template_paths())?;
        for (file, rules) in self.app_files() {
            self.check_available(file, rules.template_paths())?;
        }
        self.check_available(RuleFileKind::Steam, self.steam.template_paths())?;
        self.check_available(RuleFileKind::Databases, self.databases.template_paths())?;
        self.check_available(RuleFileKind::Misc, self.misc.template_paths())
    }

    fn check_available<'a>(
        &self,
        file: RuleFileKind,
        mut paths: impl Iterator<Item = &'a TemplatePath>,
    ) -> Result<(), RulesError> {
        match paths.find(|path| !path.is_available_on(self.os)) {
            Some(path) => Err(RulesError::PathUnavailable {
                os: self.os,
                file,
                path: path.to_string(),
            }),
            None => Ok(()),
        }
    }

    /// Rule identifiers are unique across the application files (`apps`,
    /// `gaming`, `gpu-cache`), since the interface and the CLI select rules by
    /// identifier.
    fn check_unique_rule_ids(&self) -> Result<(), RulesError> {
        let mut first_file_of = BTreeMap::new();
        for (file, rules) in self.app_files() {
            for rule in rules.apps() {
                if let Some(first) = first_file_of.insert(rule.id(), file) {
                    return Err(RulesError::DuplicateRuleId {
                        os: self.os,
                        id: rule.id().to_string(),
                        first,
                        second: file,
                    });
                }
            }
        }
        Ok(())
    }

    /// The privileged helper is asked for its targets by their path, and
    /// cleans the rules it was asked for: the path a target that needs the
    /// administrator deletes is the path no other target deletes, ordinary
    /// ones included (with the same subcategory, theirs would be the same
    /// rule to it). A maintenance operation deletes nothing: it may share it.
    fn check_unique_admin_targets(&self) -> Result<(), RulesError> {
        fn deleted(target: &CleanTarget) -> Option<&TemplatePath> {
            match target.effect() {
                Effect::Delete(deletion) => Some(deletion.path()),
                Effect::Maintenance { .. } => None,
            }
        }
        let targets = self.system.clean_targets();
        let shared = targets
            .iter()
            .filter(|target| target.needs_admin())
            .filter_map(deleted)
            .find(|path| {
                targets
                    .iter()
                    .filter_map(deleted)
                    .filter(|other| other == path)
                    .count()
                    > 1
            });
        match shared {
            Some(path) => Err(RulesError::DuplicateAdminTarget {
                os: self.os,
                path: path.to_string(),
            }),
            None => Ok(()),
        }
    }

    /// The label is how the user tells two database targets apart.
    fn check_unique_database_labels(&self) -> Result<(), RulesError> {
        let labels = self.databases.targets().iter().map(DatabaseTarget::label);
        match first_duplicate(labels) {
            Some(label) => Err(RulesError::DuplicateDatabaseLabel {
                os: self.os,
                label: label.to_string(),
            }),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::{CleanupAction, Effect};

    const MINIMAL: RuleSources<'static> = RuleSources {
        system: r#"{"type": "system", "cleanTargets": []}"#,
        browsers: r#"{"type": "browsers", "chromiumCacheDirs": {"profile": [{"dir": "Cache", "label": "Cache"}]},
                      "chromium": [], "firefox": {"base": "${HOME}/.mozilla/firefox", "cache": "${CACHE}/mozilla"}}"#,
        apps: r#"{"type": "apps", "apps": []}"#,
        gaming: r#"{"type": "gaming", "apps": []}"#,
        gpu_cache: r#"{"type": "gpu-cache", "apps": []}"#,
        steam: r#"{"type": "steam", "libraries": [], "redistPatterns": []}"#,
        databases: r#"{"type": "databases", "targets": []}"#,
        misc: r#"{"type": "misc", "protectedEventLogs": [], "trashPath": null}"#,
    };

    fn load_minimal_with(file: RuleFileKind, text: &str) -> Result<RuleSet, RulesError> {
        let mut sources = MINIMAL;
        *sources.of_kind(file) = text;
        RuleSet::from_sources(Os::Linux, &sources)
    }

    #[test]
    fn the_minimal_sources_are_valid() {
        RuleSet::from_sources(Os::Linux, &MINIMAL).unwrap();
    }

    #[test]
    fn the_embedded_rules_of_every_os_are_valid() {
        for os in Os::ALL {
            let rules = RuleSet::embedded(os).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(rules.os(), os);
            assert!(!rules.apps().is_empty(), "{os}");
            assert!(!rules.system().clean_targets().is_empty(), "{os}");
            assert!(!rules.databases().targets().is_empty(), "{os}");
            assert!(!rules.gaming().is_empty(), "{os}");
            assert!(!rules.gpu_cache().is_empty(), "{os}");
            assert!(!rules.steam().libraries().is_empty(), "{os}");
            assert!(!rules.steam().redistributable_patterns().is_empty(), "{os}");
            assert!(!rules.browsers().chromium().is_empty(), "{os}");
            assert_eq!(
                rules.misc().trash_path().is_some(),
                os != Os::Windows,
                "{os}"
            );
        }
    }

    /// Every string of a JSON value, decoded (an escaped `\u2014` included).
    fn strings(value: &serde_json::Value, found: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => found.push(text.clone()),
            serde_json::Value::Array(items) => {
                for item in items {
                    strings(item, found);
                }
            }
            serde_json::Value::Object(fields) => {
                for field in fields.values() {
                    strings(field, found);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    fn embedded_texts(sources: &RuleSources<'static>) -> [&'static str; 8] {
        [
            sources.system,
            sources.browsers,
            sources.apps,
            sources.gaming,
            sources.gpu_cache,
            sources.steam,
            sources.databases,
            sources.misc,
        ]
    }

    #[test]
    fn the_embedded_texts_have_no_dash_the_interface_forbids() {
        for sources in [
            embedded_sources!("linux"),
            embedded_sources!("macos"),
            embedded_sources!("windows"),
        ] {
            for text in embedded_texts(&sources) {
                let mut found = Vec::new();
                strings(&serde_json::from_str(text).unwrap(), &mut found);
                for string in found {
                    assert!(!string.contains(['\u{2013}', '\u{2014}']), "{string}");
                }
            }
        }
    }

    #[test]
    fn the_embedded_maintenance_operations_are_exactly_the_expected_ones() {
        use CleanupAction::{
            DeliveryOptimization, DockerBuildPrune, JournalVacuum, PnpmPrune, UvPrune,
            WindowsComponents,
        };
        let apps = [
            ("docker-build-cache", DockerBuildPrune),
            ("pnpm", PnpmPrune),
            ("uv", UvPrune),
        ];
        for (os, targets) in [
            (Os::Linux, vec![("/var/log/journal", JournalVacuum)]),
            (Os::MacOs, vec![]),
            (
                Os::Windows,
                vec![
                    (
                        "${WINDIR}/SoftwareDistribution/DeliveryOptimization",
                        DeliveryOptimization,
                    ),
                    ("${WINDIR}/WinSxS", WindowsComponents),
                ],
            ),
        ] {
            let rules = RuleSet::embedded(os).unwrap();
            let mut found_apps: Vec<(&str, CleanupAction)> = Vec::new();
            for rule in rules
                .apps()
                .iter()
                .chain(rules.gaming())
                .chain(rules.gpu_cache())
            {
                if let Effect::Maintenance { action, .. } = rule.effect() {
                    assert!(!rule.is_selected_by_default(), "{os}: {}", rule.id());
                    found_apps.push((rule.id().as_str(), *action));
                }
            }
            assert_eq!(found_apps, apps, "{os}");
            let mut found_targets: Vec<(String, CleanupAction)> = Vec::new();
            for target in rules.system().clean_targets() {
                if let Effect::Maintenance { action, affected } = target.effect() {
                    assert!(!target.is_selected_by_default(), "{os}: {affected}");
                    found_targets.push((affected.to_string(), *action));
                }
            }
            let expected: Vec<(String, CleanupAction)> = targets
                .into_iter()
                .map(|(path, action)| (path.to_owned(), action))
                .collect();
            assert_eq!(found_targets, expected, "{os}");
        }
    }

    #[test]
    fn the_performance_caches_of_browsers_are_never_preselected() {
        let resets = [
            "Code Cache",
            "GPUCache",
            "Service Worker/CacheStorage",
            "DawnWebGPUCache",
            "DawnGraphiteCache",
            "GrShaderCache",
            "ShaderCache",
            "GraphiteDawnCache",
            "GPUPersistentCache",
            "PnaclTranslationCache",
        ];
        for os in Os::ALL {
            let rules = RuleSet::embedded(os).unwrap();
            let directories = rules.browsers().chromium_cache_directories();
            for directory in directories.profile().iter().chain(directories.shared()) {
                let name = directory.directory().to_string();
                assert_eq!(
                    directory.is_cache_reset(),
                    resets.contains(&name.as_str()),
                    "{os}: {name}"
                );
            }
        }
    }

    #[test]
    fn the_linux_font_caches_are_never_preselected() {
        let rules = RuleSet::embedded(Os::Linux).unwrap();
        let fonts: Vec<(String, bool)> = rules
            .system()
            .clean_targets()
            .iter()
            .filter(|target| target.template_path().to_string().ends_with("/fontconfig"))
            .map(|target| {
                (
                    target.template_path().to_string(),
                    target.is_selected_by_default(),
                )
            })
            .collect();
        assert_eq!(
            fonts,
            [
                ("${CACHE}/fontconfig".to_owned(), false),
                ("/var/cache/fontconfig".to_owned(), false),
            ]
        );
    }

    #[test]
    fn the_linux_temporary_directories_keep_a_month() {
        let rules = RuleSet::embedded(Os::Linux).unwrap();
        let ages: Vec<(String, Option<u16>)> = rules
            .system()
            .clean_targets()
            .iter()
            .filter_map(|target| match target.effect() {
                Effect::Delete(deletion) => Some(deletion),
                Effect::Maintenance { .. } => None,
            })
            .filter(|deletion| {
                ["${TMPDIR}", "/tmp", "/var/tmp"].contains(&deletion.path().to_string().as_str())
            })
            .map(|deletion| {
                (
                    deletion.path().to_string(),
                    deletion.min_age().map(crate::AgeDays::days),
                )
            })
            .collect();
        assert_eq!(
            ages,
            [
                ("${TMPDIR}".to_owned(), Some(30)),
                ("/tmp".to_owned(), Some(30)),
                ("/var/tmp".to_owned(), Some(30)),
            ]
        );
    }

    #[test]
    fn the_windows_event_logs_are_protected() {
        let rules = RuleSet::embedded(Os::Windows).unwrap();
        for name in [
            "Security.evtx",
            "System.evtx",
            "Microsoft-Windows-GroupPolicy%4Operational.evtx",
            "Microsoft-Windows-AppLocker%4EXE and DLL.evtx",
        ] {
            assert!(rules.misc().protects(std::path::Path::new(name)), "{name}");
        }
        assert!(!rules.misc().protects(std::path::Path::new(
            "Microsoft-Windows-Shell-Core%4Operational.evtx"
        )));
    }

    #[test]
    fn rejects_unknown_fields_at_the_root_of_every_file() {
        for (file, text) in [
            (RuleFileKind::System, MINIMAL.system),
            (RuleFileKind::Browsers, MINIMAL.browsers),
            (RuleFileKind::Apps, MINIMAL.apps),
            (RuleFileKind::Gaming, MINIMAL.gaming),
            (RuleFileKind::GpuCache, MINIMAL.gpu_cache),
            (RuleFileKind::Steam, MINIMAL.steam),
            (RuleFileKind::Databases, MINIMAL.databases),
            (RuleFileKind::Misc, MINIMAL.misc),
        ] {
            let with_command = text.replacen('{', r#"{"command": "rm -rf ~", "#, 1);
            let error = load_minimal_with(file, &with_command).unwrap_err();
            assert!(
                matches!(&error, RulesError::Parse { file: found, source, .. }
                    if *found == file && source.to_string().starts_with("unknown field `command`")),
                "{file}: {error}"
            );
        }
    }

    #[test]
    fn a_file_type_read_on_its_own_still_checks_its_type() {
        let expected = "the `type` field announces the content of misc.json";
        let errors = [
            serde_json::from_str::<SystemRules>(r#"{"type": "misc", "cleanTargets": []}"#)
                .map(|_| ()),
            serde_json::from_str::<BrowserRules>(&MINIMAL.browsers.replacen("browsers", "misc", 1))
                .map(|_| ()),
            serde_json::from_str::<SteamRules>(
                r#"{"type": "misc", "libraries": [], "redistPatterns": []}"#,
            )
            .map(|_| ()),
            serde_json::from_str::<DatabaseRules>(r#"{"type": "misc", "targets": []}"#).map(|_| ()),
            serde_json::from_str::<MiscRules>(
                r#"{"type": "steam", "protectedEventLogs": [], "trashPath": null}"#,
            )
            .map(|_| ()),
        ];
        for (index, result) in errors.into_iter().enumerate() {
            let error = result.unwrap_err().to_string();
            let expected = if index == 4 {
                "the `type` field announces the content of steam.json"
            } else {
                expected
            };
            assert!(error.starts_with(expected), "{error}");
        }
    }

    const VALID: &str = "${HOME}/valid";
    const FOREIGN: &str = "${LOCALAPPDATA}/foreign";

    /// The foreign path is last in its list, behind a valid one, so a check
    /// that stops at the first path cannot pass.
    fn assert_foreign_path_rejected(file: RuleFileKind, text: &str) {
        let error = load_minimal_with(file, text).unwrap_err();
        assert!(
            matches!(&error, RulesError::PathUnavailable { file: found, path, .. } if *found == file && path == FOREIGN),
            "{file}: {error}"
        );
    }

    #[test]
    fn rejects_a_variable_of_another_os_in_every_browser_path() {
        let browsers = |chromium: &str, firefox: &str, forks: &str, safari: &str| {
            format!(
                r#"{{"type": "browsers", "chromiumCacheDirs": {{"profile": [{{"dir": "Cache", "label": "Cache"}}]}},
                     "chromium": [{chromium}], "firefox": {firefox}, "firefoxForks": [{forks}], "safari": {safari}}}"#
            )
        };
        let firefox = format!(r#"{{"base": "{VALID}", "cache": "{VALID}/cache"}}"#);
        let chrome_base = format!(r#"{{"key": "chrome", "base": "{FOREIGN}"}}"#);
        let chrome_cache =
            format!(r#"{{"key": "chrome", "base": "{VALID}", "cache": "{FOREIGN}"}}"#);
        let firefox_base = format!(r#"{{"base": "{FOREIGN}", "cache": "{VALID}"}}"#);
        let firefox_cache = format!(r#"{{"base": "{VALID}", "cache": "{FOREIGN}"}}"#);
        let fork_base = format!(r#"{{"key": "zen", "base": "{FOREIGN}", "cache": "{VALID}"}}"#);
        let fork_cache = format!(r#"{{"key": "zen", "base": "{VALID}", "cache": "{FOREIGN}"}}"#);
        let safari = format!(r#"{{"cache": "{FOREIGN}"}}"#);
        for text in [
            browsers(&chrome_base, &firefox, "", "null"),
            browsers(&chrome_cache, &firefox, "", "null"),
            browsers("", &firefox_base, "", "null"),
            browsers("", &firefox_cache, "", "null"),
            browsers("", &firefox, &fork_base, "null"),
            browsers("", &firefox, &fork_cache, "null"),
            browsers("", &firefox, "", &safari),
        ] {
            assert_foreign_path_rejected(RuleFileKind::Browsers, &text);
        }
    }

    #[test]
    fn rejects_a_variable_of_another_os_in_every_other_file() {
        let apps = |kind: &str| {
            format!(
                r#"{{"type": "{kind}", "apps": [{{"id": "a", "name": "A", "paths": ["{VALID}", "{FOREIGN}"],
                     "childSubdir": "cache"}}]}}"#
            )
        };
        let maintenance = |kind: &str| {
            format!(
                r#"{{"type": "{kind}", "apps": [{{"id": "a", "name": "A", "paths": ["{VALID}", "{FOREIGN}"],
                     "cleanupAction": "uv-prune"}}]}}"#
            )
        };
        let targets = format!(
            r#"[{{"path": "{VALID}", "subcategory": "A"}}, {{"path": "{FOREIGN}", "subcategory": "B"}}]"#
        );
        let cases = [
            (
                RuleFileKind::System,
                format!(r#"{{"type": "system", "cleanTargets": {targets}}}"#),
            ),
            (
                RuleFileKind::System,
                format!(
                    r#"{{"type": "system", "cleanTargets": [], "singleFileTargets": {targets}}}"#
                ),
            ),
            (RuleFileKind::Apps, apps("apps")),
            (RuleFileKind::Apps, maintenance("apps")),
            (RuleFileKind::Gaming, maintenance("gaming")),
            (RuleFileKind::GpuCache, maintenance("gpu-cache")),
            (
                RuleFileKind::System,
                format!(
                    r#"{{"type": "system", "cleanTargets": [{{"path": "{VALID}", "subcategory": "A"}},
                         {{"path": "{FOREIGN}", "subcategory": "B", "cleanupAction": "journal-vacuum"}}]}}"#
                ),
            ),
            (RuleFileKind::Gaming, apps("gaming")),
            (RuleFileKind::GpuCache, apps("gpu-cache")),
            (
                RuleFileKind::Steam,
                format!(
                    r#"{{"type": "steam", "libraries": ["{VALID}", "{FOREIGN}"], "redistPatterns": []}}"#
                ),
            ),
            (
                RuleFileKind::Databases,
                format!(
                    r#"{{"type": "databases", "targets": [{{"label": "A", "basePath": "{VALID}", "dbFiles": ["a"]}},
                         {{"label": "B", "basePath": "{FOREIGN}", "dbFiles": ["b"]}}]}}"#
                ),
            ),
            (
                RuleFileKind::Misc,
                format!(
                    r#"{{"type": "misc", "protectedEventLogs": [], "trashPath": "{FOREIGN}"}}"#
                ),
            ),
        ];
        for (file, text) in cases {
            assert_foreign_path_rejected(file, &text);
        }
    }

    #[test]
    fn rejects_a_windows_drive_in_linux_rules() {
        let error = load_minimal_with(
            RuleFileKind::Steam,
            r#"{"type": "steam", "libraries": ["D:/SteamLibrary"], "redistPatterns": []}"#,
        )
        .unwrap_err();
        assert!(
            matches!(&error, RulesError::PathUnavailable { file: RuleFileKind::Steam, path, .. } if path == "D:/SteamLibrary"),
            "{error}"
        );
    }

    #[test]
    fn rejects_a_file_of_the_wrong_type() {
        let error = load_minimal_with(RuleFileKind::Gaming, MINIMAL.apps).unwrap_err();
        assert!(
            matches!(
                error,
                RulesError::UnexpectedFileType {
                    file: RuleFileKind::Gaming,
                    found: RuleFileKind::Apps,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn rejects_unknown_fields_at_the_top_of_a_file() {
        let error = load_minimal_with(
            RuleFileKind::Misc,
            r#"{"type": "misc", "protectedEventLogs": [], "trashPath": null, "postCleanupScript": "rm -rf ~"}"#,
        )
        .unwrap_err();
        assert!(
            matches!(&error, RulesError::Parse { file: RuleFileKind::Misc, source, .. }
                if source.to_string().starts_with("unknown field `postCleanupScript`")),
            "{error}"
        );
        assert!(error.to_string().contains("misc.json"), "{error}");
    }

    #[test]
    fn a_validation_error_points_at_the_line_of_the_faulty_value() {
        for text in [
            "{\"type\": \"misc\",\n \"trashPath\": \"${CACHE}/../x\",\n \"protectedEventLogs\": []\n}",
            "{\"type\": \"misc\",\n \"protectedEventLogs\": [\"a\", \"a\"],\n \"trashPath\": null\n}",
        ] {
            let error = load_minimal_with(RuleFileKind::Misc, text).unwrap_err();
            assert!(
                matches!(&error, RulesError::Parse { source, .. } if source.line() == 2),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_a_rule_identifier_used_twice() {
        let rule = r#"{"id": "discord", "name": "D", "paths": ["${CACHE}/d"]}"#;
        let twice_in_apps = format!(r#"{{"type": "apps", "apps": [{rule}, {rule}]}}"#);
        let error = load_minimal_with(RuleFileKind::Apps, &twice_in_apps).unwrap_err();
        assert!(
            matches!(&error, RulesError::DuplicateRuleId { id, first: RuleFileKind::Apps, second: RuleFileKind::Apps, .. } if id == "discord"),
            "{error}"
        );

        let mut sources = MINIMAL;
        let apps = format!(r#"{{"type": "apps", "apps": [{rule}]}}"#);
        let gaming = format!(r#"{{"type": "gaming", "apps": [{rule}]}}"#);
        sources.apps = &apps;
        sources.gaming = &gaming;
        let error = RuleSet::from_sources(Os::Linux, &sources).unwrap_err();
        assert!(
            matches!(
                &error,
                RulesError::DuplicateRuleId {
                    first: RuleFileKind::Apps,
                    second: RuleFileKind::Gaming,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn rejects_a_path_an_admin_target_shares() {
        let admin = r#"{"path": "/var/crash", "subcategory": "Crash", "needsAdmin": true}"#;
        let other_admin = r#"{"path": "/var/crash", "subcategory": "Dumps", "needsAdmin": true}"#;
        // Same path and subcategory: the same rule to the helper.
        let ordinary = r#"{"path": "/var/crash", "subcategory": "Crash", "childSubdir": "x"}"#;
        for pair in [[admin, other_admin], [admin, ordinary], [ordinary, admin]] {
            let error = load_minimal_with(
                RuleFileKind::System,
                &format!(
                    r#"{{"type": "system", "cleanTargets": [{}, {}]}}"#,
                    pair[0], pair[1]
                ),
            )
            .unwrap_err();
            assert!(
                matches!(&error, RulesError::DuplicateAdminTarget { path, .. } if path == "/var/crash"),
                "{error}"
            );
        }
        let ordinary = r#"{"path": "/var/tmp", "subcategory": "Temp"}"#;
        let twice = format!(r#"{{"type": "system", "cleanTargets": [{ordinary}, {ordinary}]}}"#);
        load_minimal_with(RuleFileKind::System, &twice)
            .expect("ordinary targets are never asked for by their path");
        let maintenance = r#"{"path": "/var/crash", "subcategory": "Crash", "needsAdmin": true, "cleanupAction": "journal-vacuum"}"#;
        let beside = format!(r#"{{"type": "system", "cleanTargets": [{admin}, {maintenance}]}}"#);
        load_minimal_with(RuleFileKind::System, &beside)
            .expect("a maintenance operation deletes nothing: no rule to the helper");
    }

    #[test]
    fn rejects_two_database_targets_with_the_same_label() {
        let error = load_minimal_with(
            RuleFileKind::Databases,
            r#"{"type": "databases", "targets": [
                {"label": "Chrome", "basePath": "${CONFIG}/a", "dbFiles": ["History"]},
                {"label": "Chrome", "basePath": "${CONFIG}/b", "dbFiles": ["History"]}]}"#,
        )
        .unwrap_err();
        assert!(
            matches!(&error, RulesError::DuplicateDatabaseLabel { label, .. } if label == "Chrome"),
            "{error}"
        );
    }
}
