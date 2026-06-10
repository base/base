//! Upgrade signal configuration and CLI arguments.

use core::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_rpc_types_eth::BlockNumberOrTag;
use base_common_genesis::HardForkConfig;
use tracing::info;
use url::Url;

use crate::{
    contract::AlloyUpgradeSignalReader,
    error::UpgradeSignalError,
    metrics::UpgradeSignalMetrics,
    state::{UpgradeSignal, UpgradeSignalSchedule},
};

/// Default wall-clock interval used to check whether another L1 block polling window has elapsed.
pub const DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL: Duration = Duration::from_secs(12);

/// Default number of attempts to read the L1 upgrade signal schedule before failing startup.
pub const DEFAULT_UPGRADE_SIGNAL_READ_ATTEMPTS: u32 = 3;

/// Default backoff between L1 upgrade signal schedule read attempts.
pub const DEFAULT_UPGRADE_SIGNAL_READ_BACKOFF: Duration = Duration::from_secs(2);

/// Node protocol version supported by this binary for contract-backed upgrade signals.
///
/// Contract schedules with a higher minimum protocol version are rejected before any timestamp is
/// applied. Bump this with the node software that fully implements the next dynamic upgrade.
pub const DEFAULT_UPGRADE_SIGNAL_NODE_PROTOCOL_VERSION: u64 = 7;

/// Default hardfork IDs read from the L1 upgrade signal contract.
pub const DEFAULT_UPGRADE_SIGNAL_HARDFORK_IDS: &[&str] = HardForkConfig::CONTRACT_HARDFORK_IDS;

/// Controls which local schedule mutation paths are enabled for the L1 upgrade signal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum UpgradeSignalMode {
    /// Read the L1 signal and record metrics without mutating local fork schedules.
    #[default]
    MetricsOnly,
    /// Apply the L1 signal once before startup; live polling remains metrics-only.
    StartupApply,
    /// Apply the L1 signal before startup and expose manual runtime admin refresh.
    RuntimeAdmin,
}

impl UpgradeSignalMode {
    /// Returns true if this mode applies the schedule before node startup.
    pub const fn applies_at_startup(self) -> bool {
        matches!(self, Self::StartupApply | Self::RuntimeAdmin)
    }

    /// Returns true if this mode allows manual runtime schedule refresh.
    pub const fn allows_runtime_admin(self) -> bool {
        matches!(self, Self::RuntimeAdmin)
    }
}

/// L1 block tag used when reading the upgrade signal contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum UpgradeSignalBlockTag {
    /// Read at the latest finalized L1 block. Reorg-safe; recommended for production.
    #[default]
    Finalized,
    /// Read at the latest safe L1 block.
    Safe,
    /// Read at the latest L1 block. May reorg; useful for devnets without L1 finality.
    Latest,
}

impl UpgradeSignalBlockTag {
    /// Converts to the alloy block tag used by the contract reader.
    pub const fn block_number_or_tag(self) -> BlockNumberOrTag {
        match self {
            Self::Finalized => BlockNumberOrTag::Finalized,
            Self::Safe => BlockNumberOrTag::Safe,
            Self::Latest => BlockNumberOrTag::Latest,
        }
    }
}

/// Controls whether a service should perform its own startup signal read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpgradeSignalStartupMode {
    /// Read and apply the configured signal according to [`UpgradeSignalMode`].
    #[default]
    ReadAndApply,
    /// The caller has already applied the startup signal.
    AlreadyApplied,
}

impl UpgradeSignalStartupMode {
    /// Returns true if the service should perform its own startup signal read.
    pub const fn reads_and_applies(self) -> bool {
        matches!(self, Self::ReadAndApply)
    }
}

/// Error returned when CLI arguments cannot form an upgrade signal configuration.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeSignalConfigError {
    /// Hardfork IDs were set without a contract address.
    #[error("upgrade signal hardfork ID requires --upgrade-signal.contract")]
    MissingContractAddress,
    /// The hardfork ID is empty.
    #[error("upgrade signal hardfork ID cannot be empty")]
    EmptyHardforkId,
    /// An apply hardfork ID is not present in the set of read hardfork IDs.
    #[error(
        "upgrade signal apply hardfork ID `{0}` is not read; add it to --upgrade-signal.hardfork-id"
    )]
    ApplyHardforkIdNotRead(String),
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

    /// Hardfork IDs that are allowed to mutate the local schedule.
    ///
    /// If omitted, every read hardfork ID is eligible for application when the selected mode
    /// permits schedule mutation.
    #[arg(
        long = "upgrade-signal.apply-hardfork-id",
        env = "BASE_NODE_UPGRADE_SIGNAL_APPLY_HARDFORK_ID",
        value_delimiter = ','
    )]
    pub apply_hardfork_ids: Vec<String>,

    /// Upgrade signal application mode.
    #[arg(
        long = "upgrade-signal.mode",
        env = "BASE_NODE_UPGRADE_SIGNAL_MODE",
        value_enum,
        default_value_t = UpgradeSignalMode::MetricsOnly
    )]
    pub mode: UpgradeSignalMode,

    /// L1 block tag used to read the upgrade signal contract.
    #[arg(
        long = "upgrade-signal.l1-block-tag",
        env = "BASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG",
        value_enum,
        default_value_t = UpgradeSignalBlockTag::Finalized
    )]
    pub l1_block_tag: UpgradeSignalBlockTag,
}

impl UpgradeSignalArgs {
    /// Builds a schedule read configuration if the upgrade signal is enabled.
    pub fn config(&self) -> Result<Option<UpgradeSignalConfig>, UpgradeSignalConfigError> {
        let Some(contract_address) = self.contract_address else {
            if !self.hardfork_ids.is_empty() || !self.apply_hardfork_ids.is_empty() {
                return Err(UpgradeSignalConfigError::MissingContractAddress);
            }
            return Ok(None);
        };

        let hardfork_ids = Self::configured_hardfork_ids(&self.hardfork_ids)?;
        let apply_hardfork_ids =
            Self::configured_apply_hardfork_ids(&hardfork_ids, &self.apply_hardfork_ids)?;

        Ok(Some(UpgradeSignalConfig {
            contract_address,
            hardfork_ids,
            apply_hardfork_ids,
            mode: self.mode,
            l1_block_tag: self.l1_block_tag.block_number_or_tag(),
            node_protocol_version: U256::from(DEFAULT_UPGRADE_SIGNAL_NODE_PROTOCOL_VERSION),
        }))
    }

    /// Returns the configured hardfork IDs, or the default contract-backed hardfork schedule.
    pub fn configured_hardfork_ids(
        hardfork_ids: &[String],
    ) -> Result<Vec<String>, UpgradeSignalConfigError> {
        let source = if hardfork_ids.is_empty() {
            DEFAULT_UPGRADE_SIGNAL_HARDFORK_IDS.to_vec()
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

    /// Returns the configured apply hardfork IDs, or the read hardfork IDs when omitted.
    ///
    /// Every apply hardfork ID must also be a read hardfork ID, since only read signals can be
    /// applied. A non-subset apply ID is rejected rather than silently ignored.
    pub fn configured_apply_hardfork_ids(
        hardfork_ids: &[String],
        apply_hardfork_ids: &[String],
    ) -> Result<Vec<String>, UpgradeSignalConfigError> {
        if apply_hardfork_ids.is_empty() {
            return Ok(hardfork_ids.to_vec());
        }

        let apply_hardfork_ids = Self::configured_hardfork_ids(apply_hardfork_ids)?;
        for apply_hardfork_id in &apply_hardfork_ids {
            if !hardfork_ids.iter().any(|read_hardfork_id| {
                UpgradeSignalSchedule::hardfork_ids_match(read_hardfork_id, apply_hardfork_id)
            }) {
                return Err(UpgradeSignalConfigError::ApplyHardforkIdNotRead(
                    apply_hardfork_id.clone(),
                ));
            }
        }

        Ok(apply_hardfork_ids)
    }
}

/// Configuration for reading hardfork IDs from an L1 upgrade signal contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeSignalConfig {
    /// L1 upgrade signal contract or proxy address.
    pub contract_address: Address,
    /// Hardfork IDs to pass to the contract.
    pub hardfork_ids: Vec<String>,
    /// Hardfork IDs allowed to mutate local fork schedules.
    pub apply_hardfork_ids: Vec<String>,
    /// Local schedule mutation mode.
    pub mode: UpgradeSignalMode,
    /// L1 block tag used to read the contract.
    pub l1_block_tag: BlockNumberOrTag,
    /// Node protocol version supported by this binary.
    pub node_protocol_version: U256,
}

impl UpgradeSignalConfig {
    /// Creates a new schedule read configuration for one hardfork ID.
    pub fn new(contract_address: Address, hardfork_id: impl Into<String>) -> Self {
        let hardfork_id = hardfork_id.into();
        Self {
            contract_address,
            hardfork_ids: vec![hardfork_id.clone()],
            apply_hardfork_ids: vec![hardfork_id],
            mode: UpgradeSignalMode::MetricsOnly,
            l1_block_tag: BlockNumberOrTag::Finalized,
            node_protocol_version: U256::from(DEFAULT_UPGRADE_SIGNAL_NODE_PROTOCOL_VERSION),
        }
    }

    /// Returns a copy of `schedule` filtered to the hardfork IDs that may be applied locally.
    pub fn application_schedule(&self, schedule: &UpgradeSignalSchedule) -> UpgradeSignalSchedule {
        schedule.filtered_to_hardfork_ids(&self.apply_hardfork_ids)
    }

    /// Returns true if this node supports the minimum protocol version attached to `signal`.
    pub fn supports_signal_protocol_version(&self, signal: &UpgradeSignal) -> bool {
        signal.protocol_version <= self.node_protocol_version
    }

    /// Returns an error if a positive activation timestamp omits its minimum protocol version.
    ///
    /// This malformed-signal check applies to every signal read from L1, including signals that
    /// this node only observes (reads) but does not apply.
    pub fn validate_signal_has_protocol_version(
        &self,
        signal: &UpgradeSignal,
    ) -> Result<(), UpgradeSignalError> {
        if signal.activation_timestamp > 0 && signal.protocol_version == U256::ZERO {
            return Err(UpgradeSignalError::missing_protocol_version(signal.hardfork_id.clone()));
        }

        Ok(())
    }

    /// Returns an error if this binary cannot support the signal's minimum protocol version.
    ///
    /// This capability check applies only to signals this node will apply, so a node can observe
    /// a future hardfork that requires newer software without aborting.
    pub fn validate_signal_supported_protocol_version(
        &self,
        signal: &UpgradeSignal,
    ) -> Result<(), UpgradeSignalError> {
        if self.supports_signal_protocol_version(signal) {
            return Ok(());
        }

        Err(UpgradeSignalError::unsupported_protocol_version(
            signal.hardfork_id.clone(),
            signal.protocol_version,
            self.node_protocol_version,
        ))
    }

    /// Validates the minimum protocol version attached to one signal (presence and support).
    pub fn validate_signal_protocol_version(
        &self,
        signal: &UpgradeSignal,
    ) -> Result<(), UpgradeSignalError> {
        self.validate_signal_has_protocol_version(signal)?;
        self.validate_signal_supported_protocol_version(signal)
    }

    /// Validates that every positive signal in the full read schedule carries a protocol version.
    pub fn validate_read_schedule_protocol_versions(
        &self,
        schedule: &UpgradeSignalSchedule,
    ) -> Result<(), UpgradeSignalError> {
        for signal in &schedule.signals {
            self.validate_signal_has_protocol_version(signal)?;
        }

        Ok(())
    }

    /// Validates that this binary supports every applied signal's minimum protocol version.
    pub fn validate_applied_schedule_protocol_versions(
        &self,
        schedule: &UpgradeSignalSchedule,
    ) -> Result<(), UpgradeSignalError> {
        for signal in &schedule.signals {
            self.validate_signal_supported_protocol_version(signal)?;
        }

        Ok(())
    }

    /// Reads the L1 schedule, records metrics, logs each signal, validates it, and returns the
    /// application-filtered schedule ready to apply.
    ///
    /// This is the single read pipeline shared by startup application and runtime refresh. The
    /// malformed-signal check runs over the full read schedule; the protocol-version support check
    /// runs only over the schedule this node will apply.
    pub async fn read_validated_application_schedule(
        &self,
        reader: &AlloyUpgradeSignalReader,
        log_context: &'static str,
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let schedule = reader
            .read_schedule_with_retries(
                &self.hardfork_ids,
                DEFAULT_UPGRADE_SIGNAL_READ_ATTEMPTS,
                DEFAULT_UPGRADE_SIGNAL_READ_BACKOFF,
            )
            .await?;

        UpgradeSignalMetrics::record_schedule(&schedule);
        for signal in &schedule.signals {
            info!(
                target: "upgrade_signal",
                context = log_context,
                hardfork_id = %signal.hardfork_id,
                activation_timestamp = signal.activation_timestamp,
                minimum_protocol_version = %signal.protocol_version,
                node_protocol_version = %self.node_protocol_version,
                l1_block_number = signal.l1_block_number,
                "read dynamic upgrade signal"
            );
        }

        self.validate_read_schedule_protocol_versions(&schedule)?;
        let application_schedule = self.application_schedule(&schedule);
        self.validate_applied_schedule_protocol_versions(&application_schedule)?;

        Ok(application_schedule)
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
        assert_eq!(config.apply_hardfork_ids, DEFAULT_UPGRADE_SIGNAL_HARDFORK_IDS);
        assert_eq!(config.mode, UpgradeSignalMode::MetricsOnly);
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
    fn defaults_to_finalized_block_tag() {
        let config =
            UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), "azul");

        assert_eq!(config.l1_block_tag, BlockNumberOrTag::Finalized);
    }

    #[test]
    fn maps_configured_block_tag() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            l1_block_tag: UpgradeSignalBlockTag::Latest,
            ..Default::default()
        };

        assert_eq!(args.config().unwrap().unwrap().l1_block_tag, BlockNumberOrTag::Latest);
    }

    #[test]
    fn builds_enabled_config() {
        let contract = address!("0000000000000000000000000000000000000001");
        let args = UpgradeSignalArgs {
            contract_address: Some(contract),
            hardfork_ids: vec!["azul".to_string()],
            mode: UpgradeSignalMode::StartupApply,
            ..Default::default()
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.contract_address, contract);
        assert_eq!(config.hardfork_ids, ["azul"]);
        assert_eq!(config.apply_hardfork_ids, ["azul"]);
        assert_eq!(config.mode, UpgradeSignalMode::StartupApply);
        assert_eq!(
            config.node_protocol_version,
            U256::from(DEFAULT_UPGRADE_SIGNAL_NODE_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn uses_explicit_apply_ids() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            hardfork_ids: vec!["azul".to_string(), "beryl".to_string()],
            apply_hardfork_ids: vec!["beryl".to_string()],
            mode: UpgradeSignalMode::RuntimeAdmin,
            ..Default::default()
        };

        let config = args.config().unwrap().unwrap();

        assert_eq!(config.hardfork_ids, ["azul", "beryl"]);
        assert_eq!(config.apply_hardfork_ids, ["beryl"]);
        assert_eq!(config.mode, UpgradeSignalMode::RuntimeAdmin);
    }

    #[test]
    fn rejects_apply_id_not_in_read_ids() {
        let args = UpgradeSignalArgs {
            contract_address: Some(address!("0000000000000000000000000000000000000001")),
            hardfork_ids: vec!["azul".to_string()],
            apply_hardfork_ids: vec!["beryl".to_string()],
            ..Default::default()
        };

        assert!(matches!(
            args.config().unwrap_err(),
            UpgradeSignalConfigError::ApplyHardforkIdNotRead(_)
        ));
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

    fn malformed_read_only_schedule(config: &UpgradeSignalConfig) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(vec![
            signal(config.node_protocol_version),
            UpgradeSignal {
                hardfork_id: "beryl".to_string(),
                activation_timestamp: 5,
                protocol_version: U256::ZERO,
                l1_block_number: 1,
            },
        ])
    }

    #[test]
    fn read_validation_rejects_missing_protocol_version_on_read_only_fork() {
        let config =
            UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), "azul");
        let schedule = malformed_read_only_schedule(&config);

        // The malformed beryl signal is read but not applied; it must still be rejected.
        assert!(matches!(
            config.validate_read_schedule_protocol_versions(&schedule).unwrap_err(),
            UpgradeSignalError::MissingProtocolVersion(_)
        ));
    }

    #[test]
    fn applied_validation_allows_unsupported_version_on_read_only_fork() {
        let config =
            UpgradeSignalConfig::new(address!("0000000000000000000000000000000000000001"), "azul");
        let schedule = UpgradeSignalSchedule::new(vec![
            signal(config.node_protocol_version),
            UpgradeSignal {
                hardfork_id: "beryl".to_string(),
                activation_timestamp: 5,
                protocol_version: config.node_protocol_version + U256::from(1),
                l1_block_number: 1,
            },
        ]);
        let application_schedule = config.application_schedule(&schedule);

        // beryl is observed but not applied, so its newer protocol requirement must not abort.
        assert!(config.validate_applied_schedule_protocol_versions(&application_schedule).is_ok());
    }
}
