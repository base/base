//! Upgrade signal configuration and CLI arguments.

use core::time::Duration;

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
    /// Contract schedules with a higher minimum protocol version are rejected before any timestamp is
    /// applied. Bump this with the node software that fully implements the next dynamic upgrade.
    pub const NODE_PROTOCOL_VERSION: u64 = 7;
}
