use std::path::Path;

use super::*;

const PLATFORM: &str = "org.gnome.Platform";
const SDK: &str = "org.gnome.Sdk";
const X86: &str = "x86_64";

/// An installation in a test's folder, with its repository.
fn installation(root: &Path, name: &str, system: bool) -> Installation {
    let path = root.join(name);
    std::fs::create_dir_all(path.join(REPOSITORY)).unwrap();
    Installation::at(path, system, false)
}

/// Deploys `kind/id/arch/branch` in `installation` (its `active` naming
/// `abc`), its metadata `metadata`: the deployment.
fn deploy(
    installation: &Installation,
    kind: &str,
    id: &str,
    arch: &str,
    branch: &str,
    metadata: &str,
) -> PathBuf {
    let branch_folder = installation
        .path
        .join(kind)
        .join(id)
        .join(arch)
        .join(branch);
    let deployment = branch_folder.join("abc");
    std::fs::create_dir_all(&deployment).unwrap();
    std::os::unix::fs::symlink("abc", branch_folder.join("active")).unwrap();
    std::fs::write(deployment.join(METADATA), metadata).unwrap();
    deployment
}

fn runtime(installation: &Installation, id: &str, branch: &str, metadata: &str) -> PathBuf {
    deploy(installation, "runtime", id, X86, branch, metadata)
}

/// An application deployed as flatpak deploys one: with its `current`
/// link beside its architectures, the one link flatpak makes there.
fn app(installation: &Installation, id: &str, metadata: &str) {
    deploy(installation, "app", id, X86, "stable", metadata);
    let current = installation.path.join("app").join(id).join("current");
    if std::fs::symlink_metadata(&current).is_err() {
        std::os::unix::fs::symlink(format!("{X86}/stable"), current).unwrap();
    }
}

/// What `installations` tell unused, by reference.
fn unused_of(installations: &[Installation]) -> Vec<String> {
    unused_runtimes(installations)
        .unwrap()
        .runtimes
        .iter()
        .map(|runtime| runtime.reference.to_string())
        .collect()
}

fn platform_metadata(branch: &str) -> String {
    format!(
        "[Runtime]\nname={PLATFORM}\nsdk={SDK}/{X86}/{branch}\n\n\
         [Extension {PLATFORM}.Locale]\ndirectory=share/runtime/locale\n\n\
         [Extension org.freedesktop.Platform.GL@gl]\nversions={branch};1.4\nsubdirectories=true\n"
    )
}

#[test]
fn a_runtime_no_application_reaches_is_unused_with_its_own_extensions() {
    // Catches the runtime of an application, its SDK, the SDK's own SDK,
    // an extension of a used runtime (by its own branch, by `versions`, a
    // tag left in its name, its subdirectories, the base beside them) told
    // unused; and a runtime nothing reaches, or its translations, kept.
    let root = tempfile::tempdir().unwrap();
    let system = installation(root.path(), "system", true);
    app(
        &system,
        "org.gimp.GIMP",
        &format!(
            "[Application]\nname=org.gimp.GIMP\nruntime={PLATFORM}/{X86}/45\nsdk={SDK}/{X86}/45\n"
        ),
    );
    runtime(&system, PLATFORM, "45", &platform_metadata("45"));
    runtime(&system, SDK, "45", &format!("[Runtime]\nname={SDK}\n"));
    runtime(&system, &format!("{PLATFORM}.Locale"), "45", "[Runtime]\n");
    runtime(&system, "org.freedesktop.Platform.GL", "1.4", "[Runtime]\n");
    runtime(
        &system,
        "org.freedesktop.Platform.GL.default",
        "1.4",
        "[Runtime]\n",
    );
    runtime(
        &system,
        "org.freedesktop.Platform.GL.nvidia-570",
        "1.4",
        "[Runtime]\n",
    );
    // An older one, once GIMP's: nothing reaches it, nor its translations.
    runtime(&system, PLATFORM, "44", &platform_metadata("44"));
    runtime(&system, &format!("{PLATFORM}.Locale"), "44", "[Runtime]\n");
    // A driver of a branch no runtime used names.
    runtime(
        &system,
        "org.freedesktop.Platform.GL.default",
        "1.2",
        "[Runtime]\n",
    );
    assert_eq!(
        unused_of(&[system]),
        [
            "runtime/org.freedesktop.Platform.GL.default/x86_64/1.2",
            "runtime/org.gnome.Platform/x86_64/44",
            "runtime/org.gnome.Platform.Locale/x86_64/44",
        ]
    );
}

#[test]
fn what_another_installation_uses_or_holds_stays_and_the_user_s_is_only_counted() {
    // Catches flatpak's own fault (#2842): the runtime of an application of
    // the user's installation told unused in the system's; a runtime of the
    // user's whose SDK is the system's; the same reference in both told
    // unused in the system's; and the user's unused never counted.
    let root = tempfile::tempdir().unwrap();
    let system = installation(root.path(), "system", true);
    let user = installation(root.path(), "user", false);
    app(
        &user,
        "org.mozilla.firefox",
        &format!("[Application]\nruntime={PLATFORM}/{X86}/46\n"),
    );
    runtime(&system, PLATFORM, "46", "[Runtime]\n");
    runtime(
        &user,
        "org.kde.Platform",
        "6.8",
        &format!("[Runtime]\nsdk={SDK}/{X86}/46\n"),
    );
    runtime(&system, SDK, "46", "[Runtime]\n");
    runtime(&system, "org.kde.Platform", "6.8", "[Runtime]\n");
    // Two of the user's nothing uses: counted, never proposed.
    runtime(&user, "org.gnome.Platform", "40", "[Runtime]\n");
    runtime(&user, "org.kde.Sdk", "6.8", "[Runtime]\n");
    // And one of the system's nothing uses.
    runtime(&system, "org.freedesktop.Platform", "22.08", "[Runtime]\n");
    let unused = unused_runtimes(&[system, user]).unwrap();
    let references: Vec<String> = unused
        .runtimes
        .iter()
        .map(|runtime| runtime.reference.to_string())
        .collect();
    assert_eq!(
        references,
        ["runtime/org.freedesktop.Platform/x86_64/22.08"]
    );
    assert_eq!(
        unused.for_account_only, 3,
        "the user's three runtimes, none an application of the user's uses"
    );
}

#[test]
fn a_pinned_runtime_stays_with_what_it_uses_and_an_extension_keeps_what_it_extends() {
    // Catches a pin ignored (or its glob read wrong), the SDK of a pinned
    // runtime told unused, the runtime an extension of extra data needs,
    // and a runtime whose `[ExtensionOf]` names one used (another branch of
    // its translations, #2104).
    let root = tempfile::tempdir().unwrap();
    let system = installation(root.path(), "system", true);
    std::fs::write(
        system.path.join(REPOSITORY).join(REPOSITORY_CONFIG),
        "[core]\nrepo_version=1\nxa.pinned=runtime/org.gnome.Sdk/*/4*;org.kde.*//;\n",
    )
    .unwrap();
    runtime(
        &system,
        SDK,
        "47",
        "[Runtime]\nsdk=org.gnome.Sdk.Docs/x86_64/47\n",
    );
    runtime(&system, "org.gnome.Sdk.Docs", "47", "[Runtime]\n");
    runtime(&system, "org.kde.Sdk", "6.9", "[Runtime]\n");
    // Not matched: another branch, another id.
    runtime(&system, SDK, "50", "[Runtime]\n");
    runtime(&system, "org.freedesktop.Sdk", "24.08", "[Runtime]\n");
    app(
        &system,
        "com.spotify.Client",
        "[Application]\nruntime=org.freedesktop.Platform/x86_64/24.08\n\n\
         [Extension org.freedesktop.Platform.openh264]\nversion=2.5\n",
    );
    runtime(&system, "org.freedesktop.Platform", "24.08", "[Runtime]\n");
    runtime(
        &system,
        "org.freedesktop.Platform.openh264",
        "2.5",
        "[Runtime]\n\n[Extra Data]\nname=x\n\n[ExtensionOf]\nruntime=org.freedesktop.Platform/x86_64/23.08\n",
    );
    runtime(&system, "org.freedesktop.Platform", "23.08", "[Runtime]\n");
    runtime(
        &system,
        "org.freedesktop.Platform.Locale",
        "1",
        "[ExtensionOf]\nref=runtime/org.freedesktop.Platform/x86_64/24.08\n",
    );
    assert_eq!(
        unused_of(&[system]),
        [
            "runtime/org.freedesktop.Sdk/x86_64/24.08",
            "runtime/org.gnome.Sdk/x86_64/50",
        ]
    );
}

#[test]
fn a_runtime_of_another_architecture_is_its_own() {
    // Catches an architecture left out of a reference: the i386 runtime an
    // application uses keeping the x86_64 one, or the other way.
    let root = tempfile::tempdir().unwrap();
    let system = installation(root.path(), "system", true);
    deploy(
        &system,
        "app",
        "com.valvesoftware.Steam",
        "i386",
        "stable",
        &format!("[Application]\nruntime={PLATFORM}/i386/45\n"),
    );
    deploy(&system, "runtime", PLATFORM, "i386", "45", "[Runtime]\n");
    runtime(&system, PLATFORM, "45", "[Runtime]\n");
    assert_eq!(
        unused_of(&[system]),
        ["runtime/org.gnome.Platform/x86_64/45"]
    );
}

/// What spoils an installation, and how.
type Spoiling<'a> = (&'a str, &'a dyn Fn(&Installation));

#[test]
fn anything_read_in_part_tells_no_runtime_unused() {
    // Catches each reading that would lose a root, so a runtime in use:
    // each case alone, on an installation where one runtime is unused.
    let cases: [Spoiling<'_>; 12] = [
        ("an application without metadata", &|system| {
            let deployment = deploy(system, "app", "org.a.A", X86, "stable", "");
            std::fs::remove_file(deployment.join(METADATA)).unwrap();
        }),
        ("a runtime without metadata", &|system| {
            let deployment = runtime(system, "org.b.B", "1", "");
            std::fs::remove_file(deployment.join(METADATA)).unwrap();
        }),
        ("an SDK named unreadably", &|system| {
            app(system, "org.a.A", "[Application]\nsdk=org.gnome.Sdk\n");
        }),
        ("what extra data extends named unreadably", &|system| {
            runtime(
                system,
                "org.b.B",
                "1",
                "[Extra Data]\nname=x\n\n[ExtensionOf]\nruntime=nothing\n",
            );
        }),
        ("a repository's settings not in UTF-8", &|system| {
            std::fs::write(
                system.path.join(REPOSITORY).join(REPOSITORY_CONFIG),
                b"[core]\nxa.pinned=\xff\n",
            )
            .unwrap();
        }),
        ("a runtime named unreadably", &|system| {
            app(
                system,
                "org.a.A",
                "[Application]\nruntime=org.gnome.Platform/45\n",
            );
        }),
        ("a metadata not in UTF-8", &|system| {
            let deployment = deploy(system, "app", "org.a.A", X86, "stable", "");
            std::fs::write(deployment.join(METADATA), b"[Application]\nruntime=\xff\n").unwrap();
        }),
        ("an active leading elsewhere", &|system| {
            let branch = system.path.join("app/org.a.A").join(X86).join("stable");
            std::fs::create_dir_all(&branch).unwrap();
            std::os::unix::fs::symlink("/etc", branch.join("active")).unwrap();
        }),
        ("a folder's name refused", &|system| {
            std::fs::create_dir_all(system.path.join("app/org a")).unwrap();
        }),
        ("a pin flatpak would not read", &|system| {
            std::fs::write(
                system.path.join(REPOSITORY).join(REPOSITORY_CONFIG),
                "[core]\nxa.pinned=app/org.a.A\n",
            )
            .unwrap();
        }),
        ("a link where folders are", &|system| {
            std::fs::create_dir_all(system.path.join("app")).unwrap();
            std::os::unix::fs::symlink("/elsewhere", system.path.join("app/org.a.A")).unwrap();
        }),
        ("a branch's name refused", &|system| {
            std::fs::create_dir_all(system.path.join("app/org.a.A/i386/a b")).unwrap();
        }),
    ];
    for (what, spoil) in cases {
        let root = tempfile::tempdir().unwrap();
        let system = installation(root.path(), "system", true);
        runtime(&system, PLATFORM, "44", "[Runtime]\n");
        assert_eq!(
            unused_of(std::slice::from_ref(&system)).len(),
            1,
            "{what}: before"
        );
        spoil(&system);
        assert!(unused_runtimes(&[system]).is_err(), "{what}");
    }
}

#[test]
fn each_way_a_runtime_is_kept_keeps_it_alone() {
    // Catches, each alone: an application's own `sdk=` forgotten; extra
    // data with `NoRuntime` keeping a runtime it does not need; a runtime
    // whose `[ExtensionOf]` cannot be read told unused; a declared
    // installation counted as the user's; a pin read from another
    // installation than its own.
    let root = tempfile::tempdir().unwrap();
    let system = installation(root.path(), "system", true);
    let user = installation(root.path(), "user", false);
    let declared = Installation::at(root.path().join("declared"), false, true);
    std::fs::create_dir_all(declared.path.join(REPOSITORY)).unwrap();
    app(
        &system,
        "org.a.A",
        &format!("[Application]\nsdk={SDK}/{X86}/45\n\n[Extension org.b.Codecs]\nversion=1\n"),
    );
    runtime(&system, SDK, "45", "[Runtime]\n");
    runtime(
        &system,
        "org.b.Codecs",
        "1",
        "[Runtime]\n\n[Extra Data]\nname=x\nNoRuntime=true\n\n[ExtensionOf]\nruntime=org.b.Base/x86_64/1\n",
    );
    runtime(&system, "org.b.Base", "1", "[Runtime]\n");
    runtime(
        &system,
        "org.c.Odd",
        "1",
        "[ExtensionOf]\nref=nothing readable\n",
    );
    // Said twice, the second of something unused: kept by the first, with
    // what it uses itself.
    runtime(
        &system,
        "org.f.Docs",
        "1",
        &format!(
            "[Runtime]\nsdk=org.f.Tool/x86_64/1\n\n[ExtensionOf]\nref=runtime/{SDK}/{X86}/45\n\n[ExtensionOf]\nref=runtime/org.b.Base/x86_64/1\n"
        ),
    );
    runtime(&system, "org.f.Tool", "1", "[Runtime]\n");
    // Two levels: what extends what extends one used, and what that uses.
    runtime(
        &system,
        "org.g.Outer",
        "1",
        "[Runtime]\nsdk=org.g.Deep/x86_64/1\n\n[ExtensionOf]\nref=runtime/org.f.Docs/x86_64/1\n",
    );
    runtime(&system, "org.g.Deep", "1", "[Runtime]\n");
    // One unreadable among readable ones not reached: kept.
    runtime(
        &system,
        "org.h.Mixed",
        "1",
        "[ExtensionOf]\nref=nothing readable\n\n[ExtensionOf]\nref=runtime/org.b.Base/x86_64/1\n",
    );
    // Pinned in the user's installation: kept there, never counted, and
    // the system's copy of another branch not kept by it.
    std::fs::write(
        user.path.join(REPOSITORY).join(REPOSITORY_CONFIG),
        "[core]\nxa.pinned=org.d.Pinned//2\n",
    )
    .unwrap();
    runtime(&user, "org.d.Pinned", "2", "[Runtime]\n");
    runtime(&system, "org.d.Pinned", "1", "[Runtime]\n");
    runtime(&declared, "org.e.Elsewhere", "1", "[Runtime]\n");
    let unused = unused_runtimes(&[system, user, declared]).unwrap();
    let references: Vec<String> = unused
        .runtimes
        .iter()
        .map(|runtime| runtime.reference.to_string())
        .collect();
    assert_eq!(
        references,
        [
            "runtime/org.b.Base/x86_64/1",
            "runtime/org.d.Pinned/x86_64/1",
        ]
    );
    assert_eq!(
        unused.for_account_only, 0,
        "the user's is pinned, the declared one is no account's"
    );
}

#[test]
fn an_installation_absent_reads_as_none_and_one_unplaced_as_unread() {
    // Catches a missing user's installation taken for unread (nothing would
    // ever be proposed), and one that could not be placed taken for none.
    let root = tempfile::tempdir().unwrap();
    let system = installation(root.path(), "system", true);
    runtime(&system, PLATFORM, "44", "[Runtime]\n");
    let absent = Installation::at(root.path().join("nobody"), false, false);
    assert_eq!(unused_of(&[system.clone(), absent]).len(), 1);
    // A desktop where nothing was ever installed for the account: flatpak's
    // permission store alone.
    let portals_only = Installation::at(root.path().join("user"), false, false);
    std::fs::create_dir_all(portals_only.path.join("db")).unwrap();
    assert_eq!(unused_of(&[system.clone(), portals_only]).len(), 1);
    // An empty folder may be a disk not mounted there.
    let empty = Installation::at(root.path().join("empty"), false, false);
    std::fs::create_dir_all(&empty.path).unwrap();
    assert!(unused_runtimes(&[system.clone(), empty]).is_err());
    let unplaced = Installation::unplaced(PathBuf::new(), "no data folder".to_owned());
    assert!(unused_runtimes(&[system, unplaced]).is_err());
}

#[test]
fn a_pin_s_glob_matches_as_flatpak_reads_it() {
    let reference = FlatpakRef::new(RefKind::Runtime, "org.gnome.Sdk", X86, "45").unwrap();
    for (pattern, matches) in [
        ("org.gnome.Sdk", true),
        ("runtime/org.gnome.Sdk/x86_64/45", true),
        ("org.gnome.*", true),
        ("org.gnome.*/x86_64/4*", true),
        ("org.gnome.Sdk//45", true),
        ("org.gnome.Sdk/*/46", false),
        ("org.gnome", false),
        ("*.Sdk", true),
        ("org.kde.*", false),
    ] {
        assert_eq!(
            Pattern::parse(pattern).unwrap().matches(&reference),
            matches,
            "{pattern}"
        );
    }
    for refused in ["app/org.gnome.Sdk", "a/b/c/d", "org gnome", "org/x86_64/4?"] {
        assert_eq!(Pattern::parse(refused), None, "{refused}");
    }
}

#[test]
fn the_room_given_back_is_that_of_the_files_shared_with_the_repository_alone() {
    // Catches a file another deployment shares counted (it stays), a file
    // named twice in the deployment left out, a link followed, and the
    // repository's object counted apart.
    let root = tempfile::tempdir().unwrap();
    let repository = root.path().join("objects");
    let other = root.path().join("other");
    let deployment = root.path().join("deployment");
    for folder in [&repository, &other, &deployment.join("files/lib")] {
        std::fs::create_dir_all(folder).unwrap();
    }
    let object = |name: &str, size: usize| {
        let path = repository.join(name);
        std::fs::write(&path, vec![1_u8; size]).unwrap();
        path
    };
    // Its own: the object and its name here.
    std::fs::hard_link(object("own", 64 * 1024), deployment.join("files/own")).unwrap();
    // Shared with another deployment: three names.
    let shared = object("shared", 256 * 1024);
    std::fs::hard_link(&shared, deployment.join("files/shared")).unwrap();
    std::fs::hard_link(&shared, other.join("shared")).unwrap();
    // Named twice here, and the object: its own still.
    let twice = object("twice", 32 * 1024);
    std::fs::hard_link(&twice, deployment.join("files/twice")).unwrap();
    std::fs::hard_link(&twice, deployment.join("files/lib/twice")).unwrap();
    // A link to a large file elsewhere: never followed.
    std::fs::write(root.path().join("large"), vec![1_u8; 1024 * 1024]).unwrap();
    std::os::unix::fs::symlink(root.path().join("large"), deployment.join("files/link")).unwrap();
    let blocks = |path: &Path| std::fs::metadata(path).unwrap().blocks() * BLOCK_BYTES;
    let expected = blocks(&deployment.join("files/own")) + blocks(&twice);
    let go = AtomicBool::new(false);
    assert_eq!(about_bytes(&deployment, &go), Some(expected));
    assert_eq!(
        about_bytes(&deployment, &AtomicBool::new(true)),
        None,
        "stopped"
    );
    assert_eq!(
        about_bytes(&deployment.join("files/link"), &go),
        None,
        "a link"
    );
}

#[test]
fn the_applications_of_an_installation_are_unread_past_a_name_refused_or_a_stray_link() {
    // Catches the leftovers' reading (the same listing) passing over an
    // application it cannot see whole: its data would be taken for one of
    // no application. And a desktop where nothing was installed for the
    // account taken for unread.
    let root = tempfile::tempdir().unwrap();
    let portals_only = Installation::at(root.path().join("user"), false, false);
    std::fs::create_dir_all(portals_only.path.join("db")).unwrap();
    assert_eq!(crate::apps::flatpak::app_ids(&portals_only), Ok(Vec::new()));
    let linked = installation(root.path(), "linked", false);
    std::fs::create_dir_all(linked.path.join("app")).unwrap();
    std::os::unix::fs::symlink("/elsewhere", linked.path.join("app/org.a.A")).unwrap();
    assert!(crate::apps::flatpak::app_ids(&linked).is_err());
    let refused = installation(root.path(), "refused", false);
    std::fs::create_dir_all(refused.path.join("app/org a")).unwrap();
    assert!(crate::apps::flatpak::app_ids(&refused).is_err());
}
