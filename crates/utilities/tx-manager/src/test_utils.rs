//! Shared test utilities for the tx-manager crate.

use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_primitives::{Address, B256, Bloom, Bytes};
use alloy_rpc_types_eth::TransactionReceipt;

use crate::PreparedTx;

/// Helpers for building signed-transaction test fixtures.
#[derive(Debug)]
pub struct StubTx;

impl StubTx {
    /// Builds a minimal [`PreparedTx`] whose bytes and hash carry `marker`.
    pub fn prepared(nonce: u64, marker: u8) -> PreparedTx {
        PreparedTx {
            raw_tx: Bytes::from(vec![marker]),
            tx_hash: B256::with_last_byte(marker),
            gas_tip_cap: 1,
            gas_fee_cap: 2,
            blob_fee_cap: None,
            gas_limit: 21_000,
            nonce,
            sidecar: None,
        }
    }
}

/// Helpers for building test transaction receipts.
#[derive(Debug)]
pub struct StubReceipt;

impl StubReceipt {
    /// Builds a minimal successful `TransactionReceipt`.
    pub fn success() -> TransactionReceipt {
        let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt: Receipt {
                status: Eip658Value::success(),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::ZERO,
        });
        TransactionReceipt {
            inner,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        }
    }
}
