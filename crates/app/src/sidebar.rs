//! The sidebar's fold, as the user last chose it: read at launch, kept at
//! each choice in the interface state (`okapi_store::InterfaceState`), off
//! the interface's thread.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use okapi_store::InterfaceState;

use crate::saving::Saver;
use crate::ui::AppWindow;

/// The thread that writes the interface state.
const SAVER_NAME: &str = "okapi-interface-save";

/// Keeps the user's choices of fold, until shut down; none when they cannot
/// be kept.
#[derive(Default)]
pub struct KeptFold(Option<Rc<RefCell<Saver<InterfaceState>>>>);

impl std::fmt::Debug for KeptFold {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KeptFold")
            .field("writing", &self.0.is_some())
            .finish()
    }
}

impl KeptFold {
    /// Folds `window`'s sidebar as the user last left it, kept in
    /// `directory`, and keeps each new choice there. A state that cannot be
    /// read leaves the sidebar unfolded, traced, and is not replaced (D-10,
    /// `InterfaceState::save`).
    pub fn attach(window: &AppWindow, directory: Option<PathBuf>) -> Self {
        let Some(directory) = directory else {
            tracing::warn!("no state directory: the sidebar's fold is not kept");
            return Self::default();
        };
        match InterfaceState::load(&directory) {
            Ok(state) => window.set_sidebar_chosen_folded(state.sidebar_folded),
            Err(error) => {
                tracing::warn!(%error, "interface state unreadable: the sidebar opens unfolded");
            }
        }
        let saver = match Saver::start(
            SAVER_NAME,
            // Whether it is kept: the store traces why not.
            Box::new(move |state: &InterfaceState| state.save(&directory).is_ok()),
        ) {
            Ok(saver) => Rc::new(RefCell::new(saver)),
            Err(error) => {
                tracing::error!(%error, "interface state writer not started: the sidebar's fold not kept");
                return Self::default();
            }
        };
        let handing = Rc::clone(&saver);
        window.on_sidebar_fold_chosen(move |folded| {
            let state = InterfaceState {
                sidebar_folded: folded,
            };
            if !handing.borrow().save(state) {
                tracing::warn!(
                    folded,
                    "interface state writer gone: the sidebar's fold not kept"
                );
            }
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
    use std::fs;
    use std::path::Path;

    use okapi_rules::Language;

    use super::*;
    use crate::cleaner::tests::{button, resize, seams_never_scanning};

    /// A window as the app opens it, its sidebar's fold kept in `directory`
    /// before its first width, then wide. The testing platform is set up
    /// once per test.
    fn launched(directory: &Path) -> (AppWindow, KeptFold) {
        let (window, ..) = crate::window(
            seams_never_scanning(),
            crate::dashboard::tests::inert(),
            Language::French,
        )
        .unwrap();
        let kept = KeptFold::attach(&window, Some(directory.to_owned()));
        resize(&window, 1280);
        (window, kept)
    }

    fn folded(window: &AppWindow) -> bool {
        button(window, "Accueil").size().width < 50.0
    }

    #[test]
    fn the_fold_chosen_is_how_the_next_launch_opens() {
        i_slint_backend_testing::init_no_event_loop();
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let (first, kept) = launched(&state);
        assert!(!folded(&first), "unfolded at the first launch");
        button(&first, "Replier la barre latérale").invoke_accessible_default_action();
        kept.shut_down();
        assert_eq!(
            InterfaceState::load(&state).unwrap(),
            InterfaceState {
                sidebar_folded: true
            }
        );

        // Written otherwise than Okapi does: a launch alone writes nothing.
        let compact = r#"{"version":1,"sidebarFolded":true}"#;
        fs::write(InterfaceState::path(&state), compact).unwrap();
        let (second, kept) = launched(&state);
        assert!(second.get_sidebar_chosen_folded());
        assert!(folded(&second), "opens folded");
        kept.shut_down();
        assert_eq!(
            fs::read_to_string(InterfaceState::path(&state)).unwrap(),
            compact
        );

        let (third, kept) = launched(&state);
        button(&third, "Déplier la barre latérale").invoke_accessible_default_action();
        kept.shut_down();
        assert!(!InterfaceState::load(&state).unwrap().sidebar_folded);
        assert!(!folded(&launched(&state).0), "opens unfolded again");
    }

    /// What a damage leaves: never replaced.
    const UNREADABLE: [&str; 3] = [
        "{",
        r#"{"version": 1}"#,
        r#"{"version": 1, "sidebarFolded": "yes"}"#,
    ];

    #[test]
    fn a_state_that_cannot_be_read_is_never_replaced() {
        i_slint_backend_testing::init_no_event_loop();
        for text in UNREADABLE {
            let directory = tempfile::tempdir().unwrap();
            fs::write(InterfaceState::path(directory.path()), text).unwrap();
            let (window, kept) = launched(directory.path());
            assert!(!folded(&window), "opens unfolded: {text}");
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
        assert!(!folded(&launched(directory.path()).0));
    }

    #[test]
    fn without_a_state_directory_the_sidebar_still_folds() {
        i_slint_backend_testing::init_no_event_loop();
        let (window, ..) = crate::window(
            seams_never_scanning(),
            crate::dashboard::tests::inert(),
            Language::French,
        )
        .unwrap();
        let kept = KeptFold::attach(&window, None);
        resize(&window, 1280);
        button(&window, "Replier la barre latérale").invoke_accessible_default_action();
        assert!(folded(&window));
        kept.shut_down();
    }
}
