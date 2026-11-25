"""Settings: « Opérations administrateur » turned off, the cleaner offers nothing that needs the rights.

The switch turned off in Réglages is written to the settings; the next scan
shows no group of the administrator, no banner nor button for the rights, and
a cleaning removes the account's old thumbnail but leaves the old crash report
where it is (ADM-013).
"""

from __future__ import annotations

import time

from okapivm import cleaner, common
from okapivm.bench import Bench, ScenarioFailure

from scenarios import _admin

TAGS = ["admin", "settings"]

SWITCH = "Opérations administrateur"
PRESSED = "pressed"
SWITCH_TIMEOUT_SECONDS = 10
POLL_SECONDS = 0.5


def turn_off(bench: Bench) -> None:
    bench.launch_app()
    common.open_screen(bench, "Réglages")
    bench.check(PRESSED in bench.ui.node(SWITCH, role="toggle-button")["states"],
                "ADM-013: the administrator's operations are on by default")
    bench.ui.click(SWITCH, role="toggle-button")
    deadline = time.monotonic() + SWITCH_TIMEOUT_SECONDS
    while PRESSED in bench.ui.node(SWITCH, role="toggle-button")["states"]:
        if time.monotonic() > deadline:
            raise ScenarioFailure(f"« {SWITCH} » still on {SWITCH_TIMEOUT_SECONDS} s after its click")
        time.sleep(POLL_SECONDS)
    bench.screenshot("reglage-coupe")
    settings = common.read_store(bench, common.SETTINGS)
    bench.check(isinstance(settings, dict) and settings.get("adminOperations") is False,
                f"ADM-013: the settings keep the administrator's operations off ({settings})")


def run(bench: Bench) -> None:
    crash = common.Files(common.ROOT, ["/var/crash/okapi-vm.crash"], size=1024)
    thumbnail = common.Files(bench.accounts.admin.name, ["~/.cache/thumbnails/normal/okapi-vm.png"], size=1024)
    bench.allow_elevation_without_password()
    common.plant(bench, crash, thumbnail, age_seconds=_admin.AGING_SECONDS)
    turn_off(bench)

    cleaner.scan(bench)
    bench.screenshot("resultats")
    bench.check(not bench.ui.find(cleaner.OBTAIN, role="button"), "ADM-013: no button asks for the rights")
    bench.check(not bench.ui.find(role="label", pattern=_admin.BANNER_PATTERN), "ADM-013: no banner of the rights")
    rows = cleaner.rows(bench)
    admin = sorted(name for name, node in rows.items() if _admin.ADMINISTRATOR in node.get("description", ""))
    bench.check(cleaner.CRASH_REPORTS not in rows and not admin,
                f"ADM-013: no group of the administrator among the {len(rows)} results ({admin})")

    cleaner.clean(bench)
    bench.check(not common.present(bench, thumbnail), "ADM-013: the cleaning removes the account's old thumbnail")
    bench.check(common.present(bench, crash) == list(crash.paths),
                "ADM-013: the cleaning leaves the crash report, which needs the administrator")
    receipt = common.last_receipt(bench)
    bench.check(not receipt.get("admin"), f"ADM-013: the receipt holds no administrator's part ({receipt.get('admin')})")
