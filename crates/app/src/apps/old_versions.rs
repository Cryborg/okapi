//! The tab of the old versions the package managers keep
//! (`okapi_tools::apps::old_versions`): found on a thread of its own each
//! time the list is read, never replaced under what the user checked or
//! confirms; removed on a thread by snapd and flatpak's helper, by
//! identifiers of the report shown only (invariant 1), each written in the
//! history as it goes; the tab and the list read again after. Nothing is
//! checked beforehand (invariant 5).

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use okapi_store::{PackageSource, UninstallEntry, UninstallPart};

use okapi_tools::apps::old_versions::{
    OldKind, OldReading, OldVersion, OldVersionId, OldVersionReport, OldVersions, Retired, Room,
};
use okapi_tools::apps::uninstall::Why;
use okapi_tools::apps::{AppKey, Source};
use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use super::leftovers::shown_name;
use super::rows::Entry;
use super::{AppsScreen, Record};
use crate::shown::{shown, size};
use crate::ui;

/// Finds the old versions, measuring until the flag is set.
pub type Find = Arc<dyn Fn(&AtomicBool) -> OldVersions + Send + Sync>;
/// Asks the daemons to remove old versions of a report, each told as it
/// ends.
pub type Retire =
    Arc<dyn Fn(&OldVersionReport, &[OldVersionId], &dyn Fn(&OldVersion, &Retired)) + Send + Sync>;

/// What the tab asks of the machine: the system's, or a test's.
#[derive(Clone)]
pub struct OldVersionSeams {
    pub find: Find,
    pub retire: Retire,
}

impl std::fmt::Debug for OldVersionSeams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OldVersionSeams")
            .finish_non_exhaustive()
    }
}

/// What a removal said, once over.
#[derive(Debug)]
pub(super) struct Ended {
    removed: usize,
    kept: usize,
    /// Every one kept was for want of the rights.
    refused: bool,
    unrecorded: bool,
}

/// What the tab says of the last removal.
enum Told {
    Ended(Ended),
    /// Its thread ended without a word: what went is in the list read again.
    CutShort,
}

/// A search under way, and how to stop its measures.
struct Finding {
    receiver: Receiver<OldVersions>,
    stop: Arc<AtomicBool>,
}

/// The tab's state, kept by the screen's.
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts of the tab: each source unread, other persons, a search failed, a question open, a search owed"
)]
pub(super) struct OldVersionsState {
    report: Option<Arc<OldVersionReport>>,
    snaps_unread: bool,
    flatpaks_unread: bool,
    for_account_only: usize,
    other_accounts: bool,
    /// The last search ended without a word, or could not start.
    failed: bool,
    /// The old versions checked, by their rank in the report.
    checked: BTreeSet<usize>,
    finding: Option<Finding>,
    retiring: Option<Receiver<Ended>>,
    confirming: bool,
    /// A search asked while one could not start: made once free.
    pub(super) again: bool,
    /// What the last removal said, until closed.
    told: Option<Told>,
    snaps_model: Rc<VecModel<ui::OldVersionLine>>,
    flatpaks_model: Rc<VecModel<ui::OldVersionLine>>,
    going_model: Rc<VecModel<ui::OldVersionLine>>,
}

impl OldVersionsState {
    /// Whether a new report would come under the user's hand.
    pub(super) fn held(&self) -> bool {
        self.busy() || self.confirming || !self.checked.is_empty()
    }

    pub(super) const fn busy(&self) -> bool {
        self.finding.is_some() || self.retiring.is_some()
    }

    pub(super) const fn retiring(&self) -> bool {
        self.retiring.is_some()
    }
}

/// The history line of `version`, gone; `shown` its name.
fn entry_of(version: &OldVersion, shown: &str, time: i64) -> UninstallEntry {
    match version.kind() {
        OldKind::Revision(revision) => {
            let (freed, later) = match revision.room {
                Room::Now(bytes) => (Some(bytes), None),
                Room::Later(bytes) => (Some(0), Some(bytes)),
                Room::Shared(_) => (Some(0), None),
                Room::Unknown => (None, None),
            };
            UninstallEntry {
                time,
                source: PackageSource::Snap,
                name: format!("{shown} {}", revision.version),
                packages: vec![format!("{} {}", revision.snap, revision.revision)],
                freed_bytes: freed,
                // Its data of that revision, for every account.
                data_removed: true,
                part: UninstallPart::OldVersion,
                later_bytes: later,
            }
        }
        OldKind::Runtime(runtime) => UninstallEntry {
            time,
            source: PackageSource::Flatpak,
            name: format!("{} {}", runtime.reference.id(), runtime.reference.branch()),
            packages: vec![runtime.reference.to_string()],
            // The prune may have been skipped: never said freed.
            freed_bytes: None,
            data_removed: false,
            part: UninstallPart::UnusedRuntime,
            later_bytes: None,
        },
    }
}

/// What a line names, as the user knows it: a snap by the name the list
/// shows it, else the one Okapi saw it by, else its own.
fn title(
    version: &OldVersion,
    entries: &HashMap<AppKey, Entry>,
    seen: &okapi_store::SeenApps,
) -> String {
    match version.kind() {
        OldKind::Revision(revision) => entries
            .get(&AppKey {
                source: Source::Snap,
                id: revision.snap.clone(),
            })
            .map_or_else(
                || shown_name(seen, Source::Snap, &revision.snap).0,
                |entry| entry.app.name.clone(),
            ),
        OldKind::Runtime(runtime) => runtime.reference.id().to_owned(),
    }
}

/// What a line gives back, as far as known.
const fn bytes_of(version: &OldVersion) -> Option<u64> {
    match version.kind() {
        OldKind::Revision(revision) => match revision.room {
            Room::Now(bytes) | Room::Later(bytes) => Some(bytes),
            Room::Shared(_) | Room::Unknown => None,
        },
        OldKind::Runtime(runtime) => runtime.bytes,
    }
}

/// What `versions` give back, as far as known.
fn sum<'a>(versions: impl Iterator<Item = &'a OldVersion>) -> u64 {
    versions.filter_map(bytes_of).fold(0, u64::saturating_add)
}

/// The line of `version`, ranked `index` in its report, unchecked and named
/// by its own name; its version in use dated from `now` in `zone`.
fn line_of(
    index: usize,
    version: &OldVersion,
    now: jiff::Timestamp,
    zone: &jiff::tz::TimeZone,
) -> ui::OldVersionLine {
    let index = i32::try_from(index).unwrap_or(i32::MAX);
    match version.kind() {
        OldKind::Revision(revision) => {
            let (room, bytes) = match revision.room {
                Room::Now(bytes) => (ui::OldRoom::Now, bytes),
                Room::Later(bytes) => (ui::OldRoom::Later, bytes),
                Room::Shared(bytes) => (ui::OldRoom::Shared, bytes),
                Room::Unknown => (ui::OldRoom::Unknown, 0),
            };
            let since = revision
                .current_since
                .and_then(|since| crate::dashboard::moment(since.as_second(), now, zone));
            ui::OldVersionLine {
                index,
                source: ui::AppSource::Snap,
                name: revision.snap.as_str().into(),
                version: revision.version.as_str().into(),
                revision: revision.revision.as_str().into(),
                room,
                size: size(bytes),
                has_since: since.is_some(),
                since: since.unwrap_or_default(),
                checked: false,
            }
        }
        OldKind::Runtime(runtime) => ui::OldVersionLine {
            index,
            source: ui::AppSource::Flatpak,
            name: runtime.reference.id().into(),
            version: runtime.reference.branch().into(),
            revision: SharedString::new(),
            room: if runtime.bytes.is_some() {
                ui::OldRoom::About
            } else {
                ui::OldRoom::Unknown
            },
            size: size(runtime.bytes.unwrap_or(0)),
            has_since: false,
            since: ui::Moment::default(),
            checked: false,
        },
    }
}

/// Says on `global` what `checked` gives back, and when.
fn show_checked(global: &ui::OldVersions<'_>, checked: &[&OldVersion]) {
    global.set_checked_count(shown(checked.len()));
    global.set_checked_size(size(sum(checked.iter().copied())));
    let rooms = || {
        checked.iter().filter_map(|version| match version.kind() {
            OldKind::Revision(revision) => Some(revision.room),
            OldKind::Runtime(_) => None,
        })
    };
    let room_sum =
        |wanted: fn(Room) -> Option<u64>| rooms().filter_map(wanted).fold(0, u64::saturating_add);
    let freed_now = room_sum(|room| match room {
        Room::Now(bytes) => Some(bytes),
        _ => None,
    });
    let later = room_sum(|room| match room {
        Room::Later(bytes) => Some(bytes),
        _ => None,
    });
    global.set_checked_snaps(rooms().next().is_some());
    global.set_checked_now(size(freed_now));
    global.set_has_now(freed_now > 0);
    global.set_checked_later(size(later));
    global.set_has_later(later > 0);
    global.set_has_shared(rooms().any(|room| matches!(room, Room::Shared(_))));
    global.set_has_unknown(rooms().any(|room| room == Room::Unknown));
    let runtimes = || {
        checked
            .iter()
            .copied()
            .filter(|version| matches!(version.kind(), OldKind::Runtime(_)))
    };
    global.set_checked_runtimes(runtimes().next().is_some());
    global.set_checked_about(size(sum(runtimes())));
}

impl AppsScreen {
    pub(super) fn attach_old_versions(&self, window: &ui::AppWindow) {
        let old = window.global::<ui::OldVersions>();
        {
            let state = self.0.borrow();
            old.set_snaps(ModelRc::from(Rc::clone(&state.old_versions.snaps_model)));
            old.set_flatpaks(ModelRc::from(Rc::clone(&state.old_versions.flatpaks_model)));
            old.set_going(ModelRc::from(Rc::clone(&state.old_versions.going_model)));
        }
        let weak = Rc::downgrade(&self.0);
        let on = move |action: fn(&Self)| {
            let weak = weak.clone();
            move || {
                if let Some(state) = weak.upgrade() {
                    action(&Self(state));
                }
            }
        };
        old.on_retire(on(Self::ask_retire));
        old.on_cancel(on(Self::cancel_retire));
        old.on_confirm(on(Self::confirm_retire));
        old.on_close_outcome(on(Self::close_retired));
        let weak = Rc::downgrade(&self.0);
        old.on_toggle(move |index| {
            let Some(state) = weak.upgrade() else { return };
            if let Ok(index) = usize::try_from(index) {
                Self(state).toggle_old(index);
            } else {
                tracing::warn!(index, "an old version out of the list refused");
            }
        });
    }

    /// Looks for the old versions on a thread; held until nothing is under
    /// the user's hand.
    pub(super) fn find_old_versions(&self) {
        if self.0.borrow().old_versions.held() {
            tracing::info!("old versions looked for once the user lets go");
            self.0.borrow_mut().old_versions.again = true;
            return;
        }
        let find = Arc::clone(&self.0.borrow().seams.old_versions.find);
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("okapi-old-versions".to_owned())
            .spawn(move || {
                let _ = sender.send(find(&thread_stop));
            });
        {
            let mut state = self.0.borrow_mut();
            let old = &mut state.old_versions;
            old.again = false;
            match spawned {
                Ok(_) => old.finding = Some(Finding { receiver, stop }),
                Err(error) => {
                    tracing::error!(%error, "no thread to look for the old versions");
                    old.failed = true;
                }
            }
        }
        self.show_old_versions();
        self.watch();
    }

    /// The page left: measures stop, what was found is shown.
    pub(super) fn stop_old_versions(&self) {
        if let Some(finding) = &self.0.borrow().old_versions.finding {
            finding.stop.store(true, Ordering::Relaxed);
        }
    }

    /// What the search or the removal said; a search owed made once free.
    pub(super) fn take_old_versions(&self) {
        let found = {
            let state = self.0.borrow();
            state
                .old_versions
                .finding
                .as_ref()
                .map(|finding| finding.receiver.try_recv())
        };
        match found {
            Some(Ok(found)) => self.found_old_versions(found),
            Some(Err(TryRecvError::Disconnected)) => {
                tracing::error!("the search of old versions ended without a word");
                {
                    let mut state = self.0.borrow_mut();
                    state.old_versions.finding = None;
                    state.old_versions.failed = true;
                }
                self.show_old_versions();
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        let retired = {
            let state = self.0.borrow();
            state.old_versions.retiring.as_ref().map(Receiver::try_recv)
        };
        match retired {
            Some(Ok(ended)) => {
                tracing::info!(?ended, "old versions' removal told");
                self.retired(Some(ended));
            }
            Some(Err(TryRecvError::Disconnected)) => {
                tracing::error!("the removal of old versions ended without a word");
                self.retired(None);
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        let again = {
            let state = self.0.borrow();
            state.old_versions.again && !state.old_versions.held()
        };
        if again {
            self.find_old_versions();
        }
    }

    fn found_old_versions(&self, found: OldVersions) {
        {
            let mut state = self.0.borrow_mut();
            let old = &mut state.old_versions;
            old.finding = None;
            old.failed = false;
            // None can be checked while a search runs: nothing is lost.
            old.checked.clear();
            old.snaps_unread = matches!(found.snaps, OldReading::Unread(_));
            old.flatpaks_unread = matches!(found.flatpaks, OldReading::Unread(_));
            old.for_account_only = found.for_account_only;
            old.other_accounts = found.other_accounts;
            tracing::info!(found = found.report.versions().len(), "old versions shown");
            old.report = Some(Arc::new(found.report));
        }
        self.show_old_versions();
    }

    fn toggle_old(&self, index: usize) {
        {
            let mut state = self.0.borrow_mut();
            let old = &mut state.old_versions;
            let in_report = old
                .report
                .as_ref()
                .is_some_and(|report| index < report.versions().len());
            if old.busy() || old.confirming {
                tracing::info!(
                    index,
                    "a box ticked while old versions are read or removed: refused"
                );
            } else if !in_report {
                tracing::info!(index, "an old version out of the list refused");
            } else if !old.checked.remove(&index) {
                old.checked.insert(index);
            }
        }
        self.show_old_versions();
    }

    fn ask_retire(&self) {
        {
            let mut state = self.0.borrow_mut();
            let elsewhere = state.operation.is_some() || state.leftovers.acting();
            let old = &mut state.old_versions;
            if old.checked.is_empty() {
                tracing::warn!("a removal with nothing checked refused");
            } else if old.busy() || elsewhere {
                tracing::warn!("a removal of old versions while another action runs refused");
            } else {
                old.confirming = true;
            }
        }
        self.show_old_versions();
    }

    fn cancel_retire(&self) {
        self.0.borrow_mut().old_versions.confirming = false;
        self.show_old_versions();
        // A search owed meanwhile waits for the boxes to be let go.
    }

    /// Asks the daemons, on a thread, to remove what is checked: each read
    /// again there, each written in the history as it goes.
    fn confirm_retire(&self) {
        let asked = {
            let mut state = self.0.borrow_mut();
            let elsewhere = state.operation.is_some() || state.leftovers.acting();
            let old = &mut state.old_versions;
            old.confirming = false;
            match old.report.clone() {
                Some(report) if !old.busy() && !elsewhere && !old.checked.is_empty() => {
                    let ids: Vec<OldVersionId> = old
                        .checked
                        .iter()
                        .filter_map(|index| report.versions().get(*index))
                        .map(OldVersion::id)
                        .collect();
                    Some((report, ids))
                }
                _ => {
                    tracing::warn!(
                        "a removal confirmed with none checked, or while another runs: refused"
                    );
                    None
                }
            }
        };
        let Some((report, ids)) = asked else {
            self.show_old_versions();
            return;
        };
        tracing::info!(asked = ids.len(), "removal of old versions confirmed");
        let (retire, record, now, names) = {
            let state = self.0.borrow();
            let seen = &state.leftovers.seen;
            let names: Vec<String> = report
                .versions()
                .iter()
                .map(|version| title(version, &state.entries, seen))
                .collect();
            (
                Arc::clone(&state.seams.old_versions.retire),
                Arc::clone(&state.seams.record),
                state.seams.now,
                names,
            )
        };
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("okapi-old-versions-removal".to_owned())
            .spawn(move || {
                let _ = sender.send(retire_and_record(
                    &report, &ids, &retire, &record, now, &names,
                ));
            });
        match spawned {
            Ok(_) => {
                let mut state = self.0.borrow_mut();
                state.old_versions.retiring = Some(receiver);
                state.old_versions.told = None;
            }
            Err(error) => {
                tracing::error!(%error, "no thread to remove old versions: nothing asked");
                self.0.borrow_mut().old_versions.told = Some(Told::Ended(Ended {
                    removed: 0,
                    kept: 0,
                    refused: false,
                    unrecorded: false,
                }));
            }
        }
        self.show_old_versions();
        self.watch();
    }

    fn retired(&self, ended: Option<Ended>) {
        {
            let mut state = self.0.borrow_mut();
            let old = &mut state.old_versions;
            old.retiring = None;
            old.checked.clear();
            old.told = Some(ended.map_or(Told::CutShort, Told::Ended));
            old.again = true;
        }
        self.show_old_versions();
        // What the daemons keep changed: the list says it too.
        self.refresh();
    }

    fn close_retired(&self) {
        self.0.borrow_mut().old_versions.told = None;
        self.show_old_versions();
    }

    /// Shows the tab as it stands.
    pub(super) fn show_old_versions(&self) {
        let window = self.0.borrow().window.upgrade();
        let Some(window) = window else { return };
        let old_global = window.global::<ui::OldVersions>();
        let state = self.0.borrow();
        let old = &state.old_versions;
        let now = (state.seams.now)();
        let versions = old
            .report
            .as_ref()
            .map_or(&[][..], |report| report.versions());
        let lines = |wanted: &dyn Fn(usize, &OldVersion) -> bool| -> Vec<ui::OldVersionLine> {
            versions
                .iter()
                .enumerate()
                .filter(|(index, version)| wanted(*index, version))
                .map(|(index, version)| ui::OldVersionLine {
                    checked: old.checked.contains(&index),
                    name: title(version, &state.entries, &state.leftovers.seen).into(),
                    ..line_of(index, version, now, &state.seams.zone)
                })
                .collect()
        };
        let is_revision = |version: &OldVersion| matches!(version.kind(), OldKind::Revision(_));
        crate::shown::show(&old.snaps_model, lines(&|_, version| is_revision(version)));
        crate::shown::show(
            &old.flatpaks_model,
            lines(&|_, version| !is_revision(version)),
        );
        crate::shown::show(
            &old.going_model,
            lines(&|index, _| old.checked.contains(&index)),
        );
        let total = sum(versions.iter());
        old_global.set_total(size(total));
        old_global.set_has_total(total > 0);
        let checked: Vec<&OldVersion> = old
            .checked
            .iter()
            .filter_map(|index| versions.get(*index))
            .collect();
        show_checked(&old_global, &checked);
        old_global.set_searched(old.report.is_some());
        old_global.set_analysis_failed(old.failed);
        old_global.set_snaps_unread(old.snaps_unread);
        old_global.set_flatpaks_unread(old.flatpaks_unread);
        old_global.set_for_account_only(shown(old.for_account_only));
        old_global.set_other_accounts(old.other_accounts);
        old_global.set_finding(old.finding.is_some());
        old_global.set_retiring(old.retiring.is_some());
        old_global.set_confirming(old.confirming);
        old_global.set_busy(old.busy() || state.operation.is_some() || state.leftovers.busy());
        let (outcome, removed, kept, unrecorded) = match &old.told {
            None => (ui::OldOutcome::None, 0, 0, false),
            Some(Told::CutShort) => (ui::OldOutcome::Interrupted, 0, 0, false),
            Some(Told::Ended(ended)) => (
                match (ended.removed, ended.kept) {
                    (0, _) if ended.refused => ui::OldOutcome::Refused,
                    (0, _) => ui::OldOutcome::Nothing,
                    (_, 0) => ui::OldOutcome::Done,
                    _ => ui::OldOutcome::Partly,
                },
                ended.removed,
                ended.kept,
                ended.unrecorded,
            ),
        };
        old_global.set_outcome(outcome);
        old_global.set_removed(shown(removed));
        old_global.set_kept(shown(kept));
        old_global.set_unrecorded(unrecorded);
    }

    /// The window closes: a removal under way is left to the daemons, which
    /// end it; what goes after is not in the history (traced).
    pub(super) fn leave_old_versions(&self) {
        if self.0.borrow().old_versions.retiring() {
            tracing::warn!(
                "a removal of old versions left at closing: the daemons end what they took, its last history lines may be missing"
            );
        }
        self.stop_old_versions();
    }
}

/// Removes `ids` of `report` by `retire`, writing each gone in the history
/// by `record` as it goes, `names` naming each by its rank: what it said.
fn retire_and_record(
    report: &OldVersionReport,
    ids: &[OldVersionId],
    retire: &Retire,
    record: &Record,
    now: fn() -> jiff::Timestamp,
    names: &[String],
) -> Ended {
    let ended = std::cell::RefCell::new(Ended {
        removed: 0,
        kept: 0,
        refused: true,
        unrecorded: false,
    });
    retire(report, ids, &|version, retired| {
        let mut ended = ended.borrow_mut();
        match retired {
            Retired::Removed => {
                ended.removed += 1;
                let rank = report
                    .versions()
                    .iter()
                    .position(|listed| listed.id() == version.id());
                let name = rank
                    .and_then(|rank| names.get(rank))
                    .map_or("", String::as_str);
                let entry = entry_of(version, name, now().as_second());
                if let Err(error) = record(&entry) {
                    tracing::warn!(%error, name, "an old version removed: its history line not written");
                    ended.unrecorded = true;
                }
            }
            // Gone before, or by another: not this removal's.
            Retired::Gone => {}
            Retired::Failed(Why::NotAuthorized) | Retired::NotAsked => ended.kept += 1,
            Retired::Changed | Retired::Failed(_) => {
                ended.kept += 1;
                ended.refused = false;
            }
        }
    });
    let mut ended = ended.into_inner();
    ended.refused &= ended.kept > 0;
    ended
}

#[cfg(target_os = "linux")]
mod system {
    //! The system's search and removal.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    use okapi_rules::Variable;
    use okapi_tools::apps::leftovers::InstalledPlaces;
    use okapi_tools::apps::leftovers::installed::SNAP_FILES;
    use okapi_tools::apps::old_versions::{
        self, ACCOUNTS, OldVersion, OldVersionId, OldVersionReport, OldVersions, REVISION_WAIT,
        Retired,
    };
    use okapi_tools::packages::flatpak::SystemHelper;
    use okapi_tools::packages::snapd::Snapd;

    pub(crate) fn find(stop: &AtomicBool) -> OldVersions {
        // Without the user's data folder, the user's installation is
        // unplaced: no runtime is proposed.
        let data_home = okapi_platform::template_variables()
            .inspect_err(|error| tracing::warn!(?error, "the session's folders unread"))
            .ok()
            .and_then(|variables| {
                variables
                    .value_of(Variable::LocalShare)
                    .map(Path::to_path_buf)
            });
        let places = InstalledPlaces::new(
            PathBuf::from(SNAP_FILES),
            okapi_tools::apps::flatpak::installations(data_home.as_deref()),
            None,
        );
        old_versions::find(Snapd::system().snaps(), places, Path::new(ACCOUNTS), stop)
    }

    pub(crate) fn retire(
        report: &OldVersionReport,
        ids: &[OldVersionId],
        told: &dyn Fn(&OldVersion, &Retired),
    ) {
        old_versions::retire(
            report,
            ids,
            &Snapd::system(),
            &SystemHelper::connect,
            REVISION_WAIT,
            told,
        );
    }
}

impl OldVersionSeams {
    /// The system's.
    #[cfg(target_os = "linux")]
    pub fn system() -> Self {
        Self {
            find: Arc::new(system::find),
            retire: Arc::new(system::retire),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
