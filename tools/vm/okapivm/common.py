"""What the scenarios share: files planted and looked for, the sidebar's screens,
the last receipt of the history, the CLI run in the machine."""

from __future__ import annotations

import json
import shlex
from collections.abc import Sequence
from dataclasses import dataclass

from . import config
from .bench import ROOT, Bench, Elevation, Outcome, ScenarioFailure

__all__ = ["ROOT", "Elevation", "Files", "Outcome", "Package", "bind_mount", "build_deb", "check_gone", "check_kept",
           "cli", "cli_json", "close_fake_browser", "enable_pam_tmpdir", "expand_home", "install_deb", "journal", "last_receipt",
           "mount_fat", "open_fake_browser", "open_screen", "plant", "present", "publish_apt_repository",
           "read_store", "shell_path", "store_path", "unmount", "write_settings", "write_store"]

HOME_PREFIX = "~/"
SELECTED = "selected"
JSON_OPTION = "--json"
JSON_LINES = ".jsonl"
HISTORY = "history.jsonl"
SETTINGS = "settings.json"
# `SETTINGS_VERSION` of crates/store/src/settings.rs.
SETTINGS_VERSION = 1
CONFIG_DIRECTORY = "~/.config/okapi"
STATE_DIRECTORY = "~/.local/state/okapi"
# As `okapi-store` names them (crates/store/src/journal.rs).
APP_JOURNAL = "okapi-app.log"
# As `okapi-store` places them (crates/store/src/lib.rs).
STORE_FILES = {
    SETTINGS: CONFIG_DIRECTORY,
    "dashboard.json": CONFIG_DIRECTORY,
    HISTORY: STATE_DIRECTORY,
    "last-analysis.json": STATE_DIRECTORY,
    "interface.json": STATE_DIRECTORY,
    "selections.json": STATE_DIRECTORY,
    "seen-apps.json": STATE_DIRECTORY,
}
PAM_TMPDIR_PROFILE = "tmpdir"
MOUNT_IMAGES = "/var/tmp/okapi-vm-mounts"
FAT_SIZE_MEGABYTES = 64
FAT_LABEL = "OKAPIVM"
PACKAGES = "/var/tmp/okapi-vm-packages"
APT_REPOSITORY = "/srv/okapi-vm-apt"
APT_GNUPG = "/root/.okapi-vm-gnupg"
APT_KEY_NAME = "okapi-vm <okapi-vm@localhost>"
APT_KEYRING = "/etc/apt/keyrings/okapi-vm.gpg"
APT_SOURCE = "/etc/apt/sources.list.d/okapi-vm.list"
FAKE_BROWSER_UNIT = "okapi-vm-browser"
FAKE_BROWSER_STUBBORN = "--stubborn"
FAKE_BROWSER_STUBBORN_KILL = "SIGKILL"
FAKE_BROWSER_POLLS = 50
FAKE_BROWSER_POLL_SECONDS = 0.1
# The sidebar's pages that sit in a group, which may be closed.
SIDEBAR_GROUPS = {
    "Nettoyeur": "Nettoyage",
    "Espace disque": "Nettoyage",
    "Applications": "Logiciels",
    "Mises à jour": "Logiciels",
    "Doublons": "Stockage",
    "Gros fichiers": "Stockage",
}


def shell_path(path: str) -> str:
    """`path` for the shell of its account, a leading `~/` its home, the rest quoted."""
    if path.startswith(HOME_PREFIX):
        return '"$HOME"/' + shlex.quote(path.removeprefix(HOME_PREFIX))
    return shlex.quote(path)


def expand_home(bench: Bench, path: str, account: str | None = None) -> str:
    """`path` with a leading `~/` made the account's home (the administrator's by
    default), as the settings want it: absolute."""
    if not path.startswith(HOME_PREFIX):
        return path
    home = bench.run_as(account or bench.accounts.admin.name, 'printf %s "$HOME"')
    return f"{home}/{path.removeprefix(HOME_PREFIX)}"


@dataclass(frozen=True)
class Files:
    """Files of one account (a name of `bench.accounts`, or `ROOT`), all of one size.
    A path is absolute or starts with `~/`, that account's home."""

    account: str
    paths: Sequence[str]
    size: int = 0


def plant(bench: Bench, *groups: Files, age_seconds: int = 0) -> None:
    """Writes random content in each file, its folders made, as its account; then,
    with `age_seconds`, moves the clock forward: the only way to age a file,
    since its change time cannot be set back."""
    for files in groups:
        lines = [
            f"mkdir -p -- \"$(dirname -- {shell_path(path)})\" && head -c {files.size} /dev/urandom > {shell_path(path)}"
            for path in files.paths
        ]
        bench.run_as(files.account, " && ".join(lines))
        bench.log(f"planted {len(files.paths)} files of {files.size} bytes as {files.account}")
    if age_seconds:
        bench.advance_clock(age_seconds)


def present(bench: Bench, files: Files) -> list[str]:
    """Those of `files` still there; a failed command fails, it never reads as « gone »."""
    if not files.paths:
        return []
    tests = [f'if [ -e {shell_path(path)} ]; then printf "%s\\n" {shlex.quote(path)}; fi' for path in files.paths]
    return bench.run_as(files.account, "; ".join(tests)).splitlines()


def check_gone(bench: Bench, *groups: Files) -> None:
    left = [path for files in groups for path in present(bench, files)]
    bench.check(not left, f"the files are gone (left: {left})")


def check_kept(bench: Bench, *groups: Files) -> None:
    expected = [path for files in groups for path in files.paths]
    kept = [path for files in groups for path in present(bench, files)]
    missing = [path for path in expected if path not in kept]
    bench.check(not missing, f"the files are still there (missing: {missing})")


def open_screen(bench: Bench, page: str) -> None:
    """Goes to a page of the sidebar by its name, opening its group if it is closed."""
    group = SIDEBAR_GROUPS.get(page)
    if group is not None and not bench.ui.find(page, role="button"):
        bench.ui.click(group, role="button")
    bench.ui.click(page, role="button")
    bench.ui.wait(page, role="button", state=SELECTED)


def store_path(name: str) -> str:
    """Where Okapi keeps a file of its store, under the account's home."""
    if name not in STORE_FILES:
        raise ScenarioFailure(f"no store file {name!r}: one of {', '.join(STORE_FILES)}")
    return f"{STORE_FILES[name]}/{name}"


def read_store(bench: Bench, name: str, account: str | None = None) -> object | None:
    """A store file read, None when absent; `history.jsonl` as the list of its lines."""
    path = shell_path(store_path(name))
    text = bench.run_as(account or bench.accounts.admin.name, f"if [ -e {path} ]; then cat -- {path}; fi")
    if not text.strip():
        return None
    if name.endswith(JSON_LINES):
        return [json.loads(line) for line in text.splitlines() if line.strip()]
    return json.loads(text)


def write_store(bench: Bench, name: str, value: object, account: str | None = None) -> None:
    """Replaces a store file, as Okapi writes it (whole, then renamed); `history.jsonl` from a list."""
    lines = value if name.endswith(JSON_LINES) and isinstance(value, list) else [value]
    text = "".join(json.dumps(line, ensure_ascii=False) + "\n" for line in lines)
    path = shell_path(store_path(name))
    partial = shell_path(f"{store_path(name)}.okapi-vm")
    bench.run_as(account or bench.accounts.admin.name,
                 f'mkdir -p -- "$(dirname -- {path})" && printf %s {shlex.quote(text)} > {partial} '
                 f"&& mv -- {partial} {path}")
    bench.log(f"wrote {store_path(name)}")


def write_settings(bench: Bench, values: dict, account: str | None = None) -> None:
    """Okapi's settings replaced by `values` (camelCase keys, `minAgeMinutes`...), in the format this Okapi reads."""
    write_store(bench, SETTINGS, {"version": SETTINGS_VERSION, **values}, account)


def last_receipt(bench: Bench, account: str | None = None) -> dict:
    """The newest entry of the account's history (the administrator's by default)."""
    history = read_store(bench, HISTORY, account)
    if not history:
        raise ScenarioFailure(f"{store_path(HISTORY)} holds no receipt")
    assert isinstance(history, list)
    return history[-1]


def journal(bench: Bench, binary: str = APP_JOURNAL, account: str | None = None) -> list[dict]:
    """The events of a binary's journal (`okapi-app.log` or `okapi.log`), oldest first."""
    path = shell_path(f"{STATE_DIRECTORY}/{binary}")
    text = bench.run_as(account or bench.accounts.admin.name, f"if [ -e {path} ]; then cat -- {path}; fi")
    return [json.loads(line) for line in text.splitlines() if line.strip()]


def cli(bench: Bench, *arguments: str, account: str | None = None) -> Outcome:
    """Runs `okapi` in the machine as `account` (the administrator by default), whatever its exit code."""
    who = account or bench.accounts.admin.name
    environment = [f"{name}={value}" for name, value in bench.okapi_environment().items()]
    outcome = bench.execute(who, shlex.join(["env", *environment, f"{config.GUEST_BINARIES}/okapi", *arguments]))
    bench.log(f"okapi {shlex.join(arguments)} as {who}: exit {outcome.code}")
    return outcome


def cli_json(bench: Bench, *arguments: str, account: str | None = None, code: int = 0) -> dict:
    """`okapi --json` with `arguments`, its document read; another exit code than `code` fails."""
    run = cli(bench, JSON_OPTION, *arguments, account=account)
    if run.code != code:
        raise ScenarioFailure(f"okapi exited {run.code}, not {code}: {run.stderr.strip()}")
    return json.loads(run.stdout)


def enable_pam_tmpdir(bench: Bench) -> None:
    """Turns pam_tmpdir on, as Debian installs it, and restarts the session:
    each session's `TMPDIR` is then `/tmp/user/<uid>`."""
    bench.root(shlex.join(["pam-auth-update", "--enable", PAM_TMPDIR_PROFILE]))
    bench.restart_session()


def mount_fat(bench: Bench, mountpoint: str, *, size_megabytes: int = FAT_SIZE_MEGABYTES,
              owner: str | None = None) -> None:
    """A FAT file system on a loop device, mounted for `owner` (the administrator by default)."""
    who = owner or bench.accounts.admin.name
    image = f"{MOUNT_IMAGES}/{mountpoint.strip('/').replace('/', '-')}.img"
    bench.root(" && ".join([
        shlex.join(["install", "-d", MOUNT_IMAGES]),
        shlex.join(["truncate", f"--size={size_megabytes}M", image]),
        shlex.join(["mkfs.vfat", "-n", FAT_LABEL, image]),
        shlex.join(["install", "-d", "--owner", who, "--group", who, mountpoint]),
        f"mount -o loop,uid=$(id -u {shlex.quote(who)}),gid=$(id -g {shlex.quote(who)}) "
        f"{shlex.quote(image)} {shlex.quote(mountpoint)}",
    ]))
    bench.log(f"FAT of {size_megabytes} MB mounted on {mountpoint} for {who}")


def bind_mount(bench: Bench, source: str, mountpoint: str, *, fuse: bool = False) -> None:
    """`source` seen again at `mountpoint`, by the kernel (`mount --bind`) or through FUSE (`bindfs`)."""
    command = ["bindfs", source, mountpoint] if fuse else ["mount", "--bind", source, mountpoint]
    bench.root(f"{shlex.join(['install', '-d', mountpoint])} && {shlex.join(command)}")
    bench.log(f"{source} {'bindfs' if fuse else 'bind'}-mounted on {mountpoint}")


def unmount(bench: Bench, mountpoint: str) -> None:
    bench.root(shlex.join(["umount", mountpoint]))
    bench.log(f"{mountpoint} unmounted")


@dataclass(frozen=True)
class Package:
    """A Debian package made in the machine: an executable that waits, and a launcher if `application`."""

    name: str
    version: str = "1.0"
    application: bool = True

    @property
    def deb(self) -> str:
        return f"{PACKAGES}/{self.name}_{self.version}_all.deb"


def build_deb(bench: Bench, package: Package) -> str:
    """Builds `package` in the machine and gives the `.deb`'s path there."""
    root = f"{PACKAGES}/{package.name}_{package.version}"
    control = (f"Package: {package.name}\nVersion: {package.version}\nArchitecture: all\n"
               f"Maintainer: okapi-vm <okapi-vm@localhost>\nDescription: {package.name}, made by the test bench\n")
    files = {
        f"{root}/DEBIAN/control": control,
        f"{root}/usr/bin/{package.name}": "#!/bin/sh\nexec sleep infinity\n",
    }
    if package.application:
        files[f"{root}/usr/share/applications/{package.name}.desktop"] = (
            f"[Desktop Entry]\nType=Application\nName={package.name}\nExec=/usr/bin/{package.name}\n"
        )
    writes = [f"install -D --mode=0644 /dev/stdin {shlex.quote(path)} <<'EOF'\n{text}EOF"
              for path, text in files.items()]
    bench.root("\n".join([
        *writes,
        shlex.join(["chmod", "0755", f"{root}/usr/bin/{package.name}"]),
        shlex.join(["dpkg-deb", "--root-owner-group", "--build", root, package.deb]),
    ]))
    bench.log(f"built {package.deb}")
    return package.deb


def install_deb(bench: Bench, package: Package) -> None:
    bench.root(shlex.join(["apt-get", "install", "--yes", "--no-install-recommends", package.deb]))
    bench.log(f"installed {package.name} {package.version}")


def publish_apt_repository(bench: Bench, *packages: Package) -> None:
    """A local APT repository signed by a key of the machine, holding `packages`
    (built already), which APT and PackageKit then offer as updates or installs."""
    bench.root("\n".join([
        "set -e",
        shlex.join(["install", "-d", APT_REPOSITORY]),
        shlex.join(["cp", *(package.deb for package in packages), APT_REPOSITORY]),
        f"cd {shlex.quote(APT_REPOSITORY)}",
        "dpkg-scanpackages --multiversion . /dev/null > Packages",
        "apt-ftparchive release . > Release",
        f"export GNUPGHOME={shlex.quote(APT_GNUPG)}",
        f"if [ ! -d {shlex.quote(APT_GNUPG)} ]; then install -d --mode=0700 {shlex.quote(APT_GNUPG)}; "
        f"gpg --batch --pinentry-mode loopback --passphrase '' --quick-generate-key {shlex.quote(APT_KEY_NAME)} default sign never; fi",
        "rm -f InRelease",
        "gpg --batch --clearsign --output InRelease Release",
        f"gpg --batch --yes --export --output {shlex.quote(APT_KEYRING)} {shlex.quote(APT_KEY_NAME)}",
        f"echo {shlex.quote(f'deb [signed-by={APT_KEYRING}] file:{APT_REPOSITORY} ./')} > {shlex.quote(APT_SOURCE)}",
        "apt-get update",
    ]))
    bench.log(f"APT repository {APT_REPOSITORY}: {', '.join(f'{p.name} {p.version}' for p in packages)}")


def open_fake_browser(bench: Bench, *, firefox_profile: str | None = None,
                      chromium_profile: str | None = None, stubborn: bool = False) -> None:
    """A process of the administrator account that holds a browser's profile as
    open: Firefox's `.parentlock` (its name on Linux) locked (`/proc/locks`), Chromium's
    `SingletonLock` naming it; `stubborn`, it ignores SIGTERM.
    `close_fake_browser` ends it, stubborn or not."""
    profiles = [f"firefox={firefox_profile}"] if firefox_profile else []
    profiles += [f"chromium={chromium_profile}"] if chromium_profile else []
    unit = ["systemd-run", "--user", "--quiet", f"--unit={FAKE_BROWSER_UNIT}", "--collect"]
    if stubborn:
        unit.append(f"--property=KillSignal={FAKE_BROWSER_STUBBORN_KILL}")
        profiles.insert(0, FAKE_BROWSER_STUBBORN)
    bench.user(shlex.join([*unit, "python3", "-I", config.GUEST_FAKE_BROWSER_PATH, *profiles]))
    # SingletonLock points at no file: `-e` would follow it and find nothing.
    held = [f"[ {test} {shell_path(path + marker)} ]" for path, marker, test in
            ((firefox_profile, "/.parentlock", "-e"), (chromium_profile, "/SingletonLock", "-L")) if path]
    bench.user(f"for attempt in $(seq {FAKE_BROWSER_POLLS}); do if {' && '.join(held)}; then exit 0; fi; "
               f"sleep {FAKE_BROWSER_POLL_SECONDS}; done; exit 1")
    bench.log(f"fake browser holds {', '.join(profiles)}")


def close_fake_browser(bench: Bench) -> None:
    bench.user(shlex.join(["systemctl", "--user", "stop", FAKE_BROWSER_UNIT]))
    bench.log("fake browser closed")
