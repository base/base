//! Types for block metering responses.

use alloy_primitives::{B256, U256};
use base_bundles::MeterBundleResponse;
use serde::{Deserialize, Serialize};

/// Response for block metering RPC calls.
/// Contains the block hash plus timing information for EVM execution and state root calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeterBlockResponse {
    /// The block hash that was metered
    pub block_hash: B256,
    /// The block number that was metered
    pub block_number: u64,
    /// Duration of signer recovery in microseconds (can be parallelized)
    pub signer_recovery_time_us: u128,
    /// Duration of EVM execution in microseconds
    pub execution_time_us: u128,
    /// Duration of state root calculation in microseconds.
    ///
    /// Note: This timing is most accurate for recent blocks where state tries are cached.
    /// For older blocks, trie nodes may not be cached, which can significantly inflate this value.
    pub state_root_time_us: u128,
    /// Total duration (signer recovery + EVM execution + state root calculation) in microseconds
    pub total_time_us: u128,
    /// Per-transaction metering data
    pub transactions: Vec<MeterBlockTransactions>,
}

/// Metering data for a single transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeterBlockTransactions {
    /// Transaction hash
    pub tx_hash: B256,
    /// Gas used by this transaction
    pub gas_used: u64,
    /// Execution time in microseconds
    pub execution_time_us: u128,
}

// --- Metered priority fee types ---

/// Human-friendly representation of a resource fee quote.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
