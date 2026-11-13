use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use okapi_platform::{Device, Mount, MountClass, Space};

use super::sampler::Unavailable;

/// At or beyond this share used, a volume is nearly full. A floor of space
/// left on large volumes would add nothing: beyond 100 GB, less than 10 GB
/// left is already more than this.
const NEARLY_FULL_PERCENT: u128 = 90;

/// How the interface names a volume (it says the words).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeName {
    /// Mounted at `/`.
    System,
    /// Mounted at `/home`.
    Home,
    /// A ZFS pool.
    Pool(String),
    /// A network share, by its source (`nas:/photos`).
    Source(String),
    /// By the label of its filesystem.
    Label(String),
    /// By its folder below `/media` or `/run/media`.
    Folder(String),
    /// By where it is mounted.
    MountPoint(PathBuf),
}

/// Where a volume is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeKind {
    Local,
    Removable,
    Network,
}

/// What is known of a volume's space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeSpace {
    /// Whether it is nearly full is [`nearly_full`]'s to say.
    Known(Space),
    /// It did not answer in time (a share lost): it may still answer later.
    NotResponding,
    Failed(Unavailable),
}

/// A volume as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub name: VolumeName,
    pub mount_point: PathBuf,
    pub filesystem: String,
    pub kind: VolumeKind,
    pub space: VolumeSpace,
}

/// One filesystem whose space is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub device: Device,
    pub mount_point: PathBuf,
}

/// A volume to show, before its space is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub name: VolumeName,
    pub mount_point: PathBuf,
    pub filesystem: String,
    pub kind: VolumeKind,
    /// One, but every mounted dataset of a ZFS pool.
    pub members: Vec<Member>,
}

/// The volumes to show, and the mounts set aside by their class.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Selection {
    pub candidates: Vec<Candidate>,
    pub set_aside: BTreeMap<&'static str, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Key {
    Device(Device),
    Pool(String),
}

/// The volumes among `mounts`: each filesystem on a block device, each
/// network share, each ZFS pool, once (a filesystem mounted several times,
/// btrfs subvolumes included, shares its device), shown at its shortest mount
/// point. Virtual, loop, memory and boot mounts are set aside. `label_of`
/// gives a mount's filesystem label, `kind_of` whether it is removable.
pub(crate) fn candidates(
    mounts: &[Mount],
    label_of: impl Fn(&Mount) -> Option<String>,
    kind_of: impl Fn(&Mount) -> VolumeKind,
) -> Selection {
    let mut selection = Selection::default();
    // A path mounted over keeps only its last mount.
    let mut last_at: BTreeMap<&Path, usize> = BTreeMap::new();
    for (index, mount) in mounts.iter().enumerate() {
        last_at.insert(&mount.mount_point, index);
    }
    let mut groups: Vec<(Key, Vec<&Mount>)> = Vec::new();
    for (index, mount) in mounts.iter().enumerate() {
        if last_at.get(mount.mount_point.as_path()) != Some(&index) {
            continue;
        }
        let key = match &mount.class {
            MountClass::Block | MountClass::Network => Key::Device(mount.device),
            MountClass::Pool { pool } => Key::Pool(pool.clone()),
            other => {
                *selection.set_aside.entry(class_name(other)).or_default() += 1;
                continue;
            }
        };
        match groups.iter_mut().find(|(known, _)| *known == key) {
            Some((_, members)) => members.push(mount),
            None => groups.push((key, vec![mount])),
        }
    }
    for (key, group) in groups {
        let Some(shown) = group
            .iter()
            .copied()
            .min_by_key(|mount| mount.mount_point.as_os_str().len())
        else {
            continue;
        };
        let members = match key {
            // A dataset mounted twice (a bind mount) counts once.
            Key::Pool(_) => {
                let mut members: Vec<Member> = Vec::new();
                for mount in &group {
                    if !members.iter().any(|member| member.device == mount.device) {
                        members.push(Member {
                            device: mount.device,
                            mount_point: mount.mount_point.clone(),
                        });
                    }
                }
                members
            }
            Key::Device(device) => vec![Member {
                device,
                mount_point: shown.mount_point.clone(),
            }],
        };
        selection.candidates.push(Candidate {
            name: name(shown, &label_of),
            mount_point: shown.mount_point.clone(),
            filesystem: shown.filesystem.clone(),
            kind: if shown.class == MountClass::Network {
                VolumeKind::Network
            } else {
                kind_of(shown)
            },
            members,
        });
    }
    selection
}

fn class_name(class: &MountClass) -> &'static str {
    match class {
        MountClass::Virtual => "virtual",
        MountClass::Loop => "loop",
        MountClass::Memory => "memory",
        MountClass::Boot => "boot",
        MountClass::Network => "network",
        MountClass::Pool { .. } => "pool",
        MountClass::Block => "block",
    }
}

fn name(mount: &Mount, label_of: impl Fn(&Mount) -> Option<String>) -> VolumeName {
    let point = &mount.mount_point;
    if point == Path::new("/") {
        return VolumeName::System;
    }
    if point == Path::new("/home") {
        return VolumeName::Home;
    }
    if let MountClass::Pool { pool } = &mount.class {
        return VolumeName::Pool(pool.clone());
    }
    if mount.class == MountClass::Network {
        return VolumeName::Source(mount.source.clone());
    }
    if let Some(label) = label_of(mount) {
        return VolumeName::Label(label);
    }
    mount.media_folder().map_or_else(
        || VolumeName::MountPoint(point.clone()),
        |folder| VolumeName::Folder(folder.to_owned()),
    )
}

/// The space of a volume from that of its members: a pool's datasets share
/// what is available, each holds its own used space (snapshots and datasets
/// not mounted left out, so the total is a floor).
pub(crate) fn aggregate(spaces: &[Space]) -> Space {
    if let [single] = spaces {
        return *single;
    }
    let available = spaces
        .iter()
        .map(|space| space.available)
        .min()
        .unwrap_or(0);
    let used = spaces
        .iter()
        .fold(0_u64, |sum, space| sum.saturating_add(space.used));
    Space {
        total: used.saturating_add(available),
        available,
        used,
    }
}

/// Whether a volume is nearly full: [`NEARLY_FULL_PERCENT`] used or more (an
/// empty one never).
#[must_use]
pub fn nearly_full(space: &Space) -> bool {
    space.total > 0 && u128::from(space.used) * 100 >= u128::from(space.total) * NEARLY_FULL_PERCENT
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    fn mount(
        id: u32,
        device: (u32, u32),
        point: &str,
        filesystem: &str,
        source: &str,
        class: MountClass,
    ) -> Mount {
        Mount {
            id,
            device: Device {
                major: device.0,
                minor: device.1,
            },
            mount_point: PathBuf::from(point),
            filesystem: filesystem.to_owned(),
            source: source.to_owned(),
            class,
        }
    }

    fn select(mounts: &[Mount]) -> Selection {
        candidates(
            mounts,
            |mount| (mount.source == "/dev/nvme0n1p4").then(|| "Données".to_owned()),
            |_| VolumeKind::Local,
        )
    }

    /// A desktop's mounts: two disks, a second one under `/run/media`, a
    /// boot partition, a snap, a portal.
    fn desktop_mounts() -> Vec<Mount> {
        vec![
            mount(
                24,
                (259, 7),
                "/",
                "ext4",
                "/dev/nvme0n1p7",
                MountClass::Block,
            ),
            mount(25, (0, 23), "/proc", "proc", "proc", MountClass::Virtual),
            mount(
                27,
                (7, 3),
                "/snap/firefox/1",
                "squashfs",
                "/dev/loop3",
                MountClass::Loop,
            ),
            mount(
                28,
                (259, 2),
                "/home",
                "ext4",
                "/dev/nvme0n1p2",
                MountClass::Block,
            ),
            mount(
                29,
                (259, 1),
                "/boot/efi",
                "vfat",
                "/dev/nvme0n1p1",
                MountClass::Boot,
            ),
            mount(
                30,
                (259, 4),
                "/run/media/franck/Données",
                "ntfs3",
                "/dev/nvme0n1p4",
                MountClass::Block,
            ),
            mount(
                31,
                (0, 50),
                "/run/user/1000/doc",
                "fuse.portal",
                "portal",
                MountClass::Virtual,
            ),
        ]
    }

    #[test]
    fn the_volumes_of_a_desktop_are_its_disks_never_its_boot_nor_its_snaps() {
        let selection = select(&desktop_mounts());
        let names: Vec<&VolumeName> = selection
            .candidates
            .iter()
            .map(|candidate| &candidate.name)
            .collect();
        assert_eq!(
            names,
            [
                &VolumeName::System,
                &VolumeName::Home,
                &VolumeName::Label("Données".to_owned())
            ]
        );
        assert_eq!(
            selection.set_aside,
            BTreeMap::from([("boot", 1), ("loop", 1), ("virtual", 2)])
        );
    }

    #[test]
    fn one_filesystem_mounted_twice_is_one_volume_at_its_shortest_point() {
        let mounts = [
            mount(
                1,
                (0, 40),
                "/home/ana",
                "btrfs",
                "/dev/sda2",
                MountClass::Block,
            ),
            mount(2, (0, 40), "/data", "btrfs", "/dev/sda2", MountClass::Block),
            mount(3, (0, 41), "/srv", "btrfs", "/dev/sda2", MountClass::Block),
            mount(4, (8, 1), "/mnt/a", "ext4", "/dev/sdb1", MountClass::Block),
            mount(5, (8, 1), "/mnt/b", "ext4", "/dev/sdc1", MountClass::Block),
        ];
        let selection = select(&mounts);
        let points: Vec<&Path> = selection
            .candidates
            .iter()
            .map(|candidate| candidate.mount_point.as_path())
            .collect();
        assert_eq!(
            points,
            [Path::new("/data"), Path::new("/srv"), Path::new("/mnt/a")],
            "same device: one volume, whatever the source; other device: another"
        );
    }

    #[test]
    fn a_path_mounted_over_keeps_its_last_mount() {
        let mounts = [
            mount(
                1,
                (8, 1),
                "/mnt/usb",
                "vfat",
                "/dev/sdb1",
                MountClass::Block,
            ),
            mount(
                2,
                (8, 17),
                "/mnt/usb",
                "ext4",
                "/dev/sdc1",
                MountClass::Block,
            ),
        ];
        let selection = select(&mounts);
        assert_eq!(selection.candidates.len(), 1);
        assert_eq!(selection.candidates[0].filesystem, "ext4");
    }

    #[test]
    fn a_zfs_pool_is_one_volume_of_all_its_datasets() {
        let pool = || MountClass::Pool {
            pool: "rpool".to_owned(),
        };
        let mounts = [
            mount(1, (0, 61), "/", "zfs", "rpool/ROOT/ubuntu", pool()),
            mount(2, (0, 62), "/home/ana", "zfs", "rpool/USERDATA/ana", pool()),
            mount(
                3,
                (0, 63),
                "/var/log",
                "zfs",
                "rpool/ROOT/ubuntu/var/log",
                pool(),
            ),
            mount(4, (0, 62), "/srv/ana", "zfs", "rpool/USERDATA/ana", pool()),
        ];
        let selection = select(&mounts);
        assert_eq!(selection.candidates.len(), 1);
        let volume = &selection.candidates[0];
        assert_eq!(volume.name, VolumeName::System);
        assert_eq!(
            volume.members.len(),
            3,
            "a dataset mounted twice counts once"
        );
        let elsewhere = [mount(
            1,
            (0, 61),
            "/tank",
            "zfs",
            "tank/data",
            MountClass::Pool {
                pool: "tank".to_owned(),
            },
        )];
        assert_eq!(
            select(&elsewhere).candidates[0].name,
            VolumeName::Pool("tank".to_owned())
        );
    }

    #[test]
    fn a_pool_s_space_adds_its_datasets_used_space_to_what_they_share() {
        let dataset = |used: u64, available: u64| Space {
            total: used + available,
            available,
            used,
        };
        assert_eq!(
            aggregate(&[
                dataset(10 * GB, 7 * GB),
                dataset(3 * GB, 5 * GB),
                dataset(2 * GB, 6 * GB)
            ]),
            Space {
                total: 20 * GB,
                available: 5 * GB,
                used: 15 * GB,
            },
            "the least available is what the pool still has"
        );
        // A filesystem keeping space for root: its total is not used plus
        // available, and stays as read.
        let reserved = Space {
            total: 100,
            available: 50,
            used: 40,
        };
        assert_eq!(aggregate(&[reserved]), reserved);
    }

    #[test]
    fn names_follow_their_order_of_priority() {
        let shares = [
            mount(
                1,
                (0, 60),
                "/media/nas",
                "nfs4",
                "nas:/photos",
                MountClass::Network,
            ),
            mount(
                2,
                (8, 1),
                "/media/ana/KINGSTON",
                "vfat",
                "/dev/sdb1",
                MountClass::Block,
            ),
            mount(
                3,
                (8, 17),
                "/srv/backup",
                "ext4",
                "/dev/sdc1",
                MountClass::Block,
            ),
        ];
        let selection = select(&shares);
        let names: Vec<&VolumeName> = selection
            .candidates
            .iter()
            .map(|candidate| &candidate.name)
            .collect();
        assert_eq!(
            names,
            [
                &VolumeName::Source("nas:/photos".to_owned()),
                &VolumeName::Folder("KINGSTON".to_owned()),
                &VolumeName::MountPoint(PathBuf::from("/srv/backup")),
            ]
        );
        assert_eq!(selection.candidates[0].kind, VolumeKind::Network);
        let unlabelled = [mount(
            1,
            (8, 1),
            "/run/media/ana/Clé",
            "vfat",
            "/dev/sdb1",
            MountClass::Block,
        )];
        assert_eq!(
            select(&unlabelled).candidates[0].name,
            VolumeName::Folder("Clé".to_owned())
        );
    }

    #[test]
    fn a_volume_is_as_removable_as_its_device_is_said() {
        let mounts = [
            mount(
                1,
                (8, 1),
                "/media/key",
                "vfat",
                "/dev/sdb1",
                MountClass::Block,
            ),
            mount(
                2,
                (259, 7),
                "/",
                "ext4",
                "/dev/nvme0n1p7",
                MountClass::Block,
            ),
        ];
        let selection = candidates(
            &mounts,
            |_| None,
            |mount| {
                if mount.source == "/dev/sdb1" {
                    VolumeKind::Removable
                } else {
                    VolumeKind::Local
                }
            },
        );
        let kinds: Vec<VolumeKind> = selection
            .candidates
            .iter()
            .map(|candidate| candidate.kind)
            .collect();
        assert_eq!(kinds, [VolumeKind::Removable, VolumeKind::Local]);
        assert_eq!(
            selection.candidates[0].members,
            [Member {
                device: Device { major: 8, minor: 1 },
                mount_point: PathBuf::from("/media/key"),
            }]
        );
    }

    #[test]
    fn nearly_full_is_from_the_share_used() {
        let space = |total: u64, available: u64| Space {
            total,
            available,
            used: total - available,
        };
        assert!(!nearly_full(&space(200 * GB, 22 * GB)), "89 %");
        assert!(nearly_full(&space(200 * GB, 20 * GB)), "90 %");
        assert!(nearly_full(&space(1_000 * GB, 9 * GB)), "9 GB left of 1 TB");
        assert!(
            !nearly_full(&space(15 * GB, 3 * GB)),
            "an 8-to-16 GB key with room left"
        );
        assert!(!nearly_full(&space(0, 0)), "nothing to fill");
    }
}
