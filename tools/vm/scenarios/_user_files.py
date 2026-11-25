"""What the Duplicates and Large files scenarios share: choosing where to look,
searching, checking copies, moving to the trash, and what each move did,
read in the interface's journal."""

from __future__ import annotations

import re

from okapivm import common
from okapivm.bench import Bench, ScenarioFailure

DUPLICATES = "Doublons"
LARGE_FILES = "Gros fichiers"
SEARCH_TIMEOUT_SECONDS = 300
TRASH_TIMEOUT_SECONDS = 120
MEBIBYTE = 1024 * 1024
HOME = "/home/tester"
# The personal folders of the base image, as xdg-user-dirs wrote them.
STANDARD_FOLDERS = {
    "DESKTOP": "$HOME/Bureau",
    "DOWNLOAD": "$HOME/Téléchargements",
    "DOCUMENTS": "$HOME/Documents",
    "MUSIC": "$HOME/Musique",
    "PICTURES": "$HOME/Images",
    "VIDEOS": "$HOME/Vidéos",
}
# The labels of the boxes of « Où chercher ? », for each folder kind.
FOLDER_BOXES = ["Documents", "Images", "Musique", "Vidéos", "Téléchargements", "Bureau"]
WHOLE_HOME = "Dossier personnel"
TRASH_BUTTON = "Mettre à la corbeille"
# The confirmation's button comes after the bar's in the tree.
CONFIRM_INDEX = 1
RECEIPT = r"mis à la corbeille"
# The most rows the keyboard walks down a group to reach one of its copies.
MAX_COPIES_DOWN = 10
UNFOLDED = ", déplié"
FOLDED = ", replié"
# What the journal says of each move (crates/tools/src/{duplicates,large_files}/trash.rs).
COPY_MOVED = "copy moved to the trash"
COPY_KEPT = "copy kept"
KEEPER_PASSED_OVER = "keeper passed over"
FILE_MOVED = "file moved to the trash"
FILE_KEPT = "file kept"
TRASHED = "trashed"
TRASH_FILES = "~/.local/share/Trash/files"
TRASH_URI = "trash:///"


def checksum(bench: Bench, path: str) -> str:
    return bench.user(f"sha256sum -- {common.shell_path(path)}").split()[0]


def random_file(bench: Bench, path: str, size: int, *copies: str) -> None:
    """`path` of random content, and copies of it (folders made)."""
    lines = [f'mkdir -p -- "$(dirname -- {common.shell_path(target)})"' for target in (path, *copies)]
    lines.append(f"head -c {size} /dev/urandom > {common.shell_path(path)}")
    lines += [f"cp -- {common.shell_path(path)} {common.shell_path(copy)}" for copy in copies]
    bench.user(" && ".join(lines))


def shown(path: str) -> str:
    """A path as the screens show it, the home folder written `~`."""
    return "~" + path.removeprefix(HOME) if path.startswith(HOME) else path


def absolute(path: str) -> str:
    return HOME + path.removeprefix("~") if path.startswith("~/") else path


def choose_folders(bench: Bench, *folders: str, whole_home: bool = False) -> None:
    """On the start of the screen, checks exactly `folders` (labels of « Où chercher ? »,
    each folder's own name), every personal folder without any, or the whole home folder."""
    bench.ui.wait("Où chercher ?")
    home = bench.ui.node(WHOLE_HOME, role="check-box")
    if ("checked" in home["states"]) != whole_home:
        bench.ui.click(WHOLE_HOME, role="check-box")
    if whole_home:
        return
    for box in bench.ui.find(role="check-box"):
        if box["name"] == WHOLE_HOME:
            continue
        if ("checked" in box["states"]) != (not folders or box["name"] in folders):
            bench.ui.click(box["name"], role="check-box")


def search(bench: Bench, screen: str, *folders: str, whole_home: bool = False, done: str | None = None) -> None:
    """Opens `screen`, chooses where to look, searches, until `done` (a pattern) shows."""
    common.open_screen(bench, screen)
    if not bench.ui.find("Où chercher ?"):
        bench.ui.click("Changer les dossiers", role="button")
    choose_folders(bench, *folders, whole_home=whole_home)
    bench.ui.click("Rechercher", role="button")
    finished = done or (r"^Groupes : |^Aucun doublon" if screen == DUPLICATES
                        else r"^\d+ fichiers? de plus de |^Aucun ")
    bench.ui.wait(pattern=finished, role="label", timeout=SEARCH_TIMEOUT_SECONDS)


def search_again(bench: Bench, done: str = r"^Groupes : |^Aucun doublon") -> None:
    bench.ui.click("Rechercher à nouveau", role="button")
    bench.ui.wait(pattern=done, role="label", timeout=SEARCH_TIMEOUT_SECONDS)


def groups(bench: Bench) -> list[str]:
    """The groups' rows the list shows, in its order."""
    return [item["name"] for item in bench.ui.find(role="list-item")
            if item["name"].endswith((UNFOLDED, FOLDED))]


def unfold_only(bench: Bench, name: str) -> None:
    """Folds every other group until the group `name` shows, unfolded, with its copies:
    the list only builds the rows it shows."""
    prefix = f"{name}, "
    while True:
        rows = groups(bench)
        target = next((row for row in rows if row.startswith(prefix)), None)
        others = [row for row in rows if row.endswith(UNFOLDED) and not row.startswith(prefix)]
        if target is not None and target.endswith(FOLDED):
            bench.ui.click(target, role="list-item")
            bench.ui.wait(pattern=f"^{re.escape(prefix)}.*{UNFOLDED}$", role="list-item")
        elif others:
            bench.ui.click(others[0], role="list-item")
            bench.ui.wait(others[0].removesuffix(UNFOLDED) + FOLDED, role="list-item")
        elif target is None:
            raise ScenarioFailure(f"no group {name!r} among {rows}")
        else:
            return


def file_row(path: str) -> str:
    """The pattern of a large file's box: its name, its folder, then what its state changes."""
    folder, name = path.rsplit("/", 1)
    return f"^{re.escape(name)}, {re.escape(shown(folder))}, "


def copy_row(path: str) -> str:
    """The pattern of a copy's box: its name ends with what becomes of it, which a click changes."""
    return f"^{re.escape(shown(path))}, "


def focus_group(bench: Bench, name: str) -> None:
    """The group `name` unfolded alone, its row with the focus: the details are its own."""
    unfold_only(bench, name)
    row = bench.ui.node(pattern=f"^{re.escape(name)}, .*{UNFOLDED}$", role="list-item")
    bench.ui.click(row["name"], role="list-item")
    bench.ui.click(row["name"].removesuffix(UNFOLDED) + FOLDED, role="list-item")


def copy_box(bench: Bench, group: str, path: str) -> dict:
    """The box of the copy at `path` in `group`, that group unfolded; the list
    scrolled down to it by the keyboard when it lies below the screen."""
    unfold_only(bench, group)
    if not bench.ui.find(pattern=copy_row(path), role="check-box"):
        focus_group(bench, group)
        for _ in range(MAX_COPIES_DOWN):
            bench.key("Down")
            if bench.ui.find(pattern=copy_row(path), role="check-box"):
                break
    return bench.ui.node(pattern=copy_row(path), role="check-box")


def set_copy(bench: Bench, group: str, path: str, checked: bool) -> None:
    box = copy_box(bench, group, path)
    if ("checked" in box["states"]) != checked:
        bench.ui.click(pattern=copy_row(path), role="check-box")


def focus_copy(bench: Bench, group: str, path: str) -> None:
    """Gives the focus to the copy at `path` of `group`, by the keyboard from its group's row,
    until the details name its folder (a click on its box would change it)."""
    focus_group(bench, group)
    folder = path.rsplit("/", 1)[0]
    said = f"^Dossier : ({re.escape(shown(folder))}|{re.escape(folder)})$"
    for _ in range(MAX_COPIES_DOWN):
        bench.key("Down")
        if bench.ui.find(pattern=said, role="label"):
            return
    raise ScenarioFailure(f"the focus never reached {path} in {group!r}")


def checked(box: dict) -> bool:
    return "checked" in box["states"]


def enabled(box: dict) -> bool:
    return "enabled" in box["states"] and "read-only" not in box["states"]


def trash_checked(bench: Bench, confirmation: str = r"^Mettre .*à la corbeille \?$") -> int:
    """Moves what is checked to the trash, confirmed, until the receipt shows;
    gives where the journal stood before, for `outcomes`."""
    mark = len(common.journal_events(bench))
    bench.ui.click(TRASH_BUTTON, role="button")
    bench.ui.wait(pattern=confirmation)
    bench.ui.click(TRASH_BUTTON, role="button", index=CONFIRM_INDEX)
    bench.ui.wait(pattern=RECEIPT, role="label", timeout=TRASH_TIMEOUT_SECONDS)
    return mark


def outcomes(bench: Bench, mark: int, *, moved: str = COPY_MOVED, kept: str = COPY_KEPT) -> dict[str, str]:
    """What the move begun after `mark` did with each path: `trashed`, or the reason it was kept."""
    result = {}
    for event in common.journal_events(bench)[mark:]:
        fields = event["fields"]
        if fields.get("message") == moved:
            result[fields["path"]] = TRASHED
        elif fields.get("message") == kept:
            result[fields["path"]] = fields["reason"]
    return result


def passed_over(bench: Bench, mark: int) -> dict[str, str]:
    """Each copy asked for whose keeper was passed over since `mark`: why the keeper was unsure."""
    return {event["fields"]["copy"]: event["fields"].get("unsure") or event["fields"]["reason"]
            for event in common.journal_events(bench)[mark:]
            if event["fields"].get("message") == KEEPER_PASSED_OVER}


def check_trashed(bench: Bench, entry: str, path: str, digest: str) -> None:
    """`path` gone from its place, in the trash under its own path, its content whole."""
    trash = common.trashed(bench)
    gone = bench.execute(bench.accounts.admin.name, f"[ -e {common.shell_path(path)} ]").code != 0
    bench.check(gone and path in trash, f"{entry}: {path} left its place for the trash")
    inside = f"{TRASH_FILES}/{trash[path].removeprefix(TRASH_URI)}"
    bench.check(checksum(bench, inside) == digest, f"{entry}: the trash holds {path} whole, as {inside}")


def check_kept_whole(bench: Bench, entry: str, path: str, digest: str | None = None) -> None:
    """`path` still at its place (with `digest`, unchanged), and nothing of it in the trash."""
    present = bench.execute(bench.accounts.admin.name, f"[ -f {common.shell_path(path)} ]").code == 0
    bench.check(present, f"{entry}: {path} is still at its place")
    if digest is not None:
        bench.check(checksum(bench, path) == digest, f"{entry}: {path} is unchanged")
    bench.check(path not in common.trashed(bench), f"{entry}: nothing went to the trash from {path}")
