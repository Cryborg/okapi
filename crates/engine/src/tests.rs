//! End-to-end tests of the scan, on real temporary directories.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use okapi_platform::{FileIdentity, file_identity};
use okapi_rules::{
    AppCacheRule, CleanTarget, MiscRules, SingleFileTarget, TemplateVariables, Variable,
};

use crate::options::{Exclusions, ScanOptions};
use crate::preconditions::Stop;
use crate::report::{AppFile, EntryKind, Limit, ScanGroup, ScanIssue, ScanReport};
use crate::roots::RuleEntry;
use crate::scan::scan_entries;
use crate::walk::{Limits, Readers};

// Cleaning works through directory descriptors, on Linux only.
#[cfg(target_os = "linux")]
mod clean;

const HOUR: Duration = Duration::from_hours(1);

/// A temporary directory, by its canonical path.
struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    now: SystemTime,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        Self {
            _directory: directory,
            root,
            now: SystemTime::now(),
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn directory(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// A file of `size` bytes, last modified `age` before now.
    fn file(&self, relative: &str, size: usize, age: Duration) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; size]).unwrap();
        self.age(&path, age);
        path
    }

    fn age(&self, path: &Path, age: Duration) {
        File::open(path)
            .unwrap()
            .set_modified(self.now - age)
            .unwrap();
    }

    fn options(&self, exclusions: Exclusions) -> ScanOptions {
        ScanOptions::new(self.now, HOUR, exclusions)
    }

    fn variables(&self, values: &[(Variable, &str)]) -> TemplateVariables {
        let mut variables = TemplateVariables::new();
        for (variable, relative) in values {
            variables.insert(*variable, self.path(relative)).unwrap();
        }
        variables
    }
}

fn modification_time(metadata: &fs::Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
}

fn target(json: &str) -> CleanTarget {
    serde_json::from_str(json).unwrap()
}

fn target_at(path: &Path) -> CleanTarget {
    target(&format!(
        r#"{{"path": "{}", "subcategory": "Temp"}}"#,
        path.display()
    ))
}

fn app(json: &str) -> AppCacheRule {
    serde_json::from_str(json).unwrap()
}

fn no_protected_names() -> MiscRules {
    serde_json::from_str(r#"{"type": "misc", "protectedEventLogs": [], "trashPath": null}"#)
        .unwrap()
}

struct Run<'a> {
    entries: Vec<RuleEntry<'a>>,
    misc: MiscRules,
    variables: TemplateVariables,
    options: ScanOptions,
    readers: Readers,
    limits: Limits,
    /// Who will remove what is offered.
    user: Option<u32>,
}

/// The user the tests scan as.
const TEST_USER: u32 = 1_000;
/// Another user, owner of the entries marked as foreign.
const FOREIGN_USER: u32 = 1_001;
/// A directory marked so is sticky and owned by root, as `/tmp` is.
const STICKY_MARK: Duration = Duration::from_hours(300_000);
/// A directory marked so is sticky and owned by the test user.
const OWN_STICKY_MARK: Duration = Duration::from_hours(300_001);
/// An entry marked so belongs to another user.
const FOREIGN_MARK: Duration = Duration::from_hours(300_002);
/// An entry marked so has an owner that cannot be read.
const UNKNOWN_OWNER_MARK: Duration = Duration::from_hours(300_003);

/// Ownership as the marks say: by default the test user's, not sticky.
fn marked_ownership(metadata: &fs::Metadata) -> Option<okapi_platform::Ownership> {
    let mark = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok());
    let (owner, sticky) = match mark {
        Some(STICKY_MARK) => (0, true),
        Some(OWN_STICKY_MARK) => (TEST_USER, true),
        Some(FOREIGN_MARK) => (FOREIGN_USER, false),
        Some(UNKNOWN_OWNER_MARK) => return None,
        _ => (TEST_USER, false),
    };
    Some(okapi_platform::Ownership { owner, sticky })
}

impl<'a> Run<'a> {
    fn new(fixture: &Fixture, entries: Vec<RuleEntry<'a>>) -> Self {
        Self {
            entries,
            misc: no_protected_names(),
            variables: TemplateVariables::new(),
            options: fixture.options(Exclusions::none()),
            // A test cannot set a status change time back: ages are read on
            // the modification time, which it can.
            readers: Readers {
                identity: file_identity,
                last_change: modification_time,
                ownership: marked_ownership,
                mount_root: okapi_platform::mount_root,
            },
            limits: Limits::DEFAULT,
            user: Some(TEST_USER),
        }
    }

    fn scan(&self) -> ScanReport {
        scan_entries(
            &self.entries,
            &self.misc,
            &self.variables,
            &self.options,
            self.readers,
            self.limits,
            self.user,
        )
    }
}

fn offered(report: &ScanReport) -> Vec<(PathBuf, EntryKind)> {
    let mut paths: Vec<_> = report
        .groups()
        .iter()
        .flat_map(ScanGroup::items)
        .map(|item| (item.path().to_path_buf(), item.kind()))
        .collect();
    paths.sort();
    paths
}

/// The subcategory of the single system rule an issue comes from.
fn origin_label(origins: &[crate::report::Origin]) -> String {
    match origins {
        [
            crate::report::Origin::SystemTarget { subcategory, .. }
            | crate::report::Origin::SystemFile { subcategory, .. },
        ] => subcategory.to_string(),
        other => panic!("expected one system origin, got {other:?}"),
    }
}

fn offered_paths(report: &ScanReport) -> Vec<PathBuf> {
    offered(report).into_iter().map(|(path, _)| path).collect()
}

#[test]
fn a_settled_directory_is_one_entry_and_a_busy_one_is_opened() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/settled/a", 10, 2 * HOUR);
    fixture.file("data/settled/b", 20, 2 * HOUR);
    fixture.file("data/busy/old-1", 1, 2 * HOUR);
    fixture.file("data/busy/old-2", 2, 2 * HOUR);
    fixture.file("data/busy/new", 4, Duration::ZERO);
    fixture.age(&fixture.path("data/settled"), 2 * HOUR);
    fixture.age(&fixture.path("data/busy"), 2 * HOUR);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(
        offered(&report),
        [
            (fixture.path("data/busy/old-1"), EntryKind::File),
            (fixture.path("data/busy/old-2"), EntryKind::File),
            (fixture.path("data/settled"), EntryKind::Directory),
        ]
    );
    let settled = report
        .groups()
        .iter()
        .flat_map(ScanGroup::items)
        .find(|item| item.kind() == EntryKind::Directory)
        .unwrap();
    assert_eq!(settled.size(), 30);
    assert_eq!(report.stats().recent(), 1);
}

#[test]
fn the_root_itself_is_never_an_entry() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/only/a", 1, 2 * HOUR);
    fixture.age(&fixture.path("data/only"), 2 * HOUR);
    fixture.age(&root, 2 * HOUR);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/only")]);
}

#[test]
fn a_file_at_the_cutoff_is_offered_and_one_after_is_not() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/at", 1, HOUR);
    fixture.file(
        "data/after",
        1,
        HOUR.checked_sub(Duration::from_secs(1)).unwrap(),
    );
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/at")]);
}

#[test]
fn an_empty_directory_is_judged_by_its_own_date() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.directory("data/new-lock");
    let old = fixture.directory("data/old-empty");
    fixture.age(&old, 2 * HOUR);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [old]);
    assert_eq!(report.stats().recent(), 1);
}

#[test]
fn a_rule_age_stricter_than_the_global_one_applies_and_a_looser_one_does_not() {
    let fixture = Fixture::new();
    let root = fixture.directory("cache");
    fixture.file("cache/three-days", 1, 72 * HOUR);
    fixture.file("cache/one-day", 1, 24 * HOUR);
    fixture.file("cache/half-hour", 1, HOUR / 2);
    let strict = app(&format!(
        r#"{{"id": "strict", "name": "Strict", "paths": ["{}"], "minAgeDays": 2}}"#,
        root.display()
    ));
    let report = Run::new(
        &fixture,
        vec![RuleEntry::App {
            file: AppFile::Apps,
            rule: &strict,
        }],
    )
    .scan();
    assert_eq!(offered_paths(&report), [fixture.path("cache/three-days")]);

    let mut loose_run = Run::new(&fixture, Vec::new());
    loose_run.options = ScanOptions::new(fixture.now, 48 * HOUR, Exclusions::none());
    let loose = app(&format!(
        r#"{{"id": "loose", "name": "Loose", "paths": ["{}"], "minAgeDays": 1}}"#,
        root.display()
    ));
    loose_run.entries = vec![RuleEntry::App {
        file: AppFile::Apps,
        rule: &loose,
    }];
    assert_eq!(
        offered_paths(&loose_run.scan()),
        [fixture.path("cache/three-days")]
    );
}

#[cfg(unix)]
#[test]
fn links_sockets_and_their_parents_are_held_back() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    let fixture = Fixture::new();
    let root = fixture.directory("data");
    let outside = fixture.directory("outside");
    fixture.file("outside/precious", 1, 2 * HOUR);
    fixture.file("data/with-link/old", 1, 2 * HOUR);
    symlink(&outside, fixture.path("data/with-link/to-outside")).unwrap();
    fixture.file("data/with-file-link/old", 1, 2 * HOUR);
    symlink(
        fixture.path("outside/precious"),
        fixture.path("data/with-file-link/to-file"),
    )
    .unwrap();
    symlink(fixture.path("missing"), fixture.path("data/broken")).unwrap();
    fixture.file("data/session/old", 1, 2 * HOUR);
    let _listener = UnixListener::bind(fixture.path("data/session/socket")).unwrap();
    for parent in ["data/with-link", "data/with-file-link", "data/session"] {
        fixture.age(&fixture.path(parent), 2 * HOUR);
    }
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(
        offered_paths(&report),
        [
            fixture.path("data/session/old"),
            fixture.path("data/with-file-link/old"),
            fixture.path("data/with-link/old"),
        ]
    );
    assert_eq!(report.stats().links(), 3);
    assert_eq!(report.stats().special_files(), 1);
}

#[cfg(unix)]
#[test]
fn a_root_or_a_base_that_is_a_link_is_refused() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture.file("real/old", 1, 2 * HOUR);
    symlink(fixture.path("real"), fixture.path("alias")).unwrap();
    let rule = target_at(&fixture.path("alias"));
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert!(matches!(
        report.issues(),
        [ScanIssue::UnsafeRoot {
            reason: crate::report::UnsafeReason::Link,
            ..
        }]
    ));
}

#[cfg(unix)]
#[test]
fn child_subdirectories_are_reached_without_following_links() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let base = fixture.directory("apps");
    fixture.file("apps/one/cache/old", 1, 2 * HOUR);
    fixture.directory("apps/two/data");
    fixture.file("elsewhere/cache/old", 1, 2 * HOUR);
    symlink(fixture.path("elsewhere"), fixture.path("apps/linked-child")).unwrap();
    fixture.file("hidden/old", 1, 2 * HOUR);
    fixture.directory("apps/three");
    symlink(fixture.path("hidden"), fixture.path("apps/three/cache")).unwrap();
    let rule = target(&format!(
        r#"{{"path": "{}", "subcategory": "Flatpak", "childSubdir": "cache"}}"#,
        base.display()
    ));
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [fixture.path("apps/one/cache/old")]);
    assert_eq!(report.stats().links(), 2);
}

#[test]
fn exclusions_hold_entries_and_roots_back() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/keep/old", 1, 2 * HOUR);
    fixture.file("data/mixed/old", 1, 2 * HOUR);
    fixture.file("data/mixed/build.LOG", 1, 2 * HOUR);
    fixture.age(&fixture.path("data/mixed"), 2 * HOUR);
    fixture.age(&fixture.path("data/keep"), 2 * HOUR);
    let excluded_root = fixture.directory("excluded");
    fixture.file("excluded/old", 1, 2 * HOUR);
    let exclusions = Exclusions::new([
        fixture.path("data/keep"),
        PathBuf::from("*.log"),
        excluded_root.clone(),
    ])
    .unwrap();
    let rule = target_at(&root);
    let other = target_at(&excluded_root);
    let mut run = Run::new(
        &fixture,
        vec![RuleEntry::Target(&rule), RuleEntry::Target(&other)],
    );
    run.options = fixture.options(exclusions);
    let report = run.scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/mixed/old")]);
    assert_eq!(report.stats().excluded(), 2);
    assert!(
        report.issues().iter().any(
            |issue| matches!(issue, ScanIssue::Excluded { path, .. } if *path == excluded_root)
        )
    );
}

#[test]
fn a_protected_name_keeps_its_directory_open() {
    let fixture = Fixture::new();
    let root = fixture.directory("logs");
    fixture.file("logs/archive/Security.evtx", 1, 2 * HOUR);
    fixture.file("logs/archive/old.txt", 1, 2 * HOUR);
    fixture.age(&fixture.path("logs/archive"), 2 * HOUR);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.misc = serde_json::from_str(
        r#"{"type": "misc", "protectedEventLogs": ["security.evtx"], "trashPath": null}"#,
    )
    .unwrap();
    let report = run.scan();
    assert_eq!(
        offered_paths(&report),
        [fixture.path("logs/archive/old.txt")]
    );
    assert_eq!(report.stats().protected_names(), 1);
}

#[test]
fn a_root_that_holds_a_variable_is_refused_and_the_temporary_directory_is_not() {
    let fixture = Fixture::new();
    let shared = fixture.directory("shared");
    fixture.directory("shared/guest");
    fixture.file("shared/old", 1, 2 * HOUR);
    let temp = fixture.directory("temp");
    fixture.file("temp/old", 1, 2 * HOUR);
    let under_home = fixture.directory("shared/guest/.cache/app");
    fixture.file("shared/guest/.cache/app/old", 1, 2 * HOUR);
    let rules = [target_at(&shared), target_at(&temp), target_at(&under_home)];
    let mut run = Run::new(&fixture, rules.iter().map(RuleEntry::Target).collect());
    run.variables = fixture.variables(&[
        (Variable::Home, "shared/guest"),
        (Variable::TempDirectory, "temp"),
    ]);
    let report = run.scan();
    assert_eq!(
        offered_paths(&report),
        [
            fixture.path("shared/guest/.cache/app/old"),
            fixture.path("temp/old")
        ]
    );
    assert!(matches!(
        report.issues(),
        [ScanIssue::ProtectedRoot { path, variable: Variable::Home, .. }] if *path == shared
    ));
}

/// The modification date that gives directories one shared identity, for
/// [`same_identity_when_aliased`].
const ALIAS_MARK: Duration = Duration::from_hours(250_000);

/// The real identity, except for directories bearing [`ALIAS_MARK`], which all
/// seem to be one directory (as two spellings on a case-insensitive filesystem,
/// or a bind mount, would be).
fn same_identity_when_aliased(metadata: &fs::Metadata) -> Option<FileIdentity> {
    let identity = file_identity(metadata)?;
    if metadata.modified().ok() == Some(SystemTime::UNIX_EPOCH + ALIAS_MARK) {
        Some(FileIdentity::new(identity.device(), u64::MAX))
    } else {
        Some(identity)
    }
}

fn mark(path: &Path, mark: Duration) {
    File::open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + mark)
        .unwrap();
}

#[test]
fn a_root_with_the_identity_of_a_variable_ancestor_is_refused() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/old", 1, 2 * HOUR);
    fixture.directory("elsewhere/guest");
    mark(&root, ALIAS_MARK);
    mark(&fixture.path("elsewhere"), ALIAS_MARK);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.identity = same_identity_when_aliased;
    run.variables = fixture.variables(&[(Variable::Home, "elsewhere/guest")]);
    let report = run.scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert!(matches!(
        report.issues(),
        [ScanIssue::ProtectedRoot { variable: Variable::Home, path, .. }] if *path == root
    ));
}

#[test]
fn a_variable_directory_met_below_a_root_is_never_entered() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/bound/old", 1, 2 * HOUR);
    fixture.file("data/free/old", 1, 2 * HOUR);
    fixture.age(&fixture.path("data/free"), 2 * HOUR);
    let home = fixture.directory("home/guest");
    mark(&home, ALIAS_MARK);
    mark(&fixture.path("data/bound"), ALIAS_MARK);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.identity = same_identity_when_aliased;
    run.variables = fixture.variables(&[(Variable::Home, "home/guest")]);
    let report = run.scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/free")]);
    assert_eq!(report.stats().reserved_directories(), 1);
}

/// The modification date that marks a directory as mounted from elsewhere,
/// for [`other_device_when_marked`].
const MOUNTED_MARK: Duration = Duration::from_secs(1_000_000_000);

/// The real identity, except for directories bearing [`MOUNTED_MARK`], which
/// seem to live on another device.
fn other_device_when_marked(metadata: &fs::Metadata) -> Option<FileIdentity> {
    let identity = file_identity(metadata)?;
    let marked = metadata.modified().ok() == Some(SystemTime::UNIX_EPOCH + MOUNTED_MARK);
    Some(FileIdentity::new(
        identity.device() + u64::from(marked),
        identity.inode(),
    ))
}

#[test]
fn nothing_on_another_filesystem_is_entered() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/top", 1, 2 * HOUR);
    fixture.file("data/mounted/deep", 1, 2 * HOUR);
    mark(&fixture.path("data/mounted"), MOUNTED_MARK);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.identity = other_device_when_marked;
    let report = run.scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/top")]);
    assert_eq!(report.stats().other_filesystems(), 1);
}

#[test]
fn another_scan_root_below_a_root_keeps_its_own_entries() {
    let fixture = Fixture::new();
    let outer = fixture.directory("logs");
    let inner = fixture.directory("logs/app");
    fixture.file("logs/app/old", 1, 2 * HOUR);
    fixture.file("logs/other/old", 1, 2 * HOUR);
    fixture.age(&fixture.path("logs/other"), 2 * HOUR);
    let outer_rule = target_at(&outer);
    let inner_rule = app(&format!(
        r#"{{"id": "app", "name": "App", "paths": ["{}"]}}"#,
        inner.display()
    ));
    let report = Run::new(
        &fixture,
        vec![
            RuleEntry::Target(&outer_rule),
            RuleEntry::App {
                file: AppFile::Apps,
                rule: &inner_rule,
            },
        ],
    )
    .scan();
    let [outer_group, inner_group] = report.groups() else {
        panic!("expected two groups: {:?}", report.groups());
    };
    assert_eq!(outer_group.items()[0].path(), fixture.path("logs/other"));
    assert_eq!(outer_group.items().len(), 1);
    assert_eq!(inner_group.items()[0].path(), fixture.path("logs/app/old"));
    assert_eq!(report.stats().reserved_directories(), 1);
}

#[test]
fn a_shared_root_is_walked_once_and_never_preselected_if_one_rule_resets_a_cache() {
    let fixture = Fixture::new();
    let root = fixture.directory("unity3d");
    fixture.file("unity3d/old", 1, 3 * 24 * HOUR);
    fixture.file("unity3d/recent-for-the-strict-rule", 1, 24 * HOUR);
    let editor = app(&format!(
        r#"{{"id": "unity", "name": "Unity Editor", "paths": ["{}"]}}"#,
        root.display()
    ));
    let shaders = app(&format!(
        r#"{{"id": "unity-cache", "name": "Unity Shader Cache", "paths": ["{}"], "cacheReset": true, "minAgeDays": 2}}"#,
        root.display()
    ));
    for order in [[&editor, &shaders], [&shaders, &editor]] {
        let entries = order
            .iter()
            .map(|rule| RuleEntry::App {
                file: AppFile::Apps,
                rule,
            })
            .collect();
        let report = Run::new(&fixture, entries).scan();
        let [group] = report.groups() else {
            panic!("expected one group");
        };
        assert_eq!(group.origins().len(), 2);
        assert!(group.is_cache_reset());
        assert!(!group.is_selected_by_default());
        assert_eq!(
            offered_paths(&report),
            [fixture.path("unity3d/old")],
            "the strictest age applies"
        );
        assert_eq!(group.label().as_str(), order[0].name().as_str());
    }
}

#[test]
fn a_rule_with_several_roots_is_one_group() {
    let fixture = Fixture::new();
    let base = fixture.directory("snap");
    fixture.file("snap/one/cache/old", 1, 2 * HOUR);
    fixture.file("snap/two/cache/old", 1, 2 * HOUR);
    let rule = target(&format!(
        r#"{{"path": "{}", "subcategory": "Snap", "childSubdir": "cache"}}"#,
        base.display()
    ));
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    let [group] = report.groups() else {
        panic!("expected one group: {:?}", report.groups());
    };
    assert_eq!(group.items().len(), 2);
    assert_eq!(group.origins().len(), 1);
}

#[test]
fn rules_linked_by_one_shared_root_form_one_group() {
    let fixture = Fixture::new();
    let own = fixture.directory("own");
    let shared = fixture.directory("shared");
    fixture.file("own/old", 1, 2 * HOUR);
    fixture.file("shared/old", 1, 2 * HOUR);
    let wide = app(&format!(
        r#"{{"id": "wide", "name": "Wide", "paths": ["{}", "{}"]}}"#,
        own.display(),
        shared.display()
    ));
    let reset = app(&format!(
        r#"{{"id": "reset", "name": "Reset", "paths": ["{}"], "cacheReset": true}}"#,
        shared.display()
    ));
    let report = Run::new(
        &fixture,
        vec![
            RuleEntry::App {
                file: AppFile::Apps,
                rule: &wide,
            },
            RuleEntry::App {
                file: AppFile::Apps,
                rule: &reset,
            },
        ],
    )
    .scan();
    let [group] = report.groups() else {
        panic!("expected one group: {:?}", report.groups());
    };
    assert_eq!(group.items().len(), 2);
    assert!(
        !group.is_selected_by_default(),
        "the shared root is a cache reset for one rule"
    );
    assert_eq!(group.label().as_str(), "Wide");
}

#[test]
fn a_joined_group_takes_the_place_of_its_first_rule() {
    let fixture = Fixture::new();
    let shared = fixture.directory("shared");
    let middle = fixture.directory("middle");
    fixture.file("shared/old", 1, 2 * HOUR);
    fixture.file("middle/old", 1, 2 * HOUR);
    let rules = [target_at(&shared), target_at(&middle), target_at(&shared)];
    let report = Run::new(&fixture, rules.iter().map(RuleEntry::Target).collect()).scan();
    let roots: Vec<&Path> = report
        .groups()
        .iter()
        .map(|group| group.items()[0].root())
        .collect();
    assert_eq!(roots, [shared.as_path(), middle.as_path()]);
    assert_eq!(report.groups()[0].origins().len(), 2);
}

#[test]
fn system_and_user_temporary_files_share_one_group() {
    let fixture = Fixture::new();
    let temp = fixture.directory("tmp");
    fixture.file("tmp/old", 1, 2 * HOUR);
    let system = target_at(&temp);
    let user = target(r#"{"path": "${TMPDIR}", "subcategory": "User Temp Files"}"#);
    let mut run = Run::new(
        &fixture,
        vec![RuleEntry::Target(&system), RuleEntry::Target(&user)],
    );
    run.variables = fixture.variables(&[(Variable::TempDirectory, "tmp")]);
    let report = run.scan();
    assert_eq!(report.groups().len(), 1);
    assert_eq!(report.groups()[0].origins().len(), 2);
    assert_eq!(offered_paths(&report), [fixture.path("tmp/old")]);
}

#[test]
fn limits_hold_entries_back_and_are_reported_once() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/a/b/c/deep", 1, 2 * HOUR);
    fixture.file("data/a/shallow", 1, 2 * HOUR);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.limits = Limits {
        depth: 2,
        entries: 1_000,
    };
    let report = run.scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/a/shallow")]);
    assert!(matches!(
        report.issues(),
        [ScanIssue::LimitReached {
            limit: Limit::Depth,
            ..
        }]
    ));

    let wide = fixture.directory("wide");
    for name in ["one", "two", "three"] {
        fixture.file(&format!("wide/folder/{name}"), 1, 2 * HOUR);
    }
    fixture.age(&fixture.path("wide/folder"), 2 * HOUR);
    let wide_rule = target_at(&wide);
    run.entries = vec![RuleEntry::Target(&wide_rule)];
    // `folder` and two of its files fit; the third does not.
    run.limits = Limits {
        depth: 8,
        entries: 3,
    };
    let report = run.scan();
    assert_eq!(offered_paths(&report).len(), 2, "the folder stays open");
    assert_eq!(
        report
            .issues()
            .iter()
            .filter(|issue| matches!(
                issue,
                ScanIssue::LimitReached {
                    limit: Limit::Entries,
                    ..
                }
            ))
            .count(),
        1
    );
}

#[test]
fn maintenance_admin_and_unsupported_rules() {
    let fixture = Fixture::new();
    let journal = fixture.directory("journal");
    fixture.file("journal/old", 1, 2 * HOUR);
    let maintenance = target(&format!(
        r#"{{"path": "{}", "subcategory": "Journal", "cleanupAction": "journal-vacuum"}}"#,
        journal.display()
    ));
    let admin_root = fixture.directory("apt");
    fixture.file("apt/old.deb", 1, 2 * HOUR);
    let admin = target(&format!(
        r#"{{"path": "{}", "subcategory": "APT", "needsAdmin": true}}"#,
        admin_root.display()
    ));
    let recursive = app(&format!(
        r#"{{"id": "web", "name": "Web", "paths": ["{}"], "recursiveMatch": {{"anchor": "EBWebView", "targets": ["Cache"]}}}}"#,
        journal.display()
    ));
    let report = Run::new(
        &fixture,
        vec![
            RuleEntry::Target(&maintenance),
            RuleEntry::Target(&admin),
            RuleEntry::App {
                file: AppFile::Apps,
                rule: &recursive,
            },
        ],
    )
    .scan();
    let [group] = report.groups() else {
        panic!("only the admin target offers something");
    };
    assert!(group.needs_admin());
    assert_eq!(offered_paths(&report), [fixture.path("apt/old.deb")]);
    assert!(matches!(report.issues(), [ScanIssue::Unsupported { .. }]));
}

#[cfg(unix)]
#[test]
fn single_files_are_checked_like_roots() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let old = fixture.file("home/.xsession-errors.old", 1, 2 * HOUR);
    let recent = fixture.file("home/.recent", 1, Duration::ZERO);
    fixture.file("home/real", 1, 2 * HOUR);
    symlink(fixture.path("home/real"), fixture.path("home/link")).unwrap();
    fixture.file("home/kept", 1, 2 * HOUR);
    symlink(fixture.path("home/kept"), fixture.path("home/kept-alias")).unwrap();
    let file = |path: &Path| -> SingleFileTarget {
        serde_json::from_str(&format!(
            r#"{{"path": "{}", "subcategory": "Errors"}}"#,
            path.display()
        ))
        .unwrap()
    };
    let targets = [
        file(&old),
        file(&recent),
        file(&fixture.path("home/link")),
        file(&fixture.path("home/kept")),
    ];
    let mut run = Run::new(&fixture, targets.iter().map(RuleEntry::File).collect());
    run.options = fixture.options(Exclusions::new([fixture.path("home/kept-alias")]).unwrap());
    let report = run.scan();
    assert_eq!(offered_paths(&report), std::slice::from_ref(&old));
    assert_eq!(report.groups()[0].items()[0].root(), old);
    assert_eq!(report.stats().recent(), 1);
    assert!(
        report
            .issues()
            .iter()
            .any(|issue| matches!(issue, ScanIssue::UnsafeRoot { .. }))
    );
    assert!(
        report
            .issues()
            .iter()
            .any(|issue| matches!(issue, ScanIssue::Excluded { .. }))
    );
}

#[cfg(unix)]
#[test]
fn a_single_file_reached_through_a_linked_directory_is_excluded_by_its_real_path() {
    let fixture = Fixture::new();
    let real = fixture.file("real/errors.old", 1, 2 * HOUR);
    std::os::unix::fs::symlink(fixture.path("real"), fixture.path("linked")).unwrap();
    let target: SingleFileTarget = serde_json::from_str(&format!(
        r#"{{"path": "{}", "subcategory": "Errors"}}"#,
        fixture.path("linked/errors.old").display()
    ))
    .unwrap();
    let mut run = Run::new(&fixture, vec![RuleEntry::File(&target)]);
    run.options = fixture.options(Exclusions::new([real.clone()]).unwrap());
    let report = run.scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert!(matches!(report.issues(), [ScanIssue::Excluded { path, .. }] if *path == real));
}

#[test]
fn an_identifier_belongs_to_one_report() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/old", 1, 2 * HOUR);
    let rule = target_at(&root);
    let run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    let first = run.scan();
    let second = run.scan();
    let id = first.groups()[0].items()[0].id();
    assert!(first.find(id).is_some());
    assert!(second.find(id).is_none());
    assert_ne!(id, second.groups()[0].items()[0].id());
}

#[test]
fn a_rule_that_offers_nothing_has_no_group() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/new", 1, Duration::ZERO);
    let rule = target_at(&root);
    let missing = target_at(&fixture.path("not-installed"));
    let report = Run::new(
        &fixture,
        vec![RuleEntry::Target(&rule), RuleEntry::Target(&missing)],
    )
    .scan();
    assert_eq!(report.groups(), &[]);
    assert!(
        report.issues().is_empty(),
        "a missing root is the normal case"
    );
}

#[test]
fn identifiers_are_unique_and_lead_to_their_own_entry_and_group() {
    let fixture = Fixture::new();
    let first = fixture.directory("first");
    let second = fixture.directory("second");
    fixture.file("first/one", 1, 2 * HOUR);
    fixture.file("first/two", 2, 2 * HOUR);
    fixture.file("second/four", 4, 2 * HOUR);
    let rules = [target_at(&first), target_at(&second)];
    let report = Run::new(&fixture, rules.iter().map(RuleEntry::Target).collect()).scan();
    let items: Vec<_> = report.groups().iter().flat_map(ScanGroup::items).collect();
    assert_eq!(items.len(), 3);
    for item in &items {
        let (group, found) = report.find(item.id()).unwrap();
        assert_eq!(found.path(), item.path());
        assert_eq!(found.size(), item.size());
        assert!(
            group
                .items()
                .iter()
                .any(|candidate| candidate.id() == item.id())
        );
        assert_eq!(found.root(), item.path().parent().unwrap());
    }
    let sizes: Vec<u64> = report.groups().iter().map(ScanGroup::total_size).collect();
    assert_eq!(sizes, [3, 4]);
    let mut ids: Vec<_> = items.iter().map(|item| item.id()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3);
}

/// Both marks at once.
fn both_marks(metadata: &fs::Metadata) -> Option<FileIdentity> {
    let mounted = other_device_when_marked(metadata)?;
    let aliased = same_identity_when_aliased(metadata)?;
    Some(FileIdentity::new(mounted.device(), aliased.inode()))
}

#[test]
fn a_held_back_directory_keeps_its_parent_open() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    // A mounted directory, a variable's directory and another scan root, each
    // one level below a parent that holds an old file.
    for name in ["mounted", "bound", "inner"] {
        fixture.file(&format!("data/{name}-parent/old"), 1, 2 * HOUR);
        fixture.file(&format!("data/{name}-parent/{name}/deep"), 1, 2 * HOUR);
        fixture.age(&fixture.path(&format!("data/{name}-parent")), 2 * HOUR);
    }
    mark(&fixture.path("data/mounted-parent/mounted"), MOUNTED_MARK);
    let home = fixture.directory("home/guest");
    mark(&home, ALIAS_MARK);
    mark(&fixture.path("data/bound-parent/bound"), ALIAS_MARK);
    let outer = target_at(&root);
    let inner = target_at(&fixture.path("data/inner-parent/inner"));
    let mut run = Run::new(
        &fixture,
        vec![RuleEntry::Target(&outer), RuleEntry::Target(&inner)],
    );
    run.readers.identity = both_marks;
    run.variables = fixture.variables(&[(Variable::Home, "home/guest")]);
    let report = run.scan();
    let offered = offered_paths(&report);
    for name in ["mounted", "bound", "inner"] {
        let parent = fixture.path(&format!("data/{name}-parent"));
        assert!(
            !offered.contains(&parent),
            "{name}: its parent must stay open"
        );
        assert!(offered.contains(&parent.join("old")), "{name}");
    }
    assert_eq!(report.stats().other_filesystems(), 1);
    assert_eq!(report.stats().reserved_directories(), 2);
}

#[test]
fn an_ancestor_of_a_variable_met_below_a_root_is_never_entered() {
    let fixture = Fixture::new();
    let root = fixture.directory("tmp");
    // A chroot left mounted under the temporary directory: `chroot/home` has
    // the identity of `${HOME}`'s parent.
    fixture.file("tmp/chroot/home/other/old", 1, 2 * HOUR);
    fixture.directory("real-home/guest");
    mark(&fixture.path("real-home"), ALIAS_MARK);
    mark(&fixture.path("tmp/chroot/home"), ALIAS_MARK);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.identity = same_identity_when_aliased;
    run.variables = fixture.variables(&[(Variable::Home, "real-home/guest")]);
    let report = run.scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert_eq!(report.stats().reserved_directories(), 1);
}

#[test]
fn a_recently_changed_directory_offers_its_old_entries_but_not_itself() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/extracting/old-1", 1, 2 * HOUR);
    fixture.file("data/extracting/old-2", 1, 2 * HOUR);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(
        offered(&report),
        [
            (fixture.path("data/extracting/old-1"), EntryKind::File),
            (fixture.path("data/extracting/old-2"), EntryKind::File),
        ]
    );
    assert_eq!(report.stats().recent(), 1);
}

#[test]
fn a_file_on_another_filesystem_is_held_back() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/local", 1, 2 * HOUR);
    let mounted = fixture.file("data/bound-file", 1, 2 * HOUR);
    mark(&mounted, MOUNTED_MARK);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.identity = other_device_when_marked;
    let report = run.scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/local")]);
    assert_eq!(report.stats().other_filesystems(), 1);
}

#[test]
fn a_root_replaced_after_its_check_is_reported() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/old", 1, 2 * HOUR);
    let misc = no_protected_names();
    let protected =
        crate::protection::ProtectedRoots::new(&TemplateVariables::new(), file_identity).unwrap();
    let roots = std::collections::BTreeSet::new();
    let exclusions = Exclusions::none();
    let context = crate::walk::WalkContext {
        cutoff: fixture.now - HOUR,
        exclusions: &exclusions,
        misc: &misc,
        protected: &protected,
        roots: &roots,
        readers: Readers::SYSTEM,
        limits: Limits::DEFAULT,
        remover: None,
    };
    // The identity checked before does not match what is opened now.
    let walked = crate::walk::walk(&root, FileIdentity::new(0, 0), context);
    assert_eq!(
        walked.root_problem,
        Some(crate::walk::RootProblem::Replaced)
    );
    assert_eq!(walked.found, Vec::new());
}

#[test]
fn a_root_reached_through_a_linked_ancestor_is_walked_by_its_real_path() {
    let fixture = Fixture::new();
    fixture.file("real/data/old", 1, 2 * HOUR);
    fixture.file("real/data/keep/old", 1, 2 * HOUR);
    std::os::unix::fs::symlink(fixture.path("real"), fixture.path("alias")).unwrap();
    let rule = target_at(&fixture.path("alias/data"));
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.options = fixture.options(Exclusions::new([fixture.path("real/data/keep")]).unwrap());
    let report = run.scan();
    assert_eq!(offered_paths(&report), [fixture.path("real/data/old")]);
    assert_eq!(
        report.groups()[0].items()[0].root(),
        fixture.path("real/data")
    );
}

/// Whether this process can read a directory it made unreadable (as root can).
fn reads_through_permissions(fixture: &Fixture) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let probe = fixture.directory("probe");
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read_dir(&probe).is_ok();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
    readable
}

#[test]
fn unreadable_entries_and_roots_are_reported() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    if reads_through_permissions(&fixture) {
        return;
    }
    let root = fixture.directory("data");
    fixture.file("data/locked/x", 1, 2 * HOUR);
    fixture.file("data/parent/old", 1, 2 * HOUR);
    fixture.file("data/parent/locked/x", 1, 2 * HOUR);
    fixture.age(&fixture.path("data/parent"), 2 * HOUR);
    let unreadable_root = fixture.directory("closed");
    let base = fixture.directory("apps");
    for locked in ["data/locked", "data/parent/locked", "closed", "apps"] {
        fs::set_permissions(fixture.path(locked), fs::Permissions::from_mode(0o000)).unwrap();
    }
    let rules = [
        target_at(&root),
        target_at(&unreadable_root),
        target(&format!(
            r#"{{"path": "{}", "subcategory": "Apps", "childSubdir": "cache"}}"#,
            base.display()
        )),
    ];
    let report = Run::new(&fixture, rules.iter().map(RuleEntry::Target).collect()).scan();
    for locked in ["data/locked", "data/parent/locked", "closed", "apps"] {
        fs::set_permissions(fixture.path(locked), fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(offered_paths(&report), [fixture.path("data/parent/old")]);
    assert_eq!(report.stats().unreadable(), 2);
    let mut unreadable: Vec<(PathBuf, String)> = report
        .issues()
        .iter()
        .map(|issue| match issue {
            ScanIssue::UnreadableRoot {
                origins,
                path,
                error: std::io::ErrorKind::PermissionDenied,
            } => (path.clone(), origin_label(origins)),
            other => panic!("unexpected issue {other:?}"),
        })
        .collect();
    unreadable.sort();
    assert_eq!(
        unreadable,
        [
            (base, "Apps".to_owned()),
            (unreadable_root, "Temp".to_owned())
        ]
    );
}

#[test]
fn every_entry_carries_the_cutoff_it_was_judged_against() {
    let fixture = Fixture::new();
    let root = fixture.directory("cache");
    let file = fixture.file("cache/old", 3, 3 * 24 * HOUR);
    let strict = app(&format!(
        r#"{{"id": "strict", "name": "Strict", "paths": ["{}"], "minAgeDays": 2}}"#,
        root.display()
    ));
    let report = Run::new(
        &fixture,
        vec![RuleEntry::App {
            file: AppFile::Apps,
            rule: &strict,
        }],
    )
    .scan();
    let item = &report.groups()[0].items()[0];
    assert_eq!(item.cutoff(), fixture.now - 48 * HOUR);
    assert_eq!(item.size(), 3);
    assert_eq!(item.last_change(), fixture.now - 3 * 24 * HOUR);
    assert_eq!(
        item.identity(),
        file_identity(&fs::symlink_metadata(&file).unwrap()).unwrap()
    );
    assert_eq!(report.groups()[0].total_size(), 3);
}

#[test]
fn group_attributes_follow_the_rules() {
    let fixture = Fixture::new();
    let plain = fixture.directory("plain");
    let reset = fixture.directory("reset");
    let admin = fixture.directory("admin");
    for directory in ["plain", "reset", "admin"] {
        fixture.file(&format!("{directory}/old"), 1, 2 * HOUR);
    }
    let rules = [
        target_at(&plain),
        target(&format!(
            r#"{{"path": "{}", "subcategory": "Shaders", "cacheReset": true}}"#,
            reset.display()
        )),
        target(&format!(
            r#"{{"path": "{}", "subcategory": "APT", "needsAdmin": true}}"#,
            admin.display()
        )),
    ];
    let report = Run::new(&fixture, rules.iter().map(RuleEntry::Target).collect()).scan();
    let flags: Vec<_> = report
        .groups()
        .iter()
        .map(|group| {
            (
                group.is_selected_by_default(),
                group.is_cache_reset(),
                group.needs_admin(),
            )
        })
        .collect();
    assert_eq!(
        flags,
        [
            (true, false, false),
            (false, true, false),
            (true, false, true)
        ]
    );

    let shared = fixture.directory("shared");
    fixture.file("shared/old", 1, 2 * HOUR);
    let user = target_at(&shared);
    let system = target(&format!(
        r#"{{"path": "{}", "subcategory": "System", "needsAdmin": true}}"#,
        shared.display()
    ));
    for order in [[&user, &system], [&system, &user]] {
        let report = Run::new(&fixture, order.into_iter().map(RuleEntry::Target).collect()).scan();
        assert!(
            report.groups()[0].needs_admin(),
            "either rule needing admin is enough"
        );
    }
}

#[test]
fn a_nested_root_keeps_its_entries_in_either_order() {
    let fixture = Fixture::new();
    let outer = fixture.directory("logs");
    let inner = fixture.directory("logs/app");
    fixture.file("logs/app/old", 1, 2 * HOUR);
    fixture.file("logs/top", 1, 2 * HOUR);
    let outer_rule = target_at(&outer);
    let inner_rule = target_at(&inner);
    for order in [[&outer_rule, &inner_rule], [&inner_rule, &outer_rule]] {
        let report = Run::new(&fixture, order.into_iter().map(RuleEntry::Target).collect()).scan();
        let by_root: Vec<(PathBuf, PathBuf)> = report
            .groups()
            .iter()
            .flat_map(ScanGroup::items)
            .map(|item| (item.root().to_path_buf(), item.path().to_path_buf()))
            .collect();
        assert!(by_root.contains(&(inner.clone(), fixture.path("logs/app/old"))));
        assert!(by_root.contains(&(outer.clone(), fixture.path("logs/top"))));
        assert_eq!(by_root.len(), 2);
    }
}

#[test]
fn the_depth_limit_counts_levels_below_the_root() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/a/b/kept", 1, 2 * HOUR);
    fixture.file("data/a/b/c/too-deep", 1, 2 * HOUR);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.limits = Limits {
        depth: 2,
        entries: 1_000,
    };
    let report = run.scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/a/b/kept")]);
}

#[test]
fn single_file_targets_report_exactly_what_held_them_back() {
    let fixture = Fixture::new();
    let protected = fixture.file("logs/Security.evtx", 1, 2 * HOUR);
    let directory = fixture.directory("logs/a-directory");
    let file = |path: &Path| -> SingleFileTarget {
        serde_json::from_str(&format!(
            r#"{{"path": "{}", "subcategory": "Logs"}}"#,
            path.display()
        ))
        .unwrap()
    };
    let targets = [file(&protected), file(&directory)];
    let mut run = Run::new(&fixture, targets.iter().map(RuleEntry::File).collect());
    run.misc = serde_json::from_str(
        r#"{"type": "misc", "protectedEventLogs": ["security.evtx"], "trashPath": null}"#,
    )
    .unwrap();
    let report = run.scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert_eq!(report.stats().protected_names(), 1);
    assert!(matches!(
        report.issues(),
        [ScanIssue::UnsafeRoot {
            path,
            reason: crate::report::UnsafeReason::NotAFile,
            origins,
        }] if *path == directory && origin_label(origins) == "Logs"
    ));
}

#[test]
fn a_root_that_is_a_file_is_refused() {
    let fixture = Fixture::new();
    let not_a_directory = fixture.file("plain-file", 1, 2 * HOUR);
    let rule = target_at(&not_a_directory);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert!(matches!(
        report.issues(),
        [ScanIssue::UnsafeRoot {
            path,
            reason: crate::report::UnsafeReason::NotADirectory,
            ..
        }] if *path == not_a_directory
    ));
}

#[test]
fn ages_beyond_the_clock_and_a_zero_global_age() {
    let fixture = Fixture::new();
    let root = fixture.directory("cache");
    fixture.file("cache/three-days", 1, 3 * 24 * HOUR);
    fixture.file("cache/one-day", 1, 24 * HOUR);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.options = ScanOptions::new(fixture.now, Duration::MAX, Exclusions::none());
    let report = run.scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert_eq!(
        report.issues(),
        [ScanIssue::Stopped(Stop::MinAgeBeyondClock)]
    );
    assert!(report.issues()[0].stops_the_scan());

    // Only the rule's own age reaches before the clock's origin.
    let rule_only = app(&format!(
        r#"{{"id": "far", "name": "Far", "paths": ["{}"], "minAgeDays": 2}}"#,
        root.display()
    ));
    let mut run = Run::new(
        &fixture,
        vec![RuleEntry::App {
            file: AppFile::Apps,
            rule: &rule_only,
        }],
    );
    // The clock reaches far before 1970, but not 2 days before this instant.
    let near_the_clock_origin = SystemTime::UNIX_EPOCH
        .checked_sub(Duration::from_secs(
            u64::try_from(i64::MAX).unwrap() - 100_000,
        ))
        .unwrap();
    run.options = ScanOptions::new(near_the_clock_origin, HOUR, Exclusions::none());
    let report = run.scan();
    assert!(matches!(
        report.issues(),
        [ScanIssue::AgeBeyondClock { .. }]
    ));
    assert!(!report.issues()[0].stops_the_scan());

    let strict = app(&format!(
        r#"{{"id": "strict", "name": "Strict", "paths": ["{}"], "minAgeDays": 2}}"#,
        root.display()
    ));
    let mut run = Run::new(
        &fixture,
        vec![RuleEntry::App {
            file: AppFile::Apps,
            rule: &strict,
        }],
    );
    run.options = ScanOptions::new(fixture.now, Duration::ZERO, Exclusions::none());
    assert_eq!(
        offered_paths(&run.scan()),
        [fixture.path("cache/three-days")]
    );
}

#[test]
fn a_variable_that_cannot_be_located_stops_the_scan() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/old", 1, 2 * HOUR);
    std::os::unix::fs::symlink(fixture.path("nowhere"), fixture.path("dangling")).unwrap();
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.variables = fixture.variables(&[(Variable::Home, "dangling/guest")]);
    let report = run.scan();
    assert_eq!(report.groups(), &[]);
    assert!(matches!(
        report.issues(),
        [ScanIssue::Stopped(Stop::UnreadableVariable {
            variable: Variable::Home,
            ..
        })]
    ));
    assert!(report.issues()[0].stops_the_scan());
}

/// The modification date that marks an entry as the root of a mount on the
/// same device (a bind mount), for the mount readers below.
const BIND_MARK: Duration = Duration::from_hours(300_004);

fn bears_bind_mark(path: &Path) -> std::io::Result<bool> {
    let modified = fs::symlink_metadata(path)?.modified()?;
    Ok(modified == SystemTime::UNIX_EPOCH + BIND_MARK)
}

/// Mount roots as the marks say.
fn mount_root_when_marked(path: &Path) -> std::io::Result<Option<bool>> {
    bears_bind_mark(path).map(Some)
}

/// A mount status that cannot be read for marked entries.
fn mount_status_refused_when_marked(path: &Path) -> std::io::Result<Option<bool>> {
    if bears_bind_mark(path)? {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    Ok(Some(false))
}

/// Marked entries are gone by the time their mount status is read.
fn gone_when_marked(path: &Path) -> std::io::Result<Option<bool>> {
    if bears_bind_mark(path)? {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    Ok(Some(false))
}

/// A system that cannot tell a mount root.
#[expect(
    clippy::unnecessary_wraps,
    reason = "a stand-in for the mount reader, whose signature it must have"
)]
fn mount_root_unknown(_path: &Path) -> std::io::Result<Option<bool>> {
    Ok(None)
}

#[test]
fn a_bind_mounted_directory_or_file_keeps_its_parent_and_is_never_entered() {
    for bound in ["directory", "file"] {
        let fixture = Fixture::new();
        let root = fixture.directory("data");
        let parent = format!("data/{bound}-parent");
        let old = fixture.file(&format!("{parent}/old"), 1, 2 * HOUR);
        let marked = if bound == "directory" {
            fixture.file(&format!("{parent}/bound/deep"), 1, 2 * HOUR);
            fixture.path(&format!("{parent}/bound"))
        } else {
            fixture.file(&format!("{parent}/bound"), 1, 2 * HOUR)
        };
        mark(&marked, BIND_MARK);
        fixture.age(&fixture.path(&parent), 2 * HOUR);
        let rule = target_at(&root);
        let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
        run.readers.mount_root = mount_root_when_marked;
        let report = run.scan();
        assert_eq!(
            offered_paths(&report),
            [old],
            "{bound}: neither its parent whole nor anything below it"
        );
        assert_eq!(report.stats().other_filesystems(), 1, "{bound}");
        assert_eq!(report.stats().unreadable(), 0, "{bound}");
        assert_eq!(report.issues(), &[], "{bound}");
    }
}

#[test]
fn a_mount_status_that_cannot_be_read_holds_back_and_is_counted_once() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    let old = fixture.file("data/parent/old", 1, 2 * HOUR);
    fixture.file("data/parent/locked/deep", 1, 2 * HOUR);
    mark(&fixture.path("data/parent/locked"), BIND_MARK);
    fixture.age(&fixture.path("data/parent"), 2 * HOUR);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.mount_root = mount_status_refused_when_marked;
    let report = run.scan();
    assert_eq!(offered_paths(&report), [old]);
    assert_eq!(report.stats().unreadable(), 1);
    assert_eq!(report.stats().other_filesystems(), 0);
    assert_eq!(report.issues(), &[], "counted, not reported twice");
}

#[test]
fn an_entry_gone_before_its_mount_status_is_read_is_not_counted() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    let old = fixture.file("data/parent/old", 1, 2 * HOUR);
    let gone = fixture.file("data/parent/gone", 1, 2 * HOUR);
    mark(&gone, BIND_MARK);
    fixture.age(&fixture.path("data/parent"), 2 * HOUR);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.mount_root = gone_when_marked;
    let report = run.scan();
    assert_eq!(offered_paths(&report), [old], "nor its parent, whole");
    assert_eq!(report.stats().unreadable(), 0);
    assert_eq!(report.stats().other_filesystems(), 0);
}

#[test]
fn a_system_that_cannot_tell_mount_roots_scans_as_before() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/parent/sub/old", 1, 2 * HOUR);
    fixture.age(&fixture.path("data/parent/sub"), 2 * HOUR);
    fixture.age(&fixture.path("data/parent"), 2 * HOUR);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.mount_root = mount_root_unknown;
    assert_eq!(offered_paths(&run.scan()), [fixture.path("data/parent")]);
}

#[test]
fn a_scan_root_that_is_a_mount_is_still_walked() {
    let fixture = Fixture::new();
    let root = fixture.directory("tmp");
    let old = fixture.file("tmp/old", 1, 2 * HOUR);
    mark(&root, BIND_MARK);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.mount_root = mount_root_when_marked;
    let report = run.scan();
    assert_eq!(offered_paths(&report), [old]);
    assert_eq!(report.stats().other_filesystems(), 0);
}

/// An expansion `apps/*/cache/data`, with an old file below `local` and
/// below `bound`, and `marked` marked as a bind mount.
fn expansion_with(fixture: &Fixture, marked: &str) -> CleanTarget {
    let base = fixture.directory("apps");
    fixture.file("apps/local/cache/data/old", 1, 2 * HOUR);
    fixture.file("apps/bound/cache/data/old", 1, 2 * HOUR);
    mark(&fixture.path(marked), BIND_MARK);
    target(&format!(
        r#"{{"path": "{}", "subcategory": "Apps", "childSubdir": "cache/data"}}"#,
        base.display()
    ))
}

#[test]
fn an_expansion_never_goes_through_a_bind_mount() {
    for marked in ["apps/bound", "apps/bound/cache", "apps/bound/cache/data"] {
        let fixture = Fixture::new();
        let rule = expansion_with(&fixture, marked);
        let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
        run.readers.mount_root = mount_root_when_marked;
        let report = run.scan();
        assert_eq!(
            offered_paths(&report),
            [fixture.path("apps/local/cache/data/old")],
            "{marked}"
        );
        assert_eq!(report.stats().other_filesystems(), 1, "{marked}");
        assert_eq!(report.issues(), &[], "{marked}");
    }
}

#[test]
fn a_root_withheld_by_an_expansion_still_joins_the_rules_that_share_it() {
    for (reset_age, offered) in [("", true), (r#", "minAgeDays": 1"#, false)] {
        let fixture = Fixture::new();
        let base = fixture.directory("apps");
        let old = fixture.file("apps/bound/cache/old", 1, 2 * HOUR);
        mark(&fixture.path("apps/bound"), BIND_MARK);
        // A plain rule on the directory itself (its own path may be a
        // mount, so it walks it), and a cache reset reaching it across the
        // bind mount (withheld); in both orders.
        let plain = target_at(&fixture.path("apps/bound/cache"));
        let reset = target(&format!(
            r#"{{"path": "{}", "subcategory": "Shaders", "childSubdir": "cache", "cacheReset": true{reset_age}}}"#,
            base.display()
        ));
        for rules in [[&plain, &reset], [&reset, &plain]] {
            let mut run = Run::new(&fixture, rules.into_iter().map(RuleEntry::Target).collect());
            run.readers.mount_root = mount_root_when_marked;
            let report = run.scan();
            if offered {
                assert_eq!(offered_paths(&report), std::slice::from_ref(&old));
                let group = &report.groups()[0];
                assert!(!group.is_selected_by_default(), "the AND of both rules");
                assert!(group.is_cache_reset());
            } else {
                assert_eq!(
                    offered_paths(&report),
                    Vec::<PathBuf>::new(),
                    "the strictest age of both rules"
                );
            }
        }
    }
}

/// Mount roots by name: an entry named `bound` is one, whatever its inode
/// (a hard link elsewhere shares the inode, not the mount).
fn mount_root_when_named_bound(path: &Path) -> std::io::Result<Option<bool>> {
    fs::symlink_metadata(path)?;
    Ok(Some(
        path.file_name() == Some(std::ffi::OsStr::new("bound")),
    ))
}

#[test]
fn a_bound_file_target_still_joins_the_rule_that_offers_the_same_file() {
    let fixture = Fixture::new();
    let real = fixture.file("home/real", 1, 2 * HOUR);
    fs::hard_link(&real, fixture.path("home/bound")).unwrap();
    let file = |path: &Path, subcategory: &str| -> SingleFileTarget {
        serde_json::from_str(&format!(
            r#"{{"path": "{}", "subcategory": "{subcategory}"}}"#,
            path.display()
        ))
        .unwrap()
    };
    let bound = file(&fixture.path("home/bound"), "Bound");
    let direct = file(&real, "Direct");
    for targets in [[&bound, &direct], [&direct, &bound]] {
        let mut run = Run::new(&fixture, targets.into_iter().map(RuleEntry::File).collect());
        run.readers.mount_root = mount_root_when_named_bound;
        let report = run.scan();
        assert_eq!(offered_paths(&report), std::slice::from_ref(&real));
        assert_eq!(
            report.groups()[0].origins().len(),
            2,
            "one group, both rules"
        );
        assert_eq!(report.stats().other_filesystems(), 1);
    }
}

#[test]
fn a_withheld_root_that_cannot_be_read_is_reported() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    if reads_through_permissions(&fixture) {
        return;
    }
    let rule = expansion_with(&fixture, "apps/bound");
    let locked = fixture.path("apps/bound/cache");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.mount_root = mount_root_when_marked;
    let report = run.scan();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        offered_paths(&report),
        [fixture.path("apps/local/cache/data/old")]
    );
    assert!(matches!(
        report.issues(),
        [ScanIssue::UnreadableRoot { path, .. }] if *path == fixture.path("apps/bound/cache/data")
    ));
}

#[test]
fn a_root_withheld_through_a_link_is_walked_by_its_real_path_with_both_rules() {
    use std::os::unix::fs::symlink;

    // The link as the expanded child, then as a segment below it.
    for (link, target_of_link) in [("apps/x", "../real"), ("apps/x/cache", "../../real/cache")] {
        let fixture = Fixture::new();
        let base = fixture.directory("apps");
        let old = fixture.file("real/cache/old", 1, 2 * HOUR);
        if link == "apps/x/cache" {
            fixture.directory("apps/x");
        }
        symlink(target_of_link, fixture.path(link)).unwrap();
        let plain = target_at(&fixture.path("real/cache"));
        let reset = target(&format!(
            r#"{{"path": "{}", "subcategory": "Shaders", "childSubdir": "cache", "cacheReset": true}}"#,
            base.display()
        ));
        for rules in [[&plain, &reset], [&reset, &plain]] {
            let report =
                Run::new(&fixture, rules.into_iter().map(RuleEntry::Target).collect()).scan();
            assert_eq!(offered_paths(&report), std::slice::from_ref(&old), "{link}");
            let group = &report.groups()[0];
            assert!(
                !group.is_selected_by_default(),
                "{link}: the AND of both rules"
            );
            assert!(group.is_cache_reset(), "{link}");
        }
    }
}

#[test]
fn a_root_withheld_below_another_rules_root_is_offered_by_nobody() {
    let fixture = Fixture::new();
    let base = fixture.directory("apps");
    let top = fixture.file("apps/bound/top", 1, 2 * HOUR);
    fixture.file("apps/bound/cache/old", 1, 2 * HOUR);
    mark(&fixture.path("apps/bound"), BIND_MARK);
    // A plain rule on the bound directory itself (its own path may be a
    // mount), and a cache reset that reaches `cache` across the bind mount.
    let plain = target_at(&fixture.path("apps/bound"));
    let reset = target(&format!(
        r#"{{"path": "{}", "subcategory": "Shaders", "childSubdir": "cache", "cacheReset": true}}"#,
        base.display()
    ));
    let mut run = Run::new(
        &fixture,
        vec![RuleEntry::Target(&plain), RuleEntry::Target(&reset)],
    );
    run.readers.mount_root = mount_root_when_marked;
    let report = run.scan();
    assert_eq!(
        offered_paths(&report),
        [top],
        "the reset's root is reserved: never offered without its restrictions"
    );
}

#[test]
fn an_expansion_base_that_is_a_mount_is_still_expanded() {
    let fixture = Fixture::new();
    let rule = expansion_with(&fixture, "apps");
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.mount_root = mount_root_when_marked;
    let report = run.scan();
    assert_eq!(
        offered_paths(&report),
        [
            fixture.path("apps/bound/cache/data/old"),
            fixture.path("apps/local/cache/data/old")
        ]
    );
    assert_eq!(report.stats().other_filesystems(), 0);
}

#[test]
fn an_expansion_reports_a_mount_status_it_cannot_read_and_skips_one_gone() {
    let fixture = Fixture::new();
    let rule = expansion_with(&fixture, "apps/bound/cache");
    let local = fixture.path("apps/local/cache/data/old");
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.mount_root = mount_status_refused_when_marked;
    let report = run.scan();
    assert_eq!(offered_paths(&report), std::slice::from_ref(&local));
    assert!(matches!(
        report.issues(),
        [ScanIssue::UnreadableRoot { path, .. }] if *path == fixture.path("apps/bound/cache")
    ));
    assert_eq!(report.stats().other_filesystems(), 0);

    run.readers.mount_root = gone_when_marked;
    let report = run.scan();
    assert_eq!(offered_paths(&report), std::slice::from_ref(&local));
    assert_eq!(report.issues(), &[]);
    assert_eq!(report.stats().other_filesystems(), 0);

    run.readers.mount_root = mount_root_unknown;
    assert_eq!(
        offered_paths(&run.scan()),
        [
            fixture.path("apps/bound/cache/data/old"),
            fixture.path("apps/local/cache/data/old")
        ],
        "a system that cannot tell"
    );
}

/// A mount status that cannot be read for entries on the marked device.
fn mount_status_refused_when_mounted(path: &Path) -> std::io::Result<Option<bool>> {
    let modified = fs::symlink_metadata(path)?.modified()?;
    if modified == SystemTime::UNIX_EPOCH + MOUNTED_MARK {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    Ok(Some(false))
}

#[test]
fn an_expansion_leaves_another_device_without_reading_its_mount_status() {
    let fixture = Fixture::new();
    // Marked bound first, then on another device: the last mark wins.
    let rule = expansion_with(&fixture, "apps/bound");
    mark(&fixture.path("apps/bound"), MOUNTED_MARK);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.identity = other_device_when_marked;
    run.readers.mount_root = mount_status_refused_when_mounted;
    let report = run.scan();
    assert_eq!(report.issues(), &[], "its status was never asked for");
    assert_eq!(report.stats().unreadable(), 0);
    assert_eq!(report.stats().other_filesystems(), 1);
    assert_eq!(
        offered_paths(&report),
        [fixture.path("apps/local/cache/data/old")]
    );
}

#[test]
fn a_file_target_that_is_a_mount_root_is_not_offered() {
    let fixture = Fixture::new();
    let bound = fixture.file("home/.xsession-errors.old", 1, 2 * HOUR);
    mark(&bound, BIND_MARK);
    let target: SingleFileTarget = serde_json::from_str(&format!(
        r#"{{"path": "{}", "subcategory": "Errors"}}"#,
        bound.display()
    ))
    .unwrap();
    let mut run = Run::new(&fixture, vec![RuleEntry::File(&target)]);
    run.readers.mount_root = mount_root_when_marked;
    let report = run.scan();
    assert_eq!(report.groups(), &[]);
    assert_eq!(report.issues(), &[]);
    assert_eq!(report.stats().other_filesystems(), 1);

    run.readers.mount_root = mount_status_refused_when_marked;
    let report = run.scan();
    assert_eq!(report.groups(), &[]);
    assert!(matches!(
        report.issues(),
        [ScanIssue::UnreadableRoot { path, .. }] if *path == bound
    ));
    assert_eq!(report.stats().other_filesystems(), 0);

    run.readers.mount_root = gone_when_marked;
    let report = run.scan();
    assert_eq!(report.groups(), &[]);
    assert_eq!(report.issues(), &[]);
    assert_eq!(report.stats().other_filesystems(), 0);

    run.readers.mount_root = mount_root_unknown;
    assert_eq!(
        offered_paths(&run.scan()),
        [bound],
        "a system that cannot tell"
    );
}

#[test]
fn an_expanded_subdirectory_on_another_filesystem_is_skipped() {
    let fixture = Fixture::new();
    let base = fixture.directory("apps");
    fixture.file("apps/local/cache/old", 1, 2 * HOUR);
    // The whole mounted subtree is on the other device, its file included, so
    // only the expansion check can keep it out.
    let mounted_file = fixture.file("apps/mounted/cache/old", 1, 2 * HOUR);
    mark(&mounted_file, MOUNTED_MARK);
    mark(&fixture.path("apps/mounted/cache"), MOUNTED_MARK);
    let rule = target(&format!(
        r#"{{"path": "{}", "subcategory": "Apps", "childSubdir": "cache"}}"#,
        base.display()
    ));
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.identity = other_device_when_marked;
    let report = run.scan();
    assert_eq!(
        offered_paths(&report),
        [fixture.path("apps/local/cache/old")]
    );
    assert_eq!(report.stats().other_filesystems(), 1);
}

#[test]
fn a_variable_in_an_unreadable_directory_stops_the_scan() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    if reads_through_permissions(&fixture) {
        return;
    }
    let root = fixture.directory("data");
    fixture.file("data/old", 1, 2 * HOUR);
    fixture.directory("locked/guest");
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.variables = fixture.variables(&[(Variable::Home, "locked/guest")]);
    fs::set_permissions(fixture.path("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let report = run.scan();
    fs::set_permissions(fixture.path("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(report.groups(), &[]);
    assert!(matches!(
        report.issues(),
        [ScanIssue::Stopped(Stop::UnreadableVariable {
            variable: Variable::Home,
            error: std::io::ErrorKind::PermissionDenied,
            ..
        })]
    ));
    assert!(report.issues()[0].stops_the_scan());
}

#[test]
fn a_recent_subdirectory_keeps_its_old_parents_open() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/a/old", 1, 2 * HOUR);
    fixture.file("data/a/b/old", 1, 2 * HOUR);
    fixture.file("data/a/b/c/old", 1, 2 * HOUR);
    // `c` changed just now (an extraction under way); `a` and `b` are old.
    fixture.age(&fixture.path("data/a/b"), 2 * HOUR);
    fixture.age(&fixture.path("data/a"), 2 * HOUR);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(
        offered_paths(&report),
        [
            fixture.path("data/a/b/c/old"),
            fixture.path("data/a/b/old"),
            fixture.path("data/a/old")
        ]
    );
    assert_eq!(report.stats().recent(), 1);
}

#[test]
fn a_directory_at_the_cutoff_folds_and_one_after_does_not() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/at/old", 1, 2 * HOUR);
    fixture.file("data/after/old", 1, 2 * HOUR);
    fixture.age(&fixture.path("data/at"), HOUR);
    fixture.age(
        &fixture.path("data/after"),
        HOUR.checked_sub(Duration::from_secs(1)).unwrap(),
    );
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(
        offered(&report),
        [
            (fixture.path("data/after/old"), EntryKind::File),
            (fixture.path("data/at"), EntryKind::Directory),
        ]
    );
}

#[test]
fn a_file_set_back_in_time_is_still_recent_for_the_real_reader() {
    let fixture = Fixture::new();
    let root = fixture.directory("tmp");
    // What an extractor does: the file is new, its modification time is old.
    fixture.file("tmp/extracted", 1, 30 * 24 * HOUR);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers = Readers::SYSTEM;
    let report = run.scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert_eq!(report.stats().recent(), 1);
}

#[test]
fn each_limit_is_reported_once_per_root() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    for name in ["one", "two", "three", "four", "five"] {
        fixture.file(&format!("data/wide/{name}"), 1, 2 * HOUR);
    }
    for deep in ["data/x/y/z/deep", "data/p/q/r/deep"] {
        fixture.file(deep, 1, 2 * HOUR);
    }
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.limits = Limits {
        depth: 2,
        entries: 4,
    };
    let report = run.scan();
    let count = |limit: Limit| {
        report
            .issues()
            .iter()
            .filter(|issue| matches!(issue, ScanIssue::LimitReached { limit: found, .. } if *found == limit))
            .count()
    };
    assert_eq!(count(Limit::Entries), 1);
    run.limits = Limits {
        depth: 2,
        entries: 1_000,
    };
    let report = run.scan();
    let depth_issues = report
        .issues()
        .iter()
        .filter(|issue| {
            matches!(
                issue,
                ScanIssue::LimitReached {
                    limit: Limit::Depth,
                    ..
                }
            )
        })
        .count();
    assert_eq!(depth_issues, 1);
}

#[test]
fn an_extension_written_in_capitals_still_matches() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/build.log", 1, 2 * HOUR);
    fixture.file("data/kept.txt", 1, 2 * HOUR);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.options = fixture.options(Exclusions::new([PathBuf::from("*.LOG")]).unwrap());
    assert_eq!(offered_paths(&run.scan()), [fixture.path("data/kept.txt")]);
}

#[test]
fn child_subdirectories_of_several_segments_skip_files_and_absences_quietly() {
    let fixture = Fixture::new();
    let base = fixture.directory("apps");
    fixture.file("apps/one/var/cache/old", 1, 2 * HOUR);
    fixture.file("apps/two/var/cache", 1, 2 * HOUR);
    fixture.directory("apps/three");
    let rule = target(&format!(
        r#"{{"path": "{}", "subcategory": "Apps", "childSubdir": "var/cache"}}"#,
        base.display()
    ));
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(
        offered_paths(&report),
        [fixture.path("apps/one/var/cache/old")]
    );
    assert_eq!(report.stats().unreadable(), 0);
    assert_eq!(report.issues(), &[]);
}

#[test]
fn a_group_names_its_rules_and_their_section() {
    let fixture = Fixture::new();
    let root = fixture.directory("cache");
    fixture.file("cache/old", 1, 2 * HOUR);
    let rule = app(&format!(
        r#"{{"id": "tool", "name": "Tool Cache", "group": "Developer Tools", "paths": ["{}"]}}"#,
        root.display()
    ));
    let report = Run::new(
        &fixture,
        vec![RuleEntry::App {
            file: AppFile::Gaming,
            rule: &rule,
        }],
    )
    .scan();
    let group = &report.groups()[0];
    assert_eq!(group.label().as_str(), "Tool Cache");
    assert_eq!(group.rule_group().unwrap().as_str(), "Developer Tools");
    assert!(matches!(
        group.origins(),
        [crate::report::Origin::App { file: AppFile::Gaming, id }] if id.as_str() == "tool"
    ));
}

#[test]
fn a_file_target_below_a_directory_root_is_offered_once() {
    let fixture = Fixture::new();
    let root = fixture.directory("home");
    let file = fixture.file("home/.xsession-errors.old", 1, 2 * HOUR);
    let directory_rule = target_at(&root);
    let file_rule: SingleFileTarget = serde_json::from_str(&format!(
        r#"{{"path": "{}", "subcategory": "Errors"}}"#,
        file.display()
    ))
    .unwrap();
    let report = Run::new(
        &fixture,
        vec![
            RuleEntry::Target(&directory_rule),
            RuleEntry::File(&file_rule),
        ],
    )
    .scan();
    assert_eq!(offered_paths(&report), [file]);
    assert_eq!(report.stats().reserved_directories(), 1);
}

thread_local! {
    /// How many times [`replaced_on_reopening`] read the marked directory.
    static MARKED_READS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// How many reads of the marked directory still see the original.
    static READS_BEFORE_SWAP: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
}

/// The modification date that marks a directory replaced between its judgment
/// and its opening, for [`replaced_on_reopening`].
const REPLACED_MARK: Duration = Duration::from_hours(200_000);

/// The real identity, except that a directory bearing [`REPLACED_MARK`] gets
/// another identity from its second read on: what a swap between the two
/// reads looks like.
fn replaced_on_reopening(metadata: &fs::Metadata) -> Option<FileIdentity> {
    let identity = file_identity(metadata)?;
    if metadata.modified().ok() != Some(SystemTime::UNIX_EPOCH + REPLACED_MARK) {
        return Some(identity);
    }
    let reads = MARKED_READS.with(|reads| {
        reads.set(reads.get() + 1);
        reads.get()
    });
    let before_swap = READS_BEFORE_SWAP.with(std::cell::Cell::get);
    Some(if reads <= before_swap {
        identity
    } else {
        FileIdentity::new(identity.device(), u64::MAX - 1)
    })
}

#[test]
fn a_subdirectory_replaced_after_its_judgment_is_counted_and_not_entered() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/swapped/old", 1, 2 * HOUR);
    fixture.file("data/kept/old", 1, 2 * HOUR);
    fixture.age(&fixture.path("data/kept"), 2 * HOUR);
    mark(&fixture.path("data/swapped"), REPLACED_MARK);
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.identity = replaced_on_reopening;
    MARKED_READS.with(|reads| reads.set(0));
    READS_BEFORE_SWAP.with(|reads| reads.set(1));
    let report = run.scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/kept")]);
    assert_eq!(report.stats().replaced(), 1);
    assert_eq!(report.stats().unreadable(), 0);
}

#[test]
fn the_public_scan_plans_the_embedded_rules() {
    let fixture = Fixture::new();
    let pip = fixture.directory("cache/pip");
    fixture.file("cache/pip/old", 1, 2 * HOUR);
    for directory in ["home", "config", "share", "tmp"] {
        fixture.directory(directory);
    }
    let variables = fixture.variables(&[
        (Variable::Home, "home"),
        (Variable::Config, "config"),
        (Variable::Cache, "cache"),
        (Variable::LocalShare, "share"),
        (Variable::TempDirectory, "tmp"),
    ]);
    // Excluding the whole filesystem keeps the scan away from the real system
    // directories the rules name, while each existing root is still planned.
    let options = ScanOptions::new(
        fixture.now,
        HOUR,
        Exclusions::new([PathBuf::from("/")]).unwrap(),
    );
    let rules = okapi_rules::RuleSet::embedded(okapi_rules::Os::Linux).unwrap();
    let report = crate::scan(&rules, &variables, &options);
    assert!(
        !report
            .issues()
            .contains(&ScanIssue::Stopped(Stop::UnsupportedSystem))
    );
    assert!(report.issues().iter().any(|issue| matches!(
        issue,
        ScanIssue::Excluded { path, origins } if *path == pip
            && matches!(origins.as_slice(), [crate::report::Origin::App { id, .. }] if id.as_str() == "pip")
    )));
    assert_eq!(report.groups(), &[]);
}

/// The modification date that marks an entry whose date and identity cannot
/// be read, for [`unknown_when_marked`] and [`no_date_when_marked`].
const UNKNOWN_MARK: Duration = Duration::from_hours(100_000);

fn is_marked_unknown(metadata: &fs::Metadata) -> bool {
    metadata.modified().ok() == Some(SystemTime::UNIX_EPOCH + UNKNOWN_MARK)
}

fn unknown_when_marked(metadata: &fs::Metadata) -> Option<FileIdentity> {
    if is_marked_unknown(metadata) {
        None
    } else {
        file_identity(metadata)
    }
}

fn no_date_when_marked(metadata: &fs::Metadata) -> Option<SystemTime> {
    if is_marked_unknown(metadata) {
        None
    } else {
        modification_time(metadata)
    }
}

#[test]
fn an_entry_whose_date_or_identity_is_unknown_is_held_back() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/parent/old", 1, 2 * HOUR);
    let unknown = fixture.file("data/parent/unknown", 1, 2 * HOUR);
    mark(&unknown, UNKNOWN_MARK);
    fixture.age(&fixture.path("data/parent"), 2 * HOUR);
    let rule = target_at(&root);
    for readers in [
        Readers {
            identity: unknown_when_marked,
            last_change: modification_time,
            ownership: marked_ownership,
            mount_root: okapi_platform::mount_root,
        },
        Readers {
            identity: file_identity,
            last_change: no_date_when_marked,
            ownership: marked_ownership,
            mount_root: okapi_platform::mount_root,
        },
    ] {
        let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
        run.readers = readers;
        let report = run.scan();
        assert_eq!(offered_paths(&report), [fixture.path("data/parent/old")]);
        assert_eq!(report.stats().unreadable(), 1);
    }
}

#[test]
fn roots_whose_identity_or_date_is_unknown_are_reported() {
    let fixture = Fixture::new();
    let root = fixture.directory("unknown-root");
    fixture.file("unknown-root/old", 1, 2 * HOUR);
    mark(&root, UNKNOWN_MARK);
    let file = fixture.file("errors.old", 1, 2 * HOUR);
    mark(&file, UNKNOWN_MARK);
    let directory_rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&directory_rule)]);
    run.readers.identity = unknown_when_marked;
    let report = run.scan();
    assert!(
        matches!(
            report.issues(),
            [ScanIssue::UnreadableRoot { path, error: std::io::ErrorKind::Unsupported, .. }] if *path == root
        ),
        "{:?}",
        report.issues()
    );
    let file_rule: SingleFileTarget = serde_json::from_str(&format!(
        r#"{{"path": "{}", "subcategory": "Errors"}}"#,
        file.display()
    ))
    .unwrap();
    let mut run = Run::new(&fixture, vec![RuleEntry::File(&file_rule)]);
    run.readers.last_change = no_date_when_marked;
    let report = run.scan();
    assert!(matches!(
        report.issues(),
        [ScanIssue::UnreadableRoot { path, error: std::io::ErrorKind::Unsupported, .. }] if *path == file
    ));
    assert_eq!(report.groups(), &[]);
}

#[test]
fn a_root_replaced_at_any_step_is_reported_by_the_scan() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/old", 1, 2 * HOUR);
    mark(&root, REPLACED_MARK);
    let rule = target_at(&root);
    // Swapped while the base is read twice, then while it is opened for the
    // walk (after the base's two reads and the roots' check).
    for reads_before_swap in [1, 3] {
        let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
        run.readers.identity = replaced_on_reopening;
        MARKED_READS.with(|reads| reads.set(0));
        READS_BEFORE_SWAP.with(|reads| reads.set(reads_before_swap));
        let report = run.scan();
        assert_eq!(report.groups(), &[], "{reads_before_swap}");
        assert!(
            matches!(
                report.issues(),
                [ScanIssue::UnsafeRoot { path, reason: crate::report::UnsafeReason::Replaced, origins }]
                    if *path == root && origin_label(origins) == "Temp"
            ),
            "{reads_before_swap}: {:?}",
            report.issues()
        );
    }
}

#[test]
fn an_app_rule_with_a_missing_first_path_still_scans_the_others() {
    let fixture = Fixture::new();
    let present = fixture.directory("present");
    fixture.file("present/old", 1, 2 * HOUR);
    let rule = app(&format!(
        r#"{{"id": "a", "name": "A", "paths": ["{}", "{}"]}}"#,
        fixture.path("missing").display(),
        present.display()
    ));
    let report = Run::new(
        &fixture,
        vec![RuleEntry::App {
            file: AppFile::Apps,
            rule: &rule,
        }],
    )
    .scan();
    assert_eq!(offered_paths(&report), [fixture.path("present/old")]);
    assert_eq!(report.issues(), &[]);
}

#[test]
fn a_rule_age_beyond_the_clock_says_nothing_for_an_absent_application() {
    let fixture = Fixture::new();
    let rule = app(&format!(
        r#"{{"id": "far", "name": "Far", "paths": ["{}"], "minAgeDays": 2}}"#,
        fixture.path("not-installed").display()
    ));
    let mut run = Run::new(
        &fixture,
        vec![RuleEntry::App {
            file: AppFile::Apps,
            rule: &rule,
        }],
    );
    let near_the_clock_origin = SystemTime::UNIX_EPOCH
        .checked_sub(Duration::from_secs(
            u64::try_from(i64::MAX).unwrap() - 100_000,
        ))
        .unwrap();
    run.options = ScanOptions::new(near_the_clock_origin, HOUR, Exclusions::none());
    assert_eq!(run.scan().issues(), &[]);
}

#[test]
fn a_settled_directory_weighs_and_dates_all_it_holds() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/outer/inner/f", 5, 3 * HOUR);
    fixture.file("data/outer/g", 7, 2 * HOUR);
    fixture.age(&fixture.path("data/outer/inner"), 4 * HOUR);
    fixture.age(&fixture.path("data/outer"), 5 * HOUR);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    let [item] = report.groups()[0].items() else {
        panic!("expected one entry");
    };
    assert_eq!(item.path(), fixture.path("data/outer"));
    assert_eq!(item.size(), 12);
    assert_eq!(
        item.last_change(),
        fixture.now - 2 * HOUR,
        "its most recent descendant"
    );
    assert_eq!(report.groups()[0].total_size(), 12);
}

#[test]
fn groups_of_apps_targets_and_files_carry_their_own_attributes() {
    let fixture = Fixture::new();
    let apps = fixture.directory("apps");
    let temp = fixture.directory("temp");
    fixture.file("apps/old", 1, 2 * HOUR);
    fixture.file("temp/old", 1, 2 * HOUR);
    let errors = fixture.file("errors.old", 1, 2 * HOUR);
    let app_rule = app(&format!(
        r#"{{"id": "a", "name": "A", "paths": ["{}"]}}"#,
        apps.display()
    ));
    let target_rule = target_at(&temp);
    let file_rule: SingleFileTarget = serde_json::from_str(&format!(
        r#"{{"path": "{}", "subcategory": "Errors"}}"#,
        errors.display()
    ))
    .unwrap();
    let report = Run::new(
        &fixture,
        vec![
            RuleEntry::Target(&target_rule),
            RuleEntry::File(&file_rule),
            RuleEntry::App {
                file: AppFile::Apps,
                rule: &app_rule,
            },
        ],
    )
    .scan();
    let described: Vec<_> = report
        .groups()
        .iter()
        .map(|group| {
            (
                group.label().to_string(),
                group.is_selected_by_default(),
                group.is_cache_reset(),
                group.needs_admin(),
            )
        })
        .collect();
    assert_eq!(
        described,
        [
            ("Temp".to_owned(), true, false, false),
            ("Errors".to_owned(), true, false, false),
            ("A".to_owned(), true, false, false),
        ]
    );
    assert!(matches!(
        report.groups()[1].origins(),
        [crate::report::Origin::SystemFile { .. }]
    ));
}

#[test]
fn a_single_file_at_the_cutoff_is_offered_and_one_after_is_not() {
    let fixture = Fixture::new();
    let at = fixture.file("at", 1, HOUR);
    let after = fixture.file(
        "after",
        1,
        HOUR.checked_sub(Duration::from_secs(1)).unwrap(),
    );
    let file = |path: &Path| -> SingleFileTarget {
        serde_json::from_str(&format!(
            r#"{{"path": "{}", "subcategory": "F"}}"#,
            path.display()
        ))
        .unwrap()
    };
    let targets = [file(&at), file(&after)];
    let report = Run::new(&fixture, targets.iter().map(RuleEntry::File).collect()).scan();
    assert_eq!(offered_paths(&report), [at]);
    assert_eq!(report.stats().recent(), 1);
}

#[test]
fn a_limit_is_reported_for_each_root_with_its_rules() {
    let fixture = Fixture::new();
    let first = fixture.directory("first");
    let second = fixture.directory("second");
    fixture.file("first/a/b/c/deep", 1, 2 * HOUR);
    fixture.file("second/a/b/c/deep", 1, 2 * HOUR);
    let rules = [target_at(&first), target_at(&second)];
    let mut run = Run::new(&fixture, rules.iter().map(RuleEntry::Target).collect());
    run.limits = Limits {
        depth: 2,
        entries: 1_000,
    };
    let report = run.scan();
    let mut roots: Vec<(PathBuf, String)> = report
        .issues()
        .iter()
        .map(|issue| match issue {
            ScanIssue::LimitReached {
                root,
                origins,
                limit: Limit::Depth,
            } => (root.clone(), origin_label(origins)),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    roots.sort();
    assert_eq!(
        roots,
        [(first, "Temp".to_owned()), (second, "Temp".to_owned())]
    );
}

#[test]
fn a_scan_without_every_variable_of_the_rules_does_not_start() {
    let fixture = Fixture::new();
    let variables = fixture.variables(&[
        (Variable::Home, "h"),
        (Variable::Cache, "c"),
        (Variable::TempDirectory, "t"),
    ]);
    let options = ScanOptions::new(fixture.now, HOUR, Exclusions::none());
    let rules = okapi_rules::RuleSet::embedded(okapi_rules::Os::Linux).unwrap();
    let report = crate::scan(&rules, &variables, &options);
    assert_eq!(
        report.issues(),
        [
            ScanIssue::Stopped(Stop::MissingVariable {
                variable: Variable::Config
            }),
            ScanIssue::Stopped(Stop::MissingVariable {
                variable: Variable::LocalShare
            }),
        ]
    );
    assert!(report.issues().iter().all(ScanIssue::stops_the_scan));
    assert_eq!(report.groups(), &[]);
}

#[test]
fn a_single_file_replaced_between_its_reads_is_reported() {
    let fixture = Fixture::new();
    let file = fixture.file("errors.old", 1, 2 * HOUR);
    mark(&file, REPLACED_MARK);
    let rule: SingleFileTarget = serde_json::from_str(&format!(
        r#"{{"path": "{}", "subcategory": "Errors"}}"#,
        file.display()
    ))
    .unwrap();
    let mut run = Run::new(&fixture, vec![RuleEntry::File(&rule)]);
    run.readers.identity = replaced_on_reopening;
    MARKED_READS.with(|reads| reads.set(0));
    READS_BEFORE_SWAP.with(|reads| reads.set(1));
    let report = run.scan();
    assert_eq!(report.groups(), &[]);
    assert!(
        matches!(
            report.issues(),
            [ScanIssue::UnsafeRoot { path, reason: crate::report::UnsafeReason::Replaced, .. }] if *path == file
        ),
        "{:?}",
        report.issues()
    );
}

#[test]
fn a_system_target_keeps_what_is_younger_than_its_own_age() {
    let fixture = Fixture::new();
    let root = fixture.directory("temp");
    fixture.file("temp/three-days", 1, 72 * HOUR);
    fixture.file("temp/one-day", 1, 24 * HOUR);
    let rule = target(&format!(
        r#"{{"path": "{}", "subcategory": "Temp", "minAgeDays": 2}}"#,
        root.display()
    ));
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [fixture.path("temp/three-days")]);
    assert_eq!(report.stats().recent(), 1);
}

#[test]
fn the_policy_holds_back_mount_roots_links_and_recent_files() {
    use crate::walk::{Accepted, HoldBack, Observed, WalkContext};
    use okapi_platform::EntryType;

    let protected =
        crate::protection::ProtectedRoots::new(&TemplateVariables::new(), file_identity).unwrap();
    let misc = no_protected_names();
    let file = FileIdentity::new(7, 2);
    let root = FileIdentity::new(7, 1);
    let roots = std::collections::BTreeSet::from([root]);
    let now = SystemTime::now();
    let context = WalkContext {
        cutoff: now,
        exclusions: &Exclusions::none(),
        misc: &misc,
        protected: &protected,
        roots: &roots,
        readers: Readers::SYSTEM,
        limits: Limits::DEFAULT,
        remover: None,
    };
    let observed = |kind, identity, last_change, mount_root| Observed {
        kind,
        identity: Some(identity),
        last_change: Some(last_change),
        mount_root,
    };
    let old = now - HOUR;
    assert_eq!(
        context.by_status(&observed(EntryType::File, file, old, Some(false)), 7, None),
        Ok(Accepted {
            identity: file,
            last_change: old
        })
    );
    for (observed, expected) in [
        (
            observed(EntryType::File, file, old, Some(true)),
            HoldBack::OtherFilesystem,
        ),
        (
            observed(EntryType::File, file, old, None),
            HoldBack::Unreadable,
        ),
        (observed(EntryType::Link, file, old, None), HoldBack::Link),
        (
            observed(EntryType::Other, file, old, None),
            HoldBack::Special,
        ),
        (
            observed(EntryType::File, root, old, Some(false)),
            HoldBack::Reserved,
        ),
        (
            observed(EntryType::File, file, now + HOUR, Some(false)),
            HoldBack::Recent,
        ),
        // A recent file on another device counts as another device.
        (
            observed(
                EntryType::File,
                FileIdentity::new(8, 2),
                now + HOUR,
                Some(false),
            ),
            HoldBack::OtherFilesystem,
        ),
    ] {
        assert_eq!(
            context.by_status(&observed, 7, None),
            Err(expected),
            "{observed:?}"
        );
    }
    assert!(
        context
            .by_status(
                &observed(EntryType::File, root, old, Some(false)),
                7,
                Some(root)
            )
            .is_ok(),
        "a single file target is not another root to itself"
    );
}

#[test]
fn the_policy_holds_back_an_excluded_directory_by_identity() {
    use crate::walk::{HoldBack, Observed, WalkContext};
    use okapi_platform::EntryType;

    let fixture = Fixture::new();
    let kept = fixture.directory("kept");
    let exclusions = Exclusions::new([kept.clone()]).unwrap();
    let protected =
        crate::protection::ProtectedRoots::new(&TemplateVariables::new(), file_identity).unwrap();
    let misc = no_protected_names();
    let roots = std::collections::BTreeSet::new();
    let context = WalkContext {
        cutoff: fixture.now,
        exclusions: &exclusions,
        misc: &misc,
        protected: &protected,
        roots: &roots,
        readers: Readers::SYSTEM,
        limits: Limits::DEFAULT,
        remover: None,
    };
    // Met at another path (a bind mount shows it elsewhere): the same object.
    let identity = file_identity(&fs::symlink_metadata(&kept).unwrap()).unwrap();
    let observed = Observed {
        kind: EntryType::Directory,
        identity: Some(identity),
        last_change: Some(fixture.now - HOUR),
        mount_root: Some(false),
    };
    assert_eq!(context.by_path(&fixture.path("elsewhere")), None);
    assert_eq!(
        context.by_status(&observed, identity.device(), None),
        Err(HoldBack::Excluded)
    );
}

#[test]
fn the_policy_holds_back_a_variable_directory_and_what_cannot_be_read() {
    use crate::walk::{HoldBack, Observed, WalkContext};
    use okapi_platform::EntryType;

    let fixture = Fixture::new();
    let home = fixture.directory("home");
    let variables = fixture.variables(&[(Variable::Home, "home")]);
    let protected = crate::protection::ProtectedRoots::new(&variables, file_identity).unwrap();
    let misc = no_protected_names();
    let roots = std::collections::BTreeSet::new();
    let context = WalkContext {
        cutoff: fixture.now,
        exclusions: &Exclusions::none(),
        misc: &misc,
        protected: &protected,
        roots: &roots,
        readers: Readers::SYSTEM,
        limits: Limits::DEFAULT,
        remover: None,
    };
    let identity = file_identity(&fs::symlink_metadata(&home).unwrap()).unwrap();
    let observed = Observed {
        kind: EntryType::Directory,
        identity: Some(identity),
        last_change: Some(fixture.now),
        mount_root: Some(false),
    };
    assert_eq!(
        context.by_status(&observed, identity.device(), None),
        Err(HoldBack::Reserved)
    );
    let unknown = Observed {
        kind: EntryType::File,
        identity: None,
        last_change: Some(fixture.now - HOUR),
        mount_root: Some(false),
    };
    assert_eq!(
        context.by_status(&unknown, identity.device(), None),
        Err(HoldBack::Unreadable)
    );
    let undated = Observed {
        identity: Some(FileIdentity::new(identity.device(), 2)),
        last_change: None,
        ..unknown
    };
    assert_eq!(
        context.by_status(&undated, identity.device(), None),
        Err(HoldBack::Unreadable)
    );
    assert!(
        context
            .by_status(
                &Observed {
                    kind: EntryType::File,
                    identity: Some(FileIdentity::new(identity.device(), 2)),
                    last_change: Some(fixture.now),
                    mount_root: Some(false),
                },
                identity.device(),
                None
            )
            .is_ok(),
        "at the cutoff exactly is old enough"
    );
}

/// A root `tmp` holding `mine` (the test user's), `theirs` (another user's)
/// and `their-dir/mine` (another user's directory), all old, the root marked
/// with `root_mark` if any.
fn shared_temporary(fixture: &Fixture, root_mark: Option<Duration>) -> PathBuf {
    let root = fixture.directory("tmp");
    fixture.file("tmp/mine", 1, 2 * HOUR);
    let theirs = fixture.file("tmp/theirs", 1, 2 * HOUR);
    mark(&theirs, FOREIGN_MARK);
    fixture.file("tmp/their-dir/mine", 1, 2 * HOUR);
    mark(&fixture.path("tmp/their-dir"), FOREIGN_MARK);
    if let Some(root_mark) = root_mark {
        mark(&root, root_mark);
    }
    root
}

#[test]
fn another_users_entries_in_a_sticky_directory_are_held_back() {
    let fixture = Fixture::new();
    let root = shared_temporary(&fixture, Some(STICKY_MARK));
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [fixture.path("tmp/mine")]);
    assert_eq!(report.stats().not_owned(), 2, "theirs and their directory");
}

#[test]
fn without_a_sticky_bit_or_a_known_remover_nothing_more_is_held_back() {
    let everything = |fixture: &Fixture| {
        vec![
            fixture.path("tmp/mine"),
            fixture.path("tmp/their-dir"),
            fixture.path("tmp/theirs"),
        ]
    };
    let fixture = Fixture::new();
    let root = shared_temporary(&fixture, None);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), everything(&fixture));
    assert_eq!(report.stats().not_owned(), 0);

    let fixture = Fixture::new();
    let root = shared_temporary(&fixture, Some(STICKY_MARK));
    let rule = target_at(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.user = None;
    assert_eq!(offered_paths(&run.scan()), everything(&fixture));

    let fixture = Fixture::new();
    let root = shared_temporary(&fixture, Some(OWN_STICKY_MARK));
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(
        offered_paths(&report),
        everything(&fixture),
        "the remover owns the sticky directory"
    );
}

#[test]
fn only_the_directory_listed_counts() {
    // A sticky directory nested in an ordinary root.
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/shared/mine", 1, 2 * HOUR);
    let theirs = fixture.file("data/shared/theirs", 1, 2 * HOUR);
    mark(&theirs, FOREIGN_MARK);
    mark(&fixture.path("data/shared"), STICKY_MARK);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/shared/mine")]);
    assert_eq!(report.stats().not_owned(), 1);

    // An ordinary directory of the remover's in a sticky root: what it holds
    // is the remover's to remove, another user's file included.
    let fixture = Fixture::new();
    let root = fixture.directory("tmp");
    let theirs = fixture.file("tmp/sub/theirs", 1, 2 * HOUR);
    mark(&theirs, FOREIGN_MARK);
    fixture.age(&fixture.path("tmp/sub"), 2 * HOUR);
    mark(&root, STICKY_MARK);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [fixture.path("tmp/sub")]);
}

#[test]
fn what_needs_the_administrator_is_offered_for_root_to_remove() {
    let fixture = Fixture::new();
    let root = shared_temporary(&fixture, Some(STICKY_MARK));
    let rule = target(&format!(
        r#"{{"path": "{}", "subcategory": "Crash", "needsAdmin": true}}"#,
        root.display()
    ));
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report).len(), 3);
    assert_eq!(report.stats().not_owned(), 0);
}

#[test]
fn a_session_name_keeps_its_own_count_in_a_sticky_directory() {
    let fixture = Fixture::new();
    let root = fixture.directory("tmp");
    fixture.directory("tmp/.X11-unix");
    mark(&fixture.path("tmp/.X11-unix"), FOREIGN_MARK);
    mark(&root, STICKY_MARK);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(report.stats().protected_names(), 1);
    assert_eq!(report.stats().not_owned(), 0);
}

#[test]
fn an_unreadable_owner_holds_back_only_in_a_sticky_directory() {
    let fixture = Fixture::new();
    let root = fixture.directory("tmp");
    fixture.file("tmp/mine", 1, 2 * HOUR);
    let unknown = fixture.file("tmp/unknown", 1, 2 * HOUR);
    mark(&unknown, UNKNOWN_OWNER_MARK);
    mark(&root, STICKY_MARK);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [fixture.path("tmp/mine")]);
    assert_eq!(report.stats().not_owned(), 1);

    // In an ordinary directory, an unreadable owner holds nothing back.
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    let unknown = fixture.file("data/unknown", 1, 2 * HOUR);
    mark(&unknown, UNKNOWN_OWNER_MARK);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [unknown]);

    // The listed directory's own owner unreadable: nothing more is held back.
    let fixture = Fixture::new();
    let root = shared_temporary(&fixture, Some(UNKNOWN_OWNER_MARK));
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report).len(), 3);
}
