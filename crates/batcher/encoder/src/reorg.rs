//! Reorg detection error type.

use alloy_primitives::B256;

/// Returned by [`BatchPipeline::add_block`](crate::BatchPipeline::add_block) when a reorg
/// is detected.
#[derive(Debug, thiserror::Error)]
pub enum ReorgError {
    /// The block's parent hash does not match the current tip.
    #[error("parent hash mismatch: expected {expected}, got {got}")]
    ParentMismatch {
        /// The expected parent hash (current tip).
        expected: B256,
        /// The actual parent hash from the incoming block.
        got: B256,
    },
}
