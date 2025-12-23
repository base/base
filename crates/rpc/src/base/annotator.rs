//! Resource annotator that correlates Kafka metering data with flashblock inclusions.

use std::sync::Arc;

use alloy_primitives::TxHash;
use parking_lot::RwLock;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

use crate::{MeteredTransaction, MeteringCache};

/// Message received from the flashblocks websocket feed indicating which
/// transactions were included in a specific flashblock.
#[derive(Debug)]
pub struct FlashblockInclusion {
    /// Block number.
    pub block_number: u64,
    /// Flashblock index within the block.
    pub flashblock_index: u64,
    /// Tx hashes included in this flashblock.
    pub ordered_tx_hashes: Vec<TxHash>,
}

/// Maximum number of pending transactions before oldest entries are evicted.
const MAX_PENDING_TRANSACTIONS: usize = 10_000;

/// Annotates flashblock transactions with their resource usage.
///
/// The flow is:
/// 1. Kafka sends `MeteredTransaction` with resource usage data keyed by tx hash
/// 2. These are stored in a pending lookup table
/// 3. Websocket sends `FlashblockInclusion` with actual (block, flashblock) location
/// 4. We look up pending transactions and insert them into the cache at the real location
pub struct ResourceAnnotator {
    cache: Arc<RwLock<MeteringCache>>,
    tx_updates_rx: UnboundedReceiver<MeteredTransaction>,
    flashblock_rx: UnboundedReceiver<FlashblockInclusion>,
    /// Pending metering data awaiting flashblock inclusion confirmation.
    /// Uses IndexMap to maintain insertion order for FIFO eviction.
    pending_transactions: indexmap::IndexMap<TxHash, MeteredTransaction>,
}

impl ResourceAnnotator {
    /// Creates a new resource annotator.
    pub fn new(
        cache: Arc<RwLock<MeteringCache>>,
        tx_updates_rx: UnboundedReceiver<MeteredTransaction>,
        flashblock_rx: UnboundedReceiver<FlashblockInclusion>,
    ) -> Self {
        Self {
            cache,
            tx_updates_rx,
            flashblock_rx,
            pending_transactions: indexmap::IndexMap::new(),
        }
    }

    /// Runs the annotator until both channels are closed.
    pub async fn run(mut self) {
        info!(target: "metering::annotator", "Starting ResourceAnnotator");
        loop {
            tokio::select! {
                Some(tx_event) = self.tx_updates_rx.recv() => {
                    self.handle_tx_event(tx_event);
                }
                Some(flashblock_event) = self.flashblock_rx.recv() => {
                    self.handle_flashblock_event(flashblock_event);
                }
                else => {
                    info!(target: "metering::annotator", "ResourceAnnotator terminating");
                    break;
                }
            }
        }
    }

    fn handle_tx_event(&mut self, tx: MeteredTransaction) {
        debug!(
            tx_hash = %tx.tx_hash,
            gas_used = tx.gas_used,
            "Storing metered transaction in pending map"
        );
        self.pending_transactions.insert(tx.tx_hash, tx);

        // Evict oldest entries if we exceed the limit.
        while self.pending_transactions.len() > MAX_PENDING_TRANSACTIONS {
            if let Some((evicted_hash, _)) = self.pending_transactions.shift_remove_index(0) {
                info!(
                    tx_hash = %evicted_hash,
                    "Evicting old transaction from pending map (limit exceeded)"
                );
                metrics::counter!("metering.pending.evicted").increment(1);
            }
        }

        metrics::gauge!("metering.pending.size").set(self.pending_transactions.len() as f64);
    }

    fn handle_flashblock_event(&mut self, event: FlashblockInclusion) {

        let mut matched = 0usize;
        let mut missed = 0usize;

        {
            let mut cache = self.cache.write();
            for tx_hash in &event.ordered_tx_hashes {
                if let Some(tx) = self.pending_transactions.shift_remove(tx_hash) {
                    cache.insert_transaction(event.block_number, event.flashblock_index, tx);
                    matched += 1;
                } else {
                    missed += 1;
                }
            }
        }

        if matched > 0 {
            debug!(
                block_number = event.block_number,
                flashblock_index = event.flashblock_index,
                matched,
                "Inserted transactions into cache from flashblock"
            );
        }

        // All transactions should come through as bundles. Any misses indicate
        // the Kafka event hasn't arrived yet or was lost.
        if missed > 0 {
            warn!(
                block_number = event.block_number,
                flashblock_index = event.flashblock_index,
                matched,
                missed,
                "Flashblock contained transactions not found in pending map"
            );
            metrics::counter!("metering.streams.tx_misses_total").increment(missed as u64);
        }

        metrics::gauge!("metering.pending.size").set(self.pending_transactions.len() as f64);
    }
}
