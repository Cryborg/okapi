"""Rules: the applications' caches, their own minimal ages, and the maintenances done by hand.

Discord, VS Code and Slack, each cache of their rule planted, are a group
each (RGL-015). Cursor waits a day and Figma a week: absent at 2 hours,
Cursor offered at 2 days and Figma not yet (RGL-016). A sample of the
developer tools is offered, the gems through each version's `cache`
alone (RGL-017). Hugging Face's Xet cache, at 8 days, is offered unchecked
(RGL-019). uv's and pnpm's unused packages are a note to do by hand: their
box never checkable (INV-506), « Copier » puts their command in the
clipboard and nothing runs them, the cleaning neither (RGL-018).
"""

from __future__ import annotations

import shlex

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["rules", "apps"]

HOUR_SECONDS = 60 * 60
DAY_SECONDS = 24 * HOUR_SECONDS
SIZE = 4096
TMPFILES_TIMER = "systemd-tmpfiles-clean.timer"
APPS_OPTION = "--apps"
APPLICATIONS = "Applications"
ENTRY = "okapi-vm"
ELECTRON = ["Cache/Cache_Data", "Code Cache", "GPUCache"]
# Each group by its English label (the CLI's), its French one, and the folders planted.
GROUPS = {
    "Discord": ("Discord", [f"~/.config/discord/{folder}" for folder in ELECTRON]),
    "VS Code": ("VS Code", [f"~/.config/Code/{folder}"
                            for folder in [*ELECTRON, "logs", "CachedExtensionVSIXs", "CachedData"]]),
    "Slack": ("Slack", [f"~/.config/Slack/{folder}" for folder in ELECTRON]),
    "npm Cache": ("Cache de npm", ["~/.npm/_cacache"]),
    "pip Cache": ("Cache de pip", ["~/.cache/pip"]),
    "Cargo/Rust Cache": ("Cache de Cargo (Rust)", ["~/.cargo/registry/cache"]),
    "Gradle Cache": ("Cache de Gradle", ["~/.gradle/caches"]),
    "Maven Cache": ("Cache de Maven", ["~/.m2/repository"]),
    "Ruby Gem Cache": ("Cache des gems Ruby", ["~/.gem/ruby/cache"]),
    "Conda Package Cache": ("Cache des paquets Conda", ["~/miniconda3/pkgs"]),
}
ELECTRON_GROUPS = ["Discord", "VS Code", "Slack"]
# Beside a gem's `cache`: never offered (`childSubdir`).
GEM_SPECIFICATION = f"~/.gem/ruby/specifications/{ENTRY}.gemspec"
CURSOR = "Cursor IDE"
CURSOR_FILE = f"~/.config/Cursor/GPUCache/{ENTRY}"
FIGMA = "Figma"
FIGMA_FILE = f"~/.config/figma-linux/GPUCache/{ENTRY}"
XET = "Hugging Face Xet Cache"
XET_FRENCH = "Cache Xet de Hugging Face"
XET_FILE = f"~/.cache/huggingface/xet/{ENTRY}"
# The ages at which each check looks, from the planting.
SETTLED_SECONDS = 2 * HOUR_SECONDS
CURSOR_SECONDS = 2 * DAY_SECONDS
XET_SECONDS = 8 * DAY_SECONDS
BY_HAND_NOTE = "À faire vous-même : la commande ci-dessous, dans un terminal."
COMMAND_FIELD = "Commande à copier"
# Each maintenance done by hand: its group, the command shown, the program and the folder it would prune.
BY_HAND = {
    "Paquets uv inutilisés": ("uv cache prune", "uv", f"~/.cache/uv/{ENTRY}/entry"),
    "Paquets pnpm inutilisés": ("pnpm store prune", "pnpm", f"~/.local/share/pnpm/store/v10/{ENTRY}/entry"),
}
SENTINELS = "/usr/local/bin"
RAN = "/var/tmp/okapi-vm-ran-{}"
PASTE = "WAYLAND_DISPLAY=wayland-0 wl-paste --no-newline"
ENABLED = "enabled"
FINISH = "Terminer"


def files(bench: Bench, *paths: str) -> common.Files:
    return common.Files(bench.accounts.admin.name, list(paths), size=SIZE)


def planted_files() -> dict[str, list[str]]:
    return {label: [f"{folder}/{ENTRY}" for folder in folders] for label, (_, folders) in GROUPS.items()}


def by_label(document: dict) -> dict[str, dict]:
    return {group["label"]: group for group in document["groups"]}


def offers(bench: Bench, group: dict | None, *paths: str) -> bool:
    """Whether `group` offers each of `paths` (the file, or a folder above it)."""
    entries = [item["path"] for item in group["entries"]] if group else []
    return all(any(common.expand_home(bench, path).startswith(entry) for entry in entries) for path in paths)


def offered(bench: Bench) -> dict[str, dict]:
    return by_label(common.cli_json(bench, APPS_OPTION))


def install_sentinels(bench: Bench) -> None:
    """A `uv` and a `pnpm` first in the PATH that leave a mark if anything runs them."""
    for _, program, _ in BY_HAND.values():
        path = f"{SENTINELS}/{program}"
        script = f"#!/bin/sh\ntouch {RAN.format(program)}\n"
        bench.root(f"printf %s {shlex.quote(script)} > {path} && chmod 755 {path}")


def prepare(bench: Bench) -> None:
    bench.root(shlex.join(["systemctl", "stop", TMPFILES_TIMER]))
    common.write_settings(bench, {"deletionLog": True})
    install_sentinels(bench)
    by_hand = [path for _, _, path in BY_HAND.values()]
    common.plant(bench, files(bench, *(path for paths in planted_files().values() for path in paths),
                              GEM_SPECIFICATION, CURSOR_FILE, FIGMA_FILE, XET_FILE, *by_hand))


def minimal_ages(bench: Bench) -> dict[str, dict]:
    """RGL-016 and RGL-019 along the clock; the CLI's last analysis, at 8 days."""
    bench.advance_clock(SETTLED_SECONDS)
    settled = offered(bench)
    bench.check(not offers(bench, settled.get(CURSOR), CURSOR_FILE) and not offers(bench, settled.get(FIGMA), FIGMA_FILE),
                f"RGL-016: at 2 hours, neither {CURSOR} (1 day) nor {FIGMA} (7 days) is offered")
    bench.advance_clock(CURSOR_SECONDS - SETTLED_SECONDS)
    days = offered(bench)
    bench.check(offers(bench, days.get(CURSOR), CURSOR_FILE) and not offers(bench, days.get(FIGMA), FIGMA_FILE)
                and not offers(bench, days.get(XET), XET_FILE),
                f"RGL-016: at 2 days, {CURSOR} is offered, {FIGMA} and {XET} (7 days) not yet")
    bench.advance_clock(XET_SECONDS - CURSOR_SECONDS)
    week = offered(bench)
    xet = week.get(XET)
    bench.check(offers(bench, week.get(FIGMA), FIGMA_FILE), f"RGL-016: at 8 days, {FIGMA} is offered")
    bench.check(offers(bench, xet, XET_FILE) and xet["cache_reset"] and not xet["preselected"],
                f"RGL-019: at 8 days, {XET} is offered, resetting a cache, not preselected ({xet})")
    return week


def cli_groups(bench: Bench, groups: dict[str, dict]) -> None:
    planted = planted_files()
    electron = [label for label in ELECTRON_GROUPS if not offers(bench, groups.get(label), *planted[label])]
    bench.check(not electron, f"RGL-015: Discord, VS Code (its logs, CachedData and VSIX packages too) and Slack are "
                              f"a group each, every folder of their rule offered ({electron} wrong)")
    tools = [label for label in GROUPS if label not in ELECTRON_GROUPS
             and not offers(bench, groups.get(label), *planted[label])]
    bench.check(not tools, f"RGL-017: npm, pip, Cargo, Gradle, Maven, the gems and Conda are a group each ({tools} wrong)")
    specification = common.expand_home(bench, GEM_SPECIFICATION)
    beside = [label for label, group in groups.items() if any(specification.startswith(item["path"])
                                                              for item in group["entries"])]
    bench.check(not beside, f"RGL-017: a gem's specification, beside its `cache`, is never offered ({beside})")


def by_hand(bench: Bench, shown: dict[str, dict]) -> None:
    boxes = {group: shown.get(group, {}).get("states") for group in BY_HAND}
    bench.check(all(states is not None and ENABLED not in states and cleaner.CHECKED not in states
                    for states in boxes.values()),
                f"INV-506: the box of a maintenance done by hand is never checkable nor checked ({boxes})")
    notes = {group: shown[group].get("description") for group in BY_HAND}
    bench.check(all(note == BY_HAND_NOTE for note in notes.values()),
                f"RGL-018: each box of a maintenance done by hand says « {BY_HAND_NOTE} » ({notes})")
    fields, copied = {}, {}
    for group in BY_HAND:
        cleaner.row(bench, group)
        fields |= {node.get("description", ""): node.get("text") for node in bench.ui.find(COMMAND_FIELD)}
        cleaner.press_for(bench, cleaner.COPY, group)
        copied[group] = bench.user(PASTE, check=False)
    bench.check(fields == {group: command for group, (command, _, _) in BY_HAND.items()},
                f"RGL-018: « {COMMAND_FIELD} » holds `uv cache prune` and `pnpm store prune` ({fields})")
    bench.check(copied == {group: command for group, (command, _, _) in BY_HAND.items()},
                f"RGL-018: « {cleaner.COPY} » puts each command in the clipboard ({copied})")


def run(bench: Bench) -> None:
    prepare(bench)
    week = minimal_ages(bench)
    cli_groups(bench, week)

    bench.launch_app()
    cleaner.open_page(bench)
    cleaner.choose(bench, APPLICATIONS)
    bench.check(cleaner.analyze(bench) == cleaner.RESULTS, f"the analysis of « {APPLICATIONS} » shows results")
    shown = cleaner.rows(bench)
    bench.screenshot("applications")
    absent = [french for french, _ in GROUPS.values() if french not in shown]
    bench.check(not absent, f"RGL-015, RGL-017: each group is in the results ({absent} missing)")
    bench.check(XET_FRENCH in shown and cleaner.CHECKED not in shown[XET_FRENCH]["states"],
                f"RGL-019: « {XET_FRENCH} » is unchecked")
    by_hand(bench, shown)
    bench.screenshot("a-faire-soi-meme")

    cleaner.clean(bench)
    bench.screenshot("recu")
    gone = [label for label, paths in planted_files().items() if week[label]["preselected"]]
    common.check_gone(bench, files(bench, *(path for label in gone for path in planted_files()[label])))
    common.check_kept(bench, files(bench, GEM_SPECIFICATION, XET_FILE, *(path for _, _, path in BY_HAND.values())))
    marks = [RAN.format(program) for _, program, _ in BY_HAND.values()]
    ran = bench.user(shlex.join(["ls", "--", *marks]), check=False).split()
    bench.check(not ran, f"RGL-018: neither uv nor pnpm ran, the cleaning included, their folders kept ({ran})")
    bench.ui.click(FINISH, role=cleaner.BUTTON)
