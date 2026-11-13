use std::io;
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
use super::capped::read_capped;

/// Far beyond any of the small files read here.
#[cfg(target_os = "linux")]
const SMALL_FILE_LIMIT: usize = 64 * 1024;
/// `/proc/stat` grows with the processors and the interrupts.
#[cfg(target_os = "linux")]
const STAT_LIMIT: usize = 1024 * 1024;
#[cfg(target_os = "linux")]
const STAT: &str = "/proc/stat";
#[cfg(target_os = "linux")]
const LOAD_AVERAGE: &str = "/proc/loadavg";
#[cfg(target_os = "linux")]
const UPTIME: &str = "/proc/uptime";
#[cfg(target_os = "linux")]
const KERNEL_RELEASE: &str = "/proc/sys/kernel/osrelease";
/// Where the distribution describes itself, then its fallback.
#[cfg(target_os = "linux")]
const OS_RELEASE: [&str; 2] = ["/etc/os-release", "/usr/lib/os-release"];

/// The time the processors spent, in ticks since the boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    /// Spent working.
    pub busy: u64,
    /// Spent in all, idle included.
    pub total: u64,
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

/// The time all processors spent, from `/proc/stat`.
///
/// # Errors
///
/// The system's error, or [`io::ErrorKind::InvalidData`];
/// [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn cpu_times() -> io::Result<CpuTimes> {
    parse_cpu_times(&read_capped(Path::new(STAT), STAT_LIMIT)?)
}

/// The time all processors spent: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn cpu_times() -> io::Result<CpuTimes> {
    super::unsupported()
}

/// The first line of `/proc/stat`: `cpu user nice system idle iowait irq
/// softirq steal guest guest_nice`, at least the first four.
pub(crate) fn parse_cpu_times(text: &str) -> io::Result<CpuTimes> {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some("cpu"))
        .ok_or_else(|| invalid("no `cpu` line"))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| invalid("a `cpu` field is not a number"))?;
    if values.len() < 4 {
        return Err(invalid("fewer than four `cpu` fields"));
    }
    // `guest` and `guest_nice` (8 and 9) are already in `user` and `nice`.
    let counted = &values[..values.len().min(8)];
    let total = counted
        .iter()
        .fold(0_u64, |sum, value| sum.saturating_add(*value));
    let idle = values[3].saturating_add(values.get(4).copied().unwrap_or(0));
    Ok(CpuTimes {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// The load average over the last minute.
///
/// # Errors
///
/// The system's error, or [`io::ErrorKind::InvalidData`];
/// [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn load_average() -> io::Result<f64> {
    parse_load_average(&read_capped(Path::new(LOAD_AVERAGE), SMALL_FILE_LIMIT)?)
}

/// The load average: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn load_average() -> io::Result<f64> {
    super::unsupported()
}

pub(crate) fn parse_load_average(text: &str) -> io::Result<f64> {
    text.split_whitespace()
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| invalid("no load average"))
}

/// How long the machine has run since its boot.
///
/// # Errors
///
/// The system's error, or [`io::ErrorKind::InvalidData`];
/// [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn uptime() -> io::Result<Duration> {
    parse_uptime(&read_capped(Path::new(UPTIME), SMALL_FILE_LIMIT)?)
}

/// How long the machine has run: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn uptime() -> io::Result<Duration> {
    super::unsupported()
}

pub(crate) fn parse_uptime(text: &str) -> io::Result<Duration> {
    text.split_whitespace()
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .ok_or_else(|| invalid("no uptime"))
}

/// The name of the distribution (`Ubuntu 26.04.1 LTS`), `None` when it does
/// not tell one.
///
/// # Errors
///
/// The last file's error when neither can be read (the system's, or
/// [`io::ErrorKind::InvalidData`] for one cut or not UTF-8);
/// [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn distribution() -> io::Result<Option<String>> {
    let mut last_error = io::Error::from(io::ErrorKind::NotFound);
    for path in OS_RELEASE {
        match read_capped(Path::new(path), SMALL_FILE_LIMIT) {
            Ok(text) => return Ok(parse_pretty_name(&text)),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// The name of the distribution: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn distribution() -> io::Result<Option<String>> {
    super::unsupported()
}

/// `PRETTY_NAME` of an `os-release` text, its quotes and escapes removed.
pub(crate) fn parse_pretty_name(text: &str) -> Option<String> {
    let value = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("PRETTY_NAME="))?;
    let unquoted = value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|inner| inner.strip_suffix('\''))
        })
        .unwrap_or(value);
    let mut name = String::with_capacity(unquoted.len());
    let mut characters = unquoted.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            name.extend(characters.next());
        } else {
            name.push(character);
        }
    }
    Some(name).filter(|name| !name.is_empty())
}

/// The release of the running kernel (`7.0.0-42-generic`).
///
/// # Errors
///
/// The system's error, or [`io::ErrorKind::InvalidData`];
/// [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn kernel_release() -> io::Result<String> {
    read_capped(Path::new(KERNEL_RELEASE), SMALL_FILE_LIMIT).map(|text| text.trim().to_owned())
}

/// The release of the running kernel: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn kernel_release() -> io::Result<String> {
    super::unsupported()
}

/// The desktop the session runs and how it shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DesktopSession {
    /// `XDG_CURRENT_DESKTOP` in order, the most specific first (`ubuntu`,
    /// `GNOME`).
    pub desktops: Vec<String>,
    /// `wayland` or `x11`.
    pub session: Option<String>,
}

/// The desktop of this session, from its environment.
#[must_use]
pub fn desktop_session() -> DesktopSession {
    desktop_session_from(
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
    )
}

pub(crate) fn desktop_session_from(
    desktops: Option<&str>,
    session: Option<&str>,
) -> DesktopSession {
    DesktopSession {
        desktops: desktops
            .unwrap_or_default()
            .split(':')
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect(),
        session: session
            .filter(|session| !session.is_empty())
            .map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktops_keep_their_order_and_an_empty_session_is_none() {
        assert_eq!(
            desktop_session_from(Some("ubuntu:GNOME"), Some("wayland")),
            DesktopSession {
                desktops: vec!["ubuntu".to_owned(), "GNOME".to_owned()],
                session: Some("wayland".to_owned()),
            }
        );
        assert_eq!(
            desktop_session_from(Some(""), Some("")),
            DesktopSession::default()
        );
        assert_eq!(desktop_session_from(None, None), DesktopSession::default());
    }

    #[test]
    fn busy_time_leaves_idle_and_wait_out_and_never_counts_guests_twice() {
        // user 10, nice 20, system 30, idle 100, iowait 40, irq 1, softirq 2,
        // steal 3, guest 5, guest_nice 7.
        let times = parse_cpu_times("cpu  10 20 30 100 40 1 2 3 5 7\ncpu0 1 2 3 4\n").unwrap();
        assert_eq!(
            times,
            CpuTimes {
                busy: 66,
                total: 206
            }
        );
        let old = parse_cpu_times("cpu 10 20 30 100\n").unwrap();
        assert_eq!(
            old,
            CpuTimes {
                busy: 60,
                total: 160
            }
        );
        assert!(parse_cpu_times("cpu 1 2 3\n").is_err());
        assert!(parse_cpu_times("cpu0 1 2 3 4\n").is_err());
        assert!(parse_cpu_times("cpu 1 2 x 4\n").is_err());
    }

    #[test]
    fn a_load_average_or_an_uptime_that_is_not_a_finite_positive_number_is_refused() {
        assert!(
            (parse_load_average("1.91 2.82 3.99 2/2091 707953\n").unwrap() - 1.91).abs() < 1e-9
        );
        assert!(
            parse_load_average("0.00 0.00 0.00 1/100 5\n")
                .unwrap()
                .abs()
                < f64::EPSILON
        );
        for text in ["NaN 1 1", "inf 1 1", "-1 1 1", ""] {
            assert!(parse_load_average(text).is_err(), "{text}");
        }
        assert_eq!(
            parse_uptime("702777.58 9480406.86\n").unwrap(),
            Duration::from_millis(702_777_580)
        );
        for text in ["-3 1", "NaN 1", "x"] {
            assert!(parse_uptime(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_distribution_name_loses_its_quotes_and_escapes() {
        assert_eq!(
            parse_pretty_name("NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 26.04.1 LTS\"\n").as_deref(),
            Some("Ubuntu 26.04.1 LTS")
        );
        assert_eq!(
            parse_pretty_name("PRETTY_NAME='Arch Linux'").as_deref(),
            Some("Arch Linux")
        );
        assert_eq!(
            parse_pretty_name(r#"PRETTY_NAME="Say \"hi\" \$HOME""#).as_deref(),
            Some(r#"Say "hi" $HOME"#)
        );
        assert_eq!(
            parse_pretty_name("PRETTY_NAME=Gentoo").as_deref(),
            Some("Gentoo")
        );
        assert_eq!(parse_pretty_name("NAME=Ubuntu\n"), None);
        assert_eq!(parse_pretty_name("PRETTY_NAME=\"\""), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_processor_time_only_grows() {
        let first = cpu_times().unwrap();
        let second = cpu_times().unwrap();
        assert!(second.total >= first.total);
        assert!(second.busy >= first.busy);
        assert_ne!(kernel_release().unwrap(), "");
    }
}
