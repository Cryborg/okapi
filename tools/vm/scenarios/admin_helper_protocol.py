"""Helper: its protocol from a root shell, each refusal whole, and exclusions it never reads.

Requests written by hand to the installed helper, as root: every malformed or
overreaching cleaning is refused whole with exit 2, the crash report planted
left (ADM-018, INV-104); an age of 60 minutes is accepted. Run as the user, it
reads nothing (ADM-019). An exclusion that is a link answers the same whether
the link leads somewhere or nowhere, and its traces never say where (INV-307).
"""

from __future__ import annotations

import json
import shlex

from okapivm import common, config
from okapivm.bench import Bench

from scenarios import _admin

TAGS = ["admin", "helper", "invariants"]

VERSION = 5
MAX_REQUEST_BYTES = 64 * 1024
REFUSED = 2
NOT_ROOT = 3
CRASH = "/var/crash"
CRASH_FILE = f"{CRASH}/okapi-vm.crash"
SECRET = f"{CRASH}/okapi-vm-secret"
SECRET_FILE = f"{SECRET}/okapi-vm.crash"
# Where the dead link leads: a name that appears nowhere else.
HIDDEN = f"{CRASH}/okapi-vm-hidden"
LIVE_LINK = "/var/tmp/okapi-vm-live"
DEAD_LINK = "/var/tmp/okapi-vm-dead"


def request(**changes: object) -> dict:
    return {"version": VERSION, "operation": "clean", "targets": [CRASH], "minAgeMinutes": 60, "exclusions": [],
            **changes}


def sized(length: int) -> str:
    """A cleaning request of exactly `length` bytes, its exclusions grown to fill it."""
    exclusions = []
    while len(json.dumps(request(exclusions=[*exclusions, "/okapi-vm/" + "x" * 200]))) < length:
        exclusions.append("/okapi-vm/" + "x" * 200)
    filler = length - len(json.dumps(request(exclusions=[*exclusions, "/okapi-vm/"])))
    line = json.dumps(request(exclusions=[*exclusions, "/okapi-vm/" + "x" * filler]))
    assert len(line) == length, len(line)
    return line


# Each refused whole: (what it tries, the request, the refusal, the target at fault).
REFUSALS = [
    ("an older version", request(version=VERSION - 1), "unsupportedVersion", None),
    ("a newer version", request(version=VERSION + 1), "unsupportedVersion", None),
    ("a target of no rule", request(targets=["/var/tmp"]), "unknownTarget", "/var/tmp"),
    ("a target written otherwise", request(targets=[f"{CRASH}/"]), "unknownTarget", f"{CRASH}/"),
    ("a file named beside its target (INV-104)", request(targets=[CRASH, CRASH_FILE]), "unknownTarget", CRASH_FILE),
    ("a target twice", request(targets=[CRASH, CRASH]), "duplicateTarget", CRASH),
    ("no target", request(targets=[]), "noTarget", None),
    ("an age of 59 minutes", request(minAgeMinutes=59), "ageBelowFloor", None),
    ("a relative exclusion", request(exclusions=["okapi-vm"]), "exclusion", None),
    ("a field it does not know", request(command="rm"), "invalid", None),
    ("no JSON", "rm -rf /var/crash", "invalid", None),
    ("a line of 64 KiB and one byte", sized(MAX_REQUEST_BYTES + 1), "tooLong", None),
]


def refusals(bench: Bench) -> None:
    for tried, asked, key, target in REFUSALS:
        outcome = common.helper(bench, asked)
        answer = common.helper_answer(outcome)
        bench.check(outcome.code == REFUSED and answer.get("refused") == key and answer.get("target") == target
                    and answer.get("version") == VERSION,
                    f"ADM-018: {tried} is refused whole, {key}, exit {REFUSED} ({outcome.code}, {answer})")
    common.check_kept(bench, common.Files(common.ROOT, [CRASH_FILE, SECRET_FILE]))
    bench.log("ok: ADM-018, INV-104: no refused cleaning touched the crash reports")


def accepted(bench: Bench) -> None:
    outcome = common.helper(bench, request(operation="scan"))
    answer = common.helper_answer(outcome)
    offered = sum(group["entries"] for group in answer["groups"] if CRASH in group["targets"])
    bench.check(outcome.code == 0 and offered == 2,
                f"ADM-018: an age of 60 minutes is accepted, the two crash reports offered ({outcome.code}, {answer})")
    exact = common.helper(bench, sized(MAX_REQUEST_BYTES).replace('"clean"', '"scan" '))
    bench.check(exact.code == 0, f"ADM-018: a request of 64 KiB exactly is read ({exact.code}: {exact.stdout[:200]})")


def not_root(bench: Bench) -> None:
    outcome = common.helper(bench, request(), account=bench.accounts.admin.name)
    bench.check(outcome.code == NOT_ROOT and not outcome.stdout and "not run as root" in outcome.stderr,
                f"ADM-019: run as the user, the helper reads nothing and exits {NOT_ROOT} ({outcome.code})")


def links(bench: Bench) -> None:
    bench.root(" && ".join([shlex.join(["ln", "-s", SECRET, LIVE_LINK]), shlex.join(["ln", "-s", HIDDEN, DEAD_LINK])]))
    runs = {name: common.helper(bench, request(operation="scan", exclusions=exclusions))
            for name, exclusions in (("none", []), ("live", [LIVE_LINK]), ("dead", [DEAD_LINK]))}
    answers = {name: common.helper_answer(outcome) for name, outcome in runs.items()}
    bench.check(answers["live"] == answers["dead"] == answers["none"],
                f"INV-307: an exclusion that is a link, live or dead, answers as none: never resolved ({answers})")
    bench.check(all(HIDDEN not in outcome.stderr for outcome in runs.values()),
                "INV-307: the traces never name where the dead link leads")
    secret = {name: outcome.stderr.count(SECRET) for name, outcome in runs.items()}
    bench.check(secret["live"] == secret["none"],
                f"INV-307: the live link's traces name where it leads no more than a scan without it ({secret})")


def run(bench: Bench) -> None:
    common.plant(bench, common.Files(common.ROOT, [CRASH_FILE, SECRET_FILE], size=1024),
                 age_seconds=_admin.AGING_SECONDS)
    installed = config.helper_paths().helper
    bench.log(f"requests written by hand to {installed}")
    refusals(bench)
    accepted(bench)
    not_root(bench)
    links(bench)
    # INV-105 cannot be set up: no root of a rule that needs the administrator
    # is shared on Linux (`rule_set.rs` validates it), so no group can join a
    # rule not asked for; the helper's unit tests keep it.
    bench.log("INV-105 not set up in the machine: no administrator root is shared on Linux")
