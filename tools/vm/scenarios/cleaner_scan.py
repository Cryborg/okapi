"""Cleaner: an analysis that finds nothing, one followed while it runs, one cancelled.

« Rien à nettoyer » on a fresh machine with « Jeux » alone, then its variant
with remarks when Steam's logs cannot be read; a large thumbnail cache whose
analysis shows its progress by category and what it found so far; a second
one cancelled, back to the categories with nothing removed.
"""

from __future__ import annotations

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["cleaner"]

NOTHING = "Rien à nettoyer"
NOTHING_SAID = ("Okapi a regardé les catégories choisies et n'a rien trouvé de récupérable. Les fichiers récents "
                "et vos exclusions ont été laissés de côté, comme toujours.")
NOTHING_READABLE_SAID = ("Okapi n'a rien trouvé de récupérable dans ce qu'il a pu lire. Les fichiers récents et vos "
                         "exclusions ont été laissés de côté, comme toujours.")
REMARKS_PATTERN = r"^Remarques de l'analyse : [1-9]\d*\. Le détail est dans le journal\.$"
OTHER_CATEGORIES = "Choisir d'autres catégories"
STEAM_LOGS = "~/.local/share/Steam/logs"
SCANNING = "Analyse en cours"
CANCEL = "Annuler l'analyse"
DONE_PATTERN = r"^Catégories terminées : \d+ sur \d+\.$"
FOUND_SO_FAR = "Trouvé jusqu'ici"
PER_CATEGORY = "Progression par catégorie"
CATEGORY_STATES = ("En attente", "En cours", "Terminé")
# Enough files for the analysis to last the time of a look, and of a click.
BIG_CACHE = "~/.cache/thumbnails/okapi-vm-big"
BIG_CACHE_FILES = 400_000
AGING_SECONDS = 2 * 60 * 60
LAST_ANALYSIS = "last-analysis.json"
# What crates/app/src/cleaner.rs traces when the user cancels, then when the analysis stops.
CANCELLED_EVENTS = {"analysis cancelled by the user", "analysis cancelled: back to choosing"}


def nothing_to_clean(bench: Bench) -> None:
    cleaner.choose(bench, "Jeux")
    title = cleaner.analyze(bench)
    bench.ui.wait(NOTHING_SAID)
    scan = cleaner.last_scan(bench)
    bench.check(title == NOTHING and scan["entries"] == 0 and scan["issues"] == 0,
                f"CLN-008: « {NOTHING} » for « Jeux » on a fresh machine, nothing counted ({scan})")
    bench.screenshot("rien-a-nettoyer")
    bench.ui.click(OTHER_CATEGORIES, role=cleaner.BUTTON)
    bench.ui.wait(cleaner.ANALYZE, role=cleaner.BUTTON)

    bench.user(f"mkdir -p -- {common.shell_path(STEAM_LOGS)} && chmod 000 -- {common.shell_path(STEAM_LOGS)}")
    title = cleaner.analyze(bench)
    bench.ui.wait(NOTHING_READABLE_SAID)
    remarks = bench.ui.find(pattern=REMARKS_PATTERN)
    scan = cleaner.last_scan(bench)
    bench.check(title == NOTHING and bool(remarks) and scan["entries"] == 0 and scan["issues"] >= 1,
                f"CLN-008: « {NOTHING} » with its remarks when Steam's logs are unreadable ({remarks}, {scan})")
    bench.screenshot("rien-a-nettoyer-remarques")
    bench.user(f"chmod 755 -- {common.shell_path(STEAM_LOGS)}")
    bench.ui.click(OTHER_CATEGORIES, role=cleaner.BUTTON)
    bench.ui.wait(cleaner.ANALYZE, role=cleaner.BUTTON)


def plant_big_cache(bench: Bench) -> None:
    directory = common.shell_path(BIG_CACHE)
    bench.user(f"mkdir -p -- {directory} && cd -- {directory} && seq {BIG_CACHE_FILES} | xargs touch")
    bench.advance_clock(AGING_SECONDS)
    bench.log(f"planted {BIG_CACHE_FILES} files in {BIG_CACHE}")


def progress(bench: Bench) -> None:
    cleaner.choose(bench, *cleaner.CATEGORIES[:4])
    bench.ui.click(cleaner.ANALYZE, role=cleaner.BUTTON)
    bench.ui.wait(SCANNING)
    bench.screenshot("analyse-en-cours")
    rows = bench.ui.find(role="list-item")
    states = {row["name"]: row.get("description", "") for row in rows}
    seen = {state for description in states.values() for state in CATEGORY_STATES if description.startswith(state)}
    bench.check(bool(bench.ui.find(pattern=DONE_PATTERN)) and bool(bench.ui.find(FOUND_SO_FAR))
                and bool(bench.ui.find(PER_CATEGORY)) and "Système" in states and bool(seen),
                f"CLN-006: « {SCANNING} », categories done, a line per category ({states}), « {FOUND_SO_FAR} »")
    bench.ui.wait(cleaner.RESULTS, timeout=cleaner.SCAN_TIMEOUT_SECONDS)
    summary = common.read_store(bench, LAST_ANALYSIS)
    assert isinstance(summary, dict)
    bench.check(summary["entries"] >= 1, f"CLN-006: the analysis followed ends with its summary written ({summary})")


def cancelled(bench: Bench) -> None:
    before = common.read_store(bench, LAST_ANALYSIS)
    bench.ui.click("Retour au choix des catégories", role=cleaner.BUTTON)
    bench.ui.click(cleaner.ANALYZE, role=cleaner.BUTTON)
    bench.ui.wait(SCANNING)
    bench.ui.click(CANCEL, role=cleaner.BUTTON)
    bench.screenshot("annulation")
    bench.ui.wait(cleaner.ANALYZE, role=cleaner.BUTTON, timeout=cleaner.SCAN_TIMEOUT_SECONDS)
    count = int(bench.user(f"find {common.shell_path(BIG_CACHE)} -type f | wc -l").strip())
    messages = {event.get("fields", {}).get("message") for event in common.journal(bench)}
    bench.check(count == BIG_CACHE_FILES and common.read_store(bench, LAST_ANALYSIS) == before
                and CANCELLED_EVENTS <= messages,
                f"CLN-007: cancelled, back to the categories, the {count} files there, no summary written, "
                "the journal tells the cancellation")


def run(bench: Bench) -> None:
    bench.launch_app()
    cleaner.open_page(bench)
    nothing_to_clean(bench)
    plant_big_cache(bench)
    progress(bench)
    cancelled(bench)
