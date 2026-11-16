//! Searches of real temporary folders, and moves to a trash of their own.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use okapi_engine::Exclusions;
use okapi_platform::{Trash, effective_user};

use super::trash::trash_dated;
use super::*;
use crate::listing::Owners;

/// How long a test waits for a search: a deadlock fails, it never hangs.
const WAIT: Duration = Duration::from_secs(20);
const BLOCK: usize = 4096;
/// The smallest file listed in these tests, on disk.
const MIN: u64 = 2 * BLOCK as u64;
const DATE: &str = "2026-11-16T10:00:00";

/// `blocks` blocks of data that does not repeat (no filesystem packs it),
/// from `seed`.
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

/// A file of `length` bytes holding only its first `blocks` blocks.
fn sparse(path: &Path, blocks: usize, length: u64) {
    write(path, &data(7, blocks));
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(length)
        .unwrap();
}

fn options() -> SearchOptions {
    SearchOptions {
        min_size: MIN,
        exclusions: Exclusions::none(),
    }
}

/// The table of mounts of a machine without any below the roots.
fn no_mounts(_root: &Path) -> (BTreeSet<PathBuf>, Option<String>) {
    (BTreeSet::new(), None)
}

fn completed(search: LargeFileSearch) -> LargeFileSearch {
    let started = Instant::now();
    assert_eq!(search.wait(WAIT), Some(Outcome::Complete));
    assert!(started.elapsed() < WAIT / 4, "{:?}", started.elapsed());
    search
}

fn searched_with(roots: &[PathBuf], options: SearchOptions) -> LargeFileSearch {
    completed(LargeFileSearch::start(roots, options).unwrap())
}

fn searched(root: &Path) -> LargeFileSearch {
    searched_with(&[root.to_path_buf()], options())
}

/// The paths listed, below `root`, in the report's order.
fn listed(search: &LargeFileSearch, root: &Path) -> Vec<String> {
    let root = fs::canonicalize(root).unwrap();
    search
        .report()
        .unwrap()
        .files()
        .iter()
        .map(|file| {
            file.path()
                .strip_prefix(&root)
                .unwrap()
                .display()
                .to_string()
        })
        .collect()
}

#[test]
fn the_largest_on_disk_come_first_then_by_path() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join("a"), &data(1, 3));
    write(&root.join("sub/b"), &data(2, 5));
    write(&root.join("d"), &data(3, 4));
    write(&root.join("c"), &data(4, 4));
    let search = searched(root);
    assert_eq!(listed(&search, root), ["sub/b", "c", "d", "a"]);
    let report = search.report().unwrap();
    let sizes: Vec<u64> = report.files().iter().map(LargeFile::on_disk).collect();
    assert!(sizes.is_sorted_by(|left, right| left >= right), "{sizes:?}");
    assert_eq!(report.total(), sizes.iter().sum::<u64>());
}

#[test]
fn a_sparse_file_is_measured_by_what_it_takes_on_disk() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    // Long, but nothing on disk.
    sparse(&root.join("hollow"), 0, 64 * MIN);
    // Long, three blocks on disk.
    sparse(&root.join("holed"), 3, 64 * MIN);
    write(&root.join("full"), &data(5, 3));
    let search = searched(root);
    assert_eq!(listed(&search, root), ["full", "holed"]);
    let report = search.report().unwrap();
    let holed = &report.files()[1];
    assert_eq!(holed.size(), 64 * MIN);
    assert!(holed.on_disk() < holed.size(), "{}", holed.on_disk());
    let full = &report.files()[0];
    assert_eq!(report.total(), full.on_disk() + holed.on_disk());
    assert_eq!(report.stats().below_minimum, 1);
}

#[test]
fn two_names_of_one_file_are_one_line_that_frees_nothing() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join("a"), &data(6, 3));
    fs::hard_link(root.join("a"), root.join("b")).unwrap();
    let search = searched(root);
    let report = search.report().unwrap();
    assert_eq!(report.files().len(), 1);
    assert!(!report.files()[0].frees());
    assert_eq!(report.files()[0].links(), 2);
    assert_eq!(report.stats().other_names, 1);
    assert_eq!(search.progress().found, 1);
}

#[test]
fn hidden_folders_projects_dependencies_trashes_exclusions_and_links_are_left_out() {
    let temporary = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temporary.path()).unwrap();
    let root = base.join("home");
    let elsewhere = base.join("elsewhere");
    for folder in [".hidden", "node_modules", "project", "Trash", "excluded"] {
        write(&root.join(folder).join("big"), &data(8, 3));
    }
    fs::create_dir(root.join("project/.git")).unwrap();
    write(&elsewhere.join("big"), &data(9, 3));
    symlink(&elsewhere, root.join("link")).unwrap();
    symlink(elsewhere.join("big"), root.join("file_link")).unwrap();
    write(&root.join("kept"), &data(10, 3));
    let options = SearchOptions {
        min_size: MIN,
        exclusions: Exclusions::new([root.join("excluded")]).unwrap(),
    };
    let search = searched_with(std::slice::from_ref(&root), options);
    assert_eq!(listed(&search, &root), ["kept"]);
    let stats = search.report().unwrap().stats();
    assert_eq!(
        (
            stats.hidden_directories,
            stats.dependency_directories,
            stats.projects,
            stats.trash_directories,
            stats.excluded,
        ),
        (1, 1, 1, 1, 1)
    );
}

#[test]
fn a_file_of_another_user_is_never_listed_and_counted() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join("a"), &data(11, 3));
    write(&root.join("b"), &data(12, 3));
    // The user's files, as `start` keeps them.
    assert_eq!(searched(root).report().unwrap().files().len(), 2);
    // The same files, for someone else: none is the user's.
    let other = effective_user().unwrap() + 1;
    let search = completed(
        LargeFileSearch::start_with(
            &[root.to_path_buf()],
            options(),
            no_mounts,
            Owners::Only(Some(other)),
        )
        .unwrap(),
    );
    let report = search.report().unwrap();
    assert_eq!(report.files(), []);
    assert_eq!(report.stats().other_owners, 2);
}

#[test]
fn a_search_started_keeps_only_the_user_s_files() {
    let user = effective_user().unwrap();
    // The system's files are root's: only another user meets them as such.
    if user == 0 {
        return;
    }
    let search = searched_with(
        &[PathBuf::from("/etc")],
        SearchOptions {
            min_size: 1,
            exclusions: Exclusions::none(),
        },
    );
    let report = search.report().unwrap();
    assert!(report.stats().other_owners > 0, "{:?}", report.stats());
    assert!(
        report.files().iter().all(|file| file.seen().owner == user),
        "{:?}",
        report.files()
    );
}

#[test]
fn a_cancelled_search_gives_no_report() {
    fn slow(_root: &Path) -> (BTreeSet<PathBuf>, Option<String>) {
        std::thread::sleep(Duration::from_millis(200));
        (BTreeSet::new(), None)
    }
    let temporary = tempfile::tempdir().unwrap();
    write(&temporary.path().join("a"), &data(13, 3));
    let search = LargeFileSearch::start_with(
        &[temporary.path().to_path_buf()],
        options(),
        slow,
        Owners::All,
    )
    .unwrap();
    search.cancel();
    assert_eq!(search.wait(WAIT), Some(Outcome::Cancelled));
    assert!(search.report().is_none());
}

#[test]
fn a_search_that_panics_ends_failed_without_a_report() {
    let temporary = tempfile::tempdir().unwrap();
    let broken = |_: &Path| -> (BTreeSet<PathBuf>, Option<String>) { panic!("broken table") };
    let search = LargeFileSearch::start_with(
        &[temporary.path().to_path_buf()],
        options(),
        broken,
        Owners::All,
    )
    .unwrap();
    assert_eq!(search.wait(WAIT), Some(Outcome::Failed));
    assert!(search.report().is_none());
}

#[test]
fn the_progress_ends_on_the_files_listed_and_found() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join("a"), &data(14, 3));
    write(&root.join("b"), &data(15, 3));
    write(&root.join("small"), b"small");
    let search = searched(root);
    let report = search.report().unwrap();
    assert_eq!(
        search.progress(),
        Progress {
            files: report.stats().files,
            found: 2,
        }
    );
    assert_eq!(report.stats().files, 3);
}

#[test]
fn a_file_is_found_only_in_the_report_that_gave_it_with_its_kind() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(&root.join("film.MKV"), &data(16, 3));
    write(&root.join("larger"), &data(17, 4));
    let first = searched(root);
    let second = searched(root);
    let file = &first.report().unwrap().files()[1];
    assert_eq!(file.kind(), FileKind::Video);
    assert_eq!(first.report().unwrap().find(file.id()), Some(file));
    assert!(second.report().unwrap().find(file.id()).is_none());
}

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
    let root = base.join("home");
    fs::create_dir(&root).unwrap();
    Sandbox {
        _temporary: temporary,
        root,
        data,
    }
}

impl Sandbox {
    fn files(&self, names: &[&str]) {
        for (seed, name) in (100..).zip(names) {
            write(&self.root.join(name), &data(seed, 3));
        }
    }

    fn search(&self) -> LargeFileSearch {
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

    /// The identifier of the file at `name` below the root.
    fn id(&self, search: &LargeFileSearch, name: &str) -> FileId {
        let path = self.root.join(name);
        search
            .report()
            .unwrap()
            .files()
            .iter()
            .find(|file| file.path() == path)
            .unwrap()
            .id()
    }

    fn moved_ids(
        &self,
        search: &LargeFileSearch,
        ids: &[FileId],
        exclusions: &Exclusions,
    ) -> TrashReceipt {
        trash_dated(
            search.report().unwrap(),
            ids,
            &self.bin(),
            exclusions,
            DATE,
            None,
        )
    }

    fn moved(&self, search: &LargeFileSearch, names: &[&str]) -> Vec<TrashResult> {
        let ids: Vec<FileId> = names.iter().map(|name| self.id(search, name)).collect();
        results(&self.moved_ids(search, &ids, &Exclusions::none()))
    }
}

fn results(receipt: &TrashReceipt) -> Vec<TrashResult> {
    receipt
        .outcomes()
        .iter()
        .map(|outcome| outcome.result)
        .collect()
}

fn kept(reason: KeepReason) -> TrashResult {
    TrashResult::Kept(reason)
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
fn files_asked_for_go_to_the_trash_counted_on_disk() {
    let sandbox = sandbox();
    sandbox.files(&["a", "b", "c"]);
    sparse(&sandbox.root.join("holed"), 3, 64 * MIN);
    let search = sandbox.search();
    let report = search.report().unwrap();
    let on_disk = |name: &str| report.find(sandbox.id(&search, name)).unwrap().on_disk();
    let ids = [sandbox.id(&search, "a"), sandbox.id(&search, "holed")];
    let receipt = sandbox.moved_ids(&search, &ids, &Exclusions::none());
    assert_eq!(
        results(&receipt),
        [
            TrashResult::Trashed {
                bytes: on_disk("a")
            },
            TrashResult::Trashed {
                bytes: on_disk("holed")
            },
        ]
    );
    assert!(
        on_disk("holed") < 64 * MIN,
        "counted on disk, not its length"
    );
    assert_eq!(receipt.trashed_bytes(), on_disk("a") + on_disk("holed"));
    assert_eq!(receipt.outcomes()[0].id, ids[0]);
    assert!(sandbox.root.join("b").exists());
    assert_eq!(
        sandbox.trashed(),
        BTreeSet::from(["a".to_owned(), "holed".to_owned()])
    );
    let info = fs::read_to_string(sandbox.data.join("Trash/info/a.trashinfo")).unwrap();
    assert!(
        info.contains(&format!("Path={}/a\n", sandbox.root.display())),
        "{info}"
    );
    assert!(info.contains(&format!("DeletionDate={DATE}\n")), "{info}");
}

#[test]
fn a_file_written_replaced_removed_or_given_a_name_since_the_search_stays() {
    let sandbox = sandbox();
    sandbox.files(&["written", "replaced", "removed", "linked"]);
    let search = sandbox.search();
    written_again(&sandbox.root.join("written"));
    fs::remove_file(sandbox.root.join("replaced")).unwrap();
    write(&sandbox.root.join("replaced"), &data(1, 3));
    fs::remove_file(sandbox.root.join("removed")).unwrap();
    fs::hard_link(sandbox.root.join("linked"), sandbox.root.join("other_name")).unwrap();
    assert_eq!(
        sandbox.moved(&search, &["written", "replaced", "removed", "linked"]),
        [
            kept(KeepReason::Changed),
            kept(KeepReason::Changed),
            kept(KeepReason::Gone),
            kept(KeepReason::Changed),
        ]
    );
    assert!(sandbox.trashed().is_empty());
}

#[test]
fn a_file_written_between_its_check_and_its_move_is_put_back() {
    fn grown(path: &Path) {
        use std::io::Write as _;
        let mut file = fs::File::options().append(true).open(path).unwrap();
        file.write_all(b"more").unwrap();
    }
    let sandbox = sandbox();
    sandbox.files(&["a"]);
    let search = sandbox.search();
    let receipt = trash_dated(
        search.report().unwrap(),
        &[sandbox.id(&search, "a")],
        &sandbox.bin(),
        &Exclusions::none(),
        DATE,
        Some(grown),
    );
    assert_eq!(results(&receipt), [kept(KeepReason::Changed)]);
    assert!(sandbox.root.join("a").exists());
    assert!(sandbox.trashed().is_empty());
}

#[test]
fn a_file_the_system_refuses_to_read_again_is_refused_with_its_code() {
    use std::os::unix::fs::PermissionsExt as _;
    /// Gives the folder back its rights, whatever happens: the sandbox is
    /// removed after.
    struct Closed(PathBuf);
    impl Drop for Closed {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }
    let sandbox = sandbox();
    sandbox.files(&["sub/a"]);
    let search = sandbox.search();
    let closed = Closed(sandbox.root.join("sub"));
    fs::set_permissions(&closed.0, fs::Permissions::from_mode(0o000)).unwrap();
    // Root, or a process allowed to read through any permission, reads it.
    if fs::read_dir(&closed.0).is_ok() {
        return;
    }
    let results = sandbox.moved(&search, &["sub/a"]);
    drop(closed);
    assert_eq!(
        results,
        [kept(KeepReason::Refused(Refusal {
            kind: std::io::ErrorKind::PermissionDenied,
            code: Some(13),
        }))]
    );
}

#[test]
fn a_file_whose_folder_went_since_the_search_is_gone() {
    let sandbox = sandbox();
    sandbox.files(&["sub/a"]);
    let search = sandbox.search();
    fs::remove_dir_all(sandbox.root.join("sub")).unwrap();
    assert_eq!(sandbox.moved(&search, &["sub/a"]), [kept(KeepReason::Gone)]);
}

#[test]
fn a_link_put_on_the_way_is_never_followed() {
    let sandbox = sandbox();
    sandbox.files(&["sub/a"]);
    let search = sandbox.search();
    let elsewhere = sandbox.data.join("elsewhere");
    fs::rename(sandbox.root.join("sub"), &elsewhere).unwrap();
    symlink(&elsewhere, sandbox.root.join("sub")).unwrap();
    assert_eq!(
        sandbox.moved(&search, &["sub/a"]),
        [kept(KeepReason::Changed)]
    );
    assert!(elsewhere.join("a").exists());
    assert!(sandbox.trashed().is_empty());
}

#[test]
fn a_file_of_several_names_excluded_unknown_or_asked_twice_moves_as_it_should() {
    let sandbox = sandbox();
    sandbox.files(&["named", "excluded", "twice"]);
    fs::hard_link(sandbox.root.join("named"), sandbox.data.join("named")).unwrap();
    let search = sandbox.search();
    let other = sandbox.search();
    let exclusions = Exclusions::new([sandbox.root.join("excluded")]).unwrap();
    let twice = sandbox.id(&search, "twice");
    let receipt = sandbox.moved_ids(
        &search,
        &[
            sandbox.id(&search, "named"),
            sandbox.id(&search, "excluded"),
            sandbox.id(&other, "twice"),
            twice,
            twice,
        ],
        &exclusions,
    );
    let bytes = search.report().unwrap().find(twice).unwrap().on_disk();
    assert_eq!(
        results(&receipt),
        [
            kept(KeepReason::OtherNames),
            kept(KeepReason::Excluded),
            kept(KeepReason::UnknownFile),
            TrashResult::Trashed { bytes },
        ]
    );
    assert_eq!(sandbox.trashed(), BTreeSet::from(["twice".to_owned()]));
}

#[test]
fn a_file_in_a_folder_excluded_under_another_path_stays() {
    let sandbox = sandbox();
    sandbox.files(&["sub/a"]);
    let search = sandbox.search();
    // The folder excluded where it lies now, then back where it was found:
    // only its identity tells it, as through a bind mount.
    let elsewhere = sandbox.data.join("elsewhere");
    fs::rename(sandbox.root.join("sub"), &elsewhere).unwrap();
    let excluded = Exclusions::new([elsewhere.clone()]).unwrap();
    fs::rename(&elsewhere, sandbox.root.join("sub")).unwrap();
    let receipt = sandbox.moved_ids(&search, &[sandbox.id(&search, "sub/a")], &excluded);
    assert_eq!(results(&receipt), [kept(KeepReason::Excluded)]);
    assert!(sandbox.trashed().is_empty());
}

#[test]
fn a_move_waits_for_another_tool_s() {
    let sandbox = Arc::new(sandbox());
    sandbox.files(&["a"]);
    let search = Arc::new(sandbox.search());
    let held = crate::trashing::one_at_a_time();
    let mover = {
        let (sandbox, search) = (Arc::clone(&sandbox), Arc::clone(&search));
        std::thread::spawn(move || sandbox.moved(&search, &["a"]))
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(sandbox.root.join("a").exists(), "it waits for the other");
    drop(held);
    assert!(matches!(
        mover.join().unwrap()[..],
        [TrashResult::Trashed { .. }]
    ));
}
