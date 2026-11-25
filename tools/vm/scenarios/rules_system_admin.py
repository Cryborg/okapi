"""Rules: the system's targets that need the administrator, cleaned through the helper.

An old file in each of `/var/crash`, APT's archives, Flatpak's repository
temporaries, systemd's core dumps, the installer's logs, the man pages'
cache and APT's logs: unlocked by the rights, checked, cleaned by the helper
(RGL-003). DNF, Pacman and Zypper, absent from Ubuntu, show no line (RGL-004).
"""

from __future__ import annotations

from okapivm import cleaner, common
from okapivm.bench import Bench

from scenarios import _admin

TAGS = ["rules", "admin", "helper"]

SIZE = 64 * 1024
# Each group of the administrator and the file planted where its rule looks.
PLANTED = {
    cleaner.CRASH_REPORTS: "/var/crash/okapi-vm.crash",
    cleaner.APT_CACHE: "/var/cache/apt/archives/okapi-vm_1.0_all.deb",
    "Fichiers temporaires du dépôt Flatpak": "/var/lib/flatpak/repo/tmp/okapi-vm",
    cleaner.CORE_DUMPS: "/var/lib/systemd/coredump/core.okapi-vm.zst",
    "Journaux d'installation": "/var/log/installer/okapi-vm.log",
    "Cache des pages de manuel": "/var/cache/man/okapi-vm.gz",
    cleaner.APT_LOGS: "/var/log/apt/okapi-vm.log",
}
ABSENT = ["Cache des paquets DNF", "Cache des paquets Pacman", "Cache des paquets Zypper"]
ABSENT_RULES = ["DNF Package Cache", "Pacman Package Cache", "Zypper Package Cache"]


def run(bench: Bench) -> None:
    files = common.Files(common.ROOT, list(PLANTED.values()), size=SIZE)
    bench.allow_elevation_without_password()
    common.plant(bench, files, age_seconds=_admin.AGING_SECONDS)

    cleaner.scan(bench)
    cleaner.obtain_rights(bench)
    shown = cleaner.rows(bench)
    bench.screenshot("droits-obtenus")
    missing = [group for group in PLANTED if group not in shown]
    bench.check(not missing, f"RGL-003: each administrator's target has its group ({missing} missing)")
    unchecked = {group: cleaner.CHECKED not in shown[group]["states"] for group in PLANTED}
    bench.check(all(unchecked.values()), f"RGL-003: the administrator's groups are offered unchecked ({unchecked})")
    present = [group for group in ABSENT if group in shown]
    roots = bench.root("ls -d /var/cache/dnf /var/cache/pacman /var/cache/zypp 2>/dev/null || true").split()
    bench.check(not present and not roots,
                f"RGL-004: DNF, Pacman and Zypper, absent from the machine ({roots}), show no line ({present})")

    cleaner.check_groups(bench, *PLANTED)
    cleaner.clean(bench)
    bench.screenshot("recu")
    common.check_gone(bench, files)
    receipt = common.last_receipt(bench)
    bench.check(receipt["removed"] >= len(PLANTED) and receipt["admin"]["done"],
                f"RGL-003: the helper removed the {len(PLANTED)} files planted, its part done ({receipt})")
    traced = common.journal_text(bench)
    bench.check(not any(rule in traced for rule in ABSENT_RULES),
                f"RGL-004: no trace names {ABSENT_RULES}")
