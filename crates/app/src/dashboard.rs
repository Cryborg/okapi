//! The Home page: the widgets of the layout kept (or the default one) in a
//! grid, fed by the dashboard's sampler while the page is shown, and the
//! summary of the last analysis the cleaner kept.

mod catalog;
mod editing;
mod layout;

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use okapi_store::{
    DashboardLayout, DashboardWidget, History, LastAnalysis, LastCleaning, WidgetSize,
};
use okapi_tools::monitor::{
    ApplicationName, Applications, Command, MemoryStatus, ProcessorReading, Sample, Source,
    SystemReading, Unavailable, Volume, VolumeKind, VolumeName, VolumeSpace, nearly_full,
};
use slint::{ComponentHandle as _, Model as _, ModelRc, VecModel};

use crate::dates::{self, Day};
use crate::saving::{LastWrite, Saver};
use crate::shown::{show, shown, size};
use crate::ui;
use catalog::WidgetKind;
use editing::{Removed, Widgets};
use layout::{MAX_COLUMNS, place, shown as widgets_shown};

/// The thread that writes the layout.
const SAVER_NAME: &str = "okapi-dashboard-save";

/// Writes a layout; whether it is kept (the store traces why not).
pub type Write = crate::saving::Write<DashboardLayout>;

/// How long the note of a widget removed stays, unless it is held.
const TOAST_SHOWN_FOR: Duration = Duration::from_secs(8);
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
    /// How the layout is kept; none when there is nowhere to keep it.
    pub save: Option<Write>,
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
    /// The last cleaning the history keeps, read as the page is shown and
    /// after a cleaning of this session; none when there is none, or it
    /// cannot be read or said.
    last_cleaning: Option<LastCleaning>,
    system: Option<SystemReading>,
    volumes: Option<Result<Vec<Volume>, Unavailable>>,
    memory: Option<Result<MemoryStatus, Unavailable>>,
    /// The last list read: they are read less often than the memory.
    applications: Option<Result<Applications, Unavailable>>,
    rows: Rows,
    /// The widgets shown, in order, each at its size.
    widgets: Widgets,
    /// One row a widget, for as long as it is there: Slint keeps its
    /// element, and the focus in it.
    cells: Rc<VecModel<ui::WidgetCell>>,
    watched: BTreeSet<Source>,
    /// In the Personnaliser mode.
    editing: bool,
    /// None when the layout cannot be kept.
    saver: Option<Saver<DashboardLayout>>,
    banners: LayoutBanners,
    /// The generation of the last focus asked for.
    focus_asked: i32,
    /// The last widget removed, while "Annuler" may put it back.
    removed: Option<Removed>,
    /// Hides the note of that removal.
    toast_timer: slint::Timer,
    /// The note has the pointer or the focus: it waits.
    toast_held: bool,
    /// A widget picked up, and where it would go.
    moving: Option<Moving>,
    processor: Option<Result<ProcessorReading, Unavailable>>,
    /// The last uses, oldest first, between 0 and 1.
    uses: VecDeque<f64>,
    /// The readings no longer reach the page.
    lost: bool,
    timer: slint::Timer,
}

/// A widget picked up: from where, and where it is shown to go.
#[derive(Debug, Clone, Copy)]
struct Moving {
    from: usize,
    to: usize,
    by_pointer: bool,
}

/// What the page says of the layout kept.
#[derive(Debug, Default)]
struct LayoutBanners {
    /// The file of the layout kept, when it could not be read at launch,
    /// named for the user to repair it. The default one is shown and no
    /// change is kept this session, as it would replace that file with the
    /// layout shown (D-10); the next launch reads it again.
    unreadable: Option<PathBuf>,
    /// The last change could not be kept.
    save_failed: bool,
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
        let (cells, rows) = placed(&widgets);
        let cells = Rc::new(VecModel::from(cells));
        dashboard.set_cells(ModelRc::from(Rc::clone(&cells)));
        dashboard.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
        show_list_around(&dashboard, &widgets);
        dashboard.set_curve_span(shown(CURVE_READINGS - 1));

        let watched = sources(&widgets);
        tracing::info!(widgets = ?widgets, sources = ?watched, "dashboard attached");
        (seams.commands)(Command::Watch(watched.clone()));
        let mut seams = seams;
        let saver = seams.save.take().and_then(|write| {
            Saver::start(SAVER_NAME, write)
                .inspect_err(|error| {
                    tracing::error!(%error, "dashboard layout writer not started: changes not kept");
                })
                .ok()
        });

        let summary = read_summary(seams.state_directory.as_deref());
        let screen = Self(Rc::new(RefCell::new(State {
            window: window.as_weak(),
            seams,
            summary,
            cleaned: None,
            last_cleaning: None,
            system: None,
            volumes: None,
            memory: None,
            applications: None,
            rows: Rows::default(),
            widgets,
            cells,
            watched,
            editing: false,
            saver,
            banners: LayoutBanners {
                unreadable,
                save_failed: false,
            },
            focus_asked: 0,
            removed: None,
            toast_timer: slint::Timer::default(),
            toast_held: false,
            moving: None,
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
        screen.bind(&dashboard);
        screen.page_shown(window.get_page());
        screen
    }

    /// What a callback of one value calls: `action` on the screen, while
    /// there is one.
    fn calling<T>(&self, action: impl Fn(&Self, T) + 'static) -> impl Fn(T) + 'static {
        let weak = Rc::downgrade(&self.0);
        move |value| {
            if let Some(state) = weak.upgrade() {
                action(&Self(state), value);
            }
        }
    }

    /// Binds the page's callbacks to the screen.
    fn bind(&self, dashboard: &ui::Dashboard<'_>) {
        dashboard.on_remove(self.calling(Self::remove));
        dashboard.on_add(self.calling(|screen, kind| screen.add(catalog_kind(kind))));
        let on_call = |action: fn(&Self)| {
            let weak = Rc::downgrade(&self.0);
            move || {
                if let Some(state) = weak.upgrade() {
                    action(&Self(state));
                }
            }
        };
        dashboard.on_reset(on_call(Self::reset));
        dashboard.on_put_down(on_call(Self::put_down));
        dashboard.on_cancel_move(self.calling(|screen, why| {
            screen.cancel_by_hand(match why {
                ui::MoveCancel::Escape => "escape",
                ui::MoveCancel::FocusLost => "focus lost",
                ui::MoveCancel::Pointer => "pointer press taken away",
                ui::MoveCancel::Outside => "let go out of the list",
            });
        }));
        let weak = Rc::downgrade(&self.0);
        dashboard.on_grab(move |row, by_pointer| {
            if let Some(state) = weak.upgrade() {
                Self(state).grab(row, by_pointer);
            }
        });
        dashboard.on_move_by(self.calling(Self::move_by));
        dashboard.on_move_to_end(self.calling(Self::move_to_end));
        let weak = Rc::downgrade(&self.0);
        dashboard.on_drag_to(move |column, row, columns| {
            if let Some(state) = weak.upgrade() {
                Self(state).drag_to(f64::from(column), f64::from(row), columns);
            }
        });
        dashboard.on_undo(on_call(Self::undo));
        dashboard.on_dismiss_toast(on_call(Self::close_toast));
        dashboard.on_toast_held(self.calling(Self::hold_toast));
        dashboard.on_focus_taken(self.calling(Self::focus_taken));
        dashboard.on_customize(self.calling(Self::customize));
        let weak = Rc::downgrade(&self.0);
        dashboard.on_resize(move |row, size| {
            if let Some(state) = weak.upgrade() {
                Self(state).resize(row, store_size(size));
            }
        });
    }

    /// Enters the Personnaliser mode, or leaves it (and the note of the
    /// last removal with it, and a move under way).
    fn customize(&self, on: bool) {
        if !on {
            self.cancel_move("mode left");
        }
        {
            let mut state = self.0.borrow_mut();
            if state.editing == on {
                return;
            }
            state.editing = on;
        }
        tracing::info!(editing = on, "dashboard customizing");
        if !on {
            self.dismiss_toast();
        }
        self.show();
    }

    /// Gives the widget of `row` the size `size`.
    fn resize(&self, row: i32, size: WidgetSize) {
        if self.is_moving("resize") {
            return;
        }
        let Ok(index) = usize::try_from(row) else {
            return;
        };
        let changed = editing::resize(&mut self.0.borrow_mut().widgets, index, size);
        if changed {
            let kind = self.0.borrow().widgets[index].0;
            tracing::info!(index, ?kind, ?size, "dashboard widget resized");
            self.dismiss_toast();
            self.edited();
        }
    }

    /// Removes the widget of `row`; the focus goes to the one after it,
    /// the one before when it was the last, the mode's button when none is
    /// left.
    fn remove(&self, row: i32) {
        if self.is_moving("remove") {
            return;
        }
        let Ok(index) = usize::try_from(row) else {
            return;
        };
        let removed = {
            let mut state = self.0.borrow_mut();
            let removed = editing::remove(&mut state.widgets, index);
            if removed.is_some() {
                state.cells.remove(index);
            }
            removed
        };
        let Some(removed) = removed else {
            return;
        };
        tracing::info!(?removed, "dashboard widget removed");
        self.edited();
        self.show_toast(removed);
        let left = self.0.borrow().widgets.len();
        if left == 0 {
            self.ask_focus(0, ui::FocusPart::EmptyAdd);
        } else {
            self.ask_focus(index.min(left - 1), ui::FocusPart::Widget);
        }
    }

    /// Adds `kind` at the end, in the mode, the focus on it.
    fn add(&self, kind: WidgetKind) {
        if self.is_moving("add") {
            return;
        }
        let added = {
            let mut state = self.0.borrow_mut();
            let added = editing::add(&mut state.widgets, kind);
            if added {
                let (cells, _) = placed(&state.widgets);
                if let Some(cell) = cells.last() {
                    state.cells.push(cell.clone());
                }
            }
            added
        };
        if !added {
            return;
        }
        tracing::info!(?kind, "dashboard widget added");
        self.dismiss_toast();
        self.customize(true);
        self.edited();
        let last = self.0.borrow().widgets.len() - 1;
        self.ask_focus(last, ui::FocusPart::Widget);
    }

    /// The default dashboard again.
    fn reset(&self) {
        if self.is_moving("reset") {
            return;
        }
        let reset = {
            let mut state = self.0.borrow_mut();
            let reset = editing::reset(&mut state.widgets);
            if reset {
                let (cells, _) = placed(&state.widgets);
                state.cells.set_vec(cells);
            }
            reset
        };
        if reset {
            tracing::info!("dashboard layout reset to the default one");
            self.dismiss_toast();
            self.edited();
        }
    }

    /// Puts the widget last removed back, at its place and size.
    fn undo(&self) {
        // Asked while a widget is picked up: that one goes back first.
        self.cancel_move("undo");
        let restored = {
            let mut state = self.0.borrow_mut();
            let Some(removed) = state.removed.take() else {
                return;
            };
            state.toast_timer.stop();
            // The note goes, and what held it with it.
            state.toast_held = false;
            let restored = editing::restore(&mut state.widgets, removed);
            if restored {
                let (cells, _) = placed(&state.widgets);
                let index = removed.index.min(state.cells.row_count());
                state.cells.insert(index, cells[index].clone());
            }
            restored.then_some(removed)
        };
        let Some(removed) = restored else {
            tracing::debug!("nothing to put back: the widget came back otherwise");
            self.show();
            return;
        };
        tracing::info!(?removed, "dashboard widget put back");
        self.edited();
        let index = removed.index.min(self.0.borrow().widgets.len() - 1);
        self.ask_focus(index, ui::FocusPart::Widget);
    }

    /// Shows the note of `removed`, for a while.
    fn show_toast(&self, removed: Removed) {
        self.0.borrow_mut().removed = Some(removed);
        let held = self.0.borrow().toast_held;
        self.hold_toast(held);
        self.show();
    }

    /// The toast held stays; let go, it goes after a while.
    fn hold_toast(&self, held: bool) {
        self.0.borrow_mut().toast_held = held;
        let state = self.0.borrow();
        if held || state.removed.is_none() {
            state.toast_timer.stop();
            return;
        }
        let weak = Rc::downgrade(&self.0);
        state
            .toast_timer
            .start(slint::TimerMode::SingleShot, TOAST_SHOWN_FOR, move || {
                if let Some(state) = weak.upgrade() {
                    Self(state).dismiss_toast();
                }
            });
    }

    /// The toast closed by its button: the focus it had goes to the mode's
    /// button.
    fn close_toast(&self) {
        self.dismiss_toast();
        self.ask_focus(0, ui::FocusPart::Customize);
    }

    /// Hides the toast: "Annuler" has nothing to put back any more.
    fn dismiss_toast(&self) {
        {
            let mut state = self.0.borrow_mut();
            if state.removed.take().is_none() {
                return;
            }
            state.toast_timer.stop();
            // The note goes, and what held it with it.
            state.toast_held = false;
        }
        self.show();
    }

    /// Whether a widget is picked up: the change asked is then refused,
    /// and traced.
    fn is_moving(&self, change: &'static str) -> bool {
        let moving = self.0.borrow().moving.is_some();
        if moving {
            tracing::debug!(change, "dashboard change refused: a widget is being moved");
        }
        moving
    }

    /// Picks up the widget of `row`, by the keyboard or the pointer.
    fn grab(&self, row: i32, by_pointer: bool) {
        let Ok(from) = usize::try_from(row) else {
            return;
        };
        {
            let mut state = self.0.borrow_mut();
            let refused = if !state.editing {
                Some("not in the Personnaliser mode")
            } else if state.moving.is_some() {
                Some("a widget is being moved")
            } else if from >= state.widgets.len() {
                Some("no such widget")
            } else {
                None
            };
            if let Some(reason) = refused {
                tracing::debug!(from, reason, "dashboard widget not picked up");
                return;
            }
            state.moving = Some(Moving {
                from,
                to: from,
                by_pointer,
            });
        }
        tracing::info!(from, by_pointer, "dashboard widget picked up");
        // Its grip keeps the keys while it moves; dragged, the page does:
        // out of the list's view, the grip would lose them.
        self.ask_focus(
            from,
            if by_pointer {
                ui::FocusPart::Page
            } else {
                ui::FocusPart::Widget
            },
        );
        self.preview();
    }

    /// Shows the widget picked up `step` places further (back when below 0).
    fn move_by(&self, step: i32) {
        let target = {
            let state = self.0.borrow();
            let Some(moving) = state.moving else {
                return;
            };
            let last = state.widgets.len().saturating_sub(1);
            let to = i64::try_from(moving.to).unwrap_or(i64::MAX) + i64::from(step);
            usize::try_from(to.max(0)).unwrap_or(0).min(last)
        };
        self.show_at(target);
    }

    /// Shows the widget picked up first, or last.
    fn move_to_end(&self, last: bool) {
        let target = if last {
            self.0.borrow().widgets.len().saturating_sub(1)
        } else {
            0
        };
        self.show_at(target);
    }

    /// Shows the widget dragged where its card's centre, at `column` and
    /// `row` of a grid of `columns`, puts it.
    fn drag_to(&self, column: f64, row: f64, columns: i32) {
        let target = {
            let state = self.0.borrow();
            let Some(moving) = state.moving else {
                return;
            };
            let columns = usize::try_from(columns).unwrap_or(1).clamp(1, MAX_COLUMNS);
            editing::drop_index(&state.widgets, moving.from, (column, row), columns)
        };
        self.show_at(target);
    }

    /// Shows the widget picked up at place `to`.
    fn show_at(&self, to: usize) {
        {
            let mut state = self.0.borrow_mut();
            let Some(moving) = state.moving.as_mut() else {
                return;
            };
            if moving.to == to {
                return;
            }
            moving.to = to;
        }
        self.preview();
    }

    /// Puts the widget picked up down where it is shown: written when it
    /// moved.
    fn put_down(&self) {
        let Some(moving) = self.0.borrow_mut().moving.take() else {
            return;
        };
        let moved = {
            let mut state = self.0.borrow_mut();
            let moved = editing::move_to(&mut state.widgets, moving.from, moving.to);
            // The row itself moves; its card is drawn anew, the focus asked
            // back below.
            if moved && let Some(cell) = state.cells.row_data(moving.from) {
                state.cells.remove(moving.from);
                state.cells.insert(moving.to, cell);
            }
            moved
        };
        let kind = self
            .0
            .borrow()
            .widgets
            .get(moving.to)
            .map(|(kind, _)| *kind);
        tracing::info!(
            from = moving.from,
            to = moving.to,
            ?kind,
            "dashboard widget put down"
        );
        if moved {
            self.dismiss_toast();
            self.edited();
        } else {
            self.reposition();
            self.show();
        }
        self.ask_focus(moving.to, ui::FocusPart::Widget);
    }

    /// Puts the widget picked up back where it was (`why`): nothing
    /// written. The move given up, when there was one.
    fn cancel_move(&self, why: &'static str) -> Option<Moving> {
        let moving = self.0.borrow_mut().moving.take()?;
        tracing::info!(from = moving.from, why, "dashboard move cancelled");
        self.reposition();
        self.show();
        Some(moving)
    }

    /// The move given up from the page (`why`): a widget dragged gives the
    /// keys back to its grip.
    fn cancel_by_hand(&self, why: &'static str) {
        if let Some(moving) = self.cancel_move(why)
            && moving.by_pointer
        {
            self.ask_focus(moving.from, ui::FocusPart::Widget);
        }
    }

    /// Shows the places of the list with the widget picked up where it
    /// would go, the list itself unchanged.
    fn preview(&self) {
        {
            let state = self.0.borrow();
            let Some(moving) = state.moving else {
                return;
            };
            let preview = editing::moved(&state.widgets, moving.from, moving.to);
            let (cells, _) = placed(&preview);
            for (row, (kind, _)) in state.widgets.iter().enumerate() {
                // A widget is shown once: its kind finds it in the preview.
                let at = preview.iter().position(|(shown, _)| shown == kind);
                if let (Some(at), Some(mut cell)) = (at, state.cells.row_data(row)) {
                    cell.places = cells[at].places.clone();
                    state.cells.set_row_data(row, cell);
                }
            }
            // The tile after the list, and the rows they fill, as they
            // would be.
            if let Some(window) = state.window.upgrade() {
                show_list_around(&window.global::<ui::Dashboard>(), &preview);
            }
        }
        self.show();
    }

    /// The window took the focus asked for as `generation`: asked no more.
    fn focus_taken(&self, generation: i32) {
        let state = self.0.borrow();
        if state.focus_asked != generation {
            return;
        }
        if let Some(window) = state.window.upgrade() {
            window
                .global::<ui::Dashboard>()
                .set_focus_request(ui::FocusRequest::default());
        }
    }

    /// Asks the window to put the focus on `part` of the widget of `row`.
    fn ask_focus(&self, row: usize, part: ui::FocusPart) {
        let mut state = self.0.borrow_mut();
        state.focus_asked = state.focus_asked.wrapping_add(1).max(1);
        if let Some(window) = state.window.upgrade() {
            window
                .global::<ui::Dashboard>()
                .set_focus_request(ui::FocusRequest {
                    row: shown(row),
                    part,
                    generation: state.focus_asked,
                });
        }
    }

    /// The places of the list shown again, the sizes each widget takes
    /// kept (the same model keeps their buttons, and the focus on one).
    fn reposition(&self) {
        let state = self.0.borrow();
        let (cells, rows) = placed(&state.widgets);
        for (index, mut cell) in cells.into_iter().enumerate() {
            if let Some(shown) = state.cells.row_data(index) {
                cell.accepted = shown.accepted;
            }
            state.cells.set_row_data(index, cell);
        }
        if let Some(window) = state.window.upgrade() {
            let dashboard = window.global::<ui::Dashboard>();
            dashboard.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
            show_list_around(&dashboard, &state.widgets);
        }
    }

    /// After a change of the list: the places shown again, the sources
    /// followed again, the layout kept.
    fn edited(&self) {
        self.reposition();
        {
            let mut state = self.0.borrow_mut();
            let watched = sources(&state.widgets);
            if watched != state.watched {
                forget_newly_watched(&mut state, &watched);
                (state.seams.commands)(Command::Watch(watched.clone()));
                state.watched = watched;
            }
            let layout = layout_of(&state.widgets);
            if state.banners.unreadable.is_some() {
                tracing::warn!(
                    "dashboard layout not kept: the one there, unreadable, would be replaced"
                );
            } else {
                match state.saver.as_ref().map(|saver| saver.save(layout)) {
                    Some(true) => {}
                    Some(false) => {
                        tracing::error!("dashboard layout writer gone: changes no longer kept");
                        state.saver = None;
                        state.banners.save_failed = true;
                    }
                    None => {
                        tracing::warn!("dashboard layout not kept: no writer");
                        state.banners.save_failed = true;
                    }
                }
            }
        }
        self.show();
    }

    /// Lets the last layout handed be written, as the window closes.
    pub fn shut_down(&self) {
        if let Some(saver) = self.0.borrow_mut().saver.as_mut() {
            saver.shut_down();
        }
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
            self.read_last_cleaning();
            self.show();
        } else {
            tracing::info!(?page, "dashboard hidden: readings paused");
            (self.0.borrow().seams.commands)(Command::Pause);
            self.customize(false);
            self.dismiss_toast();
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
        // Its line is in the history by now: the next analysis must not
        // bring back the one before as the last.
        self.read_last_cleaning();
        self.show();
    }

    /// Reads the last cleaning the history keeps, whoever made it (this
    /// app or the CLI): one made while the page stays shown is seen the
    /// next time it is shown.
    fn read_last_cleaning(&self) {
        let mut state = self.0.borrow_mut();
        state.last_cleaning = state
            .seams
            .state_directory
            .as_deref()
            .and_then(last_cleaning);
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
            let before = state.banners.save_failed;
            match state.saver.as_ref().map(Saver::last_write) {
                None | Some(LastWrite::Nothing) => {}
                Some(LastWrite::Kept(kept)) => {
                    state.banners.save_failed = !kept;
                    if !kept {
                        tracing::warn!("dashboard layout not kept: said");
                    }
                }
                Some(LastWrite::Gone) => {
                    tracing::error!("dashboard layout writer gone: changes no longer kept");
                    state.saver = None;
                    state.banners.save_failed = true;
                }
            }
            changed |= state.banners.save_failed != before;
            // Cut off, nothing comes any more; the clock still turns.
            while !state.lost {
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
            state.last_cleaning,
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
        dashboard.set_editing(state.editing);
        dashboard.set_moving(
            state
                .moving
                .map_or_else(ui::Moving::default, |moving| ui::Moving {
                    shown: true,
                    row: shown(moving.from),
                    to: shown(moving.to),
                    by_pointer: moving.by_pointer,
                }),
        );
        dashboard.set_save_failed(state.banners.save_failed);
        dashboard.set_unreadable_layout(
            state
                .banners
                .unreadable
                .as_deref()
                .map(|path| path.display().to_string().into())
                .unwrap_or_default(),
        );
        dashboard.set_toast(ui::Toast {
            shown: state.removed.is_some(),
            kind: state
                .removed
                .map_or(ui::WidgetKind::Reclaimable, |removed| {
                    widget_kind(removed.kind)
                }),
        });
    }
}

/// What `widgets` need read; the system always, for the header.
fn sources(widgets: &[(WidgetKind, WidgetSize)]) -> BTreeSet<Source> {
    let mut watched: BTreeSet<Source> = widgets
        .iter()
        .flat_map(|(kind, size)| kind.sources(*size))
        .collect();
    watched.insert(Source::System);
    watched
}

/// Forgets what was read of the sources followed again: a reading from
/// before would be shown as now.
fn forget_newly_watched(state: &mut State, watched: &BTreeSet<Source>) {
    for source in watched.difference(&state.watched) {
        match source {
            Source::Volumes => state.volumes = None,
            Source::Memory => state.memory = None,
            Source::Applications => state.applications = None,
            Source::Processor => {
                state.processor = None;
                state.uses.clear();
            }
            Source::System => state.system = None,
        }
    }
}

/// What goes with the list in the mode: the tile after it, the widgets it
/// lacks.
fn show_list_around(dashboard: &ui::Dashboard<'_>, widgets: &[(WidgetKind, WidgetSize)]) {
    let (tile, rows) = tile_placed(widgets);
    dashboard.set_tile(ModelRc::from(Rc::new(VecModel::from(tile))));
    dashboard.set_rows_with_tile(ModelRc::from(Rc::new(VecModel::from(rows))));
    dashboard.set_addable(ModelRc::from(Rc::new(VecModel::from(addable(widgets)))));
}

/// `widgets` as the store keeps them.
fn layout_of(widgets: &[(WidgetKind, WidgetSize)]) -> DashboardLayout {
    DashboardLayout {
        widgets: widgets
            .iter()
            .map(|(kind, size)| DashboardWidget {
                kind: kind.name().to_owned(),
                size: *size,
            })
            .collect(),
    }
}

/// The zone of this machine; unknown, UTC, said.
pub fn system_zone() -> TimeZone {
    TimeZone::try_system().unwrap_or_else(|error| {
        tracing::warn!(%error, "time zone unknown: instants said in UTC");
        TimeZone::UTC
    })
}

/// The widgets of the layout kept in `directory`, and its file when it could
/// not be read (the default one then shown).
fn read_layout(directory: Option<&Path>) -> (Vec<(WidgetKind, WidgetSize)>, Option<PathBuf>) {
    let Some(directory) = directory else {
        tracing::warn!("no configuration directory: the default dashboard is shown");
        return (widgets_shown(None), None);
    };
    match DashboardLayout::load(directory) {
        Ok(loaded) => (widgets_shown(loaded.as_ref()), None),
        Err(error) => {
            let path = DashboardLayout::path(directory);
            tracing::warn!(%error, path = %path.display(), "dashboard layout unreadable: the default one is shown, no change kept");
            (widgets_shown(None), Some(path))
        }
    }
}

/// The last cleaning the history of `directory` keeps; none, traced, when
/// it cannot be read or is dated out of any date.
fn last_cleaning(directory: &Path) -> Option<LastCleaning> {
    match History::last_cleaning(directory) {
        // Dropped here, said once: kept, `moment` would say it at every
        // reading shown.
        Ok(Some(last)) if Timestamp::from_second(last.time).is_err() => {
            tracing::warn!(?last, "last cleaning dated out of any date");
            None
        }
        Ok(last) => last,
        Err(error) => {
            tracing::warn!(%error, "last cleaning unknown");
            None
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

fn cell_place(cell: &layout::Cell) -> ui::CellPlace {
    ui::CellPlace {
        column: shown(cell.column),
        row: shown(cell.row),
        width: shown(cell.width),
        height: shown(cell.height),
    }
}

/// Where the tile "Ajouter un widget" sits after `widgets` (a small one
/// at their end) in 1 to [`MAX_COLUMNS`] columns, and the rows filled with
/// it.
fn tile_placed(widgets: &[(WidgetKind, WidgetSize)]) -> (Vec<ui::CellPlace>, Vec<i32>) {
    let mut sizes: Vec<WidgetSize> = widgets.iter().map(|(_, size)| *size).collect();
    sizes.push(WidgetSize::Small);
    (1..=MAX_COLUMNS)
        .map(|columns| {
            let (cells, rows) = place(&sizes, columns);
            let tile = cells.last().map(cell_place).unwrap_or_default();
            (tile, shown(rows))
        })
        .unzip()
}

/// The widgets that can be added to `widgets`, at the size they come at.
fn addable(widgets: &[(WidgetKind, WidgetSize)]) -> Vec<ui::AddableWidget> {
    WidgetKind::ALL
        .into_iter()
        .filter(|kind| widgets.iter().all(|(shown, _)| shown != kind))
        .map(|kind| ui::AddableWidget {
            kind: widget_kind(kind),
            size: widget_size(catalog::starting_size(kind)),
        })
        .collect()
}

/// The cells of `widgets`, each with its places in 1 to [`MAX_COLUMNS`]
/// columns, and the rows they fill in each.
fn placed(widgets: &[(WidgetKind, WidgetSize)]) -> (Vec<ui::WidgetCell>, Vec<i32>) {
    let sizes: Vec<WidgetSize> = widgets.iter().map(|(_, size)| *size).collect();
    let grids: Vec<(Vec<layout::Cell>, usize)> = (1..=MAX_COLUMNS)
        .map(|columns| place(&sizes, columns))
        .collect();
    let cells = widgets
        .iter()
        .enumerate()
        .map(|(index, (kind, size))| {
            let places: Vec<ui::CellPlace> = grids
                .iter()
                .map(|(cells, _)| cell_place(&cells[index]))
                .collect();
            let accepted: Vec<ui::WidgetSize> = WidgetSize::ALL
                .into_iter()
                .filter(|accepted| kind.accepts(*accepted))
                .map(widget_size)
                .collect();
            ui::WidgetCell {
                kind: widget_kind(*kind),
                size: widget_size(*size),
                accepted: ModelRc::from(Rc::new(VecModel::from(accepted))),
                places: ModelRc::from(Rc::new(VecModel::from(places))),
            }
        })
        .collect();
    let rows = grids.iter().map(|(_, rows)| shown(*rows)).collect();
    (cells, rows)
}

const fn catalog_kind(kind: ui::WidgetKind) -> WidgetKind {
    match kind {
        ui::WidgetKind::Reclaimable => WidgetKind::Reclaimable,
        ui::WidgetKind::Disks => WidgetKind::Disks,
        ui::WidgetKind::Memory => WidgetKind::Memory,
        ui::WidgetKind::Processor => WidgetKind::Processor,
        ui::WidgetKind::System => WidgetKind::System,
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

const fn store_size(size: ui::WidgetSize) -> WidgetSize {
    match size {
        ui::WidgetSize::Small => WidgetSize::Small,
        ui::WidgetSize::Wide => WidgetSize::Wide,
        ui::WidgetSize::Tall => WidgetSize::Tall,
        ui::WidgetSize::Large => WidgetSize::Large,
        ui::WidgetSize::Full => WidgetSize::Full,
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
    let (day, day_of_month, month, year) = match moment.day {
        Day::Today => (ui::DayKind::Today, 0, 0, None),
        Day::Yesterday => (ui::DayKind::Yesterday, 0, 0, None),
        Day::On { day, month, year } => (ui::DayKind::Other, day, month, year),
    };
    Some(ui::Moment {
        day,
        day_of_month: day_of_month.into(),
        month: month.into(),
        year: year.map_or(0, i32::from),
        hour: moment.hour.into(),
        minute: moment.minute.into(),
    })
}

/// What Récupérable says of `summary` at `now` in `zone`: a cleaning of
/// this session first, then the summary, outdated when the history says a
/// cleaning came since, with the last cleaning the history keeps unless one
/// of this session says it.
fn reclaimable(
    summary: Summary,
    cleaned: Option<Cleaned>,
    last_cleaning: Option<LastCleaning>,
    now: Timestamp,
    zone: &TimeZone,
) -> ui::Reclaimable {
    let mut shown_state = ui::Reclaimable::default();
    if let Some(cleaned) = cleaned {
        shown_state.state = ui::ReclaimableState::Cleaned;
        let freed = said_freed(cleaned.freed);
        shown_state.freed_known = freed.is_some();
        shown_state.freed = size(freed.unwrap_or_default());
        if let Some(at) = cleaned.at.and_then(|at| moment(at, now, zone)) {
            shown_state.cleaned_at_known = true;
            shown_state.cleaned_at = at;
        }
        // The last cleaning is said already.
        return shown_state;
    }
    if let Some(last) = last_cleaning
        && let Some(at) = moment(last.time, now, zone)
    {
        let freed = said_freed(last.freed);
        shown_state.last_cleaning_known = true;
        shown_state.last_cleaning_at = at;
        shown_state.last_cleaning_freed_known = freed.is_some();
        shown_state.last_cleaning_freed = size(freed.unwrap_or_default());
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

/// What a cleaning that freed `freed` is said to have freed: no amount when
/// nothing it knows of was freed.
fn said_freed(freed: Option<u64>) -> Option<u64> {
    freed.filter(|bytes| *bytes > 0)
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
    use okapi_store::{AdminRecord, DashboardWidget, HistoryEntry};
    use okapi_tools::monitor::Space;

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
            save: None,
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
                    save: Some({
                        let config = self.config.clone();
                        Box::new(move |layout: &DashboardLayout| layout.save(&config).is_ok())
                    }),
                    zone: TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap(),
                    now: fixed_now,
                    ..seams
                },
                sampler,
            )
        }

        /// A history last written at `utc`.
        fn history_at(&self, utc: &str) {
            fs::write(History::path(&self.state), "").unwrap();
            written_at(&self.state, utc);
        }
    }

    fn on_home(seams: DashboardSeams) -> ui::AppWindow {
        on_home_with(seams_never_scanning(), seams)
    }

    /// The window on the Home page, and the page itself.
    fn home_screen(seams: DashboardSeams) -> (ui::AppWindow, DashboardScreen) {
        let (window, _cleaner, home) =
            crate::wide_window(seams_never_scanning(), seams, Language::French);
        (window, home)
    }

    /// The window on the Home page at the mockup's size (three columns,
    /// every widget in view), its cleaner working through `cleaner`.
    fn on_home_with(cleaner: Seams, seams: DashboardSeams) -> ui::AppWindow {
        let (window, _cleaner, _home) = crate::wide_window(cleaner, seams, Language::French);
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
        assert!(!shows(&window, &unreadable(&directories.config)));
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
        // 960 px: the sidebar folded, a margin of 24 px, two columns, the
        // memory under the disks.
        window.window().set_size(slint::PhysicalSize::new(960, 640));
        slint::platform::update_timers_and_animations();
        let (reclaimable_x, ..) = frame(&window, "Récupérable");
        let (disks_x, disks_y, disks_width, _) = frame(&window, "Disques");
        let (memory_x, memory_y, ..) = frame(&window, "Mémoire");
        assert!(
            (reclaimable_x - (64.0 + 24.0)).abs() < 1.0,
            "{reclaimable_x}"
        );
        assert!((disks_width - (960.0 - 64.0 - 2.0 * 24.0)).abs() < 1.0);
        assert!((memory_x - disks_x).abs() < 1.0);
        assert!((memory_y - (disks_y + 176.0 + gap)).abs() < 1.0);
        // 1280 px, the sidebar folded by choice: the window is not narrow,
        // the margin stays 32 px.
        window
            .window()
            .set_size(slint::PhysicalSize::new(1280, 800));
        window.set_sidebar_chosen_folded(true);
        slint::platform::update_timers_and_animations();
        let (reclaimable_x, _, reclaimable_width, _) = frame(&window, "Récupérable");
        assert!(
            (reclaimable_x - (64.0 + 32.0)).abs() < 1.0,
            "{reclaimable_x}"
        );
        assert!((reclaimable_width - (1280.0 - 64.0 - 2.0 * 32.0)).abs() < 1.0);
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

    /// The banner of a layout kept in `directory` that cannot be read.
    fn unreadable(directory: &Path) -> String {
        format!(
            "Votre disposition n'a pas pu être lue dans {} : la disposition par défaut est affichée, et vos changements ne sont pas enregistrés pour ne pas écraser ce fichier.",
            DashboardLayout::path(directory).display()
        )
    }

    #[test]
    fn an_unreadable_layout_says_so_and_shows_the_default_one() {
        let directories = Directories::new();
        fs::write(DashboardLayout::path(&directories.config), "{").unwrap();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert!(shows(&window, &unreadable(&directories.config)));
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
    fn an_estimate_of_another_year_says_its_year() {
        estimate_dated(
            "2025-12-30T08:05:00Z",
            "Estimation du 30 décembre 2025 à 9 h 05",
            "le 30 décembre 2025 à 9 h 05",
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
                None,
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
        cleaned_at(&directories.state, "2026-03-10T08:05:00Z", 7, None);
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
        assert!(shows(
            &window,
            "Dernier nettoyage aujourd'hui à 9 h 05 : 7 o libérés"
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
        assert!(!says_a_last_cleaning(&window), "said already");
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
        // Read back from the history this cleaning wrote.
        assert!(shows_a_text_starting(
            &window,
            "Dernier nettoyage aujourd'hui à "
        ));
    }

    /// The history of `state` said last written at `utc`.
    fn written_at(state: &Path, utc: &str) {
        let written = SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(at(utc)).unwrap());
        fs::File::options()
            .write(true)
            .open(History::path(state))
            .unwrap()
            .set_modified(written)
            .unwrap();
    }

    /// A cleaning added at `utc` to the history of `state`, then said last
    /// written then, which freed `bytes`, root's part done or not when it
    /// had one.
    fn cleaned_at(state: &Path, utc: &str, bytes: u64, root_done: Option<bool>) {
        recorded_at(state, at(utc), bytes, root_done);
        written_at(state, utc);
    }

    /// A cleaning added to the history of `state`, made `time` seconds after
    /// the clock's origin.
    fn recorded_at(state: &Path, time: i64, bytes: u64, root_done: Option<bool>) {
        History::open(state)
            .unwrap()
            .record(&HistoryEntry {
                time,
                stops: Vec::new(),
                removed_bytes: bytes,
                removed: 1,
                partial: 0,
                gone: 0,
                kept: 0,
                complete: true,
                admin: root_done.map(|done| AdminRecord { done, reason: None }),
                entries: None,
            })
            .unwrap();
    }

    fn says_a_last_cleaning(window: &ui::AppWindow) -> bool {
        shows_a_text_starting(window, "Dernier nettoyage")
    }

    #[test]
    fn reclaimable_over_every_column_says_the_last_cleaning_of_the_history() {
        let directories = Directories::new();
        cleaned_at(
            &directories.state,
            "2026-03-08T10:00:00Z",
            2_500_000_000,
            None,
        );
        cleaned_at(
            &directories.state,
            "2026-03-10T08:05:00Z",
            1_800_000_000,
            Some(true),
        );
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert!(shows(
            &window,
            "Dernier nettoyage aujourd'hui à 9 h 05 : 1,8 Go libérés"
        ));
    }

    /// Whether a last cleaning that freed `bytes`, root's part done or not,
    /// is said without an amount.
    fn says_no_amount(bytes: u64, root_done: Option<bool>) -> bool {
        let directories = Directories::new();
        cleaned_at(&directories.state, "2026-03-08T10:00:00Z", bytes, root_done);
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        shows(&window, "Dernier nettoyage le 8 mars à 11 h.")
    }

    #[test]
    fn a_last_cleaning_that_freed_nothing_says_no_amount() {
        assert!(says_no_amount(0, None));
    }

    #[test]
    fn a_last_cleaning_whose_root_part_did_not_end_says_no_amount() {
        assert!(says_no_amount(5_000_000_000, Some(false)));
    }

    #[test]
    fn reclaimable_on_part_of_the_columns_has_no_room_for_the_last_cleaning() {
        let directories = Directories::new();
        kept(&directories.config, &[("reclaimable", WidgetSize::Wide)]);
        cleaned_at(&directories.state, "2026-03-10T08:05:00Z", 7, None);
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        widget(&window, "Récupérable");
        assert!(last_cleaning(&directories.state).is_some(), "one to say");
        assert!(!says_a_last_cleaning(&window));
    }

    #[test]
    fn a_last_cleaning_dated_out_of_any_date_is_dropped_as_read() {
        let directories = Directories::new();
        recorded_at(&directories.state, i64::MAX, 7, None);
        assert_eq!(last_cleaning(&directories.state), None);
    }

    #[test]
    fn an_unavailable_reclaimable_says_no_last_cleaning() {
        let directories = Directories::new();
        cleaned_at(&directories.state, "2026-03-10T08:05:00Z", 7, None);
        fs::write(LastAnalysis::path(&directories.state), "{").unwrap();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert_eq!(
            said(&window, "Récupérable").as_deref(),
            Some("Indisponible")
        );
        assert!(!says_a_last_cleaning(&window));
    }

    #[test]
    fn a_last_line_spoiled_since_takes_the_last_cleaning_away() {
        use std::io::Write as _;
        let directories = Directories::new();
        cleaned_at(&directories.state, "2026-03-10T08:05:00Z", 7, None);
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert!(says_a_last_cleaning(&window));
        // A cleaning of the CLI cut short while writing its line.
        fs::OpenOptions::new()
            .append(true)
            .open(History::path(&directories.state))
            .unwrap()
            .write_all(b"{\"version\":1,\"tim")
            .unwrap();
        go_to(&window, "Nettoyeur");
        go_to(&window, "Accueil");
        assert!(!says_a_last_cleaning(&window), "never the one before");
    }

    #[test]
    fn a_cleaning_of_the_session_takes_the_last_cleaning_s_place() {
        let directories = Directories::new();
        cleaned_at(&directories.state, "2026-03-08T10:00:00Z", 7, None);
        let (seams, _sampler) = directories.seams();
        let (window, home) = home_screen(seams);
        assert!(says_a_last_cleaning(&window));
        // Its line written first, as the cleaner does.
        cleaned_at(&directories.state, "2026-03-10T13:00:00Z", 9, None);
        home.cleaning_done(Some(9), at("2026-03-10T13:00:00Z"));
        assert!(shows(&window, "9 o libérés"));
        assert!(!says_a_last_cleaning(&window));
        // An analysis while the page stays shown: this cleaning is the last.
        home.analysis_done(summary_at("2026-03-10T13:10:00Z"));
        assert!(shows(
            &window,
            "Dernier nettoyage aujourd'hui à 14 h : 9 o libérés"
        ));
    }

    #[test]
    fn an_estimate_a_later_cleaning_outdated_says_that_cleaning() {
        let directories = Directories::new();
        summary_at("2026-03-08T09:00:00Z")
            .save(&directories.state)
            .unwrap();
        cleaned_at(&directories.state, "2026-03-09T10:00:00Z", 7, None);
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert!(shows(&window, "Estimation à refaire"));
        assert!(shows(
            &window,
            "Dernier nettoyage hier à 11 h : 7 o libérés"
        ));
    }

    #[test]
    fn the_history_is_read_again_each_time_home_is_shown() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert!(!says_a_last_cleaning(&window), "no history");
        // Cleaned by the CLI meanwhile.
        cleaned_at(&directories.state, "2026-03-10T08:05:00Z", 7, None);
        go_to(&window, "Nettoyeur");
        go_to(&window, "Accueil");
        assert!(shows(
            &window,
            "Dernier nettoyage aujourd'hui à 9 h 05 : 7 o libérés"
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

    /// Lets the page take in what its threads said, until `done`.
    /// Waits, the page's clock stopped, until `done` (a file written by
    /// its thread): the note of a removal stays.
    fn until(window: &ui::AppWindow, done: impl Fn(&ui::AppWindow) -> bool) {
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(10) {
            slint::platform::update_timers_and_animations();
            if done(window) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the page never got there");
    }

    /// Lets the page take in what its threads said, its clock going on,
    /// until `done`.
    fn until_said(window: &ui::AppWindow, done: impl Fn(&ui::AppWindow) -> bool) {
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(10) {
            take_in();
            if done(window) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the page never said it");
    }

    fn kept_layout(directory: &Path) -> Vec<(String, WidgetSize)> {
        DashboardLayout::load(directory)
            .unwrap()
            .map(|loaded| {
                loaded
                    .layout
                    .widgets
                    .into_iter()
                    .map(|widget| (widget.kind, widget.size))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn customizing(window: &ui::AppWindow) {
        button(window, "Personnaliser").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
    }

    #[test]
    fn the_mode_shows_each_widget_s_bar_and_holds_its_actions() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Retirer Disques")
                .next()
                .is_none()
        );
        customizing(&window);
        assert!(window.global::<ui::Dashboard>().get_editing());
        button(&window, "Retirer Disques");
        assert!(shows(&window, "Taille unique"), "the system's");
        assert_eq!(
            button(&window, "Analyser").accessible_enabled(),
            Some(false)
        );
        button(&window, "Terminé").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(!window.global::<ui::Dashboard>().get_editing());
        button(&window, "Personnaliser");
        assert_eq!(button(&window, "Analyser").accessible_enabled(), Some(true));
    }

    #[test]
    fn a_size_chosen_is_drawn_kept_and_read_for() {
        let directories = Directories::new();
        kept(&directories.config, &[("memory", WidgetSize::Small)]);
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        let large = button(&window, "Grand");
        assert_eq!(large.accessible_checked(), Some(false));
        large.invoke_accessible_default_action();
        until(&window, |_| {
            kept_layout(&directories.config) == [("memory".to_owned(), WidgetSize::Large)]
        });
        assert_eq!(button(&window, "Grand").accessible_checked(), Some(true));
        assert_eq!(button(&window, "Petit").accessible_checked(), Some(false));
        assert!(
            (frame(&window, "Mémoire").3 - (2.0 * 176.0 + 16.0)).abs() < 1.0,
            "two rows"
        );
        assert_eq!(
            sampler.commands.borrow().last(),
            Some(&Command::Watch(BTreeSet::from([
                Source::Memory,
                Source::Applications,
                Source::System,
            ])))
        );
    }

    #[test]
    fn a_widget_removed_is_gone_and_the_focus_goes_to_the_next_one() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        button(&window, "Retirer Disques").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Disques")
                .all(|element| element.accessible_role() != Some(AccessibleRole::Groupbox))
        );
        // The focus on the next one's grip: Suppr removes it.
        crate::cleaner::tests::press(&window, slint::platform::Key::Delete);
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            kept_layout(&directories.config)
                == [
                    ("reclaimable".to_owned(), WidgetSize::Full),
                    ("processor".to_owned(), WidgetSize::Wide),
                    ("system".to_owned(), WidgetSize::Small),
                ]
        });
    }

    #[test]
    fn the_last_widget_removed_leaves_the_focus_on_the_empty_dashboard_s_add() {
        let directories = Directories::new();
        kept(&directories.config, &[("system", WidgetSize::Small)]);
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        button(&window, "Retirer Système").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(shows(&window, "Votre tableau de bord est vide"));
        crate::cleaner::tests::press(&window, slint::platform::Key::Return);
        slint::platform::update_timers_and_animations();
        button(&window, "Ajouter Système");
    }

    #[test]
    fn a_layout_that_cannot_be_kept_says_so() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let not_a_directory = directories.state.join("a-file");
        fs::write(&not_a_directory, "").unwrap();
        let window = on_home(DashboardSeams {
            save: Some(Box::new(move |layout: &DashboardLayout| {
                layout.save(&not_a_directory).is_ok()
            })),
            ..seams
        });
        customizing(&window);
        button(&window, "Retirer Disques").invoke_accessible_default_action();
        until_said(&window, |window| shows(window, NOT_KEPT));
    }

    #[test]
    fn with_nowhere_to_keep_it_a_change_says_so_at_once() {
        let (seams, _sampler) = noting(None);
        let window = on_home(seams);
        assert!(!shows(&window, NOT_KEPT));
        customizing(&window);
        button(&window, "Retirer Disques").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(shows(&window, NOT_KEPT));
    }

    #[test]
    fn leaving_the_home_page_leaves_the_mode() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        go_to(&window, "À propos");
        go_to(&window, "Accueil");
        assert!(!window.global::<ui::Dashboard>().get_editing());
        button(&window, "Personnaliser");
    }

    fn kinds_kept(directory: &Path) -> Vec<String> {
        kept_layout(directory)
            .into_iter()
            .map(|(kind, _)| kind)
            .collect()
    }

    fn removing_disks(directories: &Directories) -> ui::AppWindow {
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        button(&window, "Retirer Disques").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            !kinds_kept(&directories.config).contains(&"disks".to_owned())
        });
        window
    }

    #[test]
    fn a_widget_added_goes_at_the_end_with_the_focus_on_it() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        button(&window, "Ajouter un widget").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(shows(&window, "L'espace libre de chaque volume monté."));
        assert!(shows(&window, "Taille de départ : Large"));
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Ajouter Mémoire")
                .next()
                .is_none(),
            "only the widgets the dashboard lacks"
        );
        button(&window, "Ajouter Disques").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            kinds_kept(&directories.config).last() == Some(&"disks".to_owned())
        });
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Ajouter Disques")
                .next()
                .is_none(),
            "the dialog closed"
        );
        assert!(
            !shows(&window, "Disques retiré du tableau de bord"),
            "nothing to put back"
        );
        // The focus on the widget added: Suppr removes it again.
        crate::cleaner::tests::press(&window, slint::platform::Key::Delete);
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            !kinds_kept(&directories.config).contains(&"disks".to_owned())
        });
    }

    #[test]
    fn with_every_widget_there_the_dialog_says_none_is_left_and_closes_on_escape() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        button(&window, "Ajouter un widget").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(shows(
            &window,
            "Tous les widgets sont déjà sur votre tableau de bord."
        ));
        crate::cleaner::tests::press(&window, slint::platform::Key::Escape);
        slint::platform::update_timers_and_animations();
        assert!(!shows(
            &window,
            "Tous les widgets sont déjà sur votre tableau de bord."
        ));
    }

    #[test]
    fn the_default_layout_comes_back_once_confirmed() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        // The note's "Annuler" gone, the one left is the dialog's.
        button(&window, "Fermer la notification").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        button(&window, "Rétablir la disposition par défaut").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(shows(&window, "Rétablir la disposition par défaut ?"));
        button(&window, "Annuler").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(!shows(&window, "Rétablir la disposition par défaut ?"));
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Disques")
                .all(|element| element.accessible_role() != Some(AccessibleRole::Groupbox)),
            "nothing restored"
        );
        button(&window, "Rétablir la disposition par défaut").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        button(&window, "Rétablir").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            kept_layout(&directories.config)
                == catalog::default_widgets()
                    .iter()
                    .map(|(kind, size)| (kind.name().to_owned(), *size))
                    .collect::<Vec<_>>()
        });
        assert!(
            !shows(&window, "Rétablir la disposition par défaut ?"),
            "closed"
        );
    }

    #[test]
    fn the_confirmation_names_the_default_widgets_in_their_order() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        button(&window, "Rétablir la disposition par défaut").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        // Its words list the default layout: they follow it.
        assert!(shows(
            &window,
            "Votre disposition actuelle sera remplacée par celle de départ : Récupérable, Disques, Mémoire, Processeur et Système, chacun à sa taille d'origine."
        ));
        assert_eq!(
            catalog::default_widgets()
                .iter()
                .map(|(kind, _)| kind.name())
                .collect::<Vec<_>>(),
            ["reclaimable", "disks", "memory", "processor", "system"]
        );
    }

    #[test]
    fn a_widget_removed_comes_back_to_its_place_with_annuler() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        assert!(shows(&window, "Disques retiré du tableau de bord"));
        button(&window, "Annuler").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            kinds_kept(&directories.config).get(1) == Some(&"disks".to_owned())
        });
        assert!(!shows(&window, "Disques retiré du tableau de bord"));
    }

    #[test]
    fn control_z_puts_the_widget_back_while_the_note_shows() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        let key = |text: &str, pressed: bool| {
            let text = slint::SharedString::from(text);
            window.window().dispatch_event(if pressed {
                slint::platform::WindowEvent::KeyPressed { text }
            } else {
                slint::platform::WindowEvent::KeyReleased { text }
            });
        };
        let control = slint::SharedString::from(slint::platform::Key::Control);
        // Caps Lock on: the key reads "Z" without Shift.
        key(&control, true);
        key("Z", true);
        key("Z", false);
        key(&control, false);
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            kinds_kept(&directories.config).get(1) == Some(&"disks".to_owned())
        });
    }

    #[test]
    fn the_note_goes_by_itself() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        i_slint_backend_testing::mock_elapsed_time(
            TOAST_SHOWN_FOR.saturating_sub(Duration::from_secs(1)),
        );
        slint::platform::update_timers_and_animations();
        assert!(shows(&window, "Disques retiré du tableau de bord"));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(1));
        slint::platform::update_timers_and_animations();
        assert!(!shows(&window, "Disques retiré du tableau de bord"));
    }

    #[test]
    fn an_emptied_dashboard_offers_to_add_a_widget() {
        let directories = Directories::new();
        kept(&directories.config, &[]);
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        button(&window, "Ajouter un widget").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        button(&window, "Ajouter Système").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(
            window.global::<ui::Dashboard>().get_editing(),
            "into the mode"
        );
        until(&window, |_| {
            kinds_kept(&directories.config) == ["system".to_owned()]
        });
    }

    #[test]
    fn a_click_outside_a_dialog_never_closes_it() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        let click = |x: f32, y: f32| {
            let position = slint::LogicalPosition::new(x, y);
            let button = slint::platform::PointerEventButton::Left;
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
            slint::platform::update_timers_and_animations();
        };
        button(&window, "Ajouter un widget").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        click(10.0, 790.0);
        click(700.0, 700.0);
        assert!(shows(
            &window,
            "Tous les widgets sont déjà sur votre tableau de bord."
        ));
        button(&window, "Fermer").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(!shows(
            &window,
            "Tous les widgets sont déjà sur votre tableau de bord."
        ));
        button(&window, "Rétablir la disposition par défaut").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        click(10.0, 790.0);
        assert!(shows(&window, "Rétablir la disposition par défaut ?"));
        crate::cleaner::tests::press(&window, slint::platform::Key::Escape);
        slint::platform::update_timers_and_animations();
        assert!(!shows(&window, "Rétablir la disposition par défaut ?"));
    }

    #[test]
    fn delete_removes_the_widget_the_focus_is_in() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        // The focus went to Mémoire's bar: Suppr removes it.
        crate::cleaner::tests::press(&window, slint::platform::Key::Delete);
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            !kinds_kept(&directories.config).contains(&"memory".to_owned())
        });
        assert!(shows(&window, "Mémoire retiré du tableau de bord"));
    }

    #[test]
    fn the_last_of_the_list_removed_leaves_the_focus_on_the_one_before() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        button(&window, "Retirer Système").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        crate::cleaner::tests::press(&window, slint::platform::Key::Delete);
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            kinds_kept(&directories.config) == ["reclaimable", "disks", "memory"]
        });
    }

    /// The banner of a change not kept.
    const NOT_KEPT: &str = "Votre disposition n'a pas pu être enregistrée.";

    #[test]
    fn an_unreadable_layout_is_never_replaced_even_once_repaired() {
        let directories = Directories::new();
        let path = DashboardLayout::path(&directories.config);
        fs::write(&path, "{").unwrap();
        let (seams, _sampler) = directories.seams();
        let (window, home) = home_screen(seams);
        assert!(shows(&window, &unreadable(&directories.config)));
        // Repaired by the user while the page shows the default one: a
        // change of that default must not replace it.
        let repaired = r#"{"version": 1, "widgets": [{"kind": "memory", "size": "small"}]}"#;
        fs::write(&path, repaired).unwrap();
        customizing(&window);
        // Two changes: not one of them is kept.
        button(&window, "Petit").invoke_accessible_default_action();
        button(&window, "Large").invoke_accessible_default_action();
        home.shut_down();
        assert_eq!(fs::read_to_string(&path).unwrap(), repaired);
        assert!(shows(&window, &unreadable(&directories.config)));
        assert!(!shows(&window, NOT_KEPT), "the one banner says it");
    }

    #[test]
    fn a_layout_kept_after_a_failure_takes_the_banner_away() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let config = directories.config.clone();
        let fails = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let failing = std::sync::Arc::clone(&fails);
        let window = on_home(DashboardSeams {
            save: Some(Box::new(move |layout: &DashboardLayout| {
                !failing.load(std::sync::atomic::Ordering::SeqCst) && layout.save(&config).is_ok()
            })),
            ..seams
        });
        let failed = NOT_KEPT;
        customizing(&window);
        button(&window, "Retirer Disques").invoke_accessible_default_action();
        until_said(&window, |window| shows(window, failed));
        fails.store(false, std::sync::atomic::Ordering::SeqCst);
        button(&window, "Retirer Mémoire").invoke_accessible_default_action();
        until_said(&window, |window| !shows(window, failed));
    }

    #[test]
    fn the_note_stays_while_the_pointer_holds_it() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        let note =
            ElementHandle::find_by_accessible_label(&window, "Disques retiré du tableau de bord")
                .next()
                .unwrap();
        let (position, size) = (note.absolute_position(), note.size());
        let over = slint::LogicalPosition::new(position.x + size.width / 2.0, position.y + 4.0);
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: over });
        slint::platform::update_timers_and_animations();
        i_slint_backend_testing::mock_elapsed_time(TOAST_SHOWN_FOR * 2);
        slint::platform::update_timers_and_animations();
        assert!(shows(&window, "Disques retiré du tableau de bord"), "held");
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(10.0, 10.0),
            });
        slint::platform::update_timers_and_animations();
        i_slint_backend_testing::mock_elapsed_time(TOAST_SHOWN_FOR);
        slint::platform::update_timers_and_animations();
        assert!(
            !shows(&window, "Disques retiré du tableau de bord"),
            "let go"
        );
    }

    #[test]
    fn control_z_does_nothing_once_the_note_is_gone() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        i_slint_backend_testing::mock_elapsed_time(TOAST_SHOWN_FOR);
        slint::platform::update_timers_and_animations();
        let control = slint::SharedString::from(slint::platform::Key::Control);
        for (text, pressed) in [
            (control.clone(), true),
            ("z".into(), true),
            ("z".into(), false),
            (control, false),
        ] {
            window.window().dispatch_event(if pressed {
                slint::platform::WindowEvent::KeyPressed { text }
            } else {
                slint::platform::WindowEvent::KeyReleased { text }
            });
        }
        slint::platform::update_timers_and_animations();
        // A later change shows what is kept: Disques still away.
        button(&window, "Retirer Système").invoke_accessible_default_action();
        until(&window, |_| {
            !kinds_kept(&directories.config).contains(&"system".to_owned())
        });
        assert!(!kinds_kept(&directories.config).contains(&"disks".to_owned()));
    }

    #[test]
    fn annuler_puts_the_widget_back_at_its_size() {
        let directories = Directories::new();
        kept(
            &directories.config,
            &[("memory", WidgetSize::Small), ("disks", WidgetSize::Tall)],
        );
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        button(&window, "Retirer Disques").invoke_accessible_default_action();
        until(&window, |_| kinds_kept(&directories.config) == ["memory"]);
        button(&window, "Annuler").invoke_accessible_default_action();
        until(&window, |_| {
            kept_layout(&directories.config)
                == [
                    ("memory".to_owned(), WidgetSize::Small),
                    ("disks".to_owned(), WidgetSize::Tall),
                ]
        });
        // The focus on it again: Suppr removes it.
        crate::cleaner::tests::press(&window, slint::platform::Key::Delete);
        until(&window, |_| kinds_kept(&directories.config) == ["memory"]);
    }

    #[test]
    fn a_widget_removed_is_read_no_more_and_read_afresh_when_added() {
        let directories = Directories::new();
        kept(
            &directories.config,
            &[("memory", WidgetSize::Small), ("system", WidgetSize::Small)],
        );
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        button(&window, "Retirer Mémoire").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(
            sampler.commands.borrow().last(),
            Some(&Command::Watch(BTreeSet::from([Source::System])))
        );
        button(&window, "Ajouter un widget").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        button(&window, "Ajouter Mémoire").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(
            sampler.commands.borrow().last(),
            Some(&Command::Watch(BTreeSet::from([
                Source::Memory,
                Source::System
            ])))
        );
        assert_eq!(
            said(&window, "Mémoire").as_deref(),
            Some("Lecture en cours")
        );
        until(&window, |_| {
            kept_layout(&directories.config)
                == [
                    ("system".to_owned(), WidgetSize::Small),
                    ("memory".to_owned(), WidgetSize::Small),
                ]
        });
    }

    #[test]
    fn a_size_chosen_with_the_keyboard_keeps_the_focus_in_the_bar() {
        use slint::platform::Key;
        let directories = Directories::new();
        let window = removing_disks(&directories);
        // The focus on Mémoire's grip; after it, its sizes: Petit, Large,
        // Haut, Grand.
        for _ in 0..4 {
            crate::cleaner::tests::press(&window, Key::Tab);
        }
        crate::cleaner::tests::press(&window, Key::Space);
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            kept_layout(&directories.config).get(1)
                == Some(&("memory".to_owned(), WidgetSize::Large))
        });
        crate::cleaner::tests::press(&window, Key::Backtab);
        crate::cleaner::tests::press(&window, Key::Space);
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            kept_layout(&directories.config).get(1)
                == Some(&("memory".to_owned(), WidgetSize::Tall))
        });
    }

    #[test]
    fn delete_held_removes_one_widget() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        let delete = slint::SharedString::from(slint::platform::Key::Delete);
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: delete.clone(),
            });
        for _ in 0..3 {
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::KeyPressRepeated {
                    text: delete.clone(),
                });
            slint::platform::update_timers_and_animations();
        }
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: delete });
        until(&window, |_| {
            kinds_kept(&directories.config) == ["reclaimable", "processor", "system"]
        });
    }

    #[test]
    fn done_takes_the_note_away() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        button(&window, "Terminé").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(!shows(&window, "Disques retiré du tableau de bord"));
    }

    #[test]
    fn a_widget_put_back_is_read_again() {
        let directories = Directories::new();
        kept(
            &directories.config,
            &[("memory", WidgetSize::Small), ("system", WidgetSize::Small)],
        );
        let (seams, sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        button(&window, "Retirer Mémoire").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        button(&window, "Annuler").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert_eq!(
            sampler.commands.borrow().last(),
            Some(&Command::Watch(BTreeSet::from([
                Source::Memory,
                Source::System
            ])))
        );
    }

    #[test]
    fn a_writer_gone_says_the_layout_is_no_longer_kept() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(DashboardSeams {
            save: Some(Box::new(|_: &DashboardLayout| panic!("the writer dies"))),
            ..seams
        });
        customizing(&window);
        button(&window, "Retirer Disques").invoke_accessible_default_action();
        until_said(&window, |window| shows(window, NOT_KEPT));
    }

    #[test]
    fn a_note_gone_while_held_leaves_the_next_one_to_go_by_itself() {
        let directories = Directories::new();
        let window = removing_disks(&directories);
        // The pointer on the note's words: it holds it.
        let note =
            ElementHandle::find_by_accessible_label(&window, "Disques retiré du tableau de bord")
                .next()
                .unwrap();
        let (position, size) = (note.absolute_position(), note.size());
        let over = slint::LogicalPosition::new(position.x + 24.0, position.y + size.height / 2.0);
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: over });
        slint::platform::update_timers_and_animations();
        // Put back from the keyboard: the note goes while held.
        let control = slint::SharedString::from(slint::platform::Key::Control);
        for (text, pressed) in [
            (control.clone(), true),
            ("z".into(), true),
            ("z".into(), false),
            (control, false),
        ] {
            window.window().dispatch_event(if pressed {
                slint::platform::WindowEvent::KeyPressed { text }
            } else {
                slint::platform::WindowEvent::KeyReleased { text }
            });
        }
        slint::platform::update_timers_and_animations();
        until(&window, |_| {
            kinds_kept(&directories.config).get(1) == Some(&"disks".to_owned())
        });
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(10.0, 10.0),
            });
        slint::platform::update_timers_and_animations();
        button(&window, "Retirer Mémoire").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(shows(&window, "Mémoire retiré du tableau de bord"));
        i_slint_backend_testing::mock_elapsed_time(TOAST_SHOWN_FOR);
        slint::platform::update_timers_and_animations();
        assert!(!shows(&window, "Mémoire retiré du tableau de bord"));
    }

    /// The page at the mockup's size, and the page itself: its writer can
    /// be let finish, so that what it kept is all there.
    fn on_home_kept(directories: &Directories) -> (ui::AppWindow, DashboardScreen) {
        let (seams, _sampler) = directories.seams();
        home_screen(seams)
    }

    /// The layout kept once every layout handed has been written; none at
    /// all when nothing was ever written.
    fn kept_in_the_end(
        home: &DashboardScreen,
        directories: &Directories,
    ) -> Option<Vec<(String, WidgetSize)>> {
        home.shut_down();
        DashboardLayout::path(&directories.config)
            .exists()
            .then(|| kept_layout(&directories.config))
    }

    /// How many groups of sizes `title` shows: none when it has only one.
    fn size_groups(window: &ui::AppWindow, title: &str) -> usize {
        ElementHandle::find_by_accessible_label(window, &format!("Taille de {title}")).count()
    }

    /// Presses `key`, then lets the page take it in.
    fn pressing(window: &ui::AppWindow, key: slint::platform::Key) {
        crate::cleaner::tests::press(window, key);
        slint::platform::update_timers_and_animations();
    }

    /// The widget's grip picked up with Enter.
    fn picking_up(window: &ui::AppWindow, title: &str) {
        button(window, &format!("Déplacer {title}")).invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
    }

    fn moving(window: &ui::AppWindow) -> ui::Moving {
        window.global::<ui::Dashboard>().get_moving()
    }

    #[test]
    fn a_widget_moved_with_the_keyboard_is_kept_where_it_is_put_down() {
        use slint::platform::Key;
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        let grip = button(&window, "Déplacer Disques");
        assert_eq!(
            grip.accessible_description().as_deref(),
            Some("Place 2 sur 5")
        );
        let (column_one, row_two, _, _) = frame(&window, "Disques");
        picking_up(&window, "Disques");
        // Mémoire after it: Disques, two columns wide, goes after Mémoire.
        pressing(&window, Key::RightArrow);
        assert_eq!(
            grip.accessible_description().as_deref(),
            Some("Saisi, place 3 sur 5")
        );
        assert!(shows(&window, "Place 3 sur 5"), "said as it changes");
        let (x, y, _, _) = frame(&window, "Disques");
        assert!((y - row_two).abs() < 1.0 && x > column_one, "shown further");
        assert!(
            (frame(&window, "Mémoire").0 - column_one).abs() < 1.0,
            "Mémoire shown in its place"
        );
        assert!(shows(&window, "Flèches, puis Entrée"));
        pressing(&window, Key::Return);
        until(&window, |_| {
            kinds_kept(&directories.config)
                == ["reclaimable", "memory", "disks", "processor", "system"]
        });
        assert!(!shows(&window, "Flèches, puis Entrée"));
        assert!((frame(&window, "Mémoire").0 - column_one).abs() < 1.0);
        // The focus on its grip again: Suppr removes it.
        pressing(&window, Key::Delete);
        until(&window, |_| {
            kinds_kept(&directories.config) == ["reclaimable", "memory", "processor", "system"]
        });
    }

    #[test]
    fn the_wheel_leaves_a_widget_moved_with_the_keys_where_it_is() {
        use slint::platform::{Key, WindowEvent};
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        // Lower than the page: it scrolls.
        window
            .window()
            .set_size(slint::PhysicalSize::new(1280, 420));
        slint::platform::update_timers_and_animations();
        customizing(&window);
        picking_up(&window, "Récupérable");
        let top = frame(&window, "Récupérable").1;
        let wheel = WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(640.0, 300.0),
            delta_x: 0.0,
            delta_y: -400.0,
        };
        for _ in 0..5 {
            window.window().dispatch_event(wheel.clone());
            slint::platform::update_timers_and_animations();
        }
        assert!(moving(&window).shown, "still moved with the keys");
        assert!(
            (frame(&window, "Récupérable").1 - top).abs() < 1.0,
            "the page stays"
        );
        pressing(&window, Key::RightArrow);
        assert_eq!(moving(&window).to, 1, "the arrows still move it");
        pressing(&window, Key::Escape);
        assert!(!moving(&window).shown);
        // Not moved any more: the wheel scrolls the page again.
        window
            .window()
            .dispatch_event(WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(640.0, 300.0),
                delta_x: 0.0,
                delta_y: -100.0,
            });
        slint::platform::update_timers_and_animations();
        assert!(frame(&window, "Récupérable").1 < top - 1.0, "scrolled");
    }

    #[test]
    fn space_picks_up_and_puts_down_and_the_arrows_move_one_place_at_a_time() {
        use slint::platform::Key;
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        // Picked up and put back: the focus stays on the grip.
        picking_up(&window, "Processeur");
        pressing(&window, Key::Escape);
        pressing(&window, Key::Space);
        assert!(moving(&window).shown);
        pressing(&window, Key::DownArrow);
        assert!(shows(&window, "Place 5 sur 5"));
        for _ in 0..3 {
            pressing(&window, Key::UpArrow);
        }
        assert!(shows(&window, "Place 2 sur 5"));
        // Suppr waits too.
        pressing(&window, Key::Delete);
        assert!(moving(&window).shown);
        pressing(&window, Key::Space);
        until(&window, |_| {
            kinds_kept(&directories.config)
                == ["reclaimable", "processor", "disks", "memory", "system"]
        });
        // Each widget keeps its own sizes: Système, with only one, shows
        // none to choose.
        assert_eq!(
            (
                size_groups(&window, "Processeur"),
                size_groups(&window, "Système")
            ),
            (1, 0)
        );
    }

    #[test]
    fn a_widget_put_down_where_it_was_is_not_kept_and_keeps_the_focus() {
        let directories = Directories::new();
        let (window, home) = on_home_kept(&directories);
        customizing(&window);
        picking_up(&window, "Mémoire");
        // Its action, as a screen reader gives it, puts it down too.
        picking_up(&window, "Mémoire");
        assert!(!moving(&window).shown);
        pressing(&window, slint::platform::Key::Return);
        assert!(moving(&window).shown, "picked up again from its grip");
        // There and back: where it was, nothing to keep.
        pressing(&window, slint::platform::Key::RightArrow);
        pressing(&window, slint::platform::Key::LeftArrow);
        pressing(&window, slint::platform::Key::Return);
        assert!(!moving(&window).shown);
        assert_eq!(kept_in_the_end(&home, &directories), None);
    }

    #[test]
    fn a_widget_moved_keeps_its_size_and_the_others_theirs() {
        let directories = Directories::new();
        let (window, home) = on_home_kept(&directories);
        customizing(&window);
        // The first Grand: Disques's, before Mémoire's.
        button(&window, "Grand").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        picking_up(&window, "Disques");
        pressing(&window, slint::platform::Key::RightArrow);
        pressing(&window, slint::platform::Key::Return);
        assert_eq!(
            kept_in_the_end(&home, &directories),
            Some(vec![
                ("reclaimable".to_owned(), WidgetSize::Full),
                ("memory".to_owned(), WidgetSize::Small),
                ("disks".to_owned(), WidgetSize::Large),
                ("processor".to_owned(), WidgetSize::Wide),
                ("system".to_owned(), WidgetSize::Small),
            ])
        );
    }

    #[test]
    fn a_widget_put_down_out_of_view_is_brought_into_it_with_the_focus() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        window
            .window()
            .set_size(slint::PhysicalSize::new(1280, u32::from(SHORT)));
        slint::platform::update_timers_and_animations();
        customizing(&window);
        // Récupérable's grip taken down to the list's bottom (the pointer
        // held still never scrolls): its card's centre over the third row,
        // which starts below the view.
        let (_, top, _, _) = frame(&window, "Récupérable");
        let grip = button(&window, "Déplacer Récupérable");
        let grip_middle = grip.absolute_position().y + grip.size().height / 2.0;
        let at = dragging(
            &window,
            "Récupérable",
            (0.0, f32::from(SHORT) - 5.0 - grip_middle),
        );
        assert_eq!(moving(&window).to, 3);
        letting_go(&window, at);
        assert!(frame(&window, "Récupérable").1 > top, "put down further");
        assert!(!moving(&window).shown);
        let (_, y, _, height) = frame(&window, "Récupérable");
        assert!(y >= 0.0 && y + height <= f32::from(SHORT), "in view");
        // Its grip kept the focus: Suppr removes it.
        pressing(&window, slint::platform::Key::Delete);
        until(&window, |_| {
            !kinds_kept(&directories.config).contains(&"reclaimable".to_owned())
        });
    }

    #[test]
    fn the_arrows_home_and_end_move_within_the_list_and_escape_puts_it_back() {
        use slint::platform::Key;
        let directories = Directories::new();
        let (window, home) = on_home_kept(&directories);
        customizing(&window);
        // Where it sits from Récupérable: the list may scroll.
        let sitting = |window: &ui::AppWindow| {
            let (x, y, _, _) = frame(window, "Mémoire");
            let (left, top, _, _) = frame(window, "Récupérable");
            (x - left, y - top)
        };
        let before = sitting(&window);
        picking_up(&window, "Mémoire");
        let grip = button(&window, "Déplacer Mémoire");
        pressing(&window, Key::Home);
        assert_eq!(
            grip.accessible_description().as_deref(),
            Some("Saisi, place 1 sur 5")
        );
        pressing(&window, Key::UpArrow);
        assert!(shows(&window, "Place 1 sur 5"), "not before the first");
        pressing(&window, Key::End);
        pressing(&window, Key::DownArrow);
        assert!(shows(&window, "Place 5 sur 5"), "not after the last");
        // Last in three columns, it takes the tile's place: the tile goes
        // after it.
        let dashboard = window.global::<ui::Dashboard>();
        // The places of three columns; Mémoire's row of the model.
        let (three_columns, memory_row) = (2, 2);
        let tile = dashboard.get_tile().row_data(three_columns).unwrap();
        let memory = dashboard
            .get_cells()
            .row_data(memory_row)
            .unwrap()
            .places
            .row_data(three_columns)
            .unwrap();
        assert!(
            (tile.row, tile.column) > (memory.row, memory.column),
            "the tile after Mémoire"
        );
        assert!(
            dashboard
                .get_rows_with_tile()
                .row_data(three_columns)
                .unwrap()
                > memory.row
        );
        pressing(&window, Key::LeftArrow);
        assert!(shows(&window, "Place 4 sur 5"));
        pressing(&window, Key::Escape);
        assert_eq!(sitting(&window), before);
        assert_eq!(
            grip.accessible_description().as_deref(),
            Some("Place 3 sur 5")
        );
        assert!(!shows(&window, "Place 4 sur 5"));
        // Put back: the next Enter picks it up again; Tab leaves it, put
        // back too.
        pressing(&window, Key::Return);
        pressing(&window, Key::Home);
        assert!(moving(&window).shown);
        pressing(&window, Key::Tab);
        assert!(!moving(&window).shown);
        assert_eq!(sitting(&window), before);
        assert_eq!(kept_in_the_end(&home, &directories), None, "only shown");
    }
    #[test]
    fn while_a_widget_is_picked_up_the_other_changes_wait() {
        let directories = Directories::new();
        let (window, home) = on_home_kept(&directories);
        customizing(&window);
        picking_up(&window, "Disques");
        assert!(
            ElementHandle::find_by_accessible_label(&window, "Retirer Disques")
                .next()
                .is_none(),
            "its own actions hidden"
        );
        button(&window, "Retirer Mémoire").invoke_accessible_default_action();
        // Disques's sizes hidden: the first Grand is Mémoire's.
        button(&window, "Grand").invoke_accessible_default_action();
        button(&window, "Déplacer Processeur").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        // A press on another grip neither.
        let at = dragging(&window, "Processeur", (0.0, -200.0));
        assert_eq!(
            (moving(&window).row, moving(&window).by_pointer),
            (1, false)
        );
        letting_go(&window, at);
        assert!(moving(&window).shown);
        assert_eq!(button(&window, "Grand").accessible_checked(), Some(false));
        // Leaving the mode puts it back.
        button(&window, "Terminé").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(!moving(&window).shown);
        assert_eq!(kept_in_the_end(&home, &directories), None);
    }

    #[test]
    fn annuler_while_a_widget_is_picked_up_puts_it_back_first() {
        let directories = Directories::new();
        let (window, home) = on_home_kept(&directories);
        customizing(&window);
        button(&window, "Retirer Système").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        picking_up(&window, "Disques");
        pressing(&window, slint::platform::Key::End);
        assert!(
            shows(&window, "Système retiré du tableau de bord"),
            "the note stays"
        );
        button(&window, "Annuler").invoke_accessible_default_action();
        slint::platform::update_timers_and_animations();
        assert!(!moving(&window).shown);
        assert_eq!(
            kept_in_the_end(&home, &directories),
            Some(
                catalog::default_widgets()
                    .into_iter()
                    .map(|(kind, size)| (kind.name().to_owned(), size))
                    .collect()
            ),
            "Système back, Disques where it was"
        );
    }

    #[test]
    fn another_window_shown_keeps_a_widget_picked_up() {
        use slint::platform::WindowEvent;
        let (seams, _sampler) = noting(None);
        let window = on_home(seams);
        customizing(&window);
        picking_up(&window, "Mémoire");
        window
            .window()
            .dispatch_event(WindowEvent::WindowActiveChanged(false));
        window
            .window()
            .dispatch_event(WindowEvent::WindowActiveChanged(true));
        slint::platform::update_timers_and_animations();
        assert!(moving(&window).shown);
        pressing(&window, slint::platform::Key::Home);
        assert!(shows(&window, "Place 1 sur 5"), "its grip has the keys");
    }

    #[test]
    fn leaving_the_page_puts_a_widget_picked_up_back() {
        use slint::platform::Key;
        let directories = Directories::new();
        let (window, home) = on_home_kept(&directories);
        customizing(&window);
        let before = frame(&window, "Système");
        picking_up(&window, "Système");
        pressing(&window, Key::Home);
        go_to(&window, "À propos");
        go_to(&window, "Accueil");
        customizing(&window);
        assert!(!moving(&window).shown);
        assert_eq!(frame(&window, "Système"), before);
        assert_eq!(kept_in_the_end(&home, &directories), None);
    }

    /// Presses `title`'s grip, then drags it by `by` (x, y), as a hand
    /// does, in two steps; the button stays down. Where the pointer is.
    fn dragging(window: &ui::AppWindow, title: &str, by: (f32, f32)) -> slint::LogicalPosition {
        use slint::platform::{PointerEventButton, WindowEvent};
        let grip = button(window, &format!("Déplacer {title}"));
        let (position, size) = (grip.absolute_position(), grip.size());
        let from = slint::LogicalPosition::new(
            position.x + size.width / 2.0,
            position.y + size.height / 2.0,
        );
        let window = window.window();
        window.dispatch_event(WindowEvent::PointerMoved { position: from });
        window.dispatch_event(WindowEvent::PointerPressed {
            position: from,
            button: PointerEventButton::Left,
        });
        slint::platform::update_timers_and_animations();
        let mut position = from;
        for part in [0.5, 1.0] {
            position = slint::LogicalPosition::new(from.x + by.0 * part, from.y + by.1 * part);
            window.dispatch_event(WindowEvent::PointerMoved { position });
            slint::platform::update_timers_and_animations();
        }
        position
    }

    /// Lets go of the pointer's button, `at` where it is.
    fn letting_go(window: &ui::AppWindow, at: slint::LogicalPosition) {
        use slint::platform::{PointerEventButton, WindowEvent};
        window
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position: at,
                button: PointerEventButton::Left,
            });
        slint::platform::update_timers_and_animations();
    }

    #[test]
    fn a_widget_dragged_by_its_grip_is_kept_where_it_is_let_go() {
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        customizing(&window);
        let (system_x, system_y, width, height) = frame(&window, "Système");
        let (disks_x, disks_y, _, _) = frame(&window, "Disques");
        // Système's card over the first column of the second row, where
        // Disques starts.
        let at = dragging(&window, "Système", (disks_x - system_x, disks_y - system_y));
        assert_eq!(
            (moving(&window).shown, moving(&window).by_pointer),
            (true, true)
        );
        assert!(shows(&window, "Déposer ici"));
        let (x, y, _, _) = frame(&window, "Système");
        assert!(
            (x - disks_x).abs() < 1.0 && (y - disks_y).abs() < 1.0,
            "the card under the pointer"
        );
        assert!(
            frame(&window, "Disques").0 > disks_x + width / 2.0,
            "Disques shown after it"
        );
        letting_go(&window, at);
        until(&window, |_| {
            kinds_kept(&directories.config)
                == ["reclaimable", "system", "disks", "memory", "processor"]
        });
        assert!(!shows(&window, "Déposer ici"));
        assert_eq!(frame(&window, "Système"), (disks_x, disks_y, width, height));
        // Each widget keeps its own sizes: Système, with only one, shows
        // none to choose.
        assert_eq!(
            (
                size_groups(&window, "Système"),
                size_groups(&window, "Processeur")
            ),
            (0, 1)
        );
        // The focus on its grip: Suppr removes it.
        pressing(&window, slint::platform::Key::Delete);
        until(&window, |_| {
            kinds_kept(&directories.config) == ["reclaimable", "disks", "memory", "processor"]
        });
    }
    #[test]
    fn a_widget_dragged_out_of_the_list_goes_back_on_escape_or_when_let_go() {
        use slint::platform::Key;
        let directories = Directories::new();
        let (window, home) = on_home_kept(&directories);
        customizing(&window);
        let before = frame(&window, "Système");
        // Over the sidebar: the grip leaves the list's view, and its focus.
        let at = dragging(&window, "Système", (-before.0, 0.0));
        assert_eq!(
            (moving(&window).shown, moving(&window).by_pointer),
            (true, true),
            "only the pointer ends a drag"
        );
        pressing(&window, Key::Escape);
        assert!(!moving(&window).shown);
        letting_go(&window, at);
        assert_eq!(frame(&window, "Système"), before);
        // The keys back on its grip.
        pressing(&window, Key::Return);
        assert_eq!(
            (moving(&window).shown, moving(&window).by_pointer),
            (true, false)
        );
        pressing(&window, Key::Escape);
        let at = dragging(&window, "Système", (-before.0, 0.0));
        assert!(moving(&window).shown, "the grip's focus taken by the page");
        letting_go(&window, at);
        assert!(!moving(&window).shown);
        assert_eq!(frame(&window, "Système"), before);
        pressing(&window, Key::Return);
        assert!(moving(&window).shown, "the keys back on its grip");
        pressing(&window, Key::Escape);
        assert_eq!(kept_in_the_end(&home, &directories), None);
    }

    #[test]
    fn escape_puts_a_widget_dragged_back() {
        let directories = Directories::new();
        let (window, home) = on_home_kept(&directories);
        customizing(&window);
        let before = frame(&window, "Système");
        let at = dragging(&window, "Système", (-before.2, -before.3));
        pressing(&window, slint::platform::Key::Escape);
        assert!(!moving(&window).shown);
        assert!(!shows(&window, "Déposer ici"));
        assert_eq!(frame(&window, "Système"), before);
        // The release after it changes nothing.
        letting_go(&window, at);
        assert_eq!(kept_in_the_end(&home, &directories), None);
    }

    /// The page's height in the tests that scroll.
    const SHORT: u16 = 500;

    #[test]
    fn the_list_scrolls_to_a_widget_moved_with_the_keyboard_and_back() {
        use slint::platform::Key;
        let directories = Directories::new();
        let (seams, _sampler) = directories.seams();
        let window = on_home(seams);
        window
            .window()
            .set_size(slint::PhysicalSize::new(1280, u32::from(SHORT)));
        slint::platform::update_timers_and_animations();
        customizing(&window);
        let top = frame(&window, "Récupérable").1;
        picking_up(&window, "Récupérable");
        pressing(&window, Key::End);
        let (_, y, _, height) = frame(&window, "Récupérable");
        assert!(y > top && y + height <= f32::from(SHORT), "moved into view");
        // Put back: just in view again (at the list's top), its grip
        // keeping the focus.
        pressing(&window, Key::Escape);
        assert!(frame(&window, "Récupérable").1.abs() < 1.0);
        pressing(&window, Key::Return);
        assert!(moving(&window).shown);
    }
    /// The window just high enough for Système's grip to sit near the
    /// list's bottom edge, well within the band where a drag scrolls.
    fn system_at_the_bottom_edge() -> ui::AppWindow {
        /// Below the grip, in pixels.
        const UNDER_THE_GRIP: f32 = 4.0;
        let (seams, _sampler) = noting(None);
        let window = on_home(seams);
        customizing(&window);
        let grip = button(&window, "Déplacer Système");
        let bottom = grip.absolute_position().y + grip.size().height + UNDER_THE_GRIP;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a window's height, in whole pixels"
        )]
        let height = bottom.ceil() as u32;
        window
            .window()
            .set_size(slint::PhysicalSize::new(1280, height));
        slint::platform::update_timers_and_animations();
        let grip = button(&window, "Déplacer Système");
        let middle = grip.absolute_position().y + grip.size().height / 2.0;
        assert!(bottom - middle < 48.0, "in the band");
        window
    }

    #[test]
    fn a_widget_dragged_to_the_list_s_edge_scrolls_it_while_held() {
        let window = system_at_the_bottom_edge();
        let top = frame(&window, "Récupérable").1;
        // Far enough to be a drag; still near the edge.
        let at = dragging(&window, "Système", (-200.0, 0.0));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        slint::platform::update_timers_and_animations();
        let scrolled = frame(&window, "Récupérable").1;
        assert!(scrolled < top, "scrolled down");
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        slint::platform::update_timers_and_animations();
        assert!(frame(&window, "Récupérable").1 < scrolled, "still");
        // At the top edge, it scrolls back up.
        let lower = frame(&window, "Récupérable").1;
        let at = slint::LogicalPosition::new(at.x, 8.0);
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        slint::platform::update_timers_and_animations();
        assert!(frame(&window, "Récupérable").1 > lower, "scrolled up");
        letting_go(&window, at);
    }

    #[test]
    fn a_press_moved_just_past_the_threshold_is_a_drag() {
        let window = system_at_the_bottom_edge();
        let top = frame(&window, "Récupérable").1;
        let at = dragging(&window, "Système", (6.0, 0.0));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        slint::platform::update_timers_and_animations();
        assert!(frame(&window, "Récupérable").1 < top, "scrolled down");
        letting_go(&window, at);
    }

    #[test]
    fn a_click_on_a_grip_near_the_edge_neither_scrolls_nor_moves() {
        let window = system_at_the_bottom_edge();
        let top = frame(&window, "Récupérable").1;
        // Just under the 4 px of a drag, as a hand trembles.
        let at = dragging(&window, "Système", (3.0, 0.0));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
        slint::platform::update_timers_and_animations();
        assert!((frame(&window, "Récupérable").1 - top).abs() < 1.0);
        letting_go(&window, at);
        assert!(!moving(&window).shown);
        assert_eq!(
            button(&window, "Déplacer Système")
                .accessible_description()
                .as_deref(),
            Some("Place 5 sur 5")
        );
    }
}
