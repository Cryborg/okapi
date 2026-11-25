"""Download of the cloud image and its provisioning, once, into the base image."""

from __future__ import annotations

import hashlib
import json
import shutil
import subprocess
import time
from pathlib import Path

from . import config
from .machine import MachineError, check_kvm, free_port, fresh_firmware_variables, qemu_command, seed_drive

PROVISION_TIMEOUT_SECONDS = 90 * 60
PROGRESS_SECONDS = 30
HASH_CHUNK = 1 << 20
GUEST_PROVISION_SCRIPT = "/usr/local/sbin/okapi-vm-provision"

DCONF_PROFILE = """user-db:user
system-db:local
"""

# Accessibility on, so that AccessKit exposes Okapi's tree on AT-SPI; no
# blank screen, lock nor notification banner (« updates available »), which
# would hide the window from the captures.
DCONF_DEFAULTS = """[org/gnome/desktop/interface]
toolkit-accessibility=true

[org/gnome/desktop/notifications]
show-banners=false

[org/gnome/desktop/session]
idle-delay=uint32 0

[org/gnome/desktop/screensaver]
lock-enabled=false
"""


def fetch(home: config.Home) -> None:
    """Downloads the cloud image and its checksums, and keeps the image only if they agree."""
    if home.image.exists():
        print(f"{home.image} is already there, checked when it was downloaded.")
        return
    home.root.mkdir(parents=True, exist_ok=True)
    sums = home.root / config.IMAGE_SUMS_NAME
    partial = home.image.with_name(home.image.name + ".partial")
    subprocess.run(
        ["curl", "--fail", "--location", "--output", str(sums), f"{config.IMAGE_RELEASE_URL}/{config.IMAGE_SUMS_NAME}"],
        check=True,
    )
    subprocess.run(
        ["curl", "--fail", "--location", "--continue-at", "-", "--output", str(partial),
         f"{config.IMAGE_RELEASE_URL}/{config.IMAGE_NAME}"],
        check=True,
    )
    expected = expected_digest(sums)
    actual = digest(partial)
    if actual != expected:
        partial.unlink()
        raise MachineError(f"{config.IMAGE_NAME}: SHA-256 {actual}, {config.IMAGE_SUMS_NAME} says {expected}")
    partial.rename(home.image)
    print(f"{home.image} downloaded and checked.")


def expected_digest(sums: Path) -> str:
    for line in sums.read_text(encoding="utf-8").splitlines():
        value, _, name = line.partition(" ")
        if name.lstrip(" *") == config.IMAGE_NAME:
            return value
    raise MachineError(f"{sums} has no line for {config.IMAGE_NAME}")


def digest(path: Path) -> str:
    hashed = hashlib.sha256()
    with path.open("rb") as file:
        while chunk := file.read(HASH_CHUNK):
            hashed.update(chunk)
    return hashed.hexdigest()


def public_key(home: config.Home) -> str:
    if not home.key.exists():
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "okapi-vm", "-f", str(home.key)], check=True)
    return home.key.with_suffix(".pub").read_text(encoding="utf-8").strip()


def user_data(accounts: config.Accounts, key: str) -> str:
    """cloud-init's configuration, as JSON (which YAML reads): no quoting to get wrong."""
    def account(entry: config.Account, groups: list[str]) -> dict[str, object]:
        return {
            "name": entry.name,
            "groups": groups,
            "shell": "/bin/bash",
            "ssh_authorized_keys": [key],
        }

    configuration = {
        "hostname": "okapi-vm",
        "locale": "fr_FR.UTF-8",
        "timezone": "Europe/Paris",
        "keyboard": {"layout": "fr"},
        "disable_root": False,
        "ssh_authorized_keys": [key],
        "ssh_pwauth": False,
        "users": [account(accounts.admin, ["sudo"]), account(accounts.other, [])],
        "chpasswd": {
            "expire": False,
            "users": [
                {"name": entry.name, "password": entry.password, "type": "text"}
                for entry in (accounts.admin, accounts.other)
            ],
        },
        "write_files": [
            {"path": "/etc/dconf/profile/user", "content": DCONF_PROFILE},
            {"path": "/etc/dconf/db/local.d/00-okapi-vm", "content": DCONF_DEFAULTS},
            {
                "path": GUEST_PROVISION_SCRIPT,
                "permissions": "0755",
                "content": config.PROVISION_SOURCE.read_text(encoding="utf-8"),
            },
        ],
        "runcmd": [[GUEST_PROVISION_SCRIPT, accounts.admin.name, accounts.other.name, config.PROVISIONED_MARKER]],
        "power_state": {"mode": "poweroff", "condition": True},
    }
    return "#cloud-config\n" + json.dumps(configuration, indent=2) + "\n"


def prepare(home: config.Home, *, force: bool) -> None:
    """Provisions the base image from the cloud image; every run then starts from it."""
    check_kvm()
    if not home.image.exists():
        raise MachineError(f"{home.image} is missing: run `okapi-vm fetch` first (it downloads {config.IMAGE_SIZE})")
    if home.base.exists() and not force:
        print(f"{home.base} is already there; `prepare --force` builds it again.")
        return
    partial = home.base.with_name(home.base.name + ".partial")
    partial.unlink(missing_ok=True)
    subprocess.run(["qemu-img", "convert", "-O", "qcow2", str(home.image), str(partial)], check=True)
    subprocess.run(["qemu-img", "resize", "-q", str(partial), config.BASE_DISK_SIZE], check=True)
    serial_log = home.root / "prepare-serial.log"
    # Thrown away whatever happens: its user-data holds the test passwords.
    work = home.root / "prepare"
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir()
    try:
        provision(home, partial, work, serial_log)
    finally:
        shutil.rmtree(work)
    partial.rename(home.base)
    home.base.chmod(0o444)
    print(f"{home.base} is ready.")


def provision(home: config.Home, disk: Path, work: Path, serial_log: Path) -> None:
    """Boots `disk` once with cloud-init's seed, until the machine powers itself off."""
    seed = work / "seed"
    seed.mkdir()
    (seed / "user-data").write_text(user_data(config.read_accounts(), public_key(home)), encoding="utf-8")
    (seed / "meta-data").write_text(
        json.dumps({"instance-id": f"okapi-vm-{int(time.time())}", "local-hostname": "okapi-vm"}) + "\n",
        encoding="utf-8",
    )
    firmware_variables = work / "firmware-variables.fd"
    fresh_firmware_variables(firmware_variables)
    command = [*qemu_command(disk, firmware_variables, free_port(), serial_log, work / "qmp.sock"), *seed_drive(seed)]
    print(f"Provisioning {disk} (a few minutes); console in {serial_log}", flush=True)
    started = time.monotonic()
    with subprocess.Popen(command) as qemu:
        while qemu.poll() is None:
            elapsed = time.monotonic() - started
            if elapsed > PROVISION_TIMEOUT_SECONDS:
                qemu.kill()
                raise MachineError(f"provisioning still running after {elapsed:.0f} s; see {serial_log}")
            time.sleep(PROGRESS_SECONDS)
            print(f"  {time.monotonic() - started:.0f} s", flush=True)
    console = serial_log.read_text(encoding="utf-8", errors="replace").splitlines()
    # A whole line: the script's own trace (`set -x`) names the marker too.
    if qemu.returncode != 0 or config.PROVISIONED_MARKER not in (line.strip() for line in console):
        raise MachineError(f"provisioning failed (QEMU {qemu.returncode}); see {serial_log}")
