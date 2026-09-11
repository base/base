//! Transaction pool metrics.

use base_common_observability_metrics::{
    Metrics,
    metrics::{Counter, Gauge},
};

/// Transaction pool metrics
#[derive(Metrics)]
#[metrics(scope = "transaction_pool")]
pub struct TxPoolMetrics {
    /// Number of transactions inserted in the pool
    pub inserted_transactions: Counter,
    /// Number of invalid transactions
    pub invalid_transactions: Counter,
    /// Number of removed transactions from the pool
    pub removed_transactions: Counter,

    /// Number of transactions in the pending sub-pool
    pub pending_pool_transactions: Gauge,
    /// Total amount of memory used by the transactions in the pending sub-pool in bytes
    pub pending_pool_size_bytes: Gauge,

    /// Number of transactions in the basefee sub-pool
    pub basefee_pool_transactions: Gauge,
    /// Total amount of memory used by the transactions in the basefee sub-pool in bytes
    pub basefee_pool_size_bytes: Gauge,

    /// Number of transactions in the queued sub-pool
    pub queued_pool_transactions: Gauge,
    /// Total amount of memory used by the transactions in the queued sub-pool in bytes
    pub queued_pool_size_bytes: Gauge,

    /// Number of transactions in the blob sub-pool
    pub blob_pool_transactions: Gauge,
    /// Total amount of memory used by the transactions in the blob sub-pool in bytes
    pub blob_pool_size_bytes: Gauge,

    /// Number of all transactions of all sub-pools: pending + basefee + queued + blob
    pub total_transactions: Gauge,
    /// Number of all legacy transactions in the pool
    pub total_legacy_transactions: Gauge,
    /// Number of all EIP-2930 transactions in the pool
    pub total_eip2930_transactions: Gauge,
    /// Number of all EIP-1559 transactions in the pool
    pub total_eip1559_transactions: Gauge,
    /// Number of all EIP-4844 transactions in the pool
    pub total_eip4844_transactions: Gauge,
    /// Number of all EIP-7702 transactions in the pool
    pub total_eip7702_transactions: Gauge,
    /// Number of all other transactions in the pool
    pub total_other_transactions: Gauge,

    /// How often the pool was updated after the canonical state changed
    pub performed_state_updates: Counter,

    /// Counter for the number of pending transactions evicted
    pub pending_transactions_evicted: Counter,
    /// Counter for the number of basefee transactions evicted
    pub basefee_transactions_evicted: Counter,
    /// Counter for the number of blob transactions evicted
    pub blob_transactions_evicted: Counter,
    /// Counter for the number of queued transactions evicted
    pub queued_transactions_evicted: Counter,
}

/// Transaction pool maintenance metrics
#[derive(Metrics)]
#[metrics(scope = "transaction_pool")]
pub struct MaintainPoolMetrics {
    /// Gauge indicating the number of addresses with pending updates in the pool,
    /// requiring their account information to be fetched.
    pub dirty_accounts: Gauge,
    /// Counter for the number of times the pool state diverged from the canonical blockchain
    /// state.
    pub drift_count: Counter,
    /// Counter for the number of transactions reinserted into the pool following a blockchain
    /// reorganization (reorg).
    pub reinserted_transactions: Counter,
}

impl MaintainPoolMetrics {
    /// Sets the number of dirty accounts in the pool.
    #[inline]
    pub fn set_dirty_accounts_len(&self, count: usize) {
        self.dirty_accounts.set(count as f64);
    }

    /// Increments the count of reinserted transactions.
    #[inline]
    pub fn inc_reinserted_transactions(&self, count: usize) {
        self.reinserted_transactions.increment(count as u64);
    }

    /// Increments the drift count by one.
    #[inline]
    pub fn inc_drift(&self) {
        self.drift_count.increment(1);
    }
}

/// All Transactions metrics
#[derive(Metrics)]
#[metrics(scope = "transaction_pool")]
pub struct AllTransactionsMetrics {
    /// Number of all transactions by hash in the pool
    pub all_transactions_by_hash: Gauge,
    /// Number of all transactions by id in the pool
    pub all_transactions_by_id: Gauge,
    /// Number of all transactions by all senders in the pool
    pub all_transactions_by_all_senders: Gauge,
    /// Number of blob transactions nonce gaps.
    pub blob_transactions_nonce_gaps: Counter,
    /// The current blob base fee
    pub blob_base_fee: Gauge,
    /// The current base fee
    pub base_fee: Gauge,
}

/// Transaction pool validator task metrics
#[derive(Metrics)]
#[metrics(scope = "transaction_pool")]
pub struct TxPoolValidatorMetrics {
    /// Number of in-flight validation job sends waiting for channel capacity
    pub inflight_validation_jobs: Gauge,
}
