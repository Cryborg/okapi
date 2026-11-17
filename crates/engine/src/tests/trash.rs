//! The trash of the user's home: what its scan offers, by the date its
//! `.trashinfo` says, each entry whole; and its cleaning, which reads that
//! date again and removes the `.trashinfo` after its entry.
//!
//! The scan and the cleaning run [`AHEAD`] from now, so that what the test
//! just wrote, whose inode date cannot be set back, counts as older than the
//! rule's 30 days; the deletion dates are written relative to that instant.
//! The trash is the running user's: run as root, root never empties one.

use std::os::unix::fs::{MetadataExt as _, symlink};
use std::time::UNIX_EPOCH;

use super::clean::{clean, ids};
use super::*;
use crate::clean::{CleanResult, KeepReason};

const DAY: Duration = Duration::from_hours(24);
/// How far ahead of now the scan and the cleaning run.
const AHEAD: Duration = Duration::from_hours(24 * 40);

/// `at` as a `.trashinfo` writes it, universal time standing for local.
fn deletion_date(at: SystemTime) -> String {
    let seconds = at.duration_since(UNIX_EPOCH).unwrap().as_secs();
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Howard Hinnant's `civil_from_days`.
    let shifted = i64::try_from(days).unwrap() + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_based_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_based_month + 2) / 5 + 1;
    let month = if march_based_month < 10 {
        march_based_month + 3
    } else {
        march_based_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// A trash below the fixture, its rule, and the instant its scan runs at.
struct Bin<'f> {
    fixture: &'f Fixture,
    rule: CleanTarget,
    now: SystemTime,
}

impl<'f> Bin<'f> {
    fn new(fixture: &'f Fixture) -> Self {
        let files = fixture.directory("share/Trash/files");
        fixture.directory("share/Trash/info");
        let rule = target(&format!(
            r#"{{"path": "{}", "subcategory": "Trash", "minAgeDays": 30, "trash": true}}"#,
            files.display()
        ));
        Self {
            fixture,
            rule,
            now: fixture.now + AHEAD,
        }
    }

    fn files(&self, relative: &str) -> PathBuf {
        self.fixture.path(&format!("share/Trash/files/{relative}"))
    }

    fn information(&self, name: &str) -> PathBuf {
        self.fixture
            .path(&format!("share/Trash/info/{name}.trashinfo"))
    }

    /// Writes the `.trashinfo` of `name`, saying it went `age` before the
    /// scan.
    fn went(&self, name: &str, age: Duration) {
        self.say(
            name,
            &format!(
                "[Trash Info]\nPath=/home/a/{name}\nDeletionDate={}\n",
                deletion_date(self.now - age)
            ),
        );
    }

    fn say(&self, name: &str, text: &str) {
        fs::write(self.information(name), text).unwrap();
    }

    /// A file of the trash, `name` (its `.trashinfo` aside).
    fn file(&self, relative: &str) -> PathBuf {
        let path = self.files(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"trashed").unwrap();
        path
    }

    /// The run of the scan and the cleaning, as the owner of the trash.
    fn run(&self) -> Run<'_> {
        let mut run = Run::new(self.fixture, vec![RuleEntry::Target(&self.rule)]);
        run.options = ScanOptions::new(self.now, HOUR, Exclusions::none());
        run.user = Some(fs::metadata(self.files("")).unwrap().uid());
        run
    }
}

#[test]
fn an_entry_long_enough_in_the_trash_is_offered_whole_and_unselected() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    bin.file("photo.jpg");
    bin.went("photo.jpg", 41 * DAY);
    bin.file("project/src/main.rs");
    bin.file("project/README");
    bin.went("project", 31 * DAY + HOUR);
    let report = bin.run().scan();
    assert_eq!(
        offered(&report),
        [
            (bin.files("photo.jpg"), EntryKind::File),
            (bin.files("project"), EntryKind::Directory),
        ]
    );
    let [group] = report.groups() else {
        panic!("{:?}", report.groups());
    };
    assert!(group.is_trash());
    assert!(!group.is_selected_by_default());
    assert!(!group.is_checked_by_default());
    assert_eq!(report.stats().trash_kept(), 0);
}

#[test]
fn an_entry_whose_information_does_not_let_it_go_is_kept() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    // Each one alone kept, each counted once.
    bin.file("recent");
    bin.went("recent", 29 * DAY);
    // Past 30 days, within the margin a zone may be off by.
    bin.file("within-margin");
    bin.went("within-margin", 30 * DAY + 23 * HOUR);
    bin.file("future");
    bin.say(
        "future",
        "[Trash Info]\nPath=/a\nDeletionDate=2999-01-01T00:00:00\n",
    );
    bin.file("orphan");
    bin.file("undated");
    bin.say("undated", "[Trash Info]\nPath=/a\n");
    bin.file("linked");
    bin.went("elsewhere", 41 * DAY);
    symlink(bin.information("elsewhere"), bin.information("linked")).unwrap();
    // The one that goes, beside them.
    bin.file("old");
    bin.went("old", 41 * DAY);
    let report = bin.run().scan();
    assert_eq!(offered_paths(&report), [bin.files("old")]);
    assert_eq!(report.stats().trash_kept(), 6);
}

#[test]
fn an_entry_part_of_which_may_not_go_is_kept_whole() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    // A link in it: never its other files one by one.
    bin.file("venv/lib/a.py");
    bin.file("venv/lib/b.py");
    symlink("/usr/bin/python3", bin.files("venv/python")).unwrap();
    bin.went("venv", 41 * DAY);
    // A file in it written since.
    bin.file("busy/old");
    let written = bin.file("busy/deep/written");
    File::open(&written).unwrap().set_modified(bin.now).unwrap();
    bin.went("busy", 41 * DAY);
    // Itself changed since, all it holds old.
    bin.file("touched/old");
    File::open(bin.files("touched"))
        .unwrap()
        .set_modified(bin.now)
        .unwrap();
    bin.went("touched", 41 * DAY);
    let report = bin.run().scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert_eq!(report.stats().trash_kept(), 3);
}

#[test]
fn the_user_s_minimum_age_holds_the_trash_too() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    // Its inode date, [`AHEAD`] old, passes 39 days; its information,
    // 35 days old, does not.
    bin.file("photo.jpg");
    bin.went("photo.jpg", 35 * DAY);
    let mut run = bin.run();
    run.options = ScanOptions::new(bin.now, 39 * DAY, Exclusions::none());
    let report = run.scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert_eq!(report.stats().trash_kept(), 1);
}

#[test]
fn a_trash_without_its_information_folder_offers_nothing() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    bin.file("photo.jpg");
    fs::remove_dir(fixture.path("share/Trash/info")).unwrap();
    let report = bin.run().scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert_eq!(report.issues(), []);
    // A link in its place is said.
    fs::create_dir(fixture.path("elsewhere")).unwrap();
    symlink(fixture.path("elsewhere"), fixture.path("share/Trash/info")).unwrap();
    let report = bin.run().scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert!(
        matches!(
            report.issues(),
            [ScanIssue::UnsafeRoot {
                reason: crate::report::UnsafeReason::Replaced,
                ..
            }]
        ),
        "{:?}",
        report.issues()
    );
}

#[test]
fn root_never_empties_a_trash() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    bin.file("photo.jpg");
    bin.went("photo.jpg", 41 * DAY);
    let mut run = bin.run();
    run.user = Some(okapi_platform::ROOT_USER);
    let report = run.scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    // Not even read: no entry kept for its information.
    assert_eq!(report.stats().trash_kept(), 0);
}

#[test]
fn an_entry_whose_information_is_another_user_s_is_kept() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    bin.file("photo.jpg");
    bin.went("photo.jpg", 41 * DAY);
    let mut run = bin.run();
    run.user = run.user.map(|user| user.wrapping_add(1).max(1));
    let report = run.scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert_eq!(report.stats().trash_kept(), 1);
}

#[test]
fn a_trash_rule_naming_no_files_folder_walks_nothing() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    fixture.file("share/Trash/info/old", 1, 41 * DAY);
    let info = fixture.path("share/Trash/info");
    let rule = target(&format!(
        r#"{{"path": "{}", "subcategory": "Trash", "minAgeDays": 30, "trash": true}}"#,
        info.display()
    ));
    let mut run = bin.run();
    run.entries = vec![RuleEntry::Target(&rule)];
    let report = run.scan();
    assert_eq!(offered_paths(&report), Vec::<PathBuf>::new());
    assert!(
        matches!(report.issues(), [ScanIssue::Unsupported { .. }]),
        "{:?}",
        report.issues()
    );
}

#[test]
fn an_entry_goes_then_its_information() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    bin.file("photo.jpg");
    bin.went("photo.jpg", 41 * DAY);
    bin.file("project/src/main.rs");
    bin.went("project", 41 * DAY);
    // Kept at the scan: it and its information stay.
    bin.file("recent");
    bin.went("recent", DAY);
    let run = bin.run();
    let report = run.scan();
    let receipt = clean(&run, &report, &ids(&report));
    assert!(receipt.is_complete(), "{receipt:?}");
    for gone in ["photo.jpg", "project"] {
        assert!(!bin.files(gone).exists(), "{gone}");
        assert!(!bin.information(gone).exists(), "{gone}");
    }
    assert!(bin.files("recent").exists());
    assert!(bin.information("recent").exists());
}

#[test]
fn an_information_changed_since_the_scan_keeps_its_entry() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    for name in ["redated", "unsaid", "forgotten"] {
        bin.file(name);
        bin.went(name, 41 * DAY);
    }
    let run = bin.run();
    let report = run.scan();
    assert_eq!(offered_paths(&report).len(), 3);
    // Put back and trashed again, an instant ago (invariant 2).
    bin.went("redated", DAY);
    bin.say("unsaid", "[Trash Info]\nPath=/a\n");
    fs::remove_file(bin.information("forgotten")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    let mut results: Vec<_> = receipt
        .outcomes()
        .iter()
        .map(|outcome| (outcome.path().unwrap().to_path_buf(), outcome.result()))
        .collect();
    results.sort_by(|first, second| first.0.cmp(&second.0));
    assert_eq!(
        results,
        [
            (
                bin.files("forgotten"),
                CleanResult::Kept(KeepReason::Changed)
            ),
            (bin.files("redated"), CleanResult::Kept(KeepReason::Recent)),
            (bin.files("unsaid"), CleanResult::Kept(KeepReason::Changed)),
        ]
    );
    for name in ["redated", "unsaid", "forgotten"] {
        assert!(bin.files(name).exists(), "{name}");
    }
    assert!(bin.information("redated").exists());
}

#[test]
fn an_entry_restored_since_the_scan_is_gone_and_its_new_information_stays() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    bin.file("photo.jpg");
    bin.went("photo.jpg", 41 * DAY);
    let run = bin.run();
    let report = run.scan();
    fs::remove_file(bin.files("photo.jpg")).unwrap();
    // Another one of that name trashed meanwhile has only its information
    // yet: it is not this entry's to remove.
    bin.went("photo.jpg", 41 * DAY);
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(receipt.outcomes()[0].result(), CleanResult::Gone);
    assert!(bin.information("photo.jpg").exists());
}

#[test]
fn an_entry_kept_at_the_cleaning_keeps_its_information() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    bin.file("project/old");
    let written = bin.file("project/deep/written");
    bin.went("project", 41 * DAY);
    let run = bin.run();
    let report = run.scan();
    assert_eq!(offered_paths(&report), [bin.files("project")]);
    // Written since the scan: the entry stays whole, restorable.
    File::open(&written).unwrap().set_modified(bin.now).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::Recent)
    );
    assert!(bin.files("project/old").exists());
    assert!(bin.information("project").exists());
}

#[test]
fn an_information_folder_changed_since_the_scan_keeps_every_entry() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    bin.file("photo.jpg");
    bin.went("photo.jpg", 41 * DAY);
    let run = bin.run();
    let report = run.scan();
    // Its information moved away, a link in its place.
    fs::rename(
        fixture.path("share/Trash/info"),
        fixture.path("share/elsewhere"),
    )
    .unwrap();
    symlink(
        fixture.path("share/elsewhere"),
        fixture.path("share/Trash/info"),
    )
    .unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::Replaced)
    );
    // Gone altogether.
    fs::remove_file(fixture.path("share/Trash/info")).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::Changed)
    );
    assert!(bin.files("photo.jpg").exists());
    assert!(fixture.path("share/elsewhere/photo.jpg.trashinfo").exists());
}

#[test]
fn the_user_s_minimum_age_at_the_cleaning_holds_the_trash_too() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    // As at the scan: its inode date passes 39 days, its information not.
    bin.file("photo.jpg");
    bin.went("photo.jpg", 35 * DAY);
    let mut run = bin.run();
    let report = run.scan();
    assert_eq!(offered_paths(&report), [bin.files("photo.jpg")]);
    run.options = ScanOptions::new(bin.now, 39 * DAY, Exclusions::none());
    let receipt = clean(&run, &report, &ids(&report));
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::Recent)
    );
    assert!(bin.files("photo.jpg").exists());
    assert!(bin.information("photo.jpg").exists());
}

#[test]
fn an_entry_whose_information_could_not_follow_it_stays() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    if reads_through_permissions(&fixture) {
        // Root removes through permissions: nothing to observe.
        return;
    }
    let bin = Bin::new(&fixture);
    bin.file("photo.jpg");
    bin.went("photo.jpg", 41 * DAY);
    let run = bin.run();
    let report = run.scan();
    let info = fixture.path("share/Trash/info");
    fs::set_permissions(&info, fs::Permissions::from_mode(0o555)).unwrap();
    let receipt = clean(&run, &report, &ids(&report));
    fs::set_permissions(&info, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(
            receipt.outcomes()[0].result(),
            CleanResult::Kept(KeepReason::Failed {
                kind: std::io::ErrorKind::PermissionDenied,
                ..
            })
        ),
        "{receipt:?}"
    );
    assert!(bin.files("photo.jpg").exists());
    assert!(bin.information("photo.jpg").exists());
}

#[test]
fn the_trash_of_the_rules_is_the_one_the_platform_knows() {
    let rules = okapi_rules::RuleSet::embedded(okapi_rules::Os::Linux).unwrap();
    let [path] = rules
        .system()
        .clean_targets()
        .iter()
        .filter_map(|target| match target.effect() {
            okapi_rules::Effect::Delete(deletion) if deletion.is_trash() => Some(deletion.path()),
            _ => None,
        })
        .collect::<Vec<_>>()[..]
    else {
        panic!("one trash on Linux");
    };
    let data = PathBuf::from("/home/guest/.local/share");
    let mut variables = TemplateVariables::new();
    variables
        .insert(Variable::LocalShare, data.clone())
        .unwrap();
    let files = path.resolve(&variables).unwrap();
    assert_eq!(
        files.parent(),
        Some(okapi_platform::home_trash(&data).as_path())
    );
    assert_eq!(
        okapi_platform::information_folder(&files),
        Some(okapi_platform::home_trash(&data).join("info"))
    );
}

#[test]
fn an_information_or_its_folder_that_cannot_be_read_is_said_so() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    if reads_through_permissions(&fixture) {
        // Root reads through permissions: nothing to observe.
        return;
    }
    let bin = Bin::new(&fixture);
    bin.file("photo.jpg");
    bin.went("photo.jpg", 41 * DAY);
    let run = bin.run();
    let report = run.scan();
    let information = bin.information("photo.jpg");
    fs::set_permissions(&information, fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable = run.scan();
    let receipt = clean(&run, &report, &ids(&report));
    fs::set_permissions(&information, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(offered_paths(&unreadable), Vec::<PathBuf>::new());
    assert_eq!(unreadable.stats().unreadable(), 1);
    assert_eq!(unreadable.stats().trash_kept(), 0);
    assert!(
        matches!(
            receipt.outcomes()[0].result(),
            CleanResult::Kept(KeepReason::Failed {
                kind: std::io::ErrorKind::PermissionDenied,
                ..
            })
        ),
        "{receipt:?}"
    );
    // Its folder: said at the scan.
    let info = fixture.path("share/Trash/info");
    fs::set_permissions(&info, fs::Permissions::from_mode(0o000)).unwrap();
    let closed = run.scan();
    fs::set_permissions(&info, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(
            closed.issues(),
            [ScanIssue::UnreadableRoot {
                error: std::io::ErrorKind::PermissionDenied,
                ..
            }]
        ),
        "{:?}",
        closed.issues()
    );
    assert!(bin.files("photo.jpg").exists());
}

#[test]
fn root_never_empties_a_trash_at_the_cleaning_either() {
    let fixture = Fixture::new();
    let bin = Bin::new(&fixture);
    bin.file("photo.jpg");
    bin.went("photo.jpg", 41 * DAY);
    let run = bin.run();
    let report = run.scan();
    let receipt = super::clean::clean_as(
        &run,
        &report,
        &ids(&report),
        Some(okapi_platform::ROOT_USER),
    );
    assert_eq!(
        receipt.outcomes()[0].result(),
        CleanResult::Kept(KeepReason::Changed)
    );
    assert!(bin.files("photo.jpg").exists());
}

#[test]
fn a_group_sharing_the_root_of_a_trash_empties_a_trash() {
    let label = okapi_rules::Label::try_from("Trash".to_owned()).unwrap();
    let origin = |path: &str| Origin::SystemTarget {
        subcategory: label.clone(),
        path: okapi_rules::TemplatePath::try_from(path.to_owned()).unwrap(),
    };
    let trash = || {
        let mut attributes =
            crate::report::GroupAttributes::plain(origin("/a"), label.clone(), Vec::new(), false);
        attributes.trash = true;
        attributes
    };
    let plain =
        || crate::report::GroupAttributes::plain(origin("/b"), label.clone(), Vec::new(), true);
    for (mut first, second) in [(trash(), plain()), (plain(), trash())] {
        first.merge(second);
        assert!(first.trash);
        assert!(!first.selected_by_default);
    }
}
