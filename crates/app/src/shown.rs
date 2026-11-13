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

/// Shows `rows` in `model`, row by row when their number holds.
pub(crate) fn show<T: Clone + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() == rows.len() {
        for (index, row) in rows.into_iter().enumerate() {
            model.set_row_data(index, row);
        }
    } else {
        model.set_vec(rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
