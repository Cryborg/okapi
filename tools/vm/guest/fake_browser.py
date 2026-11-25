"""Holds browser profiles as a running browser does, until it is stopped.

Arguments: `firefox=<profile>` locks `.parentlock` as Firefox does on Linux
(a POSIX lock, listed in /proc/locks); `chromium=<profile>` points `SingletonLock` at
`<host>-<pid>` of this process, as Chromium does; `--stubborn` ignores
SIGTERM, as a browser that does not close when asked to.
"""

from __future__ import annotations

import fcntl
import os
import signal
import socket
import sys
from pathlib import Path

STUBBORN = "--stubborn"


def main() -> int:
    held = []
    for argument in sys.argv[1:]:
        if argument == STUBBORN:
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            continue
        browser, _, profile = argument.partition("=")
        directory = Path(profile).expanduser()
        if browser == "firefox":
            lock = (directory / ".parentlock").open("a")
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
