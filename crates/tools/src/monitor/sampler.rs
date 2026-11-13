use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use okapi_platform::{
    CpuTimes, DesktopSession, Device, MemoryStatus, Mount, MountClass, MountList, ProcessList,
    Space,
};
use okapi_rules::Language;

use super::memory::{Applications, DesktopNames, MemoryReading, group};
use super::processor::{Processor, ProcessorReading};
use super::round::{SpaceAnswer, SpaceRequest, VolumeRound};
use super::schedule::{Schedule, Source};
use super::system::SystemReading;
use super::volumes::{Volume, VolumeKind, VolumeSpace, candidates};

const THREAD_NAME: &str = "okapi-dashboard";
const SPACE_THREAD_NAME: &str = "okapi-space";
/// A read of space needs little stack: one system call, one message.
const SPACE_STACK: usize = 64 * 1024;
/// The counts are summed up in the journal this often at most.
const SUMMARY_EVERY: Duration = Duration::from_mins(1);

/// Why something could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable {
    /// [`io::ErrorKind::Unsupported`] on a system Okapi cannot read yet.
    pub kind: io::ErrorKind,
    /// The system's error code, when it gave one.
    pub code: Option<i32>,
}

impl Unavailable {
    fn of(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            code: error.raw_os_error(),
        }
    }
}

/// One reading, sent as soon as it is made.
#[derive(Debug, Clone, PartialEq)]
pub enum Sample {
    Volumes(Result<Vec<Volume>, Unavailable>),
    /// When the memory cannot be read, its applications are not read either.
    Memory(Result<MemoryReading, Unavailable>),
    Processor(Result<ProcessorReading, Unavailable>),
    System(SystemReading),
}

/// What the sampler is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Follow these sources (the widgets shown, at their sizes); following
    /// the applications reads the memory too.
    Watch(BTreeSet<Source>),
    /// Read nothing.
    Pause,
    /// Read everything followed at once, then as usual.
    Resume,
    /// Read the volumes at once.
    RefreshVolumes,
    Stop,
}

enum Message {
    Command(Command),
    Space { device: Device, answer: SpaceAnswer },
}

type Read<T> = Arc<dyn Fn() -> io::Result<T> + Send + Sync>;
type ReadAt<T> = Arc<dyn Fn(&Path) -> io::Result<T> + Send + Sync>;
type NameApplication = Arc<dyn Fn(&str, Language) -> io::Result<Option<String>> + Send + Sync>;

/// How the sampler reads the system: the platform's readers, or doubles.
#[derive(Clone)]
pub(crate) struct Readers {
    pub mounts: Read<MountList>,
    /// May block for as long as a lost share does not answer.
    pub space: ReadAt<Space>,
    pub labels: Read<Vec<(PathBuf, String)>>,
    pub canonical: ReadAt<PathBuf>,
    pub removable: ReadAt<bool>,
    pub memory: Read<MemoryStatus>,
    pub processes: Read<ProcessList>,
    pub cpu_times: Read<CpuTimes>,
    pub load_average: Read<f64>,
    pub distribution: Read<Option<String>>,
    pub kernel: Read<String>,
    pub uptime: Read<Duration>,
    pub desktop_session: Arc<dyn Fn() -> DesktopSession + Send + Sync>,
    pub application_name: NameApplication,
}

impl Readers {
    pub(crate) fn system() -> Self {
        Self {
            mounts: Arc::new(okapi_platform::mounts),
            space: Arc::new(okapi_platform::space),
            labels: Arc::new(okapi_platform::device_labels),
            canonical: Arc::new(|path: &Path| std::fs::canonicalize(path)),
            removable: Arc::new(okapi_platform::removable),
            memory: Arc::new(okapi_platform::memory),
            processes: Arc::new(okapi_platform::processes),
            cpu_times: Arc::new(okapi_platform::cpu_times),
            load_average: Arc::new(okapi_platform::load_average),
            distribution: Arc::new(okapi_platform::distribution),
            kernel: Arc::new(okapi_platform::kernel_release),
            uptime: Arc::new(okapi_platform::uptime),
            desktop_session: Arc::new(okapi_platform::desktop_session),
            application_name: Arc::new(okapi_platform::application_name),
        }
    }
}

/// The thread that reads what the dashboard follows. Dropping it tells the
/// thread to stop, without waiting for it: a read of space may be blocked on
/// a lost share, and the interface must never wait.
pub struct Sampler {
    inbox: Sender<Message>,
    thread: Option<JoinHandle<()>>,
    gone_said: AtomicBool,
}

impl std::fmt::Debug for Sampler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Sampler").finish_non_exhaustive()
    }
}

impl Sampler {
    /// Starts the thread, following nothing until told; it sends its
    /// readings to `sink`, the names of applications in `language`, and
    /// gives a filesystem `space_timeout` to tell its space.
    ///
    /// # Errors
    ///
    /// The system's, when the thread cannot be started.
    pub fn start(
        sink: Sender<Sample>,
        language: Language,
        space_timeout: Duration,
    ) -> io::Result<Self> {
        Self::start_with(Readers::system(), sink, language, space_timeout)
    }

    pub(crate) fn start_with(
        readers: Readers,
        sink: Sender<Sample>,
        language: Language,
        space_timeout: Duration,
    ) -> io::Result<Self> {
        let (inbox, messages) = mpsc::channel();
        let worker = Worker {
            readers,
            sink,
            messages,
            outbox: inbox.clone(),
            language,
            space_timeout,
            schedule: Schedule::default(),
            round: VolumeRound::default(),
            processor: Processor::default(),
            names: DesktopNames::default(),
            kinds: HashMap::new(),
            kinds_unknown: BTreeSet::new(),
            health: BTreeMap::new(),
            states: BTreeMap::new(),
            summary: Summary::default(),
        };
        let thread = thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || worker.run())?;
        Ok(Self {
            inbox,
            thread: Some(thread),
            gone_said: AtomicBool::new(false),
        })
    }

    /// Tells the thread `command`; a thread gone is said once.
    pub fn send(&self, command: Command) {
        if self.inbox.send(Message::Command(command)).is_err()
            && !self.gone_said.swap(true, Ordering::Relaxed)
        {
            tracing::warn!("dashboard sampler gone, commands dropped");
        }
    }

    /// Stops the thread and waits for it: for tests, which must not leave a
    /// thread behind.
    #[cfg(test)]
    pub(crate) fn stop_and_join(mut self) {
        self.send(Command::Stop);
        self.join();
    }

    /// Waits for the thread to end by itself: for tests.
    #[cfg(test)]
    pub(crate) fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            assert!(thread.join().is_ok(), "the sampler panicked");
        }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        if self.thread.take().is_some() {
            // A thread that already ended (nobody received its readings) has
            // nothing left to stop.
            self.inbox.send(Message::Command(Command::Stop)).ok();
        }
    }
}

/// The counts summed up in the journal.
#[derive(Debug, Default)]
struct Summary {
    last: Option<Instant>,
    volumes: usize,
    set_aside: BTreeMap<&'static str, usize>,
    malformed: usize,
    processes: usize,
    unreadable: usize,
    applications: usize,
    /// The longest reading of each source since the last summary.
    slowest: BTreeMap<Source, Duration>,
}

struct Worker {
    readers: Readers,
    sink: Sender<Sample>,
    messages: Receiver<Message>,
    /// For the reads of space, which answer here.
    outbox: Sender<Message>,
    language: Language,
    space_timeout: Duration,
    schedule: Schedule,
    round: VolumeRound,
    processor: Processor,
    names: DesktopNames,
    /// Whether each filesystem is removable, once read.
    kinds: HashMap<Device, VolumeKind>,
    /// Those whose kind could not be read: read again, said once.
    kinds_unknown: BTreeSet<Device>,
    /// Whether each source or part was readable last time: said when it
    /// changes, never at each reading.
    health: BTreeMap<&'static str, bool>,
    /// What each volume's space was last time: said when it changes.
    states: BTreeMap<PathBuf, VolumeSpace>,
    summary: Summary,
}

impl Worker {
    fn run(mut self) {
        let _span = tracing::info_span!("dashboard").entered();
        tracing::info!("dashboard sampler started");
        loop {
            let now = Instant::now();
            if !self.read_due(now) {
                break;
            }
            let deadline = [self.schedule.next_deadline(), self.round.deadline()]
                .into_iter()
                .flatten()
                .min();
            // The thread keeps a sender for the reads of space: it never sees
            // a disconnection, only `Stop` ends it.
            let message = match deadline {
                Some(deadline) => self
                    .messages
                    .recv_timeout(deadline.saturating_duration_since(Instant::now())),
                None => self
                    .messages
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
            };
            match message {
                Ok(Message::Command(Command::Stop)) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Message::Command(command)) => self.command(command, Instant::now()),
                Ok(Message::Space { device, answer }) => {
                    if let Some(volumes) = self.round.answer(device, answer, Instant::now())
                        && !self.send_volumes(volumes)
                    {
                        break;
                    }
                }
                // Only once the queue is empty: answers already arrived
                // always count before the round ends.
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(volumes) = self.round.expire(Instant::now())
                        && !self.send_volumes(volumes)
                    {
                        break;
                    }
                }
            }
        }
        tracing::info!(pending = self.round.pending(), "dashboard sampler stopped");
    }

    fn command(&mut self, command: Command, now: Instant) {
        match command {
            Command::Watch(sources) => {
                tracing::info!(?sources, "dashboard sources followed");
                if !self.schedule.follows(Source::Processor) {
                    // Followed again: the time it was not is never measured.
                    self.processor.reset();
                }
                self.schedule.watch(&sources, now);
            }
            Command::Pause => {
                tracing::info!("dashboard sampler paused");
                self.schedule.pause();
                self.round.abandon();
            }
            Command::Resume => {
                tracing::info!("dashboard sampler resumed");
                self.schedule.resume(now);
                self.processor.reset();
            }
            Command::RefreshVolumes => self.schedule.refresh_volumes(now),
            Command::Stop => {}
        }
    }

    /// Reads what is due; `false` when nobody receives the readings.
    fn read_due(&mut self, now: Instant) -> bool {
        let due = self.schedule.due(now);
        if due.contains(&Source::Memory) || due.contains(&Source::Applications) {
            let started = Instant::now();
            let reading = self.read_memory(now, due.contains(&Source::Applications));
            self.timed(Source::Memory, started);
            if !self.emit(Sample::Memory(reading)) {
                return false;
            }
        }
        if due.contains(&Source::Processor) {
            let started = Instant::now();
            let reading = self.read_processor();
            self.timed(Source::Processor, started);
            if !self.emit(Sample::Processor(reading)) {
                return false;
            }
        }
        if due.contains(&Source::System) {
            let started = Instant::now();
            let reading = self.read_system();
            self.timed(Source::System, started);
            if !self.emit(Sample::System(reading)) {
                return false;
            }
        }
        // Last: the other readings never eat into the round's time.
        if due.contains(&Source::Volumes) {
            let started = Instant::now();
            // Dated from now: the readings before took their own time.
            let going = self.begin_volumes(started);
            self.timed(Source::Volumes, started);
            if !going {
                return false;
            }
        }
        if !due.is_empty() {
            self.summarize(now);
        }
        true
    }

    /// Keeps the longest reading of `source` for the summary.
    fn timed(&mut self, source: Source, started: Instant) {
        let took = started.elapsed();
        let slowest = self.summary.slowest.entry(source).or_default();
        *slowest = (*slowest).max(took);
    }

    fn emit(&self, sample: Sample) -> bool {
        let sent = self.sink.send(sample).is_ok();
        if !sent {
            tracing::info!("nobody receives the dashboard's readings, sampler stopping");
        }
        sent
    }

    /// `result`, the health of `part` traced when it changes.
    fn available<T>(
        &mut self,
        part: &'static str,
        result: io::Result<T>,
    ) -> Result<T, Unavailable> {
        let was = self.health.insert(part, result.is_ok());
        match &result {
            Err(error) if was != Some(false) => {
                tracing::warn!(part, %error, code = error.raw_os_error(), "dashboard reading unavailable");
            }
            Ok(_) if was == Some(false) => tracing::info!(part, "dashboard reading back"),
            _ => {}
        }
        result.map_err(|error| Unavailable::of(&error))
    }

    fn begin_volumes(&mut self, now: Instant) -> bool {
        if let Some(deadline) = self.round.deadline() {
            // A round under way is never forgotten; the volumes are read
            // again once it ends.
            self.schedule.refresh_volumes(deadline);
            return true;
        }
        let list = match self.available("volumes", (self.readers.mounts)()) {
            Ok(list) => list,
            Err(unavailable) => return self.emit(Sample::Volumes(Err(unavailable))),
        };
        if list.malformed != self.summary.malformed {
            if list.malformed == 0 {
                tracing::info!("every mount line understood again");
            } else {
                tracing::warn!(malformed = list.malformed, "mount lines not understood");
            }
            self.summary.malformed = list.malformed;
        }
        let labels = self
            .available("labels", (self.readers.labels)())
            .unwrap_or_default();
        self.learn_kinds(&list.mounts);
        let readers = &self.readers;
        let kinds = &self.kinds;
        let selection = candidates(
            &list.mounts,
            |mount| label_of(readers, &labels, mount),
            |mount| {
                kinds
                    .get(&mount.device)
                    .copied()
                    .unwrap_or(VolumeKind::Local)
            },
        );
        self.summary.volumes = selection.candidates.len();
        self.summary.set_aside = selection.set_aside;
        let (requests, done) = self
            .round
            .begin(now, selection.candidates, self.space_timeout);
        let mut finished = done;
        for request in requests {
            if let Some(volumes) = self.launch(request) {
                finished = Some(volumes);
            }
        }
        finished.is_none_or(|volumes| self.send_volumes(volumes))
    }

    /// Reads whether each mounted block device can be unplugged, once it is
    /// known (until it is unmounted); one that cannot be read is said local
    /// for now, read again, and said once.
    fn learn_kinds(&mut self, mounts: &[Mount]) {
        let mounted: BTreeSet<Device> = mounts.iter().map(|mount| mount.device).collect();
        self.kinds.retain(|device, _| mounted.contains(device));
        self.kinds_unknown.retain(|device| mounted.contains(device));
        for mount in mounts
            .iter()
            .filter(|mount| mount.class == MountClass::Block)
        {
            if self.kinds.contains_key(&mount.device) {
                continue;
            }
            match (self.readers.removable)(Path::new(&mount.source)) {
                Ok(removable) => {
                    self.kinds_unknown.remove(&mount.device);
                    let kind = if removable {
                        VolumeKind::Removable
                    } else {
                        VolumeKind::Local
                    };
                    self.kinds.insert(mount.device, kind);
                }
                Err(error) => {
                    if self.kinds_unknown.insert(mount.device) {
                        tracing::warn!(
                            source = mount.source,
                            %error,
                            code = error.raw_os_error(),
                            "whether a volume is removable unknown, said local for now"
                        );
                    }
                }
            }
        }
    }

    /// Launches a read of space on its own short thread. Returns the volumes
    /// when this read cannot be launched and that ends the round.
    fn launch(&mut self, request: SpaceRequest) -> Option<Vec<Volume>> {
        let SpaceRequest {
            device,
            mount_point,
        } = request;
        let point = mount_point.display().to_string();
        let outbox = self.outbox.clone();
        let space = Arc::clone(&self.readers.space);
        let spawned = thread::Builder::new()
            .name(SPACE_THREAD_NAME.to_owned())
            .stack_size(SPACE_STACK)
            .spawn(move || {
                // A panic would leave the read in flight for good.
                let answer =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| space(&mount_point)))
                        .unwrap_or_else(|_| Err(io::Error::other("the read of space panicked")))
                        .map_err(|error| Unavailable::of(&error));
                // Nobody to tell once the sampler has stopped.
                outbox.send(Message::Space { device, answer }).ok();
            });
        match spawned {
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(mount_point = point, %error, "read of space not started");
                self.round
                    .answer(device, Err(Unavailable::of(&error)), Instant::now())
            }
        }
    }

    /// Sends the volumes, each one whose space is not known said when it
    /// appears so or becomes so, each change traced.
    fn send_volumes(&mut self, volumes: Vec<Volume>) -> bool {
        let mut seen = BTreeMap::new();
        for volume in &volumes {
            let was = self.states.get(&volume.mount_point);
            let changed = was.is_none_or(|was| !same_state(was, &volume.space));
            if changed {
                trace_volume(volume, was.is_none(), self.space_timeout);
            }
            seen.insert(volume.mount_point.clone(), volume.space);
        }
        for gone in self
            .states
            .keys()
            .filter(|point| !seen.contains_key(*point))
        {
            tracing::info!(mount_point = %gone.display(), "volume gone");
        }
        self.states = seen;
        self.emit(Sample::Volumes(Ok(volumes)))
    }

    fn read_memory(
        &mut self,
        now: Instant,
        with_applications: bool,
    ) -> Result<MemoryReading, Unavailable> {
        let status = self.available("memory", (self.readers.memory)())?;
        let applications = with_applications.then(|| self.read_applications(now));
        Ok(MemoryReading {
            status,
            applications,
        })
    }

    fn read_applications(&mut self, now: Instant) -> Result<Applications, Unavailable> {
        let list = self.available("applications", (self.readers.processes)())?;
        let readers = &self.readers;
        let language = self.language;
        let names = &mut self.names;
        let top = group(&list.processes, |identifier| {
            names.name(identifier, now, |identifier| {
                (readers.application_name)(identifier, language)
                    .inspect_err(|error| {
                        // At most once per identifier and recheck: the names
                        // are kept by `DesktopNames`.
                        tracing::debug!(identifier, %error, "desktop file unreadable");
                    })
                    .ok()
                    .flatten()
            })
        });
        self.names.end_reading();
        self.summary.processes = list.processes.len();
        self.summary.unreadable = list.unreadable;
        self.summary.applications = top.len();
        Ok(Applications {
            top,
            unreadable: list.unreadable,
        })
    }

    fn read_processor(&mut self) -> Result<ProcessorReading, Unavailable> {
        let times = self.available("processor", (self.readers.cpu_times)())?;
        let load_average = self
            .available("load average", (self.readers.load_average)())
            .ok();
        Ok(ProcessorReading {
            usage: self.processor.usage(times),
            load_average,
        })
    }

    fn read_system(&mut self) -> SystemReading {
        let distribution = (self.readers.distribution)();
        let kernel = (self.readers.kernel)();
        let uptime = (self.readers.uptime)();
        let DesktopSession { desktops, session } = (self.readers.desktop_session)();
        SystemReading {
            distribution: self.available("distribution", distribution).ok().flatten(),
            kernel: self.available("kernel", kernel).ok(),
            desktops,
            session,
            uptime: self.available("uptime", uptime).ok(),
        }
    }

    fn summarize(&mut self, now: Instant) {
        if self
            .summary
            .last
            .is_some_and(|last| now.duration_since(last) < SUMMARY_EVERY)
        {
            return;
        }
        self.summary.last = Some(now);
        let slowest_ms: BTreeMap<Source, u128> = std::mem::take(&mut self.summary.slowest)
            .into_iter()
            .map(|(source, took)| (source, took.as_millis()))
            .collect();
        let slowest_space_ms = self.round.take_slowest().map(|took| took.as_millis());
        let summary = &self.summary;
        tracing::info!(
            ?slowest_ms,
            slowest_space_ms,
            volumes = summary.volumes,
            set_aside = ?summary.set_aside,
            malformed_mounts = summary.malformed,
            processes = summary.processes,
            unreadable_processes = summary.unreadable,
            applications = summary.applications,
            pending_space_reads = self.round.pending(),
            "dashboard readings"
        );
    }
}

/// Whether two spaces are the same state: a known space that changed is
/// still known (said only by the reading).
fn same_state(first: &VolumeSpace, second: &VolumeSpace) -> bool {
    match (first, second) {
        (VolumeSpace::Known(_), VolumeSpace::Known(_)) => true,
        _ => first == second,
    }
}

fn trace_volume(volume: &Volume, appeared: bool, timeout: Duration) {
    let point = volume.mount_point.display();
    match volume.space {
        VolumeSpace::Known(_) if appeared => {
            tracing::info!(mount_point = %point, filesystem = volume.filesystem, "volume appeared");
        }
        VolumeSpace::Known(_) => tracing::info!(mount_point = %point, "volume answers"),
        VolumeSpace::NotResponding => {
            tracing::warn!(
                mount_point = %point,
                filesystem = volume.filesystem,
                waited_ms = timeout.as_millis(),
                appeared,
                "volume not responding"
            );
        }
        VolumeSpace::Failed(unavailable) => {
            tracing::warn!(
                mount_point = %point,
                filesystem = volume.filesystem,
                kind = ?unavailable.kind,
                code = unavailable.code,
                appeared,
                "space of a volume unreadable"
            );
        }
    }
}

/// The label of `mount`'s filesystem, by its device (resolved); a device
/// that cannot be resolved has none.
fn label_of(readers: &Readers, labels: &[(PathBuf, String)], mount: &Mount) -> Option<String> {
    if mount.class != MountClass::Block {
        return None;
    }
    let device = (readers.canonical)(Path::new(&mount.source)).ok()?;
    labels
        .iter()
        .find(|(labelled, _)| *labelled == device)
        .map(|(_, label)| label.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread::ThreadId;

    use okapi_platform::ProcessMemory;

    use super::super::ApplicationName;
    use super::*;

    /// Long enough for any machine, never waited unless something is wrong.
    const WAIT: Duration = Duration::from_secs(10);
    /// For the test whose read of space blocks.
    const SHORT_TIMEOUT: Duration = Duration::from_millis(50);

    fn mount(point: &str, device: (u32, u32), source: &str) -> Mount {
        Mount {
            id: 1,
            device: Device {
                major: device.0,
                minor: device.1,
            },
            mount_point: PathBuf::from(point),
            filesystem: "ext4".to_owned(),
            source: source.to_owned(),
            class: MountClass::Block,
        }
    }

    fn space() -> Space {
        Space {
            total: 100,
            available: 60,
            used: 40,
        }
    }

    fn doubles() -> Readers {
        Readers {
            mounts: Arc::new(|| {
                Ok(MountList {
                    mounts: vec![mount("/", (259, 7), "/dev/nvme0n1p7")],
                    malformed: 0,
                })
            }),
            space: Arc::new(|_| Ok(space())),
            labels: Arc::new(|| Ok(Vec::new())),
            canonical: Arc::new(|path| Ok(path.to_path_buf())),
            removable: Arc::new(|_| Ok(false)),
            memory: Arc::new(|| {
                Ok(MemoryStatus {
                    total: 8,
                    available: 3,
                    swap_total: 0,
                    swap_free: 0,
                })
            }),
            processes: Arc::new(|| {
                Ok(ProcessList {
                    processes: vec![ProcessMemory {
                        pid: 1,
                        name: "chrome".to_owned(),
                        resident_bytes: 30,
                        cgroup: Some("/app.slice/app-com.google.Chrome-1.scope".to_owned()),
                    }],
                    unreadable: 2,
                })
            }),
            cpu_times: Arc::new(|| Ok(CpuTimes { busy: 1, total: 2 })),
            load_average: Arc::new(|| Ok(0.5)),
            distribution: Arc::new(|| Ok(Some("Ubuntu".to_owned()))),
            kernel: Arc::new(|| Ok("7.0".to_owned())),
            uptime: Arc::new(|| Ok(Duration::from_secs(60))),
            desktop_session: Arc::new(|| DesktopSession {
                desktops: vec!["ubuntu".to_owned(), "GNOME".to_owned()],
                session: Some("wayland".to_owned()),
            }),
            application_name: Arc::new(|identifier, language| {
                Ok((identifier == "com.google.Chrome")
                    .then(|| format!("Google Chrome {language:?}")))
            }),
        }
    }

    fn start_with(readers: Readers, timeout: Duration) -> (Sampler, Receiver<Sample>) {
        let (sink, readings) = mpsc::channel();
        let sampler = Sampler::start_with(readers, sink, Language::French, timeout).unwrap();
        (sampler, readings)
    }

    fn start(readers: Readers) -> (Sampler, Receiver<Sample>) {
        start_with(readers, WAIT)
    }

    fn watch(sampler: &Sampler, sources: &[Source]) {
        sampler.send(Command::Watch(sources.iter().copied().collect()));
    }

    fn volumes(readings: &Receiver<Sample>) -> Vec<Volume> {
        loop {
            if let Sample::Volumes(volumes) = readings.recv_timeout(WAIT).unwrap() {
                return volumes.unwrap();
            }
        }
    }

    #[test]
    fn the_memory_followed_is_read_with_its_applications_by_name() {
        let (sampler, readings) = start(doubles());
        watch(&sampler, &[Source::Memory, Source::Applications]);
        match readings.recv_timeout(WAIT).unwrap() {
            Sample::Memory(Ok(reading)) => {
                assert_eq!(reading.status.total, 8);
                let applications = reading.applications.unwrap().unwrap();
                assert_eq!(applications.unreadable, 2);
                assert_eq!(
                    applications.top[0].name,
                    ApplicationName::Desktop("Google Chrome French".to_owned())
                );
            }
            other => panic!("{other:?}"),
        }
        sampler.stop_and_join();
    }

    #[test]
    fn an_application_whose_file_cannot_be_read_goes_by_its_process_name() {
        let readers = Readers {
            application_name: Arc::new(|_, _| Err(io::ErrorKind::InvalidData.into())),
            ..doubles()
        };
        let (sampler, readings) = start(readers);
        watch(&sampler, &[Source::Applications]);
        match readings.recv_timeout(WAIT).unwrap() {
            Sample::Memory(Ok(reading)) => assert_eq!(
                reading.applications.unwrap().unwrap().top[0].name,
                ApplicationName::Process("chrome".to_owned())
            ),
            other => panic!("{other:?}"),
        }
        sampler.stop_and_join();
    }

    #[test]
    fn a_source_that_cannot_be_read_is_unavailable_with_its_reason() {
        let readers = Readers {
            memory: Arc::new(|| Err(io::Error::from_raw_os_error(13))),
            ..doubles()
        };
        let (sampler, readings) = start(readers);
        watch(&sampler, &[Source::Memory]);
        assert_eq!(
            readings.recv_timeout(WAIT).unwrap(),
            Sample::Memory(Err(Unavailable {
                kind: io::ErrorKind::PermissionDenied,
                code: Some(13),
            }))
        );
        sampler.stop_and_join();
    }

    #[test]
    fn the_volumes_answered_are_known_named_by_label_and_kind() {
        let readers = Readers {
            mounts: Arc::new(|| {
                Ok(MountList {
                    mounts: vec![
                        mount("/", (259, 7), "/dev/nvme0n1p7"),
                        mount("/mnt/x", (8, 1), "/dev/disk/by-uuid/abc"),
                        mount("/mnt/y", (8, 17), "/dev/sdc1"),
                    ],
                    malformed: 0,
                })
            }),
            canonical: Arc::new(|path| {
                Ok(if path == Path::new("/dev/disk/by-uuid/abc") {
                    PathBuf::from("/dev/sdb1")
                } else {
                    path.to_path_buf()
                })
            }),
            labels: Arc::new(|| Ok(vec![(PathBuf::from("/dev/sdb1"), "KINGSTON".to_owned())])),
            removable: Arc::new(|source| match source.to_str() {
                Some("/dev/disk/by-uuid/abc") => Ok(true),
                Some("/dev/sdc1") => Err(io::ErrorKind::PermissionDenied.into()),
                _ => Ok(false),
            }),
            ..doubles()
        };
        let (sampler, readings) = start(readers);
        watch(&sampler, &[Source::Volumes]);
        let volumes = volumes(&readings);
        assert_eq!(volumes[0].space, VolumeSpace::Known(space()));
        assert_eq!(
            volumes[1].name,
            super::super::VolumeName::Label("KINGSTON".to_owned())
        );
        let kinds: Vec<VolumeKind> = volumes.iter().map(|volume| volume.kind).collect();
        assert_eq!(
            kinds,
            [VolumeKind::Local, VolumeKind::Removable, VolumeKind::Local]
        );
        sampler.stop_and_join();
    }

    #[test]
    fn mounts_or_a_space_that_cannot_be_read_are_said_with_their_code() {
        let readers = Readers {
            mounts: Arc::new(|| Err(io::Error::from_raw_os_error(13))),
            ..doubles()
        };
        let (sampler, readings) = start(readers);
        watch(&sampler, &[Source::Volumes]);
        assert_eq!(
            readings.recv_timeout(WAIT).unwrap(),
            Sample::Volumes(Err(Unavailable {
                kind: io::ErrorKind::PermissionDenied,
                code: Some(13),
            }))
        );
        sampler.stop_and_join();
        let readers = Readers {
            space: Arc::new(|_| Err(io::Error::from_raw_os_error(5))),
            ..doubles()
        };
        let (sampler, readings) = start(readers);
        watch(&sampler, &[Source::Volumes]);
        let failed = volumes(&readings)[0].space;
        assert!(
            matches!(
                failed,
                VolumeSpace::Failed(Unavailable { code: Some(5), .. })
            ),
            "{failed:?}"
        );
        sampler.stop_and_join();
    }

    #[test]
    fn a_volume_that_blocks_does_not_respond_and_never_holds_the_rest_back() {
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let readers = Readers {
            space: Arc::new(move |_| {
                // Blocks until the test lets it go, like a lost share.
                gate.lock().unwrap().recv().ok();
                Ok(space())
            }),
            ..doubles()
        };
        let (sampler, readings) = start_with(readers, SHORT_TIMEOUT);
        watch(&sampler, &[Source::Volumes, Source::Memory]);
        let mut memory = false;
        let volumes = loop {
            match readings.recv_timeout(WAIT).unwrap() {
                Sample::Volumes(volumes) => break volumes.unwrap(),
                Sample::Memory(_) => memory = true,
                _ => {}
            }
        };
        assert!(memory, "the memory came while the volume was blocked");
        assert_eq!(volumes[0].space, VolumeSpace::NotResponding);
        // Stopping never waits for the blocked read.
        sampler.stop_and_join();
        drop(release);
    }

    #[test]
    fn a_slow_reading_never_makes_a_sound_volume_silent() {
        let readers = Readers {
            processes: Arc::new(|| {
                thread::sleep(SHORT_TIMEOUT * 4);
                Ok(ProcessList::default())
            }),
            ..doubles()
        };
        let (sampler, readings) = start_with(readers, SHORT_TIMEOUT);
        watch(&sampler, &[Source::Volumes, Source::Applications]);
        assert_eq!(volumes(&readings)[0].space, VolumeSpace::Known(space()));
        sampler.stop_and_join();
    }

    #[test]
    fn a_refresh_during_a_round_reads_the_volumes_again_after_it() {
        let reads = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&reads);
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let readers = Readers {
            mounts: Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(MountList {
                    mounts: vec![mount("/", (259, 7), "/dev/nvme0n1p7")],
                    malformed: 0,
                })
            }),
            space: Arc::new(move |_| {
                gate.lock().unwrap().recv().ok();
                Ok(space())
            }),
            ..doubles()
        };
        let (sampler, readings) = start_with(readers, SHORT_TIMEOUT);
        watch(&sampler, &[Source::Volumes]);
        sampler.send(Command::RefreshVolumes);
        volumes(&readings);
        volumes(&readings);
        assert!(reads.load(Ordering::SeqCst) >= 2, "the mounts read again");
        sampler.stop_and_join();
        drop(release);
    }

    #[test]
    fn a_pause_forgets_the_round_under_way() {
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let readers = Readers {
            space: Arc::new(move |_| {
                gate.lock().unwrap().recv().ok();
                Ok(space())
            }),
            ..doubles()
        };
        let (sampler, readings) = start_with(readers, SHORT_TIMEOUT);
        watch(&sampler, &[Source::Volumes]);
        sampler.send(Command::Pause);
        // Waiting is the only way to see nothing come: long past the round's
        // deadline, nothing may.
        assert!(
            readings.recv_timeout(SHORT_TIMEOUT * 10).is_err(),
            "no volumes sent while paused"
        );
        sampler.stop_and_join();
        drop(release);
    }

    #[test]
    fn a_read_of_space_that_panics_is_a_failure_never_a_read_in_flight() {
        let readers = Readers {
            space: Arc::new(|_| panic!("a reader that panics")),
            ..doubles()
        };
        let (sampler, readings) = start(readers);
        watch(&sampler, &[Source::Volumes]);
        assert!(matches!(
            volumes(&readings)[0].space,
            VolumeSpace::Failed(_)
        ));
        sampler.stop_and_join();
    }

    #[test]
    fn nothing_is_read_on_the_caller_s_thread() {
        let threads: Arc<Mutex<Vec<ThreadId>>> = Arc::default();
        let recorded = Arc::clone(&threads);
        let readers = Readers {
            memory: Arc::new(move || {
                recorded.lock().unwrap().push(thread::current().id());
                Ok(MemoryStatus {
                    total: 1,
                    available: 1,
                    swap_total: 0,
                    swap_free: 0,
                })
            }),
            ..doubles()
        };
        let (sampler, readings) = start(readers);
        watch(&sampler, &[Source::Memory]);
        readings.recv_timeout(WAIT).unwrap();
        sampler.stop_and_join();
        let threads = threads.lock().unwrap();
        assert!(!threads.is_empty());
        assert!(threads.iter().all(|id| *id != thread::current().id()));
    }

    fn counting_processor() -> Readers {
        let ticks = Arc::new(AtomicU64::new(0));
        Readers {
            cpu_times: Arc::new(move || {
                let total = ticks.fetch_add(100, Ordering::SeqCst) + 100;
                Ok(CpuTimes {
                    busy: total / 4,
                    total,
                })
            }),
            ..doubles()
        }
    }

    fn processor(readings: &Receiver<Sample>) -> ProcessorReading {
        loop {
            if let Sample::Processor(reading) = readings.recv_timeout(WAIT).unwrap() {
                return reading.unwrap();
            }
        }
    }

    #[test]
    fn a_resumption_measures_the_processor_afresh() {
        let (sampler, readings) = start(counting_processor());
        watch(&sampler, &[Source::Processor]);
        let first = processor(&readings);
        assert_eq!(first.usage, None, "nothing to compare with yet");
        assert_eq!(first.load_average, Some(0.5));
        assert_eq!(processor(&readings).usage, Some(0.25));
        sampler.send(Command::Pause);
        sampler.send(Command::Resume);
        assert_eq!(
            processor(&readings).usage,
            None,
            "the pause is never measured"
        );
        sampler.stop_and_join();
    }

    #[test]
    fn a_processor_followed_again_is_measured_afresh() {
        let (sampler, readings) = start(counting_processor());
        watch(&sampler, &[Source::Processor]);
        processor(&readings);
        assert_eq!(processor(&readings).usage, Some(0.25));
        watch(&sampler, &[]);
        watch(&sampler, &[Source::Processor]);
        assert_eq!(
            processor(&readings).usage,
            None,
            "the time it was not followed"
        );
        sampler.stop_and_join();
    }

    #[test]
    fn a_processor_still_followed_keeps_its_measure() {
        let (sampler, readings) = start(counting_processor());
        watch(&sampler, &[Source::Processor]);
        assert_eq!(processor(&readings).usage, None);
        watch(&sampler, &[Source::Processor, Source::System]);
        assert_eq!(processor(&readings).usage, Some(0.25));
        sampler.stop_and_join();
    }

    #[test]
    fn the_system_tells_each_of_its_parts() {
        let (sampler, readings) = start(doubles());
        watch(&sampler, &[Source::System]);
        assert_eq!(
            readings.recv_timeout(WAIT).unwrap(),
            Sample::System(SystemReading {
                distribution: Some("Ubuntu".to_owned()),
                kernel: Some("7.0".to_owned()),
                desktops: vec!["ubuntu".to_owned(), "GNOME".to_owned()],
                session: Some("wayland".to_owned()),
                uptime: Some(Duration::from_secs(60)),
            })
        );
        sampler.stop_and_join();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_system_s_own_volumes_are_read() {
        let (sink, readings) = mpsc::channel();
        let sampler = Sampler::start(sink, Language::French, Duration::from_secs(2)).unwrap();
        watch(&sampler, &[Source::Volumes]);
        assert!(matches!(
            readings.recv_timeout(WAIT).unwrap(),
            Sample::Volumes(Ok(_))
        ));
        sampler.stop_and_join();
    }

    #[test]
    fn with_nobody_to_receive_the_sampler_stops_by_itself() {
        let (mut sampler, readings) = start(doubles());
        drop(readings);
        watch(&sampler, &[Source::Memory]);
        // No `Stop` sent: the thread ends because nobody receives.
        sampler.join();
    }
}
