//! The sidebar as the user last left it, its fold and its groups closed:
//! read at launch, kept at each choice in the interface state
//! (`okapi_store::InterfaceState`), off the interface's thread.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use okapi_store::{InterfaceState, SidebarGroup};
use slint::ComponentHandle as _;

use crate::saving::Saver;
use crate::ui::{self, AppWindow};

/// The thread that writes the interface state.
const SAVER_NAME: &str = "okapi-interface-save";

/// The group of the store the window's group is.
fn kept_as(group: ui::SidebarGroup) -> SidebarGroup {
    match group {
        ui::SidebarGroup::Cleaning => SidebarGroup::Cleaning,
        ui::SidebarGroup::Software => SidebarGroup::Software,
        ui::SidebarGroup::Storage => SidebarGroup::Storage,
    }
}

/// What the window says of a group the user opened or closed, kept or not.
fn trace_group_chosen(group: ui::SidebarGroup, open: bool) {
    tracing::debug!(group = ?kept_as(group), open, "sidebar group chosen");
}

/// Whether `group` is open in `window`'s sidebar.
fn is_open(window: &AppWindow, group: SidebarGroup) -> bool {
    match group {
        SidebarGroup::Cleaning => window.get_sidebar_cleaning_open(),
        SidebarGroup::Software => window.get_sidebar_software_open(),
        SidebarGroup::Storage => window.get_sidebar_storage_open(),
    }
}

/// Opens or closes `group` in `window`'s sidebar, which says nothing to Rust.
fn set_open(window: &AppWindow, group: SidebarGroup, open: bool) {
    match group {
        SidebarGroup::Cleaning => window.set_sidebar_cleaning_open(open),
        SidebarGroup::Software => window.set_sidebar_software_open(open),
        SidebarGroup::Storage => window.set_sidebar_storage_open(open),
    }
}

/// The sidebar of `window` as the user leaves it now: each choice keeps the
/// other ones.
fn state_of(window: &AppWindow) -> InterfaceState {
    InterfaceState {
        sidebar_folded: window.get_sidebar_chosen_folded(),
        closed_groups: SidebarGroup::ALL
            .into_iter()
            .filter(|group| !is_open(window, *group))
            .collect(),
    }
}

/// Folds `window`'s sidebar and closes its groups as kept in `directory`;
/// unfolded, every group open, when the state there cannot be read.
fn restore(window: &AppWindow, directory: &Path) {
    match InterfaceState::load(directory) {
        Ok(state) => {
            window.set_sidebar_chosen_folded(state.sidebar_folded);
            for group in SidebarGroup::ALL {
                set_open(window, group, !state.closed_groups.contains(&group));
            }
        }
        Err(error) => {
            tracing::warn!(%error, "interface state unreadable: the sidebar opens unfolded, every group open");
        }
    }
}

/// Hands `saver` the state of `window`'s sidebar each time it is called.
fn keeper(window: &AppWindow, saver: Rc<RefCell<Saver<InterfaceState>>>) -> Rc<dyn Fn()> {
    let window = window.as_weak();
    Rc::new(move || {
        let Some(window) = window.upgrade() else {
            tracing::warn!("window gone: the sidebar's fold and groups not kept");
            return;
        };
        let state = state_of(&window);
        if !saver.borrow().save(state.clone()) {
            tracing::warn!(
                ?state,
                "interface state writer gone: the sidebar's fold and groups not kept"
            );
        }
    })
}

/// Keeps the user's choices of fold and of groups, until shut down; none
/// when they cannot be kept.
#[derive(Default)]
pub struct KeptSidebar(Option<Rc<RefCell<Saver<InterfaceState>>>>);

impl std::fmt::Debug for KeptSidebar {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KeptSidebar")
            .field("writing", &self.0.is_some())
            .finish()
    }
}

impl KeptSidebar {
    /// Folds `window`'s sidebar and closes its groups as the user last left
    /// them, kept in `directory`, and keeps each new choice there. A state
    /// that cannot be read leaves the sidebar unfolded, every group open,
    /// traced, and is not replaced (D-10, `InterfaceState::save`). Each
    /// group opened or closed is traced, kept or not.
    pub fn attach(window: &AppWindow, directory: Option<PathBuf>) -> Self {
        // Traced only, until a writer replaces it.
        window.on_sidebar_group_chosen(trace_group_chosen);
        let Some(directory) = directory else {
            tracing::warn!("no state directory: the sidebar's fold and groups are not kept");
            return Self::default();
        };
        restore(window, &directory);
        let saver = match Saver::start(
            SAVER_NAME,
            // Whether it is kept: the store traces why not.
            Box::new(move |state: &InterfaceState| state.save(&directory).is_ok()),
        ) {
            Ok(saver) => Rc::new(RefCell::new(saver)),
            Err(error) => {
                tracing::error!(%error, "interface state writer not started: the sidebar's fold and groups not kept");
                return Self::default();
            }
        };
        let keep = keeper(window, Rc::clone(&saver));
        let folding = Rc::clone(&keep);
        window.on_sidebar_fold_chosen(move |_| folding());
        window.on_sidebar_group_chosen(move |group, open| {
            trace_group_chosen(group, open);
            keep();
        });
        Self(Some(saver))
    }

    /// Lets the last choice be written, and waits for it a little.
    pub fn shut_down(&self) {
        if let Some(saver) = &self.0 {
            saver.borrow_mut().shut_down();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use okapi_rules::Language;

    use super::*;
    use crate::cleaner::tests::{button, resize, seams_never_scanning};

    /// A window as the app opens it, its sidebar kept in `directory` before
    /// its first width, then wide. The testing platform is set up once per
    /// test.
    fn launched(directory: &Path) -> (AppWindow, KeptSidebar) {
        let crate::Screens { window, .. } = crate::window(
            seams_never_scanning(),
            crate::dashboard::tests::inert(),
            Language::French,
        )
        .unwrap();
        let kept = KeptSidebar::attach(&window, Some(directory.to_owned()));
        resize(&window, 1280);
        (window, kept)
    }

    fn folded(window: &AppWindow) -> bool {
        button(window, "Accueil").size().width < 50.0
    }

    fn state(folded: bool, closed: &[SidebarGroup]) -> InterfaceState {
        InterfaceState {
            sidebar_folded: folded,
            closed_groups: closed.iter().copied().collect(),
        }
    }

    /// Opens or closes the group titled `title`, as the user does.
    fn toggle(window: &AppWindow, title: &str) {
        button(window, title).invoke_accessible_default_action();
    }

    #[test]
    fn the_fold_chosen_is_how_the_next_launch_opens() {
        i_slint_backend_testing::init_no_event_loop();
        let directory = tempfile::tempdir().unwrap();
        let kept_in = directory.path().join("state");
        let (first, kept) = launched(&kept_in);
        assert!(!folded(&first), "unfolded at the first launch");
        button(&first, "Replier la barre latérale").invoke_accessible_default_action();
        kept.shut_down();
        assert_eq!(InterfaceState::load(&kept_in).unwrap(), state(true, &[]));

        // Written otherwise than Okapi does, a group unknown in it: a launch
        // alone writes nothing, neither the fold nor the groups set from the
        // file; the next choice drops the unknown one.
        let compact =
            r#"{"version":1,"sidebarFolded":true,"closedGroups":["security","software"]}"#;
        fs::write(InterfaceState::path(&kept_in), compact).unwrap();
        let (second, kept) = launched(&kept_in);
        assert!(second.get_sidebar_chosen_folded());
        assert!(folded(&second), "opens folded");
        assert!(!second.get_sidebar_software_open());
        assert!(second.get_sidebar_storage_open());
        kept.shut_down();
        assert_eq!(
            fs::read_to_string(InterfaceState::path(&kept_in)).unwrap(),
            compact
        );

        let (third, kept) = launched(&kept_in);
        button(&third, "Déplier la barre latérale").invoke_accessible_default_action();
        kept.shut_down();
        assert_eq!(
            InterfaceState::load(&kept_in).unwrap(),
            state(false, &[SidebarGroup::Software]),
            "the group kept with the fold"
        );
        assert!(
            !fs::read_to_string(InterfaceState::path(&kept_in))
                .unwrap()
                .contains("security")
        );
        assert!(!folded(&launched(&kept_in).0), "opens unfolded again");
    }

    #[test]
    fn the_groups_closed_are_how_the_next_launch_opens_with_the_fold() {
        i_slint_backend_testing::init_no_event_loop();
        let directory = tempfile::tempdir().unwrap();
        // A group, then the fold: the fold keeps the group.
        let (first, kept) = launched(directory.path());
        toggle(&first, "Stockage");
        button(&first, "Replier la barre latérale").invoke_accessible_default_action();
        kept.shut_down();
        assert_eq!(
            InterfaceState::load(directory.path()).unwrap(),
            state(true, &[SidebarGroup::Storage])
        );

        // The unfold, then a group: the group keeps the unfold and the
        // other group.
        let (second, kept) = launched(directory.path());
        assert!(!second.get_sidebar_storage_open());
        assert!(second.get_sidebar_software_open());
        assert!(second.get_sidebar_cleaning_open());
        button(&second, "Déplier la barre latérale").invoke_accessible_default_action();
        toggle(&second, "Logiciels");
        kept.shut_down();
        assert_eq!(
            InterfaceState::load(directory.path()).unwrap(),
            state(false, &[SidebarGroup::Software, SidebarGroup::Storage])
        );

        // Opened again, a group is kept open.
        let (third, kept) = launched(directory.path());
        toggle(&third, "Stockage");
        kept.shut_down();
        assert_eq!(
            InterfaceState::load(directory.path()).unwrap(),
            state(false, &[SidebarGroup::Software])
        );
        let (fourth, kept) = launched(directory.path());
        assert!(fourth.get_sidebar_storage_open());
        assert!(!fourth.get_sidebar_software_open());
        assert!(fourth.get_sidebar_cleaning_open());

        // Each group kept for itself: all of them closed.
        toggle(&fourth, "Nettoyage");
        toggle(&fourth, "Stockage");
        kept.shut_down();
        assert_eq!(
            InterfaceState::load(directory.path()).unwrap(),
            state(false, &SidebarGroup::ALL)
        );
        let (fifth, _kept) = launched(directory.path());
        assert!(!fifth.get_sidebar_cleaning_open());
        assert!(!fifth.get_sidebar_software_open());
        assert!(!fifth.get_sidebar_storage_open());
    }

    /// What a damage leaves: never replaced.
    const UNREADABLE: [&str; 4] = [
        "{",
        r#"{"version": 1}"#,
        r#"{"version": 1, "sidebarFolded": "yes"}"#,
        r#"{"version": 1, "sidebarFolded": true, "closedGroups": "storage"}"#,
    ];

    #[test]
    fn a_state_that_cannot_be_read_is_never_replaced() {
        i_slint_backend_testing::init_no_event_loop();
        for text in UNREADABLE {
            let directory = tempfile::tempdir().unwrap();
            fs::write(InterfaceState::path(directory.path()), text).unwrap();
            let (window, kept) = launched(directory.path());
            assert!(!folded(&window), "opens unfolded: {text}");
            assert!(
                window.get_sidebar_storage_open(),
                "every group open: {text}"
            );
            toggle(&window, "Stockage");
            assert!(
                !window.get_sidebar_storage_open(),
                "it still closes: {text}"
            );
            button(&window, "Replier la barre latérale").invoke_accessible_default_action();
            assert!(folded(&window), "it still folds: {text}");
            kept.shut_down();
            assert_eq!(
                fs::read_to_string(InterfaceState::path(directory.path())).unwrap(),
                text
            );
        }
    }

    #[test]
    fn a_state_made_unreadable_after_the_launch_is_never_replaced() {
        i_slint_backend_testing::init_no_event_loop();
        for text in UNREADABLE {
            let directory = tempfile::tempdir().unwrap();
            let path = InterfaceState::path(directory.path());
            let (window, kept) = launched(directory.path());
            // Damaged while the window is open.
            fs::write(&path, text).unwrap();
            toggle(&window, "Logiciels");
            button(&window, "Replier la barre latérale").invoke_accessible_default_action();
            kept.shut_down();
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn a_narrow_window_never_writes_nor_changes_the_choice() {
        i_slint_backend_testing::init_no_event_loop();
        let directory = tempfile::tempdir().unwrap();
        let (window, kept) = launched(directory.path());
        resize(&window, 1000);
        assert!(folded(&window));
        resize(&window, 1280);
        kept.shut_down();
        assert!(!InterfaceState::path(directory.path()).exists());
        let (again, _kept) = launched(directory.path());
        assert!(!folded(&again));
        assert_eq!(state_of(&again).closed_groups, BTreeSet::new());
    }

    #[test]
    fn without_a_state_directory_the_sidebar_still_folds_and_closes() {
        i_slint_backend_testing::init_no_event_loop();
        let crate::Screens { window, .. } = crate::window(
            seams_never_scanning(),
            crate::dashboard::tests::inert(),
            Language::French,
        )
        .unwrap();
        let kept = KeptSidebar::attach(&window, None);
        resize(&window, 1280);
        toggle(&window, "Nettoyage");
        assert!(!window.get_sidebar_cleaning_open());
        button(&window, "Replier la barre latérale").invoke_accessible_default_action();
        assert!(folded(&window));
        kept.shut_down();
    }
}
