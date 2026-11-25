"""Updates: installing the system's, and each way it can end short.

Ubuntu's repositories set aside, a signed suite of the bench offers new
versions: one that takes an installed package away and one that brings a new
one. The modal says what goes, « Annuler » installs nothing (MAJ-007); polkit
refusing installs nothing (MAJ-009); accepted, the update installs, says so
and writes its history line (MAJ-005). Then one package at a time: its suite
changed between the search and the update (MAJ-008), unsigned (MAJ-006), dpkg
cut short, no room to download, a download cancelled (MAJ-009).

MAJ-008's downgrade is not reachable: PackageKit never offers a lower version,
even pinned above the installed one. The tests of `okapi-tools` keep it.
"""

from __future__ import annotations

import shlex

from okapivm import common, config
from okapivm.bench import Bench
from scenarios import _updates as updates

TAGS = ["updates", "packagekit"]

REMOVER = "okapi-pa"
BRINGER = "okapi-pb"
VICTIM = "okapi-victim"
BROUGHT = "okapi-new"
SINGLE = "okapi-pc"
NOTHING_INSTALLED = "Aucune mise à jour installée"
UPDATE_ANYWAY = "Mettre à jour quand même"
CANCEL = "Annuler"
DPKG_PENDING = "/var/lib/dpkg/updates/0000"
ARCHIVES = "/var/cache/apt/archives"
ARCHIVES_SIZE = "1M"
# Four times what ARCHIVES can hold.
PAYLOAD_BYTES = 4 * 1024 * 1024
# PackageKit waits for dpkg's lock about a minute before it gives up.
INTERRUPTED_TIMEOUT_SECONDS = 240


def run(bench: Bench) -> None:
    installed = [common.Package(name) for name in (REMOVER, BRINGER, VICTIM, SINGLE)]
    updates.prepare(bench, *installed)
    bench.root(shlex.join(["apt-mark", "auto", VICTIM]))
    updates.offer(bench, common.Package(REMOVER, "2.0", fields=(("Conflicts", VICTIM), ("Replaces", VICTIM),
                                                                  ("Provides", VICTIM))),
                  common.Package(BRINGER, "2.0", fields=(("Depends", BROUGHT),)), common.Package(BROUGHT))
    bench.launch_app()
    common.open_screen(bench, updates.PAGE)
    updates.search_now(bench, pattern="echerché à l'instant$")

    removal_refused(bench)
    rights_refused(bench)
    installed_all(bench)
    changed(bench)
    unsigned(bench)
    interrupted(bench)
    no_space(bench)
    cancelled(bench)


def ask_update(bench: Bench) -> None:
    bench.ui.click(pattern=updates.SYSTEM_BUTTON, role="button")


def ask_with_removal(bench: Bench) -> list[list[dict]]:
    """The update asked, and what its modal says of the removal."""
    ask_update(bench)
    return [bench.ui.wait("Cette mise à jour retire 1 paquet", timeout=updates.SEARCH_TIMEOUT_SECONDS),
            bench.ui.wait("Pour installer les nouvelles versions, le système doit retirer ces paquets :", role="label"),
            bench.ui.wait(pattern=f"^{VICTIM}, ", role="list-item"),
            bench.ui.wait("Elle installe aussi 1 nouveau paquet.", role="label")]


def unchanged(bench: Bench, before: dict[str, str]) -> bool:
    return updates.versions(bench, *before) == before


def close(bench: Bench) -> None:
    bench.ui.click(updates.CLOSE, role="button")
    bench.ui.wait(updates.SEARCH_NOW, role="button")


def ended(bench: Bench, mark: int) -> str:
    return str(updates.wait_event(bench, mark, updates.INSTALL_ENDED, updates.INSTALL_TIMEOUT_SECONDS).get("ended"))


def removal_refused(bench: Bench) -> None:
    """MAJ-007."""
    before = updates.versions(bench, REMOVER, BRINGER, VICTIM, BROUGHT)
    mark = len(common.journal(bench))
    said = ask_with_removal(bench)
    bench.screenshot("retrait")
    bench.check(all(said), f"MAJ-007: the modal says the update removes {VICTIM} and brings one package")
    bench.ui.click(CANCEL, role="button")
    bench.ui.wait(updates.SEARCH_NOW, role="button")
    refused = updates.events(bench, mark, "what the update removes refused: nothing installed")
    bench.check(bool(refused) and unchanged(bench, before),
                f"MAJ-007: « Annuler » installs nothing, every version as it was ({updates.versions(bench, *before)})")


def rights_refused(bench: Bench) -> None:
    """MAJ-009, the rights refused."""
    bench.set_elevation(common.Elevation.REFUSED, config.UPDATE_ACTIONS)
    before = updates.versions(bench, REMOVER, BRINGER, VICTIM, BROUGHT)
    mark = len(common.journal(bench))
    ask_with_removal(bench)
    bench.ui.click(UPDATE_ANYWAY, role="button")
    said = [bench.ui.wait("L'autorisation n'a pas été donnée.", timeout=updates.INSTALL_TIMEOUT_SECONDS),
            bench.ui.wait(NOTHING_INSTALLED)]
    bench.screenshot("autorisation-refusee")
    end = ended(bench, mark)
    bench.check(all(said) and "Refused" in end and unchanged(bench, before),
                f"MAJ-009: polkit refusing, nothing is installed and the screen says so ({end})")
    close(bench)
    bench.set_elevation(common.Elevation.WITHOUT_PASSWORD, config.UPDATE_ACTIONS)


def installed_all(bench: Bench) -> None:
    """MAJ-005."""
    mark = len(common.journal(bench))
    ask_with_removal(bench)
    bench.ui.click(UPDATE_ANYWAY, role="button")
    said = bench.ui.wait("2 mises à jour installées", timeout=updates.INSTALL_TIMEOUT_SECONDS)
    bench.screenshot("installees")
    phases = {event.get("phase") for event in updates.events(bench, mark, "the update's phase")}
    now = updates.versions(bench, REMOVER, BRINGER, BROUGHT)
    bench.check(bool(said) and now == {REMOVER: "2.0", BRINGER: "2.0", BROUGHT: "1.0"}
                and not common.deb_installed(bench, VICTIM),
                f"MAJ-005: « 2 mises à jour installées », dpkg holds the new versions and {VICTIM} is gone ({now})")
    bench.check("Installing" in phases, f"MAJ-005: the progress went through dpkg's installation ({phases})")
    receipt = common.last_receipt(bench)
    names = {package["name"]: (package["version"], package["previousVersion"], package["done"])
             for package in receipt.get("packages", [])}
    bench.check(receipt.get("kind") == "update" and receipt.get("source") == "deb" and receipt.get("done") is True
                and names.get(REMOVER) == ("2.0", "1.0", True) and names.get(BRINGER) == ("2.0", "1.0", True),
                f"MAJ-005: the history's line says both updates done ({receipt})")
    close(bench)


def changed(bench: Bench) -> None:
    """MAJ-008, the list changed since the search."""
    updates.offer(bench, common.Package(SINGLE, "2.0"))
    updates.search_now(bench, pattern="^1 mise à jour, recherché à l'instant$")
    updates.offer(bench, common.Package(SINGLE, "3.0"))
    mark = len(common.journal(bench))
    ask_update(bench)
    said = bench.ui.wait("Les mises à jour ont changé depuis la recherche. La liste vient d'être relue : réessayez.",
                         timeout=updates.INSTALL_TIMEOUT_SECONDS)
    bench.screenshot("liste-changee")
    end = ended(bench, mark)
    bench.check(bool(said) and "Changed" in end and updates.version(bench, SINGLE) == "1.0",
                f"MAJ-008: a list changed since the search installs nothing and says so ({end})")
    bench.ui.click(updates.RETRY, role="button")
    retried = bench.ui.wait("1 mise à jour installée", timeout=updates.INSTALL_TIMEOUT_SECONDS)
    bench.check(bool(retried) and updates.version(bench, SINGLE) == "3.0",
                f"MAJ-008: « Réessayer » installs the version read again ({updates.version(bench, SINGLE)})")
    close(bench)


def fails(bench: Bench, entry: str, label: str, message: str, why: str, *,
          timeout: float = updates.INSTALL_TIMEOUT_SECONDS) -> None:
    """The single package's update asked, ending short with `message`: nothing installed."""
    updates.search_now(bench, pattern="^1 mise à jour, recherché à l'instant$")
    mark = len(common.journal(bench))
    ask_update(bench)
    said = [bench.ui.wait(message, timeout=timeout), bench.ui.wait(NOTHING_INSTALLED)]
    bench.screenshot(label)
    end = ended(bench, mark)
    receipt = common.last_receipt(bench)
    bench.check(all(said) and why in end and updates.version(bench, SINGLE) == "3.0",
                f"{entry}: « {message} », {SINGLE} still 3.0 ({end}, history {receipt.get('reason')})")
    close(bench)


def unsigned(bench: Bench) -> None:
    """MAJ-006."""
    updates.offer(bench, common.Package(SINGLE, "4.0"), signed=False)
    fails(bench, "MAJ-006", "non-signe", "Des paquets ne sont pas signés par une source de confiance.", "Unsigned")


def interrupted(bench: Bench) -> None:
    """MAJ-009, dpkg cut short."""
    updates.offer(bench, common.Package(SINGLE, "4.0"))
    bench.root(f"touch {DPKG_PENDING}")
    bench.log("dpkg left a pending update: cut short")
    fails(bench, "MAJ-009", "dpkg-interrompu",
          "Une installation précédente a été interrompue. Elle doit être terminée avant toute autre : "
          "lancez « sudo dpkg --configure -a » dans un terminal.", "Interrupted",
          timeout=INTERRUPTED_TIMEOUT_SECONDS)
    bench.check(common.last_receipt(bench).get("reason") == "interrupted",
                "MAJ-009: the history's line says the installation was cut short")
    bench.root(f"rm -- {DPKG_PENDING}")


def no_space(bench: Bench) -> None:
    """MAJ-009, no room left."""
    updates.offer(bench, common.Package(SINGLE, "5.0", payload=PAYLOAD_BYTES))
    bench.root("\n".join([f"mount -t tmpfs -o size={ARCHIVES_SIZE} tmpfs {ARCHIVES}", f"install -d {ARCHIVES}/partial"]))
    bench.log(f"{ARCHIVES} holds {ARCHIVES_SIZE}, the package {PAYLOAD_BYTES} bytes")
    fails(bench, "MAJ-009", "plus-de-place", "Il n'y a plus assez de place sur le disque.", "NoSpace")
    bench.root(f"umount {ARCHIVES}")


def cancelled(bench: Bench) -> None:
    """MAJ-009, « Annuler » while downloading."""
    common.stall_apt_downloads(bench)
    updates.search_now(bench, pattern="^1 mise à jour, recherché à l'instant$")
    mark = len(common.journal(bench))
    ask_update(bench)
    bench.ui.wait(pattern=r"^Système .* : téléchargement…", role="label", timeout=updates.INSTALL_TIMEOUT_SECONDS)
    bench.screenshot("telechargement")
    bench.ui.click(CANCEL, role="button")
    said = [bench.ui.wait("Mise à jour annulée", timeout=updates.INSTALL_TIMEOUT_SECONDS),
            bench.ui.wait("Rien n'a été installé.", role="label")]
    bench.screenshot("annulee")
    end = ended(bench, mark)
    bench.check(all(said) and "Cancelled" in end and updates.version(bench, SINGLE) == "3.0",
                f"MAJ-009: « Annuler » while downloading stops the update, nothing installed ({end})")
