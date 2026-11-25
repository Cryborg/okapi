"""Cleaner: what stops an analysis or a cleaning before anything is touched.

Settings unreadable, a folder to clean relative, an exclusion refused, a
minimum age before the clock: a banner, and no analysis. The cache folder
unreadable: « Rien n'a été regardé » and its reason. The history a folder: with
the deletion log asked for, nothing is removed; without it, the cleaning is
done and its receipt says the history is unavailable. A folder to clean
removed from the settings while its results are shown: kept, as no longer
chosen.
"""

from __future__ import annotations

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["cleaner"]

AGING_SECONDS = 2 * 60 * 60
FILE_SIZE = 64 * 1024
UNREADABLE_SETTINGS = "{"
RELATIVE = "okapi-vm-relative"
# Far enough back for the minimum age to start before the clock's origin.
MIN_AGE_BEFORE_CLOCK = 200_000_000_000_000_000
BANNERS = {
    "settings": "Les réglages sont illisibles : rien ne peut démarrer. Le détail est dans le journal.",
    "customFolders": ("Un dossier à nettoyer des réglages n'est pas un chemin absolu sans .. : rien ne peut démarrer. "
                      "Corrigez-le ou retirez-le dans Réglages."),
    "exclusions": "Une exclusion des réglages est refusée : rien n'a démarré. Le détail est dans le journal.",
    "minAgeMinutes": "L'âge minimal des réglages remonte avant l'horloge : rien ne serait assez ancien.",
}
SETTINGS_CASES = {
    "customFolders": {"customFolders": [RELATIVE]},
    "exclusions": {"exclusions": [RELATIVE]},
    "minAgeMinutes": {"minAgeMinutes": MIN_AGE_BEFORE_CLOCK},
}
THUMBNAIL = "~/.cache/thumbnails/normal/okapi-vm.png"
LOCKED = "~/okapi-vm-locked"
LOCKED_CACHE = f"{LOCKED}/cache"
NOTHING_LOOKED = "Rien n'a été regardé"
STOPPED_BEFORE = "L'analyse s'est arrêtée avant de commencer : aucun fichier n'a été lu ni modifié."
UNREADABLE_VARIABLE = "Le dossier de la variable CACHE est illisible : ce qu'il protège ne peut pas être garanti."
OTHER_CATEGORIES = "Choisir d'autres catégories"
LAST_ANALYSIS = "last-analysis.json"
HISTORY_KEPT = ("L'historique ne peut pas être tenu alors que vous demandez le journal des "
                "suppressions : rien n'est supprimé. Le détail est dans le journal.")
HISTORY_UNAVAILABLE = "L'historique est indisponible : ce reçu n'y est pas ajouté. Le détail est dans le journal."
NOT_RECORDED = "Non fait"
RECORDING = "Enregistrement dans l'historique"
FOLDER = "~/okapi-vm-folder"
FOLDER_FILE = f"{FOLDER}/okapi-vm-old.txt"
NO_LONGER_CHOSEN_LINE = "Déplier Gardées : dossier retiré des réglages depuis l'analyse"
NO_LONGER_CHOSEN = "no_longer_chosen"
HOME = "Accueil"


def analyses(bench: Bench) -> int:
    return sum(event.get("fields", {}).get("message") == cleaner.SCAN_FINISHED for event in common.journal(bench))


def relaunch(bench: Bench, environment: dict[str, str] | None = None) -> None:
    bench.launch_app(environment)
    cleaner.open_page(bench)


def banners(bench: Bench) -> None:
    settings = common.shell_path(common.store_path(common.SETTINGS))
    shown = {}
    before = analyses(bench)
    bench.user(f"mkdir -p -- \"$(dirname -- {settings})\" && printf %s '{UNREADABLE_SETTINGS}' > {settings}")
    relaunch(bench)
    bench.ui.click(cleaner.ANALYZE, role=cleaner.BUTTON)
    shown["settings"] = bool(bench.ui.wait(BANNERS["settings"]))
    content = bench.user(f"cat -- {settings}")
    bench.screenshot("reglages-illisibles")
    for case, values in SETTINGS_CASES.items():
        common.write_settings(bench, values)
        relaunch(bench)
        bench.ui.click(cleaner.ANALYZE, role=cleaner.BUTTON)
        shown[case] = bool(bench.ui.wait(BANNERS[case]))
        bench.screenshot(f"bandeau-{case}")
    started = analyses(bench) - before
    bench.check(all(shown.values()) and started == 0 and content == UNREADABLE_SETTINGS
                and not bench.ui.find(pattern=f"^({'|'.join(cleaner.ANALYSIS_ENDS)})$"),
                f"CLN-010: a banner for each setting refused ({shown}), « {cleaner.ANALYZE} » starts nothing "
                f"({started} analyses), the unreadable settings left as they were")
    common.write_settings(bench, {})


def unreadable_cache(bench: Bench) -> None:
    locked = common.shell_path(LOCKED)
    bench.user(f"mkdir -p -- {common.shell_path(LOCKED_CACHE)} && chmod 000 -- {locked}")
    relaunch(bench, {"XDG_CACHE_HOME": common.expand_home(bench, LOCKED_CACHE)})
    title = cleaner.analyze(bench)
    bench.ui.wait(STOPPED_BEFORE)
    bench.ui.wait(UNREADABLE_VARIABLE)
    bench.screenshot("rien-n-a-ete-regarde")
    bench.check(title == NOTHING_LOOKED and common.read_store(bench, LAST_ANALYSIS) is None,
                f"CLN-009: « {NOTHING_LOOKED} », « {UNREADABLE_VARIABLE} », no summary written")
    bench.ui.click(OTHER_CATEGORIES, role=cleaner.BUTTON)
    bench.user(f"chmod 755 -- {locked}")


def history_a_folder(bench: Bench) -> None:
    thumbnail = common.Files(bench.accounts.admin.name, [THUMBNAIL], size=FILE_SIZE)
    common.plant(bench, thumbnail, age_seconds=AGING_SECONDS)
    history = common.shell_path(common.store_path(common.HISTORY))
    bench.user(f"mkdir -p -- {history}")
    common.write_settings(bench, {"deletionLog": True})
    relaunch(bench)
    cleaner.analyze(bench)
    cleaner.confirm(bench)
    bench.ui.wait(HISTORY_KEPT, timeout=cleaner.CLEAN_TIMEOUT_SECONDS)
    bench.screenshot("historique-exige")
    common.check_kept(bench, thumbnail)
    bench.check(bool(bench.ui.find(cleaner.CLEAN, role=cleaner.BUTTON)),
                f"CLN-030: the deletion log asked for, the history a folder: « {HISTORY_KEPT} », back to the "
                "results, the thumbnail still there")

    common.write_settings(bench, {"deletionLog": False})
    relaunch(bench)
    cleaner.analyze(bench)
    cleaner.clean(bench)
    bench.ui.wait(HISTORY_UNAVAILABLE)
    step = bench.ui.node(RECORDING, role="list-item").get("description", "")
    bench.screenshot("historique-indisponible")
    common.check_gone(bench, thumbnail)
    folder = bench.user(f"if [ -d {history} ]; then echo folder; fi").strip()
    bench.check(step.startswith(NOT_RECORDED) and folder == "folder",
                f"CLN-031: without the deletion log, the cleaning done: « {HISTORY_UNAVAILABLE} », "
                f"« {RECORDING} » {step!r}, the history folder left as it was")
    bench.user(f"rmdir -- {history}")


def folder_no_longer_chosen(bench: Bench) -> None:
    common.plant(bench, common.Files(bench.accounts.admin.name, [FOLDER_FILE], size=FILE_SIZE),
                 age_seconds=AGING_SECONDS)
    common.write_settings(bench, {"deletionLog": True, "customFolders": [common.expand_home(bench, FOLDER)]})
    relaunch(bench)
    cleaner.choose(bench, "Mes dossiers")
    cleaner.analyze(bench)
    for node in bench.ui.find(role=cleaner.CHECK_BOX):
        if node["name"] not in cleaner.CATEGORIES:
            cleaner.set_checked(bench, node["name"], True)
    common.write_settings(bench, {"deletionLog": True})
    common.open_screen(bench, HOME)
    common.open_screen(bench, cleaner.PAGE)
    cleaner.clean(bench)
    bench.screenshot("dossier-retire")
    common.check_kept(bench, common.Files(bench.accounts.admin.name, [FOLDER_FILE]))
    history = common.read_store(bench, common.HISTORY)
    assert isinstance(history, list)
    reasons = [entry.get("reason") for line in history if line.get("kind") == "deletions" for entry in line["entries"]]
    bench.check(bool(bench.ui.find(NO_LONGER_CHOSEN_LINE, role=cleaner.BUTTON)) and NO_LONGER_CHOSEN in reasons,
                f"CLN-039: the folder removed from the settings since the analysis is kept, as no longer chosen "
                f"({reasons})")


def run(bench: Bench) -> None:
    banners(bench)
    unreadable_cache(bench)
    history_a_folder(bench)
    folder_no_longer_chosen(bench)
