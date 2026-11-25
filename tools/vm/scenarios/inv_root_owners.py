"""Invariant 1: root never empties a directory another user owns, and redoes its analysis.

`autre` leaves a tree of old files in `/var/crash`, open to all: its file
lying right there goes, its directory is never entered (INV-101), while an
old directory of root goes whole. A directory of root seen with the rights
is handed to `autre` before the cleaning: kept whole (INV-102). A file written again and a file planted
after the rights: the helper's own analysis at the cleaning keeps both, and
the history never names the second (INV-103).
"""

from __future__ import annotations

import shlex

from okapivm import common
from okapivm.bench import Bench

TAGS = ["invariants", "admin", "helper"]

# Beyond the floor the helper imposes on any settings (`HELPER_MIN_AGE_FLOOR`).
AGING_SECONDS = 2 * 60 * 60
SIZE = 4096
CRASH = "/var/crash"
CRASH_GROUP = "Rapports de plantage"


def owners(bench: Bench, *paths: str) -> list[str]:
    return bench.root(shlex.join(["stat", "--format=%U", *paths])).split()


def run(bench: Bench) -> None:
    other = bench.accounts.other.name
    other_tree = common.Files(other, [f"{CRASH}/okapi-vm-tree/a.crash", f"{CRASH}/okapi-vm-tree/sub/b.crash"], SIZE)
    other_loose = common.Files(other, [f"{CRASH}/okapi-vm-other.crash"], SIZE)
    handed_over = common.Files(common.ROOT, [f"{CRASH}/okapi-vm-handed/c.crash", f"{CRASH}/okapi-vm-handed/d.crash"],
                               SIZE)
    rewritten = common.Files(common.ROOT, [f"{CRASH}/okapi-vm-rewritten.crash"], SIZE)
    removable = common.Files(common.ROOT, [f"{CRASH}/okapi-vm-root.crash", f"{CRASH}/okapi-vm-root-tree/e.crash"], SIZE)
    planted_after = common.Files(common.ROOT, [f"{CRASH}/okapi-vm-after-rights.crash"], SIZE)
    bench.allow_elevation_without_password()
    common.write_settings(bench)
    common.plant(bench, other_tree, other_loose, handed_over, rewritten, removable, age_seconds=AGING_SECONDS)
    bench.check(owners(bench, f"{CRASH}/okapi-vm-tree", f"{CRASH}/okapi-vm-handed") == [other, common.ROOT],
                "INV-101, INV-102: the tree is autre's, the directory to hand over root's")

    bench.launch_app()
    common.analyse(bench)
    common.obtain_rights(bench)
    common.check_groups(bench, CRASH_GROUP)
    bench.screenshot("droits-obtenus")

    bench.root(shlex.join(["chown", "-R", f"{other}:{other}", f"{CRASH}/okapi-vm-handed"]))
    bench.root(f"echo okapi-vm >> {shlex.quote(rewritten.paths[0])}")
    common.plant(bench, planted_after)
    common.clean(bench)
    bench.screenshot("recu")

    receipt = common.last_receipt(bench)
    bench.check(bool(receipt.get("admin", {}).get("done")), f"the administrator's part ended as asked ({receipt})")
    root_tree = common.Files(common.ROOT, [f"{CRASH}/okapi-vm-root-tree"])
    bench.check(not common.present(bench, removable) and not common.present(bench, root_tree),
                "a file and a whole directory of root, old, are removed: what stays, stays for its owner or its age")
    bench.check(not common.present(bench, other_loose), "INV-101: autre's file lying in /var/crash is removed")
    bench.check(common.present(bench, other_tree) == list(other_tree.paths),
                "INV-101: autre's tree in /var/crash is left whole")
    bench.check(common.present(bench, handed_over) == list(handed_over.paths),
                "INV-102: the directory handed to autre after the rights is left whole")
    bench.check(common.present(bench, rewritten) == list(rewritten.paths),
                "INV-103: the file written again after the rights stays")
    bench.check(common.present(bench, planted_after) == list(planted_after.paths),
                "INV-103: the file planted after the rights stays")
    named = {entry.get("path") for entry in common.last_deletions(bench)}
    bench.log(f"deletion log: {sorted(map(str, named))}")
    bench.check(removable.paths[0] in named, "the deletion log names what root removed")
    bench.check(planted_after.paths[0] not in named, "INV-103: the history never names the file planted after the rights")
