//! End-to-end tests of the `okapi` binary, in a sandbox: an emptied
//! environment whose home, temporary and state directories lie in a fresh
//! directory, and every absolute target of the rules excluded, so that no
//! test can reach the real system.
#![expect(
    clippy::unwrap_used,
    reason = "an integration test fails by panicking, and clippy only takes #[test] functions for tests, not their helpers"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use okapi_rules::{Effect, Os, RuleSet};

/// The size beyond which the journal is set aside (`journal::ROTATION_SIZE`).
const JOURNAL_ROTATION_SIZE: u64 = 64 * 1024 * 1024;

struct Sandbox {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let directory = tempfile::TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        for target in absolute_targets() {
            assert!(
                !root.starts_with(&target),
                "the sandbox {} lies below the rule target {}: move the target directory",
                root.display(),
                target.display()
            );
        }
        for directory in ["home/.cache", "tmp"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        Self {
            _directory: directory,
            root,
        }
    }

    fn file(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"content").unwrap();
        path
    }

    fn journal(&self) -> PathBuf {
        self.root.join("state/okapi/okapi.log")
    }

    /// Runs `okapi` with `arguments`, every absolute target excluded.
    fn run(&self, arguments: &[&str]) -> Output {
        self.run_with_state(arguments, &self.root.join("state"))
    }

    /// [`Self::run`], with the journal's state directory at `state`.
    fn run_with_state(&self, arguments: &[&str], state: &Path) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_okapi"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("XDG_STATE_HOME", state)
            .env("LC_ALL", "C")
            .args(arguments);
        for target in absolute_targets() {
            command.arg("--exclude").arg(target);
        }
        command.output().unwrap()
    }
}

/// Every target of the Linux rules that is not below a variable.
fn absolute_targets() -> Vec<PathBuf> {
    targets(false)
}

/// [`absolute_targets`] that the scan walks: maintenance operations are
/// never scanned, so never reported as excluded.
fn absolute_deletion_targets() -> Vec<PathBuf> {
    targets(true)
}

fn targets(deletions_only: bool) -> Vec<PathBuf> {
    let rules = RuleSet::embedded(Os::Linux).unwrap();
    let system = rules.system();
    let targets = system
        .clean_targets()
        .iter()
        .filter_map(|target| match target.effect() {
            Effect::Delete(deletion) => Some(deletion.path().to_string()),
            Effect::Maintenance { affected, .. } => (!deletions_only).then(|| affected.to_string()),
        });
    let files = system
        .single_file_targets()
        .iter()
        .map(|target| target.path().to_string());
    let applications = rules
        .apps()
        .iter()
        .chain(rules.gaming())
        .chain(rules.gpu_cache())
        .flat_map(|rule| match rule.effect() {
            Effect::Delete(deletion) => deletion.paths().to_vec(),
            Effect::Maintenance { affected, .. } if !deletions_only => affected.to_vec(),
            Effect::Maintenance { .. } => Vec::new(),
        })
        .map(|path| path.to_string());
    targets
        .chain(files)
        .chain(applications)
        .filter(|path| path.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

fn running_as_root() -> bool {
    okapi_platform::running_as_root()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn group<'a>(document: &'a serde_json::Value, rule: &str) -> &'a serde_json::Value {
    document["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| {
            group["rules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == rule)
        })
        .unwrap_or_else(|| panic!("no group for {rule} in {document}"))
}

fn journal_has(path: &Path, message: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|journal| journal.contains(message))
}

/// File dates follow a coarse clock: a pause puts what was just written
/// before the run's own `now`.
fn settle() {
    std::thread::sleep(Duration::from_millis(50));
}

#[test]
fn a_scan_then_a_cleaning_take_only_the_preselected_groups() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let pip_entries = [
        sandbox.file("home/.cache/pip/http/a"),
        sandbox.file("home/.cache/pip/wheel.whl"),
    ];
    let mesa = sandbox.file("home/.cache/mesa_shader_cache/x");
    // A system target, preselected, outside the categories asked for.
    let thumbnail = sandbox.file("home/.cache/thumbnails/large/t.png");
    let unity = sandbox.file("home/.cache/unity3d/y");
    settle();

    // The safety net first: every absolute target that exists is excluded.
    let scan = sandbox.run(&["--json", "--min-age", "0"]);
    assert_eq!(
        scan.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&scan.stderr)
    );
    let document = json(&scan);
    let issues: Vec<String> = document["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["detail"].as_str().unwrap().to_owned())
        .collect();
    for target in absolute_deletion_targets()
        .iter()
        .filter(|target| target.exists())
    {
        let exact = format!("path: {target:?}");
        assert!(
            issues
                .iter()
                .any(|issue| issue.starts_with("Excluded") && issue.contains(&exact)),
            "{} not excluded: {issues:?}",
            target.display()
        );
    }
    let json_text = String::from_utf8_lossy(&scan.stdout);
    assert!(
        !json_text.contains("\"id\":") && !json_text.contains("\"ids\":"),
        "no identifier leaves the process"
    );
    assert_eq!(
        group(&document, "apps:pip")["entries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(journal_has(&sandbox.journal(), "scan finished"));

    let cleaning = sandbox.run(&["--clean", "--apps", "--gpu", "--json", "--min-age", "0"]);
    assert_eq!(
        cleaning.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&cleaning.stdout)
    );
    let document = json(&cleaning);
    for entry in &pip_entries {
        assert!(!entry.exists(), "{} should be gone", entry.display());
    }
    assert!(
        sandbox.root.join("home/.cache/pip").exists(),
        "the root itself stays"
    );
    for (rule, kept) in [("gpu:mesa-cache", &mesa), ("apps:unity", &unity)] {
        let group = group(&document, rule);
        assert_eq!(group["preselected"], false, "{rule}");
        assert_eq!(group["decision"], "not_preselected", "{rule}");
        assert!(kept.exists(), "{rule}: {} should stay", kept.display());
    }
    assert!(
        thumbnail.exists(),
        "--apps --gpu never cleans a system target"
    );
    assert_eq!(document["clean"]["complete"], true);
    assert!(journal_has(&sandbox.journal(), "entry removed"));
}

#[test]
fn an_invalid_exclusion_stops_before_any_scan() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--exclude", "relative/path"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"");
    assert!(!journal_has(&sandbox.journal(), "scan finished"));
    assert!(journal_has(&sandbox.journal(), "exclusion refused"));
}

#[test]
fn help_wins_over_cleaning_and_writes_no_journal() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--clean", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage"));
    assert!(!sandbox.journal().exists());
}

#[test]
fn licenses_name_okapi_and_kudu() {
    let sandbox = Sandbox::new();
    let output = Command::new(env!("CARGO_BIN_EXE_okapi"))
        .env_clear()
        .env("HOME", sandbox.root.join("home"))
        .arg("licenses")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    assert!(text.contains("MIT License"));
    assert!(text.contains("https://github.com/adventdevinc/kudu"));
}

#[test]
fn an_age_beyond_the_clock_is_refused_even_quietly() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--apps", "-q", "--min-age", "200000000000000000"]);
    assert_eq!(output.status.code(), Some(2));
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("Nothing was looked at"), "{errors}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Nothing found"));
}

#[test]
fn a_journal_grown_too_large_is_set_aside() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let journal = sandbox.journal();
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    let grown = fs::File::create(&journal).unwrap();
    grown.set_len(JOURNAL_ROTATION_SIZE + 1).unwrap();
    let output = sandbox.run(&["--exclude", "relative/path"]);
    assert_eq!(output.status.code(), Some(2));
    let rotated = journal.with_file_name("okapi.log.1");
    assert_eq!(
        fs::metadata(&rotated).unwrap().len(),
        JOURNAL_ROTATION_SIZE + 1
    );
    assert!(journal_has(&journal, "journal rotated"));
}

#[test]
fn without_a_minimum_age_fresh_files_are_never_offered() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let fresh = sandbox.file("home/.cache/pip/fresh");
    let output = sandbox.run(&["--clean", "--apps"]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "nothing found: all is too recent"
    );
    assert!(fresh.exists());
}

#[test]
fn the_text_output_says_what_was_found_and_cleaned() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.file("home/.cache/pip/http/a");
    sandbox.file("home/.cache/pip/wheel.whl");
    settle();
    let output = sandbox.run(&["--clean", "--apps", "--min-age", "0"]);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "pip Cache: 2 entries",
        "Found: 2 entries",
        "Cleaned:",
        "Journal:",
    ] {
        assert!(text.contains(expected), "{expected} missing in:\n{text}");
    }

    sandbox.file("home/.cache/pip/again");
    settle();
    let quiet = sandbox.run(&["--clean", "--apps", "--min-age", "0", "-q"]);
    assert_eq!(quiet.status.code(), Some(0));
    let text = String::from_utf8_lossy(&quiet.stdout);
    assert!(
        !text.contains("pip Cache:"),
        "no group lines when quiet:\n{text}"
    );
    assert!(text.contains("Cleaned:"), "{text}");
}

#[test]
fn a_journal_that_cannot_be_opened_does_not_stop_the_run() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.file("home/.cache/pip/old");
    let not_a_directory = sandbox.file("state-is-a-file");
    settle();
    let output = sandbox.run_with_state(&["--json", "--apps", "--min-age", "0"], &not_a_directory);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Journal unavailable"));
    assert_eq!(
        group(&json(&output), "apps:pip")["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn an_exclusion_that_matches_nothing_is_said() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let missing = sandbox.root.join("home/.cache/JetBrians/keep");
    let output = sandbox.run(&["--apps", "--exclude", &missing.to_string_lossy()]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "nothing to find in an empty sandbox"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not exist as written"));
    assert!(journal_has(&sandbox.journal(), "exclusion matches nothing"));
}
