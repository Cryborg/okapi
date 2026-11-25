"""What the scenarios of the administrator's rights share: the cleaner's scan,
the request for the rights, the banner that says where it stands, the rows of
the results and their cleaning."""

from __future__ import annotations

import re
import time

from okapivm import common
from okapivm.bench import Bench, ScenarioFailure

# Beyond the default minimum age and the floor the helper imposes on any
# settings (`HELPER_MIN_AGE_FLOOR`), both an hour.
AGING_SECONDS = 2 * 60 * 60
SCAN_TIMEOUT_SECONDS = 300
CLEAN_TIMEOUT_SECONDS = 300
RIGHTS_TIMEOUT_SECONDS = 120
LIST_TOP_STEPS = 50
LIST_PAGE_STEPS = 3
LIST_PAGES = 40
LIST_SETTLE_SECONDS = 0.5

RESULTS = "Éléments trouvés"
OBTAIN = "Obtenir les droits"
OBTAINED = "Droits obtenus"
CANCEL = "Annuler"
UNDERSTOOD = "J'ai compris que c'est définitif"
CLEANED = "Nettoyage terminé"
ADMINISTRATOR = "Administrateur"
CHECKED = "checked"
CRASH_REPORTS = "Rapports de plantage"
APT_LOGS = "Journaux d'APT"
CORE_DUMPS = "Vidages mémoire"
JOURNAL_LOGS = "Journaux de systemd"
# The banner of the rights (`results.slint`), a label: where the request
# stands, or how many groups wait for the rights.
STILL_WAITING = "Groupes qui les demandent toujours : "
BANNER_PATTERN = ("Groupes qui|Préparation de la demande|Saisissez le mot de passe|Analyse avec les droits"
                  "|Annulation de la demande")
WAITING_COUNT = re.compile(r"(?:administrateur|toujours) : (\d+)\.$")
HELPER_ACCEPTED = "request accepted"


def scan(bench: Bench) -> None:
    """A fresh interface's scan, its results shown."""
    bench.launch_app()
    common.open_screen(bench, "Nettoyeur")
    bench.ui.click("Analyser", role="button")
    bench.ui.wait("Résultats", timeout=SCAN_TIMEOUT_SECONDS)


def banner(bench: Bench) -> str:
    """What the banner of the rights says now."""
    return bench.ui.node(role="label", pattern=BANNER_PATTERN)["name"]


def wait_banner(bench: Bench, pattern: str, timeout: float = RIGHTS_TIMEOUT_SECONDS) -> str:
    """Waits for the banner of the rights to match `pattern` and gives what it says."""
    return bench.ui.wait(role="label", pattern=pattern, timeout=timeout)[0]["name"]


def waiting(said: str) -> int:
    """How many groups the banner `said` tells wait for the rights; 0 when it tells none."""
    found = WAITING_COUNT.search(said)
    return int(found.group(1)) if found else 0


def obtain_rights(bench: Bench) -> None:
    """Asks for the rights, polkit answering without a password, and waits for them."""
    bench.ui.click(OBTAIN, role="button")
    bench.ui.wait(OBTAINED, timeout=SCAN_TIMEOUT_SECONDS)


def row(bench: Bench, group: str) -> dict:
    """The checkbox of a group of the results, scrolled into view."""
    return bench.ui.reveal(group, role="check-box", within=RESULTS)


def rows(bench: Bench) -> dict[str, dict]:
    """Every checkbox of the results by its name, the list scrolled from its top to its end."""
    place = bench.ui.center(bench.ui.node(RESULTS, role="list-box"))
    bench.scroll_at(*place, -LIST_TOP_STEPS)
    seen: dict[str, dict] = {}
    for _ in range(LIST_PAGES):
        shown = {node["name"]: node for node in bench.ui.find(role="check-box")}
        if seen and shown.keys() <= seen.keys():
            return seen
        seen.update(shown)
        bench.scroll_at(*place, LIST_PAGE_STEPS)
        time.sleep(LIST_SETTLE_SECONDS)
    raise ScenarioFailure(f"the results never ended after {LIST_PAGES} pages")


def check_groups(bench: Bench, *groups: str) -> None:
    for group in groups:
        row(bench, group)
        bench.ui.click(group, role="check-box")
        bench.ui.wait(group, role="check-box", state=CHECKED)


def clean(bench: Bench, *, done: str = CLEANED) -> None:
    """Cleans what is checked, the confirmation accepted, until a text matching `done` shows."""
    bench.ui.click("Nettoyer", role="button")
    bench.ui.wait("Supprimer définitivement ?")
    bench.ui.click(UNDERSTOOD, role="check-box")
    bench.ui.wait(UNDERSTOOD, role="check-box", state=CHECKED)
    bench.ui.click("Supprimer définitivement", role="button")
    bench.ui.wait(pattern=done, timeout=CLEAN_TIMEOUT_SECONDS)
