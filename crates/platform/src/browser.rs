//! What a running browser leaves in its user-data directory, and how Okapi
//! closes one the user asked it to close.

/// The lock a Chromium-based browser holds in its user-data directory while
/// it runs: a link to `<host>-<pid>` (Linux and macOS).
pub const CHROMIUM_LOCK: &str = "SingletonLock";

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

/// Whether `lock` stands for a browser that runs: there, and not left by a
/// browser gone. Without a way to tell, a lock there is taken as live.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn lock_is_live(lock: &std::path::Path) -> bool {
    !matches!(std::fs::symlink_metadata(lock), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::io;
    use std::os::fd::OwnedFd;
    use std::path::{Path, PathBuf};

    use rustix::io::Errno;
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};

    use super::lock_target;

    /// The name of this machine, as a lock names the one that holds it.
    const HOSTNAME: &str = "/proc/sys/kernel/hostname";

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

    /// What `lock` names. Its failures are traced at `debug`: the engine
    /// reads a lock before each entry, its caller says once what it found.
    fn read(lock: &Path) -> Read {
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
        if pid == std::process::id() {
            return Read::Unknown(UnknownHolder::Okapi);
        }
        Read::Named(pid)
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

    /// Whether `lock` stands for a browser that runs: absent, or of this
    /// machine and naming a process gone (a lock left by a crash), it does
    /// not; anything else does, a process that took the number included.
    #[must_use]
    pub fn lock_is_live(lock: &Path) -> bool {
        match read(lock) {
            Read::Absent => false,
            Read::Unknown(_) => true,
            Read::Named(pid) => !matches!(open(pid), Opened::Gone),
        }
    }

    /// Who holds `lock`, the lock of the browser whose user-data directory
    /// is `user_data`: a process is vouched for only when the lock names this
    /// machine and it, and it holds a file open below `user_data`.
    #[must_use]
    pub fn holder(lock: &Path, user_data: &Path) -> Holder {
        let holder = match read(lock) {
            Read::Absent => Holder::Absent,
            Read::Unknown(why) => Holder::Unknown(why),
            // The descriptor first: the checks below are then about the
            // process it holds, whatever number the system gives out
            // meanwhile.
            Read::Named(pid) => match open(pid) {
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
        // A descriptor closed while listed is no file held: skipped, said.
        descriptors
            .filter_map(|descriptor| {
                descriptor
                    .and_then(|descriptor| fs::read_link(descriptor.path()))
                    .inspect_err(|error| tracing::debug!(pid, %error, "open file of the lock holder unreadable"))
                    .ok()
            })
            .any(|open: PathBuf| open.starts_with(directory))
    }

    #[cfg(test)]
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

        #[test]
        fn a_running_browser_is_vouched_for_then_closed() {
            let (_directory, user_data, file) = user_data();
            let lock = user_data.join(CHROMIUM_LOCK);
            assert!(matches!(holder(&lock, &user_data), Holder::Absent));
            assert!(!lock_is_live(&lock));

            let mut browser = holding(&file);
            let lock = lock_to(&user_data, &format!("{}-{}", this_host(), browser.id()));
            assert!(lock_is_live(&lock));
            let Holder::Process(running) = holder(&lock, &user_data) else {
                panic!("not vouched for");
            };
            assert_eq!(running.pid(), browser.id());
            running.terminate().unwrap();
            assert_eq!(browser.wait().unwrap().signal(), Some(15), "SIGTERM");
            assert!(
                matches!(holder(&lock, &user_data), Holder::Absent),
                "a lock left behind"
            );
            assert!(!lock_is_live(&lock));
            assert!(
                running.terminate().is_ok(),
                "gone before the signal: closed"
            );
        }

        #[test]
        fn a_holder_okapi_cannot_vouch_for_is_unknown_and_says_why() {
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
                    matches!(holder(&lock, &user_data), Holder::Unknown(found) if found == why),
                    "{target}"
                );
                assert!(lock_is_live(&lock), "{target}: never taken as closed");
            }
            // Not a link at all: unreadable as one.
            let lock = user_data.join(CHROMIUM_LOCK);
            fs::remove_file(&lock).unwrap();
            fs::write(&lock, "").unwrap();
            assert!(matches!(
                holder(&lock, &user_data),
                Holder::Unknown(UnknownHolder::Unreadable)
            ));
            assert!(lock_is_live(&lock), "never taken as closed");
            other.kill().unwrap();
            other.wait().unwrap();
        }

        #[test]
        fn a_neighbour_directory_is_not_the_browser_s() {
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
                holder(&lock, &user_data),
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
        assert!(!lock_is_live(&directory.path().join(CHROMIUM_LOCK)));
    }
}
