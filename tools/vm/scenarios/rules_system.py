"""Rules: the system's targets of the account, from the temporary folders to the caches of sandboxed applications.

An old file under each rule of « Système » the account cleans: `TMPDIR`,
`/tmp` and `/var/tmp` at 30 days, the thumbnails, both Tracker caches,
GStreamer's and `.xsession-errors.old`; 31 days later each group is offered,
checked, and emptied (RGL-001), a file of 29 days left out (RGL-002). The
font caches are offered unchecked, left to choose (RGL-005); a snap's and a
Flatpak's caches are an entry each, below their application (RGL-006).
"""

from __future__ import annotations

import shlex

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["rules", "system"]

DAY_SECONDS = 24 * 60 * 60
# Beyond the 30 days of the temporary folders' rules.
AGING_DAYS = 31
YOUNG_DAYS = 29
SIZE = 4096
TMPFILES_TIMER = "systemd-tmpfiles-clean.timer"
# A `TMPDIR` of its own, away from `/tmp`, whose rule shares its root otherwise.
TMPDIR = "~/.okapi-vm-tmp"
SYSTEM_JSON = "--system"
SYSTEM = "Système"
DESELECT = "Tout désélectionner"
PRESELECTED = "Présélectionné"
ADMINISTRATOR = "Administrateur"
# Each group of the account and the files planted where its rule looks.
PLANTED = {
    "Fichiers temporaires de l'utilisateur": [f"{TMPDIR}/okapi-vm.txt"],
    "Fichiers temporaires du système": ["/tmp/okapi-vm.txt"],
    "Fichiers temporaires persistants": ["/var/tmp/okapi-vm.txt"],
    "Cache des miniatures": ["~/.cache/thumbnails/normal/okapi-vm.png"],
    "Cache de GNOME Tracker": ["~/.cache/tracker3/okapi-vm.db"],
    "Cache de GNOME Tracker (ancien)": ["~/.cache/tracker/okapi-vm.db"],
    "Cache de GStreamer": ["~/.cache/gstreamer-1.0/okapi-vm.bin"],
    "Erreurs de la session X": ["~/.xsession-errors.old"],
}
YOUNG = "/var/tmp/okapi-vm-young.txt"
FONTS = {"Cache des polices": "Font Cache", "Cache des polices du système": "System Font Cache"}
FONT_FILE = "~/.cache/fontconfig/okapi-vm.cache-9"
SANDBOXED = {
    "Cache des applications Snap": "~/snap/okapi-vm/common/.cache",
    "Cache des applications Flatpak": "~/.var/app/org.okapivm.Fake/cache",
}
SANDBOXED_FILE = "okapi-vm/entry"


def files(bench: Bench, *paths: str) -> common.Files:
    return common.Files(bench.accounts.admin.name, list(paths), size=SIZE)


def by_label(document: dict) -> dict[str, dict]:
    """The groups of the CLI's analysis by their English label."""
    return {group["label"]: group for group in document["groups"]}


def entries_of(document: dict) -> set[str]:
    return {item["path"] for group in document["groups"] for item in group["entries"]}


def uncovered(paths: set[str], entries: set[str]) -> list[str]:
    """The paths that are neither one of `entries` nor below one (a thumbnail goes with its size's folder)."""
    return sorted(path for path in paths if not any(path == entry or path.startswith(f"{entry}/")
                                                    for entry in entries))


def prepare(bench: Bench) -> list[str]:
    bench.root(shlex.join(["systemctl", "stop", TMPFILES_TIMER]))
    common.write_settings(bench, {"deletionLog": True})
    planted = [path for paths in PLANTED.values() for path in paths]
    sandboxed = [f"{root}/{SANDBOXED_FILE}" for root in SANDBOXED.values()]
    common.plant(bench, files(bench, *planted, FONT_FILE, *sandboxed))
    bench.advance_clock((AGING_DAYS - YOUNG_DAYS) * DAY_SECONDS)
    common.plant(bench, files(bench, YOUNG), age_seconds=YOUNG_DAYS * DAY_SECONDS)
    return planted


def font_caches(bench: Bench, document: dict, shown: dict[str, dict]) -> None:
    groups = by_label(document)
    judged = {label: (groups[english]["preselected"], groups[english]["cache_reset"])
              for label, english in FONTS.items() if english in groups}
    said = {label: shown[label].get("description", "") for label in FONTS if label in shown}
    bench.check(len(judged) == len(FONTS) and all(not preselected and reset for preselected, reset in judged.values())
                and len(said) == len(FONTS)
                and all(cleaner.CHECKED not in shown[label]["states"] and PRESELECTED not in said[label]
                        for label in FONTS),
                f"RGL-005: the account's and the system's font caches reset a cache, offered unchecked and not "
                f"preselected ({judged}, {said})")


def sandboxed_caches(bench: Bench, document: dict, shown: dict[str, dict]) -> None:
    groups = by_label(document)
    found = {english: sorted(item["path"] for item in groups.get(english, {}).get("entries", []))
             for english in ["Snap App Cache", "Flatpak App Cache"]}
    expected = {english: [common.expand_home(bench, f"{root}/okapi-vm")]
                for english, root in zip(found, SANDBOXED.values(), strict=True)}
    bench.check(found == expected and all(label in shown for label in SANDBOXED),
                f"RGL-006: the caches of a snap and of a Flatpak are an entry each, below their application "
                f"({found}, expected {expected})")


def run(bench: Bench) -> None:
    planted = prepare(bench)
    tmpdir = common.expand_home(bench, TMPDIR)
    document = common.cli_json(bench, SYSTEM_JSON, environment={"TMPDIR": tmpdir})
    offered = entries_of(document)
    expected = {common.expand_home(bench, path) for path in planted}
    missing = uncovered(expected, offered)
    bench.check(not missing, f"RGL-001: the CLI's analysis offers every file planted ({missing} missing)")
    bench.check(uncovered({YOUNG}, offered) == [YOUNG], f"RGL-002: a temporary file of {YOUNG_DAYS} days is not offered")

    bench.launch_app({"TMPDIR": tmpdir})
    cleaner.open_page(bench)
    cleaner.choose(bench, SYSTEM)
    bench.check(cleaner.analyze(bench) == cleaner.RESULTS, "the analysis of « Système » shows results")
    shown = cleaner.rows(bench)
    bench.screenshot("resultats")
    font_caches(bench, document, shown)
    sandboxed_caches(bench, document, shown)
    absent = [group for group in PLANTED if group not in shown]
    bench.check(not absent, f"RGL-001: each rule planted has its group ({absent} missing)")
    preselected = {group: cleaner.CHECKED in shown[group]["states"] for group in PLANTED if group in shown}
    bench.log(f"preselected among the planted groups: {preselected}")

    bench.ui.click(DESELECT, role=cleaner.BUTTON)
    cleaner.check_groups(bench, *PLANTED)
    cleaner.clean(bench)
    bench.screenshot("recu")
    common.check_gone(bench, files(bench, *planted))
    common.check_kept(bench, files(bench, YOUNG, FONT_FILE, *(f"{root}/{SANDBOXED_FILE}" for root in SANDBOXED.values())))
    removed = {item["path"] for item in common.last_deletions(bench) if item["result"] == "removed"}
    left = uncovered(expected, removed)
    bench.check(not left, f"RGL-001: each group checked is emptied, as the deletion log says ({left} not removed)")
    bench.check(uncovered({YOUNG}, removed) == [YOUNG], f"RGL-002: the temporary file of {YOUNG_DAYS} days stays")
