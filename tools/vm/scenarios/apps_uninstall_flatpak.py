"""Applications: a system flatpak uninstalled through flatpak's system helper.

The image's `io.mpv.Mpv` goes: the confirmation tells its space and what it
leaves, flatpak's system helper is asked to uninstall it then to prune its
repository (seen on the system bus), its runtime stays, and the history
holds the line (APP-014).
"""

from __future__ import annotations

from okapivm import apps, common, config
from okapivm.bench import Bench

TAGS = ["apps", "flatpak"]

APP = "io.mpv.Mpv"
NAME = "Lecteur multimédia mpv"
RUNTIME = "org.freedesktop.Platform"
HELPER = "org.freedesktop.Flatpak.SystemHelper"
LEAVES = ("Ses données dans votre dossier personnel restent en place, ainsi que les environnements qu'elle "
          "utilisait.")


def installed(bench: Bench, kind: str) -> str:
    return bench.root(f"flatpak list --system --{kind} --columns=application")


def run(bench: Bench) -> None:
    bench.set_elevation(common.Elevation.WITHOUT_PASSWORD, config.UNINSTALL_ACTIONS)
    common.watch_bus(bench, f"interface='{HELPER}'", system=True)
    bench.launch_app()
    apps.open_page(bench)

    apps.ask_uninstall(bench, NAME, apps.FLATPAK, query=APP)
    texts = apps.labels(bench)
    bench.check(LEAVES in texts and any(text.startswith("Place libérée : ") for text in texts),
                f"APP-014: the confirmation tells the space freed and what stays ({texts})")
    bench.screenshot("confirmation-flatpak")
    apps.confirm(bench)
    toast = apps.wait_done(bench, NAME)
    bench.check(APP not in installed(bench, "app") and RUNTIME in installed(bench, "runtime"),
                f"APP-014: {APP} uninstalled, its runtime left ({toast!r})")

    calls = [call for call in common.bus_calls(bench, system=True) if call.interface == HELPER]
    members = [call.member for call in calls]
    first = next((call for call in calls if call.member == "Uninstall"), None)
    bench.check(first is not None and any(f"app/{APP}/" in argument for argument in first.arguments),
                f"APP-014: the helper is asked to uninstall the application first ({members})")
    bench.check("PruneLocalRepo" in members and members.index("PruneLocalRepo") > members.index("Uninstall"),
                f"APP-014: then its repository pruned ({members})")
    bench.check(set(members) <= {"Uninstall", "PruneLocalRepo", "RunTriggers"},
                f"APP-014: nothing else is asked of flatpak's helper ({members})")
    extensions = "Certaines de ses extensions sont restées installées"
    bench.log(f"APP-014 partial: « {extensions} » needs an extension of the application that fails to go; "
              f"the image's {APP} has none")
    line = apps.last_uninstall(bench) or {}
    bench.check(line.get("source") == "flatpak" and line.get("name") == NAME,
                f"APP-014: the history holds the uninstallation ({line})")
