//! End-to-end tests of the cleaning: a real scan of a temporary directory,
//! changes made to it, then the cleaning.
//!
//! A test cannot set a status change time back, and cleaning reads it: the
//! scan and the cleaning run "two hours from now" with a one hour minimum
//! age, so that whatever the test just wrote counts as old. An entry made
//! recent has its modification time set past that cutoff.

use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;
use crate::clean::{CleanReceipt, CleanResult, KeepReason, clean_items};
use crate::report::ItemId;

/// The instant the scan and the cleaning take as now.
pub(super) fn later(fixture: &Fixture) -> SystemTime {
    fixture.now + 2 * HOUR
}

pub(super) fn options_later(
    fixture: &Fixture,
    min_age: Duration,
    exclusions: Exclusions,
) -> ScanOptions {
    ScanOptions::new(later(fixture), min_age, exclusions)
}

/// Makes `path` changed after the cleaning's cutoff.
pub(super) fn make_recent(fixture: &Fixture, path: &Path) {
    File::open(path)
        .unwrap()
        .set_modified(later(fixture))
        .unwrap();
}

pub(super) fn run_later<'a>(fixture: &Fixture, entries: Vec<RuleEntry<'a>>) -> Run<'a> {
    let mut run = Run::new(fixture, entries);
    run.options = options_later(fixture, HOUR, Exclusions::none());
    run
}

pub(super) fn ids(report: &ScanReport) -> Vec<ItemId> {
    report
        .groups()
        .iter()
        .flat_map(ScanGroup::items)
        .map(crate::ScanItem::id)
        .collect()
}

pub(super) fn clean(run: &Run<'_>, report: &ScanReport, ids: &[ItemId]) -> CleanReceipt {
    clean_items(
        report,
        ids,
        &run.misc,
        &run.variables,
        &run.options,
        run.limits,
        &run.watch,
    )
}

fn results(receipt: &CleanReceipt) -> Vec<(PathBuf, CleanResult)> {
    let mut results: Vec<_> = receipt
        .outcomes()
        .iter()
        .map(|outcome| (outcome.path().unwrap().to_path_buf(), outcome.result()))
        .collect();
    results.sort_by(|first, second| first.0.cmp(&second.0));
    results
}

/// A root `data` holding `data/file` and the settled directory `data/dir`.
pub(super) fn file_and_directory(fixture: &Fixture) -> (PathBuf, CleanTarget) {
    let root = fixture.directory("data");
    fixture.file("data/file", 3, 2 * HOUR);
    fixture.file("data/dir/a", 5, 2 * HOUR);
    fixture.file("data/dir/inner/b", 7, 2 * HOUR);
    fixture.file("data/dir/second/c", 11, 2 * HOUR);
    fixture.age(&fixture.path("data/dir/inner"), 2 * HOUR);
    fixture.age(&fixture.path("data/dir/second"), 2 * HOUR);
    fixture.age(&fixture.path("data/dir"), 2 * HOUR);
    let rule = target_at(&root);
    (root, rule)
}

#[test]
fn offered_files_and_directories_are_removed_and_weighed() {
    let fixture = Fixture::new();
    let (root, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [
            (fixture.path("data/dir"), CleanResult::Removed { bytes: 23 }),
            (fixture.path("data/file"), CleanResult::Removed { bytes: 3 }),
        ]
    );
    assert_eq!(receipt.removed_bytes(), 26);
    assert!(receipt.is_complete());
    assert_eq!(receipt.stops(), &[]);
    assert!(!fixture.path("data/dir").exists());
    assert!(!fixture.path("data/file").exists());
    assert!(root.exists(), "the root itself stays");
}

#[test]
fn an_identifier_of_another_report_touches_nothing_and_one_asked_twice_counts_once() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let first = run.scan();
    let second = run.scan();
    let receipt = clean(&run, &second, &ids(&first));
    assert!(receipt.outcomes().iter().all(|outcome| outcome.result()
        == CleanResult::Kept(KeepReason::UnknownItem)
        && outcome.path().is_none()));
    assert!(fixture.path("data/file").exists());
    assert!(fixture.path("data/dir").exists());

    let id = ids(&second)[0];
    let receipt = clean(&run, &second, &[id, id]);
    assert_eq!(receipt.outcomes().len(), 1);
}

#[test]
fn an_entry_already_gone_is_gone_not_failed() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    fs::remove_file(fixture.path("data/file")).unwrap();
    fs::remove_dir_all(fixture.path("data")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert!(
        receipt
            .outcomes()
            .iter()
            .all(|outcome| outcome.result() == CleanResult::Gone),
        "{receipt:?}"
    );
}

#[test]
fn a_file_replaced_since_the_scan_stays() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    // A new file renamed over the old one: another inode, same path.
    let replacement = fixture.file("replacement", 3, 2 * HOUR);
    fs::rename(&replacement, fixture.path("data/file")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert!(results(&receipt).contains(&(
        fixture.path("data/file"),
        CleanResult::Kept(KeepReason::Replaced)
    )));
    assert!(fixture.path("data/file").exists());
}

#[test]
fn an_ancestor_swapped_for_a_link_is_never_followed() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    // `sub` is not offered whole (it holds a recent file), its old file is.
    fixture.file("data/sub/old", 1, 2 * HOUR);
    let recent = fixture.file("data/sub/new", 1, Duration::ZERO);
    make_recent(&fixture, &recent);
    fixture.age(&fixture.path("data/sub"), 2 * HOUR);
    let rule = target_at(&root);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/sub/old")]);
    let twin = fixture.file("twin/old", 1, 2 * HOUR);
    fs::rename(fixture.path("data/sub"), fixture.path("moved")).unwrap();
    symlink(fixture.path("twin"), fixture.path("data/sub")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [(
            fixture.path("data/sub/old"),
            CleanResult::Kept(KeepReason::Replaced)
        )]
    );
    assert!(twin.exists());
}

#[test]
fn a_root_replaced_since_the_scan_keeps_its_entries() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    fs::rename(fixture.path("data"), fixture.path("old-data")).unwrap();
    fixture.file("data/file", 3, 2 * HOUR);
    let receipt = clean(&run, &report, &ids(&report));
    assert!(
        receipt
            .outcomes()
            .iter()
            .all(|outcome| outcome.result() == CleanResult::Kept(KeepReason::Replaced)),
        "{receipt:?}"
    );
    assert!(fixture.path("data/file").exists());

    // The very file the scan saw, moved into a new directory of the root's
    // name: the root is not the one scanned, nothing is judged through it.
    let report = run.scan();
    fs::rename(fixture.path("data"), fixture.path("older-data")).unwrap();
    fixture.directory("data");
    fs::rename(fixture.path("older-data/file"), fixture.path("data/file")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert!(results(&receipt).contains(&(
        fixture.path("data/file"),
        CleanResult::Kept(KeepReason::Replaced)
    )));
    assert!(fixture.path("data/file").exists());
}

#[test]
fn what_became_recent_since_the_scan_stays() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    make_recent(&fixture, &fixture.path("data/file"));
    make_recent(&fixture, &fixture.path("data/dir/inner/b"));
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [
            (
                fixture.path("data/dir"),
                CleanResult::Kept(KeepReason::Recent)
            ),
            (
                fixture.path("data/file"),
                CleanResult::Kept(KeepReason::Recent)
            ),
        ]
    );
    assert!(
        fixture.path("data/dir/a").exists(),
        "nothing of it is removed"
    );
}

#[test]
fn a_minimum_age_raised_since_the_scan_applies() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    run.options = options_later(&fixture, 10 * HOUR, Exclusions::none());
    let receipt = clean(&run, &report, &ids(&report));
    assert!(
        receipt
            .outcomes()
            .iter()
            .all(|outcome| outcome.result() == CleanResult::Kept(KeepReason::Recent)),
        "{receipt:?}"
    );
}

#[test]
fn a_link_or_a_lock_appearing_in_a_directory_keeps_it_whole() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    let outside = fixture.file("outside", 1, 2 * HOUR);
    symlink(&outside, fixture.path("data/dir/link")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert!(results(&receipt).contains(&(
        fixture.path("data/dir"),
        CleanResult::Kept(KeepReason::Changed)
    )));
    assert!(outside.exists());

    fs::remove_file(fixture.path("data/dir/link")).unwrap();
    let report = run.scan();
    fixture.file("data/dir/inner/db.lck", 1, 2 * HOUR);
    fixture.age(&fixture.path("data/dir/inner"), 2 * HOUR);
    let receipt = clean(&run, &report, &ids(&report));
    assert!(results(&receipt).contains(&(
        fixture.path("data/dir"),
        CleanResult::Kept(KeepReason::Changed)
    )));
    assert!(fixture.path("data/dir/inner/db.lck").exists());
}

#[test]
fn exclusions_in_force_at_cleaning_apply() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    let exclusions =
        Exclusions::new([fixture.path("data/file"), fixture.path("data/dir/inner")]).unwrap();
    run.options = options_later(&fixture, HOUR, exclusions);
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [
            (
                fixture.path("data/dir"),
                CleanResult::Kept(KeepReason::Changed)
            ),
            (
                fixture.path("data/file"),
                CleanResult::Kept(KeepReason::Excluded)
            ),
        ]
    );

    run.options = options_later(
        &fixture,
        HOUR,
        Exclusions::new([fixture.path("data")]).unwrap(),
    );
    let receipt = clean(&run, &report, &ids(&report));
    assert!(
        receipt
            .outcomes()
            .iter()
            .all(|outcome| outcome.result() == CleanResult::Kept(KeepReason::Excluded))
    );
    assert!(fixture.path("data/file").exists());
}

#[test]
fn a_root_that_came_to_hold_a_variable_keeps_all_its_entries() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    run.variables = fixture.variables(&[(Variable::Cache, "data/dir/inner")]);
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(receipt.stops(), &[]);
    assert_eq!(receipt.outcomes().len(), 2);
    assert!(
        receipt
            .outcomes()
            .iter()
            .all(|outcome| outcome.result() == CleanResult::Kept(KeepReason::Protected)),
        "{receipt:?}"
    );
    assert!(fixture.path("data/dir/inner/b").exists());
}

#[test]
fn a_single_file_target_is_removed_unless_replaced() {
    let fixture = Fixture::new();
    let file = fixture.file("errors.old", 4, 2 * HOUR);
    let rule: SingleFileTarget = serde_json::from_str(&format!(
        r#"{{"path": "{}", "subcategory": "Errors"}}"#,
        file.display()
    ))
    .unwrap();
    let run = run_later(&fixture, vec![RuleEntry::File(&rule)]);
    let report = run.scan();
    let replacement = fixture.file("replacement", 4, 2 * HOUR);
    fs::rename(&replacement, &file).unwrap();
    assert_eq!(
        results(&clean(&run, &report, &ids(&report))),
        [(file.clone(), CleanResult::Kept(KeepReason::Replaced))]
    );
    let report = run.scan();
    assert_eq!(
        results(&clean(&run, &report, &ids(&report))),
        [(file.clone(), CleanResult::Removed { bytes: 4 })]
    );
    assert!(!file.exists());
}

#[test]
fn a_directory_is_removed_whole_in_real_time() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/outer/inner/file", 2, Duration::ZERO);
    fixture.file("data/outer/other", 3, Duration::ZERO);
    let rule = target_at(&root);
    // Everything was written before `now`; file dates follow a coarse clock,
    // which the pause leaves behind.
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers = Readers::SYSTEM;
    std::thread::sleep(Duration::from_millis(50));
    run.options = ScanOptions::new(SystemTime::now(), Duration::ZERO, Exclusions::none());
    let report = run.scan();
    assert_eq!(offered_paths(&report), [fixture.path("data/outer")]);
    std::thread::sleep(Duration::from_millis(50));
    run.options = ScanOptions::new(SystemTime::now(), Duration::ZERO, Exclusions::none());
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [(
            fixture.path("data/outer"),
            CleanResult::Removed { bytes: 5 }
        )]
    );
    assert!(!fixture.path("data/outer").exists());
}

#[test]
fn nothing_is_touched_when_the_cleaning_cannot_vouch_for_its_checks() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    run.options = ScanOptions::new(later(&fixture), Duration::MAX, Exclusions::none());
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(receipt.stops(), [Stop::MinAgeBeyondClock]);
    assert_eq!(receipt.outcomes(), &[]);

    run.options = options_later(&fixture, HOUR, Exclusions::none());
    let rules = okapi_rules::RuleSet::embedded(okapi_rules::Os::Linux).unwrap();
    let variables = fixture.variables(&[(Variable::Home, "home")]);
    let receipt = crate::clean(&report, &ids(&report), &rules, &variables, &run.options);
    assert!(receipt.stops().contains(&Stop::MissingVariable {
        variable: Variable::Cache
    }));
    assert_eq!(receipt.outcomes(), &[]);
    assert!(fixture.path("data/file").exists());
}

#[test]
fn what_the_system_refuses_is_reported() {
    let fixture = Fixture::new();
    if reads_through_permissions(&fixture) {
        // Root reads and removes through permissions: nothing to observe.
        return;
    }
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    // The directory listed last refuses: without the check made beforehand,
    // what is listed before it would already be gone.
    let last = fs::read_dir(fixture.path("data/dir"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .last()
        .unwrap();
    fs::set_permissions(&last, fs::Permissions::from_mode(0o555)).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    fs::set_permissions(&last, fs::Permissions::from_mode(0o755)).unwrap();
    let refused = KeepReason::Failed {
        kind: std::io::ErrorKind::PermissionDenied,
        // `EACCES`: the system's own number is kept for the diagnosis.
        code: Some(13),
    };
    assert!(
        results(&receipt).contains(&(fixture.path("data/dir"), CleanResult::Kept(refused))),
        "{receipt:?}"
    );
    for kept in ["data/dir/a", "data/dir/inner/b", "data/dir/second/c"] {
        assert!(
            fixture.path(kept).exists(),
            "{kept}: checked before anything goes"
        );
    }

    // The root itself refuses: neither its file nor its directory goes.
    fixture.file("data/file", 3, 2 * HOUR);
    let report = run.scan();
    let root = fixture.path("data");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(receipt.outcomes().len(), 2);
    assert!(
        receipt
            .outcomes()
            .iter()
            .all(|outcome| outcome.result() == CleanResult::Kept(refused))
    );
    assert!(fixture.path("data/dir/a").exists());
}

#[test]
fn session_files_are_never_offered() {
    let fixture = Fixture::new();
    let root = fixture.directory("tmp");
    fixture.file("tmp/.X0-lock", 1, 2 * HOUR);
    fixture.file("tmp/lock", 1, 2 * HOUR);
    fixture.file("tmp/systemd-private-0a1b-colord.service-xyz/f", 1, 2 * HOUR);
    fixture.age(
        &fixture.path("tmp/systemd-private-0a1b-colord.service-xyz"),
        2 * HOUR,
    );
    fixture.file("tmp/build/out.o", 1, 2 * HOUR);
    fixture.file("tmp/build/cache.lck", 1, 2 * HOUR);
    fixture.age(&fixture.path("tmp/build"), 2 * HOUR);
    let rule = target_at(&root);
    let report = Run::new(&fixture, vec![RuleEntry::Target(&rule)]).scan();
    assert_eq!(offered_paths(&report), [fixture.path("tmp/build/out.o")]);
    assert_eq!(report.stats().protected_names(), 4);
}

/// The removal of everything in a directory checked a moment before, run
/// against what changed since: each entry is judged again just before it
/// goes.
fn empty_now(
    fixture: &Fixture,
    directory: &str,
    exclusions: &Exclusions,
) -> Result<(), KeepReason> {
    let protected =
        crate::protection::ProtectedRoots::new(&TemplateVariables::new(), file_identity).unwrap();
    let misc = no_protected_names();
    let roots = std::collections::BTreeSet::new();
    let path = fixture.path(directory);
    let opened = okapi_platform::Directory::open(&path).unwrap();
    let mut dates = crate::clean::RemovedDates::new();
    let mut walking = crate::clean::Walking {
        context: crate::walk::WalkContext {
            cutoff: later(fixture) - HOUR,
            exclusions,
            misc: &misc,
            protected: &protected,
            roots: &roots,
            readers: Readers::SYSTEM,
            limits: Limits::DEFAULT,
            remover: None,
        },
        device: opened.status().unwrap().identity.device(),
        pass: crate::clean::CleanPass::Remove,
        removed: crate::clean::Removed::default(),
        visited: 0,
        dates: &mut dates,
        blocked: None,
    };
    crate::clean::walk_entries(&opened, &path, Limits::DEFAULT.depth, &mut walking)
}

#[test]
fn the_removal_judges_each_entry_again() {
    let fixture = Fixture::new();
    let none = Exclusions::none();
    fixture.file("recent/file", 1, 2 * HOUR);
    make_recent(&fixture, &fixture.path("recent/file"));
    assert_eq!(
        empty_now(&fixture, "recent", &none),
        Err(KeepReason::Recent)
    );

    fixture.file("busy/sub/file", 1, 2 * HOUR);
    make_recent(&fixture, &fixture.path("busy/sub"));
    assert_eq!(
        empty_now(&fixture, "busy", &none),
        Err(KeepReason::Recent),
        "a subdirectory's own date is read before it is emptied"
    );
    assert!(fixture.path("busy/sub/file").exists());

    fixture.directory("linked");
    symlink(fixture.path("settled"), fixture.path("linked/link")).unwrap();
    assert_eq!(
        empty_now(&fixture, "linked", &none),
        Err(KeepReason::Changed)
    );

    fixture.file("excluded/keep", 1, 2 * HOUR);
    let exclusions = Exclusions::new([fixture.path("excluded/keep")]).unwrap();
    assert_eq!(
        empty_now(&fixture, "excluded", &exclusions),
        Err(KeepReason::Changed)
    );
    assert!(fixture.path("excluded/keep").exists());

    fixture.file("settled/a/b", 1, 2 * HOUR);
    assert_eq!(empty_now(&fixture, "settled", &none), Ok(()));
    assert_eq!(fs::read_dir(fixture.path("settled")).unwrap().count(), 0);
}

#[test]
fn a_directory_changed_since_the_scan_stays_whole() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    make_recent(&fixture, &fixture.path("data/dir"));
    let receipt = clean(&run, &report, &ids(&report));
    assert!(results(&receipt).contains(&(
        fixture.path("data/dir"),
        CleanResult::Kept(KeepReason::Recent)
    )));
    assert!(fixture.path("data/dir/a").exists());
}

#[test]
fn hard_links_do_not_make_each_other_recent() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    let first = fixture.file("data/outer/a/file", 2, Duration::ZERO);
    fs::create_dir_all(fixture.path("data/outer/b")).unwrap();
    fs::hard_link(&first, fixture.path("data/outer/b/link")).unwrap();
    let single = fixture.file("data/single", 3, Duration::ZERO);
    fs::hard_link(&single, fixture.path("data/twin")).unwrap();
    let rule = target_at(&root);
    // In real time: removing a name sets the file's status time to now, which
    // is after the cutoff.
    let mut run = Run::new(&fixture, vec![RuleEntry::Target(&rule)]);
    run.readers = Readers::SYSTEM;
    std::thread::sleep(Duration::from_millis(50));
    run.options = ScanOptions::new(SystemTime::now(), Duration::ZERO, Exclusions::none());
    let report = run.scan();
    std::thread::sleep(Duration::from_millis(50));
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [
            (
                fixture.path("data/outer"),
                CleanResult::Removed { bytes: 4 }
            ),
            (
                fixture.path("data/single"),
                CleanResult::Removed { bytes: 3 }
            ),
            (fixture.path("data/twin"), CleanResult::Removed { bytes: 3 }),
        ]
    );
}

#[test]
fn stopping_after_nothing_keeps_and_after_something_is_partial() {
    use crate::clean::Removed;

    let reason = KeepReason::Changed;
    assert_eq!(
        Removed::default().stopped(reason),
        CleanResult::Kept(reason)
    );
    let some = Removed {
        bytes: 0,
        entries: 1,
    };
    assert_eq!(
        some.stopped(reason),
        CleanResult::Partial { bytes: 0, reason }
    );
}

#[test]
fn the_descent_goes_through_plain_directories_of_the_root_device_only() {
    use crate::clean::is_plain_directory_on;
    use okapi_platform::{EntryStatus, EntryType};

    let status = |kind, device, mount_root| EntryStatus {
        kind,
        identity: FileIdentity::new(device, 1),
        last_change: SystemTime::UNIX_EPOCH,
        modified: SystemTime::UNIX_EPOCH,
        size: 0,
        links: 1,
        owner: 0,
        sticky: false,
        locked: false,
        mount_root,
    };
    assert!(is_plain_directory_on(
        &status(EntryType::Directory, 7, Some(false)),
        7
    ));
    for refused in [
        status(EntryType::Directory, 7, Some(true)),
        status(EntryType::Directory, 7, None),
        status(EntryType::Directory, 8, Some(false)),
        status(EntryType::Link, 7, Some(false)),
    ] {
        assert!(!is_plain_directory_on(&refused, 7), "{refused:?}");
    }
}

#[test]
fn a_name_the_rules_came_to_protect_stays() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    run.misc = serde_json::from_str(
        r#"{"type": "misc", "protectedEventLogs": ["file"], "trashPath": null}"#,
    )
    .unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert!(results(&receipt).contains(&(
        fixture.path("data/file"),
        CleanResult::Kept(KeepReason::Protected)
    )));
    assert!(fixture.path("data/file").exists());
}

#[test]
fn the_inode_change_counts_at_cleaning_as_at_scanning() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    // Extracted from an archive: an old content date, a fresh inode date.
    let extracted = fixture.file("data/extracted", 1, Duration::ZERO);
    File::open(&extracted)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
        .unwrap();
    let rule = target_at(&root);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    run.options = ScanOptions::new(SystemTime::now(), HOUR, Exclusions::none());
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [(extracted.clone(), CleanResult::Kept(KeepReason::Recent))]
    );
    assert!(extracted.exists());
    assert!(!receipt.is_complete());
}

#[test]
fn entries_gone_below_a_root_still_there_are_gone() {
    let fixture = Fixture::new();
    let (root, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    fs::remove_file(fixture.path("data/file")).unwrap();
    fs::remove_dir_all(fixture.path("data/dir")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(receipt.outcomes().len(), 2);
    assert!(
        receipt
            .outcomes()
            .iter()
            .all(|outcome| outcome.result() == CleanResult::Gone)
    );
    assert!(receipt.is_complete());
    assert!(root.exists());
}

#[test]
fn a_scan_root_moved_into_an_entry_keeps_it_whole() {
    let fixture = Fixture::new();
    let (_, first) = file_and_directory(&fixture);
    let other = fixture.directory("other");
    fixture.file("other/keep", 1, 2 * HOUR);
    let second = target_at(&other);
    let run = run_later(
        &fixture,
        vec![RuleEntry::Target(&first), RuleEntry::Target(&second)],
    );
    let report = run.scan();
    fs::rename(&other, fixture.path("data/dir/other")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert!(results(&receipt).contains(&(
        fixture.path("data/dir"),
        CleanResult::Kept(KeepReason::Protected)
    )));
    assert!(fixture.path("data/dir/other/keep").exists());
}

#[test]
fn a_rule_age_still_applies_at_cleaning() {
    let fixture = Fixture::new();
    let root = fixture.directory("cache");
    fixture.file("cache/old", 1, 48 * HOUR);
    let rule = app(&format!(
        r#"{{"id": "strict", "name": "Strict", "paths": ["{}"], "minAgeDays": 1}}"#,
        root.display()
    ));
    let run = run_later(
        &fixture,
        vec![RuleEntry::App {
            file: AppFile::Apps,
            rule: &rule,
        }],
    );
    let report = run.scan();
    // Its inode changed when the test wrote it, within the rule's day.
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [(
            fixture.path("cache/old"),
            CleanResult::Kept(KeepReason::Recent)
        )]
    );
}

#[test]
fn outcomes_follow_the_order_asked_once_each() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    let other = run.scan();
    let asked = ids(&report);
    let foreign = ids(&other)[0];
    let receipt = clean(&run, &report, &[asked[1], foreign, asked[0], asked[1]]);
    let order: Vec<_> = receipt
        .outcomes()
        .iter()
        .map(crate::CleanOutcome::id)
        .collect();
    assert_eq!(order, [asked[1], foreign, asked[0]]);
    assert_eq!(
        receipt.outcomes()[1].result(),
        CleanResult::Kept(KeepReason::UnknownItem)
    );
    assert!(matches!(
        receipt.outcomes()[0].result(),
        CleanResult::Removed { .. }
    ));
}

#[test]
fn an_extension_excluded_on_a_directory_on_the_way_covers_what_is_below() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/profile.keep/old", 1, 2 * HOUR);
    let recent = fixture.file("data/profile.keep/new", 1, Duration::ZERO);
    make_recent(&fixture, &recent);
    fixture.age(&fixture.path("data/profile.keep"), 2 * HOUR);
    let rule = target_at(&root);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    assert_eq!(
        offered_paths(&report),
        [fixture.path("data/profile.keep/old")]
    );
    run.options = options_later(
        &fixture,
        HOUR,
        Exclusions::new([PathBuf::from("*.keep")]).unwrap(),
    );
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [(
            fixture.path("data/profile.keep/old"),
            CleanResult::Kept(KeepReason::Excluded)
        )]
    );
}

#[test]
fn the_blocking_entry_is_named() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    let outside = fixture.file("outside", 1, 2 * HOUR);
    let link = fixture.path("data/dir/inner/link");
    symlink(&outside, &link).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    let dir = receipt
        .outcomes()
        .iter()
        .find(|outcome| outcome.path() == Some(fixture.path("data/dir").as_path()))
        .unwrap();
    assert_eq!(dir.result(), CleanResult::Kept(KeepReason::Changed));
    assert_eq!(dir.blocked(), Some(link.as_path()));
}

#[test]
fn a_variable_that_cannot_be_located_stops_the_cleaning() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    symlink(fixture.path("nowhere"), fixture.path("dangling")).unwrap();
    run.variables = fixture.variables(&[(Variable::Home, "dangling/home")]);
    let receipt = clean(&run, &report, &ids(&report));
    assert!(matches!(
        receipt.stops(),
        [Stop::UnreadableVariable {
            variable: Variable::Home,
            ..
        }]
    ));
    assert_eq!(receipt.outcomes(), &[]);
    assert!(fixture.path("data/file").exists());
}

#[test]
fn entering_and_removing_a_directory_check_it_is_the_one_read() {
    use crate::clean::{enter, remove_directory};

    let fixture = Fixture::new();
    fixture.file("parent/full/file", 1, 2 * HOUR);
    fixture.directory("parent/twin");
    let parent = okapi_platform::Directory::open(&fixture.path("parent")).unwrap();
    let full = parent.status_of(std::ffi::OsStr::new("full")).unwrap();
    let twin = parent.status_of(std::ffi::OsStr::new("twin")).unwrap();
    let name = std::ffi::OsStr::new;
    assert_eq!(
        enter(&parent, name("full"), &twin).err(),
        Some(KeepReason::Replaced)
    );
    assert!(enter(&parent, name("full"), &full).unwrap().is_some());
    assert!(enter(&parent, name("absent"), &full).unwrap().is_none());
    assert_eq!(
        remove_directory(&parent, name("full"), &full),
        Err(KeepReason::Changed),
        "not empty"
    );
    assert_eq!(
        remove_directory(&parent, name("twin"), &full),
        Err(KeepReason::Replaced)
    );
    assert_eq!(remove_directory(&parent, name("absent"), &full), Ok(false));
    assert_eq!(remove_directory(&parent, name("twin"), &twin), Ok(true));
}

#[test]
fn a_remembered_date_never_hides_a_new_write() {
    use crate::clean::{RemovedDates, remembered};

    let fixture = Fixture::new();
    fixture.file("file", 1, 2 * HOUR);
    let directory = okapi_platform::Directory::open(&fixture.root).unwrap();
    let status = directory.status_of(std::ffi::OsStr::new("file")).unwrap();
    let before = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    let mut dates = RemovedDates::new();
    dates.insert(status.identity, (before, status.modified));
    assert_eq!(
        remembered(status, &dates).last_change,
        before,
        "the same content: the date read before its other name went"
    );
    dates.insert(status.identity, (before, before));
    assert_eq!(
        remembered(status, &dates),
        status,
        "its content changed since, or the inode is another file's now"
    );
    assert_eq!(remembered(status, &RemovedDates::new()), status);
}

#[test]
fn an_extension_excluded_on_the_root_covers_its_entries() {
    let fixture = Fixture::new();
    let root = fixture.directory("cache.keep");
    fixture.file("cache.keep/old", 1, 2 * HOUR);
    let rule = target_at(&root);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    run.options = options_later(
        &fixture,
        HOUR,
        Exclusions::new([PathBuf::from("*.keep")]).unwrap(),
    );
    assert_eq!(
        results(&clean(&run, &report, &ids(&report))),
        [(
            fixture.path("cache.keep/old"),
            CleanResult::Kept(KeepReason::Excluded)
        )]
    );
}

#[test]
fn the_temporary_directory_itself_stays_cleanable() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    run.variables = fixture.variables(&[(Variable::TempDirectory, "data")]);
    let report = run.scan();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(receipt.outcomes().len(), 2);
    assert!(receipt.is_complete(), "{receipt:?}");
}

#[test]
fn a_root_reached_through_a_link_or_replaced_by_one_keeps_its_entries() {
    let fixture = Fixture::new();
    let root = fixture.directory("place/data");
    fixture.file("place/data/old", 1, 2 * HOUR);
    let rule = target_at(&root);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    // The same root, moved, and its old path now leading to it through a link
    // in an ancestor: exclusions would judge it at the wrong place.
    fs::rename(fixture.path("place"), fixture.path("elsewhere")).unwrap();
    symlink(fixture.path("elsewhere"), fixture.path("place")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [(
            fixture.path("place/data/old"),
            CleanResult::Kept(KeepReason::Replaced)
        )]
    );
    assert!(fixture.path("elsewhere/data/old").exists());

    // The root itself swapped for a link to a twin, then for a file.
    fs::remove_file(fixture.path("place")).unwrap();
    fixture.file("place/twin/old", 1, 2 * HOUR);
    symlink(fixture.path("place/twin"), fixture.path("place/data")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [(
            fixture.path("place/data/old"),
            CleanResult::Kept(KeepReason::Replaced)
        )]
    );
    assert!(fixture.path("place/twin/old").exists());
    fs::remove_file(fixture.path("place/data")).unwrap();
    fixture.file("place/data", 1, 2 * HOUR);
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        results(&receipt),
        [(
            fixture.path("place/data/old"),
            CleanResult::Kept(KeepReason::Replaced)
        )]
    );
}

#[test]
fn an_extension_excluded_above_the_root_covers_it() {
    let fixture = Fixture::new();
    let root = fixture.directory("profile.keep/cache");
    fixture.file("profile.keep/cache/old", 1, 2 * HOUR);
    let rule = target_at(&root);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    let exclusions = Exclusions::new([PathBuf::from("*.keep")]).unwrap();
    run.options = options_later(&fixture, HOUR, exclusions.clone());
    assert_eq!(
        results(&clean(&run, &report, &ids(&report))),
        [(
            fixture.path("profile.keep/cache/old"),
            CleanResult::Kept(KeepReason::Excluded)
        )]
    );
    let excluded = run.scan();
    assert_eq!(offered_paths(&excluded), Vec::<PathBuf>::new());
}

#[test]
fn a_directory_swapped_for_a_link_after_being_read_is_not_entered() {
    let fixture = Fixture::new();
    fixture.directory("parent/sub");
    fixture.directory("twin");
    let parent = okapi_platform::Directory::open(&fixture.path("parent")).unwrap();
    let name = std::ffi::OsStr::new("sub");
    let status = parent.status_of(name).unwrap();
    fs::remove_dir(fixture.path("parent/sub")).unwrap();
    symlink(fixture.path("twin"), fixture.path("parent/sub")).unwrap();
    assert_eq!(
        crate::clean::enter(&parent, name, &status).err(),
        Some(KeepReason::Replaced)
    );
}

#[test]
fn a_removal_that_stops_halfway_is_partial_and_names_the_entry() {
    let fixture = Fixture::new();
    fixture.file("busy/one", 5, 2 * HOUR);
    fixture.file("busy/two", 7, 2 * HOUR);
    // The entry listed last turns recent: the one before it is already gone.
    let last = fs::read_dir(fixture.path("busy"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .last()
        .unwrap();
    make_recent(&fixture, &last);
    let protected =
        crate::protection::ProtectedRoots::new(&TemplateVariables::new(), file_identity).unwrap();
    let misc = no_protected_names();
    let roots = std::collections::BTreeSet::new();
    let none = Exclusions::none();
    let path = fixture.path("busy");
    let opened = okapi_platform::Directory::open(&path).unwrap();
    let mut dates = crate::clean::RemovedDates::new();
    let mut walking = crate::clean::Walking {
        context: crate::walk::WalkContext {
            cutoff: later(&fixture) - HOUR,
            exclusions: &none,
            misc: &misc,
            protected: &protected,
            roots: &roots,
            readers: Readers::SYSTEM,
            limits: Limits::DEFAULT,
            remover: None,
        },
        device: opened.status().unwrap().identity.device(),
        pass: crate::clean::CleanPass::Remove,
        removed: crate::clean::Removed::default(),
        visited: 0,
        dates: &mut dates,
        blocked: None,
    };
    let reason = crate::clean::walk_entries(&opened, &path, Limits::DEFAULT.depth, &mut walking)
        .unwrap_err();
    let first_size = if last.ends_with("two") { 5 } else { 7 };
    assert_eq!(
        walking.removed.stopped(reason),
        CleanResult::Partial {
            bytes: first_size,
            reason: KeepReason::Recent
        }
    );
    assert_eq!(walking.blocked.as_deref(), Some(last.as_path()));
    assert!(last.exists());
}

#[test]
fn the_walking_limits_hold_at_cleaning_too() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let mut run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    // `data/dir/inner` is the third directory level: within the limit.
    run.limits = Limits {
        depth: 3,
        entries: 1_000,
    };
    let report = run.scan();
    assert!(offered_paths(&report).contains(&fixture.path("data/dir")));
    // A fourth directory level appears.
    fixture.file("data/dir/inner/deeper/deepest/x", 1, 2 * HOUR);
    for level in [
        "data/dir/inner/deeper/deepest",
        "data/dir/inner/deeper",
        "data/dir/inner",
    ] {
        fixture.age(&fixture.path(level), 2 * HOUR);
    }
    let receipt = clean(&run, &report, &ids(&report));
    assert!(results(&receipt).contains(&(
        fixture.path("data/dir"),
        CleanResult::Kept(KeepReason::Changed)
    )));
    assert!(fixture.path("data/dir/inner/deeper/deepest/x").exists());

    fs::remove_dir_all(fixture.path("data/dir/inner/deeper")).unwrap();
    fixture.age(&fixture.path("data/dir/inner"), 2 * HOUR);
    run.limits = Limits::DEFAULT;
    let report = run.scan();
    run.limits = Limits {
        depth: Limits::DEFAULT.depth,
        entries: 2,
    };
    let receipt = clean(&run, &report, &ids(&report));
    assert!(results(&receipt).contains(&(
        fixture.path("data/dir"),
        CleanResult::Kept(KeepReason::Changed)
    )));
}

#[test]
fn an_entry_swapped_for_a_link_stays_and_so_does_what_it_points_to() {
    let fixture = Fixture::new();
    let (_, rule) = file_and_directory(&fixture);
    let run = run_later(&fixture, vec![RuleEntry::Target(&rule)]);
    let report = run.scan();
    let target = fixture.file("target", 3, 2 * HOUR);
    fs::remove_file(fixture.path("data/file")).unwrap();
    symlink(&target, fixture.path("data/file")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert!(results(&receipt).contains(&(
        fixture.path("data/file"),
        CleanResult::Kept(KeepReason::Replaced)
    )));
    assert!(target.exists());
}

#[test]
fn a_single_file_target_reached_through_a_link_stays() {
    let fixture = Fixture::new();
    let file = fixture.file("logs/errors.old", 4, 2 * HOUR);
    let rule: SingleFileTarget = serde_json::from_str(&format!(
        r#"{{"path": "{}", "subcategory": "Errors"}}"#,
        file.display()
    ))
    .unwrap();
    let run = run_later(&fixture, vec![RuleEntry::File(&rule)]);
    let report = run.scan();
    // The same file, its directory moved and its old path now a link to it.
    fs::rename(fixture.path("logs"), fixture.path("kept")).unwrap();
    symlink(fixture.path("kept"), fixture.path("logs")).unwrap();
    assert_eq!(
        results(&clean(&run, &report, &ids(&report))),
        [(file, CleanResult::Kept(KeepReason::Replaced))]
    );
    assert!(fixture.path("kept/errors.old").exists());
}
