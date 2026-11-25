"""The slots machines run in side by side.

A slot is busy while a process holds its lock (a run, for as long as it
lasts) or while its machine runs (one `start` or `--keep` left behind)."""

from __future__ import annotations

import contextlib
import fcntl
import os
from collections.abc import Iterator
from typing import TextIO

from . import config
from .machine import Machine, MachineError


def parse_slot(text: str) -> int:
    """A slot number as `--slot` and `OKAPI_VM_SLOT` give it; anything else is refused."""
    if not text.isdigit() or int(text) not in config.SLOTS:
        raise MachineError(f"slot {text!r}: there are slots {config.SLOTS.start} to {config.SLOTS.stop - 1} only")
    return int(text)


def chosen_slot(option: str | None) -> int | None:
    """The slot `--slot` names, else `OKAPI_VM_SLOT`, else none in particular."""
    text = option or os.environ.get(config.SLOT_VARIABLE)
    return parse_slot(text) if text else None


def _locked(home: config.Home, number: int) -> TextIO | None:
    """The slot's lock, held, or None when another process holds it."""
    directory = home.slot(number)
    directory.mkdir(parents=True, exist_ok=True)
    lock = (directory / "lock").open("a", encoding="utf-8")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        lock.close()
        return None
    return lock


@contextlib.contextmanager
def claimed(home: config.Home, wanted: int | None) -> Iterator[Machine]:
    """The machine of a free slot, `wanted` or the first free, kept for this process until the block ends."""
    for number in [wanted] if wanted is not None else config.SLOTS:
        lock = _locked(home, number)
        if lock is None:
            continue
        with lock:
            machine = Machine(home, number)
            if machine.pid() is None:
                yield machine
                return
    if wanted is not None:
        raise MachineError(f"slot {wanted} is busy: a run holds it, or a machine left running "
                           f"(`okapi-vm stop --slot {wanted}`)")
    raise MachineError(f"the {len(config.SLOTS)} slots are busy; `okapi-vm stop --slot N` frees one left running")


def running(home: config.Home, wanted: int | None) -> Machine:
    """The running machine of `wanted`, or the only one running."""
    if wanted is not None:
        machine = Machine(home, wanted)
        if machine.pid() is None:
            raise MachineError(f"no machine runs in slot {wanted}")
        return machine
    machines = [machine for machine in (Machine(home, number) for number in config.SLOTS) if machine.pid() is not None]
    if not machines:
        raise MachineError("no machine is running: `okapi-vm start` first")
    if len(machines) > 1:
        raise MachineError(f"machines run in slots {', '.join(str(machine.slot) for machine in machines)}: "
                           "pass --slot")
    return machines[0]


def stop(home: config.Home, wanted: int | None) -> None:
    """Drops a slot's machine, never one a run is still using."""
    number = wanted if wanted is not None else running(home, None).slot
    lock = _locked(home, number)
    if lock is None:
        raise MachineError(f"a run is using slot {number}; it drops its machine when it ends")
    with lock:
        Machine(home, number).destroy()


def any_running(home: config.Home) -> bool:
    return any(Machine(home, number).pid() is not None or _is_held(home, number) for number in config.SLOTS)


def _is_held(home: config.Home, number: int) -> bool:
    lock = _locked(home, number)
    if lock is None:
        return True
    lock.close()
    return False
