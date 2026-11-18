use std::cell::RefCell;

use super::*;
use crate::apps::flatpak::Installation;
use crate::packages::flatpak::tests::{Answer, fake_helper};

/// A revision as snapd lists it.
fn revision(name: &str, number: &str, kind: &str, status: &str) -> Value {
    serde_json::json!({
        "name": name,
        "revision": number,
        "version": format!("v{number}"),
        "type": kind,
        "status": status,
        "installed-size": 4096,
        "install-date": if status == "active" { Some("2026-11-06T15:46:00Z") } else { None },
    })
}

/// Places with no flatpak, the snaps' files in `snap_files`.
fn places(snap_files: &Path) -> InstalledPlaces {
    InstalledPlaces {
        snap_files: snap_files.to_path_buf(),
        installations: Vec::new(),
        runtime: None,
        confined: None,
    }
}

/// What `listed` offers, by snap and revision.
fn offered_of(listed: &Value) -> Vec<(String, String)> {
    old_revisions(listed, Path::new("/nowhere"))
        .unwrap()
        .into_iter()
        .map(|revision| (revision.snap, revision.revision))
        .collect()
}

#[test]
fn only_revisions_older_than_the_single_one_in_use_are_offered() {
    // Catches the revision in use offered, a snap disabled (no revision in
    // use: snapd would remove its current one), a revision newer than the
    // one in use (left by a revert), a kernel's, a broken one, one from a
    // file, and a snap with two in use each offered.
    let mut broken = revision("gimp", "1", "app", "installed");
    broken["broken"] = Value::from("missing file");
    let listed = Value::from(vec![
        revision("android-studio", "3", "app", "installed"),
        revision("android-studio", "5", "app", "installed"),
        revision("android-studio", "7", "app", "active"),
        revision("android-studio", "9", "app", "installed"),
        revision("disabled", "2", "app", "installed"),
        revision("disabled", "4", "app", "installed"),
        revision("twice", "1", "app", "installed"),
        revision("twice", "2", "app", "active"),
        revision("twice", "3", "app", "active"),
        revision("pc-kernel", "1", "kernel", "installed"),
        revision("pc-kernel", "2", "kernel", "active"),
        broken,
        revision("gimp", "2", "app", "active"),
        revision("tried", "x1", "app", "installed"),
        revision("tried", "2", "app", "active"),
        revision("core22", "2987", "base", "installed"),
        revision("core22", "3129", "base", "active"),
        revision("snapd", "28254", "snapd", "installed"),
        revision("snapd", "28332", "snapd", "active"),
    ]);
    let pair = |snap: &str, number: &str| (snap.to_owned(), number.to_owned());
    assert_eq!(
        offered_of(&listed),
        [
            pair("android-studio", "3"),
            pair("android-studio", "5"),
            pair("core22", "2987"),
            pair("snapd", "28254"),
        ]
    );
    let revisions = old_revisions(&listed, Path::new("/nowhere")).unwrap();
    assert_eq!(revisions[0].version, "v3");
    assert_eq!(
        revisions[0].current_since,
        Some("2026-11-06T15:46:00Z".parse().unwrap()),
        "since when the one in use is"
    );
}

#[test]
fn the_room_of_a_revision_comes_back_now_later_never_or_unknown_by_its_links() {
    // Catches a file the download cache shares told freed at once, one a
    // parallel instance shares told given back, and a file not looked at
    // told freed.
    let root = tempfile::tempdir().unwrap();
    let file = |name: &str| {
        let path = root.path().join(name);
        std::fs::write(&path, vec![1_u8; 8192]).unwrap();
        path
    };
    let alone = file("alone_1.snap");
    let cached = file("cached_1.snap");
    std::fs::hard_link(&cached, root.path().join("cache-entry")).unwrap();
    let shared = file("shared_1.snap");
    std::fs::hard_link(&shared, root.path().join("cache-entry-2")).unwrap();
    std::fs::hard_link(&shared, root.path().join("shared_other_1.snap")).unwrap();
    let blocks = std::fs::metadata(&alone).unwrap().blocks() * BLOCK_BYTES;
    assert_eq!(room(&alone), Room::Now(blocks));
    assert_eq!(room(&cached), Room::Later(blocks));
    assert_eq!(room(&shared), Room::Shared(blocks));
    assert_eq!(room(&root.path().join("none_1.snap")), Room::Unknown);
    let link = root.path().join("link_1.snap");
    std::os::unix::fs::symlink(&alone, &link).unwrap();
    assert_eq!(room(&link), Room::Unknown, "a link, never followed");
}

#[test]
fn another_person_is_one_who_logs_in_and_is_not_okapi_s_user() {
    // Catches the system's accounts, nobody, a shell that logs nobody in or
    // Okapi's own user counted as another person.
    let accounts = "root:x:0:0:root:/root:/bin/bash\n\
        daemon:x:1:1::/usr/sbin:/usr/sbin/nologin\n\
        franck:x:1000:1000:Franck:/home/franck:/bin/bash\n\
        nobody:x:65534:65534::/nonexistent:/usr/sbin/nologin\n\
        guest:x:1001:1001::/home/guest:/bin/false\n\
        broken line\n";
    assert!(!others_in(accounts, Some(1000)));
    let shared = format!("{accounts}lea:x:1002:1002::/home/lea:/usr/bin/zsh\n");
    assert!(others_in(&shared, Some(1000)));
    assert!(others_in(accounts, Some(1002)), "Okapi run by another");
    assert!(other_accounts(Path::new("/nonexistent/passwd")), "unread: they may");
}

#[test]
fn okapi_confined_or_without_snapd_proposes_nothing_and_says_why() {
    let root = tempfile::tempdir().unwrap();
    let listed = Value::from(vec![
        revision("gimp", "1", "app", "installed"),
        revision("gimp", "2", "app", "active"),
    ]);
    let go = AtomicBool::new(false);
    let found = find(Ok(listed.clone()), places(root.path()), Path::new(ACCOUNTS), &go);
    assert_eq!(found.report.versions().len(), 1);
    assert_eq!(found.snaps, OldReading::Read);
    assert_eq!(found.flatpaks, OldReading::Absent, "no installation there");

    let confined = InstalledPlaces {
        confined: Some("/.flatpak-info".to_owned()),
        ..places(root.path())
    };
    let found = find(Ok(listed), confined, Path::new(ACCOUNTS), &go);
    assert_eq!(found.report.versions(), []);
    assert!(matches!(found.snaps, OldReading::Unread(_)));
    assert!(matches!(found.flatpaks, OldReading::Unread(_)));

    let absent = Err(SnapdError::Absent {
        socket: PathBuf::from("/run/snapd.socket"),
        source: std::io::ErrorKind::NotFound.into(),
    });
    let found = find(absent, places(root.path()), Path::new(ACCOUNTS), &go);
    assert_eq!(found.snaps, OldReading::Absent);
    let silent = Err(SnapdError::Malformed("x"));
    let found = find(silent, places(root.path()), Path::new(ACCOUNTS), &go);
    assert!(matches!(found.snaps, OldReading::Unread(_)));
}

/// snapd's answer listing `revisions`.
fn listing(revisions: &[Value]) -> String {
    serde_json::json!({"type": "sync", "status-code": 200, "result": revisions}).to_string()
}

const STARTED: &str =
    r#"{"type":"async","status-code":202,"status":"Accepted","result":null,"change":"7"}"#;
const DONE: &str = r#"{"type":"sync","status-code":200,"result":{"status":"Done","ready":true}}"#;
const REFUSED: &str = r#"{"type":"error","status-code":403,"result":{"message":"cancelled","kind":"auth-cancelled"}}"#;

/// Two old revisions of GIMP, found in a report.
fn report_of_two(snap_files: &Path) -> (OldVersionReport, Vec<Value>) {
    let revisions = vec![
        revision("gimp", "1", "app", "installed"),
        revision("gimp", "2", "app", "installed"),
        revision("gimp", "3", "app", "active"),
    ];
    let found = find(
        Ok(Value::from(revisions.clone())),
        places(snap_files),
        Path::new(ACCOUNTS),
        &AtomicBool::new(false),
    );
    (found.report, revisions)
}

/// What [`retire`] told, by revision, asking `snapd`.
fn retired(report: &OldVersionReport, ids: &[OldVersionId], snapd: &Snapd) -> Vec<(String, Retired)> {
    let told = RefCell::new(Vec::new());
    retire(
        report,
        ids,
        snapd,
        &|| Err(FlatpakError::Absent),
        Duration::from_mins(1),
        &|version, retired| {
            let name = match version.kind() {
                OldKind::Revision(revision) => revision.revision.clone(),
                OldKind::Runtime(runtime) => runtime.reference.to_string(),
            };
            told.borrow_mut().push((name, retired.clone()));
        },
    );
    told.into_inner()
}

fn ids(report: &OldVersionReport) -> Vec<OldVersionId> {
    report.versions().iter().map(OldVersion::id).collect()
}

#[test]
fn a_revision_still_old_is_asked_alone_and_followed() {
    // Catches a revision asked without being read again, the whole snap
    // asked, and the second asked before the first ends.
    let root = tempfile::tempdir().unwrap();
    let (report, revisions) = report_of_two(root.path());
    let after_first: Vec<Value> = revisions[1..].to_vec();
    let (_directory, snapd, requests) = crate::packages::snapd::fake::scripted(vec![
        Some(listing(&revisions)),
        Some(STARTED.to_owned()),
        Some(DONE.to_owned()),
        Some(listing(&after_first)),
        Some(STARTED.to_owned()),
        Some(DONE.to_owned()),
    ]);
    let told = retired(&report, &ids(&report), &snapd);
    assert_eq!(
        told,
        [
            ("1".to_owned(), Retired::Removed),
            ("2".to_owned(), Retired::Removed)
        ]
    );
    let asked: Vec<String> = requests.try_iter().collect();
    assert!(asked[0].starts_with("GET /v2/snaps?select=all "), "{}", asked[0]);
    assert!(asked[1].starts_with("POST /v2/snaps/gimp "), "{}", asked[1]);
    assert!(asked[1].ends_with(r#"{"action":"remove","revision":"1"}"#), "{}", asked[1]);
    assert!(asked[4].ends_with(r#"{"action":"remove","revision":"2"}"#), "{}", asked[4]);
}

#[test]
fn a_revision_gone_or_in_use_again_is_never_asked() {
    // Catches a request for a revision snapd no longer lists, or one now in
    // use (snapd would refuse it, or remove the snap's current one).
    let root = tempfile::tempdir().unwrap();
    let (report, revisions) = report_of_two(root.path());
    let first = &ids(&report)[..1];
    let gone: Vec<Value> = revisions[1..].to_vec();
    let (_directory, snapd, requests) =
        crate::packages::snapd::fake::scripted(vec![Some(listing(&gone))]);
    assert_eq!(retired(&report, first, &snapd), [("1".to_owned(), Retired::Gone)]);
    assert_eq!(requests.try_iter().count(), 1);

    let in_use_again = vec![
        revision("gimp", "1", "app", "active"),
        revision("gimp", "2", "app", "installed"),
        revision("gimp", "3", "app", "installed"),
    ];
    let (_directory, snapd, requests) =
        crate::packages::snapd::fake::scripted(vec![Some(listing(&in_use_again))]);
    assert_eq!(retired(&report, first, &snapd), [("1".to_owned(), Retired::Changed)]);
    assert_eq!(requests.try_iter().count(), 1);

    let disabled: Vec<Value> = revisions
        .iter()
        .map(|listed| {
            let mut listed = listed.clone();
            listed["status"] = Value::from("installed");
            listed
        })
        .collect();
    let (_directory, snapd, _) =
        crate::packages::snapd::fake::scripted(vec![Some(listing(&disabled))]);
    assert_eq!(
        retired(&report, first, &snapd),
        [("1".to_owned(), Retired::Changed)],
        "its snap disabled since"
    );
}

#[test]
fn the_rights_refused_ask_no_other_revision() {
    // Catches a polkit window opened again for each revision after a no.
    let root = tempfile::tempdir().unwrap();
    let (report, revisions) = report_of_two(root.path());
    let (_directory, snapd, requests) = crate::packages::snapd::fake::scripted(vec![
        Some(listing(&revisions)),
        Some(REFUSED.to_owned()),
        Some(listing(&revisions)),
    ]);
    assert_eq!(
        retired(&report, &ids(&report), &snapd),
        [
            ("1".to_owned(), Retired::Failed(Why::NotAuthorized)),
            ("2".to_owned(), Retired::NotAsked)
        ]
    );
    assert_eq!(requests.try_iter().count(), 3);
}

#[test]
fn an_identifier_of_another_report_asks_nothing_at_all() {
    // Catches invariant 1: one foreign identifier among others still
    // removing the rest.
    let root = tempfile::tempdir().unwrap();
    let (report, revisions) = report_of_two(root.path());
    let (other, _) = report_of_two(root.path());
    // A snapd that would remove it, were it asked.
    let (_directory, snapd, requests) = crate::packages::snapd::fake::scripted(vec![
        Some(listing(&revisions)),
        Some(STARTED.to_owned()),
        Some(DONE.to_owned()),
    ]);
    let mixed = [ids(&report)[0], ids(&other)[1]];
    assert_eq!(
        retired(&report, &mixed, &snapd),
        [("1".to_owned(), Retired::Failed(Why::Unclear))]
    );
    assert_eq!(requests.try_iter().count(), 0);
}

#[test]
fn a_runtime_still_unused_goes_and_one_used_since_stays() {
    // Catches a runtime asked though an application came to use it since,
    // and what the helper did told for another runtime.
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    let deploy = |kind: &str, id: &str, metadata: &str| {
        let branch = system.join(kind).join(id).join("x86_64/45");
        std::fs::create_dir_all(branch.join("abc")).unwrap();
        std::os::unix::fs::symlink("abc", branch.join("active")).unwrap();
        std::fs::write(branch.join("abc/metadata"), metadata).unwrap();
    };
    std::fs::create_dir_all(system.join("repo")).unwrap();
    deploy("runtime", "org.gnome.Platform", "[Runtime]\n");
    deploy("runtime", "org.kde.Platform", "[Runtime]\n");
    let places = InstalledPlaces {
        installations: vec![Installation::at(system.clone(), true, false)],
        ..places(root.path())
    };
    let found = find(
        Err(SnapdError::Malformed("no snapd here")),
        places,
        Path::new(ACCOUNTS),
        &AtomicBool::new(false),
    );
    assert_eq!(found.flatpaks, OldReading::Read);
    assert_eq!(found.report.versions().len(), 2);
    // An application came to use GNOME's since.
    let app = system.join("app/org.gnome.Maps/x86_64/stable");
    std::fs::create_dir_all(app.join("def")).unwrap();
    std::os::unix::fs::symlink("def", app.join("active")).unwrap();
    std::fs::write(app.join("def/metadata"), "[Application]\nruntime=org.gnome.Platform/x86_64/45\n").unwrap();
    let (helper, heard) = fake_helper(|_, _| Answer::Done);
    let told = RefCell::new(Vec::new());
    retire(
        &found.report,
        &ids(&found.report),
        &Snapd::at(&root.path().join("none"), Duration::from_secs(1)),
        &|| Ok(helper.clone()),
        Duration::from_mins(1),
        &|version, retired| {
            if let OldKind::Runtime(runtime) = version.kind() {
                told.borrow_mut().push((runtime.reference.to_string(), retired.clone()));
            }
        },
    );
    assert_eq!(
        told.into_inner(),
        [
            ("runtime/org.gnome.Platform/x86_64/45".to_owned(), Retired::Changed),
            ("runtime/org.kde.Platform/x86_64/45".to_owned(), Retired::Removed),
        ]
    );
    let asked: Vec<(String, Vec<String>)> = heard
        .try_iter()
        .map(|heard| (heard.member, heard.strings))
        .collect();
    assert_eq!(asked[0].1[0], "runtime/org.kde.Platform/x86_64/45");
    assert_eq!(asked.len(), 2, "the runtime, then the prune: {asked:?}");
}
