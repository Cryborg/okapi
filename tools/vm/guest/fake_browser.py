"""Holds browser profiles as a running browser does, until it is stopped.

Arguments: `firefox=<profile>` locks `parent.lock` as Firefox does (a POSIX
lock, listed in /proc/locks); `chromium=<profile>` points `SingletonLock` at
`<host>-<pid>` of this process, as Chromium does.
"""

from __future__ import annotations

import fcntl
import os
import signal
import socket
import sys
from pathlib import Path


def main() -> int:
    held = []
    for argument in sys.argv[1:]:
        browser, _, profile = argument.partition("=")
        directory = Path(profile).expanduser()
        if browser == "firefox":
            lock = (directory / "parent.lock").open("a")
            fcntl.lockf(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            held.append(lock)
        elif browser == "chromium":
            singleton = directory / "SingletonLock"
            singleton.unlink(missing_ok=True)
            singleton.symlink_to(f"{socket.gethostname()}-{os.getpid()}")
        else:
            print(f"unknown browser {browser!r}", file=sys.stderr)
            return 1
    signal.pause()
    return 0


if __name__ == "__main__":
    sys.exit(main())
