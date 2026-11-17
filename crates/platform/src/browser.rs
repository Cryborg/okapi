//! What a running browser leaves in its user-data directory, and how Okapi
//! closes one the user asked it to close.

/// The lock a Chromium-based browser holds in its user-data directory while
/// it runs: a link to `<host>-<pid>` (Linux and macOS).
pub const CHROMIUM_LOCK: &str = "SingletonLock";

/// The file of a Firefox-based browser's profile that a running browser
/// locks; it stays once the browser is closed, unlocked.
#[cfg(not(windows))]
pub const FIREFOX_LOCK: &str = ".parentlock";
#[cfg(windows)]
pub const FIREFOX_LOCK: &str = "parent.lock";

/// How a browser tells that it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockKind {
    /// A link there while it runs, to `<host>-<pid>` (Chromium).
    Link,
    /// [`Self::Link`] of a browser in a pid namespace of its own (a
    /// Flatpak): the number it names is not the system's, so a link there
    /// says the browser runs, though Okapi cannot name the process.
    SandboxedLink,
    /// A file kept, locked while it runs (Firefox): the lock, not the
    /// file, says it runs.
    FileLock,
}

/// The host and the process a lock's target names: `<host>-<pid>`, cut at
/// its last dash (a host name may hold some), the pid in digits only, above
/// zero and within a process number.
#[must_use]
fn lock_target(target: &str) -> Option<(&str, u32)> {
    let (host, pid) = target.rsplit_once('-')?;
    if host.is_empty() || pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let pid: u32 = pid.parse().ok()?;
    (pid > 0 && i32::try_from(pid).is_ok()).then_some((host, pid))
}

#[cfg(target_os = "linux")]
pub use linux::{Holder, RunningBrowser, UnknownHolder, holder, lock_is_live};

/// Whether `lock`, of `kind`, stands for a browser that runs. Without a way
/// to tell, a link there is taken as live, and a file lock always is (its
/// file stays when the browser closes).
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn lock_is_live(lock: &std::path::Path, kind: LockKind) -> bool {
    match kind {
        LockKind::FileLock => true,
        LockKind::Link | LockKind::SandboxedLink => {
            !matches!(std::fs::symlink_metadata(lock), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        }
    }
}

/// Who locks a file, as `/proc/locks` says.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileLocker {
    /// Nobody.
    Nobody,
    /// A holder that cannot be named: an open-file lock, a pid of 0,
    /// several processes, or only a process waiting.
    Unnamed,
    /// One process, the only holder.
    Process(u32),
}

/// Who locks the file of inode `inode`, as `locks` (the text of
/// `/proc/locks`) says. The device is not compared: `/proc/locks` gives the
/// file system's, which on btrfs is not the subvolume's that `stat` gives;
/// another file of the same inode can only make a browser taken as open,
/// never as closed.
#[cfg(any(target_os = "linux", test))]
fn file_locked_by(locks: &str, inode: u64) -> FileLocker {
    let mut waited = false;
    let mut holders = Vec::new();
    for line in locks.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // `<n>: <type> <mode> <access> <pid> <major>:<minor>:<inode> <start> <end>`;
        // a process waiting for the lock is a line `<n>: -> …`, beside its holder.
        let waiting = fields.get(1) == Some(&"->");
        let fields = if waiting { &fields[1..] } else { &fields[..] };
        let Some(file_inode) = fields.get(5).and_then(|file| file.rsplit(':').next()) else {
            continue;
        };
        if file_inode.parse::<u64>().ok() != Some(inode) {
            continue;
        }
        if waiting {
            waited = true;
        } else {
            holders.push(
                fields[4]
                    .parse::<u32>()
                    .ok()
                    .filter(|pid| *pid > 0 && i32::try_from(*pid).is_ok()),
            );
        }
    }
    match holders.split_first() {
        None if waited => FileLocker::Unnamed,
        None => FileLocker::Nobody,
        Some((Some(pid), others)) if others.iter().all(|other| *other == Some(*pid)) => {
            FileLocker::Process(*pid)
        }
        Some(_) => FileLocker::Unnamed,
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::io;
    use std::os::fd::OwnedFd;
    use std::path::{Path, PathBuf};

    use rustix::io::Errno;
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};

    use std::os::unix::fs::MetadataExt as _;

    use super::{FileLocker, LockKind, file_locked_by, lock_target};

    /// The name of this machine, as a lock names the one that holds it.
    const HOSTNAME: &str = "/proc/sys/kernel/hostname";
    /// Every lock on a file of this machine, and who holds it.
    const LOCKS: &str = "/proc/locks";
    /// How many reads of [`LOCKS`] say a file free before it is taken so:
    /// the system gives it out a page at a time, and a lock released between
    /// two pages makes the next one skip a lock still held.
    const LOCKS_READS: usize = 2;
    /// What Okapi's pid namespace is, and what it is when it is the system's
    /// first (`PROC_PID_INIT_INO`, fixed by the kernel): only there does
    /// [`LOCKS`] list the locks of every process.
    const PID_NAMESPACE: &str = "/proc/self/ns/pid";
    const INITIAL_PID_NAMESPACE: &str = "pid:[4026531836]";

    /// Whether [`LOCKS`] lists the locks of every process of the system.
    fn sees_every_lock() -> bool {
        match fs::read_link(PID_NAMESPACE) {
            Ok(namespace) if namespace.as_os_str() == INITIAL_PID_NAMESPACE => true,
            Ok(namespace) => {
                tracing::debug!(namespace = %namespace.display(), "Okapi in a pid namespace of its own: file locks hidden");
                false
            }
            Err(error) => {
                tracing::debug!(%error, "pid namespace of Okapi unreadable");
                false
            }
        }
    }

    /// Who holds a browser's lock.
    #[derive(Debug)]
    pub enum Holder {
        /// Nobody, or a browser gone (a lock left by a crash): it is closed.
        Absent,
        /// A process of this machine, checked to be that browser.
        Process(RunningBrowser),
        /// Someone Okapi cannot vouch for: never signalled.
        Unknown(UnknownHolder),
    }

    /// Why a lock's holder cannot be vouched for.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum UnknownHolder {
        /// The lock, the name of this machine or its holder cannot be read.
        Unreadable,
        /// Its target is not `<host>-<pid>`.
        OtherForm,
        /// It names another machine.
        OtherMachine,
        /// It names Okapi itself.
        Okapi,
        /// The process holds nothing below the browser's directory: its
        /// number went to another one, or its files cannot be read.
        NotTheBrowser,
        /// The file is locked, its holder unnamed: an open-file lock, a
        /// process gone, or several.
        NoProcess,
        /// Okapi runs in a namespace of its own (a container, a sandbox):
        /// the system hides the file locks of the processes outside it.
        Confined,
        /// The browser runs in a pid namespace of its own (a Flatpak): the
        /// process its link names is numbered there, not as Okapi sees it.
        Sandboxed,
    }

    /// A browser found running, held by a descriptor of its process: a
    /// signal through it never reaches another process that took its number.
    #[derive(Debug)]
    pub struct RunningBrowser {
        pid: u32,
        process: OwnedFd,
    }

    impl RunningBrowser {
        #[must_use]
        pub const fn pid(&self) -> u32 {
            self.pid
        }

        /// Asks the browser to close, as a session ending would: `SIGTERM`,
        /// never `SIGKILL`, so that it saves what it holds.
        ///
        /// # Errors
        ///
        /// The system's refusal (the process is not the user's).
        #[expect(
            clippy::disallowed_methods,
            reason = "the one signal Okapi sends, when the user asks to close a browser; the helper never may"
        )]
        pub fn terminate(&self) -> io::Result<()> {
            match pidfd_send_signal(&self.process, Signal::TERM) {
                Ok(()) => Ok(()),
                Err(Errno::SRCH) => {
                    tracing::debug!(pid = self.pid, "browser gone before the signal: closed");
                    Ok(())
                }
                Err(error) => Err(io::Error::from(error)),
            }
        }
    }

    /// What `lock` names.
    enum Read {
        Absent,
        /// A process of this machine.
        Named(u32),
        Unknown(UnknownHolder),
    }

    /// What `lock`, of `kind`, names. Its failures are traced at `debug`:
    /// the engine reads a lock before each entry, its caller says once what
    /// it found.
    fn read(lock: &Path, kind: LockKind) -> Read {
        match kind {
            LockKind::Link => read_link(lock),
            LockKind::SandboxedLink => read_sandboxed_link(lock),
            LockKind::FileLock => read_file_lock(lock),
        }
    }

    /// Whether the link `lock` of a browser in a sandbox is there: its
    /// target is never read, the number it names being the sandbox's.
    fn read_sandboxed_link(lock: &Path) -> Read {
        match fs::symlink_metadata(lock) {
            Ok(_) => {
                tracing::debug!(lock = %lock.display(), "browser lock of a sandbox");
                Read::Unknown(UnknownHolder::Sandboxed)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Read::Absent,
            Err(error) => {
                tracing::debug!(lock = %lock.display(), %error, "browser lock unreadable");
                Read::Unknown(UnknownHolder::Unreadable)
            }
        }
    }

    /// Who locks the file `lock`, as `/proc/locks` says: whatever the
    /// process's namespace, its number as this system sees it.
    fn read_file_lock(lock: &Path) -> Read {
        let metadata = match fs::symlink_metadata(lock) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Read::Absent,
            Err(error) => {
                tracing::debug!(lock = %lock.display(), %error, "browser lock unreadable");
                return Read::Unknown(UnknownHolder::Unreadable);
            }
        };
        if !metadata.file_type().is_file() {
            tracing::debug!(lock = %lock.display(), "browser lock not a file");
            return Read::Unknown(UnknownHolder::OtherForm);
        }
        if !sees_every_lock() {
            return Read::Unknown(UnknownHolder::Confined);
        }
        let mut locker = FileLocker::Nobody;
        for _ in 0..LOCKS_READS {
            let locks = match fs::read_to_string(LOCKS) {
                Ok(locks) => locks,
                Err(error) => {
                    tracing::debug!(%error, "locks of this system unreadable");
                    return Read::Unknown(UnknownHolder::Unreadable);
                }
            };
            locker = file_locked_by(&locks, metadata.ino());
            if locker != FileLocker::Nobody {
                break;
            }
        }
        tracing::debug!(lock = %lock.display(), inode = metadata.ino(), ?locker, "browser file lock");
        match locker {
            FileLocker::Nobody => Read::Absent,
            FileLocker::Unnamed => Read::Unknown(UnknownHolder::NoProcess),
            FileLocker::Process(pid) if pid == std::process::id() => {
                Read::Unknown(UnknownHolder::Okapi)
            }
            FileLocker::Process(pid) => Read::Named(pid),
        }
    }

    /// What the link `lock` names.
    fn read_link(lock: &Path) -> Read {
        let target = match fs::read_link(lock) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Read::Absent,
            Err(error) => {
                tracing::debug!(lock = %lock.display(), %error, "browser lock unreadable");
                return Read::Unknown(UnknownHolder::Unreadable);
            }
        };
        let Some((host, pid)) = target.to_str().and_then(lock_target) else {
            tracing::debug!(lock = %lock.display(), target = %target.display(), "browser lock of another form");
            return Read::Unknown(UnknownHolder::OtherForm);
        };
        let this_host = match fs::read_to_string(HOSTNAME) {
            Ok(this_host) => this_host,
            Err(error) => {
                tracing::debug!(%error, "name of this machine unreadable");
                return Read::Unknown(UnknownHolder::Unreadable);
            }
        };
        if host != this_host.trim() {
            tracing::debug!(lock = %lock.display(), host, "browser lock of another machine");
            return Read::Unknown(UnknownHolder::OtherMachine);
        }
        named(pid, sees_every_lock())
    }

    /// What a link of this machine naming `pid` says to an Okapi that sees
    /// every process of the system (`sees_every`) or only those of its own
    /// pid namespace (a container, a sandbox): there, a number it cannot
    /// open may still be a browser that runs outside.
    fn named(pid: u32, sees_every: bool) -> Read {
        if pid == std::process::id() {
            Read::Unknown(UnknownHolder::Okapi)
        } else if sees_every {
            Read::Named(pid)
        } else {
            Read::Unknown(UnknownHolder::Confined)
        }
    }

    /// A descriptor of process `pid`.
    enum Opened {
        Process(OwnedFd),
        /// No such process: the lock was left by a browser gone.
        Gone,
        /// The system could not tell (descriptors exhausted, no pidfd, a
        /// sandbox): the process may run.
        Unknown,
    }

    fn open(pid: u32) -> Opened {
        let Some(raw) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
            return Opened::Unknown;
        };
        match pidfd_open(raw, PidfdFlags::empty()) {
            Ok(process) => Opened::Process(process),
            Err(Errno::SRCH) => Opened::Gone,
            Err(error) => {
                tracing::debug!(pid, %error, "lock holder cannot be looked at");
                Opened::Unknown
            }
        }
    }

    /// Whether `lock`, of `kind`, stands for a browser that runs: absent,
    /// unlocked, or a link of this machine naming a process gone (a lock
    /// left by a crash), it does not; anything else does, a process that
    /// took the number included, and any link of a sandbox, whose number is
    /// not the system's. A file lock the system lists is held, its
    /// locker gone or not (a lock taken by `flock` lives on in the processes
    /// that inherited it).
    #[must_use]
    pub fn lock_is_live(lock: &Path, kind: LockKind) -> bool {
        match read(lock, kind) {
            Read::Absent => false,
            Read::Unknown(_) => true,
            Read::Named(_) if kind == LockKind::FileLock => true,
            Read::Named(pid) => !matches!(open(pid), Opened::Gone),
        }
    }

    /// Who holds `lock`, of `kind`, the lock of the browser whose user-data
    /// directory (a profile's, for a file lock) is `user_data`: a process is
    /// vouched for only when the lock names it, of this machine, and it holds
    /// a file open below `user_data`.
    #[must_use]
    pub fn holder(lock: &Path, kind: LockKind, user_data: &Path) -> Holder {
        let holder = match read(lock, kind) {
            Read::Absent => Holder::Absent,
            Read::Unknown(why) => Holder::Unknown(why),
            // The descriptor first: the checks below are then about the
            // process it holds, whatever number the system gives out
            // meanwhile.
            Read::Named(pid) => match open(pid) {
                Opened::Gone if kind == LockKind::FileLock => {
                    Holder::Unknown(UnknownHolder::NoProcess)
                }
                Opened::Gone => Holder::Absent,
                Opened::Unknown => Holder::Unknown(UnknownHolder::Unreadable),
                Opened::Process(process) if holds_below(pid, user_data) => {
                    Holder::Process(RunningBrowser { pid, process })
                }
                Opened::Process(_) => Holder::Unknown(UnknownHolder::NotTheBrowser),
            },
        };
        match &holder {
            Holder::Absent => tracing::info!(lock = %lock.display(), "browser closed"),
            Holder::Process(running) => {
                tracing::info!(pid = running.pid, user_data = %user_data.display(), "browser running");
            }
            Holder::Unknown(why) => {
                tracing::info!(lock = %lock.display(), ?why, "browser lock holder not vouched for");
            }
        }
        holder
    }

    /// Whether process `pid` has a file open below `directory`, compared by
    /// path components (`google-chrome-beta` is not below `google-chrome`).
    fn holds_below(pid: u32, directory: &Path) -> bool {
        let descriptors = match fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(descriptors) => descriptors,
            Err(error) => {
                tracing::info!(pid, %error, "open files of the lock holder unreadable");
                return false;
            }
        };
        // As the system names its open files: through the links of its path
        // as given (a snap's `current`), read now.
        let directory = fs::canonicalize(directory).unwrap_or_else(|error| {
            tracing::debug!(directory = %directory.display(), %error, "browser directory not resolved: compared as given");
            directory.to_path_buf()
        });
        // A descriptor closed while listed is no file held: skipped, said.
        descriptors
            .filter_map(|descriptor| {
                descriptor
                    .and_then(|descriptor| fs::read_link(descriptor.path()))
                    .inspect_err(|error| tracing::debug!(pid, %error, "open file of the lock holder unreadable"))
                    .ok()
            })
            .any(|open: PathBuf| open.starts_with(&directory))
    }

    #[cfg(test)]
    #[expect(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        reason = "a browser is played by a process these tests start, signal and stop"
    )]
    mod tests {
        use std::os::unix::fs::symlink;
        use std::os::unix::process::ExitStatusExt;
        use std::process::{Child, Command};
        use std::time::Duration;

        use super::super::CHROMIUM_LOCK;
        use super::*;

        fn this_host() -> String {
            fs::read_to_string(HOSTNAME).unwrap().trim().to_owned()
        }

        /// A process that holds `file` open, as a browser holds its profile,
        /// once it has opened it.
        fn holding(file: &Path) -> Child {
            let mut child = Command::new("sh")
                .args(["-c", "exec 3< \"$1\"; exec sleep 30", "sh"])
                .arg(file)
                .spawn()
                .unwrap();
            for _ in 0..500 {
                if holds_below(child.id(), file) {
                    return child;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("the shell never opened {}", file.display());
        }

        /// A temporary user-data directory, canonical, with a file in it.
        /// For the tests of a process a lock names: outside the system's
        /// first pid namespace, no number is taken for a process.
        fn in_the_systems_pid_namespace() {
            assert!(
                sees_every_lock(),
                "these tests need the system's first pid namespace"
            );
        }

        fn user_data() -> (tempfile::TempDir, PathBuf, PathBuf) {
            let directory = tempfile::tempdir().unwrap();
            let user_data = directory.path().canonicalize().unwrap();
            let file = user_data.join("Local State");
            fs::write(&file, "{}").unwrap();
            (directory, user_data, file)
        }

        fn lock_to(user_data: &Path, target: &str) -> PathBuf {
            let lock = user_data.join(CHROMIUM_LOCK);
            let _ = fs::remove_file(&lock);
            symlink(target, &lock).unwrap();
            lock
        }

        /// `child` once `file` is locked by `locker`, a process the lock
        /// names.
        fn once_locked(mut child: Child, file: &Path, locker: impl Fn(u32) -> bool) -> Child {
            in_the_systems_pid_namespace();
            for _ in 0..500 {
                if matches!(read(file, LockKind::FileLock), Read::Named(pid) if locker(pid)) {
                    return child;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{} never locked", file.display());
        }

        /// A process that locks `file` (by `flock`, where Firefox takes a
        /// POSIX lock: both are a line of `/proc/locks`, the POSIX one is
        /// read in `a_file_lock_names_its_holder_by_inode`), then runs on as
        /// `sleep` (`--no-fork`: no other process is left).
        fn locking(file: &Path) -> Child {
            let child = Command::new("flock")
                .arg("--no-fork")
                .arg(file)
                .args(["sleep", "30"])
                .spawn()
                .unwrap();
            let pid = child.id();
            once_locked(child, file, |locker| locker == pid)
        }

        /// A process that holds the lock of `file` that another, gone, took:
        /// the system names the one gone.
        fn inheriting(file: &Path) -> Child {
            let child = Command::new("sh")
                .args(["-c", "exec 9>>\"$1\"; flock 9; exec sleep 30", "sh"])
                .arg(file)
                .spawn()
                .unwrap();
            let pid = child.id();
            // Once the `flock` that took it is gone, not only done.
            once_locked(child, file, |locker| {
                locker != pid && matches!(open(locker), Opened::Gone)
            })
        }

        #[test]
        fn a_profile_locked_is_vouched_for_then_closed_its_file_kept() {
            let (_directory, profile, _file) = user_data();
            let lock = profile.join(super::super::FIREFOX_LOCK);
            fs::write(&lock, "").unwrap();
            assert!(!lock_is_live(&lock, LockKind::FileLock), "unlocked: closed");
            assert!(matches!(
                holder(&lock, LockKind::FileLock, &profile),
                Holder::Absent
            ));

            let mut browser = locking(&lock);
            assert!(lock_is_live(&lock, LockKind::FileLock));
            let Holder::Process(running) = holder(&lock, LockKind::FileLock, &profile) else {
                panic!("the locking process is vouched for");
            };
            assert_eq!(running.pid(), browser.id());
            let elsewhere = tempfile::tempdir().unwrap();
            assert!(
                matches!(
                    holder(
                        &lock,
                        LockKind::FileLock,
                        &elsewhere.path().canonicalize().unwrap()
                    ),
                    Holder::Unknown(UnknownHolder::NotTheBrowser)
                ),
                "never vouched for another profile's directory"
            );
            browser.kill().unwrap();
            browser.wait().unwrap();
            assert!(
                !lock_is_live(&lock, LockKind::FileLock),
                "closed, its file still there"
            );
            assert!(lock.exists());
        }

        #[test]
        fn a_file_lock_whose_locker_is_gone_is_held_while_listed() {
            let (_directory, profile, _file) = user_data();
            let lock = profile.join(super::super::FIREFOX_LOCK);
            fs::write(&lock, "").unwrap();
            let mut heir = inheriting(&lock);
            assert!(lock_is_live(&lock, LockKind::FileLock));
            assert!(matches!(
                holder(&lock, LockKind::FileLock, &profile),
                Holder::Unknown(UnknownHolder::NoProcess)
            ));
            heir.kill().unwrap();
            heir.wait().unwrap();
            assert!(!lock_is_live(&lock, LockKind::FileLock), "released");
        }

        #[test]
        fn a_file_lock_shared_by_two_processes_is_never_closed() {
            let (_directory, profile, _file) = user_data();
            let lock = profile.join(super::super::FIREFOX_LOCK);
            fs::write(&lock, "").unwrap();
            let sharing = || {
                Command::new("flock")
                    .args(["--shared", "--no-fork"])
                    .arg(&lock)
                    .args(["sleep", "30"])
                    .spawn()
                    .unwrap()
            };
            let mut first = sharing();
            let first_pid = first.id();
            first = once_locked(first, &lock, |locker| locker == first_pid);
            let mut second = sharing();
            for _ in 0..500 {
                if matches!(read(&lock, LockKind::FileLock), Read::Unknown(_)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(lock_is_live(&lock, LockKind::FileLock));
            let held = holder(&lock, LockKind::FileLock, &profile);
            for child in [&mut first, &mut second] {
                child.kill().unwrap();
                child.wait().unwrap();
            }
            assert!(
                matches!(held, Holder::Unknown(UnknownHolder::NoProcess)),
                "two holders: neither is offered to close"
            );
        }

        #[test]
        fn a_file_lock_that_cannot_be_read_is_never_closed() {
            use std::os::unix::fs::PermissionsExt as _;

            let (_directory, profile, _file) = user_data();
            let lock = profile.join(super::super::FIREFOX_LOCK);
            fs::write(&lock, "").unwrap();
            fs::set_permissions(&profile, fs::Permissions::from_mode(0o000)).unwrap();
            let readable = fs::symlink_metadata(&lock).is_ok();
            let live = lock_is_live(&lock, LockKind::FileLock);
            let held = holder(&lock, LockKind::FileLock, &profile);
            fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
            if readable {
                eprintln!("skipped: root reads the lock of a closed profile");
                return;
            }
            assert!(live);
            assert!(matches!(held, Holder::Unknown(UnknownHolder::Unreadable)));
        }

        #[test]
        fn a_link_read_in_a_pid_namespace_of_its_own_is_held_by_someone_unknown() {
            let other = std::process::id() + 1;
            assert!(matches!(named(other, true), Read::Named(pid) if pid == other));
            assert!(matches!(
                named(other, false),
                Read::Unknown(UnknownHolder::Confined)
            ));
            for sees_every in [true, false] {
                assert!(matches!(
                    named(std::process::id(), sees_every),
                    Read::Unknown(UnknownHolder::Okapi)
                ));
            }
        }

        #[test]
        fn a_browser_is_vouched_for_through_the_links_of_its_directory() {
            in_the_systems_pid_namespace();
            let (directory, user_data, file) = user_data();
            // A snap's `current`, which names the revision it runs from.
            let current = directory.path().join("current");
            symlink(&user_data, &current).unwrap();
            let mut browser = holding(&file);
            lock_to(&user_data, &format!("{}-{}", this_host(), browser.id()));
            let held = holder(&current.join(CHROMIUM_LOCK), LockKind::Link, &current);
            browser.kill().unwrap();
            browser.wait().unwrap();
            assert!(matches!(held, Holder::Process(_)), "{held:?}");
        }

        #[test]
        fn a_link_of_a_sandbox_that_cannot_be_read_is_held() {
            use std::os::unix::fs::PermissionsExt as _;

            let (_directory, user_data, _file) = user_data();
            let lock = lock_to(&user_data, "host-1");
            fs::set_permissions(&user_data, fs::Permissions::from_mode(0o000)).unwrap();
            let readable = fs::symlink_metadata(&lock).is_ok();
            let live = lock_is_live(&lock, LockKind::SandboxedLink);
            let held = holder(&lock, LockKind::SandboxedLink, &user_data);
            fs::set_permissions(&user_data, fs::Permissions::from_mode(0o700)).unwrap();
            if readable {
                eprintln!("skipped: root reads the lock");
                return;
            }
            assert!(live);
            assert!(matches!(held, Holder::Unknown(UnknownHolder::Unreadable)));
        }

        #[test]
        fn a_file_lock_held_by_okapi_itself_is_never_closed() {
            let (_directory, profile, _file) = user_data();
            let lock = profile.join(super::super::FIREFOX_LOCK);
            fs::write(&lock, "").unwrap();
            let held = fs::File::open(&lock).unwrap();
            held.lock().unwrap();
            assert!(lock_is_live(&lock, LockKind::FileLock));
            assert!(matches!(
                holder(&lock, LockKind::FileLock, &profile),
                Holder::Unknown(UnknownHolder::Okapi)
            ));
        }

        #[test]
        fn a_file_lock_that_is_not_a_file_is_never_closed() {
            let (_directory, profile, file) = user_data();
            let lock = profile.join(super::super::FIREFOX_LOCK);
            symlink(&file, &lock).unwrap();
            assert!(
                lock_is_live(&lock, LockKind::FileLock),
                "a link is never followed"
            );
            assert!(matches!(
                holder(&lock, LockKind::FileLock, &profile),
                Holder::Unknown(UnknownHolder::OtherForm)
            ));
            fs::remove_file(&lock).unwrap();
            fs::create_dir(&lock).unwrap();
            assert!(lock_is_live(&lock, LockKind::FileLock), "a directory");
        }

        #[test]
        fn a_link_of_a_sandbox_is_held_whatever_process_it_names() {
            let (_directory, user_data, _file) = user_data();
            let lock = user_data.join(CHROMIUM_LOCK);
            assert!(!lock_is_live(&lock, LockKind::SandboxedLink));
            assert!(matches!(
                holder(&lock, LockKind::SandboxedLink, &user_data),
                Holder::Absent
            ));
            in_the_systems_pid_namespace();
            // A number above any the system gives: the sandbox's own.
            let lock = lock_to(&user_data, &format!("{}-{}", this_host(), i32::MAX));
            assert!(
                !lock_is_live(&lock, LockKind::Link),
                "read as the system's, the browser would pass for closed"
            );
            assert!(lock_is_live(&lock, LockKind::SandboxedLink));
            assert!(matches!(
                holder(&lock, LockKind::SandboxedLink, &user_data),
                Holder::Unknown(UnknownHolder::Sandboxed)
            ));
            let lock = lock_to(&user_data, "not a host and a number");
            assert!(matches!(
                holder(&lock, LockKind::SandboxedLink, &user_data),
                Holder::Unknown(UnknownHolder::Sandboxed)
            ));
        }

        #[test]
        fn a_running_browser_is_vouched_for_then_closed() {
            in_the_systems_pid_namespace();
            let (_directory, user_data, file) = user_data();
            let lock = user_data.join(CHROMIUM_LOCK);
            assert!(matches!(
                holder(&lock, LockKind::Link, &user_data),
                Holder::Absent
            ));
            assert!(!lock_is_live(&lock, LockKind::Link));

            let mut browser = holding(&file);
            let lock = lock_to(&user_data, &format!("{}-{}", this_host(), browser.id()));
            assert!(lock_is_live(&lock, LockKind::Link));
            let Holder::Process(running) = holder(&lock, LockKind::Link, &user_data) else {
                panic!("not vouched for");
            };
            assert_eq!(running.pid(), browser.id());
            running.terminate().unwrap();
            assert_eq!(browser.wait().unwrap().signal(), Some(15), "SIGTERM");
            assert!(
                matches!(holder(&lock, LockKind::Link, &user_data), Holder::Absent),
                "a lock left behind"
            );
            assert!(!lock_is_live(&lock, LockKind::Link));
            assert!(
                running.terminate().is_ok(),
                "gone before the signal: closed"
            );
        }

        #[test]
        fn a_holder_okapi_cannot_vouch_for_is_unknown_and_says_why() {
            in_the_systems_pid_namespace();
            let (_directory, user_data, _) = user_data();
            let elsewhere = tempfile::tempdir().unwrap();
            let file = elsewhere.path().canonicalize().unwrap().join("other");
            fs::write(&file, "").unwrap();
            let mut other = holding(&file);
            let host = this_host();
            for (target, why) in [
                (
                    format!("{host}-{}", other.id()),
                    UnknownHolder::NotTheBrowser,
                ),
                (
                    format!("another-machine-{}", other.id()),
                    UnknownHolder::OtherMachine,
                ),
                (
                    format!("{host}-{}", std::process::id()),
                    UnknownHolder::Okapi,
                ),
                (String::from("no-pid-here"), UnknownHolder::OtherForm),
            ] {
                let lock = lock_to(&user_data, &target);
                assert!(
                    matches!(holder(&lock, LockKind::Link, &user_data), Holder::Unknown(found) if found == why),
                    "{target}"
                );
                assert!(
                    lock_is_live(&lock, LockKind::Link),
                    "{target}: never taken as closed"
                );
            }
            // Not a link at all: unreadable as one.
            let lock = user_data.join(CHROMIUM_LOCK);
            fs::remove_file(&lock).unwrap();
            fs::write(&lock, "").unwrap();
            assert!(matches!(
                holder(&lock, LockKind::Link, &user_data),
                Holder::Unknown(UnknownHolder::Unreadable)
            ));
            assert!(lock_is_live(&lock, LockKind::Link), "never taken as closed");
            other.kill().unwrap();
            other.wait().unwrap();
        }

        #[test]
        fn a_neighbour_directory_is_not_the_browser_s() {
            in_the_systems_pid_namespace();
            let parent = tempfile::tempdir().unwrap();
            let parent = parent.path().canonicalize().unwrap();
            let user_data = parent.join("google-chrome");
            let beta = parent.join("google-chrome-beta");
            fs::create_dir(&user_data).unwrap();
            fs::create_dir(&beta).unwrap();
            let file = beta.join("Local State");
            fs::write(&file, "").unwrap();
            let mut other = holding(&file);
            let lock = lock_to(&user_data, &format!("{}-{}", this_host(), other.id()));
            assert!(matches!(
                holder(&lock, LockKind::Link, &user_data),
                Holder::Unknown(UnknownHolder::NotTheBrowser)
            ));
            other.kill().unwrap();
            other.wait().unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_target_is_read_at_its_last_dash_in_digits_only() {
        assert_eq!(
            lock_target("wobbly-tu-4067915"),
            Some(("wobbly-tu", 4_067_915))
        );
        for wrong in [
            "host-+5",
            "host-0",
            "host-",
            "-12",
            "host-123abc",
            "host-4294967295",
            "host-2147483648",
            "no dash",
        ] {
            assert_eq!(lock_target(wrong), None, "{wrong}");
        }
        assert_eq!(
            lock_target("host-2147483647"),
            Some(("host", 2_147_483_647))
        );
    }

    #[test]
    fn an_absent_lock_is_never_live() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!lock_is_live(
            &directory.path().join(CHROMIUM_LOCK),
            LockKind::Link
        ));
    }

    /// `/proc/locks` as Firefox leaves it, beside other locks.
    const LOCKS: &str = "1: POSIX  ADVISORY  WRITE 1834851 103:02:29513108 1073741824 1073742335
60: POSIX  ADVISORY  WRITE 1835867 103:02:1181494 0 EOF
60: -> POSIX  ADVISORY  WRITE 4242 103:02:1181494 0 EOF
61: OFDLCK ADVISORY  READ  -1 103:02:777 0 EOF
62: FLOCK  ADVISORY  WRITE 0 10a:1f:555 0 EOF
63: FLOCK  ADVISORY  WRITE 11 103:02:888 0 EOF
64: POSIX  ADVISORY  WRITE 12 103:02:888 0 EOF
65: POSIX  ADVISORY  WRITE 7 00:2d:999 0 EOF
66: POSIX  ADVISORY  WRITE 8 103:02:444 0 99
66: FLOCK  ADVISORY  WRITE 8 103:02:444 0 EOF
67: -> POSIX  ADVISORY  WRITE 9 103:02:333 0 EOF
68: POSIX  ADVISORY  WRITE 2147483648 103:02:222 0 EOF
69: POSIX  ADVISORY  WRITE abc 103:02:111 0 EOF
70: POSIX  ADVISORY  WRITE 10 103:02:666 0 EOF
71: OFDLCK ADVISORY  WRITE -1 103:02:666 0 EOF
72: POSIX  ADVISORY  READ  7 103:02:5555 0 9
72: POSIX  ADVISORY  READ  7 103:02:5555 10 EOF
72: OFDLCK ADVISORY  READ  -1 103:02:5555 0 EOF
73: broken line
";

    #[test]
    fn a_file_lock_names_its_holder_by_inode() {
        assert_eq!(
            file_locked_by(LOCKS, 1_181_494),
            FileLocker::Process(1_835_867),
            "its holder, not the process that waits"
        );
        assert_eq!(
            file_locked_by(LOCKS, 777),
            FileLocker::Unnamed,
            "an open-file lock"
        );
        assert_eq!(
            file_locked_by(LOCKS, 555),
            FileLocker::Unnamed,
            "a pid of 0"
        );
        assert_eq!(
            file_locked_by(LOCKS, 888),
            FileLocker::Unnamed,
            "two holders"
        );
        assert_eq!(
            file_locked_by(LOCKS, 999),
            FileLocker::Process(7),
            "whatever the device (btrfs)"
        );
        assert_eq!(
            file_locked_by(LOCKS, 1_181_495),
            FileLocker::Nobody,
            "another inode"
        );
        assert_eq!(file_locked_by("", 1_181_494), FileLocker::Nobody);
        for part in [494, 88, 1_181] {
            assert_eq!(
                file_locked_by(LOCKS, part),
                FileLocker::Nobody,
                "{part}: a part of another inode"
            );
        }
        assert_eq!(
            file_locked_by(LOCKS, 444),
            FileLocker::Process(8),
            "two locks of one process"
        );
        assert_eq!(
            file_locked_by(LOCKS, 333),
            FileLocker::Unnamed,
            "a waiter alone: its holder was there"
        );
        for (inode, why) in [
            (222, "beyond a process number"),
            (111, "unreadable"),
            (666, "a process, then an open-file lock"),
            (5555, "a process twice, then an open-file lock"),
        ] {
            assert_eq!(file_locked_by(LOCKS, inode), FileLocker::Unnamed, "{why}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_absent_file_lock_is_never_live() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!lock_is_live(
            &directory.path().join(FIREFOX_LOCK),
            LockKind::FileLock
        ));
    }

    /// Without `/proc/locks`, a file lock is never known released.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_file_lock_is_always_live_where_it_cannot_be_read() {
        let directory = tempfile::tempdir().unwrap();
        assert!(lock_is_live(
            &directory.path().join(FIREFOX_LOCK),
            LockKind::FileLock
        ));
    }
}
