//! The summary of an analysis, from a real scan of a sandbox: a report can
//! only be made by scanning.
#![expect(
    clippy::unwrap_used,
    reason = "an integration test fails by panicking, and clippy only takes #[test] functions for tests, not their helpers"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use okapi_engine::{Exclusions, ScanOptions};
use okapi_rules::{Os, RuleSet, TemplateVariables, Variable};
use okapi_store::LastAnalysis;

/// Writes `bytes` bytes at `relative` below `root`.
fn file(root: &Path, relative: &str, bytes: usize) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// A sandbox below the build directory, never below a target of the rules
/// (`/tmp` is one): its canonical root.
fn sandbox(rules: &RuleSet) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = fs::canonicalize(directory.path()).unwrap();
    assert!(
        rules
            .absolute_targets(false)
            .iter()
            .all(|target| !root.starts_with(target))
    );
    (directory, root)
}

/// The variables of a home in `root`.
fn variables(root: &Path) -> TemplateVariables {
    let mut variables = TemplateVariables::new();
    for (variable, relative) in [
        (Variable::Home, "home"),
        (Variable::TempDirectory, "tmp"),
        (Variable::Config, "home/.config"),
        (Variable::Cache, "home/.cache"),
        (Variable::LocalShare, "home/.local/share"),
    ] {
        fs::create_dir_all(root.join(relative)).unwrap();
        variables.insert(variable, root.join(relative)).unwrap();
    }
    variables
}

/// Options that reach nothing of the machine: every absolute target
/// excluded, no minimum age.
fn options(rules: &RuleSet, now: SystemTime) -> ScanOptions {
    ScanOptions::new(
        now,
        Duration::ZERO,
        Exclusions::new(rules.absolute_targets(false)).unwrap(),
    )
}

#[test]
fn the_summary_adds_up_every_group_of_the_report() {
    let rules = RuleSet::embedded(Os::Linux).unwrap();
    let (_directory, root) = sandbox(&rules);
    // Two rules, two groups, of sizes and counts that differ.
    file(&root, "home/.cache/thumbnails/normal/a.png", 1_000);
    file(&root, "home/.cache/thumbnails/large/b.png", 200);
    file(&root, "home/.cache/vlc/art/c.jpg", 30);
    let variables = variables(&root);
    // File dates follow a coarse clock: a pause puts what was just written
    // before the scan's own `now`.
    std::thread::sleep(Duration::from_millis(50));
    let started = SystemTime::now();
    let options = options(&rules, started);
    let report = okapi_engine::scan(&rules, &variables, &options);
    let mut entries: Vec<usize> = report
        .groups()
        .iter()
        .map(|group| group.items().len())
        .collect();
    entries.sort_unstable();
    assert_eq!(entries, [1, 2], "two thumbnail directories, one of VLC");
    assert_eq!(
        LastAnalysis::of(&report, &options),
        LastAnalysis {
            time: okapi_store::seconds(started),
            reclaimable_bytes: 1_230,
            entries: 3,
        },
        "the sizes and counts of both groups"
    );
}

#[test]
fn the_summary_is_dated_from_the_options_never_from_the_clock() {
    let rules = RuleSet::embedded(Os::Linux).unwrap();
    let (_directory, root) = sandbox(&rules);
    let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let options = options(&rules, started);
    let report = okapi_engine::scan(&rules, &variables(&root), &options);
    assert_eq!(
        LastAnalysis::of(&report, &options),
        LastAnalysis {
            time: 1_000_000_000,
            reclaimable_bytes: 0,
            entries: 0,
        }
    );
}
