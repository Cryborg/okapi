"""Cleaner: a cleaning followed, one cancelled, and the receipt of what went and what stayed.

A cleaning of a hundred thousand thumbnails shows its steps and what it freed
so far, then is cancelled: the entries left are those the receipt keeps as
« nettoyage annulé ». Then four thumbnails: one goes, one removed by hand
since the analysis is « Déjà parties », one touched since is kept as too
recent (invariant 2), one excluded since is kept as excluded; the receipt
says so, as does its line of `history.jsonl`; « Terminer » goes back to the
categories, and the home page and the history know about the cleaning.
"""

from __future__ import annotations

import re
import time

from okapivm import cleaner, common
from okapivm.bench import Bench, ScenarioFailure

TAGS = ["cleaner"]

AGING_SECONDS = 2 * 60 * 60
FILE_SIZE = 64 * 1024
THUMBNAILS = "~/.cache/thumbnails"
# Each file an entry of its own, fewer than a walk reads below a root (250 000).
MANY_FILES = 100_000
MANY_PREFIX = "okapi-vm-many-"
REMOVED = f"{THUMBNAILS}/okapi-vm-removed.png"
GONE = f"{THUMBNAILS}/okapi-vm-gone.png"
RECENT = f"{THUMBNAILS}/okapi-vm-recent.png"
EXCLUDED = f"{THUMBNAILS}/okapi-vm-excluded.png"
CLEANING = "Nettoyage en cours"
STEPS = "Étapes du nettoyage"
STEP_NAMES = ["Vérification de chaque élément", "Suppression des éléments vérifiés", "Enregistrement dans l'historique"]
IN_PROGRESS = "En cours"
FREED_SO_FAR = "Libéré jusqu'ici"
CANCEL_CLEANING = "Annuler le nettoyage"
CANCELLED = "Nettoyage annulé"
RECORDED = "Le reçu est enregistré dans l'historique."
FINISH = "Terminer"
STAT_PATTERN = r"^{} : (\d+)$"
STATS = {"removed": "Retirées", "partial": "Retirées en partie", "gone": "Déjà parties", "kept": "Gardées"}
FREED = "Espace libéré"
LINE = "Déplier {}"
LINE_COUNT_PATTERN = r"^Éléments : (\d+)\."
CANCELLED_LINE = "Gardées : nettoyage annulé avant leur tour"
RECENT_LINE = "Gardées : modifiées trop récemment"
EXCLUDED_LINE = "Gardées : exclues par vos réglages"
GONE_LINE = "Déjà parties"
KEPT = "kept"
CANCELLED_REASON = "cancelled"
REASONS = {REMOVED: ("removed", None), GONE: ("gone", None), RECENT: (KEPT, "recent"),
           EXCLUDED: (KEPT, "excluded")}
DELETIONS = "deletions"
HOME = "Accueil"
RECLAIMABLE = "Récupérable"
FREED_SINCE = "libérés"
HISTORY_PAGE = "Historique"
HISTORY_ITEM_PATTERN = r"^Nettoyage, .+ libérés"


def stat(bench: Bench, label: str) -> int:
    pattern = STAT_PATTERN.format(re.escape(label))
    return int(re.match(pattern, bench.ui.node(pattern=pattern)["name"]).group(1))


def line_count(bench: Bench, name: str) -> int:
    found = bench.ui.find(LINE.format(name), role=cleaner.BUTTON)
    if not found:
        return 0
    return int(re.match(LINE_COUNT_PATTERN, found[0].get("description", "")).group(1))


def deletions(bench: Bench) -> dict[str, dict]:
    """The entries of the deletion log written with the last cleaning, by path."""
    history = common.read_store(bench, common.HISTORY)
    assert isinstance(history, list)
    log = [line for line in history if line.get("kind") == DELETIONS][-1]
    return {entry["path"]: entry for entry in log["entries"]}


def finish(bench: Bench) -> None:
    bench.ui.click(FINISH, role=cleaner.BUTTON)
    bench.ui.wait(cleaner.ANALYZE, role=cleaner.BUTTON)


def deleting(bench: Bench) -> dict[str, str]:
    """The steps of the cleaning once its deletions have begun: a cancellation
    then leaves some entries removed, some kept."""
    deadline = time.monotonic() + cleaner.CLEAN_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        steps = {row["name"]: row.get("description", "") for row in bench.ui.find(role="list-item")}
        if steps.get(STEP_NAMES[1], "").startswith(IN_PROGRESS):
            return steps
    raise ScenarioFailure(f"the deletions never began ({steps})")


def cancelled_cleaning(bench: Bench) -> None:
    directory = common.shell_path(THUMBNAILS)
    bench.user(f"mkdir -p -- {directory} && cd -- {directory} && seq -f '{MANY_PREFIX}%g' {MANY_FILES} | xargs touch")
    bench.advance_clock(AGING_SECONDS)
    cleaner.analyze(bench)
    cleaner.confirm(bench)
    bench.ui.wait(CLEANING, timeout=cleaner.CLEAN_TIMEOUT_SECONDS)
    steps = deleting(bench)
    freed = bool(bench.ui.find(FREED_SO_FAR))
    bench.screenshot("nettoyage-en-cours")
    bench.ui.click(CANCEL_CLEANING, role=cleaner.BUTTON)
    bench.ui.wait(CANCELLED, timeout=cleaner.CLEAN_TIMEOUT_SECONDS)
    bench.check(all(name in steps for name in STEP_NAMES) and freed and bool(bench.ui.find(STEPS)),
                f"CLN-025: « {CLEANING} », its three steps ({steps}), « {FREED_SO_FAR} »")
    bench.screenshot("nettoyage-annule")
    left = int(bench.user(f"find {directory} -maxdepth 1 -name '{MANY_PREFIX}*' | wc -l").strip())
    home = common.expand_home(bench, THUMBNAILS)
    cancelled = [path for path, entry in deletions(bench).items()
                 if path.startswith(f"{home}/{MANY_PREFIX}") and entry.get("reason") == CANCELLED_REASON]
    receipt = common.last_receipt(bench)
    bench.check(0 < left < MANY_FILES and left == len(cancelled) and line_count(bench, CANCELLED_LINE) >= left
                and receipt["complete"] is False,
                f"CLN-026: « {CANCELLED} », the {left} entries left are those kept as cancelled ({len(cancelled)}), "
                f"the receipt in the history ({receipt})")
    finish(bench)
    bench.user(f"find {directory} -maxdepth 1 -name '{MANY_PREFIX}*' -delete")


def receipt(bench: Bench) -> None:
    common.plant(bench, common.Files(bench.accounts.admin.name, [REMOVED, GONE, RECENT, EXCLUDED], size=FILE_SIZE),
                 age_seconds=AGING_SECONDS)
    cleaner.analyze(bench)
    bench.user(f"rm -- {common.shell_path(GONE)} && touch -- {common.shell_path(RECENT)}")
    common.write_settings(bench, {"deletionLog": True,
                                  "exclusions": [common.expand_home(bench, EXCLUDED)]})
    common.open_screen(bench, HOME)
    common.open_screen(bench, cleaner.PAGE)
    cleaner.clean(bench)
    bench.screenshot("recu")
    shown = {key: stat(bench, label) for key, label in STATS.items()}
    lines = {name: line_count(bench, name) for name in [RECENT_LINE, EXCLUDED_LINE, GONE_LINE]}
    entries = deletions(bench)
    paths = {common.expand_home(bench, path): expected for path, expected in REASONS.items()}
    reasons = {path: (entries[path]["result"], entries[path].get("reason")) if path in entries else None
               for path in paths}
    bench.check(reasons == paths and all(lines.values()) and bool(bench.ui.find(pattern=f"^{FREED} : ")),
                f"CLN-027: removed, kept too recent, kept excluded, each said ({lines}) and logged ({reasons})")
    bench.check(shown["gone"] >= 1 and lines[GONE_LINE] >= 1,
                f"CLN-028: the thumbnail removed by hand since the analysis counted « {GONE_LINE} » ({shown})")
    written = common.last_receipt(bench)
    bench.check(bool(bench.ui.find(RECORDED)) and all(written[key] == value for key, value in shown.items())
                and written["removedBytes"] >= FILE_SIZE,
                f"CLN-029: « {RECORDED} », its line of the history says what the receipt shows ({shown}, {written})")
    common.check_kept(bench, common.Files(bench.accounts.admin.name, [RECENT, EXCLUDED]))


def afterwards(bench: Bench) -> None:
    finish(bench)
    common.open_screen(bench, HOME)
    reclaimable = bench.ui.node(RECLAIMABLE)
    common.open_screen(bench, HISTORY_PAGE)
    bench.ui.wait(pattern=HISTORY_ITEM_PATTERN, role="list-item")
    items = bench.ui.find(pattern=HISTORY_ITEM_PATTERN, role="list-item")
    bench.screenshot("historique")
    bench.check(reclaimable.get("description", "").endswith(FREED_SINCE) and len(items) >= 2,
                f"CLN-032: « {FINISH} » back to the categories, « {RECLAIMABLE} » says what was « {FREED_SINCE} », "
                f"the history lists the cleanings ({reclaimable}, {[item['name'] for item in items]})")


def run(bench: Bench) -> None:
    common.write_settings(bench, {"deletionLog": True})
    bench.launch_app()
    cleaner.open_page(bench)
    cancelled_cleaning(bench)
    receipt(bench)
    afterwards(bench)
