//! L1 upgrade signal contract reader.

use core::time::Duration;

use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, TransactionInput, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use base_common_genesis::BaseUpgrade;
use futures::future::{join_all, try_join};
use tokio::time::sleep;
use tracing::warn;

use crate::{
    UpgradeSignal, UpgradeSignalError, UpgradeSignalMetricLayer, UpgradeSignalMetrics,
    UpgradeSignalSchedule,
};

sol! {
    /// L1 upgrade signal interface.
    ///
    /// The address can be a proxy. Nodes only depend on this read interface.
    interface IUpgradeSignal {
        /// Emitted when an activation timestamp is set for an upgrade ID.
        event TimestampSet(string indexed upgradeId, uint256 timestamp);

        /// Emitted when a protocol version is set for an upgrade ID.
        event ProtocolVersionSet(string indexed upgradeId, uint256 protocolVersion);

        /// Returns the activation timestamp for `upgradeId`.
        function getTimestamp(string upgradeId) external view returns (uint256);

        /// Returns the minimum node protocol version for `upgradeId`.
        function getProtocolVersion(string upgradeId) external view returns (uint256);
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
    /// Pinning reads to a concrete block hash ensures every per-upgrade call in a schedule observes
    /// the same L1 state. The block tag (finalized by default) keeps the schedule reorg-stable.
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

    /// Converts an ABI uint256 timestamp into the node's `u64` timestamp representation.
    pub fn decode_timestamp(value: U256) -> Result<u64, UpgradeSignalError> {
        u64::try_from(value).map_err(|_| UpgradeSignalError::timestamp_overflow(value))
    }

    /// Reads one upgrade signal using a previously observed L1 block ID.
    pub async fn read_signal_at_l1_block(
        &self,
        upgrade_id: BaseUpgrade,
        l1_block_number: u64,
        l1_block: BlockId,
    ) -> Result<UpgradeSignal, UpgradeSignalError> {
        let (timestamp_output, version_output) = try_join(
            self.call_at_block(
                IUpgradeSignal::getTimestampCall {
                    upgradeId: upgrade_id.contract_id().to_string(),
                },
                l1_block,
                "getTimestamp failed",
            ),
            self.call_at_block(
                IUpgradeSignal::getProtocolVersionCall {
                    upgradeId: upgrade_id.contract_id().to_string(),
                },
                l1_block,
                "getProtocolVersion failed",
            ),
        )
        .await?;
        let timestamp =
            IUpgradeSignal::getTimestampCall::abi_decode_returns(timestamp_output.as_ref())
                .map_err(|error| UpgradeSignalError::decode("getTimestamp decode failed", error))?;
        let activation_timestamp = Self::decode_timestamp(timestamp)?;

        let protocol_version =
            IUpgradeSignal::getProtocolVersionCall::abi_decode_returns(version_output.as_ref())
                .map_err(|error| {
                    UpgradeSignalError::decode("getProtocolVersion decode failed", error)
                })?;

        Ok(UpgradeSignal { upgrade_id, activation_timestamp, protocol_version, l1_block_number })
    }

    /// Reads the upgrade signal for `upgrade_id`.
    pub async fn read_signal(
        &self,
        upgrade_id: BaseUpgrade,
    ) -> Result<UpgradeSignal, UpgradeSignalError> {
        let (l1_block_number, l1_block) = self.pinned_l1_block_id().await?;
        self.read_signal_at_l1_block(upgrade_id, l1_block_number, l1_block).await
    }

    /// Reads the upgrade signal schedule for `upgrade_ids`.
    ///
    /// Records `l1_read_errors_total` on failure: all upgrade IDs if the L1 block fetch fails,
    /// only the failing upgrade ID if a per-upgrade contract call fails.
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
        let mut signals = Vec::with_capacity(upgrade_ids.len());
        let mut first_error = None;

        for (upgrade_id, result) in
            join_all(upgrade_ids.iter().copied().map(|upgrade_id| async move {
                (
                    upgrade_id,
                    self.read_signal_at_l1_block(upgrade_id, l1_block_number, l1_block).await,
                )
            }))
            .await
        {
            match result {
                Ok(signal) => signals.push(signal),
                Err(error) => {
                    UpgradeSignalMetrics::record_l1_read_error_for_layers(
                        metrics_layers,
                        upgrade_id,
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if let Some(error) = first_error {
            return Err(error);
        }

        Ok(UpgradeSignalSchedule::new(signals))
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

    /// Reads the schedule, tolerating per-upgrade failures.
    ///
    /// Records `l1_read_errors_total` for each upgrade that fails and returns the signals that were
    /// read successfully. Intended for the live metrics poller, which must not abort the whole
    /// schedule (or the node) because a single upgrade read failed.
    pub async fn read_schedule_tolerant(
        &self,
        upgrade_ids: &[BaseUpgrade],
        metrics_layers: &[UpgradeSignalMetricLayer],
    ) -> UpgradeSignalSchedule {
        let (l1_block_number, l1_block) = match self.pinned_l1_block_id().await {
            Ok(block) => block,
            Err(error) => {
                UpgradeSignalMetrics::record_l1_read_errors_for_layers(metrics_layers, upgrade_ids);
                warn!(
                    target: "upgrade_signal",
                    error = %error,
                    "failed to fetch L1 block for upgrade signal poll"
                );
                return UpgradeSignalSchedule::default();
            }
        };
        let mut signals = Vec::with_capacity(upgrade_ids.len());
        for (upgrade_id, result) in
            join_all(upgrade_ids.iter().copied().map(|upgrade_id| async move {
                (
                    upgrade_id,
                    self.read_signal_at_l1_block(upgrade_id, l1_block_number, l1_block).await,
                )
            }))
            .await
        {
            match result {
                Ok(signal) => signals.push(signal),
                Err(error) => {
                    UpgradeSignalMetrics::record_l1_read_error_for_layers(
                        metrics_layers,
                        upgrade_id,
                    );
                    warn!(
                        target: "upgrade_signal",
                        upgrade_id = %upgrade_id.contract_id(),
                        error = %error,
                        "failed to read live L1 upgrade signal for upgrade"
                    );
                }
            }
        }
        UpgradeSignalSchedule::new(signals)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;

    #[test]
    fn decodes_u64_timestamp() {
        assert_eq!(AlloyUpgradeSignalReader::decode_timestamp(U256::from(42)).unwrap(), 42);
    }

    #[test]
    fn rejects_timestamp_overflow() {
        let value = U256::from(u64::MAX) + U256::from(1);

        assert!(matches!(
            AlloyUpgradeSignalReader::decode_timestamp(value).unwrap_err(),
            UpgradeSignalError::TimestampOverflow(actual) if actual == value
        ));
    }
}
