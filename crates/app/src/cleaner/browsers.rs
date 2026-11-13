//! A browser open when its caches are to be cleaned: the question of the
//! maquette, one browser after the other, before the cleaning starts. Leave
//! its caches out this time, or have Okapi close it and go on.

use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use std::time::Duration;

use okapi_engine::BrowserLock;
use slint::ComponentHandle as _;

use super::cleaning::Confirming;
use super::{CleanerScreen, size};
use crate::ui;

/// How often the lock of a browser asked to close is read again.
pub(super) const CLOSE_LOOK_EVERY: Duration = Duration::from_millis(200);
/// How many looks a browser has to close: fifteen seconds, time for it to
/// save its session.
pub(super) const CLOSE_LOOKS: u32 = 75;

/// What the user can be offered about an open browser.
pub enum Asked {
    /// It closed meanwhile.
    Closed,
    /// A process vouched to be it, `pid`: how to ask it to close.
    Closable {
        pid: u32,
        close: Box<dyn FnOnce() -> io::Result<()>>,
    },
    /// A lock Okapi cannot vouch for: it never signals it.
    Unverified,
}

/// How the screen learns who holds a browser's lock.
pub type HolderSeam = Rc<dyn Fn(&BrowserLock) -> Asked>;

/// The system's: the platform vouches for the process, or not.
#[must_use]
#[expect(
    clippy::disallowed_methods,
    reason = "closing a browser the user asked to close: the interface's one signal"
)]
pub fn system_holder() -> HolderSeam {
    Rc::new(
        |lock| match okapi_platform::holder(lock.lock(), lock.user_data()) {
            okapi_platform::Holder::Absent => Asked::Closed,
            okapi_platform::Holder::Process(running) => Asked::Closable {
                pid: running.pid(),
                close: Box::new(move || running.terminate()),
            },
            okapi_platform::Holder::Unknown(why) => {
                tracing::info!(
                    ?why,
                    browser = lock.browser().key(),
                    "open browser not vouched for"
                );
                Asked::Unverified
            }
        },
    )
}

/// The question open: what the cleaning confirmed still takes, the browsers
/// left to ask about (the first is asked), and its wait for a closing.
pub(super) struct Asking {
    confirmed: Confirming,
    open: VecDeque<BrowserLock>,
    /// The looks left while the first browser closes.
    closing: Option<u32>,
    timer: Rc<slint::Timer>,
}

impl Asking {
    /// Whether a browser was asked to close and is awaited.
    pub(super) const fn is_closing(&self) -> bool {
        self.closing.is_some()
    }
}

impl CleanerScreen {
    /// Before the cleaning `confirmed` starts: the open browsers whose
    /// caches it takes, asked about one by one.
    pub(super) fn ask_about_browsers(&self, confirmed: Confirming) {
        let open = confirmed.report().open_browsers(&confirmed.ids());
        if open.is_empty() {
            self.launch(&confirmed);
            return;
        }
        tracing::info!(browsers = open.len(), "cleaning asks about open browsers");
        self.0.borrow_mut().asking = Some(Asking {
            confirmed,
            open: open.into(),
            closing: None,
            timer: Rc::new(slint::Timer::default()),
        });
        self.ask_next_browser();
    }

    /// Asks about the first browser still open; with none left, cleans.
    fn ask_next_browser(&self) {
        loop {
            let first = {
                let mut state = self.0.borrow_mut();
                let Some(asking) = state.asking.as_mut() else {
                    return;
                };
                asking.closing = None;
                asking.timer.stop();
                match asking.open.front() {
                    Some(lock) if lock.is_held() => Some(lock.clone()),
                    Some(lock) => {
                        tracing::info!(browser = lock.browser().key(), "browser closed meanwhile");
                        asking.open.pop_front();
                        continue;
                    }
                    None => None,
                }
            };
            let Some(lock) = first else {
                let asking = self.0.borrow_mut().asking.take();
                self.close_question();
                if let Some(asking) = asking {
                    self.launch(&asking.confirmed);
                }
                return;
            };
            // Bound first: the state is borrowed again below.
            let asked = (self.0.borrow().seams.browser)(&lock);
            let step = match asked {
                Asked::Closed => {
                    if let Some(asking) = self.0.borrow_mut().asking.as_mut() {
                        asking.open.pop_front();
                    }
                    continue;
                }
                // Asked once only, for the life of the screen: a second
                // request could end it at once, in the middle of saving.
                Asked::Closable { pid, .. } if self.0.borrow().asked_to_close.contains(&pid) => {
                    ui::BrowserStep::NotClosed
                }
                Asked::Closable { .. } => ui::BrowserStep::Ask,
                Asked::Unverified => ui::BrowserStep::Unverified,
            };
            self.show_question(&lock, step);
            return;
        }
    }

    /// Shows the question about `lock`'s browser: its caches the cleaning
    /// takes, and what can be done.
    fn show_question(&self, lock: &BrowserLock, step: ui::BrowserStep) {
        let (caches, bytes) = {
            let state = self.0.borrow();
            let Some(asking) = &state.asking else {
                return;
            };
            let report = asking.confirmed.report();
            let mut labels = Vec::new();
            let mut bytes: u64 = 0;
            for index in asking.confirmed.groups() {
                let Some(group) = report.groups().get(*index) else {
                    continue;
                };
                if group.locks().contains(lock) {
                    labels.push(
                        okapi_rules::label(state.language, group.label().as_str()).to_owned(),
                    );
                    bytes = bytes.saturating_add(group.total_size());
                }
            }
            (labels.join(", "), bytes)
        };
        self.settle();
        self.with_window(|window, _| {
            let cleaner = window.global::<ui::Cleaner>();
            cleaner.set_browser_name(lock.browser().name().into());
            cleaner.set_browser_caches(caches.into());
            cleaner.set_browser_size(size(bytes));
            cleaner.set_browser_step(step);
            cleaner.set_browser_asking(true);
        });
    }

    /// Changes what the question says and offers: a click meant for the
    /// buttons before never lands on those now.
    fn set_browser_step(&self, step: ui::BrowserStep) {
        self.settle();
        self.with_window(|window, _| window.global::<ui::Cleaner>().set_browser_step(step));
    }

    fn close_question(&self) {
        self.settle();
        self.with_window(|window, _| window.global::<ui::Cleaner>().set_browser_asking(false));
    }

    /// Leaves the first browser's caches out of this cleaning (not out of the
    /// selection); with nothing left, back to the results.
    pub(super) fn ignore_browser(&self) {
        if self.is_settling("ignoring a browser") {
            return;
        }
        let nothing_left = {
            let mut state = self.0.borrow_mut();
            let Some(asking) = state.asking.as_mut() else {
                return;
            };
            asking.timer.stop();
            let Some(lock) = asking.open.pop_front() else {
                return;
            };
            let left = asking.confirmed.leave_out(&lock);
            tracing::info!(
                browser = lock.browser().key(),
                entries_left = left,
                "browser ignored"
            );
            left == 0
        };
        if nothing_left {
            tracing::info!("every cache ignored: nothing to clean");
            self.0.borrow_mut().asking = None;
            self.close_question();
            return;
        }
        self.ask_next_browser();
    }

    /// Asks the first browser to close, then waits for its lock to go.
    pub(super) fn close_browser(&self) {
        if self.is_settling("closing a browser") {
            return;
        }
        let lock = {
            let state = self.0.borrow();
            let Some(asking) = &state.asking else {
                return;
            };
            if asking.is_closing() {
                return;
            }
            let Some(lock) = asking.open.front() else {
                return;
            };
            lock.clone()
        };
        // Asked again now: what was shown only chose the buttons.
        let asked = (self.0.borrow().seams.browser)(&lock);
        match asked {
            Asked::Closed => {
                tracing::info!(
                    browser = lock.browser().key(),
                    "browser closed before the click"
                );
                if let Some(asking) = self.0.borrow_mut().asking.as_mut() {
                    asking.open.pop_front();
                }
                self.ask_next_browser();
            }
            Asked::Unverified => self.set_browser_step(ui::BrowserStep::Unverified),
            Asked::Closable { pid, close } => {
                if !self.0.borrow_mut().asked_to_close.insert(pid) {
                    tracing::warn!(
                        browser = lock.browser().key(),
                        pid,
                        "browser already asked to close: not asked again"
                    );
                    self.set_browser_step(ui::BrowserStep::NotClosed);
                    return;
                }
                if let Err(error) = close() {
                    tracing::warn!(browser = lock.browser().key(), %error, code = error.raw_os_error(), "browser not asked to close");
                    self.set_browser_step(ui::BrowserStep::Refused);
                    return;
                }
                tracing::info!(browser = lock.browser().key(), "browser asked to close");
                self.set_browser_step(ui::BrowserStep::Closing);
                self.wait_for_closing();
            }
        }
    }

    /// Reads the first browser's lock every [`CLOSE_LOOK_EVERY`], for
    /// [`CLOSE_LOOKS`] looks at most.
    fn wait_for_closing(&self) {
        let timer = {
            let mut state = self.0.borrow_mut();
            let Some(asking) = state.asking.as_mut() else {
                return;
            };
            asking.closing = Some(CLOSE_LOOKS);
            Rc::clone(&asking.timer)
        };
        let weak = Rc::downgrade(&self.0);
        timer.start(slint::TimerMode::Repeated, CLOSE_LOOK_EVERY, move || {
            if let Some(state) = weak.upgrade() {
                Self(state).look_at_closing();
            }
        });
    }

    fn look_at_closing(&self) {
        let (browser, closed, looked) = {
            let mut state = self.0.borrow_mut();
            let Some(asking) = state.asking.as_mut() else {
                return;
            };
            let Some(lock) = asking.open.front() else {
                return;
            };
            let browser = lock.browser().key();
            let closed = !lock.is_held();
            let Some(looks) = asking.closing.as_mut() else {
                return;
            };
            *looks = looks.saturating_sub(1);
            (browser, closed, CLOSE_LOOKS - *looks)
        };
        let waited = CLOSE_LOOK_EVERY * looked;
        if closed {
            tracing::info!(browser, ?waited, "browser closed");
            if let Some(asking) = self.0.borrow_mut().asking.as_mut() {
                asking.open.pop_front();
            }
            self.ask_next_browser();
        } else if looked == CLOSE_LOOKS {
            tracing::warn!(browser, ?waited, "browser did not close");
            if let Some(asking) = self.0.borrow_mut().asking.as_mut() {
                asking.closing = None;
                asking.timer.stop();
            }
            self.set_browser_step(ui::BrowserStep::NotClosed);
        }
    }

    /// Escape: the cleaning is cancelled, nothing is removed. A browser
    /// already asked to close keeps closing, unwatched.
    /// Only Escape leads here, never the second click of a double click: it
    /// is never held back, even while a browser closes.
    pub(super) fn cancel_browser_question(&self) {
        let Some(asking) = self.0.borrow_mut().asking.take() else {
            return;
        };
        asking.timer.stop();
        tracing::info!(
            closing = asking.closing.is_some(),
            "cleaning cancelled at the question about a browser"
        );
        self.close_question();
    }

    /// Whether the question is open: the results are then frozen.
    pub(super) fn is_asking(&self) -> bool {
        self.0.borrow().asking.is_some()
    }
}
