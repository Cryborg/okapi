use std::io;
use std::path::{Path, PathBuf};

/// Where udev names block devices by the label of their filesystem.
#[cfg(target_os = "linux")]
const BY_LABEL: &str = "/dev/disk/by-label";
/// Where sysfs lists block devices by their number.
#[cfg(target_os = "linux")]
const SYS_BY_NUMBER: &str = "/sys/dev/block";
/// How many device-mapper layers are followed down (LUKS over LVM...).
const MAPPER_DEPTH: usize = 4;

/// The labels of block devices, each with its device path (resolved):
/// `(/dev/nvme0n1p4, "Données")`. Empty when there is no such folder.
///
/// # Errors
///
/// The system's error; [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn device_labels() -> io::Result<Vec<(PathBuf, String)>> {
    let entries = match std::fs::read_dir(BY_LABEL) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut labels = Vec::new();
    // One entry that cannot be read (a device going away) never hides the
    // others: its volume is then named by where it is mounted.
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(super::decode_hex_escapes) else {
            continue;
        };
        if let Ok(device) = std::fs::canonicalize(entry.path()) {
            labels.push((device, name));
        }
    }
    Ok(labels)
}

/// The labels of block devices: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn device_labels() -> io::Result<Vec<(PathBuf, String)>> {
    super::unsupported()
}

/// Whether the block device at `source` (`/dev/sdb1`, `/dev/mapper/backup`)
/// can be unplugged: on a USB bus, or a disk the kernel says removable. A
/// device-mapper device is judged by the disks below it.
///
/// # Errors
///
/// The system's error; [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn removable(source: &Path) -> io::Result<bool> {
    removable_in_sysfs(&sysfs_entry(source)?, MAPPER_DEPTH)
}

/// Whether a block device can be unplugged: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn removable(_source: &Path) -> io::Result<bool> {
    super::unsupported()
}

/// Whether the block device at `source` is a rotating disk, where reading
/// several files at once seeks back and forth; a device-mapper device is one
/// when a disk below it is.
///
/// # Errors
///
/// The system's error (a disk that does not say); [`io::ErrorKind::Unsupported`]
/// outside Linux.
#[cfg(target_os = "linux")]
pub fn rotational(source: &Path) -> io::Result<bool> {
    rotational_in_sysfs(&sysfs_entry(source)?, MAPPER_DEPTH)
}

/// Whether a block device is a rotating disk: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn rotational(_source: &Path) -> io::Result<bool> {
    super::unsupported()
}

/// The sysfs entry of the block device at `source`, by its number.
#[cfg(target_os = "linux")]
fn sysfs_entry(source: &Path) -> io::Result<PathBuf> {
    let device = rustix::fs::stat(source)?.st_rdev;
    let number = format!(
        "{}:{}",
        rustix::fs::major(device),
        rustix::fs::minor(device)
    );
    Ok(Path::new(SYS_BY_NUMBER).join(number))
}

/// Whether the device whose sysfs entry is `entry` can be unplugged.
pub(crate) fn removable_in_sysfs(entry: &Path, depth: usize) -> io::Result<bool> {
    // Too deep to tell: fixed, as an internal disk is.
    any_disk(entry, depth, false, &|disk| {
        if on_usb(disk) {
            return Ok(true);
        }
        match std::fs::read_to_string(disk.join("removable")) {
            Ok(flag) => Ok(flag.trim() == FLAG_SET),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    })
}

/// Whether the device whose sysfs entry is `entry` is a rotating disk.
pub(crate) fn rotational_in_sysfs(entry: &Path, depth: usize) -> io::Result<bool> {
    // Too deep to tell: rotating, the safe guess for how many threads read.
    any_disk(entry, depth, true, &|disk| {
        std::fs::read_to_string(disk.join("queue/rotational")).map(|flag| flag.trim() == FLAG_SET)
    })
}

/// What sysfs writes for a flag that is set.
const FLAG_SET: &str = "1";

/// Whether `judge` says yes of a disk the device whose sysfs entry is
/// `entry` lies on: the disk of a partition, each disk below a
/// device-mapper device (no deeper than `depth` layers, `too_deep` past it).
fn any_disk(
    entry: &Path,
    depth: usize,
    too_deep: bool,
    judge: &dyn Fn(&Path) -> io::Result<bool>,
) -> io::Result<bool> {
    let resolved = std::fs::canonicalize(entry)?;
    // Every disk has a `slaves` folder, empty unless it is made of others.
    let slaves = resolved.join("slaves");
    let layered = std::fs::read_dir(&slaves).is_ok_and(|mut entries| entries.next().is_some());
    if layered {
        if depth == 0 {
            return Ok(too_deep);
        }
        for slave in std::fs::read_dir(&slaves)? {
            if any_disk(&slave?.path(), depth - 1, too_deep, judge)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    // A partition's disk is its parent.
    let disk = if resolved.join("partition").exists() {
        resolved
            .parent()
            .map_or(resolved.clone(), Path::to_path_buf)
    } else {
        resolved
    };
    judge(&disk)
}

/// Whether a resolved sysfs path goes through a USB bus.
fn on_usb(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.starts_with("usb"))
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    /// A sysfs of a USB key (partition), an internal disk (partition), a
    /// device-mapper volume over the key, and a removable card reader.
    #[expect(
        clippy::unwrap_used,
        reason = "a test helper fails its test by panicking"
    )]
    fn sysfs() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let devices = root.path().join("devices");
        let key = devices.join("pci0000:00/usb1/1-1/host6/block/sdb");
        let nvme = devices.join("pci0000:00/nvme/nvme0/nvme0n1");
        let card = devices.join("platform/mmc/block/mmcblk0");
        let mapper = devices.join("virtual/block/dm-0");
        let usb_disk = devices.join("pci0000:00/usb2/2-1/host7/block/sdc");
        fs::create_dir_all(usb_disk.join("sdc1")).unwrap();
        fs::write(usb_disk.join("sdc1/partition"), "1").unwrap();
        fs::write(usb_disk.join("removable"), "0\n").unwrap();
        fs::create_dir_all(card.join("mmcblk0p1")).unwrap();
        fs::write(card.join("mmcblk0p1/partition"), "1").unwrap();
        for disk in [&key, &nvme, &card] {
            fs::create_dir_all(disk).unwrap();
        }
        fs::create_dir_all(key.join("sdb1")).unwrap();
        fs::write(key.join("sdb1/partition"), "1").unwrap();
        fs::write(key.join("removable"), "1\n").unwrap();
        fs::create_dir_all(nvme.join("nvme0n1p4")).unwrap();
        fs::write(nvme.join("nvme0n1p4/partition"), "4").unwrap();
        fs::write(nvme.join("removable"), "0\n").unwrap();
        fs::write(card.join("removable"), "1\n").unwrap();
        // As sysfs gives every disk.
        fs::create_dir_all(card.join("slaves")).unwrap();
        fs::create_dir_all(mapper.join("slaves")).unwrap();
        symlink(key.join("sdb1"), mapper.join("slaves/sdb1")).unwrap();
        let numbers = root.path().join("dev-block");
        fs::create_dir(&numbers).unwrap();
        symlink(key.join("sdb1"), numbers.join("8:17")).unwrap();
        symlink(nvme.join("nvme0n1p4"), numbers.join("259:4")).unwrap();
        symlink(&card, numbers.join("179:0")).unwrap();
        symlink(&mapper, numbers.join("253:0")).unwrap();
        symlink(usb_disk.join("sdc1"), numbers.join("8:33")).unwrap();
        symlink(card.join("mmcblk0p1"), numbers.join("179:1")).unwrap();
        (root, numbers)
    }

    #[test]
    fn a_usb_device_a_removable_disk_and_a_volume_over_them_are_removable() {
        let (_root, numbers) = sysfs();
        let removable =
            |number: &str| removable_in_sysfs(&numbers.join(number), MAPPER_DEPTH).unwrap();
        assert!(removable("8:17"), "a partition of a USB key");
        assert!(!removable("259:4"), "a partition of an internal disk");
        assert!(removable("179:0"), "a card the kernel says removable");
        assert!(removable("253:0"), "an encrypted volume on the key");
        assert!(
            removable("8:33"),
            "a USB disk the kernel does not say removable"
        );
        assert!(removable("179:1"), "a partition of a removable card");
        assert!(
            !removable_in_sysfs(&numbers.join("253:0"), 0).unwrap(),
            "no deeper than allowed"
        );
        assert!(removable_in_sysfs(&numbers.join("0:29"), MAPPER_DEPTH).is_err());
    }

    #[test]
    fn a_rotating_disk_and_a_volume_over_one_are_rotational() {
        let (root, numbers) = sysfs();
        let devices = root.path().join("devices");
        // The USB key spins (an old disk in a case), the internal one not.
        let flag = |disk: &str, value: &str| {
            let queue = devices.join(disk).join("queue");
            fs::create_dir_all(&queue).unwrap();
            fs::write(queue.join("rotational"), value).unwrap();
        };
        flag("pci0000:00/usb1/1-1/host6/block/sdb", "1\n");
        flag("pci0000:00/nvme/nvme0/nvme0n1", "0\n");
        let rotational =
            |number: &str| rotational_in_sysfs(&numbers.join(number), MAPPER_DEPTH).unwrap();
        assert!(rotational("8:17"), "a partition of a rotating disk");
        assert!(!rotational("259:4"), "a partition of a solid state disk");
        assert!(rotational("253:0"), "a volume over a rotating disk");
        assert!(
            rotational_in_sysfs(&numbers.join("253:0"), 0).unwrap(),
            "too deep to tell: rotating"
        );
        // A volume over eight disks, one of them rotating: found wherever
        // the listing of its slaves puts it.
        for spinning in 0..8 {
            let volume = devices.join(format!("virtual/block/dm-wide-{spinning}"));
            fs::create_dir_all(volume.join("slaves")).unwrap();
            for index in 0..8 {
                let disk = devices.join(format!("wide-{spinning}/disk-{index}"));
                fs::create_dir_all(disk.join("queue")).unwrap();
                let flag = if index == spinning { "1\n" } else { "0\n" };
                fs::write(disk.join("queue/rotational"), flag).unwrap();
                symlink(&disk, volume.join(format!("slaves/disk-{index}"))).unwrap();
            }
            assert!(
                rotational_in_sysfs(&volume, MAPPER_DEPTH).unwrap(),
                "{spinning}"
            );
        }
        assert!(
            rotational_in_sysfs(&numbers.join("179:0"), MAPPER_DEPTH).is_err(),
            "a disk that does not say"
        );
    }
}
