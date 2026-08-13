//! L1 upgrade signal support for system test stacks.
//!
//! Deploys the mock `ProtocolVersions` schedule contract to the L1 stack and builds the
//! [`UpgradeSignalConfig`] consumed by the in-process consensus nodes.

use alloy_network::EthereumWallet;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use base_common_genesis::{BaseUpgrade, RollupConfig};
use base_execution_cli::ExecutionUpgradeSignalConfig;
use base_test_utils::MockProtocolVersions;
use base_upgrade_signal::{
    UpgradeSignalBlockTag, UpgradeSignalConfig, UpgradeSignalDefaults, UpgradeSignalMode,
};
use eyre::{Result, WrapErr};
use url::Url;

use crate::config::ANVIL_ACCOUNT_4;

/// Options for enabling the L1 upgrade signal on a system test stack.
///
/// When set on [`SystemTestStackBuilder`](crate::SystemTestStackBuilder), the stack deploys a
/// [`MockProtocolVersions`] contract to L1, seeds it with the configured schedule, and starts
/// both in-process consensus nodes with the resulting upgrade signal configuration.
#[derive(Debug, Clone)]
pub struct UpgradeSignalStackOptions {
    /// Local schedule mutation mode used by the consensus nodes.
    pub mode: UpgradeSignalMode,
    /// Optional mode for the client execution node; `None` leaves execution nodes unwired.
    pub execution_mode: Option<UpgradeSignalMode>,
    /// Explicit `(upgrade, activation timestamp)` entries seeded into the mock contract.
    ///
    /// Nodes always read and apply the full contract schedule, and a `0` contract timestamp
    /// explicitly clears an upgrade. The stack therefore seeds the contract with a baseline
    /// derived from the deployed rollup config (see [`Self::baseline_schedule`]) and these
    /// entries override the baseline per upgrade.
    pub schedule: Vec<(BaseUpgrade, u64)>,
    /// Minimum protocol version seeded into the mock contract (packed semver, must be nonzero
    /// while any activation timestamp is positive).
    pub minimum_protocol_version: U256,
}

impl UpgradeSignalStackOptions {
    /// Creates upgrade signal options with an empty schedule and a minimal protocol version.
    pub const fn new(mode: UpgradeSignalMode) -> Self {
        Self {
            mode,
            execution_mode: None,
            schedule: Vec::new(),
            minimum_protocol_version: UpgradeSignalDefaults::packed_protocol_version(0, 0, 1),
        }
    }

    /// Also wires the client execution node to the upgrade signal with the given mode.
    pub const fn with_execution_mode(mut self, mode: UpgradeSignalMode) -> Self {
        self.execution_mode = Some(mode);
        self
    }

    /// Adds an initial activation timestamp for one contract-backed upgrade.
    pub fn with_upgrade(mut self, upgrade: BaseUpgrade, activation_timestamp: u64) -> Self {
        self.schedule.push((upgrade, activation_timestamp));
        self
    }

    /// Sets the minimum protocol version seeded into the mock contract.
    pub const fn with_minimum_protocol_version(mut self, version: U256) -> Self {
        self.minimum_protocol_version = version;
        self
    }

    /// Returns the baseline schedule derived from a deployed rollup config.
    ///
    /// Every contract-backed upgrade the rollup config already schedules keeps its activation
    /// timestamp (genesis-activated upgrades map to the L2 genesis time, since the contract
    /// cannot express `0` as "active"), so applying the full contract schedule never clears an
    /// upgrade the running chain depends on. Upgrades the rollup config does not schedule stay
    /// at `0` (not scheduled).
    pub fn baseline_schedule(rollup_config: &RollupConfig) -> Vec<(BaseUpgrade, u64)> {
        BaseUpgrade::CONTRACT_VARIANTS
            .iter()
            .filter_map(|&upgrade| {
                rollup_config.upgrades.activation_timestamp(upgrade).map(|timestamp| {
                    let seeded =
                        if timestamp == 0 { rollup_config.genesis.l2_time } else { timestamp };
                    (upgrade, seeded)
                })
            })
            .collect()
    }

    /// Builds the consensus-side upgrade signal configuration for a deployed contract.
    pub fn signal_config(&self, contract_address: Address) -> UpgradeSignalConfig {
        self.signal_config_with_mode(contract_address, self.mode)
    }

    /// Builds the client execution node's upgrade signal configuration, when execution reads
    /// are enabled via [`Self::with_execution_mode`].
    pub fn execution_signal_config(
        &self,
        contract_address: Address,
        l1_rpc: Url,
    ) -> Option<ExecutionUpgradeSignalConfig> {
        self.execution_mode.map(|mode| ExecutionUpgradeSignalConfig {
            signal_config: self.signal_config_with_mode(contract_address, mode),
            l1_rpc,
        })
    }

    /// Builds an upgrade signal configuration for a deployed contract with an explicit mode.
    ///
    /// Reads at the latest L1 block because the devnet L1 takes several epochs to finalize.
    pub fn signal_config_with_mode(
        &self,
        contract_address: Address,
        mode: UpgradeSignalMode,
    ) -> UpgradeSignalConfig {
        UpgradeSignalConfig {
            contract_address,
            mode,
            l1_block_tag: UpgradeSignalBlockTag::Latest,
            node_protocol_version: UpgradeSignalDefaults::node_protocol_version(),
            request_timeout: UpgradeSignalDefaults::REQUEST_TIMEOUT,
        }
    }
}

/// Client for the mock L1 `ProtocolVersions` contract deployed by a system test stack.
///
/// Transactions are signed with [`ANVIL_ACCOUNT_4`], which is funded on L1 genesis and not
/// used by any other L1 role (deployer, batcher, proposer, challenger), so schedule updates
/// never race another service's nonce.
#[derive(Debug, Clone)]
pub struct MockProtocolVersionsClient {
    /// Public L1 RPC URL used for contract transactions.
    pub l1_rpc_url: Url,
    /// Deployed mock contract address.
    pub address: Address,
    /// Baseline `(upgrade, activation timestamp)` entries preserved by every schedule write,
    /// derived from the deployed rollup config.
    pub baseline: Vec<(BaseUpgrade, u64)>,
}

impl MockProtocolVersionsClient {
    /// Deploys the mock contract to L1 and seeds it with the rollup config baseline overlaid
    /// with the options' explicit schedule entries.
    pub async fn deploy(
        l1_rpc_url: Url,
        options: &UpgradeSignalStackOptions,
        rollup_config: &RollupConfig,
    ) -> Result<Self> {
        let provider = Self::wallet_provider(&l1_rpc_url)?;
        let contract = MockProtocolVersions::deploy(provider)
            .await
            .wrap_err("Failed to deploy MockProtocolVersions to L1")?;

        let client = Self {
            l1_rpc_url,
            address: *contract.address(),
            baseline: UpgradeSignalStackOptions::baseline_schedule(rollup_config),
        };
        client.set_minimum_protocol_version(options.minimum_protocol_version).await?;
        client.set_schedule(&options.schedule).await?;

        Ok(client)
    }

    /// Writes the baseline schedule overlaid with the given `(upgrade, activation timestamp)`
    /// entries to the contract.
    ///
    /// Nodes always apply the full contract schedule, so every write preserves the baseline;
    /// `entries` override it per upgrade. Upgrades in neither list are written as `0` (not
    /// scheduled).
    pub async fn set_schedule(&self, entries: &[(BaseUpgrade, u64)]) -> Result<()> {
        let merged: Vec<_> = self.baseline.iter().chain(entries).copied().collect();
        let provider = Self::wallet_provider(&self.l1_rpc_url)?;
        let contract = MockProtocolVersions::new(self.address, provider);
        contract
            .setSchedule(Self::id_ordered_schedule(&merged)?)
            .send()
            .await
            .wrap_err("Failed to send setSchedule")?
            .get_receipt()
            .await
            .wrap_err("setSchedule was not mined")?;
        Ok(())
    }

    /// Sets the contract's minimum protocol version (packed semver).
    pub async fn set_minimum_protocol_version(&self, version: U256) -> Result<()> {
        let provider = Self::wallet_provider(&self.l1_rpc_url)?;
        let contract = MockProtocolVersions::new(self.address, provider);
        contract
            .setMinimumProtocolVersion(version)
            .send()
            .await
            .wrap_err("Failed to send setMinimumProtocolVersion")?
            .get_receipt()
            .await
            .wrap_err("setMinimumProtocolVersion was not mined")?;
        Ok(())
    }

    /// Expands `(upgrade, timestamp)` entries into the full id-ordered schedule array.
    ///
    /// Later entries for the same upgrade overwrite earlier ones; unlisted upgrades are `0`.
    pub fn id_ordered_schedule(entries: &[(BaseUpgrade, u64)]) -> Result<Vec<u64>> {
        let mut schedule = vec![0u64; BaseUpgrade::CONTRACT_VARIANTS.len()];
        for (upgrade, timestamp) in entries {
            let position = BaseUpgrade::CONTRACT_VARIANTS
                .iter()
                .position(|variant| variant == upgrade)
                .ok_or_else(|| {
                    eyre::eyre!("upgrade is not contract-backed: {}", upgrade.contract_id())
                })?;
            schedule[position] = *timestamp;
        }
        Ok(schedule)
    }

    /// Builds a wallet-backed L1 provider for contract transactions.
    pub fn wallet_provider(l1_rpc_url: &Url) -> Result<impl Provider + Clone + use<>> {
        let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_4.private_key)
            .wrap_err("Failed to parse upgrade signal admin key")?;
        Ok(ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_http(l1_rpc_url.clone()))
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::{ChainGenesis, UpgradeActivation};

    use super::*;

    #[test]
    fn id_ordered_schedule_places_timestamps_by_contract_position() {
        let schedule = MockProtocolVersionsClient::id_ordered_schedule(&[
            (BaseUpgrade::Cobalt, 42),
            (BaseUpgrade::Regolith, 7),
        ])
        .unwrap();

        assert_eq!(schedule.len(), BaseUpgrade::CONTRACT_VARIANTS.len());
        assert_eq!(schedule[0], 7);
        assert_eq!(*schedule.last().unwrap(), 42);
        assert!(schedule[1..schedule.len() - 1].iter().all(|&ts| ts == 0));
    }

    #[test]
    fn id_ordered_schedule_later_entries_override_earlier() {
        let schedule = MockProtocolVersionsClient::id_ordered_schedule(&[
            (BaseUpgrade::Cobalt, 42),
            (BaseUpgrade::Cobalt, 84),
        ])
        .unwrap();

        assert_eq!(*schedule.last().unwrap(), 84);
    }

    #[test]
    fn baseline_schedule_keeps_scheduled_upgrades_and_maps_genesis_activations() {
        let mut rollup_config = RollupConfig {
            genesis: ChainGenesis { l2_time: 1_000, ..Default::default() },
            ..Default::default()
        };
        rollup_config
            .apply_upgrade_activation(BaseUpgrade::Regolith, UpgradeActivation::Timestamp(0));
        rollup_config
            .apply_upgrade_activation(BaseUpgrade::Azul, UpgradeActivation::Timestamp(2_000));

        let baseline = UpgradeSignalStackOptions::baseline_schedule(&rollup_config);

        assert!(baseline.contains(&(BaseUpgrade::Regolith, 1_000)));
        assert!(baseline.contains(&(BaseUpgrade::Azul, 2_000)));
        assert!(!baseline.iter().any(|(upgrade, _)| *upgrade == BaseUpgrade::Cobalt));
    }
}
