"""Drives an application through the accessibility bus (AT-SPI), inside the test machine.

Run over SSH as the account logged in to GNOME. Each command prints JSON on
standard output (`tree`: one node per line) and exits 0, or says why on
standard error and exits 1. The waits happen here, against the live tree, not
across SSH.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
from dataclasses import dataclass

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, GLib  # noqa: E402

# libatspi 2.5x does not read the signature of the cache signals AccessKit
# sends and warns once per node; the tree is read here, not from that cache.
GLib.log_set_handler("dbind", GLib.LogLevelFlags.LEVEL_WARNING, lambda *_: None)

POLL_SECONDS = 0.5
DEFAULT_TIMEOUT_SECONDS = 30.0
SHELL_APPLICATION = "gnome-shell"
ACTIVATING_ACTIONS = ("click", "press", "activate", "toggle", "default")
CHECKABLE = "checkable"
CHECKED = "checked"


@dataclass(frozen=True)
class Query:
    name: str | None
    pattern: re.Pattern[str] | None
    role: str | None
    state: str | None

    def matches(self, node: Atspi.Accessible) -> bool:
        name = node.get_name() or ""
        if self.name is not None and name != self.name:
            return False
        if self.pattern is not None and not self.pattern.search(name):
            return False
        if self.role is not None and normalized(node.get_role_name()) != self.role:
            return False
        return self.state is None or self.state in states(node)

    def describe(self) -> str:
        parts = [f"name={self.name!r}" if self.name is not None else None,
                 f"pattern={self.pattern.pattern!r}" if self.pattern else None,
                 f"role={self.role!r}" if self.role else None,
                 f"state={self.state!r}" if self.state else None]
        return ", ".join(part for part in parts if part)


class DriverError(RuntimeError):
    pass


def normalized(role: str) -> str:
    return role.replace(" ", "-").lower()


def states(node: Atspi.Accessible) -> list[str]:
    return sorted(state.value_nick for state in node.get_state_set().get_states())


def described(node: Atspi.Accessible) -> dict[str, object]:
    entry: dict[str, object] = {
        "role": normalized(node.get_role_name()),
        "name": node.get_name() or "",
        "states": states(node),
    }
    if description := node.get_description():
        entry["description"] = description
    # Through the interface's class: on the node, a method of the same name
    # of Atspi.Accessible answers instead.
    if value := node.get_value_iface():
        entry["value"] = Atspi.Value.get_current_value(value)
    if text := node.get_text_iface():
        entry["text"] = Atspi.Text.get_text(text, 0, Atspi.Text.get_character_count(text))
    return entry


def ensure_session_bus() -> None:
    os.environ.setdefault("DBUS_SESSION_BUS_ADDRESS", f"unix:path=/run/user/{os.getuid()}/bus")


def applications() -> list[Atspi.Accessible]:
    desktop = Atspi.get_desktop(0)
    found = []
    for index in range(desktop.get_child_count()):
        child = desktop.get_child_at_index(index)
        if child is not None:
            found.append(child)
    return found


def application(name: str) -> Atspi.Accessible | None:
    for candidate in applications():
        if name.lower() in (candidate.get_name() or "").lower():
            # The proxies cache what they read and nothing here listens to
            # the change events: without this, a wait sees a frozen tree.
            candidate.clear_cache()
            return candidate
    return None


def walk(node: Atspi.Accessible, *, showing_only: bool, depth: int = 0):
    yield depth, node
    for index in range(node.get_child_count()):
        child = node.get_child_at_index(index)
        if child is None:
            continue
        if showing_only and "showing" not in states(child):
            continue
        yield from walk(child, showing_only=showing_only, depth=depth + 1)


def find(app_name: str, query: Query, *, showing_only: bool = True) -> list[Atspi.Accessible]:
    app = application(app_name)
    if app is None:
        return []
    return [node for _, node in walk(app, showing_only=showing_only) if query.matches(node)]


Found = list[tuple[Atspi.Accessible, dict[str, object]]]


def snapshot(app_name: str, query: Query) -> Found | None:
    """The nodes matching, each read at once (read later, it may be gone with its
    page), or None when the tree changed while it was walked."""
    try:
        return [(node, described(node)) for node in find(app_name, query)]
    except GLib.Error:
        return None


def wait_for(app_name: str, query: Query, timeout: float, *, gone: bool = False) -> Found:
    deadline = time.monotonic() + timeout
    while True:
        found = snapshot(app_name, query)
        if found is not None and bool(found) != gone:
            return found
        if time.monotonic() > deadline:
            verb = "still shows" if gone else "shows no"
            raise DriverError(f"after {timeout:.0f} s, {app_name} {verb} node with {query.describe()}")
        time.sleep(POLL_SECONDS)


def activate(node: Atspi.Accessible) -> str:
    action = node.get_action_iface()
    if action is None or action.get_n_actions() == 0:
        raise DriverError(f"{described(node)} has no action")
    names = [Atspi.Action.get_action_name(action, index) for index in range(action.get_n_actions())]
    chosen = next((index for index, name in enumerate(names) if name in ACTIVATING_ACTIONS), 0)
    if not action.do_action(chosen):
        raise DriverError(f"{described(node)} refused its action {names[chosen]!r}")
    return names[chosen]


def tree_text(app_name: str, *, showing_only: bool) -> str:
    app = application(app_name)
    if app is None:
        names = [candidate.get_name() for candidate in applications()]
        raise DriverError(f"no application named {app_name!r}: {names}")
    lines = []
    for depth, node in walk(app, showing_only=showing_only):
        entry = described(node)
        extras = {key: value for key, value in entry.items() if key not in ("role", "name", "states")}
        suffix = f" {json.dumps(extras, ensure_ascii=False)}" if extras else ""
        lines.append(f"{'  ' * depth}{entry['role']} {entry['name']!r} [{' '.join(entry['states'])}]{suffix}")
    return "\n".join(lines)


def query_of(arguments: argparse.Namespace) -> Query:
    return Query(
        name=arguments.name,
        pattern=re.compile(arguments.pattern) if arguments.pattern else None,
        role=normalized(arguments.role) if arguments.role else None,
        state=arguments.state,
    )


def chosen(found: Found, index: int | None, query: Query) -> tuple[Atspi.Accessible, dict[str, object]]:
    if index is not None:
        if index >= len(found):
            raise DriverError(f"{len(found)} nodes with {query.describe()}, no index {index}")
        return found[index]
    if len(found) > 1:
        raise DriverError(
            f"{len(found)} nodes with {query.describe()}, pass --index: "
            + json.dumps([entry for _, entry in found], ensure_ascii=False)
        )
    return found[0]


def box_state(entry: dict[str, object]) -> tuple[bool, object]:
    return CHECKED in entry["states"], entry.get("description")


def click(app_name: str, query: Query, index: int | None, timeout: float, settle: float) -> dict[str, object]:
    """Clicks the node once it has stood for `settle` seconds, as a human would
    after a change of screen; a check box must then change state, or its
    description: a box partly checked, then unchecked, is unchecked twice."""
    wait_for(app_name, query, timeout)
    time.sleep(settle)
    node, entry = chosen(wait_for(app_name, query, timeout), index, query)
    if "enabled" not in entry["states"]:
        raise DriverError(f"{entry} is not enabled")
    action = activate(node)
    if CHECKABLE in entry["states"]:
        was = box_state(entry)
        deadline = time.monotonic() + timeout
        while box_state(chosen(wait_for(app_name, query, timeout), index, query)[1]) == was:
            if time.monotonic() > deadline:
                raise DriverError(f"{entry} did not change state after its action {action!r}")
            time.sleep(POLL_SECONDS)
    return {"node": entry, "action": action}


def extents(node: Atspi.Accessible) -> list[int]:
    """`[x, y, width, height]` in the window's content. Wayland tells no one
    where a window is: asked for screen coordinates, AT-SPI gives these."""
    component = node.get_component_iface()
    if component is None:
        raise DriverError(f"{described(node)} has no place on screen")
    rectangle = Atspi.Component.get_extents(component, Atspi.CoordType.WINDOW)
    return [rectangle.x, rectangle.y, rectangle.width, rectangle.height]


def run(arguments: argparse.Namespace) -> object:
    if arguments.command == "applications":
        return [candidate.get_name() for candidate in applications()]
    if arguments.command == "tree":
        return tree_text(arguments.app, showing_only=not arguments.all)
    query = query_of(arguments)
    if arguments.command == "find":
        deadline = time.monotonic() + arguments.timeout
        while (found := snapshot(arguments.app, query)) is None and time.monotonic() < deadline:
            time.sleep(POLL_SECONDS)
        if found is None:
            raise DriverError(f"{arguments.app}'s tree kept changing for {arguments.timeout:.0f} s")
        return [entry for _, entry in found]
    if arguments.command == "wait":
        return [entry for _, entry in wait_for(arguments.app, query, arguments.timeout, gone=arguments.gone)]
    if arguments.command == "click":
        return click(arguments.app, query, arguments.index, arguments.timeout, arguments.settle)
    node, entry = chosen(wait_for(arguments.app, query, arguments.timeout), arguments.index, query)
    if arguments.command == "node":
        return {**entry, "extents": extents(node)}
    raise DriverError(f"unknown command {arguments.command}")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    root.add_argument("--app", default="okapi", help="part of the application's accessible name")
    commands = root.add_subparsers(dest="command", required=True)
    commands.add_parser("applications", help="names of the applications on the bus")
    tree = commands.add_parser("tree", help="the application's tree, one node per line")
    tree.add_argument("--all", action="store_true", help="nodes not showing too")
    for name, help_text in (("find", "nodes showing that match"), ("wait", "wait until a node shows (or goes)"),
                            ("click", "wait for one node, then run its action"),
                            ("node", "wait for one node, then describe it with its place in its window")):
        command = commands.add_parser(name, help=help_text)
        command.add_argument("--name", help="exact accessible name")
        command.add_argument("--pattern", help="regular expression searched in the name")
        command.add_argument("--role", help="AT-SPI role, as `check-box` or `check box`")
        command.add_argument("--state", help="AT-SPI state the node must have, as `checked`")
        command.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS)
        if name == "wait":
            command.add_argument("--gone", action="store_true")
        if name in ("click", "node"):
            command.add_argument("--index", type=int)
        if name == "click":
            command.add_argument("--settle", type=float, default=0.0,
                                 help="seconds the node must stand before the click")
    return root


def main() -> int:
    ensure_session_bus()
    arguments = parser().parse_args()
    try:
        result = run(arguments)
    except DriverError as error:
        print(error, file=sys.stderr)
        return 1
    print(result if isinstance(result, str) else json.dumps(result, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
