"""Duplicates: moving to the trash, confirmed, its receipt; a trash that cannot open; showing a copy in Files.

A trash that is a plain file first: nothing moves, the screen says so. Then
the real move: the confirmation counts the files, the receipt says what left,
`gio trash` holds each copy whole and the keepers stay. Last, « Montrer dans
le dossier » opens Files on a copy, and says so when that copy is gone.
"""

from __future__ import annotations

from okapivm import common
from okapivm.bench import Bench
from scenarios import _user_files as files

TAGS = ["duplicates", "trash"]

# Sizes are said in decimal units: 2,0 Mo each, 6,0 Mo for the three asked for.
SIZE = 2_000_000
DOWNLOADS = f"{files.HOME}/Téléchargements"
DOCUMENTS = f"{files.HOME}/Documents"
TRASH = "~/.local/share/Trash"
TRIO = ["trio.bin", "trio-2.bin"]
PAIR = "pair.bin"
SHOWN = "shown.bin"
TRASH_UNOPENED = "La corbeille n'a pas pu être ouverte : rien n'a bougé. Le détail est dans le journal."
NOT_SHOWN = "Le gestionnaire de fichiers n'a pas pu montrer ce fichier : il a peut-être changé depuis la recherche."
SHOW = "Montrer dans le dossier"
KEPT_IN_EACH_GROUP = "Dans chaque groupe, au moins une copie reste à sa place."
FILE_MANAGER = "nautilus"
SHOWN_IN_FILES = "shown in the file manager"
FILE_MANAGER_TIMEOUT_SECONDS = 30


def run(bench: Bench) -> None:
    files.random_file(bench, f"{DOCUMENTS}/trio.bin", SIZE, *(f"{DOWNLOADS}/{name}" for name in TRIO))
    files.random_file(bench, f"{DOCUMENTS}/{PAIR}", SIZE, f"{DOWNLOADS}/{PAIR}")
    asked = [f"{DOWNLOADS}/{name}" for name in [*TRIO, PAIR]]
    keepers = [f"{DOCUMENTS}/trio.bin", f"{DOCUMENTS}/{PAIR}"]
    digests = {path: files.checksum(bench, path) for path in asked}
    bench.user(f"rm -rf -- {common.shell_path(TRASH)} && mkdir -p -- ~/.local/share && : > {common.shell_path(TRASH)}")
    bench.log("the trash is a plain file")

    bench.launch_app()
    files.search(bench, files.DUPLICATES, "Documents", "Téléchargements")
    trash_unopened(bench, asked, keepers)

    bench.user(f"rm -- {common.shell_path(TRASH)}")
    bench.log("the trash is no longer a file")
    moved(bench, asked, keepers, digests)
    show_in_folder(bench)


def trash_unopened(bench: Bench, asked: list[str], keepers: list[str]) -> None:
    """DBL-012."""
    mark = len(common.journal_events(bench))
    bench.ui.click(files.TRASH_BUTTON, role="button")
    bench.ui.wait("Mettre à la corbeille ?")
    bench.ui.click(files.TRASH_BUTTON, role="button", index=files.CONFIRM_INDEX)
    bench.ui.wait(TRASH_UNOPENED, timeout=files.TRASH_TIMEOUT_SECONDS)
    bench.screenshot("corbeille-illisible")
    said = [event["fields"]["message"] for event in common.journal_events(bench)[mark:]]
    bench.check("trash not opened: nothing moved" in said, f"DBL-012: the journal says the trash did not open ({said})")
    common.check_kept(bench, common.Files(bench.accounts.admin.name, [*asked, *keepers]))
    is_file = bench.execute(bench.accounts.admin.name, f"[ -f {common.shell_path(TRASH)} ]").code == 0
    bench.check(is_file, "DBL-012: nothing moved, the trash still the plain file it was")


def moved(bench: Bench, asked: list[str], keepers: list[str], digests: dict[str, str]) -> None:
    """DBL-010."""
    files.search_again(bench)
    for path in asked:
        group = PAIR if path.endswith(PAIR) else "trio.bin"
        files.set_copy(bench, group, path, checked=True)
    files.focus_group(bench, "trio.bin")
    details = "Vers la corbeille : 2. Gardées : 1."
    bench.check(bool(bench.ui.wait(details)), f"DBL-010: the group's details say « {details} »")
    mark = len(common.journal_events(bench))
    bench.ui.click(files.TRASH_BUTTON, role="button")
    total = f"Fichiers : {len(asked)} (6,0 Mo). La place ne sera libérée qu'en vidant la corbeille."
    said = [bench.ui.wait(text) for text in ("Mettre à la corbeille ?", total, KEPT_IN_EACH_GROUP)]
    bench.screenshot("confirmation")
    bench.check(all(said), f"DBL-010: the confirmation says « Mettre à la corbeille ? », « {total} »")
    bench.ui.click(files.TRASH_BUTTON, role="button", index=files.CONFIRM_INDEX)
    receipt = [bench.ui.wait("6,0 Mo mis à la corbeille", role="label", timeout=files.TRASH_TIMEOUT_SECONDS),
               bench.ui.wait(pattern=f"^Fichiers : {len(asked)}\\. ", role="label")]
    bench.screenshot("recu")
    bench.check(all(receipt), f"DBL-010: the receipt says « 6,0 Mo mis à la corbeille », « Fichiers : {len(asked)}. »")
    done = files.outcomes(bench, mark)
    bench.check(all(done.get(path) == files.TRASHED for path in asked),
                f"DBL-010: the journal says each copy asked for went to the trash ({done})")
    for path in asked:
        files.check_trashed(bench, "DBL-010", path, digests[path])
    common.check_kept(bench, common.Files(bench.accounts.admin.name, keepers))


def show_in_folder(bench: Bench) -> None:
    """DBL-011."""
    files.random_file(bench, f"{DOCUMENTS}/{SHOWN}", SIZE, f"{DOWNLOADS}/{SHOWN}")
    files.search_again(bench)
    files.focus_copy(bench, SHOWN, f"{DOCUMENTS}/{SHOWN}")
    mark = len(common.journal_events(bench))
    bench.ui.click(SHOW, role="button")
    # The portal answers once Files shows the copy, a while after the click.
    answered = bench.execute(bench.accounts.admin.name,
                             f"for attempt in $(seq {FILE_MANAGER_TIMEOUT_SECONDS}); do "
                             f"if grep -q ':\"{SHOWN_IN_FILES}\"' {common.shell_path(common.APP_JOURNAL)}; "
                             f"then exit 0; fi; sleep 1; done; exit 1").code == 0
    bench.screenshot("fichiers")
    said = [event["fields"]["message"] for event in common.journal_events(bench)[mark:]]
    running = bench.execute(bench.accounts.admin.name, f"pgrep -u \"$USER\" -x {FILE_MANAGER}").code == 0
    bench.check(answered and SHOWN_IN_FILES in said and running,
                f"DBL-011: Files was asked, opened, and showed the copy ({said})")
    bench.user(f"pkill -u \"$USER\" -x {FILE_MANAGER}; true")

    bench.user(f"rm -- {DOCUMENTS}/{SHOWN}")
    mark = len(common.journal_events(bench))
    bench.ui.click(SHOW, role="button")
    bench.ui.wait(NOT_SHOWN)
    bench.screenshot("pas-montre")
    said = [event["fields"]["message"] for event in common.journal_events(bench)[mark:]]
    bench.check("not shown in the file manager" in said,
                f"DBL-011: a copy gone since the search is not shown, and the screen says so ({said})")
