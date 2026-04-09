//! Rejected transaction types shared between builder and audit-archiver.

use alloy_primitives::TxHash;
use serde::{Deserialize, Serialize};

use crate::MeterBundleResponse;

/// A transaction that was rejected during block building.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedTransaction {
    /// The block number the transaction was intended for.
    pub block_number: u64,
    /// The transaction hash.
    pub tx_hash: TxHash,
    /// The reason the transaction was rejected.
    pub reason: String,
    /// Unix timestamp when the rejection occurred.
    pub timestamp: u64,
    /// The metering simulation response that informed the rejection decision.
    pub metering: MeterBundleResponse,
}
