"""Invariant 7, duplicates: a copy leaves only for the trash, and never once the last sure copy is gone.

Two searches. The first offers keepers that cannot surely keep anything: on
FAT, the same file again through bindfs, another account's, in the trash, in
a `$RECYCLE.BIN`. The second changes files between the search and the move:
the keeper written, removed or replaced by another file of the same name;
the copy asked for written, replaced or removed; its folder swapped for a
link. Each copy asked for must stay where it is, for the reason the journal
gives; a control copy goes to the trash and comes back whole from it.
"""

from __future__ import annotations

import re

from okapivm import common
from okapivm.bench import Bench, ScenarioFailure
from scenarios import _user_files as files

TAGS = ["duplicates", "invariant", "invariant-7", "trash"]

SIZE = 3 * files.MEBIBYTE // 2
DOWNLOADS = f"{files.HOME}/Téléchargements"
DOCUMENTS = f"{files.HOME}/Documents"
FAT = f"{files.HOME}/Musique"
ALIAS_SOURCE = f"{DOWNLOADS}/alias"
ALIAS = f"{files.HOME}/Vidéos"
SHARED = f"{DOCUMENTS}/partage"
TRASH = f"{files.HOME}/.local/share/Trash"
TRASH_FILES = f"{TRASH}/files"
RECYCLE_BIN = f"{files.HOME}/cle/$RECYCLE.BIN"
# The trash specification writes a path URL-encoded.
TRASHINFO_DOWNLOADS = "/home/tester/T%C3%A9l%C3%A9chargements"
UNSURE_FOLDERS = {
    "DOCUMENTS": "$HOME/Documents",
    "DOWNLOAD": "$HOME/Téléchargements",
    "MUSIC": "$HOME/Musique",
    "VIDEOS": "$HOME/Vidéos",
    "PICTURES": "$HOME/.local/share/Trash/files",
    "DESKTOP": "$HOME/cle/$RECYCLE.BIN",
}
NO_SURE_KEEPER = "noSureKeeper"
NO_SURE_KEEPER_SAID = "Pas d'autre copie sûre"
KEEPER_CHANGED = "keeperChanged"
CHANGED = "changed"
GONE = "gone"
REFUSED_CLICK_TIMEOUT_SECONDS = 5


def run(bench: Bench) -> None:
    unsure_keepers(bench)
    changed_since_the_search(bench)


def unsure_keepers(bench: Bench) -> None:
    """INV-704, 705, 706: no keeper the move could not trust."""
    other = bench.accounts.other.name
    bench.root(f"install -d --owner={other} --group={other} --mode=0755 {SHARED}")
    common.mount_fat(bench, FAT)
    files.random_file(bench, f"{ALIAS_SOURCE}/aliased.bin", SIZE)
    common.bind_mount(bench, ALIAS_SOURCE, ALIAS, fuse=True)
    # name: (its only other copy, why the journal finds it unsure, the entry)
    keepers = {
        "fat.bin": (f"{FAT}/fat.bin", "undated", "INV-706 (FAT)"),
        "aliased.bin": (f"{ALIAS}/aliased.bin", "alias", "INV-706 (bindfs)"),
        "other.bin": (f"{SHARED}/other.bin", "owner", "INV-704"),
        "trashed.bin": (f"{TRASH_FILES}/trashed.bin", "trash", "INV-705 (trash)"),
        "recycled.bin": (f"{RECYCLE_BIN}/recycled.bin", "trash", "INV-705 ($RECYCLE.BIN)"),
    }
    asked = {name: f"{DOWNLOADS}/{name}" for name in [*keepers, "control.bin"]}
    asked["aliased.bin"] = f"{ALIAS_SOURCE}/aliased.bin"
    files.random_file(bench, asked["fat.bin"], SIZE, keepers["fat.bin"][0])
    files.random_file(bench, asked["trashed.bin"], SIZE, keepers["trashed.bin"][0])
    files.random_file(bench, asked["recycled.bin"], SIZE, keepers["recycled.bin"][0])
    files.random_file(bench, asked["other.bin"], SIZE)
    shared = keepers["other.bin"][0]
    bench.root(f"cp -- {asked['other.bin']} {shared} && chown {other}: {shared} && chmod 0644 {shared}")
    files.random_file(bench, asked["control.bin"], SIZE, f"{DOCUMENTS}/control.bin")
    digests = {name: files.checksum(bench, path) for name, path in asked.items()}
    owner = bench.user(f"stat -c %U -- {shared}").strip()
    bench.check(owner == other, f"INV-704: the shared copy is {owner}'s")

    common.write_user_dirs(bench, UNSURE_FOLDERS)
    bench.launch_app()
    files.search(bench, files.DUPLICATES)
    bench.screenshot("gardiennes-douteuses")
    for name, path in asked.items():
        files.set_copy(bench, name, path, checked=True)
    mark = files.trash_checked(bench)
    bench.screenshot("gardiennes-douteuses-recu")
    done = files.outcomes(bench, mark)
    unsure = files.passed_over(bench, mark)

    for name, (keeper, why, entry) in keepers.items():
        path = asked[name]
        bench.check(done.get(path) == NO_SURE_KEEPER,
                    f"{entry}: {files.shown(path)} kept for want of a sure keeper (journal: {done.get(path)})")
        bench.check(unsure.get(path) == why,
                    f"{entry}: its keeper {files.shown(keeper)} judged unsure: {why} (journal: {unsure.get(path)})")
        files.check_kept_whole(bench, entry, path, digests[name])
    bench.check(done.get(asked["control.bin"]) == files.TRASHED,
                "INV-704 to 706: the control copy, its keeper sure, did go to the trash")
    files.check_trashed(bench, "INV-704 to 706", asked["control.bin"], digests["control.bin"])
    review = f"Fichiers à revoir : {len(keepers)}"
    said = [bool(bench.ui.find(pattern=f"^{re.escape(name)}, {NO_SURE_KEEPER_SAID}, ", role="list-item"))
            for name in keepers]
    bench.check(bool(bench.ui.find(review, role="list-box")) and all(said),
                f"INV-704 to 706: the receipt says « {review} », each « {NO_SURE_KEEPER_SAID} »")

    bench.stop_app()
    common.unmount(bench, ALIAS)
    common.unmount(bench, FAT)
    bench.root(f"rm -rf -- {SHARED} {files.HOME}/cle")
    bench.user(f"rm -rf -- {DOWNLOADS} {DOCUMENTS} {TRASH} && mkdir -- {DOWNLOADS} {DOCUMENTS}")


def changed_since_the_search(bench: Bench) -> None:
    """INV-701, 702, 703, 707, 718: what changes between the search and the move."""
    expected = {
        "keeper-written.bin": (KEEPER_CHANGED, "INV-702 (keeper written)"),
        "keeper-removed.bin": (KEEPER_CHANGED, "INV-702 (keeper removed)"),
        "keeper-replaced.bin": (KEEPER_CHANGED, "INV-702 (keeper replaced by another file of its name)"),
        "written.bin": (CHANGED, "INV-703 (written)"),
        "replaced.bin": (CHANGED, "INV-703 (replaced)"),
        "removed.bin": (GONE, "INV-703 (removed)"),
    }
    restored = f"{DOWNLOADS}/restored.bin"
    for name in [*expected, "restored.bin"]:
        files.random_file(bench, f"{DOWNLOADS}/{name}", SIZE, f"{DOCUMENTS}/{name}")
    linked = f"{DOWNLOADS}/folder/linked.bin"
    files.random_file(bench, linked, SIZE, f"{DOCUMENTS}/linked.bin")
    pair = [f"{DOWNLOADS}/pair-1.bin", f"{DOWNLOADS}/pair-2.bin"]
    files.random_file(bench, pair[0], SIZE, pair[1])
    digest = {name: files.checksum(bench, f"{DOWNLOADS}/{name}") for name in [*expected, "restored.bin"]}
    linked_digest = files.checksum(bench, linked)
    pair_digest = files.checksum(bench, pair[0])

    common.write_user_dirs(bench, files.STANDARD_FOLDERS)
    bench.launch_app()
    files.search(bench, files.DUPLICATES, "Documents", "Téléchargements")

    # INV-701: of a group whose copies all lie in Downloads, one stays unchecked, its box shut.
    boxes = [files.copy_box(bench, "pair-1.bin", path) for path in pair]
    kept = [box for box in boxes if not files.checked(box)]
    bench.check(len(kept) == 1 and not files.enabled(kept[0])
                and kept[0].get("description") == "Une copie reste toujours",
                f"INV-701: one copy of the pair stays unchecked, its box shut ({[box['name'] for box in boxes]})")
    try:
        bench.ui.click(kept[0]["name"], role="check-box", timeout=REFUSED_CLICK_TIMEOUT_SECONDS)
        refused = False
    except ScenarioFailure:
        refused = True
    bench.check(refused and not files.checked(bench.ui.node(kept[0]["name"], role="check-box")),
                "INV-701: the last copy's box cannot be checked")
    for name in [*expected, "restored.bin"]:
        files.set_copy(bench, name, f"{DOWNLOADS}/{name}", checked=True)
    files.set_copy(bench, "linked.bin", linked, checked=True)

    bench.user(" && ".join([
        f"printf x >> {DOCUMENTS}/keeper-written.bin",
        f"rm -- {DOCUMENTS}/keeper-removed.bin",
        f"rm -- {DOCUMENTS}/keeper-replaced.bin",
        f"head -c {SIZE} /dev/urandom > {DOCUMENTS}/keeper-replaced.bin",
        f"printf x >> {DOWNLOADS}/written.bin",
        f"cp -- {DOWNLOADS}/replaced.bin {DOWNLOADS}/replaced.new",
        f"mv -- {DOWNLOADS}/replaced.new {DOWNLOADS}/replaced.bin",
        f"rm -- {DOWNLOADS}/removed.bin",
        f"mv -- {DOWNLOADS}/folder {DOWNLOADS}/folder.real",
        f"ln -s folder.real {DOWNLOADS}/folder",
    ]))
    bench.log("files changed after the search")
    digest["written.bin"] = files.checksum(bench, f"{DOWNLOADS}/written.bin")
    mark = files.trash_checked(bench)
    bench.screenshot("changements-recu")
    done = files.outcomes(bench, mark)

    for name, (reason, entry) in expected.items():
        path = f"{DOWNLOADS}/{name}"
        bench.check(done.get(path) == reason,
                    f"{entry}: {files.shown(path)} kept: {reason} (journal: {done.get(path)})")
        if reason == GONE:
            bench.check(path not in common.trashed(bench), f"{entry}: nothing of it in the trash")
        else:
            files.check_kept_whole(bench, entry, path, digest[name])
    bench.check(done.get(linked) not in (None, files.TRASHED),
                f"INV-707: the copy behind a folder swapped for a link is kept (journal: {done.get(linked)})")
    files.check_kept_whole(bench, "INV-707", f"{DOWNLOADS}/folder.real/linked.bin", linked_digest)
    bench.check(linked not in common.trashed(bench), "INV-707: the link was not followed into the trash")
    left = [path for path in pair if bench.execute(bench.accounts.admin.name, f"[ -f {path} ]").code == 0]
    bench.check(len(left) == 1 and files.checksum(bench, left[0]) == pair_digest,
                f"INV-701: one copy of the pair is still at its place, whole ({left})")
    review = f"Fichiers à revoir : {len(expected) + 1}"
    bench.check(bool(bench.ui.find(review, role="list-box")), f"INV-702, 703, 707: the receipt says « {review} »")

    bench.check(done.get(restored) == files.TRASHED, "INV-718: the control copy went to the trash")
    files.check_trashed(bench, "INV-718", restored, digest["restored.bin"])
    name = common.trashed(bench)[restored].removeprefix(files.TRASH_URI)
    info = bench.user(f"cat -- {TRASH}/info/{name}.trashinfo")
    bench.check(f"Path={TRASHINFO_DOWNLOADS}/restored.bin" in info and "DeletionDate=" in info,
                f"INV-718: its .trashinfo names where it was and when ({info.strip()!r})")
    bench.user(f"gio trash --restore {files.TRASH_URI}{name}")
    bench.check(files.checksum(bench, restored) == digest["restored.bin"] and restored not in common.trashed(bench),
                "INV-718: restored by gio trash --restore, back at its place, its content whole")
