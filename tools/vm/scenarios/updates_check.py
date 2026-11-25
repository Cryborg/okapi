"""Updates: the search, its list and its moments, a restart awaited, snapd's schedule, the home page's widget.

Ubuntu's repositories set aside: only the bench's suites offer updates. The
widget alone on the home page says « Recherche… » while PackageKit is held,
then « Tout est à jour », and opens the page (ACC-018). Then a signed suite and
its `-security` sibling: « Rechercher maintenant » downloads the lists, and
opening the page searches again only past five minutes (MAJ-001); the list puts
the security update first, says the update held back by a missing dependency,
and hides the lines past the first ones (MAJ-002). The lists aged eight days
(MAJ-003), a restart awaited (MAJ-010), snapd's own refreshes (MAJ-012), and
last no PackageKit nor snapd at all (MAJ-004).

MAJ-012 is partial: snapd in the machine never says its next refresh
(`/v2/system-info` has no `refresh.next`), so « Prochain passage » is left to
the tests of `okapi-tools`.
"""

from __future__ import annotations

import json
import shlex

from okapivm import common
from okapivm.bench import Bench
from scenarios import _updates as updates

TAGS = ["updates"]

WIDGET = "Mises à jour"
WIDGET_ROLE = "panel"
UP_TO_DATE = "Tout est à jour"
SEARCHING = "Recherche…"
SEE_UPDATES = "Voir les mises à jour"
PACKAGEKIT = "packagekit.service"
PACKAGEKIT_NAME = "org.freedesktop.PackageKit"
PACKAGEKIT_ACTIVATION = "/usr/share/dbus-1/system-services/org.freedesktop.PackageKit.service"
PACKAGEKIT_ACTIVATION_ASIDE = "/var/tmp/okapi-vm-packagekit.service"
SNAPD_UNITS = ("snapd.socket", "snapd.service")
SNAPD_SOCKET = "/run/snapd.socket"
APT_STAMP = "/var/lib/apt/periodic/update-success-stamp"
REBOOT_REQUIRED = "/run/reboot-required"
REBOOT_PACKAGES = ("okapi-u1", "okapi-u2")

SECURITY = "okapi-sec"
HELD = "okapi-held"
PLAIN = tuple(f"okapi-u{index}" for index in range(1, 7))
NEW_VERSION = "2.0"
# The list shows this many lines first (SYSTEM_LINES_FIRST).
LINES_FIRST = 6
LISTED = 1 + len(PLAIN) + 1
DAY_SECONDS = 24 * 3600
OLD_LISTS_SECONDS = 8 * DAY_SECONDS
OLD_LISTS_AFTER_SECONDS = 7 * DAY_SECONDS


def item(name: str, kind: str) -> str:
    return f"{name}, {kind}, version {NEW_VERSION}"


def run(bench: Bench) -> None:
    installed = [common.Package(name) for name in (SECURITY, HELD, *PLAIN)]
    common.write_store(bench, "dashboard.json", {"version": 1, "widgets": [{"kind": "updates", "size": "small"}]})
    updates.prepare(bench, *installed)

    widget(bench)
    updates.offer(bench, common.Package(SECURITY, NEW_VERSION), suite=updates.SECURITY_SUITE)
    updates.offer(bench, *(common.Package(name, NEW_VERSION) for name in PLAIN),
                  common.Package(HELD, NEW_VERSION, fields=(("Depends", "okapi-missing"),)))
    search(bench)
    listed(bench)
    old_lists(bench)
    restart(bench)
    snaps(bench)
    no_source(bench)


def widget(bench: Bench) -> None:
    """ACC-018."""
    bench.root(f"busctl call {PACKAGEKIT_NAME} /org/freedesktop/PackageKit org.freedesktop.DBus.Peer Ping")
    bench.root(shlex.join(["systemctl", "kill", "--signal=SIGSTOP", PACKAGEKIT]))
    bench.log("PackageKit stopped where it stands: the search waits on it")
    mark = len(common.journal(bench))
    bench.launch_app()
    searching = bench.ui.wait(SEARCHING, role="label")
    bench.screenshot("widget-recherche")
    bench.root(shlex.join(["systemctl", "kill", "--signal=SIGCONT", PACKAGEKIT]))
    said = updates.wait_description(bench, WIDGET, WIDGET_ROLE, UP_TO_DATE)
    read = updates.wait_event(bench, mark, updates.SYSTEM_READ)
    bench.screenshot("widget-a-jour")
    bench.check(bool(searching), "ACC-018: the widget says « Recherche… » while the search runs")
    bench.check(said == UP_TO_DATE and read.get("updates") == 0,
                f"ACC-018: the widget says « {UP_TO_DATE} », and PackageKit offered nothing ({said!r}, {read})")
    bench.ui.click(SEE_UPDATES, role="button")
    shown = bench.ui.wait(updates.PAGE, role="button", state=common.SELECTED)
    bench.check(bool(shown) and bool(bench.ui.find(updates.SEARCH_NOW, role="button")),
                "ACC-018: « Voir les mises à jour » opens the updates page")


def search(bench: Bench) -> None:
    """MAJ-001."""
    stamp = int(bench.root(f"stat --format=%Y {APT_STAMP}"))
    bench.advance_clock(60)
    read = updates.search_now(bench, pattern=r"^\d+ mises à jour, dont 1 de sécurité, recherché à l'instant$")
    bench.screenshot("recherche")
    refreshed = int(bench.root(f"stat --format=%Y {APT_STAMP}"))
    bench.check(read.get("refresh") == "Done" and refreshed > stamp,
                f"MAJ-001: « Rechercher maintenant » downloads the lists again ({read}, stamp {stamp} → {refreshed})")
    bench.check(read.get("updates") == LISTED and read.get("security") == 1 and read.get("blocked") == 1,
                f"MAJ-001: the search finds the {LISTED} updates of the suites ({read})")

    mark = len(common.journal(bench))
    updates.open_again(bench)
    bench.ui.wait(updates.SEARCH_NOW, role="button")
    bench.check(not updates.events(bench, mark, updates.SEARCHED),
                "MAJ-001: the page opened again within five minutes reads no list")
    bench.advance_clock(updates.READ_AGAIN_SECONDS)
    mark = len(common.journal(bench))
    updates.open_again(bench)
    read = updates.wait_event(bench, mark, updates.SYSTEM_READ)
    bench.check(read.get("refresh") == "NotAsked" and read.get("updates") == LISTED,
                f"MAJ-001: past five minutes, opening the page searches again, lists not downloaded ({read})")


def listed(bench: Bench) -> None:
    """MAJ-002."""
    fold = bench.ui.node(pattern="^Système .*, (replié|déplié)$", role="button")
    if fold["name"].endswith("replié"):
        bench.ui.click(fold["name"], role="button")
    more = f"Afficher les {LISTED - LINES_FIRST} autres"
    bench.ui.wait(more, role="button")
    first = [node["name"] for node in bench.ui.find(pattern=f", .*, version {NEW_VERSION}$", role="list-item")]
    bench.screenshot("liste")
    bench.check(len(first) == LINES_FIRST and first[0] == item(SECURITY, "mise à jour de sécurité"),
                f"MAJ-002: {LINES_FIRST} lines first, the security update on top ({first})")
    bench.ui.click(more, role="button")
    bench.ui.wait(item(HELD, "retenue par le système"), role="list-item")
    every = [node["name"] for node in bench.ui.find(pattern=f", .*, version {NEW_VERSION}$", role="list-item")]
    expected = {item(SECURITY, "mise à jour de sécurité"), item(HELD, "retenue par le système"),
                *(item(name, "mise à jour") for name in PLAIN)}
    bench.screenshot("liste-entiere")
    bench.check(set(every) == expected and len(every) == LISTED,
                f"MAJ-002: « {more} » shows every line, the held one said held ({every})")
    simulated = bench.root("apt-get --simulate dist-upgrade")
    bench.check(f"Inst {HELD} " not in simulated and f"Inst {SECURITY} " in simulated,
                f"MAJ-002: APT holds {HELD} back and installs {SECURITY}")


def old_lists(bench: Bench) -> None:
    """MAJ-003."""
    bench.advance_clock(OLD_LISTS_SECONDS)
    mark = len(common.journal(bench))
    updates.open_again(bench)
    read = updates.wait_event(bench, mark, updates.SYSTEM_READ)
    badge = bench.ui.wait(pattern="^Liste du ", role="label")
    bench.screenshot("listes-anciennes")
    age = int(bench.root("date +%s")) - int(bench.root(f"stat --format=%Y {APT_STAMP}"))
    bench.check(bool(badge) and read.get("refresh") == "NotAsked" and age > OLD_LISTS_AFTER_SECONDS,
                f"MAJ-003: lists {age // DAY_SECONDS} days old are said of their date "
                f"({[node['name'] for node in badge]}), not downloaded at opening")


def restart(bench: Bench) -> None:
    """MAJ-010."""
    bench.root("\n".join([f"touch {REBOOT_REQUIRED}",
                          f"printf '%s\\n' {shlex.join(REBOOT_PACKAGES)} > {REBOOT_REQUIRED}.pkgs"]))
    bench.advance_clock(updates.READ_AGAIN_SECONDS)
    updates.open_again(bench)
    said = [bench.ui.wait("Redémarrage attendu", timeout=updates.SEARCH_TIMEOUT_SECONDS),
            bench.ui.wait("Un redémarrage termine des mises à jour déjà installées.", role="label"),
            bench.ui.wait(f"{len(REBOOT_PACKAGES)} paquets", role="button")]
    bench.screenshot("redemarrage")
    bench.check(all(said), "MAJ-010: /run/reboot-required gives « Redémarrage attendu » and the packages it names")
    bench.root(f"rm -f -- {REBOOT_REQUIRED} {REBOOT_REQUIRED}.pkgs")


def snaps(bench: Bench) -> None:
    """MAJ-012, partial."""
    said = bench.ui.wait("Les applis snap se mettent à jour seules.", role="label")
    information = json.loads(bench.root(f"curl --silent --unix-socket {SNAPD_SOCKET} http://localhost/v2/system-info"))
    schedule = information["result"].get("refresh", {})
    bench.check(bool(said) and "next" not in schedule,
                f"MAJ-012 (partial): snapd says no next refresh, the page says snaps refresh alone ({schedule})")


def no_source(bench: Bench) -> None:
    """MAJ-004."""
    bench.root("\n".join([
        shlex.join(["mv", PACKAGEKIT_ACTIVATION, PACKAGEKIT_ACTIVATION_ASIDE]),
        shlex.join(["systemctl", "stop", PACKAGEKIT, *SNAPD_UNITS]),
        "busctl call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig",
    ]))
    bench.log("no PackageKit to activate, snapd stopped")
    bench.advance_clock(updates.READ_AGAIN_SECONDS)
    mark = len(common.journal(bench))
    updates.open_again(bench)
    said = [bench.ui.wait("Okapi ne trouve pas de gestionnaire de mises à jour sur ce système.",
                          timeout=updates.SEARCH_TIMEOUT_SECONDS),
            bench.ui.wait("Okapi lit les mises à jour d'apt (PackageKit) et de snap. Aucun des deux n'a été trouvé ici.")]
    bench.screenshot("aucune-source")
    absent = [event["message"] for event in (updates.events(bench, mark, "no PackageKit on this system")
                                             + updates.events(bench, mark, "no snapd on this system"))]
    activatable = bench.root("busctl list --activatable --no-legend")
    bench.check(all(said) and len(absent) == 2 and PACKAGEKIT_NAME not in activatable,
                f"MAJ-004: neither PackageKit nor snapd found, and the page says so ({absent})")
