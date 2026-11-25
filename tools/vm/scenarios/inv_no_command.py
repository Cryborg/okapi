"""Invariant 4: Okapi runs no command but pkexec on its helper, and the helper none.

auditd records every `execve` of the machine while the interface scans,
copies the uv and pnpm commands the user runs by hand, gets the rights,
cleans the crash reports, vacuums systemd's journal and has Slint's logo
clicked, then while the CLI cleans. Okapi's processes, followed from their own
start through their children, run pkexec with its fixed arguments and the
helper, nothing else, the window's frame and the logo's link included
(INV-401); the helper starts nothing, the journal vacuumed without any
`journalctl` (INV-402); uv and pnpm are never run (INV-403).
"""

from __future__ import annotations

import re
import shlex
from dataclasses import dataclass

from okapivm import cleaner, common, config
from okapivm.bench import Bench

TAGS = ["invariants", "admin", "helper", "audit"]

# Beyond the 30 days of the journal's vacuum and of the temporary folders' rules.
AGING_SECONDS = 31 * 24 * 60 * 60
SIZE = 4096
TMPFILES_TIMER = "systemd-tmpfiles-clean.timer"
AUDIT_KEY = "okapi-vm"
AUDIT_LOG = "/var/log/audit/audit.log"
PKEXEC = "/usr/bin/pkexec"
PKEXEC_ARGUMENTS = [PKEXEC, "--disable-internal-agent", config.helper_paths().helper]
OKAPI_PROGRAMS = {f"{config.GUEST_BINARIES}/okapi-app", f"{config.GUEST_BINARIES}/okapi"}
NEVER_RUN = {"journalctl", "uv", "pnpm"}
BY_HAND = {"uv cache prune", "pnpm store prune"}
GROUPS = [cleaner.CRASH_REPORTS, cleaner.JOURNAL_LOGS]
JOURNAL = "/var/log/journal"
# What the base image leaves to clean, excluded so that the groups this scenario
# clicks stay among the rows the virtualized list shows.
IMAGE_LEFTOVERS = ["/var/lib/flatpak/repo/tmp", "/var/cache/fontconfig", "/var/cache/man", "/var/log/apt"]
HOME_LEFTOVERS = [".cache/tracker3", ".cache/gstreamer-1.0", ".cache/mesa_shader_cache"]
FIELD = re.compile(r'(\w+)=("[^"]*"|\S+)')
ENCODED = re.compile(r"a\d+|name|exe|proctitle")
SERIAL = re.compile(r"msg=audit\([\d.]+:(\d+)\)")


@dataclass
class Exec:
    serial: int
    pid: int
    ppid: int
    success: bool
    executable: str
    arguments: list[str]
    names: list[str]

    def __str__(self) -> str:
        outcome = "" if self.success else " (failed)"
        return f"pid {self.pid} < {self.ppid}: {self.executable} {shlex.join(self.arguments)}{outcome}"


def unquoted(name: str, value: str) -> str:
    """An audit field: quoted text, or, for an argument or a path, hexadecimal when it holds a space or a quote."""
    if value.startswith('"'):
        return value.strip('"')
    if not ENCODED.fullmatch(name):
        return value
    try:
        return bytes.fromhex(value).decode(errors="replace")
    except ValueError:
        return value


def recorded_execs(text: str) -> list[Exec]:
    """Every `execve` the audit rule recorded in `text`, in order."""
    records: dict[int, dict[str, list[dict[str, str]]]] = {}
    for line in text.splitlines():
        serial = SERIAL.search(line)
        kind = line.split(" ", 1)[0].removeprefix("type=")
        if serial is None or kind not in {"SYSCALL", "EXECVE", "PATH"}:
            continue
        fields = {name: unquoted(name, value) for name, value in FIELD.findall(line)}
        records.setdefault(int(serial.group(1)), {}).setdefault(kind, []).append(fields)
    execs = []
    for serial, kinds in sorted(records.items()):
        calls = [call for call in kinds.get("SYSCALL", []) if call.get("key") == AUDIT_KEY]
        if not calls:
            continue
        call = calls[0]
        argv = kinds.get("EXECVE", [{}])[0]
        arguments = [argv[f"a{index}"] for index in range(int(argv.get("argc", 0))) if f"a{index}" in argv]
        execs.append(Exec(serial, int(call["pid"]), int(call["ppid"]), call.get("success") == "yes",
                          call.get("exe", ""), arguments, [path.get("name", "") for path in kinds.get("PATH", [])]))
    return execs


def okapi_tree(execs: list[Exec]) -> tuple[list[Exec], list[Exec]]:
    """Okapi's own starts, and every exec of a process below one of them."""
    tree: set[int] = set()
    starts, below = [], []
    for record in execs:
        if record.ppid in tree:
            below.append(record)
            tree.add(record.pid)
        elif record.success and record.executable in OKAPI_PROGRAMS:
            starts.append(record)
            tree.add(record.pid)
    return starts, below


def journal_archives(bench: Bench) -> set[str]:
    return set(bench.root(f"find {JOURNAL} -name '*@*.journal' -printf '%p\\n'").split())


def run(bench: Bench) -> None:
    user = bench.accounts.admin.name
    helper = config.helper_paths().helper
    bench.root(shlex.join(["systemctl", "stop", TMPFILES_TIMER]))
    bench.allow_elevation_without_password()
    exclusions = [*IMAGE_LEFTOVERS, *(f"/home/{user}/{path}" for path in HOME_LEFTOVERS)]
    common.write_settings(bench, {"exclusions": exclusions, "deletionLog": True})
    crash = common.Files(common.ROOT, ["/var/crash/okapi-vm.crash"], SIZE)
    thumbnail = common.Files(user, ["~/.cache/thumbnails/normal/okapi-vm.png"], SIZE)
    by_hand = common.Files(user, ["~/.cache/uv/okapi-vm/entry", "~/.local/share/pnpm/store/v10/okapi-vm/entry"], SIZE)
    common.plant(bench, crash, thumbnail, by_hand)
    bench.root(shlex.join(["journalctl", "--rotate"]))
    old_archives = journal_archives(bench)
    bench.check(bool(old_archives), f"systemd's journal has archives to vacuum ({len(old_archives)})")
    bench.advance_clock(AGING_SECONDS)
    bench.root(shlex.join(["auditctl", "-a", "always,exit", "-F", "arch=b64", "-S", "execve,execveat",
                           "-k", AUDIT_KEY]))
    offset = int(bench.root(shlex.join(["stat", "--format=%s", AUDIT_LOG])))

    cleaner.scan(bench)
    bench.ui.click("Replier Système", role="button")
    copied = set()
    for index in range(len(BY_HAND)):
        bench.ui.click("Copier", role="button", index=index)
        copied.add(bench.user("WAYLAND_DISPLAY=wayland-0 wl-paste --no-newline", check=False).strip())
    bench.check(copied == BY_HAND, f"INV-403: « Copier » puts the commands to run by hand in the clipboard ({copied})")
    bench.ui.click("Déplier Système", role="button")
    cleaner.obtain_rights(bench)
    cleaner.check_groups(bench, *GROUPS)
    cleaner.clean(bench)
    bench.screenshot("recu")
    common.open_screen(bench, "À propos")
    bench.click_at(*bench.ui.center(bench.ui.node("#MadeWithSlint")))
    bench.screenshot("a-propos")
    bench.stop_app()
    for_the_cli = common.Files(user, ["~/.cache/thumbnails/normal/okapi-vm-cli.png"], SIZE)
    common.plant(bench, for_the_cli, age_seconds=AGING_SECONDS)
    cleaned = common.cli(bench, "--clean")
    bench.check(cleaned.code == 0, f"the CLI cleaned ({cleaned.stderr.strip()})")

    text = bench.root(f"tail --bytes=+{offset + 1} {AUDIT_LOG}")
    execs = recorded_execs(text)
    starts, below = okapi_tree(execs)
    run_below = [record for record in below if record.success]
    bench.log(f"{len(execs)} execs recorded; Okapi started {len(starts)} times; run below it: "
              f"{[str(record) for record in run_below]}")
    bench.check(len(starts) >= len(OKAPI_PROGRAMS)
                and {record.executable for record in starts} == OKAPI_PROGRAMS,
                f"the audit saw the interface and the CLI start ({[str(record) for record in starts]})")
    pkexecs = [record for record in below if record.executable == PKEXEC]
    helpers = [record for record in below if record.executable == helper]
    bench.check(bool(pkexecs) and all(record.success and record.arguments == PKEXEC_ARGUMENTS for record in pkexecs),
                f"INV-401: Okapi runs pkexec with its fixed arguments ({[str(record) for record in pkexecs]})")
    bench.check(len(helpers) == len(pkexecs) and all(record.arguments == [helper] for record in helpers)
                and {record.pid for record in helpers} == {record.pid for record in pkexecs},
                f"INV-401: pkexec runs the helper alone ({[str(record) for record in helpers]})")
    helper_pids = {record.pid for record in helpers}
    bench.check(not [record for record in below if record.ppid in helper_pids],
                "INV-402: the helper starts no process")
    named = [record for record in execs
             if {name.rsplit("/", 1)[-1] for name in [record.executable, *record.arguments[:1], *record.names]}
             & NEVER_RUN]
    bench.check(not named, f"INV-402, INV-403: no journalctl, uv nor pnpm is run on the machine "
                           f"({[str(record) for record in named]})")

    receipt = common.last_receipt(bench, user)
    bench.log(f"receipt of the interface's cleaning: {common.read_store(bench, common.HISTORY)[-3:]}")
    vacuumed = old_archives - journal_archives(bench)
    bench.check(bool(vacuumed), f"INV-402: the helper vacuumed the old archives itself ({sorted(vacuumed)})")
    common.check_gone(bench, crash, for_the_cli)
    common.check_kept(bench, by_hand)
    bench.log(f"last receipt: {receipt}")

    others = [record for record in run_below if record not in pkexecs and record not in helpers]
    bench.check(not others, "INV-401: nothing else is run below Okapi, neither by the window's frame nor by "
                            f"Slint's logo ({[str(record) for record in others]})")
