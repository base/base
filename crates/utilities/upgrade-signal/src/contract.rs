//! L1 upgrade signal contract reader.

use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;

use crate::{UpgradeSignal, UpgradeSignalError, UpgradeSignalSchedule};

sol! {
    /// L1 upgrade signal interface.
    ///
    /// The address can be a proxy. Nodes only depend on this read interface.
    interface IUpgradeSignal {
        /// Emitted when an activation timestamp is set for a hardfork ID.
        event TimestampSet(string indexed hardforkId, uint256 timestamp);

        /// Emitted when a protocol version is set for a hardfork ID.
        event ProtocolVersionSet(string indexed hardforkId, uint256 protocolVersion);

        /// Returns the activation timestamp for `hardforkId`.
        function getTimestamp(string hardforkId) external view returns (uint256);

        /// Returns the expected protocol version for `hardforkId`.
        function getProtocolVersion(string hardforkId) external view returns (uint256);
    }
}

/// Reads upgrade signals from an L1 contract with Alloy.
#[derive(Debug, Clone)]
pub struct AlloyUpgradeSignalReader {
    /// L1 provider.
    pub provider: RootProvider,
    /// L1 contract or proxy address.
    pub contract_address: Address,
}

impl AlloyUpgradeSignalReader {
    /// Creates a new Alloy-backed upgrade signal reader.
    pub const fn new(provider: RootProvider, contract_address: Address) -> Self {
        Self { provider, contract_address }
    }

    /// Executes an `eth_call` against the upgrade signal contract.
    pub async fn call<C>(&self, call: C, context: &'static str) -> Result<Bytes, UpgradeSignalError>
    where
        C: SolCall,
    {
        let request = TransactionRequest::default()
            .to(self.contract_address)
            .input(TransactionInput::new(Bytes::from(call.abi_encode())));

        self.provider
            .call(request)
            .await
            .map_err(|error| UpgradeSignalError::provider(context, error))
    }

    /// Converts an ABI uint256 timestamp into the node's `u64` timestamp representation.
    pub fn decode_timestamp(value: U256) -> Result<u64, UpgradeSignalError> {
        u64::try_from(value).map_err(|_| UpgradeSignalError::timestamp_overflow(value))
    }

    /// Reads one hardfork signal using a previously observed L1 block number.
    pub async fn read_signal_at_l1_block(
        &self,
        hardfork_id: &str,
        l1_block_number: u64,
    ) -> Result<UpgradeSignal, UpgradeSignalError> {
        let timestamp_output = self
            .call(
                IUpgradeSignal::getTimestampCall { hardforkId: hardfork_id.to_string() },
                "getTimestamp failed",
            )
            .await?;
        let timestamp =
            IUpgradeSignal::getTimestampCall::abi_decode_returns(timestamp_output.as_ref())
                .map_err(|error| UpgradeSignalError::decode("getTimestamp decode failed", error))?;
        let activation_timestamp = Self::decode_timestamp(timestamp)?;

        let version_output = self
            .call(
                IUpgradeSignal::getProtocolVersionCall { hardforkId: hardfork_id.to_string() },
                "getProtocolVersion failed",
            )
            .await?;
        let protocol_version =
            IUpgradeSignal::getProtocolVersionCall::abi_decode_returns(version_output.as_ref())
                .map_err(|error| {
                    UpgradeSignalError::decode("getProtocolVersion decode failed", error)
                })?;

        Ok(UpgradeSignal {
            hardfork_id: hardfork_id.to_string(),
            activation_timestamp,
            protocol_version,
            l1_block_number,
        })
    }
}

/// Interface for reading upgrade signal state from L1.
#[async_trait]
pub trait UpgradeSignalReader: Send + Sync {
    /// Reads the upgrade signal for `hardfork_id`.
    async fn read_signal(&self, hardfork_id: &str) -> Result<UpgradeSignal, UpgradeSignalError>;

    /// Reads the upgrade signal schedule for `hardfork_ids`.
    async fn read_schedule(
        &self,
        hardfork_ids: &[String],
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let mut signals = Vec::with_capacity(hardfork_ids.len());
        for hardfork_id in hardfork_ids {
            signals.push(self.read_signal(hardfork_id).await?);
        }
        Ok(UpgradeSignalSchedule::new(signals))
    }
}

#[async_trait]
impl UpgradeSignalReader for AlloyUpgradeSignalReader {
    async fn read_signal(&self, hardfork_id: &str) -> Result<UpgradeSignal, UpgradeSignalError> {
        let l1_block_number =
            self.provider.get_block_number().await.map_err(|error| {
                UpgradeSignalError::provider("get L1 block number failed", error)
            })?;
        self.read_signal_at_l1_block(hardfork_id, l1_block_number).await
    }

    async fn read_schedule(
        &self,
        hardfork_ids: &[String],
    ) -> Result<UpgradeSignalSchedule, UpgradeSignalError> {
        let l1_block_number =
            self.provider.get_block_number().await.map_err(|error| {
                UpgradeSignalError::provider("get L1 block number failed", error)
            })?;
        let mut signals = Vec::with_capacity(hardfork_ids.len());
        for hardfork_id in hardfork_ids {
            signals.push(self.read_signal_at_l1_block(hardfork_id, l1_block_number).await?);
        }
        Ok(UpgradeSignalSchedule::new(signals))
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
            UpgradeSignalError::TimestampOverflow(_)
        ));
    }
}
