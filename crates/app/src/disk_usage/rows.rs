//! What the screen shows of a folder, decided without Slint: its lines, in
//! the order they stay in while the user points at one, the small ones
//! gathered, the sizes in shares and bars, and whether the space the
//! analysis could not see can be told.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::Path;

use okapi_platform::{Device, Mount, MountClass, MountList};
use okapi_tools::disk_usage::{Entry, FileEntry, FileKind, NodeId, NodeState};

/// How many readings of the screen (every half second) between two sorts
/// while sizes change: two seconds.
pub(crate) const SORT_EVERY_TICKS: u64 = 4;
/// Below this share of the folder, in percents, a line joins « Autres ».
const SMALL_SHARE: u64 = 1;
/// How many small lines at least make « Autres »: one stays in its place.
const SMALLEST_GROUP: usize = 2;
const PERCENT: u64 = 100;
/// A file is said sparse when it announces this much more than it takes,
/// and more than twice as much: compression alone never makes one.
const SPARSE_GAP: u64 = 1_000_000;
const SPARSE_RATIO: u64 = 2;

/// What a line stands for, the same from one reading to the next.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Key {
    Folder(NodeId),
    File(OsString),
}

/// What a line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    Folder,
    File,
    Link,
    Other,
}

/// What a line says beside its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineState {
    /// Read whole.
    Read,
    /// Read, but something below could not be, or not yet: « au moins ».
    Partial,
    /// Not read yet: its size grows.
    Pending,
    /// Could not be opened: no size; denied, it needs the administrator's
    /// rights.
    Unreadable { denied: bool },
    /// Changed while it was read: no size.
    Changed,
    /// Another disk, or a part of this one mounted apart: never entered.
    OtherMount,
    /// A name of a file counted in the folder `NodeId`: no size here.
    CountedElsewhere(NodeId),
}

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) key: Key,
    pub(crate) name: OsString,
    pub(crate) kind: LineKind,
    pub(crate) state: LineState,
    /// On disk.
    pub(crate) allocated: u64,
    /// As its length says.
    pub(crate) apparent: u64,
    pub(crate) files: u64,
}

impl Line {
    /// Whether it has a size of its own, to sort and to share.
    pub(crate) const fn is_sized(&self) -> bool {
        matches!(self.state, LineState::Read | LineState::Partial)
    }

    /// Whether it is a file that announces far more than it takes.
    pub(crate) const fn is_sparse(&self) -> bool {
        !matches!(self.kind, LineKind::Folder)
            && self.apparent > self.allocated.saturating_add(SPARSE_GAP)
            && self.apparent > self.allocated.saturating_mul(SPARSE_RATIO)
    }
}

/// The line of a subfolder; none for one gone since it was listed.
fn folder_line(entry: &Entry) -> Option<Line> {
    let state = match entry.state {
        NodeState::Gone => return None,
        NodeState::Pending => LineState::Pending,
        NodeState::Read if entry.partial => LineState::Partial,
        NodeState::Read => LineState::Read,
        NodeState::Unreadable(kind) => LineState::Unreadable {
            denied: kind == std::io::ErrorKind::PermissionDenied,
        },
        NodeState::Changed => LineState::Changed,
        NodeState::OtherMount => LineState::OtherMount,
    };
    Some(Line {
        key: Key::Folder(entry.id),
        name: entry.name.clone(),
        kind: LineKind::Folder,
        state,
        allocated: entry.total.allocated,
        apparent: entry.total.apparent,
        files: entry.total.files,
    })
}

fn file_line(file: &FileEntry) -> Line {
    Line {
        key: Key::File(file.name.clone()),
        name: file.name.clone(),
        kind: match file.kind {
            FileKind::File => LineKind::File,
            FileKind::Link => LineKind::Link,
            FileKind::Other => LineKind::Other,
        },
        state: file
            .counted_in
            .map_or(LineState::Read, LineState::CountedElsewhere),
        allocated: file.allocated,
        apparent: file.apparent,
        files: 1,
    }
}

/// The lines of a folder: its subfolders and its files.
pub(crate) fn lines(children: &[Entry], files: &[FileEntry]) -> Vec<Line> {
    children
        .iter()
        .filter_map(folder_line)
        .chain(files.iter().map(file_line))
        .collect()
}

/// What the pointer or the keyboard's focus is on: it keeps its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held<'a> {
    Nothing,
    Line(&'a Key),
    /// « Autres »: what it holds stays.
    Others,
}

/// The lines as shown, in groups.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Arranged {
    /// With a size, in their order.
    pub(crate) lines: Vec<Line>,
    /// « Autres »: each under one percent of the folder when gathered.
    pub(crate) others: Vec<Line>,
    /// Not read yet, or come with a size while a line was held, by name:
    /// after « Autres », where they move nothing.
    pub(crate) pending: Vec<Line>,
    /// Without a size of their own, by name.
    pub(crate) sizeless: Vec<Line>,
}

impl Arranged {
    /// The largest size among the lines with one: what a bar is measured
    /// against.
    pub(crate) fn largest(&self) -> u64 {
        self.lines
            .iter()
            .chain(&self.others)
            .map(|line| line.allocated)
            .max()
            .unwrap_or(0)
    }

    /// What « Autres » holds.
    pub(crate) fn others_size(&self) -> u64 {
        self.others.iter().map(|line| line.allocated).sum()
    }
}

/// The order the lines are shown in: sorted by size at the first showing of
/// a folder, when [`Order::sort_next`] says so, and otherwise at most every
/// [`SORT_EVERY_TICKS`] readings, « Autres » made again with each sort. A
/// line held (pointed at, or with the focus) keeps its side of « Autres »
/// and its rank there while the others are sorted around it; while a line or
/// « Autres » is held, « Autres » keeps what it holds and the lines that come
/// wait after it; what was put off is owed until the hold ends.
#[derive(Debug, Default)]
pub(crate) struct Order {
    /// The keys of the lines with a size, as last shown, and those gathered.
    shown: Vec<Key>,
    gathered: HashSet<Key>,
    /// The reading the last sort was made at; none before the first.
    sorted_at: Option<u64>,
    /// The next arrangement sorts, whenever the last one was.
    sort_next: bool,
    /// A hold put off a sort, a gathering or new lines.
    owed: bool,
}

impl Order {
    /// `lines` in the order to show at reading `tick`, `total` the folder's.
    pub(crate) fn arrange(
        &mut self,
        lines: Vec<Line>,
        total: u64,
        held: Held<'_>,
        tick: u64,
    ) -> Arranged {
        let (mut sized, mut pending, mut sizeless) = split(lines);
        let first = self.sorted_at.is_none();
        let due = self.sort_next
            || self
                .sorted_at
                .is_none_or(|sorted_at| tick.saturating_sub(sorted_at) >= SORT_EVERY_TICKS);
        self.sort_next = false;
        // A line held that is gone holds nothing.
        let held_key = match held {
            Held::Line(key) if sized.iter().any(|line| line.key == *key) => Some(key),
            Held::Line(_) | Held::Nothing | Held::Others => None,
        };
        let holding = !first && (held_key.is_some() || held == Held::Others);
        // While held, the lines that come with a size wait after « Autres ».
        if holding {
            let known: HashSet<&Key> = self.shown.iter().collect();
            let (kept, come): (Vec<Line>, Vec<Line>) = sized
                .into_iter()
                .partition(|line| known.contains(&line.key));
            sized = kept;
            self.owed |= !come.is_empty();
            pending.extend(come);
        }
        self.owed = if holding {
            self.owed || due
        } else {
            self.owed && !due
        };
        let place = held_key
            .filter(|_| !first)
            .and_then(|key| self.place_of(key));
        let held_line = place.filter(|_| due).and_then(|_| {
            let index = sized.iter().position(|line| Some(&line.key) == held_key)?;
            Some(sized.remove(index))
        });
        self.order(&mut sized, due, tick);
        let mut gathered: Vec<bool> = if due && !holding {
            gathering(&sized, total)
        } else {
            sized
                .iter()
                .map(|line| self.gathered.contains(&line.key))
                .collect()
        };
        if let (Some(line), Some((side, rank))) = (held_line, place) {
            let at = gathered
                .iter()
                .enumerate()
                .filter(|(_, gathered)| **gathered == side)
                .nth(rank)
                .map_or(sized.len(), |(index, _)| index);
            sized.insert(at, line);
            gathered.insert(at, side);
        }
        self.shown = sized.iter().map(|line| line.key.clone()).collect();
        let (others, lines): (Gathered, Gathered) = sized
            .into_iter()
            .zip(gathered)
            .partition(|(_, gathered)| *gathered);
        let others: Vec<Line> = others.into_iter().map(|(line, _)| line).collect();
        self.gathered = others.iter().map(|line| line.key.clone()).collect();
        pending.sort_by(|first, second| first.name.cmp(&second.name));
        sizeless.sort_by(|first, second| first.name.cmp(&second.name));
        Arranged {
            lines: lines.into_iter().map(|(line, _)| line).collect(),
            others,
            pending,
            sizeless,
        }
    }

    /// Where `key` showed: in « Autres » or not, and its rank there.
    fn place_of(&self, key: &Key) -> Option<(bool, usize)> {
        let side = self.gathered.contains(key);
        let rank = self
            .shown
            .iter()
            .filter(|shown| self.gathered.contains(*shown) == side)
            .position(|shown| shown == key)?;
        Some((side, rank))
    }

    /// Sorts `sized` by size when it is `due`; otherwise keeps the order
    /// shown, the new lines after it in the order given.
    fn order(&mut self, sized: &mut [Line], due: bool, tick: u64) {
        if due {
            sized.sort_by(|first, second| {
                second
                    .allocated
                    .cmp(&first.allocated)
                    .then_with(|| first.name.cmp(&second.name))
            });
            self.sorted_at = Some(tick);
        } else {
            let ranks: HashMap<&Key, usize> = self
                .shown
                .iter()
                .enumerate()
                .map(|(rank, key)| (key, rank))
                .collect();
            sized.sort_by_key(|line| ranks.get(&line.key).copied().unwrap_or(usize::MAX));
        }
    }

    /// Whether a hold put something off: to be done once it ends.
    pub(crate) const fn owed(&self) -> bool {
        self.owed
    }

    /// Sorts at the next arrangement, a line held kept in its place: new
    /// lines came (the files of the folder, the end of the analysis).
    pub(crate) const fn sort_next(&mut self) {
        self.sort_next = true;
    }

    /// Forgets the order: the next lines, another folder's, are sorted at
    /// once.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The lines with a size, those not read yet, those without a size.
fn split(lines: Vec<Line>) -> (Vec<Line>, Vec<Line>, Vec<Line>) {
    let mut sized = Vec::new();
    let mut pending = Vec::new();
    let mut sizeless = Vec::new();
    for line in lines {
        if line.is_sized() {
            sized.push(line);
        } else if line.state == LineState::Pending {
            pending.push(line);
        } else {
            sizeless.push(line);
        }
    }
    (sized, pending, sizeless)
}

/// Which of `sized` « Autres » holds: each under one percent, two at least.
fn gathering(sized: &[Line], total: u64) -> Vec<bool> {
    let small: Vec<bool> = sized
        .iter()
        .map(|line| is_small(line.allocated, total))
        .collect();
    let enough = small.iter().filter(|small| **small).count() >= SMALLEST_GROUP;
    small.into_iter().map(|small| small && enough).collect()
}

/// Lines, each with whether it is gathered in « Autres ».
type Gathered = Vec<(Line, bool)>;

/// Whether `part` is under one percent of `whole`.
fn is_small(part: u64, whole: u64) -> bool {
    u128::from(part) * u128::from(PERCENT) < u128::from(whole) * u128::from(SMALL_SHARE)
}

/// The share of `whole` that `part` is, in whole percents, cut: a line under
/// one percent never reads « 1 % ». 0 of nothing.
pub(crate) fn share(part: u64, whole: u64) -> u32 {
    if whole == 0 {
        return 0;
    }
    let percents = u128::from(part) * u128::from(PERCENT) / u128::from(whole);
    u32::try_from(percents.min(u128::from(PERCENT))).unwrap_or(0)
}

/// How much of a bar `part` fills, against the largest line `largest`.
pub(crate) fn bar(part: u64, largest: u64) -> f32 {
    if largest == 0 {
        return 0.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a bar's length needs no more than a float's precision"
    )]
    let fraction = part as f64 / largest as f64;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a fraction between 0 and 1 fits a float"
    )]
    let fraction = fraction.clamp(0.0, 1.0) as f32;
    fraction
}

/// What the volume holds that the analysis did not see, its own reserve
/// included (`taken`: its total less what is available): never below 0
/// (files written while it read).
pub(crate) const fn unexplained(taken: u64, seen: u64) -> u64 {
    taken.saturating_sub(seen)
}

/// Whether what the analysis of `root` could not see can be told: `root` is
/// the mount point of a filesystem on a block device, mounted nowhere else
/// (no btrfs subvolume mounted apart, no bind mount); a ZFS dataset never
/// is (its space is its pool's).
pub(crate) fn unexplained_told(root: &Path, mounts: &MountList) -> bool {
    let Some(mount) = mounts.mounts.iter().find(|mount| mount.mount_point == root) else {
        return false;
    };
    mount.class == MountClass::Block
        && mounts
            .mounts
            .iter()
            .filter(|other| other.device == mount.device)
            .count()
            == 1
}

/// The disk `path` is on: the device of the deepest mount that holds it.
pub(crate) fn disk_of(path: &Path, mounts: &[Mount]) -> Option<Device> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.mount_point))
        .max_by_key(|mount| mount.mount_point.components().count())
        .map(|mount| mount.device)
}

/// The folders from the root down to the one shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Navigation {
    path: Vec<NodeId>,
}

impl Default for Navigation {
    fn default() -> Self {
        Self {
            path: vec![NodeId::ROOT],
        }
    }
}

impl Navigation {
    /// The folder shown.
    pub(crate) fn current(&self) -> NodeId {
        self.path.last().copied().unwrap_or(NodeId::ROOT)
    }

    /// The folders of the trail, the root first.
    pub(crate) fn trail(&self) -> &[NodeId] {
        &self.path
    }

    pub(crate) fn descend(&mut self, folder: NodeId) {
        self.path.push(folder);
    }

    /// Up one folder: the one left, `None` at the root.
    pub(crate) fn up(&mut self) -> Option<NodeId> {
        if self.path.len() > 1 {
            self.path.pop()
        } else {
            None
        }
    }

    /// Back to the folder at `index` of the trail; `false` when there is
    /// none.
    pub(crate) fn go_to(&mut self, index: usize) -> bool {
        if index < self.path.len() {
            self.path.truncate(index + 1);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use okapi_tools::disk_usage::Size;

    use super::*;

    fn line(name: &str, allocated: u64, state: LineState) -> Line {
        Line {
            key: Key::File(name.into()),
            name: name.into(),
            kind: LineKind::File,
            state,
            allocated,
            apparent: allocated,
            files: 1,
        }
    }

    fn names(lines: &[Line]) -> Vec<&str> {
        lines
            .iter()
            .map(|line| line.name.to_str().unwrap())
            .collect()
    }

    #[test]
    fn lines_are_sorted_by_size_then_name_in_their_groups() {
        let arranged = Order::default().arrange(
            vec![
                line("b", 5000, LineState::Read),
                line("tiny1", 10, LineState::Read),
                line("a", 5000, LineState::Read),
                line("big", 90_000, LineState::Partial),
                line("tiny2", 20, LineState::Read),
                line("later", 0, LineState::Pending),
                line("mount", 0, LineState::OtherMount),
                line("closed", 0, LineState::Unreadable { denied: true }),
                line("again", 0, LineState::Pending),
            ],
            100_030,
            Held::Nothing,
            0,
        );
        assert_eq!(names(&arranged.lines), ["big", "a", "b"]);
        assert_eq!(names(&arranged.others), ["tiny2", "tiny1"]);
        assert_eq!(names(&arranged.pending), ["again", "later"]);
        assert_eq!(names(&arranged.sizeless), ["closed", "mount"]);
        assert_eq!(arranged.others_size(), 30);
        assert_eq!(arranged.largest(), 90_000);
    }

    #[test]
    fn a_single_small_line_stays_in_its_place() {
        let arranged = Order::default().arrange(
            vec![
                line("big", 10_000, LineState::Read),
                line("tiny", 1, LineState::Read),
            ],
            10_001,
            Held::Nothing,
            0,
        );
        assert_eq!(names(&arranged.lines), ["big", "tiny"]);
        assert_eq!(arranged.others, Vec::<Line>::new());
    }

    fn grown() -> Vec<Line> {
        vec![
            line("a", 3, LineState::Read),
            line("b", 9, LineState::Read),
            line("c", 20, LineState::Read),
        ]
    }

    #[test]
    fn the_lines_are_sorted_at_first_then_every_two_seconds() {
        let mut order = Order::default();
        let first = order.arrange(
            vec![line("b", 2, LineState::Read), line("a", 3, LineState::Read)],
            5,
            Held::Line(&Key::File("b".into())),
            0,
        );
        assert_eq!(names(&first.lines), ["a", "b"], "sorted, held or not");
        let soon = order.arrange(grown(), 32, Held::Nothing, SORT_EVERY_TICKS - 1);
        assert_eq!(
            names(&soon.lines),
            ["a", "b", "c"],
            "too soon: new at the end"
        );
        let later = order.arrange(grown(), 32, Held::Nothing, SORT_EVERY_TICKS);
        assert_eq!(names(&later.lines), ["c", "b", "a"]);
        // Another folder, at the same reading: sorted at once.
        order.reset();
        let again = order.arrange(flipped(), 79, Held::Nothing, SORT_EVERY_TICKS);
        assert_eq!(names(&again.lines), ["a", "c", "b"]);
    }

    /// `grown`, `a` now the largest.
    fn flipped() -> Vec<Line> {
        vec![
            line("a", 50, LineState::Read),
            line("b", 9, LineState::Read),
            line("c", 20, LineState::Read),
        ]
    }

    #[test]
    fn new_lines_are_sorted_at_once_then_as_usual() {
        let mut order = Order::default();
        order.arrange(vec![line("a", 3, LineState::Read)], 3, Held::Nothing, 0);
        order.sort_next();
        let at_once = order.arrange(grown(), 32, Held::Nothing, 1);
        assert_eq!(
            names(&at_once.lines),
            ["c", "b", "a"],
            "new lines sorted at once"
        );
        let after = order.arrange(flipped(), 79, Held::Nothing, 2);
        assert_eq!(names(&after.lines), ["c", "b", "a"], "at once, then waits");
    }

    #[test]
    fn a_line_held_keeps_its_rank_the_others_sorted_around_it() {
        let mut order = Order::default();
        order.arrange(
            vec![
                line("a", 30, LineState::Read),
                line("b", 20, LineState::Read),
                line("c", 10, LineState::Read),
            ],
            60,
            Held::Nothing,
            0,
        );
        let around = order.arrange(
            vec![
                line("a", 1, LineState::Read),
                line("b", 20, LineState::Read),
                line("c", 40, LineState::Read),
            ],
            61,
            Held::Line(&Key::File("b".into())),
            SORT_EVERY_TICKS,
        );
        assert_eq!(names(&around.lines), ["c", "b", "a"], "b stays second");
        // Fewer lines than its rank: it comes last.
        let held = Key::File("a".into());
        let fewer = order.arrange(
            vec![
                line("a", 1, LineState::Read),
                line("c", 40, LineState::Read),
            ],
            41,
            Held::Line(&held),
            SORT_EVERY_TICKS * 2,
        );
        assert_eq!(names(&fewer.lines), ["c", "a"]);
        let gone = order.arrange(
            vec![
                line("b", 20, LineState::Read),
                line("c", 40, LineState::Read),
            ],
            60,
            Held::Line(&held),
            SORT_EVERY_TICKS * 3,
        );
        assert_eq!(names(&gone.lines), ["c", "b"], "a line gone holds nothing");
    }

    #[test]
    fn a_line_held_that_is_gone_keeps_nothing_in_others() {
        let mut order = Order::default();
        let first = order.arrange(
            vec![
                line("big", 1000, LineState::Read),
                line("w", 50, LineState::Read),
                line("x", 50, LineState::Read),
                line("y", 1, LineState::Read),
                line("z", 2, LineState::Read),
            ],
            1103,
            Held::Nothing,
            0,
        );
        assert_eq!(names(&first.others), ["z", "y"]);
        let gone = order.arrange(
            vec![
                line("big", 9000, LineState::Read),
                line("w", 50, LineState::Read),
                line("y", 1, LineState::Read),
                line("z", 2, LineState::Read),
            ],
            9053,
            Held::Line(&Key::File("x".into())),
            SORT_EVERY_TICKS,
        );
        assert_eq!(names(&gone.others), ["w", "z", "y"], "w gathered");
    }

    #[test]
    fn a_line_held_at_the_first_showing_leaves_the_small_ones_gathered() {
        let arranged = Order::default().arrange(
            vec![
                line("big", 1000, LineState::Read),
                line("y", 1, LineState::Read),
                line("z", 2, LineState::Read),
            ],
            1003,
            Held::Line(&Key::File("big".into())),
            0,
        );
        assert_eq!(names(&arranged.others), ["z", "y"]);
    }

    #[test]
    fn a_line_held_keeps_its_rank_among_the_lines_shown() {
        let mut order = Order::default();
        let first = order.arrange(
            vec![
                line("a", 1000, LineState::Read),
                line("b", 500, LineState::Read),
                line("h", 400, LineState::Read),
                line("s1", 1, LineState::Read),
                line("s2", 1, LineState::Read),
            ],
            1902,
            Held::Nothing,
            0,
        );
        assert_eq!(names(&first.lines), ["a", "b", "h"]);
        // A line of « Autres » grows: it stays there while h is held.
        let held = Key::File("h".into());
        let grown = order.arrange(
            vec![
                line("a", 1000, LineState::Read),
                line("b", 500, LineState::Read),
                line("h", 400, LineState::Read),
                line("s1", 600, LineState::Partial),
                line("s2", 1, LineState::Read),
            ],
            2501,
            Held::Line(&held),
            SORT_EVERY_TICKS,
        );
        assert_eq!(names(&grown.lines), ["a", "b", "h"]);
        assert_eq!(
            names(&grown.others),
            ["s1", "s2"],
            "made again once h is free"
        );
    }

    #[test]
    fn a_line_held_in_others_stays_there_however_it_grows() {
        let mut order = Order::default();
        let start = vec![
            line("big", 1000, LineState::Read),
            line("x", 50, LineState::Read),
            line("y", 1, LineState::Read),
            line("z", 2, LineState::Read),
        ];
        order.arrange(start, 1053, Held::Nothing, 0);
        let held = Key::File("y".into());
        let grown = vec![
            line("big", 1000, LineState::Read),
            line("x", 50, LineState::Read),
            line("y", 500, LineState::Partial),
            line("z", 2, LineState::Read),
        ];
        for tick in [1, SORT_EVERY_TICKS] {
            let kept = order.arrange(grown.clone(), 1552, Held::Line(&held), tick);
            assert!(names(&kept.others).contains(&"y"), "at {tick}");
        }
    }

    #[test]
    fn a_line_held_keeps_its_index_when_lines_above_would_join_others() {
        let mut order = Order::default();
        let first = order.arrange(
            vec![
                line("a", 1000, LineState::Read),
                line("b", 20, LineState::Read),
                line("h", 15, LineState::Read),
                line("s1", 1, LineState::Read),
                line("s2", 1, LineState::Read),
            ],
            1037,
            Held::Nothing,
            0,
        );
        assert_eq!(names(&first.lines), ["a", "b", "h"]);
        let held = Key::File("h".into());
        let grown = || {
            vec![
                line("a", 2500, LineState::Partial),
                line("b", 20, LineState::Read),
                line("h", 15, LineState::Read),
                line("s1", 1, LineState::Read),
                line("s2", 1, LineState::Read),
            ]
        };
        let soon = order.arrange(grown(), 2537, Held::Nothing, 1);
        assert_eq!(names(&soon.lines), ["a", "b", "h"], "too soon to gather");
        for tick in [2, SORT_EVERY_TICKS] {
            let kept = order.arrange(grown(), 2537, Held::Line(&held), tick);
            assert_eq!(names(&kept.lines), ["a", "b", "h"], "at {tick}");
        }
        let freed = order.arrange(grown(), 2537, Held::Nothing, SORT_EVERY_TICKS * 2);
        assert_eq!(names(&freed.lines), ["a"]);
    }

    #[test]
    fn a_line_held_in_others_keeps_its_index_there() {
        let mut order = Order::default();
        let first = order.arrange(
            vec![
                line("big", 1000, LineState::Read),
                line("x", 50, LineState::Read),
                line("w", 3, LineState::Read),
                line("y", 2, LineState::Read),
                line("z", 1, LineState::Read),
            ],
            1056,
            Held::Nothing,
            0,
        );
        assert_eq!(names(&first.others), ["w", "y", "z"]);
        let held = Key::File("y".into());
        let grown = order.arrange(
            vec![
                line("big", 1000, LineState::Read),
                line("x", 50, LineState::Read),
                line("w", 3, LineState::Read),
                line("y", 9, LineState::Partial),
                line("z", 1, LineState::Read),
            ],
            1063,
            Held::Line(&held),
            SORT_EVERY_TICKS,
        );
        assert_eq!(names(&grown.others), ["w", "y", "z"]);
    }

    #[test]
    fn lines_that_come_while_others_is_held_wait_after_it() {
        let mut order = Order::default();
        let first = order.arrange(
            vec![
                line("big", 1000, LineState::Read),
                line("y", 1, LineState::Read),
                line("z", 2, LineState::Read),
                line("p", 0, LineState::Pending),
            ],
            1003,
            Held::Nothing,
            0,
        );
        assert_eq!(names(&first.lines), ["big"]);
        let read = || {
            vec![
                line("big", 1000, LineState::Read),
                line("y", 1, LineState::Read),
                line("z", 2, LineState::Read),
                line("p", 500, LineState::Read),
            ]
        };
        let held = order.arrange(read(), 1503, Held::Others, 1);
        assert_eq!(names(&held.lines), ["big"], "« Autres » stays second");
        assert_eq!(names(&held.pending), ["p"]);
        assert!(order.owed());
        order.sort_next();
        let freed = order.arrange(read(), 1503, Held::Nothing, 2);
        assert_eq!(names(&freed.lines), ["big", "p"]);
        assert!(!order.owed());
    }

    #[test]
    fn others_pointed_at_keeps_what_it_holds() {
        let mut order = Order::default();
        let start = vec![
            line("big", 1000, LineState::Read),
            line("x", 50, LineState::Read),
            line("y", 1, LineState::Read),
            line("z", 2, LineState::Read),
        ];
        order.arrange(start, 1053, Held::Nothing, 0);
        let grown = vec![
            line("big", 9000, LineState::Read),
            line("x", 50, LineState::Read),
            line("y", 1, LineState::Read),
            line("z", 2, LineState::Read),
        ];
        let kept = order.arrange(grown, 9053, Held::Others, SORT_EVERY_TICKS);
        assert_eq!(
            names(&kept.others),
            ["z", "y"],
            "x not added under the pointer"
        );
    }

    #[test]
    fn what_others_holds_stays_while_a_line_is_held() {
        let mut order = Order::default();
        let start = vec![
            line("big", 1000, LineState::Read),
            line("x", 50, LineState::Read),
            line("y", 1, LineState::Read),
            line("z", 2, LineState::Read),
        ];
        let first = order.arrange(start, 1053, Held::Nothing, 0);
        assert_eq!(names(&first.others), ["z", "y"]);
        // The folder grows: `x` falls under one percent, under the pointer.
        let grown = vec![
            line("big", 9000, LineState::Read),
            line("x", 50, LineState::Read),
            line("y", 1, LineState::Read),
            line("z", 2, LineState::Read),
        ];
        let held = Key::File("x".into());
        let kept = order.arrange(grown.clone(), 9053, Held::Line(&held), SORT_EVERY_TICKS);
        assert_eq!(names(&kept.lines), ["big", "x"]);
        assert_eq!(names(&kept.others), ["z", "y"]);
        let freed = order.arrange(grown, 9053, Held::Nothing, SORT_EVERY_TICKS * 2);
        assert_eq!(names(&freed.others), ["x", "z", "y"]);
    }

    #[test]
    fn a_folder_gone_has_no_line_and_a_name_counted_elsewhere_no_size() {
        let entry = |state, partial| Entry {
            id: NodeId::ROOT,
            name: "f".into(),
            state,
            total: Size {
                allocated: 8,
                apparent: 4,
                files: 2,
            },
            partial,
        };
        assert_eq!(folder_line(&entry(NodeState::Gone, false)), None);
        let read = folder_line(&entry(NodeState::Read, false)).unwrap();
        assert_eq!(
            (read.state, read.allocated, read.apparent, read.files),
            (LineState::Read, 8, 4, 2)
        );
        for (node, state) in [
            (NodeState::Read, LineState::Partial),
            (NodeState::Pending, LineState::Pending),
            (NodeState::Changed, LineState::Changed),
            (NodeState::OtherMount, LineState::OtherMount),
            (
                NodeState::Unreadable(std::io::ErrorKind::PermissionDenied),
                LineState::Unreadable { denied: true },
            ),
            (
                NodeState::Unreadable(std::io::ErrorKind::NotFound),
                LineState::Unreadable { denied: false },
            ),
        ] {
            assert_eq!(
                folder_line(&entry(node, true)).unwrap().state,
                state,
                "{node:?}"
            );
        }
        let elsewhere = file_line(&FileEntry {
            name: "x".into(),
            kind: FileKind::Link,
            allocated: 100,
            apparent: 300,
            counted_in: Some(NodeId::ROOT),
        });
        assert_eq!(elsewhere.state, LineState::CountedElsewhere(NodeId::ROOT));
        assert_eq!(
            (elsewhere.kind, elsewhere.allocated, elsewhere.apparent),
            (LineKind::Link, 100, 300)
        );
        assert!(!elsewhere.is_sized());
    }

    #[test]
    fn a_file_is_sparse_far_beyond_what_compression_gives() {
        let file = |allocated, apparent| Line {
            apparent,
            ..line("f", allocated, LineState::Read)
        };
        assert!(file(1_000_000, 64_000_000).is_sparse());
        assert!(!file(4_000_000, 6_000_000).is_sparse(), "compressed");
        assert!(!file(100, 900_000).is_sparse(), "small");
        let folder = Line {
            kind: LineKind::Folder,
            ..file(0, 64_000_000)
        };
        assert!(!folder.is_sparse());
    }

    #[test]
    fn shares_are_cut_and_bars_follow_the_largest() {
        assert_eq!(share(1, 3), 33);
        assert_eq!(share(2, 3), 66);
        assert_eq!(share(1, 200), 0, "under one percent never reads 1 %");
        assert_eq!(share(5, 0), 0);
        assert_eq!(share(u64::MAX, u64::MAX), 100);
        assert_eq!(share(300, 200), 100, "a folder grown between two readings");
        assert!(!is_small(1000, 100_000), "one percent is no longer small");
        assert!(is_small(999, 100_000));
        assert!((bar(50, 200) - 0.25).abs() < f32::EPSILON);
        assert!((bar(300, 200) - 1.0).abs() < f32::EPSILON);
        assert!(bar(5, 0).abs() < f32::EPSILON);
    }

    fn mount(device: u32, point: &str, class: MountClass) -> Mount {
        Mount {
            id: 1,
            device: Device {
                major: 8,
                minor: device,
            },
            mount_point: PathBuf::from(point),
            filesystem: "ext4".into(),
            source: "/dev/sda".into(),
            class,
        }
    }

    #[test]
    fn the_unexplained_space_is_told_only_for_a_disk_mounted_once() {
        let list = |mounts| MountList {
            mounts,
            malformed: 0,
        };
        let alone = list(vec![
            mount(1, "/", MountClass::Block),
            mount(2, "/data", MountClass::Block),
        ]);
        assert!(unexplained_told(Path::new("/data"), &alone));
        assert!(unexplained_told(Path::new("/"), &alone));
        assert!(!unexplained_told(Path::new("/home/me"), &alone), "a folder");
        let subvolumes = list(vec![
            mount(1, "/", MountClass::Block),
            mount(1, "/home", MountClass::Block),
        ]);
        assert!(!unexplained_told(Path::new("/"), &subvolumes));
        assert!(!unexplained_told(Path::new("/home"), &subvolumes));
        let pool = list(vec![mount(
            3,
            "/",
            MountClass::Pool {
                pool: "rpool".into(),
            },
        )]);
        assert!(!unexplained_told(Path::new("/"), &pool));
        assert_eq!(unexplained(100, 70), 30);
        assert_eq!(unexplained(70, 100), 0);
    }

    #[test]
    fn a_folder_is_on_the_disk_of_the_deepest_mount_holding_it() {
        let mounts = [
            mount(1, "/", MountClass::Block),
            mount(2, "/home", MountClass::Block),
            mount(3, "/home/me/usb", MountClass::Block),
        ];
        let device = |minor| Some(Device { major: 8, minor });
        assert_eq!(disk_of(Path::new("/home/me"), &mounts), device(2));
        assert_eq!(
            disk_of(Path::new("/home/me/usb/photos"), &mounts),
            device(3)
        );
        assert_eq!(disk_of(Path::new("/srv"), &mounts), device(1));
        assert_eq!(
            disk_of(Path::new("/homework"), &mounts),
            device(1),
            "by components"
        );
        assert_eq!(disk_of(Path::new("/srv"), &[]), None);
    }

    #[test]
    fn the_trail_goes_down_up_and_back_to_any_folder() {
        let mut navigation = Navigation::default();
        assert_eq!(navigation.up(), None, "the root is the top");
        navigation.descend(NodeId::ROOT);
        navigation.descend(NodeId::ROOT);
        assert_eq!(navigation.trail().len(), 3);
        assert_eq!(navigation.up(), Some(NodeId::ROOT));
        assert_eq!(navigation.trail().len(), 2);
        assert!(navigation.go_to(0));
        assert_eq!(navigation.trail(), [NodeId::ROOT]);
        assert!(!navigation.go_to(4));
        assert_eq!(navigation.current(), NodeId::ROOT);
    }
}
