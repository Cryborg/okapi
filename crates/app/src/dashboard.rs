//! The Home page: the widgets of the layout kept (or the default one) in a
//! grid, fed by the dashboard's sampler while the page is shown, and the
//! summary of the last analysis the cleaner kept.

mod catalog;
mod layout;

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use okapi_store::{DashboardLayout, LastAnalysis, WidgetSize};
use okapi_tools::monitor::{
    ApplicationName, Applications, Command, MemoryStatus, ProcessorReading, Sample, Source,
    SystemReading, Unavailable, Volume, VolumeKind, VolumeName, VolumeSpace, nearly_full,
};
use slint::{ComponentHandle as _, ModelRc, VecModel};

use crate::dates::{self, Day};
use crate::shown::{show, shown, size};
use crate::ui;
use catalog::WidgetKind;
use layout::{MAX_COLUMNS, place, shown as widgets_shown};

/// How often the readings sent are taken in: the fastest source reads
/// every two seconds.
const TAKE_SAMPLES_EVERY: Duration = Duration::from_millis(500);
/// The uses the processor's curve holds: two minutes at a reading every
/// two seconds (the "2 min" its words say, in `dashboard-widgets.slint`).
const CURVE_READINGS: usize = 60;
/// The curve's scale while every percent said fits under it; beyond, the
/// whole of it.
const CURVE_LOW_SCALE: i32 = 50;
const CURVE_FULL_SCALE: i32 = 100;
/// A use (0 to 1) in percents; a load average in hundredths.
const PERCENT_PER_UNIT: f64 = 100.0;
const HUNDREDTHS_PER_UNIT: i32 = 100;
/// The volumes the small Disques widget names, the fullest first.
const FULLEST_SHOWN: usize = 2;
const PERCENT: u128 = 100;
const SECONDS_PER_DAY: u64 = 86_400;
const SECONDS_PER_HOUR: u64 = 3600;
const SECONDS_PER_MINUTE: u64 = 60;

/// What the page reads and whom it tells what to read.
pub struct DashboardSeams {
    /// Where the layout is kept; none when no directory is known.
    pub config_directory: Option<PathBuf>,
    /// Where the summary of the last analysis is kept (the cleaner's own).
    pub state_directory: Option<PathBuf>,
    /// The zone instants are said in.
    pub zone: TimeZone,
    /// The clock instants are said from (today, yesterday).
    pub now: fn() -> Timestamp,
    /// Where the commands go: the sampler, in the application.
    pub commands: Box<dyn Fn(Command)>,
    /// What it reads; cut off, every reading is unavailable.
    pub samples: Receiver<Sample>,
}

impl std::fmt::Debug for DashboardSeams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DashboardSeams")
            .field("config_directory", &self.config_directory)
            .field("state_directory", &self.state_directory)
            .finish_non_exhaustive()
    }
}

/// The summary of the last analysis, as the page knows it.
#[derive(Debug, Clone, Copy)]
enum Summary {
    Missing,
    Kept {
        summary: LastAnalysis,
        made: Made,
    },
    /// Unreadable, or where it is kept unknown.
    Unreadable,
}

/// Where a summary kept comes from.
#[derive(Debug, Clone, Copy)]
enum Made {
    /// An analysis of this session: nothing came after it unseen.
    ThisSession,
    /// An earlier launch, and whether the history says a cleaning came
    /// since.
    Earlier { outdated: bool },
}

/// The cleanings of this session after the summary: what they freed when
/// their receipts say (none as soon as a thread died), and when, for one
/// alone.
#[derive(Debug, Clone, Copy)]
struct Cleaned {
    freed: Option<u64>,
    at: Option<i64>,
}

struct State {
    window: slint::Weak<ui::AppWindow>,
    seams: DashboardSeams,
    summary: Summary,
    cleaned: Option<Cleaned>,
    system: Option<SystemReading>,
    volumes: Option<Result<Vec<Volume>, Unavailable>>,
    memory: Option<Result<MemoryStatus, Unavailable>>,
    /// The last list read: they are read less often than the memory.
    applications: Option<Result<Applications, Unavailable>>,
    rows: Rows,
    processor: Option<Result<ProcessorReading, Unavailable>>,
    /// The last uses, oldest first, between 0 and 1.
    uses: VecDeque<f64>,
    /// The readings no longer reach the page.
    lost: bool,
    timer: slint::Timer,
}

/// The lines the widgets list, kept from one reading to the next: Slint
/// keeps a line's element while its row changes in place.
#[derive(Default)]
struct Rows {
    volumes: Rc<VecModel<ui::VolumeShown>>,
    fullest: Rc<VecModel<ui::VolumeShown>>,
    applications: Rc<VecModel<ui::ApplicationShown>>,
}

/// The Home page bound to its window.
#[derive(Clone)]
pub struct DashboardScreen(Rc<RefCell<State>>);

impl std::fmt::Debug for DashboardScreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DashboardScreen")
            .finish_non_exhaustive()
    }
}

impl DashboardScreen {
    /// Binds the page to `window`: the layout and the summary read, the
    /// sampler told what to follow, and paused unless the page is shown.
    pub fn attach(window: &ui::AppWindow, seams: DashboardSeams) -> Self {
        let dashboard = window.global::<ui::Dashboard>();
        let (widgets, unreadable) = read_layout(seams.config_directory.as_deref());
        dashboard.set_layout_unreadable(unreadable);
        let sizes: Vec<_> = widgets.iter().map(|(_, size)| *size).collect();
        let placements: Vec<ui::Placement> = (1..=MAX_COLUMNS)
            .map(|columns| placement(&widgets, &place(&sizes, columns)))
            .collect();
        dashboard.set_placements(ModelRc::from(Rc::new(VecModel::from(placements))));
        dashboard.on_columns_shown(|columns| tracing::debug!(columns, "dashboard columns"));
        dashboard.set_curve_span(shown(CURVE_READINGS - 1));

        let mut sources: BTreeSet<Source> = widgets
            .iter()
            .flat_map(|(kind, size)| kind.sources(*size))
            .collect();
        // The header says the distribution, whatever the widgets.
        sources.insert(Source::System);
        tracing::info!(widgets = ?widgets, ?sources, "dashboard attached");
        (seams.commands)(Command::Watch(sources));

        let summary = read_summary(seams.state_directory.as_deref());
        let screen = Self(Rc::new(RefCell::new(State {
            window: window.as_weak(),
            seams,
            summary,
            cleaned: None,
            system: None,
            volumes: None,
            memory: None,
            applications: None,
            rows: Rows::default(),
            processor: None,
            uses: VecDeque::with_capacity(CURVE_READINGS),
            lost: false,
            timer: slint::Timer::default(),
        })));
        let weak = Rc::downgrade(&screen.0);
        screen
            .0
            .borrow()
            .timer
            .start(slint::TimerMode::Repeated, TAKE_SAMPLES_EVERY, move || {
                if let Some(state) = weak.upgrade() {
                    Self(state).take_samples();
                }
            });
        screen.page_shown(window.get_page());
        screen
    }

    /// Follows the page shown: read while the Home page is, nothing else.
    pub fn page_shown(&self, page: ui::Page) {
        if page == ui::Page::Home {
            tracing::info!("dashboard shown: readings resumed");
            // The readings before the pause would be joined to those after
            // as if two seconds apart: the curve starts again.
            self.0.borrow_mut().uses.clear();
            (self.0.borrow().seams.commands)(Command::Resume);
            self.judge_outdated();
            self.show();
        } else {
            tracing::info!(?page, "dashboard hidden: readings paused");
            (self.0.borrow().seams.commands)(Command::Pause);
        }
    }

    /// An analysis of this session made `summary` (kept, or not when it
    /// could not be written: the page still says it).
    pub fn analysis_done(&self, summary: LastAnalysis) {
        {
            let mut state = self.0.borrow_mut();
            state.summary = Summary::Kept {
                summary,
                made: Made::ThisSession,
            };
            state.cleaned = None;
        }
        self.show();
    }

    /// A cleaning ended: the summary no longer says what is left, and the
    /// volumes changed. What the cleanings since the summary freed adds up,
    /// unknown as soon as one's is.
    pub fn cleaning_done(&self, freed: Option<u64>, at: i64) {
        {
            let mut state = self.0.borrow_mut();
            let freed = freed_since(state.cleaned, freed);
            // Two cleanings or more have no one time.
            let at = state.cleaned.is_none().then_some(at);
            state.cleaned = Some(Cleaned { freed, at });
            (state.seams.commands)(Command::RefreshVolumes);
        }
        self.show();
    }

    /// Whether a cleaning came after a summary of an earlier launch, as the
    /// history says; unknown, the summary is said unreadable.
    fn judge_outdated(&self) {
        let mut state = self.0.borrow_mut();
        let (
            Summary::Kept {
                summary,
                made: Made::Earlier { .. },
            },
            Some(directory),
        ) = (state.summary, state.seams.state_directory.as_deref())
        else {
            return;
        };
        state.summary = match summary.outdated(directory) {
            Ok(outdated) => Summary::Kept {
                summary,
                made: Made::Earlier { outdated },
            },
            Err(error) => {
                tracing::warn!(%error, "whether the summary is outdated unknown: said unreadable");
                Summary::Unreadable
            }
        };
    }

    fn take_samples(&self) {
        let mut changed = false;
        {
            let mut state = self.0.borrow_mut();
            loop {
                match state.seams.samples.try_recv() {
                    Ok(Sample::System(reading)) => {
                        state.system = Some(reading);
                        changed = true;
                    }
                    Ok(Sample::Volumes(volumes)) => {
                        state.volumes = Some(volumes);
                        changed = true;
                    }
                    Ok(Sample::Memory(reading)) => {
                        match reading {
                            Ok(reading) => {
                                state.memory = Some(Ok(reading.status));
                                if let Some(applications) = reading.applications {
                                    state.applications = Some(applications);
                                }
                            }
                            Err(unavailable) => state.memory = Some(Err(unavailable)),
                        }
                        changed = true;
                    }
                    Ok(Sample::Processor(reading)) => {
                        match reading {
                            Ok(ProcessorReading {
                                usage: Some(usage), ..
                            }) => {
                                if state.uses.len() == CURVE_READINGS {
                                    state.uses.pop_front();
                                }
                                state.uses.push_back(usage);
                            }
                            Ok(_) => {}
                            // The next point would no longer be two seconds
                            // after the one before: the curve starts again.
                            Err(_) => state.uses.clear(),
                        }
                        state.processor = Some(reading);
                        changed = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        tracing::error!("dashboard readings cut off: every widget unavailable");
                        state.lost = true;
                        state.timer.stop();
                        changed = true;
                        break;
                    }
                }
            }
        }
        if changed {
            self.show();
        }
    }

    fn show(&self) {
        let state = self.0.borrow();
        let Some(window) = state.window.upgrade() else {
            return;
        };
        let now = (state.seams.now)();
        let dashboard = window.global::<ui::Dashboard>();
        dashboard.set_reclaimable(reclaimable(
            state.summary,
            state.cleaned,
            now,
            &state.seams.zone,
        ));
        dashboard.set_system(system(state.system.as_ref()));
        dashboard.set_disks(disks(state.volumes.as_ref(), &state.rows));
        dashboard.set_memory(memory(
            state.memory.as_ref(),
            state.applications.as_ref(),
            &state.rows,
        ));
        dashboard.set_processor(processor(state.processor.as_ref(), &state.uses));
        dashboard.set_readings_lost(state.lost);
    }
}

/// The zone of this machine; unknown, UTC, said.
pub fn system_zone() -> TimeZone {
    TimeZone::try_system().unwrap_or_else(|error| {
        tracing::warn!(%error, "time zone unknown: instants said in UTC");
        TimeZone::UTC
    })
}

/// The widgets of the layout kept in `directory`, and whether it could not
/// be read (the default one then shown).
fn read_layout(directory: Option<&Path>) -> (Vec<(WidgetKind, WidgetSize)>, bool) {
    let Some(directory) = directory else {
        tracing::warn!("no configuration directory: the default dashboard is shown");
        return (widgets_shown(None), true);
    };
    match DashboardLayout::load(directory) {
        Ok(loaded) => (widgets_shown(loaded.as_ref()), false),
        Err(error) => {
            tracing::warn!(%error, "dashboard layout unreadable: the default one is shown");
            (widgets_shown(None), true)
        }
    }
}

/// The summary kept in `directory`.
fn read_summary(directory: Option<&Path>) -> Summary {
    let Some(directory) = directory else {
        tracing::warn!("no state directory: no summary of the last analysis");
        return Summary::Unreadable;
    };
    match LastAnalysis::load(directory) {
        Ok(None) => Summary::Missing,
        Ok(Some(summary)) if Timestamp::from_second(summary.time).is_err() => {
            tracing::warn!(
                ?summary,
                "summary of the last analysis dated out of any date"
            );
            Summary::Unreadable
        }
        Ok(Some(summary)) => {
            tracing::info!(?summary, "summary of the last analysis read");
            // Whether it is outdated is judged as the page is shown.
            Summary::Kept {
                summary,
                made: Made::Earlier { outdated: false },
            }
        }
        Err(error) => {
            tracing::warn!(%error, "summary of the last analysis unreadable");
            Summary::Unreadable
        }
    }
}

fn placement(
    widgets: &[(WidgetKind, WidgetSize)],
    (cells, rows): &(Vec<layout::Cell>, usize),
) -> ui::Placement {
    let cells: Vec<ui::WidgetCell> = widgets
        .iter()
        .zip(cells)
        .map(|((kind, size), cell)| ui::WidgetCell {
            kind: widget_kind(*kind),
            size: widget_size(*size),
            column: shown(cell.column),
            row: shown(cell.row),
            width: shown(cell.width),
            height: shown(cell.height),
        })
        .collect();
    ui::Placement {
        cells: ModelRc::from(Rc::new(VecModel::from(cells))),
        rows: shown(*rows),
    }
}

const fn widget_kind(kind: WidgetKind) -> ui::WidgetKind {
    match kind {
        WidgetKind::Reclaimable => ui::WidgetKind::Reclaimable,
        WidgetKind::Disks => ui::WidgetKind::Disks,
        WidgetKind::Memory => ui::WidgetKind::Memory,
        WidgetKind::Processor => ui::WidgetKind::Processor,
        WidgetKind::System => ui::WidgetKind::System,
    }
}

const fn widget_size(size: WidgetSize) -> ui::WidgetSize {
    match size {
        WidgetSize::Small => ui::WidgetSize::Small,
        WidgetSize::Wide => ui::WidgetSize::Wide,
        WidgetSize::Tall => ui::WidgetSize::Tall,
        WidgetSize::Large => ui::WidgetSize::Large,
        WidgetSize::Full => ui::WidgetSize::Full,
    }
}

/// The instant `seconds` as the window says it at `now`; none, traced, for
/// seconds no date holds.
fn moment(seconds: i64, now: Timestamp, zone: &TimeZone) -> Option<ui::Moment> {
    let Some(moment) = dates::moment(seconds, now, zone) else {
        tracing::warn!(seconds, "an instant no date holds");
        return None;
    };
    let (day, day_of_month, month) = match moment.day {
        Day::Today => (ui::DayKind::Today, 0, 0),
        Day::Yesterday => (ui::DayKind::Yesterday, 0, 0),
        Day::On { day, month } => (ui::DayKind::Other, day, month),
    };
    Some(ui::Moment {
        day,
        day_of_month: day_of_month.into(),
        month: month.into(),
        hour: moment.hour.into(),
        minute: moment.minute.into(),
    })
}

/// What Récupérable says of `summary` at `now` in `zone`: a cleaning of
/// this session first, then the summary, outdated when the history says a
/// cleaning came since.
fn reclaimable(
    summary: Summary,
    cleaned: Option<Cleaned>,
    now: Timestamp,
    zone: &TimeZone,
) -> ui::Reclaimable {
    let mut shown_state = ui::Reclaimable::default();
    if let Some(cleaned) = cleaned {
        shown_state.state = ui::ReclaimableState::Cleaned;
        // Nothing it knows of freed: no amount said.
        let freed = cleaned.freed.filter(|bytes| *bytes > 0);
        shown_state.freed_known = freed.is_some();
        shown_state.freed = size(freed.unwrap_or_default());
        if let Some(at) = cleaned.at.and_then(|at| moment(at, now, zone)) {
            shown_state.cleaned_at_known = true;
            shown_state.cleaned_at = at;
        }
        return shown_state;
    }
    let Summary::Kept { summary, made } = summary else {
        shown_state.state = if matches!(summary, Summary::Missing) {
            ui::ReclaimableState::None
        } else {
            ui::ReclaimableState::Unavailable
        };
        return shown_state;
    };
    let Some(at) = moment(summary.time, now, zone) else {
        shown_state.state = ui::ReclaimableState::Unavailable;
        return shown_state;
    };
    shown_state.size = size(summary.reclaimable_bytes);
    shown_state.entries = shown(summary.entries);
    shown_state.at = at;
    shown_state.state = match made {
        Made::ThisSession => ui::ReclaimableState::Current,
        Made::Earlier { outdated: true } => ui::ReclaimableState::Cleaned,
        Made::Earlier { outdated: false } => ui::ReclaimableState::Estimate,
    };
    shown_state
}

/// What the cleanings since the summary freed, `freed` by the last one
/// after those `before`: unknown as soon as one's is.
fn freed_since(before: Option<Cleaned>, freed: Option<u64>) -> Option<u64> {
    match before {
        None => freed,
        Some(before) => before
            .freed
            .zip(freed)
            .map(|(before, last)| before.saturating_add(last)),
    }
}

/// How a desktop is written: `X-Cinnamon` is Cinnamon.
fn desktop_name(desktop: &str) -> String {
    desktop.strip_prefix("X-").unwrap_or(desktop).to_owned()
}

/// `part` of `whole`, in whole percents rounded down (0 for nothing): a
/// volume at 89,6 % says 89 %, never 90 % without its "nearly full" badge.
fn percent(part: u64, whole: u64) -> i32 {
    if whole == 0 {
        return 0;
    }
    shown(u128::from(part) * PERCENT / u128::from(whole))
}

/// What a reading says before its value: not read yet, or unreadable.
fn reading<T>(read: Option<&Result<T, Unavailable>>) -> Result<&T, ui::ReadingState> {
    match read {
        None => Err(ui::ReadingState::Reading),
        Some(Err(_)) => Err(ui::ReadingState::Unavailable),
        Some(Ok(value)) => Ok(value),
    }
}

fn volume(volume: &Volume) -> ui::VolumeShown {
    let (naming, name) = match &volume.name {
        VolumeName::System => (ui::VolumeNaming::System, String::new()),
        VolumeName::Home => (ui::VolumeNaming::Home, String::new()),
        VolumeName::Pool(name)
        | VolumeName::Source(name)
        | VolumeName::Label(name)
        | VolumeName::Folder(name) => (ui::VolumeNaming::Own, name.clone()),
        VolumeName::MountPoint(path) => (ui::VolumeNaming::Own, path.display().to_string()),
    };
    let (space, known) = match volume.space {
        VolumeSpace::Known(space) => (ui::SpaceState::Known, Some(space)),
        VolumeSpace::NotResponding => (ui::SpaceState::NotResponding, None),
        VolumeSpace::Failed(_) => (ui::SpaceState::Failed, None),
    };
    let space_of = known.unwrap_or_default();
    ui::VolumeShown {
        naming,
        name: name.into(),
        mount_point: volume.mount_point.display().to_string().into(),
        filesystem: volume.filesystem.clone().into(),
        place: match volume.kind {
            VolumeKind::Local => ui::VolumePlace::Local,
            VolumeKind::Removable => ui::VolumePlace::Removable,
            VolumeKind::Network => ui::VolumePlace::Network,
        },
        space,
        free: size(space_of.available),
        total: size(space_of.total),
        used: size(space_of.used),
        percent: percent(space_of.used, space_of.total),
        nearly_full: known.as_ref().is_some_and(nearly_full),
    }
}

/// The volumes read, in `rows`: every one, and the fullest whose space is
/// known (by the part used, an empty one at 0 %).
fn disks(volumes: Option<&Result<Vec<Volume>, Unavailable>>, rows: &Rows) -> ui::DisksShown {
    let volumes = match reading(volumes) {
        Ok(volumes) => volumes,
        Err(state) => {
            return ui::DisksShown {
                state,
                ..ui::DisksShown::default()
            };
        }
    };
    let mut fullest: Vec<(&Volume, u128, u128)> = volumes
        .iter()
        .filter_map(|volume| match volume.space {
            VolumeSpace::Known(space) => Some((
                volume,
                u128::from(space.used),
                u128::from(space.total.max(1)),
            )),
            _ => None,
        })
        .collect();
    // By the part used, compared without dividing: used × other total.
    fullest.sort_by(|(_, left_used, left_total), (_, right_used, right_total)| {
        (right_used * left_total).cmp(&(left_used * right_total))
    });
    fullest.truncate(FULLEST_SHOWN);
    // Every volume whose space is unknown is among the others.
    let others = volumes.len() - fullest.len();
    let not_responding = volumes
        .iter()
        .filter(|volume| volume.space == VolumeSpace::NotResponding)
        .count();
    show(&rows.volumes, volumes.iter().map(volume).collect());
    show(
        &rows.fullest,
        fullest
            .into_iter()
            .map(|(shown, ..)| volume(shown))
            .collect(),
    );
    ui::DisksShown {
        state: ui::ReadingState::Read,
        volumes: ModelRc::from(Rc::clone(&rows.volumes)),
        fullest: ModelRc::from(Rc::clone(&rows.fullest)),
        others: shown(others),
        others_not_responding: shown(not_responding),
    }
}

/// The memory read, its applications in `rows` (the last list read).
fn memory(
    status: Option<&Result<MemoryStatus, Unavailable>>,
    applications: Option<&Result<Applications, Unavailable>>,
    rows: &Rows,
) -> ui::MemoryShown {
    let status = match reading(status) {
        Ok(status) => status,
        Err(state) => {
            return ui::MemoryShown {
                state,
                ..ui::MemoryShown::default()
            };
        }
    };
    let used = status.total.saturating_sub(status.available);
    let swap_used = status.swap_total.saturating_sub(status.swap_free);
    let applications_state = match reading(applications) {
        Ok(applications) => {
            show(
                &rows.applications,
                applications
                    .top
                    .iter()
                    .map(|application| ui::ApplicationShown {
                        name: match &application.name {
                            ApplicationName::Desktop(name) | ApplicationName::Process(name) => {
                                name.clone().into()
                            }
                        },
                        size: size(application.resident_bytes),
                    })
                    .collect(),
            );
            ui::ReadingState::Read
        }
        Err(state) => state,
    };
    ui::MemoryShown {
        state: ui::ReadingState::Read,
        used: size(used),
        total: size(status.total),
        available: size(status.available),
        percent: percent(used, status.total),
        has_swap: status.swap_total > 0,
        swap_used: size(swap_used),
        swap_total: size(status.swap_total),
        swap_percent: percent(swap_used, status.swap_total),
        applications_state,
        applications: ModelRc::from(Rc::clone(&rows.applications)),
    }
}

/// A use between 0 and 1 in whole percents.
fn percent_of(usage: f64) -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a use between 0 and 1 makes 0 to 100"
    )]
    let percent = (usage.clamp(0.0, 1.0) * PERCENT_PER_UNIT).round() as i32;
    percent
}

/// The commands of the line through `uses` (oldest first) and of the area
/// under it, at the right of a box `CURVE_READINGS - 1` wide and `scale`
/// high (y going down); empty below two readings.
fn curve(uses: &VecDeque<f64>, scale: i32) -> (String, String) {
    if uses.len() < 2 {
        return (String::new(), String::new());
    }
    let first = CURVE_READINGS.saturating_sub(uses.len());
    let line = uses
        .iter()
        .enumerate()
        .map(|(index, usage)| {
            let command = if index == 0 { 'M' } else { 'L' };
            let height = f64::from(scale) - usage.clamp(0.0, 1.0) * PERCENT_PER_UNIT;
            format!("{command}{} {height:.1}", first + index)
        })
        .collect::<Vec<_>>()
        .join(" ");
    let last = CURVE_READINGS - 1;
    let area = format!("{line} L{last} {scale} L{first} {scale} Z");
    (line, area)
}

/// The load average in whole and hundredths.
fn load_parts(load: f64) -> (i32, i32) {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a load average in hundredths stays far below i32::MAX"
    )]
    let hundredths = (load.max(0.0) * f64::from(HUNDREDTHS_PER_UNIT)).round() as i32;
    (
        hundredths / HUNDREDTHS_PER_UNIT,
        hundredths % HUNDREDTHS_PER_UNIT,
    )
}

fn processor(
    read: Option<&Result<ProcessorReading, Unavailable>>,
    uses: &VecDeque<f64>,
) -> ui::ProcessorShown {
    let reading = match reading(read) {
        Ok(reading) => reading,
        Err(state) => {
            return ui::ProcessorShown {
                state,
                ..ui::ProcessorShown::default()
            };
        }
    };
    // The first reading measures nothing: no use until the second.
    let Some(last) = uses.back() else {
        return ui::ProcessorShown {
            state: ui::ReadingState::Reading,
            ..ui::ProcessorShown::default()
        };
    };
    let highest = percent_of(uses.iter().copied().fold(0.0, f64::max));
    let lowest = percent_of(uses.iter().copied().fold(1.0, f64::min));
    // On the percents said, so that the scale never contradicts them.
    let scale = if highest <= CURVE_LOW_SCALE {
        CURVE_LOW_SCALE
    } else {
        CURVE_FULL_SCALE
    };
    let (line, area) = curve(uses, scale);
    let (load_whole, load_hundredths) = reading.load_average.map_or((0, 0), load_parts);
    ui::ProcessorShown {
        state: ui::ReadingState::Read,
        percent: percent_of(*last),
        load_known: reading.load_average.is_some(),
        load_whole,
        load_hundredths,
        scale,
        line: line.into(),
        area: area.into(),
        filling: uses.len() < CURVE_READINGS,
        lowest,
        highest,
    }
}

/// How a session type is written: `wayland` is Wayland.
fn session_name(session: &str) -> String {
    match session {
        "wayland" => "Wayland".to_owned(),
        "x11" => "X11".to_owned(),
        other => other.to_owned(),
    }
}

fn system(reading: Option<&SystemReading>) -> ui::SystemShown {
    let Some(reading) = reading else {
        return ui::SystemShown {
            state: ui::ReadingState::Reading,
            ..ui::SystemShown::default()
        };
    };
    let nothing = reading.distribution.is_none()
        && reading.kernel.is_none()
        && reading.desktops.is_empty()
        && reading.session.is_none()
        && reading.uptime.is_none();
    let uptime = reading.uptime.map(|uptime| uptime.as_secs());
    ui::SystemShown {
        state: if nothing {
            ui::ReadingState::Unavailable
        } else {
            ui::ReadingState::Read
        },
        distribution: reading.distribution.clone().unwrap_or_default().into(),
        kernel: reading.kernel.clone().unwrap_or_default().into(),
        // The desktop itself is the least specific name (`ubuntu:GNOME`).
        desktop: reading
            .desktops
            .last()
            .map(|desktop| desktop_name(desktop))
            .unwrap_or_default()
            .into(),
        session: reading
            .session
            .as_deref()
            .map(session_name)
            .unwrap_or_default()
            .into(),
        uptime_known: uptime.is_some(),
        uptime_days: uptime.map_or(0, |seconds| shown(seconds / SECONDS_PER_DAY)),
        uptime_hours: uptime.map_or(0, |seconds| {
            shown(seconds % SECONDS_PER_DAY / SECONDS_PER_HOUR)
        }),
        uptime_minutes: uptime.map_or(0, |seconds| {
            shown(seconds % SECONDS_PER_HOUR / SECONDS_PER_MINUTE)
        }),
    }
}

#[cfg(test)]
pub mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;
    use std::sync::mpsc::{self, Sender};
    use std::time::{Duration, SystemTime};

    use i_slint_backend_testing::{AccessibleRole, ElementHandle};
    use okapi_rules::Language;
    use okapi_store::{DashboardWidget, History};
    use okapi_tools::monitor::Space;
    use slint::Model as _;

    use super::*;
    use crate::cleaner::Seams;
    use crate::cleaner::tests::{
        Sandbox, button, checkbox, let_the_screen_settle, scripted, seams_never_scanning,
        settle_files, shows, shows_a_text_starting, step, watch_until,
    };

    /// Seams that read no file and whose sampler never sends: the page
    /// keeps reading (the sending end lives as long as the seams).
    pub fn inert() -> DashboardSeams {
        let (sink, samples) = mpsc::channel::<Sample>();
        DashboardSeams {
            config_directory: None,
            state_directory: None,
            zone: TimeZone::UTC,
            now: Timestamp::now,
            commands: Box::new(move |_| {
                let _kept = &sink;
            }),
            samples,
        }
    }

    /// What the test sends as the sampler, and the commands the page gave.
    struct Sampler {
        sink: Option<Sender<Sample>>,
        commands: Rc<RefCell<Vec<Command>>>,
    }

    /// Seams reading `state`, on the machine's clock in UTC, their
    /// commands noted.
    fn noting(state: Option<PathBuf>) -> (DashboardSeams, Sampler) {
        let (sink, samples) = mpsc::channel();
        let commands = Rc::new(RefCell::new(Vec::new()));
        let noted = Rc::clone(&commands);
        let seams = DashboardSeams {
            state_directory: state,
            commands: Box::new(move |command| noted.borrow_mut().push(command)),
            samples,
            ..inert()
        };
        (
            seams,
            Sampler {
                sink: Some(sink),
                commands,
            },
        )
    }

    /// 2026-03-10 at 14:30 in Paris (13:30 UTC, winter).
    fn fixed_now() -> Timestamp {
        "2026-03-10T13:30:00Z".parse().unwrap()
    }

    fn at(utc: &str) -> i64 {
        utc.parse::<Timestamp>().unwrap().as_second()
    }

    /// A configuration and a state directory of the test's own, read on a
    /// fixed clock in Paris.
    struct Directories {
        _directory: tempfile::TempDir,
        config: PathBuf,
        state: PathBuf,
    }

    impl Directories {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let config = directory.path().join("config");
            let state = directory.path().join("state");
            fs::create_dir_all(&config).unwrap();
            fs::create_dir_all(&state).unwrap();
            Self {
                _directory: directory,
                config,
                state,
            }
        }

        fn seams(&self) -> (DashboardSeams, Sampler) {
            let (seams, sampler) = noting(Some(self.state.clone()));
            (
                DashboardSeams {
                    config_directory: Some(self.config.clone()),
                    zone: TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap(),
                    now: fixed_now,
                    ..seams
                },
                sampler,
            )
        }

        /// A history last written at `utc`.
        fn history_at(&self, utc: &str) {
            let path = History::path(&self.state);
            fs::write(&path, "").unwrap();
            let written =
                SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(at(utc)).unwrap());
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(written)
                .unwrap();
        }
    }

    fn on_home(seams: DashboardSeams) -> ui::AppWindow {
        on_home_with(seams_never_scanning(), seams)
    }

    /// The window on the Home page, and the page itself.
    fn home_screen(seams: DashboardSeams) -> (ui::AppWindow, DashboardScreen) {
        i_slint_backend_testing::init_no_event_loop();
        let (window, _cleaner, home) =
            crate::window(seams_never_scanning(), seams, Language::French).unwrap();
        slint::platform::update_timers_and_animations();
        (window, home)
    }

    /// The window on the Home page at the mockup's size (three columns,
    /// every widget in view), its cleaner working through `cleaner`.
    fn on_home_with(cleaner: Seams, seams: DashboardSeams) -> ui::AppWindow {
        i_slint_backend_testing::init_no_event_loop();
        let (window, _cleaner, _home) = crate::window(cleaner, seams, Language::French).unwrap();
        window
            .window()
            .set_size(slint::PhysicalSize::new(1280, 800));
        slint::platform::update_timers_and_animations();
        window
    }

    /// Lets the page take in what was sent.
    fn take_in() {
        i_slint_backend_testing::mock_elapsed_time(TAKE_SAMPLES_EVERY);
        slint::platform::update_timers_and_animations();
    }

    fn go_to(window: &ui::AppWindow, page: &str) {
        button(window, page).invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
    }

    fn widget(window: &ui::AppWindow, title: &str) -> ElementHandle {
        ElementHandle::find_by_accessible_label(window, title)
            .find(|element| element.accessible_role() == Some(AccessibleRole::Groupbox))
            .unwrap_or_else(|| panic!("no widget {title}"))
    }

    fn said(window: &ui::AppWindow, title: &str) -> Option<String> {
        widget(window, title)
            .accessible_description()
            .map(|description| description.to_string())
    }

    fn kept(directory: &Path, widgets: &[(&str, WidgetSize)]) {
        DashboardLayout {
            widgets: widgets
                .iter()
                .map(|(kind, size)| DashboardWidget {
                    kind: (*kind).to_owned(),
                    size: *size,
                })
                .collect(),
        }
        .save(directory)
        .unwrap();
    }

    /// A summary of 2,4 Go in 18312 entries, made at `utc`.
    fn summary_at(utc: &str) -> LastAnalysis {
        LastAnalysis {
            time: at(utc),
            reclaimable_bytes: 2_400_000_000,
            entries: 18_312,
        }
    }

    /// The time of `seconds` in UTC as the window says it, worked out here.
    fn utc_clock(seconds: i64) -> String {
        let at = Timestamp::from_second(seconds)
            .unwrap()
            .to_zoned(TimeZone::UTC);
        if at.minute() == 0 {
            format!("{} h", at.hour())
        } else {
            format!("{} h {:02}", at.hour(), at.minute())
        }
    }

    #[test]
    fn without_a_layout_kept_every_widget_is_shown_and_its_sources_followed() {
        let directories = Directories::new();
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        for title in ["Récupérable", "Disques", "Mémoire", "Processeur", "Système"] {
            widget(&window, title);
        }
        assert!(!shows(
            &window,
            "Votre disposition n'a pas pu être lue, la disposition par défaut est affichée."
        ));
        assert_eq!(
            sampler.commands.borrow()[0],
            Command::Watch(BTreeSet::from([
                Source::Volumes,
                Source::Memory,
                Source::Processor,
                Source::System,
            ])),
            "the memory widget at its default size lists no application"
        );
    }

    /// The top left corner and the size of a widget, in pixels.
    fn frame(window: &ui::AppWindow, title: &str) -> (f32, f32, f32, f32) {
        let widget = widget(window, title);
        let (position, size) = (widget.absolute_position(), widget.size());
        (position.x, position.y, size.width, size.height)
    }

    #[test]
    fn the_grid_follows_the_window_s_width_as_the_mockup() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        // 1280 px: a margin of 32 px, three columns.
        let (reclaimable_x, _, reclaimable_width, _) = frame(&window, "Récupérable");
        let (disks_x, disks_y, disks_width, height) = frame(&window, "Disques");
        let (memory_x, memory_y, memory_width, _) = frame(&window, "Mémoire");
        let gap = 16.0;
        assert!(
            (reclaimable_x - (248.0 + 32.0)).abs() < 1.0,
            "{reclaimable_x}"
        );
        assert!((reclaimable_width - (1280.0 - 248.0 - 2.0 * 32.0)).abs() < 1.0);
        assert!((disks_width - (2.0 * memory_width + gap)).abs() < 1.0);
        assert!((memory_x - (disks_x + disks_width + gap)).abs() < 1.0);
        assert!((memory_y - disks_y).abs() < 1.0, "side by side");
        assert!((height - 176.0).abs() < 1.0, "a row");
        // 960 px: a margin of 24 px, two columns, the memory under the
        // disks.
        window.window().set_size(slint::PhysicalSize::new(960, 640));
        slint::platform::update_timers_and_animations();
        let (reclaimable_x, ..) = frame(&window, "Récupérable");
        let (disks_x, disks_y, disks_width, _) = frame(&window, "Disques");
        let (memory_x, memory_y, ..) = frame(&window, "Mémoire");
        assert!(
            (reclaimable_x - (248.0 + 24.0)).abs() < 1.0,
            "{reclaimable_x}"
        );
        assert!((disks_width - (960.0 - 248.0 - 2.0 * 24.0)).abs() < 1.0);
        assert!((memory_x - disks_x).abs() < 1.0);
        assert!((memory_y - (disks_y + 176.0 + gap)).abs() < 1.0);
    }

    #[test]
    fn a_layout_kept_is_shown_and_the_system_always_followed() {
        let directories = Directories::new();
        kept(&directories.config, &[("memory", WidgetSize::Large)]);
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        widget(&window, "Mémoire");
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Disques")
                .all(|element| element.accessible_role() != Some(AccessibleRole::Groupbox))
        );
        assert_eq!(
            sampler.commands.borrow()[0],
            Command::Watch(BTreeSet::from([
                Source::Memory,
                Source::Applications,
                Source::System,
            ]))
        );
    }

    #[test]
    fn an_unreadable_layout_says_so_and_shows_the_default_one() {
        let directories = Directories::new();
        fs::write(DashboardLayout::path(&directories.config), "{").unwrap();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert!(shows(
            &window,
            "Votre disposition n'a pas pu être lue, la disposition par défaut est affichée."
        ));
        widget(&window, "Disques");
    }

    #[test]
    fn an_emptied_layout_shows_an_empty_dashboard() {
        let directories = Directories::new();
        kept(&directories.config, &[]);
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert!(shows(&window, "Votre tableau de bord est vide"));
    }

    #[test]
    fn the_readings_pause_off_the_home_page_and_resume_on_it() {
        let directories = Directories::new();
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        sampler.commands.borrow_mut().clear();
        go_to(&window, "À propos");
        go_to(&window, "Accueil");
        assert_eq!(
            *sampler.commands.borrow(),
            [Command::Pause, Command::Resume]
        );
    }

    fn send(sampler: &Sampler, reading: SystemReading) {
        sampler
            .sink
            .as_ref()
            .unwrap()
            .send(Sample::System(reading))
            .unwrap();
        take_in();
    }

    #[test]
    fn the_system_read_is_shown_in_its_widget_and_the_header() {
        let directories = Directories::new();
        summary_at("2026-03-10T11:18:00Z")
            .save(&directories.state)
            .unwrap();
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        assert_eq!(
            said(&window, "Système").as_deref(),
            Some("Lecture en cours")
        );
        send(
            &sampler,
            SystemReading {
                distribution: Some("Ubuntu 26.04 LTS".to_owned()),
                kernel: Some("7.0.0-42-generic".to_owned()),
                desktops: vec!["ubuntu".to_owned(), "GNOME".to_owned()],
                session: Some("wayland".to_owned()),
                uptime: Some(Duration::from_mins(3 * 60 + 5)),
            },
        );
        for text in [
            "Distribution : Ubuntu 26.04 LTS",
            "Noyau : 7.0.0-42-generic",
            "Bureau : GNOME sous Wayland",
            "Allumé depuis : 3 h 05",
            "Ubuntu 26.04 LTS · dernière analyse aujourd'hui à 12 h 18",
        ] {
            assert!(shows(&window, text), "{text}");
        }
    }

    #[test]
    fn a_partial_system_reading_shows_what_was_read() {
        let directories = Directories::new();
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        send(
            &sampler,
            SystemReading {
                kernel: Some("7.0.0".to_owned()),
                desktops: vec!["X-Cinnamon".to_owned()],
                session: Some("x11".to_owned()),
                uptime: Some(Duration::from_hours(50)),
                ..SystemReading::default()
            },
        );
        assert!(shows(&window, "Noyau : 7.0.0"));
        assert!(shows(&window, "Bureau : Cinnamon sous X11"));
        assert!(shows(&window, "Allumé depuis : 2 j 2 h"));
        assert!(!shows_a_text_starting(&window, "Distribution"));
        send(&sampler, SystemReading::default());
        assert_eq!(said(&window, "Système").as_deref(), Some("Indisponible"));
    }

    #[test]
    fn readings_cut_off_make_the_widgets_unavailable_but_not_the_summary() {
        let directories = Directories::new();
        let (seams, mut sampler) = directories.seams();
        let window = on_home(seams);
        sampler.sink = None;
        take_in();
        assert_eq!(said(&window, "Système").as_deref(), Some("Indisponible"));
        assert_eq!(said(&window, "Disques").as_deref(), Some("Indisponible"));
        assert_eq!(said(&window, "Mémoire").as_deref(), Some("Indisponible"));
        assert_eq!(said(&window, "Processeur").as_deref(), Some("Indisponible"));
        assert!(shows(
            &window,
            "Les informations du système n'ont pas pu être lues."
        ));
        assert_eq!(
            said(&window, "Récupérable").as_deref(),
            Some("Aucune analyse")
        );
    }

    #[test]
    fn without_a_summary_reclaimable_offers_an_analysis() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert!(shows(&window, "Aucune analyse"));
        assert_eq!(button(&window, "Analyser").accessible_enabled(), Some(true));
        assert!(
            !shows(&window, "Voir les derniers résultats"),
            "no results held"
        );
    }

    #[test]
    fn the_cleanings_of_a_session_add_up_until_one_is_unknown() {
        let directories = Directories::new();
        let (seams, sampler) = directories.seams();
        let (window, home) = home_screen(seams);
        home.cleaning_done(Some(5), at("2026-03-10T09:00:00Z"));
        assert!(shows(
            &window,
            "Nettoyage d'aujourd'hui à 10 h. Analysez à nouveau pour une estimation à jour."
        ));
        home.cleaning_done(Some(7), at("2026-03-10T10:05:00Z"));
        assert!(shows(&window, "12 o libérés"));
        assert!(
            shows(
                &window,
                "L'estimation date d'avant un nettoyage. Analysez à nouveau pour une estimation à jour."
            ),
            "no one time for two cleanings"
        );
        assert_eq!(
            sampler
                .commands
                .borrow()
                .iter()
                .filter(|command| **command == Command::RefreshVolumes)
                .count(),
            2
        );
        home.cleaning_done(None, at("2026-03-10T10:30:00Z"));
        assert!(shows(&window, "Estimation à refaire"));
    }

    #[test]
    fn an_analysis_without_an_estimate_says_nothing_of_one() {
        let directories = Directories::new();
        let (cleaner, scan) = scripted(Vec::new());
        let (seams, _sampler) = directories.seams();
        let window = on_home_with(cleaner, seams);
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        go_to(&window, "Accueil");
        assert!(shows(&window, "Analyse en cours"));
        assert!(!shows_a_text_starting(&window, "Le résultat remplacera"));
        scan.go.send(()).unwrap();
    }

    /// A summary of an earlier launch made at `made` (UTC) is an estimate,
    /// said `estimate` and, in the header, `said`, in Paris.
    fn estimate_dated(made: &str, estimate: &str, said: &str) {
        let directories = Directories::new();
        summary_at(made).save(&directories.state).unwrap();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert!(shows(&window, "2,4 Go"));
        // Without "Rien n'a encore été supprimé": that is this session's.
        assert!(
            shows(&window, &format!("{estimate}. Éléments trouvés : 18312.")),
            "{estimate}"
        );
        assert!(
            shows(&window, &format!("Dernière analyse {said}")),
            "{said}"
        );
    }

    #[test]
    fn an_estimate_of_today_is_dated_in_the_local_zone() {
        estimate_dated(
            "2026-03-10T11:18:00Z",
            "Estimation d'aujourd'hui à 12 h 18",
            "aujourd'hui à 12 h 18",
        );
    }

    #[test]
    fn an_estimate_of_yesterday_says_so() {
        estimate_dated(
            "2026-03-09T17:00:00Z",
            "Estimation d'hier à 18 h",
            "hier à 18 h",
        );
    }

    #[test]
    fn the_first_of_a_month_is_the_first() {
        estimate_dated(
            "2026-03-01T08:05:00Z",
            "Estimation du 1er mars à 9 h 05",
            "le 1er mars à 9 h 05",
        );
    }

    #[test]
    fn an_older_estimate_says_its_date() {
        estimate_dated(
            "2026-03-05T08:05:00Z",
            "Estimation du 5 mars à 9 h 05",
            "le 5 mars à 9 h 05",
        );
    }

    #[test]
    fn a_cleaning_since_the_summary_outdates_it_as_the_page_is_shown_again() {
        let directories = Directories::new();
        summary_at("2026-03-10T11:18:00Z")
            .save(&directories.state)
            .unwrap();
        // A history older than the summary outdates nothing.
        directories.history_at("2026-03-10T11:00:00Z");
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        go_to(&window, "À propos");
        go_to(&window, "Accueil");
        assert!(shows(
            &window,
            "Estimation d'aujourd'hui à 12 h 18. Éléments trouvés : 18312."
        ));
        assert!(!shows(&window, "Estimation à refaire"));
        go_to(&window, "À propos");
        directories.history_at("2026-03-10T12:00:00Z");
        go_to(&window, "Accueil");
        assert!(shows(&window, "Estimation à refaire"));
        assert!(shows(
            &window,
            "L'estimation date d'avant un nettoyage. Analysez à nouveau pour une estimation à jour."
        ));
    }

    #[test]
    fn a_history_that_cannot_be_judged_makes_the_summary_unavailable() {
        let directories = Directories::new();
        summary_at("2026-03-10T11:18:00Z")
            .save(&directories.state)
            .unwrap();
        fs::create_dir(History::path(&directories.state)).unwrap();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert_eq!(
            said(&window, "Récupérable").as_deref(),
            Some("Indisponible")
        );
        button(&window, "Analyser");
    }

    #[test]
    fn a_summary_dated_out_of_any_date_is_unavailable() {
        let directories = Directories::new();
        LastAnalysis {
            time: i64::MAX,
            ..summary_at("2026-03-10T11:18:00Z")
        }
        .save(&directories.state)
        .unwrap();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert_eq!(
            said(&window, "Récupérable").as_deref(),
            Some("Indisponible")
        );
    }

    #[test]
    fn what_cleanings_freed_adds_up_and_is_said_only_when_known() {
        let cleaned = |freed| Some(Cleaned { freed, at: Some(0) });
        assert_eq!(freed_since(None, Some(7)), Some(7));
        assert_eq!(freed_since(cleaned(Some(5)), Some(7)), Some(12));
        assert_eq!(freed_since(cleaned(None), Some(7)), None);
        assert_eq!(freed_since(cleaned(Some(5)), None), None);
        let said = |freed| {
            let shown = reclaimable(
                Summary::Missing,
                cleaned(freed),
                fixed_now(),
                &TimeZone::UTC,
            );
            (shown.state, shown.freed_known, shown.cleaned_at_known)
        };
        assert_eq!(said(Some(7)), (ui::ReclaimableState::Cleaned, true, true));
        assert_eq!(
            said(Some(0)),
            (ui::ReclaimableState::Cleaned, false, true),
            "nothing known freed"
        );
        assert_eq!(
            said(None),
            (ui::ReclaimableState::Cleaned, false, true),
            "its thread died"
        );
    }

    #[test]
    fn an_unreadable_summary_is_unavailable_and_still_offers_an_analysis() {
        let directories = Directories::new();
        fs::write(LastAnalysis::path(&directories.state), "{").unwrap();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert_eq!(
            said(&window, "Récupérable").as_deref(),
            Some("Indisponible")
        );
        assert!(shows(
            &window,
            "Le résumé de la dernière analyse n'a pas pu être lu. Une nouvelle analyse en refera un."
        ));
        button(&window, "Analyser");
    }

    #[test]
    fn without_a_state_directory_reclaimable_is_unavailable() {
        let (seams, _sampler) = noting(None);
        let window = on_home(seams);
        assert_eq!(
            said(&window, "Récupérable").as_deref(),
            Some("Indisponible")
        );
    }

    #[test]
    fn analyser_runs_the_default_analysis_on_the_cleaner_and_its_summary_comes_back() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let state = sandbox.root.join("state");
        // An estimate a cleaning outdated since.
        LastAnalysis {
            time: Timestamp::now().as_second() - 60,
            ..summary_at("2026-03-10T11:18:00Z")
        }
        .save(&state)
        .unwrap();
        fs::write(History::path(&state), "").unwrap();
        let window = on_home_with(sandbox.read(&state), noting(Some(state.clone())).0);
        assert!(shows(&window, "Estimation à refaire"));
        // A category left out by the user is analysed again from home.
        go_to(&window, "Nettoyeur");
        checkbox(&window, "Applications").invoke_accessible_default_action();
        go_to(&window, "Accueil");
        button(&window, "Analyser").invoke_accessible_default_action();
        assert_eq!(window.get_page(), ui::Page::Cleaner);
        // Back home while it runs: its end reaches the page shown.
        go_to(&window, "Accueil");
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        let kept = LastAnalysis::load(&state).unwrap().unwrap();
        assert_eq!(kept.entries, 1, "the pip cache, of Applications");
        assert!(shows(
            &window,
            &format!(
                "Estimation d'aujourd'hui à {}. Éléments trouvés : 1. Rien n'a encore été supprimé.",
                utc_clock(kept.time)
            )
        ));
        // The results are still held: they are a click away.
        button(&window, "Voir les derniers résultats").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(window.get_page(), ui::Page::Cleaner);
        assert_eq!(step(&window), ui::CleanerStep::Results);
    }

    #[test]
    fn a_summary_that_cannot_be_written_is_still_shown() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        let not_a_directory = sandbox.file("state-is-a-file");
        settle_files();
        let window = on_home_with(sandbox.read(&not_a_directory), noting(None).0);
        button(&window, "Analyser").invoke_accessible_default_action();
        go_to(&window, "Accueil");
        watch_until(&window, |window| step(window) == ui::CleanerStep::Results);
        assert!(shows_a_text_starting(
            &window,
            "Estimation d'aujourd'hui à "
        ));
    }

    #[test]
    fn during_an_analysis_reclaimable_says_so_and_another_is_refused() {
        let directories = Directories::new();
        summary_at("2026-03-10T11:18:00Z")
            .save(&directories.state)
            .unwrap();
        let (cleaner, scan) = scripted(Vec::new());
        let (seams, _sampler) = directories.seams();
        let window = on_home_with(cleaner, seams);
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        go_to(&window, "Accueil");
        assert!(shows(&window, "Analyse en cours"));
        assert!(shows(
            &window,
            "Le résultat remplacera l'estimation précédente (2,4 Go)."
        ));
        assert_eq!(
            button(&window, "Analyser").accessible_enabled(),
            Some(false)
        );
        let_the_screen_settle();
        window.global::<ui::Dashboard>().invoke_analyze();
        assert_eq!(
            window.get_page(),
            ui::Page::Home,
            "refused, left where it was"
        );
        assert!(scan.started.try_recv().is_err(), "no second analysis");
        scan.go.send(()).unwrap();
    }

    #[test]
    fn a_partial_analysis_never_says_it_will_replace_the_estimate() {
        let directories = Directories::new();
        summary_at("2026-03-10T11:18:00Z")
            .save(&directories.state)
            .unwrap();
        let (cleaner, scan) = scripted(Vec::new());
        let (seams, _sampler) = directories.seams();
        let window = on_home_with(cleaner, seams);
        go_to(&window, "Nettoyeur");
        checkbox(&window, "Jeux").invoke_accessible_default_action();
        button(&window, "Analyser").invoke_accessible_default_action();
        scan.started.recv().unwrap();
        go_to(&window, "Accueil");
        assert!(shows(&window, "Analyse en cours"));
        assert!(!shows_a_text_starting(&window, "Le résultat remplacera"));
        scan.go.send(()).unwrap();
        // Its end leaves the estimate as it was.
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        assert!(shows(
            &window,
            "Estimation d'aujourd'hui à 12 h 18. Éléments trouvés : 18312."
        ));
    }

    /// Analyses the sandbox from the cleaner, Jeux left out, and opens the
    /// confirmation of its cleaning.
    fn confirming(window: &ui::AppWindow) {
        go_to(window, "Nettoyeur");
        checkbox(window, "Jeux").invoke_accessible_default_action();
        button(window, "Analyser").invoke_accessible_default_action();
        watch_until(window, |window| step(window) == ui::CleanerStep::Results);
        let_the_screen_settle();
        button(window, "Nettoyer").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        let_the_screen_settle();
    }

    #[test]
    fn analyser_is_inactive_and_refused_while_a_cleaning_waits_for_its_confirmation() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let window = on_home_with(sandbox.read(&sandbox.root.join("state")), inert());
        confirming(&window);
        let chosen = window.global::<ui::Cleaner>().get_chosen_count();
        go_to(&window, "Accueil");
        assert_eq!(
            button(&window, "Analyser").accessible_enabled(),
            Some(false)
        );
        window.global::<ui::Dashboard>().invoke_analyze();
        assert_eq!(
            window.get_page(),
            ui::Page::Home,
            "refused, left where it was"
        );
        assert_eq!(
            window.global::<ui::Cleaner>().get_chosen_count(),
            chosen,
            "the choices left as they were"
        );
    }

    #[test]
    fn a_cleaning_says_what_it_freed_until_the_next_analysis() {
        let sandbox = Sandbox::new();
        sandbox.file("home/.cache/pip/old");
        settle_files();
        let state = sandbox.root.join("state");
        let (seams, sampler) = noting(Some(state.clone()));
        let window = on_home_with(sandbox.read(&state), seams);
        confirming(&window);
        ElementHandle::find_by_accessible_label(&window, "J'ai compris que c'est définitif")
            .find(|element| element.accessible_role() == Some(AccessibleRole::Checkbox))
            .unwrap()
            .invoke_accessible_default_action();
        button(&window, "Supprimer définitivement").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Receipt);
        assert!(sampler.commands.borrow().contains(&Command::RefreshVolumes));
        go_to(&window, "Accueil");
        assert!(shows(&window, "7 o libérés"));
        assert!(
            !shows(&window, "Voir les derniers résultats"),
            "the report is gone"
        );
        assert!(shows_a_text_starting(&window, "Nettoyage d'aujourd'hui à "));
        // From the receipt, a new analysis: nothing left, its summary at 0.
        let_the_screen_settle();
        button(&window, "Analyser").invoke_accessible_default_action();
        watch_until(&window, |window| step(window) == ui::CleanerStep::Outcome);
        go_to(&window, "Accueil");
        assert!(!shows(&window, "7 o libérés"));
        assert!(shows(&window, "0 o"));
        assert!(shows_a_text_starting(
            &window,
            "Estimation d'aujourd'hui à "
        ));
    }

    const GB: u64 = 1_000_000_000;

    fn known(name: VolumeName, used: u64, total: u64) -> Volume {
        Volume {
            name,
            mount_point: PathBuf::from("/okapi-test"),
            filesystem: "ext4".to_owned(),
            kind: VolumeKind::Local,
            space: VolumeSpace::Known(Space {
                total,
                available: total - used,
                used,
            }),
        }
    }

    fn silent(name: &str, space: VolumeSpace) -> Volume {
        Volume {
            name: VolumeName::Source(name.to_owned()),
            mount_point: PathBuf::from("/mnt/share"),
            filesystem: "nfs4".to_owned(),
            kind: VolumeKind::Network,
            space,
        }
    }

    fn names(model: &ModelRc<ui::VolumeShown>) -> Vec<(ui::VolumeNaming, String)> {
        model
            .iter()
            .map(|volume| (volume.naming, volume.name.to_string()))
            .collect()
    }

    #[test]
    fn the_small_widget_names_the_two_fullest_and_counts_the_others() {
        let volumes = vec![
            known(VolumeName::System, 39 * GB, 198 * GB),
            silent("nas:/photos", VolumeSpace::NotResponding),
            known(VolumeName::Label("KINGSTON".to_owned()), 14 * GB, 15 * GB),
            known(VolumeName::Home, 222 * GB, 456 * GB),
            silent(
                "nas:/films",
                VolumeSpace::Failed(Unavailable {
                    kind: std::io::ErrorKind::PermissionDenied,
                    code: Some(13),
                }),
            ),
        ];
        let shown = disks(Some(&Ok(volumes)), &Rows::default());
        assert_eq!(shown.state, ui::ReadingState::Read);
        assert_eq!(
            names(&shown.fullest),
            [
                (ui::VolumeNaming::Own, "KINGSTON".to_owned()),
                (ui::VolumeNaming::Home, String::new()),
            ],
            "by the part used, not by the bytes"
        );
        assert_eq!(
            (shown.others, shown.others_not_responding),
            (3, 1),
            "an unreadable one answered"
        );
        assert_eq!(shown.volumes.row_count(), 5, "every one, in order");
        let kingston = shown.fullest.row_data(0).unwrap();
        assert_eq!(kingston.percent, 93);
        assert!(kingston.nearly_full);
        let home = shown.fullest.row_data(1).unwrap();
        assert_eq!(home.percent, 48);
        assert!(!home.nearly_full);
        let failed = shown.volumes.row_data(4).unwrap();
        assert_eq!(
            (failed.space, failed.place),
            (ui::SpaceState::Failed, ui::VolumePlace::Network)
        );
    }

    #[test]
    fn volumes_not_read_yet_or_unreadable_say_so() {
        assert_eq!(
            disks(None, &Rows::default()).state,
            ui::ReadingState::Reading
        );
        let failed = Unavailable {
            kind: std::io::ErrorKind::Unsupported,
            code: None,
        };
        assert_eq!(
            disks(Some(&Err(failed)), &Rows::default()).state,
            ui::ReadingState::Unavailable
        );
        let none = disks(Some(&Ok(Vec::new())), &Rows::default());
        assert_eq!((none.state, none.others), (ui::ReadingState::Read, 0));
    }

    fn status(swap_total: u64) -> MemoryStatus {
        MemoryStatus {
            total: 32 * GB,
            available: 22 * GB,
            swap_total,
            swap_free: swap_total / 4 * 3,
        }
    }

    #[test]
    fn the_memory_used_is_what_applications_cannot_take_any_more() {
        let shown = memory(Some(&Ok(status(8 * GB))), None, &Rows::default());
        assert_eq!(shown.state, ui::ReadingState::Read);
        assert_eq!(shown.used, size(10 * GB));
        assert_eq!(shown.percent, 31);
        assert!(shown.has_swap);
        assert_eq!((shown.swap_used, shown.swap_percent), (size(2 * GB), 25));
        assert_eq!(shown.applications_state, ui::ReadingState::Reading);
        assert_eq!(
            (shown.available, shown.swap_total),
            (size(22 * GB), size(8 * GB))
        );
        assert!(!memory(Some(&Ok(status(0))), None, &Rows::default()).has_swap);
        assert_eq!(
            memory(None, None, &Rows::default()).state,
            ui::ReadingState::Reading
        );
        let failed = Unavailable {
            kind: std::io::ErrorKind::Unsupported,
            code: None,
        };
        assert_eq!(
            memory(Some(&Err(failed)), None, &Rows::default()).state,
            ui::ReadingState::Unavailable
        );
    }

    #[test]
    fn the_applications_are_listed_as_read_by_their_name() {
        let applications = Applications {
            top: vec![
                okapi_tools::monitor::ApplicationMemory {
                    name: ApplicationName::Desktop("Firefox".to_owned()),
                    resident_bytes: 3 * GB,
                    processes: 12,
                },
                okapi_tools::monitor::ApplicationMemory {
                    name: ApplicationName::Process("pipewire".to_owned()),
                    resident_bytes: GB / 10,
                    processes: 1,
                },
            ],
            unreadable: 0,
        };
        let shown = memory(
            Some(&Ok(status(0))),
            Some(&Ok(applications)),
            &Rows::default(),
        );
        assert_eq!(shown.applications_state, ui::ReadingState::Read);
        let listed: Vec<String> = shown
            .applications
            .iter()
            .map(|application| application.name.to_string())
            .collect();
        assert_eq!(listed, ["Firefox", "pipewire"]);
        let failed = Unavailable {
            kind: std::io::ErrorKind::PermissionDenied,
            code: None,
        };
        assert_eq!(
            memory(Some(&Ok(status(0))), Some(&Err(failed)), &Rows::default()).applications_state,
            ui::ReadingState::Unavailable
        );
    }

    #[test]
    fn the_disks_and_the_memory_read_reach_their_widgets() {
        let directories = Directories::new();
        kept(
            &directories.config,
            &[("disks", WidgetSize::Small), ("memory", WidgetSize::Large)],
        );
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        assert_eq!(
            said(&window, "Disques").as_deref(),
            Some("Lecture en cours")
        );
        let sink = sampler.sink.as_ref().unwrap();
        sink.send(Sample::Volumes(Ok(vec![
            known(VolumeName::System, 39 * GB, 198 * GB),
            known(VolumeName::Label("KINGSTON".to_owned()), 14 * GB, 15 * GB),
            silent("nas:/photos", VolumeSpace::NotResponding),
        ])))
        .unwrap();
        sink.send(Sample::Memory(Ok(okapi_tools::monitor::MemoryReading {
            status: status(8 * GB),
            applications: Some(Ok(Applications {
                top: vec![okapi_tools::monitor::ApplicationMemory {
                    name: ApplicationName::Desktop("Firefox".to_owned()),
                    resident_bytes: 3 * GB,
                    processes: 12,
                }],
                unreadable: 0,
            })),
        })))
        .unwrap();
        take_in();
        assert!(shows(&window, "KINGSTON : 1,0 Go libres, presque plein"));
        assert!(shows(&window, "Système : 159,0 Go libres"));
        assert!(shows(&window, "Autres volumes : 1 (sans réponse : 1)"));
        assert_eq!(
            said(&window, "Mémoire").as_deref(),
            Some("10,0 Go occupés sur 32,0 Go")
        );
        assert!(shows(&window, "Firefox : environ 3,0 Go"));
        // A reading of the memory alone keeps the list read before.
        sink.send(Sample::Memory(Ok(okapi_tools::monitor::MemoryReading {
            status: status(8 * GB),
            applications: None,
        })))
        .unwrap();
        take_in();
        assert!(shows(&window, "Firefox : environ 3,0 Go"));
    }

    #[test]
    fn an_empty_volume_sits_last_among_the_fullest() {
        let volumes = vec![
            known(VolumeName::Label("A".to_owned()), 50, 100),
            known(VolumeName::Label("Z".to_owned()), 0, 0),
            known(VolumeName::Label("C".to_owned()), 90, 100),
        ];
        let shown = disks(Some(&Ok(volumes)), &Rows::default());
        assert_eq!(
            names(&shown.fullest),
            [
                (ui::VolumeNaming::Own, "C".to_owned()),
                (ui::VolumeNaming::Own, "A".to_owned()),
            ]
        );
    }

    #[test]
    fn a_volume_says_where_and_what_it_is() {
        let shown = volume(&Volume {
            name: VolumeName::MountPoint(PathBuf::from("/media/usb")),
            mount_point: PathBuf::from("/media/usb"),
            filesystem: "vfat".to_owned(),
            kind: VolumeKind::Removable,
            space: VolumeSpace::Known(Space {
                total: 16 * GB,
                available: 4 * GB,
                used: 12 * GB,
            }),
        });
        assert_eq!(
            (shown.naming, shown.name.as_str()),
            (ui::VolumeNaming::Own, "/media/usb")
        );
        assert_eq!(shown.filesystem.as_str(), "vfat");
        assert_eq!(shown.place, ui::VolumePlace::Removable);
        assert_eq!(
            (shown.free, shown.used, shown.total, shown.percent),
            (size(4 * GB), size(12 * GB), size(16 * GB), 75)
        );
    }

    /// The window on a dashboard of `widgets`, the volumes and memory sent.
    fn reading_home(
        widgets: &[(&str, WidgetSize)],
        volumes: Vec<Volume>,
        memory: Option<Result<okapi_tools::monitor::MemoryReading, Unavailable>>,
    ) -> (ui::AppWindow, Sampler, Directories) {
        let directories = Directories::new();
        kept(&directories.config, widgets);
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        let sink = sampler.sink.as_ref().unwrap();
        sink.send(Sample::Volumes(Ok(volumes))).unwrap();
        if let Some(memory) = memory {
            sink.send(Sample::Memory(memory)).unwrap();
        }
        take_in();
        (window, sampler, directories)
    }

    fn six_volumes() -> Vec<Volume> {
        let mut usb = known(VolumeName::Label("KINGSTON".to_owned()), 14 * GB, 15 * GB);
        usb.kind = VolumeKind::Removable;
        let mut local = silent("disque lent", VolumeSpace::NotResponding);
        local.kind = VolumeKind::Local;
        vec![
            known(VolumeName::System, 39 * GB, 198 * GB),
            known(VolumeName::Home, 222 * GB, 456 * GB),
            usb,
            silent("nas:/photos", VolumeSpace::NotResponding),
            local,
            silent(
                "nas:/films",
                VolumeSpace::Failed(Unavailable {
                    kind: std::io::ErrorKind::PermissionDenied,
                    code: Some(13),
                }),
            ),
        ]
    }

    #[test]
    fn the_wide_disks_widget_lists_every_volume_free_of_its_total() {
        let (window, ..) = reading_home(&[("disks", WidgetSize::Wide)], six_volumes(), None);
        assert!(shows(&window, "Système : 159,0 Go libres sur 198,0 Go"));
        assert!(shows(
            &window,
            "Dossier personnel : 234,0 Go libres sur 456,0 Go"
        ));
        assert!(shows(
            &window,
            "KINGSTON (amovible) : 1,0 Go libres sur 15,0 Go, presque plein"
        ));
        assert!(shows(
            &window,
            "nas:/photos (partage réseau) : Le partage réseau n'a pas répondu au dernier relevé. Sa taille s'affichera dès qu'il répondra."
        ));
        assert!(shows(
            &window,
            "disque lent : Le volume n'a pas répondu au dernier relevé. Sa taille s'affichera dès qu'il répondra."
        ));
        assert!(shows(
            &window,
            "nas:/films (partage réseau) : Sa taille n'a pas pu être lue."
        ));
        // A badge for the nearly full one and each silent one; their words
        // are the control's to say (the sentences above).
        assert_eq!(
            ElementHandle::find_by_element_type_name(&window, "Badge").count(),
            4
        );
        assert!(shows(&window, "nfs4, aucune réponse au dernier relevé"));
        assert!(shows(&window, "Volumes : 6"));
        assert_eq!(
            said(&window, "Disques").as_deref(),
            Some("KINGSTON : 1,0 Go libres"),
            "the fullest, by its name"
        );
    }

    #[test]
    fn the_tall_disks_widget_says_where_each_volume_is_mounted() {
        let (window, ..) = reading_home(&[("disks", WidgetSize::Tall)], six_volumes(), None);
        assert!(shows(
            &window,
            "Système (/okapi-test · ext4) : 159,0 Go libres"
        ));
        assert!(shows(&window, "/okapi-test · ext4"));
    }

    #[test]
    fn the_large_disks_widget_details_each_volume() {
        let (window, ..) = reading_home(&[("disks", WidgetSize::Large)], six_volumes(), None);
        assert!(shows(
            &window,
            "Système (/okapi-test · ext4) : 159,0 Go libres sur 198,0 Go"
        ));
        assert!(shows(&window, "39,0 Go utilisés (19 %)"));
        assert!(shows(&window, "14,0 Go utilisés (93 %)"));
    }

    #[test]
    fn the_small_disks_widget_counts_the_others_apart_from_the_silent_ones() {
        let (window, ..) = reading_home(&[("disks", WidgetSize::Small)], six_volumes(), None);
        assert!(shows(&window, "Autres volumes : 4 (sans réponse : 2)"));
    }

    #[test]
    fn the_small_disks_widget_counts_the_others_alone_when_all_answer() {
        let (window, ..) = reading_home(
            &[("disks", WidgetSize::Small)],
            six_volumes().into_iter().take(3).collect(),
            None,
        );
        assert!(shows(&window, "Autres volumes : 1"));
    }

    #[test]
    fn without_a_volume_the_disks_widget_says_so() {
        let (window, ..) = reading_home(&[("disks", WidgetSize::Wide)], Vec::new(), None);
        assert!(shows(&window, "Aucun volume à afficher"));
    }

    fn reading_of(
        swap_total: u64,
        applications: Option<Result<Applications, Unavailable>>,
    ) -> okapi_tools::monitor::MemoryReading {
        okapi_tools::monitor::MemoryReading {
            status: status(swap_total),
            applications,
        }
    }

    fn firefox(gigabytes: u64) -> Applications {
        Applications {
            top: vec![okapi_tools::monitor::ApplicationMemory {
                name: ApplicationName::Desktop("Firefox".to_owned()),
                resident_bytes: gigabytes * GB,
                processes: 12,
            }],
            unreadable: 0,
        }
    }

    #[test]
    fn the_memory_widget_waits_for_its_list_and_notes_approximate_values_when_large() {
        let (window, sampler, _directories) = reading_home(
            &[("memory", WidgetSize::Large)],
            Vec::new(),
            Some(Ok(reading_of(0, None))),
        );
        assert!(shows(
            &window,
            "La liste apparaîtra au prochain relevé, dans quelques secondes."
        ));
        assert!(shows(&window, "Pas de swap"));
        let sink = sampler.sink.as_ref().unwrap();
        sink.send(Sample::Memory(Ok(reading_of(8 * GB, Some(Ok(firefox(3)))))))
            .unwrap();
        take_in();
        assert!(shows(&window, "Firefox : environ 3,0 Go"));
        assert!(shows(&window, "Swap : 2,0 Go sur 8,0 Go"));
        assert!(shows_a_text_starting(&window, "Valeurs approchées"));
        // A new list replaces the one before.
        sink.send(Sample::Memory(Ok(reading_of(8 * GB, Some(Ok(firefox(5)))))))
            .unwrap();
        take_in();
        assert!(shows(&window, "Firefox : environ 5,0 Go"));
    }

    #[test]
    fn the_wide_memory_widget_has_no_note_and_says_an_unreadable_list() {
        let failed = Unavailable {
            kind: std::io::ErrorKind::PermissionDenied,
            code: None,
        };
        let (window, ..) = reading_home(
            &[("memory", WidgetSize::Wide)],
            Vec::new(),
            Some(Ok(reading_of(8 * GB, Some(Err(failed))))),
        );
        assert!(shows(&window, "Les applications n'ont pas pu être lues."));
        assert!(!shows_a_text_starting(&window, "Valeurs approchées"));
        assert!(shows(&window, "22,0 Go disponibles"));
    }

    #[test]
    fn an_unreadable_memory_is_unavailable() {
        let failed = Unavailable {
            kind: std::io::ErrorKind::Unsupported,
            code: None,
        };
        let (window, ..) = reading_home(
            &[("memory", WidgetSize::Small)],
            Vec::new(),
            Some(Err(failed)),
        );
        assert_eq!(said(&window, "Mémoire").as_deref(), Some("Indisponible"));
        assert!(shows(
            &window,
            "La mémoire n'a pas pu être lue sur ce système."
        ));
    }

    #[test]
    fn an_odd_number_of_volumes_fills_the_two_columns_without_a_ghost() {
        let volumes: Vec<Volume> = six_volumes().into_iter().take(5).collect();
        let (window, sampler, _directories) =
            reading_home(&[("disks", WidgetSize::Wide)], volumes, None);
        assert!(shows(
            &window,
            "disque lent : Le volume n'a pas répondu au dernier relevé. Sa taille s'affichera dès qu'il répondra."
        ));
        assert!(!shows(&window, "Système : 0 o libres sur 0 o"), "no ghost");
        // Fewer volumes: those gone are no longer said.
        sampler
            .sink
            .as_ref()
            .unwrap()
            .send(Sample::Volumes(Ok(six_volumes()
                .into_iter()
                .take(3)
                .collect())))
            .unwrap();
        take_in();
        assert!(!shows(
            &window,
            "disque lent : Le volume n'a pas répondu au dernier relevé. Sa taille s'affichera dès qu'il répondra."
        ));
        assert!(!shows(&window, "Système : 0 o libres sur 0 o"), "no ghost");
        assert!(shows(&window, "Volumes : 3"));
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Disques")
                .any(|element| element.accessible_role() == Some(AccessibleRole::List)),
            "a list reached by the keyboard"
        );
    }

    #[test]
    fn the_small_memory_widget_sums_up_without_its_applications() {
        let (window, ..) = reading_home(
            &[("memory", WidgetSize::Small)],
            Vec::new(),
            Some(Ok(reading_of(8 * GB, Some(Ok(firefox(3)))))),
        );
        assert_eq!(
            said(&window, "Mémoire").as_deref(),
            Some("10,0 Go occupés sur 32,0 Go")
        );
        assert!(shows(&window, "22,0 Go disponibles"));
        assert!(shows(&window, "Swap : 2,0 Go sur 8,0 Go"));
        assert!(!shows(&window, "Firefox : environ 3,0 Go"));
    }

    #[test]
    fn the_wide_memory_widget_lists_its_applications() {
        let (window, ..) = reading_home(
            &[("memory", WidgetSize::Wide)],
            Vec::new(),
            Some(Ok(reading_of(0, Some(Ok(firefox(3)))))),
        );
        assert!(shows(&window, "Firefox : environ 3,0 Go"));
        assert!(shows(&window, "Pas de swap"));
    }

    #[test]
    fn a_placed_volume_says_what_it_is_and_its_mount_point_once() {
        let mut usb = known(
            VolumeName::MountPoint(PathBuf::from("/media/usb")),
            12 * GB,
            16 * GB,
        );
        usb.mount_point = PathBuf::from("/media/usb");
        usb.filesystem = "vfat".to_owned();
        usb.kind = VolumeKind::Removable;
        let mut share = silent("nas:/photos", VolumeSpace::NotResponding);
        share.mount_point = PathBuf::from("/mnt/photos");
        let (window, ..) = reading_home(&[("disks", WidgetSize::Tall)], vec![usb, share], None);
        assert!(shows(
            &window,
            "/media/usb (vfat · amovible) : 4,0 Go libres"
        ));
        assert!(shows(
            &window,
            "nas:/photos (/mnt/photos · nfs4 · partage réseau) : Le partage réseau n'a pas répondu au dernier relevé. Sa taille s'affichera dès qu'il répondra."
        ));
    }

    fn uses(percents: &[u32]) -> VecDeque<f64> {
        percents
            .iter()
            .map(|percent| f64::from(*percent) / 100.0)
            .collect()
    }

    #[test]
    fn the_curve_ends_at_the_right_of_its_box_and_closes_its_area_below() {
        let (line, area) = curve(&uses(&[10, 30, 20]), CURVE_LOW_SCALE);
        assert_eq!(line, "M57 40.0 L58 20.0 L59 30.0");
        assert_eq!(area, "M57 40.0 L58 20.0 L59 30.0 L59 50 L57 50 Z");
        assert_eq!(
            curve(&uses(&[10]), CURVE_LOW_SCALE),
            (String::new(), String::new()),
            "one point draws nothing"
        );
    }

    #[test]
    fn the_curve_s_scale_doubles_once_a_use_passes_its_half() {
        let reading = Ok(ProcessorReading {
            usage: Some(0.2),
            load_average: Some(0.845),
        });
        let low = processor(Some(&reading), &uses(&[20, 50, 29]));
        assert_eq!((low.scale, low.lowest, low.highest), (50, 20, 50));
        assert_eq!(low.percent, 29, "the last one");
        assert_eq!((low.load_whole, low.load_hundredths), (0, 85));
        assert!(low.filling);
        let high = processor(Some(&reading), &uses(&[20, 51, 20]));
        assert_eq!(high.scale, 100);
        assert!(high.area.ends_with("L59 100 L57 100 Z"));
        assert_eq!(load_parts(2.05), (2, 5));
    }

    #[test]
    fn the_processor_says_nothing_before_a_use_is_measured() {
        assert_eq!(
            processor(None, &VecDeque::new()).state,
            ui::ReadingState::Reading
        );
        let first = Ok(ProcessorReading {
            usage: None,
            load_average: None,
        });
        assert_eq!(
            processor(Some(&first), &VecDeque::new()).state,
            ui::ReadingState::Reading
        );
        let failed = Unavailable {
            kind: std::io::ErrorKind::Unsupported,
            code: None,
        };
        assert_eq!(
            processor(Some(&Err(failed)), &VecDeque::new()).state,
            ui::ReadingState::Unavailable
        );
        let full = processor(Some(&first), &uses(&[1; CURVE_READINGS]));
        assert!(!full.filling);
        assert!(!full.load_known);
    }

    fn sent(sampler: &Sampler, usage: Option<f64>, load_average: Option<f64>) {
        sampler
            .sink
            .as_ref()
            .unwrap()
            .send(Sample::Processor(Ok(ProcessorReading {
                usage,
                load_average,
            })))
            .unwrap();
        take_in();
    }

    fn used(sampler: &Sampler, usage: Option<f64>) {
        sent(sampler, usage, Some(0.84));
    }

    #[test]
    fn the_processor_s_curve_fills_and_starts_again_after_a_pause() {
        let directories = Directories::new();
        kept(&directories.config, &[("processor", WidgetSize::Wide)]);
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        used(&sampler, None);
        assert_eq!(
            said(&window, "Processeur").as_deref(),
            Some("Lecture en cours")
        );
        used(&sampler, Some(0.10));
        used(&sampler, Some(0.12));
        assert_eq!(
            said(&window, "Processeur").as_deref(),
            Some("Processeur utilisé à 12 %")
        );
        assert!(shows(&window, "Charge moyenne 0,84 sur 1 min"));
        assert!(shows(
            &window,
            "Courbe en cours de remplissage, 12 % maintenant"
        ));
        assert!(shows(&window, "il y a 2 min"));
        assert!(shows(&window, "maintenant · la courbe se remplit"));
        assert!(shows(&window, "50 %"), "the scale");
        assert_eq!(window.global::<ui::Dashboard>().get_curve_span(), 59);
        for _ in 1..CURVE_READINGS {
            used(&sampler, Some(0.3));
        }
        used(&sampler, Some(0.4));
        assert!(shows(
            &window,
            "Utilisation sur 2 minutes, entre 30 et 40 %, 40 % maintenant"
        ));
        assert!(!shows(&window, "maintenant · la courbe se remplit"));
        used(&sampler, Some(0.6));
        assert!(shows(&window, "100 %"), "the scale for the whole");
        go_to(&window, "À propos");
        go_to(&window, "Accueil");
        used(&sampler, None);
        assert_eq!(
            said(&window, "Processeur").as_deref(),
            Some("Lecture en cours"),
            "the curve starts again"
        );
    }

    #[test]
    fn the_small_processor_widget_shows_its_use_without_a_curve() {
        let directories = Directories::new();
        kept(&directories.config, &[("processor", WidgetSize::Small)]);
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        sent(&sampler, Some(0.12), Some(2.05));
        assert_eq!(
            said(&window, "Processeur").as_deref(),
            Some("Processeur utilisé à 12 %")
        );
        assert!(shows(&window, "Charge moyenne 2,05 sur 1 min"));
        assert!(!shows(
            &window,
            "Courbe en cours de remplissage, 12 % maintenant"
        ));
        let bar = ElementHandle::find_by_accessible_label(&window, "Processeur utilisé à 12 %")
            .find(|element| element.accessible_role() == Some(AccessibleRole::ProgressIndicator))
            .expect("its bar");
        assert_eq!(bar.accessible_value().as_deref(), Some("12"));
    }

    #[test]
    fn the_processor_s_curve_starts_again_after_a_failed_reading() {
        let directories = Directories::new();
        kept(&directories.config, &[("processor", WidgetSize::Wide)]);
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        used(&sampler, Some(0.1));
        used(&sampler, Some(0.2));
        sampler
            .sink
            .as_ref()
            .unwrap()
            .send(Sample::Processor(Err(Unavailable {
                kind: std::io::ErrorKind::InvalidData,
                code: None,
            })))
            .unwrap();
        take_in();
        assert_eq!(said(&window, "Processeur").as_deref(), Some("Indisponible"));
        assert!(shows(
            &window,
            "L'utilisation du processeur n'a pas pu être lue."
        ));
        used(&sampler, Some(0.3));
        used(&sampler, Some(0.4));
        assert!(
            shows(&window, "Courbe en cours de remplissage, 40 % maintenant"),
            "two points since the failure"
        );
        // 58 points since the failure: still filling, unless the two read
        // before it were kept (60 then, a full curve from 10 %).
        for _ in 2..CURVE_READINGS - 2 {
            used(&sampler, Some(0.3));
        }
        assert!(shows(
            &window,
            "Courbe en cours de remplissage, 30 % maintenant"
        ));
    }
}
