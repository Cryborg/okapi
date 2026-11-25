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
GUEST_FAKE_BROWSER = BENCH / "guest" / "fake_browser.py"
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
# Each slot runs its own machine: three at 4 GB leave the 30 GB host room for
# cargo builds alongside.
SLOTS = range(1, 4)
SLOT_VARIABLE = "OKAPI_VM_SLOT"
SSH_PORT_BASE = 52200
MEMORY_MEGABYTES = 4096
PROCESSORS = 4
SCREEN_WIDTH = 1280
SCREEN_HEIGHT = 800

# Each the binary of the package of the same name; the interface first.
OKAPI_BINARIES = ("okapi-app", "okapi", "okapi-helper")

GUEST_DRIVER_PATH = "/usr/local/lib/okapi-vm/atspi_driver.py"
GUEST_FAKE_BROWSER_PATH = "/usr/local/lib/okapi-vm/fake_browser.py"
GUEST_BINARIES = "/usr/local/bin"
GUEST_APPLICATIONS = "/usr/share/applications"
GUEST_ICONS = "/usr/share/icons/hicolor/scalable/apps"
GUEST_POLKIT_RULE = "/etc/polkit-1/rules.d/49-okapi-vm.rules"
POLKIT_ACTION = "io.github.cryborg.okapi.clean"
# What uninstalling an application asks polkit for: PackageKit's removal,
# snapd's, and flatpak's system helper (its repository pruned after).
UNINSTALL_ACTIONS = (
    "org.freedesktop.packagekit.package-remove",
    "io.snapcraft.snapd.manage",
    "org.freedesktop.Flatpak.app-uninstall",
    "org.freedesktop.Flatpak.runtime-uninstall",
    "org.freedesktop.Flatpak.modify-repo",
)
UPDATE_ACTIONS = ("org.freedesktop.packagekit.system-update", "io.snapcraft.snapd.manage")
APP_UNIT = "okapi-app"
PROVISIONED_MARKER = "OKAPI-VM-PROVISIONED"


def bench_home() -> Path:
    if explicit := os.environ.get("OKAPI_VM_HOME"):
        return Path(explicit)
    cache = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(cache) / "okapi-vm"


@dataclass(frozen=True)
class Home:
    """The bench's directory outside the repository: image, base and key shared
    by every slot, each slot's machine, the runs, the coverage gathered."""

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

    def slot(self, number: int) -> Path:
        return self.root / "slots" / str(number)

    @property
    def runs(self) -> Path:
        return self.root / "runs"

    @property
    def coverage(self) -> Path:
        return self.root / "coverage"


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


CLEANER_SOURCE = WORKSPACE / "crates" / "app" / "src" / "cleaner.rs"
SECOND_CLICK_PATTERN = r"^const SECOND_CLICK: Duration = Duration::from_millis\((\d+)\);$"
CLICK_MARGIN_SECONDS = 0.2


def click_settle_seconds() -> float:
    """How long the driver lets a node stand before clicking it: the interface
    ignores a click that follows a change of screen by less than `SECOND_CLICK`
    (the second click of a double click), and a human never clicks that fast."""
    found = re.search(SECOND_CLICK_PATTERN, CLEANER_SOURCE.read_text(encoding="utf-8"), re.MULTILINE)
    if found is None:
        raise RuntimeError(f"{CLEANER_SOURCE} names no SECOND_CLICK in milliseconds")
    return int(found.group(1)) / 1000 + CLICK_MARGIN_SECONDS


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
