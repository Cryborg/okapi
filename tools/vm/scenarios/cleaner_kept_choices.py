"""Cleaner: the choices among the results, kept from one analysis to the next, or not.

« Tout désélectionner » holds for this analysis alone and leaves « Nettoyer »
inert; a group unchecked is kept unchecked (`selections.json`), a group
checked is never kept checked, nor is an administrator group kept at all;
an unreadable `selections.json` leaves the boxes as by default and the file
as it was.
"""

from __future__ import annotations

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["cleaner", "admin"]

AGING_SECONDS = 2 * 60 * 60
FILE_SIZE = 64 * 1024
ACCOUNT_FILES = ["~/.cache/thumbnails/normal/okapi-vm.png", "~/.cache/fontconfig/okapi-vm.cache-9"]
ADMIN_FILES = ["/var/crash/okapi-vm.crash"]
THUMBNAILS = "Cache des miniatures"
FONTS = "Cache des polices"
DESELECT = "Tout désélectionner"
NEW_ANALYSIS = "Nouvelle analyse"
SELECTED_PATTERN = r"^Éléments : (\d+) sur \d+$"
SELECTIONS = "selections.json"
UNREADABLE = "{"
UNREADABLE_PATTERN = r"^Vos choix précédents n'ont pas pu être lus dans .*selections\.json"
# Template of the thumbnails rule (crates/rules/data/linux/system.json), in its key.
THUMBNAILS_RULE = "${CACHE}/thumbnails"
ADMIN_PATH_PREFIX = "/var/"


def unchecked_keys(bench: Bench) -> list[list[str]]:
    kept = common.read_store(bench, SELECTIONS)
    return [] if kept is None else kept["unchecked"]  # type: ignore[index]


def again(bench: Bench) -> None:
    bench.ui.click(NEW_ANALYSIS, role=cleaner.BUTTON)
    bench.ui.wait(pattern=f"^({'|'.join(cleaner.ANALYSIS_ENDS)})$", timeout=cleaner.SCAN_TIMEOUT_SECONDS)
    bench.ui.wait(THUMBNAILS, role=cleaner.CHECK_BOX)


def show_system(bench: Bench) -> None:
    for name in ["Navigateurs", "Applications", "Jeux"]:
        cleaner.fold(bench, name)


def deselect_all(bench: Bench) -> None:
    bench.ui.click(DESELECT, role=cleaner.BUTTON)
    bench.ui.wait(pattern=r"^Éléments : 0 sur \d+$")
    checked = [node["name"] for node in bench.ui.find(role=cleaner.CHECK_BOX, state=cleaner.CHECKED)]
    # AT-SPI calls every button enabled: pressed, « Nettoyer » must open nothing.
    bench.ui.click(cleaner.CLEAN, role=cleaner.BUTTON)
    bench.ui.wait(DESELECT, role=cleaner.BUTTON)
    bench.check(not checked and not bench.ui.find(cleaner.CONFIRMATION) and common.read_store(bench, SELECTIONS) is None,
                f"CLN-017: all unchecked, « {cleaner.CLEAN} » opens nothing, {SELECTIONS} untouched ({checked})")
    again(bench)
    bench.check(cleaner.is_checked(bench, THUMBNAILS),
                "CLN-017: « Tout désélectionner » holds for that analysis alone: the next one checks as by default")


def kept_unchecked(bench: Bench) -> None:
    show_system(bench)
    cleaner.set_checked(bench, THUMBNAILS, False)
    cleaner.set_checked(bench, FONTS, True)
    bench.ui.wait(THUMBNAILS, role=cleaner.CHECK_BOX)
    keys = unchecked_keys(bench)
    again(bench)
    show_system(bench)
    thumbnails, fonts = cleaner.is_checked(bench, THUMBNAILS), cleaner.is_checked(bench, FONTS)
    bench.check(any(THUMBNAILS_RULE in key for key in keys) and not thumbnails and not fonts,
                f"CLN-018: the thumbnails unchecked are kept ({keys}) and stay unchecked; the fonts checked "
                f"start unchecked again ({thumbnails}, {fonts})")
    bench.screenshot("choix-retenus")


def admin_never_kept(bench: Bench) -> None:
    bench.allow_elevation_without_password()
    cleaner.obtain_rights(bench)
    show_system(bench)
    cleaner.set_checked(bench, cleaner.CRASH_REPORTS, True)
    cleaner.set_checked(bench, cleaner.CRASH_REPORTS, False)
    bench.ui.wait(cleaner.CRASH_REPORTS, role=cleaner.CHECK_BOX)
    keys = unchecked_keys(bench)
    bench.check(not any(part.startswith(ADMIN_PATH_PREFIX) for key in keys for part in key),
                f"CLN-018: an administrator group checked then unchecked is never kept ({keys})")


def unreadable_choices(bench: Bench) -> None:
    bench.stop_app()
    common.write_store(bench, SELECTIONS, {"version": 1, "unchecked": [["systemTarget", "Thumbnail Cache",
                                                                         THUMBNAILS_RULE]]})
    path = common.shell_path(common.store_path(SELECTIONS))
    bench.user(f"printf %s {UNREADABLE} > {path}")
    bench.launch_app()
    cleaner.open_page(bench)
    cleaner.analyze(bench)
    bench.ui.wait(pattern=UNREADABLE_PATTERN)
    show_system(bench)
    default = cleaner.is_checked(bench, THUMBNAILS)
    cleaner.set_checked(bench, THUMBNAILS, False)
    content = bench.user(f"cat -- {path}")
    bench.check(default and content == UNREADABLE,
                f"CLN-019: the banner said, the boxes as by default, {SELECTIONS} left as it was ({content!r})")
    bench.screenshot("choix-illisibles")


def run(bench: Bench) -> None:
    common.plant(bench, common.Files(bench.accounts.admin.name, ACCOUNT_FILES, size=FILE_SIZE),
                 common.Files(common.ROOT, ADMIN_FILES, size=FILE_SIZE), age_seconds=AGING_SECONDS)
    bench.launch_app()
    cleaner.open_page(bench)
    cleaner.analyze(bench)
    deselect_all(bench)
    kept_unchecked(bench)
    admin_never_kept(bench)
    unreadable_choices(bench)
