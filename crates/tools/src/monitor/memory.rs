use std::collections::HashMap;
use std::time::{Duration, Instant};

use okapi_platform::{MemoryStatus, ProcessMemory, desktop_id};

use super::sampler::Unavailable;

/// The applications shown, the heaviest first.
const TOP_APPLICATIONS: usize = 10;
/// An application without a `.desktop` file is looked for again after this,
/// in case one was installed since.
const DESKTOP_RECHECK: Duration = Duration::from_mins(10);
/// A name not met for this many readings is forgotten.
const FORGET_AFTER: u64 = 10;

/// The name of an application, as the interface shows it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ApplicationName {
    /// From its `.desktop` file (`Google Chrome`).
    Desktop(String),
    /// Its process's name (cut at 15 bytes by the kernel), for a service or
    /// an application without a `.desktop` file.
    Process(String),
}

/// An application and the memory its processes hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationMemory {
    pub name: ApplicationName,
    /// Resident memory: pages shared between processes count in each, so
    /// these never add up to the memory used.
    pub resident_bytes: u64,
    pub processes: usize,
}

/// The applications holding the most memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applications {
    pub top: Vec<ApplicationMemory>,
    /// Processes that could not be read.
    pub unreadable: usize,
}

/// The memory, and when they were read this time, the applications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryReading {
    pub status: MemoryStatus,
    /// `None` when not read this time (they are read less often).
    pub applications: Option<Result<Applications, Unavailable>>,
}

/// The applications of `processes`, grouped by the application each runs in
/// (named by `name_of`, from its `.desktop` identifier), else by process
/// name; the [`TOP_APPLICATIONS`] heaviest, heaviest first, equal ones in the
/// order of their names.
pub(crate) fn group(
    processes: &[ProcessMemory],
    mut name_of: impl FnMut(&str) -> Option<String>,
) -> Vec<ApplicationMemory> {
    let mut totals: HashMap<ApplicationName, (u64, usize)> = HashMap::new();
    for process in processes {
        let name = process
            .cgroup
            .as_deref()
            .and_then(desktop_id)
            .and_then(|identifier| name_of(&identifier))
            .map_or_else(
                || ApplicationName::Process(process.name.clone()),
                ApplicationName::Desktop,
            );
        let total = totals.entry(name).or_default();
        total.0 = total.0.saturating_add(process.resident_bytes);
        total.1 += 1;
    }
    let mut applications: Vec<ApplicationMemory> = totals
        .into_iter()
        .map(|(name, (resident_bytes, processes))| ApplicationMemory {
            name,
            resident_bytes,
            processes,
        })
        .collect();
    applications.sort_by(|first, second| {
        second
            .resident_bytes
            .cmp(&first.resident_bytes)
            .then_with(|| first.name.cmp(&second.name))
    });
    applications.truncate(TOP_APPLICATIONS);
    applications
}

/// The names of `.desktop` identifiers already looked up, and those without
/// a file, for a while.
#[derive(Debug, Default)]
pub(crate) struct DesktopNames {
    entries: HashMap<String, Entry>,
    reading: u64,
}

#[derive(Debug)]
struct Entry {
    name: Option<String>,
    looked_up: Instant,
    seen: u64,
}

impl DesktopNames {
    /// The name of `identifier`, looked up by `lookup` unless known (a name
    /// found is kept; none found is looked up again after
    /// [`DESKTOP_RECHECK`]).
    pub(crate) fn name(
        &mut self,
        identifier: &str,
        now: Instant,
        lookup: impl FnOnce(&str) -> Option<String>,
    ) -> Option<String> {
        let reading = self.reading;
        if let Some(entry) = self.entries.get_mut(identifier)
            && (entry.name.is_some() || now.duration_since(entry.looked_up) < DESKTOP_RECHECK)
        {
            entry.seen = reading;
            return entry.name.clone();
        }
        let name = lookup(identifier);
        self.entries.insert(
            identifier.to_owned(),
            Entry {
                name: name.clone(),
                looked_up: now,
                seen: reading,
            },
        );
        name
    }

    /// Ends a reading: names not met for [`FORGET_AFTER`] readings go.
    pub(crate) fn end_reading(&mut self) {
        let reading = self.reading;
        self.entries
            .retain(|_, entry| entry.seen + FORGET_AFTER > reading);
        self.reading += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const APPS: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice";

    fn process(name: &str, bytes: u64, unit: Option<&str>) -> ProcessMemory {
        ProcessMemory {
            pid: 1,
            name: name.to_owned(),
            resident_bytes: bytes,
            cgroup: unit.map(|unit| {
                if unit.starts_with('/') {
                    unit.to_owned()
                } else {
                    format!("{APPS}/{unit}")
                }
            }),
        }
    }

    fn desktop(identifier: &str) -> Option<String> {
        match identifier {
            "google-chrome" | "com.google.Chrome" => Some("Google Chrome".to_owned()),
            "firefox_firefox" => Some("Firefox".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn processes_of_one_application_are_one_however_they_were_launched() {
        let processes = [
            process("chrome", 300, Some(r"app-gnome-google\x2dchrome-1.scope")),
            process("chrome", 200, Some("app-com.google.Chrome-1.scope")),
            process(
                "Isolated Web Co",
                700,
                Some("snap.firefox.firefox-0b6f2c1e-4d2a-4c1e-9a7f-1f2e3d4c5b6a.scope"),
            ),
            process(
                "WebExtensions",
                100,
                Some("snap.firefox.firefox-0b6f2c1e-4d2a-4c1e-9a7f-1f2e3d4c5b6a.scope"),
            ),
            process("gnome-shell", 400, None),
            process("tool", 50, Some("app-org.example.Tool-9.scope")),
            process("cupsd", 50, Some("/system.slice/cups.service")),
        ];
        assert_eq!(
            group(&processes, desktop),
            [
                ApplicationMemory {
                    name: ApplicationName::Desktop("Firefox".to_owned()),
                    resident_bytes: 800,
                    processes: 2,
                },
                ApplicationMemory {
                    name: ApplicationName::Desktop("Google Chrome".to_owned()),
                    resident_bytes: 500,
                    processes: 2,
                },
                ApplicationMemory {
                    name: ApplicationName::Process("gnome-shell".to_owned()),
                    resident_bytes: 400,
                    processes: 1,
                },
                ApplicationMemory {
                    name: ApplicationName::Process("cupsd".to_owned()),
                    resident_bytes: 50,
                    processes: 1,
                },
                ApplicationMemory {
                    name: ApplicationName::Process("tool".to_owned()),
                    resident_bytes: 50,
                    processes: 1,
                },
            ],
            "equal ones in the order of their names"
        );
    }

    #[test]
    fn only_the_heaviest_are_kept() {
        let processes: Vec<ProcessMemory> = (1..=12)
            .map(|index| process(&format!("p{index:02}"), index, None))
            .collect();
        let top = group(&processes, desktop);
        assert_eq!(top.len(), TOP_APPLICATIONS);
        assert_eq!(top[0].resident_bytes, 12);
        assert_eq!(top[9].resident_bytes, 3);
    }

    #[test]
    fn a_name_is_looked_up_once_and_an_absence_again_after_a_while() {
        let start = Instant::now();
        let mut names = DesktopNames::default();
        let lookups = Cell::new(0);
        let lookup = |identifier: &str| {
            lookups.set(lookups.get() + 1);
            desktop(identifier)
        };
        assert_eq!(
            names.name("firefox_firefox", start, lookup).as_deref(),
            Some("Firefox")
        );
        assert_eq!(
            names
                .name("firefox_firefox", start + DESKTOP_RECHECK * 2, lookup)
                .as_deref(),
            Some("Firefox")
        );
        assert_eq!(names.name("org.example.Tool", start, lookup), None);
        assert_eq!(
            names.name("org.example.Tool", start + Duration::from_secs(1), lookup),
            None
        );
        assert_eq!(lookups.get(), 2);
        names.name("org.example.Tool", start + DESKTOP_RECHECK, lookup);
        assert_eq!(lookups.get(), 3, "an absence is looked up again");
    }

    #[test]
    fn a_name_long_unmet_is_forgotten_and_one_met_kept() {
        let start = Instant::now();
        let mut names = DesktopNames::default();
        let lookups = Cell::new(0);
        let lookup = |identifier: &str| {
            lookups.set(lookups.get() + 1);
            desktop(identifier)
        };
        names.name("firefox_firefox", start, lookup);
        names.name("google-chrome", start, lookup);
        // Chrome met at every reading; Firefox in the first one only, then
        // kept one reading short of the limit...
        for _ in 0..FORGET_AFTER {
            names.name("google-chrome", start, lookup);
            names.end_reading();
        }
        names.name("firefox_firefox", start, lookup);
        assert_eq!(lookups.get(), 2, "not yet forgotten");
        for _ in 0..=FORGET_AFTER {
            names.name("google-chrome", start, lookup);
            names.end_reading();
        }
        names.name("google-chrome", start, lookup);
        assert_eq!(lookups.get(), 2, "met all along: kept");
        names.name("firefox_firefox", start, lookup);
        assert_eq!(lookups.get(), 3, "unmet too long: looked up again");
    }
}
