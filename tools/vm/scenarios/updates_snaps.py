"""Updates: the snaps of the store to refresh, one, all, and one whose app is open.

Needs the machine's network: two snaps of the store installed from another
channel, then switched to stable, wait for a refresh. One whose app is open:
snapd refuses and the screen says so; one refreshed by its own button; then
both, switched again, by « Tout mettre à jour » (MAJ-011).
"""

from __future__ import annotations

import json
import shlex

from okapivm import common
from okapivm.bench import Bench
from scenarios import _updates as updates

TAGS = ["updates", "snap", "network"]

STORE = "https://api.snapcraft.io"
SNAPD_SOCKET = "/run/snapd.socket"
GREETER = "hello"
GREETER_CHANNEL = "beta"
TOOLS = "test-snapd-tools"
TOOLS_CHANNEL = "edge"
BLOCKING_APP = f"{TOOLS}.block"
BLOCKING_UNIT = "okapi-vm-block"
ALL = "Mettre à jour toutes les applis"
APPS_OPEN = "Une appli à mettre à jour est ouverte. Fermez-la, puis réessayez."
STORE_TIMEOUT_SECONDS = 300


def snap(bench: Bench, name: str) -> dict:
    answer = bench.root(f"curl --silent --unix-socket {SNAPD_SOCKET} http://localhost/v2/snaps/{shlex.quote(name)}")
    return json.loads(answer)["result"]


def revision(bench: Bench, name: str) -> str:
    return str(snap(bench, name)["revision"])


def refreshable(bench: Bench) -> list[str]:
    answer = bench.root(f"curl --silent --unix-socket {SNAPD_SOCKET} 'http://localhost/v2/find?select=refresh'")
    return sorted(found["name"] for found in json.loads(answer).get("result", []) or [])


def run(bench: Bench) -> None:
    online = bench.execute(common.ROOT, f"curl --silent --head --max-time 20 {STORE}").code == 0
    bench.check(online, f"MAJ-011 needs the store: the machine reaches {STORE}")
    for name, channel in ((GREETER, GREETER_CHANNEL), (TOOLS, TOOLS_CHANNEL)):
        bench.root(shlex.join(["snap", "install", name, f"--{channel}"]))
        bench.root(shlex.join(["snap", "switch", "--stable", name]))
    waiting = refreshable(bench)
    bench.check(waiting == sorted([GREETER, TOOLS]), f"snapd offers both snaps a refresh ({waiting})")
    updates.prepare(bench)
    bench.launch_app()
    common.open_screen(bench, updates.PAGE)
    updates.search_now(bench, pattern="echerché à l'instant$")

    titles = {name: snap(bench, name)["title"] or name for name in (GREETER, TOOLS)}
    listed = [node["name"] for node in bench.ui.find(pattern=r", version .*", role="list-item")]
    bench.screenshot("applis")
    bench.check(all(any(line.startswith(f"{title}, ") for line in listed) for title in titles.values()),
                f"MAJ-011: the page lists both snaps to refresh ({listed})")
    app_open(bench, titles[TOOLS])
    one(bench, titles[GREETER])
    every(bench)


def app_open(bench: Bench, title: str) -> None:
    """MAJ-011, an app of the snap open."""
    before = revision(bench, TOOLS)
    bench.user(shlex.join(["systemd-run", "--user", "--quiet", f"--unit={BLOCKING_UNIT}", f"/snap/bin/{BLOCKING_APP}"]))
    bench.log(f"{BLOCKING_APP} runs")
    mark = len(common.journal(bench))
    bench.ui.click(f"Mettre à jour {title}", role="button")
    said = bench.ui.wait(APPS_OPEN, timeout=STORE_TIMEOUT_SECONDS)
    bench.screenshot("appli-ouverte")
    end = updates.wait_event(bench, mark, "snaps refresh ended", STORE_TIMEOUT_SECONDS)
    bench.check(bool(said) and revision(bench, TOOLS) == before,
                f"MAJ-011: snapd refuses a snap whose app is open, the screen says so ({end})")
    bench.user(shlex.join(["systemctl", "--user", "stop", BLOCKING_UNIT]))
    bench.ui.click(updates.CLOSE, role="button")
    bench.ui.wait(updates.SEARCH_NOW, role="button")


def one(bench: Bench, title: str) -> None:
    """MAJ-011, « Mettre à jour » of one snap."""
    before = revision(bench, GREETER)
    bench.ui.click(f"Mettre à jour {title}", role="button")
    said = bench.ui.wait("1 mise à jour installée", timeout=STORE_TIMEOUT_SECONDS)
    bench.screenshot("une-appli")
    after = snap(bench, GREETER)
    receipt = common.last_receipt(bench)
    bench.check(bool(said) and str(after["revision"]) != before and after["tracking-channel"].endswith("stable"),
                f"MAJ-011: {GREETER} refreshed to stable, revision {before} → {after['revision']}")
    bench.check(receipt.get("kind") == "update" and receipt.get("source") == "snap" and receipt.get("done") is True,
                f"MAJ-011: the refresh has its history line ({receipt})")
    bench.ui.click(updates.CLOSE, role="button")
    bench.ui.wait(updates.SEARCH_NOW, role="button")


def every(bench: Bench) -> None:
    """MAJ-011, « Tout mettre à jour »."""
    bench.root(shlex.join(["snap", "switch", f"--{GREETER_CHANNEL}", GREETER]))
    before = {name: revision(bench, name) for name in (GREETER, TOOLS)}
    updates.search_now(bench, pattern="echerché à l'instant$")
    bench.ui.click(ALL, role="button")
    said = bench.ui.wait("2 mises à jour installées", timeout=STORE_TIMEOUT_SECONDS)
    bench.screenshot("toutes-les-applis")
    after = {name: revision(bench, name) for name in before}
    left = refreshable(bench)
    bench.check(bool(said) and all(after[name] != before[name] for name in before) and not left,
                f"MAJ-011: « {ALL} » refreshes both snaps ({before} → {after}, still offered {left})")
