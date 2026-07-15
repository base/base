//! L1 upgrade signal contract reader.

use core::time::Duration;

use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, TransactionInput, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use base_common_genesis::BaseUpgrade;
use futures::future::try_join;
use tokio::time::sleep;
use tracing::warn;

use crate::{
    UpgradeSignal, UpgradeSignalError, UpgradeSignalMetricLayer, UpgradeSignalMetrics,
    UpgradeSignalSchedule,
};

sol! {
    /// L1 `ProtocolVersions` upgrade schedule interface.
    ///
    /// The address can be a proxy. Nodes only depend on this read interface.
    interface IProtocolVersions {
        /// Returns the activation timestamp for every registered upgrade, ordered by ascending
        /// upgrade id (`0` = not scheduled).
        function getSchedule() external view returns (uint64[] memory);

        /// Returns the minimum protocol version clients must run (packed semver).
        function minimumProtocolVersion() external view returns (uint256);
    }
}

/// Reads upgrade signals from an L1 contract with Alloy.
#[derive(Debug, Clone)]
pub struct AlloyUpgradeSignalReader {
    /// L1 provider.
    pub provider: RootProvider,
    /// L1 contract or proxy address.
    pub contract_address: Address,
    /// L1 block tag used to pin reads. Defaults to [`BlockNumberOrTag::Finalized`].
    pub block_tag: BlockNumberOrTag,
}

impl AlloyUpgradeSignalReader {
    /// Creates a new Alloy-backed upgrade signal reader that reads at the finalized L1 head.
    pub const fn new(provider: RootProvider, contract_address: Address) -> Self {
        Self { provider, contract_address, block_tag: BlockNumberOrTag::Finalized }
    }

    /// Sets the L1 block tag used to pin reads.
    pub const fn with_block_tag(mut self, block_tag: BlockNumberOrTag) -> Self {
        self.block_tag = block_tag;
        self
    }

    /// Executes an `eth_call` against the upgrade signal contract at a specific L1 block.
    pub async fn call_at_block<C>(
        &self,
        call: C,
        block: BlockId,
        context: &'static str,
    ) -> Result<Bytes, UpgradeSignalError>
    where
        C: SolCall,
    {
        let request = TransactionRequest::default()
            .to(self.contract_address)
            .input(TransactionInput::new(Bytes::from(call.abi_encode())));

        self.provider
            .call(request)
            .block(block)
            .await
            .map_err(|error| UpgradeSignalError::provider(context, error))
    }

    /// Returns the L1 block number and concrete block ID for the configured block tag.
    ///
    /// Pinning reads to a concrete block hash ensures every contract call in a schedule read
    /// observes the same L1 state. The block tag (finalized by default) keeps the schedule
    /// reorg-stable.
    pub async fn pinned_l1_block_id(&self) -> Result<(u64, BlockId), UpgradeSignalError> {
        let block = self
            .provider
            .get_block_by_number(self.block_tag)
            .await
            .map_err(|error| UpgradeSignalError::provider("get L1 block failed", error))?
            .ok_or_else(|| {
                UpgradeSignalError::provider("get L1 block failed", "missing block for tag")
            })?;

        Ok((block.header.number, BlockId::hash(block.header.hash)))
    }

    /// Reads the contract's id-ordered activation timestamps and the global minimum protocol
    /// version using a previously observed L1 block ID.
    pub async fn read_contract_schedule_at_l1_block(
        &self,
        l1_block: BlockId,
    ) -> Result<(Vec<u64>, U256), UpgradeSignalError> {
        let (schedule_output, version_output) = try_join(
            self.call_at_block(
                IProtocolVersions::getScheduleCall {},
                l1_block,
                "getSchedule failed",
            ),
            self.call_at_block(
                IProtocolVersions::minimumProtocolVersionCall {},
                l1_block,
                "minimumProtocolVersion failed",
            ),
        )
        .await?;

        let timestamps =
            IProtocolVersions::getScheduleCall::abi_decode_returns(schedule_output.as_ref())
                .map_err(|error| UpgradeSignalError::decode("getSchedule decode failed", error))?;

        let minimum_protocol_version =
            IProtocolVersions::minimumProtocolVersionCall::abi_decode_returns(
                version_output.as_ref(),
            )
            .map_err(|error| {
                UpgradeSignalError::decode("minimumProtocolVersion decode failed", error)
            })?;

        Ok((timestamps, minimum_protocol_version))
    }

    /// Maps the contract's id-ordered activation timestamps onto the node's hardfork ladder.
    ///
    /// The contract keys upgrades by ascending numeric registration id and keeps names offchain,
    /// so entries are aligned with [`BaseUpgrade::CONTRACT_VARIANTS`] by registration id: id `0`
    /// maps to the oldest contract-backed hardfork, and each following id maps to the next
    /// hardfork in the ladder. This is a positional mapping by id, not a sort by timestamp, so the
    /// timestamps need not be monotonic. Contract entries beyond the ladder
    /// belong to upgrades newer than this binary knows and are logged and ignored, hardforks
    /// without a contract entry produce no signal, and only upgrades in `upgrade_ids` produce
    /// signals. Every signal carries the contract's global minimum protocol version.
    pub fn map_schedule(
        timestamps: &[u64],
        minimum_protocol_version: U256,
        l1_block_number: u64,
        upgrade_ids: &[BaseUpgrade],
    ) -> UpgradeSignalSchedule {
        if timestamps.len() > BaseUpgrade::CONTRACT_VARIANTS.len() {
            warn!(
                target: "upgrade_signal",
                contract_upgrades = timestamps.len(),
                known_upgrades = BaseUpgrade::CONTRACT_VARIANTS.len(),
                "L1 schedule has more upgrades than this binary knows; newest entries ignored"
            );
        }

        let signals: Vec<_> = BaseUpgrade::CONTRACT_VARIANTS
            .iter()
            .zip(timestamps.iter())
            .filter(|(upgrade_id, _)| upgrade_ids.contains(upgrade_id))
            .map(|(upgrade_id, activation_timestamp)| UpgradeSignal {
                upgrade_id: *upgrade_id,
                activation_timestamp: *activation_timestamp,
                protocol_version: minimum_protocol_version,
                l1_block_number,
            })
            .collect();

        UpgradeSignalSchedule::new(signals)
    }

    /// Reads the upgrade signal schedule for `upgrade_ids`.
    ///
    /// Records `l1_read_errors_total` for all upgrade IDs when the L1 block fetch or the schedule
    /// read fails; the whole schedule is read with one `getSchedule` call, so per-upgrade failures
    /// no longer exist.
    pub async fn read_schedule(
        &self,
        upgrade_ids: &[BaseUpgrade],
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let (l1_block_number, l1_block) = match self.pinned_l1_block_id().await {
            Ok(block) => block,
            Err(error) => {
                UpgradeSignalMetrics::record_l1_read_errors_for_layers(metrics_layers, upgrade_ids);
                return Err(error);
            }
        };

        let (timestamps, minimum_protocol_version) = match self
            .read_contract_schedule_at_l1_block(l1_block)
            .await
        {
            Ok(values) => values,
            Err(error) => {
                UpgradeSignalMetrics::record_l1_read_errors_for_layers(metrics_layers, upgrade_ids);
                return Err(error);
            }
        };

        Ok(Self::map_schedule(&timestamps, minimum_protocol_version, l1_block_number, upgrade_ids))
    }

    /// Reads the schedule, retrying transient failures with a fixed backoff before giving up.
    ///
    /// Used on the startup path, where a single transient L1 error should not abort node launch
    /// outright; after `max_attempts` failures the last error is returned (fail-fast).
    pub async fn read_schedule_with_retries(
        &self,
        upgrade_ids: &[BaseUpgrade],
        max_attempts: u32,
        backoff: Duration,
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let max_attempts = max_attempts.max(1);
        let mut attempt = 1;
        loop {
            match self.read_schedule(upgrade_ids, metrics_layers).await {
                Ok(schedule) => return Ok(schedule),
                Err(error) if attempt >= max_attempts => return Err(error),
                Err(error) => {
                    warn!(
                        target: "upgrade_signal",
                        attempt,
                        max_attempts,
                        error = %error,
                        "retrying L1 upgrade signal read"
                    );
                    sleep(backoff).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Reads the schedule, tolerating read failures.
    ///
    /// Records `l1_read_errors_total` and returns an empty schedule when the read fails. Intended
    /// for the live metrics poller, which must not abort the node because a schedule read failed.
    pub async fn read_schedule_tolerant(
        &self,
        upgrade_ids: &[BaseUpgrade],
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> UpgradeSignalSchedule {
        match self.read_schedule(upgrade_ids, metrics_layers).await {
            Ok(schedule) => schedule,
            Err(error) => {
                warn!(
                    target: "upgrade_signal",
                    error = %error,
                    "failed to read live L1 upgrade signal schedule"
                );
                UpgradeSignalSchedule::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(schedule: &UpgradeSignalSchedule) -> Vec<(BaseUpgrade, u64)> {
        schedule
            .signals
            .iter()
            .map(|signal| (signal.upgrade_id, signal.activation_timestamp))
            .collect()
    }

    #[test]
    fn maps_partial_schedule_to_oldest_hardforks() {
        let schedule = AlloyUpgradeSignalReader::map_schedule(
            &[10, 20, 0],
            U256::from(7),
            99,
            &BaseUpgrade::CONTRACT_VARIANTS,
        );

        assert_eq!(
            signals(&schedule),
            vec![(BaseUpgrade::Regolith, 10), (BaseUpgrade::Canyon, 20), (BaseUpgrade::Delta, 0)]
        );
        assert!(
            schedule
                .signals
                .iter()
                .all(|signal| signal.protocol_version == U256::from(7)
                    && signal.l1_block_number == 99)
        );
    }

    #[test]
    fn maps_full_schedule_in_ladder_order() {
        let timestamps: Vec<u64> = (1..=BaseUpgrade::CONTRACT_VARIANTS.len() as u64).collect();

        let schedule = AlloyUpgradeSignalReader::map_schedule(
            &timestamps,
            U256::from(7),
            1,
            &BaseUpgrade::CONTRACT_VARIANTS,
        );

        assert_eq!(
            signals(&schedule),
            BaseUpgrade::CONTRACT_VARIANTS.iter().copied().zip(timestamps).collect::<Vec<_>>()
        );
    }

    #[test]
    fn filters_unconfigured_upgrades() {
        let schedule = AlloyUpgradeSignalReader::map_schedule(
            &[10, 20, 30],
            U256::from(7),
            1,
            &[BaseUpgrade::Canyon],
        );

        assert_eq!(signals(&schedule), vec![(BaseUpgrade::Canyon, 20)]);
    }

    #[test]
    fn ignores_entries_newer_than_known_ladder() {
        let mut timestamps: Vec<u64> = (1..=BaseUpgrade::CONTRACT_VARIANTS.len() as u64).collect();
        timestamps.push(777);

        let schedule = AlloyUpgradeSignalReader::map_schedule(
            &timestamps,
            U256::from(7),
            1,
            &BaseUpgrade::CONTRACT_VARIANTS,
        );

        assert_eq!(schedule.signals.len(), BaseUpgrade::CONTRACT_VARIANTS.len());
        assert_eq!(signals(&schedule).first().copied(), Some((BaseUpgrade::Regolith, 1)));
        assert!(!signals(&schedule).iter().any(|(_, timestamp)| *timestamp == 777));
    }

    #[test]
    fn produces_no_signal_for_hardforks_without_contract_entries() {
        let schedule = AlloyUpgradeSignalReader::map_schedule(
            &[42],
            U256::from(7),
            1,
            &BaseUpgrade::CONTRACT_VARIANTS,
        );

        assert_eq!(signals(&schedule), vec![(BaseUpgrade::Regolith, 42)]);
    }
}
