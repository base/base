//! Runtime environment of the reporting node: cloud vs bare metal, CPU, memory, and disk.

use serde::{Deserialize, Serialize};

/// Whether the node runs on a cloud instance or on bare metal.
///
/// Derived from the firmware description at `/sys/class/dmi/id/sys_vendor` rather than by
/// probing a cloud metadata endpoint: reading a file is passive, fast, and does not look
/// intrusive in a packet capture.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwarePlatform {
    /// A recognized cloud provider's virtual machine.
    Cloud,
    /// Physical hardware with no recognized cloud vendor string.
    #[serde(rename = "baremetal")]
    BareMetal,
    /// The platform could not be determined, typically because the DMI path does not exist.
    #[default]
    Unknown,
}

impl HardwarePlatform {
    /// Returns the stable wire label for this platform.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Cloud => "cloud",
            Self::BareMetal => "baremetal",
            Self::Unknown => "unknown",
        }
    }
}

/// The `hardware.*` block, present on every report event.
///
/// Every field beyond `platform`, `os`, and `arch` is optional because collection degrades
/// rather than fails: a non-Linux host, a container without `/sys` mounted, or a permission
/// error each yield `None` instead of an error. `hardware.class` is deliberately absent, since it is
/// bucketed at ingest from `cpu_cores`, `ram_bytes`, and `disk_rotational` so the bucketing
/// can be redefined later and recomputed from the archive.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    /// Cloud instance or bare metal.
    pub platform: HardwarePlatform,
    /// Raw firmware vendor string, kept next to `platform` so the classification can be redone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_vendor: Option<String>,
    /// Target operating system, from `std::env::consts::OS`.
    pub os: String,
    /// Target CPU architecture, from `std::env::consts::ARCH`.
    pub arch: String,
    /// Kernel release string, e.g. `6.8.0-45-generic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// CPU model string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    /// Number of logical CPU cores visible to the process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
    /// Total system memory in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_bytes: Option<u64>,
    /// Model string of the disk backing the node's data directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_model: Option<String>,
    /// Whether that disk is rotational. Distinguishes `baremetal-nvme` from `baremetal-spinning`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_rotational: Option<bool>,
    /// Total capacity of the filesystem holding the node's data directory, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    /// Free space on that filesystem, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
    /// Filesystem type backing the node's data directory, e.g. `ext4` or `zfs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_type: Option<String>,
    /// Mount options for that filesystem, comma-separated as `/proc/mounts` reports them.
    ///
    /// Carries the flags that change how the node performs, `noatime` and `discard` among them,
    /// and the one that explains a node that cannot write at all: `ro`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_flags: Option<String>,
    /// Whether the data directory sits on network-attached rather than local storage.
    ///
    /// The single most useful hardware fact we can collect: `ext4` on a local `NVMe` and `ext4` on
    /// a network volume are indistinguishable from `disk_model` alone and perform nothing alike,
    /// so a release that regresses only on network storage is invisible without this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_network_storage: Option<bool>,
}
