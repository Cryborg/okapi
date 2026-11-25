"""Invariant 7, large files: a file leaves only for the trash, still the one the search read; one move at a time.

Large files of the administrator account, one of another account's in a
shared folder, one on an ext4 disk of its own mounted read-only. Between the
search and the move, files are rewritten, replaced, removed, given another
name: each stays. Then a move of two thousand duplicates and a move of a
large file are asked one right after the other: the journal shows the second
waiting for the first.
"""

from __future__ import annotations

from datetime import datetime

from okapivm import common
from okapivm.bench import Bench, ScenarioFailure
from scenarios import _user_files as files

TAGS = ["large-files", "duplicates", "invariant", "invariant-7", "trash"]

LARGE = 60 * files.MEBIBYTE
DOWNLOADS = f"{files.HOME}/Téléchargements"
DOCUMENTS = f"{files.HOME}/Documents"
DESKTOP = f"{files.HOME}/Bureau"
SHARED = f"{DOCUMENTS}/partage"
OTHER_DISK = f"{files.HOME}/Vidéos"
OTHER_DISK_IMAGE = "/var/tmp/okapi-vm-ext4.img"
OTHER_DISK_MEGABYTES = 200
ATTACKED = {
    "rewritten.iso": ("changed", "INV-708 (rewritten)"),
    "replaced.iso": ("changed", "INV-708 (replaced)"),
    "gone.iso": ("gone", "INV-708 (gone)"),
    "named.iso": ("changed", "INV-708 (one more name)"),
}
CONTROL = "control.iso"
RACE = "race.iso"
# Enough copies for their move to outlast the clicks that ask for the large file's.
MANY_COPIES = 6000
REFUSED_CLICK_TIMEOUT_SECONDS = 5
NOT_ANOTHER_USERS = "file of another user not kept"
CONFIRMED = "move to the trash confirmed"
DUPLICATE_TRASH = "duplicate_trash"
LARGE_FILE_TRASH = "large_file_trash"
INNER_TRASH = "trash"


def large(bench: Bench, *paths: str) -> None:
    """Files of `LARGE` bytes, their blocks on the disk (a hole is no large file)."""
    bench.user(" && ".join(f"fallocate -l {LARGE} -- {path}" for path in paths))


def run(bench: Bench) -> None:
    other = bench.accounts.other.name
    large(bench, *(f"{DOWNLOADS}/{name}" for name in [*ATTACKED, CONTROL, RACE]))
    bench.root(" && ".join([
        f"install -d --owner={other} --group={other} --mode=0755 {SHARED}",
        f"fallocate -l {LARGE} {SHARED}/other.iso && chown {other}: {SHARED}/other.iso && chmod 0644 {SHARED}/other.iso",
        f"truncate --size={OTHER_DISK_MEGABYTES}M {OTHER_DISK_IMAGE} && mkfs.ext4 -q {OTHER_DISK_IMAGE}",
        f"mount -o loop {OTHER_DISK_IMAGE} {OTHER_DISK}",
        f"fallocate -l {LARGE} {OTHER_DISK}/clip.mkv",
        f"chown -R {bench.accounts.admin.name}: {OTHER_DISK}",
        f"mount -o remount,ro {OTHER_DISK}",
    ]))
    bench.log("large files planted, another account's in a shared folder, one on a read-only ext4 disk")
    digests = {name: files.checksum(bench, f"{DOWNLOADS}/{name}") for name in [*ATTACKED, CONTROL]}

    bench.launch_app()
    files.search(bench, files.LARGE_FILES, "Documents", "Vidéos", "Téléchargements")
    bench.screenshot("gros-fichiers")

    # INV-709: another account's file, readable, is never listed.
    bench.check(not bench.ui.find(pattern=files.file_row(f"{SHARED}/other.iso"), role="check-box"),
                "INV-709: the other account's large file is not listed")
    passed = [event["fields"]["path"] for event in common.journal_events(bench, NOT_ANOTHER_USERS)]
    bench.check(f"{SHARED}/other.iso" in passed, f"INV-709: the journal leaves it out as another user's ({passed})")

    # INV-710: a file on another disk than the trash cannot be checked.
    device = bench.user(f"stat -c %d -- {OTHER_DISK}/clip.mkv {files.HOME}").split()
    bench.check(device[0] != device[1], f"INV-710: clip.mkv lies on another disk ({device})")
    clip = bench.ui.node(pattern=files.file_row(f"{OTHER_DISK}/clip.mkv"), role="check-box")
    bench.check(not files.enabled(clip) and clip["name"].endswith("case désactivée : Pas de corbeille sur ce disque"),
                f"INV-710: its box is shut, « Pas de corbeille sur ce disque » ({clip['name']})")
    try:
        bench.ui.click(clip["name"], role="check-box", timeout=REFUSED_CLICK_TIMEOUT_SECONDS)
        refused = False
    except ScenarioFailure:
        refused = True
    bench.check(refused, "INV-710: its box cannot be checked")

    for name in [*ATTACKED, CONTROL]:
        bench.ui.click(pattern=files.file_row(f"{DOWNLOADS}/{name}"), role="check-box")
    bench.user(" && ".join([
        f"printf x | dd of={DOWNLOADS}/rewritten.iso bs=1 seek=4096 conv=notrunc status=none",
        f"rm -- {DOWNLOADS}/replaced.iso",
        f"fallocate -l {LARGE} -- {DOWNLOADS}/replaced.iso",
        f"rm -- {DOWNLOADS}/gone.iso",
        f"ln -- {DOWNLOADS}/named.iso {DOCUMENTS}/named-again.iso",
    ]))
    bench.log("files changed after the search")
    digests["rewritten.iso"] = files.checksum(bench, f"{DOWNLOADS}/rewritten.iso")
    digests["replaced.iso"] = files.checksum(bench, f"{DOWNLOADS}/replaced.iso")
    mark = files.trash_checked(bench)
    bench.screenshot("gros-fichiers-recu")
    done = files.outcomes(bench, mark, moved=files.FILE_MOVED, kept=files.FILE_KEPT)
    for name, (reason, entry) in ATTACKED.items():
        path = f"{DOWNLOADS}/{name}"
        bench.check(done.get(path) == reason, f"{entry}: {name} kept: {reason} (journal: {done.get(path)})")
        if reason == "gone":
            bench.check(path not in common.trashed(bench), f"{entry}: nothing of it in the trash")
        else:
            files.check_kept_whole(bench, entry, path, digests[name])
    bench.check(done.get(f"{DOWNLOADS}/{CONTROL}") == files.TRASHED, "INV-708: the control file went to the trash")
    files.check_trashed(bench, "INV-708", f"{DOWNLOADS}/{CONTROL}", digests[CONTROL])
    files.check_kept_whole(bench, "INV-709", f"{SHARED}/other.iso")
    files.check_kept_whole(bench, "INV-710", f"{OTHER_DISK}/clip.mkv")

    one_move_at_a_time(bench)


def one_move_at_a_time(bench: Bench) -> None:
    """INV-716: a large file's move asked while duplicates move waits for them."""
    bench.user(" && ".join([
        f"mkdir -p -- {DESKTOP}/many",
        f"truncate --size={files.MEBIBYTE} -- {DOCUMENTS}/many.raw",
        f"seq {MANY_COPIES} | sed 's|$|.raw|; s|^|{DESKTOP}/many/|' | xargs truncate --size={files.MEBIBYTE} --",
    ]))
    bench.log(f"{MANY_COPIES} copies of a hole of 1 MiB on the Desktop, one in Documents")
    race_digest = files.checksum(bench, f"{DOWNLOADS}/{RACE}")
    files.search_again(bench, done=r"^\d+ fichiers? de plus de ")
    bench.ui.click(pattern=files.file_row(f"{DOWNLOADS}/{RACE}"), role="check-box")
    files.search(bench, files.DUPLICATES, "Documents", "Bureau")
    mark = len(common.journal_events(bench))
    bench.ui.click(files.TRASH_BUTTON, role="button")
    bench.ui.wait(pattern=r"^Mettre .*à la corbeille \?$")
    bench.ui.click(files.TRASH_BUTTON, role="button", index=files.CONFIRM_INDEX)
    common.open_screen(bench, files.LARGE_FILES)
    bench.ui.click(files.TRASH_BUTTON, role="button")
    bench.ui.wait(pattern=r"^Mettre .*à la corbeille \?$")
    bench.ui.click(files.TRASH_BUTTON, role="button", index=files.CONFIRM_INDEX)
    bench.ui.wait(pattern=files.RECEIPT, role="label", timeout=files.TRASH_TIMEOUT_SECONDS)
    common.open_screen(bench, files.DUPLICATES)
    bench.ui.wait(pattern=files.RECEIPT, role="label", timeout=files.TRASH_TIMEOUT_SECONDS)
    bench.screenshot("deux-deplacements")

    events = common.journal_events(bench)[mark:]

    def moment(event: dict) -> datetime:
        return datetime.fromisoformat(event["timestamp"])

    def of(tool: str, message: str | None = None, inner: bool = False) -> list[dict]:
        chosen = []
        for event in events:
            spans = [span["name"] for span in event.get("spans", [])]
            if spans[:1] != [tool] or (inner and INNER_TRASH not in spans):
                continue
            if message is None or event["fields"].get("message") == message:
                chosen.append(event)
        return chosen

    duplicates_moves = of(DUPLICATE_TRASH, inner=True)
    large_moves = of(LARGE_FILE_TRASH, inner=True)
    large_confirmed = [event for event in events
                       if event["fields"].get("message") == CONFIRMED and event["target"].endswith("large_files")]
    bench.check(bool(duplicates_moves) and bool(large_moves), "INV-716: both moves are in the journal")
    duplicates_end = max(moment(event) for event in duplicates_moves)
    duplicates_start = min(moment(event) for event in duplicates_moves)
    large_start = min(moment(event) for event in large_moves)
    large_end = max(moment(event) for event in large_moves)
    bench.check(large_start > duplicates_end or duplicates_start > large_end,
                f"INV-716: the two moves never overlap (duplicates {duplicates_start} to {duplicates_end}, "
                f"large file {large_start} to {large_end})")
    asked = moment(large_confirmed[-1]) if large_confirmed else None
    raced = asked is not None and asked < duplicates_end
    bench.log(f"INV-716: the large file's move was asked {'while' if raced else 'after'} the duplicates moved "
              f"(asked {asked}, duplicates done {duplicates_end})")
    bench.check(raced, "INV-716: the large file's move was asked while the duplicates moved, and waited")
    files.check_trashed(bench, "INV-716", f"{DOWNLOADS}/{RACE}", race_digest)
