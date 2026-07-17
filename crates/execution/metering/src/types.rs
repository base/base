//! Types for block metering responses.

use alloy_primitives::{B256, U256};
use base_bundles::MeterBundleResponse;
use serde::{Deserialize, Serialize};

/// Response for block metering RPC calls.
/// Contains the block hash plus timing information for signer recovery and EVM execution.
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
    /// Deprecated state-root calculation duration in microseconds.
    ///
    /// State-root simulation was removed from this profiling path. The field is retained for
    /// response compatibility and is always serialized as zero by this version.
    #[serde(default)]
    pub state_root_time_us: u128,
    /// Total duration (signer recovery + EVM execution) in microseconds
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

/// Human-friendly representation of a resource fee estimate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceFeeEstimateResponse {
    /// Resource name (gasUsed or dataAvailability).
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
    /// Recommended priority fee (max across all resources).
    pub priority_fee: U256,
    /// Number of recent blocks used to compute the estimate.
    pub blocks_sampled: u64,
    /// Per-resource estimates.
    pub resource_estimates: Vec<ResourceFeeEstimateResponse>,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::{MeterBlockResponse, MeterBlockTransactions};

    #[test]
    fn meter_block_response_serializes_deprecated_state_root_time_as_zero() {
        let response = MeterBlockResponse {
            block_hash: B256::ZERO,
            block_number: 1,
            signer_recovery_time_us: 2,
            execution_time_us: 3,
            state_root_time_us: 0,
            total_time_us: 5,
            transactions: vec![MeterBlockTransactions {
                tx_hash: B256::ZERO,
                gas_used: 21_000,
                execution_time_us: 3,
            }],
        };

        let json = serde_json::to_string(&response).unwrap();

        assert!(json.contains("\"stateRootTimeUs\":0"));
    }
}
