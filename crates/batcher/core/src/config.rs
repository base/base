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
    /// When `true` and DA-backlog throttling is active, force the encoder to
    /// emit blob-typed submissions even when its configured `da_type` is
    /// calldata. Mirrors op-batcher behaviour: blobs amortise DA cost more
    /// efficiently when the L1 is congested with batcher data.
    ///
    /// No-op when the encoder is already configured for blob DA.
    /// Default: `true`.
    pub force_blobs_when_throttling: bool,
}
