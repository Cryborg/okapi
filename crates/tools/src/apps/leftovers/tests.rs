//! Leftovers found and moved in a sandbox: a home folder, a trash of its
//! own, what is installed given by hand.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use okapi_rules::{TemplateVariables, Variable};

use super::*;

/// A home folder and a trash beside it.
struct Sandbox {
    _root: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
}

fn sandbox() -> Sandbox {
    let root = tempfile::tempdir().unwrap();
    let canonical = fs::canonicalize(root.path()).unwrap();
    let home = canonical.join("home");
    let data = canonical.join("data");
    fs::create_dir_all(home.join(SNAP_DATA)).unwrap();
    fs::create_dir_all(home.join(FLATPAK_DATA)).unwrap();
    fs::create_dir_all(&data).unwrap();
    Sandbox {
        _root: root,
        home,
        data,
    }
}

impl Sandbox {
    /// A leftover-to-be: `name` under `kind`'s folder, a file two levels
    /// down.
    fn folder(&self, kind: LeftoverKind, name: &str) -> PathBuf {
        let folder = self.home.join(kind.folder()).join(name);
        fs::create_dir_all(folder.join("deep/er")).unwrap();
        fs::write(folder.join("deep/er/file"), b"0123456789").unwrap();
        folder
    }

    fn places(&self) -> Places {
        Places {
            home: self.home.clone(),
            user: okapi_platform::effective_user().unwrap(),
            most_entries: MOST_ENTRIES,
        }
    }

    fn find(&self, installed: &Installed) -> LeftoverReport {
        find(
            &self.places(),
            installed,
            &nothing_protected(),
            &Exclusions::none(),
        )
    }

    fn bin(&self) -> Trash {
        Trash::in_folder(&self.data).unwrap()
    }
}

fn nothing_protected() -> ProtectedRoots {
    ProtectedRoots::of(&TemplateVariables::new()).unwrap()
}

/// What a move checks again: no variable, `exclusions`.
fn rules(exclusions: &Exclusions) -> TrashRules<'_> {
    static NOTHING: std::sync::LazyLock<ProtectedRoots> =
        std::sync::LazyLock::new(|| ProtectedRoots::of(&TemplateVariables::new()).unwrap());
    TrashRules {
        protected: &NOTHING,
        exclusions,
    }
}

fn read(names: &[&str]) -> Known {
    Known::Read(
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<BTreeSet<_>>(),
    )
}

fn installed(snaps: &[&str], flatpaks: &[&str]) -> Installed {
    Installed {
        snaps: read(snaps),
        flatpaks: read(flatpaks),
    }
}

fn names(report: &LeftoverReport) -> Vec<(LeftoverKind, &str)> {
    report
        .leftovers()
        .iter()
        .map(|leftover| (leftover.kind(), leftover.name()))
        .collect()
}

#[test]
fn a_folder_no_application_installed_bears_is_a_leftover_and_only_that() {
    // Catches an installed one proposed, a link, a file or a name no snap
    // could have proposed, and a kind's folders taken for another's.
    let sandbox = sandbox();
    sandbox.folder(LeftoverKind::Snap, "spotify");
    sandbox.folder(LeftoverKind::Snap, "firefox");
    sandbox.folder(LeftoverKind::Snap, "Not_A_Snap");
    fs::write(sandbox.home.join("snap/file"), b"").unwrap();
    std::os::unix::fs::symlink(&sandbox.data, sandbox.home.join("snap/linked")).unwrap();
    sandbox.folder(LeftoverKind::Flatpak, "org.gimp.GIMP");
    sandbox.folder(LeftoverKind::Flatpak, "org.inkscape.Inkscape");
    let report = sandbox.find(&installed(&["firefox"], &["org.inkscape.Inkscape"]));
    assert_eq!(
        names(&report),
        [
            (LeftoverKind::Snap, "spotify"),
            (LeftoverKind::Flatpak, "org.gimp.GIMP"),
        ]
    );
    let spotify = &report.leftovers()[0];
    assert_eq!(spotify.path(), sandbox.home.join("snap/spotify"));
    assert!(spotify.bytes().is_some_and(|bytes| bytes > 0));
    assert_eq!(report.unread(), []);
}

#[test]
fn a_source_not_read_whole_proposes_nothing_of_its_own() {
    // Catches a snapd that did not answer taken for no snap installed: the
    // profiles of snaps in use would be proposed.
    let sandbox = sandbox();
    sandbox.folder(LeftoverKind::Snap, "firefox");
    sandbox.folder(LeftoverKind::Flatpak, "org.gimp.GIMP");
    let report = sandbox.find(&Installed {
        snaps: Known::Unread("snapd silent".to_owned()),
        flatpaks: read(&[]),
    });
    assert_eq!(names(&report), [(LeftoverKind::Flatpak, "org.gimp.GIMP")]);
    assert_eq!(report.unread(), [LeftoverKind::Snap]);
}

#[test]
fn a_variable_s_folder_or_an_ancestor_of_one_is_never_a_leftover() {
    // Catches `TMPDIR` itself let through as the engine lets it (invariant 6
    // for a leftover is stricter), and an ancestor of a variable.
    let sandbox = sandbox();
    let tmp = sandbox.folder(LeftoverKind::Flatpak, "org.tmp.Here");
    let holding = sandbox.folder(LeftoverKind::Flatpak, "org.config.Holder");
    sandbox.folder(LeftoverKind::Flatpak, "org.gimp.GIMP");
    let mut variables = TemplateVariables::new();
    variables.insert(Variable::TempDirectory, tmp).unwrap();
    variables
        .insert(Variable::Config, holding.join("deep"))
        .unwrap();
    let report = find(
        &sandbox.places(),
        &installed(&[], &[]),
        &ProtectedRoots::of(&variables).unwrap(),
        &Exclusions::none(),
    );
    assert_eq!(names(&report), [(LeftoverKind::Flatpak, "org.gimp.GIMP")]);
}

#[test]
fn an_excluded_folder_or_one_too_large_is_never_moved() {
    // Catches the user's exclusions ignored, and a folder too large to be
    // checked proposed for the trash all the same.
    let sandbox = sandbox();
    let excluded = sandbox.folder(LeftoverKind::Snap, "kept");
    sandbox.folder(LeftoverKind::Snap, "large");
    let places = Places {
        most_entries: 2,
        ..sandbox.places()
    };
    let report = find(
        &places,
        &installed(&[], &[]),
        &nothing_protected(),
        &Exclusions::new([excluded]).unwrap(),
    );
    assert_eq!(names(&report), [(LeftoverKind::Snap, "large")]);
    let large = &report.leftovers()[0];
    assert!(!large.checkable());
    assert_eq!(large.bytes(), None);
    let receipt = trash(
        &report,
        &[large.id()],
        &sandbox.bin(),
        &|| installed(&[], &[]),
        &rules(&Exclusions::none()),
    );
    assert_eq!(
        receipt.outcomes()[0].result,
        TrashResult::Kept(KeepReason::NotCheckable)
    );
    assert!(sandbox.home.join("snap/large").exists());
}

#[test]
fn a_folder_holding_what_the_user_excludes_is_never_proposed_nor_moved() {
    // Catches a leftover moved whole with an excluded file or folder inside
    // it, the exclusion written before the analysis or after it.
    let sandbox = sandbox();
    let vault = sandbox.folder(LeftoverKind::Flatpak, "org.keepassxc.KeePassXC");
    fs::write(vault.join("deep/coffre.KDBX"), b"secret").unwrap();
    let notes = sandbox.folder(LeftoverKind::Flatpak, "org.notes.Notes");
    let by_name = Exclusions::new([PathBuf::from("*.kdbx")]).unwrap();
    let below = Exclusions::new([notes.join("deep/er")]).unwrap();
    let above = Exclusions::new([sandbox.home.join(FLATPAK_DATA)]).unwrap();
    // An extension the folder's own name ends with.
    let itself = Exclusions::new([PathBuf::from("*.notes")]).unwrap();
    for (exclusions, left) in [
        (&by_name, vec!["org.notes.Notes"]),
        (&below, vec!["org.keepassxc.KeePassXC"]),
        (&above, vec![]),
        (&itself, vec!["org.keepassxc.KeePassXC"]),
    ] {
        let report = find(
            &sandbox.places(),
            &installed(&[], &[]),
            &nothing_protected(),
            exclusions,
        );
        let found: Vec<&str> = report.leftovers().iter().map(Leftover::name).collect();
        assert_eq!(found, left, "{exclusions:?}");
        assert_eq!(report.withheld(), 0, "excluded is no doubt");
    }
    // Each written after the analysis, each moving only the folder it
    // touches.
    let report = sandbox.find(&installed(&[], &[]));
    let id_of = |path: &Path| {
        report
            .leftovers()
            .iter()
            .find(|leftover| leftover.path() == path)
            .unwrap()
            .id()
    };
    for (exclusions, folder) in [(&by_name, &vault), (&below, &notes), (&above, &notes)] {
        let receipt = trash(
            &report,
            &[id_of(folder)],
            &sandbox.bin(),
            &|| installed(&[], &[]),
            &rules(exclusions),
        );
        assert_eq!(
            receipt.outcomes()[0].result,
            TrashResult::Kept(KeepReason::Excluded),
            "{exclusions:?}"
        );
    }
    assert!(vault.join("deep/coffre.KDBX").exists());
    assert!(notes.join("deep/er/file").exists());
}

#[test]
fn a_folder_become_a_variable_s_or_holding_an_excluded_file_by_another_name_stays() {
    // Catches invariant 6 checked at the analysis only, and an excluded
    // file reached by a hard link of another name moved along.
    let sandbox = sandbox();
    let folder = sandbox.folder(LeftoverKind::Snap, "spotify");
    let report = sandbox.find(&installed(&[], &[]));
    let id = report.leftovers()[0].id();
    let mut variables = TemplateVariables::new();
    variables.insert(Variable::Config, folder.clone()).unwrap();
    let protected = ProtectedRoots::of(&variables).unwrap();
    let receipt = trash(
        &report,
        &[id],
        &sandbox.bin(),
        &|| installed(&[], &[]),
        &TrashRules {
            protected: &protected,
            exclusions: &Exclusions::none(),
        },
    );
    assert_eq!(
        receipt.outcomes()[0].result,
        TrashResult::Kept(KeepReason::Protected)
    );

    let kept = sandbox.data.join("keep");
    fs::write(&kept, b"mine").unwrap();
    fs::hard_link(&kept, folder.join("deep/other-name")).unwrap();
    let exclusions = Exclusions::new([kept]).unwrap();
    let report = find(
        &sandbox.places(),
        &installed(&[], &[]),
        &nothing_protected(),
        &exclusions,
    );
    assert_eq!(names(&report), []);
    assert_eq!(report.withheld(), 0, "excluded, no doubt");
}

#[test]
fn a_folder_of_leftovers_that_is_a_link_or_unreadable_proposes_nothing_and_says_so() {
    // Catches `~/snap` swapped for a link followed (folders of the user's
    // documents proposed), and one unreadable said to hold nothing.
    let sandbox = sandbox();
    let documents = sandbox.home.join("documents");
    fs::create_dir_all(documents.join("photos")).unwrap();
    fs::remove_dir(sandbox.home.join(SNAP_DATA)).unwrap();
    std::os::unix::fs::symlink(&documents, sandbox.home.join(SNAP_DATA)).unwrap();
    sandbox.folder(LeftoverKind::Flatpak, "org.gimp.GIMP");
    let report = sandbox.find(&installed(&[], &[]));
    assert_eq!(names(&report), [(LeftoverKind::Flatpak, "org.gimp.GIMP")]);
    assert_eq!(report.unread(), [LeftoverKind::Snap]);

    // Closed to its owner: not listed, never said empty.
    if okapi_platform::running_as_root() {
        return;
    }
    let flatpaks = sandbox.home.join(FLATPAK_DATA);
    let closed = |mode| {
        fs::set_permissions(&flatpaks, fs::Permissions::from_mode(mode)).unwrap();
    };
    closed(0o300);
    let report = sandbox.find(&installed(&[], &[]));
    closed(0o700);
    assert_eq!(names(&report), []);
    assert_eq!(report.unread(), [LeftoverKind::Snap, LeftoverKind::Flatpak]);
}

#[test]
fn a_folder_that_cannot_be_read_whole_is_withheld_and_counted() {
    // Catches a folder not proposed for safety passed over without a word:
    // the screen would say nothing was left.
    if okapi_platform::running_as_root() {
        return;
    }
    let sandbox = sandbox();
    let folder = sandbox.folder(LeftoverKind::Snap, "spotify");
    sandbox.folder(LeftoverKind::Snap, "vlc");
    fs::set_permissions(folder.join("deep"), fs::Permissions::from_mode(0o000)).unwrap();
    let report = sandbox.find(&installed(&[], &[]));
    fs::set_permissions(folder.join("deep"), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(names(&report), [(LeftoverKind::Snap, "vlc")]);
    assert_eq!(report.withheld(), 1);
}

/// Moves `name`'s leftover of `report`, what is installed then `now`.
fn moved(sandbox: &Sandbox, report: &LeftoverReport, now: &Installed) -> TrashResult {
    let id = report.leftovers()[0].id();
    let now = now.clone();
    trash(
        report,
        &[id],
        &sandbox.bin(),
        &move || now.clone(),
        &rules(&Exclusions::none()),
    )
    .outcomes()[0]
        .result
}

#[test]
fn a_leftover_goes_to_the_trash_restorable() {
    let sandbox = sandbox();
    let folder = sandbox.folder(LeftoverKind::Snap, "spotify");
    let report = sandbox.find(&installed(&[], &[]));
    let bytes = report.leftovers()[0].bytes().unwrap();
    assert_eq!(
        moved(&sandbox, &report, &installed(&[], &[])),
        TrashResult::Trashed { bytes }
    );
    assert!(!folder.exists());
    assert_eq!(
        fs::read(sandbox.data.join("Trash/files/spotify/deep/er/file")).unwrap(),
        b"0123456789"
    );
    let info = fs::read_to_string(sandbox.data.join("Trash/info/spotify.trashinfo")).unwrap();
    assert!(
        info.contains(&format!("Path={}", folder.display())),
        "{info}"
    );
}

#[test]
fn each_check_before_the_move_alone_keeps_the_leftover() {
    // Catches each check of invariant 7 for a folder: another report's
    // identifier, the application installed again or its source unread,
    // the folder replaced, written to its depths, or its parent turned into
    // a link.
    let sandbox = sandbox();
    sandbox.folder(LeftoverKind::Snap, "spotify");

    let other = sandbox.find(&installed(&[], &[]));
    let report = sandbox.find(&installed(&[], &[]));
    let receipt = trash(
        &report,
        &[other.leftovers()[0].id()],
        &sandbox.bin(),
        &|| installed(&[], &[]),
        &rules(&Exclusions::none()),
    );
    assert_eq!(
        receipt.outcomes()[0].result,
        TrashResult::Kept(KeepReason::UnknownLeftover)
    );
    assert_eq!(
        moved(&sandbox, &report, &installed(&["spotify"], &[])),
        TrashResult::Kept(KeepReason::Installed)
    );
    let unread = Installed {
        snaps: Known::Unread("silent".to_owned()),
        flatpaks: read(&[]),
    };
    assert_eq!(
        moved(&sandbox, &report, &unread),
        TrashResult::Kept(KeepReason::SourceUnread)
    );

    // Written in place, two levels down, after the analysis, its date of
    // modification set back: only the inode's change tells.
    std::thread::sleep(Duration::from_millis(50));
    let file = sandbox.home.join("snap/spotify/deep/er/file");
    let modified = fs::metadata(&file).unwrap().modified().unwrap();
    fs::write(&file, b"9876543210").unwrap();
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(
        moved(&sandbox, &report, &installed(&[], &[])),
        TrashResult::Kept(KeepReason::Changed)
    );

    // Gone since.
    let report = sandbox.find(&installed(&[], &[]));
    fs::rename(
        sandbox.home.join("snap/spotify"),
        sandbox.home.join("snap/away"),
    )
    .unwrap();
    assert_eq!(
        moved(&sandbox, &report, &installed(&[], &[])),
        TrashResult::Kept(KeepReason::Gone)
    );
    fs::rename(
        sandbox.home.join("snap/away"),
        sandbox.home.join("snap/spotify"),
    )
    .unwrap();

    // Replaced by another folder of its name.
    let report = sandbox.find(&installed(&[], &[]));
    fs::rename(
        sandbox.home.join("snap/spotify"),
        sandbox.home.join("snap/aside"),
    )
    .unwrap();
    sandbox.folder(LeftoverKind::Snap, "spotify");
    assert_eq!(
        moved(&sandbox, &report, &installed(&[], &[])),
        TrashResult::Kept(KeepReason::Changed)
    );

    // Its parent turned into a link to a folder holding a twin.
    let report = sandbox.find(&installed(&[], &[]));
    let elsewhere = sandbox.home.join("elsewhere");
    fs::rename(sandbox.home.join("snap"), &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, sandbox.home.join("snap")).unwrap();
    assert_eq!(
        moved(&sandbox, &report, &installed(&[], &[])),
        TrashResult::Kept(KeepReason::Changed)
    );
    assert!(elsewhere.join("spotify").exists(), "nothing moved");
}

#[test]
fn a_leftover_never_moves_while_a_duplicate_does() {
    // Catches a lock of its own: a duplicate's keeper could go with the
    // leftover holding it while the duplicate goes too.
    let sandbox = std::sync::Arc::new(sandbox());
    sandbox.folder(LeftoverKind::Snap, "spotify");
    let report = std::sync::Arc::new(sandbox.find(&installed(&[], &[])));
    let held = crate::trashing::one_at_a_time();
    let mover = {
        let (sandbox, report) = (
            std::sync::Arc::clone(&sandbox),
            std::sync::Arc::clone(&report),
        );
        std::thread::spawn(move || moved(&sandbox, &report, &installed(&[], &[])))
    };
    std::thread::sleep(Duration::from_millis(100));
    assert!(sandbox.home.join("snap/spotify").exists(), "waits its turn");
    drop(held);
    assert!(matches!(mover.join().unwrap(), TrashResult::Trashed { .. }));
}

#[test]
fn nothing_installed_is_known_when_okapi_runs_confined() {
    // Catches Okapi in a container, a toolbox or a sandbox, which sees the
    // home folder but not the system that filled it: no snapd, no flatpak
    // installation there would propose every snap's and flatpak's data.
    let sandbox = sandbox();
    sandbox.folder(LeftoverKind::Snap, "firefox");
    sandbox.folder(LeftoverKind::Flatpak, "org.gimp.GIMP");
    let gone = sandbox.data.join("gone");
    let visible = InstalledPlaces {
        snap_files: gone.join("snaps"),
        installations: vec![installation(&gone.join("flatpak"), false)],
        runtime: Some(gone.join("run")),
        confined: None,
    };
    // A system seen whole without them: none installed, as before.
    let seen = installed::installed_with(&visible, read(&[]));
    assert_eq!(seen, installed(&[], &[]));
    assert_eq!(sandbox.find(&seen).leftovers().len(), 2);
    let confined = InstalledPlaces {
        confined: Some("/run/.containerenv".to_owned()),
        ..visible
    };
    for installed in [
        installed::installed_with(&confined, read(&[])),
        installed::installed_now(&confined),
    ] {
        assert!(matches!(installed.snaps, Known::Unread(_)), "{installed:?}");
        assert!(
            matches!(installed.flatpaks, Known::Unread(_)),
            "{installed:?}"
        );
        let report = sandbox.find(&installed);
        assert_eq!(report.leftovers(), []);
        assert_eq!(report.unread(), [LeftoverKind::Snap, LeftoverKind::Flatpak]);
    }
}

#[test]
fn snaps_are_unread_when_snapd_is_silent_and_snaps_are_there() {
    // Catches a snapd that did not answer taken for no snap installed. A
    // system without snapd nor its files has none: unless Okapi runs
    // confined (`nothing_installed_is_known_when_okapi_runs_confined`).
    let root = tempfile::tempdir().unwrap();
    let files = root.path().join("snaps");
    let silent = || Err("snapd silent".to_owned());
    assert_eq!(
        installed::snaps(silent(), &files),
        read(&[]),
        "no snapd, no snap"
    );
    fs::create_dir(&files).unwrap();
    fs::write(files.join("partial"), b"").unwrap();
    assert_eq!(
        installed::snaps(silent(), &files),
        read(&[]),
        "no snap file"
    );
    fs::write(files.join("firefox_4793.snap"), b"").unwrap();
    assert!(matches!(
        installed::snaps(silent(), &files),
        Known::Unread(_)
    ));
    assert_eq!(
        installed::snaps(Ok(vec!["firefox".to_owned()]), &files),
        read(&["firefox"])
    );
}

/// A flatpak `id` deployed in `installation`, its main launcher saying
/// `extra`.
fn deployed(installation: &Path, id: &str, extra: &str) {
    let branch = installation.join("app").join(id).join("x86_64/stable");
    let launchers = branch.join("abc123/export/share/applications");
    fs::create_dir_all(&launchers).unwrap();
    fs::write(
        launchers.join(format!("{id}.desktop")),
        format!("[Desktop Entry]\nName={id}\nNoDisplay=true\n{extra}\n"),
    )
    .unwrap();
    std::os::unix::fs::symlink("abc123", branch.join("active")).unwrap();
}

fn installation(path: &Path, declared: bool) -> crate::apps::flatpak::Installation {
    crate::apps::flatpak::Installation::at(path.to_path_buf(), false, declared)
}

#[test]
fn flatpaks_installed_are_those_of_every_installation_their_former_ids_and_those_running() {
    // Catches an application hidden from the menu, a declared installation,
    // an application renamed (its data not yet moved) or one running after
    // its uninstallation taken for none.
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    let declared = root.path().join("sd");
    deployed(&system, "org.hidden.App", "");
    deployed(
        &system,
        "org.gnome.TextEditor",
        "X-Flatpak-RenamedFrom=org.gnome.gedit.desktop;gedit.desktop;",
    );
    deployed(&declared, "org.on.Card", "");
    let runtime = root.path().join("run");
    let instance = runtime.join(".flatpak/1234");
    fs::create_dir_all(&instance).unwrap();
    fs::write(
        instance.join("info"),
        "[Application]\nname=org.still.Running\nruntime=runtime/x\n\n[Instance]\nname=other\n",
    )
    .unwrap();
    let known = installed::flatpaks(
        &[installation(&system, false), installation(&declared, true)],
        Some(&runtime),
    );
    assert_eq!(
        known,
        read(&[
            // A name the author listed too: protected all the same.
            "gedit",
            "org.gnome.TextEditor",
            "org.gnome.gedit",
            "org.hidden.App",
            "org.on.Card",
            "org.still.Running",
        ])
    );
    // A declared installation unplugged: unread, never none.
    let gone = root.path().join("gone");
    let unplugged = installed::flatpaks(&[installation(&gone, true)], Some(&runtime));
    assert!(matches!(unplugged, Known::Unread(_)));
    assert_eq!(
        installed::flatpaks(&[installation(&gone, false)], Some(&gone)),
        read(&[])
    );
    // Nowhere to read what runs: unread. A file beside the instances: none.
    assert!(matches!(
        installed::flatpaks(&[installation(&gone, false)], None),
        Known::Unread(_)
    ));
    fs::write(runtime.join(".flatpak/stray"), b"").unwrap();
    assert!(matches!(
        installed::flatpaks(&[], Some(&runtime)),
        Known::Read(_)
    ));
}

#[test]
fn an_installation_behind_a_link_leading_nowhere_or_without_its_repository_is_unread() {
    // Catches a disk not mounted taken for no flatpak installed: every
    // application's data there would be proposed, then moved.
    let root = tempfile::tempdir().unwrap();
    let linked = root.path().join("linked");
    std::os::unix::fs::symlink(root.path().join("unmounted/flatpak"), &linked).unwrap();
    let empty = root.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let runtime = root.path().join("run");
    for place in [&linked, &empty] {
        assert!(
            matches!(
                installed::flatpaks(&[installation(place, false)], Some(&runtime)),
                Known::Unread(_)
            ),
            "{}",
            place.display()
        );
    }
    // Under a link that leads somewhere, an installation missing is none.
    let real = root.path().join("real");
    fs::create_dir(&real).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    assert_eq!(
        installed::flatpaks(
            &[installation(&alias.join("flatpak"), false)],
            Some(&runtime)
        ),
        read(&[])
    );
    // Snaps likewise: snapd silent, its files behind a link to nowhere.
    let snap_files = root.path().join("snaps");
    std::os::unix::fs::symlink(root.path().join("nowhere"), &snap_files).unwrap();
    assert!(matches!(
        installed::snaps(Err("snapd silent".to_owned()), &snap_files),
        Known::Unread(_)
    ));
}

#[test]
fn every_snap_listed_counts_installed_and_a_list_unreadable_is_no_list() {
    // Catches a base or a snap without a launcher missed (its data then
    // proposed), and a malformed answer taken for no snap.
    let listed = serde_json::json!([
        {"name": "firefox", "type": "app"},
        {"name": "core22", "type": "base"},
        {"name": "firefox", "type": "app", "status": "disabled"},
    ]);
    assert_eq!(
        installed::snap_names(&listed).unwrap(),
        ["firefox", "core22", "firefox"]
    );
    assert!(installed::snap_names(&serde_json::json!({"name": "x"})).is_err());
    assert!(installed::snap_names(&serde_json::json!([{"type": "app"}])).is_err());
}
