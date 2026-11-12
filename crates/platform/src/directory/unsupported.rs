use std::ffi::{OsStr, OsString};
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

/// Whether this system tells a mount root from a plain directory: not here.
#[must_use]
pub const fn tells_mount_roots() -> bool {
    false
}
