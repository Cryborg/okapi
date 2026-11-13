//! What the system says of itself, for the dashboard: mounts and their
//! space, block devices, memory, processes and their systemd units, the
//! applications' `.desktop` names, the processor and the system. Each reader
//! has a pure parser, tested on samples; those of `/proc`, `/sys`, `/dev` and
//! the mounts work on Linux and say [`std::io::ErrorKind::Unsupported`]
//! elsewhere (a `.desktop` file is read anywhere).

// Outside Linux, the parsers serve no reader (the tests still run them).
#![cfg_attr(
    all(not(target_os = "linux"), not(test)),
    expect(dead_code, reason = "the parsers serve the Linux readers")
)]

mod block;
mod capped;
mod desktop;
mod memory;
mod mounts;
mod processes;
mod space;
mod system;
mod units;

pub use block::{device_labels, removable};
pub use desktop::application_name;
pub use memory::{MemoryStatus, memory};
pub use mounts::{Device, Mount, MountClass, MountList, mounts};
pub use processes::{ProcessList, ProcessMemory, processes};
pub use space::{Space, space};
pub use system::{
    CpuTimes, DesktopSession, cpu_times, desktop_session, distribution, kernel_release,
    load_average, uptime,
};
pub use units::desktop_id;

/// A name with the `\xNN` escapes of systemd and udev decoded (`\x2d` for
/// `-`, `\x20` for a space).
fn decode_hex_escapes(name: &str) -> String {
    let raw = name.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let escaped = raw
            .get(index..index + 4)
            .filter(|chunk| {
                chunk.starts_with(b"\\x") && chunk[2..].iter().all(u8::is_ascii_hexdigit)
            })
            .and_then(|chunk| std::str::from_utf8(&chunk[2..]).ok())
            .and_then(|digits| u8::from_str_radix(digits, 16).ok());
        if let Some(byte) = escaped {
            bytes.push(byte);
            index += 4;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Whether `identifier` names a file in a folder, never a path elsewhere.
fn names_a_file(identifier: &str) -> bool {
    !identifier.is_empty()
        && !identifier.contains(['/', '\0'])
        && identifier != "."
        && identifier != ".."
}

#[cfg(not(target_os = "linux"))]
fn unsupported<T>() -> std::io::Result<T> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_escapes_are_decoded_and_a_cut_one_kept() {
        assert_eq!(decode_hex_escapes(r"My\x20Disk"), "My Disk");
        assert_eq!(decode_hex_escapes(r"google\x2dchrome"), "google-chrome");
        assert_eq!(decode_hex_escapes("Données"), "Données");
        assert_eq!(decode_hex_escapes(r"a\x2"), r"a\x2");
        assert_eq!(decode_hex_escapes(r"a\xzz"), r"a\xzz");
        assert_eq!(decode_hex_escapes(r"a\x+f"), r"a\x+f");
    }
}
