"""Driving the Cleaner page: its categories, an analysis, the boxes of its
results, a cleaning confirmed. The names are the interface's French texts."""

from __future__ import annotations

from . import common
from .bench import Bench, ScenarioFailure

PAGE = "Nettoyeur"
CHECK_BOX = "check-box"
BUTTON = "button"
CHECKED = "checked"
ANALYZE = "Analyser"
RESULTS = "Résultats"
CLEAN = "Nettoyer"
CONFIRMATION = "Supprimer définitivement ?"
UNDERSTOOD = "J'ai compris que c'est définitif"
DELETE = "Supprimer définitivement"
DONE = "Nettoyage terminé"
FOLD = "Replier"
UNFOLD = "Déplier"
CATEGORIES = ["Système", "Navigateurs", "Applications", "Jeux", "Caches GPU", "Mes dossiers"]
# The titles an analysis ends on.
ANALYSIS_ENDS = ["Résultats", "Rien à nettoyer", "Rien n'a été regardé", "L'analyse a échoué"]
SCAN_TIMEOUT_SECONDS = 300
CLEAN_TIMEOUT_SECONDS = 300
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


def confirm(bench: Bench) -> None:
    """« Nettoyer », the confirmation understood, then the deletion asked for."""
    bench.ui.click(CLEAN, role=BUTTON)
    bench.ui.wait(CONFIRMATION)
    bench.ui.click(UNDERSTOOD, role=CHECK_BOX)
    bench.ui.click(DELETE, role=BUTTON)


def clean(bench: Bench) -> None:
    """A cleaning of what is checked, confirmed, to its receipt."""
    confirm(bench)
    bench.ui.wait(DONE, timeout=CLEAN_TIMEOUT_SECONDS)


def last_scan(bench: Bench) -> dict:
    """The fields the engine traced at the end of the last analysis
    (`entries`, `bytes`, `issues`...): unlike `last-analysis.json`, written
    for a partial analysis too."""
    finished = [event["fields"] for event in common.journal(bench)
                if event.get("fields", {}).get("message") == SCAN_FINISHED]
    if not finished:
        raise ScenarioFailure("no analysis finished in the journal")
    return finished[-1]
