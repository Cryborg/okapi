//! The tab of what applications uninstalled left
//! (`okapi_tools::apps::leftovers`): analysed on a thread of its own each
//! time the list is read, never replaced under what the user checked or
//! confirms; its folders moved to the trash on a thread by identifiers of
//! the report shown only (invariants 1 and 7). The copies snapd keeps of
//! snaps removed, read with them, each forgotten by snapd once confirmed
//! (`copies`). And what Okapi remembers of the applications it saw, written
//! after a whole reading.

mod copies;

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use okapi_store::{SeenApp, SeenApps};
use okapi_tools::apps::Source;
use okapi_tools::apps::leftovers::{
    KeepReason, Leftover, LeftoverId, LeftoverKind, LeftoverReport, TrashReceipt, TrashResult,
};
use okapi_tools::apps::snapshots::{Copies, CopyId, CopyReport, Forgot};
use slint::{ComponentHandle as _, ModelRc, VecModel};

use super::{AppsScreen, source_key};
use crate::cleaner::Problem;
use crate::shown::{from_home, shown, size};
use crate::ui;

/// What an analysis gave.
#[derive(Debug)]
pub struct Analysis {
    pub report: Result<LeftoverReport, Blocked>,
    /// Packages removed whose settings stay; `None` when dpkg was unread.
    pub residual: Option<usize>,
    pub seen: SeenApps,
    /// snapd's copies of snaps no longer installed, read whatever blocks
    /// the leftovers.
    pub copies: Copies,
}

/// Why no leftover could be looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// The settings (their exclusions) could not be read.
    Settings,
    /// The session's folders could not be read: a variable's (invariant 6),
    /// the home or data folder, the user; or this system has none to read.
    Variables,
}

/// Analyses the home folder, the user's exclusions (or why there are none)
/// given.
pub type Analyse = Arc<dyn Fn(Result<Vec<PathBuf>, Blocked>) -> Analysis + Send + Sync>;
/// Moves leftovers of a report to the trash, the user's exclusions given.
pub type MoveToTrash = Arc<
    dyn Fn(&LeftoverReport, &[LeftoverId], &[PathBuf]) -> io::Result<TrashReceipt> + Send + Sync,
>;
/// Keeps the applications seen installed.
pub type Remember = Arc<dyn Fn(Vec<SeenApp>) + Send + Sync>;
/// Asks snapd to forget a copy of a report.
pub type Forget = Arc<dyn Fn(&CopyReport, CopyId) -> Forgot + Send + Sync>;

/// What the tab asks of the machine: the system's, or a test's.
#[derive(Clone)]
pub struct LeftoverSeams {
    pub analyse: Analyse,
    pub trash: MoveToTrash,
    pub remember: Remember,
    pub forget: Forget,
}

impl std::fmt::Debug for LeftoverSeams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LeftoverSeams")
            .finish_non_exhaustive()
    }
}

/// What the last move did, as the receipt says it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Told {
    trashed: usize,
    bytes: u64,
    /// Left where they were.
    kept: usize,
    /// In the trash, not known to be what was checked.
    unchecked: usize,
}

impl Told {
    fn of(receipt: &TrashReceipt) -> Self {
        let mut told = Self {
            bytes: receipt.trashed_bytes(),
            ..Self::default()
        };
        for outcome in receipt.outcomes() {
            match outcome.result {
                TrashResult::Trashed { .. } => told.trashed += 1,
                // Gone already: neither moved nor left in place.
                TrashResult::Kept(KeepReason::Gone) => {}
                TrashResult::Kept(_) => told.kept += 1,
                TrashResult::Unchecked => told.unchecked += 1,
            }
        }
        told
    }
}

/// What the receipt says of the last move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Said {
    /// It could not run: nothing moved.
    NotRun,
    /// It ended without a word: some may be in the trash.
    Interrupted,
    Done(Told),
}

/// The tab's state, kept by the screen's.
#[derive(Default)]
pub(super) struct LeftoversState {
    report: Option<Arc<LeftoverReport>>,
    seen: SeenApps,
    residual: Option<usize>,
    blocked: Option<Blocked>,
    /// The last analysis ended without a word, or could not start.
    failed: bool,
    /// The leftovers checked, by their rank in the report.
    checked: BTreeSet<usize>,
    analysing: Option<Receiver<Analysis>>,
    moving: Option<Receiver<io::Result<TrashReceipt>>>,
    confirming: bool,
    /// An analysis asked while one could not start: made once free.
    again: bool,
    model: Rc<VecModel<ui::LeftoverLine>>,
    copies: copies::CopiesState,
}

impl LeftoversState {
    /// Whether a new report would come under the user's hand.
    fn held(&self) -> bool {
        self.busy() || self.confirming || !self.checked.is_empty() || self.copies.asked.is_some()
    }

    pub(super) const fn busy(&self) -> bool {
        self.analysing.is_some() || self.moving.is_some() || self.copies.forgetting.is_some()
    }
}

impl AppsScreen {
    pub(super) fn attach_leftovers(&self, window: &ui::AppWindow) {
        let leftovers = window.global::<ui::Leftovers>();
        leftovers.set_lines(ModelRc::from(Rc::clone(&self.0.borrow().leftovers.model)));
        leftovers.set_copies(ModelRc::from(Rc::clone(
            &self.0.borrow().leftovers.copies.model,
        )));
        let weak = Rc::downgrade(&self.0);
        let on = move |action: fn(&Self)| {
            let weak = weak.clone();
            move || {
                if let Some(state) = weak.upgrade() {
                    action(&Self(state));
                }
            }
        };
        leftovers.on_trash(on(Self::ask_trash));
        leftovers.on_cancel(on(Self::cancel_trash));
        leftovers.on_confirm(on(Self::confirm_trash));
        leftovers.on_close_receipt(on(Self::close_receipt));
        leftovers.on_confirm_forget(on(Self::confirm_forget));
        leftovers.on_cancel_forget(on(Self::cancel_forget));
        leftovers.on_close_forgot(on(Self::close_forgot));
        let weak = Rc::downgrade(&self.0);
        leftovers.on_forget(move |index| {
            let Some(state) = weak.upgrade() else { return };
            if let Ok(index) = usize::try_from(index) {
                Self(state).ask_forget(index);
            } else {
                tracing::warn!(index, "a copy out of the list refused");
            }
        });
        let weak = Rc::downgrade(&self.0);
        leftovers.on_toggle(move |index| {
            let Some(state) = weak.upgrade() else { return };
            if let Ok(index) = usize::try_from(index) {
                Self(state).toggle(index);
            } else {
                tracing::warn!(index, "a leftover out of the list refused");
            }
        });
    }

    /// Analyses what applications uninstalled left, on a thread; held until
    /// nothing is under the user's hand.
    pub(super) fn analyse_leftovers(&self) {
        if self.0.borrow().leftovers.held() {
            tracing::info!("leftovers analysed once the user lets go");
            self.0.borrow_mut().leftovers.again = true;
            return;
        }
        let read_exclusions = Rc::clone(&self.0.borrow().exclusions);
        let exclusions = read_exclusions().map_err(|problem| {
            tracing::warn!(?problem, "settings not read: no leftover proposed");
            match problem {
                Problem::Variables => Blocked::Variables,
                _ => Blocked::Settings,
            }
        });
        // Held at once: settings written meanwhile look again.
        let blocked_now = exclusions.as_ref().err().copied();
        let analyse = Arc::clone(&self.0.borrow().seams.leftovers.analyse);
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("okapi-leftovers".to_owned())
            .spawn(move || {
                let _ = sender.send(analyse(exclusions));
            });
        {
            let mut state = self.0.borrow_mut();
            let leftovers = &mut state.leftovers;
            // Asked once: a thread that cannot start is not asked again in
            // a loop.
            leftovers.again = false;
            if blocked_now.is_some() {
                leftovers.blocked = blocked_now;
            }
            match spawned {
                Ok(_) => leftovers.analysing = Some(receiver),
                Err(error) => {
                    tracing::error!(%error, "no thread to analyse the leftovers");
                    leftovers.failed = true;
                }
            }
        }
        self.show_leftovers();
        self.watch();
    }

    /// The settings written: leftovers they held back are looked for.
    pub fn settings_written(&self) {
        let blocked = self.0.borrow().leftovers.blocked == Some(Blocked::Settings);
        if blocked {
            tracing::info!("settings written: leftovers looked for again");
            self.analyse_leftovers();
        }
    }

    /// What the analysis or the move said; an analysis owed made once free.
    pub(super) fn take_leftovers(&self) {
        let analysed = {
            let state = self.0.borrow();
            state.leftovers.analysing.as_ref().map(Receiver::try_recv)
        };
        match analysed {
            Some(Ok(analysis)) => self.analysed(analysis),
            Some(Err(TryRecvError::Disconnected)) => {
                tracing::error!("the leftovers' analysis ended without a word");
                {
                    let mut state = self.0.borrow_mut();
                    state.leftovers.analysing = None;
                    state.leftovers.failed = true;
                }
                self.show_leftovers();
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        let moved = {
            let state = self.0.borrow();
            state.leftovers.moving.as_ref().map(Receiver::try_recv)
        };
        match moved {
            Some(Ok(Ok(receipt))) => {
                let told = Told::of(&receipt);
                tracing::info!(?told, "leftovers' move told");
                self.moved(Said::Done(told));
            }
            Some(Ok(Err(error))) => {
                tracing::error!(%error, "leftovers not moved");
                self.moved(Said::NotRun);
            }
            Some(Err(TryRecvError::Disconnected)) => {
                tracing::error!(
                    "the move of leftovers ended without a word: some may be in the trash"
                );
                self.moved(Said::Interrupted);
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        self.take_forgotten();
        let again = {
            let state = self.0.borrow();
            state.leftovers.again && !state.leftovers.held()
        };
        if again {
            self.analyse_leftovers();
        }
    }

    fn analysed(&self, analysis: Analysis) {
        {
            let mut state = self.0.borrow_mut();
            let leftovers = &mut state.leftovers;
            leftovers.analysing = None;
            leftovers.failed = false;
            // None can be checked while an analysis runs: nothing is lost.
            leftovers.checked.clear();
            leftovers.residual = analysis.residual;
            leftovers.seen = analysis.seen;
            leftovers.copies.analysed(analysis.copies);
            match analysis.report {
                Ok(report) => {
                    tracing::info!(found = report.leftovers().len(), "leftovers shown");
                    leftovers.report = Some(Arc::new(report));
                    leftovers.blocked = None;
                }
                Err(blocked) => {
                    tracing::info!(?blocked, "no leftover shown");
                    leftovers.report = None;
                    leftovers.blocked = Some(blocked);
                }
            }
        }
        self.show_leftovers();
    }

    fn toggle(&self, index: usize) {
        {
            let mut state = self.0.borrow_mut();
            let leftovers = &mut state.leftovers;
            if leftovers.busy() {
                tracing::info!(
                    index,
                    "a box ticked while the leftovers are read or moved: refused"
                );
                return;
            }
            let checkable = leftovers
                .report
                .as_ref()
                .and_then(|report| report.leftovers().get(index))
                .is_some_and(Leftover::checkable);
            if !checkable {
                tracing::info!(index, "a leftover out of the list or too large refused");
                return;
            }
            if !leftovers.checked.remove(&index) {
                leftovers.checked.insert(index);
            }
        }
        self.show_leftovers();
    }

    fn ask_trash(&self) {
        {
            let mut state = self.0.borrow_mut();
            let leftovers = &mut state.leftovers;
            if leftovers.checked.is_empty() {
                tracing::warn!("a move to the trash with nothing checked refused");
            } else if leftovers.busy() || leftovers.copies.asked.is_some() {
                tracing::warn!("a move to the trash while another action runs refused");
            } else {
                leftovers.confirming = true;
            }
        }
        self.show_leftovers();
    }

    fn cancel_trash(&self) {
        self.0.borrow_mut().leftovers.confirming = false;
        self.show_leftovers();
        // An analysis owed meanwhile waits for the boxes to be let go.
    }

    /// Moves the leftovers checked to the trash on a thread: each is read
    /// again there (invariant 7).
    fn confirm_trash(&self) {
        let asked = {
            let mut state = self.0.borrow_mut();
            let leftovers = &mut state.leftovers;
            leftovers.confirming = false;
            match leftovers.report.clone() {
                None => {
                    tracing::warn!("a confirmation without a report refused");
                    None
                }
                Some(_) if leftovers.busy() => {
                    tracing::warn!("a confirmation while another runs refused");
                    None
                }
                Some(report) => {
                    let ids: Vec<LeftoverId> = leftovers
                        .checked
                        .iter()
                        .filter_map(|index| report.leftovers().get(*index))
                        .map(Leftover::id)
                        .collect();
                    Some((report, ids))
                }
            }
        };
        let Some((report, ids)) = asked else {
            self.show_leftovers();
            return;
        };
        let read_exclusions = Rc::clone(&self.0.borrow().exclusions);
        let exclusions = match read_exclusions() {
            Ok(exclusions) => exclusions,
            Err(problem) => {
                tracing::warn!(?problem, "settings not read: nothing moved");
                self.tell(Said::NotRun);
                return;
            }
        };
        tracing::info!(leftovers = ids.len(), "move to the trash confirmed");
        let trash = Arc::clone(&self.0.borrow().seams.leftovers.trash);
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("okapi-leftovers-trash".to_owned())
            .spawn(move || {
                // The screen gone: the move ended all the same, traced.
                let _ = sender.send(trash(&report, &ids, &exclusions));
            });
        match spawned {
            Ok(_) => {
                self.0.borrow_mut().leftovers.moving = Some(receiver);
                // The last receipt is no more what is under way.
                self.with_window(|window| window.global::<ui::Leftovers>().set_receipt(false));
                self.show_leftovers();
                self.watch();
            }
            Err(error) => {
                tracing::error!(%error, "no thread to move leftovers: nothing moved");
                self.tell(Said::NotRun);
            }
        }
    }

    fn moved(&self, said: Said) {
        {
            let mut state = self.0.borrow_mut();
            let leftovers = &mut state.leftovers;
            leftovers.moving = None;
            leftovers.checked.clear();
            // What it found is no more: analysed again.
            leftovers.again = true;
        }
        self.tell(said);
    }

    /// Says what the last move did.
    fn tell(&self, said: Said) {
        self.with_window(|window| {
            let leftovers = window.global::<ui::Leftovers>();
            leftovers.set_trash_failed(said == Said::NotRun);
            leftovers.set_trash_interrupted(said == Said::Interrupted);
            let told = match said {
                Said::Done(told) => told,
                Said::NotRun | Said::Interrupted => Told::default(),
            };
            leftovers.set_trashed(shown(told.trashed));
            leftovers.set_trashed_size(size(told.bytes));
            leftovers.set_kept(shown(told.kept));
            leftovers.set_unchecked(shown(told.unchecked));
            leftovers.set_receipt(true);
        });
        self.show_leftovers();
    }

    fn close_receipt(&self) {
        self.with_window(|window| window.global::<ui::Leftovers>().set_receipt(false));
    }

    /// Shows the tab as it stands.
    pub(super) fn show_leftovers(&self) {
        let window = self.0.borrow().window.upgrade();
        let Some(window) = window else { return };
        let leftovers = window.global::<ui::Leftovers>();
        let state = self.0.borrow();
        let tab = &state.leftovers;
        let home = state.seams.home.clone();
        let lines: Vec<ui::LeftoverLine> = tab
            .report
            .iter()
            .flat_map(|report| report.leftovers().iter().enumerate())
            .map(|(index, leftover)| {
                let (source, shown_source) = match leftover.kind() {
                    LeftoverKind::Snap => (Source::Snap, ui::AppSource::Snap),
                    LeftoverKind::Flatpak => (Source::Flatpak, ui::AppSource::Flatpak),
                };
                let (name, known) = shown_name(&tab.seen, source, leftover.name());
                ui::LeftoverLine {
                    source: shown_source,
                    name: name.into(),
                    known,
                    path: from_home(leftover.path(), home.as_deref()).into(),
                    size: size(leftover.bytes().unwrap_or(0)),
                    checkable: leftover.checkable(),
                    checked: tab.checked.contains(&index),
                }
            })
            .collect();
        let bytes_of = |indexes: &mut dyn Iterator<Item = usize>| {
            indexes
                .filter_map(|index| tab.report.as_ref()?.leftovers().get(index)?.bytes())
                .fold(0, u64::saturating_add)
        };
        let count = tab
            .report
            .as_ref()
            .map_or(0, |report| report.leftovers().len());
        crate::shown::show(&tab.model, lines);
        let total = bytes_of(&mut (0..count));
        leftovers.set_total(size(total));
        leftovers.set_has_total(total > 0);
        leftovers.set_checked_count(shown(tab.checked.len()));
        leftovers.set_checked_size(size(bytes_of(&mut tab.checked.iter().copied())));
        leftovers.set_residual(tab.residual.map_or(0, shown));
        let unread = |kind| {
            tab.report
                .as_ref()
                .is_some_and(|report| report.unread().contains(&kind))
        };
        leftovers.set_snaps_unread(unread(LeftoverKind::Snap));
        leftovers.set_flatpaks_unread(unread(LeftoverKind::Flatpak));
        leftovers.set_searched(tab.report.is_some());
        leftovers.set_withheld(
            tab.report
                .as_ref()
                .map_or(0, |report| shown(report.withheld())),
        );
        // A report from before is shown under it: the list may be old.
        leftovers.set_analysis_failed(tab.failed);
        leftovers.set_blocked(match tab.blocked {
            None => ui::LeftoversBlock::None,
            Some(Blocked::Settings) => ui::LeftoversBlock::Settings,
            Some(Blocked::Variables) => ui::LeftoversBlock::Variables,
        });
        leftovers.set_analysing(tab.analysing.is_some());
        leftovers.set_trashing(tab.moving.is_some());
        leftovers.set_confirming(tab.confirming);
        tab.copies.show(
            &leftovers,
            &tab.seen,
            (state.seams.now)(),
            &state.seams.zone,
        );
        leftovers.set_busy(tab.busy());
    }

    /// The window closes: a move of leftovers is waited for a little (each
    /// folder is one rename; a cut one leaves at most a `.trashinfo`
    /// without its folder, or a folder in the trash not read again); a copy
    /// being forgotten is left to snapd, traced.
    pub(super) fn wait_for_leftovers(&self) {
        if self.0.borrow().leftovers.copies.forgetting.is_some() {
            tracing::warn!("a copy of snapd being forgotten left at closing: snapd ends it");
        }
        let Some(moving) = self.0.borrow_mut().leftovers.moving.take() else {
            return;
        };
        let started = std::time::Instant::now();
        match moving.recv_timeout(crate::duplicates::TRASH_WAIT) {
            Ok(_) => {
                tracing::info!(waited = ?started.elapsed(), "move of leftovers ended before closing");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    waited = ?started.elapsed(),
                    "the move of leftovers ended without a word: some may be in the trash"
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(waited = ?started.elapsed(), "move of leftovers left running at closing");
            }
        }
    }

    /// Keeps the applications of a whole reading as seen, on a thread.
    pub(super) fn remember_seen(&self, seen: Vec<SeenApp>) {
        let remember = Arc::clone(&self.0.borrow().seams.leftovers.remember);
        let spawned = std::thread::Builder::new()
            .name("okapi-seen-apps".to_owned())
            .spawn(move || remember(seen));
        if let Err(error) = spawned {
            tracing::warn!(%error, "no thread to keep the applications seen");
        }
    }
}

/// The name a snap or flatpak `name` of `source` is shown by: the one Okapi
/// saw it installed with, else its own; and whether Okapi saw it.
fn shown_name(seen: &SeenApps, source: Source, name: &str) -> (String, bool) {
    let card = seen.find(source_key(source), name);
    let shown = card
        .map(|card| card.shown.as_str())
        .filter(|shown| !shown.is_empty())
        .unwrap_or(name);
    (shown.to_owned(), card.is_some())
}

#[cfg(target_os = "linux")]
mod system {
    //! The system's analysis, trash and memory.

    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use okapi_engine::{Exclusions, ProtectedRoots};
    use okapi_rules::{TemplateVariables, Variable};
    use okapi_store::{SeenApp, SeenApps};
    use okapi_tools::apps::leftovers::{
        self, InstalledPlaces, LeftoverId, LeftoverReport, MOST_ENTRIES, Places, TrashReceipt,
    };
    use okapi_tools::apps::snapshots::{self, CopyId, CopyReport, FORGET_WAIT, Forgot};
    use okapi_tools::packages::snapd::Snapd;

    use super::{Analysis, Blocked};

    /// What the analysis and the move read of the session: its folders, and
    /// where what is installed is read. Never without the user's data
    /// folder: the user's flatpaks would be said uninstalled.
    fn session() -> Option<(TemplateVariables, InstalledPlaces)> {
        let variables = okapi_platform::template_variables()
            .inspect_err(|error| tracing::warn!(?error, "the session's folders unread"))
            .ok()?;
        let Some(data_home) = variables.value_of(Variable::LocalShare) else {
            tracing::warn!("no data folder: the user's flatpaks unknown");
            return None;
        };
        // Where flatpak says what runs: the runtime folder, or as GLib falls
        // back without one, the cache.
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|runtime| runtime.is_absolute())
            .or_else(|| variables.value_of(Variable::Cache).map(Path::to_path_buf));
        let places = InstalledPlaces {
            snap_files: PathBuf::from(leftovers::installed::SNAP_FILES),
            installations: okapi_tools::apps::flatpak::installations(Some(data_home)),
            runtime,
        };
        Some((variables, places))
    }

    fn seen() -> SeenApps {
        let Some(directory) = okapi_platform::state_directory() else {
            tracing::warn!("no state directory: no application seen known");
            return SeenApps::default();
        };
        SeenApps::load(&directory)
            .inspect_err(|error| tracing::warn!(%error, "applications seen unreadable"))
            .unwrap_or_default()
    }

    /// Packages removed whose settings stay, as dpkg says.
    fn residual() -> Option<usize> {
        let dpkg = Path::new(okapi_tools::packages::dpkg::DPKG_DIRECTORY);
        match okapi_tools::packages::dpkg::pending_updates(dpkg) {
            Ok(0) => {}
            Ok(pending) => {
                tracing::info!(pending, "dpkg busy: its settings left not counted");
                return None;
            }
            Err(error) => {
                tracing::warn!(%error, "dpkg's updates unreadable: its settings left not counted");
                return None;
            }
        }
        std::fs::read_to_string(dpkg.join("status"))
            .inspect_err(|error| tracing::warn!(%error, "dpkg's status unreadable"))
            .ok()
            .map(|text| okapi_tools::apps::dpkg::parse_status(&text).residual.len())
    }

    pub(crate) fn analyse(exclusions: Result<Vec<PathBuf>, Blocked>) -> Analysis {
        // Read once, whatever blocks the leftovers: snapd's copies need no
        // setting nor variable.
        let snaps = leftovers::installed::snaps_now(Path::new(leftovers::installed::SNAP_FILES));
        let copies = snapshots::find(Snapd::system().snapshots(), &snaps);
        let report = (|| {
            let exclusions = Exclusions::new(exclusions?).map_err(|error| {
                tracing::warn!(?error, "an exclusion refused: no leftover proposed");
                Blocked::Settings
            })?;
            let (variables, installed_places) = session().ok_or(Blocked::Variables)?;
            let protected = ProtectedRoots::of(&variables).map_err(|error| {
                tracing::warn!(%error, "no leftover proposed");
                Blocked::Variables
            })?;
            let Some(home) = variables.value_of(Variable::Home) else {
                tracing::warn!("no home folder: no leftover proposed");
                return Err(Blocked::Variables);
            };
            let Some(user) = okapi_platform::effective_user() else {
                tracing::warn!("the user unknown: no leftover proposed");
                return Err(Blocked::Variables);
            };
            let installed = leftovers::installed::installed_with(&installed_places, snaps);
            let places = Places {
                home: home.to_path_buf(),
                user,
                most_entries: MOST_ENTRIES,
            };
            Ok(leftovers::find(
                &places,
                &installed,
                &protected,
                &exclusions,
            ))
        })();
        Analysis {
            report,
            residual: residual(),
            seen: seen(),
            copies,
        }
    }

    pub(crate) fn forget(report: &CopyReport, id: CopyId) -> Forgot {
        snapshots::forget(report, id, &Snapd::system(), FORGET_WAIT)
    }

    pub(crate) fn trash(
        report: &LeftoverReport,
        ids: &[LeftoverId],
        exclusions: &[PathBuf],
    ) -> std::io::Result<TrashReceipt> {
        let exclusions = Exclusions::new(exclusions.iter().cloned())
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let bin = okapi_platform::Trash::home()?;
        let (variables, places) =
            session().ok_or_else(|| std::io::Error::other("the session's folders unread"))?;
        let protected = ProtectedRoots::of(&variables)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(leftovers::trash(
            report,
            ids,
            &bin,
            &|| leftovers::installed::installed_now(&places),
            &leftovers::TrashRules {
                protected: &protected,
                exclusions: &exclusions,
            },
        ))
    }

    /// Held while the applications seen are read and written: two whole
    /// readings close together would each keep what the other lost.
    static REMEMBERING: Mutex<()> = Mutex::new(());

    pub(crate) fn remember(seen: Vec<SeenApp>) {
        let _one_at_a_time = REMEMBERING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(directory) = okapi_platform::state_directory() else {
            tracing::warn!("no state directory: applications seen not kept");
            return;
        };
        let mut known = match SeenApps::load(&directory) {
            Ok(known) => known,
            Err(error) => {
                tracing::warn!(%error, "applications seen unreadable: not replaced");
                return;
            }
        };
        if known.remember(seen)
            && let Err(error) = known.save(&directory)
        {
            tracing::warn!(%error, "applications seen not kept");
        }
    }
}

impl LeftoverSeams {
    /// The system's.
    #[cfg(target_os = "linux")]
    pub fn system() -> Self {
        Self {
            analyse: Arc::new(system::analyse),
            trash: Arc::new(system::trash),
            remember: Arc::new(system::remember),
            forget: Arc::new(system::forget),
        }
    }

    /// Elsewhere, no snap nor flatpak: nothing found, nothing moved.
    #[cfg(not(target_os = "linux"))]
    pub fn system() -> Self {
        Self {
            analyse: Arc::new(|_| Analysis {
                report: Err(Blocked::Variables),
                residual: None,
                seen: SeenApps::default(),
                copies: Copies::None,
            }),
            trash: Arc::new(|_, _, _| Err(io::Error::from(io::ErrorKind::Unsupported))),
            remember: Arc::new(|_| {}),
            forget: Arc::new(|_, _| Forgot::Failed(okapi_tools::apps::uninstall::Why::Absent)),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
