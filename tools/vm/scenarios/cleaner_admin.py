"""Cleaner: scan, administrator rights through pkexec, cleaning, receipt.

Poses old files where a rule of the account and two administrator rules look
(thumbnails, crash reports, APT's cache), lets polkit run the helper without a
password, then drives the whole cleaning by the accessibility bus and checks
that the files are gone and the receipt recorded (ADM-001).
"""

from __future__ import annotations

from okapivm import common
from okapivm.bench import Bench

from scenarios import _admin

TAGS = ["cleaner", "admin", "helper"]

ADMIN_GROUPS = [_admin.CRASH_REPORTS, "Cache des paquets APT"]
RECORDED = "Le reçu est enregistré dans l'historique."


def run(bench: Bench) -> None:
    account_files = common.Files(
        bench.accounts.admin.name,
        ["~/.cache/thumbnails/normal/okapi-vm-1.png", "~/.cache/thumbnails/normal/okapi-vm-2.png"],
        size=64 * 1024,
    )
    admin_files = common.Files(
        common.ROOT, ["/var/crash/okapi-vm.crash", "/var/cache/apt/archives/okapi-vm_1.0_all.deb"], size=1024 * 1024,
    )
    bench.allow_elevation_without_password()
    common.plant(bench, account_files, admin_files, age_seconds=_admin.AGING_SECONDS)

    _admin.scan(bench)
    bench.screenshot("resultats")
    _admin.obtain_rights(bench)
    for group in ADMIN_GROUPS:
        offered = _admin.row(bench, group)
        bench.check(_admin.CHECKED not in offered["states"] and _admin.OBTAINED in offered.get("description", ""),
                    f"ADM-001: « {group} », unlocked by the rights, is offered unchecked ({offered.get('description')!r})")
    _admin.check_groups(bench, *ADMIN_GROUPS)
    bench.screenshot("droits-obtenus")

    _admin.clean(bench)
    bench.ui.wait(RECORDED)
    bench.screenshot("recu")

    common.check_gone(bench, account_files, admin_files)
    receipt = common.last_receipt(bench)
    planted = len(account_files.paths) + len(admin_files.paths)
    bench.check(receipt["removed"] >= planted and receipt["admin"]["done"],
                f"ADM-001: the receipt counts the {planted} files and the administrator's part as done")
