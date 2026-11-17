//! The caches of Chromium-based browsers: their profiles, their roots, their
//! groups, and the lock of a browser that runs. Firefox's are in `firefox`.

use std::os::unix::fs::symlink;

use okapi_platform::CHROMIUM_LOCK;
use okapi_rules::{BrowserKind, ChromiumBrowser, ChromiumBrowserKind, ChromiumCacheDirectories};

use super::clean::{clean, ids, run_later};
use super::*;
use crate::clean::{CleanResult, KeepReason};
use crate::report::{ItemId, Origin};
use crate::roots::BrowserRule;

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
                    browser: BrowserRule::Chromium(browser),
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

/// Each group by its label: its browser, whether it is checked, and what it
/// offers.
pub(super) fn groups(
    report: &ScanReport,
) -> Vec<(String, Option<BrowserKind>, bool, Vec<PathBuf>)> {
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
fn a_browser_cache_says_what_it_removes() {
    let fixture = Fixture::new();
    fixture.file("cache/chrome/Default/Cache/a", 1, 2 * HOUR);
    fixture.file("config/chrome/GrShaderCache/b", 1, 2 * HOUR);
    // The profile, under the user data: its cache is looked for under both.
    fixture.file("config/chrome/Default/Preferences", 1, 2 * HOUR);
    let browsers = [browser(&fixture, "chrome", "chrome")];
    let directories: ChromiumCacheDirectories = serde_json::from_str(
        r#"{"profile": [{"dir": "Cache", "label": "Cache", "description": "Pages kept"}],
            "shared": [{"dir": "GrShaderCache", "label": "Skia Shader Cache", "description": "Shaders kept"}]}"#,
    )
    .unwrap();
    let report = Run::new(&fixture, entries(&browsers, &directories)).scan();
    let described: Vec<Vec<String>> = report
        .groups()
        .iter()
        .map(|group| {
            group
                .descriptions()
                .iter()
                .map(ToString::to_string)
                .collect()
        })
        .collect();
    assert_eq!(described, [["Pages kept"], ["Shaders kept"]]);
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
    let chrome = Some(BrowserKind::Chromium(ChromiumBrowserKind::Chrome));
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
            browser: BrowserKind::Chromium(ChromiumBrowserKind::Chrome),
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
    assert_eq!(
        open[0].browser(),
        BrowserKind::Chromium(ChromiumBrowserKind::Chrome)
    );
    assert_eq!(open[0].lock(), lock);
    assert_eq!(open[0].user_data(), fixture.path("config/chrome"));

    let edge: Vec<ItemId> = report
        .groups()
        .iter()
        .filter(|group| group.browser() == Some(BrowserKind::Chromium(ChromiumBrowserKind::Edge)))
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
fn another_rule_on_a_browsers_root_is_refused_and_the_browser_keeps_its_lock() {
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
    assert!(matches!(
        report.issues(),
        [ScanIssue::UnsafeRoot {
            reason: crate::report::UnsafeReason::BrowserData,
            ..
        }]
    ));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::BrowserOpen),
        "the browser's own group, with its lock"
    );
    assert_eq!(
        report.groups()[0].browser(),
        Some(BrowserKind::Chromium(ChromiumBrowserKind::Chrome))
    );
    assert!(entry.exists());
}

/// Chromium as `config/chromium` and `cache/chromium` place it, in a snap
/// and in a Flatpak too.
fn chromium_everywhere(fixture: &Fixture) -> ChromiumBrowser {
    let at = |relative: &str| fixture.path(relative).display().to_string();
    serde_json::from_str(&format!(
        r#"{{"key": "chromium", "base": "{}", "cache": "{}", "sandboxed": [
               {{"packaging": "snap", "base": "{}", "cache": "{}"}},
               {{"packaging": "flatpak", "base": "{}", "cache": "{}"}}]}}"#,
        at("config/chromium"),
        at("cache/chromium"),
        at("snap/chromium/common/chromium"),
        at("snap/chromium/common/.cache/chromium"),
        at("flatpak/config/chromium"),
        at("flatpak/cache/chromium"),
    ))
    .unwrap()
}

/// The bases of `chromium_everywhere`, each installation's.
const CHROMIUM_BASES: [&str; 3] = [
    "config/chromium",
    "flatpak/config/chromium",
    "snap/chromium/common/chromium",
];

#[test]
fn only_a_flatpaks_lock_names_a_process_of_its_own_sandbox() {
    let fixture = Fixture::new();
    let mut cached: Vec<PathBuf> = CHROMIUM_BASES
        .iter()
        .map(|base| fixture.file(&format!("{base}/Default/Cache/a"), 1, 2 * HOUR))
        .collect();
    cached.sort();
    let browsers = [chromium_everywhere(&fixture)];
    let directories = directories();
    let report = Run::new(&fixture, entries(&browsers, &directories)).scan();
    let kinds: Vec<(PathBuf, okapi_platform::LockKind)> = report.groups()[0]
        .locks()
        .iter()
        .map(|lock| (lock.user_data().to_path_buf(), lock.lock_kind()))
        .collect();
    assert_eq!(
        kinds,
        [
            (
                fixture.path("config/chromium"),
                okapi_platform::LockKind::Link
            ),
            (
                fixture.path("flatpak/config/chromium"),
                okapi_platform::LockKind::SandboxedLink
            ),
            (
                fixture.path("snap/chromium/common/chromium"),
                okapi_platform::LockKind::Link
            ),
        ]
    );
    assert_eq!(offered_paths(&report), cached, "every installation's cache");
}

#[test]
fn a_flatpak_whose_link_names_no_process_of_the_system_keeps_every_cache() {
    let fixture = Fixture::new();
    let cached: Vec<PathBuf> = CHROMIUM_BASES
        .iter()
        .map(|base| fixture.file(&format!("{base}/Default/Cache/a"), 1, 2 * HOUR))
        .collect();
    let browsers = [chromium_everywhere(&fixture)];
    let directories = directories();
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    let lock_in = |base: &str| {
        symlink(
            format!("{}-{}", host.trim(), i32::MAX),
            fixture.path(base).join(CHROMIUM_LOCK),
        )
        .unwrap();
    };
    // In the snap, the number is the system's: a browser gone.
    let run = run_later(&fixture, entries(&browsers, &directories));
    let report = run.scan();
    lock_in("snap/chromium/common/chromium");
    let receipt = clean(&run, &report, &ids(&report));
    assert!(
        receipt
            .outcomes()
            .iter()
            .all(|outcome| matches!(outcome.result(), CleanResult::Removed { .. }))
    );
    assert!(cached.iter().all(|entry| !entry.exists()));
    // In the Flatpak, it is the sandbox's: the browser may run.
    let cached: Vec<PathBuf> = CHROMIUM_BASES
        .iter()
        .map(|base| fixture.file(&format!("{base}/Default/Cache/b"), 1, 2 * HOUR))
        .collect();
    let run = run_later(&fixture, entries(&browsers, &directories));
    let report = run.scan();
    lock_in("flatpak/config/chromium");
    let receipt = clean(&run, &report, &ids(&report));
    let results: Vec<CleanResult> = receipt
        .outcomes()
        .iter()
        .map(crate::CleanOutcome::result)
        .collect();
    assert_eq!(results, [CleanResult::Kept(KeepReason::BrowserOpen); 3]);
    assert!(cached.iter().all(|entry| entry.exists()));
}

#[test]
fn a_rule_on_or_below_a_chromium_base_is_refused() {
    let fixture = Fixture::new();
    fixture.file("flatpak/config/chromium/Local State", 1, 2 * HOUR);
    fixture.file("config/chromium/Default/Cookies", 1, 2 * HOUR);
    let on = target_at(&fixture.path("flatpak/config/chromium"));
    let below = target_at(&fixture.path("config/chromium/Default"));
    let browsers = [chromium_everywhere(&fixture)];
    let directories = directories();
    let mut all = vec![RuleEntry::Target(&on), RuleEntry::Target(&below)];
    all.extend(entries(&browsers, &directories));
    let report = Run::new(&fixture, all).scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    let mut refused: Vec<&Path> = report
        .issues()
        .iter()
        .filter_map(|issue| match issue {
            ScanIssue::UnsafeRoot {
                path,
                reason: crate::report::UnsafeReason::BrowserData,
                ..
            } => Some(path.as_path()),
            _ => None,
        })
        .collect();
    refused.sort();
    assert_eq!(
        refused,
        [
            fixture.path("config/chromium/Default"),
            fixture.path("flatpak/config/chromium")
        ]
    );
}

#[test]
fn a_chromium_cache_is_reserved_without_its_base() {
    let fixture = Fixture::new();
    let other = fixture.file("snap/chromium/common/.cache/fontconfig/old", 1, 2 * HOUR);
    fixture.file(
        "snap/chromium/common/.cache/chromium/Default/Cache/a",
        1,
        2 * HOUR,
    );
    let target = target(&format!(
        r#"{{"path": "{}", "subcategory": "Snap App Cache", "childSubdir": "common/.cache"}}"#,
        fixture.path("snap").display()
    ));
    let browsers = [chromium_everywhere(&fixture)];
    let directories = directories();
    let mut all = vec![RuleEntry::Target(&target)];
    all.extend(entries(&browsers, &directories));
    let report = Run::new(&fixture, all).scan();
    assert_eq!(offered_paths(&report), [other]);
}

#[test]
fn a_snaps_lock_is_read_where_its_current_revision_is_at_cleaning() {
    let fixture = Fixture::new();
    fixture.file(
        "snap/brave/1/.config/brave/Default/Preferences",
        1,
        2 * HOUR,
    );
    fixture.directory("snap/brave/2/.config/brave");
    let entry = fixture.file(
        "snap/brave/common/.cache/brave/Default/Cache/a",
        1,
        2 * HOUR,
    );
    symlink("1", fixture.path("snap/brave/current")).unwrap();
    let brave: ChromiumBrowser = serde_json::from_str(&format!(
        r#"{{"key": "brave", "base": "{}", "sandboxed": [
               {{"packaging": "snap", "base": "{}", "cache": "{}"}}]}}"#,
        fixture.path("config/brave").display(),
        fixture.path("snap/brave/current/.config/brave").display(),
        fixture.path("snap/brave/common/.cache/brave").display(),
    ))
    .unwrap();
    let browsers = [brave];
    let directories = directories();
    let run = run_later(&fixture, entries(&browsers, &directories));
    let report = run.scan();
    // Refreshed meanwhile, and launched from its new revision: the test's
    // own process, which runs, stands for it.
    fs::remove_file(fixture.path("snap/brave/current")).unwrap();
    symlink("2", fixture.path("snap/brave/current")).unwrap();
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    symlink(
        format!("{}-{}", host.trim(), std::process::id()),
        fixture
            .path("snap/brave/2/.config/brave")
            .join(CHROMIUM_LOCK),
    )
    .unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::BrowserOpen)
    );
    assert!(entry.exists());
}
