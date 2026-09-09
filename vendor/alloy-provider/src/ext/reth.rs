//! Reth-specific provider extensions.
use alloy_primitives::{Address, U64, U256, map::HashMap};
use alloy_transport::TransportResult;
use base_common_network::Network;
use base_common_rpc_types::BlockId;

#[cfg(feature = "pubsub")]
use crate::GetSubscription;
use crate::Provider;

/// Reth API namespace for reth-specific methods
#[cfg_attr(target_family = "wasm", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait::async_trait)]
pub trait RethProviderExt<N: Network>: Send + Sync {
    /// Returns all ETH balance changes in a block
    async fn reth_get_balance_changes_in_block(
        &self,
        block_id: BlockId,
    ) -> TransportResult<HashMap<Address, U256>>;

    /// Re-executes a block (or a range of blocks) and returns the execution outcome including
    /// receipts, state changes, and EIP-7685 requests.
    ///
    /// If `count` is provided, re-executes `count` consecutive blocks starting from `block_id`
    /// and returns the merged execution outcome.
    async fn reth_get_block_execution_outcome(
        &self,
        block_id: BlockId,
        count: Option<U64>,
    ) -> TransportResult<Option<serde_json::Value>>;

    /// Subscribe to json `ChainNotifications`
    #[cfg(feature = "pubsub")]
    async fn reth_subscribe_chain_notifications(
        &self,
    ) -> GetSubscription<alloy_rpc_client::NoParams, serde_json::Value>;

    /// Subscribe to persisted block notifications.
    ///
    /// Emits a notification with the block number and hash when a new block is persisted to disk.
    #[cfg(feature = "pubsub")]
    async fn reth_subscribe_persisted_block(
        &self,
    ) -> GetSubscription<alloy_rpc_client::NoParams, serde_json::Value>;
}

#[cfg_attr(target_family = "wasm", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait::async_trait)]
impl<N, P> RethProviderExt<N> for P
where
    N: Network,
    P: Provider<N>,
{
    async fn reth_get_balance_changes_in_block(
        &self,
        block_id: BlockId,
    ) -> TransportResult<HashMap<Address, U256>> {
        self.client().request("reth_getBalanceChangesInBlock", (block_id,)).await
    }

    async fn reth_get_block_execution_outcome(
        &self,
        block_id: BlockId,
        count: Option<U64>,
    ) -> TransportResult<Option<serde_json::Value>> {
        self.client().request("reth_getBlockExecutionOutcome", (block_id, count)).await
    }

    #[cfg(feature = "pubsub")]
    async fn reth_subscribe_chain_notifications(
        &self,
    ) -> GetSubscription<alloy_rpc_client::NoParams, serde_json::Value> {
        self.subscribe_to("reth_subscribeChainNotifications")
    }

    #[cfg(feature = "pubsub")]
    async fn reth_subscribe_persisted_block(
        &self,
    ) -> GetSubscription<alloy_rpc_client::NoParams, serde_json::Value> {
        self.subscribe_to("reth_subscribePersistedBlock")
    }
}
