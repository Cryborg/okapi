"""Cleaner: scan, administrator rights through pkexec, cleaning, receipt.

Poses old files where a rule of the account and two administrator rules look
(thumbnails, crash reports, APT's cache), lets polkit run the helper without a
password, then drives the whole cleaning by the accessibility bus and checks
that the files are gone and the receipt recorded.
"""

from __future__ import annotations

from okapivm import common
from okapivm.bench import Bench

TAGS = ["cleaner", "admin", "helper"]

# Beyond the default minimum age and the floor the helper imposes on any
# settings (`HELPER_MIN_AGE_FLOOR`), both an hour.
AGING_SECONDS = 2 * 60 * 60
SCAN_TIMEOUT_SECONDS = 300
CLEAN_TIMEOUT_SECONDS = 300

ADMIN_GROUPS = ["Rapports de plantage", "Cache des paquets APT"]
UNDERSTOOD = "J'ai compris que c'est définitif"


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
    common.plant(bench, account_files, admin_files, age_seconds=AGING_SECONDS)

    bench.launch_app()
    bench.screenshot("accueil")
    common.open_screen(bench, "Nettoyeur")
    bench.ui.wait("Analyser", role="button")
    bench.screenshot("nettoyeur")

    bench.ui.click("Analyser", role="button")
    bench.ui.wait("Résultats", timeout=SCAN_TIMEOUT_SECONDS)
    bench.screenshot("resultats")

    bench.ui.click("Obtenir les droits", role="button")
    bench.ui.wait("Droits obtenus", timeout=SCAN_TIMEOUT_SECONDS)
    # Groups found with the rights are offered unchecked.
    for group in ADMIN_GROUPS:
        bench.ui.click(group, role="check-box")
        bench.ui.wait(group, role="check-box", state="checked")
    bench.screenshot("droits-obtenus")

    bench.ui.click("Nettoyer", role="button")
    bench.ui.wait("Supprimer définitivement ?")
    bench.screenshot("confirmation")
    bench.ui.click(UNDERSTOOD, role="check-box")
    bench.ui.wait(UNDERSTOOD, role="check-box", state="checked")
    bench.ui.click("Supprimer définitivement", role="button")
    bench.ui.wait("Nettoyage terminé", timeout=CLEAN_TIMEOUT_SECONDS)
    bench.ui.wait("Le reçu est enregistré dans l'historique.")
    bench.screenshot("recu")

    common.check_gone(bench, account_files, admin_files)
    receipt = common.last_receipt(bench)
    planted = len(account_files.paths) + len(admin_files.paths)
    bench.check(receipt["removed"] >= planted and receipt["admin"]["done"],
                f"the receipt counts the {planted} files and the administrator's part as done")
