use std::sync::Arc;

use alloy_primitives::{Address, Bytes};
use base_batcher_encoder::{
    BatchEncoder, BatchPipeline, EncoderConfig, ReorgError, StepError, StepResult,
};
use base_comp::{
    BatchComposeError, BatchComposer, BrotliCompressor, BrotliLevel, ChannelOut, ChannelOutError,
};
use base_consensus_genesis::RollupConfig;
use base_protocol::{
    Batch, ChannelId, DERIVATION_VERSION_0, Frame, MAX_FRAME_LEN, SingleBatch, SpanBatch,
    SpanBatchError,
};
use tracing::info;

use crate::{Action, L1Miner, L2BlockProvider, PendingTx};

/// Selects whether the batcher encodes blocks as individual [`SingleBatch`]es
/// or groups them into a single [`SpanBatch`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BatchType {
    /// Each L2 block is encoded as a separate [`SingleBatch`] (default).
    #[default]
    Single,
    /// All L2 blocks in one cycle are grouped into a single [`SpanBatch`].
    Span,
}

/// Selects the kind of invalid frame data submitted by
/// [`Batcher::submit_garbage_frames`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GarbageKind {
    /// 200 bytes of `0xDE` — random-looking, no valid structure.
    Random,
    /// Valid `DERIVATION_VERSION_0` prefix + 16-byte channel ID, then EOF.
    Truncated,
    /// Valid frame header (channel ID + frame num + length), invalid RLP body.
    MalformedRlp,
    /// Valid frame header, brotli magic byte `0x00`, then random bytes.
    InvalidBrotli,
}

/// Configuration for the [`Batcher`] actor.
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// Address of the batcher account. Used as the `from` field on L1
    /// transactions so the derivation pipeline can filter by sender.
    pub batcher_address: Address,
    /// Batch inbox address on L1. Used as the `to` field on L1 transactions.
    pub inbox_address: Address,
    /// Whether to encode blocks as [`SingleBatch`]es or a [`SpanBatch`].
    pub batch_type: BatchType,
    /// Encoder configuration forwarded to [`BatchEncoder`].
    pub encoder: EncoderConfig,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        Self {
            batcher_address: Address::repeat_byte(0xBA),
            inbox_address: Address::repeat_byte(0xCA),
            batch_type: BatchType::Single,
            encoder: EncoderConfig::default(),
        }
    }
}

/// Errors returned by [`Batcher::advance`].
#[derive(Debug, thiserror::Error)]
pub enum BatcherError {
    /// The L2 source was exhausted before any blocks could be batched.
    #[error("no L2 blocks available to batch")]
    NoBlocks,
    /// Conversion from L2 block to single batch failed.
    #[error("batch compose error: {0}")]
    Compose(#[from] BatchComposeError),
    /// An L2 reorg was detected during block ingestion.
    #[error("reorg: {0}")]
    Reorg(#[from] ReorgError),
    /// Channel encoding or compression failed.
    #[error("channel error: {0}")]
    Channel(#[from] ChannelOutError),
    /// Span batch construction failed.
    #[error("span batch error: {0}")]
    SpanBatch(#[from] SpanBatchError),
}

impl From<StepError> for BatcherError {
    fn from(e: StepError) -> Self {
        match e {
            StepError::CompositionFailed { source, .. } => Self::Compose(source),
        }
    }
}

/// Batcher actor for action tests.
///
/// `Batcher` drains [`OpBlock`]s from an [`L2BlockProvider`], encodes each
/// one as a [`SingleBatch`] via [`BatchEncoder`] or groups them into a
/// [`SpanBatch`] depending on [`BatcherConfig::batch_type`], compresses
/// batches into a channel, and submits the resulting frame data to the
/// [`L1Miner`] as a [`PendingTx`].
///
/// A single call to [`advance`] (or [`Action::act`]) runs one full encode
/// cycle: drain all available L2 blocks → encode → flush → submit to L1.
/// Callers then mine an L1 block to include the submitted transactions.
///
/// [`advance`]: Batcher::advance
/// [`OpBlock`]: base_alloy_consensus::OpBlock
#[derive(Debug)]
pub struct Batcher<'a, S: L2BlockProvider> {
    l1_miner: &'a mut L1Miner,
    l2_source: S,
    pipeline: BatchEncoder,
    rollup_config: Arc<RollupConfig>,
    config: BatcherConfig,
}

impl<'a, S: L2BlockProvider> Batcher<'a, S> {
    /// Create a new [`Batcher`].
    ///
    /// The batcher borrows `l1_miner` mutably so it can submit transactions
    /// directly. `l2_source` is moved in so the batcher owns the block queue.
    pub fn new(
        l1_miner: &'a mut L1Miner,
        l2_source: S,
        rollup_config: &RollupConfig,
        config: BatcherConfig,
    ) -> Self {
        let rollup_config = Arc::new(rollup_config.clone());
        let pipeline = BatchEncoder::new(Arc::clone(&rollup_config), config.encoder.clone());
        Self { l1_miner, l2_source, pipeline, rollup_config, config }
    }

    /// Drain all available L2 blocks and encode them into frames without
    /// submitting to L1.
    ///
    /// For [`BatchType::Single`], blocks are fed through [`BatchEncoder`].
    /// For [`BatchType::Span`], all blocks are collected into one [`SpanBatch`]
    /// and encoded via a fresh [`ChannelOut`].
    ///
    /// Returns the encoded frames so callers can inspect or submit them
    /// selectively. Use [`submit_frames`] to submit a subset of frames to
    /// the L1 miner.
    ///
    /// [`submit_frames`]: Batcher::submit_frames
    ///
    /// # Errors
    ///
    /// Returns [`BatcherError::NoBlocks`] if the L2 source is empty.
    /// Returns [`BatcherError::Compose`] if the first tx is not a valid deposit.
    /// Returns [`BatcherError::Reorg`] if a block parent hash mismatch is detected.
    /// Returns [`BatcherError::Channel`] if channel encoding fails.
    /// Returns [`BatcherError::SpanBatch`] if span batch construction fails.
    pub fn encode_frames(&mut self) -> Result<Vec<Arc<Frame>>, BatcherError> {
        match self.config.batch_type {
            BatchType::Single => self.encode_single_frames(),
            BatchType::Span => self.encode_span_frames(),
        }
    }

    fn encode_single_frames(&mut self) -> Result<Vec<Arc<Frame>>, BatcherError> {
        let mut block_count = 0u64;

        while let Some(block) = self.l2_source.next_block() {
            self.pipeline.add_block(block).map_err(|(e, _)| e)?;
            block_count += 1;
        }

        if block_count == 0 {
            return Err(BatcherError::NoBlocks);
        }

        // Step until all blocks are encoded into the current channel.
        loop {
            match self.pipeline.step()? {
                StepResult::Idle => break,
                StepResult::BlockEncoded | StepResult::ChannelClosed => {}
            }
        }

        // Force-close the current channel by advancing the L1 head past the timeout.
        self.pipeline.advance_l1_head(u64::MAX);

        let mut frames = Vec::new();
        while let Some(sub) = self.pipeline.next_submission() {
            frames.extend(sub.frames);
        }

        info!(blocks = block_count, frames = frames.len(), "batcher encoded single frames");
        Ok(frames)
    }

    fn encode_span_frames(&mut self) -> Result<Vec<Arc<Frame>>, BatcherError> {
        let mut singles: Vec<(SingleBatch, u64)> = Vec::new();

        while let Some(block) = self.l2_source.next_block() {
            let (single, l1_info) = BatchComposer::block_to_single_batch(&block)?;
            singles.push((single, l1_info.sequence_number()));
        }

        if singles.is_empty() {
            return Err(BatcherError::NoBlocks);
        }

        let mut span_batch =
            SpanBatch { chain_id: self.rollup_config.l2_chain_id.id(), ..Default::default() };
        for (single, seq_num) in singles {
            span_batch.append_singular_batch(single, seq_num)?;
        }

        // Encode the span batch directly via ChannelOut.
        let compressor = BrotliCompressor::new(BrotliLevel::Brotli10);
        let mut channel_out =
            ChannelOut::new(ChannelId::default(), Arc::clone(&self.rollup_config), compressor);
        channel_out.add_batch(Batch::Span(span_batch))?;
        channel_out.flush()?;
        channel_out.close();

        let mut frames = Vec::new();
        while channel_out.ready_bytes() > 0 {
            frames.push(Arc::new(channel_out.output_frame(MAX_FRAME_LEN)?));
        }

        info!(frames = frames.len(), "batcher encoded span frames");
        Ok(frames)
    }

    /// Submit the given frames to the L1 miner as pending transactions.
    ///
    /// Each frame is submitted as a separate [`PendingTx`].
    pub fn submit_frames(&mut self, frames: &[Arc<Frame>]) {
        for frame in frames {
            let encoded = frame.encode();
            let mut input = Vec::with_capacity(1 + encoded.len());
            input.push(DERIVATION_VERSION_0);
            input.extend_from_slice(&encoded);

            self.l1_miner.submit_tx(PendingTx {
                from: self.config.batcher_address,
                to: self.config.inbox_address,
                input: Bytes::from(input),
            });
        }
        info!(frames = frames.len(), "batcher submitted frames to L1");
    }

    /// Submit intentionally malformed frame data to L1.
    ///
    /// These garbage frames should be silently dropped by the derivation
    /// pipeline. Use them to test that invalid data does not corrupt channel
    /// state or advance the safe head.
    pub fn submit_garbage_frames(&mut self, kind: GarbageKind) {
        let input = match kind {
            GarbageKind::Random => {
                // 200 bytes of 0xDE — no valid structure.
                Bytes::from(vec![0xDE_u8; 200])
            }
            GarbageKind::Truncated => {
                // DERIVATION_VERSION_0 prefix + 16-byte channel ID, then EOF.
                let mut v = vec![DERIVATION_VERSION_0];
                v.extend_from_slice(&[0u8; 16]); // channel ID
                Bytes::from(v)
            }
            GarbageKind::MalformedRlp => {
                // Valid frame header bytes then invalid RLP body.
                // Header: channel_id(16) + frame_number(2) + frame_data_length(4)
                // Body: 0xFF bytes (invalid RLP for a byte-string context).
                let mut v = vec![DERIVATION_VERSION_0];
                v.extend_from_slice(&[0u8; 16]); // channel ID
                v.extend_from_slice(&[0u8, 0u8]); // frame number = 0
                v.extend_from_slice(&[0u8, 0u8, 0u8, 10u8]); // frame data length = 10
                v.extend_from_slice(&[0xFFu8; 10]); // invalid RLP
                v.push(0u8); // is_last = false
                Bytes::from(v)
            }
            GarbageKind::InvalidBrotli => {
                // Valid frame header, brotli magic `0x00`, then random bytes.
                let mut v = vec![DERIVATION_VERSION_0];
                v.extend_from_slice(&[0u8; 16]); // channel ID
                v.extend_from_slice(&[0u8, 0u8]); // frame number = 0
                v.extend_from_slice(&[0u8, 0u8, 0u8, 20u8]); // frame data length = 20
                v.push(0x00); // brotli version prefix
                v.extend_from_slice(&[0xDE_u8; 19]); // random body
                v.push(1u8); // is_last = true
                Bytes::from(v)
            }
        };

        self.l1_miner.submit_tx(PendingTx {
            from: self.config.batcher_address,
            to: self.config.inbox_address,
            input,
        });
        info!(kind = ?kind, "batcher submitted garbage frame");
    }

    /// Encode and submit all frames in one step (convenience wrapper).
    ///
    /// Equivalent to calling [`encode_frames`] followed by [`submit_frames`]
    /// with all produced frames.
    ///
    /// [`encode_frames`]: Batcher::encode_frames
    /// [`submit_frames`]: Batcher::submit_frames
    pub fn advance(&mut self) -> Result<Vec<Arc<Frame>>, BatcherError> {
        let frames = self.encode_frames()?;
        self.submit_frames(&frames);
        Ok(frames)
    }
}

impl<S: L2BlockProvider> Action for Batcher<'_, S> {
    type Output = Vec<Arc<Frame>>;
    type Error = BatcherError;

    fn act(&mut self) -> Result<Vec<Arc<Frame>>, BatcherError> {
        self.advance()
    }
}
