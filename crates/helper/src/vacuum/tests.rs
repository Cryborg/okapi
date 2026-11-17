//! The vacuum on journals a test writes, without root: the owner admitted
//! is then the test's, the root's own.

use std::os::unix::fs::symlink;
use std::time::UNIX_EPOCH;

use okapi_engine::{Cancel, Exclusions};
use okapi_rules::{Label, TemplatePath};

use super::*;

const MACHINE: &str = "0123456789abcdef0123456789abcdef";
/// 2020-01-01, in seconds since the clock's origin: old for any test.
const LONG_AGO: u64 = 1_577_836_800;

/// The name of an archived journal whose first entry is `days` days after
/// [`LONG_AGO`], of sequence `id` at number `number`.
fn archived(id: u128, number: u64, days: u64) -> String {
    let realtime = (LONG_AGO + days * SECONDS_PER_DAY) * 1_000_000;
    format!("system@{id:032x}-{number:016x}-{realtime:016x}.journal")
}

fn corrupted(days: u64) -> String {
    let realtime = (LONG_AGO + days * SECONDS_PER_DAY) * 1_000_000;
    format!("system@{realtime:016x}-00000000000000ab.journal~")
}

/// A journal's root in a temporary directory, with one machine's directory.
struct Journal {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Journal {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap().join("journal");
        fs::create_dir_all(root.join(MACHINE)).unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    fn machine(&self) -> PathBuf {
        self.root.join(MACHINE)
    }

    /// A journal of the machine, of 10 000 bytes.
    fn write(&self, name: &str) -> PathBuf {
        self.write_of(name, 10_000)
    }

    fn write_of(&self, name: &str, size: usize) -> PathBuf {
        let path = self.machine().join(name);
        fs::write(&path, vec![b'j'; size]).unwrap();
        path
    }

    /// Dates the content of the journal at `path` long ago.
    fn age(path: &Path) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(LONG_AGO))
            .unwrap();
    }

    /// The room a journal of [`Self::write`] takes, as the vacuum counts it.
    fn usage(&self, name: &str) -> u64 {
        usage(
            &Directory::open(&self.machine())
                .unwrap()
                .status_of(OsStr::new(name))
                .unwrap(),
        )
    }

    fn plan(&self, options: &ScanOptions, limits: Limits) -> Plan {
        plan(
            &self.root,
            Ok(&MachineId(MACHINE.to_owned())),
            &origin(),
            options,
            limits,
        )
    }

    /// The names the plan of `options` and `limits` removes, in its order.
    fn planned(&self, options: &ScanOptions, limits: Limits) -> Vec<String> {
        let plan = self.plan(options, limits);
        assert!(plan.issues.is_empty(), "{:?}", plan.issues);
        planned_names(&plan)
    }
}

fn planned_names(plan: &Plan) -> Vec<String> {
    plan.removals()
        .map(|removal| removal.name.to_string_lossy().into_owned())
        .collect()
}

fn origin() -> Origin {
    Origin::SystemTarget {
        subcategory: Label::try_from("Journal Logs".to_owned()).unwrap(),
        path: TemplatePath::try_from("/var/log/journal".to_owned()).unwrap(),
    }
}

/// Every age old enough, measured now.
fn options() -> ScanOptions {
    ScanOptions::new(SystemTime::now(), Duration::ZERO, Exclusions::none())
}

/// Limits of age alone.
const BY_AGE: Limits = Limits {
    max_age: Duration::from_secs(SECONDS_PER_DAY),
    max_use: u64::MAX,
};

/// Limits of size alone, at `max_use` bytes.
const fn by_size(max_use: u64) -> Limits {
    Limits {
        max_age: Duration::MAX,
        max_use,
    }
}

#[test]
fn a_name_says_what_may_go() {
    let id = 0x485d_c3eb_f0f8_4eb6_a39f_272f_8262_3a04;
    assert_eq!(
        named(OsStr::new(
            "system@485dc3ebf0f84eb6a39f272f82623a04-00000000000102e4-00065e1ed4752b12.journal"
        )),
        Some(Named::Archived {
            sequence_id: id,
            sequence_number: 0x102e4,
            realtime: 0x0006_5e1e_d475_2b12,
        })
    );
    assert_eq!(
        named(OsStr::new(
            "user-1000@485dc3ebf0f84eb6a39f272f82623a04-0000000000000001-00065dfa4288c603.journal"
        )),
        Some(Named::Archived {
            sequence_id: id,
            sequence_number: 1,
            realtime: 0x0006_5dfa_4288_c603,
        }),
        "a user's archive"
    );
    assert_eq!(
        named(OsStr::new(
            "system@00065e1ed4a5549b-50b40f5fae2cc875.journal~"
        )),
        Some(Named::Corrupted {
            realtime: 0x0006_5e1e_d4a5_549b
        })
    );
    for active in [
        "system.journal",
        "user-1000.journal",
        "system.journal~",
        // A digit too few, or one that is not hexadecimal.
        "system@485dc3ebf0f84eb6a39f272f82623a0-00000000000102e4-00065e1ed4752b12.journal",
        "system@485dc3ebf0f84eb6a39f272f82623a04-00000000000102e4-00065e1ed4752b1g.journal",
        "system@00065e1ed4a5549b-50b40f5fae2cc87.journal~",
        // No prefix, which systemd wants.
        "@485dc3ebf0f84eb6a39f272f82623a04-00000000000102e4-00065e1ed4752b12.journal",
        "@00065e1ed4a5549b-50b40f5fae2cc875.journal~",
    ] {
        assert_eq!(named(OsStr::new(active)), Some(Named::Active), "{active}");
    }
    for other in ["notes.txt", "system.journal.bak", "system@x.journal.old"] {
        assert_eq!(named(OsStr::new(other)), None, "{other}");
    }
    assert!(is_machine_directory(OsStr::new(MACHINE)));
    for name in [
        "0123456789ABCDEF0123456789abcdef",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef.namespace",
        "remote",
    ] {
        assert!(!is_machine_directory(OsStr::new(name)), "{name}");
    }
}

#[test]
fn the_archives_past_the_age_go_and_the_journal_being_written_never() {
    let journal = Journal::new();
    let old = archived(1, 1, 0);
    let broken = corrupted(1);
    let active = journal.write("system.journal");
    let user = journal.write("user-1000.journal");
    let odd = journal.write("system@x-y-z.journal");
    let unknown = journal.write("notes.txt");
    journal.write(&old);
    journal.write(&broken);
    // Its first entry now: too young for the age.
    let realtime = microseconds(SystemTime::now());
    let young = format!("system@{:032x}-{:016x}-{realtime:016x}.journal", 1, 9);
    journal.write(&young);

    let mut planned = journal.planned(&options(), BY_AGE);
    planned.sort();
    let mut expected = vec![old, broken];
    expected.sort();
    assert_eq!(planned, expected);
    for kept in [&active, &user, &odd, &unknown] {
        assert!(kept.exists(), "a plan removes nothing: {kept:?}");
    }
}

#[test]
fn the_oldest_archives_go_while_the_journal_takes_too_much_its_active_part_counted() {
    let journal = Journal::new();
    journal.write("system.journal");
    // Oldest first by their first entry, whatever their names: the oldest is
    // the last by name.
    let oldest = archived(3, 1, 0);
    let middle = archived(2, 1, 1);
    let newest = archived(1, 1, 2);
    for name in [&oldest, &middle, &newest] {
        journal.write(name);
    }
    let usage = journal.usage(&oldest);
    assert!(usage > 0);
    // Four journals of `usage`: down to two, the active one and the newest.
    assert_eq!(
        journal.planned(&options(), by_size(2 * usage)),
        [oldest.clone(), middle.clone()]
    );
    assert_eq!(
        journal.planned(&options(), by_size(4 * usage)),
        Vec::<String>::new(),
        "within the limit, nothing goes"
    );
}

#[test]
fn archives_go_by_their_first_entry_then_their_sequence() {
    let journal = Journal::new();
    // Two archives whose first entries are the same instant go by their
    // sequence, not by their names: `user-…` of sequence 1 before `system@…`
    // of sequence 2, though `s` sorts before `u`.
    let realtime = format!("{:016x}", LONG_AGO * 1_000_000);
    let system = format!("system@{:032x}-{:016x}-{realtime}.journal", 2, 1);
    let user = format!("user-1000@{:032x}-{:016x}-{realtime}.journal", 1, 1);
    let later = archived(1, 2, 1);
    for name in [&system, &user, &later] {
        journal.write(name);
    }
    let usage = journal.usage(&user);
    assert_eq!(
        journal.planned(&options(), by_size(usage)),
        [user, system],
        "the first sequence goes first"
    );
}

/// A file's own dates may say it older than its name: then its age is
/// theirs.
#[test]
fn an_archive_older_than_its_name_goes_by_its_own_date() {
    let journal = Journal::new();
    let realtime = microseconds(SystemTime::now());
    let young = format!("system@{:032x}-{:016x}-{realtime:016x}.journal", 1, 1);
    let path = journal.write(&young);
    assert_eq!(
        journal.planned(&options(), BY_AGE),
        Vec::<String>::new(),
        "too young by its name"
    );
    Journal::age(&path);
    assert_eq!(journal.planned(&options(), BY_AGE), [young]);
}

/// The journals being written stay, however old their content and whatever
/// the limits: only archives go.
#[test]
fn the_journals_being_written_stay_whatever_their_dates() {
    let journal = Journal::new();
    for active in ["system.journal", "user-1000.journal"] {
        Journal::age(&journal.write(active));
    }
    let older = archived(1, 1, 0);
    let newer = archived(1, 2, 1);
    journal.write(&older);
    journal.write(&newer);
    let archives = [older.clone(), newer];
    assert_eq!(journal.planned(&options(), by_size(0)), archives);
    assert_eq!(journal.planned(&options(), BY_AGE), archives);
}

/// The size counts each journal's own room: archives of different sizes go
/// until the directory is just within the limit, their room said exactly.
#[test]
fn archives_of_different_sizes_go_just_until_the_limit() {
    let journal = Journal::new();
    journal.write("system.journal");
    let small = archived(1, 1, 0);
    let large = archived(1, 2, 1);
    let medium = archived(1, 3, 2);
    journal.write_of(&small, 5_000);
    journal.write_of(&large, 40_000);
    journal.write_of(&medium, 20_000);
    let [active, small_usage, large_usage, medium_usage] = [
        "system.journal",
        small.as_str(),
        large.as_str(),
        medium.as_str(),
    ]
    .map(|name| journal.usage(name));
    assert!(small_usage < medium_usage && medium_usage < large_usage);
    let total = active + small_usage + large_usage + medium_usage;
    // One byte too many once the small one is gone: the large one goes too.
    let limit = total - small_usage - 1;
    let plan = journal.plan(&options(), by_size(limit));
    assert_eq!(planned_names(&plan), [small, large]);
    assert_eq!(plan.bytes(), small_usage + large_usage);
}

/// An old archive changed within the age floor (restored, copied) stays,
/// for its age too.
#[test]
fn an_old_archive_changed_within_the_floor_stays_for_its_age() {
    let journal = Journal::new();
    let old = archived(1, 1, 0);
    journal.write(&old);
    assert_eq!(journal.planned(&options(), BY_AGE), [old], "no floor");
    let floor = ScanOptions::new(
        SystemTime::now(),
        Duration::from_secs(3600),
        Exclusions::none(),
    );
    assert_eq!(journal.planned(&floor, BY_AGE), Vec::<String>::new());
}

/// The oldest archive changed after the cutoff: the size takes no newer one
/// in its place.
#[test]
fn a_recent_archive_holds_the_size_back() {
    let journal = Journal::new();
    journal.write("system.journal");
    let newer = archived(1, 2, 1);
    journal.write(&newer);
    let cutoff = SystemTime::now();
    // A change date coarser than this pause would put both on one side.
    std::thread::sleep(Duration::from_millis(50));
    let oldest = archived(1, 1, 0);
    journal.write(&oldest);
    let usage = journal.usage(&oldest);
    let at_cutoff = ScanOptions::new(cutoff, Duration::ZERO, Exclusions::none());
    assert_eq!(
        journal.planned(&at_cutoff, by_size(usage)),
        Vec::<String>::new(),
        "the oldest kept, the newer one stays too"
    );
    assert_eq!(
        journal.planned(&options(), by_size(usage)),
        [oldest, newer],
        "both old enough: both go"
    );
}

/// The machine's identifier is all its file holds, a line break after it:
/// never a path nor anything else.
#[test]
fn a_machine_is_named_by_its_identifier_alone() {
    assert_eq!(
        MachineId::parse(format!("{MACHINE}\n").as_bytes()).unwrap(),
        MachineId(MACHINE.to_owned())
    );
    assert_eq!(
        MachineId::parse(MACHINE.as_bytes()).unwrap(),
        MachineId(MACHINE.to_owned())
    );
    for refused in [
        &b""[..],
        b"\n",
        b"0123456789abcdef0123456789abcde\n",
        b"0123456789ABCDEF0123456789ABCDEF\n",
        b"../456789abcdef0123456789abcdef\n",
        b"/etc",
        b"0123456789abcdef0123456789abcdef\n\n",
        b"0123456789abcdef0123456789abcde\xff",
    ] {
        assert_eq!(
            MachineId::parse(refused).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "{refused:?}"
        );
    }
}

/// The oldest archive kept (excluded, or changed within the age floor): the
/// size takes no newer one in its place, which systemd would have kept; the
/// age still takes those past it.
#[test]
fn an_archive_kept_holds_the_size_back_but_not_the_age() {
    let journal = Journal::new();
    journal.write("system.journal");
    let oldest = archived(1, 1, 0);
    let second = archived(1, 2, 1);
    let realtime = microseconds(SystemTime::now());
    let young = format!("system@{:032x}-{:016x}-{realtime:016x}.journal", 1, 3);
    for name in [&oldest, &second, &young] {
        journal.write(name);
    }
    let usage = journal.usage(&oldest);
    // Four journals of `usage`, two allowed.
    assert_eq!(
        journal.planned(&options(), by_size(2 * usage)),
        [oldest.clone(), second.clone()],
        "nothing kept: as systemd"
    );
    let excluding = ScanOptions::new(
        SystemTime::now(),
        Duration::ZERO,
        Exclusions::as_written([journal.machine().join(&oldest)]).unwrap(),
    );
    assert_eq!(
        journal.planned(&excluding, by_size(2 * usage)),
        Vec::<String>::new(),
        "the oldest kept, no newer one goes for the size"
    );
    let by_both = Limits {
        max_age: Duration::from_secs(SECONDS_PER_DAY),
        max_use: 2 * usage,
    };
    assert_eq!(
        journal.planned(&excluding, by_both),
        [second],
        "the age still takes what is past it"
    );

    let floor = ScanOptions::new(
        SystemTime::now(),
        Duration::from_secs(3600),
        Exclusions::none(),
    );
    assert_eq!(
        journal.planned(&floor, by_size(0)),
        Vec::<String>::new(),
        "an archive written within the age floor stays"
    );

    let whole = Exclusions::as_written([journal.root.clone()]).unwrap();
    let plan = journal.plan(
        &ScanOptions::new(SystemTime::now(), Duration::ZERO, whole),
        by_size(0),
    );
    assert_eq!(plan.entries(), 0);
    assert!(
        matches!(plan.issues.as_slice(), [ScanIssue::Excluded { .. }]),
        "{:?}",
        plan.issues
    );
    let machine = Exclusions::as_written([journal.machine()]).unwrap();
    let plan = journal.plan(
        &ScanOptions::new(SystemTime::now(), Duration::ZERO, machine),
        by_size(0),
    );
    assert_eq!(
        (plan.entries(), plan.issues.len()),
        (0, 0),
        "nothing to vacuum"
    );
}

#[test]
fn nothing_is_followed_nor_taken_but_the_local_machines_journals() {
    let journal = Journal::new();
    let outside = journal.root.parent().unwrap().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let target = outside.join("kept.journal");
    fs::write(&target, b"data").unwrap();
    // A link and a directory named as archives.
    symlink(&target, journal.machine().join(archived(1, 1, 0))).unwrap();
    fs::create_dir(journal.machine().join(archived(1, 2, 0))).unwrap();
    // Archives of another machine (a container's, an old identifier), and
    // in a directory that is no machine's: journalctl leaves them too.
    for other in ["fedcba9876543210fedcba9876543210", "remote"] {
        let directory = journal.root.join(other);
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join(archived(1, 3, 0)), b"data").unwrap();
    }
    let goes = archived(1, 4, 0);
    journal.write(&goes);

    assert_eq!(
        journal.planned(&options(), by_size(0)),
        std::slice::from_ref(&goes)
    );
    assert!(target.exists());

    // Neither the link nor the directory counts: three archives of `usage`,
    // two allowed, one goes.
    let older = archived(1, 0, 0);
    let newer = archived(1, 5, 1);
    journal.write(&older);
    journal.write(&newer);
    let usage = journal.usage(&goes);
    assert_eq!(journal.planned(&options(), by_size(2 * usage)), [older]);
}

/// What the system refuses is kept as such, its size as planned.
#[test]
fn a_journal_the_system_refuses_to_remove_stays() {
    if okapi_platform::running_as_root() {
        return;
    }
    let journal = Journal::new();
    let path = journal.write(&archived(1, 1, 0));
    let plan = journal.plan(&options(), by_size(0));
    let mut permissions = fs::metadata(journal.machine()).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o500);
    fs::set_permissions(journal.machine(), permissions.clone()).unwrap();
    let outcomes = clean(plan, Some(SystemTime::now()), &Watch::none());
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
    fs::set_permissions(journal.machine(), permissions).unwrap();
    let (outcome, scanned) = &outcomes[0];
    assert_eq!(
        (outcome.result.as_str(), outcome.reason.as_deref()),
        ("kept", Some("failed"))
    );
    assert_eq!(*scanned, journal.usage(&archived(1, 1, 0)));
    assert!(path.exists());
}

#[test]
fn a_journal_absent_offers_nothing_and_one_out_of_reach_is_said() {
    let journal = Journal::new();
    // An unknown machine as the system refused its file.
    let at = |root: &Path, machine: Option<&str>| {
        let machine = machine.map(|id| MachineId(id.to_owned()));
        let machine = machine.as_ref().ok_or(io::ErrorKind::PermissionDenied);
        plan(root, machine, &origin(), &options(), by_size(0))
    };
    let absent = at(&journal.root.join("absent"), Some(MACHINE));
    assert_eq!((absent.entries(), absent.issues.len()), (0, 0));
    let other = at(&journal.root, Some("fedcba9876543210fedcba9876543210"));
    assert_eq!(
        (other.entries(), other.issues.len()),
        (0, 0),
        "no journal of the local machine"
    );

    journal.write(&archived(1, 1, 0));
    let unknown = at(&journal.root, None);
    assert_eq!(unknown.entries(), 0);
    assert!(
        matches!(
            unknown.issues.as_slice(),
            [ScanIssue::UnreadableRoot { path, error: io::ErrorKind::PermissionDenied, .. }]
                if path == Path::new(MACHINE_ID)
        ),
        "{:?}",
        unknown.issues
    );

    let linked = journal.root.with_file_name("linked");
    symlink(&journal.root, &linked).unwrap();
    let refused = at(&linked, Some(MACHINE));
    assert_eq!(refused.entries(), 0);
    assert!(
        matches!(
            refused.issues.as_slice(),
            [ScanIssue::UnsafeRoot {
                reason: UnsafeReason::Link,
                ..
            }]
        ),
        "{:?}",
        refused.issues
    );

    // The local machine's directory a link to another.
    let elsewhere = journal.root.with_file_name("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join(archived(2, 1, 0)), b"data").unwrap();
    let local = "00112233445566778899aabbccddeeff";
    symlink(&elsewhere, journal.root.join(local)).unwrap();
    let refused = at(&journal.root, Some(local));
    assert_eq!(refused.entries(), 0);
    assert!(
        matches!(
            refused.issues.as_slice(),
            [ScanIssue::UnsafeRoot { reason: UnsafeReason::Link, path, .. }] if path.ends_with(local)
        ),
        "{:?}",
        refused.issues
    );
}

#[test]
fn a_cleaning_removes_only_what_is_still_as_planned() {
    let journal = Journal::new();
    let names: Vec<String> = (1..=5).map(|number| archived(1, number, 0)).collect();
    let paths: Vec<PathBuf> = names.iter().map(|name| journal.write(name)).collect();
    let usage = journal.usage(&names[0]);
    // All past the age: one kept holds none of the others back.
    let plan = journal.plan(&options(), BY_AGE);
    assert_eq!(planned_names(&plan), names);
    // Gone, replaced by another file (made before the first goes: never its
    // inode again), written since: each kept as such.
    fs::remove_file(&paths[1]).unwrap();
    let another = journal.machine().join("another");
    fs::write(&another, b"another").unwrap();
    fs::rename(&another, &paths[2]).unwrap();
    fs::write(&paths[3], b"written since").unwrap();

    let outcomes = clean(plan, Some(SystemTime::now()), &Watch::none());
    let results: Vec<(String, &str, Option<&str>)> = outcomes
        .iter()
        .map(|(outcome, _)| {
            (
                outcome.path.clone().unwrap(),
                outcome.result.as_str(),
                outcome.reason.as_deref(),
            )
        })
        .collect();
    let path = |index: usize| paths[index].display().to_string();
    assert_eq!(
        results,
        [
            (path(0), "removed", None),
            (path(1), "gone", None),
            (path(2), "kept", Some("replaced")),
            (path(3), "kept", Some("recent")),
            (path(4), "removed", None),
        ]
    );
    assert_eq!(outcomes[0].0.bytes, Some(usage));
    assert!(outcomes.iter().all(|(_, scanned)| *scanned == usage));
    assert!(!paths[0].exists() && !paths[4].exists());
    assert!(paths[2].exists() && paths[3].exists());
}

/// One kept at the cleaning (written since) is said as such, and the next
/// still goes, as systemd goes on past a removal that fails: what the plan
/// could tell would stay, it kept already.
#[test]
fn an_archive_kept_at_the_cleaning_is_said_and_the_next_goes() {
    let journal = Journal::new();
    let first = journal.write(&archived(1, 1, 0));
    let second = journal.write(&archived(1, 2, 1));
    let plan = journal.plan(&options(), by_size(0));
    assert_eq!(plan.entries(), 2);
    fs::write(&first, b"written since").unwrap();
    let outcomes = clean(plan, Some(SystemTime::now()), &Watch::none());
    let results: Vec<(&str, Option<&str>)> = outcomes
        .iter()
        .map(|(outcome, _)| (outcome.result.as_str(), outcome.reason.as_deref()))
        .collect();
    assert_eq!(results, [("kept", Some("recent")), ("removed", None)]);
    assert!(first.exists() && !second.exists());
}

#[test]
fn a_cleaning_cancelled_removes_nothing_more() {
    let journal = Journal::new();
    let path = journal.write(&archived(1, 1, 0));
    let plan = journal.plan(&options(), by_size(0));
    let cancel = Cancel::new();
    cancel.cancel();
    let outcomes = clean(plan, Some(SystemTime::now()), &Watch::new(cancel, None));
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].0.reason.as_deref(), Some("cancelled"));
    assert!(path.exists());
}

#[test]
fn an_archive_too_young_at_the_cleaning_stays() {
    let journal = Journal::new();
    let path = journal.write(&archived(1, 1, 0));
    let plan = journal.plan(&options(), by_size(0));
    let outcomes = clean(plan, Some(UNIX_EPOCH), &Watch::none());
    assert_eq!(outcomes[0].0.reason.as_deref(), Some("recent"));
    assert!(path.exists());
    // No cutoff at all (an age beyond the clock): nothing is old enough.
    let plan = journal.plan(&options(), by_size(0));
    let outcomes = clean(plan, None, &Watch::none());
    assert_eq!(outcomes[0].0.reason.as_deref(), Some("recent"));
    assert!(path.exists());
}

/// The oldest archive locked (immutable or append-only, which the system
/// would refuse to remove): it stays, and the size takes no newer one in
/// its place. Told by its status alone, as no test may lock a file.
#[test]
fn a_locked_archive_stays_and_holds_the_size_back() {
    let journal = Journal::new();
    let oldest = archived(1, 1, 0);
    let newer = archived(1, 2, 1);
    journal.write(&oldest);
    journal.write(&newer);
    let directory = Directory::open(&journal.machine()).unwrap();
    let (used, candidates) = candidates(&directory, &journal.machine()).unwrap();
    let usage = journal.usage(&oldest);
    let names = |removals: Vec<Removal>| -> Vec<String> {
        removals
            .into_iter()
            .map(|removal| removal.name.to_string_lossy().into_owned())
            .collect()
    };
    let locked: Vec<(Order, Removal)> = candidates
        .iter()
        .map(|(order, removal)| {
            let mut status = removal.status;
            status.locked = removal.name == OsStr::new(&oldest);
            (
                order.clone(),
                Removal {
                    name: removal.name.clone(),
                    status,
                    usage: removal.usage,
                },
            )
        })
        .collect();
    let plan = |candidates| {
        names(chosen(
            candidates,
            used,
            &journal.machine(),
            None,
            &options(),
            by_size(usage),
        ))
    };
    // Two of `usage`, one allowed.
    assert_eq!(
        plan(locked),
        Vec::<String>::new(),
        "the newer one stays too"
    );
    assert_eq!(plan(candidates), [oldest], "unlocked, the oldest goes");
}

/// A link on the way to the journal is told beforehand: root would refuse
/// it whatever the rights.
#[test]
fn a_journal_behind_a_link_is_unfit_beforehand() {
    let journal = Journal::new();
    assert_eq!(unfit(&journal.root), None);
    assert_eq!(
        unfit(&journal.root.join("absent")),
        None,
        "root's plan says"
    );
    let linked = journal.root.with_file_name("linked");
    symlink(journal.root.parent().unwrap(), &linked).unwrap();
    assert_eq!(unfit(&linked.join("journal")), Some(Unfit::LinkOnTheWay));
}

#[test]
fn the_description_shows_the_limits_the_vacuum_uses() {
    assert_eq!(DESCRIPTION_VALUES, [("days", 30), ("gigabytes", 1)]);
    assert_eq!(Limits::JOURNAL.max_age, Duration::from_hours(30 * 24));
    assert_eq!(Limits::JOURNAL.max_use, 1_000_000_000);
}
