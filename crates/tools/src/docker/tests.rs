//! The client on a daemon a test plays on a socket.

use std::collections::BTreeSet;
use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;

use super::*;

/// A daemon that answers one request with `status` and `body`; the request
/// it read (its head) comes back on the receiver.
fn serving(status: &str, body: &str) -> (tempfile::TempDir, Docker, mpsc::Receiver<String>) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (sender, requests) = mpsc::channel();
    let answer = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        let _ = sender.send(String::from_utf8_lossy(&request).into_owned());
        let _ = stream.write_all(answer.as_bytes());
    });
    (directory, Docker::at(&socket), requests)
}

const WEEK_FILTER: &str = "%7B%22until%22%3A%7B%22168h%22%3Atrue%7D%7D";

#[test]
fn a_prune_asks_for_the_entries_unused_for_a_week_above_ten_gigabytes() {
    let (_directory, docker, requests) = serving(
        "200 OK",
        r#"{"CachesDeleted":["a","b","c"],"SpaceReclaimed":3400000000}"#,
    );
    let pruned = docker
        .prune_build_cache(Duration::from_hours(1), &|| false)
        .unwrap();
    assert_eq!(
        pruned,
        Pruned {
            caches: 3,
            bytes: 3_400_000_000
        }
    );
    let request = requests.recv().unwrap();
    assert_eq!(
        request.lines().next().unwrap(),
        format!(
            "POST /build/prune?filters={WEEK_FILTER}\
             &keep-storage=10000000000&reserved-space=10000000000 HTTP/1.1"
        )
    );
    assert!(request.contains("Content-Length: 0\r\n"), "{request}");
}

/// The settings' minimum age, when longer than the week, in whole hours.
#[test]
fn a_prune_never_takes_younger_entries_than_the_settings_allow() {
    let (_directory, docker, requests) =
        serving("200 OK", r#"{"CachesDeleted":null,"SpaceReclaimed":0}"#);
    let ten_days_and_a_minute = Duration::from_hours(240) + Duration::from_mins(1);
    assert_eq!(
        docker
            .prune_build_cache(ten_days_and_a_minute, &|| false)
            .unwrap(),
        Pruned {
            caches: 0,
            bytes: 0
        },
        "nothing pruned is said as such"
    );
    let request = requests.recv().unwrap();
    assert!(
        request.contains("%7B%22until%22%3A%7B%22241h%22%3Atrue%7D%7D"),
        "{request}"
    );
}

/// A refusal of the request says the daemon's words, nothing pruned; one
/// of the server's own may come once `BuildKit` removed records; an answer
/// that cannot be read leaves what went unknown.
#[test]
fn a_refusal_says_the_daemons_words_and_only_a_requests_leaves_nothing_pruned() {
    let (_directory, docker, _) =
        serving("400 Bad Request", r#"{"message":"invalid filter 'until'"}"#);
    let error = docker
        .prune_build_cache(Duration::ZERO, &|| false)
        .unwrap_err();
    assert!(
        matches!(&error, DockerError::Refused { status: 400, message } if message == "invalid filter 'until'"),
        "{error:?}"
    );
    assert!(!error.leaves_unknown(), "refused before anything ran");

    let (_directory, docker, _) = serving(
        "500 Internal Server Error",
        r#"{"message":"failed to prune build cache"}"#,
    );
    let error = docker
        .prune_build_cache(Duration::ZERO, &|| false)
        .unwrap_err();
    assert!(
        matches!(&error, DockerError::Refused { status: 500, .. }),
        "{error:?}"
    );
    assert!(error.leaves_unknown(), "it may have pruned before failing");

    let (_directory, docker, _) = serving("200 OK", "not json");
    let error = docker
        .prune_build_cache(Duration::ZERO, &|| false)
        .unwrap_err();
    assert!(matches!(error, DockerError::Unreadable(_)), "{error:?}");
    assert!(error.leaves_unknown(), "it may have pruned");

    let (_directory, docker, _) = serving("200 OK", r#"{"CachesDeleted":null}"#);
    let error = docker
        .prune_build_cache(Duration::ZERO, &|| false)
        .unwrap_err();
    assert!(
        matches!(error, DockerError::Unreadable(_)),
        "no room said: no answer of Docker's, {error:?}"
    );
}

/// A daemon that reads the request, then closes or keeps it: Okapi cannot
/// tell what it did.
#[test]
fn a_daemon_silent_once_asked_or_a_wait_cancelled_leaves_what_went_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let mut stream = stream.unwrap();
            let mut chunk = [0; 1024];
            let _ = stream.read(&mut chunk);
            // Dropped: closed without a word.
        }
    });
    let docker = Docker::at(&socket);
    let error = docker
        .prune_build_cache(Duration::ZERO, &|| false)
        .unwrap_err();
    assert!(matches!(error, DockerError::Unreadable(_)), "{error:?}");
    assert!(error.leaves_unknown());
    let error = docker
        .prune_build_cache(Duration::ZERO, &|| true)
        .unwrap_err();
    assert!(matches!(error, DockerError::Cancelled), "{error:?}");
    assert!(error.leaves_unknown(), "asked before it was cancelled");
}

#[test]
fn a_daemon_absent_has_pruned_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let error = Docker::at(&directory.path().join("docker.sock"))
        .prune_build_cache(Duration::ZERO, &|| false)
        .unwrap_err();
    assert!(matches!(error, DockerError::Absent { .. }), "{error:?}");
    assert!(!error.leaves_unknown());
    let refused = DockerError::NotAllowed {
        socket: PathBuf::from(SYSTEM_SOCKET),
        source: io::ErrorKind::PermissionDenied.into(),
    };
    assert!(!refused.leaves_unknown(), "nothing was sent");
    assert!(!DockerError::Unreached(io::ErrorKind::Other.into()).leaves_unknown());
    assert!(DockerError::Io(io::ErrorKind::TimedOut.into()).leaves_unknown());
}

/// The system judges the socket (`okapi_platform::may_write`): nothing
/// connected, a socket not there is said.
#[test]
fn a_socket_is_judged_without_a_connection() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(access(&socket).unwrap(), Access::Allowed);
    // A process the modes do not bind (root, or one that overrides them)
    // writes whatever they say.
    let probe = directory.path().join("closed");
    fs::write(&probe, b"").unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::OpenOptions::new().write(true).open(&probe).is_err() {
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(access(&socket).unwrap(), Access::NotAllowed);
    }
    assert!(
        listener.accept().is_err(),
        "no connection: a daemon its socket starts stays asleep"
    );
    let error = access(&directory.path().join("none.sock")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

#[test]
fn the_socket_is_the_one_docker_host_names_else_the_first_known_there() {
    let variables = |pairs: &[(&str, &str)]| {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name: &str| {
            pairs
                .iter()
                .find(|(wanted, _)| wanted == name)
                .map(|(_, value)| OsString::from(value))
        }
    };
    let there = |paths: &[&str]| {
        let paths: BTreeSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |path: &Path| paths.contains(path)
    };
    let runtime = ("XDG_RUNTIME_DIR", "/run/user/1000");
    let home = ("HOME", "/nowhere/someone");
    let rootless = "/run/user/1000/docker.sock";
    let desktop = "/nowhere/someone/.docker/desktop/docker.sock";

    assert_eq!(
        socket_with(
            variables(&[("DOCKER_HOST", "unix:///srv/docker.sock"), runtime]),
            there(&[SYSTEM_SOCKET])
        ),
        Some(PathBuf::from("/srv/docker.sock")),
        "DOCKER_HOST first, whatever exists"
    );
    for host in ["tcp://127.0.0.1:2375", "unix://relative.sock", "ssh://host"] {
        assert_eq!(
            socket_with(variables(&[("DOCKER_HOST", host)]), there(&[SYSTEM_SOCKET])),
            None,
            "{host}"
        );
    }
    assert_eq!(
        socket_with(variables(&[runtime, home]), there(&[desktop, rootless])),
        Some(PathBuf::from(rootless)),
        "a rootless daemon before Desktop"
    );
    assert_eq!(
        socket_with(
            variables(&[runtime, home]),
            there(&[desktop, rootless, SYSTEM_SOCKET])
        ),
        Some(PathBuf::from(SYSTEM_SOCKET)),
        "the system's first"
    );
    assert_eq!(
        socket_with(variables(&[home]), there(&[desktop])),
        Some(PathBuf::from(desktop))
    );
    assert_eq!(socket_with(variables(&[runtime, home]), there(&[])), None);
}

/// The context the CLI is set to, by its file among the contexts (their
/// folders named by a digest Okapi never computes), before the known
/// sockets.
#[test]
fn the_socket_is_the_one_of_the_context_the_cli_is_set_to() {
    let home = tempfile::tempdir().unwrap();
    let docker_folder = home.path().join(".docker");
    let context = |digest: &str, name: &str, host: &str| {
        let folder = docker_folder.join("contexts/meta").join(digest);
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("meta.json"),
            format!(r#"{{"Name":"{name}","Metadata":{{}},"Endpoints":{{"docker":{{"Host":"{host}","SkipTLSVerify":false}}}}}}"#),
        )
        .unwrap();
    };
    context("1a2b", "rootless", "unix:///run/user/1000/docker.sock");
    context("3c4d", "remote", "tcp://10.0.0.2:2376");
    let home_value = home.path().display().to_string();
    let variables = |context: Option<&'static str>| {
        let home_value = home_value.clone();
        move |name: &str| match name {
            "HOME" => Some(OsString::from(&home_value)),
            "DOCKER_CONTEXT" => context.map(OsString::from),
            _ => None,
        }
    };
    let system_there = |path: &Path| path == Path::new(SYSTEM_SOCKET);

    assert_eq!(
        socket_with(variables(Some("rootless")), system_there),
        Some(PathBuf::from("/run/user/1000/docker.sock")),
        "DOCKER_CONTEXT before the system's socket"
    );
    assert_eq!(
        socket_with(variables(Some("remote")), system_there),
        None,
        "a context over TCP: none"
    );
    fs::write(
        docker_folder.join("config.json"),
        r#"{"auths":{},"currentContext":"rootless"}"#,
    )
    .unwrap();
    assert_eq!(
        socket_with(variables(None), system_there),
        Some(PathBuf::from("/run/user/1000/docker.sock")),
        "the configuration's context"
    );
    fs::write(
        docker_folder.join("config.json"),
        r#"{"currentContext":"default"}"#,
    )
    .unwrap();
    assert_eq!(
        socket_with(variables(None), system_there),
        Some(PathBuf::from(SYSTEM_SOCKET)),
        "the default context: the known sockets"
    );
    fs::write(
        docker_folder.join("config.json"),
        r#"{"currentContext":"remote"}"#,
    )
    .unwrap();
    assert_eq!(
        socket_with(variables(Some("rootless")), system_there),
        Some(PathBuf::from("/run/user/1000/docker.sock")),
        "DOCKER_CONTEXT before the configuration's"
    );
    assert_eq!(
        socket_with(variables(Some("gone")), system_there),
        None,
        "a context named but not found: none, as the CLI fails"
    );
    let with_host = |name: &str| match name {
        "DOCKER_HOST" => Some(OsString::from("unix:///srv/docker.sock")),
        other => variables(Some("rootless"))(other),
    };
    assert_eq!(
        socket_with(with_host, system_there),
        Some(PathBuf::from("/srv/docker.sock")),
        "DOCKER_HOST before any context"
    );
}

/// `DOCKER_CONFIG` moves the CLI's configuration and contexts out of the
/// home folder; Docker Desktop's socket stays there.
#[test]
fn the_cli_s_files_are_read_where_docker_config_says() {
    let config = tempfile::tempdir().unwrap();
    let folder = config.path().join("contexts/meta/5e6f");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("meta.json"),
        r#"{"Name":"moved","Endpoints":{"docker":{"Host":"unix:///run/moved.sock"}}}"#,
    )
    .unwrap();
    fs::write(
        config.path().join("config.json"),
        r#"{"currentContext":"moved"}"#,
    )
    .unwrap();
    let config_value = config.path().display().to_string();
    let variables = move |name: &str| match name {
        "DOCKER_CONFIG" => Some(OsString::from(&config_value)),
        "HOME" => Some(OsString::from("/nowhere/someone")),
        _ => None,
    };
    assert_eq!(
        socket_with(variables, |_: &Path| false),
        Some(PathBuf::from("/run/moved.sock"))
    );
}
