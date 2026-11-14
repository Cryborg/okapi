//! The user's choices among the results, kept from one analysis to the next
//! (`okapi_store::Selections`, D-13): read at launch, written at each choice
//! off the interface's thread.

use std::collections::BTreeSet;
use std::path::PathBuf;

use okapi_engine::OriginKey;
use okapi_store::Selections;

use crate::results::Results;
use crate::saving::{LastWrite, Saver};

/// The thread that writes the choices.
const SAVER_NAME: &str = "okapi-selections-save";

/// Why the user's choices are not kept this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unkept {
    NoDirectory,
    /// Those kept in this file cannot be read: left as they are (D-10),
    /// which the results say, naming it.
    Unreadable(PathBuf),
    /// The thread that would keep them did not start.
    NoWriter,
}

/// The rules the user unchecked, and the thread that keeps them.
pub struct KeptSelections {
    unchecked: BTreeSet<OriginKey>,
    saver: Saver<Selections>,
    /// The last write did not keep the choices, or none can be done.
    lost: bool,
    /// A write was asked for whose end is not known yet.
    writing: bool,
}

impl std::fmt::Debug for KeptSelections {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KeptSelections")
            .field("unchecked", &self.unchecked)
            .finish_non_exhaustive()
    }
}

impl KeptSelections {
    /// The choices kept in `directory`, and the thread that keeps the next
    /// ones there; why not, traced, when there is no directory, when they
    /// cannot be read (then left as they are and nothing of this session
    /// kept, D-10), or when the thread cannot start.
    ///
    /// # Errors
    ///
    /// [`Unkept`].
    pub fn start(directory: Option<PathBuf>) -> Result<Self, Unkept> {
        let Some(directory) = directory else {
            tracing::warn!("no state directory: the choices among the results are not kept");
            return Err(Unkept::NoDirectory);
        };
        let selections = Selections::load(&directory).map_err(|error| {
            tracing::warn!(%error, "choices among the results unreadable: none kept this session, the file left as it is");
            Unkept::Unreadable(Selections::path(&directory))
        })?;
        let saver = Saver::start(
            SAVER_NAME,
            // Whether they are kept: the store traces why not.
            Box::new(move |selections: &Selections| selections.save(&directory).is_ok()),
        )
        .map_err(|error| {
            tracing::error!(%error, "choices writer not started: the choices among the results are not kept");
            Unkept::NoWriter
        })?;
        Ok(Self {
            unchecked: selections.unchecked,
            saver,
            lost: false,
            writing: false,
        })
    }

    /// Whether every choice so far is kept, as the writes done say: not
    /// after a write that failed (the store traced why), until one keeps
    /// them, nor once the thread is gone.
    pub fn all_kept(&mut self) -> bool {
        let last = self.saver.last_write();
        if last != LastWrite::Nothing {
            self.writing = false;
        }
        match last {
            LastWrite::Nothing => {}
            LastWrite::Kept(kept) => {
                if !kept && !self.lost {
                    tracing::warn!("choices among the results not kept: said");
                }
                self.lost = !kept;
            }
            LastWrite::Gone => {
                if !self.lost {
                    tracing::error!(writer = SAVER_NAME, "choices writer gone: said");
                }
                self.lost = true;
            }
        }
        !self.lost
    }

    /// The rules the user unchecked.
    pub const fn unchecked(&self) -> &BTreeSet<OriginKey> {
        &self.unchecked
    }

    /// Keeps what the user changed in `results` since `before`, the groups
    /// checked then.
    pub fn update(&mut self, results: &Results, before: &BTreeSet<usize>) {
        if !results.remember(before, &mut self.unchecked) {
            return;
        }
        // What earlier writes said is taken first: the end seen next is
        // then this one's (or a later one's).
        self.all_kept();
        self.writing = true;
        if !self.saver.save(Selections {
            unchecked: self.unchecked.clone(),
        }) {
            tracing::warn!(
                writer = SAVER_NAME,
                "choices writer gone: this choice among the results not kept"
            );
        }
    }

    /// Whether a write was asked for whose end [`Self::all_kept`] has not
    /// seen yet.
    pub const fn writing(&self) -> bool {
        self.writing
    }

    /// Lets the last choice be written, and waits for it a little.
    pub fn shut_down(&mut self) {
        self.saver.shut_down();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use i_slint_backend_testing::{AccessibleRole, ElementHandle};
    use okapi_rules::Language;

    use super::*;
    use crate::cleaner::CleanerScreen;
    use crate::cleaner::tests::{
        Sandbox, button, checkbox, let_the_screen_settle, resize, settle_files, step, watch_until,
    };
    use crate::ui;

    /// The app opened as at launch, its choices kept in the sandbox's state,
    /// on the results of an analysis of Applications and GPU caches. The
    /// testing platform is set up once per test.
    fn launched(sandbox: &Sandbox) -> (ui::AppWindow, CleanerScreen) {
        let state = sandbox.root.join("state");
        let (window, screen, _home) = crate::window(
            sandbox.read(&state),
            crate::dashboard::tests::inert(),
            Language::French,
        )
        .unwrap();
        screen.keep_selections(KeptSelections::start(Some(state)));
        resize(&window, 1280);
        button(&window, "Nettoyeur").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        for category in ["Système", "Jeux", "Caches GPU"] {
            checkbox(&window, category).invoke_accessible_default_action();
        }
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        let_the_screen_settle();
        (window, screen)
    }

    /// The checkbox of the first group of the results named one of `labels`.
    fn group(window: &ui::AppWindow, labels: &[&str]) -> ElementHandle {
        labels
            .iter()
            .find_map(|label| {
                ElementHandle::find_by_accessible_label(window, label)
                    .find(|element| element.accessible_role() == Some(AccessibleRole::Checkbox))
            })
            .unwrap_or_else(|| panic!("no group {labels:?} in the results"))
    }

    fn checked(window: &ui::AppWindow, labels: &[&str]) -> Option<bool> {
        group(window, labels).accessible_checked()
    }

    fn toggle(window: &ui::AppWindow, labels: &[&str]) {
        group(window, labels).invoke_accessible_default_action();
        let_the_screen_settle();
    }

    const PIP: &[&str] = &["Cache de pip"];
    const MESA: &[&str] = &["Cache des shaders Mesa"];
    /// `unity` and `unity-cache` share their root: one group, left to choose.
    const UNITY: &[&str] = &["Unity", "Cache des shaders Unity"];

    fn kept(sandbox: &Sandbox) -> Option<String> {
        fs::read_to_string(Selections::path(&sandbox.root.join("state"))).ok()
    }

    fn sandbox() -> Sandbox {
        let sandbox = Sandbox::new();
        for file in [
            "home/.cache/pip/old",
            "home/.cache/mesa_shader_cache/old",
            "home/.cache/unity3d/old",
        ] {
            sandbox.file(file);
        }
        settle_files();
        sandbox
    }

    #[test]
    fn a_group_unchecked_one_by_one_starts_unchecked_at_the_next_launch() {
        i_slint_backend_testing::init_no_event_loop();
        let sandbox = sandbox();
        let (window, screen) = launched(&sandbox);
        assert_eq!(checked(&window, PIP), Some(true));
        assert_eq!(checked(&window, MESA), Some(false), "left to choose");
        assert_eq!(checked(&window, UNITY), Some(false), "left to choose");
        toggle(&window, PIP);
        for left_to_choose in [MESA, UNITY] {
            toggle(&window, left_to_choose);
            toggle(&window, left_to_choose);
        }
        screen.shut_down();
        let selections = Selections::load(&sandbox.root.join("state")).unwrap();
        assert_eq!(
            selections.unchecked,
            BTreeSet::from([vec!["app".to_owned(), "pip".to_owned()]]),
            "never a group left to choose (D-13)"
        );

        let (window, screen) = launched(&sandbox);
        assert_eq!(checked(&window, PIP), Some(false), "as the user left it");
        assert_eq!(checked(&window, MESA), Some(false));
        toggle(&window, PIP);
        screen.shut_down();
        assert_eq!(
            Selections::load(&sandbox.root.join("state")).unwrap(),
            Selections::default(),
            "checked again: as by default"
        );
    }

    #[test]
    fn deselecting_all_holds_for_this_analysis_alone() {
        i_slint_backend_testing::init_no_event_loop();
        let sandbox = sandbox();
        let (window, screen) = launched(&sandbox);
        button(&window, "Tout désélectionner").invoke_accessible_default_action();
        let_the_screen_settle();
        assert_eq!(checked(&window, PIP), Some(false));
        screen.shut_down();
        assert_eq!(kept(&sandbox), None, "nothing kept");

        let (window, _screen) = launched(&sandbox);
        assert_eq!(checked(&window, PIP), Some(true), "as by default");
    }

    #[test]
    fn choices_that_cannot_be_read_are_neither_used_nor_replaced() {
        i_slint_backend_testing::init_no_event_loop();
        let sandbox = sandbox();
        let path = Selections::path(&sandbox.root.join("state"));
        fs::write(&path, "{").unwrap();
        let (window, screen) = launched(&sandbox);
        assert_eq!(checked(&window, PIP), Some(true), "as by default");
        assert!(crate::cleaner::tests::shows(&window, &unreadable(&path)));
        toggle(&window, PIP);
        assert_eq!(checked(&window, PIP), Some(false), "the box changes");
        screen.shut_down();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{", "the file does not");
    }

    /// The banner of the choices of `path`, which cannot be read.
    fn unreadable(path: &std::path::Path) -> String {
        format!(
            "Vos choix précédents n'ont pas pu être lus dans {} : les cases sont comme par défaut, et vos changements ne sont pas retenus pour ne pas écraser ce fichier.",
            path.display()
        )
    }

    /// The banner of a change that could not be kept.
    const UNSAVED: &str = "Vos derniers changements de cases n'ont pas pu être enregistrés : ils seront oubliés à la fermeture d'Okapi.";

    #[test]
    fn a_change_that_cannot_be_kept_is_said() {
        i_slint_backend_testing::init_no_event_loop();
        let sandbox = sandbox();
        let (window, screen) = launched(&sandbox);
        assert!(!crate::cleaner::tests::shows(&window, UNSAVED));
        // Where the choices go, a directory now: no write keeps them.
        fs::create_dir(Selections::path(&sandbox.root.join("state"))).unwrap();
        toggle(&window, PIP);
        watch_until(&window, |window| {
            crate::cleaner::tests::shows(window, UNSAVED)
        });
        assert_eq!(checked(&window, PIP), Some(false), "for this session");
        // Writable again: the next write keeps every choice, said gone.
        fs::remove_dir(Selections::path(&sandbox.root.join("state"))).unwrap();
        toggle(&window, PIP);
        watch_until(&window, |window| {
            !crate::cleaner::tests::shows(window, UNSAVED)
        });
        screen.shut_down();
    }

    #[test]
    fn choices_kept_never_check_a_group_left_to_choose() {
        i_slint_backend_testing::init_no_event_loop();
        let sandbox = sandbox();
        // Keys of every rule there, as if the file had been tampered with.
        let keys = ["pip", "mesa-cache", "unity", "unity-cache"]
            .map(|id| vec!["app".to_owned(), id.to_owned()]);
        Selections {
            unchecked: BTreeSet::from(keys),
        }
        .save(&sandbox.root.join("state"))
        .unwrap();
        let (window, _screen) = launched(&sandbox);
        assert_eq!(checked(&window, PIP), Some(false));
        assert_eq!(checked(&window, MESA), Some(false));
        assert_eq!(checked(&window, UNITY), Some(false));
        assert!(
            !crate::cleaner::tests::shows(
                &window,
                &unreadable(&Selections::path(&sandbox.root.join("state")))
            ) && !crate::cleaner::tests::shows(&window, UNSAVED)
        );
    }

    #[test]
    fn nothing_is_kept_without_a_directory_or_readable_choices() {
        assert_eq!(
            KeptSelections::start(None).unwrap_err(),
            Unkept::NoDirectory
        );
        let directory = tempfile::tempdir().unwrap();
        fs::write(Selections::path(directory.path()), "{").unwrap();
        assert_eq!(
            KeptSelections::start(Some(directory.path().to_owned())).unwrap_err(),
            Unkept::Unreadable(Selections::path(directory.path()))
        );
    }
}
