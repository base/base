//! Test utilities for provider tests.

use alloy_consensus::Header;
use alloy_primitives::{Address, B256};

/// Creates a test header with the given block number and timestamp.
///
/// This is a shared helper for tests across provider modules.
#[cfg(test)]
pub(crate) fn test_header(number: u64, timestamp: u64) -> Header {
    Header {
        parent_hash: B256::repeat_byte(0x01),
        ommers_hash: B256::ZERO,
        beneficiary: Address::repeat_byte(0x02),
        state_root: B256::repeat_byte(0x03),
        transactions_root: B256::repeat_byte(0x04),
        receipts_root: B256::repeat_byte(0x05),
        logs_bloom: Default::default(),
        difficulty: Default::default(),
        number,
        gas_limit: 30_000_000,
        gas_used: 21_000,
        timestamp,
        extra_data: Default::default(),
        mix_hash: B256::repeat_byte(0x06),
        nonce: Default::default(),
        base_fee_per_gas: Some(1_000_000_000),
        withdrawals_root: Some(B256::repeat_byte(0x07)),
        blob_gas_used: Some(131_072),
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(B256::repeat_byte(0x08)),
        requests_hash: None,
    }
}
