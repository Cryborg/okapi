"""Bench: what the scenarios share to drive the interface, each tried once.

A node's states and place, a click at its pixels, text typed in a field,
GNOME's password dialog cancelled, a key that closes the window and the exit
code it leaves, the interface started in English, then one that cannot start.
"""

from __future__ import annotations

from okapivm import common
from okapivm.bench import Bench

TAGS = ["bench"]

SCAN_TIMEOUT_SECONDS = 300
SEARCHED = "Mpv 2.0, ok?"
FIELD = "entry"
ENGLISH = {"LANG": "en_US.UTF-8", "LANGUAGE": "en"}
NO_DISPLAY = {"WAYLAND_DISPLAY": "okapi-vm-none", "DISPLAY": ":99"}


def click_at_pixels(bench: Bench) -> None:
    shown = bench.ui.node("Accueil", role="button")
    bench.check(common.SELECTED in shown["states"] and bool(shown.get("description")),
                f"a node gives its states and its description ({shown.get('description')!r})")
    bench.click_at(*bench.ui.center(bench.ui.node("Nettoyeur", role="button")))
    bench.ui.wait("Nettoyeur", role="button", state=common.SELECTED)
    bench.log("ok: a click at the node's pixels lands on it")


def typing(bench: Bench) -> None:
    common.open_screen(bench, "Applications")
    bench.ui.set_text(role=FIELD, text=SEARCHED)
    typed = bench.ui.node(role=FIELD)
    bench.check(typed.get("text") == SEARCHED, f"the field holds what was typed ({typed.get('text')!r})")


def cancelled_password(bench: Bench) -> None:
    bench.set_elevation(common.Elevation.PASSWORD)
    common.open_screen(bench, "Nettoyeur")
    bench.ui.click("Analyser", role="button")
    bench.ui.wait("Résultats", timeout=SCAN_TIMEOUT_SECONDS)
    bench.ui.click("Obtenir les droits", role="button")
    bench.shell.wait("Annuler", role="button")
    bench.screenshot("mot-de-passe")
    # GNOME Shell's buttons offer no action to AT-SPI: clicked at their pixels.
    bench.click_at(*bench.shell.center(bench.shell.node("Annuler", role="button")))
    bench.shell.wait("Annuler", role="button", gone=True)
    bench.ui.wait("Obtenir les droits", role="button", state="enabled")
    bench.check(not bench.ui.find("Droits obtenus"), "cancelled, GNOME's dialog gives no rights")


def closed_by_key(bench: Bench) -> None:
    bench.key("Alt+F4")
    code = bench.app_exit_code()
    bench.check(code == 0, f"closed by Alt+F4, the interface exits 0 ({code})")


def started_in_english(bench: Bench) -> None:
    bench.launch_app(ENGLISH)
    bench.ui.wait("Cleaner", role="button")
    bench.log("ok: started with LANG=en_US, the interface speaks English")
    bench.stop_app()


def failed_start(bench: Bench) -> None:
    bench.launch_app(NO_DISPLAY, window=False)
    code = bench.app_exit_code()
    bench.check(code != 0, f"without a display, the interface fails ({code})")


def run(bench: Bench) -> None:
    bench.launch_app()
    click_at_pixels(bench)
    typing(bench)
    cancelled_password(bench)
    closed_by_key(bench)
    started_in_english(bench)
    failed_start(bench)
