use std::io;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use super::capped::read_capped_bytes;

/// The table of mounts of this process.
#[cfg(target_os = "linux")]
const MOUNTINFO: &str = "/proc/self/mountinfo";
/// Far beyond the mounts of a desktop, even with containers: a longer table
/// is said cut, never read in part.
#[cfg(target_os = "linux")]
const MOUNTINFO_LIMIT: usize = 16 * 1024 * 1024;
/// The filesystems reached over the network.
const NETWORK_FILESYSTEMS: [&str; 5] = ["nfs", "nfs4", "cifs", "smb3", "fuse.sshfs"];
/// Where the boot partitions are mounted.
const BOOT: &str = "/boot";

/// A device number: for a mount, its filesystem's (its superblock's), the
/// same for every mount of that filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Device {
    pub major: u32,
    pub minor: u32,
}

/// What a mount is, as Linux tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountClass {
    /// Held by the kernel or in memory (`proc`, `tmpfs`, `overlay`, a
    /// portal's `fuse`...).
    Virtual,
    /// A loop device (a snap's `squashfs`, an image).
    Loop,
    /// A compressed memory device (`zram`).
    Memory,
    /// At `/boot` or below.
    Boot,
    /// Reached over the network.
    Network,
    /// A dataset of a ZFS pool.
    Pool { pool: String },
    /// A filesystem on a block device.
    Block,
}

/// One mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub id: u32,
    pub device: Device,
    pub mount_point: PathBuf,
    pub filesystem: String,
    pub source: String,
    pub class: MountClass,
}

/// Where desktops mount removable volumes, in a folder named after them,
/// and how deep: `/media/KINGSTON` or `/media/<user>/KINGSTON`,
/// `/run/media/<user>/KINGSTON`.
const MEDIA_FOLDERS: [(&str, &[usize]); 2] = [("/media", &[1, 2]), ("/run/media", &[2])];

impl Mount {
    /// The folder a desktop named this mount after (`KINGSTON`).
    #[must_use]
    pub fn media_folder(&self) -> Option<&str> {
        MEDIA_FOLDERS.iter().find_map(|(media, depths)| {
            let below = self.mount_point.strip_prefix(media).ok()?;
            if !depths.contains(&below.components().count()) {
                return None;
            }
            below.file_name().and_then(|name| name.to_str())
        })
    }
}

/// The mounts, and how many lines could not be understood.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountList {
    pub mounts: Vec<Mount>,
    pub malformed: usize,
}

/// The mounts of this process.
///
/// # Errors
///
/// The system's error, or [`io::ErrorKind::InvalidData`] when the table is
/// cut; [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn mounts() -> io::Result<MountList> {
    read_capped_bytes(std::path::Path::new(MOUNTINFO), MOUNTINFO_LIMIT)
        .map(|bytes| parse_mountinfo(&bytes))
}

/// The mounts of this process: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn mounts() -> io::Result<MountList> {
    super::unsupported()
}

/// The mounts of a `mountinfo` table, its bytes as the kernel wrote them: a
/// mount point may be any bytes, only space, tab, line break and backslash
/// are escaped.
pub(crate) fn parse_mountinfo(table: &[u8]) -> MountList {
    let mut list = MountList::default();
    for line in table
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.trim_ascii().is_empty())
    {
        // Counted, said by the caller when the count changes: a line read
        // every few seconds would be said as often.
        if let Some(mount) = parse_line(line) {
            list.mounts.push(mount);
        } else {
            list.malformed += 1;
        }
    }
    list
}

/// `36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw`: the
/// optional fields end at `-`.
fn parse_line(line: &[u8]) -> Option<Mount> {
    let mut fields = line.split(|byte| *byte == b' ');
    let mut text = || std::str::from_utf8(fields.next()?).ok();
    let id = text()?.parse().ok()?;
    let _parent = text()?;
    let (major, minor) = text()?.split_once(':')?;
    let device = Device {
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
    };
    let _root = fields.next()?;
    let mount_point = path_from_bytes(unescape(fields.next()?));
    let _options = fields.next()?;
    fields.find(|field| *field == b"-")?;
    let filesystem = String::from_utf8_lossy(fields.next()?).into_owned();
    let source = String::from_utf8_lossy(&unescape(fields.next()?)).into_owned();
    let class = classify(&source, &mount_point, &filesystem);
    Some(Mount {
        id,
        device,
        mount_point,
        filesystem,
        source,
        class,
    })
}

/// What the mount of `source` at `mount_point`, of type `filesystem`, is.
fn classify(source: &str, mount_point: &std::path::Path, filesystem: &str) -> MountClass {
    if mount_point.starts_with(BOOT) {
        MountClass::Boot
    } else if NETWORK_FILESYSTEMS.contains(&filesystem) {
        MountClass::Network
    } else if filesystem == "zfs" && source.contains('@') {
        // A snapshot shows what its dataset already counts.
        MountClass::Virtual
    } else if filesystem == "zfs" {
        let pool = source.split('/').next().unwrap_or(source).to_owned();
        MountClass::Pool { pool }
    } else if source.starts_with("/dev/loop") {
        MountClass::Loop
    } else if source.starts_with("/dev/zram") {
        MountClass::Memory
    } else if source.starts_with("/dev/") {
        MountClass::Block
    } else {
        MountClass::Virtual
    }
}

/// A field with the kernel's octal escapes (`\040` for a space) decoded.
fn unescape(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = bytes
            .get(index + 1..index + 4)
            .filter(|_| bytes[index] == b'\\')
            .filter(|digits| digits.iter().all(|digit| (b'0'..=b'7').contains(digit)))
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        if let Some(byte) = escaped {
            decoded.push(byte);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    decoded
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A desktop's lines (a snap, a portal, zram, a second disk), then other
    /// kinds of mounts.
    const SAMPLE: &str = "\
24 1 259:7 / / rw,relatime shared:1 master:2 propagate_from:3 - ext4 /dev/nvme0n1p7 rw
25 24 0:23 / /proc rw,nosuid shared:12 - proc proc rw
26 24 0:5 / /dev rw,nosuid shared:2 - devtmpfs udev rw
27 24 7:3 / /snap/firefox/1 ro,nodev shared:30 - squashfs /dev/loop3 ro
28 24 259:2 / /home rw,relatime shared:31 - ext4 /dev/nvme0n1p2 rw
29 24 259:1 / /boot/efi rw,relatime shared:32 - vfat /dev/nvme0n1p1 rw
30 24 259:4 / /run/media/franck/Donn\\303\\251es rw shared:33 - ntfs3 /dev/nvme0n1p4 rw
31 24 0:50 / /run/user/1000/doc rw shared:40 - fuse.portal portal rw
32 24 252:0 / [SWAP] rw - swap /dev/zram0 rw
33 24 0:60 / /mnt/nas rw shared:41 - nfs4 nas:/photos rw
34 24 0:61 / / rw shared:42 - zfs rpool/ROOT/ubuntu rw
35 24 0:62 / /home/ana rw shared:43 - zfs rpool/USERDATA/ana rw
36 24 0:70 / /media/My\\040Disk rw shared:44 - btrfs /dev/mapper/backup rw
37 24 0:70
38 24 0:23 / /tmp rw shared:45 - tmpfs tmpfs rw
";

    #[test]
    fn every_kind_of_mount_is_told_and_a_cut_line_counted() {
        let list = parse_mountinfo(SAMPLE.as_bytes());
        assert_eq!(list.malformed, 1);
        let class = |point: &str| {
            list.mounts
                .iter()
                .find(|mount| mount.mount_point == std::path::Path::new(point))
                .map(|mount| mount.class.clone())
        };
        assert_eq!(class("/home"), Some(MountClass::Block));
        assert_eq!(class("/proc"), Some(MountClass::Virtual));
        assert_eq!(class("/dev"), Some(MountClass::Virtual));
        assert_eq!(class("/snap/firefox/1"), Some(MountClass::Loop));
        assert_eq!(class("/boot/efi"), Some(MountClass::Boot));
        assert_eq!(class("/run/user/1000/doc"), Some(MountClass::Virtual));
        assert_eq!(class("[SWAP]"), Some(MountClass::Memory));
        assert_eq!(class("/mnt/nas"), Some(MountClass::Network));
        assert_eq!(
            class("/home/ana"),
            Some(MountClass::Pool {
                pool: "rpool".to_owned()
            })
        );
        assert_eq!(class("/media/My Disk"), Some(MountClass::Block));
        assert_eq!(class("/tmp"), Some(MountClass::Virtual));
    }

    #[test]
    fn a_line_is_read_field_by_field_its_escapes_decoded() {
        let list = parse_mountinfo(SAMPLE.as_bytes());
        let data = list
            .mounts
            .iter()
            .find(|mount| {
                mount.device
                    == Device {
                        major: 259,
                        minor: 4,
                    }
            })
            .unwrap();
        assert_eq!(data.id, 30);
        assert_eq!(
            data.mount_point,
            std::path::Path::new("/run/media/franck/Données")
        );
        assert_eq!(data.filesystem, "ntfs3");
        assert_eq!(data.source, "/dev/nvme0n1p4");
        let nas = list
            .mounts
            .iter()
            .find(|mount| mount.filesystem == "nfs4")
            .unwrap();
        assert_eq!(nas.source, "nas:/photos");
    }

    #[cfg(unix)]
    #[test]
    fn a_mount_point_of_any_bytes_is_read_with_the_others() {
        use std::os::unix::ffi::OsStrExt;

        let mut table = b"1 1 8:1 / /mnt/caf".to_vec();
        table.push(0xE9);
        table.extend_from_slice(
            b" rw - ext4 /dev/sdb1 rw\n2 1 8:2 / /data rw - ext4 /dev/sdb2 rw\n",
        );
        let list = parse_mountinfo(&table);
        assert_eq!(list.malformed, 0);
        assert_eq!(list.mounts.len(), 2);
        assert_eq!(
            list.mounts[0].mount_point.as_os_str().as_bytes(),
            b"/mnt/caf\xE9"
        );
    }

    #[test]
    fn a_media_folder_is_where_a_desktop_mounts_a_volume() {
        let folder = |point: &str| {
            let mut mount = parse_mountinfo("1 1 8:1 / /x rw - vfat /dev/sdb1 rw\n".as_bytes())
                .mounts
                .remove(0);
            mount.mount_point = std::path::PathBuf::from(point);
            mount.media_folder().map(str::to_owned)
        };
        assert_eq!(folder("/media/KINGSTON").as_deref(), Some("KINGSTON"));
        assert_eq!(folder("/media/ana/KINGSTON").as_deref(), Some("KINGSTON"));
        assert_eq!(
            folder("/run/media/ana/KINGSTON").as_deref(),
            Some("KINGSTON")
        );
        for point in [
            "/mnt/x",
            "/media",
            "/mediax/y",
            "/run/media/ana",
            "/media/ana/KINGSTON/sub",
        ] {
            assert_eq!(folder(point), None, "{point}");
        }
    }

    #[test]
    fn boot_comes_first_whatever_the_filesystem() {
        let path = std::path::Path::new;
        assert_eq!(
            classify(
                "rpool/USERDATA/ana@daily",
                path("/home/ana/.zfs/snapshot/daily"),
                "zfs"
            ),
            MountClass::Virtual,
            "a snapshot is never part of its pool"
        );
        assert_eq!(
            classify("nas:/boot", path("/boot"), "nfs4"),
            MountClass::Boot
        );
        assert_eq!(
            classify("rpool/BOOT", path("/boot/grub"), "zfs"),
            MountClass::Boot
        );
        assert_eq!(
            classify("/dev/sda1", path("/bootx"), "ext4"),
            MountClass::Block
        );
        let tab = parse_mountinfo("40 1 8:1 / /a\\011b rw - ext4 /dev/sda1 rw\n".as_bytes());
        assert_eq!(tab.mounts[0].mount_point, path("/a\tb"));
    }

    #[test]
    fn a_backslash_without_three_octal_digits_is_kept() {
        assert_eq!(unescape(br"a\b\04"), b"a\\b\\04");
        assert_eq!(unescape(br"\134x"), b"\\x");
        assert_eq!(unescape(br"\+12"), br"\+12");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_mounts_of_this_process_hold_the_root() {
        let list = mounts().unwrap();
        assert!(
            list.mounts
                .iter()
                .any(|mount| mount.mount_point == std::path::Path::new("/"))
        );
    }
}
