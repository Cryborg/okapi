//! Every line of the history, read back for the screen that shows it: the
//! line breaks are found first, by blocks, none of the lines kept; then each
//! line is read as it streams by, its version first, its kind next, then
//! what it says. A deletion log is folded entry by entry into what the
//! caller keeps of it. A line that cannot be read is skipped, never a
//! reason to refuse the rest, and a deletion log goes with the cleaning line
//! that follows it at its time, or with none.

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::marker::PhantomData;
use std::path::Path;
use std::time::Instant;

use serde::de::{Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use super::{
    BLOCK, HistoryEntry, HistoryOutcome, LineKind, UninstallEntry, check_line_version, read_line,
};
use crate::StoreError;

/// One line of the history, read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRecord<D> {
    /// A cleaning, and what is known of its deletion log.
    Cleaning {
        line: HistoryEntry,
        deletions: Deletions<D>,
    },
    Uninstall(UninstallEntry),
}

/// What is known of the deletion log of a cleaning read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deletions<D> {
    /// None was written: the user kept none then.
    NotKept,
    /// Its line says it is one, at the cleaning's time, but its entries
    /// cannot be read.
    Unreadable,
    /// Its entries, folded as the caller keeps them.
    Read(D),
}

/// Every line of a history that could be read, oldest first, and how many
/// could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRead<D> {
    pub records: Vec<HistoryRecord<D>>,
    /// Lines skipped: unreadable (cut short, another version or kind,
    /// missing what they say), or deletion logs without their cleaning line.
    pub skipped: usize,
}

impl<D> Default for HistoryRead<D> {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            skipped: 0,
        }
    }
}

/// Where a line lies, in bytes from the file's start: `end` excluded, its
/// line break not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: u64,
    end: u64,
}

impl Span {
    const fn is_empty(self) -> bool {
        self.start == self.end
    }

    const fn length(self) -> u64 {
        self.end - self.start
    }
}

/// The lines of `history` within its first `length` bytes, each ended by a
/// line break, and the last one if it is not ended yet (a write under way,
/// or one cut short that the next write will end): read by blocks, none of
/// them kept.
fn spans(mut history: &File, length: u64) -> io::Result<(Vec<Span>, Option<Span>)> {
    let in_bytes = |index: usize| u64::try_from(index).map_err(io::Error::other);
    history.seek(SeekFrom::Start(0))?;
    let block = usize::try_from(BLOCK).map_err(io::Error::other)?;
    let mut reader = BufReader::with_capacity(block, history.take(length));
    let mut spans = Vec::new();
    let mut position = 0;
    let mut start = 0;
    loop {
        let read = reader.fill_buf()?;
        if read.is_empty() {
            break;
        }
        for (index, byte) in read.iter().enumerate() {
            if *byte == b'\n' {
                let end = position + in_bytes(index)?;
                spans.push(Span { start, end });
                start = end + 1;
            }
        }
        let size = read.len();
        position += in_bytes(size)?;
        reader.consume(size);
    }
    let unended = (start < position).then_some(Span {
        start,
        end: position,
    });
    Ok((spans, unended))
}

/// What the first fields of a line say: whose line it is, and when.
#[derive(Deserialize)]
struct Head {
    kind: Option<LineKind>,
    time: i64,
}

/// The entries of a deletion log, folded into a `D` as they stream by.
#[derive(Deserialize)]
#[serde(bound(deserialize = "D: Default + Extend<HistoryOutcome>"))]
struct DeletionLine<D> {
    entries: Folded<D>,
}

/// Entries folded into a `D` one at a time: never a list of them all.
struct Folded<D>(D);

impl<'de, D: Default + Extend<HistoryOutcome>> Deserialize<'de> for Folded<D> {
    fn deserialize<De: Deserializer<'de>>(deserializer: De) -> Result<Self, De::Error> {
        struct Fold<D>(PhantomData<D>);

        impl<'de, D: Default + Extend<HistoryOutcome>> Visitor<'de> for Fold<D> {
            type Value = D;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("the entries of a deletion log")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut entries: A) -> Result<D, A::Error> {
                let mut folded = D::default();
                while let Some(entry) = entries.next_element::<HistoryOutcome>()? {
                    folded.extend(Some(entry));
                }
                Ok(folded)
            }
        }

        deserializer.deserialize_seq(Fold(PhantomData)).map(Self)
    }
}

/// Why a line was skipped, as the journal says it.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
enum Skip {
    Unreadable,
    /// A deletion log not followed by its cleaning line.
    Orphan,
}

/// A deletion log read, waiting for its cleaning line.
struct Waiting<D> {
    span: Span,
    time: i64,
    deletions: Deletions<D>,
}

/// The lines read so far.
struct Reading<'a, D> {
    path: &'a Path,
    read: HistoryRead<D>,
    waiting: Option<Waiting<D>>,
}

impl<D> Reading<'_, D> {
    fn skip(&mut self, span: Span, why: Skip, error: Option<&StoreError>) {
        self.read.skipped += 1;
        tracing::warn!(
            path = %self.path.display(),
            offset = span.start,
            length = span.length(),
            ?why,
            error = error.map(tracing::field::display),
            "history line skipped"
        );
    }

    /// The deletion log waiting, if any, is left without its cleaning line.
    fn orphan_waiting(&mut self) {
        if let Some(waiting) = self.waiting.take() {
            self.skip(waiting.span, Skip::Orphan, None);
        }
    }
}

/// Every line of `history` (at `path`), as [`super::History::read_all`]
/// reads them, within the length it had when this began: a line added
/// meanwhile is not read.
pub(super) fn read<D: Default + Extend<HistoryOutcome>>(
    history: &File,
    path: &Path,
) -> Result<HistoryRead<D>, StoreError> {
    let started = Instant::now();
    let unreadable = |error| StoreError::Unreadable {
        path: path.to_owned(),
        error,
    };
    let length = history.metadata().map_err(unreadable)?.len();
    let (spans, unended) = spans(history, length).map_err(unreadable)?;
    let mut reading = Reading {
        path,
        read: HistoryRead::default(),
        waiting: None,
    };
    for span in spans {
        if !span.is_empty() {
            read_one(history, span, &mut reading)?;
        }
    }
    reading.orphan_waiting();
    if let Some(span) = unended {
        tracing::debug!(
            offset = span.start,
            length = span.length(),
            "last history line not ended yet: not read"
        );
    }
    let read = reading.read;
    tracing::info!(
        bytes = length,
        records = read.records.len(),
        skipped = read.skipped,
        elapsed = ?started.elapsed(),
        "history read"
    );
    Ok(read)
}

/// Reads the line at `span` of `history` into `reading`. Only the file
/// failing to be read is an error.
fn read_one<D: Default + Extend<HistoryOutcome>>(
    history: &File,
    span: Span,
    reading: &mut Reading<'_, D>,
) -> Result<(), StoreError> {
    let path = reading.path;
    let read_head = || -> Result<Head, StoreError> {
        check_line_version(history, span.start, span.end, path)?;
        read_line(history, span.start, span.end, path)
    };
    let head = match read_head() {
        Ok(head) => head,
        Err(error @ (StoreError::Invalid { .. } | StoreError::UnsupportedVersion { .. })) => {
            reading.orphan_waiting();
            reading.skip(span, Skip::Unreadable, Some(&error));
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    match head.kind {
        Some(LineKind::Deletions) => {
            reading.orphan_waiting();
            let deletions = match read_line::<DeletionLine<D>>(history, span.start, span.end, path)
            {
                Ok(line) => Deletions::Read(line.entries.0),
                Err(error @ StoreError::Invalid { .. }) => {
                    tracing::warn!(
                        offset = span.start,
                        length = span.length(),
                        %error,
                        "deletion log unreadable"
                    );
                    Deletions::Unreadable
                }
                Err(error) => return Err(error),
            };
            reading.waiting = Some(Waiting {
                span,
                time: head.time,
                deletions,
            });
        }
        Some(LineKind::Uninstall) => {
            reading.orphan_waiting();
            match read_line::<UninstallEntry>(history, span.start, span.end, path) {
                Ok(line) => reading.read.records.push(HistoryRecord::Uninstall(line)),
                Err(error @ StoreError::Invalid { .. }) => {
                    reading.skip(span, Skip::Unreadable, Some(&error));
                }
                Err(error) => return Err(error),
            }
        }
        None => {
            let line = match read_line::<HistoryEntry>(history, span.start, span.end, path) {
                Ok(line) => line,
                Err(error @ StoreError::Invalid { .. }) => {
                    reading.orphan_waiting();
                    reading.skip(span, Skip::Unreadable, Some(&error));
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            let deletions = match reading.waiting.take() {
                Some(waiting) if waiting.time == line.time => waiting.deletions,
                Some(waiting) => {
                    reading.skip(waiting.span, Skip::Orphan, None);
                    Deletions::NotKept
                }
                None => Deletions::NotKept,
            };
            reading
                .read
                .records
                .push(HistoryRecord::Cleaning { line, deletions });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::tests::{append, entry, outcome, recorded, uninstalled};
    use super::super::{History, HistoryEntry, HistoryOutcome, UninstallSource};
    use super::*;

    fn read_all(directory: &Path) -> HistoryRead<Vec<HistoryOutcome>> {
        History::read_all(directory).unwrap()
    }

    /// What a record is, by its time and, for a cleaning, its deletion log.
    fn summary(record: &HistoryRecord<Vec<HistoryOutcome>>) -> (char, i64, Option<usize>) {
        match record {
            HistoryRecord::Cleaning { line, deletions } => (
                'c',
                line.time,
                match deletions {
                    Deletions::NotKept => None,
                    Deletions::Unreadable => Some(usize::MAX),
                    Deletions::Read(entries) => Some(entries.len()),
                },
            ),
            HistoryRecord::Uninstall(line) => ('u', line.time, None),
        }
    }

    fn summaries(read: &HistoryRead<Vec<HistoryOutcome>>) -> Vec<(char, i64, Option<usize>)> {
        read.records.iter().map(summary).collect()
    }

    /// A cleaning at `time` without a deletion log.
    fn cleaned(directory: &Path, time: i64, removed_bytes: u64) {
        History::open(directory)
            .unwrap()
            .record(&HistoryEntry {
                time,
                removed_bytes,
                ..entry(1, None)
            })
            .unwrap();
    }

    /// A cleaning at `time` with a deletion log of `count` entries.
    fn cleaned_with_log(directory: &Path, time: i64, count: usize) {
        History::open(directory)
            .unwrap()
            .record(&HistoryEntry {
                time,
                ..entry(1, Some(vec![outcome("removed", Some(4), None); count]))
            })
            .unwrap();
    }

    #[test]
    fn every_line_is_read_back_in_order_with_its_own_deletion_log() {
        let directory = tempfile::tempdir().unwrap();
        cleaned(directory.path(), 100, 7);
        cleaned_with_log(directory.path(), 200, 3);
        uninstalled(directory.path(), 300);
        cleaned_with_log(directory.path(), 400, 1);
        let read = read_all(directory.path());
        assert_eq!(read.skipped, 0);
        assert_eq!(
            summaries(&read),
            [
                ('c', 100, None),
                ('c', 200, Some(3)),
                ('u', 300, None),
                ('c', 400, Some(1)),
            ],
            "each log with the cleaning after it, never the one before"
        );
        let HistoryRecord::Cleaning { line, .. } = &read.records[0] else {
            panic!("a cleaning");
        };
        assert_eq!(
            *line,
            HistoryEntry {
                time: 100,
                removed_bytes: 7,
                ..entry(1, None)
            },
            "every field read back"
        );
        assert_eq!(line.freed(), Some(7));
        let HistoryRecord::Uninstall(uninstall) = &read.records[2] else {
            panic!("an uninstallation");
        };
        assert_eq!(uninstall.source, UninstallSource::Snap);
        assert_eq!(uninstall.name, "Android Studio");
        assert_eq!(uninstall.packages, ["android-studio"]);
        assert_eq!(uninstall.freed_bytes, Some(3));
        let HistoryRecord::Cleaning {
            deletions: Deletions::Read(entries),
            ..
        } = &read.records[1]
        else {
            panic!("a deletion log");
        };
        assert_eq!(entries[0], outcome("removed", Some(4), None));
    }

    #[test]
    fn a_line_that_cannot_be_read_is_skipped_and_the_rest_read() {
        let directory = tempfile::tempdir().unwrap();
        cleaned(directory.path(), 100, 7);
        for line in [
            &b"{\"version\":1,\"tim\n"[..],
            b"{\"version\":2,\"time\":150,\"removedBytes\":1}\n",
            b"{\"version\":1,\"kind\":\"update\",\"time\":160}\n",
            b"{\"version\":1,\"time\":170}\n",
            b"{\"version\":1,\"kind\":\"uninstall\",\"time\":180}\n",
            b"not json\n",
        ] {
            append(directory.path(), line);
        }
        uninstalled(directory.path(), 300);
        let read = read_all(directory.path());
        assert_eq!(summaries(&read), [('c', 100, None), ('u', 300, None)]);
        assert_eq!(read.skipped, 6);
    }

    #[test]
    fn a_deletion_log_without_its_cleaning_line_is_skipped() {
        let directory = tempfile::tempdir().unwrap();
        cleaned_with_log(directory.path(), 100, 2);
        let text = std::fs::read_to_string(History::path(directory.path())).unwrap();
        let log_at = |time: i64| {
            let log = text.lines().next().unwrap();
            format!(
                "{}\n",
                log.replace("\"time\":100", &format!("\"time\":{time}"))
            )
        };
        // Followed by an uninstallation, by another log, by a line that
        // cannot be read, by a cleaning of another time.
        append(directory.path(), log_at(200).as_bytes());
        uninstalled(directory.path(), 210);
        // At the orphan's time, after the uninstallation: never its log.
        cleaned(directory.path(), 200, 1);
        append(directory.path(), log_at(300).as_bytes());
        cleaned_with_log(directory.path(), 310, 1);
        append(directory.path(), log_at(400).as_bytes());
        append(directory.path(), b"{\"version\":1,\"tim\n");
        append(directory.path(), log_at(500).as_bytes());
        cleaned(directory.path(), 510, 1);
        // And last of all.
        append(directory.path(), log_at(600).as_bytes());
        let read = read_all(directory.path());
        assert_eq!(
            summaries(&read),
            [
                ('c', 100, Some(2)),
                ('u', 210, None),
                ('c', 200, None),
                ('c', 310, Some(1)),
                ('c', 510, None),
            ],
            "the cleaning after an orphan read, without its log"
        );
        // 200, 300, 400 and the cut line, 500, 600.
        assert_eq!(read.skipped, 6);
    }

    #[test]
    fn a_deletion_log_that_cannot_be_read_is_said_so_with_its_cleaning() {
        let directory = tempfile::tempdir().unwrap();
        append(
            directory.path(),
            b"{\"version\":1,\"kind\":\"deletions\",\"time\":100,\"entries\":[{\"path\":3}]}\n",
        );
        cleaned(directory.path(), 100, 1);
        let read = read_all(directory.path());
        assert_eq!(summaries(&read), [('c', 100, Some(usize::MAX))]);
        assert_eq!(read.skipped, 0, "its cleaning line is read");
    }

    /// Counts the entries handed to it, and the most at once.
    #[derive(Default)]
    struct Counted {
        entries: usize,
        most_at_once: usize,
    }

    impl Extend<HistoryOutcome> for Counted {
        fn extend<I: IntoIterator<Item = HistoryOutcome>>(&mut self, entries: I) {
            let count = entries.into_iter().count();
            self.entries += count;
            self.most_at_once = self.most_at_once.max(count);
        }
    }

    #[test]
    fn a_deletion_log_is_folded_one_entry_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        cleaned_with_log(directory.path(), 100, 4000);
        let read: HistoryRead<Counted> = History::read_all(directory.path()).unwrap();
        let [
            HistoryRecord::Cleaning {
                deletions: Deletions::Read(counted),
                ..
            },
        ] = read.records.as_slice()
        else {
            panic!("one cleaning with its log");
        };
        assert_eq!(counted.entries, 4000);
        assert_eq!(counted.most_at_once, 1);
    }

    #[test]
    fn a_last_line_not_ended_yet_is_neither_read_nor_counted() {
        let directory = tempfile::tempdir().unwrap();
        cleaned(directory.path(), 100, 7);
        append(
            directory.path(),
            b"{\"version\":1,\"time\":200,\"stops\":[]",
        );
        let read = read_all(directory.path());
        assert_eq!(summaries(&read), [('c', 100, None)]);
        assert_eq!(read.skipped, 0);
    }

    #[test]
    fn a_line_added_meanwhile_is_not_read() {
        let directory = tempfile::tempdir().unwrap();
        cleaned(directory.path(), 100, 7);
        let path = History::path(directory.path());
        let file = File::open(&path).unwrap();
        let length = file.metadata().unwrap().len();
        cleaned(directory.path(), 200, 9);
        let (spans, unended) = spans(&file, length).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(unended, None);
    }

    #[test]
    fn the_lines_are_found_whatever_the_blocks_meet() {
        let directory = tempfile::tempdir().unwrap();
        recorded(directory.path(), &[(100, 7, None), (200, 9, Some(false))]);
        let text = std::fs::read_to_string(History::path(directory.path())).unwrap();
        // Lines longer than a block, and blank lines between.
        let long = format!(
            "{}\n\n{text}",
            "x".repeat(usize::try_from(BLOCK).unwrap() * 2)
        );
        std::fs::write(History::path(directory.path()), long).unwrap();
        let read = read_all(directory.path());
        assert_eq!(read.skipped, 1, "the long line");
        let times: Vec<_> = summaries(&read).iter().map(|record| record.1).collect();
        assert_eq!(times, [100, 200]);
        let HistoryRecord::Cleaning { line, .. } = &read.records[1] else {
            panic!("a cleaning");
        };
        assert_eq!(line.freed(), None, "root's part not done");
    }

    #[test]
    fn without_a_history_there_is_nothing_to_read() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(read_all(directory.path()), HistoryRead::default());
        std::fs::create_dir(History::path(directory.path())).unwrap();
        assert!(matches!(
            History::read_all::<Vec<HistoryOutcome>>(directory.path()),
            Err(StoreError::NotAFile { .. })
        ));
    }
}
