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

use okapi_rules::{Os, RuleSet};

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

    /// Writes the settings file.
    fn settings(&self, json: &str) {
        let path = self.root.join("config/okapi/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, json).unwrap();
    }

    fn history(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.root.join("state/okapi/history.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
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
        self.run_with(arguments, state, "C")
    }

    /// [`Self::run`] in a French session.
    fn run_in_french(&self, arguments: &[&str]) -> Output {
        self.run_with(arguments, &self.root.join("state"), "fr_FR.UTF-8")
    }

    fn run_with(&self, arguments: &[&str], state: &Path, locale: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_okapi"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("XDG_STATE_HOME", state)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("LC_ALL", locale)
            .args(arguments);
        for target in absolute_targets() {
            command.arg("--exclude").arg(target);
        }
        command.output().unwrap()
    }
}

/// Every target of the Linux rules that is not below a variable.
fn absolute_targets() -> Vec<PathBuf> {
    RuleSet::embedded(Os::Linux)
        .unwrap()
        .absolute_targets(false)
}

/// [`absolute_targets`] that the scan walks: maintenance operations are
/// never scanned, so never reported as excluded.
fn absolute_deletion_targets() -> Vec<PathBuf> {
    RuleSet::embedded(Os::Linux).unwrap().absolute_targets(true)
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
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("Usage"));
    assert!(
        help.contains(
            &*sandbox
                .root
                .join("config/okapi/settings.json")
                .to_string_lossy()
        ),
        "the help names the settings file in use: {help}"
    );
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
    grown
        .set_len(okapi_store::journal::ROTATION_SIZE + 1)
        .unwrap();
    let output = sandbox.run(&["--exclude", "relative/path"]);
    assert_eq!(output.status.code(), Some(2));
    let rotated = journal.with_file_name("okapi.log.1");
    assert_eq!(
        fs::metadata(&rotated).unwrap().len(),
        okapi_store::journal::ROTATION_SIZE + 1
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
fn a_french_session_reads_the_labels_in_french_and_scripts_keep_them() {
    let sandbox = Sandbox::new();
    sandbox.file("home/.cache/pip/http/a");
    sandbox.file("home/.cache/thumbnails/large/t.png");
    settle();
    let asked = ["--apps", "--system", "--min-age", "0"];
    let text = String::from_utf8_lossy(&sandbox.run_in_french(&asked).stdout).into_owned();
    assert!(text.contains("Cache de pip : 1 entrée,"), "{text}");
    assert!(text.contains("Cache des miniatures : 1 entrée,"), "{text}");
    let output = sandbox.run_in_french(&[asked.as_slice(), &["--json"]].concat());
    let document = json(&output);
    assert_eq!(
        group(&document, "apps:pip")["label"],
        "pip Cache",
        "a stable name"
    );
    assert_eq!(
        group(&document, "system:Thumbnail Cache")["label"],
        "Thumbnail Cache"
    );
    let written = String::from_utf8_lossy(&output.stdout);
    assert!(!written.contains("Cache de pip"), "{written}");
    assert!(!written.contains("Cache des miniatures"), "{written}");
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
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("does not exist as written"));
    assert!(!errors.contains("In the settings"), "{errors}");
    assert!(journal_has(&sandbox.journal(), "exclusion matches nothing"));

    sandbox.settings(&format!(
        r#"{{"version": 1, "exclusions": ["{}"]}}"#,
        missing.display()
    ));
    let output = sandbox.run(&["--apps"]);
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(
        errors.contains("In the settings") && errors.contains("does not exist as written"),
        "a missing exclusion of the settings names them: {errors}"
    );
}

#[test]
fn settings_and_command_line_exclusions_both_apply() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let by_settings = sandbox.file("home/.cache/pip/settings-keep");
    let by_flag = sandbox.file("home/.cache/pip/flag-keep");
    let witness = sandbox.file("home/.cache/pip/goes");
    sandbox.settings(&format!(
        r#"{{"version": 1, "exclusions": ["{}"]}}"#,
        by_settings.display()
    ));
    settle();
    let flag = by_flag.to_string_lossy().into_owned();
    let output = sandbox.run(&["--clean", "--apps", "--min-age", "0", "--exclude", &flag]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(by_settings.exists() && by_flag.exists());
    assert!(!witness.exists());
}

#[test]
fn the_settings_age_applies_unless_the_command_line_gives_one() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let fresh = sandbox.file("home/.cache/pip/fresh");
    settle();
    sandbox.settings(r#"{"version": 1, "minAgeMinutes": 60}"#);
    let output = sandbox.run(&["--clean", "--apps"]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "too recent for the settings' age"
    );
    assert!(fresh.exists());
    let output = sandbox.run(&["--clean", "--apps", "--min-age", "0"]);
    assert_eq!(output.status.code(), Some(0), "the command line wins");
    assert!(!fresh.exists());

    let again = sandbox.file("home/.cache/pip/again");
    settle();
    sandbox.settings(r#"{"version": 1, "minAgeMinutes": 0}"#);
    let output = sandbox.run(&["--clean", "--apps"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the settings' age, not the default"
    );
    assert!(!again.exists());
}

#[test]
fn unusable_settings_stop_before_any_scan() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    for (json, code) in [
        (r#"{"version": 1, "exclusion": []}"#, 2),
        (r#"{"version": 2}"#, 2),
        (
            r#"{"version": 1, "minAgeMinutes": 18446744073709551615}"#,
            2,
        ),
        // Fits in a duration, beyond what the clock can go back to.
        (r#"{"version": 1, "minAgeMinutes": 200000000000000000}"#, 2),
    ] {
        sandbox.settings(json);
        let output = sandbox.run(&["--apps"]);
        assert_eq!(output.status.code(), Some(code), "{json}");
        assert_eq!(output.stdout, b"", "{json}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("settings.json"),
            "{json}: the message names the settings file"
        );
    }
    let dangling = sandbox.root.join("home/dangling");
    std::os::unix::fs::symlink(sandbox.root.join("nowhere"), &dangling).unwrap();
    sandbox.settings(&format!(
        r#"{{"version": 1, "exclusions": ["{}/below"]}}"#,
        dangling.display()
    ));
    let output = sandbox.run(&["--apps"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("settings.json"));

    sandbox.settings(r#"{"version": 1, "exclusions": ["relative/keep"]}"#);
    let output = sandbox.run(&["--apps"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a settings exclusion refused"
    );
    assert_eq!(output.stdout, b"");
    assert!(String::from_utf8_lossy(&output.stderr).contains("settings.json"));

    fs::remove_file(sandbox.root.join("config/okapi/settings.json")).unwrap();
    fs::create_dir(sandbox.root.join("config/okapi/settings.json")).unwrap();
    let output = sandbox.run(&["--apps"]);
    assert_eq!(output.status.code(), Some(1), "unreadable, not invalid");
}

#[test]
fn settings_behind_a_dangling_link_stop_the_cleaning() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let kept = sandbox.file("home/.cache/pip/old");
    settle();
    let settings = sandbox.root.join("config/okapi/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(sandbox.root.join("dotfiles/settings.json"), &settings).unwrap();
    let output = sandbox.run(&["--clean", "--apps", "--min-age", "0"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        kept.exists(),
        "unreachable settings are never taken for none"
    );
}

#[test]
fn each_cleaning_leaves_a_history_line() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let file = sandbox.file("home/.cache/pip/old");
    settle();
    let output = sandbox.run(&["--clean", "--apps", "--min-age", "0"]);
    assert_eq!(output.status.code(), Some(0));
    let history = sandbox.history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["version"], 1);
    assert_eq!(history[0]["removed"], 1);
    assert_eq!(
        history[0].get("entries"),
        None,
        "no deletion log by default"
    );

    sandbox.file("home/.cache/pip/other");
    settle();
    sandbox.settings(r#"{"version": 1, "deletionLog": true}"#);
    let output = sandbox.run(&["--clean", "--apps", "--min-age", "0"]);
    assert_eq!(output.status.code(), Some(0));
    let history = sandbox.history();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[1]["entries"][0].get("inexact"),
        None,
        "a name in UTF-8 is exact"
    );
    assert_eq!(
        history[1]["entries"][0]["path"],
        sandbox
            .root
            .join("home/.cache/pip/other")
            .to_string_lossy()
            .as_ref()
    );
    assert!(!file.exists());
}

#[test]
fn the_history_line_counts_what_became_of_each_entry() {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let pip = sandbox.root.join("home/.cache/pip");
    fs::create_dir_all(pip.join("locked")).unwrap();
    fs::write(pip.join("three"), b"abc").unwrap();
    fs::write(pip.join("five"), b"abcde").unwrap();
    fs::write(pip.join("locked/kept"), b"x").unwrap();
    let read_only = fs::Permissions::from_mode(0o500);
    fs::set_permissions(pip.join("locked"), read_only).unwrap();
    settle();
    let before = seconds_now();
    let output = sandbox.run(&["--clean", "--apps", "--min-age", "0"]);
    let after = seconds_now();
    fs::set_permissions(pip.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(output.status.code(), Some(3), "a refusal of permission");
    let line = &sandbox.history()[0];
    assert_eq!(line["removed"], 2, "{line}");
    assert_eq!(line["removedBytes"], 8, "{line}");
    assert_eq!(line["gone"], 0, "{line}");
    assert_eq!(
        line["partial"].as_u64().unwrap() + line["kept"].as_u64().unwrap(),
        1,
        "the locked directory: {line}"
    );
    assert_eq!(line["complete"], false, "{line}");
    assert_eq!(line["stops"], serde_json::json!([]), "{line}");
    let time = line["time"].as_i64().unwrap();
    assert!(
        (before..=after).contains(&time),
        "{time} not in {before}..={after}"
    );
}

fn seconds_now() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    i64::try_from(since.as_secs()).unwrap()
}

#[test]
fn a_name_not_in_utf8_is_marked_inexact_in_the_deletion_log() {
    use std::os::unix::ffi::OsStrExt;
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let directory = sandbox.root.join("home/.cache/pip");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join(std::ffi::OsStr::from_bytes(b"bad\xff")),
        b"x",
    )
    .unwrap();
    settle();
    sandbox.settings(r#"{"version": 1, "deletionLog": true}"#);
    sandbox.run(&["--clean", "--apps", "--min-age", "0"]);
    let entries = &sandbox.history()[0]["entries"];
    assert_eq!(entries[0]["inexact"], true, "{entries}");
    assert_eq!(
        entries[0]["path"],
        format!("{}/bad\u{fffd}", directory.display())
    );
}

#[test]
fn a_deletion_log_that_cannot_be_kept_stops_before_cleaning() {
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let kept = sandbox.file("home/.cache/pip/old");
    let not_a_directory = sandbox.file("state-is-a-file");
    settle();
    sandbox.settings(r#"{"version": 1, "deletionLog": true}"#);
    let output = sandbox.run_with_state(&["--clean", "--apps", "--min-age", "0"], &not_a_directory);
    assert_eq!(output.status.code(), Some(1));
    assert!(kept.exists(), "nothing removed without the log asked for");

    sandbox.settings(r#"{"version": 1}"#);
    let output = sandbox.run_with_state(&["--clean", "--apps", "--min-age", "0"], &not_a_directory);
    assert_eq!(
        output.status.code(),
        Some(0),
        "without a deletion log, it goes on"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("will not be added to the history"),
        "the missing history line is said"
    );
    assert!(!kept.exists());
}

/// The exact form scripts and the history read, fixed before the names move
/// to their shared source.
#[test]
fn the_cleaning_outcomes_keep_their_exact_form() {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return;
    }
    let sandbox = Sandbox::new();
    let pip = sandbox.root.join("home/.cache/pip");
    fs::create_dir_all(pip.join("locked")).unwrap();
    fs::write(pip.join("three"), b"abc").unwrap();
    fs::write(pip.join("locked/kept"), b"x").unwrap();
    fs::set_permissions(pip.join("locked"), fs::Permissions::from_mode(0o500)).unwrap();
    settle();
    sandbox.settings(r#"{"version": 1, "deletionLog": true}"#);
    let output = sandbox.run(&["--clean", "--apps", "--min-age", "0", "--json"]);
    fs::set_permissions(pip.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
    let mut outcomes = json(&output)["clean"]["outcomes"]
        .as_array()
        .unwrap()
        .clone();
    outcomes.sort_by_key(|outcome| outcome["path"].as_str().unwrap().to_owned());
    let path = |relative: &str| pip.join(relative).to_string_lossy().into_owned();
    let expected = serde_json::json!([
        {
            "path": path("locked"),
            "result": "kept",
            "reason": "failed",
            "error": "PermissionDenied",
            "code": 13,
            "blocked": path("locked/kept"),
        },
        {"path": path("three"), "result": "removed", "bytes": 3},
    ]);
    assert_eq!(serde_json::Value::Array(outcomes.clone()), expected);
    let mut line = sandbox.history()[0].clone();
    let mut entries = line["entries"].as_array().unwrap().clone();
    entries.sort_by_key(|entry| entry["path"].as_str().unwrap().to_owned());
    assert_eq!(
        serde_json::Value::Array(entries),
        expected,
        "the deletion log"
    );
    let object = line.as_object_mut().unwrap();
    object.remove("entries");
    assert!(object.remove("time").unwrap().is_i64());
    assert_eq!(
        line,
        serde_json::json!({
            "version": 1,
            "stops": [],
            "removedBytes": 3,
            "removed": 1,
            "partial": 0,
            "gone": 0,
            "kept": 1,
            "complete": false,
        })
    );
}
