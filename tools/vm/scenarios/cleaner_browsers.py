"""Cleaner: a browser open when its caches are to be cleaned.

A fake Firefox holds its profile: « Ignorer Firefox » keeps its cache and
cleans the rest, « Annuler » and Escape cancel the whole cleaning, ignoring
the only browser checked goes back to the results, « Fermer Firefox et
continuer » ends it and cleans its cache; one that ignores SIGTERM is said
not closed, its cache kept. A Chrome of Flatpak, whose lock cannot be
vouched for, is never signalled.
"""

from __future__ import annotations

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["cleaner", "browser"]

AGING_SECONDS = 2 * 60 * 60
FILE_SIZE = 64 * 1024
FIREFOX = "Firefox"
FIREFOX_PROFILE = "~/.mozilla/firefox/okapi-vm.default"
FIREFOX_FILES = [
    f"{FIREFOX_PROFILE}/prefs.js",
    "~/.cache/mozilla/firefox/okapi-vm.default/cache2/entries/okapi-vm",
]
FIREFOX_CACHE = FIREFOX_FILES[1]
THUMBNAIL = "~/.cache/thumbnails/normal/okapi-vm.png"
CHROME = "Chrome"
FLATPAK_CHROME = "~/.var/app/com.google.Chrome/config/google-chrome"
FLATPAK_CHROME_FILES = [f"{FLATPAK_CHROME}/Default/Preferences", f"{FLATPAK_CHROME}/Default/Cache/Cache_Data/okapi-vm"]
FLATPAK_CHROME_CACHE = FLATPAK_CHROME_FILES[1]
CACHE = "Cache"
DESELECT = "Tout désélectionner"
OPEN = "{} est ouvert"
IGNORE = "Ignorer {}"
CLOSE = "Fermer {} et continuer"
CANCEL = "Annuler"
NOT_CLOSED = "{} ne s'est pas fermé : fermez-le, puis relancez le nettoyage."
UNVERIFIED = "Okapi ne peut pas fermer {} lui-même : fermez-le, puis relancez le nettoyage."
# What crates/app/src/cleaner/browsers.rs traces, and the key it names Firefox by.
BROWSER_IGNORED = "browser ignored"
FIREFOX_KEY = "firefox"
# Okapi waits about 15 s for a browser to close (CLOSE_LOOKS in crates/app/src/cleaner/browsers.rs).
CLOSE_TIMEOUT_SECONDS = 60
NEW_ANALYSIS = "Nouvelle analyse"


def files(bench: Bench, *paths: str) -> common.Files:
    return common.Files(bench.accounts.admin.name, list(paths), size=FILE_SIZE)


def ask(bench: Bench, browser: str) -> None:
    """A fresh analysis, then « Nettoyer » confirmed, up to the question about `browser`."""
    if bench.ui.find(NEW_ANALYSIS, role=cleaner.BUTTON):
        bench.ui.click(NEW_ANALYSIS, role=cleaner.BUTTON)
        bench.ui.wait(pattern=f"^({'|'.join(cleaner.ANALYSIS_ENDS)})$", timeout=cleaner.SCAN_TIMEOUT_SECONDS)
    else:
        cleaner.analyze(bench)
    cleaner.confirm(bench)
    bench.ui.wait(OPEN.format(browser), timeout=cleaner.CLEAN_TIMEOUT_SECONDS)


def received(bench: Bench) -> int:
    history = common.read_store(bench, common.HISTORY)
    return len(history) if isinstance(history, list) else 0


def fake_alive(bench: Bench) -> bool:
    outcome = bench.execute(bench.accounts.admin.name, f"systemctl --user is-active --quiet {common.FAKE_BROWSER_UNIT}")
    return outcome.code == 0


def ignored(bench: Bench) -> None:
    bench.ui.click(IGNORE.format(FIREFOX), role=cleaner.BUTTON)
    bench.ui.wait(cleaner.DONE, timeout=cleaner.CLEAN_TIMEOUT_SECONDS)
    bench.screenshot("firefox-ignore")
    common.check_kept(bench, files(bench, FIREFOX_CACHE))
    common.check_gone(bench, files(bench, THUMBNAIL))
    receipt = common.last_receipt(bench)
    ignored = [event["fields"] for event in common.journal(bench)
               if event.get("fields", {}).get("message") == BROWSER_IGNORED]
    bench.check(receipt["complete"] and receipt["removed"] >= 1 and bool(ignored)
                and ignored[-1].get("browser") == FIREFOX_KEY,
                f"CLN-033: « {IGNORE.format(FIREFOX)} » leaves its cache out of the cleaning, the thumbnail goes "
                f"({ignored}, {receipt})")
    bench.ui.click("Terminer", role=cleaner.BUTTON)


def cancelled(bench: Bench) -> None:
    common.plant(bench, files(bench, THUMBNAIL), age_seconds=AGING_SECONDS)
    before = received(bench)
    ask(bench, FIREFOX)
    bench.ui.click(CANCEL, role=cleaner.BUTTON)
    bench.ui.wait(OPEN.format(FIREFOX), gone=True)
    cleaner.confirm(bench)
    bench.ui.wait(OPEN.format(FIREFOX), timeout=cleaner.CLEAN_TIMEOUT_SECONDS)
    bench.key("Escape")
    bench.ui.wait(OPEN.format(FIREFOX), gone=True)
    common.check_kept(bench, files(bench, THUMBNAIL, FIREFOX_CACHE))
    bench.check(received(bench) == before and bool(bench.ui.find(cleaner.CLEAN, role=cleaner.BUTTON)),
                f"CLN-036: « {CANCEL} » and Escape of the question cancel the whole cleaning: nothing removed, "
                "nothing in the history")


def only_browser(bench: Bench) -> None:
    before = received(bench)
    bench.ui.click(DESELECT, role=cleaner.BUTTON)
    cleaner.show(bench, "Navigateurs", FIREFOX)
    cleaner.set_checked(bench, CACHE, True)
    cleaner.confirm(bench)
    bench.ui.wait(OPEN.format(FIREFOX), timeout=cleaner.CLEAN_TIMEOUT_SECONDS)
    bench.ui.click(IGNORE.format(FIREFOX), role=cleaner.BUTTON)
    bench.ui.wait(OPEN.format(FIREFOX), gone=True)
    bench.ui.wait(cleaner.RESULTS)
    common.check_kept(bench, files(bench, FIREFOX_CACHE))
    bench.check(received(bench) == before and bool(bench.ui.find(cleaner.CLEAN, role=cleaner.BUTTON)),
                f"CLN-037: ignoring the only browser checked goes back to « {cleaner.RESULTS} », nothing removed")


def closed(bench: Bench) -> None:
    ask(bench, FIREFOX)
    bench.ui.click(CLOSE.format(FIREFOX), role=cleaner.BUTTON)
    bench.ui.wait(cleaner.DONE, timeout=CLOSE_TIMEOUT_SECONDS)
    bench.screenshot("firefox-ferme")
    common.check_gone(bench, files(bench, FIREFOX_CACHE))
    bench.check(not fake_alive(bench),
                f"CLN-034: « {CLOSE.format(FIREFOX)} » ends the browser, then its cache goes")
    bench.ui.click("Terminer", role=cleaner.BUTTON)


def stubborn(bench: Bench) -> None:
    common.plant(bench, files(bench, FIREFOX_CACHE), age_seconds=AGING_SECONDS)
    common.open_fake_browser(bench, firefox_profile=FIREFOX_PROFILE, stubborn=True)
    ask(bench, FIREFOX)
    bench.ui.click(CLOSE.format(FIREFOX), role=cleaner.BUTTON)
    bench.ui.wait(NOT_CLOSED.format(FIREFOX), timeout=CLOSE_TIMEOUT_SECONDS)
    bench.screenshot("firefox-pas-ferme")
    alive = fake_alive(bench)
    bench.ui.click(CANCEL, role=cleaner.BUTTON)
    common.check_kept(bench, files(bench, FIREFOX_CACHE))
    bench.check(alive, f"CLN-035: « {NOT_CLOSED.format(FIREFOX)} », the browser still there, its cache kept")
    common.close_fake_browser(bench)


def unverified(bench: Bench) -> None:
    common.plant(bench, files(bench, *FLATPAK_CHROME_FILES), age_seconds=AGING_SECONDS)
    common.open_fake_browser(bench, chromium_profile=FLATPAK_CHROME)
    ask(bench, CHROME)
    if bench.ui.find(CLOSE.format(CHROME), role=cleaner.BUTTON):
        bench.ui.click(CLOSE.format(CHROME), role=cleaner.BUTTON)
    bench.ui.wait(UNVERIFIED.format(CHROME), timeout=CLOSE_TIMEOUT_SECONDS)
    bench.screenshot("chrome-flatpak")
    alive = fake_alive(bench)
    bench.ui.click(CANCEL, role=cleaner.BUTTON)
    common.check_kept(bench, files(bench, FLATPAK_CHROME_CACHE))
    bench.check(alive, f"CLN-038: a Chrome of Flatpak: « {UNVERIFIED.format(CHROME)} », never signalled, "
                       "its cache kept")
    common.close_fake_browser(bench)


def run(bench: Bench) -> None:
    common.plant(bench, files(bench, *FIREFOX_FILES, THUMBNAIL), age_seconds=AGING_SECONDS)
    common.write_settings(bench, {"deletionLog": True})
    common.open_fake_browser(bench, firefox_profile=FIREFOX_PROFILE)
    bench.launch_app()
    cleaner.open_page(bench)
    ask(bench, FIREFOX)
    ignored(bench)
    cancelled(bench)
    only_browser(bench)
    closed(bench)
    stubborn(bench)
    unverified(bench)
