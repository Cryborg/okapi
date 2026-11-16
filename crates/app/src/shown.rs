//! What the screens show the same way: counts within what the window
//! holds, with their thousands apart, paths from the home folder (cut at
//! their start when long), sizes as Slint words them (`Words.size`), models
//! filled row by row.

use okapi_engine::{ByteSize, SizeUnit};
use okapi_rules::Language;
use slint::{Model as _, VecModel};

use crate::ui;

/// A count as the window shows it: beyond what it holds, the most it holds,
/// traced.
pub(crate) fn shown<T: Copy + TryInto<i32> + std::fmt::Debug>(value: T) -> i32 {
    value.try_into().unwrap_or_else(|_| {
        tracing::warn!(?value, "beyond what the window shows");
        i32::MAX
    })
}

pub(crate) fn size(bytes: u64) -> ui::Size {
    let size = ByteSize::of(bytes);
    ui::Size {
        whole: shown(size.whole()),
        tenths: size.tenths().map_or(0, shown),
        has_tenths: size.tenths().is_some(),
        unit: match size.unit() {
            SizeUnit::Byte => ui::SizeUnit::Byte,
            SizeUnit::Kilobyte => ui::SizeUnit::Kilobyte,
            SizeUnit::Megabyte => ui::SizeUnit::Megabyte,
            SizeUnit::Gigabyte => ui::SizeUnit::Gigabyte,
            SizeUnit::Terabyte => ui::SizeUnit::Terabyte,
        },
    }
}

/// `count` with its thousands apart, as each language writes them.
pub(crate) fn grouped(count: u64, language: Language) -> String {
    let separator = match language {
        Language::French => '\u{202F}',
        Language::English => ',',
    };
    let digits = count.to_string();
    let mut text = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push(separator);
        }
        text.push(digit);
    }
    text
}

/// What stands for the home folder in a path shown.
pub(crate) const HOME_MARK: &str = "~";

/// `path` as shown: below `home` (by whole components), from `~`.
pub(crate) fn from_home(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(below) if below.as_os_str().is_empty() => HOME_MARK.to_owned(),
        Some(below) => format!("{HOME_MARK}/{}", below.display()),
        None => path.display().to_string(),
    }
}

/// The characters a path takes before its page says how many it may.
pub(crate) const DEFAULT_ROOM: usize = 60;
/// What stands for the folders left out at the start of a path.
const ELLIPSIS: &str = "…";
const SEPARATOR: char = '/';

/// The name a file or a folder is shown by: its own, as on the disk.
pub(crate) fn name_of(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The folder of `path`, as shown.
pub(crate) fn folder_shown(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    path.parent()
        .map(|parent| from_home(parent, home))
        .unwrap_or_default()
}

/// `shown` within `room` characters: whole when it fits, else `…/` and as
/// many of its last folders, whole, as fit with its name; `…/` and the name
/// alone when not even one folder fits (Slint elides its end).
pub(crate) fn truncated(shown: &str, room: usize) -> String {
    if shown.chars().count() <= room {
        return shown.to_owned();
    }
    let mut parts = shown.rsplit(SEPARATOR);
    let Some(name) = parts.next() else {
        return shown.to_owned();
    };
    let lead = ELLIPSIS.chars().count() + 1;
    let mut kept = name.to_owned();
    let mut length = lead + name.chars().count();
    for part in parts {
        let longer = length + part.chars().count() + 1;
        if longer > room {
            break;
        }
        kept = format!("{part}{SEPARATOR}{kept}");
        length = longer;
    }
    format!("{ELLIPSIS}{SEPARATOR}{kept}")
}

/// Shows `rows` in `model` in place: a row changed is set, rows added or
/// gone at the end are pushed or removed. The rows kept keep their elements,
/// so that the pointer and the focus stay on them.
pub(crate) fn show<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    let kept = model.row_count().min(rows.len());
    for (index, row) in rows.iter().enumerate().take(kept) {
        if model.row_data(index).as_ref() != Some(row) {
            model.set_row_data(index, row.clone());
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
    for row in rows.into_iter().skip(kept) {
        model.push(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_is_changed_in_place() {
        let model = VecModel::from(vec![1, 2, 3]);
        show(&model, vec![1, 5]);
        assert_eq!(model.iter().collect::<Vec<_>>(), [1, 5]);
        show(&model, vec![1, 5, 7, 8]);
        assert_eq!(model.iter().collect::<Vec<_>>(), [1, 5, 7, 8]);
    }

    #[test]
    fn sizes_and_counts_reach_the_window_within_its_bounds() {
        let shown_size = size(1_999_999);
        assert_eq!(
            (shown_size.whole, shown_size.tenths, shown_size.has_tenths),
            (1, 9, true)
        );
        assert_eq!(shown_size.unit, ui::SizeUnit::Megabyte);
        assert!(!size(999).has_tenths);
        assert_eq!(shown(usize::MAX), i32::MAX);
        assert_eq!(shown(7_usize), 7);
    }

    #[test]
    fn counts_have_their_thousands_apart() {
        assert_eq!(grouped(128_400, Language::French), "128\u{202F}400");
        assert_eq!(grouped(1_234_567, Language::English), "1,234,567");
        assert_eq!(grouped(999, Language::French), "999");
        assert_eq!(grouped(0, Language::English), "0");
    }

    #[test]
    fn a_long_path_loses_whole_folders_at_its_start() {
        let path = "~/Vidéos/Mariage de Claire et Thomas/Exports définitifs/montage-final.mp4";
        assert_eq!(truncated(path, 200), path);
        assert_eq!(truncated(path, path.chars().count()), path);
        assert_eq!(
            truncated(path, 40),
            "…/Exports définitifs/montage-final.mp4"
        );
        assert_eq!(
            truncated(path, 38),
            "…/Exports définitifs/montage-final.mp4"
        );
        assert_eq!(truncated(path, 37), "…/montage-final.mp4");
        assert_eq!(truncated(path, 5), "…/montage-final.mp4");
    }

    #[test]
    fn a_path_is_shown_from_home_by_whole_components() {
        use std::path::Path;

        let home = Path::new("/home/ana");
        assert_eq!(from_home(Path::new("/home/ana/x/y"), Some(home)), "~/x/y");
        assert_eq!(from_home(Path::new("/home/ana"), Some(home)), "~");
        assert_eq!(
            from_home(Path::new("/home/ana2/x"), Some(home)),
            "/home/ana2/x"
        );
        assert_eq!(from_home(Path::new("/data"), None), "/data");
    }
}
