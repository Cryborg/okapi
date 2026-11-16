//! The disk usage screen: an analysis of the home folder or of a volume
//! (`okapi_tools::disk_usage`) on threads of its own, read every half second
//! folder by folder, its lines sorted and gathered (`rows`), the folder
//! chosen opened in the file manager through the desktop portal
//! (`crate::portal`).
//! Nothing here removes anything.

mod rows;

use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use jiff::Timestamp;
use okapi_platform::MountList;
use okapi_rules::Language;
use okapi_tools::disk_usage::{
    FileEntry, FilesError, NodeId, Outcome, ScanError, UsageScan, View, default_threads,
};
use okapi_tools::monitor::{Unavailable, Volume, VolumeKind, VolumeSpace};
use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use crate::dates::MINUTES_EVERY;
use crate::file_manager::{Calls, Open, OpenRequest, open_in_file_manager};
use crate::shown::{grouped, shown, size};
use crate::ui;
use rows::{Arranged, Held, Key, Line, LineKind, LineState, Navigation, Order};

/// How often the screen reads the analysis.
pub(crate) const READ_EVERY: Duration = Duration::from_millis(500);
/// How often the screen reads an analysis whose threads are held by a
/// filesystem that stopped answering: what they still write shows.
const SLOW_READ_EVERY: Duration = Duration::from_secs(5);
/// How long the files of a folder may take to be read: a filesystem that
/// stopped answering would hold every folder's after it.
const FILES_WAIT: Duration = Duration::from_secs(20);
/// How many readings an analysis ended may take to settle before the screen
/// reads it only now and then: a thread held by a filesystem that stopped
/// answering never ends.
const SETTLE_READINGS: u32 = 40;
/// How many readings of the start the volumes may take to come before they
/// are said unreadable: the Home page's sampler reads them at once.
const VOLUMES_READINGS: u32 = 20;

/// The volumes the system has, as last read; `None` before a first reading.
pub type Volumes = Rc<dyn Fn() -> Option<Result<Vec<Volume>, Unavailable>>>;

/// What the screen reads and asks of the machine: the system's, or a
/// test's.
#[derive(Clone)]
pub struct DiskSeams {
    /// The home folder and the user's cache folder.
    pub home: Option<PathBuf>,
    pub cache: Option<PathBuf>,
    pub mounts: Rc<dyn Fn() -> Option<MountList>>,
    pub open: Open,
    pub now: fn() -> Timestamp,
    pub threads: NonZeroUsize,
}

impl std::fmt::Debug for DiskSeams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DiskSeams")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl DiskSeams {
    /// The session's folders, the mounts of this process, the portal.
    pub fn system() -> Self {
        let variables = okapi_platform::template_variables()
            .inspect_err(|error| tracing::warn!(?error, "home folder unknown"))
            .ok();
        let folder = |wanted| {
            variables
                .as_ref()
                .and_then(|variables| variables.value_of(wanted))
                .map(Path::to_path_buf)
        };
        Self {
            home: folder(okapi_rules::Variable::Home),
            // As the analysis names its folders: through no link.
            cache: folder(okapi_rules::Variable::Cache)
                .map(|cache| cache.canonicalize().unwrap_or(cache)),
            mounts: Rc::new(|| {
                okapi_platform::mounts()
                    .inspect_err(|error| tracing::warn!(%error, "mounts unreadable"))
                    .ok()
            }),
            open: Rc::new(open_in_file_manager),
            now: Timestamp::now,
            threads: default_threads(),
        }
    }
}

/// What is analyzed.
#[derive(Debug, Clone)]
struct Target {
    root: PathBuf,
    /// A volume's name, or none for the home folder.
    volume: Option<VolumeTarget>,
}

/// A volume analyzed whole: its space when the analysis started, and
/// whether what it could not see can be told.
#[derive(Debug, Clone)]
struct VolumeTarget {
    shown: ui::VolumeShown,
    total: u64,
    available: u64,
    told: bool,
}

/// How the analysis stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Opening,
    Running,
    Ended(Outcome),
}

/// The files of a folder, read again away from the interface's thread.
type FilesRead = Result<(Vec<FileEntry>, usize), FilesError>;

/// The files of `folder` on their way, asked at `asked`.
#[derive(Debug)]
struct FilesReading {
    folder: NodeId,
    receiver: Receiver<FilesRead>,
    asked: std::time::Instant,
}

/// The files of a folder, as read again: a failure is kept, never read
/// again on each reading.
#[derive(Debug)]
enum Files {
    /// Those read, and how many could not be.
    Listed(Vec<FileEntry>, usize),
    /// No longer the folder the analysis read.
    Changed,
    Unreadable,
}

struct State {
    window: slint::Weak<ui::AppWindow>,
    seams: DiskSeams,
    language: Language,
    volumes_source: Option<Volumes>,
    /// The volumes offered, as last read whole, and how many readings of
    /// the start they did not come in.
    volumes: Vec<Volume>,
    volumes_status: ui::DiskVolumes,
    volumes_waited: u32,
    volume_model: Rc<VecModel<ui::VolumeShown>>,
    line_model: Rc<VecModel<ui::DiskLine>>,
    trail_model: Rc<VecModel<SharedString>>,
    target: Option<Target>,
    mounts: Option<MountList>,
    /// A channel per start: one given up is dropped with its receiver.
    opening: Option<Receiver<Result<UsageScan, ScanError>>>,
    scan: Option<Arc<UsageScan>>,
    phase: Phase,
    ended_at: Option<Timestamp>,
    settle_readings: u32,
    navigation: Navigation,
    order: Order,
    /// The view last shown, to redo nothing when it did not change.
    last_view: Option<View>,
    tick: u64,
    /// The line under the pointer, and whether the user moves in the list
    /// with the keyboard (on the line `current`): as indexes of `shown`,
    /// what is there keeps its place.
    pointed: Option<usize>,
    list_focused: bool,
    chosen: Option<Chosen>,
    expanded: bool,
    /// The lines shown, in their order, for an index to name its line.
    shown: Vec<Shown>,
    files: Option<(NodeId, Files)>,
    files_reading: Option<FilesReading>,
    /// The call to the file manager awaited.
    calls: Calls,
    timer: slint::Timer,
    minutes_timer: slint::Timer,
}

/// What a line of the list is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shown {
    Line(Line),
    Others,
    Unexplained,
}

impl Shown {
    /// Whether it is `other`, read again: the same line, its size aside.
    fn is(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Line(line), Self::Line(other)) => line.key == other.key,
            (Self::Others, Self::Others) | (Self::Unexplained, Self::Unexplained) => true,
            _ => false,
        }
    }
}

/// The line whose panel is open.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Chosen {
    Line(Key),
    Unexplained,
}

/// The disk usage screen.
#[derive(Clone)]
pub struct DiskUsageScreen(Rc<RefCell<State>>);

impl std::fmt::Debug for DiskUsageScreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DiskUsageScreen")
            .finish_non_exhaustive()
    }
}

impl DiskUsageScreen {
    pub fn attach(window: &ui::AppWindow, seams: DiskSeams, language: Language) -> Self {
        let state = State {
            window: window.as_weak(),
            seams,
            language,
            volumes_source: None,
            volumes: Vec::new(),
            volumes_status: ui::DiskVolumes::Reading,
            volumes_waited: 0,
            volume_model: Rc::new(VecModel::default()),
            line_model: Rc::new(VecModel::default()),
            trail_model: Rc::new(VecModel::default()),
            target: None,
            mounts: None,
            opening: None,
            scan: None,
            phase: Phase::Ended(Outcome::Complete),
            ended_at: None,
            settle_readings: 0,
            navigation: Navigation::default(),
            order: Order::default(),
            last_view: None,
            tick: 0,
            pointed: None,
            list_focused: false,
            chosen: None,
            expanded: false,
            shown: Vec::new(),
            files: None,
            files_reading: None,
            calls: Calls::default(),
            timer: slint::Timer::default(),
            minutes_timer: slint::Timer::default(),
        };
        let disk = window.global::<ui::DiskUsage>();
        disk.set_volumes(ModelRc::from(Rc::clone(&state.volume_model)));
        disk.set_lines(ModelRc::from(Rc::clone(&state.line_model)));
        disk.set_trail(ModelRc::from(Rc::clone(&state.trail_model)));
        let screen = Self(Rc::new(RefCell::new(state)));
        let weak = Rc::downgrade(&screen.0);
        let on = move |action: fn(&Self)| {
            let weak = weak.clone();
            move || {
                if let Some(state) = weak.upgrade() {
                    action(&Self(state));
                }
            }
        };
        disk.on_analyze_home(on(Self::analyze_home));
        disk.on_stop(on(Self::stop));
        disk.on_change(on(Self::change));
        disk.on_again(on(Self::again));
        disk.on_up(on(Self::up));
        disk.on_close_detail(on(Self::close_detail));
        disk.on_open_chosen(on(Self::open_chosen));
        disk.on_open_cleaner(on(Self::open_cleaner));
        let weak = Rc::downgrade(&screen.0);
        let with_index = move |action: fn(&Self, usize)| {
            let weak = weak.clone();
            move |index: i32| {
                if let (Some(state), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                    action(&Self(state), index);
                }
            }
        };
        disk.on_analyze_volume(with_index(Self::analyze_volume));
        disk.on_enter(with_index(Self::enter));
        disk.on_go_to(with_index(Self::go_to));
        disk.on_choose(with_index(Self::choose));
        disk.on_analyze_mount(with_index(Self::analyze_mount));
        let weak = Rc::downgrade(&screen.0);
        disk.on_point(move |index| {
            if let Some(state) = weak.upgrade() {
                Self(state).point(usize::try_from(index).ok());
            }
        });
        let weak = Rc::downgrade(&screen.0);
        disk.on_list_focus(move |focused| {
            if let Some(state) = weak.upgrade() {
                Self(state).list_focus(focused);
            }
        });
        screen.set_step(ui::DiskStep::Start);
        screen
    }

    fn point(&self, index: Option<usize>) {
        self.0.borrow_mut().pointed = index;
        self.released();
    }

    fn list_focus(&self, focused: bool) {
        self.0.borrow_mut().list_focused = focused;
        self.released();
    }

    /// Nothing held any more: what the hold put off is done at the next
    /// reading (not at once, the pointer may only be crossing to the next
    /// line).
    fn released(&self) {
        let owed = {
            let mut state = self.0.borrow_mut();
            let owed = state.pointed.is_none() && !state.list_focused && state.order.owed();
            if owed {
                state.order.sort_next();
            }
            owed
        };
        if owed && self.0.borrow().scan.is_some() {
            self.watch();
        }
    }

    /// Where the volumes are read from: the Home page's readings.
    pub fn read_volumes_from(&self, volumes: Volumes) {
        self.0.borrow_mut().volumes_source = Some(volumes);
    }

    fn with_window(&self, show: impl FnOnce(&ui::AppWindow)) {
        self.with_window_value(show);
    }

    fn with_window_value<T>(&self, read: impl FnOnce(&ui::AppWindow) -> T) -> Option<T> {
        let window = self.0.borrow().window.upgrade();
        window.map(|window| read(&window))
    }

    fn set_step(&self, step: ui::DiskStep) {
        self.with_window(|window| window.global::<ui::DiskUsage>().set_step(step));
    }

    /// Follows the page shown: the volumes are read while the start is
    /// shown, and only then.
    pub fn page_shown(&self, page: ui::Page) {
        if page == ui::Page::DiskUsage {
            self.0.borrow_mut().volumes_waited = 0;
            self.show_volumes();
            self.watch();
        } else if self.step() == Some(ui::DiskStep::Start) {
            self.0.borrow().timer.stop();
        }
    }

    fn step(&self) -> Option<ui::DiskStep> {
        self.with_window_value(|window| window.global::<ui::DiskUsage>().get_step())
    }

    /// Starts the screen's readings.
    fn watch(&self) {
        self.watch_every(READ_EVERY);
    }

    fn watch_every(&self, every: Duration) {
        let weak = Rc::downgrade(&self.0);
        self.0
            .borrow()
            .timer
            .start(slint::TimerMode::Repeated, every, move || {
                if let Some(state) = weak.upgrade() {
                    Self(state).read();
                }
            });
    }

    /// One reading: the volumes on the start, the analysis while it runs or
    /// settles, the files asked for.
    fn read(&self) {
        if self.step() == Some(ui::DiskStep::Start) {
            self.show_volumes();
            return;
        }
        self.0.borrow_mut().tick += 1;
        self.take_opening();
        self.take_files();
        self.follow_end();
        self.show_folder(false);
    }

    /// The volumes offered: local or removable, their space known.
    fn show_volumes(&self) {
        let read = self
            .0
            .borrow()
            .volumes_source
            .as_ref()
            .map(|source| source());
        let (waited, before) = {
            let mut state = self.0.borrow_mut();
            state.volumes_waited = state.volumes_waited.saturating_add(1);
            (state.volumes_waited, state.volumes_status)
        };
        let (state_shown, volumes) = match read {
            None | Some(None) if waited < VOLUMES_READINGS => {
                (ui::DiskVolumes::Reading, Vec::new())
            }
            None | Some(None) => {
                if before != ui::DiskVolumes::Unavailable {
                    tracing::warn!(readings = waited, "the volumes never came");
                }
                (ui::DiskVolumes::Unavailable, Vec::new())
            }
            Some(Some(Err(error))) => {
                if before != ui::DiskVolumes::Unavailable {
                    tracing::warn!(?error, "the volumes cannot be offered");
                }
                (ui::DiskVolumes::Unavailable, Vec::new())
            }
            Some(Some(Ok(volumes))) => (
                ui::DiskVolumes::Ready,
                volumes.into_iter().filter(offered).collect(),
            ),
        };
        let ready = state_shown == ui::DiskVolumes::Ready;
        // The volume chosen stays chosen, wherever it now is; one gone
        // leaves the home folder chosen. A list not read is no list: the
        // last one read is kept, for the choice and the disks met below.
        let choice_now = if ready {
            let choice =
                self.with_window_value(|window| window.global::<ui::DiskUsage>().get_choice());
            let chosen_mount = choice
                .and_then(|choice| usize::try_from(choice).ok())
                .and_then(|choice| {
                    self.0
                        .borrow()
                        .volumes
                        .get(choice)
                        .map(|volume| volume.mount_point.clone())
                });
            chosen_mount.map(|mount| {
                volumes
                    .iter()
                    .position(|volume| volume.mount_point == mount)
                    .map_or(-1, shown)
            })
        } else {
            None
        };
        let rows: Vec<ui::VolumeShown> = volumes.iter().map(crate::dashboard::volume).collect();
        let model = Rc::clone(&self.0.borrow().volume_model);
        crate::shown::show(&model, rows);
        {
            let mut state = self.0.borrow_mut();
            if ready {
                state.volumes = volumes;
            }
            state.volumes_status = state_shown;
        }
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_volumes_state(state_shown);
            if let Some(choice) = choice_now {
                disk.set_choice(choice);
            }
        });
    }

    fn analyze_home(&self) {
        let home = self.0.borrow().seams.home.clone();
        if let Some(home) = home {
            self.start(&Target {
                root: home,
                volume: None,
            });
        } else {
            tracing::warn!("home folder unknown: nothing analyzed");
            self.start_failed(ui::DiskRefusal::Folder);
        }
    }

    fn analyze_volume(&self, index: usize) {
        let volume = self.0.borrow().volumes.get(index).cloned();
        if let Some(volume) = volume {
            self.start_volume(&volume);
        } else {
            tracing::warn!(index, "no volume offered there: nothing analyzed");
            self.start_failed(ui::DiskRefusal::Volume);
        }
    }

    /// Another disk met below: analyzed as the volume it is.
    fn analyze_mount(&self, index: usize) {
        let volume = self.mount_of(index).and_then(|path| self.volume_at(&path));
        if let Some(volume) = volume {
            self.start_volume(&volume);
        } else {
            tracing::warn!(index, "no volume offered there: nothing analyzed");
            self.start_failed(ui::DiskRefusal::Volume);
        }
    }

    /// The volume offered whose mount point is `path`.
    fn volume_at(&self, path: &Path) -> Option<Volume> {
        self.0
            .borrow()
            .volumes
            .iter()
            .find(|volume| volume.mount_point == path)
            .cloned()
    }

    fn start_volume(&self, volume: &Volume) {
        let VolumeSpace::Known(space) = volume.space else {
            tracing::warn!(mount = %volume.mount_point.display(), "volume of unknown space: nothing analyzed");
            self.start_failed(ui::DiskRefusal::Volume);
            return;
        };
        let mounts = (self.0.borrow().seams.mounts)();
        let told = mounts
            .as_ref()
            .is_some_and(|mounts| rows::unexplained_told(&volume.mount_point, mounts));
        let shown = crate::dashboard::volume(volume);
        self.0.borrow_mut().mounts = mounts;
        self.start(&Target {
            root: volume.mount_point.clone(),
            volume: Some(VolumeTarget {
                shown,
                total: space.total,
                available: space.available,
                told,
            }),
        });
    }

    /// Starts analyzing `target` on a thread of its own: opening its root
    /// may wait on a filesystem that stopped answering.
    fn start(&self, target: &Target) {
        self.give_up();
        let (sender, receiver) = mpsc::channel();
        {
            let mut state = self.0.borrow_mut();
            state.opening = Some(receiver);
            state.phase = Phase::Opening;
            state.navigation = Navigation::default();
            state.order.reset();
            state.last_view = None;
            state.chosen = None;
            state.pointed = None;
            state.expanded = false;
            state.files = None;
            state.shown.clear();
            state.ended_at = None;
            state.settle_readings = 0;
            if target.volume.is_none() {
                state.mounts = (state.seams.mounts)();
            }
            state.target = Some(target.clone());
        }
        let threads = self.0.borrow().seams.threads;
        let root = target.root.clone();
        tracing::info!(root = %root.display(), volume = target.volume.is_some(), "disk usage asked");
        let spawned = std::thread::Builder::new()
            .name("okapi-disk-usage-start".into())
            .spawn(move || {
                // Given up, or the screen gone: then nobody waits, and the
                // analysis refused stops as it is dropped.
                let _ = sender.send(UsageScan::start(&root, threads));
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "disk usage not started");
            self.start_failed(ui::DiskRefusal::Folder);
            return;
        }
        // Nothing of the analysis before stays to be chosen.
        {
            let state = self.0.borrow();
            crate::shown::show(&state.line_model, Vec::new());
            crate::shown::show(&state.trail_model, Vec::new());
        }
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_start_refused(ui::DiskRefusal::None);
            disk.set_open_refused(false);
            disk.set_chosen(-1);
            disk.set_current(0);
            disk.set_phase(ui::DiskPhase::Running);
            disk.set_step(ui::DiskStep::Opening);
        });
        self.show_target();
        self.watch();
    }

    /// The analysis opened, when it is.
    fn take_opening(&self) {
        let taken = {
            let state = self.0.borrow();
            match state.opening.as_ref().map(Receiver::try_recv) {
                Some(Ok(opened)) => Some(Some(opened)),
                Some(Err(TryRecvError::Disconnected)) => Some(None),
                _ => None,
            }
        };
        let Some(opened) = taken else {
            return;
        };
        self.0.borrow_mut().opening = None;
        match opened {
            Some(Ok(scan)) => {
                let mut state = self.0.borrow_mut();
                state.scan = Some(Arc::new(scan));
                state.phase = Phase::Running;
                drop(state);
                self.set_step(ui::DiskStep::Scanning);
            }
            Some(Err(error)) => {
                tracing::warn!(%error, "disk usage not opened");
                self.start_failed(ui::DiskRefusal::Folder);
            }
            None => {
                tracing::error!("disk usage start thread ended without word");
                self.start_failed(ui::DiskRefusal::Folder);
            }
        }
    }

    /// The analysis did not start, for `refusal`: said on the start, shown
    /// again with the volumes as they are now.
    fn start_failed(&self, refusal: ui::DiskRefusal) {
        self.give_up();
        {
            let mut state = self.0.borrow_mut();
            state.opening = None;
            state.phase = Phase::Ended(Outcome::Cancelled);
        }
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_start_refused(refusal);
            disk.set_step(ui::DiskStep::Start);
        });
        self.show_volumes();
        self.watch();
    }

    /// The end of the analysis, then of its last readings.
    fn follow_end(&self) {
        let scan = self.0.borrow().scan.clone();
        let Some(scan) = scan else {
            return;
        };
        let phase = self.0.borrow().phase;
        if phase == Phase::Running
            && let Some(outcome) = scan.wait(Duration::ZERO)
        {
            let now = (self.0.borrow().seams.now)();
            {
                let mut state = self.0.borrow_mut();
                state.phase = Phase::Ended(outcome);
                state.ended_at = Some(now);
            }
            tracing::info!(?outcome, "disk usage ended");
            self.0.borrow_mut().order.sort_next();
            self.with_window(|window| {
                let disk = window.global::<ui::DiskUsage>();
                disk.set_phase(match outcome {
                    Outcome::Complete => ui::DiskPhase::Complete,
                    Outcome::Cancelled => ui::DiskPhase::Stopped,
                });
                disk.set_step(ui::DiskStep::Result);
                disk.set_minutes(0);
            });
            self.watch_minutes();
            // Its files, now that every folder is read.
            let current = self.0.borrow().navigation.current();
            self.read_files(current);
        }
        if matches!(self.0.borrow().phase, Phase::Ended(_)) {
            let settled = scan.settled();
            let readings = {
                let mut state = self.0.borrow_mut();
                state.settle_readings += 1;
                state.settle_readings
            };
            let files_read = self.0.borrow().files_reading.is_none();
            if settled && files_read {
                // A last reading of what they wrote, then the screen rests.
                self.show_folder(true);
                self.0.borrow().timer.stop();
            } else if readings >= SETTLE_READINGS {
                if readings == SETTLE_READINGS {
                    tracing::warn!("disk usage threads still reading: read now and then");
                }
                if files_read {
                    self.watch_every(SLOW_READ_EVERY);
                }
            }
        }
    }

    fn watch_minutes(&self) {
        let weak = Rc::downgrade(&self.0);
        self.0
            .borrow()
            .minutes_timer
            .start(slint::TimerMode::Repeated, MINUTES_EVERY, move || {
                if let Some(state) = weak.upgrade() {
                    Self(state).show_minutes();
                }
            });
    }

    fn show_minutes(&self) {
        let (ended_at, now) = {
            let state = self.0.borrow();
            (state.ended_at, (state.seams.now)())
        };
        if let Some(ended_at) = ended_at {
            let minutes = shown(crate::dates::minutes_since(ended_at, now));
            self.with_window(|window| window.global::<ui::DiskUsage>().set_minutes(minutes));
        }
    }

    /// Lets the analysis go: it stops, its threads end on their own.
    fn give_up(&self) {
        let mut state = self.0.borrow_mut();
        if let Some(scan) = state.scan.take() {
            scan.cancel();
        }
        state.opening = None;
        state.files_reading = None;
        state.minutes_timer.stop();
        drop(state);
        self.forget_open_call();
    }

    /// The call to the file manager is no longer awaited: its answer would
    /// speak of another line.
    fn forget_open_call(&self) {
        self.0.borrow().calls.forget();
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_opening(false);
            disk.set_open_refused(false);
        });
    }

    fn stop(&self) {
        let opening = self.0.borrow().opening.is_some();
        if opening {
            tracing::info!("disk usage stopped while opening");
            self.give_up();
            self.0.borrow_mut().phase = Phase::Ended(Outcome::Cancelled);
            self.set_step(ui::DiskStep::Start);
            return;
        }
        let scan = self.0.borrow().scan.clone();
        if let Some(scan) = scan {
            tracing::info!("disk usage stopped by the user");
            scan.cancel();
            self.read();
        }
    }

    fn change(&self) {
        self.give_up();
        self.0.borrow().timer.stop();
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_choice(-1);
            disk.set_step(ui::DiskStep::Start);
        });
        self.show_volumes();
        self.watch();
    }

    /// The same again; a volume as it is now, its space read again.
    fn again(&self) {
        let target = self.0.borrow().target.clone();
        let Some(target) = target else {
            return;
        };
        if target.volume.is_none() {
            self.start(&target);
            return;
        }
        let read = self
            .0
            .borrow()
            .volumes_source
            .as_ref()
            .and_then(|source| source());
        let volume = match read {
            Some(Ok(volumes)) => volumes
                .into_iter()
                .find(|volume| volume.mount_point == target.root),
            Some(Err(error)) => {
                tracing::warn!(?error, "volumes unreadable");
                None
            }
            // Read again since the page showed, not come yet.
            None => {
                tracing::info!("volumes not read yet");
                None
            }
        };
        if let Some(volume) = volume {
            self.start_volume(&volume);
        } else {
            tracing::warn!(mount = %target.root.display(), "volume not read: nothing analyzed");
            self.start_failed(ui::DiskRefusal::Volume);
        }
    }

    /// What is analyzed, as the header says it.
    fn show_target(&self) {
        let (volume, path) = {
            let state = self.0.borrow();
            let Some(target) = &state.target else {
                return;
            };
            (
                target.volume.as_ref().map(|volume| volume.shown.clone()),
                target.root.display().to_string(),
            )
        };
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_target_is_home(volume.is_none());
            if let Some(volume) = volume {
                disk.set_target_volume(volume);
            }
            disk.set_target_path(path.into());
        });
    }

    /// Reads the folder shown again and shows it: only when it changed, or
    /// when `forced`.
    fn show_folder(&self, forced: bool) {
        let scan = self.0.borrow().scan.clone();
        let Some(scan) = scan else {
            return;
        };
        let current = self.0.borrow().navigation.current();
        let Some(view) = scan.view(current) else {
            tracing::warn!(?current, "folder shown unknown to the analysis");
            return;
        };
        let unchanged = self.0.borrow().last_view.as_ref() == Some(&view);
        let due = self.0.borrow().tick.is_multiple_of(rows::SORT_EVERY_TICKS);
        if unchanged && !forced && !due {
            self.show_progress(&scan);
            return;
        }
        // A folder read since it was shown pending: its files now, once,
        // listed or not.
        let files_wanted = view.directory.state == okapi_tools::disk_usage::NodeState::Read
            && self
                .0
                .borrow()
                .files
                .as_ref()
                .is_none_or(|(of, _)| *of != current)
            && self.0.borrow().files_reading.is_none();
        if files_wanted {
            self.read_files(current);
        }
        self.arrange(&scan, &view);
        self.0.borrow_mut().last_view = Some(view);
        self.show_progress(&scan);
    }

    fn arrange(&self, scan: &UsageScan, view: &View) {
        let current = view.directory.id;
        let files: Vec<FileEntry> = match &self.0.borrow().files {
            Some((of, Files::Listed(files, _))) if *of == current => files.clone(),
            _ => Vec::new(),
        };
        let lines = rows::lines(&view.children, &files);
        let total = view.directory.total.allocated;
        let focus = self
            .with_window_value(|window| window.global::<ui::DiskUsage>().get_current())
            .and_then(|current| usize::try_from(current).ok());
        // What is under the pointer, or else has the focus, keeps its place.
        let (arranged, focused) = {
            let mut state = self.0.borrow_mut();
            let focused = focus.and_then(|index| state.shown.get(index).cloned());
            // The line under the pointer if it can be held (it has a size,
            // or is « Autres »), else the one with the focus.
            let holdable = |index: usize| {
                state
                    .shown
                    .get(index)
                    .filter(|shown| match shown {
                        Shown::Line(line) => line.is_sized(),
                        Shown::Others => true,
                        Shown::Unexplained => false,
                    })
                    .cloned()
            };
            let held_shown = state
                .pointed
                .and_then(holdable)
                .or_else(|| focus.filter(|_| state.list_focused).and_then(holdable));
            let held = match &held_shown {
                Some(Shown::Line(line)) => Held::Line(&line.key),
                Some(Shown::Others) => Held::Others,
                Some(Shown::Unexplained) | None => Held::Nothing,
            };
            let tick = state.tick;
            (state.order.arrange(lines, total, held, tick), focused)
        };
        let unexplained = self.unexplained_line(current, total);
        self.show_lines(scan, view, &arranged, unexplained);
        // The focus follows its line, wherever the others moved it.
        let moved_to = focused.and_then(|focused| {
            self.0
                .borrow()
                .shown
                .iter()
                .position(|shown| shown.is(&focused))
        });
        if let (Some(at), Some(was)) = (moved_to, focus)
            && at != was
        {
            self.with_window(|window| window.global::<ui::DiskUsage>().set_current(shown(at)));
        }
    }

    /// The space the analysis could not see, at the root of a volume whose
    /// space can be told.
    fn unexplained_line(&self, current: NodeId, seen: u64) -> Option<u64> {
        if current != NodeId::ROOT {
            return None;
        }
        let state = self.0.borrow();
        let volume = state.target.as_ref()?.volume.as_ref()?;
        if !tells_unexplained(volume.told, state.phase) {
            return None;
        }
        let taken = volume.total.saturating_sub(volume.available);
        let unexplained = rows::unexplained(taken, seen);
        tracing::debug!(taken, seen, unexplained, "unexplained space");
        Some(unexplained)
    }

    fn show_lines(
        &self,
        scan: &UsageScan,
        view: &View,
        arranged: &Arranged,
        unexplained: Option<u64>,
    ) {
        let total = view.directory.total.allocated;
        let largest = arranged.largest();
        let expanded = self.0.borrow().expanded;
        let Some(folder_path) = scan.path(view.directory.id) else {
            tracing::warn!(folder = ?view.directory.id, "folder shown unknown to the analysis");
            return;
        };
        let mut shown_lines: Vec<Shown> = Vec::new();
        let mut rows_shown: Vec<ui::DiskLine> = Vec::new();
        let mut talls = 0;
        let mut push = |shown_line: Shown, row: ui::DiskLine| {
            let tall = row.tall;
            rows_shown.push(ui::DiskLine {
                talls_before: talls,
                ..row
            });
            if tall {
                talls += 1;
            }
            shown_lines.push(shown_line);
        };
        for line in &arranged.lines {
            push(
                Shown::Line(line.clone()),
                self.row(line, &folder_path, total, largest, false),
            );
        }
        if !arranged.others.is_empty() {
            push(Shown::Others, others_row(arranged, total, expanded));
            if expanded {
                for line in &arranged.others {
                    push(
                        Shown::Line(line.clone()),
                        self.row(line, &folder_path, total, largest, true),
                    );
                }
            }
        }
        for line in arranged.pending.iter().chain(&arranged.sizeless) {
            push(
                Shown::Line(line.clone()),
                self.row(line, &folder_path, total, largest, false),
            );
        }
        if let Some(unexplained) = unexplained {
            push(Shown::Unexplained, unexplained_row(unexplained));
        }
        let model = Rc::clone(&self.0.borrow().line_model);
        crate::shown::show(&model, rows_shown);
        self.0.borrow_mut().shown = shown_lines;
        self.show_trail(scan);
        self.show_detail(scan);
        let partial = view.directory.partial;
        let files_state = files_state(self.0.borrow().files.as_ref(), view.directory.id);
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_folder_size(size(total));
            disk.set_folder_partial(partial);
            disk.set_files_state(files_state);
        });
    }

    /// One line of the list.
    fn row(
        &self,
        line: &Line,
        folder: &Path,
        total: u64,
        largest: u64,
        indented: bool,
    ) -> ui::DiskLine {
        let (state, detail) = self.state_of(line, folder);
        let sized = line.is_sized();
        let counted = matches!(line.state, LineState::CountedElsewhere(_));
        ui::DiskLine {
            kind: line.kind.into(),
            state,
            name: line.name.to_string_lossy().as_ref().into(),
            size: size(line.allocated),
            has_size: sized || counted,
            bar: if sized {
                rows::bar(line.allocated, largest)
            } else {
                0.0
            },
            percent: shown(rows::share(line.allocated, total)),
            below_one: sized && rows::share(line.allocated, total) == 0,
            indented,
            tall: state != ui::DiskLineState::Read,
            talls_before: 0,
            detail: detail.into(),
            count: 0,
            expanded: false,
            enterable: line.kind == LineKind::Folder && line.is_sized(),
        }
    }

    /// What a line says beside its size.
    fn state_of(&self, line: &Line, folder: &Path) -> (ui::DiskLineState, String) {
        match line.state {
            LineState::Read => (ui::DiskLineState::Read, String::new()),
            LineState::Partial => (ui::DiskLineState::Partial, String::new()),
            LineState::Pending => (ui::DiskLineState::Pending, String::new()),
            LineState::Unreadable { denied: true } => (ui::DiskLineState::Denied, String::new()),
            LineState::Unreadable { denied: false } => {
                (ui::DiskLineState::Unreadable, String::new())
            }
            LineState::Changed => (ui::DiskLineState::Changed, String::new()),
            LineState::CountedElsewhere(counter) => {
                let scan = self.0.borrow().scan.clone();
                let path = scan.and_then(|scan| scan.path(counter)).unwrap_or_default();
                (
                    ui::DiskLineState::CountedElsewhere,
                    path.display().to_string(),
                )
            }
            LineState::OtherMount => self.mount_state(&folder.join(&line.name)),
        }
    }

    /// Another disk, or this one mounted apart, at `path`.
    fn mount_state(&self, path: &Path) -> (ui::DiskLineState, String) {
        let state = self.0.borrow();
        let mounts = state
            .mounts
            .as_ref()
            .map_or(&[][..], |mounts| &mounts.mounts[..]);
        let mount = mounts.iter().find(|mount| mount.mount_point == path);
        let root_device = state
            .target
            .as_ref()
            .and_then(|target| rows::disk_of(&target.root, mounts));
        if mount.is_some_and(|mount| Some(mount.device) == root_device) {
            return (ui::DiskLineState::MountedApart, String::new());
        }
        // Named as the volumes offered name it, when it is one of them.
        let named = self.volume_at(path).and_then(|volume| match volume.name {
            okapi_tools::monitor::VolumeName::Label(name)
            | okapi_tools::monitor::VolumeName::Source(name) => Some(name),
            _ => None,
        });
        (
            ui::DiskLineState::OtherMount,
            named.unwrap_or_else(|| path.display().to_string()),
        )
    }

    /// The trail: the first, what is analyzed, is named by the page.
    fn show_trail(&self, scan: &UsageScan) {
        let trail = self.0.borrow().navigation.trail().to_vec();
        let names: Vec<SharedString> = trail
            .iter()
            .enumerate()
            .map(|(index, id)| {
                if index == 0 {
                    SharedString::new()
                } else {
                    scan.view(*id)
                        .map(|view| view.directory.name.to_string_lossy().as_ref().into())
                        .unwrap_or_default()
                }
            })
            .collect();
        let model = Rc::clone(&self.0.borrow().trail_model);
        crate::shown::show(&model, names);
    }

    fn show_progress(&self, scan: &UsageScan) {
        let progress = scan.progress();
        let (volume, seen, phase) = {
            let state = self.0.borrow();
            let phase = state.phase;
            let volume = state
                .target
                .as_ref()
                .and_then(|target| target.volume.clone());
            let seen = scan
                .view(NodeId::ROOT)
                .map_or(0, |view| view.directory.total.allocated);
            (volume, seen, phase)
        };
        let language = self.0.borrow().language;
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_files_seen(grouped(progress.files, language).into());
            disk.set_seen(size(seen));
            disk.set_unreadable(shown(progress.unreadable));
            if let Some(volume) = &volume {
                let taken = volume.total.saturating_sub(volume.available);
                disk.set_determinate(true);
                disk.set_done(rows::bar(seen, taken));
                disk.set_volume_bar(tells_unexplained(volume.told, phase));
                disk.set_unexplained_untold(
                    !volume.told && phase == Phase::Ended(Outcome::Complete),
                );
                let unexplained = rows::unexplained(taken, seen);
                disk.set_seen_part(rows::bar(seen, volume.total));
                disk.set_unexplained_part(rows::bar(unexplained, volume.total));
                disk.set_free_part(rows::bar(volume.available, volume.total));
                disk.set_unexplained(size(unexplained));
                disk.set_free(size(volume.available));
                disk.set_total(size(volume.total));
            } else {
                disk.set_determinate(false);
                disk.set_volume_bar(false);
                disk.set_unexplained_untold(false);
            }
        });
    }

    /// Reads the files of `folder` again, away from the interface's thread:
    /// one reading at a time, its result dropped once another folder shows.
    fn read_files(&self, folder: NodeId) {
        let scan = self.0.borrow().scan.clone();
        let Some(scan) = scan else {
            return;
        };
        if self.0.borrow().files_reading.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("okapi-disk-usage-files".into())
            .spawn(move || {
                // Given up, or the screen gone: then nobody waits.
                let _ = sender.send(scan.files(folder));
            });
        match spawned {
            Ok(_) => {
                self.0.borrow_mut().files_reading = Some(FilesReading {
                    folder,
                    receiver,
                    asked: std::time::Instant::now(),
                });
                // Read on the next readings, the analysis ended or not.
                self.watch();
            }
            Err(error) => {
                tracing::warn!(%error, "files of a folder not read");
                self.0.borrow_mut().files = Some((folder, Files::Unreadable));
            }
        }
    }

    fn take_files(&self) {
        let taken = {
            let mut state = self.0.borrow_mut();
            let Some(reading) = state.files_reading.as_mut() else {
                return;
            };
            match reading.receiver.try_recv() {
                Ok(read) => Some((reading.folder, Some(read))),
                Err(TryRecvError::Disconnected) => {
                    tracing::error!(folder = ?reading.folder, "files thread ended without word");
                    Some((reading.folder, None))
                }
                Err(TryRecvError::Empty) => (reading.asked.elapsed() >= FILES_WAIT).then(|| {
                    tracing::warn!(folder = ?reading.folder, "files of a folder never came");
                    (reading.folder, None)
                }),
            }
        };
        let Some((folder, read)) = taken else {
            return;
        };
        self.0.borrow_mut().files_reading = None;
        // The analysis said why it could not (`UsageScan::files`).
        let files = match read {
            Some(Ok((files, unreadable))) => {
                if unreadable > 0 {
                    tracing::warn!(?folder, unreadable, "files of a folder not listed");
                }
                Some(Files::Listed(files, unreadable))
            }
            // Not read yet: asked again once it is.
            Some(Err(FilesError::NotRead)) => None,
            Some(Err(FilesError::Gone | FilesError::Changed)) => Some(Files::Changed),
            Some(Err(FilesError::Io(_))) | None => Some(Files::Unreadable),
        };
        let current = self.0.borrow().navigation.current();
        if let Some(files) = files
            && folder == current
        {
            let mut state = self.0.borrow_mut();
            state.files = Some((folder, files));
            state.order.sort_next();
        }
        // Left since, or done: the folder shown now is read, if it is not.
        self.show_folder(true);
    }

    /// The line at `index`, as shown.
    fn shown_at(&self, index: usize) -> Option<Shown> {
        self.0.borrow().shown.get(index).cloned()
    }

    /// Enter: into a folder, or « Autres » folded or unfolded.
    fn enter(&self, index: usize) {
        match self.shown_at(index) {
            Some(Shown::Others) => {
                let mut state = self.0.borrow_mut();
                state.expanded = !state.expanded;
                drop(state);
                self.show_folder(true);
            }
            Some(Shown::Line(Line {
                key: Key::Folder(folder),
                state: LineState::Read | LineState::Partial,
                ..
            })) => self.open_folder(folder, Some(0)),
            Some(_) => self.choose(index),
            None => {}
        }
    }

    /// Shows `folder`, the focus on line `focus`.
    fn open_folder(&self, folder: NodeId, focus: Option<usize>) {
        {
            let mut state = self.0.borrow_mut();
            state.navigation.descend(folder);
            Self::forget_folder(&mut state);
        }
        // Its files are read as it shows.
        self.after_move(focus);
    }

    fn forget_folder(state: &mut State) {
        state.order.reset();
        state.last_view = None;
        state.chosen = None;
        state.expanded = false;
        state.shown.clear();
    }

    fn up(&self) {
        let left = {
            let mut state = self.0.borrow_mut();
            let left = state.navigation.up();
            if left.is_some() {
                Self::forget_folder(&mut state);
            }
            left
        };
        let Some(left) = left else {
            return;
        };
        self.after_move(None);
        // The focus back on the folder left.
        let index =
            self.0.borrow().shown.iter().position(
                |shown| matches!(shown, Shown::Line(line) if line.key == Key::Folder(left)),
            );
        self.with_window(|window| {
            window
                .global::<ui::DiskUsage>()
                .set_current(index.map_or(0, shown));
        });
    }

    fn go_to(&self, index: usize) {
        let moved = {
            let mut state = self.0.borrow_mut();
            let moved = index + 1 < state.navigation.trail().len() && state.navigation.go_to(index);
            if moved {
                Self::forget_folder(&mut state);
            }
            moved
        };
        if moved {
            self.after_move(Some(0));
        }
    }

    fn after_move(&self, focus: Option<usize>) {
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_chosen(-1);
            if let Some(focus) = focus {
                disk.set_current(shown(focus));
            }
        });
        self.show_folder(true);
    }

    fn choose(&self, index: usize) {
        let chosen = match self.shown_at(index) {
            Some(Shown::Line(line)) => Chosen::Line(line.key),
            Some(Shown::Unexplained) => Chosen::Unexplained,
            Some(Shown::Others) => {
                self.enter(index);
                return;
            }
            None => return,
        };
        self.0.borrow_mut().chosen = Some(chosen);
        self.forget_open_call();
        let scan = self.0.borrow().scan.clone();
        if let Some(scan) = scan {
            self.show_detail(&scan);
        }
    }

    fn close_detail(&self) {
        self.0.borrow_mut().chosen = None;
        self.with_window(|window| window.global::<ui::DiskUsage>().set_chosen(-1));
    }

    /// The panel of the line chosen, if it still shows.
    fn show_detail(&self, scan: &UsageScan) {
        let (chosen, index) = {
            let state = self.0.borrow();
            let Some(chosen) = state.chosen.clone() else {
                return;
            };
            let index = state.shown.iter().position(|shown| match (&chosen, shown) {
                (Chosen::Line(key), Shown::Line(line)) => line.key == *key,
                (Chosen::Unexplained, Shown::Unexplained) => true,
                _ => false,
            });
            (chosen, index)
        };
        let Some(index) = index else {
            self.close_detail();
            return;
        };
        let detail = match chosen {
            Chosen::Unexplained => Some(unexplained_detail()),
            Chosen::Line(key) => self.line_detail(scan, index, &key),
        };
        match detail {
            Some(detail) => self.with_window(|window| {
                let disk = window.global::<ui::DiskUsage>();
                disk.set_chosen(shown(index));
                disk.set_detail(detail);
            }),
            None => self.close_detail(),
        }
    }

    /// What the panel says of the line at `index`, `key`.
    fn line_detail(&self, scan: &UsageScan, index: usize, key: &Key) -> Option<ui::DiskDetail> {
        let Some(Shown::Line(line)) = self.shown_at(index) else {
            return None;
        };
        let current = self.0.borrow().navigation.current();
        let Some(folder) = scan.path(current) else {
            tracing::warn!(?current, "folder shown unknown to the analysis");
            return None;
        };
        let path = folder.join(&line.name);
        let folders = match key {
            Key::Folder(id) => scan.view(*id).map_or(0, |view| view.children.len()),
            Key::File(_) => 0,
        };
        let cache = self
            .0
            .borrow()
            .seams
            .cache
            .as_ref()
            .is_some_and(|cache| path.starts_with(cache));
        let (state, _) = self.state_of(&line, &folder);
        let openable = cfg!(target_os = "linux")
            && matches!(
                line.state,
                LineState::Read | LineState::Partial | LineState::CountedElsewhere(_)
            );
        let analyzable = line.state == LineState::OtherMount && self.volume_at(&path).is_some();
        let language = self.0.borrow().language;
        Some(ui::DiskDetail {
            kind: line.kind.into(),
            state,
            name: line.name.to_string_lossy().as_ref().into(),
            path: path.display().to_string().into(),
            allocated: size(line.allocated),
            apparent: size(line.apparent),
            sparse: line.is_sparse(),
            files: grouped(line.files, language).into(),
            folders: shown(folders),
            cache,
            openable,
            analyzable,
        })
    }

    /// Opens the line chosen in the file manager: a folder itself, a file in
    /// its folder, a link's folder (a link is never followed).
    fn open_chosen(&self) {
        if self.0.borrow().calls.awaiting() {
            return;
        }
        let request = {
            let state = self.0.borrow();
            let (Some(scan), Some(Chosen::Line(chosen))) =
                (state.scan.clone(), state.chosen.clone())
            else {
                return;
            };
            let current = state.navigation.current();
            let Some(folder) = scan.path(current) else {
                tracing::warn!(
                    ?current,
                    "folder shown unknown to the analysis: nothing opened"
                );
                return;
            };
            let line = state.shown.iter().find_map(|shown| match shown {
                Shown::Line(line) if line.key == chosen => Some(line.clone()),
                _ => None,
            });
            let Some(line) = line else {
                return;
            };
            match (&line.key, line.kind) {
                (Key::Folder(id), _) => OpenRequest {
                    path: folder.join(&line.name),
                    folder: true,
                    expected: scan.identity(*id),
                },
                (Key::File(_), LineKind::Link) => OpenRequest {
                    path: folder,
                    folder: true,
                    expected: scan.identity(current),
                },
                (Key::File(name), _) => OpenRequest {
                    path: folder.join(name),
                    folder: false,
                    expected: None,
                },
            }
        };
        let (calls, open) = {
            let state = self.0.borrow();
            (state.calls.clone(), Rc::clone(&state.seams.open))
        };
        // Said first: the answer may come at once, and say a refusal.
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_opening(true);
            disk.set_open_refused(false);
        });
        let weak = Rc::downgrade(&self.0);
        calls.ask(&open, &request, move |opened| {
            if let Some(state) = weak.upgrade() {
                Self(state).opened(opened);
            }
        });
    }

    /// The answer to the call awaited: opened or not; none in time.
    fn opened(&self, opened: Option<bool>) {
        self.with_window(|window| {
            let disk = window.global::<ui::DiskUsage>();
            disk.set_opening(false);
            disk.set_open_refused(opened == Some(false));
        });
    }

    fn open_cleaner(&self) {
        self.with_window(|window| window.set_page(ui::Page::Cleaner));
    }

    /// The mount point a line at `index` names, if it is another disk.
    fn mount_of(&self, index: usize) -> Option<PathBuf> {
        let Some(Shown::Line(line)) = self.shown_at(index) else {
            return None;
        };
        if line.state != LineState::OtherMount {
            return None;
        }
        let state = self.0.borrow();
        let scan = state.scan.clone()?;
        let folder = scan.path(state.navigation.current())?;
        Some(folder.join(line.name))
    }
}

/// What the page says of the files of `folder`, as last read.
fn files_state(files: Option<&(NodeId, Files)>, folder: NodeId) -> ui::DiskFiles {
    match files {
        Some((of, files)) if *of == folder => match files {
            Files::Listed(_, 0) => ui::DiskFiles::Listed,
            Files::Listed(..) => ui::DiskFiles::Partial,
            Files::Changed => ui::DiskFiles::Changed,
            Files::Unreadable => ui::DiskFiles::Unreadable,
        },
        _ => ui::DiskFiles::Listed,
    }
}

/// Whether the space the analysis did not see is told: on a disk where it
/// can be, once the analysis saw all it could (running or stopped, what it
/// did not reach yet is no unexplained space).
fn tells_unexplained(told: bool, phase: Phase) -> bool {
    told && phase == Phase::Ended(Outcome::Complete)
}

/// Whether `volume` is offered to be analyzed: on this machine, its space
/// known.
fn offered(volume: &Volume) -> bool {
    matches!(volume.kind, VolumeKind::Local | VolumeKind::Removable)
        && matches!(volume.space, VolumeSpace::Known(_))
}

impl From<LineKind> for ui::DiskLineKind {
    fn from(kind: LineKind) -> Self {
        match kind {
            LineKind::Folder => Self::Folder,
            LineKind::File => Self::File,
            LineKind::Link => Self::Link,
            LineKind::Other => Self::Other,
        }
    }
}

/// The line of « Autres », folded or not.
fn others_row(arranged: &Arranged, total: u64, expanded: bool) -> ui::DiskLine {
    let size_of = arranged.others_size();
    ui::DiskLine {
        kind: ui::DiskLineKind::Others,
        state: ui::DiskLineState::Read,
        name: SharedString::new(),
        size: size(size_of),
        has_size: true,
        bar: rows::bar(size_of, arranged.largest()),
        percent: shown(rows::share(size_of, total)),
        below_one: false,
        indented: false,
        tall: true,
        talls_before: 0,
        detail: SharedString::new(),
        count: shown(arranged.others.len()),
        expanded,
        enterable: false,
    }
}

/// The line of the space the analysis could not see.
fn unexplained_row(unexplained: u64) -> ui::DiskLine {
    ui::DiskLine {
        kind: ui::DiskLineKind::Unexplained,
        state: ui::DiskLineState::Read,
        name: SharedString::new(),
        size: size(unexplained),
        has_size: true,
        bar: 0.0,
        percent: 0,
        below_one: false,
        indented: false,
        tall: true,
        talls_before: 0,
        detail: SharedString::new(),
        count: 0,
        expanded: false,
        enterable: false,
    }
}

/// What the panel says of the space unexplained: its words are the page's.
fn unexplained_detail() -> ui::DiskDetail {
    ui::DiskDetail {
        kind: ui::DiskLineKind::Unexplained,
        state: ui::DiskLineState::Read,
        name: SharedString::new(),
        path: SharedString::new(),
        allocated: size(0),
        apparent: size(0),
        sparse: false,
        files: SharedString::new(),
        folders: 0,
        cache: false,
        openable: false,
        analyzable: false,
    }
}

// Windows on real folders: Linux only, as the scan's descriptors.
#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests;

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_files_said_are_those_of_the_folder_shown() {
        let listed = |unreadable| (NodeId::ROOT, Files::Listed(Vec::new(), unreadable));
        assert_eq!(
            files_state(Some(&listed(0)), NodeId::ROOT),
            ui::DiskFiles::Listed
        );
        assert_eq!(
            files_state(Some(&listed(3)), NodeId::ROOT),
            ui::DiskFiles::Partial
        );
        assert_eq!(
            files_state(Some(&(NodeId::ROOT, Files::Changed)), NodeId::ROOT),
            ui::DiskFiles::Changed
        );
        assert_eq!(
            files_state(Some(&(NodeId::ROOT, Files::Unreadable)), NodeId::ROOT),
            ui::DiskFiles::Unreadable
        );
        assert_eq!(files_state(None, NodeId::ROOT), ui::DiskFiles::Listed);
    }

    #[test]
    fn the_unexplained_space_is_told_once_the_analysis_saw_all() {
        assert!(tells_unexplained(true, Phase::Ended(Outcome::Complete)));
        assert!(!tells_unexplained(false, Phase::Ended(Outcome::Complete)));
        for phase in [
            Phase::Opening,
            Phase::Running,
            Phase::Ended(Outcome::Cancelled),
        ] {
            assert!(!tells_unexplained(true, phase), "{phase:?}");
        }
    }
}
