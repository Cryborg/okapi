"""Invariant 2: the minimum age is checked again right before each removal, descendants included.

Old thumbnails and crash reports are planted, then attacked between the scan
and the cleaning: a file and a deep descendant of a folder written to, as the
user and as root once the rights are obtained. They stay, the file « changed
too recently », the user's folder whole, while their old neighbours go; the
helper, which scans again at the cleaning, holds the young descendant back
and offers its old neighbours one by one (INV-201, INV-203). A file dated two years back by `touch -d` is still new by its inode
date and never offered (INV-202). A folder changed at the scan (a lock taken
with `mkdir`) is not offered, its old files are (INV-204). Two names of one
old inode both go: removing the first does not make the second recent
(INV-205). With 30 minutes in the settings, a 45-minute-old file of the
account goes, a crash report as old does not: the helper keeps its floor of
an hour, without an error (INV-206).
"""

from __future__ import annotations

import json
import re
import shlex

from okapivm import cleaner, common
from okapivm.bench import Bench

from scenarios import _admin

TAGS = ["invariants", "admin", "helper", "cleaner"]

SETTINGS_MIN_AGE_MINUTES = 30
YOUNG_SECONDS = 45 * 60
# Planted before the young ones: old by the default hour and the helper's floor.
OLD_EXTRA_SECONDS = _admin.AGING_SECONDS - YOUNG_SECONDS
SIZE = 16 * 1024
RECENT = "recent"
REMOVED = "removed"
KEPT = "kept"
RECENT_LINE = "Gardées : modifiées trop récemment"
BELOW_FLOOR = "ageBelowFloor"
# What crates/app/src/elevation.rs and the engine's walk trace.
HELPER_TRACE = "helper trace"
HELD_BACK = "held back"
RECENT_HOLD = "Recent"

THUMBNAILS = "~/.cache/thumbnails"
REJUVENATED = f"{THUMBNAILS}/okapi-vm-rejuvenated.png"
OLD = f"{THUMBNAILS}/okapi-vm-old.png"
LARGE = f"{THUMBNAILS}/large"
DEEP = f"{LARGE}/a/b/c/deep.png"
SIBLING = f"{LARGE}/sibling.png"
TOUCHED = f"{THUMBNAILS}/okapi-vm-touched.png"
LOCKED = f"{THUMBNAILS}/fail"
LOCKED_FILES = [f"{LOCKED}/x.png", f"{LOCKED}/y.png"]
LOCK = f"{LOCKED}/okapi-vm-lock"
LINKED = f"{THUMBNAILS}/okapi-vm-linked-1.png"
LINK = f"{THUMBNAILS}/okapi-vm-linked-2.png"
YOUNG = f"{THUMBNAILS}/okapi-vm-45-minutes.png"

CRASH = "/var/crash"
CRASH_REJUVENATED = f"{CRASH}/okapi-vm-rejuvenated.crash"
CRASH_OLD = f"{CRASH}/okapi-vm-old.crash"
CRASH_TREE = f"{CRASH}/okapi-vm-tree"
CRASH_DEEP = f"{CRASH_TREE}/a/b/deep.crash"
CRASH_SIBLING = f"{CRASH_TREE}/sibling.crash"
CRASH_YOUNG = f"{CRASH}/okapi-vm-45-minutes.crash"


def run(bench: Bench) -> None:
    tester = bench.accounts.admin.name
    old = common.Files(tester, [REJUVENATED, OLD, DEEP, SIBLING, *LOCKED_FILES, LINKED], size=SIZE)
    crash_old = common.Files(common.ROOT, [CRASH_REJUVENATED, CRASH_OLD, CRASH_DEEP, CRASH_SIBLING], size=SIZE)
    young = common.Files(tester, [YOUNG], size=SIZE)
    crash_young = common.Files(common.ROOT, [CRASH_YOUNG], size=SIZE)
    touched = common.Files(tester, [TOUCHED], size=SIZE)

    bench.allow_elevation_without_password()
    common.write_settings(bench, {"minAgeMinutes": SETTINGS_MIN_AGE_MINUTES, "deletionLog": True})
    common.plant(bench, old, crash_old)
    # Before the clock moves: a new name changes the inode's date.
    bench.user(f"ln -- {common.shell_path(LINKED)} {common.shell_path(LINK)}")
    bench.advance_clock(OLD_EXTRA_SECONDS)
    common.plant(bench, young, crash_young, age_seconds=YOUNG_SECONDS)
    common.plant(bench, touched)
    bench.user(f"touch -d '2 years ago' -- {common.shell_path(TOUCHED)} && mkdir -- {common.shell_path(LOCK)}")

    cleaner.scan(bench)
    cleaner.obtain_rights(bench)
    cleaner.check_groups(bench, cleaner.CRASH_REPORTS)
    bench.screenshot("avant-attaque")

    bench.user(" && ".join(f"echo >> {common.shell_path(path)}" for path in (REJUVENATED, DEEP)))
    bench.root(" && ".join(f"echo >> {shlex.quote(path)}" for path in (CRASH_REJUVENATED, CRASH_DEEP)))
    bench.log("rejuvenated a file and a deep descendant, of the user's and of root's, after the scan")

    cleaner.clean(bench)
    bench.screenshot("recu")
    deletions = {entry["path"]: entry for entry in common.last_deletions(bench) if entry.get("path")}

    def outcome(path: str) -> dict | None:
        return deletions.get(common.expand_home(bench, path))

    rejuvenated = outcome(REJUVENATED)
    bench.check(kept(bench, tester, REJUVENATED) and rejuvenated is not None
                and (rejuvenated["result"], rejuvenated.get("reason")) == (KEPT, RECENT),
                f"INV-201: the user's file written to after the scan stays, kept as recent ({rejuvenated})")
    bench.check(not kept(bench, tester, OLD), "INV-201: its old neighbour goes")
    bench.check(kept(bench, common.ROOT, CRASH_REJUVENATED) and not kept(bench, common.ROOT, CRASH_OLD),
                "INV-201: root's crash report written to after the rights stays, its old neighbour goes")
    bench.check(bool(bench.ui.find(pattern=re.escape(RECENT_LINE), role="button")), f"INV-201: the receipt says « {RECENT_LINE} »")

    large = outcome(LARGE)
    bench.check(kept(bench, tester, DEEP) and kept(bench, tester, SIBLING) and large is not None
                and (large["result"], large.get("reason")) == (KEPT, RECENT),
                f"INV-203: a deep descendant written to after the scan keeps its folder whole, as recent ({large})")
    # The helper scans again at the cleaning: the folder is then no longer one
    # entry, its old files are offered one by one and the young one is held back.
    held_back = {"message": HELD_BACK, "path": json.dumps(CRASH_DEEP), "reason": RECENT_HOLD}
    bench.check(kept(bench, common.ROOT, CRASH_DEEP)
                and any(held_back.items() <= fields.items() for fields in helper_traces(bench)),
                "INV-203: root's deep descendant written to after the rights stays, held back as recent by the helper")

    bench.check(kept(bench, tester, TOUCHED) and outcome(TOUCHED) is None,
                "INV-202: a new file dated back by `touch -d` is never offered: its inode date is now")

    removed_locked = [outcome(path) for path in LOCKED_FILES]
    bench.check(all(found is not None and found["result"] == REMOVED for found in removed_locked)
                and not any(kept(bench, tester, path) for path in LOCKED_FILES),
                f"INV-204: the old files of a folder changed at the scan are offered and go ({removed_locked})")
    bench.check(kept(bench, tester, LOCK) and outcome(LOCKED) is None,
                "INV-204: the folder changed at the scan is not offered and stays, its lock in it")

    links = [outcome(path) for path in (LINKED, LINK)]
    bench.check(all(found is not None and found["result"] == REMOVED for found in links)
                and not kept(bench, tester, LINKED) and not kept(bench, tester, LINK),
                f"INV-205: both names of one old inode go, the first removed not making the second recent ({links})")

    receipt = common.last_receipt(bench)
    journal = common.journal_text(bench)
    bench.check(not kept(bench, tester, YOUNG), "INV-206: the account's 45-minute-old file goes, 30 minutes set")
    bench.check(kept(bench, common.ROOT, CRASH_YOUNG) and receipt["admin"]["done"] and BELOW_FLOOR not in journal,
                f"INV-206: root's 45-minute-old crash report stays under the helper's floor of an hour, "
                f"its part done without a refusal (admin {receipt['admin']})")


def helper_traces(bench: Bench) -> list[dict]:
    """The fields of each trace of the helper, as the interface's journal carries them."""
    return [json.loads(event["fields"]["line"]).get("fields", {}) for event in common.journal(bench)
            if event.get("fields", {}).get("message") == HELPER_TRACE]


def kept(bench: Bench, account: str, path: str) -> bool:
    return bool(common.present(bench, common.Files(account, [path])))

