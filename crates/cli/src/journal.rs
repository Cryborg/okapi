//! The journal: every run's traces, one JSON line per event, in a file the
//! developer reads back after a test.

use std::fs::{self, DirBuilder, File};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt};

use crate::text::{Language, Message, text};

const FILE_NAME: &str = "okapi.log";
const ROTATED_NAME: &str = "okapi.log.1";
/// Beyond this size the journal starts afresh, the previous one kept aside
/// (a full scan writes a few megabytes of detail).
pub const ROTATION_SIZE: u64 = 64 * 1024 * 1024;
/// Only its owner reads the journal's directory: the journal lists file names.
const PRIVATE_DIRECTORY: u32 = 0o700;

/// The journal could not be opened.
#[derive(Debug)]
pub enum Unavailable {
    /// Neither `XDG_STATE_HOME` nor `HOME` says where it goes.
    NoStateDirectory,
    Unopenable {
        path: PathBuf,
        error: io::Error,
    },
}

/// Sends every trace to the journal file (why each entry is held back
/// included), and with `verbose` the detail to standard error too. Without a
/// journal file the run goes on. Gives the journal's path and what became of
/// the previous one, to trace once the run's span is entered.
pub fn start(verbose: bool, language: Language) -> Result<(PathBuf, Rotation), Unavailable> {
    let file_level = LevelFilter::TRACE;
    let console_level = if verbose {
        LevelFilter::DEBUG
    } else {
        LevelFilter::OFF
    };
    match open() {
        Ok((file, path, rotation)) => {
            let journal = fmt::layer()
                .json()
                .with_writer(JournalFile {
                    file: Mutex::new(file),
                    failed: AtomicBool::new(false),
                    language,
                })
                .with_filter(file_level);
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
    pub fn trace(&self) {
        match self {
            Self::None => {}
            Self::Done { size } => tracing::info!(size, "journal rotated"),
            Self::Failed(error) => tracing::warn!(%error, "journal rotation failed, appending"),
        }
    }
}

/// Opens the journal, after setting the previous one aside if it grew too
/// large.
fn open() -> Result<(File, PathBuf, Rotation), Unavailable> {
    let directory = okapi_platform::state_directory().ok_or(Unavailable::NoStateDirectory)?;
    let path = directory.join(FILE_NAME);
    let unopenable = |error| Unavailable::Unopenable {
        path: path.clone(),
        error,
    };
    DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIRECTORY)
        .create(&directory)
        .and_then(|()| {
            fs::set_permissions(&directory, fs::Permissions::from_mode(PRIVATE_DIRECTORY))
        })
        .map_err(unopenable)?;
    let size = fs::symlink_metadata(&path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|metadata| metadata.len());
    let rotation = match size {
        Some(size) if size > ROTATION_SIZE => {
            match fs::rename(&path, directory.join(ROTATED_NAME)) {
                Ok(()) => Rotation::Done { size },
                Err(error) => Rotation::Failed(error),
            }
        }
        _ => Rotation::None,
    };
    let file = okapi_platform::open_journal(&path).map_err(unopenable)?;
    Ok((file, path, rotation))
}

/// The journal file. Its first write that fails is said on standard error,
/// once: a journal that stops filling up must not go unnoticed.
struct JournalFile {
    file: Mutex<File>,
    failed: AtomicBool,
    language: Language,
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
            crate::output::error_line(&text(
                self.journal.language,
                &Message::JournalWriteFailed(error),
            ));
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
