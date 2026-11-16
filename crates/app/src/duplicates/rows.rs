//! What the screen holds of each group (where each copy lies, which are
//! checked, whether it shows its copies), and the lines it shows of them:
//! the tree, the receipt. Words are Slint's; paths are cut here.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use okapi_tools::duplicates::{
    CopyId, DuplicateReport, Group, KeepReason, TrashReceipt, TrashResult,
};
use slint::SharedString;

use super::choice::{self, Chosen, Facts, Quick, Reason, Totals};
use crate::shown::{folder_shown, from_home, name_of, shown, size, truncated};
use crate::ui;
use crate::user_files::{Folder, folder_of, trashable};

/// Where a copy lies, as the rules need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Place {
    folder: Option<usize>,
    trashable: bool,
}

/// A group as the screen holds it: one place and one choice per copy of
/// the report's group, by the copy's rank (built from that group, never
/// out of step with it).
#[derive(Debug, Clone)]
pub(crate) struct GroupChoice {
    places: Vec<Place>,
    chosen: Vec<Chosen>,
    open: bool,
}

impl GroupChoice {
    /// `group` as the rule checks it, unfolded.
    pub(crate) fn ruled(group: &Group, folders: &[Folder], trash_device: Option<u64>) -> Self {
        let places = group
            .copies()
            .iter()
            .map(|copy| Place {
                folder: folder_of(copy.path(), folders),
                trashable: trashable(trash_device, copy.identity().device()),
            })
            .collect();
        let mut held = Self {
            places,
            chosen: Vec::new(),
            open: true,
        };
        held.chosen = choice::preselected(&held.facts(group), folders);
        held
    }

    fn facts<'a>(&self, group: &'a Group) -> Vec<Facts<'a>> {
        group
            .copies()
            .iter()
            .zip(&self.places)
            .map(|(copy, place)| Facts {
                path: copy.path(),
                modified: copy.modified(),
                frees: copy.frees(),
                trashable: place.trashable,
                folder: place.folder,
            })
            .collect()
    }

    pub(crate) fn chosen(&self) -> &[Chosen] {
        &self.chosen
    }

    /// The personal folders its copies lie in, by rank.
    pub(crate) fn folders(&self) -> impl Iterator<Item = usize> + '_ {
        self.places.iter().filter_map(|place| place.folder)
    }

    /// How many of its copies lie outside the personal folders.
    pub(crate) fn outside(&self) -> usize {
        self.places
            .iter()
            .filter(|place| place.folder.is_none())
            .count()
    }

    /// Turns copy `index` checked or unchecked: whether it changed.
    pub(crate) fn toggle(&mut self, group: &Group, index: usize) -> bool {
        let facts = self.facts(group);
        choice::toggle(&facts, &mut self.chosen, index)
    }

    /// Chooses as `quick` does: whether it applied.
    pub(crate) fn quick(&mut self, group: &Group, quick: Quick) -> bool {
        match choice::quick_selected(&self.facts(group), quick) {
            Some(chosen) => {
                self.chosen = chosen;
                true
            }
            None => false,
        }
    }

    /// Every copy unchecked, the rules' reasons kept.
    pub(crate) fn deselect(&mut self) {
        for chosen in &mut self.chosen {
            if chosen.is_checked() {
                *chosen = Chosen::Kept(None);
            }
        }
    }

    pub(crate) fn fold(&mut self, open: bool) {
        self.open = open;
    }

    /// What it frees at most, as the screen can.
    pub(crate) fn gain(&self, group: &Group) -> u64 {
        choice::gain(&self.facts(group), group.size())
    }

    /// The identifiers of its copies checked.
    pub(crate) fn checked<'a>(&'a self, group: &'a Group) -> impl Iterator<Item = CopyId> + 'a {
        group
            .copies()
            .iter()
            .zip(&self.chosen)
            .filter(|(_, chosen)| chosen.is_checked())
            .map(|(copy, _)| copy.id())
    }
}

/// What a line of the tree is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Line {
    Head(usize),
    Copy(usize, usize),
}

/// How the lines say what they show: the home folder, the names of the
/// personal folders by rank, the characters a path may take, an instant.
pub(crate) struct View<'a> {
    pub home: Option<&'a Path>,
    pub names: &'a [String],
    pub room: usize,
    pub moment: &'a dyn Fn(SystemTime) -> ui::Moment,
}

/// The lines of the tree, what each names, and what is checked: the
/// groups by what the screen can free, most first (a copy on another disk
/// than the trash frees nothing), in the report's order otherwise.
pub(crate) fn lines(
    report: &DuplicateReport,
    groups: &[GroupChoice],
    view: &View<'_>,
) -> (Vec<ui::DuplicateRow>, Vec<Line>, Totals) {
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    let gains: Vec<u64> = report
        .groups()
        .iter()
        .zip(groups)
        .map(|(group, held)| held.gain(group))
        .collect();
    for rank in order(&gains) {
        let (Some(group), Some(held)) = (report.groups().get(rank), groups.get(rank)) else {
            continue;
        };
        let head_line = lines.len();
        let facts = held.facts(group);
        rows.push(head_row(group, held, gains[rank]));
        lines.push(Line::Head(rank));
        if !held.open {
            continue;
        }
        for (index, (copy, chosen)) in group.copies().iter().zip(&held.chosen).enumerate() {
            let reason = facts
                .get(index)
                .and_then(|copy| choice::said_reason(copy, chosen));
            let (why, why_first, why_second) = why_of(reason.as_ref(), view.names);
            let full = from_home(copy.path(), view.home);
            rows.push(ui::DuplicateRow {
                is_head: false,
                head: shown(head_line),
                name: name_of(copy.path()).into(),
                folder: folder_shown(copy.path(), view.home).into(),
                path: truncated(&full, view.room).into(),
                full_path: full.into(),
                size: size(group.size()),
                copies: 0,
                gain: size(0),
                open: false,
                checked_count: 0,
                modified: (view.moment)(copy.modified()),
                checked: chosen.is_checked(),
                checkable: choice::checkable(&facts, &held.chosen, index),
                last: choice::is_last(&held.chosen, index),
                other_names: !copy.frees(),
                why,
                why_first: why_first.into(),
                why_second: why_second.into(),
            });
            lines.push(Line::Copy(rank, index));
        }
    }
    let totals = choice::totals(
        report
            .groups()
            .iter()
            .zip(groups)
            .map(|(group, held)| (group.size(), held.chosen.as_slice())),
    );
    (rows, lines, totals)
}

/// The ranks of groups freeing `gains`, the most first, the report's order
/// kept between equals.
fn order(gains: &[u64]) -> Vec<usize> {
    let mut ranks: Vec<usize> = (0..gains.len()).collect();
    ranks.sort_by_key(|&rank| std::cmp::Reverse(gains[rank]));
    ranks
}

/// A group's header, freeing `gain`: the fields of a copy left empty.
fn head_row(group: &Group, held: &GroupChoice, gain: u64) -> ui::DuplicateRow {
    let name = group
        .copies()
        .first()
        .map(|copy| name_of(copy.path()))
        .unwrap_or_default();
    ui::DuplicateRow {
        is_head: true,
        head: 0,
        name: name.into(),
        folder: SharedString::new(),
        path: SharedString::new(),
        full_path: SharedString::new(),
        size: size(group.size()),
        copies: shown(group.copies().len()),
        gain: size(gain),
        open: held.open,
        checked_count: shown(
            held.chosen
                .iter()
                .filter(|chosen| chosen.is_checked())
                .count(),
        ),
        modified: ui::Moment::default(),
        checked: false,
        checkable: false,
        last: false,
        other_names: false,
        why: ui::KeptWhy::None,
        why_first: SharedString::new(),
        why_second: SharedString::new(),
    }
}

/// What a badge says of `reason`, with the names of the folders it names.
pub(crate) fn why_of(reason: Option<&Reason>, names: &[String]) -> (ui::KeptWhy, String, String) {
    let name = |rank: usize| names.get(rank).cloned().unwrap_or_default();
    let alone = |why| (why, String::new(), String::new());
    match reason {
        None => alone(ui::KeptWhy::None),
        Some(Reason::OtherNames) => alone(ui::KeptWhy::OtherNames),
        Some(Reason::Outside(ranks)) => (
            ui::KeptWhy::Outside,
            ranks.first().copied().map(name).unwrap_or_default(),
            ranks.get(1).copied().map(name).unwrap_or_default(),
        ),
        Some(Reason::Oldest) => alone(ui::KeptWhy::Oldest),
        Some(Reason::Newest) => alone(ui::KeptWhy::Newest),
        Some(Reason::Shortest) => alone(ui::KeptWhy::Shortest),
        Some(Reason::InFolder(rank)) => (ui::KeptWhy::InFolder, name(*rank), String::new()),
        Some(Reason::OutsidePersonal) => alone(ui::KeptWhy::OutsidePersonal),
        Some(Reason::OtherDevice) => alone(ui::KeptWhy::OtherDevice),
    }
}

/// What the receipt says of a copy kept for `reason`.
pub(crate) fn receipt_why(reason: KeepReason) -> ui::ReceiptWhy {
    match reason {
        KeepReason::Changed => ui::ReceiptWhy::Changed,
        KeepReason::Gone => ui::ReceiptWhy::Gone,
        KeepReason::KeeperChanged => ui::ReceiptWhy::KeeperChanged,
        KeepReason::NoSureKeeper => ui::ReceiptWhy::NoSureKeeper,
        KeepReason::OtherDevice => ui::ReceiptWhy::OtherDevice,
        KeepReason::Refused(_) => ui::ReceiptWhy::Refused,
        KeepReason::Excluded => ui::ReceiptWhy::Excluded,
        KeepReason::UnsupportedSystem => ui::ReceiptWhy::Unsupported,
        KeepReason::UnknownCopy | KeepReason::LastCopy | KeepReason::OtherNames => {
            tracing::error!(
                reason = reason.key(),
                "a copy the screen never asks for was asked for"
            );
            ui::ReceiptWhy::Okapi
        }
    }
}

/// What a move to the trash comes to, as the receipt says it.
#[derive(Debug, Default)]
pub(crate) struct Receipt {
    pub trashed: usize,
    /// Gone to the trash too, but not known to be the copy checked.
    pub unchecked: usize,
    /// What went to the trash, checked or not.
    pub moved_bytes: u64,
    /// What stayed, or went unchecked, and why.
    pub kept: Vec<ui::KeptShown>,
}

/// The receipt of `receipt`, its copies found in `report`.
pub(crate) fn receipt(
    report: &DuplicateReport,
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
                let bytes = report.find(outcome.id).map_or(0, |(_, copy)| copy.size());
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
                tracing::error!(id = ?outcome.id, "a copy of the receipt unknown to the report");
                ui::KeptShown {
                    name: SharedString::new(),
                    folder: SharedString::new(),
                    size: size(0),
                    why: ui::ReceiptWhy::Okapi,
                }
            },
            |(_, copy)| ui::KeptShown {
                name: name_of(copy.path()).into(),
                folder: folder_shown(copy.path(), home).into(),
                size: size(copy.size()),
                why,
            },
        );
        said.kept.push(line);
    }
    tracing::info!(
        trashed = said.trashed,
        bytes = receipt.trashed_bytes(),
        kept = said.kept.len(),
        ?reasons,
        "move to the trash ended"
    );
    said
}

#[cfg(test)]
mod tests {
    use std::io;

    use okapi_tools::duplicates::Refusal;

    use super::*;

    #[test]
    fn the_groups_come_by_what_the_screen_frees_the_report_s_order_between_equals() {
        assert_eq!(order(&[10, 30, 20]), [1, 2, 0]);
        assert_eq!(order(&[5, 0, 5, 7]), [3, 0, 2, 1]);
        assert_eq!(order(&[]), Vec::<usize>::new());
    }

    #[test]
    fn a_badge_names_the_folders_its_reason_names() {
        let names = [
            "Documents".to_owned(),
            "Téléchargements".to_owned(),
            "Bureau".to_owned(),
        ];
        let outside = Reason::Outside(vec![1, 2]);
        assert_eq!(
            why_of(Some(&outside), &names),
            (
                ui::KeptWhy::Outside,
                "Téléchargements".to_owned(),
                "Bureau".to_owned()
            )
        );
        assert_eq!(
            why_of(Some(&Reason::InFolder(0)), &names),
            (ui::KeptWhy::InFolder, "Documents".to_owned(), String::new())
        );
        for (reason, why) in [
            (Reason::OtherNames, ui::KeptWhy::OtherNames),
            (Reason::Oldest, ui::KeptWhy::Oldest),
            (Reason::Newest, ui::KeptWhy::Newest),
            (Reason::Shortest, ui::KeptWhy::Shortest),
            (Reason::OutsidePersonal, ui::KeptWhy::OutsidePersonal),
            (Reason::OtherDevice, ui::KeptWhy::OtherDevice),
        ] {
            assert_eq!(why_of(Some(&reason), &names).0, why, "{reason:?}");
        }
        assert_eq!(why_of(None, &names).0, ui::KeptWhy::None);
    }

    #[test]
    fn each_reason_a_copy_stayed_for_has_its_words() {
        let refused = KeepReason::Refused(Refusal {
            kind: io::ErrorKind::PermissionDenied,
            code: Some(13),
        });
        for (reason, why) in [
            (KeepReason::Changed, ui::ReceiptWhy::Changed),
            (KeepReason::Gone, ui::ReceiptWhy::Gone),
            (KeepReason::KeeperChanged, ui::ReceiptWhy::KeeperChanged),
            (KeepReason::NoSureKeeper, ui::ReceiptWhy::NoSureKeeper),
            (KeepReason::OtherDevice, ui::ReceiptWhy::OtherDevice),
            (refused, ui::ReceiptWhy::Refused),
            (KeepReason::Excluded, ui::ReceiptWhy::Excluded),
            (KeepReason::UnsupportedSystem, ui::ReceiptWhy::Unsupported),
            (KeepReason::UnknownCopy, ui::ReceiptWhy::Okapi),
            (KeepReason::LastCopy, ui::ReceiptWhy::Okapi),
            (KeepReason::OtherNames, ui::ReceiptWhy::Okapi),
        ] {
            assert_eq!(receipt_why(reason), why, "{reason:?}");
        }
    }
}
