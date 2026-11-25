"""What the scenarios of the administrator's rights share: the banner that says
where the request for the rights stands. The rest of the Cleaner's page is
`okapivm.cleaner`."""

from __future__ import annotations

import re

from okapivm.bench import Bench

# Beyond the default minimum age and the floor the helper imposes on any
# settings (`HELPER_MIN_AGE_FLOOR`), both an hour.
AGING_SECONDS = 2 * 60 * 60
RIGHTS_TIMEOUT_SECONDS = 120

CANCEL = "Annuler"
ADMINISTRATOR = "Administrateur"
# The banner of the rights (`results.slint`), a label: where the request
# stands, or how many groups wait for the rights.
STILL_WAITING = "Groupes qui les demandent toujours : "
BANNER_PATTERN = ("Groupes qui|Préparation de la demande|Saisissez le mot de passe|Analyse avec les droits"
                  "|Annulation de la demande")
WAITING_COUNT = re.compile(r"(?:administrateur|toujours) : (\d+)\.$")
HELPER_ACCEPTED = "request accepted"


def banner(bench: Bench) -> str:
    """What the banner of the rights says now."""
    return bench.ui.node(role="label", pattern=BANNER_PATTERN)["name"]


def wait_banner(bench: Bench, pattern: str, timeout: float = RIGHTS_TIMEOUT_SECONDS) -> str:
    """Waits for the banner of the rights to match `pattern` and gives what it says."""
    return bench.ui.wait(role="label", pattern=pattern, timeout=timeout)[0]["name"]


def waiting(said: str) -> int:
    """How many groups the banner `said` tells wait for the rights; 0 when it tells none."""
    found = WAITING_COUNT.search(said)
    return int(found.group(1)) if found else 0
