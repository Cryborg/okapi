use std::io;

#[cfg(target_os = "linux")]
use super::capped::read_capped;

#[cfg(target_os = "linux")]
const MEMINFO: &str = "/proc/meminfo";
#[cfg(target_os = "linux")]
const MEMINFO_LIMIT: usize = 64 * 1024;
/// `/proc/meminfo` counts in kibibytes.
const KIBIBYTE: u64 = 1024;

/// The memory of the machine, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryStatus {
    pub total: u64,
    /// What applications may still take without swapping, the page cache
    /// counted as free (what `free` says is available).
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

/// The memory of the machine.
///
/// # Errors
///
/// The system's error, or [`io::ErrorKind::InvalidData`] when the file is cut
/// or not UTF-8, or misses the total memory, the available one (before Linux
/// 3.14) or, with a swap, its free part; [`io::ErrorKind::Unsupported`]
/// outside Linux.
#[cfg(target_os = "linux")]
pub fn memory() -> io::Result<MemoryStatus> {
    parse_meminfo(&read_capped(std::path::Path::new(MEMINFO), MEMINFO_LIMIT)?)
}

/// The memory of the machine: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn memory() -> io::Result<MemoryStatus> {
    super::unsupported()
}

/// The memory a `meminfo` text tells.
pub(crate) fn parse_meminfo(text: &str) -> io::Result<MemoryStatus> {
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let (name, rest) = line.split_once(':')?;
            (name == key).then(|| {
                rest.split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
                    .map(|kibibytes| kibibytes.saturating_mul(KIBIBYTE))
            })?
        })
    };
    let required = |key: &str| {
        value(key).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("no `{key}` in meminfo"))
        })
    };
    let swap_total = value("SwapTotal").unwrap_or(0);
    Ok(MemoryStatus {
        total: required("MemTotal")?,
        available: required("MemAvailable")?,
        swap_total,
        // A swap whose free part is not told is never said full.
        swap_free: if swap_total == 0 {
            value("SwapFree").unwrap_or(0)
        } else {
            required("SwapFree")?
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_is_read_in_bytes_and_a_missing_swap_is_none() {
        let status = parse_meminfo(
            "MemTotal:       31931584 kB\nMemFree:  100 kB\nMemAvailable:   12826580 kB\n\
             SwapTotal:      24354296 kB\nSwapFree:        1652132 kB\n",
        )
        .unwrap();
        assert_eq!(
            status,
            MemoryStatus {
                total: 31_931_584 * 1024,
                available: 12_826_580 * 1024,
                swap_total: 24_354_296 * 1024,
                swap_free: 1_652_132 * 1024,
            }
        );
        let without_swap = parse_meminfo("MemTotal: 4 kB\nMemAvailable: 2 kB\n").unwrap();
        assert_eq!((without_swap.swap_total, without_swap.swap_free), (0, 0));
    }

    #[test]
    fn without_the_available_memory_or_the_free_swap_nothing_false_is_said() {
        for text in [
            "MemTotal: 4 kB\nMemFree: 2 kB\n",
            "MemAvailable: 4 kB\n",
            "MemTotal: 4 kB\nMemAvailable: 2 kB\nSwapTotal: 8 kB\n",
        ] {
            let error = parse_meminfo(text).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{text}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_memory_of_this_machine_holds_together() {
        let status = memory().unwrap();
        assert!(status.total > 0);
        assert!(status.available <= status.total);
    }
}
