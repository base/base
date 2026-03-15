//! Core types for the batcher encoding pipeline.

use std::sync::Arc;

use alloy_primitives::B256;
use base_protocol::{ChannelId, Frame};

/// Identifies a batch submission for receipt tracking.
///
/// Monotonically increasing per [`BatchEncoder`](crate::BatchEncoder) instance,
/// including across [`BatchPipeline::reset`](crate::BatchPipeline::reset) calls.
/// The counter is intentionally **not** reset to 0 on reset so that post-reset
/// submissions can never share an ID with any pre-reset in-flight submission,
/// eliminating stale-confirm collisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubmissionId(pub u64);

/// Selects how batch frames are encoded for L1 submission.
///
/// The driver uses this field on each [`BatchSubmission`] to determine whether
/// frames should be packed into EIP-4844 blobs or sent as transaction calldata.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DaType {
    /// Frames are packed into EIP-4844 blobs, one blob per frame (default).
    #[default]
    Blob,
    /// Each frame is sent as a single calldata transaction
    /// (`[DERIVATION_VERSION_0] ++ frame.encode()`).
    ///
    /// When using calldata mode, set
    /// [`EncoderConfig::target_num_frames`] to `1` so that each
    /// [`BatchSubmission`] contains exactly one frame.
    Calldata,
}

/// A single L1 transaction's worth of batch data: one or more frames encoded as blobs.
///
/// Contains up to [`EncoderConfig::target_num_frames`] frames, each mapping to one
/// EIP-4844 blob. Cancun supports up to 6 blobs per transaction; Isthmus (EIP-7892)
/// supports up to 21.
#[derive(Debug)]
pub struct BatchSubmission {
    /// The unique identifier for this submission.
    pub id: SubmissionId,
    /// The channel this submission belongs to.
    pub channel_id: ChannelId,
    /// How frames in this submission should be encoded for L1.
    pub da_type: DaType,
    /// Frames to include in this L1 transaction.
    /// Each frame maps to one EIP-4844 blob. Shared via [`Arc`] to avoid
    /// deep copies of the frame payload when handing off to the driver.
    pub frames: Vec<Arc<Frame>>,
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

/// Selects whether the [`BatchEncoder`](crate::BatchEncoder) encodes blocks as individual
/// [`SingleBatch`](base_consensus_genesis::batch::SingleBatch)es or groups them into a
/// [`SpanBatch`](base_protocol::SpanBatch).
///
/// In [`Single`](BatchType::Single) mode every L2 block is fed into the open channel
/// immediately. In [`Span`](BatchType::Span) mode blocks are accumulated in memory and
/// flushed as a single [`SpanBatch`](base_protocol::SpanBatch) when the channel is closed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BatchType {
    /// Each L2 block is encoded as a separate
    /// [`SingleBatch`](base_consensus_genesis::batch::SingleBatch) (default).
    #[default]
    Single,
    /// All L2 blocks accumulated since the last channel close are grouped into a
    /// single [`SpanBatch`](base_protocol::SpanBatch) and written when the channel is
    /// closed.
    Span,
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

    /// Safety margin subtracted from `max_channel_duration` when evaluating channel
    /// timeout. The effective timeout is `max_channel_duration - sub_safety_margin`,
    /// ensuring channels are closed this many L1 blocks before the configured
    /// duration expires.
    ///
    /// Set this large enough so that in-flight frames land well within the protocol's
    /// `channel_timeout` inclusion window. A margin of 4–10 is typical; the default
    /// of 0 means no margin (effective timeout equals `max_channel_duration` exactly).
    ///
    /// Note: if `sub_safety_margin >= max_channel_duration` the effective timeout
    /// saturates to 0 L1 blocks and every channel closes immediately on the next
    /// `advance_l1_head` call. Ensure `sub_safety_margin < max_channel_duration`.
    ///
    /// Default: 0.
    pub sub_safety_margin: u64,

    /// Number of frames to pack into a single L1 transaction.
    ///
    /// Each frame maps to one EIP-4844 blob, so setting this to N submits N blobs
    /// per transaction. Cancun supports up to 6; Isthmus (EIP-7892) up to 21.
    ///
    /// Default: 1 (one blob per transaction).
    pub target_num_frames: usize,

    /// Whether to encode blocks as individual [`SingleBatch`](base_consensus_genesis::batch::SingleBatch)es
    /// or accumulate them into a single [`SpanBatch`](base_protocol::SpanBatch).
    ///
    /// Default: [`BatchType::Single`].
    pub batch_type: BatchType,

    /// How frames should be encoded for L1 submission.
    ///
    /// When set to [`DaType::Calldata`], set [`target_num_frames`] to `1` so
    /// that each [`BatchSubmission`] contains exactly one frame (one calldata
    /// tx per frame matches the derivation protocol).
    ///
    /// Default: [`DaType::Blob`].
    ///
    /// [`target_num_frames`]: EncoderConfig::target_num_frames
    pub da_type: DaType,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            target_frame_size: 130_044,
            max_frame_size: 130_044,
            max_channel_duration: 2,
            sub_safety_margin: 0,
            target_num_frames: 1,
            batch_type: BatchType::Single,
            da_type: DaType::Blob,
        }
    }
}

impl EncoderConfig {
    /// Validate the configuration, returning an error if any constraint is violated.
    ///
    /// This should be called at service startup before constructing a
    /// [`BatchEncoder`](crate::BatchEncoder). Catching misconfigurations early prevents
    /// subtle runtime failures such as channels closing immediately on every
    /// `advance_l1_head` call (which occurs when `sub_safety_margin >= max_channel_duration`).
    pub const fn validate(&self) -> Result<(), EncoderConfigError> {
        if self.sub_safety_margin >= self.max_channel_duration {
            return Err(EncoderConfigError::SafetyMarginTooLarge {
                sub_safety_margin: self.sub_safety_margin,
                max_channel_duration: self.max_channel_duration,
            });
        }
        if matches!(self.da_type, DaType::Calldata) && self.target_num_frames != 1 {
            return Err(EncoderConfigError::CalldataRequiresSingleFrame {
                target_num_frames: self.target_num_frames,
            });
        }
        Ok(())
    }
}

/// Errors returned by [`EncoderConfig::validate`].
#[derive(Debug, thiserror::Error)]
pub enum EncoderConfigError {
    /// `sub_safety_margin >= max_channel_duration`.
    ///
    /// The effective channel timeout (`max_channel_duration - sub_safety_margin`) would
    /// saturate to 0, causing every channel to close immediately on the first
    /// `advance_l1_head` call. Ensure `sub_safety_margin < max_channel_duration`.
    #[error(
        "sub_safety_margin ({sub_safety_margin}) must be less than \
         max_channel_duration ({max_channel_duration})"
    )]
    SafetyMarginTooLarge {
        /// The configured safety margin.
        sub_safety_margin: u64,
        /// The configured maximum channel duration.
        max_channel_duration: u64,
    },
    /// `da_type == DaType::Calldata` but `target_num_frames != 1`.
    ///
    /// Calldata mode submits one frame per L1 transaction. Set
    /// `target_num_frames = 1` when using [`DaType::Calldata`].
    #[error("calldata DA requires target_num_frames == 1, got {target_num_frames}")]
    CalldataRequiresSingleFrame {
        /// The configured target number of frames.
        target_num_frames: usize,
    },
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn config_with(sub_safety_margin: u64, max_channel_duration: u64) -> EncoderConfig {
        EncoderConfig { sub_safety_margin, max_channel_duration, ..EncoderConfig::default() }
    }

    #[rstest]
    #[case(0, 2)] // zero margin: always valid
    #[case(1, 2)] // one below duration
    #[case(4, 10)] // typical production values
    fn validate_ok(#[case] sub_safety_margin: u64, #[case] max_channel_duration: u64) {
        assert!(config_with(sub_safety_margin, max_channel_duration).validate().is_ok());
    }

    #[rstest]
    #[case(2, 2)] // equal: effective timeout saturates to 0
    #[case(5, 2)] // greater: same failure mode
    #[case(u64::MAX, 1)] // extreme: maximum possible margin
    fn validate_err(#[case] sub_safety_margin: u64, #[case] max_channel_duration: u64) {
        let err = config_with(sub_safety_margin, max_channel_duration).validate().unwrap_err();
        assert!(matches!(
            err,
            EncoderConfigError::SafetyMarginTooLarge {
                sub_safety_margin: m,
                max_channel_duration: d,
            } if m == sub_safety_margin && d == max_channel_duration
        ));
        // Error message must be human-readable and include both values.
        let msg = err.to_string();
        assert!(msg.contains(&sub_safety_margin.to_string()));
        assert!(msg.contains(&max_channel_duration.to_string()));
    }
}
