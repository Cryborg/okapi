//! The folders the user chose to have emptied: what is offered of them,
//! what refuses them, and their cleaning.

use std::collections::BTreeSet;
use std::os::unix::fs::symlink;

use super::clean::{clean, ids, make_recent, run_later};
use super::*;
use crate::clean::{CleanResult, KeepReason};
use crate::custom::{CustomFolder, CustomFolderRefusal, PersonalFolders};
use crate::report::{Category, Origin};

fn folder(path: &Path) -> CustomFolder {
    CustomFolder::new(path.to_path_buf()).unwrap()
}

/// The refusals of the custom folders in `report`, by the path written.
fn refusals(report: &ScanReport) -> Vec<(PathBuf, CustomFolderRefusal)> {
    report
        .issues()
        .iter()
        .filter_map(|issue| match issue {
            ScanIssue::CustomFolderRefused {
                origin: Origin::Custom { folder },
                refusal,
            } => Some((folder.path().to_path_buf(), *refusal)),
            _ => None,
        })
        .collect()
}

fn scan_of(fixture: &Fixture, folders: &[&CustomFolder]) -> ScanReport {
    Run::new(
        fixture,
        folders
            .iter()
            .map(|folder| RuleEntry::Custom(folder))
            .collect(),
    )
    .scan()
}

#[test]
fn what_a_folder_holds_is_offered_unchecked_never_the_folder() {
    let fixture = Fixture::new();
    let root = fixture.directory("build");
    fixture.file("build/old.o", 10, 2 * HOUR);
    fixture.file("build/new.o", 20, Duration::ZERO);
    fixture.file("build/cache/a", 30, 2 * HOUR);
    fixture.age(&fixture.path("build/cache"), 2 * HOUR);
    fixture.age(&root, 2 * HOUR);
    let build = folder(&root);
    let report = scan_of(&fixture, &[&build]);
    assert_eq!(
        offered_paths(&report),
        [fixture.path("build/cache"), fixture.path("build/old.o")]
    );
    let [group] = report.groups() else {
        panic!("{:?}", report.groups());
    };
    assert!(!group.is_selected_by_default());
    assert!(!group.is_checked_by_default(), "nothing checked beforehand");
    assert!(!group.needs_admin());
    assert_eq!(group.custom_folder(), Some(&build));
    assert_eq!(
        Category::of_group(group.origins()),
        BTreeSet::from([Category::Custom])
    );
    assert_eq!(
        group.origins()[0].key(),
        ["custom".to_owned(), root.display().to_string()]
    );
    assert_eq!(refusals(&report), []);
}

#[test]
fn a_variable_s_folder_or_one_of_its_ancestors_is_refused_by_identity() {
    let fixture = Fixture::new();
    fixture.directory("home/guest/.config");
    fixture.directory("home/guest/work");
    fixture.file("home/guest/work/old", 1, 2 * HOUR);
    symlink(fixture.path("home"), fixture.path("alias")).unwrap();
    let home = folder(&fixture.path("home/guest"));
    let config = folder(&fixture.path("home/guest/.config"));
    let parent = folder(&fixture.path("home"));
    // Written through a link in an ancestor: the same folder as `home`.
    let aliased = folder(&fixture.path("alias/guest"));
    let work = folder(&fixture.path("home/guest/work"));
    let mut run = Run::new(
        &fixture,
        [&home, &config, &parent, &aliased, &work]
            .into_iter()
            .map(RuleEntry::Custom)
            .collect(),
    );
    run.variables = fixture.variables(&[
        (Variable::Home, "home/guest"),
        (Variable::Config, "home/guest/.config"),
    ]);
    let report = run.scan();
    let home_refused = CustomFolderRefusal::Protected(Variable::Home);
    assert_eq!(
        refusals(&report),
        [
            (fixture.path("home/guest"), home_refused),
            (
                fixture.path("home/guest/.config"),
                CustomFolderRefusal::Protected(Variable::Config)
            ),
            // An ancestor of both: either variable names it, the first met.
            (fixture.path("home"), home_refused),
            (fixture.path("alias/guest"), home_refused),
        ]
    );
    assert_eq!(
        offered_paths(&report),
        [fixture.path("home/guest/work/old")]
    );
}

#[test]
fn a_personal_folder_or_one_of_its_ancestors_is_refused_but_not_one_below() {
    let fixture = Fixture::new();
    fixture.directory("guest/Media/Pictures/tmp");
    let pictures = folder(&fixture.path("guest/Media/Pictures"));
    let media = folder(&fixture.path("guest/Media"));
    let below = folder(&fixture.path("guest/Media/Pictures/tmp"));
    let mut run = Run::new(
        &fixture,
        vec![
            RuleEntry::Custom(&pictures),
            RuleEntry::Custom(&media),
            RuleEntry::Custom(&below),
        ],
    );
    run.personal = PersonalFolders::of(&[fixture.path("guest/Media/Pictures")], file_identity);
    assert_eq!(
        refusals(&run.scan()),
        [
            (pictures.path().to_path_buf(), CustomFolderRefusal::Personal),
            (media.path().to_path_buf(), CustomFolderRefusal::Personal),
        ]
    );
    // When they cannot be read, none can be told apart: all refused.
    symlink(fixture.path("nowhere"), fixture.path("guest/dangling")).unwrap();
    run.personal = PersonalFolders::of(&[fixture.path("guest/dangling/Documents")], file_identity);
    let unknown = CustomFolderRefusal::PersonalUnknown(std::io::ErrorKind::NotFound);
    assert_eq!(
        refusals(&run.scan()),
        [pictures.clone(), media.clone(), below.clone()]
            .map(|folder| (folder.path().to_path_buf(), unknown))
    );
    // Without a home and a configuration folder, none can be named.
    run.personal = PersonalFolders::read(&TemplateVariables::new(), file_identity);
    assert_eq!(refusals(&run.scan()).len(), 3);
    assert!(
        refusals(&run.scan())
            .iter()
            .all(|(_, refusal)| *refusal == unknown)
    );
}

#[test]
fn the_personal_folders_are_read_where_the_variables_say() {
    let fixture = Fixture::new();
    fixture.directory("guest/Projets/Docs/old");
    fixture.directory("guest/Desktop");
    fixture.file("guest/.config/user-dirs.dirs", 0, Duration::ZERO);
    std::fs::write(
        fixture.path("guest/.config/user-dirs.dirs"),
        "XDG_DOCUMENTS_DIR=\"$HOME/Projets/Docs\"\n",
    )
    .unwrap();
    let variables = fixture.variables(&[
        (Variable::Home, "guest"),
        (Variable::Config, "guest/.config"),
    ]);
    let personal = PersonalFolders::read(&variables, file_identity);
    let docs = folder(&fixture.path("guest/Projets/Docs"));
    let projets = folder(&fixture.path("guest/Projets"));
    let desktop = folder(&fixture.path("guest/Desktop"));
    let below = folder(&fixture.path("guest/Projets/Docs/old"));
    let mut run = Run::new(
        &fixture,
        [&docs, &projets, &desktop, &below]
            .into_iter()
            .map(RuleEntry::Custom)
            .collect(),
    );
    run.personal = personal;
    assert_eq!(
        refusals(&run.scan()),
        [docs, projets, desktop]
            .map(|folder| (folder.path().to_path_buf(), CustomFolderRefusal::Personal)),
        "named, its ancestor, and a default name"
    );
}

#[test]
fn a_link_a_file_an_absent_folder_or_one_of_another_is_refused() {
    let fixture = Fixture::new();
    let real = fixture.directory("real");
    fixture.file("real/old", 1, 2 * HOUR);
    symlink(&real, fixture.path("link")).unwrap();
    let file = fixture.file("plain", 1, 2 * HOUR);
    let foreign = fixture.directory("foreign");
    fixture.file("foreign/old", 1, 2 * HOUR);
    mark(&foreign, FOREIGN_MARK);
    let unknown = fixture.directory("unknown");
    mark(&unknown, UNKNOWN_OWNER_MARK);
    let link = folder(&fixture.path("link"));
    let file = folder(&file);
    let foreign = folder(&foreign);
    let unknown = folder(&unknown);
    let absent = folder(&fixture.path("absent"));
    let report = scan_of(&fixture, &[&link, &file, &foreign, &unknown, &absent]);
    assert_eq!(
        refusals(&report),
        [
            (link.path().to_path_buf(), CustomFolderRefusal::Link),
            (
                file.path().to_path_buf(),
                CustomFolderRefusal::NotADirectory
            ),
            (foreign.path().to_path_buf(), CustomFolderRefusal::NotOwned),
            (unknown.path().to_path_buf(), CustomFolderRefusal::NotOwned),
            // Named by the user: said, unlike a rule's absent root.
            (absent.path().to_path_buf(), CustomFolderRefusal::Absent),
        ]
    );
    assert!(report.groups().is_empty(), "{:?}", report.groups());
}

#[test]
fn the_user_s_exclusions_apply_to_a_folder_and_within_it() {
    let fixture = Fixture::new();
    let excluded = fixture.directory("excluded");
    fixture.file("excluded/old", 1, 2 * HOUR);
    let kept = fixture.directory("kept");
    fixture.file("kept/keep.kdbx", 1, 2 * HOUR);
    fixture.file("kept/old", 1, 2 * HOUR);
    fixture.file("kept/vault/secret", 1, 2 * HOUR);
    fixture.age(&fixture.path("kept/vault"), 2 * HOUR);
    fixture.file("kept/other/old", 1, 2 * HOUR);
    fixture.age(&fixture.path("kept/other"), 2 * HOUR);
    let excluded = folder(&excluded);
    let kept = folder(&kept);
    let mut run = Run::new(
        &fixture,
        vec![RuleEntry::Custom(&excluded), RuleEntry::Custom(&kept)],
    );
    run.options = fixture.options(
        Exclusions::new([
            excluded.path().to_path_buf(),
            fixture.path("kept/vault"),
            PathBuf::from("*.kdbx"),
        ])
        .unwrap(),
    );
    let report = run.scan();
    assert_eq!(
        offered_paths(&report),
        [fixture.path("kept/old"), fixture.path("kept/other")],
        "the excluded folder within kept, its sibling offered"
    );
    assert!(report.issues().iter().any(|issue| matches!(
        issue,
        ScanIssue::Excluded { origins, .. }
            if origins == &[Origin::Custom { folder: excluded.clone() }]
    )));
}

#[test]
fn a_folder_is_walked_only_when_its_category_is_chosen() {
    let fixture = Fixture::new();
    let root = fixture.directory("build");
    fixture.file("build/old", 1, 2 * HOUR);
    let build = folder(&root);
    let mut run = Run::new(&fixture, vec![RuleEntry::Custom(&build)]);
    run.options = fixture
        .options(Exclusions::none())
        .with_categories(BTreeSet::from([Category::System]));
    let report = run.scan();
    assert_eq!(report.groups(), []);
    assert_eq!(report.issues(), [], "nothing said of it either");
    run.options = fixture
        .options(Exclusions::none())
        .with_categories(BTreeSet::from([Category::Custom]));
    assert_eq!(offered_paths(&run.scan()), [fixture.path("build/old")]);
}

#[test]
fn a_folder_that_is_a_rule_s_root_leaves_the_group_unchecked() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/old", 1, 2 * HOUR);
    let rule = target_at(&root);
    let data = folder(&root);
    let report = Run::new(
        &fixture,
        vec![RuleEntry::Target(&rule), RuleEntry::Custom(&data)],
    )
    .scan();
    let [group] = report.groups() else {
        panic!("{:?}", report.groups());
    };
    assert_eq!(group.origins().len(), 2);
    assert!(!group.is_selected_by_default(), "the AND of their rules");
    assert_eq!(group.custom_folder(), None, "labelled by the rule");
    assert_eq!(group.label().as_str(), "Temp");
}

#[test]
fn a_cleaning_empties_the_folder_but_keeps_it_and_what_became_recent() {
    let fixture = Fixture::new();
    let root = fixture.directory("build");
    fixture.file("build/old", 1, HOUR);
    let recent = fixture.file("build/recent", 1, HOUR);
    fixture.file("build/dir/a", 1, HOUR);
    let build = folder(&root);
    let mut run = run_later(&fixture, vec![RuleEntry::Custom(&build)]);
    run.options = run.options.with_custom_folders(vec![build.clone()]);
    let report = run.scan();
    assert_eq!(offered_paths(&report).len(), 3);
    make_recent(&fixture, &recent);
    let receipt = clean(&run, &report, &ids(&report));
    assert!(root.is_dir(), "the folder itself stays");
    assert!(!fixture.path("build/old").exists());
    assert!(!fixture.path("build/dir").exists());
    assert!(recent.exists(), "changed since the scan (invariant 2)");
    assert!(
        receipt
            .outcomes()
            .iter()
            .any(|outcome| outcome.result() == CleanResult::Kept(KeepReason::Recent))
    );
}

#[test]
fn a_folder_taken_out_of_the_settings_since_the_scan_keeps_everything() {
    let fixture = Fixture::new();
    let first = fixture.directory("first");
    fixture.file("first/old", 1, HOUR);
    let second = fixture.directory("second");
    fixture.file("second/old", 1, HOUR);
    let (first, second) = (folder(&first), folder(&second));
    let mut run = run_later(
        &fixture,
        vec![RuleEntry::Custom(&first), RuleEntry::Custom(&second)],
    );
    let report = run.scan();
    assert_eq!(offered_paths(&report).len(), 2);
    // Only the second one is still in the settings at the cleaning.
    run.options = run.options.with_custom_folders(vec![second.clone()]);
    let receipt = clean(&run, &report, &ids(&report));
    assert!(fixture.path("first/old").exists(), "taken out: kept");
    assert!(
        !fixture.path("second/old").exists(),
        "still chosen: removed"
    );
    assert!(
        receipt
            .outcomes()
            .iter()
            .any(|outcome| outcome.result() == CleanResult::Kept(KeepReason::NoLongerChosen))
    );
}

#[test]
fn a_folder_shared_with_a_rule_is_cleaned_for_the_rule_s_sake() {
    let fixture = Fixture::new();
    let root = fixture.directory("data");
    fixture.file("data/old", 1, HOUR);
    let rule = target_at(&root);
    let data = folder(&root);
    // The folder is out of the settings, the rule still names the root.
    let run = run_later(
        &fixture,
        vec![RuleEntry::Target(&rule), RuleEntry::Custom(&data)],
    );
    let report = run.scan();
    clean(&run, &report, &ids(&report));
    assert!(!fixture.path("data/old").exists());
}

#[test]
fn a_folder_on_a_filesystem_whose_dates_are_unsure_is_refused() {
    let fixture = Fixture::new();
    fixture.file(&format!("{FAT_NAME}/tmp/copied"), 1, 2 * HOUR);
    let unsure: Vec<CustomFolder> = [
        FAT_NAME,
        FUSE_NAME,
        OVERLAY_NAME,
        UNMOUNTED_NAME,
        UNREADABLE_MOUNTS_NAME,
    ]
    .into_iter()
    .map(|name| folder(&fixture.directory(&format!("{name}/tmp"))))
    .collect();
    let ext4 = folder(&fixture.directory("disk/tmp"));
    fixture.file("disk/tmp/old", 1, 2 * HOUR);
    let mut entries: Vec<RuleEntry<'_>> = unsure.iter().map(RuleEntry::Custom).collect();
    entries.push(RuleEntry::Custom(&ext4));
    let report = Run::new(&fixture, entries).scan();
    assert_eq!(
        refusals(&report),
        unsure
            .iter()
            .map(|folder| (
                folder.path().to_path_buf(),
                CustomFolderRefusal::UnsureFilesystem
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(offered_paths(&report), [fixture.path("disk/tmp/old")]);
}

#[test]
fn root_never_empties_a_folder_even_its_own() {
    let fixture = Fixture::new();
    // Root's own, as the scan would read it run by root.
    let roots = fixture.directory("root-owned");
    mark(&roots, STICKY_MARK);
    let roots = folder(&roots);
    let mut run = Run::new(&fixture, vec![RuleEntry::Custom(&roots)]);
    run.user = Some(okapi_platform::ROOT_USER);
    assert_eq!(
        refusals(&run.scan()),
        [(roots.path().to_path_buf(), CustomFolderRefusal::NotOwned)]
    );
}

#[test]
fn a_folder_below_a_rule_s_root_keeps_its_contents_to_itself() {
    let fixture = Fixture::new();
    let root = fixture.directory("tmp");
    fixture.file("tmp/theirs", 1, 2 * HOUR);
    fixture.directory("tmp/mine");
    fixture.file("tmp/mine/old", 1, 2 * HOUR);
    // Old: offered whole by the rule, were the folder not its own root.
    fixture.age(&fixture.path("tmp/mine"), 2 * HOUR);
    let rule = target_at(&root);
    let mine = folder(&fixture.path("tmp/mine"));
    let report = Run::new(
        &fixture,
        vec![RuleEntry::Target(&rule), RuleEntry::Custom(&mine)],
    )
    .scan();
    let [rule_group, mine_group] = report.groups() else {
        panic!("{:?}", report.groups());
    };
    let paths = |group: &ScanGroup| -> Vec<PathBuf> {
        group
            .items()
            .iter()
            .map(|item| item.path().to_path_buf())
            .collect()
    };
    assert_eq!(paths(rule_group), [fixture.path("tmp/theirs")]);
    assert!(rule_group.is_selected_by_default());
    assert_eq!(paths(mine_group), [fixture.path("tmp/mine/old")]);
    assert!(!mine_group.is_selected_by_default());
}

/// What the settings screen asks, with the system's readers: only what the
/// machine's filesystem cannot change (the walk of a plain folder is the
/// scan's tests', with stand-in readers).
#[test]
fn the_check_judges_a_folder_against_the_variables_and_the_personal_folders() {
    if okapi_platform::running_as_root() {
        return;
    }
    let fixture = Fixture::new();
    fixture.directory("guest/.config");
    let variables = fixture.variables(&[
        (Variable::Home, "guest"),
        (Variable::Config, "guest/.config"),
    ]);
    assert_eq!(
        folder(&fixture.path("guest/Documents")).check(&variables),
        Err(CustomFolderRefusal::Absent)
    );
    fixture.directory("guest/Documents");
    assert_eq!(
        folder(&fixture.path("guest/Documents")).check(&variables),
        Err(CustomFolderRefusal::Personal),
        "no user-dirs.dirs: the default names are guarded"
    );
    assert_eq!(
        folder(&fixture.path("guest")).check(&variables),
        Err(CustomFolderRefusal::Protected(Variable::Home))
    );
}

/// Through [`crate::scan`] itself: the folders are planned only when their
/// category is chosen, and judged against the personal folders the
/// variables' configuration folder names.
#[test]
fn a_real_scan_plans_the_folders_only_when_chosen_against_the_personal_ones() {
    if okapi_platform::running_as_root() {
        return;
    }
    let fixture = Fixture::new();
    for directory in [
        "guest/.cache",
        "guest/.local/share",
        "tmp",
        "guest/Projets/Docs",
    ] {
        fixture.directory(directory);
    }
    std::fs::create_dir_all(fixture.path("guest/.config")).unwrap();
    std::fs::write(
        fixture.path("guest/.config/user-dirs.dirs"),
        "XDG_DOCUMENTS_DIR=\"$HOME/Projets/Docs\"\n",
    )
    .unwrap();
    let variables = fixture.variables(&[
        (Variable::Home, "guest"),
        (Variable::TempDirectory, "tmp"),
        (Variable::Config, "guest/.config"),
        (Variable::Cache, "guest/.cache"),
        (Variable::LocalShare, "guest/.local/share"),
    ]);
    let rules = okapi_rules::RuleSet::embedded(okapi_rules::Os::Linux).unwrap();
    let docs = folder(&fixture.path("guest/Projets/Docs"));
    let options = |chosen: Category| {
        fixture
            .options(Exclusions::none())
            .with_categories(BTreeSet::from([chosen]))
            .with_custom_folders(vec![docs.clone()])
    };
    let unchosen = crate::scan(&rules, &variables, &options(Category::Gaming));
    assert_eq!(refusals(&unchosen), [], "never planned unchosen");
    let chosen = crate::scan(&rules, &variables, &options(Category::Custom));
    assert_eq!(
        refusals(&chosen),
        [(docs.path().to_path_buf(), CustomFolderRefusal::Personal)],
        "the personal folders read where the variables say"
    );
}

/// A folder moved in keeps its old dates below it (a rename touches only
/// the folder): in the user's folder, a recent directory keeps all it
/// holds; under a rule's root, its old entries are still offered.
#[test]
fn a_folder_just_moved_into_one_of_the_user_s_offers_nothing_of_it() {
    let fixture = Fixture::new();
    for root in ["mine", "rule", "shared"] {
        fixture.file(&format!("{root}/old"), 1, 2 * HOUR);
        // Moved two levels down: its old dates kept below it.
        fixture.file(&format!("{root}/a/moved/2019/a"), 1, 2 * HOUR);
        fixture.file(&format!("{root}/a/moved/b"), 1, 2 * HOUR);
        fixture.age(&fixture.path(&format!("{root}/a/moved/2019")), 2 * HOUR);
        fixture.age(&fixture.path(&format!("{root}/a/moved")), Duration::ZERO);
        fixture.age(&fixture.path(&format!("{root}/a")), Duration::ZERO);
    }
    let mine = folder(&fixture.path("mine"));
    let rule = target_at(&fixture.path("rule"));
    // The rule first: the group named by it, a folder of the user's in it.
    let shared_rule = target_at(&fixture.path("shared"));
    let shared = folder(&fixture.path("shared"));
    let report = Run::new(
        &fixture,
        vec![
            RuleEntry::Custom(&mine),
            RuleEntry::Target(&rule),
            RuleEntry::Target(&shared_rule),
            RuleEntry::Custom(&shared),
        ],
    )
    .scan();
    assert_eq!(
        offered_paths(&report),
        [
            fixture.path("mine/old"),
            fixture.path("rule/a/moved/2019"),
            fixture.path("rule/a/moved/b"),
            fixture.path("rule/old"),
            fixture.path("shared/old"),
        ]
    );
}

#[test]
fn a_folder_whose_owner_or_user_is_unknown_is_refused() {
    let fixture = Fixture::new();
    let plain = folder(&fixture.directory("plain"));
    let unknown = fixture.directory("unknown");
    mark(&unknown, UNKNOWN_OWNER_MARK);
    let unknown = folder(&unknown);
    let mut run = Run::new(
        &fixture,
        vec![RuleEntry::Custom(&plain), RuleEntry::Custom(&unknown)],
    );
    run.user = None;
    assert_eq!(
        refusals(&run.scan()),
        [
            (plain.path().to_path_buf(), CustomFolderRefusal::NotOwned),
            (unknown.path().to_path_buf(), CustomFolderRefusal::NotOwned),
        ]
    );
}

#[test]
fn a_folder_reached_through_a_link_is_judged_where_it_lies() {
    let fixture = Fixture::new();
    fixture.file(&format!("{FAT_NAME}/tmp/copied"), 1, 2 * HOUR);
    symlink(fixture.path(FAT_NAME), fixture.path("usb")).unwrap();
    let through = folder(&fixture.path("usb/tmp"));
    let report = Run::new(&fixture, vec![RuleEntry::Custom(&through)]).scan();
    assert_eq!(
        refusals(&report),
        [(
            through.path().to_path_buf(),
            CustomFolderRefusal::UnsureFilesystem
        )]
    );
}
