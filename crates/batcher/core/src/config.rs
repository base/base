//! Configuration types for the batch driver.

use std::time::Duration;

/// Configuration for a [`BatchDriver`](crate::BatchDriver) instance.
#[derive(Debug, Clone)]
pub struct BatchDriverConfig {
    /// The batcher inbox address on L1.
    pub inbox: alloy_primitives::Address,
    /// Maximum number of in-flight transactions before back-pressure kicks in.
    pub max_pending_transactions: usize,
    /// Maximum time to wait for in-flight transactions to settle when draining
    /// on cancellation or source exhaustion. Submissions that have not
    /// confirmed within this window are abandoned.
    pub drain_timeout: Duration,
}
