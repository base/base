//! Types for the Base RPC extensions.

use alloy_primitives::U256;
use alloy_rpc_types_eth::pubsub::SubscriptionKind;
use serde::{Deserialize, Serialize};
use tips_core::types::MeterBundleResponse;

/// The status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub enum Status {
    /// Transaction is not known to the node.
    Unknown,
    /// Transaction is known to the node (in mempool or confirmed).
    Known,
}

/// Response containing the status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct TransactionStatusResponse {
    /// The status of the queried transaction.
    pub status: Status,
}

/// Extended subscription kind that includes both standard Ethereum subscription types
/// and flashblocks-specific types.
///
/// This enum encapsulates the standard [`SubscriptionKind`] from alloy and adds flashblocks
/// support, allowing `eth_subscribe` to handle both standard subscriptions (newHeads, logs, etc.)
/// and custom flashblocks subscriptions.
///
/// By encapsulating [`SubscriptionKind`] rather than redefining its variants, we automatically
/// inherit support for any new variants added upstream, or get a compile error if the signature
/// changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtendedSubscriptionKind {
    /// Standard Ethereum subscription types (newHeads, logs, newPendingTransactions, syncing).
    ///
    /// These are proxied to reth's underlying `EthPubSub` implementation.
    Standard(SubscriptionKind),
    /// Base-specific subscription types for flashblocks.
    Base(BaseSubscriptionKind),
}

/// Base-specific subscription types for flashblocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BaseSubscriptionKind {
    /// New flashblocks subscription.
    ///
    /// Fires a notification each time a new flashblock is processed, providing the current
    /// pending block state. Each flashblock represents an incremental update to the pending
    /// block, so multiple notifications may be emitted for the same block height as new
    /// flashblocks arrive.
    NewFlashblocks,
    /// Pending logs subscription.
    ///
    /// Returns logs from flashblocks pending state that match the given filter criteria.
    /// Unlike standard `logs` subscription which only includes logs from confirmed blocks,
    /// this includes logs from the current pending flashblock state.
    PendingLogs,
    /// New flashblock transactions subscription.
    ///
    /// Returns transactions from flashblocks as they are sequenced, providing higher inclusion
    /// confidence than standard `newPendingTransactions` which returns mempool transactions.
    /// Flashblock transactions have been included by the sequencer and are effectively preconfirmed.
    ///
    /// Accepts an optional boolean parameter:
    /// - `true`: Returns full transaction objects
    /// - `false` (default): Returns only transaction hashes
    NewFlashblockTransactions,
}

impl ExtendedSubscriptionKind {
    /// Returns the standard subscription kind if this is a standard subscription type.
    pub const fn as_standard(&self) -> Option<SubscriptionKind> {
        match self {
            Self::Standard(kind) => Some(*kind),
            Self::Base(_) => None,
        }
    }

    /// Returns true if this is a flashblocks-specific subscription.
    pub const fn is_flashblocks(&self) -> bool {
        matches!(self, Self::Base(_))
    }
}

impl From<SubscriptionKind> for ExtendedSubscriptionKind {
    fn from(kind: SubscriptionKind) -> Self {
        Self::Standard(kind)
    }
}

impl From<BaseSubscriptionKind> for ExtendedSubscriptionKind {
    fn from(kind: BaseSubscriptionKind) -> Self {
        Self::Base(kind)
    }
}

// --- Metered priority fee types ---

/// Human-friendly representation of a resource fee quote.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceFeeEstimateResponse {
    /// Resource name (gasUsed, executionTime, etc).
    pub resource: String,
    /// Minimum fee to displace enough capacity.
    pub threshold_priority_fee: U256,
    /// Recommended fee with safety margin.
    pub recommended_priority_fee: U256,
    /// Cumulative resource usage above threshold.
    pub cumulative_usage: U256,
    /// Number of transactions above threshold.
    pub threshold_tx_count: u64,
    /// Total transactions considered.
    pub total_transactions: u64,
}

/// Response payload for `base_meteredPriorityFeePerGas`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteredPriorityFeeResponse {
    /// Bundled metering results.
    #[serde(flatten)]
    pub meter_bundle: MeterBundleResponse,
    /// Recommended priority fee (max across all resources and median across recent blocks).
    pub priority_fee: U256,
    /// Number of recent blocks used to compute the rolling estimate.
    pub blocks_sampled: u64,
    /// Per-resource estimates (median across sampled blocks).
    pub resource_estimates: Vec<ResourceFeeEstimateResponse>,
}
