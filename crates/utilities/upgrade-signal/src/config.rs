//! Upgrade signal configuration and CLI arguments.

use core::time::Duration;

use alloy_primitives::Address;
use url::Url;

/// Default wall-clock interval used to check whether another L1 block polling window has elapsed.
pub const DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL: Duration = Duration::from_secs(12);

/// Error returned when CLI arguments cannot form an upgrade signal configuration.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeSignalConfigError {
    /// A contract address was set without a hardfork ID.
    #[error("upgrade signal contract requires --upgrade-signal.hardfork-id")]
    MissingHardforkId,
    /// A hardfork ID was set without a contract address.
    #[error("upgrade signal hardfork ID requires --upgrade-signal.contract")]
    MissingContractAddress,
    /// The hardfork ID is empty.
    #[error("upgrade signal hardfork ID cannot be empty")]
    EmptyHardforkId,
}

/// CLI arguments shared by nodes that observe the L1 upgrade signal contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct UpgradeSignalArgs {
    /// L1 upgrade signal contract or proxy address.
    #[arg(long = "upgrade-signal.contract", env = "BASE_NODE_UPGRADE_SIGNAL_CONTRACT")]
    pub contract_address: Option<Address>,

    /// Hardfork ID to pass to the L1 upgrade signal contract.
    #[arg(long = "upgrade-signal.hardfork-id", env = "BASE_NODE_UPGRADE_SIGNAL_HARDFORK_ID")]
    pub hardfork_id: Option<String>,
}

impl UpgradeSignalArgs {
    /// Builds an observer configuration if the upgrade signal is enabled.
    pub fn config(&self) -> Result<Option<UpgradeSignalConfig>, UpgradeSignalConfigError> {
        let Some(contract_address) = self.contract_address else {
            if self.hardfork_id.is_some() {
                return Err(UpgradeSignalConfigError::MissingContractAddress);
            }
            return Ok(None);
        };

        let Some(hardfork_id) = self.hardfork_id.clone() else {
            return Err(UpgradeSignalConfigError::MissingHardforkId);
        };
        if hardfork_id.is_empty() {
            return Err(UpgradeSignalConfigError::EmptyHardforkId);
        }

        Ok(Some(UpgradeSignalConfig { contract_address, hardfork_id }))
    }
}

/// Runtime configuration for observing one hardfork ID on an L1 upgrade signal contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSignalConfig {
    /// L1 upgrade signal contract or proxy address.
    pub contract_address: Address,
    /// Hardfork ID to pass to the contract.
    pub hardfork_id: String,
}

impl UpgradeSignalConfig {
    /// Creates a new observer configuration with default polling settings.
    pub fn new(contract_address: Address, hardfork_id: impl Into<String>) -> Self {
        Self { contract_address, hardfork_id: hardfork_id.into() }
    }
}

/// CLI argument for the L1 RPC endpoint used by standalone execution nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct UpgradeSignalL1RpcArgs {
    /// L1 execution RPC URL used to read the upgrade signal contract.
    #[arg(long = "upgrade-signal.l1-rpc", env = "BASE_NODE_UPGRADE_SIGNAL_L1_RPC")]
    pub upgrade_signal_l1_rpc: Option<Url>,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn disabled_when_no_contract_or_hardfork_id() {
        let args = UpgradeSignalArgs::default();

        assert_eq!(args.config().unwrap(), None);
    }

    #[test]
    fn rejects_contract_without_hardfork_id() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            ..Default::default()
        };

        assert!(matches!(args.config().unwrap_err(), UpgradeSignalConfigError::MissingHardforkId));
    }

    #[test]
    fn rejects_hardfork_id_without_contract() {
        let args =
            UpgradeSignalArgs { hardfork_id: Some("azul".to_string()), ..Default::default() };

        assert!(matches!(
            args.config().unwrap_err(),
            UpgradeSignalConfigError::MissingContractAddress
        ));
    }

    #[test]
    fn builds_enabled_config() {
        let contract = address!("0000000000000000000000000000000000000001");
        let args = UpgradeSignalArgs {
            contract_address: Some(contract),
            hardfork_id: Some("azul".to_string()),
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.contract_address, contract);
        assert_eq!(config.hardfork_id, "azul");
    }
}
