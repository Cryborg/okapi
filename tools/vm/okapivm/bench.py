"""What a scenario drives: the machine, Okapi in it, its interface through AT-SPI, the captures."""

from __future__ import annotations

import json
import shlex
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path

from . import config
from .machine import Machine, MachineError

SSH_READY_TIMEOUT_SECONDS = 300
DESKTOP_READY_TIMEOUT_SECONDS = 300
DESKTOP_POLL_SECONDS = 2
APP_READY_TIMEOUT_SECONDS = 60
DRIVER_MARGIN_SECONDS = 30
DEFAULT_UI_TIMEOUT_SECONDS = 30.0
WAYLAND_DISPLAY = "wayland-0"
OKAPI_PACKAGES = ("okapi-app", "okapi-helper", "okapi")
GUEST_STAGING = "/tmp/okapi-vm"


class ScenarioFailure(AssertionError):
    pass


def build_okapi() -> Path:
    """Okapi of this checkout, built on the host (the machine runs the same Ubuntu).

    The target directory is named: a `CARGO_TARGET_DIR` or a configured one
    would otherwise leave stale binaries where this looks."""
    target = config.WORKSPACE / "target"
    packages = [argument for package in OKAPI_PACKAGES for argument in ("--package", package)]
    subprocess.run(
        ["cargo", "build", "--locked", *packages, "--manifest-path", str(config.WORKSPACE / "Cargo.toml"),
         "--target-dir", str(target)],
        check=True,
    )
    return target / "debug"


def driver_line(words: list[str]) -> str:
    """The shell line that runs the AT-SPI driver in the machine."""
    return shlex.join(["python3", "-I", config.GUEST_DRIVER_PATH, *words])


def install_okapi(machine: Machine, binaries: Path) -> None:
    """Installs the interface, the CLI, the helper and its policy where a real install puts them."""
    paths = config.helper_paths()
    entry = config.DESKTOP_ENTRY_SOURCE.read_text(encoding="utf-8")
    staged_entry = machine.directory / config.DESKTOP_ENTRY_SOURCE.name
    staged_entry.write_text(
        "\n".join(
            f"Exec={config.GUEST_BINARIES}/okapi-app" if line.startswith("Exec=") else line
            for line in entry.splitlines()
        ) + "\n",
        encoding="utf-8",
    )
    machine.ssh("root", shlex.join(["install", "-d", GUEST_STAGING]))
    machine.copy_to(
        "root",
        [binaries / "okapi-app", binaries / "okapi", binaries / "okapi-helper", config.POLICY_SOURCE,
         staged_entry, config.ICON_SOURCE, config.GUEST_DRIVER],
        GUEST_STAGING,
    )
    staged = Path(GUEST_STAGING)
    commands = [
        ["install", "-D", "--mode=0644", str(staged / config.POLICY_SOURCE.name), paths.policy],
        ["install", "-d", "--mode=0755", str(Path(paths.helper).parent)],
        ["install", "--mode=0755", str(staged / "okapi-helper"), paths.helper],
        ["install", "--mode=0755", str(staged / "okapi-app"), str(staged / "okapi"), config.GUEST_BINARIES],
        ["install", "-D", "--mode=0644", str(staged / staged_entry.name),
         f"{config.GUEST_APPLICATIONS}/{staged_entry.name}"],
        ["install", "-D", "--mode=0644", str(staged / config.ICON_SOURCE.name),
         f"{config.GUEST_ICONS}/{staged_entry.stem}.svg"],
        ["install", "-D", "--mode=0644", str(staged / config.GUEST_DRIVER.name), config.GUEST_DRIVER_PATH],
        ["rm", "-r", GUEST_STAGING],
    ]
    machine.ssh("root", " && ".join(shlex.join(command) for command in commands))


@dataclass
class Ui:
    """Okapi's window, through the AT-SPI driver run in the logged-in session."""

    bench: Bench

    def _output(self, command: str, arguments: list[str], timeout: float) -> str:
        result = self.bench.machine.ssh(
            self.bench.accounts.admin.name, driver_line([command, *arguments]), check=False,
            timeout=timeout + DRIVER_MARGIN_SECONDS,
        )
        if result.returncode != 0:
            raise ScenarioFailure(f"{command} {shlex.join(arguments)}: {result.stderr.strip()}")
        return result.stdout

    def _driver(self, command: str, arguments: list[str], timeout: float) -> object:
        return json.loads(self._output(command, arguments, timeout))

    @staticmethod
    def _query(name: str | None, role: str | None, pattern: str | None, state: str | None) -> list[str]:
        arguments = []
        for option, value in (("--name", name), ("--role", role), ("--pattern", pattern), ("--state", state)):
            if value is not None:
                arguments += [option, value]
        return arguments

    def click(self, name: str | None = None, *, role: str | None = None, pattern: str | None = None,
              index: int | None = None, timeout: float = DEFAULT_UI_TIMEOUT_SECONDS) -> dict:
        arguments = [*self._query(name, role, pattern, None), "--timeout", str(timeout)]
        if index is not None:
            arguments += ["--index", str(index)]
        clicked = self._driver("click", arguments, timeout)
        self.bench.log(f"clicked {clicked['node']['role']} {clicked['node']['name']!r}")
        return clicked

    def wait(self, name: str | None = None, *, role: str | None = None, pattern: str | None = None,
             state: str | None = None, gone: bool = False, timeout: float = DEFAULT_UI_TIMEOUT_SECONDS) -> list[dict]:
        arguments = [*self._query(name, role, pattern, state), "--timeout", str(timeout)]
        if gone:
            arguments.append("--gone")
        return self._driver("wait", arguments, timeout)

    def find(self, name: str | None = None, *, role: str | None = None, pattern: str | None = None,
             state: str | None = None) -> list[dict]:
        return self._driver("find", self._query(name, role, pattern, state), DEFAULT_UI_TIMEOUT_SECONDS)

    def tree(self) -> str:
        return self._output("tree", [], DEFAULT_UI_TIMEOUT_SECONDS)


@dataclass
class Bench:
    """One scenario's run: its machine and the directory that keeps what it leaves."""

    machine: Machine
    accounts: config.Accounts
    directory: Path
    ui: Ui = field(init=False)
    _captures: int = field(default=0, init=False)

    def __post_init__(self) -> None:
        self.ui = Ui(self)

    def log(self, message: str) -> None:
        line = f"{time.strftime('%H:%M:%S')} {message}"
        print(line, flush=True)
        with (self.directory / "steps.log").open("a", encoding="utf-8") as steps:
            steps.write(line + "\n")

    def user(self, command: str, *, check: bool = True) -> str:
        """Runs `command` as the logged-in administrator account, in its graphical session's bus."""
        return self.machine.ssh(self.accounts.admin.name, command, check=check).stdout

    def root(self, command: str, *, check: bool = True) -> str:
        return self.machine.ssh("root", command, check=check).stdout

    def check(self, condition: bool, message: str) -> None:
        if not condition:
            raise ScenarioFailure(message)
        self.log(f"ok: {message}")

    def screenshot(self, label: str) -> Path:
        self._captures += 1
        path = self.directory / f"{self._captures:02d}-{label}.png"
        self.machine.screendump(path)
        self.log(f"capture {path.name}")
        return path

    def allow_elevation_without_password(self) -> None:
        """A polkit rule of this machine alone: the helper runs for the test account, without a prompt."""
        rule = (
            "polkit.addRule(function (action, subject) {\n"
            f"    if (action.id == {json.dumps(config.POLKIT_ACTION)} && subject.user == "
            f"{json.dumps(self.accounts.admin.name)}) {{\n"
            "        return polkit.Result.YES;\n"
            "    }\n"
            "});\n"
        )
        self.root(f"cat > {shlex.quote(config.GUEST_POLKIT_RULE)} <<'EOF'\n{rule}EOF")
        self.log("polkit lets the helper run without a password")

    def advance_clock(self, seconds: int) -> None:
        """Moves the machine's clock forward, without NTP to pull it back: what
        exists already gets older at once (its change time cannot be set back)."""
        self.root(f"timedatectl set-ntp false && date --set=@$(( $(date +%s) + {seconds} ))")
        self.log(f"clock moved forward by {seconds} s")

    def launch_app(self) -> None:
        """Starts the interface in the user's systemd, as GNOME starts an application."""
        self.user(shlex.join([
            "systemd-run", "--user", "--quiet", f"--unit={config.APP_UNIT}", "--collect",
            f"--setenv=WAYLAND_DISPLAY={WAYLAND_DISPLAY}", f"{config.GUEST_BINARIES}/okapi-app",
        ]))
        self.ui.wait(role="frame", timeout=APP_READY_TIMEOUT_SECONDS)
        self.log("Okapi's window is showing")

    def stop_app(self) -> None:
        self.user(shlex.join(["systemctl", "--user", "stop", config.APP_UNIT]), check=False)

    def wait_for_desktop(self) -> None:
        deadline = time.monotonic() + DESKTOP_READY_TIMEOUT_SECONDS
        probe = driver_line(["applications"])
        while time.monotonic() < deadline:
            result = self.machine.ssh(self.accounts.admin.name, probe, check=False, timeout=DEFAULT_UI_TIMEOUT_SECONDS)
            if result.returncode == 0 and "gnome-shell" in result.stdout:
                self.log("GNOME is up, its accessibility bus answers")
                return
            time.sleep(DESKTOP_POLL_SECONDS)
        raise MachineError(f"GNOME not up after {DESKTOP_READY_TIMEOUT_SECONDS} s")

    def collect(self) -> None:
        """Keeps on the host what the run leaves: Okapi's journals, the system's journal."""
        self.stop_app()
        state = self.directory / "okapi-state"
        try:
            self.machine.copy_from(self.accounts.admin.name, ".local/state/okapi", state)
        except subprocess.CalledProcessError as error:
            self.log(f"~/.local/state/okapi not kept: {error.stderr.strip()}")
        journal = self.root("journalctl --boot --no-pager --output=short-precise")
        (self.directory / "journal.txt").write_text(journal, encoding="utf-8")
