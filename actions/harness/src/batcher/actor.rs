use std::sync::Arc;

use alloy_eips::eip4844::Blob;
use alloy_primitives::{Address, B256, Bytes};
use base_batcher_encoder::{
    BatchEncoder, BatchPipeline, BatchType, EncoderConfig, ReorgError, StepError, StepResult,
};
use base_blobs::BlobEncoder;
use base_comp::BatchComposeError;
use base_consensus_genesis::RollupConfig;
use base_protocol::{DERIVATION_VERSION_0, Frame};
use tracing::info;

use crate::{Action, L1Miner, L2BlockProvider, PendingTx};

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
    /// Frame data without the `DERIVATION_VERSION_0` prefix byte.
    /// The derivation pipeline checks for the version byte first and ignores
    /// transactions that don't start with it.
    StripVersion,
    /// Valid `DERIVATION_VERSION_0` prefix + complete frame, then appended garbage bytes.
    /// The extra trailing bytes should be silently dropped by the frame parser.
    DirtyAppend,
}

/// Configuration for the [`Batcher`] actor.
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// Address of the batcher account. Used as the `from` field on L1
    /// transactions so the derivation pipeline can filter by sender.
    pub batcher_address: Address,
    /// Batch inbox address on L1. Used as the `to` field on L1 transactions.
    pub inbox_address: Address,
    /// Whether to encode blocks as [`SingleBatch`](base_protocol::SingleBatch)es
    /// or a [`SpanBatch`](base_protocol::SpanBatch).
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
/// one as a [`SingleBatch`] via [`BatchEncoder`] (or accumulates them into a
/// [`SpanBatch`] when configured for span mode), compresses batches into a
/// channel, and buffers the resulting frame data internally.
///
/// Call [`flush`] to drain the pending transactions and blobs into an
/// [`L1Miner`].
///
/// A single call to [`advance`] (or [`Action::act`]) runs one full encode
/// cycle: drain all available L2 blocks → encode → buffer submissions.
/// Callers then call [`flush`] and mine an L1 block to include the submitted
/// transactions.
///
/// [`advance`]: Batcher::advance
/// [`flush`]: Batcher::flush
/// [`OpBlock`]: base_alloy_consensus::OpBlock
#[derive(Debug)]
pub struct Batcher<S: L2BlockProvider> {
    l2_source: S,
    pipeline: BatchEncoder,
    config: BatcherConfig,
    pending_txs: Vec<PendingTx>,
    pending_blobs: Vec<(B256, Box<Blob>)>,
}

impl<S: L2BlockProvider> Batcher<S> {
    /// Create a new [`Batcher`].
    ///
    /// Pending transactions and blobs are buffered internally. Call [`flush`]
    /// to drain them into an [`L1Miner`].
    ///
    /// [`flush`]: Batcher::flush
    pub fn new(l2_source: S, rollup_config: &RollupConfig, config: BatcherConfig) -> Self {
        let rollup_config = Arc::new(rollup_config.clone());
        let mut encoder_config = config.encoder.clone();
        encoder_config.batch_type = config.batch_type;
        let pipeline = BatchEncoder::new(rollup_config, encoder_config);
        Self { l2_source, pipeline, config, pending_txs: Vec::new(), pending_blobs: Vec::new() }
    }

    /// Drain all available L2 blocks and encode them into frames without
    /// submitting to L1.
    ///
    /// Blocks are fed through [`BatchEncoder`], which handles both
    /// [`SingleBatch`](base_protocol::SingleBatch) and
    /// [`SpanBatch`](base_protocol::SpanBatch) modes via its internal
    /// [`EncoderConfig`].
    ///
    /// Returns the encoded frames so callers can inspect or submit them
    /// selectively. Use [`submit_frames`] to submit a subset of frames to
    /// the pending buffer.
    ///
    /// [`submit_frames`]: Batcher::submit_frames
    ///
    /// # Errors
    ///
    /// Returns [`BatcherError::NoBlocks`] if the L2 source is empty.
    /// Returns [`BatcherError::Compose`] if the first tx is not a valid deposit.
    /// Returns [`BatcherError::Reorg`] if a block parent hash mismatch is detected.
    pub fn encode_frames(&mut self) -> Result<Vec<Arc<Frame>>, BatcherError> {
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

        // Intentional test-only force-flush: advance the L1 head past the
        // channel timeout so the encoder closes the channel immediately.
        self.pipeline.advance_l1_head(u64::MAX);

        let mut frames = Vec::new();
        while let Some(sub) = self.pipeline.next_submission() {
            frames.extend(sub.frames);
        }

        info!(blocks = block_count, frames = frames.len(), "batcher encoded frames");
        Ok(frames)
    }

    /// Buffer the given frames as pending L1 transactions.
    ///
    /// Each frame is buffered as a separate [`PendingTx`]. Call [`flush`] to
    /// drain them into an [`L1Miner`].
    ///
    /// [`flush`]: Batcher::flush
    pub fn submit_frames(&mut self, frames: &[Arc<Frame>]) {
        for frame in frames {
            let encoded = frame.encode();
            let mut input = Vec::with_capacity(1 + encoded.len());
            input.push(DERIVATION_VERSION_0);
            input.extend_from_slice(&encoded);

            self.pending_txs.push(PendingTx {
                from: self.config.batcher_address,
                to: self.config.inbox_address,
                input: Bytes::from(input),
            });
        }
        info!(frames = frames.len(), "batcher buffered frames");
    }

    /// Buffer the given frames as EIP-4844 blob sidecars.
    ///
    /// Each frame is encoded into one blob using [`BlobEncoder::encode_frames`].
    /// Call [`flush`] to drain them into an [`L1Miner`].
    ///
    /// [`flush`]: Batcher::flush
    pub fn submit_blob_frames(&mut self, frames: &[Arc<Frame>]) {
        let blobs =
            BlobEncoder::encode_frames(frames).expect("frame data fits within blob capacity");
        for blob in blobs {
            self.pending_blobs.push((B256::ZERO, Box::new(blob)));
        }
        info!(frames = frames.len(), "batcher buffered frames as blobs");
    }

    /// Drain all pending transactions and blobs into the given [`L1Miner`].
    pub fn flush(&mut self, l1: &mut L1Miner) {
        for tx in self.pending_txs.drain(..) {
            l1.submit_tx(tx);
        }
        for (hash, blob) in self.pending_blobs.drain(..) {
            l1.enqueue_blob(hash, blob);
        }
    }

    /// Encode and submit all frames as blobs in one step.
    ///
    /// Equivalent to calling [`encode_frames`] followed by [`submit_blob_frames`].
    ///
    /// [`encode_frames`]: Batcher::encode_frames
    /// [`submit_blob_frames`]: Batcher::submit_blob_frames
    pub fn advance_blob(&mut self) -> Result<Vec<Arc<Frame>>, BatcherError> {
        let frames = self.encode_frames()?;
        self.submit_blob_frames(&frames);
        Ok(frames)
    }

    /// Buffer intentionally malformed frame data as a pending L1 transaction.
    ///
    /// These garbage frames should be silently dropped by the derivation
    /// pipeline. Use them to test that invalid data does not corrupt channel
    /// state or advance the safe head.
    ///
    /// Call [`flush`] to drain pending transactions into an [`L1Miner`].
    ///
    /// [`flush`]: Batcher::flush
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
            GarbageKind::StripVersion => {
                // Frame data without the DERIVATION_VERSION_0 prefix.
                // Starts directly with a channel ID — no version byte, so the
                // derivation pipeline discards the tx before parsing any frames.
                let mut v = vec![];
                v.extend_from_slice(&[0u8; 16]); // channel ID (no version prefix)
                v.extend_from_slice(&[0u8, 0u8]); // frame number = 0
                v.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]); // frame data length = 0
                v.push(1u8); // is_last = true
                Bytes::from(v)
            }
            GarbageKind::DirtyAppend => {
                // Valid DERIVATION_VERSION_0 + a minimal complete frame, then 50 garbage
                // bytes appended. The extra trailing bytes follow the valid frame.
                let mut v = vec![DERIVATION_VERSION_0];
                v.extend_from_slice(&[0u8; 16]); // channel ID
                v.extend_from_slice(&[0u8, 0u8]); // frame number = 0
                v.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]); // frame data length = 0
                v.push(1u8); // is_last = true
                v.extend_from_slice(&[0xBE_u8; 50]); // appended garbage
                Bytes::from(v)
            }
        };

        self.pending_txs.push(PendingTx {
            from: self.config.batcher_address,
            to: self.config.inbox_address,
            input,
        });
        info!(kind = ?kind, "batcher buffered garbage frame");
    }

    /// Return the estimated number of unsubmitted data bytes in the encoding pipeline.
    ///
    /// Delegates to [`BatchEncoder::da_backlog_bytes`]. Useful for testing the
    /// throttle controller's backlog detection.
    pub fn da_backlog_bytes(&self) -> u64 {
        self.pipeline.da_backlog_bytes()
    }

    /// Encode and buffer all frames in one step (convenience wrapper).
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

impl<S: L2BlockProvider> Action for Batcher<S> {
    type Output = Vec<Arc<Frame>>;
    type Error = BatcherError;

    fn act(&mut self) -> Result<Vec<Arc<Frame>>, BatcherError> {
        self.advance()
    }
}
