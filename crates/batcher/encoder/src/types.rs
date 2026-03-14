//! Core types for the batcher encoding pipeline.

use alloy_primitives::B256;
use base_protocol::{ChannelId, Frame};

/// Identifies a batch submission for receipt tracking.
///
/// Monotonically increasing per [`BatchEncoder`](crate::BatchEncoder) instance.
/// Unique within the lifetime of the encoder (reset to 0 on
/// [`BatchPipeline::reset`](crate::BatchPipeline::reset)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubmissionId(pub u64);

/// A single L1 transaction's worth of batch data: one or more frames encoded as blobs.
///
/// Currently always contains exactly one frame (one EIP-4844 blob per L1 transaction),
/// matching the op-batcher v1.16.3 behavior. The `frames` field is a `Vec` to allow
/// future extension to multi-blob transactions (e.g., when `target_num_frames > 1`
/// post-Isthmus with 21-blob limit).
#[derive(Debug)]
pub struct BatchSubmission {
    /// The unique identifier for this submission.
    pub id: SubmissionId,
    /// The channel this submission belongs to.
    pub channel_id: ChannelId,
    /// Frames to include in this L1 transaction.
    /// Each frame maps to one EIP-4844 blob. Currently length 1.
    pub frames: Vec<Frame>,
}

/// Result of a [`BatchPipeline::step`](crate::BatchPipeline::step) call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResult {
    /// One block was encoded into the current channel.
    BlockEncoded,
    /// The current channel reached a closure trigger and was moved to the submission queue.
    ChannelClosed,
    /// No work available: no pending blocks and all open channels are already at capacity
    /// or awaiting confirmation.
    Idle,
}

/// Returned by [`BatchPipeline::step`](crate::BatchPipeline::step) when a block cannot be
/// encoded and the pipeline cannot continue.
///
/// Batch composition failure is fatal: a block that cannot be serialised into a
/// [`SingleBatch`](base_consensus_genesis::batch::SingleBatch) would be silently absent
/// from the submitted data, breaking the contiguous L2 block sequence required by the
/// derivation spec. The batcher must halt rather than skip such a block.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// The block could not be converted to a [`SingleBatch`].
    #[error("batch composition failed for block at cursor {cursor}: {source}")]
    CompositionFailed {
        /// Index of the block in the encoder's input queue.
        cursor: usize,
        /// Underlying composition error.
        #[source]
        source: base_comp::BatchComposeError,
    },
}

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

/// Configuration for the [`BatchEncoder`](crate::BatchEncoder).
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Target compressed output size per channel. Drives `ShadowCompressor` closure.
    /// Default: 130,044 bytes (`BLOB_MAX_DATA_SIZE`).
    pub target_frame_size: usize,

    /// Maximum byte size of each output frame when draining a closed channel.
    ///
    /// Defaults to `target_frame_size`. Set smaller to force multi-frame output
    /// (e.g. in tests that exercise partial-channel submission and channel timeouts).
    pub max_frame_size: usize,

    /// Maximum L1 blocks a channel may stay open.
    /// Default: 2.
    pub max_channel_duration: u64,

    /// Safety margin subtracted from `max_channel_duration` when deciding whether
    /// to close a channel due to timeout. Reserved for future use; currently unused
    /// by [`BatchEncoder`](crate::BatchEncoder).
    /// Default: 4.
    pub sub_safety_margin: u64,

    /// Number of frames per L1 transaction (each frame = one EIP-4844 blob).
    /// Reserved for future use; currently unused by [`BatchEncoder`](crate::BatchEncoder),
    /// which always emits one frame per submission. Future: up to 6 (Cancun) or 21
    /// (Isthmus, EIP-7892).
    pub target_num_frames: usize,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            target_frame_size: 130_044,
            max_frame_size: 130_044,
            max_channel_duration: 2,
            sub_safety_margin: 4,
            target_num_frames: 1,
        }
    }
}
