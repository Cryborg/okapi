"""Cleaner: the results screen, its tree, its sorts, its boxes, its explanations and remarks.

Old files in « Système », a Chromium profile's caches and Steam's logs; one
cache excluded by the settings, one folder unreadable. The results show the
category → browser → group tree, fold and unfold it, sort it three ways,
describe each group, check a category's preselected groups alone, explain a
group, list the remarks; « Retour » and « Nouvelle analyse » lead where they say.
"""

from __future__ import annotations

import re

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["cleaner"]

AGING_SECONDS = 2 * 60 * 60
FILE_SIZE = 64 * 1024
ACCOUNT_FILES = [
    "~/.cache/thumbnails/normal/okapi-vm.png",
    "~/.cache/fontconfig/okapi-vm.cache-9",
    "~/.config/google-chrome/Default/Cache/Cache_Data/okapi-vm",
    "~/.config/google-chrome/Default/GPUCache/okapi-vm",
    "~/.config/google-chrome/Default/Preferences",
    "~/.local/share/Steam/logs/okapi-vm.txt",
]
EXCLUDED = "~/.cache/gstreamer-1.0"
# A rule's root: an unreadable folder below one is only counted in the journal.
UNREADABLE = "~/.cache/tracker"
FOUND_PATTERN = r"^Trouvé : .+\. Éléments : (\d+)\.$"
THUMBNAILS = "Cache des miniatures"
FONTS = "Cache des polices"
FONTS_SAID = "Cache de rendu des polices de Fontconfig : recréé tout seul quand il manque."
THUMBNAILS_SAID = ("Miniatures que les gestionnaires de fichiers créent pour l'aperçu (norme freedesktop) : ils les "
                   "recréent quand vous rouvrez un dossier. Vos images ne sont pas touchées.")
BROWSERS = "Navigateurs"
CHROME = "Chrome"
CACHE = "Cache"
GPU_CACHE = "Cache GPU"
SORT_COLUMNS = ["Élément", "Éléments", "Taille"]
DESCENDING = ", trié par ordre décroissant"
ASCENDING = ", trié par ordre croissant"
SIZE_PATTERN = r"^Éléments : \d+, (\d+)(?:,(\d))? (o|ko|Mo|Go)"
UNITS = {"o": 1, "ko": 1000, "Mo": 1000 ** 2, "Go": 1000 ** 3}
PRESELECTED = "Présélectionné"
ADMIN = "Administrateur"
PARTIAL = "sélection partielle"
GROUP_BY_GROUP = "À choisir groupe par groupe"
REMARKS_PATTERN = r"^Remarques de l'analyse : (\d+)$"
SHOW = "Afficher"
HIDE = "Masquer"
EXCLUDED_REMARK = "Dossier exclu par vos réglages : {}"
UNREADABLE_REMARK = "Dossier illisible, non compté : {}"
BACK = "Retour au choix des catégories"
NEW_ANALYSIS = "Nouvelle analyse"
LAST_ANALYSIS = "last-analysis.json"


def prepare(bench: Bench) -> None:
    files = common.Files(bench.accounts.admin.name, ACCOUNT_FILES, size=FILE_SIZE)
    common.plant(bench, files)
    unreadable = common.shell_path(UNREADABLE)
    bench.user(f"mkdir -p -- {unreadable} && chmod 000 -- {unreadable}")
    common.write_settings(bench, {"exclusions": [common.expand_home(bench, EXCLUDED)]})
    bench.advance_clock(AGING_SECONDS)


def description(bench: Bench, name: str, *, index: int | None = None) -> str:
    return bench.ui.node(name, role=cleaner.CHECK_BOX, index=index).get("description", "")


def tree(bench: Bench) -> None:
    found = bench.ui.node(pattern=FOUND_PATTERN)
    summary = common.read_store(bench, LAST_ANALYSIS)
    assert isinstance(summary, dict)
    entries = int(re.match(FOUND_PATTERN, found["name"]).group(1))
    bench.check(entries == summary["entries"],
                f"CLN-011: « {found['name']} » counts what the analysis wrote ({summary})")
    for name in ["Système", "Applications", "Jeux"]:
        cleaner.fold(bench, name)
    rows = [node["name"] for node in bench.ui.find(role=cleaner.CHECK_BOX)]
    chrome = rows.index(CHROME) if CHROME in rows else -1
    bench.check(BROWSERS in rows and 0 <= rows.index(BROWSERS) < chrome < rows.index(GPU_CACHE),
                f"CLN-011: category → browser → group ({rows})")
    cleaner.fold(bench, CHROME)
    folded = [node["name"] for node in bench.ui.find(role=cleaner.CHECK_BOX)]
    cleaner.fold(bench, CHROME, folded=False)
    bench.check(GPU_CACHE not in folded and bool(bench.ui.find(GPU_CACHE, role=cleaner.CHECK_BOX)),
                f"CLN-011: « Replier {CHROME} » hides its groups, « Déplier » shows them again ({folded})")
    bench.screenshot("arbre")


def sizes(bench: Bench) -> list[int]:
    """The sizes of the groups shown under « Système », in their order."""
    shown = []
    for node in bench.ui.find(role=cleaner.CHECK_BOX):
        match = re.match(SIZE_PATTERN, node.get("description", ""))
        if match and node["name"] not in cleaner.CATEGORIES:
            whole, tenths, unit = match.groups()
            shown.append((int(whole) * 10 + int(tenths or 0)) * UNITS[unit] // 10)
    return shown


def sorting(bench: Bench) -> None:
    for name in [BROWSERS, "Applications", "Jeux"]:
        cleaner.fold(bench, name)
    cleaner.fold(bench, "Système", folded=False)
    orders = {}
    for column in SORT_COLUMNS:
        bench.ui.click(column, role=cleaner.BUTTON)
        first = bench.ui.wait(pattern=f"^{column}({DESCENDING}|{ASCENDING})$", role=cleaner.BUTTON)[0]["name"]
        other = column + (ASCENDING if first.endswith(DESCENDING) else DESCENDING)
        bench.ui.click(first, role=cleaner.BUTTON)
        bench.ui.wait(other, role=cleaner.BUTTON)
        orders[column] = (first, other)
    bench.log(f"ok: CLN-012: each column sorts, its name says the order, the second click reverses it ({orders})")
    by_size = {}
    for _ in range(2):
        shown = bench.ui.node(pattern=f"^Taille({DESCENDING}|{ASCENDING})$", role=cleaner.BUTTON)["name"]
        by_size[shown.endswith(DESCENDING)] = sizes(bench)
        bench.ui.click(shown, role=cleaner.BUTTON)
        bench.ui.wait(shown, role=cleaner.BUTTON, gone=True)
    descending, ascending = by_size[True], by_size[False]
    bench.check(len(descending) >= 2 and descending == sorted(descending, reverse=True)
                and ascending == sorted(ascending),
                f"CLN-012: by size, larger first then smaller first ({descending}, {ascending})")
    bench.screenshot("tri")


def descriptions(bench: Bench) -> None:
    said = {
        THUMBNAILS: description(bench, THUMBNAILS),
        "Système": description(bench, "Système"),
    }
    admin = [node for node in bench.ui.find(role=cleaner.CHECK_BOX) if node.get("description", "").endswith(ADMIN)]
    cleaner.fold(bench, "Système")
    cleaner.fold(bench, "Applications", folded=False)
    said["Applications"] = description(bench, "Applications")
    bench.check(said[THUMBNAILS].endswith(PRESELECTED) and said["Système"].endswith(PARTIAL) and bool(admin)
                and said["Applications"].endswith(GROUP_BY_GROUP),
                f"CLN-013: « {PRESELECTED} », « {ADMIN} » ({len(admin)}), « {PARTIAL} », « {GROUP_BY_GROUP} » ({said})")
    cleaner.fold(bench, "Applications")


def category_box(bench: Bench) -> None:
    cleaner.fold(bench, BROWSERS, folded=False)
    cleaner.fold(bench, CHROME, folded=False)
    # Partly checked, the category's box unchecks all; unchecked, it checks the preselected.
    bench.ui.click(BROWSERS, role=cleaner.CHECK_BOX)
    off = (cleaner.is_checked(bench, CACHE), cleaner.is_checked(bench, GPU_CACHE))
    bench.ui.click(BROWSERS, role=cleaner.CHECK_BOX)
    on = (cleaner.is_checked(bench, CACHE), cleaner.is_checked(bench, GPU_CACHE))
    bench.check(off == (False, False) and on == (True, False),
                f"CLN-014: the category's box unchecks all, then checks the preselected alone ({off}, {on})")
    bench.screenshot("case-categorie")


def explanations(bench: Bench) -> None:
    cleaner.fold(bench, BROWSERS)
    cleaner.fold(bench, "Système", folded=False)
    cleaner.press_for(bench, cleaner.SAFE, THUMBNAILS)
    bench.ui.wait(THUMBNAILS_SAID)
    cleaner.press_for(bench, cleaner.WHAT_GOES, FONTS)
    bench.ui.wait(FONTS_SAID)
    bench.screenshot("explications")
    cleaner.press_for(bench, cleaner.SAFE, THUMBNAILS)
    bench.ui.wait(THUMBNAILS_SAID, gone=True)
    bench.log(f"ok: CLN-015: « {cleaner.SAFE} » and « {cleaner.WHAT_GOES} » unfold what their group says, a second click folds it")


def remarks(bench: Bench) -> None:
    count = bench.ui.node(pattern=REMARKS_PATTERN)
    bench.ui.click(SHOW, role=cleaner.BUTTON)
    excluded = EXCLUDED_REMARK.format(common.expand_home(bench, EXCLUDED))
    unreadable = UNREADABLE_REMARK.format(common.expand_home(bench, UNREADABLE))
    bench.ui.wait(excluded)
    bench.ui.wait(unreadable)
    bench.screenshot("remarques")
    bench.ui.click(HIDE, role=cleaner.BUTTON)
    bench.ui.wait(excluded, gone=True)
    gstreamer = bench.user(f"find {common.shell_path(EXCLUDED)} -type f | wc -l").strip()
    bench.check(int(gstreamer) >= 1,
                f"CLN-016: « {count['name']} », shown then hidden, names the excluded and the unreadable folders; "
                f"the excluded cache left in place ({gstreamer} files)")


def back_and_again(bench: Bench) -> None:
    bench.ui.click(BACK, role=cleaner.BUTTON)
    bench.ui.wait(cleaner.ANALYZE, role=cleaner.BUTTON)
    first = common.read_store(bench, LAST_ANALYSIS)
    cleaner.analyze(bench)
    bench.ui.click(NEW_ANALYSIS, role=cleaner.BUTTON)
    bench.ui.wait(pattern=f"^({'|'.join(cleaner.ANALYSIS_ENDS)})$", timeout=cleaner.SCAN_TIMEOUT_SECONDS)
    again = common.read_store(bench, LAST_ANALYSIS)
    assert isinstance(first, dict) and isinstance(again, dict)
    bench.check(again["time"] > first["time"],
                f"CLN-020: « Retour » leads to the categories, « {NEW_ANALYSIS} » analyses again ({first}, {again})")


def run(bench: Bench) -> None:
    prepare(bench)
    bench.launch_app()
    cleaner.open_page(bench)
    bench.check(cleaner.analyze(bench) == cleaner.RESULTS, "the analysis shows results")
    bench.screenshot("resultats")
    tree(bench)
    sorting(bench)
    descriptions(bench)
    category_box(bench)
    explanations(bench)
    remarks(bench)
    back_and_again(bench)
