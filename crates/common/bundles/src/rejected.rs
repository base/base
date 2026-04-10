//! Rejected transaction types shared between builder and audit-archiver.

use alloy_primitives::TxHash;
use serde::{Deserialize, Serialize};

use crate::MeterBundleResponse;

/// Reason why a transaction was rejected during block building.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    /// Transaction's predicted execution time exceeded its per-tx limit.
    ExecutionTimeExceeded {
        /// Predicted execution time in microseconds.
        tx_time_us: u128,
        /// Per-transaction limit in microseconds.
        limit_us: u128,
    },
}

/// A transaction that was rejected during block building.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedTransaction {
    /// The block number the transaction was intended for.
    pub block_number: u64,
    /// The transaction hash.
    pub tx_hash: TxHash,
    /// The reason the transaction was rejected.
    pub reason: RejectionReason,
    /// Unix timestamp when the rejection occurred.
    pub timestamp: u64,
    /// The metering simulation response that informed the rejection decision.
    pub metering: MeterBundleResponse,
}
