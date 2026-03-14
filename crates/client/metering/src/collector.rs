//! Metering collector that reads timing data from `PendingBlocks` broadcasts.

use std::{collections::HashMap, fmt, sync::Arc};

use alloy_consensus::BlockHeader;
use alloy_primitives::{TxHash, U256, keccak256};
use base_alloy_flz::flz_compress_len;
use base_flashblocks::PendingBlocks;
use parking_lot::RwLock;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::{MeteredTransaction, MeteringCache};

/// Subscribes to `PendingBlocks` broadcasts and populates [`MeteringCache`]
/// with per-transaction resource usage data derived from flashblock execution.
pub struct MeteringCollector {
    cache: Arc<RwLock<MeteringCache>>,
    state_root_cache: Arc<RwLock<HashMap<TxHash, u128>>>,
    flashblock_rx: broadcast::Receiver<Arc<PendingBlocks>>,
    last_block: u64,
    last_fb_idx: u64,
}

impl fmt::Debug for MeteringCollector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MeteringCollector")
            .field("last_block", &self.last_block)
            .field("last_fb_idx", &self.last_fb_idx)
            .finish_non_exhaustive()
    }
}

impl MeteringCollector {
    /// Creates a new metering collector.
    pub const fn new(
        cache: Arc<RwLock<MeteringCache>>,
        state_root_cache: Arc<RwLock<HashMap<TxHash, u128>>>,
        flashblock_rx: broadcast::Receiver<Arc<PendingBlocks>>,
    ) -> Self {
        Self { cache, state_root_cache, flashblock_rx, last_block: 0, last_fb_idx: 0 }
    }

    /// Runs the collector until the broadcast channel is closed.
    pub async fn run(mut self) {
        loop {
            match self.flashblock_rx.recv().await {
                Ok(pending) => self.handle_pending_blocks(&pending),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped = skipped, "metering collector lagged behind broadcast");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    fn handle_pending_blocks(&mut self, pending: &PendingBlocks) {
        let block_number = pending.latest_block_number();
        let fb_idx = pending.latest_flashblock_index();

        // Reorg detection: block regressed or same block with lower fb_idx.
        if block_number < self.last_block
            || (block_number == self.last_block && fb_idx < self.last_fb_idx)
        {
            let cleared = self.cache.write().clear_blocks_from(block_number);
            warn!(
                block_number = block_number,
                blocks_cleared = cleared,
                "reorg detected in metering collector"
            );
        }

        // Skip if already processed.
        if block_number == self.last_block && fb_idx <= self.last_fb_idx {
            return;
        }

        let base_fee = pending.latest_header().base_fee_per_gas().unwrap_or_default();

        // Get the latest flashblock for raw tx bytes.
        let flashblocks = pending.get_flashblocks();
        let Some(flashblock) = flashblocks.last() else {
            return;
        };

        let mut inserted = 0usize;
        let mut dropped_due_to_flashblock_capacity = false;

        {
            let mut cache = self.cache.write();
            let mut sr_cache = self.state_root_cache.write();
            let max_flashblocks_per_block = cache.max_flashblocks_per_block();

            for raw_tx in &flashblock.diff.transactions {
                let tx_hash = keccak256(raw_tx);

                // Look up the transaction to get receipt and check if it's a deposit.
                let Some(tx) = pending.get_transaction_by_hash(tx_hash) else {
                    continue;
                };

                if tx.inner.inner.is_deposit() {
                    continue;
                }

                let Some(receipt) = pending.get_receipt(tx_hash) else {
                    continue;
                };

                let gas_used = receipt.inner.gas_used;
                let effective_gas_price = receipt.inner.effective_gas_price;
                let priority_fee = effective_gas_price.saturating_sub(base_fee as u128);
                let da_bytes = flz_compress_len(raw_tx) as u64;
                let execution_time_us = pending.get_execution_time(&tx_hash).unwrap_or(0);

                // State root time: prefer simulation data from PendingBlocks,
                // fall back to externally-submitted data from setMeteringInfo.
                let state_root_time_us = pending
                    .get_state_root_time(&tx_hash)
                    .or_else(|| sr_cache.remove(&tx_hash))
                    .unwrap_or(0);

                let metered_tx = MeteredTransaction {
                    tx_hash,
                    priority_fee_per_gas: U256::from(priority_fee),
                    gas_used,
                    execution_time_us,
                    state_root_time_us,
                    data_availability_bytes: da_bytes,
                };

                if cache.push_transaction(block_number, fb_idx, metered_tx) {
                    inserted += 1;
                } else {
                    dropped_due_to_flashblock_capacity = true;
                    debug!(
                        block_number = block_number,
                        flashblock_index = fb_idx,
                        max_flashblocks_per_block = max_flashblocks_per_block,
                        tx_hash = %tx_hash,
                        "dropping metering data for flashblock beyond cache capacity"
                    );
                }
            }
        }

        if inserted > 0 {
            debug!(
                block_number = block_number,
                flashblock_index = fb_idx,
                transactions = inserted,
                "collected metering data from flashblock"
            );
        }

        if dropped_due_to_flashblock_capacity {
            warn!(
                block_number = block_number,
                flashblock_index = fb_idx,
                "dropping metering data for flashblock beyond configured cache capacity"
            );
        }

        self.last_block = block_number;
        self.last_fb_idx = fb_idx;
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Header, Receipt, ReceiptWithBloom, Sealed};
    use alloy_primitives::{Address, B256, Bloom, Bytes, Signature};
    use alloy_rpc_types_engine::PayloadId;
    use base_alloy_consensus::OpTxEnvelope;
    use base_alloy_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_alloy_rpc_types::{L1BlockInfo, OpTransactionReceipt, Transaction};
    use base_flashblocks::PendingBlocksBuilder;
    use revm::context_interface::result::ExecutionResult;

    use super::*;
    use crate::cache::MeteringCache;

    fn test_sender() -> Address {
        Address::repeat_byte(0x01)
    }

    fn make_raw_tx() -> (Bytes, B256) {
        // Create a simple legacy transaction
        let tx = alloy_consensus::TxLegacy {
            gas_limit: 21000,
            gas_price: 2_000_000_000,
            value: alloy_primitives::U256::from(1000),
            ..Default::default()
        };
        let signed =
            alloy_consensus::Signed::new_unchecked(tx, Signature::test_signature(), B256::ZERO);
        let envelope = OpTxEnvelope::Legacy(signed);
        let raw = alloy_eips::Encodable2718::encoded_2718(&envelope);
        let raw_bytes = Bytes::from(raw);
        let hash = keccak256(&raw_bytes);
        (raw_bytes, hash)
    }

    fn build_pending_with_tx(
        raw_tx: Bytes,
        tx_hash: B256,
        base_fee: u64,
        effective_gas_price: u128,
        execution_time_us: Option<u128>,
    ) -> PendingBlocks {
        let header = Header { number: 100, base_fee_per_gas: Some(base_fee), ..Default::default() };

        let tx = Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: alloy_consensus::transaction::Recovered::new_unchecked(
                    OpTxEnvelope::Legacy(alloy_consensus::Signed::new_unchecked(
                        alloy_consensus::TxLegacy::default(),
                        Signature::test_signature(),
                        tx_hash,
                    )),
                    test_sender(),
                ),
                block_hash: None,
                block_number: Some(100),
                transaction_index: Some(0),
                effective_gas_price: Some(effective_gas_price),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        };

        let receipt = OpTransactionReceipt {
            inner: alloy_rpc_types_eth::TransactionReceipt {
                inner: ReceiptWithBloom {
                    receipt: base_alloy_consensus::OpReceipt::Legacy(Receipt {
                        status: alloy_consensus::Eip658Value::Eip658(true),
                        cumulative_gas_used: 21000,
                        logs: vec![],
                    }),
                    logs_bloom: Bloom::default(),
                },
                transaction_hash: tx_hash,
                transaction_index: Some(0),
                block_hash: None,
                block_number: Some(100),
                gas_used: 21000,
                effective_gas_price,
                blob_gas_used: None,
                blob_gas_price: None,
                from: test_sender(),
                to: None,
                contract_address: None,
            },
            l1_block_info: L1BlockInfo::default(),
        };

        let flashblock = Flashblock {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: 100,
                gas_limit: 30_000_000,
                timestamp: 1_700_000_000,
                extra_data: Bytes::default(),
                base_fee_per_gas: alloy_primitives::U256::from(base_fee),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 21000,
                block_hash: B256::ZERO,
                transactions: vec![raw_tx],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata { block_number: 100 },
        };

        let mut builder = PendingBlocksBuilder::new();
        builder.with_header(Sealed::new_unchecked(header, B256::ZERO));
        builder.with_flashblocks([flashblock]);
        builder.with_transaction(tx);
        builder.with_receipt(tx_hash, receipt);
        builder.with_transaction_sender(tx_hash, test_sender());
        builder.with_transaction_state(tx_hash, Default::default());
        builder.with_transaction_result(
            tx_hash,
            ExecutionResult::Success {
                reason: revm::context::result::SuccessReason::Stop,
                gas_used: 21000,
                gas_refunded: 0,
                logs: vec![],
                output: revm::context::result::Output::Call(Bytes::new()),
            },
        );

        if let Some(time_us) = execution_time_us {
            builder.with_execution_time(tx_hash, time_us);
        }

        builder.build().expect("should build pending blocks")
    }

    #[test]
    fn collector_populates_cache_from_pending_blocks() {
        let (raw_tx, tx_hash) = make_raw_tx();
        let base_fee = 1_000_000_000u64;
        let effective_gas_price = 2_000_000_000u128;

        let pending =
            build_pending_with_tx(raw_tx, tx_hash, base_fee, effective_gas_price, Some(500));

        let cache = Arc::new(RwLock::new(MeteringCache::new(10, 1)));
        let state_root_cache = Arc::new(RwLock::new(HashMap::new()));
        let (_, rx) = broadcast::channel::<Arc<PendingBlocks>>(1);

        let mut collector = MeteringCollector::new(Arc::clone(&cache), state_root_cache, rx);
        collector.handle_pending_blocks(&pending);

        assert!(cache.read().contains_block(100));
    }

    #[test]
    fn collector_uses_state_root_cache_fallback() {
        let (raw_tx, tx_hash) = make_raw_tx();
        let base_fee = 1_000_000_000u64;
        let effective_gas_price = 2_000_000_000u128;

        let pending = build_pending_with_tx(raw_tx, tx_hash, base_fee, effective_gas_price, None);

        let cache = Arc::new(RwLock::new(MeteringCache::new(10, 1)));
        let state_root_cache = Arc::new(RwLock::new(HashMap::new()));

        // Pre-populate state root cache with external data
        state_root_cache.write().insert(tx_hash, 1234);

        let (_, rx) = broadcast::channel::<Arc<PendingBlocks>>(1);
        let mut collector =
            MeteringCollector::new(Arc::clone(&cache), Arc::clone(&state_root_cache), rx);
        collector.handle_pending_blocks(&pending);

        // State root cache entry should have been consumed
        assert!(!state_root_cache.read().contains_key(&tx_hash));
        assert!(cache.read().contains_block(100));
    }

    #[test]
    fn collector_skips_duplicate_flashblocks() {
        let (raw_tx, tx_hash) = make_raw_tx();

        let pending =
            build_pending_with_tx(raw_tx, tx_hash, 1_000_000_000, 2_000_000_000, Some(100));

        let cache = Arc::new(RwLock::new(MeteringCache::new(10, 1)));
        let state_root_cache = Arc::new(RwLock::new(HashMap::new()));
        let (_, rx) = broadcast::channel::<Arc<PendingBlocks>>(1);

        let mut collector = MeteringCollector::new(Arc::clone(&cache), state_root_cache, rx);

        // Process once
        collector.handle_pending_blocks(&pending);
        assert!(cache.read().contains_block(100));

        // Process again — should be a no-op (dedup)
        collector.handle_pending_blocks(&pending);
    }
}
