//! The folders the user chose to have emptied: what one must be to be
//! scanned, judged once here for the scan and for the screen that adds it.
//!
//! A custom folder is a plain directory of the user's, never a link, never
//! a variable's directory nor one of its ancestors (invariant 6), never one
//! of the user's personal folders (Documents, Pictures...) nor one of their
//! ancestors, all compared by identity, so an alias or a bind mount changes
//! nothing; and it lies on a filesystem whose dates and identities can be
//! trusted (not FAT, FUSE, a network share, a virtual machine's host or a
//! stacked one), since its files' age is all that keeps the recent ones
//! (invariant 2). What it holds is offered as any rule's root offers it,
//! but a directory changed lately keeps all it holds (moved there, its old
//! files keep the dates they had elsewhere); the folder itself never is.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use okapi_platform::{FileIdentity, ROOT_USER, filesystem_may_alias, filesystem_undated};
use okapi_rules::{TemplateVariables, Variable};
use thiserror::Error;

use crate::paths::{NotADirectory, is_plain_absolute, real_directory};
use crate::protection::{ProtectedRoots, identities_up};
use crate::walk::{IdentityReader, Readers};

/// A folder the user wants emptied, as written: an absolute path without
/// `..`, its place read again at each scan.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CustomFolder(PathBuf);

/// A custom folder written so that it names no single folder.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CustomFolderError {
    #[error("a folder to clean cannot be empty")]
    Empty,

    #[error("`{}` is not an absolute path without `..`", path.display())]
    Invalid { path: PathBuf },
}

/// Why a custom folder is not scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomFolderRefusal {
    /// Nothing is there.
    Absent,
    /// It is a symbolic link: never followed.
    Link,
    NotADirectory,
    /// It was replaced while it was read.
    Replaced,
    /// It, or what it is read through, cannot be read.
    Unreadable(io::ErrorKind),
    /// It belongs to another user or to the system, its owner cannot be
    /// told, or the one who would empty it is root: what it holds is not
    /// the user's to empty.
    NotOwned,
    /// It is a variable's directory or one of its ancestors (invariant 6).
    Protected(Variable),
    /// It is one of the user's personal folders, or one of their ancestors.
    Personal,
    /// The personal folders cannot be read: none may be told apart.
    PersonalUnknown(io::ErrorKind),
    /// It lies on a filesystem without a change date of its own (FAT,
    /// exFAT), that may show a file twice (FUSE, network, a virtual
    /// machine's share, stacked), or that cannot be told: the age of what
    /// it holds cannot be trusted.
    UnsureFilesystem,
}

impl CustomFolder {
    /// The folder `path` names.
    ///
    /// # Errors
    ///
    /// [`CustomFolderError`] for a path empty, relative or holding `..`,
    /// which would name another folder than the one meant.
    pub fn new(path: PathBuf) -> Result<Self, CustomFolderError> {
        if path.as_os_str().is_empty() {
            return Err(CustomFolderError::Empty);
        }
        if !is_plain_absolute(&path) {
            return Err(CustomFolderError::Invalid { path });
        }
        Ok(Self(path))
    }

    /// As written.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Whether a scan would walk it now, as the settings screen asks when it
    /// is added and shown: the variables' directories of `variables`, the
    /// personal folders they name, the user the process acts as, the
    /// system's readers. The user's exclusions are not judged here: one
    /// that covers it is said by the scan (`Excluded`).
    ///
    /// # Errors
    ///
    /// Why it would be refused; [`CustomFolderRefusal::Unreadable`] when a
    /// variable's directory cannot be read (nothing could be scanned then).
    pub fn check(&self, variables: &TemplateVariables) -> Result<(), CustomFolderRefusal> {
        let protected = ProtectedRoots::of(variables).map_err(|unreadable| {
            tracing::warn!(%unreadable, "variables unreadable: no folder to clean can be judged");
            CustomFolderRefusal::Unreadable(unreadable.error)
        })?;
        let personal = PersonalFolders::read(variables, Readers::SYSTEM.identity);
        let guard = Guard {
            protected: &protected,
            personal: &personal,
            user: okapi_platform::effective_user(),
            readers: Readers::SYSTEM,
        };
        // Asked each time the screen shows it: said quietly, the scan says it.
        self.judge(&guard).map(|_| ()).inspect_err(|refusal| {
            tracing::debug!(folder = %self.0.display(), ?refusal, "custom folder would be refused");
        })
    }

    /// Its real location and identity, if a scan may walk it ([`Guard`]): the
    /// one rule of what a custom folder must be. The walk must start from
    /// that identity.
    ///
    /// # Errors
    ///
    /// Why it may not.
    pub(crate) fn locate(
        &self,
        guard: &Guard<'_>,
    ) -> Result<(PathBuf, FileIdentity), CustomFolderRefusal> {
        self.judge(guard).inspect_err(|refusal| {
            tracing::info!(folder = %self.0.display(), ?refusal, "custom folder refused");
        })
    }

    fn judge(&self, guard: &Guard<'_>) -> Result<(PathBuf, FileIdentity), CustomFolderRefusal> {
        let (real, metadata) =
            real_directory(&self.0, guard.readers.identity).map_err(|refused| match refused {
                NotADirectory::Absent => CustomFolderRefusal::Absent,
                NotADirectory::Link => CustomFolderRefusal::Link,
                NotADirectory::Other => CustomFolderRefusal::NotADirectory,
                NotADirectory::Replaced { .. } => CustomFolderRefusal::Replaced,
                NotADirectory::Unreadable { path, error } => {
                    tracing::debug!(folder = %self.0.display(), path = %path.display(), ?error, "custom folder unreadable");
                    CustomFolderRefusal::Unreadable(error)
                }
            })?;
        let Some(identity) = (guard.readers.identity)(&metadata) else {
            return Err(CustomFolderRefusal::Unreadable(io::ErrorKind::Unsupported));
        };
        let owner = (guard.readers.ownership)(&metadata).map(|ownership| ownership.owner);
        if owner.is_none() || owner != guard.user || guard.user == Some(ROOT_USER) {
            tracing::debug!(folder = %self.0.display(), ?owner, user = ?guard.user, "custom folder not the user's");
            return Err(CustomFolderRefusal::NotOwned);
        }
        if let Some(variable) = guard.protected.refusal(identity) {
            return Err(CustomFolderRefusal::Protected(variable));
        }
        if let Some(refusal) = guard.personal.refusal(identity) {
            return Err(refusal);
        }
        // Below it, a walk never enters another filesystem: judging its own
        // is enough. One that cannot be told is no fault of the folder's:
        // every one is refused so, which is warned of.
        match (guard.readers.filesystem)(&real) {
            Ok(Some(filesystem))
                if !filesystem_undated(&filesystem) && !filesystem_may_alias(&filesystem) =>
            {
                Ok((real, identity))
            }
            Ok(Some(filesystem)) => {
                tracing::debug!(folder = %self.0.display(), filesystem, "custom folder on a filesystem whose dates cannot be trusted");
                Err(CustomFolderRefusal::UnsureFilesystem)
            }
            Ok(None) => {
                tracing::warn!(folder = %self.0.display(), real = %real.display(), "no mount holds the custom folder: its filesystem cannot be told");
                Err(CustomFolderRefusal::UnsureFilesystem)
            }
            Err(error) => {
                tracing::warn!(folder = %self.0.display(), %error, code = error.raw_os_error(), "mounts unreadable: the filesystem of a custom folder cannot be told");
                Err(CustomFolderRefusal::UnsureFilesystem)
            }
        }
    }
}

/// What a custom folder is judged against.
pub(crate) struct Guard<'a> {
    pub(crate) protected: &'a ProtectedRoots,
    pub(crate) personal: &'a PersonalFolders,
    /// Who will remove what it holds, and must own it.
    pub(crate) user: Option<u32>,
    pub(crate) readers: Readers,
}

/// The user's personal folders (Documents, Pictures...) and their
/// ancestors, by identity: never a custom folder. Or why they cannot be
/// read, which refuses every custom folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersonalFolders(Result<BTreeSet<FileIdentity>, io::ErrorKind>);

impl PersonalFolders {
    /// None: no custom folder to judge.
    pub(crate) const fn none() -> Self {
        Self(Ok(BTreeSet::new()))
    }

    /// The personal folders `folders`, with their ancestors.
    pub(crate) fn of(folders: &[PathBuf], identity_of: IdentityReader) -> Self {
        let mut identities = BTreeSet::new();
        for folder in folders {
            match identities_up(folder, identity_of) {
                Ok(found) => identities.extend(found.into_iter().map(|(_, identity)| identity)),
                Err((path, error)) => {
                    tracing::warn!(folder = %folder.display(), path = %path.display(), ?error, "personal folder unreadable: no folder to clean can be judged");
                    return Self(Err(error));
                }
            }
        }
        Self(Ok(identities))
    }

    /// The personal folders the configuration folder of `variables` names
    /// below their home folder (the default names when none does).
    pub(crate) fn read(variables: &TemplateVariables, identity_of: IdentityReader) -> Self {
        let (Some(config), Some(home)) = (
            variables.value_of(Variable::Config),
            variables.value_of(Variable::Home),
        ) else {
            tracing::warn!("no home or configuration folder: no personal folder can be told apart");
            return Self(Err(io::ErrorKind::NotFound));
        };
        match okapi_platform::guarded_personal_folders(config, home) {
            Ok(folders) => Self::of(&folders, identity_of),
            Err(error) => {
                tracing::warn!(%error, "personal folders unreadable: no folder to clean can be judged");
                Self(Err(error.kind()))
            }
        }
    }

    fn refusal(&self, identity: FileIdentity) -> Option<CustomFolderRefusal> {
        match &self.0 {
            Ok(identities) => identities
                .contains(&identity)
                .then_some(CustomFolderRefusal::Personal),
            Err(error) => Some(CustomFolderRefusal::PersonalUnknown(*error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_an_absolute_path_without_parent_steps() {
        assert_eq!(
            CustomFolder::new(PathBuf::new()),
            Err(CustomFolderError::Empty)
        );
        for written in ["relative/build", "build", "/data/../etc", "/data/x/.."] {
            assert_eq!(
                CustomFolder::new(PathBuf::from(written)),
                Err(CustomFolderError::Invalid {
                    path: PathBuf::from(written)
                }),
                "{written}"
            );
        }
        let folder = CustomFolder::new(PathBuf::from("/data/./build/")).unwrap();
        assert_eq!(folder.path(), Path::new("/data/./build/"), "as written");
    }
}
