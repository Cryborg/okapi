//! What the helper is asked, and the check that it asks for nothing more than
//! the targets of its own rules that need the administrator.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::io::{self, BufRead, Read};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use okapi_engine::{ExclusionError, Exclusions, Origin, ScanOptions, settings_min_age};
use okapi_rules::{Effect, RuleSet};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::PROTOCOL_VERSION;

/// The longest request line, without its line break: a request names a few
/// targets and exclusions, never more.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The youngest an entry the helper removes may be, whatever it is asked: a
/// floor of its own, not the settings' default, which may change.
pub const HELPER_MIN_AGE_FLOOR: Duration = Duration::from_mins(60);

/// What the helper does with the targets it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Operation {
    /// Says what the targets offer, without changing anything.
    Scan,
    /// Removes what the targets offer, but a group that joins a rule not
    /// asked for.
    Clean,
}

/// The request as written, one JSON object on one line:
/// `{"version": 2, "operation": "clean", "targets": ["/var/crash"],
/// "minAgeMinutes": 60, "exclusions": ["/var/crash/keep"]}`. The caller
/// writes it with the type the helper reads it with ([`request_line`]).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Written {
    version: u32,
    operation: Operation,
    targets: Vec<String>,
    min_age_minutes: u64,
    exclusions: Vec<PathBuf>,
}

/// The version alone, read before anything else: a document of another
/// version is refused as such, not for a field it adds.
#[derive(Deserialize)]
pub(crate) struct Versioned {
    pub(crate) version: u32,
}

/// The request line the helper reads (without its line break), for the
/// caller: `targets` by the path the rules write them with, an age in
/// minutes (the helper refuses one below [`HELPER_MIN_AGE_FLOOR`]), and the
/// exclusions.
///
/// # Errors
///
/// [`Refusal::TooLong`]: the helper would refuse it, so it is never sent;
/// [`Refusal::Invalid`]: an exclusion is not UTF-8, which no request carries.
pub fn request_line(
    operation: Operation,
    targets: &[String],
    min_age_minutes: u64,
    exclusions: &[PathBuf],
) -> Result<Vec<u8>, Refusal> {
    let written = Written {
        version: PROTOCOL_VERSION,
        operation,
        targets: targets.to_vec(),
        min_age_minutes,
        exclusions: exclusions.to_vec(),
    };
    // A structure of strings, numbers and paths: always serializable, but
    // for a path that is not UTF-8, which no request can carry.
    let line = serde_json::to_vec(&written).map_err(|error| Refusal::Invalid {
        detail: error.to_string(),
    })?;
    if line.len() > MAX_REQUEST_BYTES {
        return Err(Refusal::TooLong);
    }
    Ok(line)
}

/// Why a request is refused: nothing is then read nor touched.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("the request is longer than {MAX_REQUEST_BYTES} bytes")]
    TooLong,

    #[error("the request is not a JSON object of the expected form: {detail}")]
    Invalid { detail: String },

    #[error("the request is of version {version}, the helper reads version {PROTOCOL_VERSION}")]
    UnsupportedVersion { version: u32 },

    #[error("the request names no target")]
    NoTarget,

    #[error("`{target}` is not a target of the helper's rules that needs the administrator")]
    UnknownTarget { target: String },

    #[error("`{target}` is named twice")]
    DuplicateTarget { target: String },

    #[error("a minimum age of {minutes} minutes is below the helper's floor")]
    AgeBelowFloor { minutes: u64 },

    #[error("a minimum age of {minutes} minutes reaches before the clock's origin")]
    AgeBeyondClock { minutes: u64 },

    #[error("an exclusion cannot be used: {error}")]
    Exclusion { error: ExclusionError },
}

impl Refusal {
    /// Its stable name, for the caller.
    #[must_use]
    pub const fn key(&self) -> &'static str {
        match self {
            Self::TooLong => "tooLong",
            Self::Invalid { .. } => "invalid",
            Self::UnsupportedVersion { .. } => "unsupportedVersion",
            Self::NoTarget => "noTarget",
            Self::UnknownTarget { .. } => "unknownTarget",
            Self::DuplicateTarget { .. } => "duplicateTarget",
            Self::AgeBelowFloor { .. } => "ageBelowFloor",
            Self::AgeBeyondClock { .. } => "ageBeyondClock",
            Self::Exclusion { .. } => "exclusion",
        }
    }

    /// The target at fault, when one is.
    #[must_use]
    pub fn target(&self) -> Option<&str> {
        match self {
            Self::UnknownTarget { target } | Self::DuplicateTarget { target } => Some(target),
            Self::TooLong
            | Self::Invalid { .. }
            | Self::UnsupportedVersion { .. }
            | Self::NoTarget
            | Self::AgeBelowFloor { .. }
            | Self::AgeBeyondClock { .. }
            | Self::Exclusion { .. } => None,
        }
    }
}

/// The targets asked for, by origin, each with the path it was asked by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection(BTreeMap<Origin, String>);

impl Selection {
    /// The targets `names`, each the exact path of a target of `rules` that
    /// deletes, needs the administrator and holds no variable (the same for
    /// every user, root included); never none.
    ///
    /// # Errors
    ///
    /// [`Refusal::NoTarget`], [`Refusal::UnknownTarget`] (a path written
    /// otherwise, `/var/crash/`, is unknown) or [`Refusal::DuplicateTarget`].
    pub fn of(rules: &RuleSet, names: &[String]) -> Result<Self, Refusal> {
        if names.is_empty() {
            return Err(Refusal::NoTarget);
        }
        let allowed = admin_targets(rules);
        let mut selection = BTreeMap::new();
        for name in names {
            let origin = allowed
                .get(name.as_str())
                .ok_or_else(|| Refusal::UnknownTarget {
                    target: name.clone(),
                })?;
            match selection.entry(origin.clone()) {
                Entry::Vacant(slot) => {
                    slot.insert(name.clone());
                }
                Entry::Occupied(_) => {
                    return Err(Refusal::DuplicateTarget {
                        target: name.clone(),
                    });
                }
            }
        }
        Ok(Self(selection))
    }

    /// Whether `origin` was asked for.
    #[must_use]
    pub fn contains(&self, origin: &Origin) -> bool {
        self.0.contains_key(origin)
    }

    /// The path `origin` was asked by, if it was.
    #[must_use]
    pub fn name_of(&self, origin: &Origin) -> Option<&str> {
        self.0.get(origin).map(String::as_str)
    }

    /// `options`, walking only the targets asked for.
    #[must_use]
    pub fn restrict(&self, options: ScanOptions) -> ScanOptions {
        options.with_origins(self.0.keys().cloned().collect())
    }
}

/// The targets the helper may be asked for, by their path as written in the
/// rules: those that delete, need the administrator and hold no variable.
/// Unique: no other target that deletes has the path of one of them
/// (`okapi-rules`).
#[must_use]
pub fn admin_targets(rules: &RuleSet) -> BTreeMap<String, Origin> {
    rules
        .system()
        .clean_targets()
        .iter()
        .filter(|target| target.needs_admin())
        .filter_map(|target| match target.effect() {
            Effect::Delete(deletion) if deletion.path().is_literal() => {
                Some((deletion.path().to_string(), Origin::of_target(target)?))
            }
            Effect::Delete(_) | Effect::Maintenance { .. } => None,
        })
        .collect()
}

/// A request checked against the rules: what to do, on which targets, with
/// which age and exclusions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    operation: Operation,
    selection: Selection,
    min_age: Duration,
    exclusions: Exclusions,
}

impl Request {
    #[must_use]
    pub const fn operation(&self) -> Operation {
        self.operation
    }

    #[must_use]
    pub const fn selection(&self) -> &Selection {
        &self.selection
    }

    /// Reads the request `line` and checks it against `rules`, the ages
    /// measured from `now`.
    ///
    /// # Errors
    ///
    /// [`Refusal`]: the request asks for anything but targets of the rules
    /// that need the administrator, or for an age below the floor.
    pub fn read(line: &[u8], rules: &RuleSet, now: SystemTime) -> Result<Self, Refusal> {
        if line.len() > MAX_REQUEST_BYTES {
            return Err(Refusal::TooLong);
        }
        let invalid = |error: serde_json::Error| Refusal::Invalid {
            detail: error.to_string(),
        };
        let Versioned { version } = serde_json::from_slice(line).map_err(invalid)?;
        if version != PROTOCOL_VERSION {
            return Err(Refusal::UnsupportedVersion { version });
        }
        let written: Written = serde_json::from_slice(line).map_err(invalid)?;
        let selection = Selection::of(rules, &written.targets)?;
        let minutes = written.min_age_minutes;
        let min_age = settings_min_age(minutes, now).ok_or(Refusal::AgeBeyondClock { minutes })?;
        if min_age < HELPER_MIN_AGE_FLOOR {
            return Err(Refusal::AgeBelowFloor { minutes });
        }
        let exclusions =
            Exclusions::new(written.exclusions).map_err(|error| Refusal::Exclusion { error })?;
        Ok(Self {
            operation: written.operation,
            selection,
            min_age,
            exclusions,
        })
    }

    /// The age and exclusions of its scan and its cleaning, ages measured
    /// from `now`; [`crate::operate`] restricts them to its targets.
    #[must_use]
    pub fn options(&self, now: SystemTime) -> ScanOptions {
        ScanOptions::new(now, self.min_age, self.exclusions.clone())
    }
}

/// Reads the request line from `input`: at most [`MAX_REQUEST_BYTES`] and
/// one more byte (enough to tell a line too long), without its line break.
///
/// # Errors
///
/// The error reading `input`.
pub fn read_request(input: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let limit = u64::try_from(MAX_REQUEST_BYTES).map_or(u64::MAX, |bytes| bytes + 1);
    input.take(limit).read_until(b'\n', &mut line)?;
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(line)
}
