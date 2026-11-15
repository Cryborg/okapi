//! Which copies of each group are checked to go to the trash, and why the
//! others stay: the rule applied to a result, the quick selections, a copy
//! checked or not by hand. One copy of a group always stays unchecked; the
//! engine refuses the last one anyway (`KeepReason::LastCopy`).

use std::cmp::Ordering;
use std::path::Path;
use std::time::SystemTime;

use okapi_platform::PersonalKind;

/// What the rules read of a copy.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Facts<'a> {
    pub path: &'a Path,
    pub modified: SystemTime,
    /// Moving it frees its size: it has no other name.
    pub frees: bool,
    /// It lies on the filesystem of the trash: a move never copies.
    pub trashable: bool,
    /// The personal folder it lies in, by its rank among those known.
    pub folder: Option<usize>,
}

impl Facts<'_> {
    /// Whether it may go to the trash: it frees its size, and the trash
    /// takes it.
    const fn may_go(&self) -> bool {
        self.frees && self.trashable
    }
}

/// A personal folder, as the rules know it: its kind, its real path (a link
/// on its way resolved, as the copies' paths are).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Folder {
    pub kind: PersonalKind,
    pub path: std::path::PathBuf,
}

/// Why a copy is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reason {
    /// It has other names: moving it would free nothing.
    OtherNames,
    /// It lies outside the folders where the others are, by their ranks:
    /// Downloads, the Desktop, or both (one or two ranks).
    Outside(Vec<usize>),
    Oldest,
    Newest,
    Shortest,
    /// It lies in this folder, chosen by the user.
    InFolder(usize),
    /// It lies outside the personal folders: the rule leaves it to the
    /// user.
    OutsidePersonal,
    /// It lies on another filesystem than the trash, which cannot take it.
    OtherDevice,
}

/// A rule the user applies to every group at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quick {
    Oldest,
    Newest,
    Shortest,
    /// The copies in this personal folder, by its rank.
    InFolder(usize),
}

/// One copy as chosen: checked to go to the trash, or kept, with why the
/// last rule applied kept it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Chosen {
    Checked,
    Kept(Option<Reason>),
}

impl Default for Chosen {
    fn default() -> Self {
        Self::Kept(None)
    }
}

impl Chosen {
    pub(crate) const fn is_checked(&self) -> bool {
        matches!(self, Self::Checked)
    }

    /// Checked when it may go, kept for no rule's reason otherwise.
    const fn going_if(may_go: bool) -> Self {
        if may_go {
            Self::Checked
        } else {
            Self::Kept(None)
        }
    }
}

/// What is checked: copies, files and groups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub bytes: u64,
    pub files: usize,
    pub groups: usize,
}

/// Whether the copy lies in Downloads or on the Desktop, where copies come
/// to and are left.
fn passing_through(copy: &Facts<'_>, folders: &[Folder]) -> bool {
    copy.folder
        .and_then(|rank| folders.get(rank))
        .is_some_and(|folder| matches!(folder.kind, PersonalKind::Download | PersonalKind::Desktop))
}

/// The copy the rule keeps among `copies`, and why: one with other names,
/// else one outside Downloads and the Desktop, else the oldest, else the one
/// of the shortest path, each step among those the previous one left; the
/// reason is the step that left one alone, none when only the order of the
/// paths did. `None` without copies.
pub(crate) fn keeper(copies: &[Facts<'_>], folders: &[Folder]) -> Option<(usize, Option<Reason>)> {
    let mut left: Vec<usize> = (0..copies.len()).collect();
    if left.len() < 2 {
        return left.first().map(|&alone| (alone, None));
    }
    let other_names = left
        .iter()
        .copied()
        .filter(|&index| !copies[index].frees)
        .collect();
    if narrowed(&mut left, other_names) {
        return Some((left[0], Some(Reason::OtherNames)));
    }
    let (kept, aside): (Vec<usize>, Vec<usize>) = left
        .iter()
        .partition(|&&index| !passing_through(&copies[index], folders));
    let mut ranks: Vec<usize> = aside
        .iter()
        .filter_map(|&index| copies[index].folder)
        .collect();
    ranks.sort_unstable();
    ranks.dedup();
    if narrowed(&mut left, kept) {
        return Some((left[0], Some(Reason::Outside(ranks))));
    }
    let oldest = left.iter().map(|&index| copies[index].modified).min();
    let oldest = left
        .iter()
        .copied()
        .filter(|&index| Some(copies[index].modified) == oldest)
        .collect();
    if narrowed(&mut left, oldest) {
        return Some((left[0], Some(Reason::Oldest)));
    }
    let shortest = shortest(copies, &left);
    if narrowed(&mut left, shortest) {
        return Some((left[0], Some(Reason::Shortest)));
    }
    Some((left[0], None))
}

/// Narrows `left` to `kept` when that sets some apart, but never to none:
/// whether one is left alone by it.
fn narrowed(left: &mut Vec<usize>, kept: Vec<usize>) -> bool {
    if kept.is_empty() || kept.len() == left.len() {
        return false;
    }
    *left = kept;
    left.len() == 1
}

/// Those of `left` whose path is the shortest.
fn shortest(copies: &[Facts<'_>], left: &[usize]) -> Vec<usize> {
    let length = |index: usize| copies[index].path.as_os_str().len();
    let least = left.iter().map(|&index| length(index)).min();
    left.iter()
        .copied()
        .filter(|&index| Some(length(index)) == least)
        .collect()
}

/// A group as the rule chooses it at the result. The rule looks only at
/// the copies in a personal folder that the trash takes: a copy of an
/// application's folder, or on another disk (removable, maybe), is never
/// checked beforehand, nor kept by the rule at the expense of one of the
/// user's. Among them, the keeper is kept with its reason, every other that
/// may go is checked.
pub(crate) fn preselected(copies: &[Facts<'_>], folders: &[Folder]) -> Vec<Chosen> {
    let ruled: Vec<usize> = (0..copies.len())
        .filter(|&index| copies[index].folder.is_some() && copies[index].trashable)
        .collect();
    let among: Vec<Facts<'_>> = ruled.iter().map(|&index| copies[index]).collect();
    let kept = keeper(&among, folders).map(|(rank, reason)| (ruled[rank], reason));
    copies
        .iter()
        .enumerate()
        .map(|(index, copy)| match &kept {
            _ if copy.folder.is_none() => Chosen::Kept(Some(Reason::OutsidePersonal)),
            Some((keeper, reason)) if *keeper == index => Chosen::Kept(reason.clone()),
            _ => Chosen::going_if(copy.may_go()),
        })
        .collect()
}

/// A group as `quick` chooses it, or `None` when it does not apply (no
/// copy of the group in its folder): the copies it keeps, with their
/// reason, every other that may go checked, wherever it is.
pub(crate) fn quick_selected(copies: &[Facts<'_>], quick: Quick) -> Option<Vec<Chosen>> {
    let all: Vec<usize> = (0..copies.len()).collect();
    let by_date = |wanted: Ordering| {
        let date = |index: usize| copies[index].modified;
        let best = all.iter().map(|&index| date(index)).reduce(|best, other| {
            if other.cmp(&best) == wanted {
                other
            } else {
                best
            }
        });
        let alike: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&index| Some(date(index)) == best)
            .collect();
        shortest(copies, &alike).first().copied()
    };
    let (kept, reason): (Vec<usize>, Reason) = match quick {
        Quick::Oldest => (
            by_date(Ordering::Less).into_iter().collect(),
            Reason::Oldest,
        ),
        Quick::Newest => (
            by_date(Ordering::Greater).into_iter().collect(),
            Reason::Newest,
        ),
        Quick::Shortest => (
            shortest(copies, &all)
                .first()
                .copied()
                .into_iter()
                .collect(),
            Reason::Shortest,
        ),
        Quick::InFolder(folder) => (
            all.iter()
                .copied()
                .filter(|&index| copies[index].folder == Some(folder))
                .collect(),
            Reason::InFolder(folder),
        ),
    };
    if kept.is_empty() {
        return None;
    }
    Some(
        copies
            .iter()
            .enumerate()
            .map(|(index, copy)| {
                if kept.contains(&index) {
                    Chosen::Kept(Some(reason.clone()))
                } else {
                    Chosen::going_if(copy.may_go())
                }
            })
            .collect(),
    )
}

/// Why the copy is said kept: what the file itself is first (on another
/// disk than the trash, of other names), always true; else why the last
/// rule applied kept it. None for a copy checked.
pub(crate) fn said_reason(copy: &Facts<'_>, chosen: &Chosen) -> Option<Reason> {
    match chosen {
        Chosen::Checked => None,
        Chosen::Kept(_) if !copy.trashable => Some(Reason::OtherDevice),
        Chosen::Kept(_) if !copy.frees => Some(Reason::OtherNames),
        Chosen::Kept(reason) => reason.clone(),
    }
}

/// What moving all copies but one of a group of `size` bytes away frees at
/// most, as the screen can: a copy of other names, or on another disk than
/// the trash, never goes.
pub(crate) fn gain(copies: &[Facts<'_>], size: u64) -> u64 {
    let going = copies.iter().filter(|copy| copy.may_go()).count();
    let going = if going == copies.len() {
        going.saturating_sub(1)
    } else {
        going
    };
    size.saturating_mul(u64::try_from(going).unwrap_or(u64::MAX))
}

/// Whether the copy `index` is the last one of its group left unchecked.
pub(crate) fn is_last(chosen: &[Chosen], index: usize) -> bool {
    chosen.get(index).is_some_and(|choice| !choice.is_checked())
        && !chosen
            .iter()
            .enumerate()
            .any(|(other, choice)| other != index && !choice.is_checked())
}

/// Whether the copy `index` of a group may be checked: it frees its size,
/// the trash takes it, and another copy of its group stays unchecked.
pub(crate) fn checkable(copies: &[Facts<'_>], chosen: &[Chosen], index: usize) -> bool {
    let Some(copy) = copies.get(index) else {
        return false;
    };
    let others_left = chosen
        .iter()
        .enumerate()
        .any(|(other, choice)| other != index && !choice.is_checked());
    copy.may_go() && others_left
}

/// Turns the copy `index` of a group checked or unchecked, as the user
/// asked: whether it changed. A checked copy can always be unchecked; it
/// is then kept for no rule's reason.
pub(crate) fn toggle(copies: &[Facts<'_>], chosen: &mut [Chosen], index: usize) -> bool {
    let checkable = checkable(copies, chosen, index);
    match chosen.get_mut(index) {
        Some(choice @ Chosen::Checked) => *choice = Chosen::Kept(None),
        Some(choice @ Chosen::Kept(_)) if checkable => *choice = Chosen::Checked,
        _ => return false,
    }
    true
}

/// What the copies checked come to, each group's copies of `size` bytes.
pub(crate) fn totals<'a>(groups: impl IntoIterator<Item = (u64, &'a [Chosen])>) -> Totals {
    groups
        .into_iter()
        .fold(Totals::default(), |mut totals, (size, chosen)| {
            let checked = chosen.iter().filter(|choice| choice.is_checked()).count();
            if checked > 0 {
                totals.groups += 1;
                totals.files += checked;
                totals.bytes = totals.bytes.saturating_add(
                    size.saturating_mul(u64::try_from(checked).unwrap_or(u64::MAX)),
                );
            }
            totals
        })
}

/// The rank of the personal folder `path` lies in: the deepest that holds
/// it.
pub(crate) fn folder_of(path: &Path, folders: &[Folder]) -> Option<usize> {
    folders
        .iter()
        .enumerate()
        .filter(|(_, folder)| path.starts_with(&folder.path))
        .max_by_key(|(_, folder)| folder.path.components().count())
        .map(|(rank, _)| rank)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    const DOCUMENTS: usize = 0;
    const PICTURES: usize = 1;
    const DOWNLOAD: usize = 2;
    const DESKTOP: usize = 3;

    fn folders() -> Vec<Folder> {
        [
            (PersonalKind::Documents, "/home/ana/Documents"),
            (PersonalKind::Pictures, "/home/ana/Images"),
            (PersonalKind::Download, "/home/ana/Téléchargements"),
            (PersonalKind::Desktop, "/home/ana/Bureau"),
        ]
        .into_iter()
        .map(|(kind, path)| Folder {
            kind,
            path: PathBuf::from(path),
        })
        .collect()
    }

    /// A copy at `path`, modified `days` after the origin, freeing its size
    /// or not.
    struct Copy {
        path: PathBuf,
        days: u64,
        frees: bool,
        trashable: bool,
    }

    fn copy(path: &str, days: u64) -> Copy {
        Copy {
            path: PathBuf::from(path),
            days,
            frees: true,
            trashable: true,
        }
    }

    fn elsewhere(path: &str, days: u64) -> Copy {
        Copy {
            trashable: false,
            ..copy(path, days)
        }
    }

    fn linked(path: &str, days: u64) -> Copy {
        Copy {
            frees: false,
            ..copy(path, days)
        }
    }

    fn facts<'a>(copies: &'a [Copy], folders: &[Folder]) -> Vec<Facts<'a>> {
        copies
            .iter()
            .map(|copy| Facts {
                path: &copy.path,
                modified: SystemTime::UNIX_EPOCH + Duration::from_secs(copy.days * 86_400),
                frees: copy.frees,
                trashable: copy.trashable,
                folder: folder_of(&copy.path, folders),
            })
            .collect()
    }

    fn kept(copies: &[Copy]) -> Option<(usize, Option<Reason>)> {
        let folders = folders();
        keeper(&facts(copies, &folders), &folders)
    }

    /// Why a rule kept the copy, if it did.
    fn reason(chosen: &Chosen) -> Option<Reason> {
        match chosen {
            Chosen::Checked => None,
            Chosen::Kept(reason) => reason.clone(),
        }
    }

    fn checked(chosen: &[Chosen]) -> Vec<bool> {
        chosen.iter().map(Chosen::is_checked).collect()
    }

    #[test]
    fn a_copy_of_other_names_is_kept_before_an_older_one_outside_downloads() {
        let copies = [
            copy("/home/ana/Documents/a.mp4", 1),
            linked("/home/ana/Téléchargements/a.mp4", 9),
            copy("/home/ana/Images/long/way/a.mp4", 5),
        ];
        assert_eq!(kept(&copies), Some((1, Some(Reason::OtherNames))));
    }

    #[test]
    fn a_copy_outside_downloads_and_the_desktop_is_kept_before_an_older_one_in_them() {
        let copies = [
            copy("/home/ana/Bureau/a.jpg", 1),
            copy("/home/ana/Images/2026/a.jpg", 7),
            copy("/home/ana/Téléchargements/a.jpg", 3),
        ];
        assert_eq!(
            kept(&copies),
            Some((1, Some(Reason::Outside(vec![DOWNLOAD, DESKTOP]))))
        );
        let in_downloads = [
            copy("/home/ana/Téléchargements/a.jpg", 1),
            copy("/home/ana/Images/2026/a.jpg", 7),
        ];
        assert_eq!(
            kept(&in_downloads),
            Some((1, Some(Reason::Outside(vec![DOWNLOAD]))))
        );
    }

    #[test]
    fn then_the_oldest_then_the_shortest_path_then_the_first_path() {
        let oldest = [
            copy("/home/ana/Documents/b.tar", 9),
            copy("/home/ana/Documents/long/b.tar", 2),
            copy("/home/ana/Images/b.tar", 5),
        ];
        assert_eq!(kept(&oldest), Some((1, Some(Reason::Oldest))));
        let shortest = [
            copy("/home/ana/Documents/long/b.tar", 2),
            copy("/home/ana/Documents/b.tar", 2),
            copy("/home/ana/Images/b.tar", 5),
        ];
        assert_eq!(kept(&shortest), Some((1, Some(Reason::Shortest))));
        let tied = [
            copy("/home/ana/Documents/x/b.tar", 2),
            copy("/home/ana/Documents/y/b.tar", 2),
        ];
        assert_eq!(kept(&tied), Some((0, None)));
        assert_eq!(kept(&[]), None);
    }

    #[test]
    fn only_copies_in_personal_folders_are_checked_beforehand() {
        let folders = folders();
        let copies = [
            copy("/home/ana/Android/Sdk/lib.so", 1),
            copy("/home/ana/Documents/lib.so", 4),
            copy("/home/ana/Téléchargements/lib.so", 8),
        ];
        let chosen = preselected(&facts(&copies, &folders), &folders);
        // The keeper is chosen among the copies of personal folders, older
        // as the one of an application is: Downloads' copy goes.
        assert_eq!(checked(&chosen), [false, false, true]);
        assert_eq!(reason(&chosen[0]), Some(Reason::OutsidePersonal));
        assert_eq!(reason(&chosen[1]), Some(Reason::Outside(vec![DOWNLOAD])));
        assert_eq!(reason(&chosen[2]), None);
        let alone = [
            copy("/home/ana/Android/Sdk/lib.so", 1),
            copy("/home/ana/Téléchargements/lib.so", 8),
        ];
        let chosen = preselected(&facts(&alone, &folders), &folders);
        assert_eq!(checked(&chosen), [false, false]);
        assert_eq!(reason(&chosen[1]), None);
        let outside = [
            copy("/home/ana/Android/Sdk/a/lib.so", 1),
            copy("/home/ana/Android/Sdk/b/lib.so", 4),
        ];
        assert_eq!(
            checked(&preselected(&facts(&outside, &folders), &folders)),
            [false, false]
        );
    }

    #[test]
    fn a_copy_of_other_names_is_never_checked_beforehand() {
        let folders = folders();
        let copies = [
            linked("/home/ana/Documents/a.mp4", 1),
            linked("/home/ana/Images/a.mp4", 2),
            copy("/home/ana/Téléchargements/a.mp4", 3),
        ];
        let chosen = preselected(&facts(&copies, &folders), &folders);
        assert_eq!(checked(&chosen), [false, false, true]);
        assert_eq!(reason(&chosen[0]), Some(Reason::Oldest));
    }

    #[test]
    fn each_quick_selection_keeps_its_copy_wherever_the_others_are() {
        let folders = folders();
        let copies = [
            copy("/home/ana/Android/Sdk/a.png", 5),
            copy("/home/ana/Documents/a.png", 1),
            copy("/home/ana/Images/long/a.png", 9),
            linked("/home/ana/Bureau/a.png", 3),
        ];
        let facts = facts(&copies, &folders);
        let quick = |rule| checked(&quick_selected(&facts, rule).unwrap());
        assert_eq!(quick(Quick::Oldest), [true, false, true, false]);
        assert_eq!(quick(Quick::Newest), [true, true, false, false]);
        assert_eq!(quick(Quick::Shortest), [true, true, true, false]);
        assert_eq!(quick(Quick::InFolder(PICTURES)), [true, true, false, false]);
        let newest = quick_selected(&facts, Quick::Newest).unwrap();
        assert_eq!(reason(&newest[2]), Some(Reason::Newest));
        assert_eq!(reason(&newest[1]), None);
        assert_eq!(quick_selected(&facts, Quick::InFolder(DOWNLOAD)), None);
    }

    #[test]
    fn a_quick_selection_on_dates_alike_keeps_the_shortest_path() {
        let folders = folders();
        let copies = [
            copy("/home/ana/Documents/long/a.png", 1),
            copy("/home/ana/Documents/a.png", 1),
        ];
        let facts = facts(&copies, &folders);
        for rule in [Quick::Oldest, Quick::Newest] {
            assert_eq!(
                checked(&quick_selected(&facts, rule).unwrap()),
                [true, false]
            );
        }
    }

    #[test]
    fn the_last_copy_unchecked_and_one_of_other_names_cannot_be_checked() {
        let folders = folders();
        let copies = [
            copy("/home/ana/Documents/a", 1),
            copy("/home/ana/Images/a", 2),
            linked("/home/ana/Bureau/a", 3),
        ];
        let facts = facts(&copies, &folders);
        let mut chosen = vec![Chosen::default(); 3];
        assert!(!checkable(&facts, &chosen, 2), "other names");
        assert!(toggle(&facts, &mut chosen, 0));
        assert!(toggle(&facts, &mut chosen, 1), "the third stays");
        assert_eq!(checked(&chosen), [true, true, false]);
        let two = &facts[..2];
        let mut chosen = vec![Chosen::Checked, Chosen::default()];
        assert!(!toggle(two, &mut chosen, 1), "the last unchecked");
        assert_eq!(checked(&chosen), [true, false]);
        assert!(
            toggle(two, &mut chosen, 0),
            "a checked copy is always unchecked"
        );
        assert!(toggle(two, &mut chosen, 1));
        assert_eq!(checked(&chosen), [false, true]);
        assert!(!checkable(two, &chosen, 7));
    }

    #[test]
    fn the_totals_count_what_is_checked() {
        let on = Chosen::Checked;
        let off = Chosen::default();
        let first = [on.clone(), on.clone(), off.clone()];
        let second = [off.clone(), off.clone()];
        let third = [on, off];
        assert_eq!(
            totals([(10, &first[..]), (1000, &second[..]), (3, &third[..])]),
            Totals {
                bytes: 23,
                files: 3,
                groups: 2,
            }
        );
    }

    #[test]
    fn a_copy_lies_in_the_deepest_personal_folder_that_holds_it() {
        let mut folders = folders();
        folders.push(Folder {
            kind: PersonalKind::Music,
            path: PathBuf::from("/home/ana/Documents/Musique"),
        });
        assert_eq!(
            folder_of(Path::new("/home/ana/Documents/Musique/a"), &folders),
            Some(4)
        );
        assert_eq!(
            folder_of(Path::new("/home/ana/Documents/a"), &folders),
            Some(DOCUMENTS)
        );
        assert_eq!(
            folder_of(Path::new("/home/ana/Documentsx/a"), &folders),
            None
        );
    }

    #[test]
    fn a_copy_the_trash_cannot_take_is_never_checked() {
        let folders = folders();
        let copies = [
            copy("/home/ana/Documents/a.mov", 1),
            elsewhere("/home/ana/Images/a.mov", 5),
            copy("/home/ana/Téléchargements/a.mov", 3),
        ];
        let three = facts(&copies, &folders);
        let chosen = preselected(&three, &folders);
        assert_eq!(checked(&chosen), [false, false, true]);
        assert_eq!(
            said_reason(&three[1], &chosen[1]),
            Some(Reason::OtherDevice)
        );
        assert!(!checkable(&three, &chosen, 1));
        let newest = quick_selected(&three, Quick::Newest).unwrap();
        assert_eq!(checked(&newest), [true, false, true]);
        // What the file is is said whatever the rule.
        assert_eq!(
            said_reason(&three[1], &newest[1]),
            Some(Reason::OtherDevice)
        );
        let oldest = quick_selected(&three, Quick::Oldest).unwrap();
        assert_eq!(
            said_reason(&three[1], &oldest[1]),
            Some(Reason::OtherDevice)
        );
        assert_eq!(said_reason(&three[0], &oldest[0]), Some(Reason::Oldest));
        assert_eq!(said_reason(&three[2], &oldest[2]), None);
        // Only Documents' and Downloads' copies can go, and one stays.
        assert_eq!(gain(&three, 10), 20);
        assert_eq!(gain(&three[1..2], 10), 0);
        let all_go = [
            copy("/home/ana/Documents/b", 1),
            copy("/home/ana/Images/b", 2),
            copy("/home/ana/Bureau/b", 3),
        ];
        assert_eq!(gain(&facts(&all_go, &folders), 10), 20);
        let linked_one = [
            copy("/home/ana/Documents/b", 1),
            linked("/home/ana/Images/b", 2),
        ];
        assert_eq!(gain(&facts(&linked_one, &folders), 10), 10);
    }

    #[test]
    fn a_step_that_would_keep_none_is_passed_over() {
        let folders = folders();
        let all_passing = [
            copy("/home/ana/Téléchargements/b/a.zip", 4),
            copy("/home/ana/Bureau/a.zip", 2),
        ];
        let facts_passing = facts(&all_passing, &folders);
        assert_eq!(
            keeper(&facts_passing, &folders),
            Some((1, Some(Reason::Oldest)))
        );
        let two_named = [
            linked("/home/ana/Documents/a", 4),
            linked("/home/ana/Images/a", 2),
            copy("/home/ana/Bureau/a", 1),
        ];
        let facts_named = facts(&two_named, &folders);
        assert_eq!(
            keeper(&facts_named, &folders),
            Some((1, Some(Reason::Oldest)))
        );
    }

    /// Every group of two or three copies, of every name, folder, disk and
    /// date: whatever the rule, the quick selection or the boxes the user
    /// turns, one copy at least stays unchecked.
    #[test]
    fn one_copy_of_every_group_always_stays_unchecked() {
        let folders = folders();
        let places = [None, Some("Documents"), Some("Téléchargements")];
        let mut kinds = Vec::new();
        for frees in [true, false] {
            for trashable in [true, false] {
                for place in places {
                    for days in [1, 2] {
                        kinds.push((frees, trashable, place, days));
                    }
                }
            }
        }
        let build = |picked: &[(bool, bool, Option<&str>, u64)]| -> Vec<Copy> {
            picked
                .iter()
                .enumerate()
                .map(|(index, &(frees, trashable, place, days))| Copy {
                    path: PathBuf::from(format!(
                        "/home/ana/{}/{index}/a",
                        place.unwrap_or("Android")
                    )),
                    days,
                    frees,
                    trashable,
                })
                .collect()
        };
        let mut groups = Vec::new();
        for &first in &kinds {
            for &second in &kinds {
                groups.push(build(&[first, second]));
                for &third in &kinds {
                    groups.push(build(&[first, second, third]));
                }
            }
        }
        let some_left = |chosen: &[Chosen]| chosen.iter().any(|choice| !choice.is_checked());
        for copies in &groups {
            let facts = facts(copies, &folders);
            let mut chosen = preselected(&facts, &folders);
            assert!(some_left(&chosen));
            for quick in [
                Quick::Oldest,
                Quick::Newest,
                Quick::Shortest,
                Quick::InFolder(DOCUMENTS),
                Quick::InFolder(DOWNLOAD),
            ] {
                if let Some(quick) = quick_selected(&facts, quick) {
                    assert!(some_left(&quick));
                }
            }
            for round in 0..2 * facts.len() {
                toggle(&facts, &mut chosen, round % facts.len());
                assert!(some_left(&chosen));
                for (index, choice) in chosen.iter().enumerate() {
                    assert!(!choice.is_checked() || facts[index].may_go());
                }
            }
        }
    }

    #[test]
    fn the_rule_never_keeps_a_copy_the_trash_cannot_take_at_the_expense_of_one_it_can() {
        let folders = folders();
        // Documents on a disk of its own (removable, maybe), the oldest:
        // the copy of Downloads is the one the rule keeps, nothing goes.
        let copies = [
            elsewhere("/home/ana/Documents/a.mov", 1),
            copy("/home/ana/Téléchargements/a.mov", 3),
        ];
        let two = facts(&copies, &folders);
        let chosen = preselected(&two, &folders);
        assert_eq!(checked(&chosen), [false, false]);
        assert_eq!(said_reason(&two[0], &chosen[0]), Some(Reason::OtherDevice));
        // Three: the two the trash takes are ruled among themselves.
        let three = [
            elsewhere("/home/ana/Documents/b.mov", 1),
            copy("/home/ana/Images/b.mov", 5),
            copy("/home/ana/Téléchargements/b.mov", 3),
        ];
        let chosen = preselected(&facts(&three, &folders), &folders);
        assert_eq!(checked(&chosen), [false, false, true]);
        assert_eq!(reason(&chosen[1]), Some(Reason::Outside(vec![DOWNLOAD])));
    }

    #[test]
    fn a_copy_checked_then_unchecked_by_hand_is_kept_for_no_rule() {
        let folders = folders();
        let copies = [
            copy("/home/ana/Documents/c", 1),
            copy("/home/ana/Images/c", 2),
            copy("/home/ana/Bureau/c", 3),
        ];
        let three = facts(&copies, &folders);
        let mut chosen = preselected(&three, &folders);
        assert_eq!(reason(&chosen[0]), Some(Reason::Oldest));
        assert!(toggle(&three, &mut chosen, 1));
        assert!(toggle(&three, &mut chosen, 0), "the keeper checked");
        assert_eq!(said_reason(&three[0], &chosen[0]), None);
        assert!(toggle(&three, &mut chosen, 0));
        assert_eq!(said_reason(&three[0], &chosen[0]), None, "its reason gone");
        assert!(!is_last(&chosen, 0));
        assert!(toggle(&three, &mut chosen, 1));
        assert!(is_last(&chosen, 0));
        assert!(!is_last(&chosen, 1), "a checked copy is no last one");
    }
}
