"""Cleaner: scan, administrator rights through pkexec, cleaning, receipt.

Poses old files where a rule of the account and two administrator rules look
(thumbnails, crash reports, APT's cache), lets polkit run the helper without a
password, then drives the whole cleaning by the accessibility bus and checks
that the files are gone and the receipt recorded.
"""

from __future__ import annotations

from collections.abc import Callable

from okapivm.bench import Bench

# Beyond the default minimum age and the floor the helper imposes on any
# settings (`HELPER_MIN_AGE_FLOOR`), both an hour.
AGING_SECONDS = 2 * 60 * 60
SCAN_TIMEOUT_SECONDS = 300
CLEAN_TIMEOUT_SECONDS = 300
ACCOUNT_FILE_BYTES = 64 * 1024
ADMIN_FILE_BYTES = 1024 * 1024

# Paths without spaces nor quotes: they go to the shell as they are, `~` expanded.
ACCOUNT_DIRECTORY = "~/.cache/thumbnails/normal"
ACCOUNT_FILES = [f"{ACCOUNT_DIRECTORY}/okapi-vm-1.png", f"{ACCOUNT_DIRECTORY}/okapi-vm-2.png"]
ADMIN_FILES = ["/var/crash/okapi-vm.crash", "/var/cache/apt/archives/okapi-vm_1.0_all.deb"]
ADMIN_GROUPS = ["Rapports de plantage", "Cache des paquets APT"]
UNDERSTOOD = "J'ai compris que c'est définitif"


def plant(bench: Bench) -> None:
    bench.user(f"mkdir -p {ACCOUNT_DIRECTORY}")
    for path in ACCOUNT_FILES:
        bench.user(f"head -c {ACCOUNT_FILE_BYTES} /dev/urandom > {path}")
    for path in ADMIN_FILES:
        bench.root(f"head -c {ADMIN_FILE_BYTES} /dev/urandom > {path}")
    bench.log(f"planted {len(ACCOUNT_FILES)} account files and {len(ADMIN_FILES)} administrator files")


def present(run: Callable[[str], str], paths: list[str]) -> list[str]:
    """Those of `paths` still there; a failed command fails, it never reads as « gone »."""
    return run(f'for path in {" ".join(paths)}; do if [ -e "$path" ]; then echo "$path"; fi; done').split()


def run(bench: Bench) -> None:
    bench.allow_elevation_without_password()
    plant(bench)
    bench.advance_clock(AGING_SECONDS)

    bench.launch_app()
    bench.screenshot("accueil")
    bench.ui.click("Nettoyeur")
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

    left = [*present(bench.user, ACCOUNT_FILES), *present(bench.root, ADMIN_FILES)]
    bench.check(not left, f"the planted files are gone (left: {left})")
