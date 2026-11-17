//! What the screen holds of each file found (what the filters, the order
//! and the boxes read of it), and the lines it shows of them: the list, the
//! confirmation, the receipt. A file is held by its rank in the report,
//! never by its line: filters and order move lines, never what is checked.
//! Words are Slint's; paths are cut here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan as _};
use okapi_tools::large_files::{
    FLOORS, FileKind, KeepReason, LargeFile, LargeFileReport, TrashReceipt, TrashResult,
};
use slint::SharedString;

use crate::shown::{folder_shown, name_of, part, size, truncated};
use crate::ui;
use crate::user_files::{Folder, folder_of, trashable};

/// The ages a filter offers, in months: a file modified longer ago than
/// one of them is kept by it.
pub(crate) const AGES: [i32; 4] = [3, 6, 12, 24];
/// An age said in years when it is whole years.
const MONTHS_PER_YEAR: i32 = 12;
/// The files the confirmation names; the others are counted.
const CONFIRM_NAMED: usize = 5;

/// The age of `months` as the filter says it: in years when it is whole
/// years.
pub(crate) fn age_shown(months: i32) -> ui::AgeShown {
    if months % MONTHS_PER_YEAR == 0 {
        ui::AgeShown {
            count: months / MONTHS_PER_YEAR,
            years: true,
        }
    } else {
        ui::AgeShown {
            count: months,
            years: false,
        }
    }
}

/// What the screen reads of a file found, by its rank in the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Facts {
    pub path: PathBuf,
    pub on_disk: u64,
    pub size: u64,
    pub modified: SystemTime,
    pub kind: FileKind,
    /// Moving it frees what it takes: it has no other name.
    pub frees: bool,
    /// It lies on the filesystem of the trash.
    pub trashable: bool,
    /// It lies in a personal folder.
    pub personal: bool,
}

impl Facts {
    /// What the screen reads of `file`, the personal folders and the disk
    /// of the trash known.
    pub(crate) fn of(file: &LargeFile, folders: &[Folder], trash_device: Option<u64>) -> Self {
        Self {
            path: file.path().to_path_buf(),
            on_disk: file.on_disk(),
            size: file.size(),
            modified: file.modified(),
            kind: file.kind(),
            frees: file.frees(),
            trashable: trashable(trash_device, file.identity().device()),
            personal: folder_of(file.path(), folders).is_some(),
        }
    }

    /// Whether it may go to the trash.
    pub(crate) const fn checkable(&self) -> bool {
        self.frees && self.trashable
    }
}

/// The filters set: the smallest size on disk (one of [`floors`]), the
/// kind, the age by its rank in [`AGES`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Filters {
    pub floor: Option<u64>,
    pub kind: Option<FileKind>,
    pub age: Option<usize>,
}

/// How the list is ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Sort {
    /// The largest on disk first: the report's own order.
    #[default]
    Size,
    /// The oldest modified first, by path between equals.
    Oldest,
}

/// The floors a filter offers above `min_size`, the smallest file a search
/// lists: in the system's search, all but the first, its own.
pub(crate) fn floors(min_size: u64) -> Vec<u64> {
    FLOORS
        .iter()
        .copied()
        .filter(|&floor| floor > min_size)
        .collect()
}

/// The filters as files are compared to them at an instant.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    floor: u64,
    kind: Option<FileKind>,
    /// Modified at this instant or before.
    before: Option<SystemTime>,
}

impl Limits {
    /// `filters` at `now` in `zone`: an age counts calendar months.
    pub(crate) fn at(filters: Filters, now: Timestamp, zone: &TimeZone) -> Self {
        let before = filters.age.and_then(|rank| {
            let months = *AGES.get(rank)?;
            let at = now
                .to_zoned(zone.clone())
                .checked_sub(months.months())
                .inspect_err(|error| tracing::warn!(%error, months, "an age no date holds"))
                .ok()?;
            Some(SystemTime::from(at.timestamp()))
        });
        Self {
            floor: filters.floor.unwrap_or(0),
            kind: filters.kind,
            before,
        }
    }

    /// Whether `file` passes the size and the age, whatever its kind.
    fn passes_size_and_age(&self, file: &Facts) -> bool {
        file.on_disk >= self.floor && self.before.is_none_or(|before| file.modified <= before)
    }

    fn admits(&self, file: &Facts) -> bool {
        self.passes_size_and_age(file) && self.kind.is_none_or(|kind| file.kind == kind)
    }
}

/// The files a list shows, by rank in their order, and how many of each
/// kind the size and the age leave, in [`FileKind::ALL`]'s order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Visible {
    pub ranks: Vec<usize>,
    pub kinds: [usize; FileKind::ALL.len()],
    /// All kinds together.
    pub all_kinds: usize,
}

/// The files of `files` that `limits` let through, in `sort`'s order.
pub(crate) fn visible(files: &[Facts], limits: &Limits, sort: Sort) -> Visible {
    let mut kinds = [0; FileKind::ALL.len()];
    let mut all_kinds = 0;
    for file in files.iter().filter(|file| limits.passes_size_and_age(file)) {
        all_kinds += 1;
        if let Some(at) = FileKind::ALL.iter().position(|kind| *kind == file.kind) {
            kinds[at] += 1;
        }
    }
    let mut ranks: Vec<usize> = (0..files.len())
        .filter(|&rank| limits.admits(&files[rank]))
        .collect();
    if sort == Sort::Oldest {
        ranks.sort_by(|&left, &right| {
            let (left, right) = (&files[left], &files[right]);
            left.modified
                .cmp(&right.modified)
                .then_with(|| left.path.cmp(&right.path))
        });
    }
    Visible {
        ranks,
        kinds,
        all_kinds,
    }
}

/// Turns the box of file `rank` checked or unchecked: whether it changed
/// (a file that cannot go is never checked).
pub(crate) fn toggle(files: &[Facts], checked: &mut [bool], rank: usize) -> bool {
    let (Some(file), Some(check)) = (files.get(rank), checked.get_mut(rank)) else {
        return false;
    };
    if !*check && !file.checkable() {
        return false;
    }
    *check = !*check;
    true
}

/// What is chosen: its bytes on disk, its files, and those of them the
/// filters hide.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Totals {
    pub bytes: u64,
    pub files: usize,
    pub hidden: usize,
}

/// The ranks `checked` chooses, in the report's order.
pub(crate) fn chosen(checked: &[bool]) -> Vec<usize> {
    (0..checked.len()).filter(|&rank| checked[rank]).collect()
}

/// What the files `chosen` (by rank) come to, `shown` the ranks the list
/// shows.
pub(crate) fn totals(files: &[Facts], chosen: &[usize], shown: &[usize]) -> Totals {
    let mut visible = vec![false; files.len()];
    for &rank in shown {
        if let Some(seen) = visible.get_mut(rank) {
            *seen = true;
        }
    }
    let mut totals = Totals::default();
    for (rank, file) in chosen
        .iter()
        .filter_map(|&rank| files.get(rank).map(|file| (rank, file)))
    {
        totals.bytes = totals.bytes.saturating_add(file.on_disk);
        totals.files += 1;
        if visible.get(rank) != Some(&true) {
            totals.hidden += 1;
        }
    }
    totals
}

/// How the lines say what they show: the home folder, whether the personal
/// folders are known (a file outside them is said only then), the
/// characters a folder may take, an instant.
pub(crate) struct View<'a> {
    pub home: Option<&'a Path>,
    pub personal_known: bool,
    pub room: usize,
    pub moment: &'a dyn Fn(SystemTime) -> ui::Moment,
}

pub(crate) fn kind_shown(kind: FileKind) -> ui::FileKindShown {
    match kind {
        FileKind::Video => ui::FileKindShown::Video,
        FileKind::Image => ui::FileKindShown::Image,
        FileKind::Music => ui::FileKindShown::Music,
        FileKind::Document => ui::FileKindShown::Document,
        FileKind::Archive => ui::FileKindShown::Archive,
        FileKind::DiskImage => ui::FileKindShown::DiskImage,
        FileKind::Other => ui::FileKindShown::Other,
    }
}

/// Why a file cannot go, if it cannot.
fn why_of(file: &Facts) -> ui::LargeWhy {
    if !file.frees {
        ui::LargeWhy::OtherNames
    } else if !file.trashable {
        ui::LargeWhy::OtherDevice
    } else {
        ui::LargeWhy::None
    }
}

/// The lines of the files `shown`, by rank: each bar measured against the
/// largest file shown.
pub(crate) fn lines(
    files: &[Facts],
    checked: &[bool],
    shown: &[usize],
    view: &View<'_>,
) -> Vec<ui::LargeFileRow> {
    let largest = shown
        .iter()
        .filter_map(|&rank| files.get(rank))
        .map(|file| file.on_disk)
        .max()
        .unwrap_or(0);
    shown
        .iter()
        .filter_map(|&rank| files.get(rank).map(|file| (rank, file)))
        .map(|(rank, file)| {
            let folder = folder_shown(&file.path, view.home);
            let on_disk = size(file.on_disk);
            let apparent = size(file.size);
            ui::LargeFileRow {
                name: name_of(&file.path).into(),
                folder: truncated(&folder, view.room).into(),
                full_folder: folder.into(),
                differs: apparent != on_disk,
                size: on_disk,
                file_size: apparent,
                modified: (view.moment)(file.modified),
                kind: kind_shown(file.kind),
                bar: part(file.on_disk, largest),
                checked: checked.get(rank) == Some(&true),
                why: why_of(file),
                outside: view.personal_known && !file.personal,
            }
        })
        .collect()
}

/// What the confirmation says of the files `chosen`.
#[derive(Debug)]
pub(crate) struct Confirmed {
    /// Those it names (at most [`CONFIRM_NAMED`], by rank, in the report's
    /// order), each said hidden by the filters or not.
    pub named: Vec<ui::ChosenFile>,
    /// How many more it counts, and how many of them the filters hide.
    pub more: usize,
    pub more_hidden: usize,
}

/// What the confirmation says of the files `chosen` (by rank, in the
/// report's order), `shown` the ranks the list shows.
pub(crate) fn confirmed(
    files: &[Facts],
    chosen: &[usize],
    shown: &[usize],
    home: Option<&Path>,
) -> Confirmed {
    let named = chosen
        .iter()
        .take(CONFIRM_NAMED)
        .filter_map(|&rank| files.get(rank).map(|file| (rank, file)))
        .map(|(rank, file)| ui::ChosenFile {
            name: name_of(&file.path).into(),
            folder: folder_shown(&file.path, home).into(),
            size: size(file.on_disk),
            hidden: !shown.contains(&rank),
        })
        .collect();
    let others = chosen.iter().skip(CONFIRM_NAMED);
    Confirmed {
        named,
        more: others.clone().count(),
        more_hidden: others.filter(|rank| !shown.contains(rank)).count(),
    }
}

/// What the receipt says of a file kept for `reason`.
fn receipt_why(reason: KeepReason) -> ui::ReceiptWhy {
    match reason {
        KeepReason::Changed => ui::ReceiptWhy::Changed,
        KeepReason::Gone => ui::ReceiptWhy::Gone,
        KeepReason::OtherDevice => ui::ReceiptWhy::OtherDevice,
        KeepReason::Refused(_) => ui::ReceiptWhy::Refused,
        KeepReason::Excluded => ui::ReceiptWhy::Excluded,
        KeepReason::UnsupportedSystem => ui::ReceiptWhy::Unsupported,
        KeepReason::UnknownFile | KeepReason::OtherNames => {
            tracing::error!(
                reason = reason.key(),
                "a file the screen never asks for was asked for"
            );
            ui::ReceiptWhy::Okapi
        }
    }
}

/// What a move to the trash comes to, as the receipt says it.
#[derive(Debug, Default)]
pub(crate) struct Receipt {
    pub trashed: usize,
    /// Gone to the trash too, but not known to be the file checked.
    pub unchecked: usize,
    /// What went to the trash, checked or not, on disk.
    pub moved_bytes: u64,
    /// What stayed, or went unchecked, and why.
    pub kept: Vec<ui::KeptShown>,
}

/// The receipt of `receipt`, its files found in `report`.
pub(crate) fn receipt(
    report: &LargeFileReport,
    receipt: &TrashReceipt,
    home: Option<&Path>,
) -> Receipt {
    let mut said = Receipt::default();
    let mut reasons: BTreeMap<&'static str, usize> = BTreeMap::new();
    for outcome in receipt.outcomes() {
        let why = match outcome.result {
            TrashResult::Trashed { bytes } => {
                said.trashed += 1;
                said.moved_bytes = said.moved_bytes.saturating_add(bytes);
                continue;
            }
            TrashResult::Unchecked => {
                said.unchecked += 1;
                let bytes = report.find(outcome.id).map_or(0, LargeFile::on_disk);
                said.moved_bytes = said.moved_bytes.saturating_add(bytes);
                *reasons.entry("unchecked").or_default() += 1;
                ui::ReceiptWhy::Unchecked
            }
            TrashResult::Kept(reason) => {
                *reasons.entry(reason.key()).or_default() += 1;
                receipt_why(reason)
            }
        };
        // An identifier of another report never comes back from its move;
        // said all the same, never dropped.
        let line = report.find(outcome.id).map_or_else(
            || {
                tracing::error!(id = ?outcome.id, "a file of the receipt unknown to the report");
                ui::KeptShown {
                    name: SharedString::new(),
                    folder: SharedString::new(),
                    size: size(0),
                    why: ui::ReceiptWhy::Okapi,
                }
            },
            |file| ui::KeptShown {
                name: name_of(file.path()).into(),
                folder: folder_shown(file.path(), home).into(),
                size: size(file.on_disk()),
                why,
            },
        );
        said.kept.push(line);
    }
    tracing::info!(
        trashed = said.trashed,
        bytes = receipt.trashed_bytes(),
        unchecked = said.unchecked,
        moved_bytes = said.moved_bytes,
        kept = said.kept.len(),
        ?reasons,
        "move to the trash ended"
    );
    said
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use okapi_tools::large_files::Refusal;

    use super::*;

    const DAY: u64 = 86_400;
    const MEGA: u64 = 1_000_000;

    /// A file at `path`, taking `on_disk` megabytes on disk (ten times as
    /// many in its size: what is measured is never the size), modified
    /// `days` after the clock's origin, of `kind`.
    fn file(path: &str, on_disk: u64, days: u64, kind: FileKind) -> Facts {
        Facts {
            path: PathBuf::from(path),
            on_disk: on_disk * MEGA,
            size: 10 * on_disk * MEGA,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(days * DAY),
            kind,
            frees: true,
            trashable: true,
            personal: true,
        }
    }

    /// Three files whose orders by size, by date and by path all differ.
    fn files() -> Vec<Facts> {
        vec![
            file("/h/b.mkv", 900, 20, FileKind::Video),
            file("/h/c.iso", 600, 10, FileKind::DiskImage),
            file("/h/a.zip", 300, 30, FileKind::Archive),
        ]
    }

    fn limits(filters: Filters, now_days: u64) -> Limits {
        let now = Timestamp::from_second(i64::try_from(now_days * DAY).unwrap()).unwrap();
        Limits::at(filters, now, &TimeZone::UTC)
    }

    #[test]
    fn the_list_comes_by_size_or_oldest_first_and_back() {
        let files = files();
        let all = limits(Filters::default(), 40);
        assert_eq!(visible(&files, &all, Sort::Size).ranks, [0, 1, 2]);
        assert_eq!(visible(&files, &all, Sort::Oldest).ranks, [1, 0, 2]);
        // Between equal dates, by path.
        let mut alike = files;
        alike[0].modified = alike[2].modified;
        assert_eq!(visible(&alike, &all, Sort::Oldest).ranks, [1, 2, 0]);
    }

    #[test]
    fn a_floor_keeps_the_files_of_its_size_and_more() {
        let mut files = files();
        // Exactly at the floor: kept.
        files[1].on_disk = FLOORS[2];
        files[2].on_disk = FLOORS[2] - 1;
        let floor = |rank: usize| {
            visible(
                &files,
                &limits(
                    Filters {
                        floor: Some(FLOORS[rank]),
                        ..Filters::default()
                    },
                    40,
                ),
                Sort::Size,
            )
            .ranks
        };
        assert_eq!(floor(2), [0, 1]);
        assert_eq!(floor(4), Vec::<usize>::new());
    }

    #[test]
    fn the_floors_offered_are_those_above_the_smallest_file_listed() {
        assert_eq!(floors(FLOORS[0]), FLOORS[1..]);
        assert_eq!(floors(1), FLOORS);
        assert_eq!(floors(u64::MAX), Vec::<u64>::new());
    }

    #[test]
    fn an_age_keeps_the_files_modified_before_it_by_calendar_months() {
        // At 1970-07-31 (day 211): three months before is April 30th (day
        // 119), not 90 days before (day 121); six months before is January
        // 31st (day 30).
        let files = vec![
            file("/h/a.mkv", 900, 120, FileKind::Video),
            file("/h/b.mkv", 600, 30, FileKind::Video),
            file("/h/c.mkv", 300, 31, FileKind::Video),
        ];
        let aged = |rank| {
            visible(
                &files,
                &limits(
                    Filters {
                        age: Some(rank),
                        ..Filters::default()
                    },
                    211,
                ),
                Sort::Size,
            )
            .ranks
        };
        assert_eq!(aged(0), [1, 2]);
        assert_eq!(aged(1), [1]);
        // On the limit itself: kept.
        let at_limit = limits(
            Filters {
                age: Some(0),
                ..Filters::default()
            },
            211,
        );
        let mut limit = files.clone();
        limit[0].modified = at_limit.before.unwrap();
        assert_eq!(visible(&limit, &at_limit, Sort::Size).ranks, [0, 1, 2]);
        limit[0].modified += Duration::from_secs(1);
        assert_eq!(visible(&limit, &at_limit, Sort::Size).ranks, [1, 2]);
    }

    #[test]
    fn filters_combine_each_leaving_out_its_own_and_kinds_count_under_size_and_age() {
        let files = vec![
            file("/h/small.mkv", 60, 1, FileKind::Video),
            file("/h/new.mkv", 200, 1000, FileKind::Video),
            file("/h/old.iso", 200, 1, FileKind::DiskImage),
            file("/h/old.mkv", 200, 1, FileKind::Video),
        ];
        let set = Filters {
            floor: Some(FLOORS[1]),
            kind: Some(FileKind::Video),
            age: Some(0),
        };
        let shown = visible(&files, &limits(set, 1001), Sort::Size);
        assert_eq!(shown.ranks, [3]);
        // The kinds counted under the size and the age only.
        assert_eq!(shown.all_kinds, 2);
        let at = |kind| FileKind::ALL.iter().position(|each| *each == kind).unwrap();
        assert_eq!(shown.kinds[at(FileKind::Video)], 1);
        assert_eq!(shown.kinds[at(FileKind::DiskImage)], 1);
        assert_eq!(shown.kinds[at(FileKind::Archive)], 0);
    }

    #[test]
    fn only_a_file_that_can_go_is_checked_and_unchecking_always_is() {
        let mut files = files();
        files[1].frees = false;
        files[2].trashable = false;
        let mut checked = vec![false; 3];
        assert!(toggle(&files, &mut checked, 0));
        assert!(!toggle(&files, &mut checked, 1));
        assert!(!toggle(&files, &mut checked, 2));
        assert!(!toggle(&files, &mut checked, 3));
        assert_eq!(checked, [true, false, false]);
        assert!(toggle(&files, &mut checked, 0));
        assert_eq!(checked, [false; 3]);
    }

    #[test]
    fn a_file_checked_then_hidden_stays_chosen_and_is_said_hidden() {
        let files = files();
        let checked = chosen(&[true, false, true]);
        assert_eq!(checked, [0, 2]);
        assert_eq!(
            totals(&files, &checked, &[0, 1]),
            // On disk, never the size.
            Totals {
                bytes: 1200 * MEGA,
                files: 2,
                hidden: 1,
            }
        );
        let said = confirmed(&files, &checked, &[0, 1], Some(Path::new("/h")));
        assert_eq!(
            said.named
                .iter()
                .map(|file| (file.name.to_string(), file.hidden))
                .collect::<Vec<_>>(),
            [("b.mkv".to_owned(), false), ("a.zip".to_owned(), true)]
        );
        assert_eq!(said.named[0].folder, "~");
        assert_eq!((said.more, said.more_hidden), (0, 0));
    }

    #[test]
    fn the_confirmation_names_five_files_in_the_report_s_order_and_counts_the_rest() {
        let files: Vec<Facts> = (0..8)
            .map(|rank| file(&format!("/h/{rank}.mkv"), 100, 1, FileKind::Video))
            .collect();
        let mut checked = vec![true; 8];
        checked[2] = false;
        // Shown: all but the last two, one of them among those counted.
        let said = confirmed(&files, &chosen(&checked), &[0, 1, 2, 3, 4, 5, 6], None);
        assert_eq!(
            said.named
                .iter()
                .map(|file| file.name.to_string())
                .collect::<Vec<_>>(),
            ["0.mkv", "1.mkv", "3.mkv", "4.mkv", "5.mkv"]
        );
        assert_eq!((said.more, said.more_hidden), (2, 1));
    }

    #[test]
    fn each_line_says_its_file_its_bar_against_the_largest_shown_and_why_it_stays() {
        let mut files = files();
        files[1].size = files[1].on_disk;
        files[2].frees = false;
        files[2].personal = false;
        let moment = |_| ui::Moment::default();
        let view = View {
            home: Some(Path::new("/h")),
            personal_known: true,
            room: 60,
            moment: &moment,
        };
        // The largest of the report is not shown: the largest shown fills
        // its bar.
        let rows = lines(&files, &[false, true, false], &[1, 2], &view);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "c.iso");
        assert!((rows[0].bar - 1.0).abs() < f32::EPSILON);
        assert!((rows[1].bar - 0.5).abs() < f32::EPSILON);
        assert!(rows[0].checked && !rows[1].checked);
        assert_eq!(rows[0].kind, ui::FileKindShown::DiskImage);
        assert_eq!(rows[0].folder, "~");
        assert_eq!(rows[0].size, size(600 * MEGA));
        assert_eq!(rows[1].file_size, size(3000 * MEGA));
        assert_eq!(rows[0].why, ui::LargeWhy::None);
        assert_eq!(rows[1].why, ui::LargeWhy::OtherNames);
        assert!(rows[1].differs && !rows[0].differs);
        assert!(rows[1].outside && !rows[0].outside);
        // Personal folders unknown: none is said outside them.
        let unknown = View {
            personal_known: false,
            ..view
        };
        assert!(!lines(&files, &[false; 3], &[2], &unknown)[0].outside);
        files[1].trashable = false;
        assert_eq!(
            lines(&files, &[false; 3], &[1], &unknown)[0].why,
            ui::LargeWhy::OtherDevice
        );
        // A folder longer than its room loses whole folders at its start;
        // whole in the tooltip.
        files[0].path = PathBuf::from("/h/Vidéos/Montage/b.mkv");
        let narrow = View { room: 10, ..view };
        let row = &lines(&files, &[false; 3], &[0], &narrow)[0];
        assert_eq!(row.folder, "…/Montage");
        assert_eq!(row.full_folder, "~/Vidéos/Montage");
    }

    #[test]
    fn an_age_of_whole_years_is_said_in_years() {
        assert_eq!(
            AGES.map(age_shown).map(|age| (age.count, age.years)),
            [(3, false), (6, false), (1, true), (2, true)]
        );
    }

    #[test]
    fn each_kind_has_its_own_word() {
        let shown: Vec<ui::FileKindShown> =
            FileKind::ALL.iter().map(|kind| kind_shown(*kind)).collect();
        assert_eq!(
            shown,
            [
                ui::FileKindShown::Video,
                ui::FileKindShown::Image,
                ui::FileKindShown::Music,
                ui::FileKindShown::Document,
                ui::FileKindShown::Archive,
                ui::FileKindShown::DiskImage,
                ui::FileKindShown::Other,
            ]
        );
    }

    #[test]
    fn each_reason_a_file_stayed_for_has_its_words() {
        let refused = KeepReason::Refused(Refusal {
            kind: io::ErrorKind::PermissionDenied,
            code: Some(13),
        });
        for (reason, why) in [
            (KeepReason::Changed, ui::ReceiptWhy::Changed),
            (KeepReason::Gone, ui::ReceiptWhy::Gone),
            (KeepReason::OtherDevice, ui::ReceiptWhy::OtherDevice),
            (refused, ui::ReceiptWhy::Refused),
            (KeepReason::Excluded, ui::ReceiptWhy::Excluded),
            (KeepReason::UnsupportedSystem, ui::ReceiptWhy::Unsupported),
            (KeepReason::UnknownFile, ui::ReceiptWhy::Okapi),
            (KeepReason::OtherNames, ui::ReceiptWhy::Okapi),
        ] {
            assert_eq!(receipt_why(reason), why, "{reason:?}");
        }
    }
}
