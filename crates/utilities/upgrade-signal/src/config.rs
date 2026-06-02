//! Upgrade signal configuration and CLI arguments.

use core::time::Duration;

use alloy_primitives::Address;
use url::Url;

/// Default wall-clock interval used to check whether another L1 block polling window has elapsed.
pub const DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL: Duration = Duration::from_secs(12);

/// Default hardfork IDs read from the L1 upgrade signal contract.
pub const DEFAULT_UPGRADE_SIGNAL_HARDFORK_IDS: &[&str] = &[
    "regolith",
    "canyon",
    "delta",
    "ecotone",
    "fjord",
    "granite",
    "holocene",
    "pectra_blob_schedule",
    "isthmus",
    "jovian",
    "azul",
    "beryl",
];

/// Error returned when CLI arguments cannot form an upgrade signal configuration.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeSignalConfigError {
    /// Hardfork IDs were set without a contract address.
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

    /// Hardfork IDs to pass to the L1 upgrade signal contract.
    ///
    /// If omitted while the contract is configured, all timestamp-based Base rollup hardfork IDs
    /// are read.
    #[arg(
        long = "upgrade-signal.hardfork-id",
        env = "BASE_NODE_UPGRADE_SIGNAL_HARDFORK_ID",
        value_delimiter = ','
    )]
    pub hardfork_ids: Vec<String>,
}

impl UpgradeSignalArgs {
    /// Builds an observer configuration if the upgrade signal is enabled.
    pub fn config(&self) -> Result<Option<UpgradeSignalConfig>, UpgradeSignalConfigError> {
        let Some(contract_address) = self.contract_address else {
            if !self.hardfork_ids.is_empty() {
                return Err(UpgradeSignalConfigError::MissingContractAddress);
            }
            return Ok(None);
        };

        let hardfork_ids = Self::configured_hardfork_ids(&self.hardfork_ids)?;

        Ok(Some(UpgradeSignalConfig { contract_address, hardfork_ids }))
    }

    /// Returns the configured hardfork IDs, or the default contract-backed hardfork schedule.
    pub fn configured_hardfork_ids(
        hardfork_ids: &[String],
    ) -> Result<Vec<String>, UpgradeSignalConfigError> {
        let source = if hardfork_ids.is_empty() {
            DEFAULT_UPGRADE_SIGNAL_HARDFORK_IDS.iter().copied().collect::<Vec<_>>()
        } else {
            hardfork_ids.iter().map(String::as_str).collect::<Vec<_>>()
        };
        let mut ids = Vec::new();
        for hardfork_id in source {
            let hardfork_id = hardfork_id.trim();
            if hardfork_id.is_empty() {
                return Err(UpgradeSignalConfigError::EmptyHardforkId);
            }
            if !ids.iter().any(|existing: &String| existing.eq_ignore_ascii_case(hardfork_id)) {
                ids.push(hardfork_id.to_string());
            }
        }

        Ok(ids)
    }
}

/// Runtime configuration for observing hardfork IDs on an L1 upgrade signal contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSignalConfig {
    /// L1 upgrade signal contract or proxy address.
    pub contract_address: Address,
    /// Hardfork IDs to pass to the contract.
    pub hardfork_ids: Vec<String>,
}

impl UpgradeSignalConfig {
    /// Creates a new observer configuration with default polling settings.
    pub fn new(contract_address: Address, hardfork_id: impl Into<String>) -> Self {
        Self { contract_address, hardfork_ids: vec![hardfork_id.into()] }
    }

    /// Creates a new observer configuration for multiple hardfork IDs.
    pub fn new_many(contract_address: Address, hardfork_ids: Vec<String>) -> Self {
        Self { contract_address, hardfork_ids }
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
    fn uses_default_ids_for_contract_without_hardfork_id() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            ..Default::default()
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.hardfork_ids, DEFAULT_UPGRADE_SIGNAL_HARDFORK_IDS);
    }

    #[test]
    fn rejects_hardfork_id_without_contract() {
        let args =
            UpgradeSignalArgs { hardfork_ids: vec!["azul".to_string()], ..Default::default() };

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
            hardfork_ids: vec!["azul".to_string()],
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.contract_address, contract);
        assert_eq!(config.hardfork_ids, ["azul"]);
    }
}
