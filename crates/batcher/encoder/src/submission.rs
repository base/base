//! Submission identifier, DA type, batch submission, and frame encoding.

use std::sync::Arc;

use alloy_primitives::Bytes;
use base_protocol::{ChannelId, DERIVATION_VERSION_0, Frame};

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
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum DaType {
    /// Frames are packed into EIP-4844 blobs (default).
    ///
    /// Multiple submissions may be packed into a single blob payload per L1 transaction.
    #[default]
    #[cfg_attr(feature = "clap", value(name = "blobs", alias = "blob"))]
    Blob,
    /// Each frame is sent as a single calldata transaction
    /// (`[DERIVATION_VERSION_0] ++ frame.encode()`).
    ///
    /// When using calldata mode, set
    /// [`EncoderConfig::target_num_frames`](crate::EncoderConfig::target_num_frames) to `1`
    /// so that each [`BatchSubmission`] contains exactly one frame.
    Calldata,
}

/// A single L1 transaction's worth of batch data: one or more frames encoded as blobs.
///
/// Contains up to [`EncoderConfig::target_num_frames`](crate::EncoderConfig::target_num_frames)
/// frames, each mapping to one EIP-4844 blob. Cancun supports up to 6 blobs per transaction;
/// Isthmus (EIP-7892) supports up to 21.
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

/// Encodes batch frames for L1 submission.
///
/// This unit type groups the frame-encoding utilities used by both the production
/// `BatchDriver` (in `base-batcher-core`) and the action-test harness, so the framing
/// logic is defined exactly once.
#[derive(Debug)]
pub struct FrameEncoder;

impl FrameEncoder {
    /// Encode a single frame as L1 calldata.
    ///
    /// Prepends the [`DERIVATION_VERSION_0`] byte to the encoded frame, producing
    /// the exact byte sequence the derivation pipeline expects to find in a batcher
    /// transaction's calldata field.
    pub fn to_calldata(frame: &Frame) -> Bytes {
        let encoded = frame.encode();
        let mut data = Vec::with_capacity(1 + encoded.len());
        data.push(DERIVATION_VERSION_0);
        data.extend_from_slice(&encoded);
        Bytes::from(data)
    }
}
