//! Upgrade signal configuration and CLI arguments.

use core::time::Duration;

use alloy_primitives::U256;

mod args;
pub use args::{UpgradeSignalArgs, UpgradeSignalL1RpcArgs, UpgradeSignalStartupConfig};

mod error;
pub use error::UpgradeSignalConfigError;

mod schedule;
pub use schedule::UpgradeSignalConfig;

mod types;
pub use types::{UpgradeSignalBlockTag, UpgradeSignalMode, UpgradeSignalStartupMode};

/// Default values used by the upgrade signal reader and runtime applier.
#[derive(Debug)]
pub struct UpgradeSignalDefaults;

impl UpgradeSignalDefaults {
    /// Default wall-clock interval used to check whether another L1 block polling window has elapsed.
    pub const POLL_INTERVAL: Duration = Duration::from_secs(12);

    /// Default number of attempts to read the L1 upgrade signal schedule before failing startup.
    pub const READ_ATTEMPTS: u32 = 3;

    /// Default backoff between L1 upgrade signal schedule read attempts.
    pub const READ_BACKOFF: Duration = Duration::from_secs(2);

    /// Node protocol version supported by this binary for contract-backed upgrade signals
    /// (packed semver `1.1.0`).
    ///
    /// Contract schedules with a higher minimum protocol version are rejected before any timestamp is
    /// applied. Bump this with the node software that fully implements the next dynamic upgrade.
    pub const NODE_PROTOCOL_VERSION: U256 = Self::packed_protocol_version(1, 1, 0);

    /// Encodes a `major.minor.patch` version into the packed-semver `uint256` layout used by the
    /// L1 `ProtocolVersions` contract: `major << 96 | minor << 64 | patch << 32`, with the
    /// prerelease field left zero.
    pub const fn packed_protocol_version(major: u32, minor: u32, patch: u32) -> U256 {
        U256::from_limbs([(patch as u64) << 32, ((major as u64) << 32) | minor as u64, 0, 0])
    }
}
