//! Tests of the helper, without root: the request's checks against the
//! embedded rules, and its operations on rules a test writes, whose targets
//! lie in a temporary directory.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use okapi_engine::{Cancel, Exclusions, Origin, ScanOptions, Watch, scan};
use okapi_rules::{Effect, Os, RuleFileKind, RuleSet, TemplateVariables, Variable};
use okapi_store::HistoryOutcome;
use serde_json::{Value, json};

use super::*;
use crate::operation::{cleanable, is_cancelled};
use crate::request::{MAX_REQUEST_BYTES, admin_targets};

/// The local machine of the tests' journals.
const MACHINE: &str = "0123456789abcdef0123456789abcdef";

fn machine() -> MachineId {
    MachineId::parse(MACHINE.as_bytes()).unwrap()
}

fn embedded() -> RuleSet {
    RuleSet::embedded(Os::Linux).unwrap()
}

/// A request line with the fields given, the others valid.
fn request_line(overrides: &Value) -> Vec<u8> {
    let mut request = json!({
        "version": PROTOCOL_VERSION,
        "operation": "clean",
        "targets": ["/var/crash"],
        "minAgeMinutes": 60,
        "exclusions": [],
    });
    for (field, value) in overrides.as_object().unwrap() {
        request[field] = value.clone();
    }
    serde_json::to_vec(&request).unwrap()
}

fn read(overrides: &Value) -> Result<Request, Refusal> {
    Request::read(&request_line(overrides), &embedded(), SystemTime::now())
}

fn refusal(overrides: &Value) -> Refusal {
    read(overrides).unwrap_err()
}

#[test]
fn refuses_any_target_but_those_of_its_rules_that_need_the_administrator() {
    for target in [
        "/etc",
        // A user's target.
        "${CACHE}/thumbnails",
        // An admin target written otherwise than in the rules.
        "/var/crash/",
        "/var//crash",
        "/var/crash/../..",
        "/VAR/crash",
    ] {
        assert_eq!(
            refusal(&json!({ "targets": [target] })),
            Refusal::UnknownTarget {
                target: target.to_owned()
            },
            "{target}"
        );
    }
    assert_eq!(
        refusal(&json!({ "targets": ["/var/crash", "/etc"] })),
        Refusal::UnknownTarget {
            target: "/etc".to_owned()
        },
        "one unknown target refuses the whole request"
    );
    assert_eq!(
        refusal(&json!({ "targets": [] })),
        Refusal::NoTarget,
        "no target is never all of them"
    );
    assert_eq!(
        refusal(&json!({ "targets": ["/var/crash", "/var/crash"] })),
        Refusal::DuplicateTarget {
            target: "/var/crash".to_owned()
        }
    );
}

#[test]
fn refuses_a_request_of_another_form_or_version() {
    assert_eq!(
        refusal(&json!({ "version": PROTOCOL_VERSION + 1, "added": true })),
        Refusal::UnsupportedVersion {
            version: PROTOCOL_VERSION + 1
        },
        "another version is said as such, whatever it adds"
    );
    // Version 2 had the helper resolve the exclusions: read as resolved, its
    // aliases would protect nothing.
    assert_eq!(
        refusal(&json!({ "version": 2 })),
        Refusal::UnsupportedVersion { version: 2 }
    );
    for overrides in [
        json!({ "paths": ["/var/crash"] }),
        json!({ "operation": "run" }),
        json!({ "minAgeMinutes": -1 }),
        json!({ "exclusions": "/var/crash" }),
    ] {
        assert_eq!(refusal(&overrides).key(), "invalid", "{overrides}");
    }
    let rules = embedded();
    let now = SystemTime::now();
    for line in [
        &b"not json"[..],
        b"{\"version\": 5}",
        b"",
        // The fields as an array, which serde accepts for a structure.
        b"[5, \"clean\", [\"/var/crash\"], 60, []]",
        // Complete but for its one byte that is not UTF-8.
        b"{\"version\": 5, \"operation\": \"clean\", \"targets\": [\"/var/\xff\"], \"minAgeMinutes\": 60, \"exclusions\": []}",
    ] {
        assert!(
            matches!(
                Request::read(line, &rules, now),
                Err(Refusal::Invalid { .. })
            ),
            "{line:?}"
        );
    }
    let mut long = request_line(&json!({}));
    long.resize(MAX_REQUEST_BYTES + 1, b' ');
    assert_eq!(Request::read(&long, &rules, now), Err(Refusal::TooLong));
    long.truncate(MAX_REQUEST_BYTES);
    Request::read(&long, &rules, now).expect("trailing spaces up to the limit are valid JSON");
}

#[test]
fn refuses_an_age_below_its_floor_or_beyond_the_clock() {
    for minutes in [0, 1, 59] {
        assert_eq!(
            refusal(&json!({ "minAgeMinutes": minutes })),
            Refusal::AgeBelowFloor { minutes }
        );
    }
    read(&json!({ "minAgeMinutes": 60 })).expect("the floor itself");
    assert_eq!(
        refusal(&json!({ "minAgeMinutes": u64::MAX })),
        Refusal::AgeBeyondClock { minutes: u64::MAX }
    );
    let request = read(&json!({ "minAgeMinutes": 90, "exclusions": ["/var/crash/keep"] })).unwrap();
    let now = SystemTime::now();
    assert_eq!(
        request.options(now),
        ScanOptions::new(
            now,
            Duration::from_mins(90),
            Exclusions::as_written([PathBuf::from("/var/crash/keep")]).unwrap()
        )
    );
}

#[test]
fn a_valid_request_names_the_targets_of_its_rules() {
    let rules = embedded();
    let request = Request::read(
        &request_line(&json!({
            "operation": "scan",
            "targets": ["/var/cache/apt/archives", "/var/crash", "/var/log/journal"],
        })),
        &rules,
        SystemTime::now(),
    )
    .unwrap();
    assert_eq!(request.operation(), Operation::Scan);
    let target = |path: &str| {
        rules
            .system()
            .clean_targets()
            .iter()
            .find(|target| match target.effect() {
                Effect::Delete(deletion) => deletion.path().to_string() == path,
                Effect::Maintenance { affected, .. } => affected.to_string() == path,
            })
            .map(Origin::of_target)
            .unwrap()
    };
    let crash = target("/var/crash");
    let journal = target("/var/log/journal");
    let selection = request.selection();
    assert!(selection.contains(&crash));
    assert_eq!(selection.name_of(&crash), Some("/var/crash"));
    assert!(selection.contains(&target("/var/cache/apt/archives")));
    assert!(!selection.contains(&target("/var/tmp")));
    assert_eq!(
        selection.journals().collect::<Vec<_>>(),
        [(&journal, "/var/log/journal")],
        "the journal's is a vacuum"
    );
}

/// The exclusions are compared as received, never read: one anywhere is no
/// refusal, one of a form that cannot be used is, and a link the caller
/// placed below a target gives the same answer, wherever it leads or whether
/// it leads anywhere.
#[test]
fn an_exclusion_is_never_read() {
    for anywhere in ["/root/x", "/proc/1/cwd", "/var/crashes", "/", "*.log"] {
        read(&json!({ "exclusions": [anywhere] }))
            .unwrap_or_else(|refusal| panic!("{anywhere}: {refusal}"));
    }
    for unusable in ["", "relative", "/var/crash/../..", "*."] {
        assert!(
            matches!(
                refusal(&json!({ "exclusions": [unusable] })),
                Refusal::Exclusion { .. }
            ),
            "{unusable}"
        );
    }
    let sandbox = Sandbox::new();
    sandbox.file("var/crash/core.1");
    fs::create_dir_all(sandbox.path("elsewhere/keep")).unwrap();
    std::os::unix::fs::symlink(sandbox.path("elsewhere"), sandbox.path("var/crash/live")).unwrap();
    std::os::unix::fs::symlink(sandbox.path("nowhere"), sandbox.path("var/crash/dangling"))
        .unwrap();
    let rules = sandbox.rules(&[("var/crash", "Crash", true)]);
    let answer_through = |link: &str| {
        let exclusion = sandbox.path(&format!("var/crash/{link}/keep"));
        let line = crate::request_line(
            Operation::Scan,
            &[sandbox.name("var/crash")],
            60,
            &[exclusion],
        )
        .unwrap();
        let now = SystemTime::now();
        let request = Request::read(&line, &rules, now).unwrap();
        let answer = operate(
            request.operation(),
            request.selection(),
            &rules,
            &sandbox.variables(),
            request.options(now),
            Ok(&machine()),
            &Watch::none(),
        );
        serde_json::to_value(answer).unwrap()
    };
    assert_eq!(answer_through("live"), answer_through("dangling"));
}

std::thread_local! {
    /// The traces written on this thread since its test asked for them.
    static CAPTURED: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Writes the traces of its thread to [`CAPTURED`].
struct ThisThread;

impl io::Write for ThisThread {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        CAPTURED.with_borrow_mut(|captured| captured.extend_from_slice(bytes));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Every trace `run` writes on this thread, as the caller reads them. One
/// subscriber for the whole run, every trace wanted: a subscriber of one
/// test alone misses a trace a test alongside met first without one.
fn traces_of(run: impl FnOnce()) -> String {
    static SUBSCRIBED: std::sync::Once = std::sync::Once::new();
    SUBSCRIBED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| ThisThread)
            .with_max_level(tracing::Level::TRACE)
            .without_time()
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
    });
    CAPTURED.with_borrow_mut(Vec::clear);
    run();
    String::from_utf8(CAPTURED.with_borrow_mut(std::mem::take)).unwrap()
}

/// A refusal is traced, with its name and what it names.
#[test]
fn a_request_refused_is_traced() {
    let traces = traces_of(|| {
        refusal(&json!({ "exclusions": ["relative"] }));
    });
    assert!(traces.contains("request refused"), "{traces}");
    assert!(traces.contains("refused=\"exclusion\""), "{traces}");
    assert!(!traces.contains("request accepted"), "{traces}");
}

/// What the caller reads of a request read and carried out, its every
/// trace: never where an exclusion leads, nor its identity.
#[test]
fn the_traces_never_tell_where_an_exclusion_leads() {
    let sandbox = Sandbox::new();
    sandbox.file("var/crash/core.1");
    let elsewhere = sandbox.path("elsewhere");
    // There, so that its identity would be read if it were resolved.
    fs::create_dir_all(elsewhere.join("keep")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, sandbox.path("var/crash/alias")).unwrap();
    let rules = sandbox.rules(&[("var/crash", "Crash", true)]);
    let line = crate::request_line(
        Operation::Clean,
        &[sandbox.name("var/crash")],
        60,
        &[sandbox.path("var/crash/alias/keep")],
    )
    .unwrap();
    let traces = traces_of(|| {
        let now = SystemTime::now();
        let request = Request::read(&line, &rules, now).unwrap();
        let answer = operate(
            request.operation(),
            request.selection(),
            &rules,
            &sandbox.variables(),
            request.options(now),
            Ok(&machine()),
            &Watch::none(),
        );
        assert_eq!(answer.exit(), Exit::Success, "{answer:?}");
    });
    assert!(traces.contains("request accepted"), "{traces}");
    assert!(
        traces.contains("var/crash/alias/keep"),
        "as received: {traces}"
    );
    assert!(
        traces.contains("held back"),
        "the operation's traces are read too: {traces}"
    );
    assert!(!traces.contains("elsewhere"), "{traces}");
    let metadata = fs::symlink_metadata(elsewhere.join("keep")).unwrap();
    let identity = format!("{:?}", okapi_platform::file_identity(&metadata).unwrap());
    assert!(!traces.contains(&identity), "{identity} in {traces}");
}

/// A Linux target that needs the administrator and holds a variable, or
/// runs a maintenance the helper does not do, could never be asked for: none
/// does, so none is silently out of reach.
#[test]
fn every_linux_target_that_needs_the_administrator_can_be_asked_for() {
    let rules = embedded();
    let deleting: Vec<_> = rules
        .system()
        .clean_targets()
        .iter()
        .filter(|target| target.needs_admin())
        .map(Origin::of_target)
        .collect();
    assert!(deleting.len() > 1);
    let allowed = admin_targets(&rules);
    assert_eq!(allowed.len(), deleting.len());
    assert!(
        deleting
            .iter()
            .all(|origin| allowed.values().any(|allowed| allowed == origin))
    );
}

#[test]
fn reads_one_line_and_tells_one_too_long() {
    let mut input = &b"{\"version\": 1}\nrest"[..];
    assert_eq!(read_request(&mut input).unwrap(), b"{\"version\": 1}");
    assert_eq!(input, b"rest", "nothing read beyond the line");
    let mut unended = &b"{}"[..];
    assert_eq!(read_request(&mut unended).unwrap(), b"{}");
    let long = vec![b' '; MAX_REQUEST_BYTES + 10];
    let line = read_request(&mut &long[..]).unwrap();
    assert_eq!(
        line.len(),
        MAX_REQUEST_BYTES + 1,
        "one byte past the limit, no more"
    );
}

#[test]
fn runs_only_as_root_really_and_effectively() {
    let (root, user) = (Some(0), Some(1_000));
    assert!(runs_as_root(root, root));
    assert!(
        !runs_as_root(user, root),
        "set-user-ID: the real user is not root"
    );
    for (real, effective) in [(root, user), (user, user), (root, None), (None, root)] {
        assert!(!runs_as_root(real, effective), "{real:?} {effective:?}");
    }
}

/// The codes the caller tells the outcomes by.
#[test]
fn each_exit_keeps_its_code() {
    let codes = [Exit::Success, Exit::Failure, Exit::Refused, Exit::NotRoot].map(Exit::code);
    assert_eq!(codes, [0, 1, 2, 3]);
}

#[test]
fn the_end_of_its_input_cancels_the_helper() {
    for input in [Vec::new(), b"after the request".to_vec()] {
        let cancel = Cancel::new();
        cancel_at_end_of(io::Cursor::new(input), cancel.clone())
            .join()
            .unwrap();
        assert!(cancel.is_cancelled());
    }
}

/// The names the caller reads a refusal by, and the target it names.
#[test]
fn each_refusal_keeps_its_name() {
    let target = || "/etc".to_owned();
    let refusals = [
        (Refusal::TooLong, "tooLong"),
        (
            Refusal::Invalid {
                detail: String::new(),
            },
            "invalid",
        ),
        (
            Refusal::UnsupportedVersion { version: 2 },
            "unsupportedVersion",
        ),
        (Refusal::NoTarget, "noTarget"),
        (Refusal::UnknownTarget { target: target() }, "unknownTarget"),
        (
            Refusal::DuplicateTarget { target: target() },
            "duplicateTarget",
        ),
        (Refusal::AgeBelowFloor { minutes: 1 }, "ageBelowFloor"),
        (Refusal::AgeBeyondClock { minutes: 1 }, "ageBeyondClock"),
        (
            Refusal::Exclusion {
                error: okapi_engine::ExclusionError::Empty,
            },
            "exclusion",
        ),
    ];
    for (refusal, name) in refusals {
        let answer = serde_json::to_value(RefusalAnswer::of(&refusal)).unwrap();
        assert_eq!(answer["version"], PROTOCOL_VERSION);
        assert_eq!(answer["helperVersion"], env!("CARGO_PKG_VERSION"));
        assert_eq!(answer["refused"], name);
        assert_eq!(answer["detail"], refusal.to_string());
        let names_a_target = matches!(
            refusal,
            Refusal::UnknownTarget { .. } | Refusal::DuplicateTarget { .. }
        );
        assert_eq!(
            answer.get("target").cloned(),
            names_a_target.then(|| json!("/etc")),
            "{name}"
        );
    }
}

/// Rules whose system targets lie in a temporary directory, the variables
/// there too.
struct Sandbox {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn name(&self, relative: &str) -> String {
        self.path(relative).display().to_string()
    }

    /// A file of 7 bytes.
    fn file(&self, relative: &str) -> PathBuf {
        self.file_of(relative, 7)
    }

    fn file_of(&self, relative: &str, size: usize) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; size]).unwrap();
        path
    }

    /// The embedded rules, the system targets replaced by `targets`: each
    /// `(relative path, subcategory, needs the administrator)`.
    fn rules(&self, targets: &[(&str, &str, bool)]) -> RuleSet {
        let targets: Vec<Value> = targets
            .iter()
            .map(|(relative, subcategory, admin)| {
                json!({
                    "path": self.path(relative),
                    "subcategory": subcategory,
                    "needsAdmin": admin,
                })
            })
            .collect();
        let system = json!({ "type": "system", "cleanTargets": targets });
        RuleSet::embedded_with(Os::Linux, RuleFileKind::System, &system.to_string()).unwrap()
    }

    /// [`Self::rules`], with the journal's vacuum at `journal`.
    fn rules_with_journal(&self, targets: &[(&str, &str, bool)], journal: &str) -> RuleSet {
        let mut all: Vec<Value> = targets
            .iter()
            .map(|(relative, subcategory, admin)| {
                json!({
                    "path": self.path(relative),
                    "subcategory": subcategory,
                    "needsAdmin": admin,
                })
            })
            .collect();
        all.push(json!({
            "path": self.path(journal),
            "subcategory": "Journal Logs",
            "needsAdmin": true,
            "cleanupAction": "journal-vacuum",
        }));
        let system = json!({ "type": "system", "cleanTargets": all });
        RuleSet::embedded_with(Os::Linux, RuleFileKind::System, &system.to_string()).unwrap()
    }

    fn variables(&self) -> TemplateVariables {
        let mut variables = TemplateVariables::new();
        for (variable, relative) in [
            (Variable::Home, "home"),
            (Variable::TempDirectory, "tmp"),
            (Variable::Config, "home/.config"),
            (Variable::Cache, "home/.cache"),
            (Variable::LocalShare, "home/.local/share"),
        ] {
            let value = self.path(relative);
            fs::create_dir_all(&value).unwrap();
            variables.insert(variable, value).unwrap();
        }
        variables
    }

    fn selection(&self, rules: &RuleSet, relatives: &[&str]) -> Selection {
        let names: Vec<String> = relatives
            .iter()
            .map(|relative| self.name(relative))
            .collect();
        Selection::of(rules, &names).unwrap()
    }
}

/// Every age offered: a test cannot set a file's change time back.
fn options_now() -> ScanOptions {
    ScanOptions::new(SystemTime::now(), Duration::ZERO, Exclusions::none())
}

fn operate_on(sandbox: &Sandbox, rules: &RuleSet, operation: Operation, asked: &[&str]) -> Value {
    let answer = operate(
        operation,
        &sandbox.selection(rules, asked),
        rules,
        &sandbox.variables(),
        options_now(),
        Ok(&machine()),
        &Watch::none(),
    );
    assert_eq!(answer.exit(), Exit::Success, "{answer:?}");
    serde_json::to_value(answer).unwrap()
}

#[test]
fn a_cleaning_removes_what_its_targets_offer_and_says_so() {
    let sandbox = Sandbox::new();
    let small = sandbox.file_of("var/crash/core.1", 3);
    let large = sandbox.file_of("var/crash/core.2", 11);
    let dump = sandbox.file_of("var/dumps/core", 5);
    let other = sandbox.file("var/other/core");
    let ordinary = sandbox.file("var/user/kept");
    let rules = sandbox.rules(&[
        ("var/crash", "Crash", true),
        ("var/dumps", "Dumps", true),
        ("var/other", "Other", true),
        ("var/user", "User", false),
    ]);
    let asked = ["var/crash", "var/dumps"];

    let scanned = operate_on(&sandbox, &rules, Operation::Scan, &asked);
    assert_eq!(scanned["operation"], "scan");
    assert_eq!(
        scanned["groups"],
        json!([
            { "targets": [sandbox.name("var/crash")], "entries": 2, "bytes": 14 },
            { "targets": [sandbox.name("var/dumps")], "entries": 1, "bytes": 5 },
        ])
    );
    assert!(scanned.get("outcomes").is_none(), "a scan removes nothing");
    assert!(small.exists());

    let cleaned = operate_on(&sandbox, &rules, Operation::Clean, &asked);
    assert_eq!(cleaned["version"], PROTOCOL_VERSION);
    assert_eq!(cleaned["operation"], "clean");
    assert_eq!(cleaned["stops"], json!([]));
    let removed = |path: &Path, bytes: u64| json!({ "outcome": { "path": path.display().to_string(), "result": "removed", "bytes": bytes }, "scannedBytes": bytes });
    // In the order the directories list their entries.
    let mut outcomes = cleaned["outcomes"].as_array().unwrap().clone();
    outcomes.sort_by_key(|outcome| outcome["outcome"]["path"].as_str().unwrap().to_owned());
    assert_eq!(
        outcomes,
        [removed(&small, 3), removed(&large, 11), removed(&dump, 5)]
    );
    assert!(!small.exists() && !large.exists() && !dump.exists());
    assert!(other.exists(), "an admin target not asked for");
    assert!(
        ordinary.exists(),
        "a target that does not need the administrator"
    );
}

/// The journal asked for beside a target that deletes: root's scan says what
/// the vacuum would remove, the cleaning removes it, never the journal being
/// written; each with its outcome.
#[test]
fn the_journal_is_vacuumed_beside_the_targets_asked_for() {
    use std::os::unix::fs::MetadataExt;

    let sandbox = Sandbox::new();
    let crash = sandbox.file_of("var/crash/core", 3);
    let machine = format!("var/log/journal/{MACHINE}");
    let active = sandbox.file(&format!("{machine}/system.journal"));
    // Its first entry on 2020-01-01: past any age.
    let old = sandbox.file_of(
        &format!(
            "{machine}/system@{:032x}-{:016x}-{:016x}.journal",
            1, 1, 1_577_836_800_000_000_u64
        ),
        5000,
    );
    let usage = fs::metadata(&old).unwrap().blocks() * 512;
    let rules = sandbox.rules_with_journal(&[("var/crash", "Crash", true)], "var/log/journal");
    let asked = ["var/crash", "var/log/journal"];

    let scanned = operate_on(&sandbox, &rules, Operation::Scan, &asked);
    assert_eq!(
        scanned["groups"],
        json!([
            { "targets": [sandbox.name("var/crash")], "entries": 1, "bytes": 3 },
            { "targets": [sandbox.name("var/log/journal")], "entries": 1, "bytes": usage },
        ])
    );
    assert!(old.exists(), "a scan removes nothing");

    let only_crash = operate_on(&sandbox, &rules, Operation::Scan, &["var/crash"]);
    assert_eq!(
        only_crash["groups"].as_array().unwrap().len(),
        1,
        "the journal not asked for is not looked at"
    );
    // Asked for, the journal is the vacuum's alone: the engine, which would
    // resolve its place as root to offer it, is never asked of it.
    let restricted = sandbox.selection(&rules, &asked).restrict(options_now());
    assert_eq!(
        scan(&rules, &sandbox.variables(), &restricted).maintenances(),
        []
    );

    let cleaned = operate_on(&sandbox, &rules, Operation::Clean, &asked);
    assert_eq!(cleaned["stops"], json!([]));
    assert_eq!(
        cleaned["outcomes"],
        json!([
            { "outcome": { "path": crash.display().to_string(), "result": "removed", "bytes": 3 }, "scannedBytes": 3 },
            { "outcome": { "path": old.display().to_string(), "result": "removed", "bytes": usage }, "scannedBytes": usage },
        ])
    );
    assert!(!old.exists() && !crash.exists());
    assert!(active.exists(), "the journal being written");
}

/// A journal's root that is a link: said for its target, nothing removed.
#[test]
fn a_journal_out_of_reach_is_said_for_its_target() {
    let sandbox = Sandbox::new();
    let elsewhere = sandbox.path(&format!("elsewhere/{MACHINE}"));
    fs::create_dir_all(&elsewhere).unwrap();
    let old = elsewhere.join(format!(
        "system@{:032x}-{:016x}-{:016x}.journal",
        1, 1, 1_577_836_800_000_000_u64
    ));
    fs::write(&old, b"old").unwrap();
    fs::create_dir_all(sandbox.path("var/log")).unwrap();
    std::os::unix::fs::symlink(sandbox.path("elsewhere"), sandbox.path("var/log/journal")).unwrap();
    let rules = sandbox.rules_with_journal(&[], "var/log/journal");

    let cleaned = operate_on(&sandbox, &rules, Operation::Clean, &["var/log/journal"]);
    assert_eq!(cleaned["groups"], json!([]));
    assert_eq!(cleaned["outcomes"], json!([]));
    let issues = cleaned["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0]["kind"], "unsafeRoot");
    assert_eq!(
        issues[0]["targets"],
        json!([sandbox.name("var/log/journal")])
    );
    assert!(old.exists());
}

#[test]
fn a_group_that_joins_a_rule_not_asked_for_is_withheld_and_said() {
    let sandbox = Sandbox::new();
    let shared = sandbox.file("var/shared/core");
    let alone = sandbox.file("var/alone/core");
    let left = sandbox.file("var/left/core");
    let ordinary = sandbox.file("var/user/kept");
    // One root for an admin target and an ordinary one, reached by a link.
    std::os::unix::fs::symlink(sandbox.path("var"), sandbox.path("alias")).unwrap();
    let rules = sandbox.rules(&[
        ("var/shared", "Shared", true),
        ("alias/shared", "Shared too", false),
        ("var/alone", "Alone", true),
        ("var/left", "Left", true),
        ("var/user", "User", false),
    ]);
    let asked = ["var/shared", "var/alone"];
    let selection = sandbox.selection(&rules, &asked);
    // Scanned with no choice of rules: every group is in the report.
    let report = scan(&rules, &sandbox.variables(), &options_now());
    let offered: BTreeSet<PathBuf> = report
        .groups()
        .iter()
        .flat_map(okapi_engine::ScanGroup::items)
        .map(|item| item.path().to_path_buf())
        .collect();
    assert!(
        [&shared, &alone, &left, &ordinary]
            .iter()
            .all(|path| offered.contains(*path)),
        "{offered:?}"
    );
    let cleanable: Vec<&Path> = cleanable(&report, &selection)
        .into_iter()
        .map(|id| report.find(id).unwrap().1.path())
        .collect();
    assert_eq!(cleanable, [alone.as_path()]);

    // Root walks the roots asked for alone: the shared one and `alone`.
    let (sender, progress) = std::sync::mpsc::channel();
    let _ = operate(
        Operation::Scan,
        &selection,
        &rules,
        &sandbox.variables(),
        options_now(),
        Ok(&machine()),
        &Watch::new(Cancel::new(), Some(sender)),
    );
    let planned = progress.try_iter().find_map(|progress| match progress {
        okapi_engine::Progress::Planned { total, .. } => Some(total),
        _ => None,
    });
    assert_eq!(planned, Some(2));

    let scanned = operate_on(&sandbox, &rules, Operation::Scan, &asked);
    assert_eq!(
        scanned["groups"],
        json!([{ "targets": [sandbox.name("var/alone")], "entries": 1, "bytes": 7 }]),
        "a scan offers what the cleaning removes, no more"
    );
    assert_eq!(
        scanned["issues"],
        json!([{ "kind": "withheld", "targets": [sandbox.name("var/shared")] }])
    );
    let cleaned = operate_on(&sandbox, &rules, Operation::Clean, &asked);
    assert_eq!(
        cleaned["outcomes"].as_array().unwrap().len(),
        1,
        "{cleaned}"
    );
    assert!(shared.exists(), "its group joins a rule not asked for");
    assert!(!alone.exists());
}

#[test]
fn a_partial_cleaning_is_a_complete_answer() {
    if okapi_platform::effective_user() == Some(okapi_platform::ROOT_USER) {
        eprintln!("skipped: root removes from a read-only directory");
        return;
    }
    let sandbox = Sandbox::new();
    let free = sandbox.file("var/crash/free");
    let locked = sandbox.file("var/crash/locked/core");
    let locked_directory = locked.parent().unwrap();
    fs::set_permissions(locked_directory, fs::Permissions::from_mode(0o555)).unwrap();
    let rules = sandbox.rules(&[("var/crash", "Crash", true)]);
    let answer = operate(
        Operation::Clean,
        &sandbox.selection(&rules, &["var/crash"]),
        &rules,
        &sandbox.variables(),
        options_now(),
        Ok(&machine()),
        &Watch::none(),
    );
    fs::set_permissions(locked_directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(answer.exit(), Exit::Success, "{answer:?}");
    let answer = serde_json::to_value(answer).unwrap();
    assert_eq!(answer["stops"], json!([]));
    let outcomes: BTreeMap<String, Value> = answer["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|outcome| {
            (
                outcome["outcome"]["path"].as_str().unwrap().to_owned(),
                outcome["outcome"].clone(),
            )
        })
        .collect();
    assert_eq!(outcomes[&free.display().to_string()]["result"], "removed");
    let kept = &outcomes[&locked_directory.display().to_string()];
    assert_eq!(kept["result"], "kept", "{kept}");
    assert_eq!(kept["reason"], "failed");
    assert_eq!(kept["code"], 13, "EACCES, for the caller to tell its kind");
    assert!(!free.exists());
    assert!(locked.exists());
}

#[test]
fn the_answer_keeps_the_issues_of_its_targets_only() {
    let sandbox = Sandbox::new();
    sandbox.file("var/crash/core");
    // Two roots that are links: each an issue of its own rule.
    let beyond = sandbox.file("elsewhere/core");
    for (link, target) in [("var/linked", "elsewhere"), ("var/other", "further")] {
        fs::create_dir_all(sandbox.path(target)).unwrap();
        std::os::unix::fs::symlink(sandbox.path(target), sandbox.path(link)).unwrap();
    }
    let rules = sandbox.rules(&[
        ("var/crash", "Crash", true),
        ("var/linked", "Linked", true),
        ("var/other", "Other", true),
    ]);
    let all = scan(&rules, &sandbox.variables(), &options_now());
    assert!(all.issues().len() >= 2, "{:?}", all.issues());
    let answer = operate_on(
        &sandbox,
        &rules,
        Operation::Scan,
        &["var/crash", "var/linked"],
    );
    let linked = sandbox.name("var/linked");
    assert_eq!(
        answer["issues"],
        json!([{ "kind": "unsafeRoot", "targets": [linked], "path": linked, "detail": "Link" }])
    );
    let cleaned = operate_on(&sandbox, &rules, Operation::Clean, &["var/linked"]);
    assert_eq!(cleaned["outcomes"], json!([]));
    assert!(beyond.exists(), "never through the link (invariant 3)");
}

#[test]
fn a_scan_stopped_cleans_nothing_and_says_why() {
    let sandbox = Sandbox::new();
    let crash = sandbox.file("var/crash/core");
    let rules = sandbox.rules(&[("var/crash", "Crash", true)]);
    let cancel = Cancel::new();
    cancel.cancel();
    let answer = operate(
        Operation::Clean,
        &sandbox.selection(&rules, &["var/crash"]),
        &rules,
        &sandbox.variables(),
        options_now(),
        Ok(&machine()),
        &Watch::new(cancel, None),
    );
    assert_eq!(answer.exit(), Exit::Failure);
    let answer = serde_json::to_value(answer).unwrap();
    assert_eq!(answer["stops"], json!(["cancelled"]));
    assert_eq!(
        answer["issues"],
        json!([{ "kind": "stopped", "stop": "cancelled", "targets": [] }]),
        "a stop concerns every target"
    );
    assert_eq!(answer["outcomes"], json!([]));
    assert!(crash.exists());
}

/// A cleaning cancelled midway: the engine keeps the rest, entry by entry,
/// which the answer says as a stop.
#[test]
fn a_cleaning_cancelled_midway_is_stopped() {
    let outcome = |result: &str, reason: Option<&str>| HistoryOutcome {
        path: Some("/var/crash/core".to_owned()),
        result: result.to_owned(),
        bytes: None,
        reason: reason.map(str::to_owned),
        error: None,
        code: None,
        blocked: None,
        inexact: false,
    };
    assert!(is_cancelled(
        [outcome("removed", None), outcome("kept", Some("cancelled"))].iter()
    ));
    assert!(!is_cancelled(
        [outcome("removed", None), outcome("kept", Some("recent"))].iter()
    ));
    assert!(!is_cancelled([].iter()));
}

/// Invariant 4: the crates `Cargo.lock` lists for the helper, by name (its
/// dev-dependencies and other systems' crates included, as the lock file has
/// them). A crate added to its graph is reviewed here first; what a crate
/// already listed does is clippy's `disallowed-*` for the workspace's own,
/// and the review of the lock file's changes for the others.
#[test]
fn the_helper_depends_only_on_the_crates_reviewed() {
    let lock = include_str!("../../../Cargo.lock");
    let mut dependencies: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for package in lock.split("[[package]]").skip(1) {
        let mut lines = package.lines();
        let Some(name) = lines.find_map(|line| line.strip_prefix("name = \"")) else {
            continue;
        };
        let name = name.trim_end_matches('"');
        let listed = package
            .split_once("dependencies = [")
            .map_or("", |(_, rest)| {
                rest.split_once(']').map_or("", |(list, _)| list)
            });
        dependencies.entry(name).or_default().extend(
            listed
                .lines()
                .filter_map(|line| line.trim().strip_prefix('"'))
                .filter_map(|line| line.split(['"', ' ']).next()),
        );
    }
    let mut linked = BTreeSet::new();
    let mut pending = vec!["okapi-helper"];
    while let Some(name) = pending.pop() {
        if linked.insert(name) {
            pending.extend(dependencies.get(name).into_iter().flatten().copied());
        }
    }
    let linked: Vec<&str> = linked.into_iter().collect();
    assert_eq!(linked, REVIEWED_CRATES);
}

/// What the helper depends on, reviewed: Okapi's crates and everything their
/// dependencies bring, system-specific and test-only ones included.
const REVIEWED_CRATES: &[&str] = &[
    "bitflags",
    "cfg-if",
    "critical-section",
    "errno",
    "fastrand",
    "getrandom",
    "itoa",
    "lazy_static",
    "libc",
    "linux-raw-sys",
    "log",
    "memchr",
    "okapi-engine",
    "okapi-helper",
    "okapi-platform",
    "okapi-rules",
    "okapi-store",
    "once_cell",
    "pin-project-lite",
    "portable-atomic",
    "proc-macro2",
    "quote",
    "r-efi",
    "rustix",
    "serde",
    "serde_core",
    "serde_derive",
    "serde_json",
    "sharded-slab",
    "syn",
    "tempfile",
    "thiserror",
    "thiserror-impl",
    "thread_local",
    "tracing",
    "tracing-attributes",
    "tracing-core",
    "tracing-log",
    "tracing-serde",
    "tracing-subscriber",
    "unicode-ident",
    "valuable",
    "wasip2",
    "windows-link",
    "windows-sys",
    "windows-targets",
    "windows_aarch64_gnullvm",
    "windows_aarch64_msvc",
    "windows_i686_gnu",
    "windows_i686_gnullvm",
    "windows_i686_msvc",
    "windows_x86_64_gnu",
    "windows_x86_64_gnullvm",
    "windows_x86_64_msvc",
    "wit-bindgen",
    "zmij",
];

/// The policy lets pkexec run the helper where it is installed, and only
/// asks an active session's administrator; both scripts install and remove
/// the same two files.
#[test]
fn the_policy_and_the_scripts_name_the_installed_helper() {
    const POLICY_FILE: &str = "io.github.cryborg.okapi.policy";
    let policy = include_str!("../../../packaging/linux/io.github.cryborg.okapi.policy");
    // What polkit reads: the comments left out.
    let read: String = policy
        .split("<!--")
        .enumerate()
        .map(|(index, part)| {
            if index == 0 {
                part
            } else {
                part.split_once("-->").map_or("", |(_, after)| after)
            }
        })
        .collect();
    let path = okapi_platform::HELPER_PATH;
    for element in [
        format!("<annotate key=\"org.freedesktop.policykit.exec.path\">{path}</annotate>"),
        "<allow_any>no</allow_any>".to_owned(),
        "<allow_inactive>no</allow_inactive>".to_owned(),
        "<allow_active>auth_admin_keep</allow_active>".to_owned(),
    ] {
        assert_eq!(read.matches(&element).count(), 1, "{element}");
    }
    for tag in [
        "<action id=",
        "<allow_any>",
        "<allow_inactive>",
        "<allow_active>",
    ] {
        assert_eq!(read.matches(tag).count(), 1, "{tag}");
    }
    let install = include_str!("../../../packaging/linux/install-helper.sh");
    let uninstall = include_str!("../../../packaging/linux/uninstall-helper.sh");
    for (script, steps) in [(install, 3), (uninstall, 2)] {
        assert!(script.contains(&format!("readonly HELPER={path}\n")));
        assert!(script.contains(&format!(
            "readonly POLICY=/usr/share/polkit-1/actions/{POLICY_FILE}\n"
        )));
        assert!(!script.contains("chmod") && !script.contains("chown"));
        let as_root = script
            .lines()
            .filter(|line| line.trim_start().starts_with("sudo "))
            .count();
        assert_eq!(as_root, steps, "what runs as root");
    }
    assert!(install.contains("--package okapi-helper --target \"$host\""));
    // The policy first; then root's alone, the binary pkexec runs as root and
    // its directory: nobody else may replace it.
    let steps = [
        format!(
            "sudo install -D --owner=root --group=root --mode=0644 \\\n    \"$workspace/packaging/linux/{POLICY_FILE}\" \"$POLICY\"\n"
        ),
        "sudo install -d --owner=root --group=root --mode=0755 \"$(dirname -- \"$HELPER\")\"\n"
            .to_owned(),
        "sudo install --owner=root --group=root --mode=0755 \\\n    \"$target/$host/release/okapi-helper\" \"$HELPER\"\n".to_owned(),
    ];
    let positions: Vec<Option<usize>> = steps.iter().map(|step| install.find(step)).collect();
    assert!(positions.iter().all(Option::is_some), "{steps:?}");
    assert!(positions.is_sorted(), "in this order");
    assert!(uninstall.contains("sudo rm -f -- \"$HELPER\" \"$POLICY\"\n"));
    assert!(
        !uninstall.contains("rm -r"),
        "never anything but the two files"
    );
}

/// The caller writes the request with the type the helper reads it with.
#[test]
fn the_request_line_the_caller_writes_is_the_one_the_helper_reads() {
    let targets = vec![
        "/var/cache/apt/archives".to_owned(),
        "/var/crash".to_owned(),
    ];
    let exclusions = vec![PathBuf::from("/var/crash/keep")];
    let line = crate::request_line(Operation::Clean, &targets, 90, &exclusions).unwrap();
    let now = SystemTime::now();
    let request = Request::read(&line, &embedded(), now).unwrap();
    assert_eq!(request.operation(), Operation::Clean);
    assert_eq!(
        request.options(now),
        ScanOptions::new(
            now,
            Duration::from_mins(90),
            Exclusions::as_written(exclusions.clone()).unwrap()
        )
    );
    assert!(targets.iter().all(|target| {
        admin_targets(&embedded())
            .get(target)
            .is_some_and(|origin| request.selection().contains(origin))
    }));
    assert!(!line.contains(&b'\n'), "one line");
    let many: Vec<String> = (0..MAX_REQUEST_BYTES)
        .map(|_| "/var/crash".to_owned())
        .collect();
    assert_eq!(
        crate::request_line(Operation::Scan, &many, 60, &[]),
        Err(Refusal::TooLong),
        "never sent: the helper would refuse it"
    );
}

/// What the caller reads back of each way a run of the helper ends.
#[test]
fn the_caller_reads_back_each_reply() {
    let sandbox = Sandbox::new();
    sandbox.file("var/crash/core");
    let rules = sandbox.rules(&[("var/crash", "Crash", true)]);
    let answer = operate(
        Operation::Scan,
        &sandbox.selection(&rules, &["var/crash"]),
        &rules,
        &sandbox.variables(),
        options_now(),
        Ok(&machine()),
        &Watch::none(),
    );
    let written = serde_json::to_vec(&answer).unwrap();
    assert_eq!(
        read_reply(Operation::Scan, Some(0), &written),
        Ok(Reply::Answer(answer.clone())),
        "read back as written"
    );
    assert!(
        matches!(
            read_reply(Operation::Clean, Some(0), &written),
            Err(ReplyError::Unreadable { .. })
        ),
        "an answer to another operation"
    );
    assert!(
        matches!(
            read_reply(Operation::Scan, Some(1), &written),
            Err(ReplyError::Unreadable { .. })
        ),
        "a code its answer does not end with"
    );
    let mut older: Value = serde_json::from_slice(&written).unwrap();
    older["version"] = json!(1);
    assert_eq!(
        read_reply(Operation::Scan, Some(0), older.to_string().as_bytes()),
        Err(ReplyError::OtherVersion { version: 1 })
    );
    let refusal = RefusalAnswer::of(&Refusal::UnsupportedVersion { version: 2 });
    assert_eq!(
        read_reply(
            Operation::Scan,
            Some(2),
            &serde_json::to_vec(&refusal).unwrap()
        ),
        Ok(Reply::Refused(refusal)),
    );
    for (code, error) in [
        (Some(PKEXEC_DISMISSED), ReplyError::Dismissed),
        (Some(PKEXEC_NOT_AUTHORIZED), ReplyError::NotAuthorized),
        (Some(3), ReplyError::NotRoot),
        (Some(PANICKED), ReplyError::Panicked),
        (None, ReplyError::Killed),
        (Some(7), ReplyError::UnexpectedCode { code: 7 }),
    ] {
        assert_eq!(
            read_reply(Operation::Scan, code, b""),
            Err(error),
            "{code:?}"
        );
    }
    assert!(
        matches!(
            read_reply(Operation::Scan, Some(1), b""),
            Err(ReplyError::Unreadable { code: 1, .. })
        ),
        "a failure without an answer"
    );
    assert!(matches!(
        read_reply(Operation::Scan, Some(2), b"not json"),
        Err(ReplyError::Unreadable { code: 2, .. })
    ));
    // A cleaning that says nothing of its entries is no answer to act on.
    let silent = json!({
        "version": PROTOCOL_VERSION, "operation": "clean", "stops": [], "groups": [], "issues": [],
    })
    .to_string();
    assert!(matches!(
        read_reply(Operation::Clean, Some(0), silent.as_bytes()),
        Err(ReplyError::Unreadable { .. })
    ));
}

/// A stopped answer ends with the failure's code, and only with it.
#[test]
fn a_stopped_answer_is_read_with_its_code() {
    let stopped = json!({
        "version": PROTOCOL_VERSION,
        "operation": "scan",
        "stops": ["cancelled"],
        "groups": [],
        "issues": [],
    })
    .to_string();
    let Ok(Reply::Answer(answer)) = read_reply(Operation::Scan, Some(1), stopped.as_bytes()) else {
        panic!("a stopped answer is an answer");
    };
    assert_eq!(answer.stops(), ["cancelled"]);
    assert!(matches!(
        read_reply(Operation::Scan, Some(0), stopped.as_bytes()),
        Err(ReplyError::Unreadable { .. })
    ));
}

/// The refusal of a helper of another version, fields it adds included,
/// asks to install it again.
#[test]
fn a_refusal_of_any_version_is_read() {
    let older = br#"{"version": 1, "helperVersion": "0.1.0", "refused": "unsupportedVersion",
                     "detail": "the request is of version 2", "added": true}"#;
    let Ok(Reply::Refused(refusal)) = read_reply(Operation::Clean, Some(2), older) else {
        panic!("a refusal of another version is a refusal");
    };
    assert!(refusal.asks_to_reinstall());
    assert_eq!(refusal.helper_version(), "0.1.0");
    let exclusion = RefusalAnswer::of(&Refusal::Exclusion {
        error: okapi_engine::ExclusionError::Empty,
    });
    assert!(!exclusion.asks_to_reinstall());
}
