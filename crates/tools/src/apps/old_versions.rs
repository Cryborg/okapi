//! What the package managers keep that no application needs any more: the
//! old revisions a snap keeps to go back to (snapd keeps the one before at
//! each refresh, `refresh.retain`), and the flatpak runtimes no
//! application uses ([`flatpak::unused`]). Each is named, with the room it
//! gives back, and removed by its own daemon once the user says yes: snapd
//! on its socket (one revision at a time), flatpak's system helper on the
//! system bus. Nothing is removed by Okapi, nothing is run (invariant 4).
//! An old version is known by an identifier of the report that found it
//! (invariant 1), and is asked to go only if, read again, it still is what
//! was shown.
//!
//! A revision is never one in use, one of a snap with none in use (a snap
//! disabled: snapd would remove its current one), nor of a higher number
//! than the one in use (left by a revert); never a kernel's or a gadget's (a boot falls back
//! on them). Its file is a hard link snapd's download cache shares: the
//! room comes back once the cache lets it go, when snapd decides.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use jiff::Timestamp;
use okapi_engine::report_nonce;
use serde_json::Value;

use crate::apps::flatpak::unused::{BLOCK_BYTES, UnusedRuntime, about_bytes, unused_runtimes};
use crate::apps::flatpak::{FlatpakRef, presence};
use crate::apps::leftovers::InstalledPlaces;
use crate::apps::snap::{Revision, revisions_by_name};
use crate::apps::system::{followed, why_flatpak, why_snapd};
use crate::apps::uninstall::Why;
use crate::packages::flatpak::{FlatpakError, RuntimeEnd, SystemHelper};
use crate::packages::snapd::{Snapd, SnapdError, is_revision};

/// How long snapd's removal of a revision is followed once the rights are
/// given: unmounting it and erasing its data, minutes at worst.
pub const REVISION_WAIT: Duration = Duration::from_mins(10);
/// What snapd says of the revision in use.
const STATUS_ACTIVE: &str = "active";
/// The types of snap whose old revisions may go: never a kernel's nor a
/// gadget's, which a boot falls back on.
const TYPES_OFFERED: [&str; 4] = ["app", "base", "snapd", "os"];
/// The links a snap's file has when snapd's download cache shares it: its
/// own, and the cache's.
const LINKS_WITH_THE_CACHE: u64 = 2;
/// Where the accounts of this system are listed.
pub const ACCOUNTS: &str = "/etc/passwd";
/// The users a person logs in as: below are the system's, above the
/// nobody of user namespaces and systemd's dynamic users.
const PERSONS: std::ops::Range<u32> = 1000..60_000;
/// The shells that log nobody in.
const NO_LOGIN: [&str; 2] = ["nologin", "false"];
/// The most of the accounts' list read.
const ACCOUNTS_LIMIT: usize = 4 * 1024 * 1024;

/// When the room of a revision comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Room {
    /// Its file has no other name: as soon as it goes.
    Now(u64),
    /// snapd's download cache shares its file: once it lets it go.
    Later(u64),
    /// Another snap shares its file (a parallel instance): it stays.
    Shared(u64),
    /// Its file could not be looked at.
    Unknown,
}

/// An old revision of a snap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapRevision {
    pub snap: String,
    pub revision: String,
    pub version: String,
    pub room: Room,
    /// Since when the revision in use is, as snapd says.
    pub current_since: Option<Timestamp>,
}

/// A runtime of flatpak's system installation no application uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldRuntime {
    pub reference: FlatpakRef,
    /// About what goes once its repository is pruned; `None` when it could
    /// not be measured.
    pub bytes: Option<u64>,
    deployment: PathBuf,
}

/// What an old version is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OldKind {
    Revision(SnapRevision),
    Runtime(OldRuntime),
}

/// The identifier of an old version, valid only in the report that gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OldVersionId {
    report: u64,
    index: usize,
}

/// An old version, as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldVersion {
    id: OldVersionId,
    kind: OldKind,
}

impl OldVersion {
    #[must_use]
    pub const fn id(&self) -> OldVersionId {
        self.id
    }

    #[must_use]
    pub const fn kind(&self) -> &OldKind {
        &self.kind
    }
}

/// What a search found, built by [`find`] alone: removing takes the report
/// and identifiers of it (invariant 1), and [`retire`] reads everything
/// again before asking anything.
#[derive(Debug)]
pub struct OldVersionReport {
    id: u64,
    versions: Vec<OldVersion>,
    places: InstalledPlaces,
}

impl OldVersionReport {
    #[must_use]
    pub fn versions(&self) -> &[OldVersion] {
        &self.versions
    }

    /// The old version `id` names, if it is of this report.
    #[must_use]
    pub fn find(&self, id: OldVersionId) -> Option<&OldVersion> {
        (id.report == self.id)
            .then(|| self.versions.get(id.index))
            .flatten()
    }
}

/// How a source of old versions was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OldReading {
    /// It is not on this system.
    Absent,
    Read,
    /// It could not be read whole: none of it is proposed.
    Unread(String),
}

/// What a search found.
#[derive(Debug)]
pub struct OldVersions {
    pub report: OldVersionReport,
    pub snaps: OldReading,
    pub flatpaks: OldReading,
    /// The runtimes of the user's own installation no application uses:
    /// never proposed (no service changes it), only counted.
    pub for_account_only: usize,
    /// Other persons log in on this system: Okapi does not see what they
    /// installed for themselves, which may use a runtime of the system's.
    pub other_accounts: bool,
}

/// The old revisions `listed` (snapd's `/v2/snaps?select=all` result)
/// holds, as [`find`] offers them, the room of each looked at in
/// `snap_files`.
///
/// # Errors
///
/// `listed` is no list of revisions.
fn old_revisions(listed: &Value, snap_files: &Path) -> Result<Vec<SnapRevision>, String> {
    let by_name = revisions_by_name(listed).map_err(|error| error.to_string())?;
    Ok(by_name
        .into_iter()
        .flat_map(|(snap, revisions)| offered(&snap, revisions, snap_files))
        .collect())
}

/// The revisions of `snap` that may go: those older than the one in use,
/// when there is exactly one in use, of a type whose old revisions may go,
/// and not broken (each passed over traced).
fn offered(snap: &str, revisions: Vec<Revision>, snap_files: &Path) -> Vec<SnapRevision> {
    let (active, old): (Vec<Revision>, Vec<Revision>) = revisions
        .into_iter()
        .partition(|revision| revision.status == STATUS_ACTIVE);
    let [active] = active.as_slice() else {
        if !old.is_empty() {
            tracing::info!(
                snap,
                in_use = active.len(),
                "a snap with no single revision in use: none of its revisions offered"
            );
        }
        return Vec::new();
    };
    if !TYPES_OFFERED.contains(&active.kind.as_str()) {
        tracing::info!(
            snap,
            kind = active.kind,
            "a snap of this type keeps its old revisions"
        );
        return Vec::new();
    }
    let Some(in_use) = number(&active.number) else {
        tracing::warn!(
            snap,
            revision = active.number,
            "the revision in use not a number: none offered"
        );
        return Vec::new();
    };
    let current_since = active
        .install_date
        .as_deref()
        .and_then(|date| {
            date.parse::<Timestamp>()
                .inspect_err(|error| tracing::debug!(snap, date, %error, "since when the revision in use is, unread"))
                .ok()
        });
    old.into_iter()
        .filter(|revision| {
            let older = number(&revision.number).is_some_and(|number| number < in_use);
            if !older || revision.broken.is_some() {
                tracing::info!(snap, revision = revision.number, broken = ?revision.broken, "a revision not offered: newer than the one in use, not a number, or broken");
            }
            older && revision.broken.is_none()
        })
        .map(|revision| SnapRevision {
            snap: snap.to_owned(),
            room: room(&snap_files.join(format!("{snap}_{}.snap", revision.number))),
            revision: revision.number,
            version: revision.version,
            current_since,
        })
        .collect()
}

/// `revision` as a number, if it is one from the store.
fn number(revision: &str) -> Option<u64> {
    is_revision(revision)
        .then(|| revision.parse().ok())
        .flatten()
}

/// When the room of the snap's file at `file` comes back.
fn room(file: &Path) -> Room {
    match std::fs::symlink_metadata(file) {
        Ok(metadata) if metadata.is_file() => {
            let bytes = metadata.blocks().saturating_mul(BLOCK_BYTES);
            match metadata.nlink() {
                1 => Room::Now(bytes),
                LINKS_WITH_THE_CACHE => Room::Later(bytes),
                _ => Room::Shared(bytes),
            }
        }
        Ok(_) => {
            tracing::warn!(path = %file.display(), "a snap's file is no file: its room unknown");
            Room::Unknown
        }
        Err(error) => {
            tracing::warn!(path = %file.display(), %error, code = error.raw_os_error(), "a snap's file not looked at: its room unknown");
            Room::Unknown
        }
    }
}

/// Whether `accounts` (`/etc/passwd`) lists a person other than `me`.
fn others_in(accounts: &str, me: Option<u32>) -> bool {
    accounts.lines().any(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        let (Some(uid), Some(shell)) = (fields.get(2), fields.get(6)) else {
            return false;
        };
        let logs_in = !NO_LOGIN
            .iter()
            .any(|none| shell.rsplit('/').next() == Some(none));
        uid.parse::<u32>()
            .is_ok_and(|uid| PERSONS.contains(&uid) && Some(uid) != me && logs_in)
    })
}

/// Whether another person logs in on this system, as `accounts` lists
/// them; unreadable, they may.
fn other_accounts(accounts: &Path) -> bool {
    match okapi_platform::read_regular_capped(accounts, ACCOUNTS_LIMIT) {
        Ok(bytes) => others_in(
            &String::from_utf8_lossy(&bytes),
            okapi_platform::real_user(),
        ),
        Err(error) => {
            tracing::warn!(path = %accounts.display(), %error, "the accounts unread: others taken to log in");
            true
        }
    }
}

/// What snapd's revisions (`listed`, its `/v2/snaps?select=all` answer) and
/// flatpak's installations of `places` keep that no application needs,
/// the persons of this system read from `accounts`; the runtimes measured
/// until `stop` is set. Okapi confined sees neither as the system holds
/// them: both unread.
#[must_use]
pub fn find(
    listed: Result<Value, SnapdError>,
    places: InstalledPlaces,
    accounts: &Path,
    stop: &AtomicBool,
) -> OldVersions {
    let _span = tracing::info_span!("old_versions").entered();
    let id = report_nonce();
    let mut versions = Vec::new();
    let unread_confined = places.confined.as_ref().map(|mark| {
        tracing::warn!(mark, "Okapi runs confined: no old version proposed");
        OldReading::Unread(format!("Okapi runs confined ({mark})"))
    });
    let snaps = match (&unread_confined, listed) {
        (Some(unread), _) => unread.clone(),
        (None, Err(SnapdError::Absent { .. })) => OldReading::Absent,
        (None, Err(error)) => {
            tracing::warn!(%error, "snapd's revisions unread: none proposed");
            OldReading::Unread(error.to_string())
        }
        (None, Ok(listed)) => match old_revisions(&listed, &places.snap_files) {
            Ok(revisions) => {
                versions.extend(revisions.into_iter().map(OldKind::Revision));
                OldReading::Read
            }
            Err(why) => {
                tracing::warn!(why, "snapd's revisions unreadable: none proposed");
                OldReading::Unread(why)
            }
        },
    };
    let mut for_account_only = 0;
    let flatpaks = if let Some(unread) = unread_confined {
        unread
    } else if places
        .installations
        .iter()
        .all(|installation| presence(installation) == Ok(false))
    {
        OldReading::Absent
    } else {
        match unused_runtimes(&places.installations) {
            Ok(unused) => {
                for_account_only = unused.for_account_only;
                versions.extend(unused.runtimes.into_iter().map(|runtime| {
                    OldKind::Runtime(OldRuntime {
                        bytes: about_bytes(&runtime.deployment, stop),
                        reference: runtime.reference,
                        deployment: runtime.deployment,
                    })
                }));
                OldReading::Read
            }
            Err(why) => OldReading::Unread(why),
        }
    };
    let other_accounts = other_accounts(accounts);
    let versions: Vec<OldVersion> = versions
        .into_iter()
        .enumerate()
        .map(|(index, kind)| OldVersion {
            id: OldVersionId { report: id, index },
            kind,
        })
        .collect();
    tracing::info!(
        proposed = versions.len(),
        ?snaps,
        ?flatpaks,
        for_account_only,
        other_accounts,
        "old versions found"
    );
    OldVersions {
        report: OldVersionReport {
            id,
            versions,
            places,
        },
        snaps,
        flatpaks,
        for_account_only,
        other_accounts,
    }
}

/// What became of an old version asked to go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retired {
    Removed,
    /// Gone before it was asked, or while polkit asked the user.
    Gone,
    /// No longer what was shown (in use again, used, another deployment):
    /// kept, nothing asked.
    Changed,
    /// Not asked: the rights were refused earlier, or the daemon fell
    /// silent.
    NotAsked,
    Failed(Why),
}

/// Whether `why` keeps the next ones of the same daemon from being asked:
/// the rights refused, the daemon absent or silent past its wait.
const fn stops_the_rest(why: &Why) -> bool {
    matches!(why, Why::NotAuthorized | Why::Absent | Why::TimedOut)
}

/// Asks the daemons to remove the old versions `ids` of `report`, each read
/// again first: snapd's revisions one at a time, then flatpak's runtimes;
/// `told` hears each as it ends. An identifier of another report asks
/// nothing at all (invariant 1). Never called on the interface's thread:
/// polkit may wait for the user. Whether runtimes went while their room is
/// not given back (flatpak's helper did not prune its repository). A
/// runtime past its wait is not read again: the list read after says
/// whether it went.
#[must_use]
pub fn retire(
    report: &OldVersionReport,
    ids: &[OldVersionId],
    snapd: &Snapd,
    helper: &dyn Fn() -> Result<SystemHelper, FlatpakError>,
    wait: Duration,
    told: &dyn Fn(&OldVersion, &Retired),
) -> bool {
    let _span = tracing::info_span!("old_versions_removal", asked = ids.len()).entered();
    let Some(asked) = ids
        .iter()
        .map(|id| report.find(*id))
        .collect::<Option<Vec<&OldVersion>>>()
    else {
        tracing::warn!(
            ?ids,
            "an old version of another report asked for: nothing asked (invariant 1)"
        );
        for id in ids {
            if let Some(version) = report.find(*id) {
                told(version, &Retired::Failed(Why::Unclear));
            }
        }
        return false;
    };
    let mut stopped = false;
    for version in &asked {
        let OldKind::Revision(revision) = &version.kind else {
            continue;
        };
        let retired = if stopped {
            Retired::NotAsked
        } else {
            retire_revision(snapd, revision, &report.places.snap_files, wait)
        };
        stopped |= matches!(&retired, Retired::Failed(why) if stops_the_rest(why));
        told(version, &retired);
    }
    let runtimes: Vec<(&OldVersion, &OldRuntime)> = asked
        .iter()
        .filter_map(|version| match &version.kind {
            OldKind::Runtime(runtime) => Some((*version, runtime)),
            OldKind::Revision(_) => None,
        })
        .collect();
    !runtimes.is_empty() && retire_runtimes(&report.places, &runtimes, helper, told)
}

/// Whether `revision` is still offered as read now: `Err` says what it
/// became, nothing asked.
fn still_old(snapd: &Snapd, revision: &SnapRevision, snap_files: &Path) -> Result<(), Retired> {
    let listed = snapd.snaps().map_err(|error| {
        tracing::warn!(%error, "snapd's revisions unread again: nothing asked");
        // Absent or silent: the next ones are not asked either.
        Retired::Failed(why_snapd(&error))
    })?;
    let offered_now = old_revisions(&listed, snap_files).map_err(|why| {
        tracing::warn!(why, "snapd's revisions unreadable again: nothing asked");
        Retired::Failed(Why::Unclear)
    })?;
    if offered_now
        .iter()
        .any(|now| now.snap == revision.snap && now.revision == revision.revision)
    {
        return Ok(());
    }
    match listed_still(&listed, revision) {
        Ok(true) => {
            tracing::warn!(
                "the revision no longer offered (in use again, its snap disabled): nothing asked"
            );
            Err(Retired::Changed)
        }
        Ok(false) => {
            tracing::info!("the revision gone already");
            Err(Retired::Gone)
        }
        Err(error) => {
            tracing::warn!(%error, "snapd's revisions unreadable again: nothing asked");
            Err(Retired::Failed(Why::Unclear))
        }
    }
}

/// Whether `listed` (snapd's revisions) still holds `revision`.
///
/// # Errors
///
/// `listed` is no list of revisions: whether it went is unknown, never
/// taken for gone.
fn listed_still(listed: &Value, revision: &SnapRevision) -> Result<bool, serde_json::Error> {
    let by_name: BTreeMap<String, Vec<Revision>> = revisions_by_name(listed)?;
    Ok(by_name.get(&revision.snap).is_some_and(|revisions| {
        revisions
            .iter()
            .any(|listed| listed.number == revision.revision)
    }))
}

/// Asks snapd to remove `revision`, once read again, and follows its change
/// for `wait`; followed no longer, the revision listed or not says.
fn retire_revision(
    snapd: &Snapd,
    revision: &SnapRevision,
    snap_files: &Path,
    wait: Duration,
) -> Retired {
    let _span = tracing::info_span!(
        "snap_revision_removal",
        snap = revision.snap,
        revision = revision.revision
    )
    .entered();
    if let Err(retired) = still_old(snapd, revision, snap_files) {
        return retired;
    }
    let gone = || Ok(!listed_still(&snapd.snaps()?, revision)?);
    let change = match snapd.remove_revision(&revision.snap, &revision.revision) {
        Ok(change) => change,
        Err(error) => {
            tracing::warn!(%error, "snapd refused to remove the revision");
            return match gone() {
                Ok(true) => Retired::Gone,
                Ok(false) | Err(_) => Retired::Failed(why_snapd(&error)),
            };
        }
    };
    tracing::info!(change, "snapd removes the revision");
    match followed(snapd, &change, wait, &gone) {
        Ok(()) => {
            tracing::info!(room = ?revision.room, "the revision removed");
            Retired::Removed
        }
        Err(why) => Retired::Failed(why),
    }
}

/// Asks flatpak's helper to remove `runtimes`, those still unused at the
/// same deployment once judged again; the others kept.
fn retire_runtimes(
    places: &InstalledPlaces,
    runtimes: &[(&OldVersion, &OldRuntime)],
    helper: &dyn Fn() -> Result<SystemHelper, FlatpakError>,
    told: &dyn Fn(&OldVersion, &Retired),
) -> bool {
    let now: Result<Vec<UnusedRuntime>, Retired> = if let Some(mark) = &places.confined {
        tracing::warn!(mark, "Okapi runs confined: no runtime asked");
        Err(Retired::Failed(Why::Unclear))
    } else {
        unused_runtimes(&places.installations)
            .map(|unused| unused.runtimes)
            .map_err(|why| {
                tracing::warn!(why, "flatpak's runtimes not judged again: nothing asked");
                Retired::Failed(Why::Unclear)
            })
    };
    let now = match now {
        Ok(now) => now,
        Err(retired) => {
            for (version, _) in runtimes {
                told(version, &retired);
            }
            return false;
        }
    };
    let mut going: Vec<&OldVersion> = Vec::new();
    for (version, runtime) in runtimes {
        let same = now.iter().any(|unused| {
            unused.reference == runtime.reference && unused.deployment == runtime.deployment
        });
        if same {
            going.push(version);
        } else {
            tracing::warn!(reference = %runtime.reference, "a runtime no longer as shown (used, gone or deployed again): kept");
            told(version, &Retired::Changed);
        }
    }
    if going.is_empty() {
        return false;
    }
    let helper = match helper() {
        Ok(helper) => helper,
        Err(error) => {
            tracing::warn!(%error, "flatpak's helper not reached: no runtime asked");
            let why = why_flatpak(&error);
            for version in going {
                told(version, &Retired::Failed(why.clone()));
            }
            return false;
        }
    };
    let references: Vec<String> = going
        .iter()
        .filter_map(|version| match &version.kind {
            OldKind::Runtime(runtime) => Some(runtime.reference.to_string()),
            OldKind::Revision(_) => None,
        })
        .collect();
    let removed = helper.remove_runtimes(&references, &|rank, end| {
        let retired = match end {
            RuntimeEnd::Removed => Retired::Removed,
            RuntimeEnd::AlreadyGone => Retired::Gone,
            RuntimeEnd::NotAsked => Retired::NotAsked,
            RuntimeEnd::Failed(error) => Retired::Failed(why_flatpak(error)),
        };
        if let Some(version) = going.get(rank) {
            told(version, &retired);
        }
    });
    let withheld = !removed.pruned
        && removed
            .ends
            .iter()
            .any(|end| matches!(end, RuntimeEnd::Removed));
    if withheld {
        tracing::warn!("runtimes removed, the repository not pruned: their room not given back");
    }
    withheld
}

#[cfg(test)]
mod tests;
