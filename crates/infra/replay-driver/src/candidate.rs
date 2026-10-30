use std::time::Duration;

use alloy_primitives::{B256, U256, keccak256};
use alloy_rpc_types_engine::ExecutionPayloadV1;

/// Measurements from a discarded shadow payload, never selected as canonical head.
#[derive(Debug)]
pub struct Candidate {
    /// Payload block number.
    pub block_number: u64,
    /// Transaction hashes in execution order, including deposits.
    pub transaction_hashes: Vec<B256>,
    /// Total execution gas consumed.
    pub gas_used: u64,
    /// Exact coinbase fees require per-transaction gas usage (receipts), absent from a payload.
    pub coinbase_fee_total: Option<U256>,
    /// Monotonic elapsed time from build request through getPayload, including build wait.
    pub build_latency: Duration,
    /// Unavailable until flashblock subscription is wired into replay.
    pub flashblock_count: Option<u64>,
}

impl Candidate {
    /// Extracts payload-only metrics without executing transactions again.
    pub fn from_payload(payload: &ExecutionPayloadV1, build_latency: Duration) -> Self {
        Self {
            block_number: payload.block_number,
            transaction_hashes: payload.transactions.iter().map(keccak256).collect(),
            gas_used: payload.gas_used,
            coinbase_fee_total: None,
            build_latency,
            flashblock_count: None,
        }
    }
}
