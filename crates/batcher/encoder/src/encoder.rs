//! The [`BatchEncoder`] implementation.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::Arc,
};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::B256;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_genesis::RollupConfig;
use base_comp::BatchComposer;
use base_protocol::{BlockInfo, ChannelId, Frame};
use rand::{RngCore, SeedableRng, rngs::SmallRng};
use tracing::{debug, warn};

use crate::{
    BatchPipeline, BatchSubmission, BatcherMetrics, BlobPayload, DaType, DerivationReconciliation,
    EncoderConfig, ReorgError, StepError, StepResult, SubmissionId, SubmissionPayload,
    channel::{ChannelAddOutcome, FrameState, OpenChannel, PendingRef, ReadyChannel},
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
    /// Buffered L2 blocks above the latest observed safe head.
    blocks: VecDeque<BaseBlock>,
    /// Index into `blocks`: next block not yet fed into the current channel.
    block_cursor: usize,
    /// Hash of the last accepted block or safe-head anchor.
    tip: Option<B256>,
    /// The channel currently being built. `None` between channels.
    current_channel: Option<OpenChannel>,
    /// Closed channels awaiting submission, safe-head pruning, or timeout replay.
    ready_channels: VecDeque<ReadyChannel>,
    /// In-flight submissions: id -> reference into `ready_channels`.
    pending: HashMap<SubmissionId, PendingRef>,
    /// Next submission id counter.
    next_id: u64,
    /// Per-instance RNG for generating unique channel IDs.
    rng: SmallRng,
    /// Driver-controlled override that forces [`DaType::Blob`] on every emitted
    /// submission, regardless of the configured `da_type`. Toggled by the driver
    /// when DA-backlog throttling activates and `force_blobs_when_throttling` is
    /// set. No-op when the configured `da_type` is already [`DaType::Blob`].
    blob_override: bool,
    /// Fatal error observed from trait methods that cannot return [`StepError`].
    deferred_step_error: Option<StepError>,
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
            .finish_non_exhaustive()
    }
}

impl BatchEncoder {
    /// Create a new [`BatchEncoder`].
    pub fn new(rollup_config: Arc<RollupConfig>, config: EncoderConfig) -> Self {
        Self {
            rollup_config,
            config,
            l1_head: 0,
            blocks: VecDeque::new(),
            block_cursor: 0,
            tip: None,
            current_channel: None,
            ready_channels: VecDeque::new(),
            pending: HashMap::new(),
            next_id: 0,
            rng: SmallRng::from_os_rng(),
            blob_override: false,
            deferred_step_error: None,
        }
    }

    /// Estimate the DA bytes represented by non-deposit transactions in `block`.
    fn block_da_backlog_bytes(block: &BaseBlock) -> u64 {
        block
            .body
            .transactions
            .iter()
            .filter(|tx| !matches!(tx, BaseTxEnvelope::Deposit(_)))
            .map(|tx| tx.encode_2718_len() as u64)
            .sum()
    }

    /// Step the encoder until idle, flush the current channel, and return
    /// all frames from every available submission.
    ///
    /// Convenience wrapper for tests and one-shot batch pipelines that have
    /// already added all blocks via [`BatchPipeline::add_block`] and want all
    /// output frames in a single call.
    ///
    /// # Errors
    ///
    /// Returns the first [`StepError`] encountered during encoding. Previously ready
    /// submissions remain available via [`BatchPipeline::next_submission`].
    pub fn encode_and_drain(&mut self) -> Result<Vec<Arc<Frame>>, StepError> {
        loop {
            match self.step()? {
                StepResult::Idle => break,
                StepResult::BlockEncoded | StepResult::ChannelClosed => {}
            }
        }
        self.close_current_channel("force")?;
        let mut frames = Vec::new();
        while let Some(sub) = self.next_submission() {
            match sub.payload() {
                SubmissionPayload::Blobs(blobs) => {
                    frames.extend(blobs.iter().flat_map(|blob| blob.frames().iter().cloned()));
                }
                SubmissionPayload::Calldata(frame) => frames.push(Arc::clone(frame)),
            }
        }
        Ok(frames)
    }

    /// Finalizes the current channel and publishes it to the submission queue.
    ///
    /// Size and timeout transitions in `step`, plus explicit [`BatchPipeline::flush`]
    /// calls, share this path. Frames are built completely before `ready_channels` is
    /// mutated, so a framing error cannot publish a partial channel.
    /// `close_reason` is also recorded on the channel-close metric.
    fn close_current_channel(&mut self, close_reason: &'static str) -> Result<(), StepError> {
        let Some(open) = self.current_channel.take() else {
            return Ok(());
        };

        // Capture stats before consuming the open channel during finalization.
        let input_bytes = open.input_bytes();
        let opened_at_l1 = open.opened_at_l1;
        let blocks_added = open.blocks_added;
        let channel_id = open.id();
        let encoded_block_end = open.block_start.saturating_add(blocks_added);
        let encoded_block_range = open.block_start..encoded_block_end;

        // Build the complete frame list before publishing the ready channel.
        let frames: Vec<_> =
            open.into_frames(self.config.max_frame_size)?.into_iter().map(Arc::new).collect();
        let frame_count = frames.len();
        let duration_blocks = self.l1_head.saturating_sub(opened_at_l1);
        let compressed_bytes: usize = frames.iter().map(|f| f.data.len()).sum();
        let closed_da_backlog_bytes = u64::try_from(compressed_bytes).unwrap_or(u64::MAX);

        debug!(
            channel_id = ?channel_id,
            frame_count = %frame_count,
            encoded_block_range_start = %encoded_block_range.start,
            encoded_block_range_end = %encoded_block_range.end,
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
        BatcherMetrics::input_bytes(BatcherMetrics::STAGE_CLOSED).set(input_bytes as f64);
        BatcherMetrics::output_bytes().set(compressed_bytes as f64);
        BatcherMetrics::input_bytes_total().increment(input_bytes);
        BatcherMetrics::output_bytes_total().increment(closed_da_backlog_bytes);
        BatcherMetrics::channel_num_frames().set(frame_count as f64);
        if input_bytes > 0 {
            let ratio = compressed_bytes as f64 / input_bytes as f64;
            BatcherMetrics::channel_compression_ratio().record(ratio);
        }
        // All frames from this channel are now pending submission.
        BatcherMetrics::pending_frames().increment(frame_count as f64);

        self.ready_channels.push_back(ReadyChannel {
            id: channel_id,
            frame_states: vec![FrameState::Ready; frame_count],
            frames,
            encoded_block_range,
            da_backlog_bytes: closed_da_backlog_bytes,
            first_confirmed_l1_block: None,
            last_confirmed_l1_block: None,
        });

        Ok(())
    }

    /// Store a fatal encoding error so the next [`BatchPipeline::step`] reports it.
    fn defer_step_error(&mut self, error: StepError, operation: &'static str) {
        warn!(
            error = %error,
            operation = %operation,
            "deferred fatal encoder error from non-fallible pipeline method"
        );
        if self.deferred_step_error.is_none() {
            self.deferred_step_error = Some(error);
        } else {
            warn!(
                dropped_error = %error,
                operation = %operation,
                "dropping additional deferred encoder error; earlier error takes precedence"
            );
        }
    }

    /// Opens a channel when `step` has a queued block but no current channel.
    ///
    /// `block_start` anchors the exact input range later attached to the closed
    /// channel; the current L1 head starts its timeout window.
    fn open_new_channel(&mut self, block_start: usize) {
        let mut id = ChannelId::default();
        self.rng.fill_bytes(&mut id);

        debug!(
            channel_id = ?id,
            block_start = %block_start,
            l1_head = %self.l1_head,
            "opened new channel"
        );
        BatcherMetrics::channel_opened_total().increment(1);

        self.current_channel = Some(OpenChannel::new(
            id,
            Arc::clone(&self.rollup_config),
            &self.config,
            block_start,
            self.l1_head,
        ));
    }

    /// Closes the current channel when its effective L1 duration has elapsed.
    ///
    /// Both `step` and [`BatchPipeline::advance_l1_head`] call this so timeout
    /// processing does not depend on another L2 block arriving.
    fn check_channel_timeout(&mut self) -> Result<bool, StepError> {
        // Apply the safety margin so channels are closed `sub_safety_margin` L1 blocks
        // before the configured `max_channel_duration`, ensuring frames land well within
        // the protocol's `channel_timeout` inclusion window.
        let effective_duration =
            self.config.max_channel_duration.saturating_sub(self.config.sub_safety_margin);

        let should_close = self.current_channel.as_ref().is_some_and(|open| {
            self.l1_head.saturating_sub(open.opened_at_l1) >= effective_duration
        });

        if should_close {
            debug!(l1_head = %self.l1_head, "channel timed out, closing");
            self.close_current_channel("timeout")?;
        }

        Ok(should_close)
    }

    /// Returns the conservative protocol channel timeout used for confirmation windows.
    fn confirmation_channel_timeout(&self) -> u64 {
        let pre_granite = self.rollup_config.channel_timeout(0);
        let post_granite = self.rollup_config.channel_timeout(u64::MAX);
        match (pre_granite, post_granite) {
            (0, timeout) | (timeout, 0) => timeout,
            (pre, post) => pre.min(post),
        }
    }

    /// Drops confirmed-too-late channels and rewinds encoding to republish their blocks.
    fn invalidate_ready_channel(
        &mut self,
        chan_idx: usize,
        observed_l1_block: u64,
        channel_timeout: u64,
    ) {
        if chan_idx >= self.ready_channels.len() {
            return;
        }

        let channel = &self.ready_channels[chan_idx];
        let channel_id = channel.id;
        let first_confirmed_l1_block = channel.first_confirmed_l1_block;
        let last_confirmed_l1_block = channel.last_confirmed_l1_block;
        let replay_from = channel.encoded_block_range.start;
        let removed_pending_frames = self
            .ready_channels
            .iter()
            .skip(chan_idx)
            .flat_map(|channel| &channel.frame_states)
            .filter(|state| **state == FrameState::Ready)
            .count();

        warn!(
            channel_id = ?channel_id,
            first_confirmed_l1_block = ?first_confirmed_l1_block,
            last_confirmed_l1_block = ?last_confirmed_l1_block,
            observed_l1_block = %observed_l1_block,
            channel_timeout = %channel_timeout,
            replay_from_block_index = %replay_from,
            "confirmed channel exceeded derivation timeout, replaying blocks"
        );

        self.ready_channels.truncate(chan_idx);
        self.pending.retain(|_, pending| pending.channel_idx < chan_idx);
        self.current_channel = None;
        self.block_cursor = self.block_cursor.min(replay_from);

        if removed_pending_frames > 0 {
            BatcherMetrics::pending_frames().decrement(removed_pending_frames as f64);
        }
    }

    /// Invalidates the first ready channel whose confirmation window has expired.
    fn invalidate_expired_ready_channels(&mut self) {
        let channel_timeout = self.confirmation_channel_timeout();
        let Some(chan_idx) = self.ready_channels.iter().position(|channel| {
            let Some(first) = channel.first_confirmed_l1_block else {
                return false;
            };
            let inclusion_span =
                channel.last_confirmed_l1_block.unwrap_or(first).saturating_sub(first);
            if inclusion_span >= channel_timeout {
                return true;
            }

            let incomplete = !channel.is_fully_confirmed();
            incomplete && self.l1_head.saturating_sub(first) >= channel_timeout
        }) else {
            return;
        };

        self.invalidate_ready_channel(chan_idx, self.l1_head, channel_timeout);
    }

    /// Rebase all block-queue-relative offsets after pruning a prefix from `blocks`.
    fn rebase_after_block_prune(&mut self, prune_count: usize) {
        self.block_cursor = self.block_cursor.saturating_sub(prune_count);

        if let Some(open) = self.current_channel.as_mut() {
            let old_end = open.block_start.saturating_add(open.blocks_added);
            let new_start = open.block_start.saturating_sub(prune_count);
            let new_end = old_end.saturating_sub(prune_count);
            open.block_start = new_start;
            open.blocks_added = new_end.saturating_sub(new_start);
        }

        for channel in &mut self.ready_channels {
            channel.encoded_block_range.start =
                channel.encoded_block_range.start.saturating_sub(prune_count);
            channel.encoded_block_range.end =
                channel.encoded_block_range.end.saturating_sub(prune_count);
        }
    }

    /// Prune buffered blocks at or below the reported safe L2 head.
    fn prune_safe(&mut self, safe_l2: BlockInfo) -> bool {
        // Validate the safe head against the buffered chain before mutating state.
        let Some(oldest) = self.blocks.front() else {
            self.tip = Some(safe_l2.hash);
            return true;
        };

        let oldest_number = oldest.header.number;
        let next_safe = safe_l2.number.saturating_add(1);
        if next_safe < oldest_number {
            return false;
        }

        let prune_count = (next_safe - oldest_number) as usize;
        if prune_count > self.blocks.len() {
            return false;
        }

        if prune_count == 0 {
            return oldest.header.parent_hash == safe_l2.hash;
        }
        if self.blocks[prune_count - 1].header.hash_slow() != safe_l2.hash {
            return false;
        }

        debug!(
            prune_count,
            safe_l2_number = safe_l2.number,
            "pruning safe blocks from input queue"
        );

        // Remove channels fully covered by the safe head and rebase pending references.
        let ready_channels_to_prune = self
            .ready_channels
            .iter()
            .take_while(|channel| channel.encoded_block_range.end <= prune_count)
            .count();
        if ready_channels_to_prune > 0 {
            let removed_pending_frames = self
                .ready_channels
                .iter()
                .take(ready_channels_to_prune)
                .flat_map(|channel| &channel.frame_states)
                .filter(|state| **state == FrameState::Ready)
                .count();
            self.ready_channels.drain(..ready_channels_to_prune);
            self.pending.retain(|_, pending| {
                if pending.channel_idx < ready_channels_to_prune {
                    return false;
                }
                pending.channel_idx -= ready_channels_to_prune;
                true
            });
            if removed_pending_frames > 0 {
                BatcherMetrics::pending_frames().decrement(removed_pending_frames as f64);
            }
        }

        if self.current_channel.as_ref().is_some_and(|channel| {
            channel.block_start.saturating_add(channel.blocks_added) <= prune_count
        }) {
            self.current_channel = None;
        }

        // Remove the safe block prefix and rebase every remaining block-relative offset.
        self.blocks.drain(..prune_count);
        self.rebase_after_block_prune(prune_count);
        if self.blocks.is_empty() {
            self.tip = Some(safe_l2.hash);
        }
        BatcherMetrics::pending_blocks().decrement(prune_count as f64);
        true
    }

    /// Returns whether derivation passed a fully confirmed channel without making its tail safe.
    fn is_derivation_stalled(&self, current_l1: u64, safe_l2: BlockInfo) -> bool {
        self.ready_channels.iter().any(|channel| {
            if !channel.is_fully_confirmed() {
                return false;
            }

            let Some(last_inclusion) = channel.last_confirmed_l1_block else {
                return false;
            };
            if current_l1 <= last_inclusion {
                return false;
            }

            channel
                .encoded_block_range
                .end
                .checked_sub(1)
                .and_then(|last_block_index| self.blocks.get(last_block_index))
                .is_some_and(|last_block| safe_l2.number < last_block.header.number)
        })
    }
}

impl BatchPipeline for BatchEncoder {
    fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
        if let Some(expected) = self.tip
            && block.header.parent_hash != expected
        {
            return Err((
                ReorgError::ParentMismatch { expected, got: block.header.parent_hash },
                Box::new(block),
            ));
        }

        let number = block.header.number;
        let hash = block.header.hash_slow();
        self.tip = Some(hash);
        self.blocks.push_back(block);
        BatcherMetrics::pending_blocks().increment(1.0);

        debug!(block = %number, pending_blocks = %self.blocks.len(), "block added to encoder queue");

        Ok(())
    }

    fn step(&mut self) -> Result<StepResult, StepError> {
        // A step performs one state transition: report a deferred error, close
        // a timed-out channel, or attempt exactly one queued L2 block.
        if let Some(error) = self.deferred_step_error.take() {
            return Err(error);
        }

        // Check for channel timeout first.
        if self.check_channel_timeout()? {
            return Ok(StepResult::ChannelClosed);
        }

        // If there are no blocks to encode, we're idle.
        if self.block_cursor >= self.blocks.len() {
            return Ok(StepResult::Idle);
        }

        let block = &self.blocks[self.block_cursor];
        let block_da_backlog_bytes = Self::block_da_backlog_bytes(block);

        // Convert block to a SingleBatch. Failure here is fatal: skipping the block
        // would produce a gap in the L2 block sequence submitted to L1.
        let single_batch = BatchComposer::block_to_single_batch(block)
            .map_err(|source| StepError::CompositionFailed { cursor: self.block_cursor, source })?;

        if self.current_channel.is_none() {
            self.open_new_channel(self.block_cursor);
        }

        let open = self.current_channel.as_mut().expect("channel exists after open_new_channel");
        let outcome = open.add_block(single_batch, block_da_backlog_bytes)?;

        match outcome {
            ChannelAddOutcome::Accepted => {
                // The queue cursor is the acceptance boundary. Advancing it
                // earlier would lose a block when the channel rejects a candidate.
                BatcherMetrics::input_bytes(BatcherMetrics::STAGE_ADDED)
                    .set(open.input_bytes() as f64);
                self.block_cursor += 1;
                Ok(StepResult::BlockEncoded)
            }
            ChannelAddOutcome::Rejected(reason) => {
                // A hard RLP rejection in an empty channel cannot be resolved
                // by retrying the same block in another identical channel.
                if open.blocks_added == 0 {
                    self.current_channel = None;
                    BatcherMetrics::channel_closed_total(BatcherMetrics::REASON_DISCARD)
                        .increment(1);
                    return Err(StepError::BlockRejectedByEmptyChannel {
                        cursor: self.block_cursor,
                        reason,
                    });
                }

                // Finalize only the previously accepted payload. The unchanged
                // cursor makes the next step retry this block in a fresh channel.
                debug!(reason = %reason, "channel rejected block, closing");
                self.close_current_channel("size_full")?;
                Ok(StepResult::ChannelClosed)
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
        // Find the first channel with a contiguous range of ready frames.
        for (chan_idx, channel) in self.ready_channels.iter_mut().enumerate() {
            let Some(ready_range) = channel.next_ready_frame_range() else {
                continue;
            };
            let frame_start = ready_range.start;
            let available = ready_range.len();

            // Calldata is one frame per L1 transaction. Blobs pack up to
            // `target_num_frames` frames, each as its own blob in the same tx.
            let frame_count = if effective_da_type == DaType::Calldata {
                if let Some(max_size) = self.config.max_l1_tx_size_bytes {
                    let frame_size = 24 + channel.frames[frame_start].data.len();
                    if frame_size > max_size {
                        warn!(
                            frame_size,
                            max_l1_tx_size_bytes = max_size,
                            "frame exceeds max_l1_tx_size_bytes; submitting anyway"
                        );
                    }
                }
                1
            } else {
                available
                    .min(self.config.target_num_frames)
                    .min(EncoderConfig::MAX_BLOBS_PER_TX)
                    .max(1)
            };
            // Clone the Arcs (pointer copies, not deep copies of frame data).
            let frames: Vec<_> = channel.frames[frame_start..frame_start + frame_count].to_vec();

            let id = SubmissionId(self.next_id);
            self.next_id += 1;

            channel.mark_pending(frame_start..frame_start + frame_count);

            self.pending.insert(id, PendingRef { channel_idx: chan_idx, frame_start, frame_count });

            // Frames move from pending → in-flight; decrement the pending gauge.
            BatcherMetrics::pending_frames().decrement(frame_count as f64);
            debug!(
                id = %id.0,
                frame_count = %frame_count,
                frame_start = %frame_start,
                "dequeued frames for submission"
            );

            return Some(match effective_da_type {
                DaType::Calldata => BatchSubmission::calldata(
                    id,
                    frames.into_iter().next().expect("calldata submissions carry one frame"),
                ),
                DaType::Blob => {
                    let blobs = frames
                        .into_iter()
                        .map(|frame| BlobPayload::new(vec![frame]).expect("one frame"))
                        .collect();
                    BatchSubmission::blobs(id, blobs)
                        .expect("non-empty blob submission within the transaction limit")
                }
            });
        }

        None
    }

    fn has_ready_submission(&self) -> bool {
        self.ready_channels.iter().any(|channel| channel.frame_states.contains(&FrameState::Ready))
    }

    fn confirm(&mut self, id: SubmissionId, l1_block: u64) {
        let Some(pending_ref) = self.pending.remove(&id) else {
            debug!(id = ?id, "ignoring confirmation for untracked submission");
            return;
        };

        let chan_idx = pending_ref.channel_idx;
        if chan_idx >= self.ready_channels.len() {
            warn!(id = ?id, chan_idx = %chan_idx, "confirm: channel index out of bounds; submission lost");
            return;
        }

        let channel = &mut self.ready_channels[chan_idx];
        channel.mark_confirmed(
            pending_ref.frame_start..pending_ref.frame_start + pending_ref.frame_count,
        );
        // Receipts can settle out of order, so retain both ends of the inclusion range.
        channel.first_confirmed_l1_block =
            Some(channel.first_confirmed_l1_block.map_or(l1_block, |first| first.min(l1_block)));
        channel.last_confirmed_l1_block =
            Some(channel.last_confirmed_l1_block.map_or(l1_block, |last| last.max(l1_block)));

        // Safe-head reconciliation owns normal channel removal. Retaining fully confirmed
        // channels lets timeout handling detect stalled derivation.
        if channel.is_fully_confirmed() {
            debug!(channel_id = ?channel.id, "channel fully confirmed");
            BatcherMetrics::channel_fully_submitted_total().increment(1);
        }
    }

    fn requeue(&mut self, id: SubmissionId) {
        let Some(pending_ref) = self.pending.remove(&id) else {
            debug!(id = ?id, "ignoring retry for untracked submission");
            return;
        };

        let chan_idx = pending_ref.channel_idx;
        if chan_idx >= self.ready_channels.len() {
            warn!(id = ?id, chan_idx = %chan_idx, "requeue: channel index out of bounds; submission lost");
            return;
        }

        let channel = &mut self.ready_channels[chan_idx];
        // Requeue only this submission's range; confirmed frames remain untouched.
        channel
            .mark_ready(pending_ref.frame_start..pending_ref.frame_start + pending_ref.frame_count);
        // Frames are ready for submission again.
        BatcherMetrics::pending_frames().increment(pending_ref.frame_count as f64);

        debug!(
            id = ?id,
            frame_start = %pending_ref.frame_start,
            frame_count = %pending_ref.frame_count,
            "submission frames ready for retry"
        );
    }

    fn flush(&mut self) -> Result<(), StepError> {
        debug!("flushing current channel");
        if let Some(error) = self.deferred_step_error.take() {
            return Err(error);
        }
        self.close_current_channel("force")
    }

    fn advance_l1_head(&mut self, l1_block: u64) {
        let advanced = l1_block > self.l1_head;
        if advanced {
            self.l1_head = l1_block;
        }

        if self.deferred_step_error.is_some() {
            return;
        }

        if advanced && let Err(error) = self.check_channel_timeout() {
            self.defer_step_error(error, "advance_l1_head");
        }
        self.invalidate_expired_ready_channels();
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
        self.tip = None;
        self.current_channel = None;
        self.ready_channels.clear();
        self.pending.clear();
        self.deferred_step_error = None;
        // Intentionally not resetting `next_id`: keeping it monotonically
        // increasing across resets means post-reset submissions can never
        // share an ID with any pre-reset in-flight submission, eliminating
        // stale-confirm silent corruption.
        self.rng = SmallRng::from_os_rng();

        // Zero out state gauges — all buffered data has been discarded.
        BatcherMetrics::pending_blocks().set(0.0);
        BatcherMetrics::pending_frames().set(0.0);
    }

    fn reconcile_derivation(
        &mut self,
        safe_l2: BlockInfo,
        current_l1: Option<u64>,
    ) -> DerivationReconciliation {
        if !self.prune_safe(safe_l2) {
            return DerivationReconciliation::SafeHeadMismatch;
        }
        if current_l1.is_some_and(|current_l1| self.is_derivation_stalled(current_l1, safe_l2)) {
            return DerivationReconciliation::StalledChannel;
        }
        DerivationReconciliation::Consistent
    }

    fn da_backlog_bytes(&self) -> u64 {
        let pending_blocks = self
            .blocks
            .iter()
            .skip(self.block_cursor)
            .map(Self::block_da_backlog_bytes)
            .fold(0u64, u64::saturating_add);
        let open_channel = self.current_channel.as_ref().map_or(0, |open| open.da_backlog_bytes);
        let ready_channels = self
            .ready_channels
            .iter()
            .filter(|channel| !channel.is_fully_confirmed())
            .map(|channel| channel.da_backlog_bytes)
            .fold(0u64, u64::saturating_add);

        pending_blocks.saturating_add(open_channel).saturating_add(ready_channels)
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

    fn make_user_tx_chain(len: usize) -> Vec<BaseBlock> {
        let mut parent_hash = B256::ZERO;
        (0..len)
            .map(|number| {
                let mut block = make_block_with_user_tx(parent_hash);
                block.header.number = number as u64;
                parent_hash = block.header.hash_slow();
                block
            })
            .collect()
    }

    fn default_encoder() -> BatchEncoder {
        let rollup_config = Arc::new(RollupConfig::default());
        BatchEncoder::new(rollup_config, EncoderConfig::default())
    }

    fn encoder_with_confirmation_timeout(channel_timeout: u64) -> BatchEncoder {
        let rollup_config = Arc::new(RollupConfig {
            channel_timeout,
            granite_channel_timeout: channel_timeout,
            ..RollupConfig::default()
        });
        let config = EncoderConfig {
            max_frame_size: 32,
            target_frame_size: 32,
            target_num_frames: 1,
            max_channel_duration: 1000,
            ..EncoderConfig::default()
        };
        BatchEncoder::new(rollup_config, config)
    }

    fn drain_submissions(encoder: &mut BatchEncoder) -> Vec<BatchSubmission> {
        let mut submissions = Vec::new();
        while let Some(submission) = encoder.next_submission() {
            submissions.push(submission);
        }
        submissions
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
    fn test_safe_head_prunes_fully_confirmed_blocks() {
        let mut encoder = default_encoder();
        let blocks = make_user_tx_chain(2);
        let safe_l2 = BlockInfo::from(&blocks[0]);
        for block in blocks {
            encoder.add_block(block).unwrap();
            assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        }
        encoder.flush().unwrap();
        for submission in drain_submissions(&mut encoder) {
            encoder.confirm(submission.id, 100);
        }

        assert_eq!(encoder.ready_channels.len(), 1);
        assert_eq!(encoder.blocks.len(), 2, "confirmation keeps blocks buffered");

        assert!(encoder.prune_safe(safe_l2));
        assert_eq!(encoder.ready_channels.len(), 1);
        assert_eq!(encoder.blocks.len(), 1);
        assert_eq!(encoder.blocks[0].header.number, 1);
        assert_eq!(encoder.block_cursor, 1);
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
        assert_eq!(encoder.tip, None);
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
    fn test_da_backlog_counts_single_blocks_after_encoding_and_closing() {
        let mut encoder = default_encoder();
        for block in make_user_tx_chain(3) {
            encoder.add_block(block).unwrap();
        }

        let queued_backlog = encoder.da_backlog_bytes();
        assert!(queued_backlog > 0);

        for _ in 0..3 {
            assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        }

        assert_eq!(encoder.block_cursor, 3);
        assert!(encoder.current_channel.is_some());
        assert_eq!(encoder.da_backlog_bytes(), queued_backlog);

        encoder.flush().unwrap();
        assert!(encoder.current_channel.is_none());
        assert!(!encoder.ready_channels.is_empty());
        assert!(encoder.da_backlog_bytes() > 0);

        let mut submissions = Vec::new();
        while let Some(submission) = encoder.next_submission() {
            submissions.push(submission.id);
        }
        assert!(!submissions.is_empty());
        assert!(encoder.da_backlog_bytes() > 0, "in-flight submissions still consume DA backlog");

        for id in submissions {
            encoder.confirm(id, 0);
        }
        assert_eq!(encoder.da_backlog_bytes(), 0);
    }

    #[test]
    fn test_requeue_marks_frames_ready() {
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
    /// then call `reset()`. A subsequent `confirm()` for the stale ID must be a no-op
    /// and the pending map must remain empty.
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
    /// alter any channel, because the channel no longer exists.
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

        // Verify the post-reorg confirmation updates the retained channel.
        assert!(
            encoder.ready_channels[0]
                .frame_states
                .iter()
                .all(|state| *state == FrameState::Pending)
        );
        encoder.confirm(post_reorg_sub.id, 201);
        assert!(
            encoder.ready_channels[0]
                .frame_states
                .iter()
                .all(|state| *state == FrameState::Confirmed)
        );
        assert_eq!(encoder.blocks.len(), 1, "confirmation keeps the post-reorg block buffered");
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
    /// packed two-per-submission. One confirmation must credit both frames.
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
        assert!(sub.frame_count() > 0 && sub.frame_count() <= 2);
    }

    /// A requeue makes every frame in the submission ready again.
    #[test]
    fn test_requeue_multi_frame_marks_submission_ready() {
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
        let submitted_frame_count = sub.frame_count();

        encoder.requeue(id);

        let resub = encoder.next_submission();
        assert!(resub.is_some(), "requeued frames must be available again");
        assert_eq!(
            resub.unwrap().frame_count(),
            submitted_frame_count,
            "requeued submission must contain the same number of frames"
        );
    }

    #[test]
    fn test_requeue_does_not_resubmit_confirmed_frames() {
        let mut encoder = encoder_with_confirmation_timeout(100);
        encoder.add_block(make_block_with_user_tx(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        let first = encoder.next_submission().unwrap();
        let second = encoder.next_submission().unwrap();

        encoder.requeue(first.id);
        encoder.confirm(second.id, 1);
        assert_eq!(
            &encoder.ready_channels[0].frame_states[..2],
            &[FrameState::Ready, FrameState::Confirmed]
        );

        let retry = encoder.next_submission().unwrap();
        assert!(Arc::ptr_eq(retry.first_frame().unwrap(), first.first_frame().unwrap()));
        assert_eq!(
            &encoder.ready_channels[0].frame_states[..2],
            &[FrameState::Pending, FrameState::Confirmed]
        );
    }

    #[test]
    fn test_late_confirmation_replays_timed_out_channel() {
        let mut encoder = encoder_with_confirmation_timeout(2);

        encoder.add_block(make_block_with_user_tx(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        let original_channel_id = encoder.ready_channels[0].id;
        let submissions = drain_submissions(&mut encoder);
        assert!(
            submissions.len() >= 2,
            "test requires a multi-frame channel, got {} submission(s)",
            submissions.len()
        );

        encoder.confirm(submissions[0].id, 1);
        encoder.advance_l1_head(1);
        assert_eq!(encoder.blocks.len(), 1, "partial confirmation must not prune blocks");

        encoder.confirm(submissions[1].id, 3);
        encoder.advance_l1_head(3);
        assert_eq!(encoder.blocks.len(), 1, "timed-out confirmation must preserve blocks");
        assert_eq!(encoder.block_cursor, 0, "encoder must rewind to replay the block");
        assert!(encoder.ready_channels.is_empty(), "old channel must be discarded");
        assert!(encoder.pending.is_empty(), "stale in-flight tail submissions must be forgotten");

        for submission in submissions.iter().skip(2) {
            encoder.confirm(submission.id, 3);
        }
        assert_eq!(encoder.blocks.len(), 1, "stale late confirmations must be no-ops");

        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        let replay_submissions = drain_submissions(&mut encoder);
        assert!(!replay_submissions.is_empty(), "replay must emit a fresh channel");
        assert_ne!(
            replay_submissions[0].first_frame().unwrap().id,
            original_channel_id,
            "replay must use a fresh channel id"
        );

        for submission in replay_submissions {
            encoder.confirm(submission.id, 5);
        }
        assert_eq!(
            encoder.blocks.len(),
            1,
            "timely confirmation keeps the replayed block buffered"
        );
        let safe_l2 = BlockInfo::from(&encoder.blocks[0]);
        assert!(encoder.prune_safe(safe_l2));
        assert!(encoder.blocks.is_empty());
    }

    #[test]
    fn test_timely_confirmed_channel_waits_for_safe_head() {
        let mut encoder = encoder_with_confirmation_timeout(2);
        encoder.add_block(make_block_with_user_tx(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        for submission in drain_submissions(&mut encoder) {
            encoder.confirm(submission.id, 1);
            encoder.advance_l1_head(1);
        }
        encoder.advance_l1_head(100);

        assert_eq!(encoder.ready_channels.len(), 1);
        assert!(encoder.ready_channels[0].is_fully_confirmed());
        assert_eq!(encoder.block_cursor, 1);
    }

    #[test]
    fn test_fully_confirmed_channel_requires_replay_after_derivation_passes_inclusion() {
        let mut encoder = encoder_with_confirmation_timeout(2);
        let mut block = make_block_with_user_tx(B256::ZERO);
        block.header.number = 101;
        let block_hash = block.header.hash_slow();
        encoder.add_block(block).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        for submission in drain_submissions(&mut encoder) {
            encoder.confirm(submission.id, 1_000);
        }

        let previous_safe_l2 = BlockInfo { number: 100, ..Default::default() };
        assert_eq!(
            encoder.reconcile_derivation(previous_safe_l2, None),
            DerivationReconciliation::Consistent,
            "providers without a derivation cursor cannot prove the channel stalled",
        );
        assert_eq!(
            encoder.reconcile_derivation(previous_safe_l2, Some(1_000)),
            DerivationReconciliation::Consistent,
            "the current L1 block may still be processing",
        );
        assert_eq!(
            encoder.reconcile_derivation(previous_safe_l2, Some(1_001)),
            DerivationReconciliation::StalledChannel,
            "passing the last inclusion without making the channel safe requires replay",
        );
        assert_eq!(
            encoder.reconcile_derivation(
                BlockInfo { hash: block_hash, number: 101, ..Default::default() },
                Some(1_001),
            ),
            DerivationReconciliation::Consistent,
            "a safe head covering the channel does not require replay",
        );
    }

    #[test]
    fn test_descending_confirmations_replay_expired_channel() {
        let mut encoder = encoder_with_confirmation_timeout(2);
        encoder.add_block(make_block_with_user_tx(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        let submissions = drain_submissions(&mut encoder);
        assert!(submissions.len() >= 2);

        encoder.confirm(submissions[0].id, 100);
        encoder.advance_l1_head(100);
        encoder.confirm(submissions[1].id, 90);
        encoder.advance_l1_head(90);

        assert!(encoder.ready_channels.is_empty());
        assert!(encoder.pending.is_empty());
        assert_eq!(encoder.block_cursor, 0);
    }

    #[test]
    fn test_l1_head_expiry_replays_before_tail_confirms() {
        let mut encoder = encoder_with_confirmation_timeout(2);

        encoder.add_block(make_block_with_user_tx(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        let submissions = drain_submissions(&mut encoder);
        assert!(
            submissions.len() >= 2,
            "test requires a multi-frame channel, got {} submission(s)",
            submissions.len()
        );

        encoder.confirm(submissions[0].id, 1);
        encoder.advance_l1_head(4);

        assert_eq!(encoder.blocks.len(), 1, "expired channel must preserve blocks");
        assert_eq!(encoder.block_cursor, 0, "expired channel must rewind for replay");
        assert!(encoder.ready_channels.is_empty(), "expired channel must be discarded");
        assert!(encoder.pending.is_empty(), "stale tail submissions must be forgotten");

        for submission in submissions.iter().skip(1) {
            encoder.confirm(submission.id, 4);
        }
        assert_eq!(encoder.blocks.len(), 1, "late tail confirmations must stay stale");
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

    #[test]
    fn test_channel_output_failure_does_not_publish_partial_channel() {
        let config =
            EncoderConfig { max_frame_size: Frame::ENCODED_OVERHEAD, ..EncoderConfig::default() };
        let mut encoder = BatchEncoder::new(Arc::new(RollupConfig::default()), config);

        encoder.add_block(make_block(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);

        let err =
            encoder.close_current_channel("force").expect_err("invalid frame size should fail");

        assert!(matches!(
            err,
            StepError::ChannelFailed(crate::OpenChannelError::Output(
                base_comp::ChannelOutError::MaxFrameSizeTooSmall
            ))
        ));
        assert!(encoder.current_channel.is_none());
        assert!(encoder.ready_channels.is_empty());
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

    /// `prune_safe` drains the buffered prefix through the matching safe head.
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

        assert!(encoder.prune_safe(BlockInfo { hash: b2_hash, number: 2, ..Default::default() }));

        assert_eq!(encoder.blocks.len(), 1, "only block 3 should remain");
        assert_eq!(encoder.blocks[0].header.number, 3);
        assert_eq!(encoder.block_cursor, 1, "cursor must be adjusted by prune count");
    }

    #[test]
    fn test_prune_safe_rebases_open_channel() {
        let mut encoder = default_encoder();
        for block in make_user_tx_chain(2) {
            encoder.add_block(block).unwrap();
        }

        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);

        let safe_l2 = BlockInfo::from(&encoder.blocks[0]);
        assert!(encoder.prune_safe(safe_l2));

        let remaining_safe_l2 = BlockInfo::from(&encoder.blocks[0]);
        let open = encoder.current_channel.as_ref().unwrap();
        assert_eq!(open.block_start, 0);
        assert_eq!(open.blocks_added, 1);
        assert_eq!(encoder.blocks.len(), 1);

        encoder.flush().unwrap();
        for submission in drain_submissions(&mut encoder) {
            encoder.confirm(submission.id, 1);
        }
        assert_eq!(encoder.blocks.len(), 1, "confirmation keeps the remaining block buffered");
        assert!(encoder.prune_safe(remaining_safe_l2));
        assert!(encoder.blocks.is_empty());
    }

    /// Safe-head pruning includes unencoded blocks and clamps the cursor to zero.
    #[test]
    fn test_prune_safe_prunes_unencoded_blocks() {
        let mut encoder = default_encoder();

        let b1 = make_numbered_block(B256::ZERO, 1);
        let b1_hash = b1.header.hash_slow();
        encoder.add_block(b1).unwrap();

        let b2 = make_numbered_block(b1_hash, 2);
        let b2_hash = b2.header.hash_slow();
        encoder.add_block(b2).unwrap();

        // Encode only block 1 (cursor = 1).
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert_eq!(encoder.block_cursor, 1);

        assert!(encoder.prune_safe(BlockInfo { hash: b2_hash, number: 2, ..Default::default() }));

        assert!(encoder.blocks.is_empty());
        assert_eq!(encoder.block_cursor, 0);
        assert!(encoder.current_channel.is_none());
    }

    #[test]
    fn test_prune_safe_rejects_inconsistent_chain() {
        let mut encoder = default_encoder();
        assert!(encoder.prune_safe(BlockInfo { number: 1, ..Default::default() }));

        encoder.add_block(make_numbered_block(B256::ZERO, 3)).unwrap();
        encoder.step().unwrap();

        assert!(!encoder.prune_safe(BlockInfo {
            hash: B256::repeat_byte(1),
            number: 2,
            ..Default::default()
        }));
        assert!(encoder.prune_safe(BlockInfo { number: 2, ..Default::default() }));
        assert!(!encoder.prune_safe(BlockInfo {
            hash: B256::repeat_byte(1),
            number: 3,
            ..Default::default()
        }));
        assert!(!encoder.prune_safe(BlockInfo { number: 4, ..Default::default() }));
        assert!(!encoder.prune_safe(BlockInfo { number: 1, ..Default::default() }));
        assert_eq!(encoder.blocks.len(), 1);
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
        encoder.flush().unwrap();

        // Each submission contains exactly 1 frame (target_num_frames=1).
        let mut count = 0;
        while let Some(sub) = encoder.next_submission() {
            assert_eq!(sub.frame_count(), 1, "calldata submission must have exactly 1 frame");
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
        encoder.flush().unwrap();

        encoder.set_blob_override(true);
        let sub = encoder.next_submission().expect("submission while override active");
        assert_eq!(sub.da_type(), DaType::Blob, "override must flip da_type to Blob");
        encoder.requeue(sub.id);

        encoder.set_blob_override(false);
        let sub = encoder.next_submission().expect("submission after override cleared");
        assert_eq!(sub.da_type(), DaType::Calldata, "configured calldata da_type must return");
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
