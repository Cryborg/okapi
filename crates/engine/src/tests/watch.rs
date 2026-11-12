//! Progress and cancellation of the scan and the cleaning. A cancellation
//! "during" the work is made at an exact point: by a metadata reader the walk
//! calls (scan), or by the test hook of the watch (cleaning).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use super::clean::{clean, file_and_directory, ids, make_recent, run_later};
use super::*;
use crate::clean::{CleanPass, CleanResult, KeepReason};
use crate::report::Category;
use crate::report::ItemId;
use crate::watch::{Cancel, Progress, Watch};

thread_local! {
    /// The token the reader below cancels, and after how many reads.
    static CANCEL_AT: Cell<Option<usize>> = const { Cell::new(None) };
    static READS: Cell<usize> = const { Cell::new(0) };
    static TOKEN: std::cell::RefCell<Option<Cancel>> = const { std::cell::RefCell::new(None) };
}

/// Mount status read as the real one, counting the reads and cancelling the
/// thread's token at the read set by `CANCEL_AT`.
fn counting_mount_root(path: &Path) -> std::io::Result<Option<bool>> {
    let reads = READS.with(|reads| {
        reads.set(reads.get() + 1);
        reads.get()
    });
    if CANCEL_AT.with(Cell::get) == Some(reads) {
        TOKEN.with(|token| token.borrow().as_ref().unwrap().cancel());
    }
    okapi_platform::mount_root(path)
}

fn cancelling_after(reads: usize) -> Cancel {
    let cancel = Cancel::new();
    READS.with(|count| count.set(0));
    CANCEL_AT.with(|at| at.set(Some(reads)));
    TOKEN.with(|token| *token.borrow_mut() = Some(cancel.clone()));
    cancel
}

/// Ten old files in one root.
fn ten_files(fixture: &Fixture) -> CleanTarget {
    let root = fixture.directory("data");
    for index in 0..10 {
        fixture.file(&format!("data/file-{index}"), 1, 2 * HOUR);
    }
    target_at(&root)
}

fn is_cancelled(report: &ScanReport) -> bool {
    report
        .issues()
        .contains(&ScanIssue::Stopped(crate::Stop::Cancelled))
}

/// The identifier of the entry offered at `path`: never by position, which
/// follows the order a directory lists its entries in.
fn id_at(report: &ScanReport, path: &Path) -> ItemId {
    report
        .groups()
        .iter()
        .flat_map(ScanGroup::items)
        .find(|item| item.path() == path)
        .map(crate::ScanItem::id)
        .unwrap()
}

/// Scans with `run`, its progress gathered.
fn scan_watching(mut run: Run<'_>, cancel: Cancel) -> (ScanReport, Vec<Progress>) {
    let (sender, receiver) = mpsc::channel();
    run.watch = Watch::new(cancel, Some(sender));
    let report = run.scan();
    drop(run);
    (report, receiver.iter().collect())
}

fn roots_done(progress: &[Progress]) -> Vec<(BTreeSet<Category>, usize, u64, usize)> {
    progress
        .iter()
        .filter_map(|progress| match progress {
            Progress::RootDone {
                categories,
                entries,
                bytes,
                issues,
            } => Some((categories.clone(), *entries, *bytes, *issues)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_scan_cancelled_beforehand_reads_nothing_and_offers_nothing() {
    let fixture = Fixture::new();
    let rule = ten_files(&fixture);
    let cancel = cancelling_after(usize::MAX);
    cancel.cancel();
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.mount_root = counting_mount_root;
    let (report, progress) = scan_watching(run, cancel);
    assert!(is_cancelled(&report));
    assert_eq!(report.groups(), &[]);
    assert_eq!(READS.with(Cell::get), 0);
    assert_eq!(*report.stats(), crate::ScanStats::default());
    assert_eq!(progress, [], "not even planned");
}

#[test]
fn a_scan_cancelled_while_walking_reads_nothing_more_and_offers_nothing() {
    let fixture = Fixture::new();
    let first = ten_files(&fixture);
    let second_root = fixture.directory("second");
    for index in 0..10 {
        fixture.file(&format!("second/file-{index}"), 1, 2 * HOUR);
    }
    let second = target_at(&second_root);
    // The first root read whole (10 reads), then 3 reads into the second.
    let cancel = cancelling_after(13);
    let mut run = Run::new(
        &fixture,
        vec![RuleEntry::Target(&first), RuleEntry::Target(&second)],
    );
    run.readers.mount_root = counting_mount_root;
    let (report, progress) = scan_watching(run, cancel);
    assert!(is_cancelled(&report));
    assert_eq!(
        report.groups(),
        &[],
        "nothing of a scan cut short, not even its roots done"
    );
    assert_eq!(
        READS.with(Cell::get),
        13,
        "not one read after the cancellation"
    );
    assert_eq!(
        roots_done(&progress),
        [(BTreeSet::from([Category::System]), 10, 10, 0)],
        "the first root is done, the one cut short is not"
    );
}

#[test]
fn a_scan_reports_its_roots_by_category_and_what_each_found() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    if reads_through_permissions(&fixture) {
        return;
    }
    let first = ten_files(&fixture);
    let other = fixture.directory("other");
    fixture.file("other/seven", 7, 2 * HOUR);
    let locked = fixture.directory("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let errors = fixture.file("home/.xsession-errors.old", 5, 2 * HOUR);
    let cache = fixture.directory("cache");
    fixture.file("cache/old", 4, 2 * HOUR);
    let (other_rule, locked_rule) = (target_at(&other), target_at(&locked));
    let file: SingleFileTarget = serde_json::from_str(&format!(
        r#"{{"path": "{}", "subcategory": "Errors"}}"#,
        errors.display()
    ))
    .unwrap();
    let app = app(&format!(
        r#"{{"id": "cache", "name": "Cache", "paths": ["{}"]}}"#,
        cache.display()
    ));
    let run = Run::new(
        &fixture,
        vec![
            RuleEntry::Target(&first),
            RuleEntry::Target(&other_rule),
            RuleEntry::Target(&locked_rule),
            RuleEntry::File(&file),
            RuleEntry::App {
                file: AppFile::Apps,
                rule: &app,
            },
        ],
    );
    let (report, progress) = scan_watching(run, Cancel::new());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        progress[0],
        Progress::Planned {
            roots: BTreeMap::from([(Category::System, 4), (Category::Apps, 1)]),
            total: 5,
        }
    );
    let system = || BTreeSet::from([Category::System]);
    assert_eq!(
        roots_done(&progress),
        [
            (system(), 10, 10, 0),
            (system(), 1, 7, 0),
            (system(), 0, 0, 1),
            (system(), 1, 5, 1),
            (BTreeSet::from([Category::Apps]), 1, 4, 1),
        ],
        "each root, the unreadable one included, with the issues so far"
    );
    let offered: u64 = report.groups().iter().map(ScanGroup::total_size).sum();
    assert_eq!(offered, 26, "the sum over roots");
}

#[test]
fn a_root_shared_by_two_categories_counts_for_both() {
    let fixture = Fixture::new();
    let shared = fixture.directory("shared");
    fixture.file("shared/old", 3, 2 * HOUR);
    let system = target_at(&shared);
    let app = app(&format!(
        r#"{{"id": "shared", "name": "Shared", "paths": ["{}"]}}"#,
        shared.display()
    ));
    let run = Run::new(
        &fixture,
        vec![
            RuleEntry::Target(&system),
            RuleEntry::App {
                file: AppFile::Gaming,
                rule: &app,
            },
        ],
    );
    let (_, progress) = scan_watching(run, Cancel::new());
    let both = BTreeSet::from([Category::System, Category::Gaming]);
    assert_eq!(
        progress[0],
        Progress::Planned {
            roots: BTreeMap::from([(Category::System, 1), (Category::Gaming, 1)]),
            total: 1,
        }
    );
    assert_eq!(roots_done(&progress), [(both, 1, 3, 0)], "walked once");
}

#[test]
fn a_scan_with_no_root_is_planned_empty() {
    let fixture = Fixture::new();
    let rule = target_at(&fixture.path("absent"));
    let run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    let (_, progress) = scan_watching(run, Cancel::new());
    assert_eq!(
        progress,
        [Progress::Planned {
            roots: BTreeMap::new(),
            total: 0,
        }]
    );
}

#[test]
fn a_long_scan_says_where_it_is_across_its_roots() {
    let fixture = Fixture::new();
    let errors = fixture.file("home/.xsession-errors.old", 5, 2 * HOUR);
    let half = usize::try_from(crate::watch::PROGRESS_EVERY).unwrap() / 2 + 88;
    let mut rules = Vec::new();
    for root in ["first", "second"] {
        for index in 0..half {
            fixture.file(&format!("{root}/file-{index}"), 1, 2 * HOUR);
        }
        rules.push(target_at(&fixture.path(root)));
    }
    // Reached but held back: counted as seen, never as found.
    fixture.file("first/recent", 1, Duration::ZERO);
    std::os::unix::fs::symlink(fixture.path("first/file-0"), fixture.path("first/link")).unwrap();
    let held_back = 2;
    let file: SingleFileTarget = serde_json::from_str(&format!(
        r#"{{"path": "{}", "subcategory": "Errors"}}"#,
        errors.display()
    ))
    .unwrap();
    // A rule naming a link: an issue before any walk.
    std::os::unix::fs::symlink(fixture.path("first"), fixture.path("linked")).unwrap();
    let linked = target_at(&fixture.path("linked"));
    let mut entries = vec![RuleEntry::Target(&linked), RuleEntry::File(&file)];
    entries.extend(rules.iter().map(RuleEntry::Target));
    let (_, progress) = scan_watching(Run::new(&fixture, entries), Cancel::new());
    let walking: Vec<_> = progress
        .iter()
        .filter_map(|progress| match progress {
            Progress::Walking {
                path,
                entries_seen,
                found_bytes,
                issues,
            } => Some((path.clone(), *entries_seen, *found_bytes, *issues)),
            _ => None,
        })
        .collect();
    assert_eq!(
        walking.len(),
        1,
        "every {} entries",
        crate::watch::PROGRESS_EVERY
    );
    let (path, entries_seen, found_bytes, issues) = &walking[0];
    assert_eq!(
        path.parent(),
        Some(fixture.path("second").as_path()),
        "the entry reached, counted across roots: {}",
        path.display()
    );
    assert!(
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("file-")
    );
    assert_eq!(*entries_seen, crate::watch::PROGRESS_EVERY);
    // The file target, then every file accepted before this entry.
    assert_eq!(
        *found_bytes,
        5 + crate::watch::PROGRESS_EVERY - 1 - held_back
    );
    assert_eq!(*issues, 1, "the linked rule's");
}

#[test]
fn a_receiver_gone_away_stops_nothing() {
    let fixture = Fixture::new();
    let rule = ten_files(&fixture);
    let (sender, receiver) = mpsc::channel();
    drop(receiver);
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.watch = Watch::new(Cancel::new(), Some(sender));
    let report = run.scan();
    assert!(!is_cancelled(&report));
    assert_eq!(offered_paths(&report).len(), 10);
}

/// Asked for, in this order: an identifier of another report, `data/dir`
/// made recent after the scan, and the old `data/file`.
fn three_kinds<'a>(fixture: &Fixture, rule: &'a CleanTarget) -> (Run<'a>, ScanReport, Vec<ItemId>) {
    let run = run_later(fixture, vec![RuleEntry::Target(rule)]);
    let other = run.scan();
    let report = run.scan();
    make_recent(fixture, &fixture.path("data/dir/a"));
    let asked = vec![
        ids(&other)[0],
        id_at(&report, &fixture.path("data/dir")),
        id_at(&report, &fixture.path("data/file")),
    ];
    (run, report, asked)
}

/// `data/dir` (23 bytes) then `data/file` (3 bytes).
fn dir_then_file(fixture: &Fixture, report: &ScanReport) -> Vec<ItemId> {
    vec![
        id_at(report, &fixture.path("data/dir")),
        id_at(report, &fixture.path("data/file")),
    ]
}

fn results_in_order(receipt: &crate::CleanReceipt) -> Vec<CleanResult> {
    receipt
        .outcomes()
        .iter()
        .map(crate::CleanOutcome::result)
        .collect()
}

#[test]
fn a_cleaning_cancelled_beforehand_removes_nothing_and_says_why() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let (mut run, report, asked) = three_kinds(&fixture, &rule);
    let cancel = Cancel::new();
    cancel.cancel();
    run.watch = Watch::new(cancel, None);
    let receipt = clean(&run, &report, &asked);
    assert_eq!(
        results_in_order(&receipt),
        [
            CleanResult::Kept(KeepReason::UnknownItem),
            CleanResult::Kept(KeepReason::Cancelled),
            CleanResult::Kept(KeepReason::Cancelled),
        ]
    );
    assert_eq!(receipt.removed_bytes(), 0);
    assert!(fixture.path("data/file").exists());
}

#[test]
fn a_cleaning_cancelled_after_its_checks_keeps_each_reason_and_removes_nothing() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let (mut run, report, asked) = three_kinds(&fixture, &rule);
    let mut watch = Watch::new(Cancel::new(), None);
    watch.hook = Some(Box::new(|progress, cancel| {
        if let Progress::Cleaning {
            pass: CleanPass::Check,
            done,
            total,
            ..
        } = progress
            && done == total
        {
            cancel.cancel();
        }
    }));
    run.watch = watch;
    let receipt = clean(&run, &report, &asked);
    assert_eq!(
        results_in_order(&receipt),
        [
            CleanResult::Kept(KeepReason::UnknownItem),
            CleanResult::Kept(KeepReason::Recent),
            CleanResult::Kept(KeepReason::Cancelled),
        ]
    );
    assert_eq!(receipt.removed_bytes(), 0);
    assert!(fixture.path("data/file").exists(), "ready, not removed");
}

#[test]
fn a_cleaning_cancelled_after_one_removal_keeps_the_rest_whole() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    let mut watch = Watch::new(Cancel::new(), None);
    watch.hook = Some(Box::new(|progress, cancel| {
        if let Progress::Cleaning {
            pass: CleanPass::Remove,
            done: 1,
            ..
        } = progress
        {
            cancel.cancel();
        }
    }));
    run.watch = watch;
    let receipt = clean(&run, &report, &dir_then_file(&fixture, &report));
    assert_eq!(
        results_in_order(&receipt),
        [
            CleanResult::Removed { bytes: 23 },
            CleanResult::Kept(KeepReason::Cancelled),
        ]
    );
    assert_eq!(receipt.removed_bytes(), 23);
    assert!(!fixture.path("data/dir").exists());
    assert!(fixture.path("data/file").exists());
}

#[test]
fn a_cleaning_says_each_entry_of_each_pass_once() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let other = run.scan();
    let report = run.scan();
    let [dir, file] = dir_then_file(&fixture, &report)[..] else {
        unreachable!()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let mut watch = Watch::new(Cancel::new(), None);
    watch.hook = Some(Box::new(move |progress, _| {
        if let Progress::Cleaning {
            pass,
            done,
            total,
            removed_bytes,
        } = progress
        {
            recorded
                .lock()
                .unwrap()
                .push((*pass, *done, *total, *removed_bytes));
        }
    }));
    run.watch = watch;
    let receipt = clean(&run, &report, &[ids(&other)[0], dir, dir, file]);
    assert_eq!(
        *seen.lock().unwrap(),
        [
            (CleanPass::Check, 1, 3, 0),
            (CleanPass::Check, 2, 3, 0),
            (CleanPass::Check, 3, 3, 0),
            (CleanPass::Remove, 1, 3, 0),
            (CleanPass::Remove, 2, 3, 23),
            (CleanPass::Remove, 3, 3, 26),
        ],
        "each identifier once, the foreign one included"
    );
    assert_eq!(receipt.removed_bytes(), 26);
}

#[test]
fn a_withheld_root_is_not_counted_as_planned() {
    let fixture = Fixture::new();
    let rule = expansion_with(&fixture, "apps/bound");
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers.mount_root = mount_root_when_marked;
    let (_, progress) = scan_watching(run, Cancel::new());
    assert_eq!(
        progress[0],
        Progress::Planned {
            roots: BTreeMap::from([(Category::System, 1)]),
            total: 1,
        },
        "the bound root is withheld, only the local one is walked"
    );
    assert_eq!(roots_done(&progress).len(), 1);
}

/// A rule of `file` on `path`, a cache reset when `reset`.
fn app_on(file: AppFile, id: &str, path: &Path, reset: bool) -> (AppFile, AppCacheRule) {
    (
        file,
        app(&format!(
            r#"{{"id": "{id}", "name": "{id}", "paths": ["{}"], "cacheReset": {reset}}}"#,
            path.display()
        )),
    )
}

fn entries(rules: &[(AppFile, AppCacheRule)]) -> Vec<RuleEntry<'_>> {
    rules
        .iter()
        .map(|(file, rule)| RuleEntry::App { file: *file, rule })
        .collect()
}

#[test]
fn a_scan_walks_only_the_chosen_categories_keeping_the_attributes_of_all() {
    let fixture = Fixture::new();
    let shared = fixture.directory("shared");
    fixture.file("shared/old", 3, 2 * HOUR);
    let gpu_only = fixture.directory("gpu-only");
    fixture.file("gpu-only/old", 5, 2 * HOUR);
    let rules = [
        app_on(AppFile::Apps, "wide", &shared, false),
        app_on(AppFile::GpuCache, "reset", &shared, true),
        app_on(AppFile::GpuCache, "alone", &gpu_only, true),
    ];
    let mut run = Run::new(&fixture, entries(&rules));
    run.options = run
        .options
        .clone()
        .with_categories(BTreeSet::from([Category::Apps]));
    let (report, progress) = scan_watching(run, Cancel::new());
    assert_eq!(
        progress[0],
        Progress::Planned {
            roots: BTreeMap::from([(Category::Apps, 1), (Category::Gpu, 1)]),
            total: 1,
        },
        "the shared root counts for both, the GPU one is not walked"
    );
    assert_eq!(roots_done(&progress).len(), 1);
    let [group] = report.groups() else {
        panic!("expected the shared group: {:?}", report.groups());
    };
    assert_eq!(group.total_size(), 3);
    assert!(
        !group.is_selected_by_default(),
        "a cache reset for a rule left out still (invariant 5)"
    );

    let (everything, progress) = scan_watching(Run::new(&fixture, entries(&rules)), Cancel::new());
    assert_eq!(everything.groups().len(), 2, "none chosen: all");
    let Progress::Planned { total, .. } = progress[0] else {
        panic!("expected the plan first");
    };
    assert_eq!(total, 2);
    assert_eq!(roots_done(&progress).len(), total);
}

#[test]
fn a_root_left_out_below_a_chosen_one_is_still_reserved() {
    let fixture = Fixture::new();
    let outer = fixture.directory("outer");
    fixture.file("outer/old", 3, 2 * HOUR);
    let inner = fixture.directory("outer/inner");
    fixture.file("outer/inner/old", 5, 2 * HOUR);
    let rules = [
        app_on(AppFile::Apps, "outer", &outer, false),
        app_on(AppFile::GpuCache, "inner", &inner, true),
    ];
    let mut run = Run::new(&fixture, entries(&rules));
    run.options = run
        .options
        .clone()
        .with_categories(BTreeSet::from([Category::Apps]));
    let (report, _) = scan_watching(run, Cancel::new());
    assert_eq!(
        offered_paths(&report),
        [fixture.path("outer/old")],
        "the reset's root is neither offered by its rule nor by the outer one"
    );
}

#[test]
fn a_chosen_root_below_one_left_out_is_walked_alone() {
    let fixture = Fixture::new();
    let outer = fixture.directory("outer");
    fixture.file("outer/old", 3, 2 * HOUR);
    let inner = fixture.directory("outer/inner");
    fixture.file("outer/inner/old", 5, 2 * HOUR);
    let rules = [
        app_on(AppFile::GpuCache, "outer", &outer, true),
        app_on(AppFile::Apps, "inner", &inner, false),
    ];
    let mut run = Run::new(&fixture, entries(&rules));
    run.options = run
        .options
        .clone()
        .with_categories(BTreeSet::from([Category::Apps]));
    let (report, _) = scan_watching(run, Cancel::new());
    assert_eq!(
        offered_paths(&report),
        [fixture.path("outer/inner/old")],
        "the outer root left out is not walked, the inner one offered once"
    );
}
