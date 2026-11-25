"""Rules: the trash, judged by each entry's `.trashinfo`, read at the analysis and again at the cleaning.

Files put in the trash by `gio trash`, 32 days ago and one 30 days ago, the
trash group unchecked and said gone for good; a minimum age of 33 days set
between the analysis and the cleaning keeps every entry (INV-208). With an
age of 24 h, the entries old enough go whole with their `.trashinfo`, the
one of 30 days stays (RGL-007); one redated between the analysis and the
cleaning stays (RGL-008); `.trashinfo` that is a link, too big, undated or
another's keep their entries (RGL-009); one restored between the two is
already gone, the information of another of its name trashed meanwhile left
(RGL-010).
"""

from __future__ import annotations

import shlex

from okapivm import cleaner, common
from okapivm.bench import Bench

TAGS = ["rules", "trash"]

DAY_SECONDS = 24 * 60 * 60
MINUTES_PER_DAY = 24 * 60
# The rule's 30 days, plus the day of margin a local date of `.trashinfo` asks for.
OLD_ENOUGH_DAYS = 32
YOUNG_DAYS = 30
# Between the two: an age of the user's stricter than the rule's.
STRICT_AGE_DAYS = 33
USER_AGE_DAYS = 1
SIZE = 4096
TMPFILES_TIMER = "systemd-tmpfiles-clean.timer"
ORIGIN = "~/okapi-vm-trash"
TRASH_FILES = "~/.local/share/Trash/files"
TRASH_INFO = "~/.local/share/Trash/info"
AGED = "aged.txt"
FOLDER = "folder"
REDATED = "redated.txt"
RESTORED = "restored.txt"
YOUNG = "young.txt"
LINKED = "trap-link.txt"
BIG = "trap-big.txt"
UNDATED = "trap-undated.txt"
FOREIGN = "trap-foreign.txt"
TRAPS = [LINKED, BIG, UNDATED, FOREIGN]
OFFERED = [AGED, FOLDER, REDATED, RESTORED]
# Above the 32 KiB a `.trashinfo` may weigh (crates/platform/src/trash.rs).
BIG_PADDING = 33 * 1024
LINK_TARGET = "~/okapi-vm-elsewhere.trashinfo"
TRASH = "Corbeille"
GONE_FOR_GOOD_PATTERN = r"Supprimé définitivement : il ne pourra plus être restauré\.$"
TRASH_WARNING = "Ce qui part de la corbeille ne pourra plus être restauré."
DESELECT = "Tout désélectionner"
FINISH = "Terminer"
GONE = "Déjà parties"
SYSTEM_JSON = "--system"


def entry(name: str) -> str:
    return f"{TRASH_FILES}/{name}"


def information(name: str) -> str:
    return f"{TRASH_INFO}/{name}.trashinfo"


def account_files(bench: Bench, *paths: str) -> common.Files:
    return common.Files(bench.accounts.admin.name, list(paths))


def trash(bench: Bench, *names: str) -> None:
    bench.user(" && ".join(f"gio trash -- {common.shell_path(f'{ORIGIN}/{name}')}" for name in names))
    bench.log(f"trashed {', '.join(names)}")


def set_trashed_at(bench: Bench, name: str, days_ago: int) -> None:
    """The deletion date of `name`'s `.trashinfo`, as gio writes it (local time)."""
    path = common.shell_path(information(name))
    bench.user(f'when=$(date --date=@$(( $(date +%s) - {days_ago * DAY_SECONDS} )) +%Y-%m-%dT%H:%M:%S) '
               f'&& sed -i "s/^DeletionDate=.*/DeletionDate=$when/" {path}')


def prepare(bench: Bench) -> None:
    bench.root(shlex.join(["systemctl", "stop", TMPFILES_TIMER]))
    files = [f"{ORIGIN}/{name}" for name in [AGED, REDATED, RESTORED, YOUNG, *TRAPS]]
    common.plant(bench, account_files(bench, *files, f"{ORIGIN}/{FOLDER}/inner.txt"))
    trash(bench, AGED, FOLDER, REDATED, RESTORED, *TRAPS)
    bench.advance_clock((OLD_ENOUGH_DAYS - YOUNG_DAYS) * DAY_SECONDS)
    trash(bench, YOUNG)
    bench.advance_clock(YOUNG_DAYS * DAY_SECONDS)
    linked = common.shell_path(information(LINKED))
    target = common.shell_path(LINK_TARGET)
    bench.user(f"mv -- {linked} {target} && ln -s -- {target} {linked}")
    bench.user(f'printf "%{BIG_PADDING}s\\n" "" >> {common.shell_path(information(BIG))}')
    bench.user(f"sed -i '/^DeletionDate=/d' {common.shell_path(information(UNDATED))}")
    home = common.expand_home(bench, "~/")
    bench.root(shlex.join(["chown", "root:root", f"{home}{information(FOREIGN).removeprefix('~/')}"]))


def offered(bench: Bench) -> list[str]:
    """The entries of the trash the CLI's analysis offers, by name."""
    document = common.cli_json(bench, SYSTEM_JSON)
    trash_root = common.expand_home(bench, TRASH_FILES) + "/"
    return sorted(item["path"].removeprefix(trash_root) for group in document["groups"]
                  for item in group["entries"] if item["path"].startswith(trash_root))


def results(bench: Bench) -> dict[str, dict]:
    """The entries of the newest cleaning's deletion log, by their name in the trash."""
    trash_root = common.expand_home(bench, TRASH_FILES) + "/"
    return {item["path"].removeprefix(trash_root): item for item in common.last_deletions(bench)
            if item.get("path", "").startswith(trash_root)}


def clean_trash_alone(bench: Bench) -> None:
    """The trash checked alone among the results, then cleaned; the confirmation warns of it."""
    bench.ui.click(DESELECT, role=cleaner.BUTTON)
    cleaner.check_groups(bench, TRASH)
    bench.ui.click(cleaner.CLEAN, role=cleaner.BUTTON)
    bench.ui.wait(cleaner.CONFIRMATION)
    bench.ui.wait(TRASH_WARNING)
    bench.screenshot("confirmation-corbeille")
    bench.ui.click(cleaner.UNDERSTOOD, role=cleaner.CHECK_BOX)
    bench.ui.click(cleaner.DELETE, role=cleaner.BUTTON)
    bench.ui.wait(cleaner.DONE, timeout=cleaner.CLEAN_TIMEOUT_SECONDS)
    bench.ui.wait(cleaner.RECORDED)


def stricter_age_at_the_cleaning(bench: Bench) -> None:
    names = offered(bench)
    bench.check(names == sorted(OFFERED),
                f"RGL-007, RGL-009: the analysis offers the entries trashed {OLD_ENOUGH_DAYS} days ago, not the one "
                f"of {YOUNG_DAYS} days nor the four trapped ({names})")
    cleaner.scan(bench)
    group = cleaner.row(bench, TRASH)
    cleaner.press_for(bench, cleaner.WHAT_GOES, TRASH)
    said = bench.ui.find(pattern=GONE_FOR_GOOD_PATTERN)
    bench.screenshot("corbeille")
    bench.check(cleaner.CHECKED not in group["states"] and bool(said),
                f"RGL-007: « {TRASH} » is offered unchecked, said gone for good ({group['states']}, {said})")
    common.write_settings(bench, {"deletionLog": True, "minAgeMinutes": STRICT_AGE_DAYS * MINUTES_PER_DAY})
    clean_trash_alone(bench)
    kept = results(bench)
    bench.log(f"deletion log of the stricter age: {kept}")
    common.check_kept(bench, account_files(bench, *(entry(name) for name in OFFERED),
                                           *(information(name) for name in OFFERED)))
    bench.check(sorted(kept) == sorted(OFFERED)
                and all(item["result"] == "kept" and item["reason"] == "recent" for item in kept.values()),
                f"INV-208: an age of {STRICT_AGE_DAYS} days set after the analysis keeps every entry of "
                f"{OLD_ENOUGH_DAYS} days at the cleaning ({kept})")
    bench.ui.click(FINISH, role=cleaner.BUTTON)


def run(bench: Bench) -> None:
    prepare(bench)
    common.write_settings(bench, {"deletionLog": True})
    stricter_age_at_the_cleaning(bench)

    common.write_settings(bench, {"deletionLog": True, "minAgeMinutes": USER_AGE_DAYS * MINUTES_PER_DAY})
    cleaner.scan(bench)
    group = cleaner.row(bench, TRASH)
    bench.check(cleaner.CHECKED not in group["states"],
                f"RGL-007: « {TRASH} » is never checked beforehand ({group['states']})")
    set_trashed_at(bench, REDATED, 0)
    bench.user(f"gio trash --restore {shlex.quote('trash:///' + RESTORED)}")
    # Another file of its name on its way to the trash: its information comes first.
    bench.user(f"cp -- {common.shell_path(information(AGED))} {common.shell_path(information(RESTORED))} "
               f"&& sed -i 's/{AGED}/{RESTORED}/' {common.shell_path(information(RESTORED))}")
    clean_trash_alone(bench)
    bench.screenshot("recu")
    done = results(bench)
    bench.log(f"deletion log: {done}")

    common.check_gone(bench, account_files(bench, entry(AGED), information(AGED), entry(FOLDER), information(FOLDER)))
    common.check_kept(bench, account_files(bench, entry(YOUNG), information(YOUNG)))
    bench.check(done.get(AGED, {}).get("result") == "removed" and done.get(FOLDER, {}).get("result") == "removed",
                f"RGL-007: with an age of {USER_AGE_DAYS} day, the entries trashed {OLD_ENOUGH_DAYS} days ago go "
                f"whole with their information, the one of {YOUNG_DAYS} days stays (INV-208) ({done})")
    common.check_kept(bench, account_files(bench, entry(REDATED), information(REDATED)))
    bench.check(done.get(REDATED, {}).get("reason") == "recent",
                f"RGL-008: the entry redated after the analysis stays, its information too ({done.get(REDATED)})")
    common.check_kept(bench, account_files(bench, *(entry(name) for name in TRAPS), *(information(name) for name in TRAPS)))
    bench.check(not set(TRAPS) & set(done),
                f"RGL-009: the entries of a linked, too big, undated or foreign `.trashinfo` are never offered, "
                f"all kept ({sorted(set(TRAPS) & set(done))})")
    common.check_kept(bench, account_files(bench, f"{ORIGIN}/{RESTORED}", information(RESTORED)))
    common.check_gone(bench, account_files(bench, entry(RESTORED)))
    shown_gone = bench.ui.find(pattern=f"^{GONE}")
    bench.check(done.get(RESTORED, {}).get("result") == "gone" and bool(shown_gone),
                f"RGL-010: the entry restored after the analysis is « {GONE} », back where it was, the new "
                f"information of its name left ({done.get(RESTORED)}, {shown_gone})")
