//! The [`BatchEncoder`] implementation.

use std::{collections::VecDeque, fmt, sync::Arc};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::B256;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_flz::tx_estimated_size_fjord_bytes;
use base_common_genesis::RollupConfig;
use base_protocol::{BlockInfo, ChannelId};
use rand::{RngCore, SeedableRng, rngs::SmallRng};
use tracing::{debug, warn};

use crate::{
    ArtifactId, BatchComposer, BatchPipeline, BatchSubmission, BatcherMetrics, Channel,
    ChannelAddOutcome, ChannelCloseReason, DaEgress, DaType, DerivationReconciliation,
    EncoderConfig, EncoderConfigError, ReorgError, StepError, StepResult, SubmissionId,
};

/// The batcher encoding pipeline state machine.
///
/// Transforms L2 blocks into calldata or blob L1 submissions. No async, no I/O.
/// The caller drives the encoder synchronously via the [`BatchPipeline`] trait.
pub struct BatchEncoder {
    /// The rollup configuration.
    rollup_config: Arc<RollupConfig>,
    /// Encoder-specific configuration.
    config: EncoderConfig,
    /// Current L1 head block number (for channel duration tracking).
    l1_head: u64,
    /// Buffered L2 blocks above the latest observed safe head.
    blocks: VecDeque<BaseBlock>,
    /// Index into `blocks`: next block not yet appended to the writable channel tail.
    block_cursor: usize,
    /// Hash of the last accepted block or safe-head anchor.
    tip: Option<B256>,
    /// Append-only channel FIFO. At most its tail may remain open.
    channels: VecDeque<Channel>,
    /// Streaming DA artifact builder and immutable submission ledger.
    egress: DaEgress,
    /// Next submission id counter.
    next_id: u64,
    /// Per-instance RNG for generating unique channel IDs.
    rng: SmallRng,
    /// When throttling requests it, emit blobs even if `da_type` is calldata.
    /// No-op when blob DA is already configured.
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
            .field("channels", &self.channels.len())
            .field("egress_artifacts", &self.egress.artifacts().len())
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl BatchEncoder {
    /// Creates a [`BatchEncoder`] after validating all structural limits.
    ///
    /// # Errors
    ///
    /// Returns [`EncoderConfigError`] when `config` would violate an encoder or
    /// derivation invariant.
    pub fn new(
        rollup_config: Arc<RollupConfig>,
        config: EncoderConfig,
    ) -> Result<Self, EncoderConfigError> {
        config.validate()?;
        Ok(Self {
            rollup_config,
            config,
            l1_head: 0,
            blocks: VecDeque::new(),
            block_cursor: 0,
            tip: None,
            channels: VecDeque::new(),
            egress: DaEgress::new(),
            next_id: 0,
            rng: SmallRng::from_os_rng(),
            blob_override: false,
            deferred_step_error: None,
        })
    }

    /// Estimate the DA bytes represented by non-deposit transactions in `block`.
    ///
    /// Uses the same per-transaction `FastLZ` estimate the block builder applies to
    /// its DA limits, so the backlog and the throttle limits share one unit.
    fn block_da_backlog_bytes(block: &BaseBlock) -> u64 {
        block
            .body
            .transactions
            .iter()
            .filter(|tx| !matches!(tx, BaseTxEnvelope::Deposit(_)))
            .map(|tx| tx_estimated_size_fjord_bytes(&tx.encoded_2718()))
            .sum()
    }

    /// Steps until idle, flushes the writable channel, and drains all ready submissions.
    ///
    /// # Errors
    ///
    /// Returns the first [`StepError`]. Submissions prepared before the error remain
    /// available through [`BatchPipeline::next_submission`].
    pub fn encode_and_drain(&mut self) -> Result<Vec<BatchSubmission>, StepError> {
        loop {
            match self.step()? {
                StepResult::Idle => break,
                StepResult::BlockEncoded | StepResult::ChannelClosed => {}
            }
        }

        self.flush_channels()?;

        let mut submissions = Vec::new();
        while let Some(sub) = self.next_submission() {
            submissions.push(sub);
        }

        Ok(submissions)
    }

    /// Close the writable channel.
    fn close_current_channel(&mut self, close_reason: ChannelCloseReason) -> Result<(), StepError> {
        let Some(channel) = self.channels.back_mut().filter(|channel| channel.is_open()) else {
            return Ok(());
        };

        if channel.is_empty() {
            return Ok(());
        }

        let input_bytes = channel.input_bytes();
        let opened_l1_block = channel.opened_l1_block();
        let blocks_added = channel.blocks_added();
        let channel_id = channel.id();
        channel.close()?;

        let frames_emitted = channel.frame_count();
        let duration_blocks = self.l1_head.saturating_sub(opened_l1_block);
        let compressed_bytes = channel.compressed_bytes();

        debug!(
            channel_id = ?channel_id,
            frames_emitted = %frames_emitted,
            encoded_block_range_start = %channel.block_range().start,
            encoded_block_range_end = %channel.block_range().end,
            close_reason = %close_reason.metric_label(),
            duration_blocks = %duration_blocks,
            input_bytes = %input_bytes,
            compressed_bytes = %compressed_bytes,
            "closed channel"
        );

        // Close metrics.
        BatcherMetrics::channel_closed_total(close_reason.metric_label()).increment(1);
        BatcherMetrics::channel_duration_blocks().record(duration_blocks as f64);
        BatcherMetrics::l2_blocks_per_channel().record(blocks_added as f64);
        BatcherMetrics::input_bytes_total().increment(input_bytes);
        BatcherMetrics::output_bytes_total().increment(compressed_bytes);
        if input_bytes > 0 {
            let ratio = compressed_bytes as f64 / input_bytes as f64;
            BatcherMetrics::channel_compression_ratio().record(ratio);
        }

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

    /// Opens a channel for the next queued block.
    ///
    /// `block_start` anchors its buffered block range; the current L1 head starts
    /// its close deadline.
    fn open_new_channel(&mut self, block_start: usize) {
        let mut id = ChannelId::default();
        self.rng.fill_bytes(&mut id);
        let channel = Channel::new(
            id,
            Arc::clone(&self.rollup_config),
            &self.config,
            block_start,
            self.l1_head,
        )
        .expect("BatchEncoder validates its channel configuration at construction");
        debug!(
            channel_id = ?id,
            block_start = %block_start,
            l1_head = %self.l1_head,
            "opened new channel"
        );
        BatcherMetrics::channel_opened_total().increment(1);
        self.channels.push_back(channel);
    }

    /// Closes the writable channel if its duration has elapsed.
    ///
    /// Called from both [`BatchPipeline::step`] and [`BatchPipeline::advance_l1_head`],
    /// so closure does not depend on another L2 block arriving.
    fn check_channel_timeout(&mut self) -> Result<bool, StepError> {
        // Same deadline for closing an open channel and releasing a closed partial tail.
        let should_close = self
            .channels
            .back()
            .is_some_and(|channel| channel.is_open() && channel.deadline_due(self.l1_head));

        if should_close {
            debug!(l1_head = %self.l1_head, "channel timed out, closing");
            self.close_current_channel(ChannelCloseReason::Timeout)?;
        }

        Ok(should_close)
    }

    /// Returns the conservative protocol channel timeout used for confirmation windows.
    fn confirmation_channel_timeout(&self) -> u64 {
        EncoderConfig::confirmation_channel_timeout(&self.rollup_config)
    }

    /// Invalidates one channel and every atomic artifact or submission dependency.
    fn invalidate_channel(
        &mut self,
        channel_idx: usize,
        observed_l1_block: u64,
        channel_timeout: u64,
    ) {
        let Some(channel) = self.channels.get(channel_idx) else {
            return;
        };
        let expired_channel_id = channel.id();
        let first_confirmed_l1_block = channel.first_confirmed_l1_block();
        let last_confirmed_l1_block = channel.last_confirmed_l1_block();
        let mut affected_channels = vec![expired_channel_id];
        let mut affected_artifacts = Vec::<ArtifactId>::new();

        // Blobs and transactions are atomic. Expand replay until every channel
        // contributing to an affected artifact or submission is included.
        loop {
            let channel_count = affected_channels.len();
            let artifact_count = affected_artifacts.len();

            for artifact in self.egress.artifacts() {
                if artifact.channel_ids().iter().any(|id| affected_channels.contains(id))
                    || affected_artifacts.contains(&artifact.id())
                {
                    if !affected_artifacts.contains(&artifact.id()) {
                        affected_artifacts.push(artifact.id());
                    }
                    for channel_id in artifact.channel_ids() {
                        if !affected_channels.contains(channel_id) {
                            affected_channels.push(*channel_id);
                        }
                    }
                }
            }

            self.egress.extend_with_submission_artifacts(&mut affected_artifacts);

            if let Some(replay_idx) =
                self.channels.iter().position(|channel| affected_channels.contains(&channel.id()))
            {
                for channel in self.channels.iter().skip(replay_idx) {
                    if !affected_channels.contains(&channel.id()) {
                        affected_channels.push(channel.id());
                    }
                }
            }

            if affected_channels.len() == channel_count
                && affected_artifacts.len() == artifact_count
            {
                break;
            }
        }

        let replay_idx = self
            .channels
            .iter()
            .position(|channel| affected_channels.contains(&channel.id()))
            .unwrap_or(channel_idx);
        let replay_from = self.channels[replay_idx].block_range().start;

        warn!(
            channel_id = ?expired_channel_id,
            first_confirmed_l1_block = ?first_confirmed_l1_block,
            last_confirmed_l1_block = ?last_confirmed_l1_block,
            observed_l1_block = %observed_l1_block,
            channel_timeout = %channel_timeout,
            replay_from_block_index = %replay_from,
            "confirmed channel exceeded derivation timeout, replaying blocks"
        );

        BatcherMetrics::channel_replay_total().increment(1);
        self.egress.invalidate_artifacts(&affected_artifacts);
        BatcherMetrics::pending_frames().set(self.egress.artifacts().ready_frame_count() as f64);
        self.channels.truncate(replay_idx);
        self.block_cursor = self.block_cursor.min(replay_from);
    }

    /// Invalidates the first channel whose derivation confirmation window expired.
    fn invalidate_expired_channels(&mut self) {
        let channel_timeout = self.confirmation_channel_timeout();
        let Some(channel_idx) = self.channels.iter().position(|channel| {
            let Some(first) = channel.first_confirmed_l1_block() else {
                return false;
            };
            let inclusion_span =
                channel.last_confirmed_l1_block().unwrap_or(first).saturating_sub(first);
            if inclusion_span > channel_timeout {
                return true;
            }

            let incomplete = !self.egress.channel_fully_confirmed(channel);
            incomplete && self.l1_head > first.saturating_add(channel_timeout)
        }) else {
            return;
        };

        self.invalidate_channel(channel_idx, self.l1_head, channel_timeout);
    }

    /// Rebase all block-queue-relative offsets after pruning a prefix from `blocks`.
    fn rebase_after_block_prune(&mut self, prune_count: usize) {
        self.block_cursor = self.block_cursor.saturating_sub(prune_count);
        for channel in &mut self.channels {
            channel.rebase_after_prune(prune_count);
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
        let next_safe = safe_l2.number + 1;
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

        // Remove channels fully covered by the safe head. Stable artifact IDs
        // require no positional rebasing.
        let channels_to_prune = self
            .channels
            .iter()
            .take_while(|channel| channel.block_range().end <= prune_count)
            .count();
        if channels_to_prune > 0 {
            let channel_ids: Vec<_> =
                self.channels.iter().take(channels_to_prune).map(Channel::id).collect();
            self.channels.drain(..channels_to_prune);
            self.egress.prune_channels(&channel_ids);
            BatcherMetrics::pending_frames()
                .set(self.egress.artifacts().ready_frame_count() as f64);
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
        self.channels.iter().any(|channel| {
            if !self.egress.channel_fully_confirmed(channel) {
                return false;
            }

            let Some(last_inclusion) = channel.last_confirmed_l1_block() else {
                return false;
            };
            if current_l1 <= last_inclusion {
                return false;
            }

            channel
                .block_range()
                .end
                .checked_sub(1)
                .and_then(|last_block_index| self.blocks.get(last_block_index))
                .is_some_and(|last_block| safe_l2.number < last_block.header.number)
        })
    }

    /// Closes the writable tail and makes retained partial channel output eligible for framing.
    fn flush_channels(&mut self) -> Result<(), StepError> {
        self.close_current_channel(ChannelCloseReason::Flush)?;
        for channel in &mut self.channels {
            channel.release_at(self.l1_head);
        }
        Ok(())
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
        // One transition: deferred error, timeout close, or one queued block.
        if let Some(error) = self.deferred_step_error.take() {
            return Err(error);
        }

        if self.check_channel_timeout()? {
            return Ok(StepResult::ChannelClosed);
        }

        if self.block_cursor >= self.blocks.len() {
            return Ok(StepResult::Idle);
        }

        let block = &self.blocks[self.block_cursor];
        let block_da_backlog_bytes = Self::block_da_backlog_bytes(block);

        // Composition failure is fatal: skipping the block would gap the L2 sequence.
        let single_batch = BatchComposer::block_to_single_batch(block)
            .map_err(|source| StepError::CompositionFailed { cursor: self.block_cursor, source })?;

        if !self.channels.back().is_some_and(Channel::is_open) {
            self.open_new_channel(self.block_cursor);
        }

        let channel = self.channels.back_mut().expect("channel exists after open_new_channel");
        let outcome = channel.add_batch(&single_batch, block_da_backlog_bytes)?;

        match outcome {
            accepted @ (ChannelAddOutcome::Accepted | ChannelAddOutcome::TargetReached) => {
                // Cursor advances only after accept, so a later reject retries this block.
                self.block_cursor += 1;
                if accepted == ChannelAddOutcome::TargetReached {
                    self.close_current_channel(ChannelCloseReason::SoftTarget)?;
                    Ok(StepResult::ChannelClosed)
                } else {
                    Ok(StepResult::BlockEncoded)
                }
            }
            ChannelAddOutcome::Rejected(limit) => {
                // Empty channel: this block cannot fit anywhere. Discard and fail.
                if channel.is_empty() {
                    self.channels.pop_back();
                    BatcherMetrics::channel_closed_total(BatcherMetrics::REASON_DISCARD)
                        .increment(1);
                    return Err(StepError::BlockExceedsChannelLimit {
                        cursor: self.block_cursor,
                        limit,
                    });
                }

                // Close what we have; next step retries this block in a new channel.
                debug!(%limit, "channel reached a protocol size limit, closing");
                self.close_current_channel(ChannelCloseReason::ProtocolLimit)?;
                Ok(StepResult::ChannelClosed)
            }
        }
    }

    fn next_submission(&mut self) -> Option<BatchSubmission> {
        let effective_da_type = if self.blob_override && self.config.da_type == DaType::Calldata {
            DaType::Blob
        } else {
            self.config.da_type
        };
        let id = SubmissionId(self.next_id);
        let submission = self.egress.next_submission(
            &mut self.channels,
            effective_da_type,
            self.l1_head,
            self.config.max_blobs_per_tx,
            id,
        )?;

        self.next_id += 1;
        let frame_count = submission.frame_count();
        let blob_count = submission.blob_count();
        BatcherMetrics::pending_frames().set(self.egress.artifacts().ready_frame_count() as f64);
        debug!(
            id = %id.0,
            frame_count = %frame_count,
            blob_count = %blob_count,
            "dequeued DA artifacts for submission"
        );

        Some(submission)
    }

    fn confirm(&mut self, id: SubmissionId, l1_block: u64) {
        let Some(channel_ids) = self.egress.confirm(id) else {
            debug!(id = ?id, "ignoring confirmation for untracked submission");
            return;
        };

        for channel_id in channel_ids {
            if let Some(channel) =
                self.channels.iter_mut().find(|channel| channel.id() == channel_id)
            {
                channel.record_confirmation(l1_block);
            }
            if self
                .channels
                .iter()
                .find(|channel| channel.id() == channel_id)
                .is_some_and(|channel| self.egress.channel_fully_confirmed(channel))
            {
                debug!(channel_id = ?channel_id, "channel fully confirmed");
                BatcherMetrics::channel_fully_submitted_total().increment(1);
            }
        }
    }

    fn requeue(&mut self, id: SubmissionId) {
        let Some(frame_count) = self.egress.requeue(id) else {
            debug!(id = ?id, "ignoring retry for untracked submission");
            return;
        };

        BatcherMetrics::pending_frames().set(self.egress.artifacts().ready_frame_count() as f64);

        debug!(
            id = ?id,
            frame_count = %frame_count,
            "submission frames ready for retry"
        );
    }

    fn flush(&mut self) -> Result<(), StepError> {
        debug!("flushing channel pipeline");
        self.flush_channels()
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

        self.invalidate_expired_channels();
    }

    fn reset(&mut self) {
        warn!(
            pending_blocks = %self.blocks.len(),
            channels = %self.channels.len(),
            in_pending = %self.egress.pending_submission_count(),
            "resetting encoder pipeline (reorg or explicit reset)"
        );
        self.blocks.clear();
        self.block_cursor = 0;
        self.tip = None;
        self.channels.clear();
        self.egress.reset();
        self.deferred_step_error = None;
        // Keep `next_id` monotonic across reset so stale confirms cannot collide.
        self.rng = SmallRng::from_os_rng();

        // Zero out state gauges — all buffered data has been discarded.
        BatcherMetrics::pending_blocks().set(0.0);
        BatcherMetrics::pending_frames().set(0.0);
    }

    fn reconcile_derivation(
        &mut self,
        safe_l2: BlockInfo,
        current_l1: u64,
    ) -> DerivationReconciliation {
        if !self.prune_safe(safe_l2) {
            return DerivationReconciliation::SafeHeadMismatch;
        }
        if self.is_derivation_stalled(current_l1, safe_l2) {
            return DerivationReconciliation::StalledChannel;
        }
        DerivationReconciliation::Consistent
    }

    fn da_backlog_bytes(&self) -> u64 {
        let pending_blocks: u64 =
            self.blocks.iter().skip(self.block_cursor).map(Self::block_da_backlog_bytes).sum();
        let channels: u64 = self
            .channels
            .iter()
            .filter(|channel| !self.egress.channel_fully_confirmed(channel))
            .map(Channel::da_backlog_bytes)
            .sum();

        pending_blocks + channels
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
