//! Transaction flow collector for Sankey diagram visualization.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::{TxLink, TxNode, TxPoolData};

/// Defines the nodes in the transaction flow Sankey diagram.
pub(crate) fn get_tx_nodes() -> Vec<TxNode> {
    vec![
        TxNode { name: "P2P Hashes".to_string(), inclusion: None },
        TxNode { name: "Received Txs".to_string(), inclusion: None },
        TxNode { name: "Duplicate".to_string(), inclusion: None },
        TxNode { name: "Invalid".to_string(), inclusion: None },
        TxNode { name: "Tx Pool".to_string(), inclusion: None },
        TxNode { name: "Added To Block".to_string(), inclusion: Some(true) },
        TxNode { name: "Evicted".to_string(), inclusion: None },
    ]
}

/// Atomic counters for tracking transaction flow.
#[derive(Debug, Default)]
pub(crate) struct TxFlowCounters {
    /// Transaction hashes announced via P2P.
    hashes_received: AtomicU64,
    /// Full transactions received.
    txs_received: AtomicU64,
    /// Duplicate transactions detected.
    duplicates: AtomicU64,
    /// Invalid transactions rejected.
    invalid: AtomicU64,
    /// Transactions added to pool.
    pool_adds: AtomicU64,
    /// Transactions included in blocks.
    block_adds: AtomicU64,
    /// Transactions evicted from pool.
    evicted: AtomicU64,
}

impl TxFlowCounters {
    /// Creates a new set of flow counters.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records transactions included in a block.
    pub(crate) fn record_block_adds(&self, count: u64) {
        self.block_adds.fetch_add(count, Ordering::Relaxed);
    }
}

/// Collector for transaction flow statistics.
#[derive(Debug)]
pub(crate) struct TxFlowCollector {
    /// Shared counters for transaction flow events.
    counters: Arc<TxFlowCounters>,
}

impl TxFlowCollector {
    /// Creates a new transaction flow collector.
    pub(crate) const fn new(counters: Arc<TxFlowCounters>) -> Self {
        Self { counters }
    }

    /// Collects current transaction flow data for Sankey diagram.
    pub(crate) fn collect(&self, pooled_tx: usize, pooled_blob_tx: usize) -> TxPoolData {
        let hashes_received = self.counters.hashes_received.load(Ordering::Relaxed);
        let txs_received = self.counters.txs_received.load(Ordering::Relaxed);
        let duplicates = self.counters.duplicates.load(Ordering::Relaxed);
        let invalid = self.counters.invalid.load(Ordering::Relaxed);
        let pool_adds = self.counters.pool_adds.load(Ordering::Relaxed);
        let block_adds = self.counters.block_adds.load(Ordering::Relaxed);
        let evicted = self.counters.evicted.load(Ordering::Relaxed);

        // Build Sankey links representing transaction flow
        let links = vec![
            TxLink {
                source: "P2P Hashes".to_string(),
                target: "Received Txs".to_string(),
                value: txs_received,
            },
            TxLink {
                source: "Received Txs".to_string(),
                target: "Duplicate".to_string(),
                value: duplicates,
            },
            TxLink {
                source: "Received Txs".to_string(),
                target: "Invalid".to_string(),
                value: invalid,
            },
            TxLink {
                source: "Received Txs".to_string(),
                target: "Tx Pool".to_string(),
                value: pool_adds,
            },
            TxLink {
                source: "Tx Pool".to_string(),
                target: "Added To Block".to_string(),
                value: block_adds,
            },
            TxLink { source: "Tx Pool".to_string(), target: "Evicted".to_string(), value: evicted },
        ];

        TxPoolData { pooled_tx, pooled_blob_tx, hashes_received, links }
    }
}
