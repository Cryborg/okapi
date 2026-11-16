//! Analyses of real temporary folders, their sizes checked against a plain
//! walk by path.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::*;

/// How long a test waits for an analysis: a deadlock fails, it never hangs.
const WAIT: Duration = Duration::from_secs(20);
const BLOCK: usize = 4096;

fn threads(count: usize) -> std::num::NonZeroUsize {
    std::num::NonZeroUsize::new(count).unwrap()
}

/// A file of `blocks` blocks of data that does not compress.
fn file(path: &Path, blocks: usize) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut state: u32 = 0x9E37_79B9 ^ u32::try_from(blocks).unwrap();
    let data: Vec<u8> = (0..blocks * BLOCK)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect();
    fs::write(path, data).unwrap();
    path.to_path_buf()
}

/// What `root` takes, read by path without following a link: every entry
/// below it and itself, a file of several names counted once.
fn oracle(root: &Path) -> Size {
    let mut seen = BTreeSet::new();
    let mut total = Size::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            total.add(Size {
                allocated: metadata.blocks() * 512,
                apparent: metadata.size(),
                files: 0,
            });
            for entry in fs::read_dir(&path).unwrap() {
                pending.push(entry.unwrap().path());
            }
            continue;
        }
        let first = metadata.nlink() == 1 || seen.insert((metadata.dev(), metadata.ino()));
        total.add(Size {
            allocated: if first { metadata.blocks() * 512 } else { 0 },
            apparent: if first { metadata.size() } else { 0 },
            files: 1,
        });
    }
    total
}

/// The analysis of `root`, run to its end, far within the wait: a thread
/// never told of the end shows as a slow test, not a passing one.
fn analyzed(root: &Path, count: usize) -> UsageScan {
    let started = Instant::now();
    let scan = UsageScan::start(root, threads(count)).unwrap();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Complete));
    assert!(started.elapsed() < WAIT / 4, "{:?}", started.elapsed());
    scan
}

/// The table of mounts of a machine without any below the root.
fn no_mounts(_root: &Path) -> (BTreeSet<PathBuf>, Option<String>) {
    (BTreeSet::new(), None)
}

/// The analysis of `root` with `hook` before each folder read, to its end.
fn hooked(root: &Path, count: usize, hook: fn(&Path) -> bool) -> UsageScan {
    let scan = UsageScan::start_with(root, threads(count), no_mounts, Some(hook)).unwrap();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Complete));
    scan
}

/// The directory at `names` below the root.
fn find(scan: &UsageScan, names: &[&str]) -> Entry {
    let mut entry = scan.view(NodeId::ROOT).unwrap().directory;
    for name in names {
        entry = scan
            .view(entry.id)
            .unwrap()
            .children
            .into_iter()
            .find(|child| child.name == OsStr::new(name))
            .unwrap_or_else(|| panic!("no {name}"));
    }
    entry
}

#[test]
fn what_a_folder_takes_is_summed_at_every_level() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("a/x"), 3);
    file(&root.join("a/b/y"), 5);
    file(&root.join("z"), 1);
    let sparse = fs::File::create(root.join("sparse")).unwrap();
    sparse.set_len(10 << 20).unwrap();
    let scan = analyzed(root, 4);
    let total = scan.view(NodeId::ROOT).unwrap().directory;
    assert_eq!(total.total, oracle(root));
    assert!(!total.partial);
    assert_eq!(total.total.files, 4);
    assert!(total.total.apparent >= 10 << 20);
    assert!(
        total.total.allocated < 1 << 20,
        "the sparse file takes little"
    );
    assert_eq!(find(&scan, &["a"]).total, oracle(&root.join("a")));
    let a = fs::symlink_metadata(root.join("a")).unwrap();
    assert_eq!(
        scan.identity(find(&scan, &["a"]).id),
        Some(okapi_platform::FileIdentity::new(a.dev(), a.ino()))
    );
    assert_eq!(find(&scan, &["a", "b"]).total, oracle(&root.join("a/b")));
    assert_eq!(scan.progress().files, 4);
    assert_eq!(scan.progress().directories, 3);
    assert_eq!(scan.progress().allocated, total.total.allocated);
}

#[test]
fn a_file_of_several_names_is_counted_once_whatever_the_threads() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let shared = file(&root.join("d0/shared"), 768);
    file(&root.join("d0/witness"), 256);
    for index in 1..40 {
        let directory = root.join(format!("d{index}"));
        fs::create_dir(&directory).unwrap();
        fs::hard_link(&shared, directory.join("shared")).unwrap();
    }
    for _ in 0..5 {
        let scan = analyzed(root, 4);
        assert_eq!(
            scan.view(NodeId::ROOT).unwrap().directory.total,
            oracle(root)
        );
        let counters: Vec<(NodeId, Option<NodeId>)> = scan
            .view(NodeId::ROOT)
            .unwrap()
            .children
            .iter()
            .map(|child| {
                let (files, _) = scan.files(child.id).unwrap();
                let shared = files.iter().find(|file| file.name == "shared").unwrap();
                (child.id, shared.counted_in)
            })
            .collect();
        let unmarked: Vec<NodeId> = counters
            .iter()
            .filter(|(_, counted_in)| counted_in.is_none())
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(unmarked.len(), 1, "the name counted");
        assert!(
            counters
                .iter()
                .all(|(_, counted_in)| counted_in.is_none_or(|counter| counter == unmarked[0])),
            "every other name points to it"
        );
    }
}

#[test]
fn a_link_is_never_followed() {
    let temporary = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    file(&elsewhere.path().join("big"), 1280);
    let root = temporary.path();
    file(&root.join("small"), 1);
    symlink(elsewhere.path(), root.join("link")).unwrap();
    let scan = analyzed(root, 2);
    let total = scan.view(NodeId::ROOT).unwrap().directory.total;
    assert_eq!(total, oracle(root));
    assert!(total.allocated < 1 << 20);
    assert_eq!(
        scan.view(NodeId::ROOT).unwrap().children,
        Vec::<Entry>::new()
    );
    let (files, _) = scan.files(NodeId::ROOT).unwrap();
    assert!(
        files
            .iter()
            .any(|file| file.name == "link" && file.kind == FileKind::Link)
    );
}

#[test]
fn an_unreadable_folder_leaves_its_ancestors_partial() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("a/closed/inside"), 4);
    file(&root.join("a/open"), 1);
    let closed = root.join("a/closed");
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&closed).is_ok() {
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipped: root reads every folder");
        return;
    }
    let scan = analyzed(root, 2);
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(scan.progress().unreadable, 1);
    assert!(scan.settled());
    let entry = find(&scan, &["a", "closed"]);
    assert_eq!(
        entry.state,
        NodeState::Unreadable(std::io::ErrorKind::PermissionDenied)
    );
    assert!(entry.partial);
    assert!(find(&scan, &["a"]).partial);
    assert!(scan.view(NodeId::ROOT).unwrap().directory.partial);
    assert_eq!(scan.view(NodeId::ROOT).unwrap().directory.total.files, 1);
}

#[test]
fn a_mount_point_is_never_entered() {
    let Ok(list) = okapi_platform::mounts() else {
        eprintln!("skipped: no mount table");
        return;
    };
    let cgroup = Path::new("/sys/fs/cgroup");
    if !list.mounts.iter().any(|mount| mount.mount_point == cgroup) {
        eprintln!("skipped: no cgroup mount");
        return;
    }
    let scan = analyzed(Path::new("/sys/fs"), 2);
    let entry = find(&scan, &["cgroup"]);
    assert_eq!(entry.state, NodeState::OtherMount);
    assert_eq!(entry.total, Size::default());
    assert_eq!(scan.view(entry.id).unwrap().children, Vec::<Entry>::new());
}

/// The folder `listed` of the tests below, said a mount point by the table.
fn listed_mount(root: &Path) -> (BTreeSet<PathBuf>, Option<String>) {
    (BTreeSet::from([root.join("listed")]), None)
}

#[test]
fn a_mount_point_of_the_table_is_never_read() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("kept"), 1);
    // `listed` a link: read, it would count as one entry.
    let elsewhere = tempfile::tempdir().unwrap();
    symlink(elsewhere.path(), root.join("listed")).unwrap();
    let scan = UsageScan::start_with(root, threads(2), listed_mount, None).unwrap();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Complete));
    let entry = find(&scan, &["listed"]);
    assert_eq!(entry.state, NodeState::OtherMount);
    assert_eq!(scan.view(NodeId::ROOT).unwrap().directory.total.files, 1);
    let (files, _) = scan.files(NodeId::ROOT).unwrap();
    assert!(files.iter().all(|file| file.name != "listed"), "never read");
    assert!(
        matches!(scan.files(entry.id), Err(FilesError::NotRead)),
        "a mount is never opened"
    );
}

/// The table says the root a mount point too.
fn root_mounted(root: &Path) -> (BTreeSet<PathBuf>, Option<String>) {
    (BTreeSet::from([root.to_path_buf()]), None)
}

#[test]
fn a_root_that_is_a_mount_point_is_read() {
    let temporary = tempfile::tempdir().unwrap();
    file(&temporary.path().join("f"), 1);
    let scan = UsageScan::start_with(temporary.path(), threads(1), root_mounted, None).unwrap();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Complete));
    let root = scan.view(NodeId::ROOT).unwrap().directory;
    assert_eq!(root.state, NodeState::Read);
    assert_eq!(root.total.files, 1);
}

/// Panics on reading the folder `boom`.
fn boom(path: &Path) -> bool {
    assert!(
        path.file_name() != Some(OsStr::new("boom")),
        "a panic, for the test"
    );
    false
}

/// On reading a folder of the folder `root`, does `change` to every other
/// folder of it.
fn to_siblings(path: &Path, change: fn(&Path)) {
    let Some(parent) = path.parent() else {
        return;
    };
    if parent.file_name() != Some(OsStr::new("root")) {
        return;
    }
    for entry in fs::read_dir(parent).unwrap() {
        let sibling = entry.unwrap().path();
        if sibling != path && sibling.is_dir() && !sibling.is_symlink() {
            change(&sibling);
        }
    }
}

fn remove_siblings(path: &Path) -> bool {
    to_siblings(path, |sibling| fs::remove_dir_all(sibling).unwrap());
    false
}

static REPLACED: AtomicBool = AtomicBool::new(false);

/// Once, on reading a first folder of `root`, moves `folder`, `file` and
/// `link` away and puts in their place a new folder, a file and a link to
/// the full folder moved away.
fn replace_siblings(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if parent.file_name() != Some(OsStr::new("root")) || REPLACED.swap(true, Ordering::SeqCst) {
        return false;
    }
    for name in ["folder", "file", "link"] {
        let sibling = parent.join(name);
        if sibling == path {
            continue;
        }
        let away = parent.join(format!("{name}.away"));
        fs::rename(&sibling, &away).unwrap();
        match name {
            "folder" => fs::create_dir(&sibling).unwrap(),
            "file" => fs::write(&sibling, b"x").unwrap(),
            _ => symlink(&away, &sibling).unwrap(),
        }
    }
    false
}

/// Cancels the analysis as it reads the folder `root`, before any entry.
fn cancel_at_root(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new("root"))
}

/// The root once the thread reading it when the analysis was cancelled has
/// written it: `wait` says the end at once, the thread finishes its folder.
fn settled_root(scan: &UsageScan) -> Entry {
    let started = Instant::now();
    loop {
        let root = scan.view(NodeId::ROOT).unwrap().directory;
        if root.state != NodeState::Pending || started.elapsed() > WAIT {
            return root;
        }
        std::thread::yield_now();
    }
}

#[test]
fn a_folder_replaced_during_the_analysis_is_a_doubt_and_never_followed() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    for name in ["folder", "file", "link"] {
        file(&root.join(name).join("inside"), 8);
    }
    let scan = hooked(&root, 1, replace_siblings);
    let view = scan.view(NodeId::ROOT).unwrap();
    assert!(view.directory.partial, "{view:?}");
    let read: Vec<&Entry> = view
        .children
        .iter()
        .filter(|child| child.state == NodeState::Read)
        .collect();
    assert_eq!(read.len(), 1, "only the first folder read: {view:?}");
    assert_eq!(view.directory.total.files, 1, "nothing moved away counted");
    assert_eq!(view.children.len(), 3);
    for name in ["folder", "file", "link"] {
        let replaced = view.children.iter().find(|child| child.name == name);
        if let Some(replaced) = replaced.filter(|child| child.state != NodeState::Read) {
            assert_eq!(replaced.state, NodeState::Changed, "{name}");
        }
    }
    for child in view
        .children
        .iter()
        .filter(|child| child.state != NodeState::Read)
    {
        assert!(child.partial, "a replaced folder is a doubt: {child:?}");
    }
}

#[test]
fn a_cancellation_in_the_middle_of_a_folder_leaves_it_partial() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    for name in ["a", "b", "c"] {
        file(&root.join(name), 1);
    }
    let scan = UsageScan::start_with(&root, threads(1), no_mounts, Some(cancel_at_root)).unwrap();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Cancelled));
    let root_entry = settled_root(&scan);
    assert_eq!(root_entry.state, NodeState::Read);
    assert!(root_entry.partial, "cut short");
    assert_eq!(root_entry.total.files, 0);
}

#[test]
fn the_files_of_a_folder_cut_short_never_read_a_mount_point() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    file(&root.join("kept"), 1);
    // `listed`, said a mount point by the table, a link: read, it would be
    // listed as one.
    let elsewhere = tempfile::tempdir().unwrap();
    symlink(elsewhere.path(), root.join("listed")).unwrap();
    let scan =
        UsageScan::start_with(&root, threads(1), listed_mount, Some(cancel_at_root)).unwrap();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Cancelled));
    assert_eq!(settled_root(&scan).state, NodeState::Read);
    let (files, _) = scan.files(NodeId::ROOT).unwrap();
    let names: Vec<&OsStr> = files.iter().map(|file| file.name.as_os_str()).collect();
    assert_eq!(names, [OsStr::new("kept")]);
}

/// How many threads are reading `x` or `y` at once, and whether two did.
static INSIDE: AtomicUsize = AtomicUsize::new(0);
static MET: AtomicBool = AtomicBool::new(false);

/// Holds the reading of `x` and `y` until both are read at once (or a
/// quarter of the wait passed).
fn meet(path: &Path) -> bool {
    if !matches!(path.file_name().and_then(OsStr::to_str), Some("x" | "y")) {
        return false;
    }
    INSIDE.fetch_add(1, Ordering::SeqCst);
    let started = Instant::now();
    while started.elapsed() < WAIT / 4 {
        if INSIDE.load(Ordering::SeqCst) >= 2 {
            MET.store(true, Ordering::SeqCst);
            break;
        }
        std::thread::yield_now();
    }
    false
}

#[test]
fn two_threads_meeting_one_file_of_two_names_count_it_once() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    // Many names in each, so that a check then an insertion apart would
    // race on one of them.
    fs::create_dir(root.join("y")).unwrap();
    for index in 0..500 {
        let shared = file(&root.join(format!("x/n{index}")), 1);
        fs::hard_link(&shared, root.join(format!("y/n{index}"))).unwrap();
    }
    let scan = hooked(root, 2, meet);
    assert!(MET.load(Ordering::SeqCst), "both folders read at once");
    assert_eq!(
        scan.view(NodeId::ROOT).unwrap().directory.total,
        oracle(root)
    );
}

#[test]
fn a_folder_removed_before_its_reading_leaves_no_doubt() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    for name in ["a", "b", "c"] {
        file(&root.join(name).join("f"), 1);
    }
    let scan = UsageScan::start_with(&root, threads(1), no_mounts, Some(remove_siblings)).unwrap();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Complete));
    let view = scan.view(NodeId::ROOT).unwrap();
    let gone = view
        .children
        .iter()
        .filter(|child| child.state == NodeState::Gone)
        .count();
    assert_eq!(gone, 2, "{view:?}");
    assert!(!view.directory.partial, "a folder gone is no doubt");
}

#[test]
fn a_panic_in_a_thread_leaves_one_folder_unread_and_the_analysis_ends() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("boom/lost"), 1);
    file(&root.join("fine/kept"), 1);
    let scan = UsageScan::start_with(root, threads(1), no_mounts, Some(boom)).unwrap();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Complete));
    assert_eq!(
        find(&scan, &["boom"]).state,
        NodeState::Unreadable(std::io::ErrorKind::Other)
    );
    assert_eq!(find(&scan, &["fine"]).total.files, 1);
}

#[test]
fn a_cancellation_after_the_end_changes_nothing() {
    let temporary = tempfile::tempdir().unwrap();
    file(&temporary.path().join("f"), 1);
    let scan = analyzed(temporary.path(), 1);
    scan.cancel();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Complete));
    assert!(!scan.is_cancelled());
}

#[test]
fn two_names_of_one_file_in_one_folder_count_once() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let first = file(&root.join("first"), 3);
    fs::hard_link(&first, root.join("second")).unwrap();
    let scan = analyzed(root, 1);
    assert_eq!(
        scan.view(NodeId::ROOT).unwrap().directory.total,
        oracle(root)
    );
    let (files, _) = scan.files(NodeId::ROOT).unwrap();
    let mut counted: Vec<Option<NodeId>> = files.iter().map(|file| file.counted_in).collect();
    counted.sort();
    assert_eq!(
        counted,
        [None, Some(NodeId::ROOT)],
        "one name counted, the other points here"
    );
}

#[test]
fn a_folder_replaced_since_the_analysis_is_not_listed() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("old/f"), 1);
    let scan = analyzed(root, 1);
    let old = find(&scan, &["old"]);
    assert_eq!(scan.files(old.id).unwrap().0.len(), 1, "read again before");
    fs::rename(root.join("old"), root.join("moved")).unwrap();
    file(&root.join("old/other"), 1);
    assert!(matches!(scan.files(old.id), Err(FilesError::Changed)));
}

#[test]
fn a_folder_removed_since_the_analysis_is_said_gone() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("old/f"), 1);
    let scan = analyzed(root, 1);
    let old = find(&scan, &["old"]);
    fs::remove_dir_all(root.join("old")).unwrap();
    assert!(matches!(scan.files(old.id), Err(FilesError::Gone)));
}

#[test]
fn a_root_replaced_since_the_analysis_is_not_listed() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    file(&root.join("f"), 1);
    file(&root.join("sub/g"), 1);
    let scan = analyzed(&root, 1);
    let sub = find(&scan, &["sub"]);
    fs::rename(&root, temporary.path().join("moved")).unwrap();
    file(&root.join("sub/g"), 1);
    assert!(matches!(scan.files(NodeId::ROOT), Err(FilesError::Changed)));
    assert!(matches!(scan.files(sub.id), Err(FilesError::Changed)));
}

/// `root/sub/listed` said a mount point by the table.
fn nested_mount(root: &Path) -> (BTreeSet<PathBuf>, Option<String>) {
    (BTreeSet::from([root.join("sub/listed")]), None)
}

/// Cancels the analysis as it reads the folder `sub`.
fn cancel_at_sub(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new("sub"))
}

#[test]
fn the_files_of_a_nested_folder_cut_short_never_read_a_mount_point() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("sub/kept"), 1);
    let elsewhere = tempfile::tempdir().unwrap();
    symlink(elsewhere.path(), root.join("sub/listed")).unwrap();
    let scan = UsageScan::start_with(root, threads(1), nested_mount, Some(cancel_at_sub)).unwrap();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Cancelled));
    let started = Instant::now();
    let sub = loop {
        let sub = find(&scan, &["sub"]);
        if sub.state != NodeState::Pending || started.elapsed() > WAIT {
            break sub;
        }
        std::thread::yield_now();
    };
    assert_eq!(
        scan.path(sub.id),
        Some(root.canonicalize().unwrap().join("sub"))
    );
    let (files, _) = scan.files(sub.id).unwrap();
    let names: Vec<&OsStr> = files.iter().map(|file| file.name.as_os_str()).collect();
    assert_eq!(names, [OsStr::new("kept")]);
}

#[test]
fn a_folder_made_since_the_analysis_is_no_file_of_it() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("f"), 1);
    let scan = analyzed(root, 1);
    fs::create_dir(root.join("new")).unwrap();
    let (files, _) = scan.files(NodeId::ROOT).unwrap();
    let names: Vec<&OsStr> = files.iter().map(|file| file.name.as_os_str()).collect();
    assert_eq!(names, [OsStr::new("f")]);
}

#[test]
fn the_files_of_a_root_that_is_a_mount_point_are_read() {
    let cgroup = Path::new("/sys/fs/cgroup");
    if okapi_platform::mount_root(cgroup).ok().flatten() != Some(true) {
        eprintln!("skipped: no cgroup mount");
        return;
    }
    let scan = analyzed(cgroup, 2);
    assert!(scan.files(NodeId::ROOT).is_ok());
}

#[test]
fn a_mount_missed_by_the_table_is_still_never_entered() {
    let dev = Path::new("/dev");
    let Some(mounted) = ["pts", "shm", "mqueue", "hugepages"]
        .into_iter()
        .find(|name| okapi_platform::mount_root(&dev.join(name)).ok().flatten() == Some(true))
    else {
        eprintln!("skipped: no mount under /dev");
        return;
    };
    let scan = UsageScan::start_with(dev, threads(2), no_mounts, None).unwrap();
    assert_eq!(scan.wait(WAIT), Some(Outcome::Complete));
    let entry = find(&scan, &[mounted]);
    assert_eq!(entry.state, NodeState::OtherMount);
    assert_eq!(entry.total, Size::default());
    assert_eq!(scan.view(entry.id).unwrap().children, Vec::<Entry>::new());
}

#[test]
fn a_folder_whose_parent_was_replaced_is_not_listed() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("a/b/f"), 1);
    let scan = analyzed(root, 1);
    let b = find(&scan, &["a", "b"]);
    // `b` itself, moved into a new `a`: only its parent changed.
    fs::rename(root.join("a"), root.join("a.old")).unwrap();
    fs::create_dir(root.join("a")).unwrap();
    fs::rename(root.join("a.old/b"), root.join("a/b")).unwrap();
    assert!(matches!(scan.files(b.id), Err(FilesError::Changed)));
}

#[test]
fn a_folder_whose_parent_was_removed_is_said_gone() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("a/b/f"), 1);
    let scan = analyzed(root, 1);
    let b = find(&scan, &["a", "b"]);
    fs::remove_dir_all(root.join("a")).unwrap();
    assert!(matches!(scan.files(b.id), Err(FilesError::Gone)));
}

#[test]
fn a_root_removed_since_the_analysis_is_said_gone() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    file(&root.join("sub/f"), 1);
    let scan = analyzed(&root, 1);
    let sub = find(&scan, &["sub"]);
    fs::remove_dir_all(&root).unwrap();
    assert!(matches!(scan.files(NodeId::ROOT), Err(FilesError::Gone)));
    assert!(matches!(scan.files(sub.id), Err(FilesError::Gone)));
}

#[test]
fn a_root_reached_through_a_link_since_the_analysis_is_not_listed() {
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path().join("base");
    let root = base.join("root");
    file(&root.join("sub/f"), 1);
    let scan = analyzed(&root, 1);
    let sub = find(&scan, &["sub"]);
    assert!(scan.files(sub.id).is_ok(), "read again before");
    // The very root, its parent moved and its old path now a link to it.
    fs::rename(&base, temporary.path().join("moved")).unwrap();
    symlink(temporary.path().join("moved"), &base).unwrap();
    assert!(matches!(scan.files(NodeId::ROOT), Err(FilesError::Changed)));
    assert!(matches!(scan.files(sub.id), Err(FilesError::Changed)));
}

#[test]
fn a_folder_the_system_refuses_on_the_way_is_a_refusal() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("a/b/f"), 1);
    let scan = analyzed(root, 1);
    let b = find(&scan, &["a", "b"]);
    let closed = root.join("a");
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    let files = scan.files(b.id);
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    if files.is_ok() {
        eprintln!("skipped: root reads every folder");
        return;
    }
    assert!(
        matches!(
            files,
            Err(FilesError::Io(ref error)) if error.kind() == std::io::ErrorKind::PermissionDenied
        ),
        "{files:?}"
    );
}

#[test]
fn the_files_of_a_folder_are_read_again_largest_first() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    file(&root.join("f/small"), 1);
    file(&root.join("f/large"), 6);
    let odd = OsStr::from_bytes(b"caf\xe9");
    file(&root.join("f").join(odd), 3);
    file(&root.join("f/sub/inner"), 2);
    // Of one size, by name; the sparse one, large but taking little, last.
    file(&root.join("f/tie-b"), 2);
    file(&root.join("f/tie-a"), 2);
    fs::File::create(root.join("f/sparse"))
        .unwrap()
        .set_len(10 << 20)
        .unwrap();
    let scan = analyzed(root, 2);
    let folder = find(&scan, &["f"]);
    let (files, unreadable) = scan.files(folder.id).unwrap();
    assert_eq!(unreadable, 0);
    let names: Vec<&OsStr> = files.iter().map(|file| file.name.as_os_str()).collect();
    assert_eq!(
        names,
        [
            OsStr::new("large"),
            odd,
            OsStr::new("tie-a"),
            OsStr::new("tie-b"),
            OsStr::new("small"),
            OsStr::new("sparse")
        ]
    );
    assert!(
        files
            .iter()
            .all(|file| file.kind == FileKind::File && file.counted_in.is_none())
    );
    // The folder swapped for a link since: never followed.
    let sub = find(&scan, &["f", "sub"]);
    fs::rename(root.join("f/sub"), root.join("moved")).unwrap();
    symlink(root.join("moved"), root.join("f/sub")).unwrap();
    assert!(matches!(scan.files(sub.id), Err(FilesError::Changed)));
}

#[test]
fn one_thread_or_eight_find_the_same_and_a_deep_chain_ends() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    for index in 0..300 {
        file(
            &root.join(format!("d{}/d{index}/f", index % 17)),
            index % 3 + 1,
        );
    }
    let one = analyzed(root, 1)
        .view(NodeId::ROOT)
        .unwrap()
        .directory
        .total;
    let eight = analyzed(root, 8)
        .view(NodeId::ROOT)
        .unwrap()
        .directory
        .total;
    assert_eq!(one, oracle(root));
    assert_eq!(eight, one);

    let deep = tempfile::tempdir().unwrap();
    let mut path = deep.path().to_path_buf();
    for _ in 0..300 {
        path.push("d");
    }
    file(&path.join("bottom"), 1);
    let scan = analyzed(deep.path(), 1);
    assert_eq!(
        scan.view(NodeId::ROOT).unwrap().directory.total,
        oracle(deep.path())
    );
    assert_eq!(scan.progress().directories, 301);
}

#[test]
fn a_root_that_is_a_file_is_refused_and_a_link_is_followed_there_only() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let plain = file(&root.join("plain"), 1);
    fs::create_dir(root.join("folder")).unwrap();
    symlink(root.join("folder"), root.join("link")).unwrap();
    assert!(matches!(
        UsageScan::start(&plain, threads(1)),
        Err(ScanError::Root(_))
    ));
    // Given by its link, the folder is analyzed where it is.
    let scan = analyzed(&root.join("link"), 1);
    assert_eq!(scan.root(), root.join("folder").canonicalize().unwrap());
}
