//! Collection of the node's runtime environment.

use std::path::Path;

use base_telemetry_types::{Hardware, HardwarePlatform};

/// Firmware vendor strings that identify a virtualized or cloud host.
///
/// Matched as a case-insensitive prefix against `/sys/class/dmi/id/sys_vendor`. Anything not on
/// this list and not empty is reported as bare metal, and the raw vendor string travels
/// alongside the classification so the list can be revised and the archive recomputed.
const CLOUD_VENDOR_PREFIXES: &[&str] = &[
    "amazon",
    "alibaba",
    "digitalocean",
    "google",
    "hetzner",
    "linode",
    "microsoft",
    "openstack",
    "oracle",
    "parallels",
    "qemu",
    "scaleway",
    "vmware",
    "vultr",
    "xen",
];

/// Filesystem types that are network-attached by definition.
const NETWORK_FILESYSTEMS: &[&str] = &[
    "9p",
    "afs",
    "beegfs",
    "ceph",
    "cifs",
    "fuse.glusterfs",
    "fuse.s3fs",
    "fuse.sshfs",
    "gfs2",
    "glusterfs",
    "lustre",
    "nfs",
    "nfs4",
    "ocfs2",
    "smb3",
    "smbfs",
];

/// Block device name prefixes that are network-attached by definition.
///
/// These carry a local filesystem such as `ext4`, so the filesystem type alone does not reveal
/// them: `ext4` on `rbd0` is Ceph over the network and reads exactly like `ext4` on `nvme0n1`.
const NETWORK_BLOCK_DEVICES: &[&str] = &["drbd", "nbd", "rbd"];

/// Disk model strings that identify a cloud volume attached over the network.
///
/// Matched case-insensitively as a prefix. These present to the guest as ordinary `NVMe` or SCSI
/// devices, so nothing about the device name or the filesystem gives them away — only the model
/// string the hypervisor reports does.
const NETWORK_DISK_MODELS: &[&str] =
    &["amazon elastic block store", "google persistentdisk", "virtual disk"];

/// One line of `/proc/mounts`: what is mounted where, as what, and with which options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// Device backing the mount, e.g. `/dev/nvme0n1p3` or `10.0.0.1:/export`.
    pub device: String,
    /// Where the filesystem is mounted.
    pub mount_point: String,
    /// Filesystem type, e.g. `ext4`.
    pub fs_type: String,
    /// Comma-separated mount options.
    pub options: String,
}

/// Reads the node's runtime environment.
///
/// Collection degrades field by field. A non-Linux host, a container without `/sys` or `/proc`
/// mounted, and a permission error all yield `None` for the affected fields rather than an
/// error, because failing to describe the machine must never stop a node from reporting.
#[derive(Debug, Clone, Copy)]
pub struct HardwareCollector;

impl HardwareCollector {
    /// Collects the runtime environment, sizing the disk fields against `data_dir` when given.
    pub fn collect(data_dir: Option<&Path>) -> Hardware {
        let vendor = Self::read_sys_vendor();
        let (disk_total_bytes, disk_free_bytes) =
            data_dir.map_or((None, None), Self::filesystem_capacity);
        let mount = data_dir.and_then(Self::mount_entry_for);
        let device = mount.as_ref().and_then(|mount| Self::whole_disk_device(&mount.device));
        let (disk_model, disk_rotational) =
            device.as_deref().map_or((None, None), Self::backing_disk_details);

        Hardware {
            platform: Self::classify_platform(vendor.as_deref()),
            cloud_vendor: vendor,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            kernel: Self::read_kernel_release(),
            cpu_model: Self::read_cpu_model(),
            cpu_cores: Self::cpu_cores(),
            ram_bytes: Self::read_memory_bytes(),
            disk_model: disk_model.clone(),
            disk_rotational,
            disk_total_bytes,
            disk_free_bytes,
            fs_type: mount.as_ref().map(|mount| mount.fs_type.clone()),
            fs_flags: mount.as_ref().map(|mount| mount.options.clone()),
            is_network_storage: mount.as_ref().map(|mount| {
                Self::is_network_storage(&mount.fs_type, &mount.device, disk_model.as_deref())
            }),
        }
    }

    /// Classifies a firmware vendor string as cloud or bare metal.
    pub fn classify_platform(vendor: Option<&str>) -> HardwarePlatform {
        let Some(vendor) = vendor else {
            return HardwarePlatform::Unknown;
        };
        let vendor = vendor.trim().to_ascii_lowercase();
        if vendor.is_empty() {
            return HardwarePlatform::Unknown;
        }
        if CLOUD_VENDOR_PREFIXES.iter().any(|prefix| vendor.starts_with(prefix)) {
            return HardwarePlatform::Cloud;
        }
        HardwarePlatform::BareMetal
    }

    /// Returns the number of logical cores visible to this process.
    ///
    /// This is the process's own view, so a container with a CPU quota reports the quota rather
    /// than the host's core count. That is the number that explains the node's performance.
    pub fn cpu_cores() -> Option<u32> {
        std::thread::available_parallelism().ok().map(|cores| cores.get() as u32)
    }

    /// Returns total and free bytes on the filesystem holding `path`.
    pub fn filesystem_capacity(path: &Path) -> (Option<u64>, Option<u64>) {
        #[cfg(not(unix))]
        {
            let _ = path;
            return (None, None);
        }
        #[cfg(unix)]
        {
            use std::{ffi::CString, os::unix::ffi::OsStrExt};

            let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
                return (None, None);
            };
            // SAFETY: `c_path` is a valid NUL-terminated C string that outlives the call, and
            // `stats` is a correctly sized, writable `statvfs` the kernel fills in.
            let stats = unsafe {
                let mut stats: libc::statvfs = std::mem::zeroed();
                if libc::statvfs(c_path.as_ptr(), &mut stats) != 0 {
                    return (None, None);
                }
                stats
            };

            let block_size = stats.f_frsize as u64;
            (
                Some(block_size.saturating_mul(stats.f_blocks as u64)),
                Some(block_size.saturating_mul(stats.f_bavail as u64)),
            )
        }
    }

    /// Returns the model string and rotational flag of the whole-disk block device `disk`.
    ///
    /// Everything here comes from sysfs, so a host without `/sys/class/block` reports neither.
    pub fn backing_disk_details(disk: &str) -> (Option<String>, Option<bool>) {
        let sysfs = Path::new("/sys/class/block").join(disk);
        let model = Self::read_trimmed(&sysfs.join("device/model"));
        let rotational = Self::read_trimmed(&sysfs.join("queue/rotational"))
            .and_then(|value| value.parse::<u8>().ok())
            .map(|value| value != 0);
        (model, rotational)
    }

    /// Returns the firmware vendor string, which is how we tell cloud from bare metal.
    ///
    /// Reading a file is passive and fast, and unlike probing a cloud metadata endpoint it does
    /// not look intrusive in a packet capture. The DMI tree is Linux-only; anywhere else the
    /// read simply fails and the vendor stays unknown.
    pub fn read_sys_vendor() -> Option<String> {
        Self::read_trimmed(Path::new("/sys/class/dmi/id/sys_vendor"))
    }

    /// Returns the CPU model string from `/proc/cpuinfo`.
    pub fn read_cpu_model() -> Option<String> {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;
        cpuinfo.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            let key = key.trim();
            (key == "model name" || key == "Model").then(|| value.trim().to_string())
        })
    }

    /// Returns total system memory in bytes from `/proc/meminfo`.
    pub fn read_memory_bytes() -> Option<u64> {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        Self::parse_mem_total_kib(&meminfo).map(|kib| kib.saturating_mul(1024))
    }

    /// Extracts `MemTotal` in `KiB` from the contents of `/proc/meminfo`.
    pub fn parse_mem_total_kib(meminfo: &str) -> Option<u64> {
        meminfo.lines().find_map(|line| {
            let value = line.strip_prefix("MemTotal:")?;
            value.split_whitespace().next()?.parse::<u64>().ok()
        })
    }

    /// Returns the kernel release string, e.g. `6.8.0-45-generic`.
    ///
    /// Read from `procfs` rather than through `uname(2)` so it works the same in a container with
    /// `/proc` mounted and degrades to `None` everywhere else, like every other field here.
    pub fn read_kernel_release() -> Option<String> {
        Self::read_trimmed(Path::new("/proc/sys/kernel/osrelease"))
    }

    /// Returns the `/proc/mounts` entry for the filesystem covering `path`.
    pub fn mount_entry_for(path: &Path) -> Option<MountEntry> {
        let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
        Self::parse_mount_entry(&mounts, path)
    }

    /// Picks the entry with the longest matching mount point from the contents of `/proc/mounts`.
    ///
    /// Longest wins because mount points nest: a data directory under `/var/lib/base` on its own
    /// volume matches both `/` and `/var/lib/base`, and only the deeper one describes it.
    pub fn parse_mount_entry(mounts: &str, path: &Path) -> Option<MountEntry> {
        let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        mounts
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let device = fields.next()?;
                let mount_point = fields.next()?;
                let fs_type = fields.next()?;
                let options = fields.next()?;
                target.starts_with(mount_point).then(|| MountEntry {
                    device: device.to_string(),
                    mount_point: mount_point.to_string(),
                    fs_type: fs_type.to_string(),
                    options: options.to_string(),
                })
            })
            .max_by_key(|entry| entry.mount_point.len())
    }

    /// Returns the whole-disk block device name behind a mount's device string, e.g. `nvme0n1`.
    ///
    /// Walks from the partition to its parent disk, so a node on `/dev/nvme0n1p3` is attributed
    /// to `nvme0n1` and the rotational flag comes from the disk rather than the partition. A
    /// device that is not under `/dev` — an NFS export, a tmpfs — has no block device at all.
    pub fn whole_disk_device(device: &str) -> Option<String> {
        let device = device.strip_prefix("/dev/")?;
        let sysfs = Path::new("/sys/class/block").join(device);
        if !sysfs.join("partition").exists() {
            return Some(device.to_string());
        }
        // A partition's sysfs entry is nested under its whole-disk device, so the parent
        // directory of the resolved symlink names the disk.
        let resolved = sysfs.canonicalize().ok()?;
        Some(resolved.parent()?.file_name()?.to_string_lossy().into_owned())
    }

    /// Returns whether a mount is network-attached rather than local.
    ///
    /// Three signals, because no single one covers the field. The filesystem type catches NFS and
    /// friends; the device name catches a local filesystem layered on a network block device such
    /// as `rbd`; the disk model catches a cloud volume that presents to the guest as ordinary
    /// `NVMe`. Direct-attached instance storage matches none of them and reports `false`, which is
    /// correct. This cannot see an iSCSI LUN behind a plain `sd*` device, so `false` means "no
    /// evidence of network storage" rather than "proven local".
    pub fn is_network_storage(fs_type: &str, device: &str, disk_model: Option<&str>) -> bool {
        let fs_type = fs_type.to_ascii_lowercase();
        if NETWORK_FILESYSTEMS.contains(&fs_type.as_str()) {
            return true;
        }
        if let Some(name) = device.strip_prefix("/dev/")
            && NETWORK_BLOCK_DEVICES.iter().any(|prefix| name.starts_with(prefix))
        {
            return true;
        }
        disk_model.is_some_and(|model| {
            let model = model.trim().to_ascii_lowercase();
            NETWORK_DISK_MODELS.iter().any(|known| model.starts_with(known))
        })
    }

    /// Reads a file and trims it, returning `None` when it is missing, unreadable, or empty.
    pub fn read_trimmed(path: &Path) -> Option<String> {
        let contents = std::fs::read_to_string(path).ok()?;
        let trimmed = contents.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collection_never_fails_on_this_host() {
        let hardware = HardwareCollector::collect(Some(Path::new(".")));

        assert_eq!(hardware.os, std::env::consts::OS);
        assert_eq!(hardware.arch, std::env::consts::ARCH);
        assert!(hardware.cpu_cores.is_some_and(|cores| cores > 0));
    }

    #[test]
    fn test_collection_degrades_off_linux() {
        let hardware = HardwareCollector::collect(None);

        if cfg!(target_os = "linux") {
            return;
        }
        assert_eq!(
            hardware.platform,
            HardwarePlatform::Unknown,
            "there is no DMI vendor to read off Linux, so the platform is unknown, not bare metal"
        );
        assert!(hardware.cloud_vendor.is_none());
        assert!(hardware.cpu_model.is_none());
        assert!(hardware.ram_bytes.is_none());
        assert!(hardware.kernel.is_none(), "there is no procfs osrelease to read off Linux");
        assert!(hardware.fs_type.is_none(), "there is no /proc/mounts to read off Linux");
    }

    #[test]
    fn test_disk_fields_are_absent_without_a_data_dir() {
        let hardware = HardwareCollector::collect(None);
        assert!(hardware.disk_total_bytes.is_none());
        assert!(hardware.disk_free_bytes.is_none());
        assert!(hardware.disk_model.is_none());
        assert!(hardware.disk_rotational.is_none());
        assert!(hardware.fs_type.is_none());
        assert!(hardware.fs_flags.is_none());
        assert!(hardware.is_network_storage.is_none());
    }

    #[test]
    fn test_filesystem_capacity_reads_a_real_path_on_unix() {
        let (total, free) = HardwareCollector::filesystem_capacity(Path::new("."));
        if cfg!(unix) {
            assert!(total.is_some_and(|bytes| bytes > 0), "the working directory has a filesystem");
            assert!(free.is_some());
        }
    }

    #[test]
    fn test_filesystem_capacity_tolerates_a_missing_path() {
        let (total, free) =
            HardwareCollector::filesystem_capacity(Path::new("/definitely/not/a/real/path"));
        assert!(total.is_none());
        assert!(free.is_none());
    }

    #[test]
    fn test_cloud_vendors_are_recognized() {
        assert_eq!(
            HardwareCollector::classify_platform(Some("Amazon EC2")),
            HardwarePlatform::Cloud
        );
        assert_eq!(
            HardwareCollector::classify_platform(Some("Google Compute Engine")),
            HardwarePlatform::Cloud
        );
        assert_eq!(
            HardwareCollector::classify_platform(Some("microsoft corporation")),
            HardwarePlatform::Cloud,
            "matching must be case-insensitive"
        );
    }

    #[test]
    fn test_unrecognized_vendors_are_bare_metal_and_absent_ones_are_unknown() {
        assert_eq!(
            HardwareCollector::classify_platform(Some("Supermicro")),
            HardwarePlatform::BareMetal
        );
        assert_eq!(HardwareCollector::classify_platform(None), HardwarePlatform::Unknown);
        assert_eq!(
            HardwareCollector::classify_platform(Some("   ")),
            HardwarePlatform::Unknown,
            "an empty DMI file tells us nothing, which is not the same as bare metal"
        );
    }

    #[test]
    fn test_mem_total_is_parsed_from_meminfo() {
        let meminfo = "MemTotal:       65809192 kB\nMemFree:         1234 kB\n";
        assert_eq!(HardwareCollector::parse_mem_total_kib(meminfo), Some(65_809_192));
    }

    #[test]
    fn test_mem_total_absent_from_meminfo_is_none() {
        assert_eq!(HardwareCollector::parse_mem_total_kib("MemFree: 1234 kB\n"), None);
        assert_eq!(HardwareCollector::parse_mem_total_kib("MemTotal:  not-a-number kB\n"), None);
    }

    /// A `/proc/mounts` excerpt with nested mount points, a network export, and a pseudo-filesystem.
    const MOUNTS: &str = "\
/dev/nvme0n1p3 / ext4 rw,relatime 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
/dev/nvme0n1p3 /var ext4 rw,relatime 0 0
10.0.0.1:/export /var/lib/base nfs4 rw,noatime,vers=4.2 0 0
";

    #[test]
    fn test_the_deepest_matching_mount_point_wins() {
        let entry = HardwareCollector::parse_mount_entry(MOUNTS, Path::new("/var/lib/base/chain"))
            .expect("a mount covers the path");

        assert_eq!(entry.mount_point, "/var/lib/base", "/var and / also match but are shallower");
        assert_eq!(entry.fs_type, "nfs4");
        assert_eq!(entry.options, "rw,noatime,vers=4.2");
        assert_eq!(entry.device, "10.0.0.1:/export");
    }

    #[test]
    fn test_a_path_under_no_mount_point_has_no_entry() {
        assert_eq!(HardwareCollector::parse_mount_entry("", Path::new("/var/lib/base")), None);
    }

    #[test]
    fn test_network_filesystems_are_network_storage() {
        assert!(HardwareCollector::is_network_storage("nfs4", "10.0.0.1:/export", None));
        assert!(HardwareCollector::is_network_storage("NFS", "10.0.0.1:/export", None));
        assert!(HardwareCollector::is_network_storage("cifs", "//server/share", None));
    }

    #[test]
    fn test_a_local_filesystem_on_a_network_block_device_is_network_storage() {
        assert!(
            HardwareCollector::is_network_storage("ext4", "/dev/rbd0", None),
            "ext4 on Ceph reads exactly like ext4 on NVMe from the filesystem type alone"
        );
    }

    #[test]
    fn test_a_cloud_volume_is_network_storage_despite_looking_local() {
        assert!(HardwareCollector::is_network_storage(
            "ext4",
            "/dev/nvme1n1",
            Some("Amazon Elastic Block Store")
        ));
    }

    #[test]
    fn test_direct_attached_storage_is_not_network_storage() {
        assert!(!HardwareCollector::is_network_storage(
            "ext4",
            "/dev/nvme0n1p3",
            Some("Samsung SSD 990 PRO 2TB")
        ));
        assert!(!HardwareCollector::is_network_storage("zfs", "tank/base", None));
    }

    #[test]
    fn test_a_device_outside_dev_has_no_whole_disk() {
        assert_eq!(HardwareCollector::whole_disk_device("10.0.0.1:/export"), None);
        assert_eq!(HardwareCollector::whole_disk_device("tmpfs"), None);
    }
}
