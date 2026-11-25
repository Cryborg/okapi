"""Coverage of what the scenarios run: the instrumented build, the profiles its
binaries leave in the machine, and the reports merged from every run.

Each build is kept under `coverage/builds/<digest>/` with the profiles of its
runs. A report merges every profile against the newest build: a function
unchanged since an older build keeps that build's counts, a changed one counts
only from runs of the new code (its hash no longer matches)."""

from __future__ import annotations

import hashlib
import os
import shlex
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

from . import config
from .machine import MachineError

if TYPE_CHECKING:
    from .bench import Bench

INSTALL_SCRIPT = config.BENCH / "install-coverage-tools.sh"
# Continuous mode (`%c` below) keeps the counters in the file as they change:
# the interface ends on SIGTERM, which skips the write at exit. On Linux it
# needs the counters relocated at run time.
INSTRUMENT_FLAGS = ("-C", "instrument-coverage", "-C", "llvm-args=-runtime-counter-relocation")
GUEST_PROFILES = "/var/tmp/okapi-coverage"
PROFILE_PATTERN = f"{GUEST_PROFILES}/%p%c.profraw"
# Gathered from the whole machine: pkexec clears the helper's environment, so
# it writes its profile under its default name in its working directory.
GUEST_GATHERED = "/var/tmp/okapi-coverage-gathered"
PROFILE_GLOB = "*.profraw"
IGNORED_SOURCES = r"/\.cargo/|/rustc/|/target/"
DEMANGLER = "rustfilt"
DIGEST_LENGTH = 16
SOURCES_FILE = "sources"


def rustflags() -> str:
    return shlex.join([*shlex.split(os.environ.get("RUSTFLAGS", "")), *INSTRUMENT_FLAGS])


def tool(name: str) -> Path:
    """An LLVM tool of the toolchain's own LLVM, the one the profiles are written in."""
    library = subprocess.run(["rustc", "--print", "target-libdir"], capture_output=True, text=True, check=True)
    path = Path(library.stdout.strip()).parent / "bin" / name
    if not path.exists():
        raise MachineError(f"{name} is missing: run {INSTALL_SCRIPT}")
    return path


@dataclass(frozen=True)
class Build:
    """An instrumented build, its binaries and the profiles its runs left."""

    directory: Path

    @property
    def binaries(self) -> list[Path]:
        return [self.directory / name for name in config.OKAPI_BINARIES]

    @property
    def profiles(self) -> Path:
        return self.directory / "profiles"

    @property
    def sources(self) -> Path:
        return Path((self.directory / SOURCES_FILE).read_text(encoding="utf-8").strip())

    @property
    def built(self) -> float:
        return self.binaries[0].stat().st_mtime


def digest(binaries: list[Path]) -> str:
    hashed = hashlib.sha256()
    for path in binaries:
        hashed.update(path.read_bytes())
    return hashed.hexdigest()[:DIGEST_LENGTH]


def keep(home: config.Home, built: Path) -> Build:
    """The build in `built` kept with its sources' place; two runs keeping the same one share it."""
    sources = [built / name for name in config.OKAPI_BINARIES]
    build = Build(home.coverage / "builds" / digest(sources))
    if build.directory.exists():
        return build
    partial = build.directory.with_name(f"{build.directory.name}.{os.getpid()}.partial")
    partial.mkdir(parents=True)
    for source in sources:
        shutil.copy2(source, partial)
    (partial / SOURCES_FILE).write_text(f"{config.WORKSPACE}\n", encoding="utf-8")
    try:
        partial.rename(build.directory)
    except OSError:
        if not build.directory.exists():
            raise
        shutil.rmtree(partial)
    return build


def prepare_machine(bench: Bench) -> None:
    bench.root(shlex.join(["install", "-d", "--mode=1777", GUEST_PROFILES]))


def gather(bench: Bench, build: Build) -> None:
    """Copies to the host every profile the machine holds, once its binaries have ended."""
    found = bench.root(shlex.join(["find", "/", "-xdev", "-type", "f", "-name", PROFILE_GLOB, "-print"])).split()
    bench.log(f"coverage profiles: {', '.join(found) or 'none'}")
    if not found:
        return
    bench.root(" && ".join([
        shlex.join(["install", "-d", GUEST_GATHERED]),
        shlex.join(["cp", f"--target-directory={GUEST_GATHERED}", *found]),
        shlex.join(["chmod", "-R", "a+r", GUEST_GATHERED]),
    ]))
    build.profiles.mkdir(parents=True, exist_ok=True)
    bench.machine.copy_from("root", GUEST_GATHERED, build.profiles / bench.directory.name)


def builds(home: config.Home) -> list[Build]:
    root = home.coverage / "builds"
    if not root.exists():
        return []
    return [Build(path) for path in root.iterdir() if path.is_dir() and not path.name.endswith(".partial")]


@dataclass(frozen=True)
class Report:
    directory: Path
    runs: int
    profiles: int
    build: Build

    @property
    def files(self) -> Path:
        return self.directory / "files.txt"

    @property
    def functions(self) -> Path:
        return self.directory / "functions.txt"

    @property
    def html(self) -> Path:
        return self.directory / "html" / "index.html"


def report(home: config.Home) -> Report:
    """Merges every profile gathered and writes the reports by file and by function, as text and HTML."""
    kept = builds(home)
    profiles = sorted(path for build in kept for path in build.profiles.rglob(PROFILE_GLOB))
    if not profiles:
        raise MachineError("no profile gathered yet: `okapi-vm run --coverage <scenario>` first")
    newest = max(kept, key=lambda build: build.built)
    if not newest.sources.is_dir():
        raise MachineError(f"the sources of the newest build, {newest.sources}, are gone")
    directory = home.coverage / "report"
    if directory.exists():
        shutil.rmtree(directory)
    directory.mkdir(parents=True)
    merged = directory / "merged.profdata"
    subprocess.run(
        [str(tool("llvm-profdata")), "merge", "--sparse", "--failure-mode=all", f"--output={merged}",
         *map(str, profiles)],
        check=True,
    )
    first, *others = newest.binaries
    common = [str(first), *(f"--object={binary}" for binary in others), f"--instr-profile={merged}",
              f"--ignore-filename-regex={IGNORED_SOURCES}"]
    if demangler := shutil.which(DEMANGLER):
        common.append(f"-Xdemangler={demangler}")
    else:
        print(f"okapi-vm: {DEMANGLER} is missing, function names stay mangled: run {INSTALL_SCRIPT}")
    cov = str(tool("llvm-cov"))
    sources = str(newest.sources / "crates")
    reports = Report(directory, runs=len({path.parent for path in profiles}), profiles=len(profiles), build=newest)
    with reports.files.open("w", encoding="utf-8") as output:
        subprocess.run([cov, "report", *common], stdout=output, check=True)
    with reports.functions.open("w", encoding="utf-8") as output:
        subprocess.run([cov, "report", "--show-functions", *common, sources], stdout=output, check=True)
    subprocess.run([cov, "show", "--format=html", f"--output-dir={reports.html.parent}", *common, sources],
                   check=True)
    return reports


def reset(home: config.Home) -> None:
    if home.coverage.exists():
        shutil.rmtree(home.coverage)
