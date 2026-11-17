//! The history line of an installation of updates
//! (`okapi_store::UpdateEntry`): every update asked, and what each came to,
//! told from the outcome the screen shows (one source for both); and the
//! stable names of its reasons, which the History screen reads back.

use okapi_store::{PackageSource, UpdateEntry, UpdatedPackage};
use okapi_tools::updates::SnapUpdate;
use okapi_tools::updates::install::{End, Ended, Why};

use super::install::{Outcome, Target, waits};
use crate::ui;

/// It was cancelled before anything was installed, or while dpkg ran.
pub(crate) const CANCELLED: &str = "cancelled";
/// The update waits still once the list was read again, no cause said.
pub(crate) const WAITING: &str = "waiting";
/// Whether the update went is not known: dpkg ran then stopped, or the
/// daemon fell silent, or the list could not be read again.
pub(crate) const UNKNOWN: &str = "unknown";

/// Every cause, for its name to be read back, each at its place
/// (`why_place` makes the compiler name a cause left out).
const WHYS: [Why; 10] = [
    Why::Busy,
    Why::Interrupted,
    Why::Offline,
    Why::NoSpace,
    Why::Refused,
    Why::Changed,
    Why::AppsOpen,
    Why::Unsigned,
    Why::Unanswered,
    Why::Broken,
];

/// The name the history keeps `why` by: never changed once written.
pub(crate) const fn why_key(why: Why) -> &'static str {
    match why {
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
    }
}

/// Where `why` is in [`WHYS`]; a cause added fails to compile here until it
/// is placed there too.
#[cfg(test)]
const fn why_place(why: Why) -> usize {
    match why {
        Why::Busy => 0,
        Why::Interrupted => 1,
        Why::Offline => 2,
        Why::NoSpace => 3,
        Why::Refused => 4,
        Why::Changed => 5,
        Why::AppsOpen => 6,
        Why::Unsigned => 7,
        Why::Unanswered => 8,
        Why::Broken => 9,
    }
}

/// The cause named `key`, if it names one.
pub(crate) fn why_of_key(key: &str) -> Option<Why> {
    WHYS.into_iter().find(|why| why_key(*why) == key)
}

/// The line of the installation of `target` that ended as `ended`, at
/// `time`, whose outcome shown is `told`; `snaps_read` is the list of snaps
/// read again (`None`: not read). Done when the outcome says so. Each
/// update went, did not, or is not known: the system's all together, as
/// PackageKit installs them all or none until dpkg runs; once dpkg ran and
/// stopped, or the daemon fell silent, what went is not known. Snaps are
/// told one by one by the list read again, as the outcome counts them; not
/// read again, they are known only done.
pub(super) fn entry(
    target: &Target,
    ended: &Ended,
    told: &Outcome,
    snaps_read: Option<&[SnapUpdate]>,
    time: i64,
) -> UpdateEntry {
    let done = told.end == ui::InstallEnd::Done;
    let silent = told.why == Some(Why::Unanswered);
    // Each one went (`Some(true)`), did not, or is not known (`None`).
    let all = match told.end {
        ui::InstallEnd::Done => Some(true),
        ui::InstallEnd::Stopped => None,
        ui::InstallEnd::Cancelled | ui::InstallEnd::Failed if silent => None,
        ui::InstallEnd::Cancelled | ui::InstallEnd::Failed => Some(false),
    };
    let (source, asked) = match target {
        Target::System(asked) => (
            PackageSource::Deb,
            asked
                .iter()
                .map(|update| (update.name.as_str(), update.version.as_str(), all))
                .collect::<Vec<_>>(),
        ),
        Target::Snaps(asked) => (
            PackageSource::Snap,
            asked
                .iter()
                .map(|snap| {
                    let went = match (snaps_read, told.counts.is_some()) {
                        (Some(read), true) => Some(!waits(snap, read)),
                        _ => all.filter(|went| *went),
                    };
                    (snap.name.as_str(), snap.version.as_str(), went)
                })
                .collect(),
        ),
    };
    // Why it was not done; one said done by its outcome needs none.
    let cause = match ended.end {
        _ if done => None,
        End::Done => Some(WAITING),
        End::Cancelled => Some(CANCELLED),
        End::Failed(why) => Some(why_key(why)),
    };
    // None known gone: the line says why for all.
    let line_reason = cause.filter(|_| !asked.iter().any(|(.., went)| *went == Some(true)));
    let packages = asked
        .into_iter()
        .map(|(name, version, went)| UpdatedPackage {
            name: name.to_owned(),
            version: version.to_owned(),
            done: went == Some(true),
            reason: match went {
                Some(true) => None,
                None => Some(UNKNOWN),
                Some(false) if line_reason.is_some() => None,
                Some(false) => Some(cause.unwrap_or(WAITING)),
            }
            .map(str::to_owned),
        })
        .collect();
    UpdateEntry {
        time,
        source,
        packages,
        done,
        reason: line_reason.map(str::to_owned),
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
    /// shown is; checked against that outcome: done when it says done, and
    /// as many gone as it counts installed.
    fn line_of(
        target: &Target,
        ended: &Ended,
        system_read: Option<&[SystemUpdate]>,
        snaps_read: Option<&[SnapUpdate]>,
    ) -> UpdateEntry {
        let told = outcome(target, ended, system_read, snaps_read);
        let written = entry(target, ended, &told, snaps_read, TIME);
        assert_eq!(
            written.done,
            told.end == ui::InstallEnd::Done,
            "{ended:?}: {told:?}"
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
        let read = [system_update("bash")];
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
    }

    #[test]
    fn the_system_ended_before_dpkg_says_none_went_and_why_once() {
        for (end, reason) in [
            (End::Cancelled, CANCELLED),
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
                CANCELLED,
            ),
            // Fallen silent before dpkg: it may be finishing without Okapi.
            (Ended::before(End::Failed(Why::Unanswered)), "unanswered"),
        ] {
            let written = line_of(&system(), &ended, None, None);
            assert_eq!(line(&written), (false, Some(reason)), "{ended:?}");
            assert_eq!(each(&written), [(false, Some(UNKNOWN)); 3], "{ended:?}");
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
    fn snaps_done_by_snapd_yet_waiting_are_done_and_said_waiting() {
        // The outcome says done, the one waiting said so.
        let read = [snap("firefox")];
        let written = line_of(&snaps(), &Ended::before(End::Done), None, Some(&read));
        assert_eq!(line(&written), (true, None));
        assert_eq!(packages(&written)[1], ("firefox", false, Some(WAITING)));
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
        assert_eq!(each(&written), [(false, Some(UNKNOWN)); 3]);
    }

    #[test]
    fn each_cause_comes_back_from_its_name_and_no_other() {
        for (place, why) in WHYS.into_iter().enumerate() {
            assert_eq!(why_place(why), place, "{why:?}");
            assert_eq!(why_of_key(why_key(why)), Some(why));
        }
        let keys: std::collections::BTreeSet<_> = WHYS.into_iter().map(why_key).collect();
        assert_eq!(keys.len(), WHYS.len(), "names unique");
        for other in [CANCELLED, WAITING, UNKNOWN, "Busy", ""] {
            assert_eq!(why_of_key(other), None, "{other}");
        }
    }
}
