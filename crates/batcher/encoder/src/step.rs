//! Step result and error types for the batcher pipeline.

use base_comp::{BatchComposeError, ChannelOutError};
use base_protocol::SpanBatchError;

/// Result of a [`BatchPipeline::step`](crate::BatchPipeline::step) call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResult {
    /// One block was encoded into the current channel.
    BlockEncoded,
    /// A pending span batch was flushed into a channel without consuming another block.
    SpanFlushed,
    /// The current channel reached a closure trigger and was moved to the submission queue.
    ChannelClosed,
    /// No work available: no pending blocks and all open channels are already at capacity
    /// or awaiting confirmation.
    Idle,
}

/// Returned by [`BatchPipeline::step`](crate::BatchPipeline::step) when a block cannot be
/// encoded and the pipeline cannot continue.
///
/// Encoding failures are fatal. Continuing after a block-composition, span-construction,
/// or channel-output error could omit part of the contiguous L2 block sequence required
/// by the derivation spec.
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
    /// The accumulated span batch could not be built.
    #[error("span batch build failed for {blocks} accumulated blocks: {source}")]
    SpanBatchBuildFailed {
        /// Number of L2 blocks in the span accumulator.
        blocks: usize,
        /// Underlying span batch construction error.
        #[source]
        source: SpanBatchError,
    },
    /// The accumulated span batch could not fit in a fresh channel.
    #[error(
        "span batch with {blocks} accumulated blocks was rejected by an empty channel: {source}"
    )]
    SpanBatchRejectedByEmptyChannel {
        /// Number of L2 blocks in the span accumulator.
        blocks: usize,
        /// Underlying channel rejection.
        #[source]
        source: ChannelOutError,
    },
    /// A channel could not be finalized into frames.
    #[error("failed to finalize channel: {0}")]
    ChannelOutputFailed(#[from] ChannelOutError),
}
