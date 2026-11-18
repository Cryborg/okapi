use std::os::unix::net::{UnixListener, UnixStream};
use std::str::FromStr as _;
use std::sync::mpsc;

use futures_lite::StreamExt as _;

use super::*;

const APP: &str = "app/org.gimp.GIMP/x86_64/stable";
const LOCALE: &str = "runtime/org.gimp.GIMP.Locale/x86_64/stable";
/// Short: a test waits for a silent helper no longer.
const SHORT: Waits = Waits {
    uninstall: Duration::from_millis(300),
    cleanup: Duration::from_millis(300),
};

/// What the fake helper answers a call.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Answer {
    Done,
    Error(&'static str),
    Silent,
}

/// One call the fake helper heard, whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Heard {
    destination: String,
    path: String,
    interface: String,
    pub(crate) member: String,
    signature: String,
    flags: u32,
    /// The reference (`Uninstall`), then the installation.
    pub(crate) strings: Vec<String>,
}

/// A helper on the other end of a socket pair, answering each call as
/// `answer` says from its method and its first string; what it heard.
pub(crate) fn fake_helper(
    answer: impl Fn(&str, &str) -> Answer + Send + 'static,
) -> (SystemHelper, mpsc::Receiver<Heard>) {
    fake_helper_waiting(answer, SHORT)
}

/// [`fake_helper`], its client waiting `waits`.
fn fake_helper_waiting(
    answer: impl Fn(&str, &str) -> Answer + Send + 'static,
    waits: Waits,
) -> (SystemHelper, mpsc::Receiver<Heard>) {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let (server, client) = futures_lite::future::block_on(futures_lite::future::zip(
        zbus::connection::Builder::async_io_unix_stream(server_end)
            .p2p()
            .server(zbus::Guid::generate())
            .unwrap()
            .build(),
        zbus::connection::Builder::async_io_unix_stream(client_end)
            .p2p()
            .build(),
    ));
    let server = server.unwrap();
    // Listened to before the client sends anything.
    let mut messages = zbus::MessageStream::from(&server);
    let (told, heard) = mpsc::channel();
    std::thread::spawn(move || {
        futures_lite::future::block_on(async move {
            while let Some(Ok(message)) = messages.next().await {
                let header = message.header();
                if header.message_type() != zbus::message::Type::MethodCall {
                    continue;
                }
                let body = message.body();
                let signature = body.signature().to_string();
                // A body's signature reads as a structure's: `(uss)`.
                let (flags, strings) = match signature.as_str() {
                    "(uss)" => {
                        let (flags, reference, installation): (u32, String, String) =
                            body.deserialize().unwrap();
                        (flags, vec![reference, installation])
                    }
                    "(us)" => {
                        let (flags, installation): (u32, String) = body.deserialize().unwrap();
                        (flags, vec![installation])
                    }
                    _ => (u32::MAX, Vec::new()),
                };
                let member = header.member().map(ToString::to_string).unwrap_or_default();
                let answered = answer(&member, strings.first().map_or("", String::as_str));
                let _ = told.send(Heard {
                    destination: header
                        .destination()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    path: header.path().map(ToString::to_string).unwrap_or_default(),
                    interface: header
                        .interface()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    member,
                    signature,
                    flags,
                    strings,
                });
                match answered {
                    Answer::Done => server.reply(&header, &()).await.unwrap(),
                    Answer::Error(name) => server
                        .reply_error(&header, name, &"the helper's words")
                        .await
                        .unwrap(),
                    Answer::Silent => {}
                }
            }
        });
    });
    let helper = SystemHelper {
        connection: client.unwrap(),
        waits,
    };
    (helper, heard)
}

fn members(heard: &mpsc::Receiver<Heard>) -> Vec<(String, Vec<String>)> {
    heard
        .try_iter()
        .map(|heard| (heard.member, heard.strings))
        .collect()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn a_removal_is_the_application_then_its_extensions_then_the_repository_pruned_then_the_triggers() {
    // Catches a clean-up forgotten, steps in another order, flags set
    // (`FORCE_REMOVE`), another installation, another service or object.
    let (helper, heard) = fake_helper(|_, _| Answer::Done);
    assert_eq!(
        helper.remove(APP, &[LOCALE.to_owned()]).unwrap(),
        Removed {
            left: Vec::new(),
            already_gone: Vec::new(),
            pruned: true,
        }
    );
    let calls: Vec<Heard> = heard.try_iter().collect();
    for call in &calls {
        assert_eq!(call.destination, DESTINATION);
        assert_eq!(call.path, OBJECT);
        assert_eq!(call.interface, INTERFACE);
        assert_eq!(call.flags, 0, "{call:?}");
        assert_eq!(
            call.strings.last().map(String::as_str),
            Some(""),
            "{call:?}"
        );
    }
    let said: Vec<(&str, &str, &str)> = calls
        .iter()
        .map(|call| {
            (
                call.member.as_str(),
                call.signature.as_str(),
                call.strings[0].as_str(),
            )
        })
        .collect();
    assert_eq!(
        said,
        [
            (UNINSTALL, "(uss)", APP),
            (UNINSTALL, "(uss)", LOCALE),
            (PRUNE, "(us)", ""),
            (RUN_TRIGGERS, "(us)", ""),
        ]
    );
}

#[test]
fn the_application_refused_nothing_more_is_asked() {
    // Catches extensions or a clean-up run for an application still there,
    // and a refusal of polkit told as a failure of the helper.
    let (helper, heard) = fake_helper(|member, reference| {
        if member == UNINSTALL && reference == APP {
            Answer::Error("org.freedesktop.DBus.Error.AccessDenied")
        } else {
            Answer::Done
        }
    });
    // The helper's own words carried, to be shown.
    assert!(matches!(
        helper.remove(APP, &[LOCALE.to_owned()]),
        Err(FlatpakError::NotAuthorized(words)) if words == "the helper's words"
    ));
    assert_eq!(
        members(&heard),
        [(UNINSTALL.to_owned(), strings(&[APP, ""]))]
    );
}

#[test]
fn an_extension_or_a_clean_up_that_fails_never_undoes_the_removal() {
    // Catches a failure past the application told as the application still
    // there, the triggers skipped when pruning failed, and the space said
    // free when the repository was not pruned.
    let (helper, heard) = fake_helper(|member, reference| match (member, reference) {
        (UNINSTALL, LOCALE) => Answer::Error("org.freedesktop.DBus.Error.AccessDenied"),
        (PRUNE, _) => Answer::Error("org.freedesktop.Flatpak.Error.Aborted"),
        _ => Answer::Done,
    });
    assert_eq!(
        helper.remove(APP, &[LOCALE.to_owned()]).unwrap(),
        Removed {
            left: vec![LOCALE.to_owned()],
            already_gone: Vec::new(),
            pruned: false,
        }
    );
    assert_eq!(heard.try_iter().count(), 4, "the triggers run all the same");

    // The triggers failing frees no less space.
    let (helper, _heard) = fake_helper(|member, _| {
        if member == RUN_TRIGGERS {
            Answer::Error("org.freedesktop.Flatpak.Error.Aborted")
        } else {
            Answer::Done
        }
    });
    assert!(helper.remove(APP, &[]).unwrap().pruned);
}

#[test]
fn a_helper_that_never_answers_is_waited_for_no_longer_than_its_wait() {
    // Catches a call without a bound: the uninstallation's thread held
    // forever.
    let (helper, _heard) = fake_helper(|_, _| Answer::Silent);
    assert!(matches!(
        helper.remove(APP, &[]),
        Err(FlatpakError::TimedOut { waited }) if waited == SHORT.uninstall
    ));
}

#[test]
fn a_bus_that_never_answers_is_joined_no_longer_than_its_wait() {
    // Catches a connection without a bound: a socket that accepts and never
    // speaks.
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("bus");
    let listener = UnixListener::bind(&socket).unwrap();
    let held = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));
    let address = zbus::Address::from_str(&format!("unix:path={}", socket.display())).unwrap();
    assert!(matches!(
        joined(address, Duration::from_millis(200)),
        Err(FlatpakError::Unanswered(_))
    ));
    drop(held);
}

#[test]
fn each_error_of_the_helper_is_told_by_its_name() {
    // Catches a refusal of polkit shown as a failure, a helper absent shown
    // as a refusal, an outcome unknown shown as the service's words.
    let told = |name: &str| of_reply(name, "words".to_owned());
    for name in bus::UNKNOWN_SERVICE.into_iter().chain(NOT_PROVIDED) {
        assert!(matches!(told(name), FlatpakError::Absent), "{name}");
    }
    for name in NOT_AUTHORIZED {
        assert!(
            matches!(told(name), FlatpakError::NotAuthorized(_)),
            "{name}"
        );
    }
    for name in NOT_INSTALLED {
        assert!(
            matches!(told(name), FlatpakError::NotInstalled(_)),
            "{name}"
        );
    }
    for name in UNANSWERED
        .into_iter()
        .chain(["org.freedesktop.DBus.Error.Spawn.ChildExited"])
    {
        assert!(matches!(told(name), FlatpakError::Unanswered(_)), "{name}");
    }
    assert!(matches!(
        told("org.freedesktop.Flatpak.Error.Aborted"),
        FlatpakError::Service { name, message }
            if name == "org.freedesktop.Flatpak.Error.Aborted" && message == "words"
    ));
}

#[test]
fn a_refusal_without_words_is_told_by_its_name() {
    // Catches « refused: » followed by nothing on the screen.
    assert!(matches!(
        of_reply("org.freedesktop.Flatpak.Error.Aborted", String::new()),
        FlatpakError::Service { message, .. } if message == "org.freedesktop.Flatpak.Error.Aborted"
    ));
}

#[test]
fn each_extension_is_asked_whatever_the_one_before_and_one_already_gone_went() {
    // Catches the extensions given up after the first that fails, and one
    // already gone told left.
    let second = "runtime/org.gimp.GIMP.Debug/x86_64/stable";
    let (helper, heard) = fake_helper(move |member, reference| match (member, reference) {
        (UNINSTALL, LOCALE) => Answer::Error("org.freedesktop.Flatpak.Error.NotInstalled"),
        (UNINSTALL, reference) if reference == second => {
            Answer::Error("org.freedesktop.Flatpak.Error.Aborted")
        }
        _ => Answer::Done,
    });
    let third = "runtime/org.gimp.GIMP.Plugin/x86_64/stable";
    assert_eq!(
        helper
            .remove(
                APP,
                &[LOCALE.to_owned(), second.to_owned(), third.to_owned()]
            )
            .unwrap(),
        Removed {
            left: vec![second.to_owned()],
            already_gone: vec![LOCALE.to_owned()],
            pruned: true,
        }
    );
    assert_eq!(heard.try_iter().count(), 6, "each asked, then the clean-up");
}

#[test]
fn a_silent_clean_up_or_extension_holds_no_longer_than_its_own_wait() {
    // Catches a clean-up waited for as long as an uninstallation (minutes
    // on a silent helper), and a silent extension told gone.
    let waits = Waits {
        uninstall: Duration::from_secs(5),
        cleanup: Duration::from_millis(200),
    };
    for silent in [PRUNE, RUN_TRIGGERS] {
        let (helper, _heard) = fake_helper_waiting(
            move |member, _| {
                if member == silent {
                    Answer::Silent
                } else {
                    Answer::Done
                }
            },
            waits,
        );
        let started = Instant::now();
        let removed = helper.remove(APP, &[]).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "{silent}");
        assert_eq!(removed.pruned, silent != PRUNE, "{silent}");
    }
    let (helper, _heard) = fake_helper(|member, reference| {
        if member == UNINSTALL && reference == LOCALE {
            Answer::Silent
        } else {
            Answer::Done
        }
    });
    assert_eq!(
        helper.remove(APP, &[LOCALE.to_owned()]).unwrap().left,
        [LOCALE]
    );
}

#[test]
fn an_application_and_its_extensions_wait_as_long_as_an_uninstallation() {
    // Catches an extension (or the application) waited for only as long as a
    // clean-up: a polkit window still open given up too soon.
    let waits = Waits {
        uninstall: Duration::from_millis(600),
        cleanup: Duration::from_millis(100),
    };
    let (helper, _heard) = fake_helper_waiting(
        |member, reference| {
            if member == UNINSTALL && reference == LOCALE {
                Answer::Silent
            } else {
                Answer::Done
            }
        },
        waits,
    );
    let started = Instant::now();
    assert_eq!(
        helper.remove(APP, &[LOCALE.to_owned()]).unwrap().left,
        [LOCALE]
    );
    assert!(
        started.elapsed() >= waits.uninstall,
        "{:?}",
        started.elapsed()
    );
    let (helper, _heard) = fake_helper_waiting(|_, _| Answer::Silent, waits);
    let started = Instant::now();
    assert!(matches!(
        helper.remove(APP, &[]),
        Err(FlatpakError::TimedOut { .. })
    ));
    assert!(
        started.elapsed() >= waits.uninstall,
        "{:?}",
        started.elapsed()
    );
}

const PLATFORM: &str = "runtime/org.gnome.Platform/x86_64/45";
const SDK: &str = "runtime/org.gnome.Sdk/x86_64/45";
const CODECS: &str = "runtime/org.freedesktop.Platform.codecs-extra/x86_64/23.08";

/// The kind of each end, in order: what a test compares.
fn ends(removed: &RuntimesRemoved) -> Vec<&'static str> {
    removed
        .ends
        .iter()
        .map(|end| match end {
            RuntimeEnd::Removed => "removed",
            RuntimeEnd::AlreadyGone => "gone",
            RuntimeEnd::NotAsked => "not asked",
            RuntimeEnd::Failed(_) => "failed",
        })
        .collect()
}

#[test]
fn runtimes_are_removed_in_their_order_then_the_repository_pruned_once_without_triggers() {
    // Catches a runtime asked with flags or for another installation, the
    // repository pruned after each or never, the triggers run for runtimes.
    let (helper, heard) = fake_helper(|_, _| Answer::Done);
    let removed = helper.remove_runtimes(&strings(&[PLATFORM, SDK]), &|_, _| {});
    assert_eq!(ends(&removed), ["removed", "removed"]);
    assert!(removed.pruned);
    let calls: Vec<Heard> = heard.try_iter().collect();
    assert!(calls.iter().all(|call| call.flags == 0 && call.strings.last().is_some_and(String::is_empty)), "{calls:?}");
    let said: Vec<(&str, &str)> = calls
        .iter()
        .map(|call| (call.member.as_str(), call.strings[0].as_str()))
        .collect();
    assert_eq!(
        said,
        [(UNINSTALL, PLATFORM), (UNINSTALL, SDK), (PRUNE, "")]
    );
}

#[test]
fn the_rights_refused_ask_no_other_runtime_and_a_runtime_kept_asks_the_next() {
    // Catches a polkit window opened again for each runtime after a no, a
    // runtime the helper keeps (needed by an application) stopping the
    // others, one already gone told removed, and a prune of nothing.
    let (helper, heard) = fake_helper(|member, reference| match (member, reference) {
        (UNINSTALL, PLATFORM) => Answer::Error("org.freedesktop.Flatpak.Error.RuntimeUsed"),
        (UNINSTALL, SDK) => Answer::Error("org.freedesktop.Flatpak.Error.NotInstalled"),
        (UNINSTALL, CODECS) => Answer::Error("org.freedesktop.DBus.Error.AccessDenied"),
        _ => Answer::Done,
    });
    // Each end told as it comes, by its rank: a line of history is written
    // for each before the next is asked.
    let told = std::cell::RefCell::new(Vec::new());
    let removed = helper.remove_runtimes(&strings(&[PLATFORM, SDK, CODECS, LOCALE]), &|rank, _| {
        told.borrow_mut().push(rank);
    });
    assert_eq!(ends(&removed), ["failed", "gone", "failed", "not asked"]);
    assert_eq!(told.into_inner(), [0, 1, 2, 3]);
    assert!(!removed.pruned, "nothing went: nothing pruned");
    let asked: Vec<String> = heard.try_iter().map(|heard| heard.strings[0].clone()).collect();
    assert_eq!(asked, strings(&[PLATFORM, SDK, CODECS]));

    // One went, the prune refused: the space is not known free.
    let (helper, _heard) = fake_helper(|member, _| {
        if member == PRUNE {
            Answer::Error("org.freedesktop.Flatpak.Error.Aborted")
        } else {
            Answer::Done
        }
    });
    let removed = helper.remove_runtimes(&strings(&[PLATFORM]), &|_, _| {});
    assert_eq!(ends(&removed), ["removed"]);
    assert!(!removed.pruned);
}
