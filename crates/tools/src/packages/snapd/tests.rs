use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::thread;

use super::*;

const REFRESHES: &str = r#"{"type":"sync","status-code":200,"status":"OK","result":[
{"name":"snap-store","title":"App Center","version":"0+git.476c121a","download-size":12283904,
 "publisher":{"id":"canonical","username":"canonical","display-name":"Canonical","validation":"verified"},"channel":"2/stable"},
{"name":"tiny","version":"2.1"}]}"#;

/// A snapd that answers one request with `answer`, sent in `pieces`; the
/// request it read comes back on the receiver.
fn serving(answer: Vec<u8>) -> (tempfile::TempDir, Snapd, mpsc::Receiver<String>) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("snapd.socket");
    let listener = UnixListener::bind(&socket).unwrap();
    let (sender, requests) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = vec![0; 4096];
        let read = stream.read(&mut request).unwrap();
        // Not every test reads it back; the peer may be gone already (a
        // deadline): nothing to tell either way.
        let _ = sender.send(String::from_utf8_lossy(&request[..read]).into_owned());
        let _ = stream.write_all(&answer);
    });
    let snapd = Snapd::at(&socket, Duration::from_secs(5));
    (directory, snapd, requests)
}

fn with_length(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn in_chunks(status: &str, body: &str) -> Vec<u8> {
    let (first, second) = body.split_at(body.len() / 3);
    format!(
        "HTTP/1.1 {status}\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{first}\r\n{:X};ext=1\r\n{second}\r\n0\r\n\r\n",
        first.len(),
        second.len()
    )
    .into_bytes()
}

#[test]
fn an_answer_by_length_or_in_chunks_says_the_same() {
    // Catches a chunked answer read as one, or its sizes misread.
    for answer in [
        with_length("200 OK", REFRESHES),
        in_chunks("200 OK", REFRESHES),
    ] {
        let (_directory, snapd, _) = serving(answer);
        let refreshes = snapd.refreshes().unwrap();
        assert_eq!(
            refreshes,
            [
                Refresh {
                    name: "snap-store".into(),
                    title: Some("App Center".into()),
                    version: "0+git.476c121a".into(),
                    publisher: Some(Publisher {
                        display_name: "Canonical".into(),
                        validation: Some("verified".into()),
                    }),
                    download_size: Some(12_283_904),
                },
                // What is optional may be missing.
                Refresh {
                    name: "tiny".into(),
                    title: None,
                    version: "2.1".into(),
                    publisher: None,
                    download_size: None,
                },
            ]
        );
    }
}

#[test]
fn a_request_asks_its_path_and_closes_the_connection() {
    // Catches a request that keeps the connection open: its answer would
    // never end.
    let (_directory, snapd, requests) = serving(with_length("200 OK", REFRESHES));
    snapd.refreshes().unwrap();
    let request = requests.recv().unwrap();
    assert!(request.starts_with("GET /v2/find?select=refresh HTTP/1.1\r\n"));
    for header in ["Host: localhost\r\n", "Connection: close\r\n"] {
        assert!(request.contains(header), "{header}: {request}");
    }
    assert!(request.ends_with("\r\n\r\n"), "{request}");

    // Nothing waiting: an empty list, not an error.
    let (_directory, snapd, _) = serving(with_length(
        "200 OK",
        r#"{"type":"sync","status-code":200,"result":[]}"#,
    ));
    assert_eq!(snapd.refreshes().unwrap(), []);
}

#[test]
fn an_error_of_snapd_is_a_refusal_with_its_words() {
    // Catches an error envelope taken for an answer, or its status lost.
    for (status, code) in [
        ("401 Unauthorized", 401),
        ("500 Internal Server Error", 500),
    ] {
        let body = format!(
            r#"{{"type":"error","status-code":{code},"result":{{"kind":"login-required","message":"access denied"}}}}"#
        );
        let (_directory, snapd, _) = serving(with_length(status, &body));
        match snapd.refreshes() {
            Err(SnapdError::Refused {
                status,
                kind,
                message,
            }) => {
                assert_eq!(status, code);
                assert_eq!(
                    (kind.as_str(), message.as_str()),
                    ("login-required", "access denied")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    // A store out of reach is told apart; an error without its words
    // still is one.
    for (kind, offline) in [
        (KIND_NETWORK_TIMEOUT, true),
        (KIND_DNS_FAILURE, true),
        ("login-required", false),
    ] {
        let body = format!(
            r#"{{"type":"error","status-code":500,"result":{{"kind":"{kind}","message":"x"}}}}"#
        );
        let (_directory, snapd, _) = serving(with_length("500 Internal Server Error", &body));
        assert_eq!(snapd.refreshes().unwrap_err().offline(), offline, "{kind}");
    }
    let (_directory, snapd, _) = serving(with_length(
        "503 Service Unavailable",
        r#"{"type":"error","status-code":503,"result":"busy"}"#,
    ));
    assert!(matches!(
        snapd.refreshes(),
        Err(SnapdError::Refused { status: 503, ref kind, .. }) if kind.is_empty()
    ));
    // The envelope and the head each say a failure on their own: an error
    // envelope under a 200, a status of failure in the envelope under a
    // 200, a failure of HTTP under any envelope (whose status is the head's).
    for (head, envelope, status) in [
        (
            "200 OK",
            r#"{"type":"error","status-code":200,"result":{"kind":"x","message":"y"}}"#,
            200,
        ),
        (
            "200 OK",
            r#"{"type":"sync","status-code":404,"result":[]}"#,
            404,
        ),
        (
            "502 Bad Gateway",
            r#"{"type":"sync","status-code":200,"result":[]}"#,
            502,
        ),
    ] {
        let (_directory, snapd, _) = serving(with_length(head, envelope));
        match snapd.refreshes() {
            Err(SnapdError::Refused { status: said, .. }) => assert_eq!(said, status, "{head}"),
            other => panic!("{head}: {other:?}"),
        }
    }
    // An error whose words are of another shape keeps them.
    let (_directory, snapd, _) = serving(with_length(
        "500 Internal Server Error",
        r#"{"type":"error","status-code":500,"result":"store unreachable"}"#,
    ));
    assert!(matches!(
        snapd.refreshes(),
        Err(SnapdError::Refused { ref message, .. }) if message.contains("store unreachable")
    ));
}

#[test]
fn a_cut_a_huge_or_a_garbled_answer_is_told_apart() {
    // Cut before the length it announced.
    let mut cut = with_length("200 OK", REFRESHES);
    cut.truncate(cut.len() - 10);
    let (_directory, snapd, _) = serving(cut);
    assert!(matches!(
        snapd.refreshes(),
        Err(SnapdError::Truncated { .. })
    ));

    // Chunks that stop within one, or after a whole one but before the last
    // (empty) chunk: the body may read as JSON, it is not whole.
    for cut_off in [7, 5] {
        let mut cut = in_chunks("200 OK", REFRESHES);
        cut.truncate(cut.len() - cut_off);
        let (_directory, snapd, _) = serving(cut);
        assert!(
            matches!(snapd.refreshes(), Err(SnapdError::Truncated { .. })),
            "{cut_off}"
        );
    }

    // Beyond the cap.
    let huge = "x".repeat(LARGEST_ANSWER + 1);
    let (_directory, snapd, _) = serving(with_length("200 OK", &huge));
    assert!(matches!(snapd.refreshes(), Err(SnapdError::TooLarge)));

    // Not JSON.
    let (_directory, snapd, _) = serving(with_length("200 OK", "<html>"));
    assert!(matches!(snapd.refreshes(), Err(SnapdError::Json(_))));

    // Not HTTP.
    let (_directory, snapd, _) = serving(b"SSH-2.0-OpenSSH\r\n\r\n".to_vec());
    assert!(matches!(snapd.refreshes(), Err(SnapdError::Malformed(_))));
}

#[test]
fn no_socket_is_no_snapd_and_a_silent_one_times_out() {
    let directory = tempfile::tempdir().unwrap();
    let missing = Snapd::at(&directory.path().join("none"), Duration::from_secs(1));
    assert!(matches!(
        missing.refreshes(),
        Err(SnapdError::Absent { .. })
    ));

    let socket = directory.path().join("silent");
    let listener = UnixListener::bind(&socket).unwrap();
    let held = thread::spawn(move || listener.accept().map(|(stream, _)| stream));
    let silent = Snapd::at(&socket, Duration::from_millis(200));
    let error = silent.refreshes().unwrap_err();
    assert!(error.timed_out(), "{error:?}");
    drop(held.join());
}

#[test]
fn the_schedule_says_the_next_automatic_refresh() {
    let body = r#"{"type":"sync","status-code":200,"result":{"refresh":{"timer":"00:00~24:00/4","last":"2026-11-14T19:04:00+01:00","next":"2026-11-15T01:37:00+01:00"},"on-classic":true}}"#;
    let (_directory, snapd, _) = serving(with_length("200 OK", body));
    assert_eq!(
        snapd.schedule().unwrap().next.as_deref(),
        Some("2026-11-15T01:37:00+01:00")
    );
    // A snapd that does not say it.
    let (_directory, snapd, _) = serving(with_length(
        "200 OK",
        r#"{"type":"sync","status-code":200,"result":{}}"#,
    ));
    assert_eq!(snapd.schedule().unwrap(), Schedule::default());
}

#[test]
fn a_removal_asks_polkit_and_names_the_change_it_started() {
    // Catches a removal that cannot ask for the rights, or that loses its
    // change.
    for purge in [false, true] {
        let (_directory, snapd, requests) = serving(with_length(
            "202 Accepted",
            r#"{"type":"async","status-code":202,"status":"Accepted","result":null,"change":"42"}"#,
        ));
        assert_eq!(snapd.remove("android-studio", purge).unwrap(), "42");
        let request = requests.recv().unwrap();
        assert!(
            request.starts_with("POST /v2/snaps/android-studio HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains("\r\nX-Allow-Interaction: true\r\n"),
            "{request}"
        );
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            serde_json::json!({ "action": "remove", "purge": purge })
        );
    }
    // A read never asks.
    let (_directory, snapd, requests) = serving(with_length(
        "200 OK",
        r#"{"type":"sync","status-code":200,"status":"OK","result":[]}"#,
    ));
    snapd.snaps().unwrap();
    let request = requests.recv().unwrap();
    assert!(request.starts_with("GET /v2/snaps?select=all HTTP/1.1\r\n"));
    assert!(!request.contains("X-Allow-Interaction"));
}

#[test]
fn the_rights_refused_are_told_by_their_kind_and_a_change_by_its_status() {
    let (_directory, snapd, _) = serving(with_length(
        "403 Forbidden",
        r#"{"type":"error","status-code":403,"status":"Forbidden","result":{"kind":"auth-cancelled","message":"cancelled"}}"#,
    ));
    assert!(matches!(
        snapd.remove("x", false),
        Err(SnapdError::Refused { kind, .. }) if kind == KIND_AUTH_CANCELLED
    ));
    let (_directory, snapd, _) = serving(with_length(
        "200 OK",
        r#"{"type":"sync","status-code":200,"status":"OK","result":{"id":"42","status":"Error","ready":true,"err":"cannot remove"}}"#,
    ));
    assert_eq!(
        snapd.change("42").unwrap(),
        Change {
            status: "Error".to_owned(),
            ready: true,
            err: Some("cannot remove".to_owned()),
            tasks: Vec::new(),
        }
    );
}

#[test]
fn a_snap_name_is_safe_in_a_path_and_a_request() {
    for name in [
        "firefox",
        "gnome-46-2404",
        "core24",
        "android-studio",
        "lxd_test2",
    ] {
        assert!(is_snap_name(name), "{name}");
    }
    for name in [
        "", "-x", "Firefox", "a/b", "..", "a b", "a_", "a_B", "a%2Fb", "a_b_c",
    ] {
        assert!(!is_snap_name(name), "{name}");
    }
}

#[test]
fn a_name_that_is_no_snap_s_is_never_asked() {
    // Catches a name sent as it came: a path or a query in the request.
    // No socket: anything asked would fail as `Absent`.
    let directory = tempfile::tempdir().unwrap();
    let snapd = Snapd::at(&directory.path().join("none"), Duration::from_secs(1));
    assert!(matches!(
        snapd.remove("../v2/snaps", false),
        Err(SnapdError::InvalidName(name)) if name == "../v2/snaps"
    ));
}

#[test]
fn a_refresh_asks_polkit_for_the_snaps_named_and_never_for_none() {
    // Catches a refresh that cannot ask for the rights, or names the snaps
    // wrong; and an empty list, which snapd takes for every snap.
    let (_directory, snapd, requests) = serving(with_length(
        "202 Accepted",
        r#"{"type":"async","status-code":202,"status":"Accepted","result":null,"change":"9"}"#,
    ));
    let names = ["snap-store".to_owned(), "obs-studio".to_owned()];
    assert_eq!(snapd.refresh(&names).unwrap(), "9");
    let request = requests.recv().unwrap();
    assert!(
        request.starts_with("POST /v2/snaps HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(request.contains("\r\nX-Allow-Interaction: true\r\n"));
    let body = request.split("\r\n\r\n").nth(1).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(body).unwrap(),
        serde_json::json!({ "action": "refresh", "snaps": names })
    );
    let directory = tempfile::tempdir().unwrap();
    let snapd = Snapd::at(&directory.path().join("none"), Duration::from_secs(1));
    assert!(matches!(
        snapd.refresh(&[]),
        Err(SnapdError::InvalidName(_))
    ));
    assert!(matches!(
        snapd.refresh(&["ok".to_owned(), "../x".to_owned()]),
        Err(SnapdError::InvalidName(name)) if name == "../x"
    ));
}

#[test]
fn a_change_s_tasks_say_their_progress() {
    let (_directory, snapd, _) = serving(with_length(
        "200 OK",
        r#"{"type":"sync","status-code":200,"result":{"status":"Doing","ready":false,"tasks":[
            {"kind":"download-snap","status":"Doing","progress":{"label":"x","done":400,"total":1000}},
            {"kind":"link-snap","status":"Do","progress":{"done":0,"total":1}},
            {"kind":"run-hook","status":"Do"}]}}"#,
    ));
    let change = snapd.change("9").unwrap();
    assert_eq!(change.tasks.len(), 3);
    assert_eq!(change.tasks[0].kind, TASK_DOWNLOAD);
    assert_eq!(change.tasks[0].status, "Doing");
    assert_eq!(
        change.tasks[0].progress,
        TaskProgress {
            done: 400,
            total: 1000
        }
    );
    assert_eq!(change.tasks[2].progress, TaskProgress::default());
}

/// A snapd that answers each request, one per connection, with the next of
/// `answers` (`None`: closed unanswered).
fn answering(answers: Vec<Option<&'static str>>) -> (tempfile::TempDir, Snapd) {
    let (directory, snapd, _) = super::fake::scripted(
        answers
            .into_iter()
            .map(|answer| answer.map(str::to_owned))
            .collect(),
    );
    (directory, snapd)
}

const DOING: &str =
    r#"{"type":"sync","status-code":200,"result":{"status":"Doing","ready":false}}"#;
const DONE: &str = r#"{"type":"sync","status-code":200,"result":{"status":"Done","ready":true}}"#;

#[test]
fn a_change_is_followed_through_short_silences_and_heard_at_each_look() {
    // Catches failed looks counted in total rather than in a row, the
    // tolerance asked ignored, and its progress never heard.
    let (_directory, snapd) = answering(vec![
        Some(DOING),
        None,
        None,
        Some(DOING),
        None,
        None,
        Some(DONE),
    ]);
    let heard = std::cell::Cell::new(0);
    let followed = snapd.follow("9", Duration::from_mins(1), 3, &|_| {
        heard.set(heard.get() + 1);
    });
    assert!(matches!(followed, Followed::Ready(change) if change.status == CHANGE_DONE));
    assert_eq!(heard.get(), 2);

    let (_directory, snapd) = answering(vec![None, None, None, Some(DONE)]);
    assert_eq!(
        snapd.follow("9", Duration::from_mins(1), 3, &|_| {}),
        Followed::Unanswered
    );
    let (_directory, snapd) = answering(vec![Some(DONE)]);
    assert_eq!(
        snapd.follow("9", Duration::ZERO, 3, &|_| {}),
        Followed::TimedOut,
        "no look once the wait has passed"
    );
}
