"""`okapi-vm`: Okapi's test machines, from the download to the scenarios' runs and their coverage."""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import textwrap
import time
import traceback
from pathlib import Path

from . import catalog, config, coverage, prepare, slots
from .bench import Bench, ScenarioFailure, build_okapi, driver_line, stage
from .catalog import Scenario
from .machine import Machine, MachineError

DESCRIPTION_INDENT = "    "


def new_run(home: config.Home, machine: Machine, label: str) -> Path:
    directory = home.runs / f"{time.strftime('%Y%m%d-%H%M%S')}-{machine.slot}-{label}"
    directory.mkdir(parents=True)
    return directory


def booted(machine: Machine, directory: Path, measured: coverage.Build | None) -> Bench:
    """A machine just started from the base; what can fail before is done first, so that it starts nothing."""
    accounts = config.read_accounts()
    machine.start()
    bench = Bench(machine, accounts, directory, measured)
    bench.log(f"machine started in slot {machine.slot}, SSH on 127.0.0.1:{machine.ssh_port}")
    return bench


def run_scenario(home: config.Home, machine: Machine, scenario: Scenario, binaries: Path,
                 measured: coverage.Build | None, *, keep: bool) -> bool:
    directory = new_run(home, machine, scenario.name)
    passed = False
    try:
        bench = booted(machine, directory, measured)
    except (MachineError, OSError, subprocess.SubprocessError) as error:
        print(f"okapi-vm: {error}", file=sys.stderr)
    else:
        try:
            bench.ready(binaries)
            bench.log(f"scenario {scenario.name}")
            scenario.run(bench)
            passed = True
        except (ScenarioFailure, MachineError) as failure:
            bench.log(f"FAILED: {failure}")
        except Exception:
            (directory / "error.txt").write_text(traceback.format_exc(), encoding="utf-8")
            print(traceback.format_exc(), file=sys.stderr)
        finally:
            wind_up(bench, passed=passed, keep=keep)
    result = {"scenario": scenario.name, "slot": machine.slot, "coverage": measured is not None, "passed": passed}
    (directory / "result.json").write_text(json.dumps(result) + "\n", encoding="utf-8")
    print(f"{'PASSED' if passed else 'FAILED'} {scenario.name}: {directory}")
    return passed


def wind_up(bench: Bench, *, passed: bool, keep: bool) -> None:
    """Keeps on the host what the machine can still give, then drops it unless asked to keep it."""
    attempts = [bench.collect, lambda: shutil.copy(bench.machine.serial_log, bench.directory)]
    if not passed:
        attempts = [lambda: bench.screenshot("echec"),
                    lambda: (bench.directory / "tree.txt").write_text(bench.ui.tree(), encoding="utf-8"),
                    *attempts]
    try:
        for attempt in attempts:
            try:
                attempt()
            except (ScenarioFailure, MachineError, OSError, subprocess.SubprocessError) as error:
                bench.log(f"evidence not kept: {error}")
    finally:
        if not keep:
            bench.machine.destroy()


def run(home: config.Home, arguments: argparse.Namespace) -> int:
    scenarios = catalog.select(arguments.scenarios, arguments.tag)
    with slots.claimed(home, slots.chosen_slot(arguments.slot)) as machine:
        # Once for all: an edit during the series must not test two builds.
        built = build_okapi(instrumented=arguments.coverage)
        if arguments.coverage:
            measured = coverage.keep(home, built)
            binaries = measured.directory
        else:
            measured = None
            binaries = stage(built, home.slot(machine.slot) / "binaries")
        series = [scenario for _ in range(arguments.repeat) for scenario in scenarios]
        results = [
            (scenario.name, run_scenario(home, machine, scenario, binaries, measured,
                                         keep=arguments.keep and position == len(series) - 1))
            for position, scenario in enumerate(series)
        ]
    if len(results) > 1:
        failed = [name for name, passed in results if not passed]
        print(f"{len(results) - len(failed)}/{len(results)} passed" + (f"; failed: {', '.join(failed)}" if failed else ""))
    return 0 if all(passed for _, passed in results) else 1


def start(home: config.Home, arguments: argparse.Namespace) -> None:
    with slots.claimed(home, slots.chosen_slot(arguments.slot)) as machine:
        binaries = stage(build_okapi(instrumented=False), home.slot(machine.slot) / "binaries")
        bench = booted(machine, new_run(home, machine, "start"), None)
        try:
            bench.ready(binaries)
        except BaseException:
            wind_up(bench, passed=False, keep=False)
            raise
    print(f"running in slot {machine.slot}; `okapi-vm stop --slot {machine.slot}` drops it. Notes in {bench.directory}")


def listing() -> None:
    for scenario in catalog.every():
        tags = f" [{', '.join(scenario.tags)}]" if scenario.tags else ""
        print(f"{scenario.name}{tags}")
        print(textwrap.indent(scenario.description or "(no docstring)", DESCRIPTION_INDENT))
        print()


def report(home: config.Home) -> None:
    done = coverage.report(home)
    print(done.files.read_text(encoding="utf-8").rstrip().splitlines()[-1])
    print(f"{done.profiles} profiles from {done.runs} runs, against build {done.build.directory.name}")
    print(f"by file: {done.files}\nby function: {done.functions}\nHTML: {done.html}")


def passed_on(words: list[str]) -> list[str]:
    """The words after `--`, which keeps the driver's own options from argparse."""
    return words[1:] if words[:1] == ["--"] else words


def forward(result: subprocess.CompletedProcess[str]) -> int:
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    return result.returncode


def parser() -> argparse.ArgumentParser:
    slot = argparse.ArgumentParser(add_help=False)
    slot.add_argument("--slot", help=f"{config.SLOTS.start} to {config.SLOTS.stop - 1}; default "
                                     f"${config.SLOT_VARIABLE}, else a free one (or the only one running)")
    root = argparse.ArgumentParser(prog="okapi-vm", description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    commands.add_parser("fetch", help=f"download the Ubuntu cloud image ({config.IMAGE_SIZE}) and check it")
    prepare_command = commands.add_parser("prepare", help="provision the base image, once")
    prepare_command.add_argument("--force", action="store_true", help="build it again")
    run_command = commands.add_parser("run", parents=[slot], help="run scenarios, each on a fresh machine")
    run_command.add_argument("scenarios", nargs="*", help="names or shell patterns (quoted: 'cleaner_*'); default: all")
    run_command.add_argument("--tag", action="append", default=[], help="only those with this tag (repeatable)")
    run_command.add_argument("--keep", action="store_true", help="leave the last machine running")
    run_command.add_argument("--coverage", action="store_true", help="instrumented build, profiles gathered")
    run_command.add_argument("--repeat", type=int, default=1, metavar="N", help="run the series N times")
    commands.add_parser("list", help="the scenarios, their tags and what they try")
    commands.add_parser("start", parents=[slot], help="start a fresh machine with Okapi, to explore it")
    commands.add_parser("stop", parents=[slot], help="stop a machine and drop everything it wrote")
    ssh = commands.add_parser("ssh", parents=[slot], help="run a command in a running machine")
    ssh.add_argument("--root", action="store_true")
    ssh.add_argument("words", nargs=argparse.REMAINDER)
    ui = commands.add_parser("ui", parents=[slot], help="the AT-SPI driver in a running machine (`ui -- --help`)")
    ui.add_argument("words", nargs=argparse.REMAINDER)
    shot = commands.add_parser("shot", parents=[slot], help="capture a running machine's screen")
    shot.add_argument("path", type=Path)
    coverage_command = commands.add_parser("coverage", help="what the runs with --coverage gathered")
    coverage_command.add_argument("action", choices=["report", "reset"],
                                  help="report: merge everything and write the reports; reset: forget it all")
    return root


def main() -> int:
    arguments = parser().parse_args()
    home = config.Home(config.bench_home())
    try:
        return dispatch(arguments, home)
    except (MachineError, OSError, subprocess.SubprocessError) as error:
        print(f"okapi-vm: {error}", file=sys.stderr)
        return 1


def dispatch(arguments: argparse.Namespace, home: config.Home) -> int:
    match arguments.command:
        case "fetch":
            prepare.fetch(home)
        case "prepare":
            if arguments.force and slots.any_running(home):
                raise MachineError("a slot is in use, its machine runs over the base: stop it first")
            prepare.prepare(home, force=arguments.force)
        case "list":
            listing()
        case "run":
            return run(home, arguments)
        case "start":
            start(home, arguments)
        case "stop":
            slots.stop(home, slots.chosen_slot(arguments.slot))
        case "ssh":
            machine = slots.running(home, slots.chosen_slot(arguments.slot))
            user = "root" if arguments.root else config.read_accounts().admin.name
            # Joined as ssh itself joins its words: the remote shell splits them.
            return forward(machine.ssh(user, " ".join(passed_on(arguments.words)), check=False))
        case "ui":
            machine = slots.running(home, slots.chosen_slot(arguments.slot))
            line = driver_line(passed_on(arguments.words))
            return forward(machine.ssh(config.read_accounts().admin.name, line, check=False))
        case "shot":
            slots.running(home, slots.chosen_slot(arguments.slot)).screendump(arguments.path.resolve())
        case "coverage" if arguments.action == "report":
            report(home)
        case "coverage":
            coverage.reset(home)
    return 0
