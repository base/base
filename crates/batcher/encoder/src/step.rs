//! Step result and error types for the batcher pipeline.

use base_comp::BatchComposeError;

use crate::{ChannelFullReason, OpenChannelError};

/// Result of a [`BatchPipeline::step`](crate::BatchPipeline::step) call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResult {
    /// One block was encoded into the current channel.
    BlockEncoded,
    /// The current channel reached a closure trigger and was moved to the submission queue.
    ChannelClosed,
    /// No transition is available: no block is pending and no channel must close.
    Idle,
}

/// Returned by [`BatchPipeline::step`](crate::BatchPipeline::step) when a block cannot be
/// encoded and the pipeline cannot continue.
///
/// Encoding failures are fatal. Continuing after a block-composition or channel-output
/// error could omit part of the contiguous L2 block sequence required by the derivation
/// spec.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// The block could not be converted to a [`SingleBatch`].
    #[error("batch composition failed for block at cursor {cursor}: {source}")]
    CompositionFailed {
        /// Index of the block in the encoder's input queue.
        cursor: usize,
        /// Underlying composition error.
        #[source]
        source: BatchComposeError,
    },
    /// A block cannot fit in an empty channel and therefore cannot be published.
    #[error("block at cursor {cursor} was rejected by an empty channel: {reason}")]
    BlockRejectedByEmptyChannel {
        /// Index of the block in the encoder's input queue.
        cursor: usize,
        /// Size constraint that rejected the block.
        reason: ChannelFullReason,
    },
    /// Channel construction, compression, or framing failed.
    #[error(transparent)]
    ChannelFailed(#[from] OpenChannelError),
}
