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
fn a_root_shared_with_an_application_rule_takes_every_profile_lock_along() {
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
