//! What the Personnaliser mode does to the list of widgets: change a size,
//! remove one and put it back, add one, start again from the default.
//! Nothing here writes: the page keeps the list it gets back.

use okapi_store::WidgetSize;

use super::catalog::{WidgetKind, default_widgets, starting_size};

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

/// The default dashboard again; whether the list changed.
pub(crate) fn reset(widgets: &mut Widgets) -> bool {
    let default = default_widgets();
    if *widgets == default {
        return false;
    }
    *widgets = default;
    true
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
    fn the_default_comes_back_whole() {
        let mut widgets = list();
        assert!(reset(&mut widgets));
        assert_eq!(widgets, default_widgets());
        assert!(!reset(&mut widgets));
        let mut empty = Widgets::new();
        assert!(reset(&mut empty));
    }
}
