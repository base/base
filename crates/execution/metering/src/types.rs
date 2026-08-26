//! Types for block metering responses.

use std::collections::BTreeSet;

use alloy_primitives::{Address, B256, U256};
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
    /// Parent-state provider reads performed during block execution.
    #[serde(default)]
    pub state_provider: MeterStateProviderStats,
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
    /// Parent-state provider reads first encountered while executing this transaction.
    #[serde(default)]
    pub state_provider: MeterStateProviderStats,
    /// Parent-state account and storage fetches grouped by address.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state_provider_accounts: Vec<MeterStateProviderAccountAccess>,
    /// Parent-state bytecode fetches grouped by code hash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state_provider_code_hashes: Vec<MeterStateProviderCodeAccess>,
}

/// Parent-state provider reads and their cumulative latency.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeterStateProviderStats {
    /// Number of account fetches.
    pub account_fetches: u64,
    /// Time spent fetching accounts in microseconds.
    pub account_fetch_time_us: u128,
    /// Number of storage fetches.
    pub storage_fetches: u64,
    /// Time spent fetching storage in microseconds.
    pub storage_fetch_time_us: u128,
    /// Number of bytecode fetches.
    pub code_fetches: u64,
    /// Time spent fetching bytecode in microseconds.
    pub code_fetch_time_us: u128,
    /// Total bytecode bytes fetched.
    pub code_fetched_bytes: u64,
}

/// Parent-state account and storage reads for one address.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeterStateProviderAccountAccess {
    /// Address fetched from parent state.
    pub address: Address,
    /// Bytecode hash returned with the account, when it is a contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytecode_hash: Option<B256>,
    /// Number of account fetches.
    pub account_fetches: u64,
    /// Time spent fetching the account in microseconds.
    pub account_fetch_time_us: u128,
    /// Number of storage fetches.
    pub storage_fetches: u64,
    /// Time spent fetching storage in microseconds.
    pub storage_fetch_time_us: u128,
    /// Unique storage keys fetched for this address.
    pub storage_keys: BTreeSet<B256>,
}

/// Parent-state bytecode reads for one code hash.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeterStateProviderCodeAccess {
    /// Bytecode hash fetched from parent state.
    pub code_hash: B256,
    /// Number of fetches for this code hash.
    pub fetches: u64,
    /// Time spent fetching this bytecode in microseconds.
    pub fetch_time_us: u128,
    /// Total bytecode bytes returned.
    pub fetched_bytes: u64,
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
    use std::collections::BTreeSet;

    use alloy_primitives::{Address, B256};

    use super::{
        MeterBlockResponse, MeterBlockTransactions, MeterStateProviderAccountAccess,
        MeterStateProviderStats,
    };

    #[test]
    fn meter_block_response_serializes_deprecated_state_root_time_as_zero() {
        let storage_key = B256::repeat_byte(0x11);
        let response = MeterBlockResponse {
            block_hash: B256::ZERO,
            block_number: 1,
            signer_recovery_time_us: 2,
            execution_time_us: 3,
            state_root_time_us: 0,
            total_time_us: 5,
            state_provider: MeterStateProviderStats::default(),
            transactions: vec![MeterBlockTransactions {
                tx_hash: B256::ZERO,
                gas_used: 21_000,
                execution_time_us: 3,
                state_provider: MeterStateProviderStats::default(),
                state_provider_accounts: vec![MeterStateProviderAccountAccess {
                    address: Address::repeat_byte(0x22),
                    storage_keys: BTreeSet::from([storage_key]),
                    ..Default::default()
                }],
                state_provider_code_hashes: Vec::new(),
            }],
        };

        let json = serde_json::to_value(&response).unwrap();

        assert_eq!(json["stateRootTimeUs"], 0);
        assert!(json.get("stateProvider").is_some());
        assert_eq!(
            json["transactions"][0]["stateProviderAccounts"][0]["storageKeys"][0],
            storage_key.to_string()
        );
    }
}
