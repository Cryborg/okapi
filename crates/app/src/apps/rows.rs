//! What each line of the applications screen says, and their order: by the
//! column its header sorts by, at first the space each takes in all (the
//! application, its data, its old versions), the largest first; filtered by
//! source and by what is searched.

use okapi_tools::apps::measure::Measured;
use okapi_tools::apps::{App, AppKey, AppSize, Source};

use crate::shown::size;
use crate::sorting::{self, Column, Sorting};
use crate::ui;

/// An application and what was measured of it so far.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub app: App,
    /// Its deployment (a flatpak's), once measured.
    pub deployment: Option<Measured>,
    /// Its data folder, once measured.
    pub data: Option<Measured>,
}

impl Entry {
    pub(crate) const fn new(app: App) -> Self {
        Self {
            app,
            deployment: None,
            data: None,
        }
    }

    /// What the application itself takes, once known.
    pub(crate) fn app_bytes(&self) -> Option<u64> {
        match (&self.app.size, self.deployment) {
            (AppSize::Known(bytes), _) => Some(*bytes),
            (AppSize::ToMeasure(_), Some(Measured::Bytes { bytes, .. })) => Some(bytes),
            (AppSize::Unknown, _)
            | (AppSize::ToMeasure(_), None | Some(Measured::Absent | Measured::NotMeasured)) => {
                None
            }
        }
    }

    /// What is known of what the application itself takes.
    pub(crate) fn app_space(&self) -> ui::AppSpace {
        if self.app_bytes().is_some() {
            ui::AppSpace::Known
        } else if matches!(self.app.size, AppSize::ToMeasure(_)) && self.deployment.is_none() {
            ui::AppSpace::Measuring
        } else {
            ui::AppSpace::Unknown
        }
    }

    /// Its folders still waiting once the reading ended: said not measured,
    /// and how many.
    pub(crate) fn never_measured(&mut self) -> usize {
        let mut never = 0;
        if self.app.data.is_some() && self.data.is_none() {
            self.data = Some(Measured::NotMeasured);
            never += 1;
        }
        if matches!(self.app.size, AppSize::ToMeasure(_)) && self.deployment.is_none() {
            self.deployment = Some(Measured::NotMeasured);
            never += 1;
        }
        never
    }

    /// What is known of its data.
    pub(crate) fn data(&self) -> ui::AppData {
        if self.app.data.is_none() {
            return ui::AppData::Unknown;
        }
        match self.data {
            None => ui::AppData::Measuring,
            Some(Measured::Absent) => ui::AppData::None,
            Some(Measured::Bytes { partial: false, .. }) => ui::AppData::Measured,
            Some(Measured::Bytes { partial: true, .. }) => ui::AppData::Partial,
            Some(Measured::NotMeasured) => ui::AppData::NotMeasured,
        }
    }

    pub(crate) const fn data_bytes(&self) -> u64 {
        match self.data {
            Some(Measured::Bytes { bytes, .. }) => bytes,
            _ => 0,
        }
    }

    /// What its data takes, once known: unknown when it has no data folder,
    /// is being measured or could not be; zero when its folder is absent; a
    /// partial measure as shown.
    fn data_known(&self) -> Option<u64> {
        match self.data() {
            ui::AppData::Measured | ui::AppData::Partial => Some(self.data_bytes()),
            ui::AppData::None => Some(0),
            ui::AppData::Unknown | ui::AppData::Measuring | ui::AppData::NotMeasured => None,
        }
    }

    /// All it takes, as known so far.
    pub(crate) fn total(&self) -> u64 {
        self.app_bytes()
            .unwrap_or(0)
            .saturating_add(self.data_bytes())
            .saturating_add(self.app.old_bytes)
    }

    /// All it takes, once anything of it is known (shown only then): nothing
    /// measured yet, its total is neither zero nor anything else.
    pub(crate) fn total_known(&self) -> Option<u64> {
        let total = self.total();
        (self.app_bytes().is_some() || total > 0).then_some(total)
    }

    /// Its total leaves out what is not known yet, or counts what was read
    /// only in part.
    pub(crate) fn total_partial(&self) -> bool {
        self.app_space() != ui::AppSpace::Known
            || matches!(self.deployment, Some(Measured::Bytes { partial: true, .. }))
            || matches!(
                self.data(),
                ui::AppData::Measuring | ui::AppData::NotMeasured | ui::AppData::Partial
            )
    }

    /// Whether it is shown with `search` (its name or its package's, in any
    /// case) and `filter`.
    pub(crate) fn matches(&self, search: &str, filter: ui::AppsFilter) -> bool {
        let source = match filter {
            ui::AppsFilter::All => true,
            ui::AppsFilter::Deb => self.app.source == Source::Deb,
            ui::AppsFilter::Snap => self.app.source == Source::Snap,
            ui::AppsFilter::Flatpak => self.app.source == Source::Flatpak,
        };
        let search = search.trim().to_lowercase();
        source
            && (search.is_empty()
                || self.app.name.to_lowercase().contains(&search)
                || self.app.package.to_lowercase().contains(&search))
    }
}

/// How the list is sorted.
pub(crate) type Sort = Sorting<ui::AppsSort>;

impl Column for ui::AppsSort {
    /// The largest first; names from A.
    fn descending_first(self) -> bool {
        self != Self::Name
    }
}

/// The order the list shows first: the largest in all first.
pub(crate) fn first_sort() -> Sort {
    Sorting::by(ui::AppsSort::Total)
}

/// The keys of `entries` in `sort`'s order, a size not known yet after the
/// known whichever way; by name between equals, then by source.
pub(crate) fn ordered<'a>(entries: impl Iterator<Item = &'a Entry>, sort: Sort) -> Vec<AppKey> {
    let mut sorted: Vec<&Entry> = entries.collect();
    sorted.sort_by(|left, right| {
        let by_name = || sorting::by_name(&left.app.name, &right.app.name);
        match sort.column {
            ui::AppsSort::Name => sort.directed(by_name()),
            ui::AppsSort::App => sort.known_first(left.app_bytes(), right.app_bytes()),
            ui::AppsSort::Data => sort.known_first(left.data_known(), right.data_known()),
            ui::AppsSort::Total => sort.known_first(left.total_known(), right.total_known()),
        }
        .then_with(by_name)
        // The same name in two sources: always in the same order.
        .then_with(|| left.app.key().cmp(&right.app.key()))
    });
    sorted.iter().map(|entry| entry.app.key()).collect()
}

pub(crate) const fn source(source: Source) -> ui::AppSource {
    match source {
        Source::Deb => ui::AppSource::Deb,
        Source::Snap => ui::AppSource::Snap,
        Source::Flatpak => ui::AppSource::Flatpak,
    }
}

/// The line of `entry`, its bar relative to `largest`.
pub(crate) fn line(
    entry: &Entry,
    icon: Option<&slint::Image>,
    largest: u64,
    uninstalling: bool,
) -> ui::AppLine {
    let app_bytes = entry.app_bytes();
    #[expect(
        clippy::cast_precision_loss,
        reason = "a bar's length: a part of a byte is never seen"
    )]
    let bar = if largest == 0 {
        0.0
    } else {
        entry.total() as f32 / largest as f32
    };
    ui::AppLine {
        name: entry.app.name.as_str().into(),
        source: source(entry.app.source),
        icon: icon.cloned().unwrap_or_default(),
        has_icon: icon.is_some(),
        app_space: entry.app_space(),
        app_size: size(app_bytes.unwrap_or(0)),
        data: entry.data(),
        data_size: size(entry.data_bytes()),
        has_old: entry.app.old_bytes > 0,
        old_size: size(entry.app.old_bytes),
        system: entry.app.system,
        uninstalling,
        bar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn app(source: Source, name: &str, app_bytes: u64, data: bool) -> App {
        App {
            source,
            id: name.to_lowercase(),
            package: format!("pkg-{}", name.to_lowercase()),
            name: name.to_owned(),
            aliases: Vec::new(),
            launchers: Vec::new(),
            version: String::new(),
            publisher: None,
            origin: None,
            icon: None,
            size: AppSize::Known(app_bytes),
            old_bytes: 0,
            old_versions: 0,
            data: data.then(|| std::path::PathBuf::from("/home/u/snap/x")),
            system: false,
            target: None,
        }
    }

    #[test]
    fn the_largest_in_all_comes_first_data_and_old_versions_counted() {
        let small_app_big_data = Entry {
            data: Some(Measured::Bytes {
                bytes: 30,
                partial: false,
            }),
            ..Entry::new(app(Source::Snap, "Data", 10, true))
        };
        let big_app = Entry::new(app(Source::Deb, "Big", 35, false));
        let old = Entry::new(App {
            old_bytes: 25,
            ..app(Source::Snap, "Old", 11, false)
        });
        let order = ordered(
            [&big_app, &small_app_big_data, &old].into_iter(),
            first_sort(),
        );
        let names: Vec<&str> = order.iter().map(|key| key.id.as_str()).collect();
        assert_eq!(names, ["data", "old", "big"]);
    }

    #[test]
    fn each_column_sorts_either_way_what_is_not_known_last() {
        let measured = |bytes| {
            Some(Measured::Bytes {
                bytes,
                partial: false,
            })
        };
        // By name: Big, data, Measuring, Old. By application: Big 35, Old
        // 11, data 10, Measuring unknown. By data: data 30, Old none (0),
        // Big and Measuring unknown. In all: data 40, Old 36, Big 35,
        // Measuring unknown.
        let data = Entry {
            data: measured(30),
            ..Entry::new(app(Source::Snap, "data", 10, true))
        };
        let big = Entry::new(app(Source::Deb, "Big", 35, false));
        let old = Entry {
            data: Some(Measured::Absent),
            ..Entry::new(App {
                old_bytes: 25,
                ..app(Source::Snap, "Old", 11, true)
            })
        };
        let measuring = Entry::new(App {
            size: AppSize::ToMeasure(std::path::PathBuf::from("/deployment")),
            ..app(Source::Flatpak, "Measuring", 0, true)
        });
        let names = |sort: Sort| -> Vec<String> {
            ordered([&big, &measuring, &data, &old].into_iter(), sort)
                .into_iter()
                .map(|key| key.id)
                .collect()
        };
        // Its application unknown, its old versions are known.
        let ghost = Entry::new(App {
            size: AppSize::Unknown,
            old_bytes: 5,
            ..app(Source::Deb, "Ghost", 0, false)
        });
        assert_eq!(ghost.total_known(), Some(5));
        assert_eq!(measuring.total_known(), None);
        // Measured empty: known, nothing.
        let empty = Entry::new(app(Source::Deb, "Empty", 0, false));
        assert_eq!(empty.total_known(), Some(0));
        // A partial measure counts as shown; one never made, unknown.
        let data_of = |data| Entry {
            data: Some(data),
            ..Entry::new(app(Source::Snap, "X", 1, true))
        };
        assert_eq!(
            data_of(Measured::Bytes {
                bytes: 20,
                partial: true
            })
            .data_known(),
            Some(20)
        );
        assert_eq!(data_of(Measured::NotMeasured).data_known(), None);
        let by = Sorting::by;
        let turned = |column| by(column).clicked(column);
        assert_eq!(names(first_sort()), ["data", "old", "big", "measuring"]);
        assert_eq!(
            names(turned(ui::AppsSort::Total)),
            ["big", "old", "data", "measuring"]
        );
        assert_eq!(
            names(by(ui::AppsSort::Name)),
            ["big", "data", "measuring", "old"]
        );
        assert_eq!(
            names(turned(ui::AppsSort::Name)),
            ["old", "measuring", "data", "big"]
        );
        assert_eq!(
            names(by(ui::AppsSort::App)),
            ["big", "old", "data", "measuring"]
        );
        assert_eq!(
            names(turned(ui::AppsSort::App)),
            ["data", "old", "big", "measuring"]
        );
        // Unknown between them: by name.
        assert_eq!(
            names(by(ui::AppsSort::Data)),
            ["data", "old", "big", "measuring"]
        );
        assert_eq!(
            names(turned(ui::AppsSort::Data)),
            ["old", "data", "big", "measuring"]
        );
    }

    #[test]
    fn what_is_known_of_the_data_is_said() {
        let entry = |data| Entry {
            data,
            ..Entry::new(app(Source::Snap, "X", 1, true))
        };
        assert_eq!(entry(None).data(), ui::AppData::Measuring);
        assert_eq!(entry(Some(Measured::Absent)).data(), ui::AppData::None);
        assert_eq!(
            entry(Some(Measured::Bytes {
                bytes: 1,
                partial: true
            }))
            .data(),
            ui::AppData::Partial
        );
        assert_eq!(
            entry(Some(Measured::NotMeasured)).data(),
            ui::AppData::NotMeasured
        );
        assert_eq!(
            Entry::new(app(Source::Deb, "D", 1, false)).data(),
            ui::AppData::Unknown
        );
    }

    #[test]
    fn a_flatpak_is_known_once_measured_never_as_nothing() {
        let flatpak = Entry::new(App {
            size: AppSize::ToMeasure(std::path::PathBuf::from("/deployment")),
            ..app(Source::Flatpak, "F", 0, false)
        });
        assert_eq!(flatpak.app_bytes(), None);
        let measured = |deployment| Entry {
            deployment: Some(deployment),
            ..flatpak.clone()
        };
        assert_eq!(
            measured(Measured::Bytes {
                bytes: 9,
                partial: false,
            })
            .app_bytes(),
            Some(9)
        );
        assert_eq!(
            measured(Measured::NotMeasured).app_bytes(),
            None,
            "unknown, never 0"
        );
        let unknown = Entry::new(App {
            size: AppSize::Unknown,
            ..app(Source::Deb, "U", 0, false)
        });
        assert_eq!(unknown.app_bytes(), None);
    }

    #[test]
    fn a_size_never_to_come_is_said_unknown_not_measuring() {
        let flatpak = Entry::new(App {
            size: AppSize::ToMeasure(std::path::PathBuf::from("/deployment")),
            ..app(Source::Flatpak, "F", 0, false)
        });
        assert_eq!(flatpak.app_space(), ui::AppSpace::Measuring);
        let measured = |deployment| Entry {
            deployment: Some(deployment),
            ..flatpak.clone()
        };
        let bytes = Measured::Bytes {
            bytes: 9,
            partial: false,
        };
        assert_eq!(measured(bytes).app_space(), ui::AppSpace::Known);
        assert_eq!(
            measured(Measured::NotMeasured).app_space(),
            ui::AppSpace::Unknown
        );
        assert_eq!(
            measured(Measured::Absent).app_space(),
            ui::AppSpace::Unknown
        );
        let deb = Entry::new(App {
            size: AppSize::Unknown,
            ..app(Source::Deb, "U", 0, false)
        });
        assert_eq!(deb.app_space(), ui::AppSpace::Unknown);
        assert_eq!(line(&deb, None, 0, false).app_space, ui::AppSpace::Unknown);
    }

    #[test]
    fn a_total_is_shown_once_anything_is_known_and_said_partial_when_some_is_not() {
        let measured = |bytes, partial| Some(Measured::Bytes { bytes, partial });
        let deb = Entry::new(App {
            size: AppSize::Unknown,
            ..app(Source::Deb, "U", 0, false)
        });
        assert_eq!(deb.total_known(), None, "never « 0 o »");
        let unmeasured = Entry {
            deployment: Some(Measured::NotMeasured),
            data: Some(Measured::Absent),
            ..Entry::new(App {
                size: AppSize::ToMeasure(std::path::PathBuf::from("/deployment")),
                ..app(Source::Flatpak, "F", 0, true)
            })
        };
        assert_eq!(unmeasured.total_known(), None);
        let data_only = Entry {
            data: measured(4, false),
            ..Entry::new(App {
                size: AppSize::Unknown,
                ..app(Source::Snap, "S", 0, true)
            })
        };
        assert_eq!(data_only.total_known(), Some(4));
        assert!(data_only.total_partial());
        let with_data = |data| Entry {
            data,
            ..Entry::new(app(Source::Snap, "K", 3, true))
        };
        assert!(!with_data(measured(4, false)).total_partial());
        assert!(!with_data(Some(Measured::Absent)).total_partial());
        assert!(with_data(None).total_partial(), "its data being measured");
        assert!(with_data(Some(Measured::NotMeasured)).total_partial());
        assert!(with_data(measured(4, true)).total_partial());
        assert!(!Entry::new(app(Source::Deb, "D", 3, false)).total_partial());
        let partly_deployed = Entry {
            deployment: measured(5, true),
            ..unmeasured.clone()
        };
        assert!(partly_deployed.total_partial());
    }

    #[test]
    fn a_folder_never_measured_is_said_so_once_the_reading_ends() {
        let mut entry = Entry::new(App {
            size: AppSize::ToMeasure(std::path::PathBuf::from("/deployment")),
            ..app(Source::Flatpak, "F", 0, true)
        });
        assert_eq!(entry.never_measured(), 2);
        assert_eq!(entry.data(), ui::AppData::NotMeasured);
        assert_eq!(entry.never_measured(), 0, "said once");
    }

    #[test]
    fn equal_lines_keep_one_order_and_the_bar_is_relative_to_the_largest() {
        let snap = Entry::new(app(Source::Snap, "Firefox", 5, false));
        let deb = Entry::new(app(Source::Deb, "Firefox", 5, false));
        assert_eq!(
            ordered([&snap, &deb].into_iter(), first_sort()),
            ordered([&deb, &snap].into_iter(), first_sort())
        );
        let line = line(&snap, None, 20, false);
        assert!((line.bar - 0.25).abs() < f32::EPSILON, "{}", line.bar);
        assert!(!line.has_icon);
        assert!(
            (super::line(&snap, None, 0, false).bar).abs() < f32::EPSILON,
            "nothing measured"
        );
    }

    #[test]
    fn each_filter_keeps_its_source_alone_and_equal_totals_go_by_name() {
        let entries = [
            Entry::new(app(Source::Deb, "D", 1, false)),
            Entry::new(app(Source::Snap, "S", 1, false)),
            Entry::new(app(Source::Flatpak, "F", 1, false)),
        ];
        for (filter, kept) in [
            (ui::AppsFilter::All, vec!["D", "S", "F"]),
            (ui::AppsFilter::Deb, vec!["D"]),
            (ui::AppsFilter::Snap, vec!["S"]),
            (ui::AppsFilter::Flatpak, vec!["F"]),
        ] {
            let shown: Vec<&str> = entries
                .iter()
                .filter(|entry| entry.matches("", filter))
                .map(|entry| entry.app.name.as_str())
                .collect();
            assert_eq!(shown, kept, "{filter:?}");
        }
        // Equal totals: by name, whatever the case, never by key.
        let beta = Entry::new(App {
            id: "a".to_owned(),
            ..app(Source::Deb, "beta", 5, false)
        });
        let alpha = Entry::new(App {
            id: "z".to_owned(),
            ..app(Source::Deb, "Alpha", 5, false)
        });
        let order = ordered([&beta, &alpha].into_iter(), first_sort());
        assert_eq!(order[0].id, "z", "Alpha before beta");
    }

    #[test]
    fn the_search_reads_the_name_and_the_package_and_the_filter_the_source() {
        let gimp = Entry::new(app(Source::Deb, "GIMP", 1, false));
        assert!(gimp.matches("", ui::AppsFilter::All));
        assert!(gimp.matches(" gi ", ui::AppsFilter::Deb));
        assert!(gimp.matches("PKG-g", ui::AppsFilter::All), "its package");
        assert!(!gimp.matches("gimp", ui::AppsFilter::Snap));
        assert!(!gimp.matches("photoshop", ui::AppsFilter::All));
    }
}
