//! Error types for the batch driver.

use base_batcher_encoder::StepError;

use crate::BatchTxCandidateError;

/// Errors produced by the [`BatchDriver`](crate::BatchDriver).
#[derive(Debug, thiserror::Error)]
pub enum BatchDriverError {
    /// A blob submission could not be built into a transaction. The encoder packs blobs
    /// within protocol limits, so a retry would fail the same way.
    #[error("blob transaction could not be built: {0}")]
    Blob(#[from] BatchTxCandidateError),
    /// A block could not be composed into a batch; continuing would produce a gap
    /// in the submitted L2 block sequence.
    #[error("fatal pipeline step error: {0}")]
    Step(#[from] StepError),
    /// The required derivation-status source stopped.
    #[error("derivation-status source closed")]
    DerivationStatusSourceClosed,
}
