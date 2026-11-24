use std::io;

/// A process and the memory it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessMemory {
    pub pid: u32,
    /// Its name as the kernel keeps it (`comm`, cut at 15 bytes).
    pub name: String,
    /// Its resident memory, in bytes: shared pages are counted in every
    /// process that maps them.
    pub resident_bytes: u64,
    /// The path of its cgroup (`0::` line), if told.
    pub cgroup: Option<String>,
}

/// The processes that hold memory, and how many could not be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessList {
    pub processes: Vec<ProcessMemory>,
    pub unreadable: usize,
}

/// The processes holding memory (kernel threads hold none, and are left
/// out before anything else of them is read). A process gone between the
/// listing and its reading is left out unsaid; one that cannot be read, or
/// listed, is counted.
///
/// # Errors
///
/// The system's error when `/proc` cannot be listed;
/// [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn processes() -> io::Result<ProcessList> {
    linux::processes()
}

/// The processes holding memory: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn processes() -> io::Result<ProcessList> {
    super::unsupported()
}

/// The unified (v2) cgroup paths Okapi itself runs in, then each of its
/// ancestors up to the first process: closing any of them may end Okapi.
///
/// # Errors
///
/// The system's error reading one; [`io::ErrorKind::InvalidData`] when the
/// system has no unified hierarchy (cgroup v1) or a `stat` cannot be read;
/// [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn lineage_cgroups() -> io::Result<Vec<String>> {
    linux::lineage_cgroups()
}

/// Okapi's cgroups: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn lineage_cgroups() -> io::Result<Vec<String>> {
    super::unsupported()
}

/// The cgroup of the system's first process.
const FIRST_PROCESS_CGROUP: &str = "/init.scope";

/// Whether `cgroups` (a lineage, Okapi's first) is seen whole: up to the
/// system's first process, every path absolute below the root. In a cgroup
/// or pid namespace of its own, paths are relative (`/`, `/../..`) or the
/// lineage stops at the namespace's first process: where Okapi runs is not
/// known.
pub(crate) fn lineage_whole(cgroups: &[String]) -> bool {
    cgroups.last().map(String::as_str) == Some(FIRST_PROCESS_CGROUP)
        && cgroups
            .iter()
            .all(|cgroup| cgroup != "/" && cgroup.split('/').all(|part| part != ".."))
}

/// The parent's pid a `stat` line says (its fourth field, after the name in
/// parentheses, which may hold anything).
pub(crate) fn parent_pid(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The resident pages of a `statm` line (its second field).
pub(crate) fn resident_pages(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
}

/// The unified (v2) cgroup path of a `cgroup` file: its `0::` line.
pub(crate) fn unified_cgroup(text: &str) -> Option<&str> {
    text.lines().find_map(|line| line.strip_prefix("0::"))
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::Path;

    use super::super::capped::{read_capped, read_capped_bytes};
    use super::{
        ProcessList, ProcessMemory, io, lineage_whole, parent_pid, resident_pages, unified_cgroup,
    };

    const PROC: &str = "/proc";
    const STATM_LIMIT: usize = 512;
    const STAT_LIMIT: usize = 4096;
    const COMM_LIMIT: usize = 64;
    const CGROUP_LIMIT: usize = 16 * 1024;
    /// `ESRCH`: the process ended while being read.
    const NO_SUCH_PROCESS: i32 = rustix::io::Errno::SRCH.raw_os_error();

    pub(super) fn lineage_cgroups() -> io::Result<Vec<String>> {
        let proc = Path::new(PROC);
        let mut cgroups = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut directory = proc.join("self");
        loop {
            let text = read_capped(&directory.join("cgroup"), CGROUP_LIMIT)?;
            let cgroup = unified_cgroup(&text).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "no unified cgroup hierarchy")
            })?;
            cgroups.push(cgroup.to_owned());
            let stat = read_capped(&directory.join("stat"), STAT_LIMIT)?;
            let parent = parent_pid(&stat)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "stat"))?;
            if parent == 0 {
                return if lineage_whole(&cgroups) {
                    Ok(cgroups)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the lineage is not seen whole (a namespace of its own)",
                    ))
                };
            }
            if !seen.insert(parent) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a process met twice in the lineage",
                ));
            }
            directory = proc.join(parent.to_string());
        }
    }

    pub(super) fn processes() -> io::Result<ProcessList> {
        let page = u64::try_from(rustix::param::page_size()).unwrap_or(u64::MAX);
        processes_in(Path::new(PROC), page)
    }

    /// The processes of the `proc` filesystem at `root`, of pages of `page`
    /// bytes.
    fn processes_in(root: &Path, page: u64) -> io::Result<ProcessList> {
        let mut list = ProcessList::default();
        for entry in fs::read_dir(root)? {
            let Ok(entry) = entry else {
                list.unreadable += 1;
                continue;
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            match process(&root.join(pid.to_string()), pid, page) {
                Ok(Some(process)) => list.processes.push(process),
                Ok(None) => {}
                Err(error) if gone(&error) => {}
                Err(_) => list.unreadable += 1,
            }
        }
        Ok(list)
    }

    fn gone(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(NO_SUCH_PROCESS)
    }

    /// The process at `directory`; `None` when it holds no memory.
    fn process(directory: &Path, pid: u32, page: u64) -> io::Result<Option<ProcessMemory>> {
        let statm = read_capped(&directory.join("statm"), STATM_LIMIT)?;
        let pages = resident_pages(&statm)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "statm"))?;
        if pages == 0 {
            return Ok(None);
        }
        // Cut at 15 bytes by the kernel, perhaps within a character.
        let name =
            String::from_utf8_lossy(&read_capped_bytes(&directory.join("comm"), COMM_LIMIT)?)
                .trim_end()
                .to_owned();
        let cgroup = read_capped(&directory.join("cgroup"), CGROUP_LIMIT)?;
        Ok(Some(ProcessMemory {
            pid,
            name,
            resident_bytes: pages.saturating_mul(page),
            cgroup: unified_cgroup(&cgroup).map(str::to_owned),
        }))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_process_holding_no_memory_is_left_out_before_anything_else_is_read() {
            let directory = tempfile::tempdir().unwrap();
            fs::write(directory.path().join("statm"), "0 0 0 0 0 0 0\n").unwrap();
            // No `comm` nor `cgroup`: reading them would fail.
            assert_eq!(process(directory.path(), 7, 4_096).unwrap(), None);
        }

        #[test]
        fn a_process_is_read_its_pages_in_bytes_its_name_trimmed() {
            let directory = tempfile::tempdir().unwrap();
            fs::write(directory.path().join("statm"), "9 5 3 1 0 2 0\n").unwrap();
            fs::write(directory.path().join("comm"), "gnome-terminal-\n").unwrap();
            fs::write(
                directory.path().join("cgroup"),
                "0::/app.slice/app-x-1.scope\n",
            )
            .unwrap();
            assert_eq!(
                process(directory.path(), 7, 4_096).unwrap(),
                Some(ProcessMemory {
                    pid: 7,
                    name: "gnome-terminal-".to_owned(),
                    resident_bytes: 20_480,
                    cgroup: Some("/app.slice/app-x-1.scope".to_owned()),
                })
            );
        }

        #[test]
        fn every_process_is_read_a_gone_one_unsaid_an_unreadable_one_counted() {
            let root = tempfile::tempdir().unwrap();
            let process = |pid: &str, statm: Option<&str>| {
                let directory = root.path().join(pid);
                fs::create_dir(&directory).unwrap();
                if let Some(statm) = statm {
                    fs::write(directory.join("statm"), statm).unwrap();
                    fs::write(directory.join("comm"), "app\n").unwrap();
                    fs::write(directory.join("cgroup"), "0::/\n").unwrap();
                }
            };
            process("1", Some("9 5 3 1 0 2 0\n"));
            process("2", None);
            process("3", Some("x\n"));
            process("4", Some("9 0 0 0 0 0 0\n"));
            process("self", Some("9 5 3 1 0 2 0\n"));
            fs::write(root.path().join("cpuinfo"), "").unwrap();
            let list = processes_in(root.path(), 1).unwrap();
            assert_eq!(
                list.processes
                    .iter()
                    .map(|process| process.pid)
                    .collect::<Vec<_>>(),
                [1],
                "gone (2), holding nothing (4) and not processes left out"
            );
            assert_eq!(list.unreadable, 1, "3 could not be read");
            assert!(processes_in(&root.path().join("none"), 1).is_err());
        }

        #[test]
        fn a_name_cut_within_a_character_is_still_read() {
            let directory = tempfile::tempdir().unwrap();
            fs::write(directory.path().join("statm"), "9 5 3 1 0 2 0\n").unwrap();
            fs::write(directory.path().join("comm"), b"caf\xC3\n").unwrap();
            fs::write(directory.path().join("cgroup"), "0::/\n").unwrap();
            let process = process(directory.path(), 7, 1).unwrap().unwrap();
            assert_eq!(process.name, "caf\u{FFFD}");
        }

        #[test]
        fn a_process_gone_is_told_apart_from_one_unreadable() {
            assert!(gone(&io::ErrorKind::NotFound.into()));
            assert!(gone(&io::Error::from_raw_os_error(NO_SUCH_PROCESS)));
            assert!(!gone(&io::ErrorKind::PermissionDenied.into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resident_pages_and_the_unified_cgroup_are_read() {
        assert_eq!(
            resident_pages("1047552 21347 9216 18 0 33280 0\n"),
            Some(21_347)
        );
        assert_eq!(resident_pages("0 0 0 0 0 0 0"), Some(0));
        assert_eq!(resident_pages("12"), None);
        assert_eq!(
            unified_cgroup("0::/user.slice/app.slice/app-x-1.scope\n"),
            Some("/user.slice/app.slice/app-x-1.scope")
        );
        assert_eq!(unified_cgroup("12:pids:/\n"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_s_cgroups_are_its_own_then_its_ancestors() {
        let lineage = lineage_cgroups().unwrap();
        let listed = processes().unwrap().processes;
        let cgroup_of = |pid: u32| {
            listed
                .iter()
                .find(|process| process.pid == pid)
                .and_then(|process| process.cgroup.clone())
        };
        assert_eq!(cgroup_of(std::process::id()).as_ref(), lineage.first());
        // Up to the first process, whose cgroup is the system's.
        assert!(lineage.len() >= 2, "{lineage:?}");
        assert_eq!(lineage.last().map(String::as_str), Some("/init.scope"));
    }

    #[test]
    fn a_lineage_is_whole_only_up_to_the_system_s_first_process_seen_from_the_root() {
        let lineage =
            |paths: &[&str]| -> Vec<String> { paths.iter().map(|&path| path.to_owned()).collect() };
        assert!(lineage_whole(&lineage(&[
            "/user.slice/app-gnome-x-1.scope",
            "/init.scope"
        ])));
        // In a cgroup namespace of its own.
        assert!(!lineage_whole(&lineage(&["/", "/../../../init.scope"])));
        assert!(!lineage_whole(&lineage(&["/", "/init.scope"])));
        // In a pid namespace: it stops at that namespace's first process.
        assert!(!lineage_whole(&lineage(&[
            "/user.slice/app-gnome-x-1.scope",
            "/user.slice/app-gnome-x-1.scope"
        ])));
        assert!(!lineage_whole(&[]));
    }

    #[test]
    fn the_parent_is_read_past_a_name_that_holds_anything() {
        assert_eq!(parent_pid("1234 (cargo) S 987 1234 1234 0"), Some(987));
        assert_eq!(parent_pid("42 (a) b) c) R 7 42"), Some(7));
        assert_eq!(parent_pid("1 (systemd) S 0 1 1"), Some(0));
        assert_eq!(parent_pid("garbage"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_is_among_those_holding_memory() {
        let list = processes().unwrap();
        let this = list
            .processes
            .iter()
            .find(|process| process.pid == std::process::id())
            .unwrap();
        assert!(this.resident_bytes > 0);
        assert_ne!(this.name, "");
    }
}
