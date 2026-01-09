//! Types for the transaction status rpc

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

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

// Block metering types

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
