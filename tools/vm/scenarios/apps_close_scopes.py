"""Applications: an open application closed by its scopes before it goes, and only those.

Fake applications run in scopes named as GNOME, flatpak or Chromium name
theirs. Okapi sends SIGTERM to each of the application's own, SIGKILL only
when asked to force, never to another application's, and never to the scope
it runs in itself; the session bus shows each call it makes to systemd
(APP-015 to 017, INV-405, 406).
"""

from __future__ import annotations

import shlex

from okapivm import apps, common, config
from okapivm.bench import Bench

TAGS = ["apps", "invariants", "scopes"]

SYSTEMD = "org.freedesktop.systemd1"
MANAGER = f"{SYSTEMD}.Manager"
LIST_UNITS = "ListUnitsByPatterns"
KILL_UNIT = "KillUnit"
SIGTERM = "15"
SIGKILL = "9"
# What crates/tools/src/packages/systemd_session.rs asks systemd for.
LIST_ARGUMENTS = ("active", "activating", "deactivating", "*.scope")
FLATPAK_APP = "io.mpv.Mpv"
FLATPAK_NAME = "Lecteur multimédia mpv"
# Ignores SIGTERM, as an application that does not close; its `sleep` is
# signalled too, and started again.
STUBBORN_PROGRAM = "#!/bin/sh\ntrap '' TERM\nwhile :; do sleep 1; done\n"
# Okapi's wait for an application to close after SIGTERM (crates/app/src/closing.rs).
CLOSE_WAIT_SECONDS = 15

SIMPLE = common.Package("okapi-vm-simple")
STUBBORN = common.Package("okapi-vm-stubborn", program=STUBBORN_PROGRAM)
BROWSER = common.Package("okapi-vm-browser", hidden_launchers=("okapi-vm-browser-second",))
BYSTANDER = common.Package("okapi-vm-bystander")
TERMINAL = common.Package("okapi-vm-terminal")


def kills(bench: Bench) -> list[common.BusCall]:
    return [call for call in common.bus_calls(bench) if call.member == KILL_UNIT]


def check_bus(bench: Bench, okapi: set[str], wanted: dict[str, set[str]]) -> None:
    """INV-405: Okapi asks systemd only its scopes' list and `KillUnit` of the
    scopes in `wanted` (unit → signals), each with the very signals asked."""
    calls = [call for call in common.bus_calls(bench) if call.sender in okapi]
    members = {call.member for call in calls}
    bench.check(members <= {LIST_UNITS, KILL_UNIT} and all(call.interface == MANAGER for call in calls),
                f"INV-405: Okapi calls only {LIST_UNITS} and {KILL_UNIT} of systemd's manager ({sorted(members)})")
    lists = {call.arguments for call in calls if call.member == LIST_UNITS}
    bench.check(lists == {LIST_ARGUMENTS}, f"INV-405: the scopes are listed by fixed patterns ({lists})")
    sent: dict[str, set[str]] = {}
    whom = set()
    for call in calls:
        if call.member == KILL_UNIT:
            unit, who, signal = call.arguments
            whom.add(who)
            sent.setdefault(unit, set()).add(signal)
    bench.check(sent == wanted and whom == {"all"},
                f"INV-405: KillUnit(scope, \"all\", signal) reached exactly {wanted} ({sent}, {whom})")


def closes_on_sigterm(bench: Bench) -> str:
    scope = common.scope_unit(SIMPLE.name)
    common.open_in_scope(bench, scope, [SIMPLE.executable])
    apps.ask_uninstall(bench, SIMPLE.name)
    bench.check(f"{SIMPLE.name} est ouverte : Okapi va la fermer. Enregistrez d'abord votre travail." in apps.labels(bench),
                "APP-015: the dialog says the application is open and will be closed")
    bench.screenshot("ouverte")
    apps.confirm(bench, apps.CLOSE_AND_UNINSTALL)
    apps.wait_done(bench, SIMPLE.name)
    bench.check(not common.scope_running(bench, scope) and not common.deb_installed(bench, SIMPLE.name),
                f"APP-015: {scope} closed by SIGTERM, then {SIMPLE.name} uninstalled")
    return scope


def forced(bench: Bench) -> str:
    scope = common.scope_unit(STUBBORN.name)
    common.open_in_scope(bench, scope, [STUBBORN.executable])
    apps.ask_uninstall(bench, STUBBORN.name)
    apps.confirm(bench, apps.CLOSE_AND_UNINSTALL)
    apps.wait_step(bench, STUBBORN.name, apps.CLOSING)
    apps.wait_step(bench, STUBBORN.name, apps.NOT_CLOSED, timeout=CLOSE_WAIT_SECONDS * 3)
    bench.check(common.scope_running(bench, scope) and bench.ui.find(apps.FORCE, role=apps.BUTTON),
                "APP-016: SIGTERM ignored, the dialog offers « Forcer la fermeture »")
    bench.check(all(call.arguments[2] == SIGTERM for call in kills(bench)), "APP-016: no SIGKILL before consent")
    bench.screenshot("pas-fermee")
    bench.ui.click(apps.CANCEL, role=apps.BUTTON)
    bench.ui.wait(apps.NOT_CLOSED.format(STUBBORN.name), gone=True)

    terms = len(kills(bench))
    apps.ask_uninstall(bench, STUBBORN.name, step=apps.CONFIRMING)
    apps.confirm(bench, apps.CLOSE_AND_UNINSTALL)
    apps.wait_step(bench, STUBBORN.name, apps.NOT_CLOSED, timeout=CLOSE_WAIT_SECONDS)
    bench.check(len(kills(bench)) == terms,
                "APP-016: on the second try, « Forcer la fermeture » comes at once, the scope not asked again")
    bench.ui.click(apps.FORCE, role=apps.BUTTON)
    apps.wait_done(bench, STUBBORN.name)
    bench.check(not common.scope_running(bench, scope) and not common.deb_installed(bench, STUBBORN.name),
                f"APP-016: forced by SIGKILL, {scope} closed and {STUBBORN.name} uninstalled")
    return scope


def every_scope_of_its_own(bench: Bench) -> tuple[str, str]:
    first = common.scope_unit(BROWSER.name, number=11)
    second = common.scope_unit(BROWSER.hidden_launchers[0], kind=common.OWN_SCOPE, number=12)
    other = common.scope_unit(BYSTANDER.name, number=13)
    for unit, package in ((first, BROWSER), (second, BROWSER), (other, BYSTANDER)):
        common.open_in_scope(bench, unit, [package.executable])
    apps.uninstall(bench, BROWSER.name, button=apps.CLOSE_AND_UNINSTALL)
    bench.check(not common.scope_running(bench, first) and not common.scope_running(bench, second),
                f"APP-017: both scopes of {BROWSER.name} closed, its second launcher's ({second}) too")
    bench.check(common.scope_running(bench, other), f"APP-017: {other}, another application's, still runs")
    return first, second


def flatpak_scopes(bench: Bench) -> str:
    """A flatpak runs in `app-flatpak-*`; a GNOME scope of the same id is not it."""
    own = common.scope_unit(FLATPAK_APP, kind=common.FLATPAK_SCOPE, number=21)
    lookalike = common.scope_unit(FLATPAK_APP, number=22)
    for unit in (own, lookalike):
        common.open_in_scope(bench, unit, ["sleep", "infinity"])
    apps.uninstall(bench, FLATPAK_NAME, apps.FLATPAK, query=FLATPAK_APP, button=apps.CLOSE_AND_UNINSTALL)
    bench.check(not common.scope_running(bench, own) and common.scope_running(bench, lookalike),
                f"APP-017: the flatpak's {own} closed, {lookalike} left")
    installed = bench.root("flatpak list --system --app --columns=application")
    bench.check(FLATPAK_APP not in installed, f"APP-017: {FLATPAK_APP} uninstalled once closed")
    return own


def okapi_in_its_scope(bench: Bench) -> None:
    """INV-406: Okapi started from the terminal it is asked to uninstall, in its scope."""
    bench.stop_app()
    scope = common.scope_unit(TERMINAL.name, number=31)
    okapi = f"{config.GUEST_BINARIES}/okapi-app"
    command = f"{shlex.quote(TERMINAL.executable)} & exec {shlex.quote(okapi)}"
    environment = {"WAYLAND_DISPLAY": "wayland-0", **bench.okapi_environment()}
    common.open_in_scope(bench, scope, ["sh", "-c", command], environment)
    bench.ui.wait(role="frame", timeout=60)
    apps.open_page(bench)
    before = len(kills(bench))
    apps.ask_uninstall(bench, TERMINAL.name)
    bench.check(not any("est ouverte" in label for label in apps.labels(bench))
                and bench.ui.find(apps.UNINSTALL, role=apps.BUTTON),
                "INV-406: Okapi runs in the terminal's scope: the terminal is not said open")
    apps.confirm(bench)
    apps.wait_done(bench, TERMINAL.name)
    bench.check(len(kills(bench)) == before and common.scope_running(bench, scope),
                f"INV-406: no KillUnit sent, {scope} where Okapi runs still lives")
    refused = apps.traced(bench, "Okapi lives by a scope of the application: never closed")
    bench.check(bool(refused), "INV-406: the journal says why the application is not closed")
    bench.user(shlex.join(["systemctl", "--user", "stop", scope]))


def run(bench: Bench) -> None:
    bench.set_elevation(common.Elevation.WITHOUT_PASSWORD, config.UNINSTALL_ACTIONS)
    packages = [SIMPLE, STUBBORN, BROWSER, BYSTANDER, TERMINAL]
    for package in packages:
        common.build_deb(bench, package)
    common.install_deb(bench, *packages)
    common.watch_bus(bench, f"destination='{SYSTEMD}'")

    bench.launch_app()
    apps.open_page(bench)
    pid = int(bench.user(shlex.join(["systemctl", "--user", "show", config.APP_UNIT, "--property=MainPID",
                                     "--value"])).strip())
    simple = closes_on_sigterm(bench)
    stubborn = forced(bench)
    first, second = every_scope_of_its_own(bench)
    flatpak = flatpak_scopes(bench)
    check_bus(bench, common.bus_names(bench, pid), {
        simple: {SIGTERM}, stubborn: {SIGTERM, SIGKILL}, first: {SIGTERM}, second: {SIGTERM}, flatpak: {SIGTERM},
    })
    okapi_in_its_scope(bench)
