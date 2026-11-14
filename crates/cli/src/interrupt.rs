//! Ctrl-C: the first one cancels what runs, which then ends cleanly (its
//! receipt and history line kept); a second one ends the process at once,
//! except while a cleaning runs and writes its history line.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use okapi_engine::{Cancel, Watch};
use signal_hook::SigId;
use signal_hook::consts::SIGINT;
use signal_hook::flag;
use signal_hook::low_level::unregister;

use crate::exit;

/// What Ctrl-C does to this run.
#[derive(Debug)]
pub struct Interrupt {
    /// The token's flag, raised by Ctrl-C.
    raised: Arc<AtomicBool>,
    /// What raises the token.
    token: SigId,
    /// What ends the process at the second Ctrl-C; none while it only
    /// cancels.
    shutdown: Option<SigId>,
}

impl Interrupt {
    /// Sets what Ctrl-C does; none, traced, when a handler cannot be set:
    /// Ctrl-C then still ends the process at once, with
    /// [`exit::INTERRUPTED`] once the shutdown is set, by default otherwise.
    pub fn set() -> Option<Self> {
        let raised = Arc::new(AtomicBool::new(false));
        let (shutdown, token) = handle(&raised)
            .inspect_err(|error| {
                tracing::warn!(%error, "Ctrl-C handler not set: Ctrl-C ends the run at once");
            })
            .ok()?;
        tracing::info!("Ctrl-C cancels, a second one ends the run at once");
        Some(Self {
            raised,
            token,
            shutdown: Some(shutdown),
        })
    }

    /// The token Ctrl-C raises.
    pub fn cancel(&self) -> Cancel {
        Cancel::raised_by(Arc::clone(&self.raised))
    }

    /// Until [`Self::ends_again`], Ctrl-C only cancels, however often: a
    /// cleaning starts, and its history line (with the deletion log asked
    /// for) must be written once it ends, cancelled or not.
    pub fn cancels_only(&mut self) {
        let Some(shutdown) = self.shutdown.take() else {
            return;
        };
        if unregister(shutdown) {
            tracing::info!("Ctrl-C only cancels: a cleaning runs");
        } else {
            tracing::warn!("Ctrl-C shutdown already removed");
        }
    }

    /// A second Ctrl-C ends the process at once again, with
    /// [`exit::INTERRUPTED`] whatever the run did: the cleaning's history
    /// line is written. Both handlers are set again, in their order (a
    /// shutdown set after the token would end the process at the first
    /// Ctrl-C); a Ctrl-C in between is lost, nothing being at stake.
    pub fn ends_again(&mut self) {
        if self.shutdown.is_some() {
            return;
        }
        if !unregister(self.token) {
            tracing::warn!(
                "Ctrl-C token handler not removed: the next Ctrl-C may end the run at once"
            );
        }
        match handle(&self.raised) {
            Ok((shutdown, token)) => {
                tracing::info!("Ctrl-C ends the run at once again at the second one");
                self.shutdown = Some(shutdown);
                self.token = token;
            }
            Err(error) => {
                tracing::warn!(%error, "Ctrl-C handlers not set again: Ctrl-C does nothing more");
            }
        }
    }
}

/// Sets what Ctrl-C does with `raised`: what ends the process with
/// [`exit::INTERRUPTED`] once it is raised, then what raises it, in this
/// order: the first Ctrl-C finds it not raised yet, so only raises it.
fn handle(raised: &Arc<AtomicBool>) -> io::Result<(SigId, SigId)> {
    let shutdown = flag::register_conditional_shutdown(
        SIGINT,
        i32::from(exit::INTERRUPTED),
        Arc::clone(raised),
    )?;
    match flag::register(SIGINT, Arc::clone(raised)) {
        Ok(token) => Ok((shutdown, token)),
        Err(error) => {
            // Defensive, never met once the signal's handler is in place:
            // without the token, the shutdown would never end the process;
            // the next Ctrl-C does.
            raised.store(true, Ordering::SeqCst);
            Err(error)
        }
    }
}

/// What a scan and a cleaning are watched by: cancelled by Ctrl-C, once
/// its handler is set.
pub fn watch(interrupt: Option<&Interrupt>) -> Watch {
    Watch::new(interrupt.map_or_else(Cancel::new, Interrupt::cancel), None)
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "invariant 4: signals received, raised in a test process of their own, which this test runs"
    )]

    use std::env;
    use std::process::Command;

    use super::*;

    /// The steps [`signals_raised_here`] takes in its own process: `R`
    /// raises SIGINT, `C` makes it cancel only, `E` ends again.
    const STEPS: &str = "OKAPI_TEST_INTERRUPT_STEPS";
    /// Said once every step is taken: the process lived through them.
    const LIVED: &str = "lived through every Ctrl-C";

    /// Runs this test binary's [`signals_raised_here`] alone, taking
    /// `steps`: its exit code, and whether it lived through them.
    fn taken_apart(steps: &str) -> (Option<i32>, bool) {
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "interrupt::tests::signals_raised_here",
            ])
            .env(STEPS, steps)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.code() != Some(101),
            "{steps}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.code(), said.contains(LIVED))
    }

    /// Takes the steps [`STEPS`] says, in its own process.
    #[test]
    #[ignore = "run in a process of its own by the test below"]
    fn signals_raised_here() {
        // Run with the others (`--include-ignored`): nothing to take.
        let Ok(steps) = env::var(STEPS) else {
            return;
        };
        let mut interrupt = Interrupt::set().unwrap();
        let cancel = interrupt.cancel();
        assert!(!cancel.is_cancelled());
        for step in steps.chars() {
            match step {
                'R' => {
                    signal_hook::low_level::raise(SIGINT).unwrap();
                    assert!(cancel.is_cancelled());
                }
                'C' => interrupt.cancels_only(),
                'E' => interrupt.ends_again(),
                _ => panic!("no step {step}"),
            }
        }
        println!("{LIVED}");
    }

    #[test]
    fn ctrl_c_cancels_then_ends_the_run_with_130_but_never_a_cleaning() {
        let ended = (Some(i32::from(exit::INTERRUPTED)), false);
        let lived = (Some(0), true);
        assert_eq!(taken_apart("R"), lived, "the first one cancels");
        assert_eq!(taken_apart("RR"), ended, "the second one ends it");
        assert_eq!(taken_apart("CRR"), lived, "never while a cleaning runs");
        assert_eq!(taken_apart("CER"), lived, "after it, the first one cancels");
        assert_eq!(taken_apart("CERR"), ended, "and the second one ends it");
        assert_eq!(taken_apart("CRER"), ended, "one during it counts");
    }
}
