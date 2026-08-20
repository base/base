//! Upgrade signal configuration and CLI arguments.

use core::time::Duration;

use alloy_primitives::U256;

use crate::PackedProtocolVersion;

mod args;
pub use args::{UpgradeSignalArgs, UpgradeSignalL1RpcArgs, UpgradeSignalStartupConfig};

mod schedule;
pub use schedule::UpgradeSignalConfig;

mod types;
pub use types::{UpgradeSignalBlockTag, UpgradeSignalMode, UpgradeSignalStartupMode};

/// Default values used by the upgrade signal reader and runtime applier.
#[derive(Debug)]
pub struct UpgradeSignalDefaults;

impl UpgradeSignalDefaults {
    /// Default total deadline for each upgrade signal JSON-RPC request.
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

    /// Default number of attempts to read the L1 upgrade signal schedule before failing startup.
    pub const READ_ATTEMPTS: u32 = 3;

    /// Initial backoff between L1 upgrade signal schedule read attempts.
    pub const READ_BACKOFF: Duration = Duration::from_secs(2);

    /// Maximum jittered backoff between L1 upgrade signal schedule read attempts.
    pub const READ_MAX_BACKOFF: Duration = Duration::from_secs(10);

    /// Fixed interval between fail-closed startup schedule read attempts while the L1 contract has
    /// not yet returned a valid schedule.
    ///
    /// Startup blocks indefinitely on an empty or unreachable contract (see
    /// [`UpgradeSignalConfig::read_required_startup_schedule`](crate::UpgradeSignalConfig::read_required_startup_schedule)),
    /// so this is paced to keep the loud retry logs legible rather than to recover quickly.
    pub const STARTUP_SCHEDULE_RETRY_INTERVAL: Duration = Duration::from_secs(5);

    /// Node protocol version supported by this binary for contract-backed upgrade signals.
    ///
    /// Release branches sync the Cargo package version to the `GitHub` release tag, so release
    /// binaries advertise the release semver as their supported protocol version. Dev builds
    /// (workspace `0.0.0`) advertise the maximum version-type-`0` version so no contract minimum
    /// rejects them.
    pub fn node_protocol_version() -> U256 {
        Self::advertised_protocol_version(Self::packed_protocol_version(
            env!("CARGO_PKG_VERSION_MAJOR").parse::<u32>().expect("Cargo package major is numeric"),
            env!("CARGO_PKG_VERSION_MINOR").parse::<u32>().expect("Cargo package minor is numeric"),
            env!("CARGO_PKG_VERSION_PATCH").parse::<u32>().expect("Cargo package patch is numeric"),
        ))
    }

    /// Encodes a final-release `major.minor.patch` version into the packed-semver `uint256` layout
    /// used by the L1 `ProtocolVersions` contract, leaving the prerelease and build fields zero.
    ///
    /// See [`PackedProtocolVersion`] for the field layout and the ordering rules that govern
    /// protocol-version compatibility checks.
    pub const fn packed_protocol_version(major: u32, minor: u32, patch: u32) -> U256 {
        PackedProtocolVersion::pack(major, minor, patch, 0).into_inner()
    }

    /// Maps a packed Cargo version to the advertised node protocol version, promoting the
    /// dev-build `0.0.0` (zero) to the maximum version-type-`0` version so no contract minimum can
    /// reject a dev build.
    ///
    /// The sentinel is the top element of [`PackedProtocolVersion`]'s ordering by construction:
    /// `U256::MAX` is *not* usable here, since it decodes to a pre-release (its pre-release field is
    /// non-zero) and so ranks below the corresponding final release under the semver ordering.
    pub fn advertised_protocol_version(cargo_version: U256) -> U256 {
        if cargo_version == U256::ZERO {
            PackedProtocolVersion::pack(u32::MAX, u32::MAX, u32::MAX, 0).into_inner()
        } else {
            cargo_version
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_build_advertises_max_protocol_version() {
        // Dev builds (0.0.0 -> zero) must bypass any contract minimum-version check, so the
        // sentinel is the top element of the semver ordering (not the raw `U256::MAX`, which decodes
        // to a pre-release and ranks below its final release).
        let zero = UpgradeSignalDefaults::packed_protocol_version(0, 0, 0);
        let sentinel = UpgradeSignalDefaults::advertised_protocol_version(zero);
        assert_eq!(
            sentinel,
            PackedProtocolVersion::pack(u32::MAX, u32::MAX, u32::MAX, 0).into_inner()
        );

        // No version-type-`0` minimum can outrank the sentinel.
        let highest_minimum =
            UpgradeSignalDefaults::packed_protocol_version(u32::MAX, u32::MAX, u32::MAX);
        assert!(
            PackedProtocolVersion::new(highest_minimum) <= PackedProtocolVersion::new(sentinel)
        );
    }

    #[test]
    fn release_build_advertises_its_own_version() {
        let version = UpgradeSignalDefaults::packed_protocol_version(1, 2, 0);
        assert_eq!(UpgradeSignalDefaults::advertised_protocol_version(version), version);
    }
}
