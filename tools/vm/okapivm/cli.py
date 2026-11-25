"""`okapi-vm`: Okapi's test machine, from the download to the scenarios' runs."""

from __future__ import annotations

import argparse
import importlib.util
import json
import shutil
import subprocess
import sys
import time
import traceback
from pathlib import Path
from types import ModuleType

from . import config, prepare
from .bench import SSH_READY_TIMEOUT_SECONDS, Bench, ScenarioFailure, build_okapi, driver_line, install_okapi
from .machine import Machine, MachineError


def scenario_names() -> list[str]:
    return sorted(path.stem for path in config.SCENARIOS.glob("*.py") if not path.stem.startswith("_"))


def load_scenario(name: str) -> ModuleType:
    path = config.SCENARIOS / f"{name}.py"
    if not path.exists():
        raise MachineError(f"no scenario {name!r}; there are: {', '.join(scenario_names())}")
    spec = importlib.util.spec_from_file_location(f"scenarios.{name}", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def new_run(home: config.Home, label: str) -> Path:
    directory = home.runs / f"{time.strftime('%Y%m%d-%H%M%S')}-{label}"
    directory.mkdir(parents=True)
    return directory


def booted(home: config.Home, directory: Path) -> Bench:
    """A machine just started from the base; what can fail before is done first, so that it starts nothing."""
    accounts = config.read_accounts()
    bench = Bench(Machine.start(home), accounts, directory)
    bench.log(f"machine started, SSH on 127.0.0.1:{bench.machine.ssh_port}")
    return bench


def ready(bench: Bench, binaries: Path) -> None:
    """This checkout's Okapi installed in the machine, GNOME up."""
    bench.machine.wait_for_ssh(SSH_READY_TIMEOUT_SECONDS)
    install_okapi(bench.machine, binaries)
    bench.log("Okapi installed")
    bench.wait_for_desktop()


def run_scenario(home: config.Home, name: str, binaries: Path, *, keep: bool) -> bool:
    scenario = load_scenario(name)
    directory = new_run(home, name)
    passed = False
    try:
        bench = booted(home, directory)
    except (MachineError, OSError, subprocess.SubprocessError) as error:
        print(f"okapi-vm: {error}", file=sys.stderr)
    else:
        try:
            ready(bench, binaries)
            bench.log(f"scenario {name}")
            scenario.run(bench)
            passed = True
        except (ScenarioFailure, MachineError) as failure:
            bench.log(f"FAILED: {failure}")
        except Exception:
            (directory / "error.txt").write_text(traceback.format_exc(), encoding="utf-8")
            print(traceback.format_exc(), file=sys.stderr)
        finally:
            wind_up(bench, passed=passed, keep=keep)
    (directory / "result.json").write_text(json.dumps({"scenario": name, "passed": passed}) + "\n", encoding="utf-8")
    print(f"{'PASSED' if passed else 'FAILED'} {name}: {directory}")
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


def running_machine(home: config.Home) -> Machine:
    machine = Machine(home)
    if machine.pid() is None:
        raise MachineError("no machine is running: `okapi-vm start` first")
    return machine


def passed_on(words: list[str]) -> list[str]:
    """The words after `--`, which keeps the driver's own options from argparse."""
    return words[1:] if words[:1] == ["--"] else words


def forward(result: subprocess.CompletedProcess[str]) -> int:
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    return result.returncode


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(prog="okapi-vm", description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    commands.add_parser("fetch", help=f"download the Ubuntu cloud image ({config.IMAGE_SIZE}) and check it")
    prepare_command = commands.add_parser("prepare", help="provision the base image, once")
    prepare_command.add_argument("--force", action="store_true", help="build it again")
    run = commands.add_parser("run", help="run scenarios, each on a fresh machine")
    run.add_argument("scenarios", nargs="*", help=f"default: all ({', '.join(scenario_names())})")
    run.add_argument("--keep", action="store_true", help="leave the last machine running")
    commands.add_parser("list", help="the scenarios")
    commands.add_parser("start", help="start a fresh machine with Okapi, to explore it")
    commands.add_parser("stop", help="stop the machine and drop everything it wrote")
    ssh = commands.add_parser("ssh", help="run a command in the running machine")
    ssh.add_argument("--root", action="store_true")
    ssh.add_argument("words", nargs=argparse.REMAINDER)
    ui = commands.add_parser("ui", help="the AT-SPI driver in the running machine (`ui -- --help`)")
    ui.add_argument("words", nargs=argparse.REMAINDER)
    shot = commands.add_parser("shot", help="capture the running machine's screen")
    shot.add_argument("path", type=Path)
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
            prepare.prepare(home, force=arguments.force)
        case "list":
            for name in scenario_names():
                summary, _, _ = (load_scenario(name).__doc__ or "").strip().partition("\n")
                print(f"{name}: {summary}")
        case "run":
            names = arguments.scenarios or scenario_names()
            # Once for all: an edit during the series must not test two builds.
            binaries = build_okapi()
            results = [run_scenario(home, name, binaries, keep=arguments.keep and name == names[-1])
                       for name in names]
            return 0 if all(results) else 1
        case "start":
            binaries = build_okapi()
            bench = booted(home, new_run(home, "start"))
            try:
                ready(bench, binaries)
            except BaseException:
                wind_up(bench, passed=False, keep=False)
                raise
            print(f"running; `okapi-vm stop` drops it. Notes in {bench.directory}")
        case "stop":
            Machine(home).destroy()
        case "ssh":
            machine = running_machine(home)
            user = "root" if arguments.root else config.read_accounts().admin.name
            # Joined as ssh itself joins its words: the remote shell splits them.
            return forward(machine.ssh(user, " ".join(passed_on(arguments.words)), check=False))
        case "ui":
            machine = running_machine(home)
            line = driver_line(passed_on(arguments.words))
            return forward(machine.ssh(config.read_accounts().admin.name, line, check=False))
        case "shot":
            running_machine(home).screendump(arguments.path.resolve())
    return 0
