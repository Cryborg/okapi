//! The journal: every run's traces, one JSON line per event, in a file of
//! `$XDG_STATE_HOME/okapi/`, which the developer reads back after a test. Each
//! binary names its own file: a rotation by one never moves the file another
//! is writing to. A journal grown too large is set aside as `<name>.1`, at
//! launch and during a run, between two lines: a run left open for days
//! keeps two journals at most (a rotation that failed is tried again once the
//! journal grew as much again). Two runs of one binary write whole lines, to a
//! file opened for appending, but each counts only its own; every megabyte it
//! writes, and before setting the journal aside, a run looks whether the
//! journal's path still leads to its file, and follows it otherwise (another
//! run set it aside, or a person removed it, or its directory). Lines are
//! lost: what a run writes, until its next look, to a journal no longer
//! named (removed, or set aside twice by another run); where files have no
//! known identity (outside Unix for now) it cannot look, and two runs set
//! each other's journal aside; two runs that cross the limit together, or
//! start together on a journal too large, may both set it aside, the previous
//! journal then lost.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt};

/// The command line's journal file.
pub const CLI_FILE: &str = "okapi.log";
/// The interface's journal file: its own, so that a rotation by the command
/// line never moves the file the interface is writing to.
pub const APP_FILE: &str = "okapi-app.log";

/// The prefix of every target of Okapi's crates (`okapi`, `okapi_engine`...).
const OKAPI_TARGETS: &str = "okapi";

/// What a journal set aside is named after: `<name>.1`.
const ROTATED_SUFFIX: &str = ".1";
/// Beyond this size the journal starts afresh, the previous one kept aside
/// (a full scan writes a few megabytes of detail).
pub const ROTATION_SIZE: u64 = 64 * 1024 * 1024;
/// What a run writes between two looks at where the journal's path leads.
const LOOK_EVERY: u64 = 1024 * 1024;

/// Said once, on the journal's first write that fails, or the first time a
/// run cannot open it again (its lines then go to a file that may no longer
/// be named): a journal that stops filling up must not go unnoticed. The
/// caller tells its user, without tracing.
pub type WriteFailure = Box<dyn Fn(&io::Error) + Send + Sync>;

/// The journal could not be opened.
#[derive(Debug, Error)]
pub enum Unavailable {
    /// Neither an absolute `XDG_STATE_HOME` nor `HOME` says where it goes.
    #[error("no state directory: neither an absolute XDG_STATE_HOME nor HOME")]
    NoStateDirectory,
    /// Its directory or file cannot be created or opened.
    #[error("`{}` cannot be opened: {error}", path.display())]
    Unopenable {
        path: PathBuf,
        #[source]
        error: io::Error,
    },
}

/// Sends every trace to the journal file `file_name` of the state directory
/// (why each entry is held back included), and with `verbose` the detail to
/// standard error too. Without a journal file the run goes on. Gives the
/// journal's path and what became of the previous one, to trace once the
/// run's span is entered.
///
/// # Errors
///
/// [`Unavailable`]: the traces then go to standard error with `verbose`,
/// nowhere without.
///
/// # Panics
///
/// When tracing already has a global subscriber, or `log` a logger: called
/// once per process, before anything sets either.
pub fn start(
    file_name: &str,
    verbose: bool,
    on_write_failure: WriteFailure,
) -> Result<(PathBuf, Rotation), Unavailable> {
    let console_level = if verbose {
        LevelFilter::DEBUG
    } else {
        LevelFilter::OFF
    };
    let opened = okapi_platform::state_directory()
        .ok_or(Unavailable::NoStateDirectory)
        .and_then(|directory| open(&directory, file_name, on_write_failure));
    match opened {
        Ok((journal, rotation)) => {
            let path = journal.path.clone();
            tracing_subscriber::registry()
                .with(journal_layer(journal))
                .with(console(console_level))
                .init();
            Ok((path, rotation))
        }
        Err(unavailable) => {
            tracing_subscriber::registry()
                .with(console(console_level))
                .init();
            Err(unavailable)
        }
    }
}

/// Sends every trace, as the journal holds them, to standard error, for a
/// binary that writes no file of its own: the privileged helper, whose
/// caller adds them to its own journal. The caller reads that stream as it
/// comes, beside the standard output: a full pipe would block the helper.
/// After a panic ([`trace_panics`]), the default report follows in plain text.
///
/// # Panics
///
/// As [`start`].
pub fn start_on_standard_error() {
    tracing_subscriber::registry()
        .with(journal_layer(io::stderr))
        .init();
}

/// The level of `line`, if it is a trace as the journal writes it: the
/// caller of [`start_on_standard_error`] adds it at that level.
#[must_use]
pub fn level_of(line: &str) -> Option<tracing::Level> {
    #[derive(serde::Deserialize)]
    struct Trace {
        level: String,
    }
    serde_json::from_str::<Trace>(line).ok()?.level.parse().ok()
}

/// The journal's form, written to `writer`: one JSON line per event, with
/// every detail of Okapi; from its dependencies (their `log` messages
/// included, bridged), what they think worth saying, not a trace of each
/// message they pass (zbus, for accessibility, would flood the journal).
fn journal_layer<S, W>(writer: W) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let level = Targets::new()
        .with_target(OKAPI_TARGETS, LevelFilter::TRACE)
        .with_default(LevelFilter::INFO);
    fmt::layer().json().with_writer(writer).with_filter(level)
}

/// Traces every panic, with its backtrace, before the default report on
/// standard error: a binary launched from a menu has nobody reading it.
pub fn trace_panics() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%panic, %backtrace, "okapi panicked");
        default_hook(panic);
    }));
}

/// The traces on standard error, readable by a person, from `level` up.
fn console<S>(level: LevelFilter) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    fmt::layer().with_writer(io::stderr).with_filter(level)
}

/// What became of the previous journal.
#[derive(Debug)]
pub enum Rotation {
    None,
    Done {
        /// At launch, the file's size; during a run, what this run counted.
        size: u64,
    },
    /// Not set aside: the lines go on to the same journal.
    Failed(io::Error),
    /// The journal's path led to another file (another run of the same
    /// binary set it aside, or a person removed it): the lines go on to the
    /// file it leads to now.
    Followed,
    /// No journal could be opened at its path: the lines go on to the file
    /// opened before (`<name>.1`, or one no longer named), until a later
    /// look opens one.
    Diverted(io::Error),
}

impl Rotation {
    /// Traces what became of the previous journal, once tracing goes there.
    pub fn trace(&self) {
        match self {
            Self::None => {}
            Self::Done { size } => tracing::info!(size, "journal rotated"),
            Self::Failed(error) => tracing::warn!(%error, "journal rotation failed, appending"),
            Self::Followed => tracing::info!("journal path leads to another file, following it"),
            Self::Diverted(error) => tracing::error!(
                %error,
                "journal cannot be opened again, writing on to the previous file"
            ),
        }
    }
}

/// Opens the journal `file_name` of `directory`, after setting the previous
/// one aside if it grew too large.
fn open(
    directory: &Path,
    file_name: &str,
    on_write_failure: WriteFailure,
) -> Result<(JournalFile, Rotation), Unavailable> {
    let path = directory.join(file_name);
    let unopenable = |error| Unavailable::Unopenable {
        path: path.clone(),
        error,
    };
    okapi_platform::create_private_directory(directory).map_err(unopenable)?;
    let size = fs::symlink_metadata(&path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|metadata| metadata.len());
    let rotation = match size {
        Some(size) if size > ROTATION_SIZE => match fs::rename(&path, aside_path(&path)) {
            Ok(()) => Rotation::Done { size },
            Err(error) => Rotation::Failed(error),
        },
        _ => Rotation::None,
    };
    let file = okapi_platform::open_journal(&path).map_err(unopenable)?;
    let written = file.metadata().map_err(unopenable)?.len();
    let journal = JournalFile {
        opened: Mutex::new(Opened {
            file,
            written,
            limit: limit_after(&rotation, written),
            looked: written,
        }),
        directory: directory.to_owned(),
        path,
        failed: AtomicBool::new(false),
        on_write_failure,
    };
    Ok((journal, rotation))
}

/// Where the journal at `path` is set aside: `<path>.1`.
fn aside_path(path: &Path) -> PathBuf {
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(ROTATED_SUFFIX);
    rotated.into()
}

/// Beyond what size of a journal holding `written` bytes, after `rotation`,
/// it is set aside: a rotation that failed is not tried again before each
/// line.
fn limit_after(rotation: &Rotation, written: u64) -> u64 {
    match rotation {
        Rotation::None | Rotation::Done { .. } | Rotation::Followed => ROTATION_SIZE,
        Rotation::Failed(_) | Rotation::Diverted(_) => written.saturating_add(ROTATION_SIZE),
    }
}

/// Traces `rotation`, in the current span, from another thread waited for:
/// the journal's writer runs inside tracing, which keeps what it filters per
/// thread; a trace of its own on this thread would upset it.
fn trace_aside(rotation: &Rotation) {
    let span = tracing::Span::current();
    std::thread::scope(|scope| {
        let spawned = std::thread::Builder::new()
            .name("journal-rotation".to_owned())
            .spawn_scoped(scope, || span.in_scope(|| rotation.trace()));
        if let Err(error) = spawned {
            // Nowhere else to say it: the journal is the one writing. A
            // standard error closed is no reason to stop the line.
            let _ = writeln!(
                io::stderr(),
                "okapi: a rotation of the journal cannot be traced: {error}"
            );
        }
    });
}

/// The journal file, set aside when it grows too large. Its first failure is
/// said, once.
struct JournalFile {
    directory: PathBuf,
    path: PathBuf,
    opened: Mutex<Opened>,
    failed: AtomicBool,
    on_write_failure: WriteFailure,
}

/// The file the journal writes to, and how much it holds.
struct Opened {
    file: File,
    /// Its size when opened, then what this run added: another run's lines
    /// are not counted.
    written: u64,
    /// Beyond it, the journal is set aside before the next line.
    limit: u64,
    /// `written` when the run last looked where the journal's path leads.
    looked: u64,
}

struct JournalWriter<'a> {
    journal: &'a JournalFile,
}

impl JournalFile {
    fn with_opened<T>(&self, action: impl FnOnce(&mut Opened) -> io::Result<T>) -> io::Result<T> {
        self.opened
            .lock()
            .map_err(|_| io::Error::other("journal lock poisoned"))
            .and_then(|mut opened| action(&mut opened))
    }

    /// Before the next line (a line never straddles two files), follows the
    /// journal's path when it leads to another file, and sets the journal
    /// aside when it grew too large. Nothing called under the lock may trace:
    /// the trace would come back here, to a lock already held.
    fn rotate_if_grown(&self) {
        // A poisoned lock: the write that follows says it.
        let Ok(mut opened) = self.opened.lock() else {
            return;
        };
        let grown = opened.written > opened.limit;
        if !grown && opened.written < opened.looked.saturating_add(LOOK_EVERY) {
            return;
        }
        let rotation = match self.still_opened(&opened.file) {
            Ok(true) if grown => self.set_aside(&mut opened),
            Ok(true) => Rotation::None,
            Ok(false) => self.follow(&mut opened),
            Err(error) => Rotation::Failed(error),
        };
        opened.looked = opened.written;
        if matches!(rotation, Rotation::None) {
            return;
        }
        // A look that failed, the journal not grown: the limit stays.
        if grown || !matches!(rotation, Rotation::Failed(_)) {
            opened.limit = limit_after(&rotation, opened.written);
        }
        drop(opened);
        // The lines may go to a file no longer named, the trace with them:
        // said as a write that fails is.
        if let Rotation::Diverted(error) = &rotation {
            self.say_failure(error);
        }
        trace_aside(&rotation);
    }

    /// Says `error` through the caller, the first time only.
    fn say_failure(&self, error: &io::Error) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            (self.on_write_failure)(error);
        }
    }

    /// Whether the journal's path still leads to `file`. Where files have
    /// no known identity, it is taken to.
    fn still_opened(&self, file: &File) -> io::Result<bool> {
        if !okapi_platform::IDENTITY_SUPPORTED {
            return Ok(true);
        }
        let opened = okapi_platform::file_identity(&file.metadata()?);
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => Ok(okapi_platform::file_identity(&metadata) == opened),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Renames the journal to `<name>.1` and opens a new one.
    fn set_aside(&self, opened: &mut Opened) -> Rotation {
        let size = opened.written;
        match fs::rename(&self.path, aside_path(&self.path)) {
            Err(error) => Rotation::Failed(error),
            Ok(()) => match self.follow(opened) {
                Rotation::Followed => Rotation::Done { size },
                diverted => diverted,
            },
        }
    }

    /// Opens the journal at its path (its directory too, if it was
    /// removed), and counts from its size.
    fn follow(&self, opened: &mut Opened) -> Rotation {
        match okapi_platform::create_private_directory(&self.directory)
            .and_then(|()| okapi_platform::open_journal(&self.path))
            .and_then(|file| Ok((file.metadata()?.len(), file)))
        {
            Ok((written, file)) => {
                opened.file = file;
                opened.written = written;
                Rotation::Followed
            }
            Err(error) => Rotation::Diverted(error),
        }
    }
}

impl Write for JournalWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // The whole line under the lock: a rotation never falls within it.
        let written = self.journal.with_opened(|opened| {
            opened.file.write_all(bytes)?;
            opened.written += bytes.len() as u64;
            Ok(bytes.len())
        });
        if let Err(error) = &written {
            self.journal.say_failure(error);
        }
        written
    }

    fn flush(&mut self) -> io::Result<()> {
        self.journal.with_opened(|opened| opened.file.flush())
    }
}

impl<'a> MakeWriter<'a> for JournalFile {
    type Writer = JournalWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        self.rotate_if_grown();
        JournalWriter { journal: self }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Says nothing of a write that fails.
    fn unheard() -> WriteFailure {
        Box::new(|_| {})
    }

    /// Writes `line` to `journal` as tracing does: a writer per line.
    fn write_line(journal: &JournalFile, line: &str) {
        journal.make_writer().write_all(line.as_bytes()).unwrap();
    }

    /// The journal `app.log` of `directory`, at `size` bytes.
    fn grown_to(directory: &Path, size: u64) {
        File::create(directory.join("app.log"))
            .unwrap()
            .set_len(size)
            .unwrap();
    }

    /// Counts `bytes` more in `journal`, as if this run had written them.
    fn grown_by(journal: &JournalFile, bytes: u64) {
        journal.opened.lock().unwrap().written += bytes;
    }

    fn length_of(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }

    /// Lines written to a buffer shared with the test.
    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<u8>>>);

    impl Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_trace_keeps_its_level_from_the_helper_to_its_caller() {
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::registry().with(journal_layer(move || writer.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(path = "/var/crash", "refused");
            tracing::error!("failed");
            tracing::info!("done");
        });
        let written = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        let levels: Vec<_> = written.lines().map(level_of).collect();
        assert_eq!(
            levels,
            [
                Some(tracing::Level::WARN),
                Some(tracing::Level::ERROR),
                Some(tracing::Level::INFO)
            ]
        );
        assert_eq!(level_of("Error executing command as another user"), None);
        assert_eq!(level_of("{\"level\":\"LOUD\"}"), None);
    }

    #[test]
    fn a_journal_grown_too_large_is_set_aside_under_its_own_name() {
        let directory = tempfile::tempdir().unwrap();
        let grown = File::create(directory.path().join("app.log")).unwrap();
        grown.set_len(ROTATION_SIZE + 1).unwrap();
        let (journal, rotation) = open(directory.path(), "app.log", unheard()).unwrap();
        assert!(matches!(rotation, Rotation::Done { size } if size == ROTATION_SIZE + 1));
        assert_eq!(
            fs::metadata(directory.path().join("app.log.1"))
                .unwrap()
                .len(),
            ROTATION_SIZE + 1
        );
        assert_eq!(length_of(&journal.path), 0, "started afresh");

        let (_, rotation) = open(directory.path(), "app.log", unheard()).unwrap();
        assert!(matches!(rotation, Rotation::None), "small: appended to");
    }

    #[test]
    fn a_journal_at_the_limit_is_kept() {
        let directory = tempfile::tempdir().unwrap();
        let full = File::create(directory.path().join("app.log")).unwrap();
        full.set_len(ROTATION_SIZE).unwrap();
        let (_, rotation) = open(directory.path(), "app.log", unheard()).unwrap();
        assert!(matches!(rotation, Rotation::None));
        assert!(!directory.path().join("app.log.1").exists());
    }

    #[test]
    fn a_rotation_that_fails_appends_to_the_old_journal() {
        let directory = tempfile::tempdir().unwrap();
        let grown = File::create(directory.path().join("app.log")).unwrap();
        grown.set_len(ROTATION_SIZE + 1).unwrap();
        // A non-empty directory where the old journal would go.
        fs::create_dir_all(directory.path().join("app.log.1/kept")).unwrap();
        let (journal, rotation) = open(directory.path(), "app.log", unheard()).unwrap();
        assert!(matches!(rotation, Rotation::Failed(_)));
        assert_eq!(
            length_of(&directory.path().join("app.log")),
            ROTATION_SIZE + 1
        );

        // Not tried again before each line, even once it could be.
        fs::remove_dir_all(directory.path().join("app.log.1")).unwrap();
        write_line(&journal, "one\n");
        assert_eq!(length_of(&journal.path), ROTATION_SIZE + 5);
        assert!(!directory.path().join("app.log.1").exists());

        // Tried again once it grew as much again.
        grown_by(&journal, ROTATION_SIZE);
        write_line(&journal, "two\n");
        assert_eq!(
            length_of(&directory.path().join("app.log.1")),
            ROTATION_SIZE + 5
        );
        assert_eq!(fs::read_to_string(&journal.path).unwrap(), "two\n");
    }

    #[test]
    fn a_journal_that_grows_too_large_during_a_run_is_set_aside_between_two_lines() {
        let directory = tempfile::tempdir().unwrap();
        grown_to(directory.path(), ROTATION_SIZE);
        let (journal, rotation) = open(directory.path(), "app.log", unheard()).unwrap();
        assert!(matches!(rotation, Rotation::None), "at the limit: kept");

        write_line(&journal, "one\n");
        write_line(&journal, "two\n");
        write_line(&journal, "three\n");
        let rotated = directory.path().join("app.log.1");
        assert_eq!(length_of(&rotated), ROTATION_SIZE + 4);
        let old = fs::read(rotated).unwrap();
        assert!(old.ends_with(b"\0one\n"), "the line that crossed the limit");
        assert_eq!(fs::read_to_string(&journal.path).unwrap(), "two\nthree\n");
    }

    #[test]
    fn a_journal_set_aside_at_launch_counts_afresh() {
        let directory = tempfile::tempdir().unwrap();
        grown_to(directory.path(), ROTATION_SIZE + 1);
        let (journal, _) = open(directory.path(), "app.log", unheard()).unwrap();
        // The one set aside at launch is replaced only after the new journal
        // grows too large in turn.
        write_line(&journal, "one\n");
        write_line(&journal, "two\n");
        assert_eq!(
            length_of(&directory.path().join("app.log.1")),
            ROTATION_SIZE + 1
        );
        assert_eq!(fs::read_to_string(&journal.path).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn a_run_follows_the_journal_another_run_set_aside() {
        let directory = tempfile::tempdir().unwrap();
        grown_to(directory.path(), ROTATION_SIZE);
        let (first, _) = open(directory.path(), "app.log", unheard()).unwrap();
        let (second, _) = open(directory.path(), "app.log", unheard()).unwrap();
        write_line(&first, "a1\n");
        write_line(&second, "b1\n");
        write_line(&first, "a2\n");
        // Its own count crossed the limit too, but the journal is new.
        write_line(&second, "b2\n");
        let aside = directory.path().join("app.log.1");
        assert_eq!(length_of(&aside), ROTATION_SIZE + 6);
        // It counts from the new journal's size: six bytes, then three.
        write_line(&second, "b3\n");
        assert_eq!(length_of(&aside), ROTATION_SIZE + 6);
        assert_eq!(fs::read_to_string(&second.path).unwrap(), "a2\nb2\nb3\n");
        // At the limit, kept; past it, set aside.
        grown_by(&second, ROTATION_SIZE - 9);
        write_line(&second, "b4\n");
        assert_eq!(length_of(&aside), ROTATION_SIZE + 6);
        write_line(&second, "b5\n");
        assert_eq!(length_of(&aside), 12);
        assert_eq!(fs::read_to_string(&second.path).unwrap(), "b5\n");
    }

    #[test]
    fn a_run_that_writes_little_follows_the_journal_within_a_megabyte() {
        let directory = tempfile::tempdir().unwrap();
        grown_to(directory.path(), 0);
        let (first, _) = open(directory.path(), "app.log", unheard()).unwrap();
        let (second, _) = open(directory.path(), "app.log", unheard()).unwrap();
        grown_by(&first, ROTATION_SIZE + 1);
        write_line(&first, "a1\n");
        // Far from its own limit, it has not looked yet.
        write_line(&second, "b1\n");
        grown_by(&second, LOOK_EVERY);
        write_line(&second, "b2\n");
        let aside = directory.path().join("app.log.1");
        assert_eq!(fs::read_to_string(&aside).unwrap(), "b1\n");
        assert_eq!(fs::read_to_string(&second.path).unwrap(), "a1\nb2\n");
    }

    #[test]
    fn a_journal_whose_directory_is_removed_during_a_run_comes_back() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let (journal, _) = open(&state, "app.log", unheard()).unwrap();
        write_line(&journal, "one\n");
        fs::remove_dir_all(&state).unwrap();
        grown_by(&journal, LOOK_EVERY);
        write_line(&journal, "two\n");
        assert_eq!(fs::read_to_string(&journal.path).unwrap(), "two\n");
    }

    #[test]
    fn a_journal_moved_during_a_run_is_opened_again_once_it_can_be() {
        let directory = tempfile::tempdir().unwrap();
        let said = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&said);
        let (journal, _) = open(
            directory.path(),
            "app.log",
            Box::new(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .unwrap();
        write_line(&journal, "one\n");
        let moved = directory.path().join("moved.log");
        fs::rename(&journal.path, &moved).unwrap();
        // Nothing can be opened at its path: the lines go on to the old file.
        fs::create_dir(&journal.path).unwrap();
        grown_by(&journal, LOOK_EVERY);
        write_line(&journal, "two\n");
        assert_eq!(
            said.load(Ordering::Relaxed),
            1,
            "said, as a write that fails"
        );
        // Not tried again before the next look.
        fs::remove_dir(&journal.path).unwrap();
        write_line(&journal, "three\n");
        assert!(!journal.path.exists());
        grown_by(&journal, LOOK_EVERY);
        write_line(&journal, "four\n");
        assert_eq!(fs::read_to_string(&moved).unwrap(), "one\ntwo\nthree\n");
        assert_eq!(fs::read_to_string(&journal.path).unwrap(), "four\n");
    }

    #[test]
    #[cfg(unix)]
    fn a_look_that_fails_leaves_the_limit_where_it_was() {
        use std::os::unix::fs::PermissionsExt;

        if okapi_platform::running_as_root() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let (journal, _) = open(directory.path(), "app.log", unheard()).unwrap();
        grown_by(&journal, LOOK_EVERY);
        // The directory unreadable: where the path leads cannot be known.
        let permissions = |mode| fs::Permissions::from_mode(mode);
        fs::set_permissions(directory.path(), permissions(0o600)).unwrap();
        write_line(&journal, "one\n");
        fs::set_permissions(directory.path(), permissions(0o700)).unwrap();
        // Past the limit it had: set aside.
        grown_by(&journal, ROTATION_SIZE - LOOK_EVERY - 3);
        write_line(&journal, "two\n");
        assert_eq!(fs::read_to_string(&journal.path).unwrap(), "two\n");
    }

    #[test]
    fn a_rotation_that_fails_during_a_run_appends_to_the_same_journal() {
        let directory = tempfile::tempdir().unwrap();
        grown_to(directory.path(), ROTATION_SIZE);
        let (journal, _) = open(directory.path(), "app.log", unheard()).unwrap();
        fs::create_dir_all(directory.path().join("app.log.1/kept")).unwrap();

        write_line(&journal, "one\n");
        write_line(&journal, "two\n");
        assert_eq!(
            length_of(&directory.path().join("app.log")),
            ROTATION_SIZE + 8
        );
        // Not tried again before each line, even once it could be.
        fs::remove_dir_all(directory.path().join("app.log.1")).unwrap();
        write_line(&journal, "three\n");
        assert_eq!(length_of(&journal.path), ROTATION_SIZE + 14);

        // Tried again once it grew as much again.
        grown_by(&journal, ROTATION_SIZE);
        write_line(&journal, "four\n");
        assert_eq!(fs::read_to_string(&journal.path).unwrap(), "four\n");
    }

    #[test]
    fn each_binary_has_its_own_journal() {
        assert_ne!(CLI_FILE, APP_FILE);
    }

    #[test]
    fn a_journal_that_cannot_be_opened_names_its_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("app.log")).unwrap();
        match open(directory.path(), "app.log", unheard()) {
            Err(Unavailable::Unopenable { path, .. }) => {
                assert_eq!(path, directory.path().join("app.log"));
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("a directory opened as the journal"),
        }
    }

    #[test]
    fn a_failing_write_is_said_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("read-only.log");
        File::create(&path).unwrap();
        let said = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&said);
        let journal = JournalFile {
            // Opened for reading: every write fails.
            opened: Mutex::new(Opened {
                file: File::open(&path).unwrap(),
                written: 0,
                limit: ROTATION_SIZE,
                looked: 0,
            }),
            directory: directory.path().to_owned(),
            path,
            failed: AtomicBool::new(false),
            on_write_failure: Box::new(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        };
        assert!(journal.make_writer().write(b"one").is_err());
        assert!(journal.make_writer().write(b"two").is_err());
        assert_eq!(said.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_journal_that_writes_says_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let said = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&said);
        let (journal, _) = open(
            directory.path(),
            "app.log",
            Box::new(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .unwrap();
        write_line(&journal, "line\n");
        assert_eq!(said.load(Ordering::Relaxed), 0);
    }
}
