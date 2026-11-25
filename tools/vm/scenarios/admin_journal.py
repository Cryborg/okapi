"""Journal: the vacuum of systemd's journal, by age, by size, and never without the machine's name.

Archives are planted beside the machine's real journals, named as journald
names them (the vacuum dates them by their name and their own dates, never by
their header). Through the interface: the line is offered unchecked, measured
by root, and its cleaning removes the archives older than 30 days but one
changed since the rights and one too young (ADM-015, INV-207). By hand, as
root: over 1 GB the oldest go, unless a locked one holds the size back
(ADM-016); `/etc/machine-id` away, nothing goes (ADM-017).
"""

from __future__ import annotations

import secrets
import shlex
from typing import NamedTuple

from okapivm import common
from okapivm.bench import Bench

from scenarios import _admin

TAGS = ["admin", "helper", "journal"]

JOURNAL = "/var/log/journal"
MACHINE_ID = "/etc/machine-id"
MACHINE_ID_ASIDE = "/etc/machine-id.okapi-vm"
DAY_SECONDS = 24 * 60 * 60
MAX_AGE_DAYS = 30
OLD_DAYS = MAX_AGE_DAYS + 10
MAX_USE_BYTES = 1_000_000_000
MEGABYTE = 1024 * 1024
SMALL_ARCHIVE_BYTES = MEGABYTE
LARGE_ARCHIVES = 6
LARGE_ARCHIVE_BYTES = 200 * MEGABYTE
BLOCK_BYTES = 512
LOCKED_TRACE = "an archive locked: kept, counted"


class Archive(NamedTuple):
    """An archived journal named as journald names one: its first entry at `realtime` (seconds)."""

    sequence: int
    realtime: float

    @property
    def name(self) -> str:
        return f"system@{SEQUENCE_ID}-{self.sequence:016x}-{int(self.realtime * 1_000_000):016x}.journal"


SEQUENCE_ID = secrets.token_hex(16)


def clock(bench: Bench) -> float:
    return float(bench.root("date +%s").strip())


def directory(bench: Bench) -> str:
    return f"{JOURNAL}/{bench.root(shlex.join(['cat', MACHINE_ID])).strip()}"


def plant(bench: Bench, where: str, archives: list[Archive], size: int, *, allocated: bool = False) -> None:
    write = (lambda path: shlex.join(["fallocate", "--length", str(size), path])) if allocated else (
        lambda path: f"head -c {size} /dev/urandom > {shlex.quote(path)}")
    bench.root(" && ".join(write(f"{where}/{archive.name}") for archive in archives))
    bench.log(f"planted {len(archives)} archives of {size} bytes in {where}")


def names(bench: Bench, where: str) -> set[str]:
    return set(bench.root(shlex.join(["ls", "-1", where])).split())


def used(bench: Bench, where: str) -> dict[str, int]:
    """The room each file of `where` takes on disk, as the vacuum counts it."""
    listed = bench.root(shlex.join(["find", where, "-maxdepth", "1", "-type", "f", "-printf", "%b %f\\n"]))
    return {name: int(blocks) * BLOCK_BYTES for blocks, name in (line.split(" ", 1) for line in listed.splitlines())}


def vacuum(bench: Bench) -> common.Outcome:
    return common.helper(bench, {"version": 5, "operation": "clean", "targets": [JOURNAL], "minAgeMinutes": 60,
                                 "exclusions": []})


def by_age_through_the_interface(bench: Bench, where: str) -> None:
    now = clock(bench)
    removed = Archive(1, now - OLD_DAYS * DAY_SECONDS)
    touched = Archive(2, now - OLD_DAYS * DAY_SECONDS + 1)
    recent = Archive(3, now - DAY_SECONDS)
    young = Archive(4, now - OLD_DAYS * DAY_SECONDS + 2)
    plant(bench, where, [removed, touched, recent], SMALL_ARCHIVE_BYTES)
    bench.advance_clock(_admin.AGING_SECONDS)
    plant(bench, where, [young], SMALL_ARCHIVE_BYTES)
    active = {name for name in names(bench, where) if "@" not in name}

    _admin.scan(bench)
    line = _admin.row(bench, _admin.JOURNAL_LOGS)
    bench.check(line.get("description") == _admin.ADMINISTRATOR,
                f"ADM-015: before the rights, the journal's line is not measured ({line.get('description')!r})")
    _admin.obtain_rights(bench)
    line = _admin.row(bench, _admin.JOURNAL_LOGS)
    bench.screenshot("journal-mesure")
    bench.check(_admin.CHECKED not in line["states"],
                f"ADM-015: the journal's line is offered unchecked ({line['states']})")
    bench.check("Éléments : 2," in line.get("description", ""),
                f"ADM-015: root measured the two old archives, the young one counted out ({line.get('description')!r})")

    bench.root(shlex.join(["touch", f"{where}/{touched.name}"]))
    _admin.check_groups(bench, _admin.JOURNAL_LOGS)
    _admin.clean(bench)
    bench.screenshot("journal-nettoye")
    left = names(bench, where)
    bench.check(removed.name not in left, "ADM-015: the archive older than 30 days is gone")
    bench.check(recent.name in left and active <= left,
                f"ADM-015: the recent archive and the journals being written stay ({sorted(active - left)} gone)")
    bench.check(touched.name in left, "INV-207: an archive changed after the rights stays at the cleaning")
    bench.check(young.name in left, "INV-207: an archive too young at the cleaning stays")
    receipt = common.last_receipt(bench)
    bench.check(receipt["admin"]["done"], f"ADM-015: the receipt holds the administrator's part done ({receipt['admin']})")
    bench.stop_app()
    bench.root(shlex.join(["rm", "-f", *(f"{where}/{archive.name}" for archive in (touched, recent, young))]))


def by_size(bench: Bench, where: str) -> None:
    now = clock(bench)
    archives = [Archive(10 + index, now - DAY_SECONDS + index) for index in range(LARGE_ARCHIVES)]
    plant(bench, where, archives, LARGE_ARCHIVE_BYTES, allocated=True)
    bench.advance_clock(_admin.AGING_SECONDS)
    oldest = f"{where}/{archives[0].name}"
    bench.root(shlex.join(["chattr", "+i", oldest]))
    held = vacuum(bench)
    left = names(bench, where)
    bench.check(held.code == 0 and all(archive.name in left for archive in archives),
                f"ADM-016: the oldest archive locked, no newer one goes in its place (exit {held.code})")
    bench.check(LOCKED_TRACE in held.stderr, "ADM-016: the helper's traces say the locked archive was kept")

    bench.root(shlex.join(["chattr", "-i", oldest]))
    # Unlocking it changed it: aged again, beyond the floor.
    bench.advance_clock(_admin.AGING_SECONDS)
    before = used(bench, where)
    total = sum(before.values())
    expected = []
    for archive in archives:
        if total <= MAX_USE_BYTES:
            break
        expected.append(archive.name)
        total -= before[archive.name]
    outcome = vacuum(bench)
    left = names(bench, where)
    gone = [archive.name for archive in archives if archive.name not in left]
    bench.check(outcome.code == 0 and gone == expected and expected,
                f"ADM-016: over 1 GB the oldest archives go until it fits ({len(gone)} gone, {len(expected)} expected)")
    bench.check(sum(used(bench, where).values()) <= MAX_USE_BYTES + 10 * MEGABYTE,
                "ADM-016: the machine's journal takes at most 1 GB once vacuumed (journald's writes aside)")
    bench.root(shlex.join(["rm", "-f", *(f"{where}/{name}" for name in left if SEQUENCE_ID in name)]))


def without_machine_id(bench: Bench, where: str) -> None:
    old = Archive(20, clock(bench) - OLD_DAYS * DAY_SECONDS)
    plant(bench, where, [old], SMALL_ARCHIVE_BYTES)
    bench.advance_clock(_admin.AGING_SECONDS)
    bench.root(shlex.join(["mv", MACHINE_ID, MACHINE_ID_ASIDE]))
    try:
        outcome = vacuum(bench)
    finally:
        bench.root(shlex.join(["mv", MACHINE_ID_ASIDE, MACHINE_ID]))
    answer = common.helper_answer(outcome)
    issues = [issue for issue in answer["issues"] if issue["kind"] == "unreadableRoot" and issue.get("path") == MACHINE_ID]
    bench.check(bool(issues), f"ADM-017: without {MACHINE_ID}, the vacuum says it unreadable ({answer['issues']})")
    bench.check(old.name in names(bench, where), "ADM-017: without the machine's name, no archive goes")


def run(bench: Bench) -> None:
    bench.allow_elevation_without_password()
    where = directory(bench)
    bench.log(f"the machine's journal: {where}")
    by_age_through_the_interface(bench, where)
    by_size(bench, where)
    without_machine_id(bench, where)
