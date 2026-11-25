"""Driving the Cleaner page: its categories, an analysis, the administrator's
rights, the boxes of its results, a cleaning confirmed. The names are the
interface's French texts."""

from __future__ import annotations

import time

from . import common
from .bench import Bench, ScenarioFailure

PAGE = "Nettoyeur"
CHECK_BOX = "check-box"
BUTTON = "button"
CHECKED = "checked"
ANALYZE = "Analyser"
RESULTS = "Résultats"
RESULTS_LIST = "Éléments trouvés"
OBTAIN = "Obtenir les droits"
OBTAINED = "Droits obtenus"
CLEAN = "Nettoyer"
CONFIRMATION = "Supprimer définitivement ?"
UNDERSTOOD = "J'ai compris que c'est définitif"
DELETE = "Supprimer définitivement"
DONE = "Nettoyage terminé"
RECORDED = "Le reçu est enregistré dans l'historique."
CRASH_REPORTS = "Rapports de plantage"
APT_LOGS = "Journaux d'APT"
APT_CACHE = "Cache des paquets APT"
CORE_DUMPS = "Vidages mémoire"
JOURNAL_LOGS = "Journaux de systemd"
FOLD = "Replier"
UNFOLD = "Déplier"
CATEGORIES = ["Système", "Navigateurs", "Applications", "Jeux", "Caches GPU", "Mes dossiers"]
# The titles an analysis ends on.
ANALYSIS_ENDS = ["Résultats", "Rien à nettoyer", "Rien n'a été regardé", "L'analyse a échoué"]
SCAN_TIMEOUT_SECONDS = 300
CLEAN_TIMEOUT_SECONDS = 300
LIST_TOP_STEPS = 50
LIST_PAGE_STEPS = 3
LIST_PAGES = 40
LIST_SETTLE_SECONDS = 0.5
# What crates/engine/src/scan.rs traces when an analysis ends.
SCAN_FINISHED = "scan finished"


def open_page(bench: Bench) -> None:
    common.open_screen(bench, PAGE)
    bench.ui.wait(ANALYZE, role=BUTTON)


def is_checked(bench: Bench, name: str, *, index: int | None = None) -> bool:
    return CHECKED in bench.ui.node(name, role=CHECK_BOX, index=index)["states"]


def set_checked(bench: Bench, name: str, checked: bool, *, index: int | None = None) -> None:
    """Checks or unchecks a box (a category, a group of the results) unless it already is."""
    if is_checked(bench, name, index=index) != checked:
        bench.ui.click(name, role=CHECK_BOX, index=index)


def choose(bench: Bench, *categories: str) -> None:
    """On the choosing page, `categories` checked and every other card offered unchecked."""
    offered = {node["name"] for node in bench.ui.find(role=CHECK_BOX)}
    for name in CATEGORIES:
        if name in offered:
            set_checked(bench, name, name in categories)


def fold(bench: Bench, name: str, folded: bool = True) -> None:
    """Folds (or unfolds) a category or a browser of the results unless it
    already is: the list shows only the rows on screen."""
    button = f"{FOLD if folded else UNFOLD} {name}"
    if bench.ui.find(button, role=BUTTON):
        bench.ui.click(button, role=BUTTON)
        bench.ui.wait(f"{UNFOLD if folded else FOLD} {name}", role=BUTTON)


def show(bench: Bench, category: str, *browsers: str) -> None:
    """The results of `category` alone unfolded, with those of `browsers`:
    the list holds only the rows on screen."""
    for name in CATEGORIES:
        if name != category:
            fold(bench, name)
    for name in [category, *browsers]:
        fold(bench, name, folded=False)


def analyze(bench: Bench) -> str:
    """Starts the analysis and gives the title it ends on (`ANALYSIS_ENDS`)."""
    bench.ui.click(ANALYZE, role=BUTTON)
    pattern = "^(" + "|".join(ANALYSIS_ENDS) + ")$"
    ended = bench.ui.wait(pattern=pattern, timeout=SCAN_TIMEOUT_SECONDS)
    title = ended[0]["name"]
    bench.log(f"analysis ended: {title}")
    return title


def scan(bench: Bench) -> None:
    """A fresh interface's analysis, until its results show."""
    bench.launch_app()
    open_page(bench)
    title = analyze(bench)
    if title != RESULTS:
        raise ScenarioFailure(f"the analysis ended on « {title} », not « {RESULTS} »")


def obtain_rights(bench: Bench) -> None:
    """Asks for the administrator's rights from the results, until root's analysis is in."""
    bench.ui.click(OBTAIN, role=BUTTON)
    bench.ui.wait(OBTAINED, timeout=SCAN_TIMEOUT_SECONDS)


def row(bench: Bench, group: str) -> dict:
    """The box of a group of the results, the list scrolled until it shows."""
    return bench.ui.reveal(group, role=CHECK_BOX, within=RESULTS_LIST)


def rows(bench: Bench) -> dict[str, dict]:
    """Every box of the results by its name, the list scrolled from its top to its end."""
    place = bench.ui.center(bench.ui.node(RESULTS_LIST, role="list-box"))
    bench.scroll_at(*place, -LIST_TOP_STEPS)
    seen: dict[str, dict] = {}
    for _ in range(LIST_PAGES):
        shown = {node["name"]: node for node in bench.ui.find(role=CHECK_BOX)}
        if seen and shown.keys() <= seen.keys():
            return seen
        seen.update(shown)
        bench.scroll_at(*place, LIST_PAGE_STEPS)
        time.sleep(LIST_SETTLE_SECONDS)
    raise ScenarioFailure(f"the results never ended after {LIST_PAGES} pages")


def check_groups(bench: Bench, *groups: str) -> None:
    """Checks the boxes of `groups` among the results (those of root are offered unchecked)."""
    for group in groups:
        row(bench, group)
        bench.ui.click(group, role=CHECK_BOX)
        bench.ui.wait(group, role=CHECK_BOX, state=CHECKED)


def confirm(bench: Bench) -> None:
    """« Nettoyer », the confirmation understood, then the deletion asked for."""
    bench.ui.click(CLEAN, role=BUTTON)
    bench.ui.wait(CONFIRMATION)
    bench.ui.click(UNDERSTOOD, role=CHECK_BOX)
    bench.ui.click(DELETE, role=BUTTON)


def clean(bench: Bench, *, recorded: bool = True) -> None:
    """A cleaning of what is checked, confirmed, to its end; `recorded`, to its
    receipt written in the history."""
    confirm(bench)
    bench.ui.wait(DONE, timeout=CLEAN_TIMEOUT_SECONDS)
    if recorded:
        bench.ui.wait(RECORDED)


def last_scan(bench: Bench) -> dict:
    """The fields the engine traced at the end of the last analysis
    (`entries`, `bytes`, `issues`...): unlike `last-analysis.json`, written
    for a partial analysis too."""
    finished = [event["fields"] for event in common.journal(bench)
                if event.get("fields", {}).get("message") == SCAN_FINISHED]
    if not finished:
        raise ScenarioFailure("no analysis finished in the journal")
    return finished[-1]
