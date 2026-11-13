use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Memory and processor are read this often.
const FAST: Duration = Duration::from_secs(2);
/// The applications holding the memory: reading every process costs more.
const APPLICATIONS: Duration = Duration::from_secs(5);
/// The volumes' space changes slowly; a key plugged in shows within this.
const VOLUMES: Duration = Duration::from_secs(15);
/// The system's description and how long it has run.
const SYSTEM: Duration = Duration::from_secs(60);

/// What the dashboard may follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    Volumes,
    Memory,
    /// The applications holding the memory, read with it.
    Applications,
    Processor,
    System,
}

impl Source {
    const fn period(self) -> Duration {
        match self {
            Self::Memory | Self::Processor => FAST,
            Self::Applications => APPLICATIONS,
            Self::Volumes => VOLUMES,
            Self::System => SYSTEM,
        }
    }
}

/// When each followed source is due. Pure: told the time, it decides.
#[derive(Debug, Default)]
pub(crate) struct Schedule {
    next: BTreeMap<Source, Instant>,
    paused: bool,
}

impl Schedule {
    /// Follows `sources` from now on: one newly followed is due at once, one
    /// no longer followed is dropped.
    pub(crate) fn watch(&mut self, sources: &BTreeSet<Source>, now: Instant) {
        self.next.retain(|source, _| sources.contains(source));
        for source in sources {
            self.next.entry(*source).or_insert(now);
        }
    }

    /// The sources due at `now`, each then due again a period later. None
    /// while paused.
    pub(crate) fn due(&mut self, now: Instant) -> BTreeSet<Source> {
        if self.paused {
            return BTreeSet::new();
        }
        let mut due = BTreeSet::new();
        for (source, next) in &mut self.next {
            if *next <= now {
                due.insert(*source);
                *next = now + source.period();
            }
        }
        due
    }

    /// Whether `source` is followed.
    pub(crate) fn follows(&self, source: Source) -> bool {
        self.next.contains_key(&source)
    }

    /// When the next source is due; none while paused or following nothing.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if self.paused {
            return None;
        }
        self.next.values().min().copied()
    }

    pub(crate) fn pause(&mut self) {
        self.paused = true;
    }

    /// Every followed source is due at once.
    pub(crate) fn resume(&mut self, now: Instant) {
        self.paused = false;
        for next in self.next.values_mut() {
            *next = now;
        }
    }

    /// The volumes, if followed, are due at once; while paused, the
    /// resumption reads them anyway.
    pub(crate) fn refresh_volumes(&mut self, now: Instant) {
        if let Some(next) = self.next.get_mut(&Source::Volumes) {
            *next = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(list: &[Source]) -> BTreeSet<Source> {
        list.iter().copied().collect()
    }

    #[test]
    fn each_followed_source_is_due_at_once_then_at_its_period() {
        let start = Instant::now();
        let mut schedule = Schedule::default();
        schedule.watch(&sources(&[Source::Memory, Source::Volumes]), start);
        assert_eq!(
            schedule.due(start),
            sources(&[Source::Memory, Source::Volumes])
        );
        assert_eq!(schedule.next_deadline(), Some(start + FAST));
        assert!(
            schedule
                .due(start + FAST.saturating_sub(Duration::from_millis(1)))
                .is_empty()
        );
        assert_eq!(schedule.due(start + FAST), sources(&[Source::Memory]));
        assert_eq!(
            schedule.due(start + VOLUMES),
            sources(&[Source::Memory, Source::Volumes])
        );
    }

    #[test]
    fn only_followed_sources_are_due_and_a_new_one_at_once() {
        let start = Instant::now();
        let mut schedule = Schedule::default();
        assert_eq!(schedule.next_deadline(), None);
        schedule.watch(&sources(&[Source::Memory]), start);
        schedule.due(start);
        let later = start + Duration::from_secs(1);
        schedule.watch(&sources(&[Source::Memory, Source::Applications]), later);
        assert_eq!(schedule.due(later), sources(&[Source::Applications]));
        schedule.watch(&sources(&[Source::Applications]), later);
        assert!(!schedule.due(start + SYSTEM).contains(&Source::Memory));
    }

    #[test]
    fn paused_nothing_is_due_and_resumed_everything_is_at_once() {
        let start = Instant::now();
        let mut schedule = Schedule::default();
        schedule.watch(&sources(&[Source::Volumes, Source::Processor]), start);
        schedule.due(start);
        schedule.pause();
        assert_eq!(schedule.next_deadline(), None);
        assert!(schedule.due(start + SYSTEM).is_empty());
        // Back before either period ended: both are due anyway.
        let back = start + Duration::from_secs(1);
        schedule.resume(back);
        assert_eq!(schedule.next_deadline(), Some(back));
        assert_eq!(
            schedule.due(back),
            sources(&[Source::Processor, Source::Volumes])
        );
    }

    #[test]
    fn each_source_keeps_its_own_period() {
        let start = Instant::now();
        let mut schedule = Schedule::default();
        schedule.watch(&sources(&[Source::Applications, Source::System]), start);
        schedule.due(start);
        assert_eq!(
            schedule.due(start + APPLICATIONS),
            sources(&[Source::Applications])
        );
        assert_eq!(
            schedule.due(start + SYSTEM),
            sources(&[Source::Applications, Source::System])
        );
    }

    #[test]
    fn a_refresh_never_reads_volumes_nobody_follows() {
        let start = Instant::now();
        let mut schedule = Schedule::default();
        schedule.watch(&sources(&[Source::Memory]), start);
        schedule.due(start);
        schedule.refresh_volumes(start);
        assert!(!schedule.follows(Source::Volumes));
        assert!(schedule.follows(Source::Memory));
        assert!(!schedule.due(start + SYSTEM).contains(&Source::Volumes));
    }

    #[test]
    fn a_refresh_makes_the_volumes_due_at_once() {
        let start = Instant::now();
        let mut schedule = Schedule::default();
        schedule.watch(&sources(&[Source::Volumes]), start);
        schedule.due(start);
        let soon = start + Duration::from_secs(1);
        schedule.refresh_volumes(soon);
        assert_eq!(schedule.due(soon), sources(&[Source::Volumes]));
    }
}
