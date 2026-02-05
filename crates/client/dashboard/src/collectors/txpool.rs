//! Transaction pool statistics collector.

use reth_transaction_pool::TransactionPool;

/// Simple struct to hold pool size counts.
#[derive(Debug, Clone, Default)]
pub(crate) struct PoolSizeCounts {
    /// Number of pending transactions.
    pub pending_count: usize,
    /// Number of queued transactions.
    pub queued_count: usize,
}

/// Collector for transaction pool statistics.
#[derive(Debug, Default)]
pub(crate) struct TxPoolCollector;

impl TxPoolCollector {
    /// Creates a new transaction pool collector.
    pub(crate) const fn new() -> Self {
        Self
    }

    /// Collects transaction pool size counts.
    pub(crate) fn collect<P: TransactionPool>(&self, pool: &P) -> PoolSizeCounts {
        let size = pool.pool_size();

        PoolSizeCounts { pending_count: size.pending, queued_count: size.queued }
    }
}
