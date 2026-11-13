//! What the Personnaliser mode does to the list of widgets: change a size,
//! remove one and put it back, add one, move one, start again from the
//! default.
//! Nothing here writes: the page keeps the list it gets back.

use okapi_store::WidgetSize;

use super::catalog::{WidgetKind, default_widgets, starting_size};
use super::layout::place;

/// The widgets shown, in order, each at its size.
pub(crate) type Widgets = Vec<(WidgetKind, WidgetSize)>;

/// A widget removed, and where it was: what "Annuler" puts back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Removed {
    pub index: usize,
    pub kind: WidgetKind,
    pub size: WidgetSize,
}

/// Gives the widget at `index` the size `size`, when it is drawn at it;
/// whether the list changed.
pub(crate) fn resize(widgets: &mut Widgets, index: usize, size: WidgetSize) -> bool {
    match widgets.get_mut(index) {
        Some((kind, current)) if kind.accepts(size) && *current != size => {
            *current = size;
            true
        }
        _ => false,
    }
}

/// Removes the widget at `index`; what was removed, to put it back.
pub(crate) fn remove(widgets: &mut Widgets, index: usize) -> Option<Removed> {
    (index < widgets.len()).then(|| {
        let (kind, size) = widgets.remove(index);
        Removed { index, kind, size }
    })
}

/// Puts `removed` back where it was (at the end when the list is shorter
/// now), unless it is there again; whether the list changed.
pub(crate) fn restore(widgets: &mut Widgets, removed: Removed) -> bool {
    if widgets.iter().any(|(kind, _)| *kind == removed.kind) {
        return false;
    }
    let index = removed.index.min(widgets.len());
    widgets.insert(index, (removed.kind, removed.size));
    true
}

/// Adds `kind` at the end, at its starting size, unless it is there
/// already (a widget at most once); whether the list changed.
pub(crate) fn add(widgets: &mut Widgets, kind: WidgetKind) -> bool {
    if widgets.iter().any(|(shown, _)| *shown == kind) {
        return false;
    }
    widgets.push((kind, starting_size(kind)));
    true
}

/// Moves the widget at `from` to `to` (bounded to the list), the others
/// keeping their order; whether the list changed.
pub(crate) fn move_to(widgets: &mut Widgets, from: usize, to: usize) -> bool {
    if from >= widgets.len() {
        return false;
    }
    let to = to.min(widgets.len() - 1);
    if from == to {
        return false;
    }
    let widget = widgets.remove(from);
    widgets.insert(to, widget);
    true
}

/// The default dashboard again; whether the list changed.
pub(crate) fn reset(widgets: &mut Widgets) -> bool {
    let default = default_widgets();
    if *widgets == default {
        return false;
    }
    *widgets = default;
    true
}

/// The list with the widget at `from` moved to `to`: what a move shows
/// before it is put down.
pub(crate) fn moved(widgets: &[(WidgetKind, WidgetSize)], from: usize, to: usize) -> Widgets {
    let mut preview = widgets.to_vec();
    move_to(&mut preview, from, to);
    preview
}

/// Where the widget at `from` lands when its card's centre is at `centre`
/// (in columns and rows of a grid of `columns`): the place whose cell, the
/// widget moved there, has its centre nearest; at equal distance, the one
/// nearest `from`.
pub(crate) fn drop_index(
    widgets: &[(WidgetKind, WidgetSize)],
    from: usize,
    centre: (f64, f64),
    columns: usize,
) -> usize {
    let distance = |to: usize| {
        let sizes: Vec<WidgetSize> = moved(widgets, from, to)
            .iter()
            .map(|(_, size)| *size)
            .collect();
        let (cells, _) = place(&sizes, columns);
        cells.get(to).map_or(f64::INFINITY, |cell| {
            let x = middle(cell.column, cell.width);
            let y = middle(cell.row, cell.height);
            (x - centre.0).powi(2) + (y - centre.1).powi(2)
        })
    };
    (0..widgets.len())
        .min_by(|left, right| {
            distance(*left)
                .total_cmp(&distance(*right))
                .then(left.abs_diff(from).cmp(&right.abs_diff(from)))
        })
        .unwrap_or(from)
}

/// The middle of `span` columns (or rows) from `start`.
#[expect(
    clippy::cast_precision_loss,
    reason = "a grid has a few columns and rows, far from 2^52"
)]
fn middle(start: usize, span: usize) -> f64 {
    start as f64 + span as f64 / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Widgets {
        vec![
            (WidgetKind::Reclaimable, WidgetSize::Full),
            (WidgetKind::Memory, WidgetSize::Tall),
            (WidgetKind::System, WidgetSize::Small),
            (WidgetKind::Disks, WidgetSize::Wide),
        ]
    }

    fn kinds(widgets: &Widgets) -> Vec<WidgetKind> {
        widgets.iter().map(|(kind, _)| *kind).collect()
    }

    #[test]
    fn a_size_changes_only_to_one_the_widget_is_drawn_in() {
        let mut widgets = list();
        assert!(resize(&mut widgets, 1, WidgetSize::Large));
        assert_eq!(widgets[1], (WidgetKind::Memory, WidgetSize::Large));
        assert!(
            !resize(&mut widgets, 2, WidgetSize::Wide),
            "System: small only"
        );
        assert!(!resize(&mut widgets, 1, WidgetSize::Large), "already");
        assert!(
            !resize(&mut widgets, 9, WidgetSize::Small),
            "no such widget"
        );
        assert_eq!(widgets[2], (WidgetKind::System, WidgetSize::Small));
    }

    #[test]
    fn a_widget_removed_goes_back_to_its_place_and_size() {
        let mut widgets = list();
        let removed = remove(&mut widgets, 1).unwrap();
        assert_eq!(
            removed,
            Removed {
                index: 1,
                kind: WidgetKind::Memory,
                size: WidgetSize::Tall
            }
        );
        assert_eq!(
            kinds(&widgets),
            [
                WidgetKind::Reclaimable,
                WidgetKind::System,
                WidgetKind::Disks
            ]
        );
        assert!(restore(&mut widgets, removed));
        assert_eq!(widgets, list());
        assert!(!restore(&mut widgets, removed), "there again");
        assert_eq!(remove(&mut widgets, 4), None);
    }

    #[test]
    fn a_widget_put_back_into_a_shorter_list_goes_at_its_end() {
        let mut widgets = list();
        let removed = remove(&mut widgets, 3).unwrap();
        remove(&mut widgets, 0);
        assert!(restore(&mut widgets, removed));
        assert_eq!(
            kinds(&widgets),
            [WidgetKind::Memory, WidgetKind::System, WidgetKind::Disks]
        );
    }

    #[test]
    fn a_widget_is_added_once_at_the_end_at_its_starting_size() {
        let mut widgets = list();
        assert!(add(&mut widgets, WidgetKind::Processor));
        assert_eq!(
            widgets.last(),
            Some(&(WidgetKind::Processor, WidgetSize::Wide))
        );
        assert!(
            !add(&mut widgets, WidgetKind::Memory),
            "a widget at most once"
        );
        assert_eq!(widgets.len(), 5);
    }

    #[test]
    fn a_widget_moves_both_ways_and_stays_within_the_list() {
        let mut widgets = list();
        assert!(move_to(&mut widgets, 0, 2));
        assert_eq!(
            kinds(&widgets),
            [
                WidgetKind::Memory,
                WidgetKind::System,
                WidgetKind::Reclaimable,
                WidgetKind::Disks
            ]
        );
        assert!(move_to(&mut widgets, 3, 0));
        assert_eq!(kinds(&widgets)[0], WidgetKind::Disks);
        assert!(move_to(&mut widgets, 0, 99), "bounded to the end");
        assert_eq!(kinds(&widgets)[3], WidgetKind::Disks);
        assert!(!move_to(&mut widgets, 3, 3));
        assert!(!move_to(&mut widgets, 4, 0));
    }

    #[test]
    fn the_default_comes_back_whole() {
        let mut widgets = list();
        assert!(reset(&mut widgets));
        assert_eq!(widgets, default_widgets());
        assert!(!reset(&mut widgets));
        let mut empty = Widgets::new();
        assert!(reset(&mut empty));
    }

    #[test]
    fn a_widget_dragged_lands_where_its_card_is_nearest() {
        // Three columns: Récupérable over the row, then Mémoire (Haut),
        // Système, Disques (Large).
        let widgets = list();
        // Disques' card centred on the first column of the second row: it
        // takes Mémoire's place.
        assert_eq!(drop_index(&widgets, 3, (0.5, 1.5), 3), 1);
        // Centred where it already is (two columns from column 1, row 2):
        // it stays.
        assert_eq!(drop_index(&widgets, 3, (2.0, 2.5), 3), 3);
        // Mémoire far below everything: the last place.
        assert_eq!(drop_index(&widgets, 1, (1.0, 40.0), 3), 3);
        // Récupérable over the top: the first place.
        assert_eq!(drop_index(&widgets, 0, (1.5, -3.0), 3), 0);
    }

    #[test]
    fn where_a_widget_lands_follows_the_number_of_columns() {
        let widgets = list();
        // One column: Récupérable, Mémoire (two rows), Système, Disques.
        // Disques dragged to the third row lands after Mémoire.
        assert_eq!(drop_index(&widgets, 3, (0.5, 3.2), 1), 2);
        // Four columns: Système (second row, second column) dragged to the
        // last column lands after Disques, which takes the two before it.
        assert_eq!(drop_index(&widgets, 2, (3.5, 1.5), 4), 3);
        assert_eq!(drop_index(&widgets, 2, (3.5, 1.5), 3), 2);
    }

    #[test]
    fn halfway_between_two_places_a_widget_lands_on_the_one_nearest_where_it_was() {
        // One column of small widgets, each on its row: a centre on the
        // line between the first two rows is as near to both.
        let widgets = vec![
            (WidgetKind::System, WidgetSize::Small),
            (WidgetKind::Processor, WidgetSize::Small),
            (WidgetKind::Memory, WidgetSize::Small),
        ];
        assert_eq!(drop_index(&widgets, 0, (0.5, 1.0), 1), 0);
        assert_eq!(drop_index(&widgets, 2, (0.5, 1.0), 1), 1);
    }
}
