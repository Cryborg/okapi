"""Rights: a helper missing, of another version of the protocol, or that answers nonsense.

Each time the rights are asked for and the interface says what is wrong, the
crash report planted left where it is: the helper removed (pkexec never
launched, ADM-007), the helper of the commit before the protocol's version 5,
built on the host (ADM-008), then a script in its place that writes `x`
(ADM-009).
"""

from __future__ import annotations

import io
import shlex
import subprocess
import tarfile
from pathlib import Path

from okapivm import common, config
from okapivm.bench import Bench

from scenarios import _admin

TAGS = ["admin", "helper"]

# The last commit whose helper speaks version 4 of the protocol: the next one
# (ae6d9e9) raised it to 5.
OLD_PROTOCOL_COMMIT = "a21b0917ab2fddf7e3c738e14af843561ac249bd"
OLD_PROTOCOL_VERSION = 4
NONSENSE_HELPER = "#!/bin/sh\necho x\n"
STAGED_HELPER = "/var/tmp/okapi-vm-helper"
# What the interface traces once pkexec runs (`elevation.rs`).
LAUNCHED = "helper launched"

NOT_INSTALLED = "L'assistant administrateur n'est pas installé : lancez packaging/linux/install-helper.sh."
OUTDATED = "L'assistant administrateur installé n'est pas à jour : relancez packaging/linux/install-helper.sh."
FAILED = "L'assistant administrateur a échoué. Le détail est dans le journal."


def old_helper() -> Path:
    """The helper of `OLD_PROTOCOL_COMMIT`, built once on the host and kept in the bench's directory."""
    root = config.bench_home() / "old-helper" / OLD_PROTOCOL_COMMIT
    binary = root / "target" / "debug" / "okapi-helper"
    if binary.exists():
        return binary
    source = root / "source"
    if not source.exists():
        archive = subprocess.run(["git", "-C", str(config.WORKSPACE), "archive", "--format=tar", OLD_PROTOCOL_COMMIT],
                                 check=True, capture_output=True).stdout
        partial = root / "source.partial"
        partial.mkdir(parents=True, exist_ok=True)
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            tar.extractall(partial, filter="data")
        partial.rename(source)
    subprocess.run(["cargo", "build", "--locked", "--package", "okapi-helper",
                    "--manifest-path", str(source / "Cargo.toml"), "--target-dir", str(root / "target")], check=True)
    return binary


def replace_helper(bench: Bench, local: Path | None = None, *, text: str | None = None) -> None:
    """Puts a file of the host, or `text`, where the helper is installed, as root and executable."""
    installed = config.helper_paths().helper
    if local is not None:
        bench.machine.copy_to(common.ROOT, [local], STAGED_HELPER)
    else:
        bench.root(f"printf %s {shlex.quote(text or '')} > {STAGED_HELPER}")
    bench.root(shlex.join(["install", "--mode=0755", STAGED_HELPER, installed]))
    bench.root(shlex.join(["rm", STAGED_HELPER]))


def ask(bench: Bench, label: str) -> str:
    """Asks for the rights from a fresh start and gives the banner once it says what went wrong."""
    _admin.scan(bench)
    bench.ui.click(_admin.OBTAIN, role="button")
    said = _admin.wait_banner(bench, _admin.STILL_WAITING)
    bench.screenshot(label)
    return said


def launches(bench: Bench) -> int:
    """How many times the interface ran pkexec, by its journal."""
    return common.journal_text(bench).count(LAUNCHED)


def run(bench: Bench) -> None:
    old = old_helper()
    crash = common.Files(common.ROOT, ["/var/crash/okapi-vm.crash"], size=1024)
    bench.allow_elevation_without_password()
    common.plant(bench, crash, age_seconds=_admin.AGING_SECONDS)

    bench.root(shlex.join(["rm", config.helper_paths().helper]))
    said = ask(bench, "absent")
    bench.check(said.startswith(NOT_INSTALLED) and _admin.waiting(said) > 0,
                f"ADM-007: the helper missing, the interface says so and the group still waits ({said!r})")
    bench.check(launches(bench) == 0, "ADM-007: pkexec is never launched for a helper missing")
    bench.check("the helper is not installed" in common.journal_text(bench),
                "ADM-007: the interface's journal says the helper is not installed")

    replace_helper(bench, old)
    version = common.helper(bench, {"version": OLD_PROTOCOL_VERSION, "operation": "scan", "targets": ["/var/crash"],
                                    "minAgeMinutes": 60, "exclusions": []})
    answered = common.helper_answer(version).get("version")
    bench.check(version.code == 0 and answered == OLD_PROTOCOL_VERSION,
                f"ADM-008: the helper installed speaks version {OLD_PROTOCOL_VERSION} (exit {version.code}, {answered})")
    said = ask(bench, "ancien")
    bench.check(said.startswith(OUTDATED), f"ADM-008: a helper of version {OLD_PROTOCOL_VERSION} is said outdated ({said!r})")
    bench.check("unsupportedVersion" in common.journal_text(bench),
                "ADM-008: the interface's journal holds the old helper's refusal, unsupportedVersion")
    bench.check(launches(bench) == 1, "ADM-007: the journal does trace a run of pkexec, so its absence above is one")

    replace_helper(bench, text=NONSENSE_HELPER)
    said = ask(bench, "illisible")
    bench.check(said.startswith(FAILED), f"ADM-009: an answer that cannot be read is said a failure ({said!r})")
    bench.check("helper gave no answer to act on" in common.journal_text(bench),
                "ADM-009: the interface's journal keeps the answer it could not read")

    kept = common.present(bench, crash)
    bench.check(kept == list(crash.paths), "ADM-007, ADM-008, ADM-009: the crash report is still there")
