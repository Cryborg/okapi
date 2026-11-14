//! What the screens show the same way: counts within what the window
//! holds, sizes as Slint words them (`Words.size`), models filled row by row.

use okapi_engine::{ByteSize, SizeUnit};
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
}
