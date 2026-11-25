"""Duplicates: the groups by the room they free, the copies checked beforehand, the quick selections, why a copy stays.

Groups of distinct gains, listed largest first, folded and unfolded. The copy
outside Downloads stays even when newer and named otherwise; a group all in
Downloads keeps one copy. Each quick selection keeps the copy it names in a
group whose oldest, newest (in Musique) and shortest path are three
different files. A copy with other names, a copy on FAT: kept, and said.
"""

from __future__ import annotations

import re

from okapivm import common
from okapivm.bench import Bench
from scenarios import _user_files as files

TAGS = ["duplicates"]

MEGABYTE = 1_000_000
DOWNLOADS = f"{files.HOME}/Téléchargements"
DOCUMENTS = f"{files.HOME}/Documents"
DESKTOP = f"{files.HOME}/Bureau"
HIDDEN = f"{files.HOME}/.cachette"
# Each copy's age, in days: `touch` sets it.
QUICK = {
    "oldest": (f"{DOCUMENTS}/older/quick.bin", 5),
    "newest": (f"{files.HOME}/Musique/newer-one/quick.bin", 0),
    "shortest": (f"{files.HOME}/Images/quick.bin", 2),
}
QUICK_RULES = [
    ("Garder la plus ancienne", "oldest", "Gardée : la plus ancienne"),
    ("Garder la plus récente", "newest", "Gardée : la plus récente"),
    ("Garder le chemin le plus court", "shortest", "Gardée : le chemin le plus court"),
]
BY_FOLDER = "Garder celles d'un dossier…"
QUICK_APPLIED = "quick selection applied"
TO_THE_TRASH = "Vers la corbeille"
GAIN = re.compile(r", (\d+(?:,\d)?) (o|ko|Mo) à gagner, ")
UNITS = {"o": 1, "ko": 1_000, "Mo": MEGABYTE}
OTHER_NAMES = "Gardée : a d'autres noms"
OTHER_NAMES_SAID = "Ce fichier a d'autres noms : le retirer ne libérerait rien, son contenu reste sous ses autres noms."
NO_TRASH = "Gardée : pas de corbeille sur ce disque"
LAST_KEPT = "Seule copie gardée du groupe : décochez-en une autre pour la garder à sa place."


def fate(box: dict) -> str:
    return box["name"].rsplit(", ", 1)[-1]


def run(bench: Bench) -> None:
    files.random_file(bench, f"{DOCUMENTS}/big.bin", 3 * MEGABYTE, f"{DOWNLOADS}/big.bin", f"{DOWNLOADS}/big-2.bin")
    files.random_file(bench, f"{DOCUMENTS}/mid.bin", 4 * MEGABYTE + MEGABYTE // 2, f"{DOWNLOADS}/mid.bin")
    files.random_file(bench, f"{DOWNLOADS}/recent.bin", 2 * MEGABYTE, f"{DOCUMENTS}/recent-renamed.bin")
    bench.user(f"touch -d '2 days ago' -- {DOWNLOADS}/recent.bin")
    files.random_file(bench, f"{DOWNLOADS}/pair-1.bin", 2 * MEGABYTE + MEGABYTE // 2, f"{DOWNLOADS}/pair-2.bin")
    oldest, *others = (path for path, _ in QUICK.values())
    files.random_file(bench, oldest, 3 * MEGABYTE + MEGABYTE // 2, *others)
    bench.user(" && ".join(f"touch -d '{days} days ago' -- {path}" for path, days in QUICK.values()))
    files.random_file(bench, f"{DOCUMENTS}/named.bin", 1_500_000, f"{DOWNLOADS}/named.bin")
    bench.user(f"mkdir -p -- {HIDDEN} && ln -- {DOWNLOADS}/named.bin {HIDDEN}/named.bin")
    common.mount_fat(bench, DESKTOP)
    files.random_file(bench, f"{DOCUMENTS}/fat.bin", 1_200_000, f"{DESKTOP}/fat.bin")
    bench.log("groups of distinct gains, a quick-selection group, a copy with another name, one on FAT")

    bench.launch_app()
    files.search(bench, files.DUPLICATES)
    by_gain(bench)
    checked_beforehand(bench)
    why_kept(bench)
    quick_selections(bench)


def by_gain(bench: Bench) -> None:
    """DBL-006."""
    bench.check(bool(bench.ui.find("Groupes de doublons, triés par place à gagner", role="list-box")),
                "DBL-006: « Groupes de doublons, triés par place à gagner »")
    while unfolded := [row for row in files.groups(bench) if row.endswith(files.UNFOLDED)]:
        bench.ui.click(unfolded[0], role="list-item")
        bench.ui.wait(unfolded[0].removesuffix(files.UNFOLDED) + files.FOLDED, role="list-item")
    rows = files.groups(bench)
    bench.screenshot("groupes-replies")
    gains = [float(match[1].replace(",", ".")) * UNITS[match[2]] for row in rows if (match := GAIN.search(row))]
    bench.check(len(gains) == len(rows) == 7 and gains == sorted(gains, reverse=True),
                f"DBL-006: the seven groups, folded, largest gain first ({rows})")
    bench.check(not bench.ui.find(role="check-box"), "DBL-006: a folded group shows none of its copies")
    big = next(row for row in rows if row.startswith("big"))
    bench.ui.click(big, role="list-item")
    unfolded = bench.ui.wait(big.removesuffix(files.FOLDED) + files.UNFOLDED, role="list-item")
    bench.check(bool(unfolded) and len(bench.ui.find(role="check-box")) == 3,
                f"DBL-006: unfolded, « {big} » shows its three copies")


def checked_beforehand(bench: Bench) -> None:
    """DBL-007."""
    kept = files.copy_box(bench, "recent-renamed.bin", f"{DOCUMENTS}/recent-renamed.bin")
    asked = files.copy_box(bench, "recent-renamed.bin", f"{DOWNLOADS}/recent.bin")
    bench.screenshot("coche-d-avance")
    bench.check(not files.checked(kept) and fate(kept).startswith("Gardée : hors ")
                and files.checked(asked) and fate(asked) == TO_THE_TRASH,
                f"DBL-007: the newer copy, named otherwise, outside Downloads, stays; the older goes "
                f"({kept['name']} / {asked['name']})")
    pair = [files.copy_box(bench, "pair-1.bin", f"{DOWNLOADS}/pair-{i}.bin") for i in (1, 2)]
    bench.check(sorted(files.checked(box) for box in pair) == [False, True],
                f"DBL-007: of a group all in Downloads, one copy stays unchecked ({[box['name'] for box in pair]})")
    files.focus_copy(bench, "recent-renamed.bin", f"{DOCUMENTS}/recent-renamed.bin")
    bench.check(bool(bench.ui.wait(LAST_KEPT)), f"DBL-009: its details say « {LAST_KEPT} »")


def why_kept(bench: Bench) -> None:
    """DBL-009."""
    named = files.copy_box(bench, "named.bin", f"{DOWNLOADS}/named.bin")
    bench.check(not files.checked(named) and not files.enabled(named) and fate(named) == OTHER_NAMES,
                f"DBL-009: a copy with another name stays, its box shut: « {OTHER_NAMES} » ({named['name']})")
    files.focus_copy(bench, "named.bin", f"{DOWNLOADS}/named.bin")
    bench.check(bool(bench.ui.wait(OTHER_NAMES_SAID)), f"DBL-009: its details say « {OTHER_NAMES_SAID} »")
    fat = files.copy_box(bench, "fat.bin", f"{DESKTOP}/fat.bin")
    bench.screenshot("raisons-de-garde")
    bench.check(not files.checked(fat) and not files.enabled(fat) and fate(fat) == NO_TRASH,
                f"DBL-009: a copy on FAT stays, its box shut: « {NO_TRASH} » ({fat['name']})")


def quick_kept(bench: Bench, rule: str, keeper: str, said: str) -> None:
    boxes = {path: files.copy_box(bench, "quick.bin", path) for path, _ in QUICK.values()}
    left = [box["name"] for box in boxes.values() if not files.checked(box)]
    bench.check(left == [boxes[keeper]["name"]] and fate(boxes[keeper]) == said,
                f"DBL-008: « {rule} » keeps {files.shown(keeper)} alone, « {said} » ({left})")


def quick_selections(bench: Bench) -> None:
    """DBL-008."""
    for rule, keeper, said in QUICK_RULES:
        mark = len(common.journal(bench))
        bench.ui.click("Sélection rapide", role="button")
        bench.ui.click(rule, role="button")
        quick_kept(bench, rule, QUICK[keeper][0], said)
        applied = [event for event in common.journal(bench)[mark:]
                   if event["fields"].get("message") == QUICK_APPLIED]
        bench.check(bool(applied), f"DBL-008: the journal says « {rule} » was applied ({applied})")
    bench.ui.click("Sélection rapide", role="button")
    bench.ui.click(BY_FOLDER, role="button")
    bench.ui.click("Garder celles de Musique", role="button")
    quick_kept(bench, "Garder celles de Musique", QUICK["newest"][0], "Gardée : dans Musique")
    bench.screenshot("selection-rapide")
    bench.ui.click("Tout désélectionner", role="button")
    files.unfold_only(bench, "quick.bin")
    asked = [box["name"] for box in bench.ui.find(role="check-box") if files.checked(box)]
    bench.check(not asked, f"DBL-008: « Tout désélectionner » leaves nothing checked ({asked})")
