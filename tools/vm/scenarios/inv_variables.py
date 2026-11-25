"""Invariant 6: a root never holds a variable's folder, judged by identity.

By the CLI, each attack with old files that would go if it worked: the cache
folder unreadable stops everything (INV-605); `XDG_CACHE_HOME` under
`/var/tmp`, then seen through a bind mount of it (INV-603); a guest whose
home is under `/tmp` (INV-602); a `TMPDIR` that holds the session's runtime
folder (INV-604). Then pam_tmpdir turned on: `/tmp` is walked around
`/tmp/user`, the session's `TMPDIR` emptied by its own rule only, another
account's left alone, by the CLI and by the interface (INV-601).
"""

from __future__ import annotations

import json
import shlex

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["invariants", "variables", "mounts"]

# Beyond the 30 days of the temporary folders' rules.
AGING_SECONDS = 31 * 24 * 60 * 60
SIZE = 4096
# Would clean /tmp and /var/tmp behind Okapi's back once the clock jumps.
TMPFILES_TIMER = "systemd-tmpfiles-clean.timer"
VAR_TMP_BIND = "/mnt/okapi-vm-var-tmp"
GUEST_HOME = "/tmp/okapi-vm-guest"
SYSTEM_TEMP_FILES = "System Temp Files"
USER_TEMP_FILES = "User Temp Files"


def issues(document: dict, kind: str) -> list[str]:
    return [issue["detail"] for issue in document["issues"] if issue["detail"].startswith(kind)]


def protected(document: dict, path: str, variable: str) -> bool:
    return any(f'path: "{path}", variable: {variable} ' in detail for detail in issues(document, "ProtectedRoot"))


def removed(document: dict) -> set[str]:
    return {outcome["path"] for outcome in document["clean"]["outcomes"] if outcome["result"] == "removed"}


def group_entries(document: dict, label: str) -> list[str]:
    return [entry["path"] for group in document["groups"] if group["label"] == label for entry in group["entries"]]


def unreadable_cache(bench: Bench, base: str) -> None:
    kept = common.Files(bench.accounts.admin.name, ["/var/tmp/okapi-vm-605.tmp", "~/.cache/thumbnails/normal/605.png"],
                        SIZE)
    common.plant(bench, kept, age_seconds=AGING_SECONDS)
    outcome = common.cli(bench, common.JSON_OPTION, "--clean",
                         environment={"XDG_CACHE_HOME": "/root/okapi-vm-cache", "HOME": base})
    bench.log(f"stderr: {outcome.stderr.strip()}")
    document = json.loads(outcome.stdout)
    stops = [issue["detail"] for issue in document["issues"] if issue["stops"]]
    bench.check(outcome.code != 0 and any("UnreadableVariable { variable: Cache" in stop for stop in stops)
                and "clean" not in document and not document["groups"],
                f"INV-605: an unreadable cache folder stops everything, nothing scanned nor cleaned ({stops})")
    common.check_kept(bench, kept)


def cache_under_var_tmp(bench: Bench, base: str, session_temporary: str) -> None:
    user = bench.accounts.admin.name
    loose = common.Files(user, ["/var/tmp/okapi-vm-603.tmp"], SIZE)
    common.bind_mount(bench, "/var/tmp", VAR_TMP_BIND)
    for cache, seen in (("/var/tmp/okapi-vm-cache", "as written"), (f"{VAR_TMP_BIND}/okapi-vm-cache", "through a bind")):
        thumbnail = common.Files(user, [f"/var/tmp/okapi-vm-cache/thumbnails/normal/{len(seen)}.png"], SIZE)
        common.plant(bench, loose, thumbnail, age_seconds=AGING_SECONDS)
        document = common.cli_json(bench, "--clean", environment={
            "XDG_CACHE_HOME": cache, "HOME": base, "TMPDIR": session_temporary})
        bench.check(protected(document, "/var/tmp", "Cache"),
                    f"INV-603: /var/tmp, holding XDG_CACHE_HOME {seen}, is refused ({issues(document, 'Protected')})")
        bench.check(not any(path.startswith("/var/tmp/") for path in group_entries(document, "Persistent Temp Files")),
                    f"INV-603: nothing of /var/tmp is offered, XDG_CACHE_HOME {seen}")
        common.check_kept(bench, loose)
        bench.check(any(path.startswith(f"{cache}/thumbnails") for path in removed(document)),
                    f"INV-603: the rest of the analysis is done, the cache's thumbnails cleaned ({seen})")
    common.unmount(bench, VAR_TMP_BIND)


def guest_home_under_tmp(bench: Bench) -> None:
    other = bench.accounts.other.name
    loose = common.Files(other, ["/tmp/okapi-vm-602.tmp"], SIZE)
    thumbnail = common.Files(other, [f"{GUEST_HOME}/.cache/thumbnails/normal/602.png"], SIZE)
    bench.other(f"mkdir -p {GUEST_HOME}/tmp")
    common.plant(bench, loose, thumbnail, age_seconds=AGING_SECONDS)
    stopped = common.cli(bench, common.JSON_OPTION, "--clean", account=other,
                         environment={"HOME": GUEST_HOME, "TMPDIR": "/tmp"})
    bench.log(f"stderr: {stopped.stderr.strip()}")
    bench.check(stopped.code != 0 and not stopped.stdout.strip() and "CONFIG" in stopped.stderr,
                "INV-602: a home under /tmp, the temporary folder, stops the run before anything is read")
    common.check_kept(bench, loose, thumbnail)
    document = common.cli_json(bench, "--clean", account=other,
                               environment={"HOME": GUEST_HOME, "TMPDIR": f"{GUEST_HOME}/tmp"})
    bench.check(protected(document, "/tmp", "Home"),
                f"INV-602: /tmp, holding the guest's home, is refused ({issues(document, 'Protected')})")
    bench.check(not group_entries(document, SYSTEM_TEMP_FILES), "INV-602: nothing of /tmp is offered")
    common.check_kept(bench, loose)
    bench.check(f"{GUEST_HOME}/.cache/thumbnails/normal" in removed(document),
                "INV-602: the rest of the analysis is done, the guest's thumbnails cleaned")


def temporary_holding_runtime(bench: Bench, base: str, uid: str) -> None:
    runtime = f"/run/user/{uid}"
    canary = common.Files(bench.accounts.admin.name, [f"{runtime}/okapi-vm-604.tmp"], SIZE)
    thumbnail = common.Files(bench.accounts.admin.name, ["~/.cache/thumbnails/normal/604.png"], SIZE)
    common.plant(bench, canary, thumbnail, age_seconds=AGING_SECONDS)
    document = common.cli_json(bench, "--clean", environment={
        "TMPDIR": "/run/user", "XDG_RUNTIME_DIR": runtime, "HOME": base})
    bench.check(protected(document, "/run/user", "RuntimeDirectory"),
                f"INV-604: the rule of TMPDIR, holding XDG_RUNTIME_DIR, is set aside ({issues(document, 'Protected')})")
    bench.check(not [issue for issue in document["issues"] if issue["stops"]]
                and f"{base}/.cache/thumbnails/normal" in removed(document),
                "INV-604: the rest of the analysis is done, the thumbnails cleaned")
    common.check_kept(bench, canary)


def pam_tmpdir(bench: Bench, base: str, uid: str) -> None:
    user, other = bench.accounts.admin.name, bench.accounts.other.name
    common.enable_pam_tmpdir(bench)
    environment = bench.user("systemctl --user show-environment").splitlines()
    session_temporary = f"/tmp/user/{uid}"
    bench.check(f"TMPDIR={session_temporary}" in environment, "INV-601: pam_tmpdir gives the session its TMPDIR")
    # A session of the other account makes its own folder, as pam_tmpdir does at each login.
    bench.root(shlex.join(["su", "--login", other, "--command", "true"]))
    other_uid = bench.other("id -u").strip()
    others = common.Files(other, [f"/tmp/user/{other_uid}/okapi-vm-601.tmp"], SIZE)
    for attempt in ("cli", "interface"):
        loose = common.Files(user, [f"/tmp/okapi-vm-601-{attempt}.tmp"], SIZE)
        own = common.Files(user, [f"{session_temporary}/okapi-vm-601-{attempt}.tmp"], SIZE)
        common.plant(bench, loose, own, others, age_seconds=AGING_SECONDS)
        if attempt == "cli":
            outcome = common.cli(bench, common.JSON_OPTION, "--clean",
                                 environment={"TMPDIR": session_temporary, "HOME": base})
            document = json.loads(outcome.stdout)
            system = group_entries(document, SYSTEM_TEMP_FILES)
            bench.log(f"System Temp Files offered: {system}")
            bench.check(loose.paths[0] in system and not any(path.startswith("/tmp/user") for path in system),
                        "INV-601: /tmp is walked around /tmp/user, never entering it")
            bench.check(own.paths[0] in group_entries(document, USER_TEMP_FILES),
                        "INV-601: the session's TMPDIR is offered by its own rule")
            outcomes = document["clean"]["outcomes"]
        else:
            cleaner.scan(bench)
            cleaner.clean(bench)
            bench.screenshot("recu-pam-tmpdir")
            outcomes = common.last_deletions(bench)
        gone = {entry["path"] for entry in outcomes if entry.get("result") == "removed"}
        bench.check(loose.paths[0] in gone, f"INV-601: the old file of /tmp is cleaned by the {attempt}")
        bench.check(not any(path.startswith("/tmp/user") for path in gone - set(own.paths)),
                    f"INV-601: nothing else under /tmp/user is cleaned by the {attempt}")
        common.check_kept(bench, others)
        bench.root(shlex.join(["test", "-d", session_temporary]))
        bench.log(f"ok: INV-601: /tmp/user and {session_temporary} are left ({attempt})")
        left = [entry for entry in outcomes if entry.get("path") in own.paths]
        bench.check(own.paths[0] in gone and not common.present(bench, own),
                    f"INV-601: the session's TMPDIR, past /tmp/user (mode 711), is emptied by the {attempt} ({left})")


def run(bench: Bench) -> None:
    bench.root(shlex.join(["systemctl", "stop", TMPFILES_TIMER]))
    common.write_settings(bench, {"deletionLog": True})
    base = f"/home/{bench.accounts.admin.name}"
    uid = bench.user("id -u").strip()
    unreadable_cache(bench, base)
    cache_under_var_tmp(bench, base, "/tmp")
    guest_home_under_tmp(bench)
    temporary_holding_runtime(bench, base, uid)
    pam_tmpdir(bench, base, uid)
