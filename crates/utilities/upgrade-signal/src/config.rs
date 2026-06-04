//! Upgrade signal configuration and CLI arguments.

use core::time::Duration;

use alloy_primitives::{Address, U256};
use url::Url;

use crate::{
    error::UpgradeSignalError,
    state::{UpgradeSignal, UpgradeSignalSchedule},
};

/// Default wall-clock interval used to check whether another L1 block polling window has elapsed.
pub const DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL: Duration = Duration::from_secs(12);

/// Node protocol version supported by this binary for contract-backed upgrade signals.
///
/// Contract schedules with a higher minimum protocol version are rejected before any timestamp is
/// applied. Bump this with the node software that fully implements the next dynamic upgrade.
pub const DEFAULT_UPGRADE_SIGNAL_NODE_PROTOCOL_VERSION: u64 = 7;

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

/// CLI arguments shared by nodes that read the L1 upgrade signal contract.
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
    /// Builds a schedule read configuration if the upgrade signal is enabled.
    pub fn config(&self) -> Result<Option<UpgradeSignalConfig>, UpgradeSignalConfigError> {
        let Some(contract_address) = self.contract_address else {
            if !self.hardfork_ids.is_empty() {
                return Err(UpgradeSignalConfigError::MissingContractAddress);
            }
            return Ok(None);
        };

        let hardfork_ids = Self::configured_hardfork_ids(&self.hardfork_ids)?;

        Ok(Some(UpgradeSignalConfig {
            contract_address,
            hardfork_ids,
            node_protocol_version: U256::from(DEFAULT_UPGRADE_SIGNAL_NODE_PROTOCOL_VERSION),
        }))
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

/// Configuration for reading hardfork IDs from an L1 upgrade signal contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSignalConfig {
    /// L1 upgrade signal contract or proxy address.
    pub contract_address: Address,
    /// Hardfork IDs to pass to the contract.
    pub hardfork_ids: Vec<String>,
    /// Node protocol version supported by this binary.
    pub node_protocol_version: U256,
}

impl UpgradeSignalConfig {
    /// Creates a new schedule read configuration for one hardfork ID.
    pub fn new(contract_address: Address, hardfork_id: impl Into<String>) -> Self {
        Self {
            contract_address,
            hardfork_ids: vec![hardfork_id.into()],
            node_protocol_version: U256::from(DEFAULT_UPGRADE_SIGNAL_NODE_PROTOCOL_VERSION),
        }
    }

    /// Returns true if this node supports the minimum protocol version attached to `signal`.
    pub fn supports_signal_protocol_version(&self, signal: &UpgradeSignal) -> bool {
        signal.protocol_version <= self.node_protocol_version
    }

    /// Validates the minimum protocol version attached to one signal.
    pub fn validate_signal_protocol_version(
        &self,
        signal: &UpgradeSignal,
    ) -> Result<(), UpgradeSignalError> {
        if signal.activation_timestamp > 0 && signal.protocol_version == U256::ZERO {
            return Err(UpgradeSignalError::missing_protocol_version(signal.hardfork_id.clone()));
        }

        if self.supports_signal_protocol_version(signal) {
            return Ok(());
        }

        Err(UpgradeSignalError::unsupported_protocol_version(
            signal.hardfork_id.clone(),
            signal.protocol_version,
            self.node_protocol_version,
        ))
    }

    /// Validates every minimum protocol version in a schedule.
    pub fn validate_schedule_protocol_versions(
        &self,
        schedule: &UpgradeSignalSchedule,
    ) -> Result<(), UpgradeSignalError> {
        for signal in &schedule.signals {
            self.validate_signal_protocol_version(signal)?;
        }

        Ok(())
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
    use alloy_primitives::{U256, address};

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
        assert_eq!(
            config.node_protocol_version,
            U256::from(DEFAULT_UPGRADE_SIGNAL_NODE_PROTOCOL_VERSION)
        );
    }

    fn signal(protocol_version: U256) -> UpgradeSignal {
        UpgradeSignal {
            hardfork_id: "azul".to_string(),
            activation_timestamp: 42,
            protocol_version,
            l1_block_number: 1,
        }
    }

    #[test]
    fn accepts_signal_at_node_protocol_version() {
        let config =
            UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), "azul");

        assert!(
            config.validate_signal_protocol_version(&signal(config.node_protocol_version)).is_ok()
        );
    }

    #[test]
    fn rejects_signal_above_node_protocol_version() {
        let config =
            UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), "azul");
        let minimum_protocol_version = config.node_protocol_version + U256::from(1);

        assert!(matches!(
            config.validate_signal_protocol_version(&signal(minimum_protocol_version)).unwrap_err(),
            UpgradeSignalError::UnsupportedProtocolVersion { .. }
        ));
    }

    #[test]
    fn rejects_positive_signal_without_protocol_version() {
        let config =
            UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), "azul");

        assert!(matches!(
            config.validate_signal_protocol_version(&signal(U256::ZERO)).unwrap_err(),
            UpgradeSignalError::MissingProtocolVersion(_)
        ));
    }
}
