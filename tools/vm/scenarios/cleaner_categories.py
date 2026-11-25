"""Cleaner: the categories offered, what the settings spare, an analysis limited to the categories checked.

The four default categories checked and the GPU caches unchecked, « Mes
dossiers » offered (unchecked, last) once a folder is chosen, « Analyser »
inert with no category, the line that says what is always spared, then an
analysis of « Jeux » alone that leaves the thumbnail planted in « Système »
out of the results.
"""

from __future__ import annotations

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["cleaner"]

DEFAULT_CHECKED = ["Système", "Applications", "Navigateurs", "Jeux"]
GPU = "Caches GPU"
CUSTOM = "Mes dossiers"
NONE_CHOSEN = "Aucune catégorie sélectionnée"
SCANNING = "Analyse en cours"
MIN_AGE_MINUTES = 120
EXCLUSIONS = ["~/Documents", "~/okapi-vm-kept"]
CUSTOM_FOLDER = "~/okapi-vm-folder"
SPARED = f"Toujours épargnés : fichiers modifiés depuis moins de {MIN_AGE_MINUTES} min. Exclusions : {len(EXCLUSIONS)}."
# Beyond the minimum age of the settings above.
AGING_SECONDS = 3 * 60 * 60
FILE_SIZE = 64 * 1024
LAST_ANALYSIS = "last-analysis.json"


def default_categories(bench: Bench) -> None:
    states = {name: cleaner.is_checked(bench, name) for name in [*DEFAULT_CHECKED, GPU]}
    bench.check(all(states[name] for name in DEFAULT_CHECKED) and not states[GPU],
                f"CLN-001: the four categories checked, the GPU caches unchecked ({states})")
    bench.check(not bench.ui.find(CUSTOM, role=cleaner.CHECK_BOX), f"CLN-001: no « {CUSTOM} » without a folder chosen")


def nothing_chosen(bench: Bench) -> None:
    cleaner.choose(bench)
    bench.ui.wait(NONE_CHOSEN)
    bench.screenshot("aucune-categorie")
    # AT-SPI calls every button enabled (AccessKit sets no state for a
    # disabled one): pressed, it must do nothing.
    bench.ui.click(cleaner.ANALYZE, role=cleaner.BUTTON)
    bench.ui.wait(NONE_CHOSEN)
    started = bench.ui.find(pattern=f"^({SCANNING}|{'|'.join(cleaner.ANALYSIS_ENDS)})$")
    bench.check(not started and common.read_store(bench, LAST_ANALYSIS) is None,
                f"CLN-003: « {cleaner.ANALYZE} » starts nothing with no category, « {NONE_CHOSEN} » said")


def chosen_folder_and_spared(bench: Bench) -> None:
    bench.user(f"mkdir -p -- {common.shell_path(CUSTOM_FOLDER)}")
    common.write_settings(bench, {
        "minAgeMinutes": MIN_AGE_MINUTES,
        "exclusions": [common.expand_home(bench, path) for path in EXCLUSIONS],
        "customFolders": [common.expand_home(bench, CUSTOM_FOLDER)],
    })
    bench.launch_app()
    cleaner.open_page(bench)
    names = [card["name"] for card in bench.ui.find(role=cleaner.CHECK_BOX)]
    bench.check(bool(names) and names[-1] == CUSTOM and not cleaner.is_checked(bench, CUSTOM),
                f"CLN-002: « {CUSTOM} » offered last and unchecked once a folder is chosen ({names})")
    bench.ui.wait(SPARED)
    bench.log(f"ok: CLN-004: « {SPARED} » follows the settings")
    bench.screenshot("mes-dossiers")


def limited_analysis(bench: Bench) -> None:
    system = common.Files(bench.accounts.admin.name, ["~/.cache/thumbnails/normal/okapi-vm-system.png"], size=FILE_SIZE)
    gaming = common.Files(bench.accounts.admin.name,
                          ["~/.local/share/Steam/logs/okapi-vm-1.txt", "~/.local/share/Steam/logs/okapi-vm-2.txt"],
                          size=FILE_SIZE)
    common.plant(bench, system, gaming, age_seconds=AGING_SECONDS)
    cleaner.choose(bench, "Jeux")
    title = cleaner.analyze(bench)
    bench.screenshot("jeux-seuls")
    shown = {node["name"] for node in bench.ui.find(role=cleaner.CHECK_BOX)}
    others = {*DEFAULT_CHECKED, GPU, CUSTOM} - {"Jeux"}
    bench.check(title == cleaner.RESULTS and "Jeux" in shown and not others & shown,
                f"CLN-005: only « Jeux » in the results ({sorted(shown)})")
    scan = cleaner.last_scan(bench)
    planted = len(gaming.paths) * FILE_SIZE
    bench.check(scan["bytes"] == planted and common.read_store(bench, LAST_ANALYSIS) is None,
                f"CLN-005: the analysis counts the Steam logs alone, not the thumbnail, and keeps no summary "
                f"of a partial analysis ({scan})")


def run(bench: Bench) -> None:
    bench.launch_app()
    cleaner.open_page(bench)
    bench.screenshot("categories")
    default_categories(bench)
    nothing_chosen(bench)
    chosen_folder_and_spared(bench)
    limited_analysis(bench)
