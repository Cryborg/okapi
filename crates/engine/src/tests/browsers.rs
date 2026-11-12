//! The caches of Chromium-based browsers: their profiles, their roots, their
//! groups, and the lock of a browser that runs.

use std::os::unix::fs::symlink;

use okapi_platform::CHROMIUM_LOCK;
use okapi_rules::{ChromiumBrowser, ChromiumBrowserKind, ChromiumCacheDirectories};

use super::clean::{clean, ids, run_later};
use super::*;
use crate::clean::{CleanResult, KeepReason};
use crate::report::{ItemId, Origin};

/// A browser whose user data is under `config/<name>` and its disk caches
/// under `cache/<name>`.
fn browser(fixture: &Fixture, key: &str, name: &str) -> ChromiumBrowser {
    serde_json::from_str(&format!(
        r#"{{"key": "{key}", "base": "{}", "cache": "{}"}}"#,
        fixture.path(&format!("config/{name}")).display(),
        fixture.path(&format!("cache/{name}")).display(),
    ))
    .unwrap()
}

/// Two profile caches, one a cache reset and one two segments deep, and a
/// shared cache reset.
fn directories() -> ChromiumCacheDirectories {
    serde_json::from_str(
        r#"{"profile": [
                {"dir": "Cache", "label": "Cache"},
                {"dir": "Code Cache", "label": "Code Cache", "cacheReset": true},
                {"dir": "Shared Dictionary/cache", "label": "Shared Dictionary Cache"}
            ],
            "shared": [{"dir": "GrShaderCache", "label": "Skia Shader Cache", "cacheReset": true}]}"#,
    )
    .unwrap()
}

fn entries<'a>(
    browsers: &'a [ChromiumBrowser],
    directories: &'a ChromiumCacheDirectories,
) -> Vec<RuleEntry<'a>> {
    browsers
        .iter()
        .flat_map(|browser| {
            let entry = move |shared| {
                move |directory| RuleEntry::Browser {
                    browser,
                    directory,
                    shared,
                }
            };
            directories
                .profile()
                .iter()
                .map(entry(false))
                .chain(directories.shared().iter().map(entry(true)))
        })
        .collect()
}

/// Each group by its label: whether it is checked, and what it offers.
fn groups(report: &ScanReport) -> Vec<(String, Option<ChromiumBrowserKind>, bool, Vec<PathBuf>)> {
    report
        .groups()
        .iter()
        .map(|group| {
            let mut paths: Vec<PathBuf> = group
                .items()
                .iter()
                .map(|item| item.path().to_path_buf())
                .collect();
            paths.sort();
            (
                group.label().to_string(),
                group.browser(),
                group.is_checked_by_default(),
                paths,
            )
        })
        .collect()
}

#[test]
fn each_cache_is_one_group_across_profiles_and_both_roots() {
    let fixture = Fixture::new();
    // Files right under each root: a directory just made is too recent to be
    // offered whole.
    for path in [
        "config/chrome/Default/Code Cache/a",
        "config/chrome/Profile 2/Code Cache/b",
        "cache/chrome/Default/Cache/c",
        "cache/chrome/Profile 2/Cache/d",
        "config/chrome/Default/Shared Dictionary/cache/e",
        "config/chrome/GrShaderCache/f",
        // A shared cache is under the user data only.
        "cache/chrome/GrShaderCache/never",
        "config/chrome/System Profile/Code Cache/g",
        "config/chrome/Guest Profile/Cache/h",
        "config/chrome/Profiles/Cache/i",
        "config/chrome/ProfileBackup/Cache/j",
        "config/chrome/Default/Preferences",
    ] {
        fixture.file(path, 1, 2 * HOUR);
    }
    let browsers = [browser(&fixture, "chrome", "chrome")];
    let directories = directories();
    let report = Run::new(&fixture, entries(&browsers, &directories)).scan();
    let chrome = Some(ChromiumBrowserKind::Chrome);
    assert_eq!(
        groups(&report),
        [
            (
                "Cache".to_owned(),
                chrome,
                true,
                vec![
                    fixture.path("cache/chrome/Default/Cache/c"),
                    fixture.path("cache/chrome/Profile 2/Cache/d"),
                ]
            ),
            (
                "Code Cache".to_owned(),
                chrome,
                false,
                vec![
                    fixture.path("config/chrome/Default/Code Cache/a"),
                    fixture.path("config/chrome/Profile 2/Code Cache/b"),
                ]
            ),
            (
                "Shared Dictionary Cache".to_owned(),
                chrome,
                true,
                vec![fixture.path("config/chrome/Default/Shared Dictionary/cache/e")]
            ),
            (
                "Skia Shader Cache".to_owned(),
                chrome,
                false,
                vec![fixture.path("config/chrome/GrShaderCache/f")]
            ),
        ]
    );
    assert!(matches!(
        report.groups()[2].origins(),
        [Origin::Browser {
            browser: ChromiumBrowserKind::Chrome,
            directory,
        }] if directory.to_string() == "Shared Dictionary/cache"
    ));
    assert!(report.issues().is_empty(), "{:?}", report.issues());
}

#[test]
fn a_linked_profile_is_not_followed_and_a_browser_without_profiles_is_read_at_its_base() {
    let fixture = Fixture::new();
    fixture.file("elsewhere/Code Cache/x", 1, 2 * HOUR);
    fixture.directory("config/chrome/Default");
    symlink(
        fixture.path("elsewhere"),
        fixture.path("config/chrome/Profile 3"),
    )
    .unwrap();
    fixture.file("config/opera/Code Cache/y", 1, 2 * HOUR);
    fixture.file("cache/opera/Cache/z", 1, 2 * HOUR);
    fixture.file("config/opera/Preferences", 1, 2 * HOUR);
    let browsers = [
        browser(&fixture, "chrome", "chrome"),
        browser(&fixture, "opera", "opera"),
    ];
    let directories = directories();
    let report = Run::new(&fixture, entries(&browsers, &directories)).scan();
    let offered = offered_paths(&report);
    assert_eq!(
        offered,
        [
            fixture.path("cache/opera/Cache/z"),
            fixture.path("config/opera/Code Cache/y"),
        ]
    );
    assert!(
        !offered
            .iter()
            .any(|path| path.starts_with(fixture.path("elsewhere"))),
        "a linked profile is never walked"
    );
}

#[test]
fn a_base_that_is_a_link_is_said_once() {
    let fixture = Fixture::new();
    fixture.file("elsewhere/Default/Cache/x", 1, 2 * HOUR);
    fixture.directory("config");
    symlink(fixture.path("elsewhere"), fixture.path("config/brave")).unwrap();
    let browsers = [browser(&fixture, "brave", "brave")];
    let directories = directories();
    let report = Run::new(&fixture, entries(&browsers, &directories)).scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert_eq!(report.issues().len(), 1, "{:?}", report.issues());
}

#[test]
fn caches_on_another_device_than_the_profiles_are_walked() {
    let fixture = Fixture::new();
    fixture.directory("config/chrome/Default");
    let data = fixture.file("cache/chrome/Default/Cache/c", 1, 2 * HOUR);
    // The whole disk cache tree on its own device.
    for path in [
        "cache/chrome",
        "cache/chrome/Default",
        "cache/chrome/Default/Cache",
    ] {
        mark(&fixture.path(path), MOUNTED_MARK);
    }
    mark(&data, MOUNTED_MARK);
    let browsers = [browser(&fixture, "chrome", "chrome")];
    let directories = directories();
    let mut run = Run::new(&fixture, entries(&browsers, &directories));
    run.readers.identity = other_device_when_marked;
    let report = run.scan();
    assert_eq!(offered_paths(&report), [data]);
}

/// A dangling link where a running Chrome keeps its lock, naming this
/// machine and this process: a browser as live as can be.
fn lock_chrome(fixture: &Fixture) -> PathBuf {
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    let lock = fixture.path("config/chrome").join(CHROMIUM_LOCK);
    // The process of the tests themselves: alive, and never signalled here.
    symlink(format!("{}-{}", host.trim(), std::process::id()), &lock).unwrap();
    lock
}

#[test]
fn the_open_browsers_are_those_whose_lock_is_live() {
    let fixture = Fixture::new();
    fixture.file("config/chrome/Default/Code Cache/a", 1, 2 * HOUR);
    fixture.file("config/edge/Default/Code Cache/b", 1, 2 * HOUR);
    let browsers = [
        browser(&fixture, "chrome", "chrome"),
        browser(&fixture, "edge", "edge"),
    ];
    let directories = directories();
    let run = Run::new(&fixture, entries(&browsers, &directories));
    let report = run.scan();
    let all = ids(&report);
    assert_eq!(report.open_browsers(&all), []);

    let lock = lock_chrome(&fixture);
    let open = report.open_browsers(&all);
    assert_eq!(open.len(), 1, "one per browser");
    assert_eq!(open[0].browser(), ChromiumBrowserKind::Chrome);
    assert_eq!(open[0].lock(), lock);
    assert_eq!(open[0].user_data(), fixture.path("config/chrome"));

    let edge: Vec<ItemId> = report
        .groups()
        .iter()
        .filter(|group| group.browser() == Some(ChromiumBrowserKind::Edge))
        .flat_map(|group| group.items().iter().map(crate::ScanItem::id))
        .collect();
    assert_eq!(
        report.open_browsers(&edge),
        [],
        "only the browsers of the entries asked for"
    );

    // A lock that cannot be read as a link: taken as held.
    fs::remove_file(&lock).unwrap();
    fs::write(&lock, "").unwrap();
    assert_eq!(report.open_browsers(&all).len(), 1, "an unreadable lock");

    // A lock left by a crash: this machine, a process gone.
    fs::remove_file(&lock).unwrap();
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    symlink(format!("{}-{}", host.trim(), i32::MAX), &lock).unwrap();
    assert!(report.open_browsers(&all).is_empty(), "a stale lock");
}

#[test]
fn a_browser_opened_after_the_scan_keeps_its_caches() {
    let fixture = Fixture::new();
    let entry = fixture.file("config/chrome/Default/Cache/a", 1, 2 * HOUR);
    let other = fixture.file("config/edge/Default/Cache/b", 1, 2 * HOUR);
    let browsers = [
        browser(&fixture, "chrome", "chrome"),
        browser(&fixture, "edge", "edge"),
    ];
    let directories = directories();
    let run = run_later(&fixture, entries(&browsers, &directories));
    let report = run.scan();
    let lock = lock_chrome(&fixture);
    let receipt = clean(&run, &report, &ids(&report));
    let results: Vec<(PathBuf, CleanResult, Option<PathBuf>)> = receipt
        .outcomes()
        .iter()
        .map(|outcome| {
            (
                outcome.path().unwrap().to_path_buf(),
                outcome.result(),
                outcome.blocked().map(Path::to_path_buf),
            )
        })
        .collect();
    assert!(results.contains(&(
        entry.clone(),
        CleanResult::Kept(KeepReason::BrowserOpen),
        Some(lock)
    )));
    assert!(entry.exists());
    assert!(!other.exists(), "the closed browser's cache is cleaned");
}

#[test]
fn a_browser_opened_between_the_check_and_the_removal_keeps_its_caches() {
    use crate::clean::CleanPass;
    use crate::watch::{Cancel, Progress, Watch};

    let fixture = Fixture::new();
    let entry = fixture.file("config/chrome/Default/Cache/a", 1, 2 * HOUR);
    let browsers = [browser(&fixture, "chrome", "chrome")];
    let directories = directories();
    let mut run = run_later(&fixture, entries(&browsers, &directories));
    let report = run.scan();
    let lock_path = fixture.path("config/chrome").join(CHROMIUM_LOCK);
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    let target = format!("{}-{}", host.trim(), std::process::id());
    let mut watch = Watch::new(Cancel::new(), None);
    watch.hook = Some(Box::new(move |progress, _| {
        if let Progress::Cleaning {
            pass: CleanPass::Check,
            done,
            total,
            ..
        } = progress
            && done == total
        {
            symlink(&target, &lock_path).unwrap();
        }
    }));
    run.watch = watch;
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::BrowserOpen)
    );
    assert!(entry.exists());
}

#[test]
fn a_browser_open_at_the_check_keeps_its_caches_though_it_closes_after() {
    use crate::clean::CleanPass;
    use crate::watch::{Cancel, Progress, Watch};

    let fixture = Fixture::new();
    let entry = fixture.file("config/chrome/Default/Cache/a", 1, 2 * HOUR);
    let browsers = [browser(&fixture, "chrome", "chrome")];
    let directories = directories();
    let mut run = run_later(&fixture, entries(&browsers, &directories));
    let report = run.scan();
    let lock = lock_chrome(&fixture);
    let closed = lock.clone();
    let mut watch = Watch::new(Cancel::new(), None);
    // Closed once checked: the entry was kept at its check, for good.
    watch.hook = Some(Box::new(move |progress, _| {
        if let Progress::Cleaning {
            pass: CleanPass::Check,
            done,
            total,
            ..
        } = progress
            && done == total
        {
            fs::remove_file(&closed).unwrap();
        }
    }));
    run.watch = watch;
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::BrowserOpen)
    );
    assert!(entry.exists());
    assert!(
        fs::symlink_metadata(&lock).is_err(),
        "closed after the check"
    );
}

#[test]
fn a_lock_left_by_a_crash_keeps_nothing() {
    let fixture = Fixture::new();
    let entry = fixture.file("config/chrome/Default/Cache/a", 1, 2 * HOUR);
    let browsers = [browser(&fixture, "chrome", "chrome")];
    let directories = directories();
    let run = run_later(&fixture, entries(&browsers, &directories));
    let report = run.scan();
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    symlink(
        format!("{}-{}", host.trim(), i32::MAX),
        fixture.path("config/chrome").join(CHROMIUM_LOCK),
    )
    .unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert!(matches!(
        receipt.outcomes()[0].result(),
        CleanResult::Removed { .. }
    ));
    assert!(!entry.exists());
}

#[test]
fn a_root_shared_with_another_rule_takes_the_browser_s_lock_along() {
    let fixture = Fixture::new();
    let entry = fixture.file("config/chrome/Default/Cache/a", 1, 2 * HOUR);
    let browsers = [browser(&fixture, "chrome", "chrome")];
    let directories = directories();
    let rule = app(&format!(
        r#"{{"id": "custom", "name": "Custom", "paths": ["{}"]}}"#,
        fixture.path("config/chrome/Default/Cache").display()
    ));
    let mut all = vec![RuleEntry::App {
        file: AppFile::Apps,
        rule: &rule,
    }];
    all.extend(entries(&browsers, &directories));
    let run = run_later(&fixture, all);
    let report = run.scan();
    lock_chrome(&fixture);
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::BrowserOpen),
        "the application rule's group holds the browser's lock too"
    );
    assert_eq!(
        report.groups()[0].browser(),
        None,
        "its label is the rule's"
    );
    assert!(entry.exists());
}
