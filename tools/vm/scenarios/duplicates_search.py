"""Duplicates: where to look, what is compared, what is never searched, a search stopped, nothing found.

The whole home folder searched, a copy of a file of Documents planted in each
place a search never enters: a hidden folder, a project, `~/snap`, a
`$RECYCLE.BIN`, behind a link, on another disk, in an exclusion. Only the copy
outside the personal folders makes a group. Files of the same size but not
the same content make none. Then a search stopped while it compares, one
that finds nothing, and personal folders that `user-dirs.dirs` no longer names.
"""

from __future__ import annotations

from okapivm import common
from okapivm.bench import Bench
from scenarios import _user_files as files

TAGS = ["duplicates"]

SIZE = 3 * files.MEBIBYTE // 2
DOCUMENTS = f"{files.HOME}/Documents"
PICTURES = f"{files.HOME}/Images"
MUSIC = f"{files.HOME}/Musique"
OUTSIDE = f"{files.HOME}/ailleurs/outside.bin"
OUTSIDE_TARGET = "/var/tmp/okapi-vm-outside"
EXCLUDED = f"{files.HOME}/exclu"
FAT = f"{files.HOME}/cle-fat"
PROJECT = f"{files.HOME}/projet"
# name: (where its only copy lies, never searched)
NEVER_SEARCHED = {
    "hidden.bin": f"{files.HOME}/.cachette/hidden.bin",
    "project.bin": f"{PROJECT}/project.bin",
    "snap.bin": f"{files.HOME}/snap/app/snap.bin",
    "recycled.bin": f"{DOCUMENTS}/$RECYCLE.BIN/recycled.bin",
    "trashed.bin": f"{files.HOME}/.local/share/Trash/files/trashed.bin",
    "linked.bin": f"{OUTSIDE_TARGET}/linked.bin",
    "fat.bin": f"{FAT}/fat.bin",
    "excluded.bin": f"{EXCLUDED}/excluded.bin",
}
# What the journal says of each folder left out (crates/tools/src/listing/walk.rs).
LEFT_OUT = {
    f"{files.HOME}/.cachette": "hidden",
    f"{DOCUMENTS}/$RECYCLE.BIN": "trash",
    EXCLUDED: "excluded",
    f"{files.HOME}/snap": "excluded",
}
NOT_SEARCHED = "folder not searched"
PROJECT_NOT_SEARCHED = "project: not searched"
MOUNT_NOT_ENTERED = "mount not entered"
STOPPED_BY_THE_USER = "duplicate search stopped by the user"
STOPPED = "Recherche arrêtée. Rien n'a été gardé : une nouvelle recherche repart du début."
MANY = 300
MANY_SIZE = 100 * files.MEBIBYTE
COMPARING = r"^Comparaison du contenu… "
NO_FOLDER_FOUND = "Aucun dossier Documents, Images… trouvé : cochez « Dossier personnel » pour chercher."
NO_FOLDER_KNOWN = "Aucun dossier Documents, Images… connu : rien n'est coché d'avance."
NOTHING_FOUND = "Aucun doublon dans ces dossiers"
SEARCH_ELSEWHERE = "Chercher dans d'autres dossiers"


def group_named(bench: Bench, name: str) -> list[str]:
    return [row for row in files.groups(bench) if row.startswith(f"{name}, ")]


def run(bench: Bench) -> None:
    plant_never_searched(bench)
    plant_compared(bench)
    common.write_settings(bench, {"exclusions": [EXCLUDED]})
    bench.launch_app()
    where_to_look(bench)
    never_searched(bench)
    compared(bench)
    nothing_found(bench)
    stopped(bench)
    no_personal_folder(bench)


def plant_never_searched(bench: Bench) -> None:
    bench.root(f"install -d --owner={bench.accounts.admin.name} -- {OUTSIDE_TARGET}")
    common.mount_fat(bench, FAT)
    for name, copy in NEVER_SEARCHED.items():
        files.random_file(bench, f"{DOCUMENTS}/{name}", SIZE, copy)
    bench.user(f"mkdir -p -- {PROJECT}/.git && ln -s -- {OUTSIDE_TARGET} {DOCUMENTS}/lien")
    files.random_file(bench, f"{DOCUMENTS}/outside.bin", SIZE, OUTSIDE)
    bench.log("a copy of a file of Documents in each place never searched, one outside the personal folders")


def where_to_look(bench: Bench) -> None:
    """DBL-001."""
    common.open_screen(bench, files.DUPLICATES)
    bench.ui.wait("Où chercher ?")
    boxes = {name: bench.ui.node(name, role="check-box") for name in files.FOLDER_BOXES}
    bench.check(all(files.checked(box) and files.enabled(box) for box in boxes.values())
                and not files.checked(bench.ui.node(files.WHOLE_HOME, role="check-box")),
                f"DBL-001: every personal folder is checked, the home folder is not "
                f"({ {name: box['states'] for name, box in boxes.items()} })")
    bench.ui.click(files.WHOLE_HOME, role="check-box")
    boxes = {name: bench.ui.node(name, role="check-box") for name in files.FOLDER_BOXES}
    bench.screenshot("dossier-personnel")
    bench.check(all(files.checked(box) and not files.enabled(box) for box in boxes.values()),
                f"DBL-001: « Dossier personnel » checks every folder and shuts its box "
                f"({ {name: box['states'] for name, box in boxes.items()} })")


def never_searched(bench: Bench) -> None:
    """DBL-001, DBL-004."""
    mark = len(common.journal(bench))
    bench.ui.click("Rechercher", role="button")
    bench.ui.wait(pattern=r"^Groupes : ", role="label", timeout=files.SEARCH_TIMEOUT_SECONDS)
    bench.screenshot("dossier-personnel-groupes")
    outside = group_named(bench, "outside.bin")
    bench.check(len(outside) == 1 and ", 2 copies de " in outside[0],
                f"DBL-001: the whole home folder searched, the copy outside the personal folders found ({outside})")
    for name, copy in NEVER_SEARCHED.items():
        bench.check(not group_named(bench, name), f"DBL-004: no group of {name}, its copy at {copy} never searched")
    events = common.journal(bench)[mark:]
    said = {(event["fields"].get("path"), event["fields"].get("reason")) for event in events
            if event["fields"].get("message") == NOT_SEARCHED}
    for folder, reason in LEFT_OUT.items():
        bench.check((folder, reason) in said, f"DBL-004: the journal says {folder} was not searched: {reason}")
    messages = {(event["fields"].get("message"), event["fields"].get("path")) for event in events}
    bench.check((PROJECT_NOT_SEARCHED, PROJECT) in messages, f"DBL-004: the journal says {PROJECT} is a project")
    bench.check((MOUNT_NOT_ENTERED, FAT) in messages or (FAT, "mount") in said,
                f"DBL-004: the journal says {FAT}, another disk, was not entered")
    common.unmount(bench, FAT)


def plant_compared(bench: Bench) -> None:
    folder = f"{DOCUMENTS}/compare"
    files.random_file(bench, f"{folder}/same-1.bin", SIZE, f"{folder}/same-2.bin", f"{folder}/same-3.bin")
    files.random_file(bench, f"{folder}/sized-1.bin", SIZE)
    files.random_file(bench, f"{folder}/sized-2.bin", SIZE)
    files.random_file(bench, f"{folder}/ends-1.bin", SIZE, f"{folder}/ends-2.bin")
    bench.user(f"printf x | dd of={folder}/ends-2.bin bs=1 seek={SIZE // 2} conv=notrunc status=none")
    files.random_file(bench, f"{folder}/small-1.bin", files.MEBIBYTE - 1, f"{folder}/small-2.bin")
    bench.log("three copies, two files of one size, two files alike but in their middle, two copies under 1 MiB")


def compared(bench: Bench) -> None:
    """DBL-003."""
    same = group_named(bench, "same-1.bin") or group_named(bench, "same-2.bin") or group_named(bench, "same-3.bin")
    bench.check(len(same) == 1 and ", 3 copies de " in same[0], f"DBL-003: the three copies make one group ({same})")
    for name, why in [("sized", "the same size, not the same content"),
                      ("ends", "the same start and end, not the same middle"),
                      ("small", "under 1 MiB")]:
        found = group_named(bench, f"{name}-1.bin") + group_named(bench, f"{name}-2.bin")
        bench.check(not found, f"DBL-003: no group of two files of {why} ({found})")


def nothing_found(bench: Bench) -> None:
    """DBL-005."""
    files.random_file(bench, f"{MUSIC}/alone.bin", SIZE)
    files.search(bench, files.DUPLICATES, "Musique")
    examined = "Fichiers d'au moins 1,0 Mo examinés : 1."
    said = [bench.ui.wait(NOTHING_FOUND), bench.ui.wait(examined)]
    bench.screenshot("aucun-doublon")
    bench.check(all(said), f"DBL-005: « {NOTHING_FOUND} », « {examined} »")
    bench.ui.click(SEARCH_ELSEWHERE, role="button")
    bench.check(bool(bench.ui.wait("Où chercher ?")), f"DBL-005: « {SEARCH_ELSEWHERE} » goes back to the folders")


def stopped(bench: Bench) -> None:
    """DBL-002."""
    bench.user(f"mkdir -p -- {PICTURES}/many && for i in $(seq {MANY}); do "
               f"truncate --size={MANY_SIZE} -- {PICTURES}/many/$i.raw; done")
    bench.log(f"{MANY} holes of {MANY_SIZE} bytes in Images")
    listed = bench.user(f"find {PICTURES} -type f | wc -l").strip()
    files.choose_folders(bench, "Images")
    mark = len(common.journal(bench))
    bench.ui.click("Rechercher", role="button")
    comparing = bench.ui.wait(pattern=COMPARING, role="label", timeout=files.SEARCH_TIMEOUT_SECONDS)
    bench.screenshot("comparaison")
    listing = bench.ui.find(pattern=r"^Liste des fichiers", role="label")
    said = [label["name"] for label in listing]
    bench.check(bool(comparing), f"DBL-002: the search says « {comparing[0]['name']} »")
    bench.check(f"Liste des fichiers : terminée. Fichiers : {listed}" in said,
                f"DBL-002: the listing says it is done, with the {listed} files listed ({said})")
    bench.ui.click("Arrêter", role="button")
    bench.check(bool(bench.ui.wait(STOPPED)), f"DBL-002: « {STOPPED} »")
    bench.screenshot("arretee")
    messages = [event["fields"].get("message") for event in common.journal(bench)[mark:]]
    bench.check(STOPPED_BY_THE_USER in messages, "DBL-002: the journal says the user stopped the search")
    bench.user(f"rm -rf -- {PICTURES}/many")


def no_personal_folder(bench: Bench) -> None:
    """DBL-013."""
    bench.stop_app()
    common.write_user_dirs(bench, {})
    bench.launch_app()
    common.open_screen(bench, files.DUPLICATES)
    if not bench.ui.find("Où chercher ?"):
        bench.ui.click("Changer les dossiers", role="button")
    said = bench.ui.wait(NO_FOLDER_FOUND)
    bench.screenshot("aucun-dossier")
    boxes = [box["name"] for box in bench.ui.find(role="check-box")]
    bench.check(bool(said) and boxes == [files.WHOLE_HOME],
                f"DBL-013: « {NO_FOLDER_FOUND} », only the home folder offered ({boxes})")
    files.choose_folders(bench, whole_home=True)
    bench.ui.click("Rechercher", role="button")
    bench.ui.wait(pattern=r"^Groupes : ", role="label", timeout=files.SEARCH_TIMEOUT_SECONDS)
    asked = [box["name"] for box in bench.ui.find(role="check-box") if files.checked(box)]
    bench.check(not asked, f"DBL-013: nothing checked beforehand ({asked})")
    bench.ui.click("Sélection rapide", role="button")
    note = bench.ui.wait(NO_FOLDER_KNOWN)
    bench.screenshot("rien-coche-d-avance")
    bench.check(bool(note), f"DBL-013: the quick selection says « {NO_FOLDER_KNOWN} »")
    bench.key("Escape")
