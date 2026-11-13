use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use okapi_platform::{Device, Space};

use super::sampler::Unavailable;
use super::volumes::{Candidate, Volume, VolumeSpace, aggregate};

/// While this many reads of space from earlier rounds are still unanswered,
/// no read is launched: a read that never answers keeps its thread.
pub(crate) const MAX_PENDING: usize = 16;

/// The longest a round waits, whatever its timeout.
const LONGEST_WAIT: Duration = Duration::from_hours(24);

/// A read of space to launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpaceRequest {
    pub device: Device,
    pub mount_point: PathBuf,
}

/// The answer to a read of space, or why there is none.
pub(crate) type SpaceAnswer = Result<Space, Unavailable>;

/// One tour of the volumes: the reads it waits for, their answers, and its
/// deadline. Pure: told the time and the answers, it decides.
#[derive(Debug, Default)]
pub(crate) struct VolumeRound {
    /// The filesystems whose read has not answered yet, whichever round
    /// launched it, and when: never read twice at once.
    in_flight: BTreeMap<Device, Instant>,
    current: Option<Current>,
    at_cap: bool,
    /// The longest read answered since last asked.
    slowest: Option<Duration>,
}

#[derive(Debug)]
struct Current {
    deadline: Instant,
    candidates: Vec<Candidate>,
    awaited: BTreeSet<Device>,
    answers: BTreeMap<Device, SpaceAnswer>,
}

impl VolumeRound {
    /// Starts a round over `candidates`, answered by `now + timeout` at the
    /// latest: the reads to launch, and the volumes at once when there is
    /// nothing to wait for. A filesystem still read since an earlier round is
    /// not read again: that read's answer, when it comes, counts for this
    /// one. While [`MAX_PENDING`] reads of earlier rounds are unanswered, no
    /// read is launched and the rest do not respond. A round under way is
    /// forgotten (the caller lets one end first).
    pub(crate) fn begin(
        &mut self,
        now: Instant,
        candidates: Vec<Candidate>,
        timeout: Duration,
    ) -> (Vec<SpaceRequest>, Option<Vec<Volume>>) {
        let mut requests = Vec::new();
        let mut awaited = BTreeSet::new();
        let stuck = self.in_flight.len();
        let mut capped = false;
        for member in candidates.iter().flat_map(|candidate| &candidate.members) {
            if self.in_flight.contains_key(&member.device) {
                awaited.insert(member.device);
                continue;
            }
            if stuck >= MAX_PENDING {
                capped = true;
                continue;
            }
            self.in_flight.insert(member.device, now);
            awaited.insert(member.device);
            requests.push(SpaceRequest {
                device: member.device,
                mount_point: member.mount_point.clone(),
            });
        }
        if capped != self.at_cap {
            self.at_cap = capped;
            if capped {
                tracing::warn!(
                    pending = self.in_flight.len(),
                    "space reads at their cap, the next not launched"
                );
            } else {
                tracing::info!("space reads back under their cap");
            }
        }
        let nothing_awaited = awaited.is_empty();
        self.current = Some(Current {
            // A timeout too long to add waits as long as one can.
            deadline: now
                .checked_add(timeout)
                .unwrap_or_else(|| now + LONGEST_WAIT),
            candidates,
            awaited,
            answers: BTreeMap::new(),
        });
        let done = if nothing_awaited { self.finish() } else { None };
        (requests, done)
    }

    /// Takes the answer, at `now`, of the read of `device`: it is no longer
    /// in flight, and counts for the round under way if that round waits for
    /// it. The volumes when that round has all its answers.
    pub(crate) fn answer(
        &mut self,
        device: Device,
        answer: SpaceAnswer,
        now: Instant,
    ) -> Option<Vec<Volume>> {
        if let Some(launched) = self.in_flight.remove(&device) {
            let took = now.saturating_duration_since(launched);
            self.slowest = Some(self.slowest.map_or(took, |slowest| slowest.max(took)));
        }
        let current = self.current.as_mut()?;
        if !current.awaited.remove(&device) {
            return None;
        }
        current.answers.insert(device, answer);
        if current.awaited.is_empty() {
            self.finish()
        } else {
            None
        }
    }

    /// When the round under way must end, answered or not.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.current.as_ref().map(|current| current.deadline)
    }

    /// The volumes of the round under way if its deadline has passed, those
    /// without an answer not responding.
    pub(crate) fn expire(&mut self, now: Instant) -> Option<Vec<Volume>> {
        if self.current.as_ref()?.deadline <= now {
            self.finish()
        } else {
            None
        }
    }

    /// Forgets the round under way (a pause); its reads stay in flight until
    /// they answer.
    pub(crate) fn abandon(&mut self) {
        self.current = None;
    }

    /// How many reads have not answered.
    pub(crate) fn pending(&self) -> usize {
        self.in_flight.len()
    }

    /// The longest read answered since last asked.
    pub(crate) fn take_slowest(&mut self) -> Option<Duration> {
        self.slowest.take()
    }

    fn finish(&mut self) -> Option<Vec<Volume>> {
        let current = self.current.take()?;
        let volumes = current
            .candidates
            .into_iter()
            .map(|candidate| {
                let mut spaces = Vec::with_capacity(candidate.members.len());
                let mut failed = None;
                let mut missing = false;
                for member in &candidate.members {
                    match current.answers.get(&member.device) {
                        Some(Ok(space)) => spaces.push(*space),
                        Some(Err(unavailable)) => failed = failed.or(Some(*unavailable)),
                        None => missing = true,
                    }
                }
                let space = match (failed, missing) {
                    (Some(unavailable), _) => VolumeSpace::Failed(unavailable),
                    (None, true) => VolumeSpace::NotResponding,
                    (None, false) => VolumeSpace::Known(aggregate(&spaces)),
                };
                Volume {
                    name: candidate.name,
                    mount_point: candidate.mount_point,
                    filesystem: candidate.filesystem,
                    kind: candidate.kind,
                    space,
                }
            })
            .collect();
        Some(volumes)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::Path;

    use super::super::volumes::{Member, VolumeKind, VolumeName};
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(2);

    fn device(minor: u32) -> Device {
        Device { major: 8, minor }
    }

    fn candidate(point: &str, minors: &[u32]) -> Candidate {
        Candidate {
            name: VolumeName::MountPoint(PathBuf::from(point)),
            mount_point: PathBuf::from(point),
            filesystem: "ext4".to_owned(),
            kind: VolumeKind::Local,
            members: minors
                .iter()
                .map(|minor| Member {
                    device: device(*minor),
                    mount_point: PathBuf::from(format!("{point}/{minor}")),
                })
                .collect(),
        }
    }

    fn space(used: u64) -> Space {
        Space {
            total: 100,
            available: 100 - used,
            used,
        }
    }

    fn denied() -> Unavailable {
        Unavailable {
            kind: io::ErrorKind::PermissionDenied,
            code: Some(13),
        }
    }

    fn spaces(volumes: &[Volume]) -> Vec<(&Path, VolumeSpace)> {
        volumes
            .iter()
            .map(|volume| (volume.mount_point.as_path(), volume.space))
            .collect()
    }

    #[test]
    fn a_round_answered_whole_gives_every_volume_each_read_at_its_member() {
        let start = Instant::now();
        let mut round = VolumeRound::default();
        let (requests, done) = round.begin(
            start,
            vec![candidate("/a", &[1]), candidate("/b", &[2])],
            TIMEOUT,
        );
        assert_eq!(
            requests,
            [
                SpaceRequest {
                    device: device(1),
                    mount_point: PathBuf::from("/a/1"),
                },
                SpaceRequest {
                    device: device(2),
                    mount_point: PathBuf::from("/b/2"),
                },
            ]
        );
        assert!(done.is_none());
        assert!(round.answer(device(1), Ok(space(95)), start).is_none());
        let volumes = round.answer(device(2), Ok(space(10)), start).unwrap();
        assert_eq!(
            spaces(&volumes),
            [
                (Path::new("/a"), VolumeSpace::Known(space(95))),
                (Path::new("/b"), VolumeSpace::Known(space(10))),
            ]
        );
        assert_eq!(round.pending(), 0);
        assert_eq!(round.deadline(), None);
    }

    #[test]
    fn a_silent_volume_does_not_respond_and_its_late_answer_counts_for_the_next_round() {
        let start = Instant::now();
        let mut round = VolumeRound::default();
        round.begin(
            start,
            vec![candidate("/a", &[1]), candidate("/nas", &[2])],
            TIMEOUT,
        );
        assert!(round.answer(device(1), Ok(space(10)), start).is_none());
        assert!(
            round
                .expire(start + TIMEOUT.saturating_sub(Duration::from_millis(1)))
                .is_none()
        );
        let volumes = round.expire(start + TIMEOUT).unwrap();
        assert_eq!(volumes[1].space, VolumeSpace::NotResponding);
        assert_eq!(volumes[0].space, VolumeSpace::Known(space(10)));
        // Next round: the share still reading is not launched again...
        let later = start + Duration::from_secs(15);
        let (requests, _) = round.begin(
            later,
            vec![candidate("/a", &[1]), candidate("/nas", &[2])],
            TIMEOUT,
        );
        assert_eq!(
            requests
                .iter()
                .map(|request| request.device)
                .collect::<Vec<_>>(),
            [device(1)]
        );
        // ...but waited for: its answer, late, counts.
        assert!(round.answer(device(2), Ok(space(30)), start).is_none());
        let volumes = round.answer(device(1), Ok(space(10)), start).unwrap();
        assert_eq!(volumes[1].space, VolumeSpace::Known(space(30)));
        assert_eq!(round.pending(), 0);
    }

    #[test]
    fn a_failure_wins_over_a_silence_and_a_pool_missing_a_dataset_does_not_respond() {
        let start = Instant::now();
        let mut round = VolumeRound::default();
        round.begin(
            start,
            vec![candidate("/x", &[1, 4]), candidate("/pool", &[2, 3])],
            TIMEOUT,
        );
        round.answer(device(1), Err(denied()), start);
        round.answer(device(2), Ok(space(10)), start);
        let volumes = round.expire(start + TIMEOUT).unwrap();
        assert_eq!(volumes[0].space, VolumeSpace::Failed(denied()));
        assert_eq!(volumes[1].space, VolumeSpace::NotResponding);
    }

    #[test]
    fn a_pool_answered_whole_adds_its_datasets() {
        let start = Instant::now();
        let mut round = VolumeRound::default();
        round.begin(start, vec![candidate("/pool", &[2, 3])], TIMEOUT);
        round.answer(device(2), Ok(space(10)), start);
        let volumes = round.answer(device(3), Ok(space(20)), start).unwrap();
        assert_eq!(
            volumes[0].space,
            VolumeSpace::Known(aggregate(&[space(10), space(20)]))
        );
    }

    #[test]
    fn a_pool_of_many_datasets_is_read_whole() {
        let start = Instant::now();
        let mut round = VolumeRound::default();
        let datasets: Vec<u32> = (0..=u32::try_from(MAX_PENDING).unwrap()).collect();
        let (requests, _) = round.begin(start, vec![candidate("/", &datasets)], TIMEOUT);
        assert_eq!(requests.len(), MAX_PENDING + 1);
        let mut finished = None;
        for request in requests {
            finished = round.answer(request.device, Ok(space(1)), start);
        }
        assert!(matches!(finished.unwrap()[0].space, VolumeSpace::Known(_)));
    }

    #[test]
    fn with_many_reads_still_unanswered_none_is_launched() {
        let start = Instant::now();
        let mut round = VolumeRound::default();
        let silent: Vec<Candidate> = (0..u32::try_from(MAX_PENDING).unwrap())
            .map(|minor| candidate(&format!("/m{minor}"), &[minor]))
            .collect();
        round.begin(start, silent.clone(), TIMEOUT);
        round.expire(start + TIMEOUT).unwrap();
        let mut next = silent;
        next.push(candidate("/new", &[99]));
        let (requests, _) = round.begin(start + TIMEOUT, next, TIMEOUT);
        assert_eq!(requests, [], "none launched while the earlier ones hang");
        let mut finished = None;
        for minor in 0..u32::try_from(MAX_PENDING).unwrap() {
            finished = round.answer(device(minor), Ok(space(1)), start + TIMEOUT);
        }
        let volumes = finished.expect("the hanging ones answered: the round ends");
        assert_eq!(volumes.last().unwrap().space, VolumeSpace::NotResponding);
        assert!(
            volumes[0..MAX_PENDING]
                .iter()
                .all(|volume| matches!(volume.space, VolumeSpace::Known(_)))
        );
    }

    #[test]
    fn the_longest_read_is_kept_until_asked() {
        let start = Instant::now();
        let mut round = VolumeRound::default();
        round.begin(
            start,
            vec![candidate("/a", &[1]), candidate("/b", &[2])],
            TIMEOUT,
        );
        round.answer(device(1), Ok(space(1)), start + Duration::from_millis(300));
        round.answer(device(2), Ok(space(1)), start + Duration::from_millis(100));
        assert_eq!(round.take_slowest(), Some(Duration::from_millis(300)));
        assert_eq!(round.take_slowest(), None);
    }

    #[test]
    fn a_round_with_nothing_to_read_ends_at_once_and_an_abandoned_one_only_frees() {
        let start = Instant::now();
        let mut round = VolumeRound::default();
        let (requests, done) = round.begin(start, Vec::new(), TIMEOUT);
        assert_eq!(requests, []);
        assert_eq!(done, Some(Vec::new()));
        round.begin(start, vec![candidate("/a", &[1])], TIMEOUT);
        round.abandon();
        assert_eq!(round.deadline(), None);
        assert!(round.answer(device(1), Ok(space(1)), start).is_none());
        assert_eq!(round.pending(), 0);
    }
}
