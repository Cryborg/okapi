use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use okapi_platform::FileIdentity;
use okapi_rules::{Label, RuleId, TemplateError, TemplatePath, Variable};

use crate::walk::Found;

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
    /// This system gives no file identity, which mount points and bind mounts
    /// are detected with: nothing is scanned.
    UnsupportedSystem,
    /// A variable the rules use has no value: what it would protect cannot be
    /// vouched for (invariant 6), so nothing is scanned.
    MissingVariable { variable: Variable },
    /// A variable's directory, or one of its ancestors, cannot be read: what it
    /// protects cannot be vouched for (invariant 6), so nothing is scanned.
    UnreadableVariable {
        variable: Variable,
        path: PathBuf,
        error: io::ErrorKind,
    },
    /// The minimum age of the scan reaches before the clock's origin: nothing
    /// can be old enough, so nothing is scanned.
    MinAgeBeyondClock,
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
        matches!(
            self,
            Self::UnsupportedSystem
                | Self::MissingVariable { .. }
                | Self::UnreadableVariable { .. }
                | Self::MinAgeBeyondClock
        )
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

    /// Files and directories on another device (mount points). A bind mount
    /// of a directory of the same device is not told apart this way.
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

    /// Entries whose name the rules protect.
    #[must_use]
    pub const fn protected_names(&self) -> u64 {
        self.protected_names
    }

    pub(crate) const fn add(&mut self, other: &Self) {
        self.links += other.links;
        self.special_files += other.special_files;
        self.other_filesystems += other.other_filesystems;
        self.reserved_directories += other.reserved_directories;
        self.unreadable += other.unreadable;
        self.replaced += other.replaced;
        self.recent += other.recent;
        self.excluded += other.excluded;
        self.protected_names += other.protected_names;
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
}

impl ScanReport {
    /// Groups in the order of the rule files, then of the rules.
    #[must_use]
    pub fn groups(&self) -> &[ScanGroup] {
        &self.groups
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
}

impl GroupAttributes {
    /// Merges the rules of another group sharing a root: the group is
    /// unselected and a cache reset if either rule is.
    pub(crate) fn merge(&mut self, other: Self) {
        self.origins.extend(other.origins);
        self.cache_reset |= other.cache_reset;
        self.selected_by_default &= other.selected_by_default;
        self.needs_admin |= other.needs_admin;
    }
}

/// An entry found, with the root it was found under and the cutoff it was
/// judged against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoundItem {
    pub(crate) found: Found,
    pub(crate) root: PathBuf,
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
            },
        }
    }

    pub(crate) fn issue(&mut self, issue: ScanIssue) {
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
