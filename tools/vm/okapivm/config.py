"""Where the bench keeps its files, on the host and in the machine, and what it downloads."""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path

WORKSPACE = Path(__file__).resolve().parents[3]
BENCH = WORKSPACE / "tools" / "vm"
SCENARIOS = BENCH / "scenarios"
GUEST_DRIVER = BENCH / "guest" / "atspi_driver.py"
PROVISION_SOURCE = BENCH / "guest" / "provision.sh"
ACCOUNTS_FILE = BENCH / "test-accounts.env"
INSTALL_HELPER_SCRIPT = WORKSPACE / "packaging" / "linux" / "install-helper.sh"
POLICY_SOURCE = WORKSPACE / "packaging" / "linux" / "io.github.cryborg.okapi.policy"
DESKTOP_ENTRY_SOURCE = WORKSPACE / "packaging" / "linux" / "io.github.cryborg.Okapi.desktop"
ICON_SOURCE = WORKSPACE / "crates" / "app" / "assets" / "okapi-icon.svg"

IMAGE_RELEASE_URL = "https://cloud-images.ubuntu.com/releases/resolute/release-20261108"
IMAGE_NAME = "ubuntu-26.04-server-cloudimg-amd64.img"
IMAGE_SUMS_NAME = "SHA256SUMS"
IMAGE_SIZE = "862 MB"

FIRMWARE_CODE = Path("/usr/share/OVMF/OVMF_CODE_4M.fd")
FIRMWARE_VARIABLES = Path("/usr/share/OVMF/OVMF_VARS_4M.fd")

BASE_DISK_SIZE = "40G"
MEMORY_MEGABYTES = 4096
PROCESSORS = 4
SCREEN_WIDTH = 1280
SCREEN_HEIGHT = 800

GUEST_DRIVER_PATH = "/usr/local/lib/okapi-vm/atspi_driver.py"
GUEST_BINARIES = "/usr/local/bin"
GUEST_APPLICATIONS = "/usr/share/applications"
GUEST_ICONS = "/usr/share/icons/hicolor/scalable/apps"
GUEST_POLKIT_RULE = "/etc/polkit-1/rules.d/49-okapi-vm.rules"
POLKIT_ACTION = "io.github.cryborg.okapi.clean"
APP_UNIT = "okapi-app"
PROVISIONED_MARKER = "OKAPI-VM-PROVISIONED"


def bench_home() -> Path:
    if explicit := os.environ.get("OKAPI_VM_HOME"):
        return Path(explicit)
    cache = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(cache) / "okapi-vm"


@dataclass(frozen=True)
class Home:
    """The bench's directory outside the repository: image, base, key, running machine, runs."""

    root: Path

    @property
    def image(self) -> Path:
        return self.root / IMAGE_NAME

    @property
    def base(self) -> Path:
        return self.root / "base.qcow2"

    @property
    def key(self) -> Path:
        return self.root / "id_ed25519"

    @property
    def machine(self) -> Path:
        return self.root / "machine"

    @property
    def runs(self) -> Path:
        return self.root / "runs"


@dataclass(frozen=True)
class Account:
    name: str
    password: str


@dataclass(frozen=True)
class Accounts:
    """The machine's two human accounts: an administrator (the one logged in) and another."""

    admin: Account
    other: Account


def read_accounts() -> Accounts:
    values = dict(
        line.split("=", 1)
        for line in ACCOUNTS_FILE.read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#")
    )
    return Accounts(
        admin=Account(values["OKAPI_VM_ADMIN_USER"], values["OKAPI_VM_ADMIN_PASSWORD"]),
        other=Account(values["OKAPI_VM_OTHER_USER"], values["OKAPI_VM_OTHER_PASSWORD"]),
    )


@dataclass(frozen=True)
class HelperPaths:
    helper: str
    policy: str


def helper_paths() -> HelperPaths:
    """The helper's and the policy's installed paths, as `install-helper.sh` names them."""
    script = INSTALL_HELPER_SCRIPT.read_text(encoding="utf-8")

    def value(name: str) -> str:
        found = re.search(rf"^readonly {name}=(\S+)$", script, re.MULTILINE)
        if found is None:
            raise RuntimeError(f"{INSTALL_HELPER_SCRIPT} names no {name}")
        return found.group(1)

    return HelperPaths(helper=value("HELPER"), policy=value("POLICY"))
