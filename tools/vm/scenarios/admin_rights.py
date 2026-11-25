"""Rights: the request for the administrator's rights, each way it can end, and the cleaning they unlock.

Old files planted where three groups of the administrator look (crash reports,
APT's logs, core dumps that only root may read). The banner counts the groups
that wait for the rights (ADM-002), the core dumps' line among them, never
checked (ADM-014). With a password asked: the step shown while GNOME's dialog
is open, the request cancelled in the interface then the dialog closed
(ADM-003, ADM-006), the dialog closed alone (ADM-004). Polkit refusing
(ADM-005). The request held in its preparation by an exclusion on an NFS mount
whose server is gone, then withdrawn before any dialog (ADM-003, ADM-006). An
exclusion added after the rights: nothing removed (ADM-010). The password
typed once by Okapi's own policy, the cleaning asks no more (ADM-021), cleans
the administrator's part and records it (ADM-011), the helper writing no file
of its own (ADM-020). A tree deeper than root's walk grown in the crash folder
after the rights: the cleaning says it changed and leaves it (ADM-012).

« Analyse avec les droits administrateur… » (ADM-003) lasts less than a look
at the interface: not checked.
"""

from __future__ import annotations

import re
import shlex

from okapivm import common
from okapivm.bench import Bench

from scenarios import _admin

TAGS = ["admin", "helper", "rights"]

CRASH = "/var/crash"
DEEP = f"{CRASH}/okapi-vm-deep"
DEEP_LEVELS = 10
CORE_DUMPS = "/var/lib/systemd/coredump"
MARKER = "/var/tmp/okapi-vm-before-cleaning"
NFS_EXPORT = "/srv/okapi-vm-nfs"
NFS_MOUNT = "/mnt/okapi-vm-nfs"
NFS_EXPORTS = "/etc/exports.d/okapi-vm.exports"
NFS_HELD_EXCLUSION = f"{NFS_MOUNT}/kept"
SETTINGS = "settings.json"

UNREADABLE = "Illisible sans les droits administrateur"
PREPARING = "Préparation de la demande des droits…"
AUTHENTICATING = "Saisissez le mot de passe dans la fenêtre du système. Annuler n'agit qu'une fois cette fenêtre fermée."
CANCELLING = "Annulation de la demande des droits : elle prend effet une fois la fenêtre du système fermée."
WITHDRAWING = "Annulation de la demande des droits…"
DISMISSED = "L'authentification a été annulée."
CANCELLED = "La demande a été annulée avant la fin."
REFUSED = "Les droits administrateur ont été refusés, ou la demande n'a pas pu aboutir. Le détail est dans le journal."
RIGHTS_CHANGED = ("Les réglages ont changé depuis l'obtention des droits administrateur : rien n'est supprimé, "
                  "obtenez-les à nouveau.")
PART_CHANGED = ("Une partie de ce que l'assistant administrateur devait nettoyer a changé depuis l'obtention des "
                "droits : elle n'a pas été nettoyée en entier. Le détail est dans le journal.")
ADMIN_STEP = "Nettoyage des éléments administrateur"
DONE = "Fait"
PASSWORD_FIELD = "password-text"
# What a root process of the machine writes on its own while the scenario runs.
OWN_WRITES = re.compile(r"^/var/log/(journal/.+\.journal|audit/audit\.log)$"
                        r"|^/var/lib/systemd/timers/|^/var/spool/anacron/|\.profraw$")


def forget_settings(bench: Bench) -> None:
    bench.run_as(bench.accounts.admin.name, f"rm -f {common.shell_path(common.store_path(SETTINGS))}")


def planted(bench: Bench) -> tuple[common.Files, common.Files, common.Files]:
    crash = common.Files(common.ROOT, [f"{CRASH}/okapi-vm.crash", f"{CRASH}/okapi-vm-kept.crash"], size=1024)
    apt_log = common.Files(common.ROOT, ["/var/log/apt/okapi-vm.log.1.gz"], size=1024)
    core_dump = common.Files(common.ROOT, [f"{CORE_DUMPS}/core.okapi-vm.zst"], size=1024)
    common.plant(bench, crash, apt_log, core_dump, age_seconds=_admin.AGING_SECONDS)
    # As systemd-coredump makes it once it has kept a dump; absent from a fresh machine.
    bench.root(shlex.join(["chmod", "0700", CORE_DUMPS]))
    return crash, apt_log, core_dump


def waiting_rows(bench: Bench) -> dict[str, dict]:
    return {name: node for name, node in _admin.rows(bench).items()
            if _admin.ADMINISTRATOR in node.get("description", "")}


def banner_counts_the_groups(bench: Bench) -> None:
    _admin.scan(bench)
    said = _admin.banner(bench)
    bench.screenshot("bandeau")
    waiting = waiting_rows(bench)
    bench.check(_admin.waiting(said) == len(waiting) > 0,
                f"ADM-002: the banner counts the {len(waiting)} groups waiting for the rights ({said!r}, {sorted(waiting)})")
    bench.check({_admin.CRASH_REPORTS, _admin.APT_LOGS, _admin.CORE_DUMPS} <= waiting.keys(),
                f"ADM-002: the crash reports, APT's logs and the core dumps wait for the rights ({sorted(waiting)})")
    core = waiting[_admin.CORE_DUMPS]
    bench.check(UNREADABLE in core.get("description", "") and _admin.CHECKED not in core["states"],
                f"ADM-014: the core dumps' line says root alone reads it, unchecked ({core.get('description')!r})")


def dialog_cancel(bench: Bench) -> None:
    bench.shell.wait(_admin.CANCEL, role="button", timeout=_admin.RIGHTS_TIMEOUT_SECONDS)
    # GNOME Shell's buttons offer no action to AT-SPI: clicked at their pixels.
    bench.click_at(*bench.shell.center(bench.shell.node(_admin.CANCEL, role="button")))
    bench.shell.wait(_admin.CANCEL, role="button", gone=True)


def accepted(bench: Bench) -> int:
    return common.journal_text(bench).count(_admin.HELPER_ACCEPTED)


def password_cancelled(bench: Bench) -> None:
    bench.set_elevation(common.Elevation.PASSWORD)
    _admin.scan(bench)
    before = accepted(bench)
    bench.ui.click(_admin.OBTAIN, role="button")
    _admin.wait_banner(bench, re.escape(AUTHENTICATING))
    bench.shell.wait(_admin.CANCEL, role="button", timeout=_admin.RIGHTS_TIMEOUT_SECONDS)
    bench.screenshot("mot-de-passe")
    bench.check(bool(bench.ui.find(_admin.CANCEL, role="button")),
                "ADM-003: while GNOME's dialog asks the password, the banner says so and « Annuler » is offered")
    bench.ui.click(_admin.CANCEL, role="button")
    said = _admin.wait_banner(bench, re.escape(CANCELLING))
    bench.check(said == CANCELLING, "ADM-006: cancelled in the interface during the dialog, it waits for the dialog")
    dialog_cancel(bench)
    said = _admin.wait_banner(bench, _admin.STILL_WAITING)
    bench.screenshot("annule-pendant")
    bench.check(said.startswith(DISMISSED) and _admin.waiting(said) > 0,
                f"ADM-006: the dialog closed, the request ends and the groups still wait ({said!r})")

    bench.ui.click(_admin.OBTAIN, role="button")
    _admin.wait_banner(bench, re.escape(AUTHENTICATING))
    dialog_cancel(bench)
    said = _admin.wait_banner(bench, _admin.STILL_WAITING)
    bench.screenshot("fenetre-fermee")
    bench.check(said.startswith(DISMISSED) and _admin.waiting(said) > 0,
                f"ADM-004: GNOME's dialog closed, the authentication is said cancelled, the groups still wait ({said!r})")
    bench.check(accepted(bench) == before, "ADM-004, ADM-006: no request reached the helper")


def refused(bench: Bench) -> None:
    bench.set_elevation(common.Elevation.REFUSED)
    _admin.scan(bench)
    bench.ui.click(_admin.OBTAIN, role="button")
    said = _admin.wait_banner(bench, _admin.STILL_WAITING)
    bench.screenshot("refuse")
    bench.check(said.startswith(REFUSED) and _admin.waiting(said) > 0,
                f"ADM-005: polkit refusing, the rights are said refused and the groups still wait ({said!r})")


def held_by_a_gone_server(bench: Bench) -> None:
    bench.root(f"mkdir -p {NFS_EXPORT} {NFS_MOUNT} /etc/exports.d && "
               f"echo '{NFS_EXPORT} 127.0.0.1(rw,no_subtree_check,no_root_squash)' > {NFS_EXPORTS} && "
               f"systemctl start nfs-kernel-server && exportfs -ra && "
               f"mount -t nfs -o hard,vers=4,noac,lookupcache=none 127.0.0.1:{NFS_EXPORT} {NFS_MOUNT} && "
               f"mkdir -p {NFS_HELD_EXCLUSION}")
    bench.root("systemctl stop nfs-kernel-server")


def release_the_mount(bench: Bench) -> None:
    bench.root("systemctl start nfs-kernel-server", check=False)
    bench.root(f"umount -f -l {NFS_MOUNT}", check=False)
    bench.root(f"rm -f {NFS_EXPORTS}; exportfs -ra; systemctl stop nfs-kernel-server", check=False)


def withdrawn_before_the_dialog(bench: Bench) -> None:
    bench.set_elevation(common.Elevation.PASSWORD)
    _admin.scan(bench)
    before = accepted(bench)
    held_by_a_gone_server(bench)
    try:
        common.write_store(bench, SETTINGS, {"version": 1, "exclusions": [NFS_HELD_EXCLUSION]})
        bench.ui.click(_admin.OBTAIN, role="button")
        said = _admin.wait_banner(bench, re.escape(PREPARING))
        bench.screenshot("preparation")
        bench.check(said == PREPARING, "ADM-003, ADM-006: before any dialog, the request is said under preparation")
        bench.ui.click(_admin.CANCEL, role="button")
        said = _admin.wait_banner(bench, re.escape(WITHDRAWING))
        bench.check(said == WITHDRAWING, "ADM-006: cancelled before the dialog, the request is withdrawn")
        said = _admin.wait_banner(bench, _admin.STILL_WAITING)
        bench.screenshot("retiree")
        bench.check(said.startswith(CANCELLED) and _admin.waiting(said) > 0,
                    f"ADM-006: withdrawn, the request is said cancelled before its end and the groups still wait ({said!r})")
        bench.check(not bench.shell.find(_admin.CANCEL, role="button"), "ADM-006: no dialog was opened")
        bench.check(accepted(bench) == before, "ADM-006: no request reached the helper")
    finally:
        bench.stop_app()
        release_the_mount(bench)
        forget_settings(bench)


def settings_changed(bench: Bench, crash: common.Files) -> None:
    bench.allow_elevation_without_password()
    _admin.scan(bench)
    _admin.obtain_rights(bench)
    _admin.check_groups(bench, _admin.CRASH_REPORTS)
    common.write_store(bench, SETTINGS, {"version": 1, "exclusions": [crash.paths[1]]})
    _admin.clean(bench, done=re.escape(RIGHTS_CHANGED))
    bench.screenshot("reglages-changes")
    bench.check(common.present(bench, crash) == list(crash.paths),
                "ADM-010: the settings changed since the rights, no crash report is removed")
    bench.check(bool(bench.ui.find(_admin.OBTAIN, role="button")),
                "ADM-010: the rights are to be obtained again")
    bench.stop_app()
    forget_settings(bench)


def type_password(bench: Bench) -> None:
    bench.shell.wait(role=PASSWORD_FIELD, timeout=_admin.RIGHTS_TIMEOUT_SECONDS)
    bench.type_text(bench.accounts.admin.password)
    bench.key("Enter")


def written_by_root_since(bench: Bench) -> list[str]:
    listed = bench.root(shlex.join(["find", "/", "/tmp", "/dev/shm", "-xdev", "-newer", MARKER, "-user", "root",
                                    "-not", "-type", "d", "-print"]), check=False)
    return [path for path in listed.splitlines() if not OWN_WRITES.search(path)]


def cleaned_with_one_password(bench: Bench, crash: common.Files, apt_log: common.Files,
                              core_dump: common.Files) -> None:
    bench.elevation_by_policy()
    _admin.scan(bench)
    bench.ui.click(_admin.OBTAIN, role="button")
    type_password(bench)
    bench.ui.wait(_admin.OBTAINED, timeout=_admin.SCAN_TIMEOUT_SECONDS)
    core = _admin.row(bench, _admin.CORE_DUMPS)
    bench.check(_admin.CHECKED not in core["states"],
                f"ADM-014: after the rights, the core dumps' line is still unchecked ({core['states']})")
    _admin.check_groups(bench, _admin.CRASH_REPORTS, _admin.APT_LOGS)
    bench.root(shlex.join(["touch", MARKER]))
    accepted_before = accepted(bench)
    _admin.clean(bench, done=_admin.CLEANED)
    bench.screenshot("recu")
    bench.check(not bench.shell.find(role=PASSWORD_FIELD),
                "ADM-021: the password typed for the rights is not asked again for the cleaning")
    step = bench.ui.node(ADMIN_STEP, role="list-item")
    bench.check(step.get("description", "").startswith(DONE),
                f"ADM-011: the receipt lists the administrator's step done ({step.get('description')!r})")
    receipt = common.last_receipt(bench)
    bench.check(bool(receipt.get("admin", {}).get("done")),
                f"ADM-011: the history records the administrator's part done ({receipt.get('admin')})")
    bench.check(not common.present(bench, crash) and not common.present(bench, apt_log),
                "ADM-011: the crash reports and APT's old log are gone")
    bench.check(common.present(bench, core_dump) == list(core_dump.paths), "ADM-014: the core dump stays")
    bench.check(accepted(bench) > accepted_before,
                "ADM-020: the helper's traces reach the interface's journal (request accepted)")
    written = written_by_root_since(bench)
    bench.check(not written, f"ADM-020: the helper writes no file of its own ({written})")


def part_changed(bench: Bench) -> None:
    bench.allow_elevation_without_password()
    first = common.Files(common.ROOT, [f"{CRASH}/okapi-vm.crash"], size=1024)
    common.plant(bench, first, age_seconds=_admin.AGING_SECONDS)
    _admin.scan(bench)
    _admin.obtain_rights(bench)
    _admin.check_groups(bench, _admin.CRASH_REPORTS)
    # Deeper than the walk goes (`Limits::depth`): root's walk stops short of it.
    deep = common.Files(common.ROOT, [f"{DEEP}/{'/'.join(['d'] * DEEP_LEVELS)}/okapi-vm.crash"], size=1024)
    try:
        common.plant(bench, deep, age_seconds=_admin.AGING_SECONDS)
        _admin.clean(bench)
        bench.screenshot("arborescence-profonde")
        bench.check(bool(bench.ui.find(pattern=re.escape(PART_CHANGED))),
                    "ADM-012: the receipt says part of what root was to clean changed since the rights")
        bench.check(common.present(bench, deep) == list(deep.paths),
                    "ADM-012: nothing is removed from the tree grown since the rights beyond root's walk")
    finally:
        bench.stop_app()
        bench.root(shlex.join(["rm", "-rf", DEEP]))


def run(bench: Bench) -> None:
    crash, apt_log, core_dump = planted(bench)
    bench.allow_elevation_without_password()
    banner_counts_the_groups(bench)
    password_cancelled(bench)
    refused(bench)
    withdrawn_before_the_dialog(bench)
    settings_changed(bench, crash)
    cleaned_with_one_password(bench, crash, apt_log, core_dump)
    part_changed(bench)
