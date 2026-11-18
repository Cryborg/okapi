//! The order a list's header sets (`SortHeader` in `widgets.slint`): the
//! column the list is sorted by, and which way. A click on a column sorts
//! by it its own way first; a click on the column sorted turns it the other
//! way. One column at a time, never kept from one launch to the next.

use std::cmp::Ordering;

/// A column a list may be sorted by.
pub(crate) trait Column: Copy + PartialEq {
    /// Whether a first click sorts it the largest first (sizes, counts),
    /// rather than the smallest first (names from A).
    fn descending_first(self) -> bool;
}

/// The column a list is sorted by, and which way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sorting<C> {
    pub column: C,
    pub descending: bool,
}

impl<C: Column> Sorting<C> {
    /// Sorted by `column`, its own way first.
    pub(crate) fn by(column: C) -> Self {
        Self {
            column,
            descending: column.descending_first(),
        }
    }

    /// The order once `column`'s header is clicked: the other way when it
    /// is the column sorted, its own way first otherwise.
    #[must_use]
    pub(crate) fn clicked(self, column: C) -> Self {
        if column == self.column {
            Self {
                column,
                descending: !self.descending,
            }
        } else {
            Self::by(column)
        }
    }

    /// `ascending`, how two lines compare from the smallest, the way the
    /// list is sorted.
    pub(crate) fn directed(self, ascending: Ordering) -> Ordering {
        if self.descending {
            ascending.reverse()
        } else {
            ascending
        }
    }

    /// Two values either of which may not be known yet (a size being
    /// measured): the known the way the list is sorted, the unknown after
    /// them either way.
    pub(crate) fn known_first<T: Ord>(self, left: Option<T>, right: Option<T>) -> Ordering {
        match (left, right) {
            (Some(left), Some(right)) => self.directed(left.cmp(&right)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
}

/// Two names from A, whatever their case.
pub(crate) fn by_name(left: &str, right: &str) -> Ordering {
    left.to_lowercase().cmp(&right.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Test {
        Name,
        Size,
    }

    impl Column for Test {
        fn descending_first(self) -> bool {
            self == Self::Size
        }
    }

    #[test]
    fn a_column_starts_its_own_way_and_turns_on_a_second_click() {
        let size = Sorting::by(Test::Size);
        assert!(size.descending, "a size the largest first");
        let name = size.clicked(Test::Name);
        assert_eq!((name.column, name.descending), (Test::Name, false));
        let turned = name.clicked(Test::Name);
        assert_eq!((turned.column, turned.descending), (Test::Name, true));
        assert_eq!(turned.clicked(Test::Name), name, "back the first way");
        // Another column starts its own way, whichever way the last went.
        assert_eq!(turned.clicked(Test::Size), size);
        assert_eq!(size.clicked(Test::Size).clicked(Test::Name), name);
    }

    #[test]
    fn the_direction_turns_the_order_and_the_unknown_stay_last() {
        let up = Sorting::by(Test::Name);
        let down = Sorting::by(Test::Size);
        assert_eq!(up.directed(1.cmp(&2)), Ordering::Less);
        assert_eq!(down.directed(1.cmp(&2)), Ordering::Greater);
        for sorting in [up, down] {
            let mut values = vec![None, Some(2), Some(1), None, Some(3)];
            values.sort_by(|left, right| sorting.known_first(*left, *right));
            let expected = if sorting.descending {
                [Some(3), Some(2), Some(1), None, None]
            } else {
                [Some(1), Some(2), Some(3), None, None]
            };
            assert_eq!(values, expected, "{sorting:?}");
        }
    }

    #[test]
    fn names_compare_whatever_their_case() {
        assert_eq!(by_name("beta", "Alpha"), Ordering::Greater);
        assert_eq!(by_name("GIMP", "gimp"), Ordering::Equal);
    }
}
