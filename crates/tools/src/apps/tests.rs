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
    assert_eq!(gimp.size, AppSize::Known(3 * 1024));
    assert_eq!(
        gimp.target,
        Some(uninstall::Target::Deb {
            package: "gimp".to_owned(),
            architecture: "amd64".to_owned()
        })
    );
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
    assert_eq!(studio.size, AppSize::Known(3000));
    assert_eq!(
        studio.target,
        Some(uninstall::Target::Snap {
            name: "android-studio".to_owned(),
            purge: false
        })
    );
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
    assert_eq!(apps[0].size, AppSize::ToMeasure(gimp), "measured");
    assert_eq!(apps[0].target, None, "never uninstalled yet");
    assert_eq!(apps[0].data, Some(home.join(".var/app/org.gimp.GIMP")));
    assert_eq!(rest, Rest::default());
    assert_eq!(
        read_flatpaks(&[root.path().join("none")], None, &reading(root.path())),
        SourceReading::Absent
    );
}

fn app(source: Source, id: &str, data: Option<PathBuf>) -> App {
    App {
        source,
        id: id.to_owned(),
        package: id.to_owned(),
        name: id.to_owned(),
        version: String::new(),
        publisher: None,
        origin: None,
        icon: None,
        size: AppSize::Known(1),
        old_bytes: 0,
        old_versions: 0,
        data,
        system: false,
        target: None,
    }
}

#[test]
fn a_slow_source_never_holds_the_others_and_each_says_its_folders_then_its_end() {
    use std::sync::mpsc;
    use std::time::Duration;

    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("snap/x");
    fs::create_dir_all(&data).unwrap();
    let (release, released) = mpsc::channel::<()>();
    let slow: Reader = Box::new(move || {
        released.recv().unwrap();
        SourceReading::Absent
    });
    let quick_app = app(Source::Snap, "x", Some(data));
    let quick: Reader = Box::new(move || SourceReading::Read {
        apps: vec![quick_app],
        rest: Rest::default(),
    });
    let (sink, arrivals) = mpsc::channel();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    read_all(
        vec![(Source::Deb, slow), (Source::Snap, quick)],
        &sink,
        &stop,
    );
    drop(sink);
    let next = || arrivals.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(
        next(),
        Arrival::Source(Source::Snap, SourceReading::Read { .. })
    ));
    assert!(matches!(
        next(),
        Arrival::Measured(key, Part::Data, measure::Measured::Bytes { .. }) if key.id == "x"
    ));
    release.send(()).unwrap();
    assert!(matches!(
        next(),
        Arrival::Source(Source::Deb, SourceReading::Absent)
    ));
    assert!(
        matches!(
            arrivals.recv_timeout(Duration::from_secs(10)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ),
        "every sender gone"
    );
}

#[test]
fn a_package_of_another_architecture_or_of_every_one_keeps_its_own_launcher_and_origin() {
    let root = tempfile::tempdir().unwrap();
    let places = debs(root.path());
    write(
        &places.dpkg.join("status"),
        "Package: libbar\nStatus: install ok installed\nArchitecture: amd64\nInstalled-Size: 4\n\n\
         Package: libbar\nStatus: install ok installed\nArchitecture: i386\nInstalled-Size: 2\n\n\
         Package: orca\nStatus: install ok installed\nArchitecture: all\nInstalled-Size: 1\n\n\
         Package: editor\nStatus: install ok installed\nArchitecture: amd64\n",
    );
    write(
        &places.dpkg.join("info/libbar:i386.list"),
        "/usr/share/applications/bar.desktop\n",
    );
    write(
        &places.dpkg.join("info/orca.list"),
        "/usr/share/applications/orca.desktop\n",
    );
    write(
        &places.dpkg.join("info/editor.list"),
        "/usr/share/applications/editor.desktop\n",
    );
    let applications = root.path().join("usr/share/applications");
    write(&applications.join("bar.desktop"), &launcher("Bar", ""));
    write(&applications.join("orca.desktop"), &launcher("Orca", ""));
    write(
        &applications.join("editor.desktop"),
        &launcher("Editor", ""),
    );
    // apt files a package of every architecture under the system's.
    write(
        &places.extended_states,
        "Package: orca\nArchitecture: amd64\nAuto-Installed: 1\n",
    );
    let (found, rest) = apps(read_debs(&places, &reading(root.path())));
    let ids: Vec<&str> = found.iter().map(|app| app.id.as_str()).collect();
    assert_eq!(
        ids,
        ["libbar:i386", "orca:all", "editor:amd64"],
        "never the other architecture's"
    );
    assert_eq!(found[1].origin, Some(Origin::AsDependency));
    assert_eq!(found[2].origin, Some(Origin::ByUser));
    assert_eq!(found[2].size, AppSize::Unknown, "no size made up");
    assert_eq!(
        rest,
        Rest {
            count: 1,
            bytes: 4 * 1024
        }
    );

    // apt's state unreadable: the origin is unknown, never « by you ».
    fs::remove_file(&places.extended_states).unwrap();
    fs::create_dir(&places.extended_states).unwrap();
    let (found, _) = apps(read_debs(&places, &reading(root.path())));
    assert!(found.iter().all(|app| app.origin.is_none()), "{found:?}");
}

#[test]
fn only_a_snap_of_type_app_with_a_launcher_is_an_app_and_one_all_disabled_is_rest() {
    let root = tempfile::tempdir().unwrap();
    let applications = root.path().join("var/lib/snapd/desktop/applications");
    write(
        &applications.join("base_tool.desktop"),
        &launcher("A base with a launcher", ""),
    );
    let list = serde_json::json!([
        { "name": "base", "type": "base", "status": "active", "installed-size": 7,
          "apps": [{ "name": "tool", "desktop-file": "/var/lib/snapd/desktop/applications/base_tool.desktop" }] },
        { "name": "gone", "type": "app", "status": "installed", "installed-size": 3 },
        { "name": "gone", "type": "app", "status": "installed", "installed-size": 2 },
    ]);
    let (found, rest) = apps(read_snaps(&list, root.path(), None, &reading(root.path())));
    assert_eq!(found, Vec::new());
    assert_eq!(
        rest,
        Rest {
            count: 2,
            bytes: 7 + 3 + 2
        }
    );
}

#[test]
fn an_active_deployment_elsewhere_is_never_read_even_when_it_exists() {
    let root = tempfile::tempdir().unwrap();
    let user = root.path().join("user");
    // A real folder with a real launcher, out of the branch's folder.
    write(
        &root
            .path()
            .join("elsewhere/export/share/applications/org.example.Away.desktop"),
        &launcher("Away", ""),
    );
    let branch = user.join("app/org.example.Away/x86_64/stable");
    fs::create_dir_all(&branch).unwrap();
    let absolute = root.path().join("elsewhere");
    for target in [
        "../../../../../elsewhere",
        absolute.to_str().unwrap(),
        "/tmp",
        ".",
        "..",
    ] {
        let _ = fs::remove_file(branch.join("active"));
        std::os::unix::fs::symlink(target, branch.join("active")).unwrap();
        assert_eq!(
            read_flatpaks(std::slice::from_ref(&user), None, &reading(root.path())),
            SourceReading::Read {
                apps: Vec::new(),
                rest: Rest::default()
            },
            "{target}"
        );
    }
}

#[test]
fn a_flatpak_installed_twice_is_one_line() {
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    let user = root.path().join("user");
    let in_system = deploy(&system, "org.gimp.GIMP", "abc123");
    deploy(&user, "org.gimp.GIMP", "abc123");
    let (found, _) = apps(read_flatpaks(&[system, user], None, &reading(root.path())));
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].size,
        AppSize::ToMeasure(in_system),
        "the system's first"
    );
}

#[test]
fn a_reading_stopped_sends_its_source_and_measures_nothing() {
    use std::sync::mpsc;

    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let measured = app(Source::Snap, "x", Some(data));
    let reader: Reader = Box::new(move || SourceReading::Read {
        apps: vec![measured],
        rest: Rest::default(),
    });
    let (sink, arrivals) = mpsc::channel();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    read_all(vec![(Source::Snap, reader)], &sink, &stop);
    drop(sink);
    let arrived: Vec<Arrival> = arrivals.iter().collect();
    assert_eq!(arrived.len(), 1, "{arrived:?}");
    assert!(matches!(
        arrived[0],
        Arrival::Source(Source::Snap, SourceReading::Read { .. })
    ));
}

#[test]
fn a_snap_of_no_size_said_is_of_a_size_unknown() {
    let root = tempfile::tempdir().unwrap();
    let applications = root.path().join("var/lib/snapd/desktop/applications");
    write(&applications.join("x_x.desktop"), &launcher("X", ""));
    let list = serde_json::json!([
        { "name": "x", "type": "app", "status": "active",
          "apps": [{ "name": "x", "desktop-file": "/var/lib/snapd/desktop/applications/x_x.desktop" }] },
        { "name": "x", "type": "app", "status": "installed", "installed-size": 4 },
        { "name": "x", "type": "app", "status": "installed" },
    ]);
    let (found, _) = apps(read_snaps(&list, root.path(), None, &reading(root.path())));
    assert_eq!(found[0].size, AppSize::Unknown, "never 0");
    assert_eq!((found[0].old_bytes, found[0].old_versions), (4, 2));
}

#[test]
fn a_flatpak_first_without_a_launcher_never_hides_its_other_install() {
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    let user = root.path().join("user");
    let in_system = deploy(&system, "org.gimp.GIMP", "abc123");
    // The system's has no launcher the menu shows.
    write(
        &in_system.join("export/share/applications/org.gimp.GIMP.desktop"),
        &launcher("GIMP", "NoDisplay=true"),
    );
    let in_user = deploy(&user, "org.gimp.GIMP", "abc123");
    let (found, rest) = apps(read_flatpaks(&[system, user], None, &reading(root.path())));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].size, AppSize::ToMeasure(in_user));
    assert_eq!(rest.count, 1);
}

#[test]
fn a_flatpak_installation_unreadable_is_a_failure_never_none() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let closed = root.path().join("closed");
    let installation = closed.join("flatpak");
    fs::create_dir_all(&installation).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    let read = read_flatpaks(&[installation], None, &reading(root.path()));
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    // Root reads through anything: then it is simply read.
    if !matches!(read, SourceReading::Read { .. }) {
        assert!(matches!(read, SourceReading::Failed(_)), "{read:?}");
    }
}
