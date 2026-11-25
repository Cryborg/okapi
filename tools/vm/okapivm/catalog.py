"""The scenarios of `tools/vm/scenarios/`: each a module with a docstring, `TAGS` and `run(bench)`."""

from __future__ import annotations

import fnmatch
import importlib.util
import sys
from collections.abc import Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING

from . import config
from .machine import MachineError

if TYPE_CHECKING:
    from .bench import Bench


@dataclass(frozen=True)
class Scenario:
    name: str
    tags: tuple[str, ...]
    description: str
    run: Callable[[Bench], None]


def _load(name: str) -> Scenario:
    path = config.SCENARIOS / f"{name}.py"
    spec = importlib.util.spec_from_file_location(f"scenarios.{name}", path)
    if spec is None or spec.loader is None:
        raise MachineError(f"{path} cannot be loaded")
    module = importlib.util.module_from_spec(spec)
    # Where a dataclass of the scenario looks its module up.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    tags = getattr(module, "TAGS", [])
    if not isinstance(tags, list | tuple) or not all(isinstance(tag, str) for tag in tags):
        raise MachineError(f"{path}: TAGS is a list of words")
    run = getattr(module, "run", None)
    if not callable(run):
        raise MachineError(f"{path} has no run(bench)")
    return Scenario(name=name, tags=tuple(tags), description=(module.__doc__ or "").strip(), run=run)


def every() -> list[Scenario]:
    return [_load(path.stem) for path in sorted(config.SCENARIOS.glob("*.py")) if not path.stem.startswith("_")]


def select(patterns: list[str], tags: list[str]) -> list[Scenario]:
    """The scenarios one of `patterns` names (a name or a shell pattern, `cleaner_*`;
    all without one) that carry every tag of `tags`. A pattern naming none is an error."""
    scenarios = every()
    for pattern in patterns:
        if not any(fnmatch.fnmatchcase(scenario.name, pattern) for scenario in scenarios):
            known = ", ".join(scenario.name for scenario in scenarios)
            raise MachineError(f"no scenario matches {pattern!r}; there are: {known}")
    chosen = [
        scenario for scenario in scenarios
        if (not patterns or any(fnmatch.fnmatchcase(scenario.name, pattern) for pattern in patterns))
        and set(tags) <= set(scenario.tags)
    ]
    if not chosen:
        raise MachineError(f"no scenario carries the tags {', '.join(tags)} among those named")
    return chosen
