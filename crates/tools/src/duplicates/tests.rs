//! Searches of real temporary folders.

use std::collections::BTreeSet;
use std::fs;
use std::num::NonZeroUsize;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use okapi_engine::Exclusions;

use super::*;

/// How long a test waits for a search: a deadlock fails, it never hangs.
const WAIT: Duration = Duration::from_secs(20);
const BLOCK: usize = 4096;
/// The smallest file compared in these tests.
const MIN: u64 = 2 * BLOCK as u64;

/// `blocks` blocks of data that does not repeat, from `seed`.
fn data(seed: u32, blocks: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9 ^ seed;
    (0..blocks * BLOCK)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect()
}

fn write(path: &Path, content: &[u8]) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
    path.to_path_buf()
}

/// The table of mounts of a machine without any below the roots.
fn no_mounts(_root: &Path) -> (BTreeSet<PathBuf>, Option<String>) {
    (BTreeSet::new(), None)
}

fn options(threads: usize) -> SearchOptions {
    SearchOptions {
        min_size: MIN,
        threads: NonZeroUsize::new(threads).unwrap(),
        exclusions: Exclusions::none(),
    }
}

fn searched_with(roots: &[PathBuf], options: SearchOptions) -> DuplicateSearch {
    let started = Instant::now();
    let search = DuplicateSearch::start(roots, options).unwrap();
    assert_eq!(search.wait(WAIT), Some(Outcome::Complete));
    assert!(started.elapsed() < WAIT / 4, "{:?}", started.elapsed());
    search
}

fn searched(root: &Path) -> DuplicateSearch {
    searched_with(&[root.to_path_buf()], options(2))
}

/// The groups found, each a set of paths relative to `root`.
fn groups(search: &DuplicateSearch, root: &Path) -> BTreeSet<BTreeSet<PathBuf>> {
    let root = fs::canonicalize(root).unwrap();
    search
        .report()
        .unwrap()
        .groups()
        .iter()
        .map(|group| {
            group
                .copies()
                .iter()
                .map(|copy| copy.path().strip_prefix(&root).unwrap().to_path_buf())
                .collect()
        })
        .collect()
}

fn set(paths: &[&str]) -> BTreeSet<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

#[test]
fn files_of_one_content_make_a_group_and_nothing_else_does() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let content = data(1, 6);
    write(&root.join("a"), &content);
    write(&root.join("x/a copy"), &content);
    write(&root.join("x/y/a again"), &content);
    // Same size, same ends, another middle: only the whole content tells.
    let mut middle = content.clone();
    middle[3 * BLOCK] ^= 0xFF;
    write(&root.join("middle"), &middle);
    // Same size, another start: its ends tell.
    let mut start = content.clone();
    start[0] ^= 0xFF;
    write(&root.join("start"), &start);
    // Same size, another end.
    let mut end = content.clone();
    *end.last_mut().unwrap() ^= 0xFF;
    write(&root.join("end"), &end);
    write(&root.join("other size"), &data(1, 7));
    let search = searched(root);
    assert_eq!(
        groups(&search, root),
        BTreeSet::from([set(&["a", "x/a copy", "x/y/a again"])])
    );
    // Only the files whose ends agree are read whole.
    let Progress::Contents { bytes_to_read, .. } = search.progress() else {
        panic!("{:?}", search.progress());
    };
    assert_eq!(bytes_to_read, 4 * content.len() as u64);
    let group = &search.report().unwrap().groups()[0];
    assert_eq!(group.size(), content.len() as u64);
    assert_eq!(group.gain(), 2 * content.len() as u64);
}

#[test]
fn groups_come_by_what_they_free_most_first() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    // Three copies of 4 blocks free 8; two of 6 free 6.
    for name in ["small 1", "small 2", "small 3"] {
        write(&root.join(name), &data(2, 4));
    }
    for name in ["large 1", "large 2"] {
        write(&root.join(name), &data(3, 6));
    }
    let report_groups: Vec<u64> = searched(root)
        .report()
        .unwrap()
        .groups()
        .iter()
        .map(Group::size)
        .collect();
    assert_eq!(report_groups, [4 * BLOCK as u64, 6 * BLOCK as u64]);
}

#[test]
fn two_names_of_one_file_are_one_copy() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let first = write(&root.join("first"), &data(4, 3));
    fs::hard_link(&first, root.join("second name")).unwrap();
    assert!(
        groups(&searched(root), root).is_empty(),
        "a file is never its own copy"
    );
    write(&root.join("real copy"), &data(4, 3));
    let search = searched(root);
    let report = search.report().unwrap();
    let [group] = report.groups() else {
        panic!("one group: {:?}", report.groups());
    };
    assert_eq!(group.copies().len(), 2);
    let linked = group
        .copies()
        .iter()
        .find(|copy| copy.links() == 2)
        .unwrap();
    assert!(!linked.frees(), "its other name keeps the content");
    assert_eq!(group.gain(), 3 * BLOCK as u64, "only the plain copy frees");
    assert_eq!(report.stats().other_names, 1);
}

#[test]
fn a_folder_given_twice_or_inside_another_is_searched_once() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join("inner/a"), &data(5, 3));
    write(&root.join("b"), &data(5, 3));
    let roots = [
        root.join("inner"),
        root.to_path_buf(),
        root.join("inner"),
        root.join("inner/../inner"),
    ];
    let search = searched_with(&roots, options(1));
    assert_eq!(
        groups(&search, root),
        BTreeSet::from([set(&["inner/a", "b"])])
    );
    assert_eq!(search.report().unwrap().roots().count(), 2);
    assert_eq!(search.report().unwrap().stats().files, 2);
}

#[test]
fn a_root_below_a_folder_another_root_skips_is_searched() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join(".local/photos/a"), &data(17, 3));
    write(&root.join("project/.git/HEAD"), b"ref");
    write(&root.join("project/b"), &data(17, 3));
    let roots = [
        root.to_path_buf(),
        root.join(".local/photos"),
        root.join("project"),
    ];
    let search = searched_with(&roots, options(1));
    assert_eq!(
        groups(&search, root),
        BTreeSet::from([set(&[".local/photos/a", "project/b"])])
    );
}

#[test]
fn a_link_is_never_followed_nor_a_copy() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    let outside = temporary.path().join("outside");
    write(&outside.join("a"), &data(6, 3));
    write(&outside.join("b"), &data(6, 3));
    write(&root.join("c"), &data(6, 3));
    symlink(&outside, root.join("to outside")).unwrap();
    symlink(root.join("c"), root.join("to c")).unwrap();
    let search = searched(&root);
    assert!(groups(&search, &root).is_empty());
    assert_eq!(search.report().unwrap().stats().files, 1);
}

#[test]
fn hidden_folders_projects_dependencies_and_exclusions_are_left_out() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let content = data(7, 3);
    write(&root.join("kept"), &content);
    write(&root.join(".config/hidden"), &content);
    write(&root.join("project/.git/HEAD"), b"ref");
    write(&root.join("project/src/copy"), &content);
    write(&root.join("web/node_modules/copy"), &content);
    write(&root.join("share/Trash/files/copy"), &content);
    write(&root.join("excluded/copy"), &content);
    write(&root.join("logs/copy.log"), &content);
    // A hidden file is no hidden folder: it is compared.
    write(&root.join(".hidden file"), &content);
    write(&root.join("small"), &content[..BLOCK]);
    write(&root.join("small copy"), &content[..BLOCK]);
    let canonical = fs::canonicalize(root).unwrap();
    let exclusions = Exclusions::new([canonical.join("excluded"), PathBuf::from("*.log")]).unwrap();
    let search = searched_with(
        &[root.to_path_buf()],
        SearchOptions {
            exclusions,
            ..options(2)
        },
    );
    assert_eq!(
        groups(&search, root),
        BTreeSet::from([set(&["kept", ".hidden file"])])
    );
    let stats = search.report().unwrap().stats();
    assert_eq!(stats.hidden_directories, 1);
    assert_eq!(stats.projects, 1);
    assert_eq!(stats.dependency_directories, 1);
    assert_eq!(stats.trash_directories, 1);
    assert_eq!(stats.excluded, 2);
    assert_eq!(stats.below_minimum, 2);
}

#[test]
fn a_root_that_is_a_project_is_searched() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join(".git/HEAD"), b"ref");
    write(&root.join("a"), &data(8, 3));
    write(&root.join("b"), &data(8, 3));
    assert_eq!(
        groups(&searched(root), root),
        BTreeSet::from([set(&["a", "b"])])
    );
}

/// What a test does to a file before its `n`th reading by the search
/// (1: its ends; 2: its whole content), by the file's canonical path:
/// tests running at once never touch each other's files.
type Before = (u32, fn(&Path));

static BEFORE: std::sync::Mutex<Vec<(PathBuf, u32, Before)>> = std::sync::Mutex::new(Vec::new());

/// Has `action` done to the file at `path` before its `nth` reading.
fn before_reading(path: &Path, nth: u32, action: fn(&Path)) {
    let path = fs::canonicalize(path).unwrap();
    BEFORE.lock().unwrap().push((path, 0, (nth, action)));
}

/// The seam of the search: does what a test asked before this reading.
fn act(path: &Path) {
    let action = {
        let mut planned = BEFORE.lock().unwrap();
        planned
            .iter_mut()
            .find(|(file, _, _)| file == path)
            .and_then(|(_, readings, (nth, action))| {
                *readings += 1;
                (*readings == *nth).then_some(*action)
            })
    };
    if let Some(action) = action {
        action(path);
    }
}

/// The search of `root` with the seam, run to its end.
fn searched_acting(root: &Path, threads: usize) -> DuplicateSearch {
    let search = DuplicateSearch::start_with(
        &[root.to_path_buf()],
        options(threads),
        no_mounts,
        Some(act),
    )
    .unwrap();
    assert_eq!(search.wait(WAIT), Some(Outcome::Complete));
    search
}

/// Writes `path` again, the same content, past the tick of a coarse clock,
/// its modification date set back: only the change of its inode says it was
/// written.
fn written_again(path: &Path) {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let content = fs::read(path).unwrap();
    fs::write(path, content).unwrap();
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(modified).unwrap();
}

#[test]
fn a_file_written_since_it_was_listed_leaves_the_search() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join("a"), &data(9, 3));
    let written = write(&root.join("b"), &data(9, 3));
    // Once its ends matched, before its whole content is read.
    before_reading(&written, 2, written_again);
    let search = searched_acting(root, 1);
    let report = search.report().unwrap();
    assert!(report.groups().is_empty(), "{:?}", report.groups());
    assert_eq!(report.stats().dropped, 1);
    // What it left out counts as read: the progress reaches its end.
    let size = 2 * 3 * BLOCK as u64;
    assert_eq!(
        search.progress(),
        Progress::Contents {
            bytes_read: size,
            bytes_to_read: size
        }
    );
}

#[test]
fn a_file_that_cannot_be_read_is_never_equal() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join("a"), &data(10, 3));
    // Two unreadable files of another content, but of the same size: no key
    // read for either must never make them equal.
    let locked = [
        write(&root.join("b"), &data(13, 3)),
        write(&root.join("c"), &data(14, 3)),
    ];
    for path in &locked {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    }
    if fs::read(&locked[0]).is_ok() {
        eprintln!("skipped: root reads every file");
        return;
    }
    let search = searched(root);
    for path in &locked {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let report = search.report().unwrap();
    assert!(report.groups().is_empty(), "{:?}", report.groups());
    assert_eq!(report.stats().dropped, 2);
}

#[test]
fn a_cancelled_search_gives_no_report() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    for index in 0..50 {
        write(&root.join(format!("{index}")), &data(11, 3));
    }
    let started = Instant::now();
    // Each file read waits: the search cannot end before it is cancelled.
    let slowly = |_: &Path| std::thread::sleep(Duration::from_millis(50));
    let search =
        DuplicateSearch::start_with(&[root.to_path_buf()], options(1), no_mounts, Some(slowly))
            .unwrap();
    search.cancel();
    assert_eq!(search.wait(WAIT), Some(Outcome::Cancelled));
    assert!(started.elapsed() < WAIT / 4);
    assert!(search.report().is_none());
    // A cancellation after the end changes nothing.
    let ended = searched(root);
    ended.cancel();
    assert_eq!(ended.wait(WAIT), Some(Outcome::Complete));
    assert!(ended.report().is_some());
}

#[test]
fn one_thread_or_four_find_the_same() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    for group in 0..12_u32 {
        for copy in 0..=(group % 3) + 1 {
            write(
                &root.join(format!("{group}/{copy}")),
                &data(group, 2 + usize::try_from(group % 4).unwrap()),
            );
        }
    }
    let one = groups(&searched_with(&[root.to_path_buf()], options(1)), root);
    let four = groups(&searched_with(&[root.to_path_buf()], options(4)), root);
    assert_eq!(one.len(), 12);
    assert_eq!(one, four);
}

#[test]
fn a_copy_is_found_only_in_the_report_that_gave_it() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join("a"), &data(12, 3));
    write(&root.join("b"), &data(12, 3));
    let first = searched(root);
    let second = searched(root);
    let id = first.report().unwrap().groups()[0].copies()[0].id();
    let (_, copy) = first.report().unwrap().find(id).unwrap();
    assert_eq!(copy.id(), id);
    assert!(second.report().unwrap().find(id).is_none(), "invariant 1");
}

#[test]
fn a_root_that_is_no_folder_is_refused() {
    let temporary = tempfile::tempdir().unwrap();
    let file = write(&temporary.path().join("file"), b"x");
    assert!(matches!(
        DuplicateSearch::start(&[file], options(1)),
        Err(SearchError::Root { .. })
    ));
    assert!(matches!(
        DuplicateSearch::start(&[], options(1)),
        Err(SearchError::NoRoot)
    ));
}

#[test]
fn a_mount_point_of_the_table_is_never_read() {
    let temporary = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temporary.path()).unwrap();
    write(&root.join("a"), &data(15, 3));
    write(&root.join("mounted/b"), &data(15, 3));
    // A name the table says is a mount, though it is a link here: read, it
    // would at least have been looked at.
    symlink(root.join("mounted/b"), root.join("mounted link")).unwrap();
    let mounted = |root: &Path| {
        let below = [root.join("mounted"), root.join("mounted link")];
        (below.into_iter().collect(), None)
    };
    let search =
        DuplicateSearch::start_with(std::slice::from_ref(&root), options(1), mounted, None)
            .unwrap();
    assert_eq!(search.wait(WAIT), Some(Outcome::Complete));
    let report = search.report().unwrap();
    assert!(report.groups().is_empty(), "{:?}", report.groups());
    assert_eq!(report.stats().other_mounts, 2);
    assert_eq!(report.stats().files, 1);
}

#[test]
fn an_excluded_root_is_not_searched() {
    let temporary = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temporary.path()).unwrap();
    write(&root.join("a"), &data(16, 3));
    write(&root.join("b"), &data(16, 3));
    let search = searched_with(
        std::slice::from_ref(&root),
        SearchOptions {
            exclusions: Exclusions::new([root.clone()]).unwrap(),
            ..options(1)
        },
    );
    let report = search.report().unwrap();
    assert!(report.groups().is_empty(), "{:?}", report.groups());
    assert_eq!((report.stats().excluded, report.stats().files), (1, 0));
}

#[test]
fn a_search_that_panics_ends_failed_without_a_report() {
    let temporary = tempfile::tempdir().unwrap();
    let broken = |_: &Path| -> (BTreeSet<PathBuf>, Option<String>) { panic!("broken table") };
    let search =
        DuplicateSearch::start_with(&[temporary.path().to_path_buf()], options(1), broken, None)
            .unwrap();
    assert_eq!(search.wait(WAIT), Some(Outcome::Failed));
    assert!(search.report().is_none());
}

#[test]
fn files_that_differ_past_the_first_block_are_no_copies() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    // 100 blocks: past one read of the whole content, and not a whole
    // number of them.
    let content = data(34, 100);
    write(&root.join("a"), &content);
    write(&root.join("b"), &content);
    let mut other = content.clone();
    other[200_000] ^= 0xFF;
    write(&root.join("c"), &other);
    let search = searched(root);
    assert_eq!(groups(&search, root), BTreeSet::from([set(&["a", "b"])]));
    let size = 3 * content.len() as u64;
    assert_eq!(
        search.progress(),
        Progress::Contents {
            bytes_read: size,
            bytes_to_read: size
        }
    );
}

/// Puts a link to its twin `a` in the place of the file at `path`.
fn swapped_for_a_link(path: &Path) {
    fs::remove_file(path).unwrap();
    symlink(path.with_file_name("a"), path).unwrap();
}

#[test]
fn a_file_swapped_for_a_link_leaves_the_search() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join("a"), &data(35, 3));
    let swapped = write(&root.join("b"), &data(35, 3));
    before_reading(&swapped, 2, swapped_for_a_link);
    let report_search = searched_acting(root, 1);
    let report = report_search.report().unwrap();
    assert!(report.groups().is_empty(), "{:?}", report.groups());
    assert_eq!(report.stats().dropped, 1);
}

fn panics(_: &Path) {
    panic!("reading this file broke");
}

#[test]
fn a_file_whose_reading_panics_leaves_the_search_alone() {
    for threads in [1, 4] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        for name in ["a", "c"] {
            write(&root.join(name), &data(36, 3));
        }
        let broken = write(&root.join("b"), &data(36, 3));
        before_reading(&broken, 1, panics);
        let search = searched_acting(root, threads);
        assert_eq!(groups(&search, root), BTreeSet::from([set(&["a", "c"])]));
        assert_eq!(search.report().unwrap().stats().dropped, 1);
    }
}

#[test]
fn a_copy_of_several_names_frees_nothing_in_the_gain() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    for name in ["a", "b", "c"] {
        write(&root.join(name), &data(37, 3));
    }
    for name in ["a", "b"] {
        fs::hard_link(root.join(name), temporary.path().join(name)).unwrap();
    }
    let search = searched(&root);
    let report = search.report().unwrap();
    // Only `c` frees its size: two copies go at most, but only one frees.
    assert_eq!(report.groups()[0].gain(), 3 * BLOCK as u64);
    let paths: Vec<&Path> = report.groups()[0]
        .copies()
        .iter()
        .map(FileCopy::path)
        .collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "copies by path");
}

#[test]
fn small_files_are_compared_at_every_size_and_empty_ones_never() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let sizes = [1, 4095, 4096, 4097, 8191, 8192, 8193];
    let content = data(38, 3);
    for size in sizes {
        let same = &content[..size];
        write(&root.join(format!("{size}/a")), same);
        write(&root.join(format!("{size}/b")), same);
        // Another last byte: its tail, or its head when it is that small.
        let mut other = same.to_vec();
        *other.last_mut().unwrap() ^= 0xFF;
        write(&root.join(format!("{size}/c")), &other);
    }
    write(&root.join("empty/a"), b"");
    write(&root.join("empty/b"), b"");
    let search = searched_with(
        &[root.to_path_buf()],
        SearchOptions {
            min_size: 0,
            ..options(2)
        },
    );
    let expected: BTreeSet<BTreeSet<PathBuf>> = sizes
        .iter()
        .map(|size| set(&[&format!("{size}/a"), &format!("{size}/b")]))
        .collect();
    assert_eq!(groups(&search, root), expected);
}

#[test]
fn a_filesystem_that_may_show_a_file_twice_is_known() {
    for filesystem in [
        "fuse.bindfs",
        "fuse.gvfsd-fuse",
        "fuseblk",
        "nfs4",
        "cifs",
        "overlay",
    ] {
        assert!(search::may_alias(filesystem), "{filesystem}");
    }
    for filesystem in ["ext4", "btrfs", "xfs", "vfat", "zfs"] {
        assert!(!search::may_alias(filesystem), "{filesystem}");
    }
}

mod moving {
    use std::sync::Arc;

    use okapi_platform::Trash;

    use super::*;
    use crate::duplicates::trash::trash_dated;

    const DATE: &str = "2026-11-14T23:30:00";

    /// A sandbox: folders searched below `root`, the trash in `data`.
    struct Sandbox {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        data: PathBuf,
    }

    fn sandbox() -> Sandbox {
        let temporary = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temporary.path()).unwrap();
        let data = base.join("share");
        fs::create_dir(&data).unwrap();
        Sandbox {
            _temporary: temporary,
            root: base.join("home"),
            data,
        }
    }

    impl Sandbox {
        fn copies(&self, names: &[&str], seed: u32) {
            for name in names {
                write(&self.root.join(name), &data(seed, 3));
            }
        }

        fn search(&self) -> DuplicateSearch {
            searched(&self.root)
        }

        fn bin(&self) -> Trash {
            Trash::in_folder(&self.data).unwrap()
        }

        fn trashed(&self) -> BTreeSet<String> {
            fs::read_dir(self.data.join("Trash/files"))
                .map(|entries| {
                    entries
                        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    /// The identifier of the copy at `name` below the root.
    fn id(search: &DuplicateSearch, sandbox: &Sandbox, name: &str) -> CopyId {
        let path = sandbox.root.join(name);
        search
            .report()
            .unwrap()
            .groups()
            .iter()
            .flat_map(Group::copies)
            .find(|copy| copy.path() == path)
            .unwrap()
            .id()
    }

    fn results(receipt: &TrashReceipt) -> Vec<TrashResult> {
        receipt
            .outcomes()
            .iter()
            .map(|outcome| outcome.result)
            .collect()
    }

    fn moved(search: &DuplicateSearch, sandbox: &Sandbox, names: &[&str]) -> Vec<TrashResult> {
        let ids: Vec<CopyId> = names.iter().map(|name| id(search, sandbox, name)).collect();
        let receipt = trash_dated(
            search.report().unwrap(),
            &ids,
            &sandbox.bin(),
            &Exclusions::none(),
            DATE,
        );
        results(&receipt)
    }

    const SIZE: u64 = 3 * BLOCK as u64;
    const TRASHED: TrashResult = TrashResult::Trashed { bytes: SIZE };

    #[test]
    fn copies_asked_for_go_to_the_trash_and_one_stays() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b", "c"], 20);
        let search = sandbox.search();
        let ids = [id(&search, &sandbox, "a"), id(&search, &sandbox, "b")];
        let receipt = trash_dated(
            search.report().unwrap(),
            &ids,
            &sandbox.bin(),
            &Exclusions::none(),
            DATE,
        );
        assert_eq!(results(&receipt), [TRASHED, TRASHED]);
        assert_eq!(receipt.trashed_bytes(), 2 * SIZE);
        assert_eq!(receipt.outcomes()[0].id, ids[0]);
        assert!(sandbox.root.join("c").exists());
        assert_eq!(
            sandbox.trashed(),
            BTreeSet::from(["a".to_owned(), "b".to_owned()])
        );
        let info = fs::read_to_string(sandbox.data.join("Trash/info/a.trashinfo")).unwrap();
        assert!(
            info.contains(&format!("Path={}/a\n", sandbox.root.display())),
            "{info}"
        );
        assert!(info.contains(&format!("DeletionDate={DATE}\n")), "{info}");
    }

    #[test]
    fn every_copy_of_a_group_asked_for_keeps_them_all() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b"], 21);
        sandbox.copies(&["x", "y", "z"], 22);
        let search = sandbox.search();
        // Two groups mixed: the whole of one, part of the other.
        assert_eq!(
            moved(&search, &sandbox, &["a", "x", "b"]),
            [
                TrashResult::Kept(KeepReason::LastCopy),
                TRASHED,
                TrashResult::Kept(KeepReason::LastCopy),
            ]
        );
        assert_eq!(sandbox.trashed(), BTreeSet::from(["x".to_owned()]));
    }

    #[test]
    fn a_keeper_is_never_a_copy_asked_for() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b", "c"], 23);
        let search = sandbox.search();
        // The only copy not asked for is gone: `a` and `b` would each have
        // the other as a keeper, were it taken among those asked for.
        fs::remove_file(sandbox.root.join("c")).unwrap();
        let kept = TrashResult::Kept(KeepReason::KeeperChanged);
        assert_eq!(moved(&search, &sandbox, &["a", "b"]), [kept, kept]);
        assert!(sandbox.trashed().is_empty());
    }

    #[test]
    fn a_keeper_written_since_is_passed_over_for_another() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b", "c"], 24);
        let search = sandbox.search();
        written_again(&sandbox.root.join("b"));
        assert_eq!(moved(&search, &sandbox, &["a"]), [TRASHED]);
    }

    #[test]
    fn a_copy_whose_every_keeper_was_written_since_stays() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b", "c"], 31);
        let search = sandbox.search();
        // `a` itself is untouched: only its keepers changed.
        written_again(&sandbox.root.join("b"));
        written_again(&sandbox.root.join("c"));
        assert_eq!(
            moved(&search, &sandbox, &["a"]),
            [TrashResult::Kept(KeepReason::KeeperChanged)]
        );
        assert!(sandbox.trashed().is_empty());
        assert!(sandbox.root.join("a").exists());
    }

    #[test]
    fn a_write_under_a_future_modification_date_is_seen() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b"], 32);
        // A camera with a clock ahead: the latest of the two dates stays
        // its modification date, whatever is written after.
        let ahead = std::time::SystemTime::now() + Duration::from_hours(24);
        let file = fs::File::options()
            .write(true)
            .open(sandbox.root.join("a"))
            .unwrap();
        file.set_modified(ahead).unwrap();
        let search = sandbox.search();
        written_again(&sandbox.root.join("a"));
        assert_eq!(
            moved(&search, &sandbox, &["a"]),
            [TrashResult::Kept(KeepReason::Changed)]
        );
    }

    #[test]
    fn a_keeper_of_several_names_still_keeps() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b"], 33);
        fs::hard_link(sandbox.root.join("b"), sandbox.data.join("other name")).unwrap();
        let search = sandbox.search();
        assert_eq!(moved(&search, &sandbox, &["a"]), [TRASHED]);
    }

    #[test]
    fn a_copy_written_replaced_or_removed_since_the_search_stays() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b", "c", "d"], 25);
        let search = sandbox.search();
        written_again(&sandbox.root.join("a"));
        // Another file of the same name and content.
        fs::remove_file(sandbox.root.join("b")).unwrap();
        // Past the tick of a coarse clock: the new file may reuse the inode,
        // only its change date tells it apart.
        std::thread::sleep(Duration::from_millis(30));
        write(&sandbox.root.join("b"), &data(25, 3));
        fs::remove_file(sandbox.root.join("c")).unwrap();
        assert_eq!(
            moved(&search, &sandbox, &["a", "b", "c"]),
            [
                TrashResult::Kept(KeepReason::Changed),
                TrashResult::Kept(KeepReason::Changed),
                TrashResult::Kept(KeepReason::Gone),
            ]
        );
        assert!(sandbox.trashed().is_empty());
    }

    #[test]
    fn a_link_put_on_the_way_is_never_followed() {
        let sandbox = sandbox();
        sandbox.copies(&["sub/a", "b"], 26);
        let search = sandbox.search();
        // `sub` moved away and a link to it put in its place: the file is
        // reachable through the link, but never through one.
        let moved_away = sandbox.data.join("sub");
        fs::rename(sandbox.root.join("sub"), &moved_away).unwrap();
        symlink(&moved_away, sandbox.root.join("sub")).unwrap();
        assert_eq!(
            moved(&search, &sandbox, &["sub/a"]),
            [TrashResult::Kept(KeepReason::Changed)]
        );
        assert!(moved_away.join("a").exists());
    }

    #[test]
    fn an_identifier_of_another_report_moves_nothing() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b"], 27);
        let first = sandbox.search();
        let second = sandbox.search();
        let foreign = id(&first, &sandbox, "a");
        let receipt = trash_dated(
            second.report().unwrap(),
            &[foreign],
            &sandbox.bin(),
            &Exclusions::none(),
            DATE,
        );
        assert_eq!(
            results(&receipt),
            [TrashResult::Kept(KeepReason::UnknownCopy)]
        );
        assert!(sandbox.trashed().is_empty());
    }

    #[test]
    fn an_identifier_given_twice_is_moved_once() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b"], 28);
        let search = sandbox.search();
        assert_eq!(moved(&search, &sandbox, &["a", "a"]), [TRASHED]);
    }

    #[test]
    fn a_copy_of_several_names_or_excluded_stays() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b", "c"], 29);
        fs::hard_link(sandbox.root.join("a"), sandbox.data.join("other name")).unwrap();
        let search = sandbox.search();
        assert_eq!(
            moved(&search, &sandbox, &["a"]),
            [TrashResult::Kept(KeepReason::OtherNames)]
        );
        let excluded = Exclusions::new([sandbox.root.join("b")]).unwrap();
        let receipt = trash_dated(
            search.report().unwrap(),
            &[id(&search, &sandbox, "b")],
            &sandbox.bin(),
            &excluded,
            DATE,
        );
        assert_eq!(results(&receipt), [TrashResult::Kept(KeepReason::Excluded)]);
        assert!(sandbox.trashed().is_empty());
    }

    #[test]
    fn the_copy_moved_first_is_no_keeper_for_the_next() {
        let sandbox = sandbox();
        sandbox.copies(&["a", "b"], 39);
        let search = sandbox.search();
        assert_eq!(moved(&search, &sandbox, &["a"]), [TRASHED]);
        assert_eq!(
            moved(&search, &sandbox, &["b"]),
            [TrashResult::Kept(KeepReason::KeeperChanged)]
        );
        assert!(sandbox.root.join("b").exists());
    }

    #[test]
    fn copies_of_two_roots_are_reached_each_from_its_own() {
        let sandbox = sandbox();
        let other = sandbox.data.join("other");
        write(&sandbox.root.join("a"), &data(40, 3));
        write(&other.join("deep/b"), &data(40, 3));
        write(&other.join("c"), &data(40, 3));
        let search = searched_with(&[sandbox.root.clone(), other.clone()], options(1));
        let report = search.report().unwrap();
        let id_of = |path: &Path| {
            report.groups()[0]
                .copies()
                .iter()
                .find(|copy| copy.path() == path)
                .unwrap()
                .id()
        };
        let receipt = trash_dated(
            report,
            &[id_of(&other.join("deep/b"))],
            &sandbox.bin(),
            &Exclusions::none(),
            DATE,
        );
        assert_eq!(results(&receipt), [TRASHED]);
        // Its last keeper, in the other root, written since: `a` stays.
        written_again(&other.join("c"));
        let receipt = trash_dated(
            report,
            &[id_of(&sandbox.root.join("a"))],
            &sandbox.bin(),
            &Exclusions::none(),
            DATE,
        );
        assert_eq!(
            results(&receipt),
            [TrashResult::Kept(KeepReason::KeeperChanged)]
        );
    }

    #[test]
    fn a_copy_already_in_the_trash_keeps_nothing() {
        let sandbox = sandbox();
        write(&sandbox.root.join("a"), &data(41, 3));
        let trashed = sandbox.data.join("Trash/files");
        write(&trashed.join("b"), &data(41, 3));
        // The trash searched on purpose, as a folder of its own.
        let search = searched_with(&[sandbox.root.clone(), trashed], options(1));
        assert_eq!(
            moved(&search, &sandbox, &["a"]),
            [TrashResult::Kept(KeepReason::NoSureKeeper)]
        );
        assert!(sandbox.root.join("a").exists());
    }

    #[test]
    fn two_movings_never_run_at_once() {
        let sandbox = Arc::new(sandbox());
        sandbox.copies(&["a", "b"], 30);
        let search = Arc::new(sandbox.search());
        let held = crate::duplicates::trash::MOVING.lock().unwrap();
        let mover = {
            let (sandbox, search) = (Arc::clone(&sandbox), Arc::clone(&search));
            std::thread::spawn(move || moved(&search, &sandbox, &["a"]))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(sandbox.root.join("a").exists(), "it waits for the other");
        drop(held);
        assert_eq!(mover.join().unwrap(), [TRASHED]);
    }
}
