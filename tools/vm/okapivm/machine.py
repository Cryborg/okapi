"""The QEMU machine of a slot: its command line, its QMP socket and SSH into it."""

from __future__ import annotations

import contextlib
import json
import os
import shutil
import signal
import socket
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

from . import config

MACHINE_NAME = "okapi-vm"
SSH_POLL_SECONDS = 2
SSH_CONNECT_TIMEOUT_SECONDS = 5
# A frozen guest under a live QEMU keeps the TCP session open: without these,
# an SSH command would wait for it forever.
SSH_ALIVE_INTERVAL_SECONDS = 5
SSH_ALIVE_COUNT = 3
SSH_COMMAND_TIMEOUT_SECONDS = 300
SSH_CONTROL_PERSIST_SECONDS = 120
QUIT_TIMEOUT_SECONDS = 30
QUIT_POLL_SECONDS = 0.2
QMP_TIMEOUT_SECONDS = 30
ABSOLUTE_MAXIMUM = 0x7FFF


class MachineError(RuntimeError):
    pass


def fresh_firmware_variables(path: Path) -> None:
    """The UEFI variables a machine starts from, the firmware's own defaults."""
    shutil.copyfile(config.FIRMWARE_VARIABLES, path)


def qemu_command(name: str, disk: Path, firmware_variables: Path, ssh_port: int, serial_log: Path,
                 qmp_socket: Path) -> list[str]:
    """UEFI, the main disk first: under SeaBIOS, the 26.04 cloud image stopped at « Booting from Hard Disk »."""
    return [
        "qemu-system-x86_64",
        "-name", name,
        "-machine", "q35,accel=kvm",
        "-cpu", "host",
        "-smp", str(config.PROCESSORS),
        "-m", str(config.MEMORY_MEGABYTES),
        "-drive", f"if=pflash,format=raw,readonly=on,file={config.FIRMWARE_CODE}",
        "-drive", f"if=pflash,format=raw,file={firmware_variables}",
        "-drive", f"file={disk},if=none,id=disk,format=qcow2,discard=unmap",
        "-device", "virtio-blk-pci,drive=disk,bootindex=0",
        "-nic", f"user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:{ssh_port}-:22",
        "-device", f"virtio-vga,xres={config.SCREEN_WIDTH},yres={config.SCREEN_HEIGHT}",
        "-device", "virtio-rng-pci",
        # An absolute pointer: QMP then clicks at a place of the screen.
        "-device", "virtio-tablet-pci",
        "-display", "none",
        "-serial", f"file:{serial_log}",
        "-qmp", f"unix:{qmp_socket},server=on,wait=off",
    ]


def seed_drive(seed_directory: Path) -> list[str]:
    """A read-only FAT drive labelled as cloud-init's NoCloud seed: no ISO tool needed."""
    return [
        "-blockdev", f"driver=vvfat,node-name=seed,dir={seed_directory},label=CIDATA,read-only=on",
        "-device", "virtio-blk-pci,drive=seed",
    ]


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def check_kvm() -> None:
    if not os.access("/dev/kvm", os.R_OK | os.W_OK):
        raise MachineError("/dev/kvm cannot be opened: is this account in the kvm group?")


class Qmp:
    """QEMU's machine protocol, one command at a time on its Unix socket."""

    def __init__(self, path: Path) -> None:
        self._socket = socket.socket(socket.AF_UNIX)
        self._socket.settimeout(QMP_TIMEOUT_SECONDS)
        self._socket.connect(str(path))
        self._lines = self._socket.makefile("r", encoding="utf-8")
        self._read()
        self.execute("qmp_capabilities")

    def _read(self) -> dict:
        line = self._lines.readline()
        if not line:
            raise MachineError("QEMU closed its QMP socket")
        return json.loads(line)

    def execute(self, command: str, **arguments: object) -> object:
        message: dict[str, object] = {"execute": command}
        if arguments:
            message["arguments"] = arguments
        self._socket.sendall(json.dumps(message).encode() + b"\n")
        while True:
            answer = self._read()
            if "return" in answer:
                return answer["return"]
            if "error" in answer:
                raise MachineError(f"QMP {command}: {answer['error'].get('desc')}")

    def close(self) -> None:
        self._lines.close()
        self._socket.close()


@dataclass(frozen=True)
class Machine:
    """A slot's machine, known by the files of its directory. The SSH port is
    the slot's own, fixed: two machines starting together never race for one."""

    home: config.Home
    slot: int

    @property
    def name(self) -> str:
        return f"{MACHINE_NAME}-{self.slot}"

    @property
    def directory(self) -> Path:
        return self.home.slot(self.slot) / "machine"

    @property
    def overlay(self) -> Path:
        return self.directory / "overlay.qcow2"

    @property
    def qmp_socket(self) -> Path:
        return self.directory / "qmp.sock"

    @property
    def pid_file(self) -> Path:
        return self.directory / "qemu.pid"

    @property
    def serial_log(self) -> Path:
        return self.directory / "serial.log"

    @property
    def ssh_port(self) -> int:
        return config.SSH_PORT_BASE + self.slot

    def pid(self) -> int | None:
        """QEMU's process, only if it is still this machine's: after a crash or
        a reboot of the host, the pid file may name someone else's process."""
        try:
            pid = int(self.pid_file.read_text(encoding="utf-8"))
            arguments = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
        except FileNotFoundError:
            return None
        if b"-pidfile" not in arguments or str(self.pid_file).encode() not in arguments:
            return None
        return pid

    def start(self) -> None:
        """Boots a throwaway overlay over the base image; nothing it writes reaches the base."""
        check_kvm()
        if not self.home.base.exists():
            raise MachineError(f"{self.home.base} is missing: run `okapi-vm prepare` first")
        if self.pid() is not None:
            raise MachineError(f"slot {self.slot} already runs a machine: `okapi-vm stop --slot {self.slot}` first")
        self.remove_files()
        self.directory.mkdir(parents=True)
        subprocess.run(
            ["qemu-img", "create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", str(self.home.base), str(self.overlay)],
            check=True,
        )
        firmware_variables = self.directory / "firmware-variables.fd"
        fresh_firmware_variables(firmware_variables)
        command = qemu_command(self.name, self.overlay, firmware_variables, self.ssh_port, self.serial_log,
                               self.qmp_socket)
        subprocess.run([*command, "-daemonize", "-pidfile", str(self.pid_file)], check=True)

    def qmp(self) -> Qmp:
        return Qmp(self.qmp_socket)

    def _execute(self, command: str, **arguments: object) -> object:
        qmp = self.qmp()
        try:
            return qmp.execute(command, **arguments)
        finally:
            qmp.close()

    def screendump(self, path: Path) -> None:
        self._execute("screendump", filename=str(path), format="png")

    def send_key(self, qcodes: list[str]) -> None:
        """Presses the keys together, then releases them: a keyboard's keys, whatever the guest's layout."""
        self._execute("send-key", keys=[{"type": "qcode", "data": qcode} for qcode in qcodes])

    def click_at(self, x: int, y: int) -> None:
        """A left click at a pixel of the screen, through the absolute pointer."""
        def axis(name: str, pixel: int, size: int) -> dict[str, object]:
            return {"type": "abs", "data": {"axis": name, "value": pixel * ABSOLUTE_MAXIMUM // (size - 1)}}

        def button(down: bool) -> dict[str, object]:
            return {"type": "btn", "data": {"down": down, "button": "left"}}

        self._execute("input-send-event", events=[axis("x", x, config.SCREEN_WIDTH),
                                                  axis("y", y, config.SCREEN_HEIGHT)])
        self._execute("input-send-event", events=[button(True)])
        self._execute("input-send-event", events=[button(False)])

    def _ssh_options(self) -> list[str]:
        return [
            "-i", str(self.home.key),
            "-o", "BatchMode=yes",
            "-o", "IdentitiesOnly=yes",
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR",
            "-o", f"ConnectTimeout={SSH_CONNECT_TIMEOUT_SECONDS}",
            "-o", f"ServerAliveInterval={SSH_ALIVE_INTERVAL_SECONDS}",
            "-o", f"ServerAliveCountMax={SSH_ALIVE_COUNT}",
            "-o", "ControlMaster=auto",
            "-o", f"ControlPath={self.directory}/ssh-%r",
            "-o", f"ControlPersist={SSH_CONTROL_PERSIST_SECONDS}",
        ]

    def ssh(self, user: str, command: str, *, check: bool = True,
            timeout: float = SSH_COMMAND_TIMEOUT_SECONDS) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            ["ssh", *self._ssh_options(), "-p", str(self.ssh_port), f"{user}@127.0.0.1", "--", command],
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
        if check and result.returncode != 0:
            raise MachineError(
                f"`{command}` as {user} ended with {result.returncode}: {result.stderr.strip() or result.stdout.strip()}"
            )
        return result

    def copy_to(self, user: str, sources: list[Path], destination: str) -> None:
        subprocess.run(
            ["scp", "-q", *self._ssh_options(), "-P", str(self.ssh_port), *map(str, sources),
             f"{user}@127.0.0.1:{destination}"],
            check=True,
            timeout=SSH_COMMAND_TIMEOUT_SECONDS,
        )

    def copy_from(self, user: str, source: str, destination: Path) -> None:
        subprocess.run(
            ["scp", "-q", "-r", *self._ssh_options(), "-P", str(self.ssh_port), f"{user}@127.0.0.1:{source}",
             str(destination)],
            capture_output=True,
            text=True,
            check=True,
            timeout=SSH_COMMAND_TIMEOUT_SECONDS,
        )

    def wait_for_ssh(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.pid() is None:
                raise MachineError(f"QEMU stopped while booting; see {self.serial_log}")
            if self.ssh("root", "true", check=False).returncode == 0:
                return
            time.sleep(SSH_POLL_SECONDS)
        raise MachineError(f"no SSH after {timeout:.0f} s; see {self.serial_log}")

    def destroy(self) -> None:
        """Stops QEMU at once and removes the overlay: the next start is from the base again."""
        pid = self.pid()
        if pid is not None:
            try:
                qmp = self.qmp()
                qmp.execute("quit")
                qmp.close()
            except (OSError, MachineError):
                with contextlib.suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGTERM)
            deadline = time.monotonic() + QUIT_TIMEOUT_SECONDS
            while self.pid() is not None:
                if time.monotonic() > deadline:
                    raise MachineError(f"QEMU {pid} does not stop")
                time.sleep(QUIT_POLL_SECONDS)
        self.remove_files()

    def remove_files(self) -> None:
        if not self.directory.exists():
            return
        # The SSH masters remove their own sockets as QEMU goes.
        for entry in self.directory.iterdir():
            entry.unlink(missing_ok=True)
        self.directory.rmdir()
