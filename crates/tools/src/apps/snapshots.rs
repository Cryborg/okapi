//! The copies snapd keeps of the data of a snap removed (its automatic
//! snapshots), for every account: each one of a snap no longer installed
//! may be forgotten, by snapd, once the user says yes. snapd forgets them
//! by itself after a while (31 days unless set otherwise); Okapi only gives
//! the space back sooner. Nothing is removed by Okapi (invariant 4): snapd
//! erases its own copy, asked on its socket, polkit asking the rights. A
//! copy is known by an identifier of the report that found it (invariant
//! 1), and is asked to be forgotten only if snapd, asked again, still lists
//! it as it was and its snap is still not installed.

use std::time::Duration;

use jiff::Timestamp;
use okapi_engine::report_nonce;
use serde::Deserialize;
use serde_json::Value;

use crate::apps::leftovers::Known;
use crate::apps::system::{followed, why_snapd};
use crate::apps::uninstall::Why;
use crate::packages::snapd::{Snapd, SnapdError, is_snap_name};

/// How long snapd's forgetting is followed once the rights are given:
/// erasing a copy's archives takes seconds, minutes at worst.
pub const FORGET_WAIT: Duration = Duration::from_mins(5);

/// A copy as snapd lists it, by the fields read: snapd never gives a set's
/// number twice, and dates a copy to the nanosecond.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Snapshot {
    set: u64,
    snap: String,
    /// When it was made, in RFC 3339.
    time: String,
    /// snapd leaves out what is zero or false.
    #[serde(default)]
    size: u64,
    #[serde(default)]
    auto: bool,
    /// Why it cannot be restored, when it cannot.
    #[serde(default)]
    broken: Option<String>,
}

/// The copies of every set `listed` (snapd's `/v2/snapshots` result) holds.
///
/// # Errors
///
/// `listed` is no list of sets.
fn entries(listed: &Value) -> Result<impl Iterator<Item = &Value>, &'static str> {
    Ok(listed
        .as_array()
        .ok_or("snapd's copies are no list")?
        .iter()
        .filter_map(|set| set.get("snapshots").and_then(Value::as_array))
        .flatten())
}

/// The copies snapd made by itself that `listed` (its `/v2/snapshots`
/// result) holds, each whole, of a snap's name, made at a date read; the
/// others passed over, traced: a copy made by hand is the user's choice.
///
/// # Errors
///
/// `listed` is no list of sets.
fn automatic(listed: &Value) -> Result<Vec<(Snapshot, Timestamp)>, &'static str> {
    let mut found = Vec::new();
    for entry in entries(listed)? {
        let snapshot = match Snapshot::deserialize(entry) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "a copy of snapd unreadable: passed over");
                continue;
            }
        };
        if !snapshot.auto {
            tracing::debug!(
                set = snapshot.set,
                snap = snapshot.snap,
                "a copy made by hand: never proposed"
            );
            continue;
        }
        let made = snapshot
            .time
            .parse::<Timestamp>()
            .ok()
            .filter(|made| *made > Timestamp::UNIX_EPOCH);
        match made {
            Some(made) if snapshot.broken.is_none() && is_snap_name(&snapshot.snap) => {
                found.push((snapshot, made));
            }
            _ => {
                tracing::warn!(set = snapshot.set, snap = snapshot.snap, time = snapshot.time, broken = ?snapshot.broken, "a copy of snapd broken, undated or of no snap's name: passed over");
            }
        }
    }
    Ok(found)
}

/// The identifier of a copy, valid only in the report that gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyId {
    report: u64,
    index: usize,
}

/// A copy snapd made by itself of the data of a snap no longer installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomaticCopy {
    id: CopyId,
    listed: Snapshot,
    made: Timestamp,
}

impl AutomaticCopy {
    #[must_use]
    pub const fn id(&self) -> CopyId {
        self.id
    }

    /// The snap whose data it holds.
    #[must_use]
    pub fn snap(&self) -> &str {
        &self.listed.snap
    }

    #[must_use]
    pub const fn made(&self) -> Timestamp {
        self.made
    }

    /// What it takes, as snapd says.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.listed.size
    }
}

/// What a reading of the copies found, built by [`find`] alone: forgetting
/// a copy takes the report and an identifier of it (invariant 1), and
/// [`forget`] reads snapd again before asking anything.
#[derive(Debug)]
pub struct CopyReport {
    id: u64,
    copies: Vec<AutomaticCopy>,
}

impl CopyReport {
    #[must_use]
    pub fn copies(&self) -> &[AutomaticCopy] {
        &self.copies
    }

    /// The copy `id` names, if it is of this report.
    #[must_use]
    pub fn find(&self, id: CopyId) -> Option<&AutomaticCopy> {
        (id.report == self.id)
            .then(|| self.copies.get(id.index))
            .flatten()
    }
}

/// What snapd keeps, as far as it could be read.
#[derive(Debug)]
pub enum Copies {
    /// No snapd on this system: no copy either.
    None,
    Found(CopyReport),
    /// The copies, or the snaps installed, could not be read: none is
    /// proposed.
    Unread(String),
}

/// The copies snapd made by itself of snaps not installed, as `listed`
/// (its `/v2/snapshots` answer) and `installed` (the snaps, read whole)
/// say.
#[must_use]
pub fn find(listed: Result<Value, SnapdError>, installed: &Known) -> Copies {
    let _span = tracing::info_span!("snap_copies").entered();
    let listed = match listed {
        Ok(listed) => listed,
        Err(SnapdError::Absent { .. }) => {
            tracing::info!("no snapd: no copy");
            return Copies::None;
        }
        Err(error) => {
            tracing::warn!(%error, "snapd's copies unread: none proposed");
            return Copies::Unread(error.to_string());
        }
    };
    if let Known::Unread(why) = installed {
        tracing::warn!(why, "the snaps installed unread: no copy proposed");
        return Copies::Unread(why.clone());
    }
    let automatic = match automatic(&listed) {
        Ok(automatic) => automatic,
        Err(why) => {
            tracing::warn!(why, "snapd's copies unreadable: none proposed");
            return Copies::Unread(why.to_owned());
        }
    };
    let id = report_nonce();
    let listed_count = automatic.len();
    let copies: Vec<AutomaticCopy> = automatic
        .into_iter()
        .filter(|(snapshot, _)| installed.bears(&snapshot.snap) == Some(false))
        .enumerate()
        .map(|(index, (listed, made))| AutomaticCopy {
            id: CopyId { report: id, index },
            listed,
            made,
        })
        .collect();
    tracing::info!(
        automatic = listed_count,
        proposed = copies.len(),
        "snapd's copies read"
    );
    Copies::Found(CopyReport { id, copies })
}

/// What became of a copy asked to be forgotten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forgot {
    Forgotten,
    /// snapd lists it no more, or no longer as it was (broken since, made
    /// again): erased by snapd itself, before the asking or while polkit
    /// asked the user.
    Gone,
    /// Its snap is installed again: nothing asked.
    Reinstalled,
    Failed(Why),
}

/// Whether snapd still lists `copy` as it was read; read quietly, the list
/// once traced at the analysis.
fn still_listed(snapd: &Snapd, copy: &AutomaticCopy) -> Result<bool, SnapdError> {
    let listed = snapd.snapshots()?;
    Ok(entries(&listed)
        .map_err(SnapdError::Malformed)?
        .any(|entry| Snapshot::deserialize(entry).is_ok_and(|snapshot| snapshot == copy.listed)))
}

/// Whether `copy` may be asked to be forgotten: still listed as it was, its
/// snap still not installed.
///
/// # Errors
///
/// What it became instead, nothing asked.
fn still_to_forget(snapd: &Snapd, copy: &AutomaticCopy) -> Result<(), Forgot> {
    match still_listed(snapd, copy) {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!("the copy listed no more as it was: nothing asked");
            return Err(Forgot::Gone);
        }
        Err(error) => {
            // Nothing asked: whatever kept snapd from answering, no change
            // of its is unknown.
            tracing::warn!(%error, "snapd's copies unread again: nothing asked");
            return Err(Forgot::Failed(Why::Unclear));
        }
    }
    match snapd.installed(copy.snap()) {
        Ok(false) => Ok(()),
        Ok(true) => {
            tracing::warn!("the snap installed again: nothing asked");
            Err(Forgot::Reinstalled)
        }
        Err(error) => {
            tracing::warn!(%error, "whether the snap is installed unknown: nothing asked");
            Err(Forgot::Failed(Why::Unclear))
        }
    }
}

/// Asks snapd to forget the copy `id` of `report`, once read again: still
/// listed as it was, its snap still not installed. snapd's change is
/// followed for `wait`; followed no longer, the copy listed or not says.
/// Never called on the interface's thread: polkit may wait for the user.
#[must_use]
pub fn forget(report: &CopyReport, id: CopyId, snapd: &Snapd, wait: Duration) -> Forgot {
    let Some(copy) = report.find(id) else {
        tracing::warn!(
            ?id,
            "a copy of another report asked for: nothing asked (invariant 1)"
        );
        return Forgot::Failed(Why::Unclear);
    };
    let (set, snap) = (copy.listed.set, copy.snap());
    let _span = tracing::info_span!("snap_copy_forget", set, snap).entered();
    if let Err(forgot) = still_to_forget(snapd, copy) {
        return forgot;
    }
    let change = match snapd.forget(set, snap) {
        Ok(change) => change,
        Err(error) => {
            tracing::warn!(%error, "snapd refused to forget the copy");
            // Erased by snapd meanwhile (its own deadline), it is gone.
            return match still_listed(snapd, copy) {
                Ok(false) => {
                    tracing::info!("the copy listed no more: snapd erased it itself");
                    Forgot::Gone
                }
                Ok(true) => Forgot::Failed(why_snapd(&error)),
                Err(reread) => {
                    tracing::warn!(%reread, "snapd's copies unread after its refusal");
                    Forgot::Failed(why_snapd(&error))
                }
            };
        }
    };
    tracing::info!(change, "snapd forgets the copy");
    let gone = || still_listed(snapd, copy).map(|listed| !listed);
    match followed(snapd, &change, wait, &gone) {
        Ok(()) => {
            tracing::info!(bytes = copy.bytes(), "the copy forgotten");
            Forgot::Forgotten
        }
        Err(why) => Forgot::Failed(why),
    }
}

#[cfg(test)]
mod tests;
