"""What a scenario drives: the machine, Okapi in it, its interface through AT-SPI, the captures."""

from __future__ import annotations

import enum
import json
import os
import shlex
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path

from . import config, coverage
from .machine import Machine, MachineError

SSH_READY_TIMEOUT_SECONDS = 300
DESKTOP_READY_TIMEOUT_SECONDS = 300
DESKTOP_POLL_SECONDS = 2
APP_READY_TIMEOUT_SECONDS = 60
DRIVER_MARGIN_SECONDS = 30
DEFAULT_UI_TIMEOUT_SECONDS = 30.0
WAYLAND_DISPLAY = "wayland-0"
GUEST_STAGING = "/tmp/okapi-vm"
ROOT = "root"
PROFILE_VARIABLE = "LLVM_PROFILE_FILE"
OKAPI_APPLICATION = "okapi"
SHELL_APPLICATION = "gnome-shell"
APP_EXIT_TIMEOUT_SECONDS = 60
APP_POLL_SECONDS = 0.5
SESSION_READY_TIMEOUT_SECONDS = 120
DISPLAY_MANAGER = "gdm"
REVEAL_STEPS = 2
REVEAL_TOP_STEPS = 50
REVEAL_ATTEMPTS = 30
KEY_QCODES = {
    "Escape": "esc", "Tab": "tab", "Enter": "ret", "Space": "spc", "Backspace": "backspace", "Delete": "delete",
    "Up": "up", "Down": "down", "Left": "left", "Right": "right", "Home": "home", "End": "end",
    "PageUp": "pgup", "PageDown": "pgdn", "Ctrl": "ctrl", "Shift": "shift", "Alt": "alt",
    **{f"F{number}": f"f{number}" for number in range(1, 13)},
}
# A qcode names a key's place; the machine's keyboard is French (AZERTY).
AZERTY_LETTERS = {"a": "q", "q": "a", "z": "w", "w": "z", "m": "semicolon"}
SHIFT = "shift"
# Digits are shifted on AZERTY.
AZERTY_CHARACTERS = {
    " ": ["spc"], "-": ["6"], "_": ["8"], ",": ["m"], ";": ["comma"], ":": ["dot"], "!": ["slash"],
    ".": [SHIFT, "comma"], "/": [SHIFT, "dot"], "?": [SHIFT, "m"],
    **{digit: [SHIFT, digit] for digit in "0123456789"},
}


class Elevation(enum.Enum):
    """What the machine's polkit rule answers when the helper is asked for."""

    WITHOUT_PASSWORD = "polkit.Result.YES"
    REFUSED = "polkit.Result.NO"
    PASSWORD = "polkit.Result.AUTH_SELF"


@dataclass(frozen=True)
class Outcome:
    """A command's exit code and its two outputs, kept apart."""

    code: int
    stdout: str
    stderr: str


def qcode(key: str) -> str:
    if key in KEY_QCODES:
        return KEY_QCODES[key]
    if len(key) == 1 and key.isascii() and key.isalpha():
        return AZERTY_LETTERS.get(key.lower(), key.lower())
    raise ScenarioFailure(f"no key {key!r}: one of {', '.join(KEY_QCODES)} or a letter")


def keystroke(character: str) -> list[str]:
    """The qcodes pressed together to type `character` on the AZERTY keyboard."""
    if character in AZERTY_CHARACTERS:
        return AZERTY_CHARACTERS[character]
    if len(character) == 1 and character.isascii() and character.isalpha():
        return [SHIFT, qcode(character)] if character.isupper() else [qcode(character)]
    raise ScenarioFailure(f"cannot type {character!r}: ASCII letters, digits and {''.join(AZERTY_CHARACTERS)}")


class ScenarioFailure(AssertionError):
    pass


def build_okapi(*, instrumented: bool) -> Path:
    """Okapi of this checkout, built on the host (the machine runs the same Ubuntu).

    The target directory is named: a `CARGO_TARGET_DIR` or a configured one
    would otherwise leave stale binaries where this looks. The instrumented
    build has its own, so that it never invalidates the plain one."""
    target = config.WORKSPACE / "target"
    environment = None
    if instrumented:
        target = target / "coverage"
        # RUSTFLAGS instruments the build scripts and procedural macros too,
        # which would leave their profiles in the checkout.
        environment = {**os.environ, "RUSTFLAGS": coverage.rustflags(),
                       PROFILE_VARIABLE: str(target / "build-profiles" / "%p.profraw")}
    packages = [argument for package in config.OKAPI_BINARIES for argument in ("--package", package)]
    subprocess.run(
        ["cargo", "build", "--locked", *packages, "--manifest-path", str(config.WORKSPACE / "Cargo.toml"),
         "--target-dir", str(target)],
        check=True,
        env=environment,
    )
    return target / "debug"


def stage(built: Path, directory: Path) -> Path:
    """A copy of the binaries just built: another run building meanwhile cannot change what this one installs."""
    if directory.exists():
        shutil.rmtree(directory)
    directory.mkdir(parents=True)
    for name in config.OKAPI_BINARIES:
        shutil.copy2(built / name, directory)
    return directory


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
    machine.ssh(ROOT, shlex.join(["install", "-d", GUEST_STAGING]))
    machine.copy_to(
        ROOT,
        [*(binaries / name for name in config.OKAPI_BINARIES), config.POLICY_SOURCE, staged_entry,
         config.ICON_SOURCE, config.GUEST_DRIVER, config.GUEST_FAKE_BROWSER],
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
        ["install", "-D", "--mode=0644", str(staged / config.GUEST_FAKE_BROWSER.name),
         config.GUEST_FAKE_BROWSER_PATH],
        ["rm", "-r", GUEST_STAGING],
    ]
    machine.ssh(ROOT, " && ".join(shlex.join(command) for command in commands))


@dataclass
class Ui:
    """An application's tree (Okapi's, or GNOME Shell's for its own dialogs),
    through the AT-SPI driver run in the logged-in session."""

    bench: Bench
    application: str = OKAPI_APPLICATION

    def _output(self, command: str, arguments: list[str], timeout: float) -> str:
        result = self.bench.machine.ssh(
            self.bench.accounts.admin.name, driver_line(["--app", self.application, command, *arguments]),
            check=False, timeout=timeout + DRIVER_MARGIN_SECONDS,
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
        arguments = [*self._query(name, role, pattern, None), "--timeout", str(timeout),
                     "--settle", str(self.bench.click_settle)]
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

    def node(self, name: str | None = None, *, role: str | None = None, pattern: str | None = None,
             state: str | None = None, index: int | None = None,
             timeout: float = DEFAULT_UI_TIMEOUT_SECONDS) -> dict:
        """One node: its role, name, states, description, value, text, and its
        `extents`, `[x, y, width, height]` in its window's content."""
        arguments = [*self._query(name, role, pattern, state), "--timeout", str(timeout)]
        if index is not None:
            arguments += ["--index", str(index)]
        return self._driver("node", arguments, timeout)

    def center(self, node: dict) -> tuple[int, int]:
        """The screen pixel at the middle of `node`. GNOME Shell's nodes are
        placed on the screen; an application's in its window, whose place
        Wayland keeps to itself: the window is taken as maximized, its content
        then ending at the screen's bottom right corner."""
        x, y, width, height = node["extents"]
        if self.application == SHELL_APPLICATION:
            left, top = 0, 0
        else:
            _, _, window_width, window_height = self.node(role="frame")["extents"]
            left, top = config.SCREEN_WIDTH - window_width, config.SCREEN_HEIGHT - window_height
        return left + x + width // 2, top + y + height // 2

    def set_text(self, name: str | None = None, text: str = "", *, role: str | None = None,
                 pattern: str | None = None, index: int | None = None) -> None:
        """Replaces the contents of a field as a user does: a click in it, all
        selected, then typed, until the field reads it. AccessKit offers no
        editable text to AT-SPI."""
        self.bench.click_at(*self.center(self.node(name, role=role, pattern=pattern, index=index)))
        self.bench.key("Ctrl+a")
        self.bench.type_text(text)
        # QEMU answers before the keys are typed.
        deadline = time.monotonic() + DEFAULT_UI_TIMEOUT_SECONDS
        while (read := self.node(name, role=role, pattern=pattern, index=index).get("text")) != text:
            if time.monotonic() > deadline:
                raise ScenarioFailure(f"the field reads {read!r}, not the {text!r} typed")
            time.sleep(APP_POLL_SECONDS)

    def reveal(self, name: str | None = None, *, role: str | None = None, pattern: str | None = None,
               within: str, within_role: str = "list-box", steps: int = REVEAL_STEPS) -> dict:
        """One node of a scrolled list, `within`, that draws only its rows in
        view: the list is scrolled down from its top until the node shows."""
        place = self.center(self.node(within, role=within_role))
        self.bench.scroll_at(*place, -REVEAL_TOP_STEPS)
        for _ in range(REVEAL_ATTEMPTS):
            if self.find(name, role=role, pattern=pattern):
                return self.node(name, role=role, pattern=pattern)
            self.bench.scroll_at(*place, steps)
            time.sleep(APP_POLL_SECONDS)
        raise ScenarioFailure(f"{name or pattern!r} not found in {within!r}, scrolled to its end")

    def tree(self) -> str:
        return self._output("tree", [], DEFAULT_UI_TIMEOUT_SECONDS)


@dataclass
class Bench:
    """One scenario's run: its machine, the directory that keeps what it leaves,
    and the instrumented build whose profiles it gathers, if any."""

    machine: Machine
    accounts: config.Accounts
    directory: Path
    measured: coverage.Build | None = None
    ui: Ui = field(init=False)
    shell: Ui = field(init=False)
    click_settle: float = field(init=False)
    _captures: int = field(default=0, init=False)

    def __post_init__(self) -> None:
        self.ui = Ui(self)
        self.shell = Ui(self, SHELL_APPLICATION)
        self.click_settle = config.click_settle_seconds()

    def log(self, message: str) -> None:
        line = f"{time.strftime('%H:%M:%S')} {message}"
        print(f"[{self.machine.slot}] {line}", flush=True)
        with (self.directory / "steps.log").open("a", encoding="utf-8") as steps:
            steps.write(line + "\n")

    def run_as(self, account: str, command: str, *, check: bool = True) -> str:
        """Runs a shell line as `account` (a name of `accounts`, or `ROOT`) and gives its output."""
        return self.machine.ssh(account, command, check=check).stdout

    def execute(self, account: str, command: str) -> Outcome:
        """Runs a shell line as `account`, whatever its exit code."""
        result = self.machine.ssh(account, command, check=False)
        return Outcome(result.returncode, result.stdout, result.stderr)

    def user(self, command: str, *, check: bool = True) -> str:
        """As the logged-in administrator account, in its graphical session's bus."""
        return self.run_as(self.accounts.admin.name, command, check=check)

    def other(self, command: str, *, check: bool = True) -> str:
        """As the second account, without administrator rights nor graphical session."""
        return self.run_as(self.accounts.other.name, command, check=check)

    def root(self, command: str, *, check: bool = True) -> str:
        return self.run_as(ROOT, command, check=check)

    def okapi_environment(self) -> dict[str, str]:
        """What Okapi's binaries are started with: where their profiles go, when measured."""
        return {PROFILE_VARIABLE: coverage.PROFILE_PATTERN} if self.measured else {}

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

    def key(self, *combinations: str) -> None:
        """Presses keys as a keyboard would, each combination in turn: `"Escape"`, `"Ctrl+Z"`, `"Shift+Tab"`."""
        for combination in combinations:
            time.sleep(self.click_settle)
            self.machine.send_key([qcode(key) for key in combination.split("+")])
            self.log(f"pressed {combination}")

    def type_text(self, text: str) -> None:
        """Types `text` on the keyboard, in whatever has the focus."""
        strokes = [keystroke(character) for character in text]
        time.sleep(self.click_settle)
        for stroke in strokes:
            self.machine.send_key(stroke)
        self.log(f"typed {text!r}")

    def click_at(self, x: int, y: int) -> None:
        """A mouse click at a pixel of the screen, as `ui.center(node)` gives one."""
        time.sleep(self.click_settle)
        self.machine.click_at(x, y)
        self.log(f"clicked at {x}, {y}")

    def scroll_at(self, x: int, y: int, steps: int) -> None:
        """Turns the mouse wheel at a pixel of the screen: `steps` notches down, up when negative."""
        self.machine.scroll_at(x, y, steps)
        self.log(f"scrolled {steps} at {x}, {y}")

    def set_elevation(self, answer: Elevation) -> None:
        """A polkit rule of this machine alone, for the helper's action and the test account."""
        rule = (
            "polkit.addRule(function (action, subject) {\n"
            f"    if (action.id == {json.dumps(config.POLKIT_ACTION)} && subject.user == "
            f"{json.dumps(self.accounts.admin.name)}) {{\n"
            f"        return {answer.value};\n"
            "    }\n"
            "});\n"
        )
        self.root(f"cat > {shlex.quote(config.GUEST_POLKIT_RULE)} <<'EOF'\n{rule}EOF")
        self.log(f"polkit answers the helper's action: {answer.name}")

    def elevation_by_policy(self) -> None:
        """No rule of this machine: the policy Okapi installs answers alone
        (`auth_admin_keep`, the password asked once, then kept a few minutes)."""
        self.root(shlex.join(["rm", "-f", config.GUEST_POLKIT_RULE]))
        self.log("polkit answers the helper's action by Okapi's policy alone")

    def allow_elevation_without_password(self) -> None:
        """Without it, pkexec waits for GNOME's password dialog."""
        self.set_elevation(Elevation.WITHOUT_PASSWORD)

    def advance_clock(self, seconds: int) -> None:
        """Moves the machine's clock forward, without NTP to pull it back: what
        exists already gets older at once (its change time cannot be set back)."""
        self.root(f"timedatectl set-ntp false && date --set=@$(( $(date +%s) + {seconds} ))")
        self.log(f"clock moved forward by {seconds} s")

    def launch_app(self, environment: dict[str, str] | None = None, *, window: bool = True) -> None:
        """Starts the interface in the user's systemd, as GNOME starts an application,
        with `environment` added (`LANG`, `TMPDIR`...); waits for its window unless
        `window` is false. The unit stays once the interface ends, for `app_exit_code`."""
        self.stop_app()
        variables = {"WAYLAND_DISPLAY": WAYLAND_DISPLAY, **self.okapi_environment(), **(environment or {})}
        self.user(shlex.join([
            "systemd-run", "--user", "--quiet", f"--unit={config.APP_UNIT}", "--property=RemainAfterExit=yes",
            *(f"--setenv={name}={value}" for name, value in variables.items()),
            f"{config.GUEST_BINARIES}/okapi-app",
        ]))
        if window:
            self.ui.wait(role="frame", timeout=APP_READY_TIMEOUT_SECONDS)
            self.log("Okapi's window is showing")

    def app_exit_code(self, timeout: float = APP_EXIT_TIMEOUT_SECONDS) -> int:
        """Waits for the interface to end on its own and gives its exit code."""
        query = shlex.join(["systemctl", "--user", "show", config.APP_UNIT, "--property=MainPID",
                            "--property=ExecMainCode", "--property=ExecMainStatus", "--property=LoadState"])
        deadline = time.monotonic() + timeout
        while True:
            properties = dict(line.split("=", 1) for line in self.user(query).splitlines() if "=" in line)
            if properties.get("LoadState") != "loaded":
                raise ScenarioFailure("the interface was not started by launch_app")
            if properties.get("MainPID") == "0" and properties.get("ExecMainCode") != "0":
                code = int(properties["ExecMainStatus"])
                self.log(f"the interface ended with {code}")
                return code
            if time.monotonic() > deadline:
                raise ScenarioFailure(f"the interface still runs after {timeout:.0f} s")
            time.sleep(APP_POLL_SECONDS)

    def stop_app(self) -> None:
        for action in ("stop", "reset-failed"):
            self.user(shlex.join(["systemctl", "--user", action, config.APP_UNIT]), check=False)

    def restart_session(self) -> None:
        """Restarts gdm, which ends the administrator account's session and logs
        it in again: a group joined or a PAM module turned on since then applies.
        gdm logs in automatically only when it starts: after a mere logout, it
        waits on its greeter."""
        account = self.accounts.admin.name
        before = self.root(shlex.join(["loginctl", "show-user", account, "--property=Display", "--value"])).strip()
        self.root(shlex.join(["systemctl", "restart", DISPLAY_MANAGER]))
        deadline = time.monotonic() + SESSION_READY_TIMEOUT_SECONDS
        while True:
            now = self.root(shlex.join(["loginctl", "show-user", account, "--property=Display", "--value"]),
                            check=False).strip()
            if now and now != before:
                break
            if time.monotonic() > deadline:
                raise MachineError(f"gdm did not log {account} in again after {SESSION_READY_TIMEOUT_SECONDS} s")
            time.sleep(DESKTOP_POLL_SECONDS)
        self.log(f"{account}'s session restarted ({before} → {now})")
        self.wait_for_desktop()

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

    def ready(self, binaries: Path) -> None:
        """This checkout's Okapi installed in the machine, GNOME up."""
        self.machine.wait_for_ssh(SSH_READY_TIMEOUT_SECONDS)
        install_okapi(self.machine, binaries)
        if self.measured:
            coverage.prepare_machine(self)
        self.log("Okapi installed" + (", instrumented" if self.measured else ""))
        self.wait_for_desktop()

    def collect(self) -> None:
        """Keeps on the host what the run leaves: Okapi's journals, the system's journal, the profiles."""
        self.stop_app()
        state = self.directory / "okapi-state"
        try:
            self.machine.copy_from(self.accounts.admin.name, ".local/state/okapi", state)
        except subprocess.CalledProcessError as error:
            self.log(f"~/.local/state/okapi not kept: {error.stderr.strip()}")
        journal = self.root("journalctl --boot --no-pager --output=short-precise")
        (self.directory / "journal.txt").write_text(journal, encoding="utf-8")
        if self.measured:
            coverage.gather(self, self.measured)
