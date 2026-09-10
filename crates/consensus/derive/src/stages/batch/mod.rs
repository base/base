//! Validation of singular batches read from assembled channels.
use alloc::boxed::Box;

use async_trait::async_trait;
use base_consensus_batch::SingleBatch;

use crate::PipelineResult;
mod batch_validator;
pub use batch_validator::BatchValidator;

/// Supplies decoded singular batches to the validator.
#[async_trait]
pub trait NextBatchProvider {
    /// Reads the next batch, or returns EOF when the channel is exhausted.
    async fn next_batch(&mut self) -> PipelineResult<SingleBatch>;
    /// Discards the current channel after an invalid batch.
    fn flush(&mut self);
}
