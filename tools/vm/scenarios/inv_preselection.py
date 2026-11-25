"""Invariant 5: what slows the next launch, maintains, or is gone for good is never checked beforehand.

Old files are planted for each kind of line left to the user: cache resets
(fonts, shaders of Mesa, NVIDIA and Unity, Chromium's code and GPU caches,
Firefox's startup cache), maintenances (Docker's build cache, uv, pnpm, the
journal of systemd), the trash, a folder chosen in the settings; beside them,
a preselected thumbnail and pip cache. After the analysis, after a category's
and a browser's checkbox, and after the rights, none of the former is checked
while the latter are (INV-501 to 503). The old versions of snaps and the
unused flatpak environments are offered unchecked, when the machine holds
some (INV-503; the base image holds none). Choices kept in `selections.json` only ever uncheck: a valid file
naming a rule that does not exist, then one forged to say « checked », check
nothing left to the user (INV-504).
"""

from __future__ import annotations

import shlex
import time

from okapivm import cleaner, common
from okapivm.bench import Bench, ScenarioFailure

TAGS = ["invariants", "admin", "cleaner"]

SIZE = 16 * 1024
# What a category's checkbox says over a partial selection (results.slint).
PARTIAL = "sélection partielle"
NOTHING_CHECKED = (False, False)
TOGGLE_TIMEOUT_SECONDS = 10
TOGGLE_POLL_SECONDS = 0.5
SELECTIONS_TIMEOUT_SECONDS = 10
# The tab once read: its list, or the line that says it holds nothing (old-versions.slint).
OLD_VERSIONS_READ = "Aucune ancienne version à retirer|Retirer la sélection"
SYSTEM, BROWSERS, APPLICATIONS = cleaner.CATEGORIES[:3]
CHROMIUM = "Chromium"
SELECTIONS = "selections.json"
TRASH_DAYS = 40
# Beyond the trash rule's 30 days, which its entries' own inode dates must pass too.
AGING_SECONDS = 31 * 24 * 60 * 60
CHOSEN_FOLDER = "~/okapi-vm-folder"
DOCKER_CONTEXT = "/var/tmp/okapi-vm-docker"

THUMBNAILS = "Cache des miniatures"
PIP = "Cache de pip"
PRESELECTED = [THUMBNAILS, PIP]
CACHE_RESETS = ["Cache des polices", "Cache des shaders Mesa", "Cache des shaders NVIDIA", 
                # Unity's root is shared with the game's rule: one line, named after it (INV-505).
                "Unity",
                "Cache du code", "Cache GPU", "Cache de démarrage"]
MAINTENANCES = ["Cache de construction Docker", "Paquets uv inutilisés", "Paquets pnpm inutilisés"]
TRASH = "Corbeille"
ADMIN_LEFT = ["Journaux de systemd", "Cache des polices du système"]

FILES = [
    "~/.cache/thumbnails/normal/okapi-vm.png",
    "~/.cache/pip/http/okapi-vm",
    "~/.cache/fontconfig/okapi-vm.cache-9",
    "~/.cache/mesa_shader_cache/okapi-vm",
    "~/.cache/nvidia/GLCache/okapi-vm",
    "~/.cache/unity3d/okapi-vm",
    "~/.config/chromium/Default/Preferences",
    "~/.config/chromium/Default/Code Cache/js/okapi-vm",
    "~/.config/chromium/Default/GPUCache/data_0",
    "~/.cache/chromium/Default/Cache/Cache_Data/okapi-vm",
    "~/.mozilla/firefox/okapi-vm.default/prefs.js",
    # Its lock, unheld: a profile launched once, closed now.
    "~/.mozilla/firefox/okapi-vm.default/.parentlock",
    "~/.cache/mozilla/firefox/okapi-vm.default/startupCache/okapi-vm",
    "~/.cache/mozilla/firefox/okapi-vm.default/cache2/entries/okapi-vm",
    "~/.cache/uv/okapi-vm",
    "~/.local/share/pnpm/store/v10/files/okapi-vm",
    "~/.local/share/Trash/files/okapi-vm.txt",
    f"{CHOSEN_FOLDER}/okapi-vm.txt",
]
TRASH_INFO = "~/.local/share/Trash/info/okapi-vm.txt.trashinfo"
CRASH = "/var/crash/okapi-vm.crash"


def run(bench: Bench) -> None:
    tester = bench.accounts.admin.name
    bench.allow_elevation_without_password()
    common.write_settings(bench, {"customFolders": [common.expand_home(bench, CHOSEN_FOLDER)]})
    docker_build_cache(bench)
    common.plant(bench, common.Files(tester, FILES, size=SIZE), common.Files(common.ROOT, [CRASH], size=SIZE))
    # Aged with it: a `.trashinfo` written since the cutoff keeps its entry.
    trashed(bench)
    bench.advance_clock(AGING_SECONDS)
    # After a jump of weeks, the session's accessibility bus no longer lists a new window.
    bench.restart_session()

    analyse(bench)
    bench.screenshot("resultats")
    left = [*CACHE_RESETS, *MAINTENANCES, TRASH, CHOSEN_FOLDER]
    check_states(bench, PRESELECTED, left, "after the analysis")

    for category in (APPLICATIONS, BROWSERS, CHROMIUM):
        toggle_twice(bench, category)
        check_states(bench, PRESELECTED, left, f"after « {category} » unchecked then checked")

    # Their banner, above the results, says when they are obtained.
    cleaner.to_top(bench)
    cleaner.obtain_rights(bench)
    check_states(bench, PRESELECTED, [*left, *ADMIN_LEFT], "after the rights")
    toggle_twice(bench, SYSTEM)
    check_states(bench, PRESELECTED, [*left, *ADMIN_LEFT], f"after the rights and « {SYSTEM} » toggled")
    bench.screenshot("apres-droits")

    kept_choices(bench, left)
    old_versions(bench)


def docker_build_cache(bench: Bench) -> None:
    """A build cache in the machine's Docker, made offline (`FROM scratch`), its
    socket open to the account as the session's group would make it."""
    context = shlex.quote(DOCKER_CONTEXT)
    bench.root(f"mkdir -p {context} && echo okapi-vm > {context}/file "
               f"&& printf 'FROM scratch\\nCOPY file /file\\n' > {context}/Dockerfile "
               f"&& docker build -q -t okapi-vm-build {context} && docker rmi okapi-vm-build "
               "&& chmod 666 /run/docker.sock")


def trashed(bench: Bench) -> None:
    """The planted file of the trash, said put there long ago, by its `.trashinfo`."""
    deleted = bench.user(f"date -d '{TRASH_DAYS} days ago' +%Y-%m-%dT%H:%M:%S").strip()
    info = f"[Trash Info]\nPath={common.expand_home(bench, '~/okapi-vm.txt')}\nDeletionDate={deleted}\n"
    bench.user(f"mkdir -p -- \"$(dirname -- {common.shell_path(TRASH_INFO)})\" "
               f"&& printf %s {shlex.quote(info)} > {common.shell_path(TRASH_INFO)}")


def analyse(bench: Bench) -> None:
    """A fresh interface's analysis of every category, the chosen folders included."""
    bench.launch_app()
    cleaner.open_page(bench)
    for category in cleaner.CATEGORIES:
        cleaner.set_checked(bench, category, True)
    if (title := cleaner.analyze(bench)) != cleaner.RESULTS:
        raise ScenarioFailure(f"the analysis ended on « {title} », not « {cleaner.RESULTS} »")


def checked(node: dict) -> bool:
    return cleaner.CHECKED in node["states"]


def check_states(bench: Bench, preselected: list[str], left: list[str], when: str) -> None:
    """Those of `preselected` checked, those `left` to the user not, all offered."""
    shown = cleaner.rows(bench)
    missing = [name for name in [*preselected, *left] if name not in shown]
    if missing:
        raise ScenarioFailure(f"lines never offered {when}: {missing} (offered: {sorted(shown)})")
    for name in preselected:
        bench.check(checked(shown[name]), f"INV-501: the preselected « {name} » is checked {when}")
    for name in left:
        entry = "INV-501" if name in CACHE_RESETS else "INV-502" if name in MAINTENANCES + ADMIN_LEFT else "INV-503"
        bench.check(not checked(shown[name]), f"{entry}: « {name} » is unchecked {when}")


def selection(node: dict) -> tuple[bool, bool]:
    """Whether a checkbox is checked, and whether it says only part of what is below it is."""
    return checked(node), PARTIAL in node.get("description", "")


def toggle(bench: Bench, line: str) -> tuple[bool, bool]:
    """Clicks a category's or a browser's checkbox at its pixels and gives what
    it then says. A partial one reads « unchecked » to AT-SPI and stays so once
    clicked, which the driver's click takes for a click lost."""
    cleaner.to_top(bench)
    before = selection(cleaner.row(bench, line))
    bench.click_at(*bench.ui.center(bench.ui.node(line, role=cleaner.CHECK_BOX)))
    deadline = time.monotonic() + TOGGLE_TIMEOUT_SECONDS
    while (now := selection(bench.ui.node(line, role=cleaner.CHECK_BOX))) == before:
        if time.monotonic() > deadline:
            raise ScenarioFailure(f"« {line} » still says {before} once clicked")
        time.sleep(TOGGLE_POLL_SECONDS)
    return now


def toggle_twice(bench: Bench, line: str) -> None:
    """A category's or a browser's checkbox clicked until it has unchecked
    everything below it, then once more: it checks what is preselected."""
    cleaner.to_top(bench)
    if selection(cleaner.row(bench, line)) != NOTHING_CHECKED:
        bench.check(toggle(bench, line) == NOTHING_CHECKED, f"« {line} » clicked unchecks everything below it")
    bench.check(toggle(bench, line) != NOTHING_CHECKED, f"« {line} » clicked again checks what is preselected")


def kept_choices(bench: Bench, left: list[str]) -> None:
    """INV-504: what `selections.json` says only unchecks."""
    cleaner.row(bench, THUMBNAILS)
    bench.ui.click(THUMBNAILS, role=cleaner.CHECK_BOX)
    deadline = time.monotonic() + SELECTIONS_TIMEOUT_SECONDS
    kept = common.read_store(bench, SELECTIONS)
    while not kept and time.monotonic() < deadline:
        time.sleep(1)
        kept = common.read_store(bench, SELECTIONS)
    if not isinstance(kept, dict) or not kept.get("unchecked"):
        raise ScenarioFailure(f"Okapi kept no choice in {SELECTIONS} once « {THUMBNAILS} » was unchecked: {kept}")
    bench.stop_app()

    unknown = ["systemTarget", "Okapi VM", "/okapi-vm/nowhere"]
    common.write_store(bench, SELECTIONS, {"version": kept["version"], "unchecked": [*kept["unchecked"], unknown]})
    analyse(bench)
    check_states(bench, [PIP], left, "with kept choices naming an unknown rule")
    bench.check(not checked(cleaner.rows(bench)[THUMBNAILS]),
                "INV-504: the line the user unchecked stays unchecked, an unknown rule beside it")
    bench.stop_app()

    forged = {"version": kept["version"], "unchecked": [], "checked": kept["unchecked"]}
    common.write_store(bench, SELECTIONS, forged)
    analyse(bench)
    check_states(bench, PRESELECTED, left, "with choices forged to say « checked »")
    bench.stop_app()


def old_versions(bench: Bench) -> None:
    """INV-503: the old versions of snaps and the unused flatpak environments, offered unchecked."""
    bench.launch_app()
    common.open_screen(bench, "Applications")
    bench.ui.click("Anciennes versions")
    bench.ui.wait(pattern=OLD_VERSIONS_READ, timeout=cleaner.SCAN_TIMEOUT_SECONDS)
    bench.screenshot("anciennes-versions")
    lines = [node for node in bench.ui.find(role="list-item") if "checkable" in node["states"]]
    if not lines:
        # The base image holds no disabled snap revision, and Okapi judges its
        # flatpak runtimes all used: nothing to offer, so nothing proven here.
        bench.log("INV-503: no old version in this image, their preselection not tried")
        return
    bench.check(not any(checked(node) for node in lines),
                f"INV-503: the old versions are offered unchecked ({[node['name'] for node in lines]})")
