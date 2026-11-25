"""Applications: a snap uninstalled by snapd, its data kept 31 days or removed with it.

Two local snaps with data in the account's `~/snap/<name>`: the first goes
with the box « Supprimer aussi ses données » unchecked, and snapd keeps a
copy (`snap saved`); the second with it checked, and no copy is left. Each
leaves its line in the history (APP-013).
"""

from __future__ import annotations

import shlex

from okapivm import apps, common, config
from okapivm.bench import Bench

TAGS = ["apps", "snap"]

DATA_BYTES = 1024 * 1024
CHECK_BOX = "check-box"
KEEP_COPY = "Une copie de ses données (de tous les comptes et du système) est gardée 31 jours par le système."
NO_COPY = "Ses données partent tout de suite, pour tous les comptes et celles du système, sans copie gardée."
REMOVE_DATA = r"^Supprimer aussi ses données"

KEPT = common.Snap("okapi-vm-kept")
PURGED = common.Snap("okapi-vm-purged")


def data(bench: Bench, snap: common.Snap) -> common.Files:
    """What the snap keeps in the account's home, as it would."""
    return common.Files(bench.accounts.admin.name, [f"~/snap/{snap.name}/common/okapi-vm-data.bin"],
                        size=DATA_BYTES)


def saved(bench: Bench, snap: common.Snap) -> bool:
    return snap.name in bench.root(f"snap saved {shlex.quote(snap.name)} 2>&1", check=False)


def uninstall(bench: Bench, snap: common.Snap, *, remove_data: bool) -> None:
    apps.ask_uninstall(bench, snap.name, apps.SNAP)
    box = bench.ui.node(pattern=REMOVE_DATA, role=CHECK_BOX)
    bench.check("checked" not in box["states"] and KEEP_COPY in apps.labels(bench),
                f"APP-013: the box is unchecked at first, the copy kept 31 days said ({box['name']!r})")
    if remove_data:
        bench.ui.click(pattern=REMOVE_DATA, role=CHECK_BOX)
        bench.check(NO_COPY in apps.labels(bench) and KEEP_COPY not in apps.labels(bench),
                    "APP-013: checked, the data are said to go at once, no copy")
    bench.screenshot(f"snap-{snap.name}")
    apps.confirm(bench)
    apps.wait_done(bench, snap.name)
    bench.check(not common.snap_installed(bench, snap.name), f"APP-013: snapd removed {snap.name}")
    line = apps.last_uninstall(bench) or {}
    bench.check(line.get("source") == "snap" and line.get("name") == snap.name
                and line.get("dataRemoved") is remove_data,
                f"APP-013: the history holds {snap.name}'s uninstallation, data removed {remove_data} ({line})")


def run(bench: Bench) -> None:
    bench.set_elevation(common.Elevation.WITHOUT_PASSWORD, config.UNINSTALL_ACTIONS)
    for snap in (KEPT, PURGED):
        common.build_snap(bench, snap)
        common.install_snap(bench, snap)
        common.plant(bench, data(bench, snap))

    bench.launch_app()
    apps.open_page(bench)
    uninstall(bench, KEPT, remove_data=False)
    bench.check(saved(bench, KEPT), f"APP-013: snapd keeps a copy of {KEPT.name}'s data (snap saved)")
    uninstall(bench, PURGED, remove_data=True)
    bench.check(not saved(bench, PURGED), f"APP-013: no copy of {PURGED.name}'s data is kept (snap saved)")
