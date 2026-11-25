"""Cleaner: the confirmation of a cleaning, before anything is removed.

Its size and count are those selected, its box « J'ai compris » unchecked at
each opening, « Supprimer définitivement » inert without it; « Annuler » and
Escape remove nothing; a sentence is added for the trash, the caches
recreated, Docker and the password; a double click on « Nettoyer » does not
go through the confirmation.
"""

from __future__ import annotations

import re
import time

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["cleaner", "admin"]

# Beyond the 30 days the trash rule asks of each entry, its change time included.
AGING_SECONDS = 31 * 24 * 60 * 60
FILE_SIZE = 64 * 1024
ACCOUNT_FILES = [
    "~/.cache/thumbnails/normal/okapi-vm.png",
    "~/.config/google-chrome/Default/GPUCache/okapi-vm",
    "~/.config/google-chrome/Default/Preferences",
    "~/.local/share/Trash/files/okapi-vm-old.txt",
]
TRASH_INFO = "~/.local/share/Trash/info/okapi-vm-old.txt.trashinfo"
# Long before the 30 days the trash rule asks for.
TRASH_INFO_TEXT = "[Trash Info]\\nPath=/home/okapi-vm-old.txt\\nDeletionDate=2020-01-01T00:00:00\\n"
ADMIN_FILES = ["/var/crash/okapi-vm.crash"]
THUMBNAILS = "Cache des miniatures"
TRASH = "Corbeille"
GPU_CACHE = "Cache GPU"
CHROME = "Chrome"
DOCKER = "Cache de construction Docker"
DOCKER_SOCKET = "/run/docker.sock"
CRASHES = "Rapports de plantage"
RIGHTS = "Obtenir les droits"
RIGHTS_GIVEN = "Droits obtenus"
CANCEL = "Annuler"
SELECTED_PATTERN = r"^Éléments : (\d+) sur \d+$"
SAID_PATTERN = (r"^Taille : (.+)\. Éléments : (\d+)\. Rien ne passe par la corbeille : cette action est "
                r"irréversible\.$")
SENTENCES = {
    TRASH: "Ce qui part de la corbeille ne pourra plus être restauré.",
    GPU_CACHE: ("Les caches retirés seront recréés : ce qui s'en sert démarrera un peu plus lentement la "
                "prochaine fois."),
    DOCKER: "Docker élaguera son cache de construction : ce qui part n'est connu qu'après.",
    CRASHES: ("Une fenêtre du système peut vous demander votre mot de passe pour les éléments administrateur. Ils "
              "sont mesurés de nouveau juste avant d'être retirés : leur nombre a pu changer depuis l'obtention "
              "des droits."),
}
PLACES = {TRASH: ["Système"], CRASHES: ["Système"], GPU_CACHE: ["Navigateurs", CHROME], DOCKER: ["Applications"]}
# What crates/app/src/cleaner.rs traces for a click within the settle delay.
IGNORED = "second click of a double click ignored"
DELETION_ASKED = "deletion"
CANCELLATION_ASKED = "cancellation of the confirmation"
# Quicker than the settle delay (500 ms) between the clicks of a double click.
DOUBLE_CLICK_SECONDS = 0.05
# Time for the confirmation to show, still within the settle delay.
OPENING_SECONDS = 0.2


def prepare(bench: Bench) -> tuple[common.Files, common.Files]:
    mine = common.Files(bench.accounts.admin.name, ACCOUNT_FILES, size=FILE_SIZE)
    admin = common.Files(common.ROOT, ADMIN_FILES, size=FILE_SIZE)
    common.plant(bench, mine, admin)
    info = common.shell_path(TRASH_INFO)
    bench.user(f"mkdir -p -- \"$(dirname -- {info})\" && printf '{TRASH_INFO_TEXT}' > {info}")
    bench.advance_clock(AGING_SECONDS)
    return mine, admin


def open_confirmation(bench: Bench) -> dict:
    bench.ui.click(cleaner.CLEAN, role=cleaner.BUTTON)
    bench.ui.wait(cleaner.CONFIRMATION)
    return bench.ui.node(cleaner.UNDERSTOOD, role=cleaner.CHECK_BOX)


def cancel(bench: Bench) -> None:
    bench.ui.click(CANCEL, role=cleaner.BUTTON)
    bench.ui.wait(cleaner.CONFIRMATION, gone=True)


def amount_and_box(bench: Bench, kept: common.Files) -> None:
    selected = int(re.match(SELECTED_PATTERN, bench.ui.node(pattern=SELECTED_PATTERN)["name"]).group(1))
    understood = open_confirmation(bench)
    said = bench.ui.node(pattern=SAID_PATTERN)["name"]
    entries = int(re.match(SAID_PATTERN, said).group(2))
    bench.screenshot("confirmation")
    # AT-SPI calls every button enabled: pressed without the box, it must remove nothing.
    bench.ui.click(cleaner.DELETE, role=cleaner.BUTTON)
    still = bool(bench.ui.find(cleaner.CONFIRMATION))
    bench.ui.click(cleaner.UNDERSTOOD, role=cleaner.CHECK_BOX)
    cancel(bench)
    again = open_confirmation(bench)
    common.check_kept(bench, kept)
    bench.check(entries == selected and cleaner.CHECKED not in understood["states"] and still
                and cleaner.CHECKED not in again["states"],
                f"CLN-021: « {said} » counts the {selected} selected; « {cleaner.DELETE} » does nothing "
                "without the box, unchecked again at each opening")


def cancelled(bench: Bench, kept: common.Files) -> None:
    cancel(bench)
    open_confirmation(bench)
    bench.ui.click(cleaner.UNDERSTOOD, role=cleaner.CHECK_BOX)
    bench.key("Escape")
    bench.ui.wait(cleaner.CONFIRMATION, gone=True)
    common.check_kept(bench, kept)
    bench.check(not common.read_store(bench, common.HISTORY),
                "CLN-022: « Annuler » and Escape close the confirmation, nothing removed, nothing in the history")


def sentences(bench: Bench) -> None:
    bench.allow_elevation_without_password()
    bench.ui.click(RIGHTS, role=cleaner.BUTTON)
    bench.ui.wait(RIGHTS_GIVEN, timeout=cleaner.SCAN_TIMEOUT_SECONDS)
    checked = {}
    for group, (category, *browsers) in PLACES.items():
        cleaner.show(bench, category, *browsers)
        cleaner.set_checked(bench, group, True)
        checked[group] = cleaner.is_checked(bench, group)
    open_confirmation(bench)
    bench.screenshot("confirmation-phrases")
    said = {group: bool(bench.ui.find(sentence)) for group, sentence in SENTENCES.items()}
    cancel(bench)
    bench.check(all(said[group] == checked[group] for group in SENTENCES) and all(said.values()),
                f"CLN-023: a sentence for the trash, the caches recreated, Docker and the password "
                f"(checked {checked}, said {said})")


def ignored_since(bench: Bench, seen: int) -> set[str]:
    return {event["fields"].get("asked") for event in common.journal(bench)[seen:]
            if event.get("fields", {}).get("message") == IGNORED}


def double_click(bench: Bench, *, then: str, kept: common.Files) -> None:
    """Double clicks « Nettoyer », then ticks the box and clicks `then` at
    once: within the settle delay, ignored. A round trip of the driver takes
    longer than that delay: the clicks are mouse clicks."""
    nettoyer = bench.ui.center(bench.ui.node(cleaner.CLEAN, role=cleaner.BUTTON))
    open_confirmation(bench)
    box = bench.ui.center(bench.ui.node(cleaner.UNDERSTOOD, role=cleaner.CHECK_BOX))
    target = bench.ui.center(bench.ui.node(then, role=cleaner.BUTTON))
    bench.click_at(*box)
    bench.check(cleaner.is_checked(bench, cleaner.UNDERSTOOD),
                f"a mouse click at {box} ticks the box of the confirmation")
    cancel(bench)
    time.sleep(bench.click_settle)
    seen = len(common.journal(bench))
    for pixel, pause in [(nettoyer, DOUBLE_CLICK_SECONDS), (nettoyer, OPENING_SECONDS), (box, DOUBLE_CLICK_SECONDS),
                         (target, 0)]:
        bench.machine.click_at(*pixel)
        time.sleep(pause)
    open_still = bool(bench.ui.find(cleaner.CONFIRMATION))
    asked = ignored_since(bench, seen)
    bench.screenshot(f"double-clic-{'suppression' if then == cleaner.DELETE else 'annulation'}")
    common.check_kept(bench, kept)
    expected = DELETION_ASKED if then == cleaner.DELETE else CANCELLATION_ASKED
    bench.check(open_still and expected in asked,
                f"CLN-024: a double click on « {cleaner.CLEAN} » then « {then} » at once: the confirmation "
                f"stays open, nothing removed, the click ignored ({asked})")
    cancel(bench)


def run(bench: Bench) -> None:
    # Docker answers only its group; the user's systemd, which starts Okapi,
    # would know of a group added only once restarted.
    bench.root(f"chmod 666 -- {DOCKER_SOCKET}")
    mine, admin = prepare(bench)
    bench.launch_app()
    cleaner.open_page(bench)
    cleaner.analyze(bench)
    amount_and_box(bench, mine)
    cancelled(bench, mine)
    sentences(bench)
    double_click(bench, then=cleaner.DELETE, kept=mine)
    double_click(bench, then=CANCEL, kept=mine)
    common.check_kept(bench, admin)
