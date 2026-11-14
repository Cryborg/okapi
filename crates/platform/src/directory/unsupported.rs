use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::Path;

use super::EntryStatus;

/// An open directory: this system provides none, so no value of it exists.
#[derive(Debug)]
pub enum Directory {}

impl Directory {
    /// # Errors
    ///
    /// Always [`io::ErrorKind::Unsupported`].
    pub fn open(_path: &Path) -> io::Result<Self> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn status(&self) -> io::Result<EntryStatus> {
        match *self {}
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn ensure_removable(&self, _entry: &EntryStatus) -> io::Result<()> {
        match *self {}
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn names(&self) -> io::Result<Vec<OsString>> {
        match *self {}
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn status_of(&self, _name: &OsStr) -> io::Result<EntryStatus> {
        match *self {}
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn open_child(&self, _name: &OsStr) -> io::Result<Self> {
        match *self {}
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn open_file(&self, _name: &OsStr) -> io::Result<(File, EntryStatus)> {
        match *self {}
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn create_directory(&self, _name: &OsStr) -> io::Result<()> {
        match *self {}
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn create_new_file(&self, _name: &OsStr) -> io::Result<File> {
        match *self {}
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn move_to(&self, _name: &OsStr, _target: &Self, _new_name: &OsStr) -> io::Result<()> {
        match *self {}
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn remove_file(&self, _name: &OsStr) -> io::Result<()> {
        match *self {}
    }

    /// Never called: no directory exists.
    ///
    /// # Errors
    ///
    /// None.
    pub fn remove_directory(&self, _name: &OsStr) -> io::Result<()> {
        match *self {}
    }
}

/// Whether the entry at `path` is the root of a mount: this system cannot
/// tell.
///
/// # Errors
///
/// None here.
pub fn mount_root(_path: &Path) -> io::Result<Option<bool>> {
    Ok(None)
}

/// Whether this system tells a mount root from a plain directory: not here.
#[must_use]
pub const fn tells_mount_roots() -> bool {
    false
}

/// Whether `error` says the process ran out of open file descriptors: not
/// told apart here.
#[must_use]
pub const fn out_of_descriptors(_error: &io::Error) -> bool {
    false
}

/// Whether `error` is the refusal to open a link: no directory opens here.
pub(crate) const fn link_refused(_error: &io::Error) -> bool {
    false
}
