"""Rules: the browsers' caches, by profile and shared, and the locks that say a browser runs.

Chrome with two profiles and every cache of the table, a Brave of snap; a
Firefox native, one of snap and one of Flatpak, their profiles' lock free.
Each cache is its group, labelled « {browser} : {label} » and checked as the
table says (RGL-011, RGL-012). A fake Firefox holds its profile's lock, which
`/proc/locks` shows: the cleaning asks about it; a profile with no lock has
no group (RGL-013). Chrome's `SingletonLock` names a dead process: Chrome is
judged closed, its preselected caches go (RGL-014).
"""

from __future__ import annotations

import shlex
from typing import NamedTuple

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["rules", "browser"]


class Cache(NamedTuple):
    """A cache of the table of `crates/rules/data/linux/browsers.json`."""

    directory: str
    english: str
    french: str
    preselected: bool


AGING_SECONDS = 2 * 60 * 60
SIZE = 4096
ENTRY = "okapi-vm"
BROWSERS = "Navigateurs"
CHROME = "Chrome"
BRAVE = "Brave"
FIREFOX = "Firefox"
CHROME_BASE = "~/.config/google-chrome"
CHROME_PROFILES = ["Default", "Profile 1"]
CHROME_PROFILE_CACHES = [
    Cache("Cache/Cache_Data", "Cache", "Cache", True),
    Cache("Code Cache/js", "Code Cache", "Cache du code", False),
    Cache("GPUCache", "GPU Cache", "Cache GPU", False),
    Cache("Shared Dictionary/cache", "Shared Dictionary Cache", "Cache des dictionnaires partagés", True),
    Cache("Service Worker/CacheStorage", "Service Worker Cache", "Cache des service workers", False),
    Cache("DawnWebGPUCache", "WebGPU Shader Cache", "Cache des shaders WebGPU", False),
    Cache("DawnGraphiteCache", "Graphite Shader Cache", "Cache des shaders Graphite", False),
]
CHROME_SHARED_CACHES = [
    Cache("component_crx_cache", "Component Extension Cache", "Cache des extensions intégrées", True),
    Cache("extensions_crx_cache", "Extension Update Cache", "Cache des mises à jour d'extensions", True),
    Cache("GrShaderCache", "Skia Shader Cache", "Cache des shaders Skia", False),
    Cache("ShaderCache", "Shader Cache", "Cache des shaders", False),
    Cache("GraphiteDawnCache", "Graphite Pipeline Cache", "Cache des pipelines Graphite", False),
    Cache("GPUPersistentCache", "Persistent GPU Cache", "Cache GPU persistant", False),
    Cache("PnaclTranslationCache", "PNaCl Translation Cache", "Cache de traduction PNaCl", False),
]
CHROME_CACHES = [*CHROME_PROFILE_CACHES, *CHROME_SHARED_CACHES]
# The snap's base goes through `current`, a link to its revision.
BRAVE_REVISION = "42"
BRAVE_CURRENT = "~/snap/brave/current"
BRAVE_BASE = f"~/snap/brave/{BRAVE_REVISION}/.config/BraveSoftware/Brave-Browser/Default"
BRAVE_CACHE = "~/snap/brave/common/.cache/BraveSoftware/Brave-Browser/Default"
BRAVE_FILES = {CHROME_PROFILE_CACHES[0]: f"{BRAVE_CACHE}/Cache/Cache_Data/{ENTRY}",
               CHROME_PROFILE_CACHES[2]: f"{BRAVE_BASE}/GPUCache/{ENTRY}"}
# Each packaging of Firefox, native first: its profile, and where its caches are.
FIREFOX_PROFILES = [
    ("~/.mozilla/firefox/okapi-vm.default", "~/.cache/mozilla/firefox/okapi-vm.default"),
    ("~/snap/firefox/common/.mozilla/firefox/okapi-vm.snap",
     "~/snap/firefox/common/.cache/mozilla/firefox/okapi-vm.snap"),
    ("~/.var/app/org.mozilla.firefox/.mozilla/firefox/okapi-vm.flatpak",
     "~/.var/app/org.mozilla.firefox/cache/mozilla/firefox/okapi-vm.flatpak"),
]
NATIVE_PROFILE, NATIVE_CACHES = FIREFOX_PROFILES[0]
FIREFOX_CACHES = [
    Cache("cache2/entries", "Cache", "Cache", True),
    Cache("thumbnails", "Thumbnails", "Vignettes", True),
    Cache("startupCache", "Startup Cache", "Cache de démarrage", False),
]
UNLAUNCHED_PROFILE = "~/.mozilla/firefox/okapi-vm.unlaunched"
UNLAUNCHED_CACHE = f"~/.cache/mozilla/firefox/okapi-vm.unlaunched/cache2/entries/{ENTRY}"
LOCK = ".parentlock"
SINGLETON_LOCK = "SingletonLock"
DEAD_PID = 99999
BROWSERS_OPTION = "--browsers"
OPEN = "{} est ouvert"
IGNORE = "Ignorer {}"
FINISH = "Terminer"
# A line of the CLI's text names a browser's cache by its browser, then its label.
CLI_LABEL = "{} : {}"


def files(bench: Bench, *paths: str) -> common.Files:
    return common.Files(bench.accounts.admin.name, list(paths), size=SIZE)


def chrome_files() -> dict[Cache, list[str]]:
    """The files planted in each Chrome cache, in each profile for a profile's."""
    planted = {cache: [f"{CHROME_BASE}/{profile}/{cache.directory}/{ENTRY}" for profile in CHROME_PROFILES]
               for cache in CHROME_PROFILE_CACHES}
    return planted | {cache: [f"{CHROME_BASE}/{cache.directory}/{ENTRY}"] for cache in CHROME_SHARED_CACHES}


def firefox_files(*profiles: tuple[str, str]) -> dict[Cache, list[str]]:
    return {cache: [f"{caches}/{cache.directory}/{ENTRY}" for _, caches in profiles or FIREFOX_PROFILES]
            for cache in FIREFOX_CACHES}


def flatten(planted: dict[Cache, list[str]], *, preselected: bool | None = None) -> list[str]:
    return [path for cache, paths in planted.items() if preselected in (None, cache.preselected) for path in paths]


def prepare(bench: Bench) -> None:
    common.plant(bench, files(
        bench, *(f"{CHROME_BASE}/{profile}/Preferences" for profile in CHROME_PROFILES), *flatten(chrome_files()),
        f"{BRAVE_BASE}/Preferences", *BRAVE_FILES.values(),
        *(f"{profile}/prefs.js" for profile, _ in FIREFOX_PROFILES), *flatten(firefox_files()),
        f"{UNLAUNCHED_PROFILE}/prefs.js", UNLAUNCHED_CACHE,
    ))
    bench.user(shlex.join(["ln", "-s", "--", BRAVE_REVISION, common.expand_home(bench, BRAVE_CURRENT)]))
    bench.user(shlex.join(["touch", "--", *(common.expand_home(bench, f"{profile}/{LOCK}")
                                            for profile, _ in FIREFOX_PROFILES)]))
    hostname = bench.user("hostname").strip()
    bench.user(shlex.join(["ln", "-s", "--", f"{hostname}-{DEAD_PID}",
                           common.expand_home(bench, f"{CHROME_BASE}/{SINGLETON_LOCK}")]))
    bench.advance_clock(AGING_SECONDS)


def by_browser(document: dict, browser: str) -> dict[str, dict]:
    """The CLI's groups of `browser` (its key) by their English label."""
    return {group["label"]: group for group in document["groups"] if group.get("browser") == browser}


def offered_paths(group: dict) -> set[str]:
    return {item["path"] for item in group["entries"]}


def misjudged(bench: Bench, groups: dict[str, dict], planted: dict[Cache, list[str]]) -> dict[str, object]:
    """The caches whose group does not offer each planted file (or a folder
    above it), or is not preselected as the table says: their preselection."""
    wrong: dict[str, object] = {}
    for cache, paths in planted.items():
        group = groups.get(cache.english)
        offered = offered_paths(group) if group else set()
        held = all(any(common.expand_home(bench, path).startswith(entry) for entry in offered) for path in paths)
        if not held or group["preselected"] != cache.preselected:
            wrong[cache.english] = group["preselected"] if group else None
    return wrong


def cli_tables(bench: Bench) -> None:
    document = common.cli_json(bench, BROWSERS_OPTION)
    chrome = misjudged(bench, by_browser(document, "chrome"), chrome_files())
    brave = misjudged(bench, by_browser(document, "brave"),
                      {cache: [path] for cache, path in BRAVE_FILES.items()})
    bench.check(not chrome and not brave,
                f"RGL-011: each cache of Chrome's two profiles and its shared ones, and Brave's of snap, is its "
                f"group, preselected as the table says (wrong: {chrome}, {brave})")
    text = common.cli(bench, BROWSERS_OPTION).stdout
    labels = [CLI_LABEL.format(CHROME, cache.french) for cache in CHROME_CACHES]
    labels += [CLI_LABEL.format(BRAVE, cache.french) for cache in BRAVE_FILES]
    unlabelled = [label for label in labels if label not in text]
    bench.check(not unlabelled, f"RGL-011: the CLI names each « {{browser}} : {{label}} » ({unlabelled} missing)")

    firefox = by_browser(document, "firefox")
    wrong = misjudged(bench, firefox, firefox_files())
    bench.check(not wrong,
                f"RGL-012: cache2 and thumbnails preselected, startupCache not, for Firefox native, of snap and of "
                f"Flatpak (wrong: {wrong})")
    unlaunched = common.expand_home(bench, UNLAUNCHED_CACHE)
    offered = [path for group in firefox.values() for path in offered_paths(group) if unlaunched.startswith(path)]
    bench.check(not offered, f"RGL-013: a profile without its lock is never offered ({offered})")


def shown_states(bench: Bench, browser: str) -> dict[str, bool]:
    """Whether each cache of `browser` is checked, its browser alone unfolded
    (the others folded beforehand: the list shows only the rows on screen)."""
    cleaner.fold(bench, browser, folded=False)
    states = {name: cleaner.CHECKED in row["states"] for name, row in cleaner.rows(bench).items()
              if name not in [BROWSERS, CHROME, BRAVE, FIREFOX]}
    # The list was scrolled to its end: the browser's line is back on screen first.
    bench.ui.reveal(f"{cleaner.FOLD} {browser}", role=cleaner.BUTTON, within=cleaner.RESULTS_LIST)
    cleaner.fold(bench, browser)
    return states


def interface_tables(bench: Bench) -> None:
    expected = {CHROME: {cache.french: cache.preselected for cache in CHROME_CACHES},
                BRAVE: {cache.french: cache.preselected for cache in BRAVE_FILES},
                FIREFOX: {cache.french: cache.preselected for cache in FIREFOX_CACHES}}
    for browser in expected:
        cleaner.fold(bench, browser)
    shown = {browser: shown_states(bench, browser) for browser in expected}
    bench.screenshot("navigateurs")
    bench.check(shown[CHROME] == expected[CHROME] and shown[BRAVE] == expected[BRAVE],
                f"RGL-011: under « {CHROME} » and « {BRAVE} », each cache checked as the table says ({shown})")
    bench.check(shown[FIREFOX] == expected[FIREFOX],
                f"RGL-012: under « {FIREFOX} », « Cache » and « Vignettes » checked, « Cache de démarrage » not "
                f"({shown[FIREFOX]})")


def locked(bench: Bench, profile: str) -> bool:
    """Whether `/proc/locks` holds the lock of `profile`, found by its inode."""
    inode = bench.user(shlex.join(["stat", "--format=%i", "--", common.expand_home(bench, f"{profile}/{LOCK}")]))
    # A line of /proc/locks: `1: POSIX ADVISORY WRITE <pid> <major>:<minor>:<inode> <start> <end>`.
    return any(len(fields) > 5 and fields[5].endswith(f":{inode.strip()}")
               for fields in (line.split() for line in bench.user("cat /proc/locks").splitlines()))


def run(bench: Bench) -> None:
    prepare(bench)
    common.write_settings(bench, {"deletionLog": True})
    cli_tables(bench)
    common.open_fake_browser(bench, firefox_profile=NATIVE_PROFILE)
    bench.check(locked(bench, NATIVE_PROFILE), "the fake Firefox's lock is in /proc/locks, by its inode")
    bench.check(bench.execute(bench.accounts.admin.name, f"kill -0 {DEAD_PID}").code != 0,
                f"no process {DEAD_PID}, which Chrome's `{SINGLETON_LOCK}` names")
    bench.launch_app()
    cleaner.open_page(bench)
    cleaner.choose(bench, BROWSERS)
    bench.check(cleaner.analyze(bench) == cleaner.RESULTS, f"the analysis of « {BROWSERS} » shows results")
    interface_tables(bench)

    cleaner.confirm(bench)
    bench.ui.wait(OPEN.format(FIREFOX), timeout=cleaner.CLEAN_TIMEOUT_SECONDS)
    bench.screenshot("firefox-ouvert")
    asked_chrome = bench.ui.find(OPEN.format(CHROME))
    bench.log(f"ok: RGL-013: the cleaning asks « {OPEN.format(FIREFOX)} »")
    bench.ui.click(IGNORE.format(FIREFOX), role=cleaner.BUTTON)
    bench.ui.wait(cleaner.DONE, timeout=cleaner.CLEAN_TIMEOUT_SECONDS)
    bench.ui.wait(cleaner.RECORDED)
    bench.screenshot("recu")
    common.close_fake_browser(bench)

    planted = chrome_files()
    common.check_gone(bench, files(bench, *flatten(planted, preselected=True)))
    common.check_kept(bench, files(bench, *flatten(planted, preselected=False)))
    bench.check(not asked_chrome,
                f"RGL-014: Chrome, whose `{SINGLETON_LOCK}` names a dead process, is never asked about: its "
                f"preselected caches are gone, the others kept")
    common.check_kept(bench, files(bench, *flatten(firefox_files((NATIVE_PROFILE, NATIVE_CACHES))), UNLAUNCHED_CACHE))
    bench.log(f"ok: RGL-013: « {IGNORE.format(FIREFOX)} » keeps the open profile's caches")
    bench.ui.click(FINISH, role=cleaner.BUTTON)
