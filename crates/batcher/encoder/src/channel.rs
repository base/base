//! Channel state machine types.

use std::{fmt, ops::Range, sync::Arc};

use alloy_rlp::Encodable;
use base_common_genesis::RollupConfig;
use base_comp::{
    ChannelOut, ChannelOutError, CompressorError, CompressorWriter, ShadowCompressor,
    VariantCompressor,
};
use base_protocol::{BatchType, ChannelId, Frame, SingleBatch, SpanBatch, SpanBatchError};

use crate::EncoderConfig;

/// Why a candidate block did not fit in an open channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChannelFullReason {
    /// The candidate exceeded the configured compressed output target.
    #[error("compressed output target exceeded")]
    CompressedOutput,
    /// The candidate exceeded the protocol RLP input limit.
    #[error("maximum RLP input bytes reached")]
    RlpInput,
}

/// Result of attempting to add one L2 block to an open channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelAddOutcome {
    /// The block was accepted and the channel can accept more blocks.
    Accepted,
    /// The block was accepted and exactly filled or exceeded the compression target.
    AcceptedAndFull,
    /// The block was not accepted; the caller decides whether it can be retried.
    Rejected(ChannelFullReason),
}

/// Failure while building or finalizing an open channel.
#[derive(Debug, thiserror::Error)]
pub enum OpenChannelError {
    /// A single-batch channel or frame operation failed.
    #[error("channel output failed: {0}")]
    Output(#[from] ChannelOutError),
    /// Span-batch construction or encoding failed.
    #[error("span batch failed: {0}")]
    SpanBatch(#[from] SpanBatchError),
    /// Exact span-channel compression failed.
    #[error("span channel compression failed: {0}")]
    Compression(#[from] CompressorError),
}

/// Span-mode state for one open derivation channel.
///
/// [`OpenChannelKind::Span`] owns this state while the encoder fills the channel.
/// One channel may contain multiple sealed [`SpanBatch`]es and one active span.
/// `accepted_rlp` is the sole committed payload; `candidate_rlp` is swapped into
/// it only after the candidate passes the RLP and compressed-size limits.
#[derive(Debug)]
pub struct SpanChannel {
    /// Unique channel identifier used when producing frames.
    id: ChannelId,
    /// Rollup configuration used for protocol limits and span metadata.
    rollup_config: Arc<RollupConfig>,
    /// Compressor checkpointed against accepted RLP input.
    compressor: VariantCompressor,
    /// Span batch currently accepting blocks.
    active_span: SpanBatch,
    /// RLP input containing all accepted span batches.
    accepted_rlp: Vec<u8>,
    /// Reusable RLP buffer for the next candidate state.
    candidate_rlp: Vec<u8>,
    /// Reusable buffer for the encoded active span batch.
    encoded_span: Vec<u8>,
    /// Number of accepted RLP bytes belonging to sealed span batches.
    sealed_rlp_bytes: usize,
    /// Accepted RLP length represented by the current compressor checkpoint.
    compressed_rlp_bytes: usize,
    /// Maximum compressed bytes that fit in the configured target frames.
    target_output_bytes: usize,
    /// Optional maximum number of blocks per span batch.
    max_blocks_per_span_batch: Option<usize>,
}

impl SpanChannel {
    /// Creates the Span state selected by [`OpenChannel::new`].
    pub fn new(id: ChannelId, rollup_config: Arc<RollupConfig>, config: &EncoderConfig) -> Self {
        let compressor = VariantCompressor::from(config.compression_algo);
        let active_span = SpanBatch {
            chain_id: rollup_config.l2_chain_id.id(),
            genesis_timestamp: rollup_config.genesis.l2_time,
            ..Default::default()
        };

        Self {
            id,
            rollup_config,
            compressor,
            active_span,
            accepted_rlp: Vec::new(),
            candidate_rlp: Vec::new(),
            encoded_span: Vec::new(),
            sealed_rlp_bytes: 0,
            compressed_rlp_bytes: 0,
            target_output_bytes: config.target_output_size(),
            max_blocks_per_span_batch: config.max_blocks_per_span_batch,
        }
    }

    /// Attempts to add one L2 block, represented as a [`SingleBatch`].
    ///
    /// [`OpenChannel::add_block`] calls this from the encoder's `step` transition.
    /// The returned [`ChannelAddOutcome`] tells the caller whether to advance the
    /// block cursor, close the channel, or retry the block in a fresh channel.
    pub fn add_block(
        &mut self,
        batch: SingleBatch,
        sequence_number: u64,
    ) -> Result<ChannelAddOutcome, OpenChannelError> {
        // A span at the configured block limit is already represented by
        // `accepted_rlp`; seal it before building the next candidate span.
        if self.max_blocks_per_span_batch.is_some_and(|max| self.active_span.batches.len() == max) {
            self.sealed_rlp_bytes = self.accepted_rlp.len();
            self.candidate_rlp.clear();
            self.candidate_rlp.extend_from_slice(&self.accepted_rlp);
            self.active_span = SpanBatch {
                chain_id: self.rollup_config.l2_chain_id.id(),
                genesis_timestamp: self.rollup_config.genesis.l2_time,
                ..Default::default()
            };
        }

        // `active_span` may retain a rejected block, but that channel becomes
        // terminal. Only `accepted_rlp` is ever finalized or submitted.
        let timestamp = batch.timestamp;
        self.active_span.append_singular_batch(batch, sequence_number)?;

        // Rebuild the candidate while preserving already sealed spans and
        // replacing only the active span's RLP byte string.
        self.encoded_span.clear();
        self.encoded_span.push(BatchType::Span as u8);
        self.active_span.encode(&mut self.encoded_span)?;
        self.candidate_rlp.truncate(self.sealed_rlp_bytes);
        self.encoded_span.as_slice().encode(&mut self.candidate_rlp);

        // The protocol RLP limit is hard: unlike the compressed target, even
        // the first block cannot exceed it.
        let max_rlp_bytes = self.rollup_config.max_rlp_bytes_per_channel(timestamp) as usize;
        if self.candidate_rlp.len() > max_rlp_bytes {
            return Ok(ChannelAddOutcome::Rejected(ChannelFullReason::RlpInput));
        }

        // Defer full recompression until the exact checkpoint plus the new
        // uncompressed bytes approaches the target.
        // Swapping the candidate into `accepted_rlp` is the commit point in each
        // accepted branch below.
        let rlp_growth = self.candidate_rlp.len() - self.compressed_rlp_bytes;
        if self.compressor.compressed_len()? + rlp_growth < self.target_output_bytes {
            std::mem::swap(&mut self.accepted_rlp, &mut self.candidate_rlp);
            return Ok(ChannelAddOutcome::Accepted);
        }

        // Near the boundary, recompress the complete candidate so the decision
        // uses its actual encoded size.
        self.compressor.reset();
        self.compressor.write(&self.candidate_rlp)?;
        self.compressed_rlp_bytes = self.candidate_rlp.len();
        let compressed_bytes = self.compressor.compressed_len()?;
        if compressed_bytes < self.target_output_bytes {
            std::mem::swap(&mut self.accepted_rlp, &mut self.candidate_rlp);
            return Ok(ChannelAddOutcome::Accepted);
        }

        // A channel must make progress even when one block exceeds the soft
        // compression target. An exact fit is also retained.
        if self.accepted_rlp.is_empty() || compressed_bytes == self.target_output_bytes {
            std::mem::swap(&mut self.accepted_rlp, &mut self.candidate_rlp);
            return Ok(ChannelAddOutcome::AcceptedAndFull);
        }

        // The candidate did not fit. Restore the compressor to the committed
        // payload before the caller finalizes this channel and retries the block.
        self.compress_accepted()?;
        Ok(ChannelAddOutcome::Rejected(ChannelFullReason::CompressedOutput))
    }

    /// Restores the compressor to the committed payload.
    ///
    /// This is the rollback path after a rejected candidate and the finalization
    /// path when accepted candidates used only the cheap size pre-check.
    pub fn compress_accepted(&mut self) -> Result<usize, CompressorError> {
        self.compressor.reset();
        self.compressor.write(&self.accepted_rlp)?;
        self.compressed_rlp_bytes = self.accepted_rlp.len();
        self.compressor.compressed_len()
    }

    /// Consumes accepted Span state and produces the channel's complete frame list.
    ///
    /// [`OpenChannel::into_frames`] calls this when the encoder closes the channel.
    /// Uncommitted candidate bytes are never passed to the framing layer.
    pub fn into_frames(mut self, max_frame_size: usize) -> Result<Vec<Frame>, OpenChannelError> {
        // Accepted candidates may have used the cheap size pre-check, leaving
        // the compressor at an older checkpoint.
        if self.compressed_rlp_bytes != self.accepted_rlp.len() {
            self.compress_accepted()?;
        }
        Ok(ChannelOut::new(self.id, self.rollup_config, self.compressor)
            .into_frames(max_frame_size)?)
    }
}

/// Batch-type-specific state owned by an open channel.
#[derive(Debug)]
pub enum OpenChannelKind {
    /// Incrementally compressed single batches.
    Single(ChannelOut<ShadowCompressor>),
    /// Transactionally encoded span batches.
    Span(Box<SpanChannel>),
}

/// Batch-type facade for the channel currently accepting L2 blocks.
///
/// The encoder owns at most one `OpenChannel`. This type keeps lifecycle metadata
/// common to both producer modes and delegates encoding to [`OpenChannelKind`].
pub struct OpenChannel {
    /// Batch-type-specific channel state.
    kind: OpenChannelKind,
    /// Index of the first block encoded into this channel.
    pub(crate) block_start: usize,
    /// L1 block number when this channel was opened (for `MaxChannelDuration`).
    pub(crate) opened_at_l1: u64,
    /// Number of L2 blocks fed into this channel so far.
    pub(crate) blocks_added: usize,
    /// Estimated DA bytes for blocks fed into this channel.
    pub(crate) da_backlog_bytes: u64,
}

impl fmt::Debug for OpenChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenChannel")
            .field("channel_id", &self.id())
            .field("block_start", &self.block_start)
            .field("opened_at_l1", &self.opened_at_l1)
            .field("blocks_added", &self.blocks_added)
            .field("da_backlog_bytes", &self.da_backlog_bytes)
            .finish()
    }
}

impl OpenChannel {
    /// Creates the channel requested by the encoder when `step` needs one.
    pub fn new(
        id: ChannelId,
        rollup_config: Arc<RollupConfig>,
        config: &EncoderConfig,
        block_start: usize,
        opened_at_l1: u64,
    ) -> Self {
        let kind = match config.batch_type {
            BatchType::Single => {
                let compressor =
                    ShadowCompressor::new(config.target_frame_size as u64, config.compression_algo);
                OpenChannelKind::Single(ChannelOut::new(id, rollup_config, compressor))
            }
            BatchType::Span => {
                OpenChannelKind::Span(Box::new(SpanChannel::new(id, rollup_config, config)))
            }
        };

        Self { kind, block_start, opened_at_l1, blocks_added: 0, da_backlog_bytes: 0 }
    }

    /// Returns the channel identifier.
    pub const fn id(&self) -> ChannelId {
        match &self.kind {
            OpenChannelKind::Single(out) => out.id(),
            OpenChannelKind::Span(channel) => channel.id,
        }
    }

    /// Returns the accepted uncompressed RLP input length.
    pub const fn input_bytes(&self) -> u64 {
        match &self.kind {
            OpenChannelKind::Single(out) => out.input_bytes(),
            OpenChannelKind::Span(channel) => channel.accepted_rlp.len() as u64,
        }
    }

    /// Dispatches one block to the configured producer mode.
    ///
    /// The encoder calls this once per `step`. Accepted outcomes also update the
    /// channel's block count and DA backlog before the caller advances its cursor.
    pub fn add_block(
        &mut self,
        batch: SingleBatch,
        sequence_number: u64,
        da_backlog_bytes: u64,
    ) -> Result<ChannelAddOutcome, OpenChannelError> {
        let outcome = match &mut self.kind {
            OpenChannelKind::Single(out) => match out.add_single_batch(batch) {
                Ok(()) => ChannelAddOutcome::Accepted,
                Err(ChannelOutError::Compression(CompressorError::Full)) => {
                    ChannelAddOutcome::Rejected(ChannelFullReason::CompressedOutput)
                }
                Err(ChannelOutError::ExceedsMaxRlpBytesPerChannel) => {
                    ChannelAddOutcome::Rejected(ChannelFullReason::RlpInput)
                }
                Err(error) => return Err(error.into()),
            },
            OpenChannelKind::Span(channel) => channel.add_block(batch, sequence_number)?,
        };

        if matches!(outcome, ChannelAddOutcome::Accepted | ChannelAddOutcome::AcceptedAndFull) {
            self.blocks_added += 1;
            self.da_backlog_bytes += da_backlog_bytes;
        }
        Ok(outcome)
    }

    /// Consumes the open channel and returns all frames ready for submission.
    ///
    /// The encoder calls this exactly once when size, timeout, or a force-close
    /// request closes the channel.
    pub fn into_frames(self, max_frame_size: usize) -> Result<Vec<Frame>, OpenChannelError> {
        match self.kind {
            OpenChannelKind::Single(out) => Ok(out.into_frames(max_frame_size)?),
            OpenChannelKind::Span(channel) => channel.into_frames(max_frame_size),
        }
    }
}

/// Submission lifecycle of a frame in a closed channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameState {
    /// The frame is available for submission.
    Ready,
    /// The frame was submitted and is awaiting an outcome.
    Pending,
    /// The frame was confirmed on L1.
    Confirmed,
}

/// A closed channel retained while its frames and derivation progress are tracked.
#[derive(Debug)]
pub struct ReadyChannel {
    /// The channel identifier.
    pub(crate) id: ChannelId,
    /// All frames, in order. Wrapped in [`Arc`] so that the slice handed to
    /// [`BatchSubmission`] is a cheap pointer copy rather than a deep clone of
    /// the frame payload (up to `max_frame_size` bytes per frame).
    pub(crate) frames: Vec<Arc<Frame>>,
    /// Submission state for each frame.
    pub(crate) frame_states: Vec<FrameState>,
    /// Exact input block range encoded into this channel.
    pub(crate) encoded_block_range: Range<usize>,
    /// DA bytes still represented by this closed channel's frames.
    pub(crate) da_backlog_bytes: u64,
    /// Earliest L1 block that confirmed any frame from this channel.
    pub(crate) first_confirmed_l1_block: Option<u64>,
    /// Latest L1 block that confirmed any frame from this channel.
    pub(crate) last_confirmed_l1_block: Option<u64>,
}

impl ReadyChannel {
    /// Returns the first contiguous range of frames available for submission.
    ///
    /// Per-frame state replaces a monotonic cursor so retries can make only the
    /// affected frames ready again without resubmitting confirmed frames.
    pub fn next_ready_frame_range(&self) -> Option<Range<usize>> {
        let start = self.frame_states.iter().position(|state| *state == FrameState::Ready)?;
        let count = self.frame_states[start..]
            .iter()
            .take_while(|state| **state == FrameState::Ready)
            .count();
        Some(start..start + count)
    }

    /// Marks a ready frame range as awaiting an L1 submission outcome.
    pub fn mark_pending(&mut self, range: Range<usize>) {
        debug_assert!(
            self.frame_states[range.clone()].iter().all(|state| *state == FrameState::Ready)
        );
        self.frame_states[range].fill(FrameState::Pending);
    }

    /// Marks a pending frame range as confirmed on L1.
    pub fn mark_confirmed(&mut self, range: Range<usize>) {
        debug_assert!(
            self.frame_states[range.clone()].iter().all(|state| *state == FrameState::Pending)
        );
        self.frame_states[range].fill(FrameState::Confirmed);
    }

    /// Returns a pending frame range to the submission queue.
    pub fn mark_ready(&mut self, range: Range<usize>) {
        debug_assert!(
            self.frame_states[range.clone()].iter().all(|state| *state == FrameState::Pending)
        );
        self.frame_states[range].fill(FrameState::Ready);
    }

    /// Returns `true` once every frame in the channel is confirmed on L1.
    pub fn is_fully_confirmed(&self) -> bool {
        self.frame_states.iter().all(|state| *state == FrameState::Confirmed)
    }
}

/// Tracks a pending submission back to its channel and frame range.
#[derive(Debug, Clone)]
pub struct PendingRef {
    /// Index into the `ready_channels` deque.
    pub(crate) channel_idx: usize,
    /// Index of the first frame in the ready channel covered by this submission.
    pub(crate) frame_start: usize,
    /// Number of frames included in this submission (1 when `target_num_frames == 1`).
    pub(crate) frame_count: usize,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{SignableTransaction, TxEnvelope, TxLegacy};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Bytes, Signature};
    use base_common_genesis::RollupConfig;
    use base_protocol::{ChannelId, Frame, SingleBatch};

    use super::{ChannelAddOutcome, FrameState, ReadyChannel, SpanChannel};
    use crate::{CompressionAlgo, EncoderConfig};

    fn span_config(target_output_bytes: usize) -> EncoderConfig {
        EncoderConfig {
            batch_type: base_protocol::BatchType::Span,
            compression_algo: CompressionAlgo::Zlib,
            target_frame_size: Frame::ENCODED_OVERHEAD + target_output_bytes,
            max_frame_size: EncoderConfig::MAX_BLOB_FRAME_SIZE,
            ..EncoderConfig::default()
        }
    }

    fn single_batch(timestamp: u64) -> SingleBatch {
        SingleBatch { epoch_num: timestamp, timestamp, ..Default::default() }
    }

    fn exact_compressed_size(blocks: usize) -> usize {
        let mut channel = SpanChannel::new(
            ChannelId::default(),
            Arc::new(RollupConfig::default()),
            &span_config(EncoderConfig::MAX_BLOB_FRAME_SIZE - Frame::ENCODED_OVERHEAD),
        );
        for timestamp in 1..=blocks as u64 {
            assert_eq!(
                channel.add_block(single_batch(timestamp), timestamp - 1).unwrap(),
                ChannelAddOutcome::Accepted
            );
        }
        channel.compress_accepted().unwrap()
    }

    fn channel_with_states(frame_states: Vec<FrameState>) -> ReadyChannel {
        let frames = frame_states.iter().map(|_| Arc::new(Frame::default())).collect();
        ReadyChannel {
            id: Default::default(),
            frames,
            frame_states,
            encoded_block_range: 0..0,
            da_backlog_bytes: 0,
            first_confirmed_l1_block: None,
            last_confirmed_l1_block: None,
        }
    }

    #[test]
    fn returns_first_contiguous_ready_range() {
        let channel = channel_with_states(vec![
            FrameState::Confirmed,
            FrameState::Ready,
            FrameState::Ready,
            FrameState::Pending,
            FrameState::Ready,
        ]);

        assert_eq!(channel.next_ready_frame_range(), Some(1..3));
    }

    #[test]
    fn transitions_only_selected_frame_range() {
        let mut channel = channel_with_states(vec![
            FrameState::Ready,
            FrameState::Ready,
            FrameState::Pending,
            FrameState::Confirmed,
        ]);

        channel.mark_pending(0..2);
        channel.mark_confirmed(0..1);
        channel.mark_ready(1..3);

        assert_eq!(
            channel.frame_states,
            [FrameState::Confirmed, FrameState::Ready, FrameState::Ready, FrameState::Confirmed,]
        );
        assert_eq!(channel.next_ready_frame_range(), Some(1..3));
    }

    #[test]
    fn span_channel_rejects_overflow_without_committing_candidate() {
        let one_block_size = exact_compressed_size(1);
        let two_block_size = exact_compressed_size(2);
        assert!(two_block_size > one_block_size);
        let target = one_block_size + 1;
        let mut channel = SpanChannel::new(
            ChannelId::default(),
            Arc::new(RollupConfig::default()),
            &span_config(target),
        );

        assert_eq!(channel.add_block(single_batch(1), 0).unwrap(), ChannelAddOutcome::Accepted);
        let accepted = channel.accepted_rlp.clone();
        let outcome = channel.add_block(single_batch(2), 1).unwrap();
        assert_eq!(
            outcome,
            ChannelAddOutcome::Rejected(super::ChannelFullReason::CompressedOutput),
            "one_block_size={one_block_size}, two_block_size={two_block_size}, target={target}"
        );

        assert_eq!(channel.accepted_rlp, accepted);
        assert_eq!(channel.compressed_rlp_bytes, channel.accepted_rlp.len());
    }

    #[test]
    fn span_channel_accepts_exact_compression_target() {
        let target = exact_compressed_size(1);
        let mut channel = SpanChannel::new(
            ChannelId::default(),
            Arc::new(RollupConfig::default()),
            &span_config(target),
        );

        assert_eq!(
            channel.add_block(single_batch(1), 0).unwrap(),
            ChannelAddOutcome::AcceptedAndFull
        );
        assert!(!channel.accepted_rlp.is_empty());
    }

    #[test]
    fn span_channel_accepts_oversized_first_block() {
        let target = exact_compressed_size(1) - 1;
        let mut channel = SpanChannel::new(
            ChannelId::default(),
            Arc::new(RollupConfig::default()),
            &span_config(target),
        );

        assert_eq!(
            channel.add_block(single_batch(1), 0).unwrap(),
            ChannelAddOutcome::AcceptedAndFull
        );
        assert!(!channel.accepted_rlp.is_empty());
    }

    #[test]
    fn span_channel_rejects_first_block_over_rlp_limit() {
        let rollup_config = Arc::new(RollupConfig::default());
        let oversized_input = vec![0; rollup_config.max_rlp_bytes_per_channel(1) as usize].into();
        let signed = TxLegacy { input: oversized_input, ..Default::default() }
            .into_signed(Signature::test_signature());
        let mut encoded_tx = Vec::new();
        TxEnvelope::Legacy(signed).encode_2718(&mut encoded_tx);
        let batch = SingleBatch {
            epoch_num: 1,
            timestamp: 1,
            transactions: vec![Bytes::from(encoded_tx)],
            ..Default::default()
        };
        let mut channel = SpanChannel::new(ChannelId::default(), rollup_config, &span_config(1024));

        assert_eq!(
            channel.add_block(batch, 0).unwrap(),
            ChannelAddOutcome::Rejected(super::ChannelFullReason::RlpInput)
        );
        assert!(channel.accepted_rlp.is_empty());
    }
}
