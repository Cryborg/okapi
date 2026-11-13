//! The widgets the dashboard knows: each one's stable name in the layout,
//! the sizes it is drawn in, and what it needs read at each size.

use std::collections::BTreeSet;

use okapi_store::WidgetSize;
use okapi_tools::monitor::Source;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WidgetKind {
    Reclaimable,
    Disks,
    Memory,
    Processor,
    System,
}

impl WidgetKind {
    pub(crate) const ALL: [Self; 5] = [
        Self::Reclaimable,
        Self::Disks,
        Self::Memory,
        Self::Processor,
        Self::System,
    ];

    /// The name the layout keeps it by: never changed once released.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Reclaimable => "reclaimable",
            Self::Disks => "disks",
            Self::Memory => "memory",
            Self::Processor => "processor",
            Self::System => "system",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Whether it is drawn at `size`.
    pub(crate) const fn accepts(self, size: WidgetSize) -> bool {
        match self {
            Self::Reclaimable => matches!(size, WidgetSize::Wide | WidgetSize::Full),
            Self::Disks | Self::Memory => !matches!(size, WidgetSize::Full),
            Self::Processor => matches!(size, WidgetSize::Small | WidgetSize::Wide),
            Self::System => matches!(size, WidgetSize::Small),
        }
    }

    /// What it shows at `size` needs read: the memory widget lists the
    /// applications only from Wide on.
    pub(crate) fn sources(self, size: WidgetSize) -> BTreeSet<Source> {
        match self {
            Self::Reclaimable => BTreeSet::new(),
            Self::Disks => BTreeSet::from([Source::Volumes]),
            Self::Memory if size == WidgetSize::Small => BTreeSet::from([Source::Memory]),
            Self::Memory => BTreeSet::from([Source::Memory, Source::Applications]),
            Self::Processor => BTreeSet::from([Source::Processor]),
            Self::System => BTreeSet::from([Source::System]),
        }
    }
}

/// The size `kind` comes at, in the default dashboard as when added.
pub(crate) const fn starting_size(kind: WidgetKind) -> WidgetSize {
    match kind {
        WidgetKind::Reclaimable => WidgetSize::Full,
        WidgetKind::Disks | WidgetKind::Processor => WidgetSize::Wide,
        WidgetKind::Memory | WidgetKind::System => WidgetSize::Small,
    }
}

/// The dashboard shown when none is kept: every widget, in the catalog's
/// order, at its starting size, as the mockup lays them out.
pub(crate) fn default_widgets() -> Vec<(WidgetKind, WidgetSize)> {
    WidgetKind::ALL
        .into_iter()
        .map(|kind| (kind, starting_size(kind)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [WidgetSize; 5] = WidgetSize::ALL;

    #[test]
    fn each_widget_is_found_again_by_its_own_name_and_no_other() {
        let names: BTreeSet<&str> = WidgetKind::ALL.iter().map(|kind| kind.name()).collect();
        assert_eq!(names.len(), WidgetKind::ALL.len(), "names unique");
        for kind in WidgetKind::ALL {
            assert_eq!(WidgetKind::named(kind.name()), Some(kind));
        }
        assert_eq!(WidgetKind::named("cpu"), None);
        assert_eq!(WidgetKind::named("Disks"), None, "names are exact");
    }

    #[test]
    fn each_widget_takes_the_sizes_the_mockup_draws_it_in() {
        let accepted = |kind: WidgetKind| -> Vec<WidgetSize> {
            SIZES
                .into_iter()
                .filter(|size| kind.accepts(*size))
                .collect()
        };
        assert_eq!(
            accepted(WidgetKind::Reclaimable),
            [WidgetSize::Wide, WidgetSize::Full]
        );
        assert_eq!(accepted(WidgetKind::Disks), &SIZES[..4]);
        assert_eq!(accepted(WidgetKind::Memory), &SIZES[..4]);
        assert_eq!(
            accepted(WidgetKind::Processor),
            [WidgetSize::Small, WidgetSize::Wide]
        );
        assert_eq!(accepted(WidgetKind::System), [WidgetSize::Small]);
    }

    #[test]
    fn the_default_dashboard_has_every_widget_once_at_a_size_it_takes() {
        let widgets = default_widgets();
        assert_eq!(widgets.len(), WidgetKind::ALL.len());
        for kind in WidgetKind::ALL {
            let sizes: Vec<_> = widgets
                .iter()
                .filter(|(shown, _)| *shown == kind)
                .map(|(_, size)| *size)
                .collect();
            assert_eq!(sizes.len(), 1, "{kind:?} once");
            assert!(kind.accepts(sizes[0]), "{kind:?} at a size it takes");
        }
    }

    #[test]
    fn the_default_dashboard_is_the_mockup_s() {
        assert_eq!(
            default_widgets(),
            [
                (WidgetKind::Reclaimable, WidgetSize::Full),
                (WidgetKind::Disks, WidgetSize::Wide),
                (WidgetKind::Memory, WidgetSize::Small),
                (WidgetKind::Processor, WidgetSize::Wide),
                (WidgetKind::System, WidgetSize::Small),
            ]
        );
    }

    #[test]
    fn the_memory_widget_reads_the_applications_only_when_it_lists_them() {
        assert_eq!(
            WidgetKind::Memory.sources(WidgetSize::Small),
            BTreeSet::from([Source::Memory])
        );
        for size in [WidgetSize::Wide, WidgetSize::Tall, WidgetSize::Large] {
            assert_eq!(
                WidgetKind::Memory.sources(size),
                BTreeSet::from([Source::Memory, Source::Applications])
            );
        }
        assert!(WidgetKind::Reclaimable.sources(WidgetSize::Full).is_empty());
    }

    #[test]
    fn each_other_widget_reads_its_own_source_at_every_size() {
        for (kind, source) in [
            (WidgetKind::Disks, Source::Volumes),
            (WidgetKind::Processor, Source::Processor),
            (WidgetKind::System, Source::System),
        ] {
            for size in SIZES.into_iter().filter(|size| kind.accepts(*size)) {
                assert_eq!(kind.sources(size), BTreeSet::from([source]), "{kind:?}");
            }
        }
    }
}
