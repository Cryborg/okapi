//! The copies of snapd read from its answers, and forgotten against a snapd
//! on a socket that tells what it was asked.

use std::collections::BTreeSet;

use super::*;
use crate::packages::snapd::fake::scripted;

/// A copy as snapd lists it, in the set `set`.
fn entry(set: u64, snap: &str, extra: &str) -> String {
    format!(
        r#"{{"set":{set},"time":"2026-10-17T18:39:44.184212789+02:00","snap":"{snap}","revision":"8763","snap-id":"x","epoch":{{"read":[0]}},"summary":"","version":"1","sha3-384":{{}}{extra}}}"#
    )
}

fn sets(sets: &[(u64, Vec<String>)]) -> String {
    let sets: Vec<String> = sets
        .iter()
        .map(|(id, entries)| format!(r#"{{"id":{id},"snapshots":[{}]}}"#, entries.join(",")))
        .collect();
    format!(
        r#"{{"type":"sync","status-code":200,"status":"OK","result":[{}]}}"#,
        sets.join(",")
    )
}

fn result(answer: &str) -> Value {
    serde_json::from_str::<Value>(answer).unwrap()["result"].clone()
}

fn installed(names: &[&str]) -> Known {
    Known::Read(
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<BTreeSet<_>>(),
    )
}

fn found(copies: Copies) -> CopyReport {
    match copies {
        Copies::Found(report) => report,
        other => panic!("{other:?}"),
    }
}

/// firefox's copy, made by snapd, of 669 MB, alone in set 1.
fn firefox() -> String {
    sets(&[(
        1,
        vec![entry(1, "firefox", r#","size":668852084,"auto":true"#)],
    )])
}

#[test]
fn only_the_copies_snapd_made_of_snaps_not_installed_are_proposed() {
    // Catches a copy made by hand proposed, one of a snap installed, a broken
    // one, a whole set of two taken for one, and the list failing whole on
    // a field snapd leaves out.
    let listed = sets(&[
        (
            1,
            vec![entry(1, "firefox", r#","size":668852084,"auto":true"#)],
        ),
        (2, vec![entry(2, "vlc", r#","size":10"#)]),
        (3, vec![entry(3, "tiny", r#","auto":true"#)]),
        (
            4,
            vec![
                entry(4, "code", r#","size":5,"auto":true"#),
                entry(4, "gimp", r#","size":7,"auto":true"#),
            ],
        ),
        (
            5,
            vec![entry(
                5,
                "old",
                r#","size":1,"auto":true,"broken":"no archive""#,
            )],
        ),
        (6, vec![r#"{"set":6,"snap":"x"}"#.to_owned()]),
        (
            7,
            vec![entry(7, "undated", r#","size":1,"auto":true"#).replace(
                "2026-10-17T18:39:44.184212789+02:00",
                "0001-01-01T00:00:00Z",
            )],
        ),
        (8, vec![entry(8, "../x", r#","size":1,"auto":true"#)]),
    ]);
    let report = found(find(Ok(result(&listed)), &installed(&["code"])));
    let copies: Vec<(&str, u64)> = report
        .copies()
        .iter()
        .map(|copy| (copy.snap(), copy.bytes()))
        .collect();
    assert_eq!(copies, [("firefox", 668_852_084), ("tiny", 0), ("gimp", 7)]);
    assert_eq!(
        report.copies()[0].made(),
        "2026-10-17T16:39:44.184212789Z"
            .parse::<Timestamp>()
            .unwrap()
    );
}

#[test]
fn copies_never_read_whole_are_unread_and_no_snapd_is_no_copy() {
    // Catches snapd's silence, or the snaps installed unread, taken for no
    // copy: the card would vanish without a word.
    assert!(matches!(
        find(
            Err(SnapdError::Absent {
                socket: "/run/snapd.socket".into(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            }),
            &installed(&[])
        ),
        Copies::None
    ));
    assert!(matches!(
        find(Err(SnapdError::Malformed("broken")), &installed(&[])),
        Copies::Unread(_)
    ));
    assert!(matches!(
        find(Ok(result(&firefox())), &Known::Unread("silent".to_owned())),
        Copies::Unread(_)
    ));
    assert!(matches!(
        find(Ok(serde_json::json!({"not": "a list"})), &installed(&[])),
        Copies::Unread(_)
    ));
}

/// snapd's answer, word for word, to reading a snap absent.
const NOT_INSTALLED: &str = r#"{"type":"error","status-code":404,"status":"Not Found","result":{"message":"snap not installed","kind":"snap-not-found","value":"firefox"}}"#;
const INSTALLED: &str =
    r#"{"type":"sync","status-code":200,"status":"OK","result":{"name":"firefox"}}"#;
const ACCEPTED: &str =
    r#"{"type":"async","status-code":202,"status":"Accepted","result":null,"change":"7"}"#;
const DONE: &str =
    r#"{"type":"sync","status-code":200,"status":"OK","result":{"status":"Done","ready":true}}"#;
const NONE: &str = r#"{"type":"sync","status-code":200,"status":"OK","result":[]}"#;

/// The report of firefox's copy, and the outcome of forgetting it against
/// a snapd answering `answers`; what snapd was asked.
fn forgotten(answers: &[Option<&str>], wait: Duration) -> (Forgot, Vec<String>) {
    let report = found(find(Ok(result(&firefox())), &installed(&[])));
    let (_directory, snapd, requests) = scripted(
        answers
            .iter()
            .map(|answer| answer.map(str::to_owned))
            .collect(),
    );
    let forgot = forget(&report, report.copies()[0].id(), &snapd, wait);
    (forgot, requests.try_iter().collect())
}

const LONG: Duration = Duration::from_mins(1);

#[test]
fn a_copy_still_there_of_a_snap_still_absent_is_forgotten_alone() {
    // Catches a whole set forgotten (no snap named), another set or snap
    // asked, and the rights never asked for.
    let (forgot, requests) = forgotten(
        &[
            Some(&firefox()),
            Some(NOT_INSTALLED),
            Some(ACCEPTED),
            Some(DONE),
        ],
        LONG,
    );
    assert_eq!(forgot, Forgot::Forgotten);
    let asked = &requests[2];
    assert!(asked.starts_with("POST /v2/snapshots "), "{asked}");
    assert!(asked.contains("X-Allow-Interaction: true"), "{asked}");
    assert!(
        asked.ends_with(r#"{"action":"forget","set":1,"snaps":["firefox"]}"#),
        "{asked}"
    );
}

#[test]
fn each_check_before_asking_alone_asks_nothing() {
    // Catches each check of the copy before snapd is asked: gone, made
    // again (another date, another size), its snap installed again, snapd
    // silent.
    let changed = |extra: &str, time: &str| {
        sets(&[(
            1,
            vec![entry(1, "firefox", extra).replace("2026-10-17T18:39:44.184212789+02:00", time)],
        )])
    };
    let other_size = changed(
        r#","size":1,"auto":true"#,
        "2026-10-17T18:39:44.184212789+02:00",
    );
    let other_date = changed(
        r#","size":668852084,"auto":true"#,
        "2026-10-18T09:00:00+02:00",
    );
    for listed in [NONE.to_owned(), other_size, other_date] {
        // One answer more than the checks need: a request past them is seen.
        let (forgot, requests) = forgotten(&[Some(&listed), Some(ACCEPTED)], LONG);
        assert_eq!(forgot, Forgot::Gone, "{listed}");
        assert_eq!(requests.len(), 1, "nothing asked");
    }
    let (forgot, requests) = forgotten(&[Some(&firefox()), Some(INSTALLED), Some(ACCEPTED)], LONG);
    assert_eq!(forgot, Forgot::Reinstalled);
    assert_eq!(requests.len(), 2);
    // snapd silent before anything is asked: nothing unknown, nothing done.
    let (forgot, _) = forgotten(&[None], LONG);
    assert_eq!(forgot, Forgot::Failed(Why::Unclear));
}

#[test]
fn a_copy_of_another_report_is_never_asked_for() {
    let other = found(find(Ok(result(&firefox())), &installed(&[])));
    let report = found(find(Ok(result(&firefox())), &installed(&[])));
    let (_directory, snapd, requests) = scripted(vec![Some(firefox())]);
    assert_eq!(
        forget(&report, other.copies()[0].id(), &snapd, LONG),
        Forgot::Failed(Why::Unclear)
    );
    assert_eq!(requests.try_iter().count(), 0);
}

#[test]
fn snapd_s_refusal_or_failure_is_told_and_a_copy_erased_meanwhile_is_gone() {
    // Catches a refusal told in snapd's English, and the copy snapd erased
    // by itself while polkit asked taken for a failure.
    let refused = r#"{"type":"error","status-code":403,"status":"Forbidden","result":{"kind":"auth-cancelled","message":"cancelled"}}"#;
    let (forgot, _) = forgotten(
        &[
            Some(&firefox()),
            Some(NOT_INSTALLED),
            Some(refused),
            Some(&firefox()),
        ],
        LONG,
    );
    assert_eq!(forgot, Forgot::Failed(Why::NotAuthorized));
    let missing = r#"{"type":"error","status-code":404,"status":"Not Found","result":{"message":"cannot find snapshot set #1"}}"#;
    let (forgot, _) = forgotten(
        &[
            Some(&firefox()),
            Some(NOT_INSTALLED),
            Some(missing),
            Some(NONE),
        ],
        LONG,
    );
    assert_eq!(forgot, Forgot::Gone);
    let failed = r#"{"type":"sync","status-code":200,"status":"OK","result":{"status":"Error","ready":true,"err":"cannot forget"}}"#;
    let (forgot, _) = forgotten(
        &[
            Some(&firefox()),
            Some(NOT_INSTALLED),
            Some(ACCEPTED),
            Some(failed),
        ],
        LONG,
    );
    assert_eq!(
        forgot,
        Forgot::Failed(Why::Service("cannot forget".to_owned()))
    );
}

#[test]
fn a_forgetting_followed_no_longer_is_told_by_the_copy_listed_or_not() {
    for (listed, expected) in [
        (NONE.to_owned(), Forgot::Forgotten),
        (firefox(), Forgot::Failed(Why::TimedOut)),
    ] {
        let (forgot, _) = forgotten(
            &[
                Some(&firefox()),
                Some(NOT_INSTALLED),
                Some(ACCEPTED),
                Some(&listed),
            ],
            Duration::ZERO,
        );
        assert_eq!(forgot, expected);
    }
}

#[test]
fn a_name_that_is_no_snap_s_is_never_sent() {
    let (_directory, snapd, requests) = scripted(vec![Some(ACCEPTED.to_owned())]);
    assert!(matches!(
        snapd.forget(1, "../x"),
        Err(SnapdError::InvalidName(_))
    ));
    assert!(matches!(
        snapd.forget(1, ""),
        Err(SnapdError::InvalidName(_))
    ));
    assert_eq!(requests.try_iter().count(), 0);
}
