//! The Home page: the widgets of the layout kept (or the default one) in a
//! grid, fed by the dashboard's sampler while the page is shown, and the
//! summary of the last analysis the cleaner kept.

mod catalog;
mod layout;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use okapi_store::{DashboardLayout, LastAnalysis, WidgetSize};
use okapi_tools::monitor::{Command, Sample, Source, SystemReading};
use slint::{ComponentHandle as _, ModelRc, VecModel};

use crate::dates::{self, Day};
use crate::shown::{shown, size};
use crate::ui;
use catalog::WidgetKind;
use layout::{MAX_COLUMNS, place, shown as widgets_shown};

/// How often the readings sent are taken in: the fastest source reads
/// every two seconds.
const TAKE_SAMPLES_EVERY: Duration = Duration::from_millis(500);
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
    /// The readings no longer reach the page.
    lost: bool,
    timer: slint::Timer,
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
                    // No widget of this page draws them yet.
                    Ok(Sample::Volumes(_) | Sample::Memory(_) | Sample::Processor(_)) => {}
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
}
