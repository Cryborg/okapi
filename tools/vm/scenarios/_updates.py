"""What the updates scenarios share: the page, its search, the packages a
local APT suite offers, and what the journal says of the updates read."""

from __future__ import annotations

import shlex
import time

from okapivm import common, config
from okapivm.bench import Bench

PAGE = "Mises à jour"
HOME_PAGE = "Accueil"
SEARCH_NOW = "Rechercher maintenant"
SYSTEM_BUTTON = "^Mettre à jour Système"
CLOSE = "Fermer"
RETRY = "Réessayer"
SYSTEM_READ = "system updates read"
SEARCHED = "updates searched"
INSTALL_ENDED = "installation ended: the list read again"
SECURITY_SUITE = f"{common.APT_SUITE}-security"
SEARCH_TIMEOUT_SECONDS = 120
INSTALL_TIMEOUT_SECONDS = 180
# Okapi reads the list again on opening its page past this age (READ_AGAIN_AFTER).
READ_AGAIN_SECONDS = 6 * 60
POLL_SECONDS = 0.5


def prepare(bench: Bench, *installed: common.Package) -> None:
    """Ubuntu's repositories out of the way, `installed` built and installed,
    and the updates asked of PackageKit and snapd granted without a password."""
    common.set_ubuntu_sources_aside(bench)
    bench.root("apt-get update")
    for package in installed:
        common.build_deb(bench, package)
    if installed:
        common.install_deb(bench, *installed)
    bench.set_elevation(common.Elevation.WITHOUT_PASSWORD, config.UPDATE_ACTIONS)


def offer(bench: Bench, *packages: common.Package, suite: str = common.APT_SUITE, signed: bool = True) -> None:
    for package in packages:
        common.build_deb(bench, package)
    common.publish_apt_repository(bench, *packages, suite=suite, signed=signed)


def events(bench: Bench, mark: int, message: str) -> list[dict]:
    return [event["fields"] for event in common.journal(bench)[mark:] if event["fields"].get("message") == message]


def search_now(bench: Bench, expected: str | None = None, *, pattern: str | None = None) -> dict:
    """« Rechercher maintenant », then the page's summary (`expected` or
    `pattern`) and the trace of the system's updates read with it."""
    mark = len(common.journal(bench))
    bench.ui.click(SEARCH_NOW, role="button")
    bench.ui.wait(expected, pattern=pattern, timeout=SEARCH_TIMEOUT_SECONDS)
    return wait_event(bench, mark, SYSTEM_READ)


def wait_event(bench: Bench, mark: int, message: str, timeout: float = SEARCH_TIMEOUT_SECONDS) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found = events(bench, mark, message)
        if found:
            return found[-1]
        time.sleep(POLL_SECONDS)
    bench.check(False, f"the journal says « {message} » within {timeout} s")
    return {}


def version(bench: Bench, name: str) -> str:
    return bench.root(f"dpkg-query --show --showformat='${{Version}}' {shlex.quote(name)} 2>/dev/null", check=False)


def versions(bench: Bench, *names: str) -> dict[str, str]:
    return {name: version(bench, name) for name in names}


def open_again(bench: Bench) -> None:
    """The page left for the home page, then opened again."""
    common.open_screen(bench, HOME_PAGE)
    common.open_screen(bench, PAGE)


def wait_description(bench: Bench, name: str, role: str, expected: str,
                     timeout: float = SEARCH_TIMEOUT_SECONDS) -> str:
    """Waits for the node's description to be `expected`, and gives the last one read."""
    deadline = time.monotonic() + timeout
    said = ""
    while time.monotonic() < deadline:
        said = bench.ui.node(name, role=role).get("description", "")
        if said == expected:
            break
        time.sleep(POLL_SECONDS)
    return said
