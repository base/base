//! Submission identifier, DA type, batch submission, and frame encoding.

use std::sync::Arc;

use alloy_primitives::Bytes;
use base_protocol::{DERIVATION_VERSION_0, Frame};

use crate::EncoderConfig;

/// Identifies a batch submission for receipt tracking.
///
/// Monotonic per encoder instance, including across `reset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubmissionId(pub u64);

/// Selects how batch frames are encoded for L1 submission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum DaType {
    /// EIP-4844 blobs.
    #[default]
    #[cfg_attr(feature = "clap", value(name = "blobs", alias = "blob"))]
    Blob,
    /// One frame as calldata (`[DERIVATION_VERSION_0] ++ frame.encode()`).
    Calldata,
}

/// Frames packed into one EIP-4844 blob payload.
#[derive(Debug, Clone)]
pub struct BlobPayload {
    /// Ordered derivation frames encoded into this blob.
    frames: Vec<Arc<Frame>>,
}

impl BlobPayload {
    /// Creates a non-empty blob payload.
    pub fn new(frames: Vec<Arc<Frame>>) -> Option<Self> {
        (!frames.is_empty()).then_some(Self { frames })
    }

    /// Returns the ordered frames carried by this blob.
    pub fn frames(&self) -> &[Arc<Frame>] {
        &self.frames
    }
}

/// Payload carried by one L1 batch transaction.
#[derive(Debug)]
pub enum SubmissionPayload {
    /// One or more packed EIP-4844 blob payloads.
    Blobs(Vec<BlobPayload>),
    /// One frame encoded directly as transaction calldata.
    Calldata(Arc<Frame>),
}

/// A single L1 transaction's worth of batch data.
#[derive(Debug)]
pub struct BatchSubmission {
    /// The unique identifier for this submission.
    pub id: SubmissionId,
    /// Data encoded into the transaction.
    payload: SubmissionPayload,
}

impl BatchSubmission {
    /// Creates a single-frame calldata submission.
    pub const fn calldata(id: SubmissionId, frame: Arc<Frame>) -> Self {
        Self { id, payload: SubmissionPayload::Calldata(frame) }
    }

    /// Creates a non-empty blob submission within the protocol transaction limit.
    pub fn blobs(id: SubmissionId, blobs: Vec<BlobPayload>) -> Option<Self> {
        (!blobs.is_empty() && blobs.len() <= EncoderConfig::MAX_BLOBS_PER_TX)
            .then_some(Self { id, payload: SubmissionPayload::Blobs(blobs) })
    }

    /// Returns the transaction payload.
    pub const fn payload(&self) -> &SubmissionPayload {
        &self.payload
    }

    /// Returns how this submission carries derivation data.
    pub const fn da_type(&self) -> DaType {
        match self.payload {
            SubmissionPayload::Blobs(_) => DaType::Blob,
            SubmissionPayload::Calldata(_) => DaType::Calldata,
        }
    }

    /// Returns the number of derivation frames in the transaction.
    pub fn frame_count(&self) -> usize {
        match &self.payload {
            SubmissionPayload::Blobs(blobs) => blobs.iter().map(|blob| blob.frames.len()).sum(),
            SubmissionPayload::Calldata(_) => 1,
        }
    }

    /// Returns the number of blobs in this submission.
    pub const fn blob_count(&self) -> usize {
        match &self.payload {
            SubmissionPayload::Blobs(blobs) => blobs.len(),
            SubmissionPayload::Calldata(_) => 0,
        }
    }

    /// Returns the total compressed channel bytes carried by all frames.
    pub fn frame_bytes(&self) -> usize {
        match &self.payload {
            SubmissionPayload::Blobs(blobs) => {
                blobs.iter().flat_map(|blob| &blob.frames).map(|frame| frame.data.len()).sum()
            }
            SubmissionPayload::Calldata(frame) => frame.data.len(),
        }
    }

    /// Returns the first frame in this submission.
    #[cfg(test)]
    pub fn first_frame(&self) -> Option<&Arc<Frame>> {
        match &self.payload {
            SubmissionPayload::Blobs(blobs) => blobs.first()?.frames.first(),
            SubmissionPayload::Calldata(frame) => Some(frame),
        }
    }
}

/// Encodes batch frames for L1 submission.
#[derive(Debug)]
pub struct FrameEncoder;

impl FrameEncoder {
    /// `[DERIVATION_VERSION_0] ++ frame.encode()`.
    pub fn to_calldata(frame: &Frame) -> Bytes {
        let encoded = frame.encode();
        let mut data = Vec::with_capacity(1 + encoded.len());
        data.push(DERIVATION_VERSION_0);
        data.extend_from_slice(&encoded);
        Bytes::from(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_blob_payloads_and_submissions() {
        assert!(BlobPayload::new(Vec::new()).is_none());
        assert!(BatchSubmission::blobs(SubmissionId(0), Vec::new()).is_none());
    }

    #[test]
    fn rejects_submission_above_blob_transaction_limit() {
        let blobs = (0..=EncoderConfig::MAX_BLOBS_PER_TX)
            .map(|_| BlobPayload::new(vec![Arc::new(Frame::default())]).unwrap())
            .collect();

        assert!(BatchSubmission::blobs(SubmissionId(0), blobs).is_none());
    }
}
