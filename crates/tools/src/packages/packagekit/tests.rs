use std::cell::RefCell;

use futures_lite::future::block_on;
use zbus::zvariant::Value;

use super::*;

/// `PkExitEnum::Failed`.
const EXIT_FAILED: u32 = 2;
/// `PkInfoEnum::Installing`.
const INFO_INSTALLING: u32 = 12;
/// `PkStatusEnum::Running`.
const STATUS_RUNNING: u32 = 3;
const TRANSACTION_PATH: &str = "/org/freedesktop/PackageKit/1_abc";

/// What a status changed says.
fn status(status: u32) -> Signal {
    Signal::Properties {
        status: Some(status),
        percentage: None,
    }
}

fn package(info: u32, name: &str) -> Package {
    Package {
        info,
        id: format!("{name};1.0;amd64;ubuntu-security"),
        summary: format!("{name} summary"),
    }
}

fn folded(signals: Vec<Signal>) -> Option<Result<Vec<Package>, PackageKitError>> {
    folded_whole(signals).map(|ended| ended.map(|said| said.packages))
}

fn folded_whole(signals: Vec<Signal>) -> Option<Result<Said, PackageKitError>> {
    let mut collector = Collector::default();
    signals
        .into_iter()
        .find_map(|signal| collector.take(signal))
}

#[test]
fn packages_one_by_one_or_together_come_out_the_same_in_order() {
    // Catches either signal ignored, or their order lost when they mix.
    let one_by_one = folded(vec![
        Signal::Package(package(8, "openssl")),
        Signal::Package(package(6, "bash")),
        Signal::Package(package(5, "zsh")),
        Signal::Package(package(3, "vim")),
        Signal::Finished { exit: EXIT_SUCCESS },
    ]);
    let mixed = folded(vec![
        Signal::Package(package(8, "openssl")),
        Signal::Packages(vec![package(6, "bash"), package(5, "zsh")]),
        status(STATUS_RUNNING),
        Signal::Package(package(3, "vim")),
        Signal::Finished { exit: EXIT_SUCCESS },
    ]);
    let expected = vec![
        package(8, "openssl"),
        package(6, "bash"),
        package(5, "zsh"),
        package(3, "vim"),
    ];
    assert_eq!(one_by_one.unwrap().unwrap(), expected);
    assert_eq!(mixed.unwrap().unwrap(), expected);
}

#[test]
fn an_error_said_is_the_answer_whatever_the_end_and_the_packages_before() {
    // Catches an error swallowed into the packages said before it, or
    // overwritten by the end or by a later error.
    for exit in [EXIT_SUCCESS, EXIT_FAILED] {
        let ended = folded(vec![
            Signal::Package(package(8, "openssl")),
            Signal::ErrorCode {
                code: error_code::CANNOT_GET_LOCK,
                details: "held by unattended-upgrades".into(),
            },
            Signal::ErrorCode {
                code: 4,
                details: "then this".into(),
            },
            Signal::Finished { exit },
        ]);
        match ended {
            Some(Err(PackageKitError::Failed { code, details })) => {
                assert_eq!(code, error_code::CANNOT_GET_LOCK);
                assert_eq!(details, "held by unattended-upgrades");
            }
            other => panic!("{exit}: {other:?}"),
        }
    }
}

#[test]
fn a_failure_without_its_reason_is_no_empty_list() {
    // Catches a failed or cancelled transaction taken for « nothing to
    // update ».
    for exit in [EXIT_FAILED, EXIT_CANCELLED] {
        assert!(
            matches!(
                folded(vec![Signal::Package(package(5, "a")), Signal::Finished { exit }]),
                Some(Err(PackageKitError::Ended { exit: said })) if said == exit
            ),
            "{exit}"
        );
    }
    assert_eq!(
        folded(vec![Signal::Finished { exit: EXIT_SUCCESS }])
            .unwrap()
            .unwrap(),
        []
    );
    assert!(matches!(
        folded(vec![Signal::Package(package(5, "a")), Signal::Destroy]),
        Some(Err(PackageKitError::Vanished))
    ));
    assert!(
        folded(vec![Signal::Package(package(5, "a"))]).is_none(),
        "not ended"
    );
}

#[test]
fn waiting_follows_the_last_status_only() {
    let mut collector = Collector::default();
    assert!(!collector.waiting());
    for (said, waiting) in [
        (status::WAIT, true),
        (STATUS_RUNNING, false),
        (status::WAITING_FOR_LOCK, true),
        (STATUS_RUNNING, false),
    ] {
        collector.take(status(said));
        assert_eq!(collector.waiting(), waiting, "{said}");
    }
}

#[test]
fn a_transaction_followed_tells_its_waits_and_its_end() {
    let heard = RefCell::new(Vec::new());
    let signals = futures_lite::stream::iter(vec![
        status(status::WAITING_FOR_LOCK),
        status(status::WAITING_FOR_LOCK),
        status(STATUS_RUNNING),
        Signal::Package(package(8, "openssl")),
        Signal::Finished { exit: EXIT_SUCCESS },
    ]);
    let ended = block_on(follow(
        signals,
        Duration::from_secs(10),
        Collector::default(),
        &|waiting| heard.borrow_mut().push(waiting),
        &|_| {},
    ));
    assert_eq!(ended.unwrap().packages, [package(8, "openssl")]);
    assert_eq!(*heard.borrow(), [true, false], "each change told once");
}

#[test]
fn a_silent_transaction_times_out_saying_whether_it_still_waits() {
    // Catches a deadline never kept, or a wait once seen taken for a wait
    // still going on.
    for (statuses, still, told) in [
        // Still waiting when the time runs out: its end ends the wait too.
        (vec![status::WAITING_FOR_LOCK], true, vec![true, false]),
        (
            vec![status::WAITING_FOR_LOCK, STATUS_RUNNING],
            false,
            vec![true, false],
        ),
        (vec![], false, vec![]),
    ] {
        let heard = RefCell::new(Vec::new());
        let signals = futures_lite::stream::iter(statuses.into_iter().map(status))
            .chain(futures_lite::stream::pending());
        let ended = block_on(follow(
            signals,
            Duration::from_millis(50),
            Collector::default(),
            &|waiting| heard.borrow_mut().push(waiting),
            &|_| {},
        ));
        assert!(
            matches!(
                ended,
                Err(PackageKitError::TimedOut { waiting, waited })
                    if waiting == still && waited == Duration::from_millis(50)
            ),
            "{ended:?}"
        );
        assert_eq!(*heard.borrow(), told);
    }
    // Waiting when it fails: the wait is over all the same.
    let heard = RefCell::new(Vec::new());
    let ended = block_on(follow(
        futures_lite::stream::iter(vec![
            status(status::WAIT),
            Signal::Finished { exit: EXIT_FAILED },
        ]),
        Duration::from_secs(10),
        Collector::default(),
        &|waiting| heard.borrow_mut().push(waiting),
        &|_| {},
    ));
    assert!(matches!(ended, Err(PackageKitError::Ended { .. })));
    assert_eq!(*heard.borrow(), [true, false]);
    // A stream that ends without a word: the transaction is gone.
    let ended = block_on(follow(
        futures_lite::stream::empty(),
        Duration::from_secs(10),
        Collector::default(),
        &|_| {},
        &|_| {},
    ));
    assert!(matches!(ended, Err(PackageKitError::Vanished)));
}

fn signal<B>(member: &str, interface: &str, body: &B) -> zbus::Message
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    zbus::Message::signal(TRANSACTION_PATH, interface, member)
        .unwrap()
        .build(body)
        .unwrap()
}

#[test]
fn the_messages_of_the_bus_become_signals() {
    // Catches a body read with the wrong signature: the signal would be
    // skipped and the transaction wait for nothing.
    assert_eq!(
        signal_of(&signal(
            "Package",
            TRANSACTION,
            &(8_u32, "openssl;3.5.1;amd64;security", "TLS")
        )),
        Some(Signal::Package(Package {
            info: 8,
            id: "openssl;3.5.1;amd64;security".into(),
            summary: "TLS".into(),
        }))
    );
    assert_eq!(
        signal_of(&signal(
            "Packages",
            TRANSACTION,
            &(vec![
                (6_u32, "bash;5;amd64;x", "shell"),
                (5, "zsh;5;amd64;x", "")
            ],)
        )),
        Some(Signal::Packages(vec![
            Package {
                info: 6,
                id: "bash;5;amd64;x".into(),
                summary: "shell".into()
            },
            Package {
                info: 5,
                id: "zsh;5;amd64;x".into(),
                summary: String::new()
            },
        ]))
    );
    assert_eq!(
        signal_of(&signal("ErrorCode", TRANSACTION, &(26_u32, "locked"))),
        Some(Signal::ErrorCode {
            code: 26,
            details: "locked".into()
        })
    );
    assert_eq!(
        signal_of(&signal("Finished", TRANSACTION, &(1_u32, 301_u32))),
        Some(Signal::Finished { exit: 1 })
    );
    assert_eq!(
        signal_of(&signal("Destroy", TRANSACTION, &())),
        Some(Signal::Destroy)
    );
    let changed = |name: &str, value: Value<'static>| {
        signal(
            PROPERTIES_CHANGED,
            PROPERTIES,
            &(
                TRANSACTION,
                HashMap::from([(name, value)]),
                Vec::<String>::new(),
            ),
        )
    };
    assert_eq!(
        signal_of(&changed(STATUS, Value::from(30_u32))),
        Some(status(30))
    );
    // The percentage comes apart from the status: a status left untold.
    assert_eq!(
        signal_of(&changed(PERCENTAGE, Value::from(50_u32))),
        Some(Signal::Properties {
            status: None,
            percentage: Some(50)
        })
    );
    // Another property, a status of the wrong type, another signal: skipped.
    assert_eq!(signal_of(&changed("Speed", Value::from(50_u32))), None);
    assert_eq!(signal_of(&changed(STATUS, Value::from("running"))), None);
    assert_eq!(
        signal_of(&signal("ItemProgress", TRANSACTION, &("x", 1_u32, 2_u32))),
        None
    );
    // What tells packages, an error or the end, unreadable: said so, never
    // dropped.
    for member in ["Package", "Packages", ERROR_CODE, FINISHED] {
        assert_eq!(
            signal_of(&signal(member, TRANSACTION, &("wrong",))),
            Some(Signal::Unreadable {
                member: member.into()
            }),
            "{member}"
        );
    }
}

#[test]
fn an_update_s_severity_is_no_part_of_its_kind() {
    // Catches a kind read whole: apt sends a security update as `0x80008`
    // (its severity in the upper half), which no kind equals.
    let security = info::SECURITY | info::SECURITY << 16;
    assert_eq!(
        signal_of(&signal(
            "Package",
            TRANSACTION,
            &(security, "openssl;3.5.1;amd64;security", "TLS")
        )),
        Some(Signal::Package(Package {
            info: info::SECURITY,
            id: "openssl;3.5.1;amd64;security".into(),
            summary: "TLS".into(),
        }))
    );
    assert_eq!(
        signal_of(&signal(
            "Packages",
            TRANSACTION,
            &(vec![(security, "curl;8;amd64;x", "")],)
        )),
        Some(Signal::Packages(vec![Package {
            info: info::SECURITY,
            id: "curl;8;amd64;x".into(),
            summary: String::new()
        }]))
    );
}

#[test]
fn a_package_that_could_not_be_read_makes_the_list_unknown() {
    // Catches a list short of a package, taken for the whole list: one
    // security update lost would say « Tout est à jour ».
    let ended = folded(vec![
        Signal::Package(package(5, "bash")),
        Signal::Unreadable {
            member: "Package".into(),
        },
        Signal::Package(package(6, "zsh")),
        Signal::Finished { exit: EXIT_SUCCESS },
    ]);
    assert!(
        matches!(&ended, Some(Err(PackageKitError::Unreadable { member })) if member == "Package"),
        "{ended:?}"
    );
    // An error said wins over it: it is the cause.
    let ended = folded(vec![
        Signal::Unreadable {
            member: "Packages".into(),
        },
        Signal::ErrorCode {
            code: error_code::NO_NETWORK,
            details: "offline".into(),
        },
        Signal::Finished { exit: EXIT_FAILED },
    ]);
    assert!(matches!(
        ended,
        Some(Err(PackageKitError::Failed { code: 2, .. }))
    ));
}

#[test]
fn an_end_or_an_error_that_could_not_be_read_ends_at_once() {
    // Catches a transaction waiting for its deadline after its end went by.
    for member in [FINISHED, ERROR_CODE] {
        let mut collector = Collector::default();
        assert!(
            collector
                .take(Signal::Package(package(5, "bash")))
                .is_none()
        );
        let ended = collector.take(Signal::Unreadable {
            member: member.into(),
        });
        assert!(
            matches!(&ended, Some(Err(PackageKitError::Unreadable { member: said })) if said == member),
            "{member}: {ended:?}"
        );
    }
    // An error read before is the cause: the unreadable end does not hide it.
    let ended = folded(vec![
        Signal::ErrorCode {
            code: error_code::CANNOT_GET_LOCK,
            details: "held".into(),
        },
        Signal::Unreadable {
            member: FINISHED.into(),
        },
    ]);
    assert!(matches!(
        ended,
        Some(Err(PackageKitError::Failed {
            code: error_code::CANNOT_GET_LOCK,
            ..
        }))
    ));
}

#[test]
fn an_identifier_has_its_four_parts_and_a_name() {
    assert_eq!(
        PackageId::parse("openssl;3.5.1-1ubuntu1;amd64;ubuntu-security"),
        Some(PackageId {
            name: "openssl",
            version: "3.5.1-1ubuntu1",
            arch: "amd64",
            data: "ubuntu-security",
        })
    );
    // The data may be empty; the name may not.
    assert!(PackageId::parse("bash;5.2;amd64;").is_some());
    for wrong in ["", "bash;5.2;amd64", ";5.2;amd64;installed", "a;b;c;d;e"] {
        assert_eq!(PackageId::parse(wrong), None, "{wrong}");
    }
}

#[test]
fn only_a_missing_service_is_no_packagekit() {
    // Catches a PackageKit whose interface changed (`packagekit1`) taken for
    // one not installed.
    let name = |text: &str| zbus::names::OwnedErrorName::try_from(text.to_owned()).unwrap();
    let message = zbus::Message::method_call("/", "Ping")
        .unwrap()
        .build(&())
        .unwrap();
    for missing in super::bus::UNKNOWN_SERVICE {
        let error = zbus::Error::MethodError(name(missing), None, message.clone());
        assert!(
            matches!(absent_or_bus(error), PackageKitError::Absent),
            "{missing}"
        );
    }
    for changed in [
        "org.freedesktop.DBus.Error.AccessDenied",
        "org.freedesktop.DBus.Error.UnknownInterface",
        "org.freedesktop.DBus.Error.UnknownMethod",
    ] {
        let error = zbus::Error::MethodError(name(changed), None, message.clone());
        assert!(
            matches!(absent_or_bus(error), PackageKitError::Bus(_)),
            "{changed}"
        );
    }
}

#[test]
fn only_a_transaction_left_running_is_cancelled() {
    // Catches a late transaction left to hold the next one, or a finished
    // one cancelled for nothing.
    let unreadable = |member: &str| {
        Err(PackageKitError::Unreadable {
            member: member.into(),
        })
    };
    assert!(given_up(&Err(PackageKitError::TimedOut {
        waited: Duration::from_secs(1),
        waiting: false,
    })));
    assert!(given_up(&unreadable(FINISHED)));
    assert!(given_up(&unreadable(ERROR_CODE)));
    // Its end seen: over.
    assert!(!given_up(&unreadable("Package")));
    assert!(!given_up(&Ok(Said::default())));
    assert!(!given_up(&Err(PackageKitError::Ended {
        exit: EXIT_FAILED
    })));
}

#[test]
fn a_simulated_removal_that_would_install_anything_is_refused_never_filtered() {
    // Catches a filter that drops what is not a removal: an install unseen,
    // or every removal said by PackageKit 1.3 (`Remove`) thrown away.
    let removing = vec![
        package(info::REMOVING, "gimp"),
        package(info::REMOVE, "gimp-data"),
    ];
    assert_eq!(only_removing(removing.clone()).unwrap(), removing);
    let mut installing = removing;
    installing.push(package(INFO_INSTALLING, "gimp-alternative"));
    assert!(matches!(
        only_removing(installing),
        Err(PackageKitError::NotOnlyRemoving { packages }) if packages.len() == 1
    ));
}

#[test]
fn a_percentage_said_alone_leaves_the_status_as_it_was() {
    // Catches a percentage taken for a status of 0: the wait for another
    // program would flicker at every tick of the progress.
    let mut collector = Collector::default();
    collector.take(status(status::WAITING_FOR_LOCK));
    collector.take(Signal::Properties {
        status: None,
        percentage: Some(40),
    });
    assert!(collector.waiting());
}

#[test]
fn details_and_a_restart_are_read_and_their_loss_is_no_failure() {
    let details = |entries: Vec<(&str, Value<'static>)>| {
        signal(
            DETAILS,
            TRANSACTION,
            &(HashMap::<&str, Value<'static>>::from_iter(entries),),
        )
    };
    assert_eq!(
        signal_of(&details(vec![
            (DETAILS_ID, Value::from("curl;8;amd64;x")),
            ("size", Value::from(900_u64)),
            (DETAILS_DOWNLOAD_SIZE, Value::from(300_u64)),
        ])),
        Some(Signal::Details(Details {
            id: "curl;8;amd64;x".into(),
            download_size: Some(300),
        }))
    );
    // An older PackageKit gives no download size: unknown, never `size`.
    assert_eq!(
        signal_of(&details(vec![
            (DETAILS_ID, Value::from("curl;8;amd64;x")),
            ("size", Value::from(900_u64)),
        ])),
        Some(Signal::Details(Details {
            id: "curl;8;amd64;x".into(),
            download_size: None,
        }))
    );
    assert_eq!(
        signal_of(&details(vec![("size", Value::from(900_u64))])),
        None
    );
    assert_eq!(
        signal_of(&signal(
            REQUIRE_RESTART,
            TRANSACTION,
            &(4_u32, "linux;7;amd64;x")
        )),
        Some(Signal::RequireRestart { kind: 4 })
    );
    for member in [DETAILS, REQUIRE_RESTART] {
        assert_eq!(
            signal_of(&signal(member, TRANSACTION, &("wrong",))),
            None,
            "{member}"
        );
    }
}

#[test]
fn only_a_restart_of_the_system_is_one_and_the_details_are_kept() {
    // Catches a session's restart said as the system's, or the reverse.
    for (kind, system) in [
        (RESTART_SYSTEM, true),
        (RESTART_SECURITY_SYSTEM, true),
        (3, false),
        (5, false),
    ] {
        let said = folded_whole(vec![
            Signal::RequireRestart { kind },
            Signal::Details(Details {
                id: "a;1;amd64;x".into(),
                download_size: Some(7),
            }),
            Signal::Finished { exit: EXIT_SUCCESS },
        ])
        .unwrap()
        .unwrap();
        assert_eq!(said.system_restart, system, "{kind}");
        assert_eq!(said.details.len(), 1);
    }
}

#[test]
fn a_package_of_the_progress_lost_never_fails_the_update() {
    // Catches two hours of installation said failed for one signal of its
    // progress that could not be read.
    let mut collector = Collector::of_progress();
    collector.take(Signal::Unreadable {
        member: "Package".into(),
    });
    assert!(matches!(
        collector.take(Signal::Finished { exit: EXIT_SUCCESS }),
        Some(Ok(_))
    ));
    // Its end unreadable still ends it, unknown.
    let mut collector = Collector::of_progress();
    assert!(matches!(
        collector.take(Signal::Unreadable {
            member: FINISHED.into()
        }),
        Some(Err(PackageKitError::Unreadable { .. }))
    ));
}

#[test]
fn every_signal_followed_is_heard_in_order() {
    let heard = RefCell::new(Vec::new());
    let said = vec![
        status(status::DOWNLOAD),
        Signal::Package(package(info::FINISHED, "a")),
        Signal::Finished { exit: EXIT_SUCCESS },
    ];
    let ended = block_on(follow(
        futures_lite::stream::iter(said.clone()),
        Duration::from_secs(10),
        Collector::of_progress(),
        &|_| {},
        &|signal| heard.borrow_mut().push(signal.clone()),
    ));
    assert!(ended.is_ok());
    assert_eq!(*heard.borrow(), said);
}

#[test]
fn an_update_installs_only_signed_packages_and_a_simulation_changes_nothing() {
    // Catches an update that would install a package no trusted key signs,
    // or a simulation that would install: the bits of `pk-bitfield.h`.
    assert_eq!(
        update_call(false),
        UpdateCall {
            flags: 2,
            wait: Duration::from_hours(2),
            interactive: true,
        }
    );
    assert_eq!(
        update_call(true),
        UpdateCall {
            flags: 2 | 4,
            wait: Duration::from_mins(2),
            interactive: false,
        }
    );
}

#[test]
fn what_an_update_says_becomes_its_progress() {
    // Catches several packages at once (asked for) left unheard, and a lost
    // package never told.
    assert_eq!(
        updating_of(&Signal::Packages(vec![
            package(10, "a"),
            package(info::FINISHED, "a")
        ])),
        [
            Updating::Package {
                info: 10,
                id: package(10, "a").id
            },
            Updating::Package {
                info: info::FINISHED,
                id: package(10, "a").id
            },
        ]
    );
    assert_eq!(
        updating_of(&Signal::Properties {
            status: Some(8),
            percentage: Some(40)
        }),
        [Updating::Status(8), Updating::Percentage(40)]
    );
    for member in ["Package", "Packages"] {
        assert_eq!(
            updating_of(&Signal::Unreadable {
                member: member.into()
            }),
            [Updating::Unreadable],
            "{member}"
        );
    }
    assert_eq!(
        updating_of(&Signal::Finished { exit: EXIT_SUCCESS }),
        Vec::new()
    );
}
