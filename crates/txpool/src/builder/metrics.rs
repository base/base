use metrics::{Counter, Histogram};
use metrics_derive::Metrics;

/// Metrics for the builder RPC handler.
#[derive(Metrics, Clone)]
#[metrics(scope = "txpool.builder_rpc")]
pub struct BuilderApiMetrics {
    /// Transactions successfully inserted into the pool.
    #[metric(describe = "Transactions successfully inserted into the pool")]
    pub txs_inserted: Counter,
    /// Transactions that failed to decode.
    #[metric(describe = "Transactions that failed to decode")]
    pub decode_errors: Counter,
    /// Transactions rejected by the pool.
    #[metric(describe = "Transactions rejected by the pool")]
    pub txs_rejected: Counter,
    /// Time to insert a transaction in the local txpool.
    #[metric(describe = "Time to insert a transaction in the local txpool")]
    pub insert_duration: Histogram,
}
