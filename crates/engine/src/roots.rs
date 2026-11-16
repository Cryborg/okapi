use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use std::collections::BTreeMap;
use std::ffi::OsString;

use okapi_platform::{CHROMIUM_LOCK, FIREFOX_LOCK, FileIdentity};
use okapi_rules::{
    AgeDays, AppCacheRule, BrowserCacheDirectory, BrowserKind, ChromiumBrowser, CleanTarget,
    Effect, FirefoxBrowser, Label, MiscRules, Name, PathMatcher, RelativePath, SingleFileTarget,
    TemplatePath, TemplateVariables, Variable,
};

use crate::custom::{CustomFolder, CustomFolderRefusal, Guard, PersonalFolders};
use crate::options::ScanOptions;
use crate::paths::{NotADirectory, is_absent, is_system_temporary, real_directory};
use crate::protection::ProtectedRoots;
use crate::report::{
    AppFile, BrowserLock, Category, EntryKind, GroupAttributes, Origin, ScanIssue, ScanStats,
    UnsafeReason,
};
use crate::walk::{Found, Readers, by_name};

/// A rule to scan, from any of the rule files the scan handles.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RuleEntry<'a> {
    Target(&'a CleanTarget),
    File(&'a SingleFileTarget),
    App {
        file: AppFile,
        rule: &'a AppCacheRule,
    },
    /// A cache directory of a browser, in each profile or, `shared`, once
    /// for all of them (Chromium only).
    Browser {
        browser: BrowserRule<'a>,
        directory: &'a BrowserCacheDirectory,
        shared: bool,
    },
    /// A folder the user chose to have emptied.
    Custom(&'a CustomFolder),
}

/// Where the rules say a browser keeps its data, by its family.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BrowserRule<'a> {
    Chromium(&'a ChromiumBrowser),
    Firefox(&'a FirefoxBrowser),
}

impl BrowserRule<'_> {
    pub(crate) const fn kind(self) -> BrowserKind {
        match self {
            Self::Chromium(browser) => BrowserKind::Chromium(browser.kind()),
            Self::Firefox(browser) => BrowserKind::Firefox(browser.kind()),
        }
    }
}

impl RuleEntry<'_> {
    pub(crate) const fn category(&self) -> Category {
        match self {
            Self::Target(_) | Self::File(_) => Category::System,
            Self::App { file, .. } => Category::of_file(*file),
            Self::Browser { .. } => Category::Browsers,
            Self::Custom(_) => Category::Custom,
        }
    }

    /// Whether the rule is selected before the user chooses (invariant 5):
    /// never a folder of the user's, whose contents are the user's to pick
    /// each time.
    pub(crate) const fn is_selected_by_default(&self) -> bool {
        match self {
            Self::Target(target) => target.is_selected_by_default(),
            Self::File(target) => target.is_selected_by_default(),
            Self::App { rule, .. } => rule.is_selected_by_default(),
            Self::Browser { directory, .. } => directory.is_selected_by_default(),
            Self::Custom(_) => false,
        }
    }
}

/// The first profile of a Chromium-based browser.
const DEFAULT_PROFILE: &str = "Default";
/// How the other profiles are named (`Profile 2`); `System Profile` and
/// `Guest Profile` are no user's.
const OTHER_PROFILES: &str = "Profile ";

/// A canonical directory and its device, `None` when it is not known.
type Home = (PathBuf, Option<u64>);

/// Where a browser keeps what its caches are found under, read once for all
/// its directories.
#[derive(Debug, Clone)]
pub(crate) struct BrowserBases {
    /// Chromium's user-data directory, which holds its profiles and its
    /// shared caches; Firefox keeps no cache with its profiles.
    base: Option<Home>,
    /// Where the OS keeps its disk caches apart, if it does.
    cache: Option<Home>,
    /// The profiles' names; none when a Chromium browser keeps all at its
    /// base. A Firefox browser without any has no bases.
    profiles: Vec<OsString>,
    /// The locks that say the browser runs: one per profile for Firefox. The
    /// caches of every profile wait for all of them.
    locks: BTreeSet<BrowserLock>,
}

/// What a Firefox base holds.
#[derive(Default)]
struct FirefoxProfiles {
    /// Its profiles, by name and path.
    judged: Vec<(OsString, PathBuf)>,
    /// The names of directories that may be profiles but cannot be judged:
    /// links, on another filesystem, or with a lock that cannot be read.
    doubtful: Vec<OsString>,
}

/// What a planned root is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlannedKind {
    /// A directory whose contents are walked.
    Directory,
    /// A single file, already found offerable.
    File(Found),
    /// A directory a rule reaches only across a link, another device or a
    /// mount, or a file target that is a bind mount: never walked nor offered
    /// for that rule, it still joins the group of the rules that share it, so
    /// that their AND (invariant 5) and strictest age hold for whoever walks
    /// or offers it. Met below another rule's root, it is reserved as any root
    /// is: then nobody offers it, rather than that rule without this one's
    /// restrictions.
    Withheld,
}

/// A root an expansion reached, walked or withheld.
struct Candidate {
    path: PathBuf,
    withheld: bool,
}

impl Candidate {
    fn walked(path: PathBuf) -> Self {
        Self {
            path,
            withheld: false,
        }
    }
}

/// What a segment of an expansion is.
enum Segment {
    /// A plain directory on the base's filesystem: the expansion goes on.
    Plain,
    /// A link, another device or a mount: never entered.
    Refused,
    /// Absent, or not a directory.
    Missing,
    /// Unreadable (reported).
    Unreadable,
}

/// A canonical root, checked, with the group it fills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedRoot {
    pub(crate) path: PathBuf,
    pub(crate) identity: FileIdentity,
    pub(crate) cutoff: SystemTime,
    pub(crate) attributes: GroupAttributes,
    pub(crate) kind: PlannedKind,
}

/// What planning the roots reads: each one a seam the tests set apart.
#[derive(Clone, Copy)]
pub(crate) struct Planning<'a> {
    pub(crate) misc: &'a MiscRules,
    pub(crate) variables: &'a TemplateVariables,
    pub(crate) options: &'a ScanOptions,
    pub(crate) readers: Readers,
    /// Who will remove what is offered, and must own a custom folder.
    pub(crate) user: Option<u32>,
    /// What no custom folder may be, besides the variables' directories.
    pub(crate) personal: &'a PersonalFolders,
}

/// Turns rules into checked roots, recording why a root is not walked.
pub(crate) struct Planner<'a> {
    pub(crate) planning: Planning<'a>,
    pub(crate) protected: &'a ProtectedRoots,
    pub(crate) issues: Vec<ScanIssue>,
    pub(crate) stats: ScanStats,
    /// The bases of each browser met, `None` when it has none to scan.
    pub(crate) browsers: BTreeMap<BrowserKind, Option<BrowserBases>>,
}

impl Planner<'_> {
    pub(crate) fn plan(&mut self, entry: RuleEntry<'_>) -> Vec<PlannedRoot> {
        match entry {
            RuleEntry::Custom(folder) => self.plan_custom(folder),
            RuleEntry::Target(target) => self.plan_target(target),
            RuleEntry::File(target) => self.plan_file(target).into_iter().collect(),
            RuleEntry::App { file, rule } => self.plan_app(file, rule),
            RuleEntry::Browser {
                browser,
                directory,
                shared,
            } => self.plan_browser(browser, directory, shared),
        }
    }

    fn plan_browser(
        &mut self,
        browser: BrowserRule<'_>,
        directory: &BrowserCacheDirectory,
        shared: bool,
    ) -> Vec<PlannedRoot> {
        let origin = Origin::Browser {
            browser: browser.kind(),
            directory: directory.directory().clone(),
        };
        let Some(bases) = self.browser_bases(&origin, browser) else {
            return Vec::new();
        };
        let Some((first, rest)) = directory.directory().segments().split_first() else {
            return Vec::new();
        };
        let attributes = GroupAttributes {
            origins: vec![origin.clone()],
            label: directory.label().clone(),
            descriptions: directory.description().cloned().into_iter().collect(),
            rule_group: None,
            cache_reset: directory.is_cache_reset(),
            selected_by_default: directory.is_selected_by_default(),
            needs_admin: false,
            keep_hard_linked: false,
            locks: bases.locks.clone(),
        };
        // A shared directory is under the base only; a profile's, under the
        // base and the cache, in each profile (or at their roots without any).
        let homes = bases
            .base
            .iter()
            .chain(bases.cache.iter().filter(|_| !shared));
        let mut roots = Vec::new();
        for (home, device) in homes {
            if shared || bases.profiles.is_empty() {
                roots.extend(self.reach(&origin, home.join(first.as_str()), rest, *device));
            } else {
                for profile in &bases.profiles {
                    let segments = directory.directory().segments();
                    roots.extend(self.reach(&origin, home.join(profile), segments, *device));
                }
            }
        }
        self.check_roots(&origin, roots, None, &attributes)
    }

    /// The bases of `browser`, read the first time one of its directories is
    /// planned: a base that cannot be used is reported once.
    fn browser_bases(&mut self, origin: &Origin, browser: BrowserRule<'_>) -> Option<BrowserBases> {
        if let Some(known) = self.browsers.get(&browser.kind()) {
            return known.clone();
        }
        let bases = match browser {
            BrowserRule::Chromium(chromium) => self.chromium_bases(origin, chromium),
            BrowserRule::Firefox(firefox) => self.firefox_bases(origin, firefox),
        };
        self.browsers.insert(browser.kind(), bases.clone());
        bases
    }

    fn chromium_bases(
        &mut self,
        origin: &Origin,
        browser: &ChromiumBrowser,
    ) -> Option<BrowserBases> {
        let base = self.open_base(origin, browser.base())?;
        let base_device = self.device(origin, &base).ok()?;
        let cache = match browser.cache() {
            Some(cache) => self.open_base(origin, cache).and_then(|cache| {
                let device = self.device(origin, &cache).ok()?;
                Some((cache, device))
            }),
            None => None,
        };
        let entries = match fs::read_dir(&base) {
            Ok(entries) => entries,
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(origin, base, error.kind()),
        };
        // By name only: `reach` judges each one as any segment.
        let mut profiles: Vec<OsString> = entries
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry.file_name()),
                Err(error) => {
                    tracing::debug!(base = ?base, %error, "browser base entry unreadable");
                    self.stats.unreadable += 1;
                    None
                }
            })
            .filter(|name| {
                name == DEFAULT_PROFILE
                    || name
                        .to_str()
                        .is_some_and(|name| name.starts_with(OTHER_PROFILES))
            })
            .collect();
        profiles.sort();
        tracing::debug!(
            browser = browser.kind().key(),
            base = ?base,
            profiles = profiles.len(),
            "browser bases"
        );
        let lock = BrowserLock::new(
            BrowserKind::Chromium(browser.kind()),
            base.join(CHROMIUM_LOCK),
            base.clone(),
        );
        Some(BrowserBases {
            base: Some((base, base_device)),
            cache,
            profiles,
            locks: BTreeSet::from([lock]),
        })
    }

    /// A Firefox browser's profiles: the directories of its cache that are,
    /// under one of its bases, a plain directory holding the lock a running
    /// browser takes (a profile never launched has neither the lock nor a
    /// cache), and under none a directory that cannot be judged. A base
    /// absent is passed over; one refused or unreadable leaves the browser
    /// without bases, a profile there being unknown.
    fn firefox_bases(&mut self, origin: &Origin, browser: &FirefoxBrowser) -> Option<BrowserBases> {
        let kind = BrowserKind::Firefox(browser.kind());
        let Some(cache) = self.open_base(origin, browser.cache()) else {
            tracing::debug!(browser = kind.key(), cache = %browser.cache(), "browser cache absent or refused: nothing to clean");
            return None;
        };
        let Ok(cache_device) = self.device(origin, &cache) else {
            tracing::debug!(browser = kind.key(), cache = ?cache, "browser cache gone or unreadable: nothing to clean");
            return None;
        };
        let mut profiles = BTreeSet::new();
        let mut doubtful = BTreeSet::new();
        let mut locks = BTreeSet::new();
        for template in browser.bases() {
            let Ok(found) = self.read_base(origin, template) else {
                tracing::info!(browser = kind.key(), base = %template, "browser base refused: its caches are left");
                return None;
            };
            let Some(base) = found else {
                continue;
            };
            let Some(base_profiles) = self.firefox_profiles(origin, &base) else {
                tracing::info!(browser = kind.key(), base = ?base, "browser base unreadable or gone while read: its caches are left");
                return None;
            };
            for (name, profile) in base_profiles.judged {
                locks.insert(BrowserLock::new(kind, profile.join(FIREFOX_LOCK), profile));
                profiles.insert(name);
            }
            doubtful.extend(base_profiles.doubtful);
        }
        // A profile of the same name elsewhere could be the one open.
        for name in &doubtful {
            if profiles.remove(name) {
                tracing::debug!(browser = kind.key(), profile = ?name, "profile not judged under every base: its caches are left");
            }
        }
        tracing::info!(
            browser = kind.key(),
            profiles = profiles.len(),
            not_judged = doubtful.len(),
            "browser profiles"
        );
        if profiles.is_empty() {
            return None;
        }
        Some(BrowserBases {
            base: None,
            cache: Some((cache, cache_device)),
            profiles: profiles.into_iter().collect(),
            locks,
        })
    }

    /// The profiles under the Firefox base `base`; `None` when it, or one of
    /// its entries, cannot be read (reported): a profile there is unknown.
    fn firefox_profiles(&mut self, origin: &Origin, base: &Path) -> Option<FirefoxProfiles> {
        let entries = match fs::read_dir(base) {
            // Gone since it was opened: as a base absent.
            Err(error) if is_absent(&error) => return Some(FirefoxProfiles::default()),
            Err(error) => return self.unreadable(origin, base.to_path_buf(), error.kind()),
            Ok(entries) => entries,
        };
        let device = self.device(origin, base).ok()?;
        let mut found = FirefoxProfiles::default();
        for entry in entries {
            let name = match entry {
                Ok(entry) => entry.file_name(),
                Err(error) => return self.unreadable(origin, base.to_path_buf(), error.kind()),
            };
            let profile = base.join(&name);
            match self.segment(origin, &profile, device) {
                Segment::Plain => {}
                // A file, or gone (`profiles.ini`): no profile.
                Segment::Missing => continue,
                Segment::Refused | Segment::Unreadable => {
                    tracing::debug!(profile = ?profile, "profile a link, on another filesystem or unreadable: not judged");
                    found.doubtful.push(name);
                    continue;
                }
            }
            match fs::symlink_metadata(profile.join(FIREFOX_LOCK)) {
                Ok(_) => {
                    tracing::debug!(profile = ?profile, "profile");
                    found.judged.push((name, profile));
                }
                Err(error) if is_absent(&error) => {
                    tracing::debug!(profile = ?profile, "no lock: not a profile launched");
                }
                Err(error) => {
                    tracing::debug!(profile = ?profile, %error, "profile lock unreadable: not judged");
                    self.unreadable::<()>(origin, profile, error.kind());
                    found.doubtful.push(name);
                }
            }
        }
        Some(found)
    }

    /// The device of the directory `base`, `None` when it is not known; an
    /// error when `base` is gone or unreadable (reported).
    fn device(&mut self, origin: &Origin, base: &Path) -> Result<Option<u64>, ()> {
        match fs::symlink_metadata(base) {
            Ok(metadata) => {
                Ok((self.planning.readers.identity)(&metadata).map(FileIdentity::device))
            }
            Err(error) if is_absent(&error) => Err(()),
            Err(error) => {
                self.unreadable::<()>(origin, base.to_path_buf(), error.kind());
                Err(())
            }
        }
    }

    fn plan_target(&mut self, target: &CleanTarget) -> Vec<PlannedRoot> {
        let Effect::Delete(deletion) = target.effect() else {
            return Vec::new();
        };
        let origin = Origin::of_target(target);
        let Some(base) = self.open_base(&origin, deletion.path()) else {
            return Vec::new();
        };
        // Judged on the real path: `/tmp/x` may be a link to a folder of the
        // user's.
        let presumed_temporary = deletion.path().variable() != Some(Variable::TempDirectory)
            || is_system_temporary(&base);
        if !presumed_temporary {
            tracing::debug!(base = ?base, "temporary directory outside /tmp and /var/tmp: left to choose");
        }
        let attributes = GroupAttributes {
            origins: vec![origin.clone()],
            label: target.subcategory().clone(),
            descriptions: target.description().cloned().into_iter().collect(),
            rule_group: None,
            cache_reset: deletion.is_cache_reset(),
            selected_by_default: target.is_selected_by_default() && presumed_temporary,
            needs_admin: target.needs_admin(),
            keep_hard_linked: deletion.keeps_hard_linked(),
            locks: BTreeSet::new(),
        };
        let roots = match deletion.child_subdirectory() {
            Some(subdirectory) => self.expand(&origin, &base, subdirectory),
            None => vec![Candidate::walked(base)],
        };
        let rule_age = deletion.min_age().map(AgeDays::duration);
        self.check_roots(&origin, roots, rule_age, &attributes)
    }

    /// The folder's contents, if it is one a scan may walk
    /// ([`CustomFolder::locate`]); and the user's exclusions, as for any root.
    fn plan_custom(&mut self, folder: &CustomFolder) -> Vec<PlannedRoot> {
        let origin = Origin::Custom {
            folder: folder.clone(),
        };
        let guard = Guard {
            protected: self.protected,
            personal: self.planning.personal,
            user: self.planning.user,
            readers: self.planning.readers,
        };
        let refuse = |planner: &mut Self, refusal| {
            planner.issues.push(ScanIssue::CustomFolderRefused {
                origin: origin.clone(),
                refusal,
            });
            Vec::new()
        };
        let (path, judged) = match folder.locate(&guard) {
            Ok(located) => located,
            Err(refusal) => return refuse(self, refusal),
        };
        // Its path is the user's, shown as written: never a text of the
        // rules to translate.
        let Ok(label) = Label::try_from(folder.path().to_string_lossy().into_owned()) else {
            tracing::error!(folder = %folder.path().display(), "custom folder without a label: not scanned");
            return refuse(
                self,
                CustomFolderRefusal::Unreadable(io::ErrorKind::InvalidData),
            );
        };
        let attributes = GroupAttributes {
            origins: vec![origin.clone()],
            label,
            descriptions: Vec::new(),
            rule_group: None,
            cache_reset: false,
            selected_by_default: RuleEntry::Custom(folder).is_selected_by_default(),
            needs_admin: false,
            keep_hard_linked: false,
            locks: BTreeSet::new(),
        };
        let issues = self.issues.len();
        let planned = self.check_roots(&origin, vec![Candidate::walked(path)], None, &attributes);
        // Gone since it was judged: said, as when it was not there.
        if planned.is_empty() && self.issues.len() == issues {
            return refuse(self, CustomFolderRefusal::Absent);
        }
        // Read again by `check_roots`: another folder put there since it was
        // judged is never walked in its place.
        if planned.iter().any(|root| root.identity != judged) {
            tracing::info!(folder = %folder.path().display(), "custom folder replaced since it was judged");
            return refuse(self, CustomFolderRefusal::Replaced);
        }
        planned
    }

    fn plan_app(&mut self, file: AppFile, rule: &AppCacheRule) -> Vec<PlannedRoot> {
        let Effect::Delete(deletion) = rule.effect() else {
            return Vec::new();
        };
        let origin = Origin::App {
            file,
            id: rule.id().clone(),
        };
        let subdirectory = match deletion.matcher() {
            PathMatcher::WholeDirectory => None,
            PathMatcher::ChildSubdirectory(subdirectory) => Some(subdirectory),
            PathMatcher::Recursive(_) | PathMatcher::DirectFiles(_) => {
                self.issues.push(ScanIssue::Unsupported { origin });
                return Vec::new();
            }
        };
        let attributes = GroupAttributes {
            origins: vec![origin.clone()],
            label: rule.name().clone(),
            descriptions: rule.description().cloned().into_iter().collect(),
            rule_group: rule.group().cloned(),
            cache_reset: deletion.is_cache_reset(),
            selected_by_default: rule.is_selected_by_default(),
            needs_admin: false,
            keep_hard_linked: false,
            locks: BTreeSet::new(),
        };
        let mut roots = Vec::new();
        for path in deletion.paths() {
            let Some(base) = self.open_base(&origin, path) else {
                continue;
            };
            match subdirectory {
                Some(subdirectory) => roots.extend(self.expand(&origin, &base, subdirectory)),
                None => roots.push(Candidate::walked(base)),
            }
        }
        let rule_age = deletion.min_age().map(AgeDays::duration);
        self.check_roots(&origin, roots, rule_age, &attributes)
    }

    fn plan_file(&mut self, target: &SingleFileTarget) -> Option<PlannedRoot> {
        let origin = Origin::SystemFile {
            subcategory: target.subcategory().clone(),
            path: target.path().clone(),
        };
        let resolved = self.resolve(&origin, target.path())?;
        let metadata = match fs::symlink_metadata(&resolved) {
            Ok(metadata) => metadata,
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(&origin, resolved, error.kind()),
        };
        if !metadata.file_type().is_file() {
            return self.unsafe_root(&origin, resolved, &metadata, UnsafeReason::NotAFile);
        }
        let canonical = match fs::canonicalize(&resolved) {
            Ok(canonical) => canonical,
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(&origin, resolved, error.kind()),
        };
        let metadata = match fs::symlink_metadata(&canonical) {
            Ok(again) if again.file_type().is_file() => match self.same_object(&metadata, &again) {
                Some(true) => again,
                Some(false) => return self.replaced(&origin, canonical),
                None => return self.unreadable(&origin, canonical, io::ErrorKind::Unsupported),
            },
            Ok(_) => return self.replaced(&origin, canonical),
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(&origin, canonical, error.kind()),
        };
        let mount_root = match (self.planning.readers.mount_root)(&canonical) {
            Ok(mount_root) => mount_root == Some(true),
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(&origin, canonical, error.kind()),
        };
        let (Some(last_change), Some(identity)) = (
            (self.planning.readers.last_change)(&metadata),
            (self.planning.readers.identity)(&metadata),
        ) else {
            return self.unreadable(&origin, canonical, io::ErrorKind::Unsupported);
        };
        if let Some(variable) = self.protected.refusal(identity) {
            self.issues.push(ScanIssue::ProtectedRoot {
                origins: vec![origin],
                path: canonical,
                variable,
            });
            return None;
        }
        if self.planning.options.exclusions().excludes(&canonical) {
            self.issues.push(ScanIssue::Excluded {
                origins: vec![origin],
                path: canonical,
            });
            return None;
        }
        if by_name(&canonical, false, true).is_some() || self.planning.misc.protects(&canonical) {
            tracing::debug!(path = ?canonical, "file target has a protected name");
            self.stats.protected_names += 1;
            return None;
        }
        // The global age was checked before planning: it is within the clock.
        let cutoff = self.planning.options.cutoff(None)?;
        let attributes = GroupAttributes {
            origins: vec![origin],
            label: target.subcategory().clone(),
            descriptions: target.description().cloned().into_iter().collect(),
            rule_group: None,
            cache_reset: false,
            selected_by_default: target.is_selected_by_default(),
            needs_admin: false,
            keep_hard_linked: false,
            locks: BTreeSet::new(),
        };
        // A bind mount of a file: never offered by this rule, it still joins
        // the rules that share it (invariant 5, strictest age).
        if mount_root {
            tracing::trace!(path = ?canonical, "file target is a mount root");
            self.stats.other_filesystems += 1;
            return Some(PlannedRoot {
                path: canonical,
                identity,
                cutoff,
                attributes,
                kind: PlannedKind::Withheld,
            });
        }
        if last_change > cutoff {
            tracing::trace!(path = ?canonical, "file target recent");
            self.stats.recent += 1;
            return None;
        }
        Some(PlannedRoot {
            path: canonical.clone(),
            identity,
            cutoff,
            attributes,
            kind: PlannedKind::File(Found {
                path: canonical,
                size: metadata.len(),
                last_change,
                kind: EntryKind::File,
                identity,
            }),
        })
    }

    fn resolve(&mut self, origin: &Origin, path: &TemplatePath) -> Option<PathBuf> {
        match path.resolve(self.planning.variables) {
            Ok(resolved) => Some(resolved),
            Err(error) => {
                self.issues.push(ScanIssue::Unresolvable {
                    origin: origin.clone(),
                    error,
                });
                None
            }
        }
    }

    /// The canonical directory a rule path names, if it exists and is a real
    /// directory: the last component is never a link (an ancestor may be). It
    /// may be a mount (`/tmp`), so that is not read here.
    fn open_base(&mut self, origin: &Origin, path: &TemplatePath) -> Option<PathBuf> {
        self.read_base(origin, path).ok().flatten()
    }

    /// [`Self::open_base`], telling an absent directory (`Ok(None)`) from one
    /// refused or unreadable (an error, reported).
    fn read_base(&mut self, origin: &Origin, path: &TemplatePath) -> Result<Option<PathBuf>, ()> {
        let resolved = self.resolve(origin, path).ok_or(())?;
        let unsafe_root = |reason| ScanIssue::UnsafeRoot {
            origins: vec![origin.clone()],
            path: resolved.clone(),
            reason,
        };
        let issue = match real_directory(&resolved, self.planning.readers.identity) {
            Ok((real, _)) => return Ok(Some(real)),
            Err(NotADirectory::Absent) => return Ok(None),
            Err(NotADirectory::Link) => unsafe_root(UnsafeReason::Link),
            Err(NotADirectory::Other) => unsafe_root(UnsafeReason::NotADirectory),
            Err(NotADirectory::Replaced { real }) => ScanIssue::UnsafeRoot {
                origins: vec![origin.clone()],
                path: real,
                reason: UnsafeReason::Replaced,
            },
            Err(NotADirectory::Unreadable { path, error }) => ScanIssue::UnreadableRoot {
                origins: vec![origin.clone()],
                path,
                error,
            },
        };
        self.issues.push(issue);
        Err(())
    }

    /// The directories `<base>/*/<subdirectory>`: walked when reached
    /// without following any link nor leaving the base's filesystem, withheld
    /// otherwise.
    fn expand(
        &mut self,
        origin: &Origin,
        base: &Path,
        subdirectory: &RelativePath,
    ) -> Vec<Candidate> {
        let Ok(device) = self.device(origin, base) else {
            return Vec::new();
        };
        let entries = match fs::read_dir(base) {
            Ok(entries) => entries,
            Err(error) if is_absent(&error) => return Vec::new(),
            Err(error) => {
                self.unreadable::<()>(origin, base.to_path_buf(), error.kind());
                return Vec::new();
            }
        };
        let mut roots = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::debug!(base = ?base, %error, "expanded entry unreadable");
                    self.stats.unreadable += 1;
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    tracing::debug!(path = ?entry.path(), %error, "expanded entry unreadable");
                    self.stats.unreadable += 1;
                    continue;
                }
            };
            // A link is judged by `reach`, as any segment: withheld, it still
            // joins the rules that reach its target directly.
            if (file_type.is_dir() || file_type.is_symlink())
                && let Some(root) =
                    self.reach(origin, entry.path(), subdirectory.segments(), device)
            {
                roots.push(root);
            }
        }
        roots
    }

    /// `<child>/<segments>`: a candidate walked when the child and every
    /// segment is a plain directory on the base's device, withheld from the
    /// first one that is a link, another device or a mount
    /// ([`Self::segment`]; the segments after it are not judged), none when
    /// one before is missing.
    fn reach(
        &mut self,
        origin: &Origin,
        child: PathBuf,
        segments: &[Name],
        device: Option<u64>,
    ) -> Option<Candidate> {
        // The child first, as segment zero: a listing gives only its type.
        let mut path = child;
        let mut withheld = false;
        for segment in std::iter::once(None).chain(segments.iter().map(Some)) {
            if let Some(segment) = segment {
                path.push(segment.as_str());
            }
            if !withheld {
                match self.segment(origin, &path, device) {
                    Segment::Plain => {}
                    Segment::Refused => withheld = true,
                    Segment::Missing | Segment::Unreadable => return None,
                }
            }
        }
        Some(Candidate { path, withheld })
    }

    /// What `path` is to an expansion below a base on `device`: the base
    /// itself is never judged so (it may be a mount, as `/tmp` is).
    fn segment(&mut self, origin: &Origin, path: &Path, device: Option<u64>) -> Segment {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if is_absent(&error) => return Segment::Missing,
            Err(error) => {
                self.unreadable::<()>(origin, path.to_path_buf(), error.kind());
                return Segment::Unreadable;
            }
        };
        if metadata.file_type().is_symlink() {
            tracing::trace!(path = ?path, "expanded segment is a link");
            self.stats.links += 1;
            return Segment::Refused;
        }
        if !metadata.is_dir() {
            return Segment::Missing;
        }
        // Another device is enough: its mount status is then never read.
        let mut other_filesystem =
            (self.planning.readers.identity)(&metadata).map(FileIdentity::device) != device;
        if !other_filesystem {
            other_filesystem = match (self.planning.readers.mount_root)(path) {
                Ok(mount_root) => mount_root == Some(true),
                Err(error) if is_absent(&error) => return Segment::Missing,
                Err(error) => {
                    self.unreadable::<()>(origin, path.to_path_buf(), error.kind());
                    return Segment::Unreadable;
                }
            };
        }
        if other_filesystem {
            tracing::trace!(path = ?path, "expanded segment on another filesystem");
            self.stats.other_filesystems += 1;
            return Segment::Refused;
        }
        Segment::Plain
    }

    /// The identity of a withheld root, read through links: it is never
    /// walked, only matched against the roots of other rules. `None` when it
    /// is gone or not a directory (no other directory root can share it), or
    /// when it cannot be read: then reported, since a rule that walks it
    /// directly would do so without this rule's restrictions.
    fn withheld_identity(&mut self, origin: &Origin, path: &Path) -> Option<FileIdentity> {
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                match (self.planning.readers.identity)(&metadata) {
                    Some(identity) => Some(identity),
                    None => self.unreadable(origin, path.to_path_buf(), io::ErrorKind::Unsupported),
                }
            }
            Ok(_) => None,
            Err(error) if is_absent(&error) => None,
            Err(error) => self.unreadable(origin, path.to_path_buf(), error.kind()),
        }
    }

    /// Keeps the roots a walk may enter: not a variable's directory nor one of
    /// its ancestors (invariant 6), not excluded. Not judged as a mount here:
    /// a rule's own path may be one (`/tmp`); an expanded root was judged by
    /// [`Self::reach`]. A rule age beyond the clock
    /// leaves nothing old enough; it is reported once the rule has a root.
    fn check_roots(
        &mut self,
        origin: &Origin,
        roots: Vec<Candidate>,
        rule_age: Option<Duration>,
        attributes: &GroupAttributes,
    ) -> Vec<PlannedRoot> {
        if roots.is_empty() {
            return Vec::new();
        }
        let Some(cutoff) = self.planning.options.cutoff(rule_age) else {
            self.issues.push(ScanIssue::AgeBeyondClock {
                origin: origin.clone(),
            });
            return Vec::new();
        };
        let mut planned = Vec::new();
        for Candidate {
            path: root,
            withheld,
        } in roots
        {
            if withheld {
                if let Some(identity) = self.withheld_identity(origin, &root) {
                    planned.push(PlannedRoot {
                        path: root,
                        identity,
                        cutoff,
                        attributes: attributes.clone(),
                        kind: PlannedKind::Withheld,
                    });
                }
                continue;
            }
            let metadata = match fs::symlink_metadata(&root) {
                Ok(metadata) => metadata,
                Err(error) if is_absent(&error) => continue,
                Err(error) => {
                    self.unreadable::<()>(origin, root, error.kind());
                    continue;
                }
            };
            if !metadata.is_dir() {
                self.unsafe_root::<()>(origin, root, &metadata, UnsafeReason::NotADirectory);
                continue;
            }
            let Some(identity) = (self.planning.readers.identity)(&metadata) else {
                self.unreadable::<()>(origin, root, io::ErrorKind::Unsupported);
                continue;
            };
            if let Some(variable) = self.protected.refusal(identity) {
                self.issues.push(ScanIssue::ProtectedRoot {
                    origins: vec![origin.clone()],
                    path: root,
                    variable,
                });
                continue;
            }
            let exclusions = self.planning.options.exclusions();
            if exclusions.excludes(&root)
                || exclusions.excludes_by_identity(&root, self.planning.readers.identity)
            {
                self.issues.push(ScanIssue::Excluded {
                    origins: vec![origin.clone()],
                    path: root,
                });
                continue;
            }
            planned.push(PlannedRoot {
                path: root,
                identity,
                cutoff,
                attributes: attributes.clone(),
                kind: PlannedKind::Directory,
            });
        }
        planned
    }

    /// Whether two reads saw the same object; `None` when an identity cannot
    /// be read, which says nothing either way.
    fn same_object(&self, first: &fs::Metadata, second: &fs::Metadata) -> Option<bool> {
        Some((self.planning.readers.identity)(first)? == (self.planning.readers.identity)(second)?)
    }

    fn replaced<T>(&mut self, origin: &Origin, path: PathBuf) -> Option<T> {
        self.issues.push(ScanIssue::UnsafeRoot {
            origins: vec![origin.clone()],
            path,
            reason: UnsafeReason::Replaced,
        });
        None
    }

    fn unreadable<T>(&mut self, origin: &Origin, path: PathBuf, error: io::ErrorKind) -> Option<T> {
        self.issues.push(ScanIssue::UnreadableRoot {
            origins: vec![origin.clone()],
            path,
            error,
        });
        None
    }

    fn unsafe_root<T>(
        &mut self,
        origin: &Origin,
        path: PathBuf,
        metadata: &fs::Metadata,
        expected: UnsafeReason,
    ) -> Option<T> {
        let reason = if metadata.file_type().is_symlink() {
            UnsafeReason::Link
        } else {
            expected
        };
        self.issues.push(ScanIssue::UnsafeRoot {
            origins: vec![origin.clone()],
            path,
            reason,
        });
        None
    }
}
