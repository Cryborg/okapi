"""Applications: a deb uninstalled through PackageKit, with what depends on it, or refused.

Fake packages: an application with a plugin that depends on it, one a
metapackage holds, one whose removal takes long. The confirmation tells what
goes; a list changed meanwhile is shown again; polkit's refusal, a package
gone already, a lock held by another installation and a missing PackageKit
each leave the package; closing Okapi while it removes takes two closings
(APP-007 to 012, NAV-011).
"""

from __future__ import annotations

import shlex
import time

from okapivm import apps, common, config
from okapivm.bench import Bench

TAGS = ["apps", "packagekit"]

# A removal long enough to be seen running: dpkg runs it, PackageKit can no longer cancel.
SLOW_REMOVAL_SECONDS = 20
SLOW_PRERM = f"#!/bin/sh\nsleep {SLOW_REMOVAL_SECONDS}\n"
DPKG_LOCK = "/var/lib/dpkg/lock-frontend"
LOCK_UNIT = "okapi-vm-dpkg-lock"
LOCK_SECONDS = 25
PACKAGEKIT = "packagekit"
PACKAGEKIT_BUS_SERVICE = "/usr/share/dbus-1/system-services/org.freedesktop.PackageKit.service"
PACKAGEKIT_BUS_HIDDEN = f"{PACKAGEKIT_BUS_SERVICE}.okapi-vm"

EDITOR = common.Package("okapi-vm-editor")
PLUGIN = common.Package("okapi-vm-plugin", application=False, fields=(("Depends", EDITOR.name),))
CORE = common.Package("okapi-vm-core")
META = common.Package("okapi-vm-desktop", application=False,
                      fields=(("Depends", CORE.name), ("Section", "metapackages")))
VIEWER = common.Package("okapi-vm-viewer")
VIEWER_PLUGIN = common.Package("okapi-vm-viewer-plugin", application=False, fields=(("Depends", VIEWER.name),))
REFUSED = common.Package("okapi-vm-refused")
GONE = common.Package("okapi-vm-gone")
LOCKED = common.Package("okapi-vm-locked")
SLOW = common.Package("okapi-vm-slow", scripts=(("prerm", SLOW_PRERM),))
CLOSED = common.Package("okapi-vm-closed", scripts=(("prerm", SLOW_PRERM),))
ABSENT = common.Package("okapi-vm-absent")


def installed(bench: Bench, *packages: common.Package) -> list[str]:
    return [package.name for package in packages if common.deb_installed(bench, package.name)]


def with_dependent(bench: Bench) -> None:
    apps.ask_uninstall(bench, EDITOR.name)
    texts = apps.labels(bench)
    bench.check(any(text.startswith(f"{EDITOR.name} et un paquet qui en dépend partiront. Place libérée : ")
                    for text in texts) and f"Ce qui partirait : {EDITOR.name}, {PLUGIN.name}" in texts,
                f"APP-007: the simulation tells {PLUGIN.name} goes with {EDITOR.name}, and the space freed")
    bench.screenshot("confirmation-deb")
    apps.confirm(bench)
    toast = apps.wait_done(bench, EDITOR.name)
    bench.check(toast.startswith(f"{EDITOR.name} désinstallée : ") and toast.endswith(" libérés"),
                f"APP-007: « {toast} »")
    bench.check(not installed(bench, EDITOR, PLUGIN), f"APP-007: {EDITOR.name} and {PLUGIN.name} are gone (dpkg)")
    line = apps.last_uninstall(bench) or {}
    bench.check(line.get("source") == "deb" and line.get("name") == EDITOR.name
                and set(line.get("packages", [])) == {EDITOR.name, PLUGIN.name} and line.get("freedBytes", 0) > 0,
                f"APP-007: the history holds the uninstallation ({line})")


def held_by_metapackage(bench: Bench) -> None:
    apps.ask_uninstall(bench, CORE.name, step=apps.REFUSED)
    bench.check("La désinstaller retirerait aussi des paquets dont le système a besoin. Rien n'a été modifié."
                in apps.labels(bench), f"APP-008: {META.name}, a metapackage, refuses the removal")
    bench.screenshot("refus-metapaquet")
    bench.ui.click(apps.CLOSE, role=apps.BUTTON)
    bench.check(installed(bench, CORE, META) == [CORE.name, META.name], "APP-008: both packages are still there")


def list_changed(bench: Bench) -> None:
    apps.ask_uninstall(bench, VIEWER.name)
    bench.check("Place libérée : " in " ".join(apps.labels(bench))
                and not any(VIEWER_PLUGIN.name in text for text in apps.labels(bench)),
                f"APP-009: {VIEWER.name} goes alone at first")
    common.install_deb(bench, VIEWER_PLUGIN)
    apps.confirm(bench)
    bench.ui.wait(pattern=f"^Ce qui partirait : .*{VIEWER_PLUGIN.name}", timeout=60)
    bench.check(installed(bench, VIEWER, VIEWER_PLUGIN) == [VIEWER.name, VIEWER_PLUGIN.name]
                and bool(apps.traced(bench, "what would go changed since it was shown: nothing removed")),
                f"APP-009: {VIEWER_PLUGIN.name} installed meanwhile: nothing removed, the new list shown")
    bench.ui.click(apps.CANCEL, role=apps.BUTTON)


def refused_by_polkit(bench: Bench) -> None:
    bench.set_elevation(common.Elevation.REFUSED, config.UNINSTALL_ACTIONS)
    apps.ask_uninstall(bench, REFUSED.name)
    apps.confirm(bench)
    apps.wait_step(bench, REFUSED.name, apps.FAILED)
    bench.check("L'autorisation n'a pas été donnée." in apps.labels(bench) and installed(bench, REFUSED),
                f"APP-010: polkit refuses, {REFUSED.name} stays")
    bench.ui.click(apps.CLOSE, role=apps.BUTTON)
    bench.set_elevation(common.Elevation.WITHOUT_PASSWORD, config.UNINSTALL_ACTIONS)


def gone_meanwhile(bench: Bench) -> None:
    apps.ask_uninstall(bench, GONE.name)
    bench.root(shlex.join(["dpkg", "--remove", GONE.name]))
    apps.confirm(bench)
    apps.wait_step(bench, GONE.name, apps.ALREADY_GONE)
    bench.check(not bench.ui.find(apps.RETRY, role=apps.BUTTON), f"APP-010: « {apps.ALREADY_GONE.format(GONE.name)} », nothing to retry")
    bench.ui.click(apps.CLOSE, role=apps.BUTTON)


def another_installation(bench: Bench) -> None:
    bench.root(shlex.join(["systemd-run", "--quiet", f"--unit={LOCK_UNIT}", "flock", DPKG_LOCK,
                           "sleep", str(LOCK_SECONDS)]))
    apps.ask_uninstall(bench, LOCKED.name)
    apps.confirm(bench)
    waiting = "Une autre installation est en cours : Okapi attend son tour."
    busy = "Une autre installation est en cours : réessayez dans un moment."
    seen = bench.ui.wait(pattern=f"^({waiting}|{busy})$", timeout=60)[0]["name"]
    bench.check(installed(bench, LOCKED) == [LOCKED.name], f"APP-010: dpkg's lock held: « {seen} », nothing removed")
    bench.screenshot("verrou")
    if seen == waiting:
        apps.wait_done(bench, LOCKED.name)
        bench.check(not installed(bench, LOCKED), f"APP-010: the lock released, {LOCKED.name} goes")
    else:
        bench.ui.click(apps.CLOSE, role=apps.BUTTON)


def too_late(bench: Bench) -> None:
    apps.ask_uninstall(bench, SLOW.name)
    apps.confirm(bench)
    apps.wait_step(bench, SLOW.name, apps.REMOVING)
    bench.ui.wait("Trop tard pour annuler : le système termine.", timeout=60)
    bench.check(installed(bench, SLOW) == [SLOW.name], "APP-011: dpkg runs: « Trop tard pour annuler »")
    apps.wait_done(bench, SLOW.name)


def close_twice(bench: Bench) -> None:
    apps.ask_uninstall(bench, CLOSED.name)
    apps.confirm(bench)
    apps.wait_step(bench, CLOSED.name, apps.REMOVING)
    bench.key("Alt+F4")
    bench.ui.wait("Une désinstallation est en cours. Fermez encore Okapi pour le quitter quand même : elle se "
                  "terminera sans lui, et ne sera pas inscrite à l'historique.", timeout=30)
    bench.check(bool(apps.traced(bench, "closing asked during an uninstallation: held once")),
                "APP-012, NAV-011: the first closing is held, the dialog says why")
    bench.screenshot("fermeture-retenue")
    before = len(common.read_store(bench, common.HISTORY) or [])
    bench.key("Alt+F4")
    bench.app_exit_code()
    deadline = time.monotonic() + SLOW_REMOVAL_SECONDS * 3
    while installed(bench, CLOSED) and time.monotonic() < deadline:
        time.sleep(1)
    after = len(common.read_store(bench, common.HISTORY) or [])
    bench.check(not installed(bench, CLOSED) and after == before,
                f"APP-012, NAV-011: the second closing quits; {CLOSED.name} removed without Okapi, not recorded")


def service_absent(bench: Bench) -> None:
    bench.root(" && ".join([
        shlex.join(["systemctl", "stop", PACKAGEKIT]),
        shlex.join(["mv", PACKAGEKIT_BUS_SERVICE, PACKAGEKIT_BUS_HIDDEN]),
        shlex.join(["busctl", "call", "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                    "ReloadConfig"]),
    ]))
    bench.launch_app()
    apps.open_page(bench)
    apps.ask_uninstall(bench, ABSENT.name, step=apps.FAILED)
    bench.check("Le service de désinstallation est absent de ce système." in apps.labels(bench)
                and installed(bench, ABSENT), "APP-010: PackageKit absent: said, nothing removed")
    bench.root(shlex.join(["mv", PACKAGEKIT_BUS_HIDDEN, PACKAGEKIT_BUS_SERVICE]))


def run(bench: Bench) -> None:
    bench.set_elevation(common.Elevation.WITHOUT_PASSWORD, config.UNINSTALL_ACTIONS)
    packages = [EDITOR, PLUGIN, CORE, META, VIEWER, VIEWER_PLUGIN, REFUSED, GONE, LOCKED, SLOW, CLOSED, ABSENT]
    for package in packages:
        common.build_deb(bench, package)
    common.install_deb(bench, *(package for package in packages if package is not VIEWER_PLUGIN))

    bench.launch_app()
    apps.open_page(bench)
    with_dependent(bench)
    held_by_metapackage(bench)
    list_changed(bench)
    refused_by_polkit(bench)
    gone_meanwhile(bench)
    another_installation(bench)
    too_late(bench)
    close_twice(bench)
    service_absent(bench)
