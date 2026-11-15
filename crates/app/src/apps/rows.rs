//! What each line of the applications screen says, and their order: by the
//! space each takes in all (the application, its data, its old versions),
//! the largest first; filtered by source and by what is searched.

use okapi_tools::apps::measure::Measured;
use okapi_tools::apps::{App, AppKey, AppSize, Source};

use crate::shown::size;
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

    /// All it takes, as known so far.
    pub(crate) fn total(&self) -> u64 {
        self.app_bytes()
            .unwrap_or(0)
            .saturating_add(self.data_bytes())
            .saturating_add(self.app.old_bytes)
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

/// The keys of `entries`, the largest in all first, then by name.
pub(crate) fn ordered<'a>(entries: impl Iterator<Item = &'a Entry>) -> Vec<AppKey> {
    let mut sorted: Vec<&Entry> = entries.collect();
    sorted.sort_by(|left, right| {
        right
            .total()
            .cmp(&left.total())
            .then_with(|| {
                left.app
                    .name
                    .to_lowercase()
                    .cmp(&right.app.name.to_lowercase())
            })
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
        app_known: app_bytes.is_some(),
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
        let order = ordered([&big_app, &small_app_big_data, &old].into_iter());
        let names: Vec<&str> = order.iter().map(|key| key.id.as_str()).collect();
        assert_eq!(names, ["data", "old", "big"]);
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
            ordered([&snap, &deb].into_iter()),
            ordered([&deb, &snap].into_iter())
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
    fn the_search_reads_the_name_and_the_package_and_the_filter_the_source() {
        let gimp = Entry::new(app(Source::Deb, "GIMP", 1, false));
        assert!(gimp.matches("", ui::AppsFilter::All));
        assert!(gimp.matches(" gi ", ui::AppsFilter::Deb));
        assert!(gimp.matches("PKG-g", ui::AppsFilter::All), "its package");
        assert!(!gimp.matches("gimp", ui::AppsFilter::Snap));
        assert!(!gimp.matches("photoshop", ui::AppsFilter::All));
    }
}
