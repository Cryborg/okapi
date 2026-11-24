//! The history line of an installation of updates
//! (`okapi_store::UpdateEntry`): every update asked, and what each came to,
//! told from the outcome the screen shows (one source for both); and the
//! stable names of its reasons, which the History screen reads back.

use okapi_store::{PackageSource, UpdateEntry, UpdatedPackage};
use okapi_tools::updates::SnapUpdate;
use okapi_tools::updates::install::{End, Ended, Why};

use super::install::{Outcome, Target, waits};
use crate::ui;

/// Why an update, or a whole installation, did not go, as its line keeps
/// it by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    /// What the installation failed of.
    Cause(Why),
    /// It was cancelled.
    Cancelled,
    /// It waits still once the list was read again, no cause said.
    Waiting,
    /// Whether it went is not known: dpkg ran then stopped, the daemon fell
    /// silent, or the list could not be read again.
    Unknown,
}

/// Every reason, each at its place (`reason_place` makes the compiler name
/// one left out), for its name to be read back.
const REASONS: [Reason; 13] = [
    Reason::Cancelled,
    Reason::Waiting,
    Reason::Unknown,
    Reason::Cause(Why::Busy),
    Reason::Cause(Why::Interrupted),
    Reason::Cause(Why::Offline),
    Reason::Cause(Why::NoSpace),
    Reason::Cause(Why::Refused),
    Reason::Cause(Why::Changed),
    Reason::Cause(Why::AppsOpen),
    Reason::Cause(Why::Unsigned),
    Reason::Cause(Why::Unanswered),
    Reason::Cause(Why::Broken),
];

impl Reason {
    /// The name the history keeps it by: never changed once written.
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Waiting => "waiting",
            Self::Unknown => "unknown",
            Self::Cause(why) => match why {
                Why::Busy => "busy",
                Why::Interrupted => "interrupted",
                Why::Offline => "offline",
                Why::NoSpace => "noSpace",
                Why::Refused => "refused",
                Why::Changed => "changed",
                Why::AppsOpen => "appsOpen",
                Why::Unsigned => "unsigned",
                Why::Unanswered => "unanswered",
                Why::Broken => "broken",
            },
        }
    }

    /// The reason named `key`, if it names one.
    pub(crate) fn of_key(key: &str) -> Option<Self> {
        REASONS.into_iter().find(|reason| reason.key() == key)
    }

    /// Why it ended as `end`, when it was not done.
    const fn of_end(end: End) -> Option<Self> {
        match end {
            End::Done => None,
            End::Cancelled => Some(Self::Cancelled),
            End::Failed(why) => Some(Self::Cause(why)),
        }
    }
}

/// Where `reason` is in [`REASONS`]; one added fails to compile here until
/// it is placed there too.
#[cfg(test)]
const fn reason_place(reason: Reason) -> usize {
    match reason {
        Reason::Cancelled => 0,
        Reason::Waiting => 1,
        Reason::Unknown => 2,
        Reason::Cause(why) => match why {
            Why::Busy => 3,
            Why::Interrupted => 4,
            Why::Offline => 5,
            Why::NoSpace => 6,
            Why::Refused => 7,
            Why::Changed => 8,
            Why::AppsOpen => 9,
            Why::Unsigned => 10,
            Why::Unanswered => 11,
            Why::Broken => 12,
        },
    }
}

/// Whether each update of the installation of `target` went: told by
/// `told`, the outcome shown, and `snaps_read`, the list of snaps read
/// again (`None`: not read). `Some(true)`: it went; `Some(false)`: it did
/// not; `None`: not known. The system's all together, as PackageKit
/// installs them all or none until dpkg runs; once dpkg ran and stopped, or
/// the daemon fell silent, what went is not known. Snaps one by one by the
/// list read again, as the outcome counts them, one still listed after
/// snapd fell silent not known (it may finish it yet); not read again, they
/// are known only done.
fn went(target: &Target, told: &Outcome, snaps_read: Option<&[SnapUpdate]>) -> Vec<Option<bool>> {
    let silent = told.why == Some(Why::Unanswered);
    let all = match told.end {
        ui::InstallEnd::Done => Some(true),
        ui::InstallEnd::Stopped => None,
        ui::InstallEnd::Cancelled | ui::InstallEnd::Failed if silent => None,
        ui::InstallEnd::Cancelled | ui::InstallEnd::Failed => Some(false),
    };
    match target {
        Target::System(asked) => vec![all; asked.len()],
        Target::Snaps(asked) => asked
            .iter()
            .map(|snap| match snaps_read {
                // Waiting still after snapd fell silent: it may finish them yet.
                Some(read) if waits(snap, read) && silent => None,
                Some(read) => Some(!waits(snap, read)),
                None => all.filter(|went| *went),
            })
            .collect(),
    }
}

/// The line of the installation of `target` that ended as `ended`, at
/// `time`, whose outcome shown is `told`; `snaps_read` is the list of snaps
/// read again (`None`: not read). Done when every update asked went. A
/// reason is said once for all on the line when none went, else by each
/// update that did not.
pub(super) fn entry(
    target: &Target,
    ended: &Ended,
    told: &Outcome,
    snaps_read: Option<&[SnapUpdate]>,
    time: i64,
) -> UpdateEntry {
    let asked = |name: &str, version: &str, previous: Option<&str>| UpdatedPackage {
        name: name.to_owned(),
        version: version.to_owned(),
        previous_version: previous.map(str::to_owned),
        done: false,
        reason: None,
    };
    let (source, asked): (PackageSource, Vec<UpdatedPackage>) = match target {
        Target::System(updates) => (
            PackageSource::Deb,
            updates
                .iter()
                .map(|update| asked(&update.name, &update.version, update.installed.as_deref()))
                .collect(),
        ),
        Target::Snaps(snaps) => (
            PackageSource::Snap,
            snaps
                .iter()
                .map(|snap| asked(&snap.name, &snap.version, snap.installed.as_deref()))
                .collect(),
        ),
    };
    let went = went(target, told, snaps_read);
    let done = !went.is_empty() && went.iter().all(|went| *went == Some(true));
    // Ended done by its daemon yet some wait still: they say so.
    let cause = Reason::of_end(ended.end).unwrap_or(Reason::Waiting);
    let line_reason = (!done && !went.contains(&Some(true))).then_some(cause);
    let packages = asked
        .into_iter()
        .zip(went)
        .map(|(package, went)| UpdatedPackage {
            done: went == Some(true),
            reason: match went {
                Some(true) => None,
                None => Some(Reason::Unknown),
                Some(false) => line_reason.is_none().then_some(cause),
            }
            .map(|reason| reason.key().to_owned()),
            ..package
        })
        .collect();
    UpdateEntry {
        time,
        source,
        packages,
        done,
        reason: line_reason.map(|reason| reason.key().to_owned()),
        restart_required: ended.system_restart,
    }
}

#[cfg(test)]
mod tests {
    use okapi_tools::updates::{Kind, SystemUpdate};

    use super::super::install::outcome;
    use super::*;

    const TIME: i64 = 1_794_693_600;

    fn system_update(name: &str) -> SystemUpdate {
        SystemUpdate {
            id: format!("{name};2.0;amd64;resolute-updates"),
            name: name.to_owned(),
            version: "2.0".to_owned(),
            installed: Some("1.0".to_owned()),
            summary: String::new(),
            kind: Kind::Other,
        }
    }

    fn snap(name: &str) -> SnapUpdate {
        SnapUpdate {
            name: name.to_owned(),
            title: format!("{name} title"),
            publisher: None,
            verified: false,
            version: "1.5".to_owned(),
            installed: None,
            download_size: None,
        }
    }

    fn system() -> Target {
        Target::System(vec![
            system_update("openssl"),
            system_update("bash"),
            system_update("zsh"),
        ])
    }

    fn snaps() -> Target {
        Target::Snaps(vec![snap("spotify"), snap("firefox"), snap("obs-studio")])
    }

    /// The line of `target` ended as `ended`, the system and the snaps
    /// read again as `system_read` and `snaps_read`, told as the outcome
    /// shown is; checked against that outcome: done only when it says done
    /// and every update went, and as many gone as it counts installed.
    fn line_of(
        target: &Target,
        ended: &Ended,
        system_read: Option<&[SystemUpdate]>,
        snaps_read: Option<&[SnapUpdate]>,
    ) -> UpdateEntry {
        let told = outcome(target, ended, system_read, snaps_read);
        let written = entry(target, ended, &told, snaps_read, TIME);
        assert!(
            !written.done || told.end == ui::InstallEnd::Done,
            "{ended:?}: {told:?}"
        );
        assert_eq!(
            written.done,
            written.packages.iter().all(|package| package.done)
        );
        if let Some(counts) = &told.counts {
            let gone = written
                .packages
                .iter()
                .filter(|package| package.done)
                .count();
            assert_eq!(gone, counts.installed, "{ended:?}: {told:?}");
        }
        written
    }

    /// What each package came to, by name: done, and its reason.
    fn packages(written: &UpdateEntry) -> Vec<(&str, bool, Option<&str>)> {
        written
            .packages
            .iter()
            .map(|package| {
                (
                    package.name.as_str(),
                    package.done,
                    package.reason.as_deref(),
                )
            })
            .collect()
    }

    fn line(written: &UpdateEntry) -> (bool, Option<&str>) {
        (written.done, written.reason.as_deref())
    }

    fn each(written: &UpdateEntry) -> Vec<(bool, Option<&str>)> {
        written
            .packages
            .iter()
            .map(|package| (package.done, package.reason.as_deref()))
            .collect()
    }

    #[test]
    fn the_system_done_says_every_update_went_with_its_version() {
        let ended = Ended {
            system_restart: true,
            reached_dpkg: true,
            ..Ended::before(End::Done)
        };
        // One waits anew, in a newer version come since: it went all the
        // same.
        let read = [SystemUpdate {
            id: "bash;2.1;amd64;resolute-updates".to_owned(),
            version: "2.1".to_owned(),
            ..system_update("bash")
        }];
        let written = line_of(&system(), &ended, Some(&read), None);
        assert_eq!(written.source, PackageSource::Deb);
        assert_eq!(written.time, TIME);
        assert!(written.restart_required);
        assert_eq!(line(&written), (true, None));
        assert_eq!(
            packages(&written),
            [
                ("openssl", true, None),
                ("bash", true, None),
                ("zsh", true, None)
            ]
        );
        assert_eq!(written.packages[0].version, "2.0");
        assert_eq!(written.packages[0].previous_version.as_deref(), Some("1.0"));
    }

    #[test]
    fn the_system_ended_before_dpkg_says_none_went_and_why_once() {
        for (end, reason) in [
            (End::Cancelled, "cancelled"),
            (End::Failed(Why::Busy), "busy"),
            (End::Failed(Why::Changed), "changed"),
        ] {
            let written = line_of(&system(), &Ended::before(end), None, None);
            assert_eq!(line(&written), (false, Some(reason)), "{end:?}");
            assert_eq!(each(&written), [(false, None); 3], "{end:?}");
        }
    }

    #[test]
    fn the_system_stopped_in_dpkg_or_silent_says_what_went_is_unknown() {
        for (ended, reason) in [
            (
                Ended {
                    reached_dpkg: true,
                    ..Ended::before(End::Failed(Why::Broken))
                },
                "broken",
            ),
            (
                Ended {
                    reached_dpkg: true,
                    ..Ended::before(End::Cancelled)
                },
                "cancelled",
            ),
            // Fallen silent before dpkg: it may be finishing without Okapi.
            (Ended::before(End::Failed(Why::Unanswered)), "unanswered"),
        ] {
            let written = line_of(&system(), &ended, None, None);
            assert_eq!(line(&written), (false, Some(reason)), "{ended:?}");
            assert_eq!(each(&written), [(false, Some("unknown")); 3], "{ended:?}");
        }
    }

    #[test]
    fn snaps_read_again_say_each_that_went_and_why_the_others_did_not() {
        // One of three still waits: the line says nothing, the snap why.
        let read = [snap("spotify")];
        let ended = Ended::before(End::Failed(Why::AppsOpen));
        let written = line_of(&snaps(), &ended, None, Some(&read));
        assert_eq!(written.source, PackageSource::Snap);
        assert_eq!(line(&written), (false, None));
        assert_eq!(
            packages(&written),
            [
                ("spotify", false, Some("appsOpen")),
                ("firefox", true, None),
                ("obs-studio", true, None),
            ]
        );
    }

    #[test]
    fn snaps_none_of_which_went_say_why_on_the_line() {
        let read = [snap("obs-studio"), snap("spotify"), snap("firefox")];
        let written = line_of(
            &snaps(),
            &Ended::before(End::Failed(Why::Refused)),
            None,
            Some(&read),
        );
        assert_eq!(line(&written), (false, Some("refused")));
        assert_eq!(each(&written), [(false, None); 3]);
    }

    #[test]
    fn snaps_done_by_snapd_yet_waiting_are_said_waiting_never_done() {
        // The outcome says done, as snapd did: the line, only when all went.
        let read = [snap("firefox")];
        let written = line_of(&snaps(), &Ended::before(End::Done), None, Some(&read));
        assert_eq!(line(&written), (false, None));
        assert_eq!(packages(&written)[1], ("firefox", false, Some("waiting")));
        let all_waiting = [snap("spotify"), snap("firefox"), snap("obs-studio")];
        let written = line_of(
            &snaps(),
            &Ended::before(End::Done),
            None,
            Some(&all_waiting),
        );
        assert_eq!(line(&written), (false, Some("waiting")));
    }

    #[test]
    fn snaps_failed_by_snapd_yet_all_gone_are_done() {
        // The list read again decides, as the outcome shown.
        let written = line_of(
            &snaps(),
            &Ended::before(End::Failed(Why::Unanswered)),
            None,
            Some(&[]),
        );
        assert_eq!(line(&written), (true, None));
        assert_eq!(each(&written), [(true, None); 3]);
    }

    #[test]
    fn snaps_cancelled_after_some_went_say_it_by_those_left() {
        let read = [snap("spotify")];
        let written = line_of(&snaps(), &Ended::before(End::Cancelled), None, Some(&read));
        assert_eq!(line(&written), (false, None));
        assert_eq!(packages(&written)[0], ("spotify", false, Some("cancelled")));
    }

    #[test]
    fn snaps_not_read_again_are_known_only_done() {
        let written = line_of(&snaps(), &Ended::before(End::Done), None, None);
        assert_eq!(line(&written), (true, None));
        assert_eq!(each(&written), [(true, None); 3]);
        // Failed, some may have gone: never said not installed.
        let written = line_of(
            &snaps(),
            &Ended::before(End::Failed(Why::Broken)),
            None,
            None,
        );
        assert_eq!(line(&written), (false, Some("broken")));
        assert_eq!(each(&written), [(false, Some("unknown")); 3]);
    }

    #[test]
    fn snaps_snapd_fell_silent_on_and_still_waiting_are_unknown() {
        // Read again while snapd may still finish them: never not installed.
        let read = [snap("firefox")];
        let written = line_of(
            &snaps(),
            &Ended::before(End::Failed(Why::Unanswered)),
            None,
            Some(&read),
        );
        assert_eq!(
            packages(&written),
            [
                ("spotify", true, None),
                ("firefox", false, Some("unknown")),
                ("obs-studio", true, None),
            ]
        );
    }

    #[test]
    fn nothing_asked_is_never_said_done() {
        let ended = Ended::before(End::Done);
        let told = outcome(&Target::System(Vec::new()), &ended, None, None);
        let written = entry(&Target::System(Vec::new()), &ended, &told, None, TIME);
        assert!(!written.done);
    }

    #[test]
    fn each_reason_comes_back_from_its_name_and_no_other() {
        for (place, reason) in REASONS.into_iter().enumerate() {
            assert_eq!(reason_place(reason), place, "{reason:?}");
            assert_eq!(Reason::of_key(reason.key()), Some(reason));
        }
        let keys: std::collections::BTreeSet<_> = REASONS.into_iter().map(Reason::key).collect();
        assert_eq!(keys.len(), REASONS.len(), "names unique");
        for other in ["Busy", "appRunning", ""] {
            assert_eq!(Reason::of_key(other), None, "{other}");
        }
    }
}
