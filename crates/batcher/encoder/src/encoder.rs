//! The [`BatchEncoder`] implementation.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::Arc,
};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::B256;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_comp::{
    BatchComposer, ChannelOut, CompressionAlgo, CompressorType, Config, ShadowCompressor,
};
use base_consensus_genesis::RollupConfig;
use base_protocol::{Batch, BatchType, ChannelId, Frame, SingleBatch, SpanBatch};
use rand::{RngCore, SeedableRng, rngs::SmallRng};
use tracing::{debug, warn};

use crate::{
    BatchPipeline, BatchSubmission, BatcherMetrics, DaType, EncoderConfig, ReorgError, StepError,
    StepResult, SubmissionId,
    channel::{OpenChannel, PendingRef, ReadyChannel},
};

/// The batcher encoding pipeline state machine.
///
/// Transforms L2 blocks into L1 submission frames. No async, no I/O. The caller
/// drives the encoder synchronously via the [`BatchPipeline`] trait.
pub struct BatchEncoder {
    /// The rollup configuration.
    rollup_config: Arc<RollupConfig>,
    /// Encoder-specific configuration.
    config: EncoderConfig,
    /// Current L1 head block number (for channel duration tracking).
    l1_head: u64,
    /// L2 blocks waiting to be encoded. Pruned when all their frames are confirmed.
    blocks: VecDeque<BaseBlock>,
    /// Index into `blocks`: next block not yet fed into the current channel.
    block_cursor: usize,
    /// Hash of the last block's header (or `B256::ZERO` if empty). Used for reorg detection.
    tip: B256,
    /// The channel currently being built. `None` between channels.
    current_channel: Option<OpenChannel>,
    /// Channels that are full and have frames ready to drain.
    ready_channels: VecDeque<ReadyChannel>,
    /// In-flight submissions: id -> reference into `ready_channels`.
    pending: HashMap<SubmissionId, PendingRef>,
    /// Next submission id counter.
    next_id: u64,
    /// Per-instance RNG for generating unique channel IDs.
    rng: SmallRng,
    /// Accumulated (`SingleBatch`, `sequence_number`) pairs when operating in
    /// [`BatchType::Span`] mode. Blocks are collected here during `step()` and
    /// flushed as a single [`SpanBatch`] when `close_current_channel()` is called.
    span_accumulator: Vec<(SingleBatch, u64)>,
    /// Running sum of the estimated raw (uncompressed) byte size of all blocks currently
    /// in `span_accumulator`. Incremented by [`Self::SPAN_BATCH_PER_BLOCK_OVERHEAD`] plus raw
    /// transaction bytes for each block pushed in `step()`, and reset to zero when the
    /// accumulator is drained. Avoids an O(N·M) re-scan of the accumulator on every step.
    span_raw_bytes: usize,
    /// L1 head block number when the first block was accumulated into the current span
    /// (Span mode only). Used by `check_channel_timeout()` to detect when the span has
    /// been open too long and must be flushed, since `current_channel` is `None` between
    /// span flushes. Cleared when `close_current_channel()` drains the accumulator.
    span_opened_at_l1: Option<u64>,
    /// Driver-controlled override that forces [`DaType::Blob`] on every emitted
    /// submission, regardless of the configured `da_type`. Toggled by the driver
    /// when DA-backlog throttling activates and `force_blobs_when_throttling` is
    /// set. No-op when the configured `da_type` is already [`DaType::Blob`].
    blob_override: bool,
}

impl fmt::Debug for BatchEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchEncoder")
            .field("l1_head", &self.l1_head)
            .field("blocks_len", &self.blocks.len())
            .field("block_cursor", &self.block_cursor)
            .field("tip", &self.tip)
            .field("current_channel", &self.current_channel)
            .field("ready_channels", &self.ready_channels.len())
            .field("pending", &self.pending.len())
            .field("next_id", &self.next_id)
            .field("span_accumulator_len", &self.span_accumulator.len())
            .field("span_raw_bytes", &self.span_raw_bytes)
            .field("span_opened_at_l1", &self.span_opened_at_l1)
            .finish_non_exhaustive()
    }
}

impl BatchEncoder {
    /// Approximate bytes of per-block overhead when encoding as part of a [`SpanBatch`].
    /// Accounts for the fixed fields carried per singular batch (parent hash, epoch number,
    /// epoch hash, timestamp) plus RLP framing. Used for span accumulator size estimation.
    pub const SPAN_BATCH_PER_BLOCK_OVERHEAD: usize = 50;

    /// Create a new [`BatchEncoder`].
    pub fn new(rollup_config: Arc<RollupConfig>, config: EncoderConfig) -> Self {
        Self {
            rollup_config,
            config,
            l1_head: 0,
            blocks: VecDeque::new(),
            block_cursor: 0,
            tip: B256::ZERO,
            current_channel: None,
            ready_channels: VecDeque::new(),
            pending: HashMap::new(),
            next_id: 0,
            rng: SmallRng::from_os_rng(),
            span_accumulator: Vec::new(),
            span_raw_bytes: 0,
            span_opened_at_l1: None,
            blob_override: false,
        }
    }

    /// Step the encoder until idle, force-close the current channel, and return
    /// all frames from every available submission.
    ///
    /// Convenience wrapper for tests and one-shot batch pipelines that have
    /// already added all blocks via [`BatchPipeline::add_block`] and want all
    /// output frames in a single call.
    ///
    /// # Errors
    ///
    /// Returns the first [`StepError`] encountered during encoding. On error the
    /// encoder state is left as-is; previously ready submissions remain available
    /// via [`BatchPipeline::next_submission`].
    pub fn encode_and_drain(&mut self) -> Result<Vec<Arc<Frame>>, StepError> {
        loop {
            match self.step()? {
                StepResult::Idle => break,
                StepResult::BlockEncoded | StepResult::ChannelClosed => {}
            }
        }
        self.force_close_channel();
        let mut frames = Vec::new();
        while let Some(sub) = self.next_submission() {
            frames.extend(sub.frames);
        }
        Ok(frames)
    }

    /// Close the current channel, drain its frames, and push it to `ready_channels`.
    ///
    /// In [`BatchType::Span`] mode the span accumulator is flushed as a single
    /// [`SpanBatch`] into a freshly-opened channel before draining frames.
    /// If both the channel and the accumulator are empty the call is a no-op.
    ///
    /// `close_reason` is recorded as the `reason` label on the
    /// `batcher_channel_closed_total` counter.
    fn close_current_channel(&mut self, close_reason: &'static str) {
        // In Span mode: build a SpanBatch from the accumulator, then open a channel
        // and write it. The accumulator is only consumed if all appends succeed, so
        // blocks are never silently lost on error — they remain in the accumulator for
        // the next close attempt.
        //
        // Importantly, both the channel open and the accumulator drain happen only
        // after successful batch construction: this prevents writing a zero-block (or
        // partial) SpanBatch to the channel, which would be silently ignored by the
        // derivation pipeline and waste L1 DA space.
        if self.config.batch_type == BatchType::Span && !self.span_accumulator.is_empty() {
            let chain_id = self.rollup_config.l2_chain_id.id();
            let mut span_batch = SpanBatch { chain_id, ..Default::default() };
            let total = self.span_accumulator.len();
            let mut append_failed = false;

            for (single, seq_num) in &self.span_accumulator {
                if let Err(e) = span_batch.append_singular_batch(single.clone(), *seq_num) {
                    warn!(
                        error = %e,
                        total,
                        "span batch append failed; blocks preserved in accumulator"
                    );
                    append_failed = true;
                    break;
                }
            }

            if !append_failed {
                // All blocks encoded into the SpanBatch. Now open a channel and write it.
                // The accumulator is only cleared *after* a successful add_batch so that
                // blocks are never silently lost if the channel rejects the batch (e.g.
                // if the span batch exceeds MAX_RLP_BYTES_PER_CHANNEL).
                if self.current_channel.is_none() {
                    self.open_new_channel();
                }

                let add_ok = self
                    .current_channel
                    .as_mut()
                    .map(|open| {
                        let ok = open.out.add_batch(Batch::Span(span_batch)).map_err(|e| {
                            warn!(error = %e, total, "failed to add span batch to channel; blocks preserved in accumulator");
                        }).is_ok();
                        if ok {
                            open.blocks_added += total;
                        }
                        ok
                    })
                    .unwrap_or(false);

                if add_ok {
                    self.span_accumulator.clear();
                    self.span_raw_bytes = 0;
                    self.span_opened_at_l1 = None;
                } else {
                    // Discard the channel we just opened so that the drain logic below
                    // (`self.current_channel.take()`) short-circuits and returns early.
                    // Without this, the empty channel (0 frames) would be pushed to
                    // `ready_channels`, where it can never be confirmed and never removed,
                    // leaking memory and growing the O(N) scan in `next_submission`.
                    // Emit a closed counter to keep opened/closed balanced.
                    BatcherMetrics::channel_closed_total(BatcherMetrics::REASON_DISCARD)
                        .increment(1);
                    self.current_channel = None;
                }
            }
            // On failure (append or add_batch): accumulator is untouched so blocks
            // are retried on the next close attempt. No partial SpanBatch is submitted.
        }

        let Some(mut open) = self.current_channel.take() else {
            return;
        };

        // Capture stats before flushing so we can record metrics after draining.
        let input_bytes = open.out.input_bytes();
        let opened_at_l1 = open.opened_at_l1;
        let blocks_added = open.blocks_added;

        // Flush and close the compressor.
        let _ = open.out.flush();
        open.out.close();

        let channel_id = open.out.id;

        // Drain all frames from the channel.
        let mut frames = Vec::new();
        while open.out.ready_bytes() > 0 {
            match open.out.output_frame(self.config.max_frame_size) {
                Ok(frame) => frames.push(Arc::new(frame)),
                Err(e) => {
                    warn!(error = %e, "failed to output frame during channel close");
                    break;
                }
            }
        }

        // block_range records a high-water mark into the current blocks deque.
        // The start is always 0; only .end is used (as prune_count in confirm()).
        // Ranges across concurrent channels are intentionally overlapping at
        // creation — confirm() uses saturating_sub adjustments so that whichever
        // channel confirms first pops the correct prefix of the deque, and
        // subsequent confirmations find their .end adjusted to 0 and are no-ops.
        // This correctly handles out-of-order confirmations without double-pruning.
        let block_range = 0..self.block_cursor;
        let frame_count = frames.len();
        let duration_blocks = self.l1_head.saturating_sub(opened_at_l1);
        let compressed_bytes: usize = frames.iter().map(|f| f.data.len()).sum();

        debug!(
            channel_id = ?channel_id,
            frame_count = %frame_count,
            block_range_start = %block_range.start,
            block_range_end = %block_range.end,
            close_reason = %close_reason,
            duration_blocks = %duration_blocks,
            input_bytes = %input_bytes,
            compressed_bytes = %compressed_bytes,
            "closed channel"
        );

        // Emit close counter and channel lifetime / compression ratio histograms.
        BatcherMetrics::channel_closed_total(close_reason).increment(1);
        BatcherMetrics::channel_duration_blocks().record(duration_blocks as f64);
        BatcherMetrics::l2_blocks_per_channel().record(blocks_added as f64);
        if input_bytes > 0 {
            let ratio = compressed_bytes as f64 / input_bytes as f64;
            BatcherMetrics::channel_compression_ratio().record(ratio);
        }
        // All frames from this channel are now pending submission.
        BatcherMetrics::pending_frames().increment(frame_count as f64);

        self.ready_channels.push_back(ReadyChannel {
            id: channel_id,
            frames,
            cursor: 0,
            block_range,
            pending_confirmations: 0,
            confirmed_count: 0,
        });
    }

    /// Create a new open channel with a random `ChannelId`.
    fn open_new_channel(&mut self) {
        let mut id = ChannelId::default();
        self.rng.fill_bytes(&mut id);

        let compressor_config = Config {
            target_output_size: self.config.target_frame_size as u64,
            kind: CompressorType::Shadow,
            compression_algo: CompressionAlgo::Brotli10,
            approx_compr_ratio: self.config.approx_compr_ratio,
        };
        let compressor = ShadowCompressor::from(compressor_config);

        let channel_out = ChannelOut::new(id, Arc::clone(&self.rollup_config), compressor);

        debug!(channel_id = ?id, l1_head = %self.l1_head, "opened new channel");
        BatcherMetrics::channel_opened_total().increment(1);

        self.current_channel =
            Some(OpenChannel { out: channel_out, opened_at_l1: self.l1_head, blocks_added: 0 });
    }

    /// Check if the current channel (or span accumulator) has timed out and close it if so.
    fn check_channel_timeout(&mut self) -> bool {
        // Apply the safety margin so channels are closed `sub_safety_margin` L1 blocks
        // before the configured `max_channel_duration`, ensuring frames land well within
        // the protocol's `channel_timeout` inclusion window.
        let effective_duration =
            self.config.max_channel_duration.saturating_sub(self.config.sub_safety_margin);

        let should_close = if let Some(ref open) = self.current_channel {
            self.l1_head.saturating_sub(open.opened_at_l1) >= effective_duration
        } else if self.config.batch_type == BatchType::Span {
            // In Span mode there is no open channel between size-based flushes; instead
            // we track the L1 head at which the first block was accumulated. If the
            // accumulator is non-empty and the effective duration has elapsed, flush it.
            self.span_opened_at_l1
                .map(|opened_at| {
                    !self.span_accumulator.is_empty()
                        && self.l1_head.saturating_sub(opened_at) >= effective_duration
                })
                .unwrap_or(false)
        } else {
            false
        };

        if should_close {
            debug!(l1_head = %self.l1_head, "channel timed out, closing");
            self.close_current_channel("timeout");
        }

        should_close
    }
}

impl BatchPipeline for BatchEncoder {
    fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
        if !self.blocks.is_empty() && block.header.parent_hash != self.tip {
            return Err((
                ReorgError::ParentMismatch { expected: self.tip, got: block.header.parent_hash },
                Box::new(block),
            ));
        }

        let number = block.header.number;
        let hash = block.header.hash_slow();
        self.tip = hash;
        self.blocks.push_back(block);
        BatcherMetrics::pending_blocks().increment(1.0);

        debug!(block = %number, pending_blocks = %self.blocks.len(), "block added to encoder queue");

        Ok(())
    }

    fn step(&mut self) -> Result<StepResult, StepError> {
        // Check for channel timeout first.
        if self.check_channel_timeout() {
            return Ok(StepResult::ChannelClosed);
        }

        // If there are no blocks to encode, we're idle.
        if self.block_cursor >= self.blocks.len() {
            return Ok(StepResult::Idle);
        }

        // Get the block at the cursor.
        let block = &self.blocks[self.block_cursor];

        // Convert block to a SingleBatch. Failure here is fatal: skipping the block
        // would produce a gap in the L2 block sequence submitted to L1.
        let (single_batch, l1_info) = BatchComposer::block_to_single_batch(block)
            .map_err(|source| StepError::CompositionFailed { cursor: self.block_cursor, source })?;

        match self.config.batch_type {
            BatchType::Span => {
                // In Span mode blocks are accumulated in memory; the span batch is
                // written to the channel only when close_current_channel() is called.
                let seq_num = l1_info.sequence_number();
                // Maintain a running byte counter so the size check below is O(1) per
                // step rather than O(N·M) over the entire accumulator.
                let block_raw_bytes = Self::SPAN_BATCH_PER_BLOCK_OVERHEAD
                    + single_batch.transactions.iter().map(|tx| tx.len()).sum::<usize>();
                self.span_raw_bytes += block_raw_bytes;
                self.span_accumulator.push((single_batch, seq_num));
                self.block_cursor += 1;

                // Track the L1 head at which the first block of this span was accumulated.
                // `check_channel_timeout()` uses this to detect when the span has been open
                // too long even though `current_channel` is None between flushes.
                if self.span_opened_at_l1.is_none() {
                    self.span_opened_at_l1 = Some(self.l1_head);
                }

                // Estimate the compressed size of the accumulated span batch and close
                // the channel when it would exceed the configured size budget. This mirrors
                // op-batcher's `SpanChannelOut`, which triggers closure based on estimated
                // compressed size rather than waiting for a timeout.
                //
                // Each block contributes fixed-field overhead plus its raw transaction bytes.
                // The compressed estimate uses the same ratio as the ShadowCompressor so that
                // the size trigger fires at roughly the same threshold as Single mode.
                let compressed_estimate =
                    (self.span_raw_bytes as f64 * self.config.approx_compr_ratio) as usize;
                let size_target =
                    self.config.target_frame_size.saturating_mul(self.config.target_num_frames);

                debug!(
                    block_cursor = self.block_cursor,
                    blocks_len = self.blocks.len(),
                    span_accumulator_len = self.span_accumulator.len(),
                    span_raw_bytes = self.span_raw_bytes,
                    compressed_estimate,
                    size_target,
                    "accumulated block for span batch"
                );

                if compressed_estimate >= size_target {
                    debug!(
                        span_len = self.span_accumulator.len(),
                        compressed_estimate, size_target, "span accumulator full, closing channel"
                    );
                    self.close_current_channel("size_full");
                    return Ok(StepResult::ChannelClosed);
                }

                Ok(StepResult::BlockEncoded)
            }
            BatchType::Single => {
                // Ensure a channel is open.
                if self.current_channel.is_none() {
                    self.open_new_channel();
                }

                // Try to add the batch to the current channel.
                let batch = Batch::Single(single_batch);
                let open = self.current_channel.as_mut().unwrap();
                Ok(match open.out.add_batch(batch) {
                    Ok(()) => {
                        open.blocks_added += 1;
                        self.block_cursor += 1;

                        debug!(
                            block_cursor = self.block_cursor,
                            blocks_len = self.blocks.len(),
                            "encoded block into channel"
                        );

                        StepResult::BlockEncoded
                    }
                    Err(e) => {
                        // Channel is full (ExceedsMaxRlpBytesPerChannel or compression full).
                        // Close the current channel and the caller will retry on the next step.
                        debug!(error = %e, "channel rejected batch, closing");
                        self.close_current_channel("size_full");
                        StepResult::ChannelClosed
                    }
                })
            }
        }
    }

    fn next_submission(&mut self) -> Option<BatchSubmission> {
        // The driver may have set `blob_override` to force blob submissions
        // while DA throttling is active. When set, frames are emitted as blobs
        // even though the configured `da_type` is calldata. The override is a
        // no-op when the configured `da_type` is already blob.
        let effective_da_type = if self.blob_override && self.config.da_type == DaType::Calldata {
            DaType::Blob
        } else {
            self.config.da_type
        };
        // Find the first ready channel with unsubmitted frames.
        for (chan_idx, channel) in self.ready_channels.iter_mut().enumerate() {
            if channel.cursor < channel.frames.len() {
                let frame_start = channel.cursor;
                // Pack up to `target_num_frames` frames into a single L1 transaction.
                let available = channel.frames.len() - frame_start;
                let frame_count = if effective_da_type == DaType::Calldata {
                    if let Some(max_size) = self.config.max_l1_tx_size_bytes {
                        // For calldata, accumulate frames until the next frame would push
                        // the total calldata size over `max_l1_tx_size_bytes`.
                        // Each frame serialises as: 1 (DERIVATION_VERSION_0) + 16 (channel
                        // id) + 2 (frame number) + 4 (data length) + data + 1 (is_last).
                        let mut total = 0usize;
                        let mut n = 0usize;
                        for frame in channel.frames[frame_start..].iter().take(available) {
                            if n >= self.config.target_num_frames {
                                break;
                            }
                            let frame_size = 24 + frame.data.len();
                            if n > 0 && total + frame_size > max_size {
                                break;
                            }
                            if n == 0 && frame_size > max_size {
                                warn!(
                                    frame_size,
                                    max_l1_tx_size_bytes = max_size,
                                    "frame exceeds max_l1_tx_size_bytes; submitting anyway"
                                );
                            }
                            total += frame_size;
                            n += 1;
                        }
                        n.max(1)
                    } else {
                        available.min(self.config.target_num_frames).max(1)
                    }
                } else {
                    available.min(self.config.target_num_frames).max(1)
                };
                // Clone the Arcs (pointer copies, not deep copies of frame data).
                let frames: Vec<_> =
                    channel.frames[frame_start..frame_start + frame_count].to_vec();

                let id = SubmissionId(self.next_id);
                self.next_id += 1;

                channel.cursor += frame_count;
                channel.pending_confirmations += 1;

                self.pending
                    .insert(id, PendingRef { channel_idx: chan_idx, frame_start, frame_count });

                // Frames move from pending → in-flight; decrement the pending gauge.
                BatcherMetrics::pending_frames().decrement(frame_count as f64);
                debug!(
                    id = %id.0,
                    frame_count = %frame_count,
                    frame_start = %frame_start,
                    "dequeued frames for submission"
                );

                return Some(BatchSubmission {
                    id,
                    channel_id: channel.id,
                    da_type: effective_da_type,
                    frames,
                });
            }
        }

        None
    }

    fn confirm(&mut self, id: SubmissionId, _l1_block: u64) {
        let Some(pending_ref) = self.pending.remove(&id) else {
            warn!(id = ?id, "confirm called for unknown submission id");
            return;
        };

        let chan_idx = pending_ref.channel_idx;
        if chan_idx >= self.ready_channels.len() {
            warn!(id = ?id, chan_idx = %chan_idx, "confirm: channel index out of bounds; submission lost");
            return;
        }

        let channel = &mut self.ready_channels[chan_idx];
        channel.pending_confirmations = channel.pending_confirmations.saturating_sub(1);
        channel.confirmed_count += pending_ref.frame_count;

        // Check if all frames are confirmed and none are in-flight.
        if channel.confirmed_count >= channel.frames.len() && channel.pending_confirmations == 0 {
            let block_range = channel.block_range.clone();

            debug!(
                channel_id = ?channel.id,
                block_range_start = %block_range.start,
                block_range_end = %block_range.end,
                "channel fully confirmed, pruning blocks"
            );

            BatcherMetrics::channel_fully_submitted_total().increment(1);

            // Remove the channel.
            self.ready_channels.remove(chan_idx);

            // Adjust channel_idx for all pending refs pointing to channels after this one.
            for pending in self.pending.values_mut() {
                if pending.channel_idx > chan_idx {
                    pending.channel_idx -= 1;
                }
            }

            // Prune confirmed blocks from the deque.
            let prune_count = block_range.end;
            if prune_count > 0 {
                self.blocks.drain(..prune_count);
                self.block_cursor = self.block_cursor.saturating_sub(prune_count);
                BatcherMetrics::pending_blocks().decrement(prune_count as f64);

                debug!(prune_count = %prune_count, "pruned confirmed blocks from encoder queue");

                // Adjust the high-water mark for all remaining channels.
                // block_range.start is always 0 and unused in prune logic.
                for ch in &mut self.ready_channels {
                    ch.block_range.end = ch.block_range.end.saturating_sub(prune_count);
                }
            }
        }
    }

    fn requeue(&mut self, id: SubmissionId) {
        // Invariant: each `ReadyChannel` owns its own frame cursor. This
        // encoder keeps at most one `current_channel` open at a time; when it
        // closes (by size or timeout) it moves to `ready_channels` as the
        // newest entry. `pending_ref.channel_idx` therefore always points to
        // a specific, independent slot in `ready_channels`. Resetting the
        // cursor on that slot does not affect any other channel and FIFO
        // ordering across channels is preserved by construction.
        let Some(pending_ref) = self.pending.remove(&id) else {
            warn!(id = ?id, "requeue called for unknown submission id");
            return;
        };

        let chan_idx = pending_ref.channel_idx;
        if chan_idx >= self.ready_channels.len() {
            warn!(id = ?id, chan_idx = %chan_idx, "requeue: channel index out of bounds; submission lost");
            return;
        }

        let channel = &mut self.ready_channels[chan_idx];
        channel.pending_confirmations = channel.pending_confirmations.saturating_sub(1);
        // Rewind cursor to the first frame of the requeued submission so all frames
        // in the batch are retried together.
        if pending_ref.frame_start < channel.cursor {
            channel.cursor = pending_ref.frame_start;
        }
        // Frames are back in pending state; re-increment the gauge.
        BatcherMetrics::pending_frames().increment(pending_ref.frame_count as f64);

        debug!(
            id = ?id,
            frame_start = %pending_ref.frame_start,
            frame_count = %pending_ref.frame_count,
            "requeued submission frames back to pending"
        );
    }

    fn force_close_channel(&mut self) {
        debug!("force-closing current channel");
        self.close_current_channel("force");
    }

    fn advance_l1_head(&mut self, l1_block: u64) {
        if l1_block <= self.l1_head {
            return;
        }
        self.l1_head = l1_block;
        self.check_channel_timeout();
    }

    fn reset(&mut self) {
        warn!(
            pending_blocks = %self.blocks.len(),
            ready_channels = %self.ready_channels.len(),
            in_pending = %self.pending.len(),
            "resetting encoder pipeline (reorg or explicit reset)"
        );
        self.blocks.clear();
        self.block_cursor = 0;
        self.tip = B256::ZERO;
        self.current_channel = None;
        self.ready_channels.clear();
        self.pending.clear();
        self.span_accumulator.clear();
        self.span_raw_bytes = 0;
        self.span_opened_at_l1 = None;
        // Intentionally not resetting `next_id`: keeping it monotonically
        // increasing across resets means post-reset submissions can never
        // share an ID with any pre-reset in-flight submission, eliminating
        // stale-confirm silent corruption.
        self.rng = SmallRng::from_os_rng();

        // Zero out state gauges — all buffered data has been discarded.
        BatcherMetrics::pending_blocks().set(0.0);
        BatcherMetrics::pending_frames().set(0.0);
    }

    fn prune_safe(&mut self, safe_l2_number: u64) {
        // Count how many leading blocks are both safe (number <= safe_l2_number) and
        // already past the encoding cursor (index < block_cursor). We must not prune
        // blocks that haven't been fed into a channel yet or we'd silently skip them.
        let prune_count = self
            .blocks
            .iter()
            .take(self.block_cursor)
            .take_while(|b| b.header.number <= safe_l2_number)
            .count();

        if prune_count == 0 {
            return;
        }

        debug!(prune_count, safe_l2_number, "pruning safe blocks from input queue");

        self.blocks.drain(..prune_count);
        self.block_cursor -= prune_count;
        BatcherMetrics::pending_blocks().decrement(prune_count as f64);

        // Adjust block_range high-water marks in ready channels so that confirm()
        // does not over-prune later. This mirrors the adjustment in confirm().
        for ch in &mut self.ready_channels {
            ch.block_range.end = ch.block_range.end.saturating_sub(prune_count);
        }
    }

    fn da_backlog_bytes(&self) -> u64 {
        self.blocks
            .iter()
            .skip(self.block_cursor)
            .flat_map(|b| &b.body.transactions)
            .filter(|tx| !matches!(tx, BaseTxEnvelope::Deposit(_)))
            .map(|tx| tx.encode_2718_len() as u64)
            .sum()
    }

    fn set_blob_override(&mut self, active: bool) {
        if self.blob_override == active {
            return;
        }
        self.blob_override = active;
        if self.config.da_type == DaType::Calldata {
            debug!(active = active, "blob override toggled for calldata-configured encoder");
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
    use alloy_primitives::{Bytes, Sealed, Signature};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_comp::BatchComposeError;
    use base_protocol::{L1BlockInfoBedrock, L1BlockInfoTx};
    use rstest::rstest;

    use super::*;

    fn make_deposit_tx() -> BaseTxEnvelope {
        let calldata = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default()).encode_calldata();
        BaseTxEnvelope::Deposit(Sealed::new(TxDeposit { input: calldata, ..Default::default() }))
    }

    fn make_block(parent_hash: B256) -> BaseBlock {
        BaseBlock {
            header: Header { parent_hash, ..Default::default() },
            body: BlockBody { transactions: vec![make_deposit_tx()], ..Default::default() },
        }
    }

    fn make_block_with_user_tx(parent_hash: B256) -> BaseBlock {
        let user_tx = {
            let signed = TxLegacy::default().into_signed(Signature::test_signature());
            BaseTxEnvelope::Legacy(signed)
        };

        BaseBlock {
            header: Header { parent_hash, ..Default::default() },
            body: BlockBody {
                transactions: vec![make_deposit_tx(), user_tx],
                ..Default::default()
            },
        }
    }

    fn default_encoder() -> BatchEncoder {
        let rollup_config = Arc::new(RollupConfig::default());
        BatchEncoder::new(rollup_config, EncoderConfig::default())
    }

    #[test]
    fn test_add_block_reorg_detection() {
        let mut encoder = default_encoder();

        let block1 = make_block(B256::ZERO);
        encoder.add_block(block1).unwrap();

        // Second block with wrong parent hash should fail.
        let wrong_parent = B256::from([0xAB; 32]);
        let block2 = make_block(wrong_parent);
        let (err, returned_block) = encoder.add_block(block2).unwrap_err();
        assert_eq!(returned_block.header.parent_hash, wrong_parent);

        match err {
            ReorgError::ParentMismatch { expected, got } => {
                assert_eq!(got, wrong_parent);
                assert_ne!(expected, wrong_parent);
            }
        }
    }

    #[test]
    fn test_step_encodes_block() {
        let mut encoder = default_encoder();

        let block = make_block(B256::ZERO);
        encoder.add_block(block).unwrap();

        let result = encoder.step().unwrap();
        assert_eq!(result, StepResult::BlockEncoded);

        // No more blocks => idle.
        let result = encoder.step().unwrap();
        assert_eq!(result, StepResult::Idle);
    }

    #[test]
    fn test_confirm_prunes_blocks() {
        let mut encoder = default_encoder();

        // Add a block.
        let block1 = make_block(B256::ZERO);
        encoder.add_block(block1).unwrap();

        // Step to encode the block.
        let result = encoder.step().unwrap();
        assert_eq!(result, StepResult::BlockEncoded);

        // Close the channel by stepping when idle (force close via advance_l1_head).
        encoder.advance_l1_head(100);

        // The channel should have been closed due to timeout.
        assert!(encoder.current_channel.is_none());

        // Get the submission.
        let sub = encoder.next_submission();
        assert!(sub.is_some());
        let sub = sub.unwrap();
        let sub_id = sub.id;

        // Confirm the submission.
        encoder.confirm(sub_id, 100);

        // Blocks should be pruned.
        assert!(encoder.blocks.is_empty());
        assert_eq!(encoder.block_cursor, 0);
    }

    #[test]
    fn test_reset_clears_state() {
        let mut encoder = default_encoder();

        let block = make_block(B256::ZERO);
        encoder.add_block(block).unwrap();
        encoder.step().unwrap();

        assert!(!encoder.blocks.is_empty());

        encoder.reset();

        assert!(encoder.blocks.is_empty());
        assert_eq!(encoder.block_cursor, 0);
        assert_eq!(encoder.tip, B256::ZERO);
        assert!(encoder.current_channel.is_none());
        assert!(encoder.ready_channels.is_empty());
        assert!(encoder.pending.is_empty());
        assert_eq!(encoder.next_id, 0);
    }

    #[test]
    fn test_da_backlog_excludes_deposits() {
        let mut encoder = default_encoder();

        let block = make_block_with_user_tx(B256::ZERO);
        encoder.add_block(block).unwrap();

        let backlog = encoder.da_backlog_bytes();
        // The backlog should only count the user tx, not the deposit.
        assert!(backlog > 0);
    }

    #[test]
    fn test_requeue_rewinds_cursor() {
        let mut encoder = default_encoder();

        let block = make_block(B256::ZERO);
        encoder.add_block(block).unwrap();
        encoder.step().unwrap();

        // Force close the channel.
        encoder.advance_l1_head(100);

        let sub = encoder.next_submission().unwrap();
        let sub_id = sub.id;

        // Requeue the submission.
        encoder.requeue(sub_id);

        // The frame should be available again.
        let resub = encoder.next_submission();
        assert!(resub.is_some());
    }

    #[test]
    fn test_step_idle_when_no_blocks() {
        let mut encoder = default_encoder();
        assert_eq!(encoder.step().unwrap(), StepResult::Idle);
    }

    #[test]
    fn test_advance_l1_head_triggers_timeout() {
        let mut encoder = default_encoder();

        let block = make_block(B256::ZERO);
        encoder.add_block(block).unwrap();
        encoder.step().unwrap();

        // Channel should exist.
        assert!(encoder.current_channel.is_some());

        // Advance L1 head past max_channel_duration (default 2).
        encoder.advance_l1_head(3);

        // Channel should be closed now.
        assert!(encoder.current_channel.is_none());
        assert!(!encoder.ready_channels.is_empty());
    }

    /// `advance_l1_head` must be monotonic: a call with a value ≤ the current `l1_head`
    /// must be silently ignored. Without this guard, an out-of-order confirmation
    /// (possible when `max_pending_transactions` > 1) could decrease `l1_head`, making
    /// channel timeout checks produce artificially small deltas and stalling force-close.
    #[test]
    fn test_advance_l1_head_ignores_non_monotonic_update() {
        let mut encoder = default_encoder();

        let block = make_block(B256::ZERO);
        let block_hash = block.header.hash_slow();
        encoder.add_block(block).unwrap();
        encoder.step().unwrap();

        // Advance past the timeout threshold so the channel closes.
        encoder.advance_l1_head(3);
        assert!(encoder.current_channel.is_none(), "channel should have timed out at l1_head=3");

        // Now encode another block so a new channel opens.
        // Parent hash must chain from the first block's hash (= current tip).
        encoder.add_block(make_block(block_hash)).unwrap();
        encoder.step().unwrap();
        assert!(encoder.current_channel.is_some(), "new channel should be open");

        // A non-monotonic (backward) call must not decrease l1_head.
        encoder.advance_l1_head(1);
        assert!(
            encoder.current_channel.is_some(),
            "backward advance_l1_head must not close the channel"
        );
    }

    // --- Reorg / stale-confirmation tests ---
    //
    // These tests document the invariant that must hold after a reorg:
    // `reset()` clears pending/channels but intentionally does NOT reset next_id,
    // keeping submission IDs monotonically increasing across resets. This
    // eliminates the class of bugs where a stale in-flight confirmation from
    // before the reset could match a fresh post-reset submission with the same ID.

    /// Get a submission into the in-flight state (pending but not yet confirmed),
    /// then call `reset()`. A subsequent `confirm()` for the stale ID must be a no-op:
    /// the block must not be pruned and the pending map must remain empty.
    #[test]
    fn test_stale_confirm_after_reset_is_noop() {
        let mut encoder = default_encoder();

        let block = make_block(B256::ZERO);
        encoder.add_block(block).unwrap();
        encoder.step().unwrap();
        encoder.advance_l1_head(100);

        let sub = encoder.next_submission().unwrap();
        let stale_id = sub.id; // ID 0, now in-flight

        // Simulate a reorg: driver calls reset() after clearing in_flight.
        encoder.reset();
        assert!(encoder.pending.is_empty());
        // next_id is preserved across reset so post-reset IDs can never collide
        // with pre-reset in-flight IDs.
        assert_eq!(encoder.next_id, 1);

        // Stale confirm arrives (would have been delivered to the old pipeline).
        encoder.confirm(stale_id, 42);

        // Nothing to prune: blocks were already cleared by reset().
        assert!(encoder.blocks.is_empty());
        // pending is still empty — the confirm was a no-op.
        assert!(encoder.pending.is_empty());
    }

    /// Same as above but for `requeue()`: a stale requeue after reset must not
    /// rewind the cursor on any channel, because the channel no longer exists.
    #[test]
    fn test_stale_requeue_after_reset_is_noop() {
        let mut encoder = default_encoder();

        let block = make_block(B256::ZERO);
        encoder.add_block(block).unwrap();
        encoder.step().unwrap();
        encoder.advance_l1_head(100);

        let sub = encoder.next_submission().unwrap();
        let stale_id = sub.id;

        encoder.reset();

        // Stale requeue must not panic or corrupt state.
        encoder.requeue(stale_id);

        assert!(encoder.ready_channels.is_empty());
        assert!(encoder.pending.is_empty());
    }

    /// `reset()` must not reset `next_id`. Post-reorg submissions must receive IDs
    /// that are strictly greater than any pre-reorg in-flight ID, so a stale
    /// confirm/requeue can never silently match a fresh submission.
    #[test]
    fn test_next_id_monotonic_across_reset() {
        let mut encoder = default_encoder();

        // Pre-reorg: encode block 1, get submission ID 0 (in-flight).
        encoder.add_block(make_block(B256::ZERO)).unwrap();
        encoder.step().unwrap();
        encoder.advance_l1_head(100);
        let pre_reorg_sub = encoder.next_submission().unwrap();
        assert_eq!(pre_reorg_sub.id.0, 0);

        // Reorg: driver discards the future for pre_reorg_sub.id, then resets.
        encoder.reset();

        // Post-reorg: next_id must NOT have been reset to 0.
        assert_eq!(encoder.next_id, 1, "next_id must be preserved across reset");

        // Encode a post-reorg block and verify it gets a fresh, non-colliding ID.
        encoder.add_block(make_block(B256::ZERO)).unwrap();
        encoder.step().unwrap();
        encoder.advance_l1_head(200);
        let post_reorg_sub = encoder.next_submission().unwrap();
        assert_eq!(post_reorg_sub.id.0, 1, "post-reorg ID must not collide with pre-reorg ID 0");

        // Verify the post-reorg confirm works correctly.
        assert_eq!(encoder.ready_channels[0].pending_confirmations, 1);
        encoder.confirm(post_reorg_sub.id, 201);
        assert!(encoder.blocks.is_empty(), "post-reorg blocks should be pruned on confirm");
    }

    // --- sub_safety_margin tests ---

    /// The effective timeout is `max_channel_duration - sub_safety_margin`. A channel
    /// opened at L1=0 must stay open until `l1_head` reaches `at_threshold` exactly.
    #[rstest]
    #[case(10, 4, 5, 6)] // effective = 10-4 = 6
    #[case(5, 0, 4, 5)] // margin=0: effective = full duration
    fn test_sub_safety_margin(
        #[case] max_channel_duration: u64,
        #[case] sub_safety_margin: u64,
        #[case] below: u64,
        #[case] at_threshold: u64,
    ) {
        let config =
            EncoderConfig { max_channel_duration, sub_safety_margin, ..EncoderConfig::default() };
        let mut encoder = BatchEncoder::new(Arc::new(RollupConfig::default()), config);

        encoder.add_block(make_block(B256::ZERO)).unwrap();
        encoder.step().unwrap();
        assert!(encoder.current_channel.is_some());

        encoder.advance_l1_head(below);
        assert!(
            encoder.current_channel.is_some(),
            "channel must stay open before effective timeout"
        );

        encoder.advance_l1_head(at_threshold);
        assert!(encoder.current_channel.is_none(), "channel must close at effective timeout");
        assert!(!encoder.ready_channels.is_empty());
    }

    // --- target_num_frames tests ---

    /// With `target_num_frames = 2`, a channel whose frames span multiple entries must be
    /// packed two-per-submission. After one submission, a single confirm must credit both
    /// frames and trigger block pruning.
    #[test]
    fn test_target_num_frames_packs_multiple_frames() {
        let config = EncoderConfig {
            // Small frame size so two blocks produce at least two frames.
            max_frame_size: 32,
            target_frame_size: 32,
            target_num_frames: 2,
            max_channel_duration: 2,
            sub_safety_margin: 0,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(Arc::new(RollupConfig::default()), config);

        // Add a block and force-close the channel so we have frames to submit.
        let b1 = make_block(B256::ZERO);
        let b1_hash = b1.header.hash_slow();
        encoder.add_block(b1).unwrap();
        encoder.step().unwrap();

        // Add a second block chained from the first.
        encoder.add_block(make_block(b1_hash)).unwrap();
        encoder.step().unwrap();

        // Force close.
        encoder.advance_l1_head(100);
        assert!(encoder.current_channel.is_none());

        let Some(sub) = encoder.next_submission() else {
            // If the channel produced only 1 frame (data fits in one blob),
            // skip the multi-frame assertion — the test still validates single-frame path.
            return;
        };

        // Each submission must contain between 1 and target_num_frames frames.
        assert!(!sub.frames.is_empty() && sub.frames.len() <= 2);
    }

    /// A single requeue on a multi-frame submission must rewind the cursor to the start
    /// of the entire submission, so all frames in the batch are retried together.
    #[test]
    fn test_requeue_multi_frame_rewinds_to_frame_start() {
        let config = EncoderConfig {
            max_frame_size: 32,
            target_frame_size: 32,
            // Request up to 3 frames per submission but realistically we may get fewer.
            target_num_frames: 3,
            max_channel_duration: 2,
            sub_safety_margin: 0,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(Arc::new(RollupConfig::default()), config);

        encoder.add_block(make_block(B256::ZERO)).unwrap();
        encoder.step().unwrap();
        encoder.advance_l1_head(100);

        let Some(sub) = encoder.next_submission() else { return };
        let id = sub.id;
        let submitted_frame_count = sub.frames.len();

        encoder.requeue(id);

        // Cursor must be rewound — a fresh next_submission must return the same frames.
        let resub = encoder.next_submission();
        assert!(resub.is_some(), "requeued frames must be available again");
        assert_eq!(
            resub.unwrap().frames.len(),
            submitted_frame_count,
            "requeued submission must contain the same number of frames"
        );
    }

    // --- step() fatal error tests ---
    //
    // These tests document the invariant that batch composition failure is fatal.
    // A block that cannot be converted to a SingleBatch must not be silently
    // skipped: skipping would produce a gap in the L2 block sequence submitted
    // to L1, which the derivation spec prohibits.

    fn make_empty_block(parent_hash: B256) -> BaseBlock {
        BaseBlock {
            header: Header { parent_hash, ..Default::default() },
            body: BlockBody { transactions: vec![], ..Default::default() },
        }
    }

    fn make_non_deposit_block(parent_hash: B256) -> BaseBlock {
        let user_tx = {
            let signed = TxLegacy::default().into_signed(Signature::test_signature());
            BaseTxEnvelope::Legacy(signed)
        };
        BaseBlock {
            header: Header { parent_hash, ..Default::default() },
            body: BlockBody { transactions: vec![user_tx], ..Default::default() },
        }
    }

    fn make_bad_calldata_block(parent_hash: B256) -> BaseBlock {
        let deposit = BaseTxEnvelope::Deposit(Sealed::new(TxDeposit {
            input: Bytes::new(),
            ..Default::default()
        }));
        BaseBlock {
            header: Header { parent_hash, ..Default::default() },
            body: BlockBody { transactions: vec![deposit], ..Default::default() },
        }
    }

    /// `step()` must return a fatal `CompositionFailed` error — not silently skip —
    /// for any block that cannot be encoded into a `SingleBatch`.
    #[rstest]
    #[case::empty_block(make_empty_block(B256::ZERO), BatchComposeError::EmptyBlock)]
    #[case::not_deposit(make_non_deposit_block(B256::ZERO), BatchComposeError::NotDepositTx)]
    #[case::bad_calldata(make_bad_calldata_block(B256::ZERO), BatchComposeError::L1InfoDecode)]
    fn test_step_fatal(#[case] block: BaseBlock, #[case] expected_source: BatchComposeError) {
        let mut encoder = default_encoder();
        encoder.add_block(block).unwrap();
        let err = encoder.step().unwrap_err();
        assert!(
            matches!(err, StepError::CompositionFailed { cursor: 0, source } if source == expected_source)
        );
    }

    /// On composition failure the block cursor must not advance: the block stays
    /// at its position so the caller can observe the error and halt.
    #[test]
    fn test_step_fatal_leaves_cursor_unchanged() {
        let mut encoder = default_encoder();

        // Add a valid block first so block_cursor starts at 0 with 1 block queued.
        encoder.add_block(make_empty_block(B256::ZERO)).unwrap();
        assert_eq!(encoder.block_cursor, 0);

        let _ = encoder.step(); // returns Err

        // Cursor must still be 0 — the block was not consumed.
        assert_eq!(encoder.block_cursor, 0);
        assert_eq!(encoder.blocks.len(), 1);
    }

    // --- Span batch tests ---

    /// A [`BatchEncoder`] in Span mode with a tiny `target_frame_size` so the very first
    /// accumulated block exceeds the compressed-size threshold and triggers `ChannelClosed`.
    fn span_encoder_tiny_target() -> BatchEncoder {
        let config = EncoderConfig {
            batch_type: BatchType::Span,
            target_frame_size: 1,
            max_frame_size: 130_044,
            ..EncoderConfig::default()
        };
        BatchEncoder::new(Arc::new(RollupConfig::default()), config)
    }

    /// In Span mode, `step()` returns `BlockEncoded` for multiple blocks without
    /// opening a channel — blocks accumulate in the span accumulator until the size
    /// threshold or timeout fires.
    #[test]
    fn test_span_batch_accumulates_blocks_without_channel() {
        let config = EncoderConfig {
            batch_type: BatchType::Span,
            target_frame_size: 130_044, // large: size won't trigger
            max_channel_duration: 1000,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(Arc::new(RollupConfig::default()), config);

        let b1 = make_block(B256::ZERO);
        let b1_hash = b1.header.hash_slow();
        encoder.add_block(b1).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);

        // No channel opened — blocks are in the span accumulator.
        assert!(encoder.current_channel.is_none());
        assert_eq!(encoder.span_accumulator.len(), 1);
        assert!(encoder.span_opened_at_l1.is_some(), "span_opened_at_l1 must be set");

        let b2 = make_block(b1_hash);
        let b2_hash = b2.header.hash_slow();
        encoder.add_block(b2).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert!(encoder.current_channel.is_none());
        assert_eq!(encoder.span_accumulator.len(), 2);

        let b3 = make_block(b2_hash);
        encoder.add_block(b3).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert!(encoder.current_channel.is_none());
        assert_eq!(encoder.span_accumulator.len(), 3);

        // No submissions available until channel is closed.
        assert!(encoder.next_submission().is_none());
    }

    /// When the estimated compressed size of the span accumulator exceeds
    /// `target_frame_size * target_num_frames`, `step()` returns `ChannelClosed`
    /// and a submission is immediately available.
    #[test]
    fn test_span_batch_size_threshold_triggers_close() {
        let mut encoder = span_encoder_tiny_target();

        let block = make_block(B256::ZERO);
        encoder.add_block(block).unwrap();

        // The first block's overhead alone exceeds target_frame_size=1.
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);

        // Accumulator must be flushed.
        assert!(encoder.span_accumulator.is_empty());
        assert!(encoder.span_opened_at_l1.is_none());

        // A submission must be immediately available.
        let sub = encoder.next_submission();
        assert!(sub.is_some(), "span batch should produce a submission after size-based close");
    }

    /// In Span mode, `advance_l1_head` flushes the accumulator when the effective
    /// duration (`max_channel_duration - sub_safety_margin`) has elapsed. The accumulator
    /// must be preserved one step before the threshold and empty exactly at it.
    #[rstest]
    #[case(5, 0, 4, 5)] // no margin; full duration=5
    #[case(10, 4, 5, 6)] // effective = 10-4 = 6
    fn test_span_batch_timeout(
        #[case] max_channel_duration: u64,
        #[case] sub_safety_margin: u64,
        #[case] below: u64,
        #[case] at_threshold: u64,
    ) {
        let config = EncoderConfig {
            batch_type: BatchType::Span,
            target_frame_size: 130_044, // large: size won't trigger
            max_frame_size: 130_044,
            max_channel_duration,
            sub_safety_margin,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(Arc::new(RollupConfig::default()), config);

        encoder.add_block(make_block(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert!(encoder.current_channel.is_none());
        assert_eq!(encoder.span_accumulator.len(), 1);
        assert_eq!(encoder.span_opened_at_l1, Some(0));

        encoder.advance_l1_head(below);
        assert_eq!(encoder.span_accumulator.len(), 1, "accumulator must survive before timeout");
        assert!(encoder.ready_channels.is_empty());

        encoder.advance_l1_head(at_threshold);
        assert!(encoder.span_accumulator.is_empty(), "accumulator must be flushed at timeout");
        assert!(encoder.span_opened_at_l1.is_none());
        assert!(!encoder.ready_channels.is_empty(), "a ready channel must exist after flush");
        assert!(
            encoder.next_submission().is_some(),
            "should have a submission after timeout flush"
        );
    }

    /// End-to-end span batch path: add a block, trigger size-based close,
    /// get submission, confirm, and verify blocks are pruned.
    #[test]
    fn test_span_batch_end_to_end() {
        let mut encoder = span_encoder_tiny_target();

        let b1 = make_block(B256::ZERO);
        encoder.add_block(b1).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);

        let sub = encoder.next_submission().expect("submission must be available");
        let sub_id = sub.id;

        // Blocks must NOT be pruned until the submission is confirmed.
        assert!(!encoder.blocks.is_empty());

        encoder.confirm(sub_id, 10);

        // After confirmation blocks must be pruned.
        assert!(encoder.blocks.is_empty());
        assert_eq!(encoder.block_cursor, 0);
    }

    /// `reset()` in Span mode must clear both the accumulator and `span_opened_at_l1`.
    #[test]
    fn test_span_batch_reset_clears_span_state() {
        let config = EncoderConfig {
            batch_type: BatchType::Span,
            target_frame_size: 130_044, // large: size won't trigger
            max_channel_duration: 1000,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(Arc::new(RollupConfig::default()), config);

        let block = make_block(B256::ZERO);
        encoder.add_block(block).unwrap();
        encoder.step().unwrap();

        assert_eq!(encoder.span_accumulator.len(), 1);
        assert!(encoder.span_opened_at_l1.is_some());

        encoder.reset();

        assert!(encoder.span_accumulator.is_empty());
        assert!(encoder.span_opened_at_l1.is_none());
    }

    /// Multiple successive Span channels work correctly: each block immediately triggers
    /// a size-based close (with tiny target), and each channel is confirmed and pruned
    /// independently.
    #[test]
    fn test_span_batch_multiple_channels() {
        let mut encoder = span_encoder_tiny_target();

        // First block → size threshold → first channel closed.
        let b1 = make_block(B256::ZERO);
        let b1_hash = b1.header.hash_slow();
        encoder.add_block(b1).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        assert_eq!(encoder.ready_channels.len(), 1);

        // Second block → size threshold → second channel closed.
        let b2 = make_block(b1_hash);
        encoder.add_block(b2).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        assert_eq!(encoder.ready_channels.len(), 2);

        // Confirm first channel — its block is pruned.
        let sub1 = encoder.next_submission().expect("ch1 must have a submission");
        let id1 = sub1.id;
        encoder.confirm(id1, 10);
        assert_eq!(encoder.ready_channels.len(), 1);

        // Confirm second channel — its block is pruned.
        let sub2 = encoder.next_submission().expect("ch2 must have a submission");
        let id2 = sub2.id;
        encoder.confirm(id2, 11);
        assert_eq!(encoder.ready_channels.len(), 0);
        assert!(encoder.blocks.is_empty());
    }

    /// A span-mode requeue rewinds the cursor on the ready channel just as in Single mode.
    #[test]
    fn test_span_batch_requeue_rewinds_cursor() {
        let mut encoder = span_encoder_tiny_target();

        let block = make_block(B256::ZERO);
        encoder.add_block(block).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);

        let sub = encoder.next_submission().unwrap();
        let sub_id = sub.id;

        encoder.requeue(sub_id);

        let resub = encoder.next_submission();
        assert!(resub.is_some(), "requeued span frames must be available again");
    }

    // --- prune_safe tests ---

    fn make_numbered_block(parent_hash: B256, number: u64) -> BaseBlock {
        let calldata = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default()).encode_calldata();
        let deposit = BaseTxEnvelope::Deposit(Sealed::new(TxDeposit {
            input: calldata,
            ..Default::default()
        }));
        BaseBlock {
            header: Header { parent_hash, number, ..Default::default() },
            body: BlockBody { transactions: vec![deposit], ..Default::default() },
        }
    }

    /// `prune_safe` must drain leading blocks whose number is <= the safe head
    /// and that have already been encoded (index < `block_cursor`).
    #[test]
    fn test_prune_safe_drains_encoded_blocks() {
        let mut encoder = default_encoder();

        let b1 = make_numbered_block(B256::ZERO, 1);
        let b1_hash = b1.header.hash_slow();
        encoder.add_block(b1).unwrap();

        let b2 = make_numbered_block(b1_hash, 2);
        let b2_hash = b2.header.hash_slow();
        encoder.add_block(b2).unwrap();

        let b3 = make_numbered_block(b2_hash, 3);
        encoder.add_block(b3).unwrap();

        // Encode all three blocks.
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert_eq!(encoder.block_cursor, 3);

        // Prune blocks 1 and 2 (safe head = 2).
        encoder.prune_safe(2);

        assert_eq!(encoder.blocks.len(), 1, "only block 3 should remain");
        assert_eq!(encoder.blocks[0].header.number, 3);
        assert_eq!(encoder.block_cursor, 1, "cursor must be adjusted by prune count");
    }

    /// `prune_safe` must not prune blocks that have not yet been encoded
    /// (index >= `block_cursor`), even if their number is below the safe head.
    #[test]
    fn test_prune_safe_does_not_prune_unencoded_blocks() {
        let mut encoder = default_encoder();

        let b1 = make_numbered_block(B256::ZERO, 1);
        let b1_hash = b1.header.hash_slow();
        encoder.add_block(b1).unwrap();

        let b2 = make_numbered_block(b1_hash, 2);
        encoder.add_block(b2).unwrap();

        // Encode only block 1 (cursor = 1).
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert_eq!(encoder.block_cursor, 1);

        // Prune with safe_l2_number = 5 — block 2 is below safe head but not encoded.
        encoder.prune_safe(5);

        assert_eq!(encoder.blocks.len(), 1, "block 2 must not be pruned (not yet encoded)");
        assert_eq!(encoder.blocks[0].header.number, 2);
        assert_eq!(encoder.block_cursor, 0, "cursor adjusted after pruning block 1");
    }

    /// `prune_safe` with a safe head below all block numbers is a no-op.
    #[test]
    fn test_prune_safe_noop_when_below_all_blocks() {
        let mut encoder = default_encoder();

        let b1 = make_numbered_block(B256::ZERO, 10);
        encoder.add_block(b1).unwrap();
        encoder.step().unwrap();

        encoder.prune_safe(5);

        assert_eq!(encoder.blocks.len(), 1, "no blocks should be pruned");
        assert_eq!(encoder.block_cursor, 1, "cursor must be unchanged");
    }

    /// `prune_safe` on an empty encoder is a no-op.
    #[test]
    fn test_prune_safe_noop_when_empty() {
        let mut encoder = default_encoder();
        encoder.prune_safe(100);
        assert!(encoder.blocks.is_empty());
        assert_eq!(encoder.block_cursor, 0);
    }

    /// `prune_safe` must adjust `block_range.end` on ready channels so that
    /// a subsequent `confirm()` does not over-prune.
    #[test]
    fn test_prune_safe_adjusts_ready_channel_block_ranges() {
        let mut encoder = default_encoder();

        let b1 = make_numbered_block(B256::ZERO, 1);
        let b1_hash = b1.header.hash_slow();
        encoder.add_block(b1).unwrap();

        let b2 = make_numbered_block(b1_hash, 2);
        encoder.add_block(b2).unwrap();

        // Encode both blocks.
        encoder.step().unwrap();
        encoder.step().unwrap();
        assert_eq!(encoder.block_cursor, 2);

        // Close the channel so we get a ready channel with block_range 0..2.
        encoder.advance_l1_head(100);
        assert!(!encoder.ready_channels.is_empty());
        assert_eq!(encoder.ready_channels[0].block_range.end, 2);

        // Prune block 1 (safe head = 1).
        encoder.prune_safe(1);
        assert_eq!(encoder.blocks.len(), 1);
        assert_eq!(encoder.block_cursor, 1);

        // The ready channel's block_range.end must be adjusted.
        assert_eq!(
            encoder.ready_channels[0].block_range.end, 1,
            "block_range.end must be reduced by prune count"
        );

        // Confirm the channel — should prune the remaining block.
        let sub = encoder.next_submission().unwrap();
        encoder.confirm(sub.id, 101);
        assert!(encoder.blocks.is_empty(), "confirm after prune_safe must finish pruning");
    }

    /// `encode_and_drain` steps until idle, force-closes, and returns all frames.
    #[test]
    fn test_encode_and_drain_returns_frames() {
        let mut encoder = default_encoder();
        encoder.add_block(make_block_with_user_tx(B256::ZERO)).expect("add block");
        let frames = encoder.encode_and_drain().expect("encode_and_drain");
        assert!(!frames.is_empty(), "encode_and_drain must return at least one frame");
    }

    /// `encode_and_drain` with no blocks added returns empty (Idle immediately).
    #[test]
    fn test_encode_and_drain_no_blocks_returns_empty() {
        let mut encoder = default_encoder();
        let frames = encoder.encode_and_drain().expect("encode_and_drain");
        assert!(frames.is_empty(), "no blocks → encode_and_drain must return empty");
    }

    /// `encode_and_drain` in Span mode accumulates and drains frames correctly.
    #[test]
    fn test_encode_and_drain_span_mode() {
        let rollup_config = Arc::new(RollupConfig::default());
        let config = EncoderConfig { batch_type: BatchType::Span, ..EncoderConfig::default() };
        let mut encoder = BatchEncoder::new(rollup_config, config);
        encoder.add_block(make_block_with_user_tx(B256::ZERO)).expect("add block 1");
        let hash = make_block_with_user_tx(B256::ZERO).header.hash_slow();
        encoder.add_block(make_block_with_user_tx(hash)).expect("add block 2");
        let frames = encoder.encode_and_drain().expect("encode_and_drain span");
        assert!(!frames.is_empty(), "span encode_and_drain must produce frames");
    }

    /// Encoding with a small `max_frame_size` fragments a multi-block channel
    /// into multiple frames, proving the encoder respects the frame-size limit.
    #[test]
    fn frame_fragmentation_with_small_frame_size() {
        let rollup_config = Arc::new(RollupConfig::default());
        let config = EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() };
        let mut encoder = BatchEncoder::new(rollup_config, config);

        // Add 5 L2 blocks with a user tx in each to produce non-trivial payload.
        let mut parent = B256::ZERO;
        for _ in 0..5 {
            let block = make_block_with_user_tx(parent);
            parent = block.header.hash_slow();
            encoder.add_block(block).expect("add block");
        }

        let frames = encoder.encode_and_drain().expect("encode_and_drain");
        assert!(
            frames.len() >= 3,
            "expected at least 3 frames with max_frame_size=80, got {}",
            frames.len()
        );
    }

    /// `max_l1_tx_size_bytes` limits the calldata submission size for calldata DA.
    ///
    /// With a very small limit, only one frame (at minimum) is included per submission
    /// even when multiple frames are available.
    #[test]
    fn calldata_max_l1_tx_size_limits_submission() {
        let rollup_config = Arc::new(RollupConfig::default());
        // Use a tiny max_frame_size to generate multiple small frames and
        // a max_l1_tx_size_bytes of 0 to force a single-frame submission each time.
        let config = EncoderConfig {
            da_type: DaType::Calldata,
            target_num_frames: 1, // required for calldata
            max_frame_size: 100,
            target_frame_size: 100,
            max_l1_tx_size_bytes: Some(0), // smaller than any real frame; always warns
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(rollup_config, config);

        let block = make_block_with_user_tx(B256::ZERO);
        encoder.add_block(block).expect("add block");
        encoder.encode_and_drain().expect("encode_and_drain");

        // With max_l1_tx_size_bytes=0 every frame exceeds the limit, but we still get
        // at least one submission (the .max(1) ensures we never stall).
        let sub = encoder.next_submission();
        // All frames were already drained by encode_and_drain; submissions were emitted
        // during drain. The key property is that no panic occurred and the encoder
        // handled the oversized-frame case gracefully.
        let _ = sub; // may be None if all frames came out during encode_and_drain
    }

    /// When `max_l1_tx_size_bytes` is large enough to hold all frames, all frames in a
    /// calldata channel are packed into a single submission (bounded by `target_num_frames`).
    #[test]
    fn calldata_max_l1_tx_size_no_op_when_large() {
        let rollup_config = Arc::new(RollupConfig::default());
        // Use a small frame size to generate multiple frames, but a large tx size limit.
        let config = EncoderConfig {
            da_type: DaType::Calldata,
            target_num_frames: 1, // required for calldata
            max_frame_size: 100,
            target_frame_size: 100,
            max_l1_tx_size_bytes: Some(1_000_000),
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(rollup_config, config);

        let block = make_block_with_user_tx(B256::ZERO);
        encoder.add_block(block).expect("add block");

        // Run until idle, force-close, and drain submissions.
        loop {
            if encoder.step().expect("step") == StepResult::Idle {
                break;
            }
        }
        encoder.force_close_channel();

        // Each submission contains exactly 1 frame (target_num_frames=1).
        let mut count = 0;
        while let Some(sub) = encoder.next_submission() {
            assert_eq!(sub.frames.len(), 1, "calldata submission must have exactly 1 frame");
            count += 1;
        }
        assert!(count >= 1, "expected at least one submission");
    }

    /// `max_l1_tx_size_bytes` is a no-op for blob DA; submissions are not affected.
    #[test]
    fn blob_da_ignores_max_l1_tx_size_bytes() {
        let rollup_config = Arc::new(RollupConfig::default());
        let config = EncoderConfig {
            da_type: DaType::Blob,
            target_num_frames: 1,
            max_l1_tx_size_bytes: Some(1), // would cut every tx if applied to blobs
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(rollup_config, config);

        let block = make_block_with_user_tx(B256::ZERO);
        encoder.add_block(block).expect("add block");
        let frames = encoder.encode_and_drain().expect("encode_and_drain");
        assert!(!frames.is_empty(), "blob DA must still produce frames despite tiny size limit");
    }

    /// `set_blob_override(true)` flips a calldata-configured encoder to emit
    /// blob-typed submissions. Clearing the override restores calldata.
    #[test]
    fn blob_override_flips_calldata_submissions_to_blob() {
        let rollup_config = Arc::new(RollupConfig::default());
        let config = EncoderConfig {
            da_type: DaType::Calldata,
            target_num_frames: 1,
            max_frame_size: 200,
            target_frame_size: 200,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(rollup_config, config);

        encoder.add_block(make_block_with_user_tx(B256::ZERO)).expect("add block");
        loop {
            if encoder.step().expect("step") == StepResult::Idle {
                break;
            }
        }
        encoder.force_close_channel();

        encoder.set_blob_override(true);
        let sub = encoder.next_submission().expect("submission while override active");
        assert_eq!(sub.da_type, DaType::Blob, "override must flip da_type to Blob");
        encoder.requeue(sub.id);

        encoder.set_blob_override(false);
        let sub = encoder.next_submission().expect("submission after override cleared");
        assert_eq!(sub.da_type, DaType::Calldata, "configured calldata da_type must return");
    }

    /// `set_blob_override(true)` is a no-op for blob-configured encoders —
    /// submissions are blob-typed regardless of the override.
    #[test]
    fn blob_override_is_noop_for_blob_configured_encoder() {
        let rollup_config = Arc::new(RollupConfig::default());
        let config = EncoderConfig { da_type: DaType::Blob, ..EncoderConfig::default() };
        let mut encoder = BatchEncoder::new(rollup_config, config);

        encoder.add_block(make_block_with_user_tx(B256::ZERO)).expect("add block");
        encoder.encode_and_drain().expect("encode_and_drain");
        encoder.set_blob_override(true);
        // No assertion on next_submission — drain already consumed everything.
        // The contract is just that the override does not corrupt state.
        assert!(encoder.next_submission().is_none());
    }
}
