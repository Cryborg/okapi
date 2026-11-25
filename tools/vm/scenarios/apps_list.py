"""Applications: the list of what is installed, its filters, search, sorts, details and states.

Debs with a launcher (one installed by the user, one as a dependency, one
the system requires), a local snap and a component snap, the image's system
flatpak and a flatpak of the account. Each shows as dpkg, snapd and flatpak
say; what cannot be uninstalled says why; a dpkg that writes, a snapd that
does not answer, flatpak installations unreadable show their banner; the
header tells when the list was read (APP-001 to 006, APP-018).
"""

from __future__ import annotations

import re
import shlex

from okapivm import apps, common
from okapivm.bench import Bench

TAGS = ["apps"]

FLATPAK_APP = "io.mpv.Mpv"
FLATPAK_NAME = "Lecteur multimédia mpv"
USER_FLATPAK = "org.okapivm.Mine"
USER_INSTALLATION = "~/.local/share/flatpak"
SYSTEM_INSTALLATION = "/var/lib/flatpak"
DPKG_PENDING = "/var/lib/dpkg/updates/0001"
SNAPD_SOCKET = "/run/snapd.socket"
FAKE_SNAPD_UNIT = "okapi-vm-snapd"
# Answers every request with a server error, as a snapd that fails.
FAKE_SNAPD = (
    "import os, socket\n"
    f"path = {SNAPD_SOCKET!r}\n"
    "server = socket.socket(socket.AF_UNIX)\n"
    "server.bind(path)\n"
    "os.chmod(path, 0o666)\n"
    "server.listen()\n"
    "while True:\n"
    "    client, _ = server.accept()\n"
    "    client.recv(65536)\n"
    "    client.sendall(b'HTTP/1.1 500 Internal Server Error\\r\\nContent-Length: 0\\r\\n\\r\\n')\n"
    "    client.close()\n"
)
MINUTES = 10
SIZE = re.compile(r"^([\d ,]+) (o|ko|Mo|Go)$")
UNITS = {"o": 1, "ko": 1e3, "Mo": 1e6, "Go": 1e9}

MINE = common.Package("okapi-vm-mine", fields=(("Depends", "okapi-vm-library"),))
LIBRARY = common.Package("okapi-vm-library")
REQUIRED = common.Package("okapi-vm-required", fields=(("Priority", "required"),))
LATER = common.Package("okapi-vm-later")
SNAP = common.Snap("okapi-vm-tool")
COMPONENT = common.Snap("okapi-vm-component", application=False)


def plant_user_flatpak(bench: Bench) -> None:
    """A flatpak deployed in the account's own installation, as flatpak lays it out."""
    branch = f"{USER_INSTALLATION}/app/{USER_FLATPAK}/x86_64/stable"
    deployment = f"{branch}/okapi-vm"
    launcher = common.launcher(USER_FLATPAK, USER_FLATPAK).replace(f"Name={USER_FLATPAK}", "Name=Okapi Vm Mine")
    bench.user(" && ".join([
        f"mkdir -p -- {common.shell_path(f'{deployment}/export/share/applications')} "
        f"{common.shell_path(f'{USER_INSTALLATION}/repo')}",
        f"printf %s {shlex.quote(f'[Application]{chr(10)}name={USER_FLATPAK}{chr(10)}')} > "
        f"{common.shell_path(f'{deployment}/metadata')}",
        f"printf %s {shlex.quote(launcher)} > "
        f"{common.shell_path(f'{deployment}/export/share/applications/{USER_FLATPAK}.desktop')}",
        f"ln -s okapi-vm {common.shell_path(f'{branch}/active')}",
    ]))


def size(text: str) -> float:
    match = SIZE.match(text)
    if not match:
        raise AssertionError(f"not a size: {text!r}")
    return float(match.group(1).replace(" ", "").replace(",", ".")) * UNITS[match.group(2)]


def listed(bench: Bench) -> None:
    for name, source in ((MINE.name, apps.UBUNTU), (SNAP.name, apps.SNAP), (FLATPAK_NAME, apps.FLATPAK),
                         ("Okapi Vm Mine", apps.FLATPAK)):
        apps.show_only(bench, name.split()[0] if source == apps.FLATPAK else name)
        bench.check(any(re.match(apps.row_pattern(name, source), row) for row in apps.rows(bench)),
                    f"APP-001: {name} is listed from {source}")
    apps.show_only(bench, COMPONENT.name)
    bench.check(not apps.rows(bench), f"APP-001: {COMPONENT.name}, a snap without application, is not a row")
    components = len([line for line in bench.root("snap list").splitlines()[1:] if line.strip()]) - 1
    summary = next(text for text in apps.labels(bench) if text.startswith("paquets : "))
    bench.check(f"composants snap : {components} (" in summary,
                f"APP-001: « {apps.SYSTEM} » counts the {components} snaps without application ({summary!r})")
    bench.ui.click(apps.WHAT_IS_IT, role=apps.BUTTON)
    bench.check(any(text.startswith("Ce qui fait tourner le système et les applis sans apparaître dans le menu")
                    for text in apps.labels(bench)), "APP-001: « C'est quoi ? » tells what they are")
    bench.ui.click(apps.WHAT_IS_IT, role=apps.BUTTON)


def filtered_and_searched(bench: Bench) -> None:
    apps.search(bench, "")
    for source in (apps.UBUNTU, apps.SNAP, apps.FLATPAK):
        apps.choose_source(bench, source)
        shown = apps.rows(bench)
        bench.check(bool(shown) and all(row.split(", ")[1] == source for row in shown),
                    f"APP-002: « {source} » shows its rows alone ({len(shown)})")
    apps.choose_source(bench, apps.ALL)
    apps.search(bench, FLATPAK_APP)
    bench.check([row.split(", ")[0] for row in apps.rows(bench)] == [FLATPAK_NAME],
                "APP-002: the search finds by the package's name too")
    apps.search(bench, "zzz")
    bench.ui.wait("Aucune appli ne correspond à « zzz »")
    bench.ui.click(apps.CLEAR_SEARCH, role=apps.BUTTON)
    bench.check(bool(apps.rows(bench)) and bench.ui.node(role="entry").get("text") == "",
                "APP-002: « Effacer la recherche » empties the field, the rows come back")


def sorted_rows(bench: Bench) -> None:
    apps.choose_source(bench, apps.UBUNTU)
    bench.check(bool(bench.ui.find(f"Place prise{apps.SORTED_DOWN}", role=apps.BUTTON)),
                "APP-003: sorted by « Place prise » at first, the largest first")
    bench.ui.click("Nom", role=apps.BUTTON)
    names = [row.split(", ")[0] for row in apps.rows(bench)]
    bench.check(bool(bench.ui.find(f"Nom{apps.SORTED_UP}", role=apps.BUTTON))
                and names == sorted(names, key=str.casefold), f"APP-003: by « Nom », in order ({names})")
    bench.ui.click("Appli", role=apps.BUTTON)
    sizes = [size(row.split(", ")[2]) for row in apps.rows(bench)]
    bench.check(bool(bench.ui.find(f"Appli{apps.SORTED_DOWN}", role=apps.BUTTON))
                and sizes == sorted(sizes, reverse=True), "APP-003: by « Appli », the largest first")
    bench.ui.click(f"Appli{apps.SORTED_DOWN}", role=apps.BUTTON)
    bench.check(bool(bench.ui.find(f"Appli{apps.SORTED_UP}", role=apps.BUTTON)), "APP-003: a second click reverses")
    apps.choose_source(bench, apps.SNAP)
    bench.ui.click("Données", role=apps.BUTTON)
    bench.check(bool(bench.ui.find(f"Données{apps.SORTED_DOWN}", role=apps.BUTTON)), "APP-003: by « Données »")
    bench.ui.click("Place prise", role=apps.BUTTON)
    apps.choose_source(bench, apps.ALL)


def details(bench: Bench) -> None:
    auto = bench.root("apt-mark showauto").split()
    for package, origin in ((MINE, "Installée par vous"), (LIBRARY, "Installée avec une autre appli"),
                            (REQUIRED, "Installée avec le système")):
        apps.open_detail(bench, package.name)
        texts = apps.labels(bench)
        bench.check(f"Ubuntu, version {package.version}" in texts and origin in texts and "Paquet" in texts,
                    f"APP-004: {package.name}: « {origin} » (apt-mark showauto: {package.name in auto})")
        bench.ui.click(apps.CLOSE_DETAIL, role=apps.BUTTON)
        bench.ui.wait(apps.CLOSE_DETAIL, role=apps.BUTTON, gone=True)
    bench.check(LIBRARY.name in auto and MINE.name not in auto, "APP-004: apt says who installed what")
    apps.open_detail(bench, SNAP.name)
    bench.check(any(text.startswith("Données : ") for text in apps.labels(bench)), "APP-004: a snap tells its data")
    bench.ui.click(apps.CLOSE_DETAIL, role=apps.BUTTON)


def refused_in_advance(bench: Bench) -> None:
    apps.open_detail(bench, REQUIRED.name)
    bench.check(f"Le bureau a besoin de {REQUIRED.name} pour fonctionner. Okapi ne propose donc pas de la "
                f"désinstaller." in apps.labels(bench) and not bench.ui.find(apps.UNINSTALL, role=apps.BUTTON),
                f"APP-006: {REQUIRED.name}, Priority: required, is not offered to uninstall")
    bench.ui.click(apps.CLOSE_DETAIL, role=apps.BUTTON)
    apps.open_detail(bench, "Okapi Vm Mine", query="Okapi")
    bench.check("Installée pour votre compte seulement : Okapi ne sait pas la désinstaller." in apps.labels(bench)
                and not bench.ui.find(apps.UNINSTALL, role=apps.BUTTON),
                "APP-006: the account's own flatpak is not offered to uninstall")
    bench.ui.click(apps.CLOSE_DETAIL, role=apps.BUTTON)
    apps.search(bench, "")


def read_when(bench: Bench) -> None:
    first = apps.header(bench)
    bench.check(first.group(2) == "à l'instant", f"APP-018: « {first.group(0)} »")
    bench.advance_clock(MINUTES * 60)
    later = bench.ui.wait(pattern=rf"^Applis : \d+, lues il y a {MINUTES} min$", timeout=90)[0]["name"]
    bench.check(later.startswith(f"Applis : {first.group(1)}, "), f"APP-018: {MINUTES} minutes later: « {later} »")
    common.build_deb(bench, LATER)
    common.install_deb(bench, LATER)
    apps.refresh(bench)
    again = apps.header(bench)
    bench.check(again.group(2) == "à l'instant" and int(again.group(1)) == int(first.group(1)) + 1,
                f"APP-018: « Actualiser » reads again, {LATER.name} counted ({again.group(0)})")


def banner(bench: Bench, text: str) -> bool:
    """Read again, whether `text` shows."""
    apps.refresh(bench)
    return bool(bench.ui.wait(text, timeout=60))


def source_traced(bench: Bench, message: str) -> bool:
    return bool(apps.traced(bench, message))


def states(bench: Bench) -> None:
    bench.root(f"touch {shlex.quote(DPKG_PENDING)}")
    shown = banner(bench, "Les paquets Ubuntu ne sont pas lisibles pour l'instant : une installation est en cours.")
    bench.check(shown and source_traced(bench, "dpkg is writing: packages read later"),
                f"APP-005: dpkg midway through a write ({DPKG_PENDING}): the debs are read later")
    bench.root(f"rm -- {shlex.quote(DPKG_PENDING)}")

    bench.root(" && ".join([
        "systemctl stop snapd.socket snapd.service",
        f"rm -f -- {SNAPD_SOCKET}",
        shlex.join(["systemd-run", "--quiet", f"--unit={FAKE_SNAPD_UNIT}", "python3", "-I", "-c", FAKE_SNAPD]),
        f"for attempt in $(seq 50); do [ -S {SNAPD_SOCKET} ] && exit 0; sleep 0.1; done; exit 1",
    ]))
    shown = banner(bench, "Les snaps ne sont pas lisibles : le service snapd ne répond pas.")
    bench.check(shown and source_traced(bench, "snapd's list not read"),
                "APP-005: snapd answers an error: the snaps are said unreadable")
    bench.root(f"systemctl stop {FAKE_SNAPD_UNIT} && rm -f -- {SNAPD_SOCKET} && systemctl start snapd.socket")

    bench.user(f"chmod 000 {common.shell_path(f'{USER_INSTALLATION}/app')}")
    shown = banner(bench, "Une partie des flatpaks n'a pas pu être lue : certains peuvent manquer. Le détail est "
                          "dans le journal.")
    bench.check(shown and source_traced(bench, "flatpak installation unread"),
                f"APP-005: {USER_INSTALLATION}/app unreadable: the flatpaks are said partly read")
    bench.root(f"chmod 000 {SYSTEM_INSTALLATION}/app")
    shown = banner(bench, "Les flatpaks n'ont pas pu être lus. Le détail est dans le journal.")
    bench.check(shown and source_traced(bench, "no flatpak installation read"),
                "APP-005: no flatpak installation readable: the flatpaks are said unread")
    bench.root(f"chmod 755 {SYSTEM_INSTALLATION}/app")
    bench.user(f"chmod 755 {common.shell_path(f'{USER_INSTALLATION}/app')}")


def run(bench: Bench) -> None:
    for package in (MINE, LIBRARY, REQUIRED):
        common.build_deb(bench, package)
    common.install_deb(bench, MINE, LIBRARY, REQUIRED)
    bench.root(shlex.join(["apt-mark", "auto", LIBRARY.name]))
    for snap in (SNAP, COMPONENT):
        common.build_snap(bench, snap)
        common.install_snap(bench, snap)
    plant_user_flatpak(bench)

    bench.launch_app()
    apps.open_page(bench)
    bench.screenshot("liste")
    listed(bench)
    filtered_and_searched(bench)
    sorted_rows(bench)
    details(bench)
    refused_in_advance(bench)
    read_when(bench)
    states(bench)
