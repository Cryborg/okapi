//! The layout of the dashboard: its widgets in order, each with its size.
//! Which widgets exist is the interface's knowledge: here a widget is only
//! its `kind`, kept as written.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{StoreError, file};

const DASHBOARD_FILE: &str = "dashboard.json";
/// The layout format this Okapi reads.
const DASHBOARD_VERSION: u32 = 1;
/// The entries left out that are traced one by one; beyond, only counted.
const TRACED_LEFT_OUT: usize = 10;

/// How much of the grid a widget takes, in columns by rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WidgetSize {
    /// One column, one row.
    Small,
    /// Two columns, one row.
    Wide,
    /// One column, two rows.
    Tall,
    /// Two columns, two rows.
    Large,
    /// Every column, one row.
    Full,
}

impl WidgetSize {
    pub const ALL: [Self; 5] = [Self::Small, Self::Wide, Self::Tall, Self::Large, Self::Full];
}

/// One widget of the layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardWidget {
    /// Which widget, by the interface's stable name (`disks`).
    pub kind: String,
    pub size: WidgetSize,
}

/// The widgets of the dashboard, in the order they are shown.
// No `Default`: an empty layout is a dashboard the user emptied, never the
// interface's default (which is `None` from `load`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DashboardLayout {
    pub widgets: Vec<DashboardWidget>,
}

/// A layout read back, and how many of its entries were left out: saving it
/// again drops them for good, which the interface must not do unsaid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedLayout {
    pub layout: DashboardLayout,
    pub left_out: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayout {
    #[expect(dead_code, reason = "checked by `file::check_version` first")]
    version: u32,
    widgets: Vec<serde_json::Value>,
}

impl DashboardLayout {
    /// The file the layout of `directory` lives in.
    #[must_use]
    pub fn path(directory: &Path) -> PathBuf {
        directory.join(DASHBOARD_FILE)
    }

    /// Reads the layout kept in `directory`; `None` when there is none (the
    /// interface shows its default). An entry that cannot be understood (an
    /// unknown size, a missing or unknown field, not an object) is left out
    /// (traced, and counted in [`LoadedLayout::left_out`]), the others kept
    /// in their order; whether the interface
    /// knows each `kind`, or holds one twice, is for it to judge. When no
    /// entry is understood (there being one at least), the file is invalid: never an
    /// emptied dashboard in place of the default.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unreadable`], [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`], [`StoreError::TooLarge`],
    /// [`StoreError::UnsupportedVersion`] or [`StoreError::Invalid`] (not
    /// JSON, no `version`, an unknown field, no `widgets`, no entry
    /// understood).
    pub fn load(directory: &Path) -> Result<Option<LoadedLayout>, StoreError> {
        let path = Self::path(directory);
        let Some(raw) = file::read_versioned::<RawLayout>(&path, DASHBOARD_VERSION)? else {
            tracing::info!(path = %path.display(), "no dashboard layout");
            return Ok(None);
        };
        let mut widgets = Vec::with_capacity(raw.widgets.len());
        let (mut skipped, mut first_error) = (0_usize, None);
        for (index, entry) in raw.widgets.into_iter().enumerate() {
            match serde_json::from_value(entry) {
                Ok(widget) => widgets.push(widget),
                Err(error) => {
                    skipped += 1;
                    if skipped <= TRACED_LEFT_OUT {
                        tracing::warn!(
                            path = %path.display(),
                            index,
                            %error,
                            "dashboard widget not understood, left out"
                        );
                    }
                    first_error.get_or_insert(error);
                }
            }
        }
        if widgets.is_empty()
            && let Some(error) = first_error
        {
            return Err(StoreError::Invalid { path, error });
        }
        tracing::info!(
            path = %path.display(),
            widgets = widgets.len(),
            skipped,
            "dashboard layout loaded"
        );
        Ok(Some(LoadedLayout {
            layout: Self { widgets },
            left_out: skipped,
        }))
    }

    /// Keeps the layout in `directory`, replacing the one there whole.
    ///
    /// # Errors
    ///
    /// Those of an atomic write: [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`] (a link there is never written through) or
    /// [`StoreError::Unwritable`].
    pub fn save(&self, directory: &Path) -> Result<(), StoreError> {
        file::write_versioned(directory, DASHBOARD_FILE, DASHBOARD_VERSION, self)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn widget(kind: &str, size: WidgetSize) -> DashboardWidget {
        DashboardWidget {
            kind: kind.to_owned(),
            size,
        }
    }

    #[test]
    fn every_size_is_listed() {
        for size in WidgetSize::ALL {
            // A size added later fails here until it is listed.
            match size {
                WidgetSize::Small
                | WidgetSize::Wide
                | WidgetSize::Tall
                | WidgetSize::Large
                | WidgetSize::Full => {}
            }
        }
        let distinct: std::collections::BTreeSet<String> = WidgetSize::ALL
            .iter()
            .map(|size| format!("{size:?}"))
            .collect();
        assert_eq!(distinct.len(), 5);
    }

    fn load(text: &str) -> Result<Option<LoadedLayout>, StoreError> {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(DASHBOARD_FILE), text).unwrap();
        DashboardLayout::load(directory.path())
    }

    fn whole(layout: DashboardLayout) -> LoadedLayout {
        LoadedLayout {
            layout,
            left_out: 0,
        }
    }

    #[test]
    fn a_layout_comes_back_as_it_was_kept() {
        let directory = tempfile::tempdir().unwrap();
        let layout = DashboardLayout {
            widgets: vec![
                widget("reclaimable", WidgetSize::Full),
                widget("disks", WidgetSize::Wide),
                widget("memory", WidgetSize::Small),
                widget("processor", WidgetSize::Tall),
                widget("system", WidgetSize::Large),
            ],
        };
        layout.save(directory.path()).unwrap();
        assert_eq!(
            DashboardLayout::load(directory.path()).unwrap(),
            Some(whole(layout))
        );
    }

    #[test]
    fn the_file_has_the_documented_form() {
        let directory = tempfile::tempdir().unwrap();
        DashboardLayout {
            widgets: vec![
                widget("disks", WidgetSize::Wide),
                widget("memory", WidgetSize::Full),
            ],
        }
        .save(directory.path())
        .unwrap();
        let text = fs::read_to_string(directory.path().join(DASHBOARD_FILE)).unwrap();
        assert!(text.ends_with("}\n"), "{text}");
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "version": 1,
                "widgets": [
                    {"kind": "disks", "size": "wide"},
                    {"kind": "memory", "size": "full"},
                ],
            })
        );
        for (size, name) in [
            (WidgetSize::Small, "small"),
            (WidgetSize::Tall, "tall"),
            (WidgetSize::Large, "large"),
        ] {
            assert_eq!(serde_json::to_value(size).unwrap(), name);
        }
    }

    #[test]
    fn no_file_is_no_layout_and_an_empty_one_is_a_layout() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(DashboardLayout::load(directory.path()).unwrap(), None);
        assert_eq!(
            load(r#"{"version": 1, "widgets": []}"#).unwrap(),
            Some(whole(DashboardLayout {
                widgets: Vec::new()
            }))
        );
    }

    #[test]
    fn a_widget_not_understood_is_left_out_and_the_others_kept_in_order() {
        let layout = load(
            r#"{"version": 1, "widgets": [
                {"kind": "disks", "size": "wide"},
                {"kind": "memory", "size": "huge"},
                {"size": "small"},
                {"kind": "processor", "size": "small", "colour": "red"},
                "system",
                {"kind": "reclaimable", "size": "full"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            layout,
            Some(LoadedLayout {
                layout: DashboardLayout {
                    widgets: vec![
                        widget("disks", WidgetSize::Wide),
                        widget("reclaimable", WidgetSize::Full),
                    ],
                },
                left_out: 4,
            })
        );
    }

    #[test]
    fn every_widget_left_out_is_counted_beyond_those_traced() {
        let mut entries = vec![r#"{"kind": "disks", "size": "wide"}"#.to_owned()];
        entries.extend((0..=TRACED_LEFT_OUT).map(|index| format!(r#"{{"kind": "k{index}"}}"#)));
        let loaded = load(&format!(
            r#"{{"version": 1, "widgets": [{}]}}"#,
            entries.join(",")
        ))
        .unwrap()
        .unwrap();
        assert_eq!(loaded.left_out, TRACED_LEFT_OUT + 1);
        assert_eq!(loaded.layout.widgets, [widget("disks", WidgetSize::Wide)]);
    }

    #[test]
    fn a_layout_of_which_nothing_is_understood_is_invalid_not_emptied() {
        assert!(matches!(
            load(r#"{"version": 1, "widgets": [{"kind": "disks", "size": "huge"}, 3]}"#),
            Err(StoreError::Invalid { .. })
        ));
    }

    #[test]
    fn a_kind_twice_is_kept_twice_for_the_interface_to_judge() {
        let layout = load(
            r#"{"version": 1, "widgets": [
                {"kind": "disks", "size": "tall"},
                {"kind": "disks", "size": "wide"}
            ]}"#,
        )
        .unwrap()
        .unwrap()
        .layout;
        assert_eq!(
            layout.widgets,
            [
                widget("disks", WidgetSize::Tall),
                widget("disks", WidgetSize::Wide)
            ]
        );
    }

    #[test]
    fn a_mistake_in_the_file_is_an_error_never_no_layout() {
        for text in [
            r#"{"version": 1}"#,
            r#"{"version": 1, "widgets": [], "columns": 3}"#,
            r#"{"widgets": []}"#,
            "not json",
        ] {
            assert!(
                matches!(load(text), Err(StoreError::Invalid { .. })),
                "{text}"
            );
        }
        assert!(matches!(
            load(r#"{"version": 2, "widgets": [], "pages": []}"#),
            Err(StoreError::UnsupportedVersion {
                version: 2,
                supported: DASHBOARD_VERSION,
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_layout_behind_a_link_that_leads_nowhere_is_never_taken_for_none() {
        let directory = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            directory.path().join("dotfiles/dashboard.json"),
            directory.path().join(DASHBOARD_FILE),
        )
        .unwrap();
        assert!(matches!(
            DashboardLayout::load(directory.path()),
            Err(StoreError::BrokenLink { .. })
        ));
    }
}
