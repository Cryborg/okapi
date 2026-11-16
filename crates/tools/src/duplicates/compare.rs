//! The comparison: files of one size are told apart by a key of their
//! content, computed on a few threads, a pass at a time. A file whose key
//! could not be read leaves the search: it is never equal to another.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use okapi_platform::{Directory, EntryType, Unreached};

use crate::listing::{Cancelled, Found, Root, parent_of};

/// How much of each end of a file the first pass reads.
pub(crate) const END_BYTES: u64 = 4096;
/// How much of a file is read at a time by the last pass.
const BLOCK_BYTES: usize = 128 * 1024;

/// The key of a file's content: what equal contents share.
pub(crate) type Key = [u8; blake3::OUT_LEN];

/// How a pass reads a file's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    /// Its first and last bytes ([`END_BYTES`] each, fewer in a small file).
    Ends,
    /// All of it.
    Content,
}

/// Why a file left the search.
#[derive(Debug)]
pub(crate) enum Dropped {
    /// No longer the file listed: written, swapped, shortened.
    Changed,
    Io(io::Error),
    /// Reading it panicked, saying this.
    Panicked(String),
    /// The search was cancelled while it was read.
    Cancelled,
}

/// What the passes share: the roots to open the files from, the
/// cancellation, the progress.
pub(crate) struct Comparing<'a> {
    pub(crate) roots: &'a [Root],
    pub(crate) threads: NonZeroUsize,
    pub(crate) cancelled: &'a AtomicBool,
    /// The bytes of contents read so far.
    pub(crate) bytes_read: &'a AtomicU64,
    /// The files whose ends were read so far.
    pub(crate) files_done: &'a AtomicU64,
    /// Called with each file's path before it is read, to test what happens
    /// meanwhile.
    #[cfg(test)]
    pub(crate) before_read: Option<fn(&std::path::Path)>,
}

/// Splits each group of files of one size by the key `pass` reads: groups of
/// one are dropped, and so is each file whose key could not be read
/// (counted in `dropped`).
pub(crate) fn split(
    groups: Vec<Vec<Found>>,
    pass: Pass,
    comparing: &Comparing<'_>,
    dropped: &mut u64,
) -> Result<Vec<Vec<Found>>, Cancelled> {
    let files: Vec<(usize, Found)> = groups
        .into_iter()
        .enumerate()
        .flat_map(|(group, files)| files.into_iter().map(move |file| (group, file)))
        .collect();
    let keys = keys(&files, pass, comparing);
    if comparing.cancelled.load(Ordering::Acquire) {
        return Err(Cancelled);
    }
    let mut split: HashMap<(usize, Key), Vec<Found>> = HashMap::new();
    for ((group, file), key) in files.into_iter().zip(keys) {
        match key {
            Some(key) => split.entry((group, key)).or_default().push(file),
            None => *dropped += 1,
        }
    }
    Ok(split
        .into_values()
        .filter(|files| files.len() > 1)
        .collect())
}

/// The key of each file, in order, read on the pass's threads: `None` for one
/// that could not be read, or was not read because the search was
/// cancelled.
fn keys(files: &[(usize, Found)], pass: Pass, comparing: &Comparing<'_>) -> Vec<Option<Key>> {
    let next = AtomicUsize::new(0);
    let work = || {
        let mut keys = Vec::new();
        loop {
            if comparing.cancelled.load(Ordering::Acquire) {
                return keys;
            }
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some((_, file)) = files.get(index) else {
                return keys;
            };
            keys.push((index, key_of(file, pass, comparing)));
        }
    };
    let mut keys: Vec<Option<Key>> = vec![None; files.len()];
    std::thread::scope(|scope| {
        let mut helpers = Vec::new();
        for index in 1..comparing.threads.get() {
            match std::thread::Builder::new()
                .name(format!("okapi-duplicates-{index}"))
                .spawn_scoped(scope, work)
            {
                Ok(helper) => helpers.push(helper),
                // This thread compares on its own: slower, never wrong.
                Err(error) => {
                    tracing::warn!(%error, "comparison thread not started");
                    break;
                }
            }
        }
        let mut found = work();
        for helper in helpers {
            // Each file is read under `catch_unwind`: a helper never panics,
            // and one that did loses only keys, which leave their files out.
            match helper.join() {
                Ok(keys) => found.extend(keys),
                Err(panic) => tracing::error!(
                    message = crate::panic_message(&*panic),
                    "comparison thread panicked: its files left out"
                ),
            }
        }
        for (index, key) in found {
            if let Some(slot) = keys.get_mut(index) {
                *slot = key;
            }
        }
    });
    keys
}

/// The key of `file`, read as `pass` says; `None`, traced, when it could
/// not be.
fn key_of(file: &Found, pass: Pass, comparing: &Comparing<'_>) -> Option<Key> {
    let size = file.status.size;
    let mut counted = 0;
    let read = std::panic::catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        if let Some(before_read) = comparing.before_read {
            before_read(&file.path);
        }
        let mut opened = open(file, comparing.roots)?;
        match pass {
            Pass::Ends => ends(&mut opened, size),
            Pass::Content => content_key(&mut opened, size, comparing, &mut counted),
        }
    }))
    .unwrap_or_else(|panic| Err(Dropped::Panicked(crate::panic_message(&*panic))));
    match pass {
        Pass::Ends => {
            comparing.files_done.fetch_add(1, Ordering::Relaxed);
        }
        // What a file left out did not read is done all the same: the
        // progress reaches its end.
        Pass::Content if read.is_err() => {
            comparing
                .bytes_read
                .fetch_add(size.saturating_sub(counted), Ordering::Relaxed);
        }
        Pass::Content => {}
    }
    match read {
        Ok(key) => Some(key),
        Err(Dropped::Panicked(message)) => {
            tracing::error!(path = %file.path.display(), message, "reading a file panicked: left out");
            None
        }
        Err(Dropped::Changed) => {
            tracing::debug!(path = %file.path.display(), "file changed during the search: left out");
            None
        }
        Err(Dropped::Io(error)) => {
            tracing::debug!(path = %file.path.display(), %error, "file unreadable: left out");
            None
        }
        Err(Dropped::Cancelled) => None,
    }
}

/// Opens the file listed as `file` again, from its root down, one folder at
/// a time, never through a link nor into another mount: it must still be
/// the file listed, unchanged.
pub(crate) fn open(file: &Found, roots: &[Root]) -> Result<File, Dropped> {
    let root = roots.get(file.root).ok_or(Dropped::Changed)?;
    let (directory, name) = parent_of(&file.path, root).map_err(|unreached| match unreached {
        Unreached::Changed => Dropped::Changed,
        Unreached::Io(error) => Dropped::Io(error),
    })?;
    let (opened, status) = directory
        .open_file(name)
        .map_err(|error| refused(&directory, name, error))?;
    if file.status.unchanged(&status) {
        Ok(opened)
    } else {
        Err(Dropped::Changed)
    }
}

/// Why the file `name` of `directory` did not open: read again, gone or no
/// longer a plain file (a link put in its place), it changed; otherwise the
/// system refused it.
fn refused(directory: &Directory, name: &OsStr, error: io::Error) -> Dropped {
    match directory.status_of(name) {
        Ok(status) if status.kind != EntryType::File => Dropped::Changed,
        Err(again) if again.kind() == io::ErrorKind::NotFound => Dropped::Changed,
        _ => Dropped::Io(error),
    }
}

/// The key of the first and last bytes of a file of `size` bytes; in a
/// small file, the two overlap.
fn ends(file: &mut File, size: u64) -> Result<Key, Dropped> {
    let end = size.min(END_BYTES);
    let mut hasher = blake3::Hasher::new();
    read_into(file, end, &mut hasher)?;
    file.seek(SeekFrom::Start(size - end))
        .map_err(Dropped::Io)?;
    read_into(file, end, &mut hasher)?;
    Ok(*hasher.finalize().as_bytes())
}

/// The key of all of a file of `size` bytes, its progress counted (in
/// `counted` too); a file longer or shorter than listed has changed.
fn content_key(
    file: &mut File,
    size: u64,
    comparing: &Comparing<'_>,
    counted: &mut u64,
) -> Result<Key, Dropped> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; BLOCK_BYTES];
    let mut left = size;
    loop {
        if comparing.cancelled.load(Ordering::Acquire) {
            return Err(Dropped::Cancelled);
        }
        let read = match file.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Dropped::Io(error)),
        };
        if read == 0 {
            return if left == 0 {
                Ok(*hasher.finalize().as_bytes())
            } else {
                Err(Dropped::Changed)
            };
        }
        left = left.checked_sub(read as u64).ok_or(Dropped::Changed)?;
        hasher.update(&buffer[..read]);
        *counted += read as u64;
        comparing
            .bytes_read
            .fetch_add(read as u64, Ordering::Relaxed);
    }
}

/// Reads exactly `count` bytes of `file` into `hasher`: fewer, it has
/// changed.
fn read_into(file: &mut File, count: u64, hasher: &mut blake3::Hasher) -> Result<(), Dropped> {
    let copied = io::copy(&mut file.take(count), hasher).map_err(Dropped::Io)?;
    if copied == count {
        Ok(())
    } else {
        Err(Dropped::Changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_whole(content: &[u8], size: u64) -> Result<Key, Dropped> {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("file");
        std::fs::write(&path, content).unwrap();
        let cancelled = AtomicBool::new(false);
        let bytes_read = AtomicU64::new(0);
        let comparing = Comparing {
            roots: &[],
            threads: NonZeroUsize::MIN,
            cancelled: &cancelled,
            bytes_read: &bytes_read,
            files_done: &AtomicU64::new(0),
            before_read: None,
        };
        content_key(&mut File::open(&path).unwrap(), size, &comparing, &mut 0)
    }

    #[test]
    fn a_file_longer_or_shorter_than_listed_has_changed() {
        let content = vec![7; BLOCK_BYTES + 10];
        let size = content.len() as u64;
        assert!(read_whole(&content, size).is_ok());
        assert!(matches!(
            read_whole(&content, size + 1),
            Err(Dropped::Changed)
        ));
        assert!(matches!(
            read_whole(&content, size - 1),
            Err(Dropped::Changed)
        ));
    }
}
