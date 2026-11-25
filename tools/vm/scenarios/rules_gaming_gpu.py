"""Rules: the games' caches, the GPUs' ones, and Unity's root shared by both categories.

A file in each game's cache: Steam's logs and web cache, itch.io, Heroic,
Lutris and Minecraft are checked, Steam's shader pre-cache not (RGL-020).
The « Caches GPU » card is offered unchecked, and Mesa's, NVIDIA's, CUDA's and
Unity's caches are offered unchecked (RGL-021). `${CACHE}/unity3d`, a root
of « Applications » and of « Caches GPU » (a cache to reset), is one group,
never preselected (RGL-022), an analysis of « Applications » alone
included (INV-505).
"""

from __future__ import annotations

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["rules", "gaming", "gpu"]

AGING_SECONDS = 2 * 60 * 60
SIZE = 4096
ENTRY = "okapi-vm"
APPLICATIONS = "Applications"
GAMES = "Jeux"
GPU = "Caches GPU"
CLI_CATEGORIES = ["--apps", "--gaming", "--gpu"]
APPS_OPTION = "--apps"
# Each game's group, in French, the files planted, and whether it is checked.
GAMING = {
    "Steam": ([f"~/.local/share/Steam/logs/{ENTRY}", f"~/.local/share/Steam/appcache/httpcache/{ENTRY}"], True),
    "itch.io": ([f"~/.config/itch/logs/{ENTRY}"], True),
    "Heroic Games Launcher": ([f"~/.config/heroic/GPUCache/{ENTRY}"], True),
    "Lutris": ([f"~/.cache/lutris/{ENTRY}"], True),
    "Minecraft Java Edition": ([f"~/.minecraft/logs/{ENTRY}"], True),
    "Précache des shaders Steam": ([f"~/.local/share/Steam/steamapps/shadercache/{ENTRY}"], False),
}
UNITY = "Unity"
UNITY_FILE = f"~/.cache/unity3d/{ENTRY}"
UNITY_RULES = {"apps:unity", "gpu:unity-cache"}
GPU_CACHES = {
    "Cache des shaders Mesa": f"~/.cache/mesa_shader_cache/{ENTRY}",
    "Cache des shaders NVIDIA": f"~/.cache/nvidia/GLCache/{ENTRY}",
    "Cache de calcul CUDA": f"~/.nv/ComputeCache/{ENTRY}",
    UNITY: UNITY_FILE,
}
FINISH = "Terminer"


def files(bench: Bench, *paths: str) -> common.Files:
    return common.Files(bench.accounts.admin.name, list(paths), size=SIZE)


def holding(bench: Bench, document: dict, path: str) -> list[dict]:
    """The CLI's groups that offer `path`, or a folder above it."""
    full = common.expand_home(bench, path)
    return [group for group in document["groups"] if any(full.startswith(item["path"]) for item in group["entries"])]


def shared_root(bench: Bench) -> None:
    every = holding(bench, common.cli_json(bench, *CLI_CATEGORIES), UNITY_FILE)
    judged = [(set(group["rules"]), group["preselected"]) for group in every]
    bench.check(judged == [(UNITY_RULES, False)],
                f"RGL-022: `${{CACHE}}/unity3d`, a root of « {APPLICATIONS} » and of « {GPU} », is one group of both "
                f"rules, never preselected ({judged})")
    alone = [group["preselected"] for group in holding(bench, common.cli_json(bench, APPS_OPTION), UNITY_FILE)]
    bench.check(alone == [False], f"INV-505: an analysis of « {APPLICATIONS} » alone keeps Unity's cache to reset, "
                                  f"not preselected ({alone})")


def checked(shown: dict[str, dict], group: str) -> bool | None:
    return cleaner.CHECKED in shown[group]["states"] if group in shown else None


def run(bench: Bench) -> None:
    common.plant(bench, files(bench, *(path for paths, _ in GAMING.values() for path in paths), *GPU_CACHES.values()))
    bench.advance_clock(AGING_SECONDS)
    shared_root(bench)

    bench.launch_app()
    cleaner.open_page(bench)
    card = bench.ui.node(GPU, role=cleaner.CHECK_BOX)
    bench.screenshot("choix")
    bench.check(cleaner.CHECKED not in card["states"], f"RGL-021: the « {GPU} » card is offered unchecked")
    cleaner.choose(bench, APPLICATIONS)
    bench.check(cleaner.analyze(bench) == cleaner.RESULTS, f"the analysis of « {APPLICATIONS} » shows results")
    bench.check(checked(cleaner.rows(bench), UNITY) is False,
                f"INV-505: in an analysis of « {APPLICATIONS} » alone, « {UNITY} » is offered unchecked")
    bench.stop_app()

    bench.launch_app()
    cleaner.open_page(bench)
    cleaner.choose(bench, GAMES, GPU)
    bench.check(cleaner.analyze(bench) == cleaner.RESULTS, f"the analysis of « {GAMES} » and « {GPU} » shows results")
    shown = cleaner.rows(bench)
    bench.screenshot("jeux-gpu")
    games = {group: checked(shown, group) for group in GAMING}
    bench.check(games == {group: chosen for group, (_, chosen) in GAMING.items()},
                f"RGL-020: Steam, itch.io, Heroic, Lutris and Minecraft checked, Steam's shader pre-cache not ({games})")
    gpu = {group: checked(shown, group) for group in GPU_CACHES}
    bench.check(all(state is False for state in gpu.values()),
                f"RGL-021, RGL-022: Mesa's, NVIDIA's, CUDA's and Unity's caches offered unchecked ({gpu})")

    cleaner.clean(bench)
    bench.screenshot("recu")
    common.check_gone(bench, files(bench, *(path for paths, chosen in GAMING.values() if chosen for path in paths)))
    common.check_kept(bench, files(bench, *(path for paths, chosen in GAMING.values() if not chosen for path in paths),
                                   *GPU_CACHES.values()))
    bench.ui.click(FINISH, role=cleaner.BUTTON)
