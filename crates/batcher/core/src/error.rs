//! Error types for the batch driver.

use base_batcher_encoder::{ReorgError, StepError};
use base_batcher_source::SourceError;
use base_blobs::BlobEncodeError;

/// Errors produced by the [`BatchDriver`](crate::BatchDriver).
#[derive(Debug, thiserror::Error)]
pub enum BatchDriverError {
    /// An error from the L2 block source.
    #[error("source error: {0}")]
    Source(#[from] SourceError),
    /// An error encoding frames into blobs.
    #[error("blob encoding error: {0}")]
    Blob(#[from] BlobEncodeError),
    /// A reorg was detected during block ingestion.
    #[error("reorg during block ingestion: {0}")]
    Reorg(#[from] ReorgError),
    /// A block could not be composed into a batch; continuing would produce a gap
    /// in the submitted L2 block sequence.
    #[error("fatal pipeline step error: {0}")]
    Step(#[from] StepError),
}
