//! The applications read from a sandbox laid out as the system is.

use std::fs;
use std::path::{Path, PathBuf};

use okapi_rules::Language;

use super::flatpak::{FlatpakRef, Installation, read_flatpaks, related};
use super::icons::IconRoots;
use super::snap::read_snaps;
use super::uninstall::{Target, Why};
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
        SourceReading::Read { apps, rest, .. } => (apps, rest),
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
        &launcher("GIMP extra", "NoDisplay=true"),
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
    assert_eq!(
        gimp.launchers,
        ["gimp", "gimp-extra"],
        "every launcher, the one kept from the menu too"
    );
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
    assert_eq!(studio.launchers, ["android-studio_android-studio"]);
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

/// The remote every deployment of these tests comes from, but where said.
const ORIGIN: &str = "flathub";

/// What flatpak writes as a deployment's data (`deploy`) for one from
/// `origin`: the remote, its NUL, then the rest of the `GVariant`.
fn deploy_data(origin: &str) -> Vec<u8> {
    [origin.as_bytes(), b"\0", b"\0\x01commit"].concat()
}

/// A flatpak installation with `id` deployed from [`ORIGIN`], `active`
/// naming `target`.
fn deploy(installation: &Path, id: &str, target: &str) -> PathBuf {
    let branch = installation.join("app").join(id).join("x86_64/stable");
    let deployment = branch.join("abc123");
    fs::create_dir_all(&deployment).unwrap();
    write(
        &deployment.join(format!("export/share/applications/{id}.desktop")),
        &launcher(id, ""),
    );
    fs::write(deployment.join("deploy"), deploy_data(ORIGIN)).unwrap();
    std::os::unix::fs::symlink(target, branch.join("active")).unwrap();
    deployment
}

fn system_installation(path: &Path) -> Installation {
    Installation::at(path.to_path_buf(), true, false)
}

fn user_installation(path: &Path) -> Installation {
    Installation::at(path.to_path_buf(), false, false)
}

fn declared_installation(path: &Path) -> Installation {
    Installation::at(path.to_path_buf(), false, true)
}

fn both(system: &Path, user: &Path) -> [Installation; 2] {
    [system_installation(system), user_installation(user)]
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
        &[
            system_installation(&system),
            user_installation(&user),
            user_installation(&root.path().join("none")),
        ],
        Some(&home),
        &reading(root.path()),
    ));
    assert_eq!(apps.len(), 1, "{apps:?}");
    assert_eq!(apps[0].id, "org.gimp.GIMP");
    assert_eq!(apps[0].launchers, ["org.gimp.GIMP"]);
    assert_eq!(apps[0].size, AppSize::ToMeasure(gimp), "measured");
    assert_eq!(
        apps[0].target,
        Some(Target::Flatpak(
            FlatpakRef::app("org.gimp.GIMP", "x86_64", "stable").unwrap()
        )),
        "the system's installation's"
    );
    assert_eq!(apps[0].data, Some(home.join(".var/app/org.gimp.GIMP")));
    assert_eq!(rest, Rest::default());
    assert_eq!(
        read_flatpaks(
            &[user_installation(&root.path().join("none"))],
            None,
            &reading(root.path())
        ),
        SourceReading::Absent
    );
}

fn app(source: Source, id: &str, data: Option<PathBuf>) -> App {
    App {
        source,
        id: id.to_owned(),
        package: id.to_owned(),
        name: id.to_owned(),
        aliases: Vec::new(),
        launchers: Vec::new(),
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
        partial: false,
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
            read_flatpaks(&[user_installation(&user)], None, &reading(root.path())),
            SourceReading::Read {
                apps: Vec::new(),
                rest: Rest::default(),
                partial: false,
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
    let (found, _) = apps(read_flatpaks(
        &both(&system, &user),
        None,
        &reading(root.path()),
    ));
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
        partial: false,
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
    let (found, rest) = apps(read_flatpaks(
        &both(&system, &user),
        None,
        &reading(root.path()),
    ));
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
    let read = read_flatpaks(
        &[user_installation(&installation)],
        None,
        &reading(root.path()),
    );
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    // Root reads through anything: then it is simply read.
    if !matches!(read, SourceReading::Read { .. }) {
        assert!(matches!(read, SourceReading::Failed(_)), "{read:?}");
    }
}

#[test]
fn only_the_system_s_installation_gives_a_flatpak_its_target() {
    // Catches a button for an installation the helper cannot change, and
    // installations whose kinds were swapped.
    let config = tempfile::tempdir().unwrap();
    let data = Path::new("/home/f/.local/share");
    let none = config.path().join("none");
    let environment = |name: &str| (name == "FLATPAK_CONFIG_DIR").then(|| none.clone().into());
    assert_eq!(
        flatpak::installations_from(&environment, Some(data)),
        [
            system_installation(Path::new(flatpak::SYSTEM_INSTALLATION)),
            user_installation(&data.join(flatpak::USER_INSTALLATION)),
        ]
    );
    let root = tempfile::tempdir().unwrap();
    let user = root.path().join("user");
    deploy(&user, "org.gimp.GIMP", "abc123");
    let (found, _) = apps(read_flatpaks(
        &[user_installation(&user)],
        None,
        &reading(root.path()),
    ));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].target, None);
}

/// Whether the tests run as root, who reads through any permission.
fn as_root() -> bool {
    okapi_platform::running_as_root()
}

/// Closes `folder` to everyone while `read` runs.
fn closed<T>(folder: &Path, read: impl FnOnce() -> T) -> T {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(folder, fs::Permissions::from_mode(0o000)).unwrap();
    let read = read();
    fs::set_permissions(folder, fs::Permissions::from_mode(0o700)).unwrap();
    read
}

#[test]
fn a_part_of_an_installation_unreadable_makes_the_reading_partial_never_failed() {
    // Catches one unreadable folder failing the whole source, and a reading
    // that says itself whole with applications missing.
    if as_root() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    let user = root.path().join("user");
    deploy(&system, "org.gimp.GIMP", "abc123");
    deploy(&system, "org.inkscape.Inkscape", "abc123");
    deploy(&user, "org.example.Hidden", "abc123");
    for closed_folder in [
        user.join("app"),
        user.join("app/org.example.Hidden"),
        user.join("app/org.example.Hidden/x86_64"),
        user.clone(),
    ] {
        let read = closed(&closed_folder, || {
            read_flatpaks(&both(&system, &user), None, &reading(root.path()))
        });
        let SourceReading::Read { apps, partial, .. } = read else {
            panic!("{}: {read:?}", closed_folder.display());
        };
        assert_eq!(apps.len(), 2, "{}", closed_folder.display());
        assert!(partial, "{}", closed_folder.display());
    }
    let read = read_flatpaks(&both(&system, &user), None, &reading(root.path()));
    assert!(matches!(read, SourceReading::Read { partial: false, .. }));
    // Alone and unreadable, an installation is a failure, never none.
    let read = closed(root.path(), || {
        read_flatpaks(&[user_installation(&user)], None, &reading(root.path()))
    });
    assert!(matches!(read, SourceReading::Failed(_)), "{read:?}");
}

#[test]
fn a_link_among_the_architectures_or_branches_is_never_read() {
    // Catches `app/<id>/current` followed to another branch, or anywhere.
    let root = tempfile::tempdir().unwrap();
    let user = root.path().join("user");
    let elsewhere = root.path().join("elsewhere");
    deploy(&elsewhere, "org.example.Away", "abc123");
    let away = user.join("app/org.example.Away");
    fs::create_dir_all(&away).unwrap();
    std::os::unix::fs::symlink(
        elsewhere.join("app/org.example.Away/x86_64"),
        away.join("current"),
    )
    .unwrap();
    let branches = user.join("app/org.example.Branch/x86_64");
    fs::create_dir_all(&branches).unwrap();
    std::os::unix::fs::symlink(
        elsewhere.join("app/org.example.Away/x86_64/stable"),
        branches.join("stable"),
    )
    .unwrap();
    let read = read_flatpaks(&[user_installation(&user)], None, &reading(root.path()));
    // The link among the branches hides what may be there: said partial,
    // `current` among the architectures is flatpak's own and hides nothing.
    assert!(
        matches!(read, SourceReading::Read { partial: true, .. }),
        "{read:?}"
    );
    let (found, _) = apps(read);
    assert_eq!(found, Vec::new());
    fs::remove_file(branches.join("stable")).unwrap();
    assert!(matches!(
        read_flatpaks(&[user_installation(&user)], None, &reading(root.path())),
        SourceReading::Read { partial: false, .. }
    ));
    // A name refused among the applications hides one too.
    fs::create_dir_all(user.join("app/org a")).unwrap();
    assert!(matches!(
        read_flatpaks(&[user_installation(&user)], None, &reading(root.path())),
        SourceReading::Read { partial: true, .. }
    ));
}

/// The metadata of an extension that says it extends `reference`, as
/// flatpak-builder writes it.
fn extension_of(id: &str, reference: &str) -> String {
    format!("[Runtime]\nname={id}\n\n[ExtensionOf]\nref={reference}\n")
}

/// The runtime `id` deployed in `installation` on `branch`, from `origin`,
/// its metadata `metadata`.
fn deploy_extension(installation: &Path, id: &str, branch: &str, metadata: &str, origin: &str) {
    let folder = installation
        .join("runtime")
        .join(id)
        .join("x86_64")
        .join(branch);
    let deployment = folder.join("abc123");
    fs::create_dir_all(&deployment).unwrap();
    fs::write(deployment.join("metadata"), metadata).unwrap();
    fs::write(deployment.join("deploy"), deploy_data(origin)).unwrap();
    std::os::unix::fs::symlink("abc123", folder.join("active")).unwrap();
}

/// An extension of GIMP's stable branch, `id`, deployed in `installation`
/// on `branch`, from [`ORIGIN`].
fn deploy_runtime(installation: &Path, id: &str, branch: &str) {
    deploy_extension(
        installation,
        id,
        branch,
        &extension_of(id, "app/org.gimp.GIMP/x86_64/stable"),
        ORIGIN,
    );
}

#[test]
fn the_extensions_that_go_with_a_flatpak_are_those_marked_and_installed() {
    // Catches an extension named but not installed asked to go, a
    // subdirectory of an extension forgotten, and another application's
    // extension taken.
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    let deployment = deploy(&system, "org.gimp.GIMP", "abc123");
    write(
        &deployment.join("metadata"),
        "[Application]\nname=org.gimp.GIMP\n\n\
         [Extension org.gimp.GIMP.Locale]\nlocale-subset=true\n\n\
         [Extension org.gimp.GIMP.Debug]\n\n\
         [Extension org.gimp.GIMP.Plugin]\nautodelete=true\nversion=3\nsubdirectories=true\n\n\
         [Extension org.gimp.GIMP.Help]\nversion=3\n\n\
         [Extension org.gimp.GIMP.Filters]\nautodelete=true\nsubdirectories=true\n\n\
         [Extension org.gimp.GIMP.Theme]\nautodelete=true\nversions=stable;stable\n",
    );
    // An extension installed itself: its subdirectories are not looked for
    // (libflatpak's rule).
    deploy_runtime(&system, "org.gimp.GIMP.Filters", "stable");
    deploy_runtime(&system, "org.gimp.GIMP.Filters.Extra", "stable");
    // Named twice: asked once.
    deploy_runtime(&system, "org.gimp.GIMP.Theme", "stable");
    deploy_runtime(&system, "org.gimp.GIMP.Locale", "stable");
    deploy_runtime(&system, "org.gimp.GIMP.Plugin.Resynthesizer", "3");
    deploy_runtime(&system, "org.gimp.GIMP.Plugin.Old", "2");
    // Its name only begins as the extension's: another extension.
    deploy_runtime(&system, "org.gimp.GIMP.PluginX", "3");
    deploy_runtime(&system, "org.gimp.GIMP.Help", "3");
    deploy_extension(
        &system,
        "org.inkscape.Inkscape.Locale",
        "stable",
        &extension_of(
            "org.inkscape.Inkscape.Locale",
            "app/org.inkscape.Inkscape/x86_64/stable",
        ),
        ORIGIN,
    );
    let app = FlatpakRef::app("org.gimp.GIMP", "x86_64", "stable").unwrap();
    let names: Vec<String> = related(&system, &app)
        .unwrap()
        .extensions
        .iter()
        .map(|(reference, _)| reference.to_string())
        .collect();
    assert_eq!(
        names,
        [
            "runtime/org.gimp.GIMP.Locale/x86_64/stable",
            "runtime/org.gimp.GIMP.Plugin.Resynthesizer/x86_64/3",
            "runtime/org.gimp.GIMP.Filters/x86_64/stable",
            "runtime/org.gimp.GIMP.Theme/x86_64/stable",
        ]
    );
    // No metadata: it goes alone.
    fs::remove_file(deployment.join("metadata")).unwrap();
    assert_eq!(
        related(&system, &app).map(|related| related.extensions.len()),
        Ok(0)
    );
    // No longer deployed: not found, nothing to remove.
    fs::remove_file(system.join("app/org.gimp.GIMP/x86_64/stable/active")).unwrap();
    assert_eq!(related(&system, &app), Err(Why::NotFound));
}

/// The references of what goes with GIMP's stable branch in `system`.
fn going_with_gimp(system: &Path) -> Vec<String> {
    let app = FlatpakRef::app("org.gimp.GIMP", "x86_64", "stable").unwrap();
    related(system, &app)
        .unwrap()
        .extensions
        .iter()
        .map(|(reference, _)| reference.to_string())
        .collect()
}

#[test]
fn an_application_never_makes_another_s_extensions_or_a_shared_runtime_go() {
    // Catches the application's metadata trusted to say what is its own:
    // flatpak's helper would remove, as root, another application's
    // translations, the graphics drivers, the codecs.
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    let deployment = deploy(&system, "org.gimp.GIMP", "abc123");
    write(
        &deployment.join("metadata"),
        "[Application]\nname=org.gimp.GIMP\n\n\
         [Extension org.gimp.GIMP.Locale]\n\n\
         [Extension org.mozilla.firefox.Locale]\n\n\
         [Extension org.freedesktop.Platform.GL]\nsubdirectories=true\nautodelete=true\nversions=24.08\n\n\
         [Extension org.freedesktop.Platform.ffmpeg-full]\nautodelete=true\nversion=24.08\n",
    );
    deploy_runtime(&system, "org.gimp.GIMP.Locale", "stable");
    deploy_extension(
        &system,
        "org.mozilla.firefox.Locale",
        "stable",
        &extension_of(
            "org.mozilla.firefox.Locale",
            "app/org.mozilla.firefox/x86_64/stable",
        ),
        ORIGIN,
    );
    // As on Flathub: the drivers extend the platform, the codecs say
    // nothing.
    deploy_extension(
        &system,
        "org.freedesktop.Platform.GL.default",
        "24.08",
        &extension_of(
            "org.freedesktop.Platform.GL.default",
            "runtime/org.freedesktop.Platform/x86_64/24.08",
        ),
        ORIGIN,
    );
    deploy_extension(
        &system,
        "org.freedesktop.Platform.ffmpeg-full",
        "24.08",
        "[Runtime]\nname=org.freedesktop.Platform.ffmpeg-full\n",
        ORIGIN,
    );
    assert_eq!(
        going_with_gimp(&system),
        ["runtime/org.gimp.GIMP.Locale/x86_64/stable"]
    );
}

#[test]
fn an_extension_goes_only_when_it_says_itself_the_application_s_and_came_from_its_remote() {
    // Catches each check of an extension's own metadata and origin skipped:
    // each plug-in below but `Beta` fails one alone, and would go, as root.
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    let deployment = deploy(&system, "org.gimp.GIMP", "abc123");
    write(
        &deployment.join("metadata"),
        "[Extension org.gimp.GIMP.Plugin]\nautodelete=true\nsubdirectories=true\n",
    );
    let deployed = |name: &str, metadata: &str, origin: &str| {
        let id = format!("org.gimp.GIMP.Plugin.{name}");
        deploy_extension(&system, &id, "stable", metadata, origin);
        system.join(format!("runtime/{id}/x86_64/stable/abc123"))
    };
    let plugin = |name: &str, reference: &str, origin: &str| {
        let id = format!("org.gimp.GIMP.Plugin.{name}");
        deployed(name, &extension_of(&id, reference), origin)
    };
    plugin("Evil", "app/org.gimp.GIMP/x86_64/stable", "evil-remote");
    plugin("Runtime", "runtime/org.gimp.GIMP/x86_64/stable", ORIGIN);
    plugin("Prefix", "app/org.gimp.GIMP.Evil/x86_64/stable", ORIGIN);
    plugin("Three", "app/org.gimp.GIMP/x86_64", ORIGIN);
    plugin("Five", "app/org.gimp.GIMP/x86_64/stable/x", ORIGIN);
    plugin("Empty", "app/org.gimp.GIMP/x86_64/", ORIGIN);
    plugin("Arm", "app/org.gimp.GIMP/aarch64/stable", ORIGIN);
    // Built against another branch, which is not installed: it goes.
    plugin("Beta", "app/org.gimp.GIMP/x86_64/beta", ORIGIN);
    // Built against another branch still installed: that one's.
    plugin("Other", "app/org.gimp.GIMP/x86_64/2-40", ORIGIN);
    let other_branch = system.join("app/org.gimp.GIMP/x86_64/2-40");
    fs::create_dir_all(other_branch.join("def456")).unwrap();
    std::os::unix::fs::symlink("def456", other_branch.join("active")).unwrap();
    // `ref` outside its group says nothing; the last group says.
    deployed(
        "Misplaced",
        "[Runtime]\nref=app/org.gimp.GIMP/x86_64/stable\n",
        ORIGIN,
    );
    deployed(
        "Twice",
        "[ExtensionOf]\nref=app/org.gimp.GIMP/x86_64/stable\nref=app/org.mozilla.firefox/x86_64/stable\n",
        ORIGIN,
    );
    deployed(
        "Last",
        "[ExtensionOf]\nref=app/org.gimp.GIMP/x86_64/stable\n\n\
         [ExtensionOf]\nref=app/org.mozilla.firefox/x86_64/stable\n",
        ORIGIN,
    );
    // Its metadata or its origin missing, or naming no remote.
    let no_metadata = plugin("NoMetadata", "app/org.gimp.GIMP/x86_64/stable", ORIGIN);
    fs::remove_file(no_metadata.join("metadata")).unwrap();
    let no_origin = plugin("NoOrigin", "app/org.gimp.GIMP/x86_64/stable", ORIGIN);
    fs::remove_file(no_origin.join("deploy")).unwrap();
    let no_nul = plugin("NoNul", "app/org.gimp.GIMP/x86_64/stable", ORIGIN);
    fs::write(no_nul.join("deploy"), ORIGIN).unwrap();
    assert_eq!(
        going_with_gimp(&system),
        ["runtime/org.gimp.GIMP.Plugin.Beta/x86_64/stable"]
    );
}

#[test]
fn a_part_unreadable_even_to_root_makes_the_reading_partial() {
    // Catches an installation, an application folder or an `active` that
    // cannot be read taken for nothing there (no permission involved: these
    // run as root too).
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    deploy(&system, "org.gimp.GIMP", "abc123");
    deploy(&system, "org.inkscape.Inkscape", "abc123");
    let partial_with = |installations: &[Installation]| {
        let SourceReading::Read { apps, partial, .. } =
            read_flatpaks(installations, None, &reading(root.path()))
        else {
            panic!("read");
        };
        (apps.len(), partial)
    };
    // The user's installation under a file: its state unreadable.
    let blocked = root.path().join("blocked");
    fs::write(&blocked, "").unwrap();
    assert_eq!(
        partial_with(&[
            system_installation(&system),
            user_installation(&blocked.join("flatpak")),
        ]),
        (2, true),
        "an installation unreadable beside one read"
    );
    // Its applications' folder a file.
    let user = root.path().join("user");
    fs::create_dir_all(&user).unwrap();
    fs::write(user.join("app"), "").unwrap();
    assert_eq!(
        partial_with(&both(&system, &user)),
        (2, true),
        "applications not listed"
    );
    // An `active` that is no link.
    let other = root.path().join("other");
    let branch = other.join("app/org.example.Odd/x86_64/stable");
    fs::create_dir_all(&branch).unwrap();
    fs::write(branch.join("active"), "").unwrap();
    assert_eq!(
        partial_with(&both(&system, &other)),
        (2, true),
        "a deployment unreadable"
    );
    assert_eq!(partial_with(&[system_installation(&system)]), (2, false));
}

#[test]
fn what_goes_with_a_flatpak_unclear_is_never_told_nothing() {
    // Catches a deployment, a metadata, an extension or the runtimes that
    // cannot be read told « no extension »: they would stay unseen.
    let app = FlatpakRef::app("org.gimp.GIMP", "x86_64", "stable").unwrap();
    let unclear = |prepare: &dyn Fn(&Path, &Path)| {
        let root = tempfile::tempdir().unwrap();
        let system = root.path().join("system");
        let deployment = deploy(&system, "org.gimp.GIMP", "abc123");
        prepare(&system, &deployment);
        related(&system, &app)
    };
    // The application's `active` no link.
    assert_eq!(
        unclear(&|system, _| {
            let active = system.join("app/org.gimp.GIMP/x86_64/stable/active");
            fs::remove_file(&active).unwrap();
            fs::write(&active, "").unwrap();
        }),
        Err(Why::Unclear)
    );
    // Its metadata a folder, or too long.
    assert_eq!(
        unclear(&|_, deployment| fs::create_dir(deployment.join("metadata")).unwrap()),
        Err(Why::Unclear)
    );
    assert_eq!(
        unclear(&|_, deployment| {
            fs::write(
                deployment.join("metadata"),
                vec![b'#'; flatpak::METADATA_LIMIT + 1],
            )
            .unwrap();
        }),
        Err(Why::Unclear)
    );
    // An extension whose `active` is no link.
    assert_eq!(
        unclear(&|system, deployment| {
            write(
                &deployment.join("metadata"),
                "[Extension org.gimp.GIMP.Locale]\n",
            );
            let branch = system.join("runtime/org.gimp.GIMP.Locale/x86_64/stable");
            fs::create_dir_all(&branch).unwrap();
            fs::write(branch.join("active"), "").unwrap();
        }),
        Err(Why::Unclear)
    );
    // The runtimes not listable (still crossed), when subdirectories count
    // and the extension itself is not installed.
    if !as_root() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let system = root.path().join("system");
        let deployment = deploy(&system, "org.gimp.GIMP", "abc123");
        write(
            &deployment.join("metadata"),
            "[Extension org.gimp.GIMP.Plugin]\nautodelete=true\nsubdirectories=true\n",
        );
        deploy_runtime(&system, "org.gimp.GIMP.Plugin.Resynthesizer", "stable");
        let runtime = system.join("runtime");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o300)).unwrap();
        let related = related(&system, &app);
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(related, Err(Why::Unclear));
    }
    // No runtime at all: nothing more goes.
    assert_eq!(
        unclear(&|_, deployment| {
            write(
                &deployment.join("metadata"),
                "[Extension org.gimp.GIMP.Plugin]\nautodelete=true\nsubdirectories=true\n",
            );
        })
        .map(|related| related.extensions.len()),
        Ok(0)
    );
}

#[test]
fn whose_an_extension_is_unclear_is_never_told_nothing() {
    // Catches an origin or an extension's metadata that cannot be read told
    // « not the application's »: it would stay unseen, or go unchecked.
    let app = FlatpakRef::app("org.gimp.GIMP", "x86_64", "stable").unwrap();
    let unclear = |prepare: &dyn Fn(&Path, &Path)| {
        let root = tempfile::tempdir().unwrap();
        let system = root.path().join("system");
        let deployment = deploy(&system, "org.gimp.GIMP", "abc123");
        prepare(&system, &deployment);
        related(&system, &app)
    };
    // The application's origin, once an extension is deployed: missing,
    // without its NUL, empty, unreadable.
    let with_locale = |system: &Path, deployment: &Path| {
        write(
            &deployment.join("metadata"),
            "[Extension org.gimp.GIMP.Locale]\n",
        );
        deploy_runtime(system, "org.gimp.GIMP.Locale", "stable");
    };
    for data in [None, Some(&b"flathub"[..]), Some(&b"\0flathub"[..])] {
        assert_eq!(
            unclear(&|system, deployment| {
                with_locale(system, deployment);
                fs::remove_file(deployment.join("deploy")).unwrap();
                if let Some(data) = data {
                    fs::write(deployment.join("deploy"), data).unwrap();
                }
            }),
            Err(Why::Unclear),
            "{data:?}"
        );
    }
    assert_eq!(
        unclear(&|system, deployment| {
            with_locale(system, deployment);
            fs::remove_file(deployment.join("deploy")).unwrap();
            fs::create_dir(deployment.join("deploy")).unwrap();
        }),
        Err(Why::Unclear)
    );
    // The extension's metadata or data unreadable.
    for file in ["metadata", "deploy"] {
        assert_eq!(
            unclear(&|system, deployment| {
                with_locale(system, deployment);
                let extension = system.join("runtime/org.gimp.GIMP.Locale/x86_64/stable/abc123");
                fs::remove_file(extension.join(file)).unwrap();
                fs::create_dir(extension.join(file)).unwrap();
            }),
            Err(Why::Unclear),
            "{file}"
        );
    }
    // No extension deployed: the application's origin is never needed.
    assert_eq!(
        unclear(&|_, deployment| {
            write(
                &deployment.join("metadata"),
                "[Extension org.gimp.GIMP.Locale]\n",
            );
            fs::remove_file(deployment.join("deploy")).unwrap();
        })
        .map(|related| related.extensions.len()),
        Ok(0)
    );
}

#[test]
fn an_installation_that_is_a_file_is_never_said_absent() {
    // Catches an installation path holding a file taken for no installation.
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    deploy(&system, "org.gimp.GIMP", "abc123");
    deploy(&system, "org.inkscape.Inkscape", "abc123");
    let file = root.path().join("file");
    fs::write(&file, "").unwrap();
    let read = read_flatpaks(
        &[system_installation(&system), user_installation(&file)],
        None,
        &reading(root.path()),
    );
    assert!(
        matches!(&read, SourceReading::Read { apps, partial: true, .. } if apps.len() == 2),
        "{read:?}"
    );
    let read = read_flatpaks(&[user_installation(&file)], None, &reading(root.path()));
    assert!(matches!(read, SourceReading::Failed(_)), "{read:?}");
}

#[test]
fn a_folder_unread_is_never_forgotten_by_one_read_after_it() {
    // Catches `partial` set by one folder and reset by the next one read.
    if as_root() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let user = root.path().join("user");
    deploy(&user, "org.aaa.First", "abc123");
    deploy(&user, "org.zzz.Last", "abc123");
    let read = closed(&user.join("app/org.aaa.First/x86_64"), || {
        read_flatpaks(&[user_installation(&user)], None, &reading(root.path()))
    });
    assert!(
        matches!(&read, SourceReading::Read { apps, partial: true, .. } if apps.len() == 1),
        "{read:?}"
    );
}

#[test]
fn a_subextension_unreadable_is_never_told_nothing() {
    // Catches a subextension whose deployment cannot be read passed over:
    // it would stay unseen.
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    let deployment = deploy(&system, "org.gimp.GIMP", "abc123");
    write(
        &deployment.join("metadata"),
        "[Extension org.gimp.GIMP.Plugin]\nautodelete=true\nsubdirectories=true\n",
    );
    let branch = system.join("runtime/org.gimp.GIMP.Plugin.Odd/x86_64/stable");
    fs::create_dir_all(&branch).unwrap();
    fs::write(branch.join("active"), "").unwrap();
    let app = FlatpakRef::app("org.gimp.GIMP", "x86_64", "stable").unwrap();
    assert_eq!(related(&system, &app), Err(Why::Unclear));
}

#[test]
fn installations_are_placed_as_flatpak_places_them() {
    // Catches the user's installation fixed under the home folder when the
    // data folder moved, the variables flatpak reads ignored, and the
    // installations declared in `installations.d` forgotten.
    let config = tempfile::tempdir().unwrap();
    let declarations = config.path().join("installations.d");
    fs::create_dir(&declarations).unwrap();
    write(
        &declarations.join("sd.conf"),
        "[Installation \"sd\"]\nPath=/media/sd/flatpak\nDisplayName=SD\n",
    );
    write(
        &declarations.join("broken.conf"),
        "[Installation \"x\"]\nDisplayName=No path\n",
    );
    write(&declarations.join("ignored.txt"), "Path=/elsewhere\n");
    let folder = config.path().to_path_buf();
    let environment = move |name: &str| match name {
        "FLATPAK_CONFIG_DIR" => Some(folder.clone().into()),
        "FLATPAK_USER_DIR" => Some("/custom/user".into()),
        "FLATPAK_SYSTEM_DIR" => Some("/custom/system".into()),
        _ => None,
    };
    let broken = declarations.join("broken.conf");
    assert_eq!(
        flatpak::installations_from(&environment, Some(Path::new("/data"))),
        [
            // Moved, the system's is no longer the helper's.
            user_installation(Path::new("/custom/system")),
            user_installation(Path::new("/custom/user")),
            Installation::unplaced(broken, "a path missing or relative".to_owned()),
            declared_installation(Path::new("/media/sd/flatpak")),
        ]
    );
}

#[test]
fn an_installation_that_cannot_be_placed_is_unread_never_none() {
    // Catches the user's installation dropped without a data folder, and a
    // declaration folder that cannot be listed taken for an empty
    // installation: either way the ids there would be said uninstalled.
    let config = tempfile::tempdir().unwrap();
    let declarations = config.path().join("installations.d");
    fs::create_dir(&declarations).unwrap();
    let folder = config.path().to_path_buf();
    let environment =
        move |name: &str| (name == "FLATPAK_CONFIG_DIR").then(|| folder.clone().into());
    let found = closed(&declarations, || {
        flatpak::installations_from(&environment, None)
    });
    let unplaced: Vec<&Installation> = found
        .iter()
        .filter(|installation| installation.unplaced.is_some())
        .collect();
    // Root lists through anything: then only the user's is unplaced.
    let expected = if as_root() { 1 } else { 2 };
    assert_eq!(unplaced.len(), expected, "{found:?}");
    for installation in unplaced {
        assert!(flatpak::app_ids(installation).is_err());
    }
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    deploy(&system, "org.gimp.GIMP", "abc123");
    let read = read_flatpaks(
        &[
            system_installation(&system),
            Installation::unplaced(PathBuf::new(), "no data folder".to_owned()),
        ],
        None,
        &reading(root.path()),
    );
    assert!(
        matches!(&read, SourceReading::Read { partial: true, .. }),
        "{read:?}"
    );
}

#[test]
fn a_declared_installation_missing_or_unreadable_is_unread_never_none() {
    // Catches a disk unplugged, or a declaration without its path, taken
    // for no flatpak installed there.
    let root = tempfile::tempdir().unwrap();
    let system = root.path().join("system");
    deploy(&system, "org.gimp.GIMP", "abc123");
    for declared in [root.path().join("unplugged"), {
        let file = root.path().join("broken.conf");
        fs::write(&file, "").unwrap();
        file
    }] {
        let read = read_flatpaks(
            &[
                system_installation(&system),
                declared_installation(&declared),
            ],
            None,
            &reading(root.path()),
        );
        assert!(
            matches!(&read, SourceReading::Read { partial: true, .. }),
            "{}: {read:?}",
            declared.display()
        );
    }
}

#[test]
fn a_place_written_relative_is_unread_and_a_declaration_named_out_of_utf_8_is_read() {
    // Catches a relative place read from Okapi's own folder (said absent),
    // the system's default named by the variable losing its helper, and a
    // declaration dropped for its name.
    use std::os::unix::ffi::OsStrExt as _;
    let config = tempfile::tempdir().unwrap();
    let declarations = config.path().join("installations.d");
    fs::create_dir(&declarations).unwrap();
    write(
        &declarations.join(std::ffi::OsStr::from_bytes(b"carte-\xff.conf")),
        "[Installation \"sd\"]\nPath=/media/sd/flatpak\n",
    );
    let folder = config.path().to_path_buf();
    let environment = move |name: &str| match name {
        "FLATPAK_CONFIG_DIR" => Some(folder.clone().into()),
        "FLATPAK_USER_DIR" => Some("relative/user".into()),
        "FLATPAK_SYSTEM_DIR" => Some(flatpak::SYSTEM_INSTALLATION.into()),
        _ => None,
    };
    assert_eq!(
        flatpak::installations_from(&environment, None),
        [
            system_installation(Path::new(flatpak::SYSTEM_INSTALLATION)),
            Installation::unplaced("relative/user".into(), "placed relative".to_owned()),
            declared_installation(Path::new("/media/sd/flatpak")),
        ]
    );
    let relative = |name: &str| (name == "FLATPAK_CONFIG_DIR").then(|| "etc".into());
    let found = flatpak::installations_from(&relative, Some(Path::new("/data")));
    assert_eq!(
        found.last(),
        Some(&Installation::unplaced(
            "etc".into(),
            "placed relative".to_owned()
        ))
    );
}

#[test]
fn a_launcher_gives_its_file_s_name_its_window_class_and_its_plain_name() {
    // Catches the names a folder of the application may bear lost: what
    // the leftovers of an application uninstalled are told by.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("org.gnome.TextEditor.desktop");
    write(
        &path,
        &launcher(
            "Text Editor",
            "Name[fr]=Éditeur de texte\nStartupWMClass=gnome-text-editor",
        ),
    );
    let read = reading(root.path())
        .launcher(&path, &reading(root.path()).icons)
        .unwrap();
    assert_eq!(read.name, "Éditeur de texte");
    assert_eq!(
        read.aliases,
        ["org.gnome.TextEditor", "gnome-text-editor", "Text Editor"]
    );
}
