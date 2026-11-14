//! The applications read from a sandbox laid out as the system is.

use std::fs;
use std::path::{Path, PathBuf};

use okapi_rules::Language;

use super::flatpak::read_flatpaks;
use super::icons::IconRoots;
use super::snap::read_snaps;
use super::*;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn reading(root: &Path) -> Reading {
    Reading {
        language: Language::French,
        desktops: vec!["ubuntu".to_owned(), "GNOME".to_owned()],
        icons: IconRoots {
            themes: vec![root.join("usr/share/icons")],
            pixmaps: Vec::new(),
            absolute: vec![root.join("snap")],
        },
    }
}

fn launcher(name: &str, extra: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName={name}\n{extra}\n")
}

fn apps(read: SourceReading) -> (Vec<App>, Rest) {
    match read {
        SourceReading::Read { apps, rest } => (apps, rest),
        other => panic!("{other:?}"),
    }
}

/// A dpkg sandbox: `status`, the lists of each package, its launchers.
fn debs(root: &Path) -> DebPlaces {
    let dpkg = root.join("var/lib/dpkg");
    fs::create_dir_all(dpkg.join("updates")).unwrap();
    write(
        &dpkg.join("status"),
        "Package: gimp\nStatus: install ok installed\nArchitecture: amd64\nVersion: 3\nInstalled-Size: 3\n\n\
         Package: nautilus\nStatus: install ok installed\nArchitecture: amd64\nVersion: 49\nInstalled-Size: 1\nPriority: required\n\n\
         Package: libfoo\nStatus: install ok installed\nArchitecture: amd64\nVersion: 1\nInstalled-Size: 5\n\n\
         Package: hidden-tool\nStatus: install ok installed\nArchitecture: amd64\nVersion: 1\nInstalled-Size: 2\n",
    );
    write(
        &dpkg.join("info/gimp.list"),
        "/usr/share/applications/gimp-extra.desktop\n/usr/share/applications/gimp.desktop\n",
    );
    write(
        &dpkg.join("info/nautilus.list"),
        "/usr/share/applications/org.gnome.Nautilus.desktop\n",
    );
    write(
        &dpkg.join("info/hidden-tool.list"),
        "/usr/share/applications/hidden-tool.desktop\n",
    );
    write(&dpkg.join("info/libfoo.list"), "/usr/lib/libfoo.so\n");
    let applications = root.join("usr/share/applications");
    write(
        &applications.join("gimp.desktop"),
        &launcher("GIMP", "Icon=gimp"),
    );
    write(
        &applications.join("gimp-extra.desktop"),
        &launcher("GIMP extra", ""),
    );
    write(
        &applications.join("org.gnome.Nautilus.desktop"),
        &launcher("Fichiers", ""),
    );
    write(
        &applications.join("hidden-tool.desktop"),
        &launcher("Hidden", "NoDisplay=true"),
    );
    write(
        &root.join("usr/share/icons/hicolor/scalable/apps/gimp.svg"),
        "<svg/>",
    );
    let extended_states = root.join("var/lib/apt/extended_states");
    write(
        &extended_states,
        "Package: gimp\nArchitecture: amd64\nAuto-Installed: 1\n",
    );
    DebPlaces {
        dpkg,
        extended_states,
        root: root.to_owned(),
    }
}

#[test]
fn a_package_with_launchers_is_one_app_and_the_others_their_rest() {
    let root = tempfile::tempdir().unwrap();
    let places = debs(root.path());
    let (apps, rest) = apps(read_debs(&places, &reading(root.path())));
    let names: Vec<&str> = apps.iter().map(|app| app.name.as_str()).collect();
    assert_eq!(
        names,
        ["GIMP", "Fichiers"],
        "one line for gimp's two launchers"
    );
    let gimp = &apps[0];
    assert_eq!(gimp.id, "gimp:amd64");
    assert_eq!(gimp.app_bytes, Some(3 * 1024));
    assert_eq!(gimp.origin, Some(Origin::AsDependency));
    assert_eq!(
        gimp.icon.as_deref(),
        Some(
            root.path()
                .join("usr/share/icons/hicolor/scalable/apps/gimp.svg")
                .as_path()
        )
    );
    assert!(apps[1].system && apps[1].origin == Some(Origin::WithSystem));
    // libfoo and the tool hidden from the menu: 5 + 2 KiB.
    assert_eq!(
        rest,
        Rest {
            count: 2,
            bytes: 7 * 1024
        }
    );
}

#[test]
fn packages_are_not_read_while_dpkg_writes() {
    let root = tempfile::tempdir().unwrap();
    let places = debs(root.path());
    write(&places.dpkg.join("updates/0001"), "");
    assert_eq!(
        read_debs(&places, &reading(root.path())),
        SourceReading::Busy
    );
    let none = DebPlaces {
        dpkg: root.path().join("none"),
        ..places
    };
    assert_eq!(
        read_debs(&none, &reading(root.path())),
        SourceReading::Absent
    );
}

fn snapd_list(root: &Path) -> serde_json::Value {
    let applications = root.join("var/lib/snapd/desktop/applications");
    write(
        &applications.join("android-studio_android-studio.desktop"),
        &launcher(
            "Android Studio",
            "Icon=/snap/android-studio/current/bin/studio.png",
        ),
    );
    write(
        &applications.join("daemon_daemon.desktop"),
        &launcher("Daemon", "NoDisplay=true"),
    );
    let desktop = |name: &str| format!("/var/lib/snapd/desktop/applications/{name}.desktop");
    serde_json::json!([
        { "name": "android-studio", "type": "app", "status": "active", "version": "2026.1",
          "installed-size": 3000, "publisher": { "display-name": "Snapcrafters" },
          "apps": [{ "name": "android-studio", "desktop-file": desktop("android-studio_android-studio") }] },
        { "name": "android-studio", "type": "app", "status": "installed", "installed-size": 1000 },
        { "name": "android-studio", "type": "app", "status": "installed", "installed-size": 500 },
        { "name": "core24", "type": "base", "status": "active", "installed-size": 70 },
        { "name": "daemon", "type": "app", "status": "active", "installed-size": 9,
          "apps": [{ "name": "daemon", "desktop-file": desktop("daemon_daemon") }] },
        { "name": "cli", "type": "app", "status": "active", "installed-size": 20, "apps": [{ "name": "cli" }] },
        { "name": "cli", "type": "app", "status": "installed", "installed-size": 4 },
        { "name": "../evil", "type": "app", "status": "active", "installed-size": 1 },
    ])
}

#[test]
fn a_snap_keeps_its_old_versions_and_the_others_are_its_rest() {
    let root = tempfile::tempdir().unwrap();
    let list = snapd_list(root.path());
    let home = root.path().join("home");
    let (apps, rest) = apps(read_snaps(
        &list,
        root.path(),
        Some(&home),
        &reading(root.path()),
    ));
    assert_eq!(apps.len(), 1);
    let studio = &apps[0];
    assert_eq!(studio.name, "Android Studio");
    assert_eq!(studio.app_bytes, Some(3000));
    assert_eq!(
        (studio.old_bytes, studio.old_versions),
        (1500, 2),
        "both disabled revisions"
    );
    assert_eq!(studio.publisher.as_deref(), Some("Snapcrafters"));
    assert_eq!(studio.data, Some(home.join("snap/android-studio")));
    // core24, the hidden daemon, the tool without a launcher and its old
    // revision; never the refused name.
    assert_eq!(
        rest,
        Rest {
            count: 3,
            bytes: 70 + 9 + 20 + 4
        }
    );
}

#[test]
fn a_list_snapd_cannot_be_read_is_a_failure() {
    let root = tempfile::tempdir().unwrap();
    let read = read_snaps(
        &serde_json::json!({ "not": "a list" }),
        root.path(),
        None,
        &reading(root.path()),
    );
    assert!(matches!(read, SourceReading::Failed(_)), "{read:?}");
}

/// A flatpak installation with `id` deployed, `active` naming `target`.
fn deploy(installation: &Path, id: &str, target: &str) -> PathBuf {
    let branch = installation.join("app").join(id).join("x86_64/stable");
    let deployment = branch.join("abc123");
    fs::create_dir_all(&deployment).unwrap();
    write(
        &deployment.join(format!("export/share/applications/{id}.desktop")),
        &launcher(id, ""),
    );
    std::os::unix::fs::symlink(target, branch.join("active")).unwrap();
    deployment
}

#[test]
fn a_flatpak_is_read_from_its_active_deployment_never_through_a_link_elsewhere() {
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    let user = root.path().join("user");
    let gimp = deploy(&system, "org.gimp.GIMP", "abc123");
    deploy(&user, "org.example.Away", "../../../../../elsewhere");
    let linked = deploy(&user, "org.example.Linked", "linked");
    std::os::unix::fs::symlink(&gimp, linked.parent().unwrap().join("linked")).unwrap();
    let home = root.path().join("home");
    let (apps, rest) = apps(read_flatpaks(
        &[system, user, root.path().join("none")],
        Some(&home),
        &reading(root.path()),
    ));
    assert_eq!(apps.len(), 1, "{apps:?}");
    assert_eq!(apps[0].id, "org.gimp.GIMP");
    assert_eq!(apps[0].deployment, Some(gimp));
    assert_eq!(apps[0].app_bytes, None, "measured");
    assert_eq!(apps[0].data, Some(home.join(".var/app/org.gimp.GIMP")));
    assert_eq!(rest, Rest::default());
    assert_eq!(
        read_flatpaks(&[root.path().join("none")], None, &reading(root.path())),
        SourceReading::Absent
    );
}
