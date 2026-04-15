//! Metering collector that reads timing data from `PendingBlocks` broadcasts.

use std::{collections::HashMap, fmt, sync::Arc};

use alloy_consensus::BlockHeader;
use alloy_primitives::{Bytes, U256, keccak256};
use base_common_flz::flz_compress_len;
use base_flashblocks::PendingBlocks;
use parking_lot::RwLock;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::{MeteredTransaction, MeteringCache, PendingStateRootTimes};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FlashblockPosition {
    block_number: u64,
    flashblock_index: u64,
}

impl FlashblockPosition {
    const fn new(block_number: u64, flashblock_index: u64) -> Self {
        Self { block_number, flashblock_index }
    }
}

/// Subscribes to `PendingBlocks` broadcasts and populates [`MeteringCache`]
/// with per-transaction resource usage data derived from flashblock execution.
pub struct MeteringCollector {
    cache: Arc<RwLock<MeteringCache>>,
    state_root_cache: Arc<RwLock<PendingStateRootTimes>>,
    flashblock_rx: broadcast::Receiver<Arc<PendingBlocks>>,
    last_earliest_block: Option<u64>,
    last_processed: Option<FlashblockPosition>,
}

impl fmt::Debug for MeteringCollector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MeteringCollector")
            .field("last_earliest_block", &self.last_earliest_block)
            .field("last_processed", &self.last_processed)
            .finish_non_exhaustive()
    }
}

impl MeteringCollector {
    /// Creates a new metering collector.
    pub const fn new(
        cache: Arc<RwLock<MeteringCache>>,
        state_root_cache: Arc<RwLock<PendingStateRootTimes>>,
        flashblock_rx: broadcast::Receiver<Arc<PendingBlocks>>,
    ) -> Self {
        Self {
            cache,
            state_root_cache,
            flashblock_rx,
            last_earliest_block: None,
            last_processed: None,
        }
    }

    /// Runs the collector until the broadcast channel is closed.
    pub async fn run(mut self) {
        loop {
            match self.flashblock_rx.recv().await {
                Ok(pending) => self.handle_pending_blocks(&pending),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(
                        skipped = skipped,
                        "metering collector lagged behind broadcast; unseen flashblocks will be replayed from the next snapshot"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    fn handle_pending_blocks(&mut self, pending: &PendingBlocks) {
        let earliest_block_number = pending.earliest_block_number();
        let latest_position = FlashblockPosition::new(
            pending.latest_block_number(),
            pending.latest_flashblock_index(),
        );
        debug!(
            earliest_block_number = earliest_block_number,
            latest_block_number = latest_position.block_number,
            latest_flashblock_index = latest_position.flashblock_index,
            "metering collector received pending blocks snapshot"
        );

        let snapshot_regressed = self.last_processed.is_some_and(|last| latest_position < last)
            || self
                .last_earliest_block
                .is_some_and(|last_earliest| earliest_block_number < last_earliest);

        if snapshot_regressed {
            let cleared = self.cache.write().clear_blocks_from(earliest_block_number);
            warn!(
                earliest_block_number = earliest_block_number,
                latest_block_number = latest_position.block_number,
                latest_flashblock_index = latest_position.flashblock_index,
                blocks_cleared = cleared,
                "reorg detected in metering collector"
            );
            self.last_processed = None;
        }

        let flashblocks = pending.get_flashblocks();
        if flashblocks.is_empty() {
            self.last_earliest_block = Some(earliest_block_number);
            return;
        }

        let latest_base_fee = pending.latest_header().base_fee_per_gas().unwrap_or_default();
        let mut base_fees_by_block =
            HashMap::from([(latest_position.block_number, latest_base_fee)]);
        for flashblock in &flashblocks {
            if let Some(base) = flashblock.base.as_ref() {
                base_fees_by_block.insert(
                    flashblock.metadata.block_number,
                    base.base_fee_per_gas.saturating_to(),
                );
            }
        }

        let mut last_processed = self.last_processed;
        for flashblock in flashblocks {
            let position =
                FlashblockPosition::new(flashblock.metadata.block_number, flashblock.index);
            if matches!(last_processed, Some(last) if position <= last) {
                continue;
            }

            let Some(base_fee) = base_fees_by_block.get(&position.block_number).copied() else {
                warn!(
                    block_number = position.block_number,
                    flashblock_index = position.flashblock_index,
                    "skipping metering data for flashblock without a known base fee"
                );
                last_processed = Some(position);
                continue;
            };

            self.process_flashblock(
                pending,
                position.block_number,
                position.flashblock_index,
                &flashblock.diff.transactions,
                base_fee,
            );
            last_processed = Some(position);
        }

        self.last_earliest_block = Some(earliest_block_number);
        self.last_processed = last_processed;
    }

    fn process_flashblock(
        &self,
        pending: &PendingBlocks,
        block_number: u64,
        flashblock_index: u64,
        raw_transactions: &[Bytes],
        base_fee: u64,
    ) {
        let mut metered_transactions = Vec::new();
        let mut saw_effective_gas_price_below_base_fee = false;
        let mut transactions_with_execution_time = 0usize;
        let mut transactions_missing_execution_time = 0usize;

        for raw_tx in raw_transactions {
            let tx_hash = keccak256(raw_tx);

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
            if effective_gas_price < base_fee as u128 {
                saw_effective_gas_price_below_base_fee = true;
            }
            let priority_fee = effective_gas_price.saturating_sub(base_fee as u128);
            let da_bytes = flz_compress_len(raw_tx) as u64;
            let execution_time_us = pending.get_execution_time(&tx_hash).map_or_else(
                || {
                    transactions_missing_execution_time += 1;
                    0
                },
                |execution_time_us| {
                    transactions_with_execution_time += 1;
                    execution_time_us
                },
            );

            // State root time: prefer simulation data from PendingBlocks,
            // fall back to externally-submitted data from setMeteringInformation.
            let state_root_time_us = pending
                .get_state_root_time(&tx_hash)
                .or_else(|| self.state_root_cache.write().pop(&tx_hash))
                .unwrap_or(0);

            metered_transactions.push(MeteredTransaction {
                tx_hash,
                priority_fee_per_gas: U256::from(priority_fee),
                gas_used,
                execution_time_us,
                state_root_time_us,
                data_availability_bytes: da_bytes,
            });
        }

        if saw_effective_gas_price_below_base_fee {
            warn!(
                block_number = block_number,
                flashblock_index = flashblock_index,
                base_fee = base_fee,
                "found transaction with effective gas price below base fee in pending metering data"
            );
        }

        let mut inserted = 0usize;
        let mut dropped_due_to_flashblock_capacity = false;
        {
            let mut cache = self.cache.write();
            let max_flashblocks_per_block = cache.max_flashblocks_per_block();

            for metered_tx in metered_transactions {
                let tx_hash = metered_tx.tx_hash;
                if cache.push_transaction(block_number, flashblock_index, metered_tx) {
                    inserted += 1;
                } else {
                    dropped_due_to_flashblock_capacity = true;
                    debug!(
                        block_number = block_number,
                        flashblock_index = flashblock_index,
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
                flashblock_index = flashblock_index,
                transactions = inserted,
                transactions_with_execution_time = transactions_with_execution_time,
                transactions_missing_execution_time = transactions_missing_execution_time,
                "collected metering data from flashblock"
            );
        }

        if dropped_due_to_flashblock_capacity {
            warn!(
                block_number = block_number,
                flashblock_index = flashblock_index,
                "dropping metering data for flashblock beyond configured cache capacity"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use alloy_consensus::{Header, Receipt, ReceiptWithBloom, Sealed};
    use alloy_primitives::{Address, B256, Bloom, Bytes, Signature};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_consensus::BaseTxEnvelope;
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_common_rpc_types::{BaseTransactionReceipt, L1BlockInfo, Transaction};
    use base_flashblocks::PendingBlocksBuilder;
    use revm::context_interface::result::ExecutionResult;

    use super::*;
    use crate::{PendingStateRootTimes, cache::MeteringCache};

    struct TestFlashblockTx {
        index: u64,
        raw_tx: Bytes,
        tx_hash: B256,
        effective_gas_price: u128,
        execution_time_us: Option<u128>,
    }

    fn test_sender() -> Address {
        Address::repeat_byte(0x01)
    }

    fn make_raw_tx_with_nonce(nonce: u64) -> (Bytes, B256) {
        // Create a simple legacy transaction
        let tx = alloy_consensus::TxLegacy {
            nonce,
            gas_limit: 21000,
            gas_price: 2_000_000_000,
            value: alloy_primitives::U256::from(1000),
            ..Default::default()
        };
        let signed =
            alloy_consensus::Signed::new_unchecked(tx, Signature::test_signature(), B256::ZERO);
        let envelope = BaseTxEnvelope::Legacy(signed);
        let raw = alloy_eips::Encodable2718::encoded_2718(&envelope);
        let raw_bytes = Bytes::from(raw);
        let hash = keccak256(&raw_bytes);
        (raw_bytes, hash)
    }

    fn make_raw_tx() -> (Bytes, B256) {
        make_raw_tx_with_nonce(0)
    }

    fn build_pending_with_flashblocks(
        base_fee: u64,
        entries: &[TestFlashblockTx],
    ) -> PendingBlocks {
        let header = Header { number: 100, base_fee_per_gas: Some(base_fee), ..Default::default() };

        let mut builder = PendingBlocksBuilder::new();
        builder.with_header(Sealed::new_unchecked(header, B256::ZERO));

        for entry in entries {
            let tx = Transaction {
                inner: alloy_rpc_types_eth::Transaction {
                    inner: alloy_consensus::transaction::Recovered::new_unchecked(
                        BaseTxEnvelope::Legacy(alloy_consensus::Signed::new_unchecked(
                            alloy_consensus::TxLegacy::default(),
                            Signature::test_signature(),
                            entry.tx_hash,
                        )),
                        test_sender(),
                    ),
                    block_hash: None,
                    block_number: Some(100),
                    transaction_index: Some(entry.index),
                    effective_gas_price: Some(entry.effective_gas_price),
                },
                deposit_nonce: None,
                deposit_receipt_version: None,
            };

            let receipt = BaseTransactionReceipt {
                inner: alloy_rpc_types_eth::TransactionReceipt {
                    inner: ReceiptWithBloom {
                        receipt: base_common_consensus::BaseReceipt::Legacy(Receipt {
                            status: alloy_consensus::Eip658Value::Eip658(true),
                            cumulative_gas_used: 21000,
                            logs: vec![],
                        }),
                        logs_bloom: Bloom::default(),
                    },
                    transaction_hash: entry.tx_hash,
                    transaction_index: Some(entry.index),
                    block_hash: None,
                    block_number: Some(100),
                    gas_used: 21000,
                    effective_gas_price: entry.effective_gas_price,
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
                index: entry.index,
                base: (entry.index == 0).then_some(ExecutionPayloadBaseV1 {
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
                    transactions: vec![entry.raw_tx.clone()],
                    withdrawals: vec![],
                    withdrawals_root: B256::ZERO,
                    blob_gas_used: None,
                },
                metadata: Metadata { block_number: 100 },
            };

            builder.with_flashblocks([flashblock]);
            builder.with_transaction(tx);
            builder.with_receipt(entry.tx_hash, receipt);
            builder.with_transaction_sender(entry.tx_hash, test_sender());
            builder.with_transaction_state(entry.tx_hash, Default::default());
            builder.with_transaction_result(
                entry.tx_hash,
                ExecutionResult::Success {
                    reason: revm::context::result::SuccessReason::Stop,
                    gas: revm::context::result::ResultGas::new(21_000, 21_000, 0, 0, 0),
                    logs: vec![],
                    output: revm::context::result::Output::Call(Bytes::new()),
                },
            );

            if let Some(time_us) = entry.execution_time_us {
                builder.with_execution_time(entry.tx_hash, time_us);
            }
        }

        builder.build().expect("should build pending blocks")
    }

    fn build_pending_with_tx(
        raw_tx: Bytes,
        tx_hash: B256,
        base_fee: u64,
        effective_gas_price: u128,
        execution_time_us: Option<u128>,
    ) -> PendingBlocks {
        build_pending_with_flashblocks(
            base_fee,
            &[TestFlashblockTx {
                index: 0,
                raw_tx,
                tx_hash,
                effective_gas_price,
                execution_time_us,
            }],
        )
    }

    #[test]
    fn collector_populates_cache_from_pending_blocks() {
        let (raw_tx, tx_hash) = make_raw_tx();
        let base_fee = 1_000_000_000u64;
        let effective_gas_price = 2_000_000_000u128;

        let pending =
            build_pending_with_tx(raw_tx, tx_hash, base_fee, effective_gas_price, Some(500));

        let cache = Arc::new(RwLock::new(MeteringCache::new(10, 1)));
        let state_root_cache =
            Arc::new(RwLock::new(PendingStateRootTimes::new(NonZeroUsize::new(8).unwrap())));
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
        let state_root_cache =
            Arc::new(RwLock::new(PendingStateRootTimes::new(NonZeroUsize::new(8).unwrap())));

        // Pre-populate state root cache with external data
        state_root_cache.write().push(tx_hash, 1234);

        let (_, rx) = broadcast::channel::<Arc<PendingBlocks>>(1);
        let mut collector =
            MeteringCollector::new(Arc::clone(&cache), Arc::clone(&state_root_cache), rx);
        collector.handle_pending_blocks(&pending);

        // State root cache entry should have been consumed
        assert!(!state_root_cache.read().contains(&tx_hash));
        assert!(cache.read().contains_block(100));
    }

    #[test]
    fn collector_skips_duplicate_flashblocks() {
        let (raw_tx, tx_hash) = make_raw_tx();

        let pending =
            build_pending_with_tx(raw_tx, tx_hash, 1_000_000_000, 2_000_000_000, Some(100));

        let cache = Arc::new(RwLock::new(MeteringCache::new(10, 1)));
        let state_root_cache =
            Arc::new(RwLock::new(PendingStateRootTimes::new(NonZeroUsize::new(8).unwrap())));
        let (_, rx) = broadcast::channel::<Arc<PendingBlocks>>(1);

        let mut collector = MeteringCollector::new(Arc::clone(&cache), state_root_cache, rx);

        // Process once
        collector.handle_pending_blocks(&pending);
        assert!(cache.read().contains_block(100));

        // Process again — should be a no-op (dedup)
        collector.handle_pending_blocks(&pending);
    }

    #[test]
    fn collector_replays_unseen_flashblocks_from_latest_snapshot() {
        let (raw_tx_0, tx_hash_0) = make_raw_tx_with_nonce(0);
        let (raw_tx_1, tx_hash_1) = make_raw_tx_with_nonce(1);
        let (raw_tx_2, tx_hash_2) = make_raw_tx_with_nonce(2);

        let pending_0 = build_pending_with_flashblocks(
            1_000_000_000,
            &[TestFlashblockTx {
                index: 0,
                raw_tx: raw_tx_0.clone(),
                tx_hash: tx_hash_0,
                effective_gas_price: 2_000_000_000,
                execution_time_us: Some(100),
            }],
        );
        let pending_2 = build_pending_with_flashblocks(
            1_000_000_000,
            &[
                TestFlashblockTx {
                    index: 0,
                    raw_tx: raw_tx_0,
                    tx_hash: tx_hash_0,
                    effective_gas_price: 2_000_000_000,
                    execution_time_us: Some(100),
                },
                TestFlashblockTx {
                    index: 1,
                    raw_tx: raw_tx_1,
                    tx_hash: tx_hash_1,
                    effective_gas_price: 3_000_000_000,
                    execution_time_us: Some(200),
                },
                TestFlashblockTx {
                    index: 2,
                    raw_tx: raw_tx_2,
                    tx_hash: tx_hash_2,
                    effective_gas_price: 4_000_000_000,
                    execution_time_us: Some(300),
                },
            ],
        );

        let cache = Arc::new(RwLock::new(MeteringCache::new(10, 3)));
        let state_root_cache =
            Arc::new(RwLock::new(PendingStateRootTimes::new(NonZeroUsize::new(8).unwrap())));
        let (_, rx) = broadcast::channel::<Arc<PendingBlocks>>(1);
        let mut collector = MeteringCollector::new(Arc::clone(&cache), state_root_cache, rx);

        collector.handle_pending_blocks(&pending_0);
        collector.handle_pending_blocks(&pending_2);

        let cache = cache.read();
        let block = cache.block(100).expect("block should exist");
        let flashblock_indexes: Vec<_> =
            block.flashblocks().map(|fb| fb.flashblock_index).collect();
        assert_eq!(flashblock_indexes, vec![0, 1, 2]);
    }

    #[test]
    fn collector_reprocesses_pending_range_after_regression() {
        let (raw_tx_0, tx_hash_0) = make_raw_tx_with_nonce(0);
        let (raw_tx_1, tx_hash_1) = make_raw_tx_with_nonce(1);

        let pending_1 = build_pending_with_flashblocks(
            1_000_000_000,
            &[
                TestFlashblockTx {
                    index: 0,
                    raw_tx: raw_tx_0.clone(),
                    tx_hash: tx_hash_0,
                    effective_gas_price: 2_000_000_000,
                    execution_time_us: Some(100),
                },
                TestFlashblockTx {
                    index: 1,
                    raw_tx: raw_tx_1,
                    tx_hash: tx_hash_1,
                    effective_gas_price: 3_000_000_000,
                    execution_time_us: Some(200),
                },
            ],
        );
        let pending_0 = build_pending_with_flashblocks(
            1_000_000_000,
            &[TestFlashblockTx {
                index: 0,
                raw_tx: raw_tx_0,
                tx_hash: tx_hash_0,
                effective_gas_price: 2_000_000_000,
                execution_time_us: Some(100),
            }],
        );

        let cache = Arc::new(RwLock::new(MeteringCache::new(10, 2)));
        let state_root_cache =
            Arc::new(RwLock::new(PendingStateRootTimes::new(NonZeroUsize::new(8).unwrap())));
        let (_, rx) = broadcast::channel::<Arc<PendingBlocks>>(1);
        let mut collector = MeteringCollector::new(Arc::clone(&cache), state_root_cache, rx);

        collector.handle_pending_blocks(&pending_1);
        collector.handle_pending_blocks(&pending_0);

        let cache = cache.read();
        let block = cache.block(100).expect("block should exist");
        let flashblock_indexes: Vec<_> =
            block.flashblocks().map(|fb| fb.flashblock_index).collect();
        assert_eq!(flashblock_indexes, vec![0]);
    }
}
