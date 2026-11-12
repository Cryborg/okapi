use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use okapi_platform::FileIdentity;
use okapi_rules::{
    ChromiumBrowserKind, Label, RelativePath, RuleId, TemplateError, TemplatePath, Variable,
};

use crate::preconditions::Stop;

use crate::walk::{Found, HoldBack};

/// Whether an offered entry is a file or a whole directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    File,
    Directory,
}

/// A walking limit that held entries back below a root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Limit {
    /// Too many directory levels.
    Depth,
    /// Too many entries visited.
    Entries,
}

/// The application rule file a rule comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AppFile {
    Apps,
    Gaming,
    GpuCache,
}

/// The rule a group of entries comes from: a stable key, for remembering what
/// the user selected (never an index, which a rule update would shift).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    /// A directory of `system.json`.
    SystemTarget {
        subcategory: Label,
        path: TemplatePath,
    },
    /// A single file of `system.json`.
    SystemFile {
        subcategory: Label,
        path: TemplatePath,
    },
    /// A rule of `apps.json`, `gaming.json` or `gpu-cache.json`.
    App { file: AppFile, id: RuleId },
    /// A cache directory of a Chromium-based browser, in every profile:
    /// never the profile, which is no stable key.
    Browser {
        browser: ChromiumBrowserKind,
        directory: RelativePath,
    },
}

impl Origin {
    /// The family of rules it belongs to.
    #[must_use]
    pub const fn category(&self) -> Category {
        match self {
            Self::SystemTarget { .. } | Self::SystemFile { .. } => Category::System,
            Self::App { file, .. } => Category::of_file(*file),
            Self::Browser { .. } => Category::Browsers,
        }
    }
}

/// A family of rules, one per rule file: what a user picks to scan or clean,
/// and what the scan's progress is counted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// The targets and single files of `system.json`.
    System,
    /// The caches of `browsers.json`.
    Browsers,
    /// `apps.json`.
    Apps,
    /// `gaming.json`.
    Gaming,
    /// `gpu-cache.json`.
    Gpu,
}

impl Category {
    pub const ALL: [Self; 5] = [
        Self::System,
        Self::Browsers,
        Self::Apps,
        Self::Gaming,
        Self::Gpu,
    ];

    /// The category of the rules of `file`.
    pub(crate) const fn of_file(file: AppFile) -> Self {
        match file {
            AppFile::Apps => Self::Apps,
            AppFile::Gaming => Self::Gaming,
            AppFile::GpuCache => Self::Gpu,
        }
    }

    /// The categories of a group: those of its rules `origins`.
    #[must_use]
    pub fn of_group(origins: &[Origin]) -> BTreeSet<Self> {
        origins.iter().map(Origin::category).collect()
    }

    /// Whether a group of `categories` is among those `chosen` (all when none
    /// is): one of its rules is.
    #[must_use]
    pub fn is_chosen(categories: &BTreeSet<Self>, chosen: &BTreeSet<Self>) -> bool {
        chosen.is_empty() || !chosen.is_disjoint(categories)
    }

    /// Its stable key (`apps`), for flags and qualified rule names.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Browsers => "browsers",
            Self::Apps => "apps",
            Self::Gaming => "gaming",
            Self::Gpu => "gpu",
        }
    }
}

/// Whether a group is checked before the user chooses: selected by default
/// (never a cache reset nor a maintenance, invariant 5) and removable without
/// the administrator. The one rule of every interface.
#[must_use]
pub const fn checked_by_default(selected_by_default: bool, needs_admin: bool) -> bool {
    selected_by_default && !needs_admin
}

/// Why a root that exists was not walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafeReason {
    /// It was replaced between its check and its walk.
    Replaced,
    /// It is a symbolic link.
    Link,
    /// A directory was expected.
    NotADirectory,
    /// A regular file was expected.
    NotAFile,
}

/// Something a scan could not do, or chose not to do.
#[derive(Debug, PartialEq, Eq)]
pub enum ScanIssue {
    /// Nothing was offered: nothing was scanned, or the scan was cancelled
    /// (its statistics and other issues then say how far it went).
    Stopped(Stop),
    /// The rule asks for an age reaching before the clock's origin: nothing
    /// can be old enough.
    AgeBeyondClock { origin: Origin },
    /// The rule uses a matcher the scan does not handle yet.
    Unsupported { origin: Origin },
    /// A path of the rule cannot be resolved.
    Unresolvable {
        origin: Origin,
        error: TemplateError,
    },
    /// A root, or a directory it expands to, exists but cannot be read.
    UnreadableRoot {
        origins: Vec<Origin>,
        path: PathBuf,
        error: io::ErrorKind,
    },
    /// A root exists but is not what the rule expects.
    UnsafeRoot {
        origins: Vec<Origin>,
        path: PathBuf,
        reason: UnsafeReason,
    },
    /// The root would hold a variable's directory (invariant 6).
    ProtectedRoot {
        origins: Vec<Origin>,
        path: PathBuf,
        variable: Variable,
    },
    /// The root is excluded by the user.
    Excluded { origins: Vec<Origin>, path: PathBuf },
    /// A limit held entries back below this root.
    LimitReached {
        origins: Vec<Origin>,
        root: PathBuf,
        limit: Limit,
    },
}

impl ScanIssue {
    /// Whether this issue stopped the whole scan: the report then offers
    /// nothing, which does not mean there is nothing to clean.
    #[must_use]
    pub const fn stops_the_scan(&self) -> bool {
        matches!(self, Self::Stopped(_))
    }
}

/// How many entries were held back, by reason: below the roots, among the
/// directories a rule's paths expand to, and among the single file targets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub(crate) links: u64,
    pub(crate) special_files: u64,
    pub(crate) other_filesystems: u64,
    pub(crate) reserved_directories: u64,
    pub(crate) unreadable: u64,
    pub(crate) replaced: u64,
    pub(crate) recent: u64,
    pub(crate) excluded: u64,
    pub(crate) protected_names: u64,
    pub(crate) not_owned: u64,
}

impl ScanStats {
    /// Symbolic links, never followed.
    #[must_use]
    pub const fn links(&self) -> u64 {
        self.links
    }

    /// Sockets, pipes and devices.
    #[must_use]
    pub const fn special_files(&self) -> u64 {
        self.special_files
    }

    /// Files and directories on another device, or the root of a mount (a
    /// bind mount of the same device included).
    #[must_use]
    pub const fn other_filesystems(&self) -> u64 {
        self.other_filesystems
    }

    /// Entries that are a variable's directory or one of its ancestors (reached
    /// through a bind mount), or another scan root (nested, or a file target).
    #[must_use]
    pub const fn reserved_directories(&self) -> u64 {
        self.reserved_directories
    }

    /// Entries that could not be read.
    #[must_use]
    pub const fn unreadable(&self) -> u64 {
        self.unreadable
    }

    /// Directories that were not, once opened, the directory judged a moment
    /// before (swapped for a link, or replaced).
    #[must_use]
    pub const fn replaced(&self) -> u64 {
        self.replaced
    }

    /// Entries that changed after the cutoff.
    #[must_use]
    pub const fn recent(&self) -> u64 {
        self.recent
    }

    /// Entries the user excluded.
    #[must_use]
    pub const fn excluded(&self) -> u64 {
        self.excluded
    }

    /// Entries whose name the rules protect, or one a running session relies
    /// on (locks, X11 socket directories, private temporary directories).
    #[must_use]
    pub const fn protected_names(&self) -> u64 {
        self.protected_names
    }

    /// Entries of another user in a sticky directory (`/tmp`): only their
    /// owner, the directory's or root may remove them.
    #[must_use]
    pub const fn not_owned(&self) -> u64 {
        self.not_owned
    }

    /// Counts one entry held back for `reason`.
    pub(crate) const fn count(&mut self, reason: HoldBack) {
        let counter = match reason {
            HoldBack::Excluded => &mut self.excluded,
            HoldBack::ProtectedName => &mut self.protected_names,
            HoldBack::Link => &mut self.links,
            HoldBack::Special => &mut self.special_files,
            HoldBack::Unreadable => &mut self.unreadable,
            HoldBack::OtherFilesystem => &mut self.other_filesystems,
            HoldBack::Reserved => &mut self.reserved_directories,
            HoldBack::Recent => &mut self.recent,
            HoldBack::NotOwned => &mut self.not_owned,
        };
        *counter += 1;
    }

    pub(crate) const fn add(&mut self, other: &Self) {
        // Destructured: a new counter cannot be forgotten here.
        let Self {
            links,
            special_files,
            other_filesystems,
            reserved_directories,
            unreadable,
            replaced,
            recent,
            excluded,
            protected_names,
            not_owned,
        } = *other;
        self.links += links;
        self.special_files += special_files;
        self.other_filesystems += other_filesystems;
        self.reserved_directories += reserved_directories;
        self.unreadable += unreadable;
        self.replaced += replaced;
        self.recent += recent;
        self.excluded += excluded;
        self.protected_names += protected_names;
        self.not_owned += not_owned;
    }
}

/// The identifier of an offered entry, valid only in the report that gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId {
    report: u64,
    index: usize,
}

/// An entry a scan offers for cleaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanItem {
    id: ItemId,
    item: FoundItem,
}

impl ScanItem {
    #[must_use]
    pub const fn id(&self) -> ItemId {
        self.id
    }

    /// Its canonical path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.item.found.path
    }

    /// Its size in bytes, a whole directory's included.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.item.found.size
    }

    /// When it last changed, content or inode: what its age is measured on. For
    /// a directory, the most recent change of the directory and everything in it.
    #[must_use]
    pub const fn last_change(&self) -> SystemTime {
        self.item.found.last_change
    }

    #[must_use]
    pub const fn kind(&self) -> EntryKind {
        self.item.found.kind
    }

    /// What the entry was when scanned: cleaning checks it has not been replaced.
    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.item.found.identity
    }

    /// The canonical scan root it lies below (the file itself for a file target).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.item.root
    }

    /// Anything modified after this instant makes the entry too recent: cleaning
    /// checks it again, descendants included (invariant 2).
    #[must_use]
    pub const fn cutoff(&self) -> SystemTime {
        self.item.cutoff
    }
}

/// The entries one rule (or several sharing a root) offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanGroup {
    attributes: GroupAttributes,
    items: Vec<ScanItem>,
}

impl ScanGroup {
    /// Every rule whose root this is.
    #[must_use]
    pub fn origins(&self) -> &[Origin] {
        &self.attributes.origins
    }

    /// The rule name or subcategory shown for the group.
    #[must_use]
    pub const fn label(&self) -> &Label {
        &self.attributes.label
    }

    /// The section that gathers related rules.
    #[must_use]
    pub const fn rule_group(&self) -> Option<&Label> {
        self.attributes.rule_group.as_ref()
    }

    /// The browser whose cache the group is, when its label is a browser
    /// cache's: that of its first rule, which gives the label.
    #[must_use]
    pub fn browser(&self) -> Option<ChromiumBrowserKind> {
        match self.attributes.origins.first() {
            Some(Origin::Browser { browser, .. }) => Some(*browser),
            _ => None,
        }
    }

    /// The locks of the browsers whose caches the group holds.
    #[must_use]
    pub const fn locks(&self) -> &BTreeSet<BrowserLock> {
        &self.attributes.locks
    }

    /// A performance cache: clearing it slows the next launch.
    #[must_use]
    pub const fn is_cache_reset(&self) -> bool {
        self.attributes.cache_reset
    }

    /// Never for a performance cache (invariant 5).
    #[must_use]
    pub const fn is_selected_by_default(&self) -> bool {
        self.attributes.selected_by_default
    }

    /// Whether the group is checked before the user chooses.
    #[must_use]
    pub const fn is_checked_by_default(&self) -> bool {
        checked_by_default(
            self.attributes.selected_by_default,
            self.attributes.needs_admin,
        )
    }

    /// Removing these entries goes through the privileged helper.
    #[must_use]
    pub const fn needs_admin(&self) -> bool {
        self.attributes.needs_admin
    }

    #[must_use]
    pub fn items(&self) -> &[ScanItem] {
        &self.items
    }

    /// The bytes all its entries hold.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.items
            .iter()
            .fold(0, |total, item| total.saturating_add(item.size()))
    }
}

/// What a scan found. It cannot be serialized nor built outside the engine,
/// so it stays in the process that scanned: cleaning takes the report and the
/// identifiers of the entries to remove (invariant 1).
#[derive(Debug)]
pub struct ScanReport {
    id: u64,
    groups: Vec<ScanGroup>,
    /// Where each entry is, by the index of its identifier.
    locations: Vec<(usize, usize)>,
    issues: Vec<ScanIssue>,
    stats: ScanStats,
    /// The identity of every root, walked or withheld: cleaning judges an
    /// entry against them as the scan did.
    roots: BTreeSet<FileIdentity>,
}

impl ScanReport {
    /// Groups in the order of the rule files, then of the rules.
    #[must_use]
    pub fn groups(&self) -> &[ScanGroup] {
        &self.groups
    }

    /// The size of everything offered: the sum over groups, each entry once.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.groups
            .iter()
            .map(ScanGroup::total_size)
            .fold(0, u64::saturating_add)
    }

    #[must_use]
    pub fn issues(&self) -> &[ScanIssue] {
        &self.issues
    }

    #[must_use]
    pub const fn stats(&self) -> &ScanStats {
        &self.stats
    }

    /// The entry with this identifier and its group, if the identifier comes
    /// from this report.
    #[must_use]
    pub fn find(&self, id: ItemId) -> Option<(&ScanGroup, &ScanItem)> {
        if id.report != self.id {
            return None;
        }
        let &(group, item) = self.locations.get(id.index)?;
        let group = self.groups.get(group)?;
        Some((group, group.items.get(item)?))
    }

    /// The locks held of the browsers whose caches these identifiers are in,
    /// one per browser: what to ask the user about before cleaning them.
    #[must_use]
    pub fn open_browsers(&self, ids: &[ItemId]) -> Vec<BrowserLock> {
        let locks: BTreeSet<&BrowserLock> = ids
            .iter()
            .filter_map(|id| self.find(*id))
            .flat_map(|(group, _)| group.locks())
            .collect();
        let mut seen = BTreeSet::new();
        locks
            .into_iter()
            .filter(|lock| lock.is_held() && seen.insert(lock.browser))
            .inspect(|lock| {
                tracing::info!(browser = lock.browser.key(), lock = %lock.lock.display(), "browser open");
            })
            .cloned()
            .collect()
    }

    pub(crate) const fn roots(&self) -> &BTreeSet<FileIdentity> {
        &self.roots
    }
}

impl ScanItem {
    pub(crate) const fn found_item(&self) -> &FoundItem {
        &self.item
    }
}

/// The lock a running browser holds in its user-data directory: while it is
/// there, the browser's caches are not to be emptied.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BrowserLock {
    browser: ChromiumBrowserKind,
    /// The lock itself, a link that may point nowhere.
    lock: PathBuf,
    /// The browser's user-data directory, which holds the lock.
    user_data: PathBuf,
}

impl BrowserLock {
    pub(crate) const fn new(
        browser: ChromiumBrowserKind,
        lock: PathBuf,
        user_data: PathBuf,
    ) -> Self {
        Self {
            browser,
            lock,
            user_data,
        }
    }

    #[must_use]
    pub const fn browser(&self) -> ChromiumBrowserKind {
        self.browser
    }

    #[must_use]
    pub fn lock(&self) -> &Path {
        &self.lock
    }

    #[must_use]
    pub fn user_data(&self) -> &Path {
        &self.user_data
    }

    /// Whether the browser runs: its lock is there and not left by a browser
    /// gone (`okapi_platform::lock_is_live`); a lock that cannot be judged
    /// is held (a browser not known closed is not emptied).
    #[must_use]
    pub fn is_held(&self) -> bool {
        okapi_platform::lock_is_live(&self.lock)
    }
}

/// The attributes of a group, before its entries are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupAttributes {
    pub(crate) origins: Vec<Origin>,
    pub(crate) label: Label,
    pub(crate) rule_group: Option<Label>,
    pub(crate) cache_reset: bool,
    pub(crate) selected_by_default: bool,
    pub(crate) needs_admin: bool,
    /// The locks of the browsers it empties, those of every rule it gathers.
    pub(crate) locks: BTreeSet<BrowserLock>,
}

impl GroupAttributes {
    /// Merges the rules of another group sharing a root: the group is
    /// unselected and a cache reset if either rule is.
    pub(crate) fn merge(&mut self, other: Self) {
        self.origins.extend(other.origins);
        self.cache_reset |= other.cache_reset;
        self.selected_by_default &= other.selected_by_default;
        self.needs_admin |= other.needs_admin;
        self.locks.extend(other.locks);
    }
}

/// An entry found, with the root it was found under and the cutoff it was
/// judged against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoundItem {
    pub(crate) found: Found,
    pub(crate) root: PathBuf,
    pub(crate) root_identity: FileIdentity,
    pub(crate) cutoff: SystemTime,
}

/// Builds a report, numbering its entries.
pub(crate) struct ReportBuilder {
    report: ScanReport,
}

impl ReportBuilder {
    pub(crate) fn new() -> Self {
        Self {
            report: ScanReport {
                id: report_nonce(),
                groups: Vec::new(),
                locations: Vec::new(),
                issues: Vec::new(),
                stats: ScanStats::default(),
                roots: BTreeSet::new(),
            },
        }
    }

    /// The scan was cancelled after `roots_done` roots of `roots_total`, and
    /// `entries_seen` entries: no group is offered, the statistics and issues gathered so
    /// far stay.
    pub(crate) fn cancel(&mut self, roots_done: usize, roots_total: usize, entries_seen: u64) {
        tracing::info!(
            roots_done,
            roots_total,
            entries_seen,
            stats = ?self.report.stats,
            issues = self.report.issues.len(),
            "scan cancelled"
        );
        self.report.issues.push(ScanIssue::Stopped(Stop::Cancelled));
    }

    /// How many issues so far.
    pub(crate) const fn issue_count(&self) -> usize {
        self.report.issues.len()
    }

    /// Records `issue`; a cancellation goes through [`Self::cancel`], which
    /// traces it as such rather than as a failure.
    pub(crate) fn issue(&mut self, issue: ScanIssue) {
        if issue.stops_the_scan() {
            tracing::error!(?issue, "scan stopped");
        } else {
            tracing::warn!(?issue, "scan issue");
        }
        self.report.issues.push(issue);
    }

    pub(crate) const fn add_stats(&mut self, stats: &ScanStats) {
        self.report.stats.add(stats);
    }

    /// Adds a group, unless it offers nothing.
    pub(crate) fn group(&mut self, attributes: GroupAttributes, found: Vec<FoundItem>) {
        if found.is_empty() {
            return;
        }
        let group = self.report.groups.len();
        let items = found
            .into_iter()
            .enumerate()
            .map(|(position, item)| {
                let id = ItemId {
                    report: self.report.id,
                    index: self.report.locations.len(),
                };
                self.report.locations.push((group, position));
                ScanItem { id, item }
            })
            .collect();
        self.report.groups.push(ScanGroup { attributes, items });
    }

    pub(crate) fn roots(&mut self, roots: BTreeSet<FileIdentity>) {
        self.report.roots = roots;
    }

    pub(crate) fn finish(self) -> ScanReport {
        self.report
    }
}

/// A number that differs from one report to the next with overwhelming
/// probability, so an identifier from another report (or another run) is not
/// taken for one of this report.
fn report_nonce() -> u64 {
    use std::hash::{BuildHasher, RandomState};

    RandomState::new().hash_one(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use okapi_rules::{Label, RuleId, TemplatePath};

    use super::*;

    #[test]
    fn each_rule_file_has_its_category() {
        let app = |file, id: &str| Origin::App {
            file,
            id: RuleId::try_from(id.to_owned()).unwrap(),
        };
        assert_eq!(app(AppFile::Apps, "pip").category(), Category::Apps);
        assert_eq!(app(AppFile::Gaming, "steam").category(), Category::Gaming);
        assert_eq!(app(AppFile::GpuCache, "mesa").category(), Category::Gpu);
        let label = Label::try_from("Temp".to_owned()).unwrap();
        let path = TemplatePath::try_from("/tmp".to_owned()).unwrap();
        let target = Origin::SystemTarget {
            subcategory: label.clone(),
            path: path.clone(),
        };
        assert_eq!(target.category(), Category::System);
        assert_eq!(
            Origin::SystemFile {
                subcategory: label,
                path
            }
            .category(),
            Category::System
        );
        let names: Vec<_> = Category::ALL
            .iter()
            .map(|category| category.name())
            .collect();
        assert_eq!(names, ["system", "browsers", "apps", "gaming", "gpu"]);
    }

    #[test]
    fn a_group_is_chosen_when_one_of_its_rules_is() {
        let app = |file, id: &str| Origin::App {
            file,
            id: RuleId::try_from(id.to_owned()).unwrap(),
        };
        let shared =
            Category::of_group(&[app(AppFile::Apps, "pip"), app(AppFile::GpuCache, "mesa")]);
        assert_eq!(shared, BTreeSet::from([Category::Apps, Category::Gpu]));
        let gpu = Category::of_group(&[app(AppFile::GpuCache, "mesa")]);
        let apps = BTreeSet::from([Category::Apps]);
        assert!(Category::is_chosen(&shared, &apps));
        assert!(!Category::is_chosen(&gpu, &apps));
        assert!(
            Category::is_chosen(&gpu, &BTreeSet::new()),
            "none chosen: all"
        );
    }

    #[test]
    fn the_default_check_needs_a_preselection_and_no_administrator() {
        assert!(checked_by_default(true, false));
        assert!(!checked_by_default(false, false), "invariant 5");
        assert!(!checked_by_default(true, true));
        assert!(!checked_by_default(false, true));
    }

    /// The names scripts and the history read: public, never renamed.
    #[test]
    fn every_stop_reason_and_result_keeps_its_name() {
        use std::io::ErrorKind;

        use okapi_rules::Variable;

        use crate::{CleanResult, KeepReason, Stop};

        let stops = [
            (Stop::UnsupportedSystem, "unsupportedSystem"),
            (
                Stop::MissingVariable {
                    variable: Variable::Config,
                },
                "missingVariable",
            ),
            (
                Stop::UnreadableVariable {
                    variable: Variable::Config,
                    path: "/home/ana/.config".into(),
                    error: ErrorKind::PermissionDenied,
                },
                "unreadableVariable",
            ),
            (Stop::MinAgeBeyondClock, "minAgeBeyondClock"),
            (Stop::Cancelled, "cancelled"),
        ];
        for (stop, name) in stops {
            assert_eq!(stop.key(), name);
        }
        let failed = KeepReason::Failed {
            kind: ErrorKind::PermissionDenied,
            code: Some(13),
        };
        let reasons = [
            (KeepReason::UnknownItem, "unknown_item"),
            (KeepReason::Replaced, "replaced"),
            (KeepReason::Recent, "recent"),
            (KeepReason::Changed, "changed"),
            (KeepReason::Excluded, "excluded"),
            (KeepReason::Protected, "protected"),
            (failed, "failed"),
            (KeepReason::Cancelled, "cancelled"),
        ];
        for (reason, name) in reasons {
            assert_eq!(reason.key(), name);
        }
        let results = [
            (CleanResult::Removed { bytes: 1 }, "removed"),
            (
                CleanResult::Partial {
                    bytes: 1,
                    reason: failed,
                },
                "partial",
            ),
            (CleanResult::Gone, "gone"),
            (CleanResult::Kept(failed), "kept"),
        ];
        for (result, name) in results {
            assert_eq!(result.key(), name);
        }
    }
}
