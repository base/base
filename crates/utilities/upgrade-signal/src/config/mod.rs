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

    /// Node protocol version supported by this binary for contract-backed upgrade signals.
    ///
    /// Release branches sync the Cargo package version to the `GitHub` release tag, so release
    /// binaries advertise the release semver as their supported protocol version. Dev builds
    /// (workspace `0.0.0`) advertise `U256::MAX` so no contract minimum rejects them.
    pub fn node_protocol_version() -> U256 {
        Self::advertised_protocol_version(Self::packed_protocol_version(
            env!("CARGO_PKG_VERSION_MAJOR").parse::<u32>().expect("Cargo package major is numeric"),
            env!("CARGO_PKG_VERSION_MINOR").parse::<u32>().expect("Cargo package minor is numeric"),
            env!("CARGO_PKG_VERSION_PATCH").parse::<u32>().expect("Cargo package patch is numeric"),
        ))
    }

    /// Encodes a `major.minor.patch` version into the packed-semver `uint256` layout used by the
    /// L1 `ProtocolVersions` contract: `major << 96 | minor << 64 | patch << 32`, with the
    /// prerelease field left zero.
    pub const fn packed_protocol_version(major: u32, minor: u32, patch: u32) -> U256 {
        U256::from_limbs([(patch as u64) << 32, ((major as u64) << 32) | minor as u64, 0, 0])
    }

    /// Maps a packed Cargo version to the advertised node protocol version, promoting the
    /// dev-build `0.0.0` (zero) to `U256::MAX` so no contract minimum can reject a dev build.
    pub fn advertised_protocol_version(cargo_version: U256) -> U256 {
        if cargo_version == U256::ZERO { U256::MAX } else { cargo_version }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_build_advertises_max_protocol_version() {
        // Dev builds (0.0.0 -> zero) must bypass any contract minimum-version check.
        let zero = UpgradeSignalDefaults::packed_protocol_version(0, 0, 0);
        assert_eq!(UpgradeSignalDefaults::advertised_protocol_version(zero), U256::MAX);
    }

    #[test]
    fn release_build_advertises_its_own_version() {
        let version = UpgradeSignalDefaults::packed_protocol_version(1, 2, 0);
        assert_eq!(UpgradeSignalDefaults::advertised_protocol_version(version), version);
    }
}
