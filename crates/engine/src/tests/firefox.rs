//! The caches of Firefox-based browsers: their profiles, found under any of
//! their bases by the lock a running browser takes, and that lock.

use std::fs::File;
use std::os::unix::fs::symlink;

use okapi_platform::FIREFOX_LOCK;
use okapi_rules::{BrowserCacheDirectory, BrowserKind, FirefoxBrowser, FirefoxKind};

use super::browsers::groups;
use super::clean::{clean, ids, run_later};
use super::*;
use crate::clean::{CleanResult, KeepReason};
use crate::report::BrowserLock;
use crate::roots::BrowserRule;

/// Firefox, its profiles under `home/.mozilla/firefox` then
/// `config/mozilla/firefox`, their caches under `cache/mozilla/firefox`.
fn firefox(fixture: &Fixture) -> FirefoxBrowser {
    serde_json::from_str(&format!(
        r#"{{"key": "firefox", "bases": ["{}", "{}"], "cache": "{}"}}"#,
        fixture.path("home/.mozilla/firefox").display(),
        fixture.path("config/mozilla/firefox").display(),
        fixture.path("cache/mozilla/firefox").display(),
    ))
    .unwrap()
}

/// A cache, a directory of thumbnails, and a cache reset.
fn directories() -> Vec<BrowserCacheDirectory> {
    serde_json::from_str(
        r#"[{"dir": "cache2", "label": "Cache"},
            {"dir": "thumbnails", "label": "Thumbnails"},
            {"dir": "startupCache", "label": "Startup Cache", "cacheReset": true}]"#,
    )
    .unwrap()
}

fn entries<'a>(
    browser: &'a FirefoxBrowser,
    directories: &'a [BrowserCacheDirectory],
) -> Vec<RuleEntry<'a>> {
    directories
        .iter()
        .map(|directory| RuleEntry::Browser {
            browser: BrowserRule::Firefox(browser),
            directory,
            shared: false,
        })
        .collect()
}

/// The profile `name` under the base `base`, launched once: it holds the
/// lock file a running Firefox locks.
fn profile(fixture: &Fixture, base: &str, name: &str) -> PathBuf {
    let profile = fixture.directory(&format!("{base}/{name}"));
    fs::write(profile.join(FIREFOX_LOCK), "").unwrap();
    profile
}

const LEGACY: &str = "home/.mozilla/firefox";
const XDG: &str = "config/mozilla/firefox";

/// A file in the cache directory `directory` of the profile `name`, old
/// enough to be offered.
fn cached(fixture: &Fixture, name: &str, directory: &str) -> PathBuf {
    fixture.file(
        &format!("cache/mozilla/firefox/{name}/{directory}/entry"),
        1,
        2 * HOUR,
    )
}

/// The browser `lock` names, running: the lock file locked, until the file
/// returned is dropped.
fn running(lock: &Path) -> File {
    let file = File::open(lock).unwrap();
    file.lock().unwrap();
    file
}

#[test]
fn each_cache_is_one_group_across_the_profiles_of_every_base() {
    let fixture = Fixture::new();
    let xdg = profile(&fixture, XDG, "a.default-release");
    let legacy = profile(&fixture, LEGACY, "b.default");
    let cache = cached(&fixture, "a.default-release", "cache2");
    let thumbnail = cached(&fixture, "a.default-release", "thumbnails");
    let startup = cached(&fixture, "a.default-release", "startupCache");
    let other = cached(&fixture, "b.default", "cache2");
    // Never emptied: a directory no rule names, nor a profile's settings.
    cached(&fixture, "a.default-release", "safebrowsing");
    fixture.file(
        &format!("{XDG}/a.default-release/cache2/prefs"),
        1,
        2 * HOUR,
    );
    let browser = firefox(&fixture);
    let directories = directories();
    let report = Run::new(&fixture, entries(&browser, &directories)).scan();
    let firefox = Some(BrowserKind::Firefox(FirefoxKind::Firefox));
    assert_eq!(
        groups(&report),
        [
            ("Cache".to_owned(), firefox, true, vec![cache, other]),
            ("Thumbnails".to_owned(), firefox, true, vec![thumbnail]),
            ("Startup Cache".to_owned(), firefox, false, vec![startup]),
        ]
    );
    let locks: Vec<(&Path, &Path)> = report.groups()[0]
        .locks()
        .iter()
        .map(|lock| (lock.lock(), lock.user_data()))
        .collect();
    assert_eq!(
        locks,
        [
            (xdg.join(FIREFOX_LOCK).as_path(), xdg.as_path()),
            (legacy.join(FIREFOX_LOCK).as_path(), legacy.as_path()),
        ],
        "every profile's lock (in the order of their paths)"
    );
    for group in report.groups() {
        assert_eq!(
            group.locks(),
            report.groups()[0].locks(),
            "{}",
            group.label()
        );
    }
    assert!(report.issues().is_empty(), "{:?}", report.issues());
}

#[test]
fn a_profile_is_a_plain_directory_of_a_base_that_holds_the_lock() {
    let fixture = Fixture::new();
    let kept = cached(&fixture, "kept", "cache2");
    profile(&fixture, XDG, "kept");
    // Never launched: no lock.
    fixture.directory(&format!("{XDG}/created"));
    cached(&fixture, "created", "cache2");
    // A link to a profile elsewhere.
    profile(&fixture, "elsewhere", "linked");
    symlink(
        fixture.path("elsewhere/linked"),
        fixture.path(&format!("{XDG}/linked")),
    )
    .unwrap();
    cached(&fixture, "linked", "cache2");
    // A cache without any profile.
    cached(&fixture, "orphan", "cache2");
    let browser = firefox(&fixture);
    let directories = directories();
    let report = Run::new(&fixture, entries(&browser, &directories)).scan();
    assert_eq!(offered_paths(&report), [kept]);
    assert_eq!(report.groups()[0].locks().len(), 1);
}

#[test]
fn a_profile_under_both_bases_has_both_locks() {
    let fixture = Fixture::new();
    let legacy = profile(&fixture, LEGACY, "a.default");
    let xdg = profile(&fixture, XDG, "a.default");
    let entry = cached(&fixture, "a.default", "cache2");
    let browser = firefox(&fixture);
    let directories = directories();
    let report = Run::new(&fixture, entries(&browser, &directories)).scan();
    assert_eq!(offered_paths(&report), [entry]);
    let locks: Vec<&Path> = report.groups()[0]
        .locks()
        .iter()
        .map(BrowserLock::lock)
        .collect();
    assert_eq!(locks, [xdg.join(FIREFOX_LOCK), legacy.join(FIREFOX_LOCK)]);
}

#[test]
fn a_base_refused_leaves_every_profile_of_the_browser() {
    let fixture = Fixture::new();
    profile(&fixture, XDG, "a.default");
    cached(&fixture, "a.default", "cache2");
    // The legacy base a link: a profile there, unknown, could be open.
    profile(&fixture, "elsewhere", "b.default");
    fixture.directory("home/.mozilla");
    symlink(fixture.path("elsewhere"), fixture.path(LEGACY)).unwrap();
    cached(&fixture, "b.default", "cache2");
    let browser = firefox(&fixture);
    let directories = directories();
    let report = Run::new(&fixture, entries(&browser, &directories)).scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert_eq!(report.issues().len(), 1, "said once: {:?}", report.issues());
}

#[test]
fn an_open_profile_keeps_the_caches_of_every_profile() {
    let fixture = Fixture::new();
    let first = profile(&fixture, XDG, "a.default");
    let second = profile(&fixture, XDG, "b.default");
    let entries_kept = [
        cached(&fixture, "a.default", "cache2"),
        cached(&fixture, "b.default", "cache2"),
    ];
    let browser = firefox(&fixture);
    let directories = directories();
    let run = run_later(&fixture, entries(&browser, &directories));
    let report = run.scan();
    let all = ids(&report);
    assert_eq!(report.open_browsers(&all), []);

    // The second profile only, in the order of the locks.
    let open = running(&second.join(FIREFOX_LOCK));
    let asked = report.open_browsers(&all);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].lock(), second.join(FIREFOX_LOCK));
    assert_eq!(asked[0].lock_kind(), okapi_platform::LockKind::FileLock);
    let receipt = clean(&run, &report, &all);
    assert_eq!(receipt.outcomes().len(), entries_kept.len());
    for outcome in receipt.outcomes() {
        assert_eq!(
            outcome.result(),
            CleanResult::Kept(KeepReason::BrowserOpen),
            "{:?}",
            outcome.path()
        );
    }
    assert!(entries_kept.iter().all(|entry| entry.exists()));

    // Both open: one question each.
    let also = running(&first.join(FIREFOX_LOCK));
    assert_eq!(report.open_browsers(&all).len(), 2);

    // Closed, the lock file kept: nothing is held any more.
    drop((open, also));
    assert_eq!(report.open_browsers(&all), []);
    let receipt = clean(&run, &report, &all);
    assert!(
        receipt
            .outcomes()
            .iter()
            .all(|outcome| matches!(outcome.result(), CleanResult::Removed { .. })),
        "{:?}",
        receipt.outcomes()
    );
    assert!(entries_kept.iter().all(|entry| !entry.exists()));
}

#[test]
fn a_browser_without_a_profile_launched_leaves_its_cache_alone() {
    let fixture = Fixture::new();
    fixture.directory(&format!("{XDG}/created"));
    cached(&fixture, "created", "cache2");
    // Right under the cache: no profile's.
    fixture.file("cache/mozilla/firefox/cache2/entry", 1, 2 * HOUR);
    let browser = firefox(&fixture);
    let directories = directories();
    let report = Run::new(&fixture, entries(&browser, &directories)).scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert!(report.issues().is_empty(), "{:?}", report.issues());
}

#[test]
fn a_profile_on_another_device_than_its_base_is_not_judged() {
    let fixture = Fixture::new();
    let mounted = profile(&fixture, XDG, "m.default");
    mark(&mounted, MOUNTED_MARK);
    cached(&fixture, "m.default", "cache2");
    profile(&fixture, XDG, "n.default");
    let kept = cached(&fixture, "n.default", "cache2");
    let browser = firefox(&fixture);
    let directories = directories();
    let mut run = Run::new(&fixture, entries(&browser, &directories));
    run.readers.identity = other_device_when_marked;
    let report = run.scan();
    assert_eq!(offered_paths(&report), [kept]);
    assert_eq!(report.groups()[0].locks().len(), 1);
}

#[test]
fn a_profile_whose_mount_status_cannot_be_read_is_left_under_every_base() {
    let fixture = Fixture::new();
    let unknown = profile(&fixture, LEGACY, "a.default");
    mark(&unknown, BIND_MARK);
    profile(&fixture, XDG, "a.default");
    cached(&fixture, "a.default", "cache2");
    profile(&fixture, XDG, "b.default");
    let kept = cached(&fixture, "b.default", "cache2");
    let browser = firefox(&fixture);
    let directories = directories();
    let mut run = Run::new(&fixture, entries(&browser, &directories));
    run.readers.mount_root = mount_status_refused_when_marked;
    let report = run.scan();
    assert_eq!(offered_paths(&report), [kept]);
    assert_eq!(report.issues().len(), 1, "said: {:?}", report.issues());
}

#[test]
fn a_profile_not_judged_under_one_base_is_left_under_every_base() {
    let fixture = Fixture::new();
    // Under the legacy base a link (to where it runs, say); under the other
    // a plain copy, its lock free.
    profile(&fixture, "elsewhere", "a.default");
    fixture.directory(LEGACY);
    symlink(
        fixture.path("elsewhere/a.default"),
        fixture.path(&format!("{LEGACY}/a.default")),
    )
    .unwrap();
    profile(&fixture, XDG, "a.default");
    cached(&fixture, "a.default", "cache2");
    profile(&fixture, XDG, "b.default");
    let kept = cached(&fixture, "b.default", "cache2");
    let browser = firefox(&fixture);
    let directories = directories();
    let report = Run::new(&fixture, entries(&browser, &directories)).scan();
    assert_eq!(offered_paths(&report), [kept]);
}

#[test]
fn a_profile_whose_lock_cannot_be_read_is_left_under_every_base() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = Fixture::new();
    profile(&fixture, LEGACY, "a.default");
    let unreadable = profile(&fixture, XDG, "a.default");
    cached(&fixture, "a.default", "cache2");
    profile(&fixture, XDG, "b.default");
    let kept = cached(&fixture, "b.default", "cache2");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::symlink_metadata(unreadable.join(FIREFOX_LOCK)).is_ok() {
        // Root reads it all: nothing to see.
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let browser = firefox(&fixture);
    let directories = directories();
    let report = Run::new(&fixture, entries(&browser, &directories)).scan();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(offered_paths(&report), [kept]);
    assert_eq!(report.issues().len(), 1, "said: {:?}", report.issues());
}

#[test]
fn a_cache_directory_that_is_a_link_is_never_walked() {
    let fixture = Fixture::new();
    profile(&fixture, XDG, "a.default");
    profile(&fixture, XDG, "b.default");
    fixture.file("elsewhere/cache2/x", 1, 2 * HOUR);
    fixture.file("elsewhere-too/y", 1, 2 * HOUR);
    fixture.directory("cache/mozilla/firefox/b.default");
    symlink(
        fixture.path("elsewhere"),
        fixture.path("cache/mozilla/firefox/a.default"),
    )
    .unwrap();
    symlink(
        fixture.path("elsewhere-too"),
        fixture.path("cache/mozilla/firefox/b.default/cache2"),
    )
    .unwrap();
    let browser = firefox(&fixture);
    let directories = directories();
    let report = Run::new(&fixture, entries(&browser, &directories)).scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
}

#[test]
fn an_application_rule_on_a_profiles_cache_leaves_it_to_the_browser_and_its_locks() {
    let fixture = Fixture::new();
    profile(&fixture, XDG, "a.default");
    let open = profile(&fixture, XDG, "b.default");
    let entry = cached(&fixture, "a.default", "cache2");
    let browser = firefox(&fixture);
    let directories = directories();
    let rule = app(&format!(
        r#"{{"id": "custom", "name": "Custom", "paths": ["{}"]}}"#,
        fixture
            .path("cache/mozilla/firefox/a.default/cache2")
            .display()
    ));
    let mut all = vec![RuleEntry::App {
        file: AppFile::Apps,
        rule: &rule,
    }];
    all.extend(entries(&browser, &directories));
    let run = run_later(&fixture, all);
    let report = run.scan();
    let _running = running(&open.join(FIREFOX_LOCK));
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::BrowserOpen)
    );
    assert!(entry.exists());
}

/// Where the fixture's snaps are, and Firefox's in it.
const SNAPS: &str = "snap";
const SNAP_BASE: &str = "snap/firefox/common/.mozilla/firefox";
const SNAP_CACHE: &str = "snap/firefox/common/.cache/mozilla/firefox";

/// Firefox as `firefox` places it, and in a snap too.
fn firefox_and_its_snap(fixture: &Fixture) -> FirefoxBrowser {
    serde_json::from_str(&format!(
        r#"{{"key": "firefox", "bases": ["{}"], "cache": "{}",
             "sandboxed": [{{"packaging": "snap", "bases": ["{}"], "cache": "{}"}}]}}"#,
        fixture.path(XDG).display(),
        fixture.path("cache/mozilla/firefox").display(),
        fixture.path(SNAP_BASE).display(),
        fixture.path(SNAP_CACHE).display(),
    ))
    .unwrap()
}

/// The system target of the snaps' caches, as `system.json` writes it.
fn snaps_target(fixture: &Fixture) -> CleanTarget {
    target(&format!(
        r#"{{"path": "{}", "subcategory": "Snap App Cache", "childSubdir": "common/.cache"}}"#,
        fixture.path(SNAPS).display()
    ))
}

/// What the snaps' target offers, Firefox's rules scanned with it.
fn offered_by_the_snaps_target(fixture: &Fixture, options: ScanOptions) -> Vec<PathBuf> {
    let browser = firefox_and_its_snap(fixture);
    let directories = directories();
    let target = snaps_target(fixture);
    let mut all = vec![RuleEntry::Target(&target)];
    all.extend(entries(&browser, &directories));
    let mut run = Run::new(fixture, all);
    run.options = options;
    let report = run.scan();
    let mut offered: Vec<PathBuf> = report
        .groups()
        .iter()
        .filter(|group| group.label().to_string() == "Snap App Cache")
        .flat_map(ScanGroup::items)
        .map(|item| item.path().to_path_buf())
        .collect();
    offered.sort();
    offered
}

/// The snap's cache besides Firefox's (in the order of their paths), and in
/// Firefox's what none of its rules empties: what the snaps' target must
/// leave.
fn snap_caches(fixture: &Fixture) -> (Vec<PathBuf>, Vec<PathBuf>) {
    // Neighbours a reservation too wide would keep: beside Firefox's cache,
    // and the same names in another snap.
    let others = vec![
        fixture.file("snap/firefox/common/.cache/fontconfig/old", 1, 2 * HOUR),
        fixture.file("snap/firefox/common/.cache/loose", 1, 2 * HOUR),
        fixture.file("snap/firefox/common/.cache/mozilla/other/x", 1, 2 * HOUR),
        fixture.file("snap/other/common/.cache/mozilla/firefox/x", 1, 2 * HOUR),
    ];
    let firefoxs = vec![
        fixture.file(
            &format!("{SNAP_CACHE}/a.default/safebrowsing/x"),
            1,
            2 * HOUR,
        ),
        fixture.file(&format!("{SNAP_CACHE}/loose"), 1, 2 * HOUR),
    ];
    (others, firefoxs)
}

#[test]
fn the_snaps_target_leaves_the_cache_of_a_browser_in_a_snap_to_it() {
    let fixture = Fixture::new();
    profile(&fixture, SNAP_BASE, "a.default");
    let (others, _) = snap_caches(&fixture);
    let entry = fixture.file(&format!("{SNAP_CACHE}/a.default/cache2/entry"), 1, 2 * HOUR);
    let browser = firefox_and_its_snap(&fixture);
    let directories = directories();
    let target = snaps_target(&fixture);
    let mut all = vec![RuleEntry::Target(&target)];
    all.extend(entries(&browser, &directories));
    let report = Run::new(&fixture, all).scan();
    let firefox = Some(BrowserKind::Firefox(FirefoxKind::Firefox));
    assert_eq!(
        groups(&report),
        [
            ("Snap App Cache".to_owned(), None, true, others),
            ("Cache".to_owned(), firefox, true, vec![entry]),
        ],
        "Firefox's cache by its own rule, with its lock; nothing else of it"
    );
    assert!(report.issues().is_empty(), "{:?}", report.issues());
}

#[test]
fn a_browsers_cache_is_reserved_whatever_its_profiles_and_the_categories() {
    // Never launched in the snap.
    let fixture = Fixture::new();
    let (others, _) = snap_caches(&fixture);
    let options = fixture.options(Exclusions::none());
    assert_eq!(offered_by_the_snaps_target(&fixture, options), others);
    // A base refused: the snap's profiles cannot be judged.
    let fixture = Fixture::new();
    let (others, _) = snap_caches(&fixture);
    fixture.directory("elsewhere/a.default");
    fixture.directory("snap/firefox/common/.mozilla");
    symlink(fixture.path("elsewhere"), fixture.path(SNAP_BASE)).unwrap();
    let options = fixture.options(Exclusions::none());
    assert_eq!(offered_by_the_snaps_target(&fixture, options), others);
    // The browsers not chosen.
    let fixture = Fixture::new();
    profile(&fixture, SNAP_BASE, "a.default");
    let (others, _) = snap_caches(&fixture);
    let options = fixture
        .options(Exclusions::none())
        .with_categories(BTreeSet::from([Category::System]));
    assert_eq!(offered_by_the_snaps_target(&fixture, options), others);
}

#[test]
fn a_browsers_cache_moved_into_an_entry_keeps_it_at_cleaning() {
    let fixture = Fixture::new();
    let (_, firefoxs) = snap_caches(&fixture);
    let browser = firefox_and_its_snap(&fixture);
    let directories = directories();
    let target = snaps_target(&fixture);
    let mut all = vec![RuleEntry::Target(&target)];
    all.extend(entries(&browser, &directories));
    let run = run_later(&fixture, all);
    let report = run.scan();
    let fontconfig = fixture.path("snap/firefox/common/.cache/fontconfig");
    assert!(offered_paths(&report).contains(&fontconfig), "whole");
    fs::rename(fixture.path(SNAP_CACHE), fontconfig.join("firefox")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert!(receipt.outcomes().iter().any(|outcome| {
        outcome.path() == Some(fontconfig.as_path())
            && outcome.result() == CleanResult::Kept(KeepReason::Protected)
    }));
    for moved in firefoxs {
        let relative = moved.strip_prefix(fixture.path(SNAP_CACHE)).unwrap();
        assert!(fontconfig.join("firefox").join(relative).exists());
    }
}

#[test]
fn a_root_of_another_rule_on_a_browsers_cache_is_refused() {
    let fixture = Fixture::new();
    snap_caches(&fixture);
    let browser = firefox_and_its_snap(&fixture);
    let directories = directories();
    let mine = target_at(&fixture.path(SNAP_CACHE));
    let mut all = vec![RuleEntry::Target(&mine)];
    all.extend(entries(&browser, &directories));
    let report = Run::new(&fixture, all).scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert!(
        matches!(
            report.issues(),
            [ScanIssue::UnsafeRoot { path, reason: crate::report::UnsafeReason::BrowserData, .. }]
                if *path == fixture.path(SNAP_CACHE)
        ),
        "{:?}",
        report.issues()
    );
}

#[test]
fn a_folder_chosen_below_a_browsers_cache_is_refused() {
    let fixture = Fixture::new();
    snap_caches(&fixture);
    let browser = firefox_and_its_snap(&fixture);
    let directories = directories();
    let chosen =
        crate::custom::CustomFolder::new(fixture.path(&format!("{SNAP_CACHE}/a.default"))).unwrap();
    let mut all = vec![RuleEntry::Custom(&chosen)];
    all.extend(entries(&browser, &directories));
    let report = Run::new(&fixture, all).scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert!(
        matches!(
            report.issues(),
            [ScanIssue::UnsafeRoot { path, reason: crate::report::UnsafeReason::BrowserData, .. }]
                if *path == fixture.path(&format!("{SNAP_CACHE}/a.default"))
        ),
        "{:?}",
        report.issues()
    );
}

#[test]
fn a_withheld_root_on_a_browsers_cache_is_not_refused() {
    let fixture = Fixture::new();
    snap_caches(&fixture);
    let browser = firefox_and_its_snap(&fixture);
    let directories = directories();
    // `links/alias/firefox` is the cache, reached across a link: withheld.
    fixture.directory("links");
    symlink(
        fixture.path("snap/firefox/common/.cache/mozilla"),
        fixture.path("links/alias"),
    )
    .unwrap();
    let across = target(&format!(
        r#"{{"path": "{}", "subcategory": "Across", "childSubdir": "firefox"}}"#,
        fixture.path("links").display()
    ));
    let mut all = vec![RuleEntry::Target(&across)];
    all.extend(entries(&browser, &directories));
    let report = Run::new(&fixture, all).scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert!(
        !report.issues().iter().any(|issue| matches!(
            issue,
            ScanIssue::UnsafeRoot {
                reason: crate::report::UnsafeReason::BrowserData,
                ..
            }
        )),
        "never walked, it only joins the rules that share it: {:?}",
        report.issues()
    );
}

#[test]
fn a_browsers_installations_fill_the_same_groups_with_all_their_locks() {
    let fixture = Fixture::new();
    let own = profile(&fixture, XDG, "a.default");
    let snapped = profile(&fixture, SNAP_BASE, "b.default");
    let entries_found = vec![
        cached(&fixture, "a.default", "cache2"),
        fixture.file(&format!("{SNAP_CACHE}/b.default/cache2/entry"), 1, 2 * HOUR),
    ];
    let browser = firefox_and_its_snap(&fixture);
    let directories = directories();
    let report = Run::new(&fixture, entries(&browser, &directories)).scan();
    let firefox = Some(BrowserKind::Firefox(FirefoxKind::Firefox));
    assert_eq!(
        groups(&report),
        [("Cache".to_owned(), firefox, true, entries_found)]
    );
    let locks: Vec<&Path> = report.groups()[0]
        .locks()
        .iter()
        .map(BrowserLock::user_data)
        .collect();
    assert_eq!(locks, [own.as_path(), snapped.as_path()]);
}

#[test]
fn a_base_refused_in_one_installation_leaves_only_its_caches() {
    let fixture = Fixture::new();
    profile(&fixture, XDG, "a.default");
    let own = cached(&fixture, "a.default", "cache2");
    // The snap's profile, launched, under its second base: its first one,
    // refused, may hold a copy of it that is open.
    profile(&fixture, "snap/firefox/current/firefox", "b.default");
    fixture.file(&format!("{SNAP_CACHE}/b.default/cache2/entry"), 1, 2 * HOUR);
    fixture.directory("elsewhere/b.default");
    fixture.directory("snap/firefox/common/.mozilla");
    symlink(fixture.path("elsewhere"), fixture.path(SNAP_BASE)).unwrap();
    let browser: FirefoxBrowser = serde_json::from_str(&format!(
        r#"{{"key": "firefox", "bases": ["{}"], "cache": "{}",
             "sandboxed": [{{"packaging": "snap", "bases": ["{}", "{}"], "cache": "{}"}}]}}"#,
        fixture.path(XDG).display(),
        fixture.path("cache/mozilla/firefox").display(),
        fixture.path(SNAP_BASE).display(),
        fixture.path("snap/firefox/current/firefox").display(),
        fixture.path(SNAP_CACHE).display(),
    ))
    .unwrap();
    let directories = directories();
    // The base after the refused one is reserved all the same.
    let second = target_at(&fixture.path("snap/firefox/current/firefox"));
    let mut all = vec![RuleEntry::Target(&second)];
    all.extend(entries(&browser, &directories));
    let report = Run::new(&fixture, all).scan();
    assert_eq!(offered_paths(&report), [own]);
    assert!(
        report.issues().iter().any(|issue| matches!(
            issue,
            ScanIssue::UnsafeRoot { path, reason: crate::report::UnsafeReason::BrowserData, .. }
                if *path == fixture.path("snap/firefox/current/firefox")
        )),
        "{:?}",
        report.issues()
    );
}

#[test]
fn an_open_installation_keeps_the_caches_of_the_others() {
    let fixture = Fixture::new();
    profile(&fixture, XDG, "a.default");
    let snapped = profile(&fixture, SNAP_BASE, "b.default");
    // An installation without a cache has nothing to empty, nor a lock to
    // wait for.
    fixture.directory(&format!("{SNAP_CACHE}/b.default"));
    let own = cached(&fixture, "a.default", "cache2");
    let browser = firefox_and_its_snap(&fixture);
    let directories = directories();
    let run = run_later(&fixture, entries(&browser, &directories));
    let report = run.scan();
    let _running = running(&snapped.join(FIREFOX_LOCK));
    let receipt = clean(&run, &report, &ids(&report));
    let results: Vec<CleanResult> = receipt
        .outcomes()
        .iter()
        .map(crate::CleanOutcome::result)
        .collect();
    assert_eq!(results, [CleanResult::Kept(KeepReason::BrowserOpen)]);
    assert!(own.exists());
}

/// Firefox in a snap whose profiles are under `snap/firefox/current/firefox`,
/// `current` a link to the revision `1`, its profile `b.default` launched in
/// both revisions (a refresh copies them).
fn firefox_in_revisions(fixture: &Fixture) -> (FirefoxBrowser, PathBuf) {
    for revision in ["1", "2"] {
        profile(
            fixture,
            &format!("snap/firefox/{revision}/firefox"),
            "b.default",
        );
    }
    symlink("1", fixture.path("snap/firefox/current")).unwrap();
    let entry = fixture.file(&format!("{SNAP_CACHE}/b.default/cache2/entry"), 1, 2 * HOUR);
    let browser = serde_json::from_str(&format!(
        r#"{{"key": "firefox", "bases": ["{}"], "cache": "{}",
             "sandboxed": [{{"packaging": "snap", "bases": ["{}"], "cache": "{}"}}]}}"#,
        fixture.path(XDG).display(),
        fixture.path("cache/mozilla/firefox").display(),
        fixture.path("snap/firefox/current/firefox").display(),
        fixture.path(SNAP_CACHE).display(),
    ))
    .unwrap();
    (browser, entry)
}

#[test]
fn a_snaps_profile_is_judged_in_the_revision_it_runs_from_after_a_refresh() {
    // Refreshed, then launched from the new revision.
    let fixture = Fixture::new();
    let (browser, entry) = firefox_in_revisions(&fixture);
    let directories = directories();
    let run = run_later(&fixture, entries(&browser, &directories));
    let report = run.scan();
    fs::remove_file(fixture.path("snap/firefox/current")).unwrap();
    symlink("2", fixture.path("snap/firefox/current")).unwrap();
    let new_revision =
        running(&fixture.path(&format!("snap/firefox/2/firefox/b.default/{FIREFOX_LOCK}")));
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::BrowserOpen)
    );
    assert!(entry.exists());
    drop(new_revision);
    // Refreshed as it runs from the revision analysed.
    let fixture = Fixture::new();
    let (browser, entry) = firefox_in_revisions(&fixture);
    let run = run_later(&fixture, entries(&browser, &directories));
    let report = run.scan();
    let _running =
        running(&fixture.path(&format!("snap/firefox/1/firefox/b.default/{FIREFOX_LOCK}")));
    fs::remove_file(fixture.path("snap/firefox/current")).unwrap();
    symlink("2", fixture.path("snap/firefox/current")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::BrowserOpen)
    );
    assert!(entry.exists());
}
