"""Invariant 3: links, mounts and special files in a target are never followed nor removed.

By the CLI: a link planted in a target towards `~/Documents` (INV-301), a
root that is a link (INV-303), a target seen again through a bind mount the
user excludes (INV-308), a FIFO, a socket and a device (INV-309). Then by
the interface, between the analysis and the cleaning: an ancestor of a target
swapped for a link (INV-302), a mount laid over a folder offered (INV-305);
and for root, a link to `/etc` planted in `/var/crash` (INV-304) and an
exclusion written through a link into `/var/crash` (INV-306). Every
`~/Documents` file is old enough to go: only following a link would take it.
"""

from __future__ import annotations

import re
import shlex

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["invariants", "admin", "helper", "mounts"]

AGING_SECONDS = 2 * 60 * 60
SIZE = 4096
CRASH = "/var/crash"
KEPT_RESULTS = {"kept", "partial"}


def home(bench: Bench) -> str:
    return f"/home/{bench.accounts.admin.name}"


def is_link(bench: Bench, path: str) -> bool:
    return bench.execute(bench.accounts.admin.name, f"[ -L {common.shell_path(path)} ]").code == 0


def etc_listing(bench: Bench) -> str:
    return bench.root("find /etc -xdev -printf '%p %i\\n' | sort | sha256sum")


def entries(document: dict) -> list[str]:
    return [entry["path"] for group in document["groups"] for entry in group["entries"]]


def by_the_cli(bench: Bench, documents: common.Files) -> None:
    user = bench.accounts.admin.name
    base = home(bench)
    thumbnail = common.Files(user, ["~/.cache/thumbnails/normal/okapi-vm.png"], SIZE)
    tracker = common.Files(user, ["~/.cache/tracker3/okapi-vm.db"], SIZE)
    mirrored = common.Files(user, ["~/.cache/tracker/okapi-vm.db"], SIZE)
    special = common.Files(user, [f"~/.cache/tracker3/okapi-vm-{kind}" for kind in ("fifo", "socket", "device")])
    common.plant(bench, documents, thumbnail, tracker, mirrored)
    bench.user(f"ln -s -- {shlex.quote(base)}/Documents ~/.cache/thumbnails/okapi-vm-trap")
    bench.user(f"rm -rf -- ~/.cache/gstreamer-1.0 && ln -s -- {shlex.quote(base)}/Documents ~/.cache/gstreamer-1.0")
    bench.user("mkfifo ~/.cache/tracker3/okapi-vm-fifo && python3 -c 'import socket, sys; "
               "socket.socket(socket.AF_UNIX).bind(sys.argv[1])' ~/.cache/tracker3/okapi-vm-socket")
    device = f"{base}/.cache/tracker3/okapi-vm-device"
    bench.root(f"mknod {shlex.quote(device)} c 1 3 && chown -h {user}: {shlex.quote(device)}")
    common.bind_mount(bench, f"{base}/.cache/tracker", f"{base}/okapi-vm-mirror")
    bench.advance_clock(AGING_SECONDS)

    document = common.cli_json(bench, "--clean")
    offered = entries(document)
    bench.log(f"offered by the CLI: {[path for path in offered if path.startswith(base)]}")
    removed = {outcome["path"] for outcome in document["clean"]["outcomes"] if outcome["result"] == "removed"}
    bench.check(thumbnail.paths[0].replace("~", base, 1).rsplit("/", 1)[0] in removed
                and f"{base}/.cache/tracker3/okapi-vm.db" in removed,
                f"the cleaning ran: the old thumbnail and tracker file went ({sorted(removed)})")

    bench.check(not any(path.startswith(f"{base}/Documents") or path.endswith("okapi-vm-trap") for path in offered),
                "INV-301: the link planted in the thumbnails is neither followed nor offered")
    bench.check(is_link(bench, "~/.cache/thumbnails/okapi-vm-trap"), "INV-301: the link itself is left")
    unsafe = [issue["detail"] for issue in document["issues"]
              if issue["detail"].startswith("UnsafeRoot") and f'"{base}/.cache/gstreamer-1.0"' in issue["detail"]]
    bench.check(bool(unsafe) and re.search(r"reason: Link\b", unsafe[0]) is not None,
                f"INV-303: a root that is a link is refused as such ({unsafe})")
    bench.check(not any(path.startswith(f"{base}/.cache/gstreamer-1.0") for path in offered),
                "INV-303: nothing is offered from the root that is a link")
    bench.check(common.present(bench, documents) == list(documents.paths),
                "INV-301, INV-303: every file of ~/Documents is still there")
    bench.check(common.present(bench, mirrored) == list(mirrored.paths)
                and not any(path.startswith(f"{base}/.cache/tracker/") for path in removed),
                "INV-308: the target the excluded bind mount shows again is kept whole")
    bench.check(common.present(bench, special) == list(special.paths)
                and not any(path in offered for path in (path.replace("~", base, 1) for path in special.paths)),
                "INV-309: the FIFO, the socket and the device are neither offered nor removed")
    common.unmount(bench, f"{base}/okapi-vm-mirror")


def by_the_interface(bench: Bench, documents: common.Files) -> None:
    user = bench.accounts.admin.name
    base = home(bench)
    swapped = common.Files(user, ["~/.cache/thumbnails/large/okapi-vm.png"], SIZE)
    covered = common.Files(user, ["~/.cache/tracker3/okapi-vm-covered/okapi-vm.db"], SIZE)
    guarded = common.Files(common.ROOT, [f"{CRASH}/okapi-vm-guarded/okapi-vm.crash"], SIZE)
    removable = common.Files(common.ROOT, [f"{CRASH}/okapi-vm.crash"], SIZE)
    common.plant(bench, swapped, covered, guarded, removable)
    bench.user(f"ln -s /etc {CRASH}/okapi-vm-etc")
    bench.user(f"ln -s {CRASH}/okapi-vm-guarded ~/okapi-vm-crash-link")
    bench.advance_clock(AGING_SECONDS)
    etc_before = etc_listing(bench)

    cleaner.scan(bench)
    cleaner.obtain_rights(bench)
    cleaner.check_groups(bench, cleaner.CRASH_REPORTS)
    bench.screenshot("droits-obtenus")

    bench.user(f"mv ~/.cache/thumbnails ~/.cache/okapi-vm-thumbnails && ln -s -- {shlex.quote(base)}/Documents "
               "~/.cache/thumbnails")
    common.bind_mount(bench, f"{base}/Documents", f"{base}/.cache/tracker3/okapi-vm-covered")
    cleaner.clean(bench)
    bench.screenshot("recu")
    common.unmount(bench, f"{base}/.cache/tracker3/okapi-vm-covered")

    deletions = {entry.get("path"): entry for entry in common.last_deletions(bench)}
    bench.log(f"deletion log: {deletions}")
    receipt = common.last_receipt(bench)
    bench.check(bool(receipt.get("admin", {}).get("done")), f"the administrator's part ended as asked ({receipt})")
    common.check_gone(bench, removable)
    bench.check(common.present(bench, documents) == list(documents.paths),
                "INV-302, INV-305: every file of ~/Documents is still there")
    swapped_entry = deletions.get(f"{base}/.cache/thumbnails/large")
    bench.check(swapped_entry is not None and swapped_entry["result"] in KEPT_RESULTS,
                f"INV-302: the folder behind the ancestor swapped for a link is kept ({swapped_entry})")
    bench.check(bool(common.present(bench, common.Files(user, ["~/.cache/okapi-vm-thumbnails/large/okapi-vm.png"]))),
                "INV-302: the thumbnail moved away with its folder is still there")
    bench.check(common.present(bench, covered) == list(covered.paths),
                "INV-305: the folder a mount was laid over is kept, the mount never entered")
    bench.check(is_link(bench, f"{CRASH}/okapi-vm-etc") and etc_listing(bench) == etc_before,
                "INV-304: root leaves the link to /etc planted in /var/crash, and /etc unchanged")
    bench.check(deletions.get(f"{base}/.cache/tracker/okapi-vm.db", {}).get("result") == "removed",
                "INV-308: once the bind mount is gone, the same target is cleaned: its exclusion is what kept it")
    bench.check(common.present(bench, guarded) == list(guarded.paths),
                "INV-306: what the user excludes through a link into /var/crash is kept by root")


def run(bench: Bench) -> None:
    documents = common.Files(bench.accounts.admin.name, ["~/Documents/okapi-vm-1.txt", "~/Documents/okapi-vm-2.txt"],
                             SIZE)
    bench.allow_elevation_without_password()
    common.write_settings(bench, {"exclusions": [f"{home(bench)}/okapi-vm-mirror", f"{home(bench)}/okapi-vm-crash-link"],
                                  "deletionLog": True})
    by_the_cli(bench, documents)
    by_the_interface(bench, documents)
