"""Driving the Applications page: its list read, filtered, searched and
sorted, a row's detail, an uninstallation asked, confirmed and its end. The
names are the interface's French texts."""

from __future__ import annotations

import re
import time

from . import common
from .bench import Bench, ScenarioFailure

PAGE = "Applications"
BUTTON = "button"
LABEL = "label"
ROW = "list-item"
FILTER = "toggle-button"
LIST = "Applications installées"
REFRESH = "Actualiser"
READ = re.compile(r"^Applis : (\d+), lues (à l'instant|il y a (\d+) min)$")
READ_PATTERN = r"^Applis : \d+, lues "
ALL = "Toutes"
UBUNTU = "Ubuntu"
SNAP = "Snap"
FLATPAK = "Flatpak"
CLOSE_DETAIL = "Fermer le détail"
UNINSTALL = "Désinstaller"
CLOSE_AND_UNINSTALL = "Fermer et désinstaller"
FORCE = "Forcer la fermeture"
CANCEL = "Annuler"
CLOSE = "Fermer"
RETRY = "Réessayer"
SYSTEM = "Système et composants"
WHAT_IS_IT = "C'est quoi ?"
CLEAR_SEARCH = "Effacer la recherche"
SORTED_UP = ", trié par ordre croissant"
SORTED_DOWN = ", trié par ordre décroissant"
# Each the title of the uninstallation's dialog at one of its steps.
CONFIRMING = "Désinstaller {} ?"
REFUSED = "{} ne peut pas être désinstallée"
REMOVING = "Désinstallation de {}…"
CLOSING = "Fermeture de {}…"
NOT_CLOSED = "{} ne s'est pas fermée"
ALREADY_GONE = "{} est déjà désinstallée"
FAILED = "{} n'a pas été désinstallée"
DONE = "{} désinstallée"
# What crates/app/src/apps.rs traces once a reading ends.
READ_TO_END = "applications read to their end"
UNINSTALL_KIND = "uninstall"
READ_TIMEOUT_SECONDS = 180
UNINSTALL_TIMEOUT_SECONDS = 180
POLL_SECONDS = 0.5


def open_page(bench: Bench) -> None:
    """The page shown and its first reading ended."""
    common.open_screen(bench, PAGE)
    bench.ui.wait(pattern=READ_PATTERN, role=LABEL, timeout=READ_TIMEOUT_SECONDS)
    wait_read(bench, 1)


def readings(bench: Bench) -> int:
    return sum(1 for event in common.journal(bench) if event.get("fields", {}).get("message") == READ_TO_END)


def wait_read(bench: Bench, count: int) -> None:
    """Waits for the interface's journal to tell `count` readings ended."""
    deadline = time.monotonic() + READ_TIMEOUT_SECONDS
    while readings(bench) < count:
        if time.monotonic() > deadline:
            raise ScenarioFailure(f"the applications were not read {count} times in {READ_TIMEOUT_SECONDS} s")
        time.sleep(POLL_SECONDS)


def refresh(bench: Bench) -> None:
    """« Actualiser », until the new reading ends."""
    done = readings(bench)
    bench.ui.click(REFRESH, role=BUTTON)
    wait_read(bench, done + 1)


def header(bench: Bench) -> re.Match[str]:
    """« Applis : {}, lues … » read: its count is group 1."""
    text = bench.ui.node(pattern=READ_PATTERN, role=LABEL)["name"]
    if not (match := READ.match(text)):
        raise ScenarioFailure(f"the header reads {text!r}")
    return match


def rows(bench: Bench) -> list[str]:
    """The names of the rows drawn, in their order (a row: « name, source, size… »)."""
    return [node["name"] for node in bench.ui.find(role=ROW)]


def row_pattern(name: str, source: str | None = None) -> str:
    return f"^{re.escape(name)}, {re.escape(source) if source else '[^,]+'}, "


def show_only(bench: Bench, name: str, source: str = ALL) -> None:
    """Only the rows of `source` whose name or package holds `name`: the list
    draws only its rows in view."""
    choose_source(bench, source)
    search(bench, name)


def choose_source(bench: Bench, source: str) -> None:
    if "pressed" not in bench.ui.node(source, role=FILTER)["states"]:
        bench.ui.click(source, role=FILTER)


def search(bench: Bench, text: str) -> None:
    bench.ui.set_text(role="entry", text=text)


def open_detail(bench: Bench, name: str, source: str = ALL, query: str | None = None) -> None:
    """The detail of the row named `name`, searched for first by `query` (its
    name by default; its package's when the keyboard cannot type its name)."""
    show_only(bench, query or name, source)
    bench.ui.click(pattern=row_pattern(name), role=ROW)
    bench.ui.wait(CLOSE_DETAIL, role=BUTTON)


def labels(bench: Bench) -> list[str]:
    return [node["name"] for node in bench.ui.find(role=LABEL)]


def ask_uninstall(bench: Bench, name: str, source: str = ALL, *, query: str | None = None,
                  step: str = CONFIRMING) -> None:
    """Its detail opened, « Désinstaller », until the dialog's title is `step`."""
    open_detail(bench, name, source, query)
    bench.ui.click(UNINSTALL, role=BUTTON)
    wait_step(bench, name, step)


def wait_step(bench: Bench, name: str, step: str, timeout: float = UNINSTALL_TIMEOUT_SECONDS) -> None:
    bench.ui.wait(step.format(name), timeout=timeout)
    bench.log(f"the dialog reads {step.format(name)!r}")


def confirm(bench: Bench, button: str = UNINSTALL) -> None:
    """The dialog's own button: the last of that name, the detail's being behind it."""
    bench.ui.click(button, role=BUTTON, index=len(bench.ui.find(button, role=BUTTON)) - 1)


def uninstall(bench: Bench, name: str, source: str = ALL, *, query: str | None = None,
              button: str = UNINSTALL) -> None:
    """Asked, confirmed, until the notification says it is done."""
    ask_uninstall(bench, name, source, query=query)
    confirm(bench, button)
    wait_done(bench, name)


def wait_done(bench: Bench, name: str) -> str:
    """The notification of the end, read."""
    return bench.ui.wait(pattern=f"^{re.escape(DONE.format(name))}", timeout=UNINSTALL_TIMEOUT_SECONDS)[0]["name"]


def last_uninstall(bench: Bench) -> dict | None:
    """The newest `uninstall` line of the history."""
    history = common.read_store(bench, common.HISTORY) or []
    assert isinstance(history, list)
    lines = [line for line in history if line.get("kind") == UNINSTALL_KIND]
    return lines[-1] if lines else None


def traced(bench: Bench, message: str) -> list[dict]:
    """The fields of each event of the interface's journal with `message`."""
    return [event["fields"] for event in common.journal(bench) if event.get("fields", {}).get("message") == message]
