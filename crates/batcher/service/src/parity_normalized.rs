//! Normalized batcher submission data.

use alloy_primitives::{B256, FixedBytes};
use serde::{Deserialize, Serialize};

/// Batch kind after channel and frame metadata have been stripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NormalizedBatchKind {
    /// Single-batch submission.
    Single,
    /// Span-batch submission.
    Span,
}

/// Stable batch-level summary used for parity comparisons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedBatch {
    /// Decoded batch kind.
    pub kind: NormalizedBatchKind,
    /// Single-batch parent hash, if this is a single batch.
    pub parent_hash: Option<B256>,
    /// Single-batch L1 origin hash, if this is a single batch.
    pub epoch_hash: Option<B256>,
    /// Span-batch parent hash prefix, if this is a span batch.
    pub parent_check: Option<FixedBytes<20>>,
    /// Span-batch L1 origin hash prefix, if this is a span batch.
    pub l1_origin_check: Option<FixedBytes<20>>,
    /// Span-batch chain ID, if this is a span batch.
    pub chain_id: Option<u64>,
    /// Span-batch L1 origin transition bitfield, if this is a span batch.
    pub origin_bits: Option<Vec<u8>>,
    /// First L2 timestamp covered by this decoded batch.
    pub start_timestamp: u64,
    /// Last L2 timestamp covered by this decoded batch.
    pub end_timestamp: u64,
    /// First L1 origin number covered by this decoded batch.
    pub start_epoch_num: u64,
    /// Last L1 origin number covered by this decoded batch.
    pub end_epoch_num: u64,
    /// Number of L2 blocks represented by this decoded batch.
    pub block_count: u64,
    /// Transaction count for each L2 block represented by this decoded batch.
    pub tx_counts: Vec<u64>,
    /// Keccak256 hash of each encoded L2 transaction, in derived block order.
    pub tx_hashes: Vec<B256>,
}

/// Normalized view of one submitted DA payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedSubmission {
    /// Decoded batches.
    pub batches: Vec<NormalizedBatch>,
    /// Number of complete channels decoded from the submission.
    pub complete_channels: usize,
    /// Number of incomplete channels left after ingesting all frames.
    pub incomplete_channels: usize,
    /// Number of frames rejected while assembling channels.
    pub rejected_frames: usize,
    /// Number of complete channels that failed strict batch decoding.
    pub decode_errors: usize,
}

/// Normalized view of one complete channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedChannel {
    /// Decoded batches.
    pub batches: Vec<NormalizedBatch>,
}

/// Summary comparison between two normalized submissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParityComparison {
    /// Whether both normalized batch lists are exactly equal.
    pub is_match: bool,
    /// Number of batches on the left side.
    pub left_len: usize,
    /// Number of batches on the right side.
    pub right_len: usize,
    /// First mismatching batch index, or the first missing index if lengths differ.
    pub first_mismatch: Option<usize>,
}
