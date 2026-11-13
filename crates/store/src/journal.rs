//! The journal: every run's traces, one JSON line per event, in a file of
//! `$XDG_STATE_HOME/okapi/`, which the developer reads back after a test. Each
//! binary names its own file: a rotation by one never moves the file another
//! is writing to. Two runs of one binary write whole lines (one call each, to
//! a file opened for appending); two of them starting together on a journal
//! too large may both set it aside: the previous journal is then lost, and
//! the first one goes on writing in `<name>.1`.

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

/// Said once, on the journal's first write that fails: a journal that stops
/// filling up must not go unnoticed. The caller tells its user.
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
        .and_then(|directory| open(&directory, file_name));
    match opened {
        Ok((file, path, rotation)) => {
            let journal = journal_layer(JournalFile {
                file: Mutex::new(file),
                failed: AtomicBool::new(false),
                on_write_failure,
            });
            tracing_subscriber::registry()
                .with(journal)
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
    Done { size: u64 },
    Failed(io::Error),
}

impl Rotation {
    /// Traces what became of the previous journal, once tracing goes there.
    pub fn trace(&self) {
        match self {
            Self::None => {}
            Self::Done { size } => tracing::info!(size, "journal rotated"),
            Self::Failed(error) => tracing::warn!(%error, "journal rotation failed, appending"),
        }
    }
}

/// Opens the journal `file_name` of `directory`, after setting the previous
/// one aside if it grew too large.
fn open(directory: &Path, file_name: &str) -> Result<(File, PathBuf, Rotation), Unavailable> {
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
        Some(size) if size > ROTATION_SIZE => {
            match fs::rename(
                &path,
                directory.join(format!("{file_name}{ROTATED_SUFFIX}")),
            ) {
                Ok(()) => Rotation::Done { size },
                Err(error) => Rotation::Failed(error),
            }
        }
        _ => Rotation::None,
    };
    let file = okapi_platform::open_journal(&path).map_err(unopenable)?;
    Ok((file, path, rotation))
}

/// The journal file. Its first write that fails is said, once.
struct JournalFile {
    file: Mutex<File>,
    failed: AtomicBool,
    on_write_failure: WriteFailure,
}

struct JournalWriter<'a> {
    journal: &'a JournalFile,
}

impl JournalFile {
    fn with_file<T>(&self, action: impl FnOnce(&mut File) -> io::Result<T>) -> io::Result<T> {
        self.file
            .lock()
            .map_err(|_| io::Error::other("journal lock poisoned"))
            .and_then(|mut file| action(&mut file))
    }
}

impl Write for JournalWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.journal.with_file(|file| file.write(bytes));
        if let Err(error) = &written
            && !self.journal.failed.swap(true, Ordering::Relaxed)
        {
            (self.journal.on_write_failure)(error);
        }
        written
    }

    fn flush(&mut self) -> io::Result<()> {
        self.journal.with_file(Write::flush)
    }
}

impl<'a> MakeWriter<'a> for JournalFile {
    type Writer = JournalWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        JournalWriter { journal: self }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn a_journal_grown_too_large_is_set_aside_under_its_own_name() {
        let directory = tempfile::tempdir().unwrap();
        let grown = File::create(directory.path().join("app.log")).unwrap();
        grown.set_len(ROTATION_SIZE + 1).unwrap();
        let (_, path, rotation) = open(directory.path(), "app.log").unwrap();
        assert!(matches!(rotation, Rotation::Done { size } if size == ROTATION_SIZE + 1));
        assert_eq!(
            fs::metadata(directory.path().join("app.log.1"))
                .unwrap()
                .len(),
            ROTATION_SIZE + 1
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), 0, "started afresh");

        let (_, _, rotation) = open(directory.path(), "app.log").unwrap();
        assert!(matches!(rotation, Rotation::None), "small: appended to");
    }

    #[test]
    fn a_journal_at_the_limit_is_kept() {
        let directory = tempfile::tempdir().unwrap();
        let full = File::create(directory.path().join("app.log")).unwrap();
        full.set_len(ROTATION_SIZE).unwrap();
        let (_, _, rotation) = open(directory.path(), "app.log").unwrap();
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
        let (_, path, rotation) = open(directory.path(), "app.log").unwrap();
        assert!(matches!(rotation, Rotation::Failed(_)));
        assert_eq!(fs::metadata(path).unwrap().len(), ROTATION_SIZE + 1);
    }

    #[test]
    fn each_binary_has_its_own_journal() {
        assert_ne!(CLI_FILE, APP_FILE);
    }

    #[test]
    fn a_journal_that_cannot_be_opened_names_its_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("app.log")).unwrap();
        match open(directory.path(), "app.log") {
            Err(Unavailable::Unopenable { path, .. }) => {
                assert_eq!(path, directory.path().join("app.log"));
            }
            other => panic!("{other:?}"),
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
            file: Mutex::new(File::open(&path).unwrap()),
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
        let journal = JournalFile {
            file: Mutex::new(File::create(directory.path().join("app.log")).unwrap()),
            failed: AtomicBool::new(false),
            on_write_failure: Box::new(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        };
        journal.make_writer().write_all(b"line\n").unwrap();
        assert_eq!(said.load(Ordering::Relaxed), 0);
    }
}
