use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use okapi_rules::{Os, TemplateVariables, Variable};

use crate::options::ScanOptions;
use crate::protection::ProtectedRoots;
use crate::walk::IdentityReader;

/// What keeps a scan or a cleaning from looking at anything, or a scan from
/// offering anything: the report or the receipt then holds nothing offered,
/// which does not mean there is nothing to clean.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// This system lacks what the engine needs to tell mount points, bind
    /// mounts and replaced entries apart (file identity, and for cleaning,
    /// removal through directory descriptors).
    UnsupportedSystem,
    /// A variable the rules use has no value: what it would protect cannot be
    /// vouched for (invariant 6).
    MissingVariable { variable: Variable },
    /// A variable's directory, or one of its ancestors, cannot be read: what it
    /// protects cannot be vouched for (invariant 6).
    UnreadableVariable {
        variable: Variable,
        path: PathBuf,
        error: io::ErrorKind,
    },
    /// The minimum age reaches before the clock's origin: nothing can be old
    /// enough.
    MinAgeBeyondClock,
    /// The scan was cancelled ([`crate::Cancel`]): what it found is not
    /// offered, a directory half read must not pass for whole. Never in a
    /// cleaning receipt.
    Cancelled,
}

/// The stops that need no file to be read: an unsupported system, or the
/// variables of `os` that have no value (a rule naming `/tmp` would then walk
/// into a `${HOME}` placed below it).
pub(crate) fn stops_before_reading(
    supported: bool,
    os: Os,
    variables: &TemplateVariables,
) -> Vec<Stop> {
    if !supported {
        return vec![Stop::UnsupportedSystem];
    }
    Variable::ALL
        .into_iter()
        .filter(|variable| variable.is_available_on(os))
        .filter(|variable| !variables.iter().any(|(set, _)| set == *variable))
        .map(|variable| Stop::MissingVariable { variable })
        .collect()
}

/// The cutoff of the minimum age, and the protection of the variables'
/// directories.
///
/// # Errors
///
/// [`Stop::MinAgeBeyondClock`] or [`Stop::UnreadableVariable`].
pub(crate) fn protection(
    variables: &TemplateVariables,
    options: &ScanOptions,
    identity_of: IdentityReader,
) -> Result<(SystemTime, ProtectedRoots), Stop> {
    let cutoff = options.cutoff(None).ok_or(Stop::MinAgeBeyondClock)?;
    let protected = ProtectedRoots::new(variables, identity_of).map_err(|unreadable| {
        Stop::UnreadableVariable {
            variable: unreadable.variable,
            path: unreadable.path,
            error: unreadable.error,
        }
    })?;
    Ok((cutoff, protected))
}
