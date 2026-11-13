//! Which widgets the dashboard shows, and where each one sits in a grid of
//! a given number of columns. Slint only draws what is placed here.

use okapi_store::{LoadedLayout, WidgetSize};

use super::catalog::{WidgetKind, default_widgets};

/// The most columns the grid ever has; Slint picks how many fit.
pub(crate) const MAX_COLUMNS: usize = 4;

/// Where a widget sits, in columns and rows of the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell {
    pub column: usize,
    pub row: usize,
    pub width: usize,
    pub height: usize,
}

/// The widgets of `loaded`, in their order: an entry the application does
/// not know, a size its widget is not drawn in, or a widget already shown
/// is left out (traced). With no layout kept, or none of its entries left,
/// the default dashboard; a layout the user emptied stays empty.
pub(crate) fn shown(loaded: Option<&LoadedLayout>) -> Vec<(WidgetKind, WidgetSize)> {
    let Some(loaded) = loaded else {
        return default_widgets();
    };
    let mut widgets: Vec<(WidgetKind, WidgetSize)> = Vec::new();
    for (index, widget) in loaded.layout.widgets.iter().enumerate() {
        let left_out = match WidgetKind::named(&widget.kind) {
            None => "unknown widget",
            Some(kind) if !kind.accepts(widget.size) => "size the widget is not drawn in",
            Some(kind) if widgets.iter().any(|(shown, _)| *shown == kind) => "widget already shown",
            Some(kind) => {
                widgets.push((kind, widget.size));
                continue;
            }
        };
        tracing::warn!(
            index,
            kind = %widget.kind,
            size = ?widget.size,
            reason = left_out,
            "dashboard widget left out"
        );
    }
    if widgets.is_empty() && !loaded.layout.widgets.is_empty() {
        tracing::warn!("no dashboard widget left: the default one is shown");
        return default_widgets();
    }
    widgets
}

/// How many columns and rows a widget of `size` takes in a grid of
/// `columns`: never more columns than the grid has.
fn span(size: WidgetSize, columns: usize) -> (usize, usize) {
    let (width, height) = match size {
        WidgetSize::Small => (1, 1),
        WidgetSize::Wide => (2, 1),
        WidgetSize::Tall => (1, 2),
        WidgetSize::Large => (2, 2),
        WidgetSize::Full => (columns, 1),
    };
    (width.min(columns), height)
}

/// The cells of widgets of `sizes`, in order, in a grid of `columns` (at
/// least one), and how many rows they fill. Each widget takes the first
/// free place after the one before it, never before: a hole a wide widget
/// leaves at the end of a row stays, as in the mockup and CSS's sparse
/// grid.
pub(crate) fn place(sizes: &[WidgetSize], columns: usize) -> (Vec<Cell>, usize) {
    let columns = columns.max(1);
    let mut taken: Vec<Vec<bool>> = Vec::new();
    let is_free = |taken: &[Vec<bool>], cell: Cell| {
        (cell.row..cell.row + cell.height).all(|row| {
            taken
                .get(row)
                .is_none_or(|line| !line[cell.column..cell.column + cell.width].contains(&true))
        })
    };
    let (mut row, mut column) = (0, 0);
    let cells: Vec<Cell> = sizes
        .iter()
        .map(|size| {
            let (width, height) = span(*size, columns);
            let cell = loop {
                if column + width > columns {
                    row += 1;
                    column = 0;
                    continue;
                }
                let cell = Cell {
                    column,
                    row,
                    width,
                    height,
                };
                if is_free(&taken, cell) {
                    break cell;
                }
                column += 1;
            };
            if taken.len() < row + height {
                taken.resize_with(row + height, || vec![false; columns]);
            }
            for line in &mut taken[row..row + height] {
                line[column..column + width].fill(true);
            }
            column += width;
            cell
        })
        .collect();
    (cells, taken.len())
}

#[cfg(test)]
mod tests {
    use okapi_store::{DashboardLayout, DashboardWidget};

    use super::*;

    fn loaded(widgets: &[(&str, WidgetSize)]) -> LoadedLayout {
        LoadedLayout {
            layout: DashboardLayout {
                widgets: widgets
                    .iter()
                    .map(|(kind, size)| DashboardWidget {
                        kind: (*kind).to_owned(),
                        size: *size,
                    })
                    .collect(),
            },
            left_out: 0,
        }
    }

    fn cell(column: usize, row: usize, width: usize, height: usize) -> Cell {
        Cell {
            column,
            row,
            width,
            height,
        }
    }

    #[test]
    fn without_a_layout_kept_the_default_dashboard_is_shown() {
        assert_eq!(shown(None), default_widgets());
    }

    #[test]
    fn unknown_widgets_refused_sizes_and_repeats_are_left_out_in_order() {
        let layout = loaded(&[
            ("weather", WidgetSize::Small),
            ("system", WidgetSize::Small),
            ("processor", WidgetSize::Large),
            ("disks", WidgetSize::Tall),
            ("processor", WidgetSize::Wide),
            ("disks", WidgetSize::Small),
        ]);
        assert_eq!(
            shown(Some(&layout)),
            [
                (WidgetKind::System, WidgetSize::Small),
                (WidgetKind::Disks, WidgetSize::Tall),
                (WidgetKind::Processor, WidgetSize::Wide),
            ],
            "a widget refused at one size is still shown by a later entry"
        );
    }

    #[test]
    fn a_layout_the_user_emptied_stays_empty_and_one_left_with_nothing_is_the_default() {
        assert_eq!(shown(Some(&loaded(&[]))), []);
        assert_eq!(
            shown(Some(&loaded(&[("weather", WidgetSize::Small)]))),
            default_widgets()
        );
    }

    #[test]
    fn the_default_dashboard_sits_as_in_the_mockup() {
        let sizes: Vec<WidgetSize> = default_widgets()
            .into_iter()
            .map(|(_, size)| size)
            .collect();
        // Three columns (1280 px): Reclaimable over the row, then Disks and
        // Memory, then the Processor and the System.
        assert_eq!(
            place(&sizes, 3),
            (
                vec![
                    cell(0, 0, 3, 1),
                    cell(0, 1, 2, 1),
                    cell(2, 1, 1, 1),
                    cell(0, 2, 2, 1),
                    cell(2, 2, 1, 1),
                ],
                3
            )
        );
        // Two columns (960 px): Memory goes below Disks.
        assert_eq!(
            place(&sizes, 2).0,
            [
                cell(0, 0, 2, 1),
                cell(0, 1, 2, 1),
                cell(0, 2, 1, 1),
                cell(0, 3, 2, 1),
                cell(0, 4, 1, 1),
            ]
        );
    }

    #[test]
    fn a_hole_left_at_the_end_of_a_row_is_never_filled_by_a_later_widget() {
        let sizes = [
            WidgetSize::Small,
            WidgetSize::Small,
            WidgetSize::Wide,
            WidgetSize::Small,
        ];
        assert_eq!(
            place(&sizes, 3),
            (
                vec![
                    cell(0, 0, 1, 1),
                    cell(1, 0, 1, 1),
                    cell(0, 1, 2, 1),
                    cell(2, 1, 1, 1),
                ],
                2
            )
        );
    }

    #[test]
    fn tall_widgets_take_two_rows_and_the_next_ones_go_around_them() {
        let sizes = [
            WidgetSize::Tall,
            WidgetSize::Large,
            WidgetSize::Small,
            WidgetSize::Small,
            WidgetSize::Small,
        ];
        assert_eq!(
            place(&sizes, 4),
            (
                vec![
                    cell(0, 0, 1, 2),
                    cell(1, 0, 2, 2),
                    cell(3, 0, 1, 1),
                    cell(3, 1, 1, 1),
                    cell(0, 2, 1, 1),
                ],
                3
            )
        );
    }

    #[test]
    fn in_one_column_every_widget_is_one_column_wide() {
        let sizes = [WidgetSize::Full, WidgetSize::Large, WidgetSize::Wide];
        assert_eq!(
            place(&sizes, 1),
            (
                vec![cell(0, 0, 1, 1), cell(0, 1, 1, 2), cell(0, 3, 1, 1)],
                4
            )
        );
    }

    #[test]
    fn an_empty_dashboard_fills_no_row() {
        assert_eq!(place(&[], MAX_COLUMNS), (Vec::new(), 0));
    }
}
