"""Large files: what is listed, the filters, the sorts, a box that follows its file, the move to the trash.

Five large files of distinct sizes and dates, one of them with two names,
one on FAT, and a hole of 1 GiB that holds nothing. Each filter narrows the
list, together they empty it and say the files are still there. Two files
checked, one hidden by a filter: both go to the trash, whole.
"""

from __future__ import annotations

from okapivm import common
from okapivm.bench import Bench
from scenarios import _user_files as files

TAGS = ["large-files", "trash"]

DOWNLOADS = f"{files.HOME}/Téléchargements"
DOCUMENTS = f"{files.HOME}/Documents"
VIDEOS = f"{files.HOME}/Vidéos"
MUSIC = f"{files.HOME}/Musique"
FAT_MEGABYTES = 100
# path: (MiB, its age as `touch -d` says it); sizes and dates all distinct.
LARGE = {
    f"{DOWNLOADS}/disque.iso": (120, "now"),
    f"{MUSIC}/musique.flac": (55, "1 hour ago"),
    f"{DOCUMENTS}/lien-dur.bin": (70, "5 hours ago"),
    f"{DOCUMENTS}/archive.zip": (60, "1 day ago"),
    f"{VIDEOS}/film.mkv": (80, "2 years ago"),
}
ISO, FLAC, LINKED, ZIP, FILM = LARGE
OTHER_NAME = f"{DOCUMENTS}/autre-nom.bin"
HOLE = f"{DOCUMENTS}/creux.raw"
HOLE_SIZE = "1G"
SIZE = "Taille minimale"
KIND = "Type"
AGE = "Date de modification"
CLEAR = "Effacer les filtres"
# The empty list says it again, after the filters bar.
EMPTY_STATE_CLEAR_INDEX = 1
NONE_MATCH = "Aucun fichier ne correspond à ces filtres"
STILL_THERE = f"Les {len(LARGE)} gros fichiers trouvés sont toujours là : changez la taille, le type ou la date pour les revoir."
OTHER_NAMES = "case désactivée : A d'autres noms : le retirer ne libérerait rien"
NO_TRASH = "case désactivée : Pas de corbeille sur ce disque"
TRASH_CONFIRMATION = r"^Mettre 2 fichiers à la corbeille \?$"


def boxes(bench: Bench) -> list[dict]:
    return bench.ui.find(role="check-box")


def shown_paths(bench: Bench) -> list[str]:
    """The files the list shows, in its order, by the path it shows."""
    paths = []
    for box in boxes(bench):
        name, folder = box["name"].split(", ")[:2]
        paths.append(f"{files.absolute(folder)}/{name}")
    return paths


def listed(bench: Bench) -> list[str]:
    """The files the list shows, in its order, the file with two names by its first."""
    return [LINKED if path == OTHER_NAME else path for path in shown_paths(bench)]


def box_of(bench: Bench, path: str) -> dict:
    return bench.ui.node(pattern=files.file_row(path), role="check-box")


def pick(bench: Bench, menu: str, choice: str) -> None:
    """Opens the filter `menu`, chooses the item whose name matches `choice`, closes it."""
    bench.ui.click(menu, role="combo-box")
    bench.ui.click(pattern=choice, role="list-item")
    if bench.ui.find(menu, role="list"):
        bench.key("Escape")


def clear(bench: Bench) -> None:
    bench.ui.click(CLEAR, role="button")
    bench.ui.wait(pattern=rf"^{len(LARGE)} fichiers$", role="label")


def run(bench: Bench) -> None:
    common.mount_fat(bench, MUSIC, size_megabytes=FAT_MEGABYTES)
    bench.user(" && ".join(
        [f"fallocate -l {megabytes * files.MEBIBYTE} -- {path} && touch -d '{age}' -- {path}"
         for path, (megabytes, age) in LARGE.items()]
        + [f"ln -- {LINKED} {OTHER_NAME}", f"truncate --size={HOLE_SIZE} -- {HOLE}"]))
    bench.log("five large files, one with two names, one on FAT, a hole of 1 GiB")
    digests = {path: files.checksum(bench, path) for path in (ISO, FILM)}

    bench.launch_app()
    files.search(bench, files.LARGE_FILES)
    bench.screenshot("gros-fichiers")
    what_is_listed(bench)
    shut_boxes(bench)
    sorts(bench)
    filters(bench)
    followed_and_trashed(bench, digests)


def what_is_listed(bench: Bench) -> None:
    """GRF-001."""
    shown = shown_paths(bench)
    bench.check(sorted(listed(bench)) == sorted(LARGE), f"GRF-001: the five large files are listed ({shown})")
    bench.check(HOLE not in shown, f"GRF-001: the hole of {HOLE_SIZE}, holding nothing, is no large file")
    bench.check(sum(path in (LINKED, OTHER_NAME) for path in shown) == 1,
                "GRF-001: a file with two names is listed once")
    said = bench.ui.find(pattern=rf"^{len(LARGE)} fichiers de plus de 50,0 Mo, ", role="label")
    bench.check(bool(said), f"GRF-001: « {len(LARGE)} fichiers de plus de 50,0 Mo »")


def shut_boxes(bench: Bench) -> None:
    """GRF-005."""
    linked = next(box for box in boxes(bench) if box["name"].startswith(("lien-dur.bin, ", "autre-nom.bin, ")))
    flac = box_of(bench, FLAC)
    bench.check(not files.enabled(linked) and linked["name"].endswith(OTHER_NAMES),
                f"GRF-005: a file with other names cannot be checked ({linked['name']})")
    bench.check(not files.enabled(flac) and flac["name"].endswith(NO_TRASH),
                f"GRF-005: a file on FAT cannot be checked ({flac['name']})")


def sorts(bench: Bench) -> None:
    """GRF-003."""
    by_size = sorted(LARGE, key=lambda path: LARGE[path][0], reverse=True)
    shown = listed(bench)
    bench.check(shown == by_size, f"GRF-003: largest first ({shown})")
    bench.ui.click("Modifié", role="button")
    newest_first = list(LARGE)
    shown = listed(bench)
    bench.screenshot("tri-date")
    bench.check(shown in (newest_first, newest_first[::-1]), f"GRF-003: by date of modification ({shown})")
    bench.ui.click(pattern=r"^Taille", role="button")
    shown = listed(bench)
    if shown != by_size:
        bench.ui.click(pattern=r"^Taille", role="button")
        shown = listed(bench)
    bench.check(shown == by_size, f"GRF-003: by size again, largest first ({shown})")


def filters(bench: Bench) -> None:
    """GRF-002, GRF-007."""
    pick(bench, SIZE, r"^Plus de 100,0 Mo")
    bench.check(listed(bench) == [ISO], f"GRF-002: over 100 Mo, the disk image alone ({listed(bench)})")
    bench.check(bool(bench.ui.find(f"1 fichier sur {len(LARGE)}", role="label")),
                f"GRF-002: « 1 fichier sur {len(LARGE)} »")
    clear(bench)
    pick(bench, KIND, r"^Vidéos, ")
    bench.check(listed(bench) == [FILM], f"GRF-002: the videos, the film alone ({listed(bench)})")
    clear(bench)
    pick(bench, AGE, r"^Modifiés il y a plus de 1 an(, |$)")
    bench.check(listed(bench) == [FILM], f"GRF-002: modified years ago, the film alone ({listed(bench)})")
    pick(bench, SIZE, r"^Plus de 100,0 Mo")
    said = [bench.ui.wait(NONE_MATCH), bench.ui.wait(STILL_THERE)]
    bench.screenshot("aucun-ne-correspond")
    bench.check(all(said) and not boxes(bench), f"GRF-002, GRF-007: « {NONE_MATCH} », « {STILL_THERE} »")
    bench.ui.click(CLEAR, role="button", index=EMPTY_STATE_CLEAR_INDEX)
    bench.ui.wait(pattern=rf"^{len(LARGE)} fichiers$", role="label")
    bench.check(len(listed(bench)) == len(LARGE), "GRF-002: « Effacer les filtres » shows them all again")


def followed_and_trashed(bench: Bench, digests: dict[str, str]) -> None:
    """GRF-004, GRF-006."""
    bench.ui.click(pattern=files.file_row(ISO), role="check-box")
    bench.ui.click(pattern=files.file_row(FILM), role="check-box")
    bench.ui.click("Modifié", role="button")
    pick(bench, KIND, r"^Images disque, ")
    asked = [path for path in listed(bench) if files.checked(box_of(bench, path))]
    bench.check(asked == [ISO], f"GRF-004: sorted and filtered, the disk image still checked ({asked})")
    chosen = bench.ui.wait(pattern=r"^2 fichiers, dont 1 hors des filtres$", role="label")
    bench.check(bool(chosen), "GRF-004: the film, hidden by the filter, still chosen: « 2 fichiers, dont 1 hors des filtres »")
    mark = files.trash_checked(bench, confirmation=TRASH_CONFIRMATION)
    bench.screenshot("recu")
    receipt = bench.ui.find(pattern=r"^2 fichiers mis à la corbeille, ", role="label")
    bench.check(bool(receipt), f"GRF-006: « 2 fichiers mis à la corbeille, … » ({receipt})")
    done = files.outcomes(bench, mark, moved=files.FILE_MOVED, kept=files.FILE_KEPT)
    bench.check(done == {ISO: files.TRASHED, FILM: files.TRASHED}, f"GRF-006: the journal says both went ({done})")
    for path, digest in digests.items():
        files.check_trashed(bench, "GRF-006", path, digest)
    common.check_kept(bench, common.Files(bench.accounts.admin.name, [FLAC, LINKED, ZIP, OTHER_NAME, HOLE]))
