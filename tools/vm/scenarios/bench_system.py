"""Bench: what the scenarios share outside the interface, each tried once.

Exit codes and outputs kept apart, the second account, Okapi's store, the
CLI, polkit's refusal, FAT and bind mounts, packages and an APT repository, a local snap,
a browser holding its profiles, then pam_tmpdir after a new session.
"""

from __future__ import annotations

import shlex

from okapivm import common, config
from okapivm.bench import Bench

TAGS = ["bench"]

FAILING_CODE = 3
NOT_AUTHORIZED = 127
FAT_MOUNTPOINT = "/mnt/okapi-vm-fat"
BIND_SOURCE = "/srv/okapi-vm-bind"
BIND_MOUNTPOINT = "/mnt/okapi-vm-bind"
FUSE_MOUNTPOINT = "/mnt/okapi-vm-bindfs"
FIREFOX_PROFILE = "~/.mozilla/firefox/okapi-vm.default"
CHROMIUM_PROFILE = "~/.config/chromium"
STORE_PROBE = "seen-apps.json"


def filesystem(bench: Bench, mountpoint: str) -> str:
    return bench.root(shlex.join(["findmnt", "--noheadings", "--output=FSTYPE", mountpoint])).strip()


def outputs_and_accounts(bench: Bench) -> None:
    outcome = bench.execute(bench.accounts.admin.name, f"echo out; echo err >&2; exit {FAILING_CODE}")
    bench.check((outcome.code, outcome.stdout, outcome.stderr) == (FAILING_CODE, "out\n", "err\n"),
                "a failing command gives its code, its output and its errors apart")
    bench.check(bench.other("id -un").strip() == bench.accounts.other.name, "a command runs as the second account")


def store(bench: Bench) -> None:
    bench.check(common.read_store(bench, STORE_PROBE) is None, f"{STORE_PROBE} is absent at first")
    value = {"okapi-vm": ["é", 1]}
    common.write_store(bench, STORE_PROBE, value)
    bench.check(common.read_store(bench, STORE_PROBE) == value, f"{STORE_PROBE} reads back as written")
    bench.user(f"rm -- {common.shell_path(common.store_path(STORE_PROBE))}")


def command_line(bench: Bench) -> None:
    analysis = common.cli_json(bench)
    bench.check(isinstance(analysis, dict) and bool(analysis), "okapi --json gives a document")
    wrong = common.cli(bench, "--okapi-vm", account=bench.accounts.other.name)
    bench.check(wrong.code != 0 and not wrong.stdout and bool(wrong.stderr),
                "an unknown option, as the second account, fails on the error output alone")


def refused_elevation(bench: Bench) -> None:
    bench.set_elevation(common.Elevation.REFUSED)
    helper = bench.execute(bench.accounts.admin.name, shlex.join(["pkexec", config.helper_paths().helper]))
    bench.check(helper.code == NOT_AUTHORIZED, f"polkit refuses the helper (pkexec exits {helper.code})")


def mounts(bench: Bench) -> None:
    common.mount_fat(bench, FAT_MOUNTPOINT)
    bench.check(filesystem(bench, FAT_MOUNTPOINT) == "vfat", "a FAT file system is mounted")
    common.plant(bench, common.Files(bench.accounts.admin.name, [f"{FAT_MOUNTPOINT}/okapi-vm.txt"], size=1))
    common.unmount(bench, FAT_MOUNTPOINT)
    bench.root(shlex.join(["install", "-d", BIND_SOURCE]))
    for mountpoint, fuse, kind in ((BIND_MOUNTPOINT, False, "ext4"), (FUSE_MOUNTPOINT, True, "fuse")):
        common.bind_mount(bench, BIND_SOURCE, mountpoint, fuse=fuse)
        found = filesystem(bench, mountpoint)
        bench.check(found.partition(".")[0] == kind, f"{mountpoint} is a {kind} mount ({found})")
        common.unmount(bench, mountpoint)


def packages(bench: Bench) -> None:
    first = common.Package("okapi-vm-demo")
    common.build_deb(bench, first)
    common.install_deb(bench, first)
    installed = bench.root("dpkg-query --show --showformat='${Version}' okapi-vm-demo")
    bench.check(installed == first.version, f"{first.name} {first.version} is installed")
    second = common.Package(first.name, version="2.0")
    common.build_deb(bench, second)
    common.publish_apt_repository(bench, second)
    upgradable = bench.root("apt list --upgradable 2>/dev/null")
    bench.check(f"{second.name}/" in upgradable and second.version in upgradable,
                f"APT offers {second.name} {second.version} from the local repository")
    dependent = common.Package("okapi-vm-dependent", application=False, hidden_launchers=("okapi-vm-hidden",),
                               fields=(("Depends", first.name), ("Section", "metapackages")))
    common.build_deb(bench, dependent)
    common.install_deb(bench, dependent)
    bench.check(common.deb_installed(bench, dependent.name)
                and first.name in bench.root(f"dpkg-query --show --showformat='${{Depends}}' {dependent.name}"),
                f"{dependent.name} is installed and depends on {first.name}")
    bench.check("NoDisplay=true" in bench.root("cat /usr/share/applications/okapi-vm-hidden.desktop"),
                "a package can carry a hidden launcher")


def snaps(bench: Bench) -> None:
    snap = common.Snap("okapi-vm-snap")
    common.build_snap(bench, snap)
    common.install_snap(bench, snap)
    common.install_snap(bench, snap)
    revisions = bench.root("snap list --all okapi-vm-snap")
    bench.check(common.snap_installed(bench, snap.name) and "x1" in revisions and "x2" in revisions,
                "a local snap installs, twice as two revisions, without the store")
    bench.check(bench.root("ls /var/lib/snapd/desktop/applications").count(f"{snap.name}_{snap.name}.desktop") == 1,
                "the snap's launcher is exported")


def browser(bench: Bench) -> None:
    bench.user(f"mkdir -p -- {common.shell_path(FIREFOX_PROFILE)} {common.shell_path(CHROMIUM_PROFILE)}")
    common.open_fake_browser(bench, firefox_profile=FIREFOX_PROFILE, chromium_profile=CHROMIUM_PROFILE)
    lock = common.shell_path(f"{FIREFOX_PROFILE}/.parentlock")
    inode = bench.user(f"stat --format=%i -- {lock}").strip()
    bench.check(f":{inode} " in bench.user("cat /proc/locks"), "Firefox's .parentlock is in /proc/locks")
    target = bench.user(f"readlink -- {common.shell_path(f'{CHROMIUM_PROFILE}/SingletonLock')}").strip()
    host = bench.user("hostname").strip()
    bench.check(target.startswith(f"{host}-"), f"Chromium's SingletonLock names a process ({target})")
    common.close_fake_browser(bench)
    bench.check(f":{inode} " not in bench.user("cat /proc/locks"), "the lock goes with the browser")


def private_temporary(bench: Bench) -> None:
    common.enable_pam_tmpdir(bench)
    uid = bench.user("id -u").strip()
    # What `launch_app` starts Okapi with, not the SSH session's own environment.
    environment = bench.user("systemctl --user show-environment").splitlines()
    bench.check(f"TMPDIR=/tmp/user/{uid}" in environment, "pam_tmpdir gives the graphical session its TMPDIR")


def run(bench: Bench) -> None:
    outputs_and_accounts(bench)
    store(bench)
    command_line(bench)
    refused_elevation(bench)
    mounts(bench)
    packages(bench)
    snaps(bench)
    browser(bench)
    private_temporary(bench)
