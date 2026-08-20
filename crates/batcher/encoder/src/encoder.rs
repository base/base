//! The [`BatchEncoder`] implementation.

use std::{collections::VecDeque, fmt, sync::Arc};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::B256;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_genesis::RollupConfig;
use base_protocol::{BlockInfo, ChannelId};
use rand::{RngCore, SeedableRng, rngs::SmallRng};
use tracing::{debug, warn};

use crate::{
    ArtifactId, BatchComposer, BatchPipeline, BatchSubmission, BatcherMetrics, ChannelAddOutcome,
    ChannelCloseReason, ChannelRecord, DaEgress, DaType, DerivationReconciliation, EncoderConfig,
    EncoderConfigError, ReorgError, StepError, StepResult, SubmissionId,
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
    /// Append-only channel FIFO. At most its tail may remain open.
    channels: VecDeque<ChannelRecord>,
    /// Streaming DA artifact builder and immutable submission ledger.
    egress: DaEgress,
    /// Next submission id counter.
    next_id: u64,
    /// Per-instance RNG for generating unique channel IDs.
    rng: SmallRng,
    /// When set, emit blobs even if `da_type` is calldata.
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
    fn block_da_backlog_bytes(block: &BaseBlock) -> u64 {
        block
            .body
            .transactions
            .iter()
            .filter(|tx| !matches!(tx, BaseTxEnvelope::Deposit(_)))
            .map(|tx| tx.encode_2718_len() as u64)
            .sum()
    }

    /// Step until idle, flush, and drain submissions. Test/one-shot helper.
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
        BatcherMetrics::input_bytes(BatcherMetrics::STAGE_CLOSED).set(input_bytes as f64);
        BatcherMetrics::output_bytes().set(compressed_bytes as f64);
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
    fn open_new_channel(&mut self, block_start: usize) {
        let mut id = ChannelId::default();
        self.rng.fill_bytes(&mut id);
        let channel = ChannelRecord::new(
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

    /// Close the writable channel if its duration has elapsed.
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

        self.egress.invalidate_artifacts(&affected_artifacts);
        BatcherMetrics::pending_frames().set(self.egress.ready_frame_count() as f64);
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
                self.channels.iter().take(channels_to_prune).map(ChannelRecord::id).collect();
            self.channels.drain(..channels_to_prune);
            self.egress.prune_channels(&channel_ids);
            BatcherMetrics::pending_frames().set(self.egress.ready_frame_count() as f64);
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

    /// Closes the writable tail and releases every retained partial artifact.
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

        if !self.channels.back().is_some_and(ChannelRecord::is_open) {
            self.open_new_channel(self.block_cursor);
        }

        let channel = self.channels.back_mut().expect("channel exists after open_new_channel");
        let outcome = channel.add_batch(single_batch, block_da_backlog_bytes)?;

        match outcome {
            accepted @ (ChannelAddOutcome::Accepted | ChannelAddOutcome::TargetReached) => {
                // Cursor advances only after accept, so a later reject retries this block.
                BatcherMetrics::input_bytes(BatcherMetrics::STAGE_ADDED)
                    .set(channel.input_bytes() as f64);
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
        BatcherMetrics::pending_frames().set(self.egress.ready_frame_count() as f64);
        if let Some(channel) = self.channels.iter().rev().find(|channel| channel.framing_complete())
        {
            BatcherMetrics::channel_num_frames().set(channel.frame_count() as f64);
        }
        debug!(
            id = %id.0,
            frame_count = %frame_count,
            blob_count = %blob_count,
            "dequeued DA artifacts for submission"
        );

        Some(submission)
    }

    fn has_ready_submission(&self) -> bool {
        let effective_da_type = if self.blob_override && self.config.da_type == DaType::Calldata {
            DaType::Blob
        } else {
            self.config.da_type
        };
        self.egress.has_ready_submission(&self.channels, effective_da_type, self.l1_head)
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

        BatcherMetrics::pending_frames().set(self.egress.ready_frame_count() as f64);

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
        let pending_blocks: u64 =
            self.blocks.iter().skip(self.block_cursor).map(Self::block_da_backlog_bytes).sum();
        let channels: u64 = self
            .channels
            .iter()
            .filter(|channel| !self.egress.channel_fully_confirmed(channel))
            .map(ChannelRecord::da_backlog_bytes)
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

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
    use alloy_primitives::{Bytes, Sealed, Signature};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_protocol::{Frame, L1BlockInfoBedrock, L1BlockInfoTx};
    use rstest::rstest;

    use super::*;
    use crate::{BatchComposeError, ChannelLimit, CompressionAlgo, SubmissionPayload};

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

    fn make_block_with_user_tx_bytes(
        parent_hash: B256,
        payload_len: usize,
        seed: u64,
    ) -> BaseBlock {
        let mut state = seed;
        let input: Vec<u8> = (0..payload_len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let signed = TxLegacy { input: Bytes::from(input), ..Default::default() }
            .into_signed(Signature::test_signature());
        BaseBlock {
            header: Header { parent_hash, ..Default::default() },
            body: BlockBody {
                transactions: vec![make_deposit_tx(), BaseTxEnvelope::Legacy(signed)],
                ..Default::default()
            },
        }
    }

    fn make_block_with_large_user_tx(parent_hash: B256, seed: u64) -> BaseBlock {
        make_block_with_user_tx_bytes(parent_hash, 200_000, seed)
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
        BatchEncoder::new(rollup_config, EncoderConfig::default()).expect("valid default config")
    }

    #[test]
    fn new_rejects_invalid_config() {
        let config = EncoderConfig { max_blobs_per_tx: 0, ..EncoderConfig::default() };

        assert!(matches!(
            BatchEncoder::new(Arc::new(RollupConfig::default()), config),
            Err(EncoderConfigError::MaxBlobsPerTxZero)
        ));
    }

    fn encoder_with_confirmation_timeout(channel_timeout: u64) -> BatchEncoder {
        let rollup_config = Arc::new(RollupConfig {
            channel_timeout,
            granite_channel_timeout: channel_timeout,
            ..RollupConfig::default()
        });
        let config = EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: 32,
            max_blobs_per_tx: 1,
            max_channel_duration: 1000,
            ..EncoderConfig::default()
        };
        BatchEncoder::new(rollup_config, config).expect("valid test config")
    }

    fn drain_submissions(encoder: &mut BatchEncoder) -> Vec<BatchSubmission> {
        let mut submissions = Vec::new();
        while let Some(submission) = encoder.next_submission() {
            submissions.push(submission);
        }
        submissions
    }

    fn has_open_channel(encoder: &BatchEncoder) -> bool {
        encoder.channels.back().is_some_and(ChannelRecord::is_open)
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

    fn tiny_frame_zlib_encoder() -> BatchEncoder {
        let config = EncoderConfig {
            max_frame_size: Frame::ENCODED_OVERHEAD + 1,
            compression_algo: CompressionAlgo::Zlib,
            ..EncoderConfig::default()
        };
        BatchEncoder::new(Arc::new(RollupConfig::default()), config)
            .expect("valid tiny-frame config")
    }

    #[test]
    fn test_step_retries_rejected_block_after_protocol_limit_close() {
        let mut encoder = tiny_frame_zlib_encoder();
        let first = make_block_with_user_tx_bytes(B256::ZERO, 30_000, 1);
        let second = make_block_with_user_tx_bytes(first.header.hash_slow(), 30_000, 2);
        encoder.add_block(first).unwrap();
        encoder.add_block(second).unwrap();

        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert_eq!(encoder.block_cursor, 1);
        assert_eq!(encoder.channels.len(), 1);
        assert!(has_open_channel(&encoder));

        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        assert_eq!(encoder.block_cursor, 1, "rejected block stays at the cursor");
        assert_eq!(encoder.channels.len(), 1);
        assert!(!has_open_channel(&encoder));
        assert!(encoder.channels[0].terminal_pending());

        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        assert_eq!(encoder.block_cursor, 2);
        assert_eq!(encoder.channels.len(), 2);
        assert!(has_open_channel(&encoder));
        assert_eq!(encoder.channels[1].blocks_added(), 1);
    }

    #[test]
    fn test_step_discards_block_that_exceeds_empty_channel() {
        let mut encoder = tiny_frame_zlib_encoder();
        encoder.add_block(make_block_with_user_tx_bytes(B256::ZERO, 100_000, 1)).unwrap();

        let err = encoder.step().unwrap_err();
        assert!(matches!(
            err,
            StepError::BlockExceedsChannelLimit {
                cursor: 0,
                limit: ChannelLimit::FrameCount { .. }
            }
        ));
        assert!(encoder.channels.is_empty());
        assert_eq!(encoder.block_cursor, 0);
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

        assert_eq!(encoder.channels.len(), 1);
        assert_eq!(encoder.blocks.len(), 2, "confirmation keeps blocks buffered");

        assert!(encoder.prune_safe(safe_l2));
        assert_eq!(encoder.channels.len(), 1);
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
        assert!(encoder.channels.is_empty());
        assert_eq!(encoder.egress.pending_submission_count(), 0);
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
        assert!(has_open_channel(&encoder));
        assert_eq!(encoder.da_backlog_bytes(), queued_backlog);

        encoder.flush().unwrap();
        assert!(!has_open_channel(&encoder));
        assert!(!encoder.channels.is_empty());
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
        assert!(has_open_channel(&encoder));

        // Advance L1 head past max_channel_duration (default 2).
        encoder.advance_l1_head(3);

        // Channel should be closed now.
        assert!(!has_open_channel(&encoder));
        assert!(!encoder.channels.is_empty());
    }

    /// `advance_l1_head` must be monotonic: a call with a value ≤ the current `l1_head`
    /// must be silently ignored. Without this guard, an out-of-order confirmation
    /// (possible when `max_pending_transactions` > 1) could decrease `l1_head`, making
    /// channel timeout checks produce artificially small deltas and stall timeout closure.
    #[test]
    fn test_advance_l1_head_ignores_non_monotonic_update() {
        let mut encoder = default_encoder();

        let block = make_block(B256::ZERO);
        let block_hash = block.header.hash_slow();
        encoder.add_block(block).unwrap();
        encoder.step().unwrap();

        // Advance past the timeout threshold so the channel closes.
        encoder.advance_l1_head(3);
        assert!(!has_open_channel(&encoder), "channel should have timed out at l1_head=3");

        // Now encode another block so a new channel opens.
        // Parent hash must chain from the first block's hash (= current tip).
        encoder.add_block(make_block(block_hash)).unwrap();
        encoder.step().unwrap();
        assert!(has_open_channel(&encoder), "new channel should be open");

        // A non-monotonic (backward) call must not decrease l1_head.
        encoder.advance_l1_head(1);
        assert!(has_open_channel(&encoder), "backward advance_l1_head must not close the channel");
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
        assert_eq!(encoder.egress.pending_submission_count(), 0);
        // next_id is preserved across reset so post-reset IDs can never collide
        // with pre-reset in-flight IDs.
        assert_eq!(encoder.next_id, 1);

        // Stale confirm arrives (would have been delivered to the old pipeline).
        encoder.confirm(stale_id, 42);

        // Nothing to prune: blocks were already cleared by reset().
        assert!(encoder.blocks.is_empty());
        // pending is still empty — the confirm was a no-op.
        assert_eq!(encoder.egress.pending_submission_count(), 0);
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

        assert!(encoder.channels.is_empty());
        assert_eq!(encoder.egress.pending_submission_count(), 0);
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

        // Verify the post-reorg confirmation updates the immutable artifact.
        assert!(
            encoder
                .egress
                .artifacts()
                .iter()
                .all(|artifact| { artifact.state() == crate::ArtifactState::Pending })
        );
        encoder.confirm(post_reorg_sub.id, 201);
        assert!(
            encoder
                .egress
                .artifacts()
                .iter()
                .all(|artifact| artifact.state() == crate::ArtifactState::Confirmed)
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
        let mut encoder =
            BatchEncoder::new(Arc::new(RollupConfig::default()), config).expect("valid config");

        encoder.add_block(make_block(B256::ZERO)).unwrap();
        encoder.step().unwrap();
        assert!(has_open_channel(&encoder));

        encoder.advance_l1_head(below);
        assert!(has_open_channel(&encoder), "channel must stay open before effective timeout");

        encoder.advance_l1_head(at_threshold);
        assert!(!has_open_channel(&encoder), "channel must close at effective timeout");
        assert!(!encoder.channels.is_empty());
        assert!(encoder.has_ready_submission(), "timeout must release the partial blob");
    }

    // --- max_blobs_per_tx tests ---

    /// Small frames are packed together instead of wasting one blob per frame.
    #[test]
    fn test_packs_multiple_frames_into_one_blob() {
        let config =
            EncoderConfig { max_frame_size: 32, max_blobs_per_tx: 2, ..EncoderConfig::default() };
        let mut encoder =
            BatchEncoder::new(Arc::new(RollupConfig::default()), config).expect("valid config");

        encoder.add_block(make_block_with_user_tx(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        let submission = encoder.next_submission().expect("multi-frame submission");
        assert!(submission.frame_count() > 2);
        let SubmissionPayload::Blobs(blobs) = submission.payload() else {
            panic!("expected blob submission");
        };
        assert_eq!(blobs.len(), 1);
        encoder.confirm(submission.id, 1);
        assert!(encoder.egress.channel_fully_confirmed(&encoder.channels[0]));
    }

    #[test]
    fn test_flush_releases_tail_from_closed_channel() {
        let config = EncoderConfig { compressed_size_target: Some(1), ..EncoderConfig::default() };
        let mut encoder =
            BatchEncoder::new(Arc::new(RollupConfig::default()), config).expect("valid config");

        encoder.add_block(make_block_with_user_tx(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        assert!(
            !encoder.has_ready_submission(),
            "size-closed partial blob must wait even when the input queue is empty"
        );

        encoder.flush().unwrap();

        assert!(encoder.has_ready_submission(), "explicit flush must release the partial blob");
        assert!(encoder.next_submission().is_some());
    }

    #[test]
    fn test_size_closed_tail_keeps_its_original_timeout() {
        let config = EncoderConfig {
            compressed_size_target: Some(1),
            max_channel_duration: 5,
            ..EncoderConfig::default()
        };
        let mut encoder =
            BatchEncoder::new(Arc::new(RollupConfig::default()), config).expect("valid config");
        encoder.advance_l1_head(10);

        let first = make_block_with_user_tx(B256::ZERO);
        let second = make_block_with_user_tx(first.header.hash_slow());
        encoder.add_block(first).unwrap();
        encoder.add_block(second).unwrap();

        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        assert!(!encoder.channels[0].deadline_due(14));
        assert!(encoder.channels[0].deadline_due(15));

        encoder.advance_l1_head(12);
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        assert!(encoder.channels[0].deadline_due(15));
        assert!(!encoder.channels[1].deadline_due(16));
        assert!(encoder.channels[1].deadline_due(17));

        encoder.advance_l1_head(14);
        assert!(
            !encoder.has_ready_submission(),
            "size-closed tails must remain held before the oldest channel timeout"
        );

        encoder.advance_l1_head(15);
        let submission = encoder.next_submission().expect("oldest channel timeout releases FIFO");
        encoder.requeue(submission.id);
        assert!(
            encoder.has_ready_submission(),
            "requeue must preserve an already-reached release deadline"
        );
    }

    #[test]
    fn test_open_channel_emits_full_blob_without_closing() {
        for compression_algo in [CompressionAlgo::Zlib, CompressionAlgo::Brotli(10)] {
            let config = EncoderConfig {
                compressed_size_target: None,
                max_channel_duration: 100,
                compression_algo,
                ..EncoderConfig::default()
            };
            let mut encoder =
                BatchEncoder::new(Arc::new(RollupConfig::default()), config).expect("valid config");

            let mut parent_hash = B256::ZERO;
            // Brotli may retain one 4 MiB window before exposing output. Feed
            // enough incompressible input to exercise emission without flushes.
            for seed in 1..=32 {
                let block = make_block_with_large_user_tx(parent_hash, seed);
                parent_hash = block.header.hash_slow();
                encoder.add_block(block).unwrap();
                assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
                if encoder.has_ready_submission() {
                    break;
                }
            }
            assert!(has_open_channel(&encoder), "full blob emission must not close the channel");
            assert!(
                encoder.has_ready_submission(),
                "{compression_algo:?} emitted {} compressed bytes with {} bytes available",
                encoder.channels[0].compressed_bytes(),
                encoder.channels[0].available_output()
            );

            let submission = encoder.next_submission().expect("open channel produced a full blob");
            let SubmissionPayload::Blobs(blobs) = submission.payload() else {
                panic!("expected blob submission");
            };
            assert!(!blobs.is_empty());
            for blob in blobs {
                assert!(blob.frames().iter().all(|frame| !frame.is_last));
                assert_eq!(
                    blob.frames()
                        .iter()
                        .map(|frame| Frame::ENCODED_OVERHEAD + frame.data.len())
                        .sum::<usize>()
                        + EncoderConfig::BLOB_DERIVATION_PREFIX_SIZE,
                    EncoderConfig::BLOB_MAX_DATA_SIZE
                );
            }
        }
    }

    #[test]
    fn test_packs_adjacent_channels_across_blob_boundary() {
        let config = EncoderConfig {
            compressed_size_target: Some(1),
            max_blobs_per_tx: 6,
            ..EncoderConfig::default()
        };
        let mut encoder =
            BatchEncoder::new(Arc::new(RollupConfig::default()), config).expect("valid config");
        let first = make_block_with_large_user_tx(B256::ZERO, 1);
        let second = make_block_with_large_user_tx(first.header.hash_slow(), 2);
        encoder.add_block(first).unwrap();
        encoder.add_block(second).unwrap();

        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        let first_channel_id = encoder.channels[0].id();
        let first_submission = encoder.next_submission().expect("full first blob");
        let SubmissionPayload::Blobs(first_blobs) = first_submission.payload() else {
            panic!("expected blob submission");
        };
        assert_eq!(first_blobs.len(), 1);
        assert_eq!(
            first_blobs[0]
                .frames()
                .iter()
                .map(|frame| Frame::ENCODED_OVERHEAD + frame.data.len())
                .sum::<usize>()
                + EncoderConfig::BLOB_DERIVATION_PREFIX_SIZE,
            EncoderConfig::BLOB_MAX_DATA_SIZE
        );
        assert!(encoder.next_submission().is_none(), "partial tail must wait for more data");

        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        let second_channel_id = encoder.channels[1].id();
        let second_submission = encoder.next_submission().expect("cross-channel packed blobs");
        let SubmissionPayload::Blobs(blobs) = second_submission.payload() else {
            panic!("expected blob submission");
        };
        let first_blob_channel_ids: Vec<_> =
            blobs[0].frames().iter().map(|frame| frame.id).collect();
        assert!(first_blob_channel_ids.contains(&first_channel_id));
        assert!(first_blob_channel_ids.contains(&second_channel_id));
        assert!(
            encoder
                .egress
                .pending_artifacts(second_submission.id)
                .expect("pending submission")
                .iter()
                .filter_map(|id| {
                    encoder.egress.artifacts().iter().find(|artifact| artifact.id() == *id)
                })
                .any(|artifact| {
                    artifact.channel_ids().contains(&first_channel_id)
                        && artifact.channel_ids().contains(&second_channel_id)
                }),
            "one immutable artifact must track both channels"
        );

        encoder.requeue(second_submission.id);
        assert!(
            encoder
                .egress
                .artifacts()
                .iter()
                .any(|artifact| artifact.state() == crate::ArtifactState::Ready),
            "requeue must restore the immutable cross-channel artifact"
        );
        let retry = encoder.next_submission().expect("cross-channel retry");

        encoder.confirm(first_submission.id, 1);
        encoder.confirm(retry.id, 1);
        assert!(encoder.egress.channel_fully_confirmed(&encoder.channels[0]));
        assert!(!encoder.egress.channel_fully_confirmed(&encoder.channels[1]));

        encoder.flush().unwrap();
        let tail = encoder.next_submission().expect("flushed second-channel tail");
        encoder.confirm(tail.id, 1);

        assert!(
            encoder.channels.iter().all(|channel| encoder.egress.channel_fully_confirmed(channel))
        );
    }

    #[test]
    fn test_safe_prune_preserves_pending_range_from_packed_next_channel() {
        let config = EncoderConfig {
            compressed_size_target: Some(1),
            max_blobs_per_tx: 6,
            ..EncoderConfig::default()
        };
        let mut encoder =
            BatchEncoder::new(Arc::new(RollupConfig::default()), config).expect("valid config");
        let mut first = make_block_with_large_user_tx(B256::ZERO, 1);
        first.header.number = 0;
        let safe_l2 = BlockInfo::from(&first);
        let mut second = make_block_with_large_user_tx(first.header.hash_slow(), 2);
        second.header.number = 1;
        encoder.add_block(first).unwrap();
        encoder.add_block(second).unwrap();

        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        let first_submission = encoder.next_submission().expect("first full blob");
        assert!(encoder.next_submission().is_none());
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        let packed_submission = encoder.next_submission().expect("cross-channel submission");
        let packed_artifact_id =
            encoder.egress.pending_artifacts(packed_submission.id).expect("pending submission")[0];
        let packed_artifact = encoder
            .egress
            .artifacts()
            .iter()
            .find(|artifact| artifact.id() == packed_artifact_id)
            .expect("packed artifact");
        assert!(
            packed_artifact.channel_ids().contains(&encoder.channels[0].id())
                && packed_artifact.channel_ids().contains(&encoder.channels[1].id())
        );

        // The safe head proves that the shared L1 blob landed even if its
        // submission receipt has not reached the batcher yet.
        assert!(encoder.prune_safe(safe_l2));
        assert_eq!(encoder.channels.len(), 1);
        assert!(encoder.egress.pending_artifacts(first_submission.id).is_none());
        let remaining_id = encoder.channels[0].id();
        let packed_artifact = encoder
            .egress
            .artifacts()
            .iter()
            .find(|artifact| artifact.id() == packed_artifact_id)
            .expect("mixed artifact retained for remaining channel");
        assert_eq!(packed_artifact.channel_ids(), &[remaining_id]);

        encoder.confirm(packed_submission.id, 10);
        assert!(!encoder.egress.channel_fully_confirmed(&encoder.channels[0]));

        encoder.flush().unwrap();
        let tail = encoder.next_submission().expect("flushed remaining channel tail");
        encoder.confirm(tail.id, 10);

        assert!(encoder.egress.channel_fully_confirmed(&encoder.channels[0]));
    }

    #[test]
    fn test_timeout_replay_includes_earlier_channel_from_packed_blob() {
        let rollup_config = Arc::new(RollupConfig {
            channel_timeout: 2,
            granite_channel_timeout: 2,
            ..RollupConfig::default()
        });
        let config = EncoderConfig {
            compressed_size_target: Some(1),
            max_blobs_per_tx: 1,
            max_channel_duration: 1000,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(rollup_config, config).expect("valid config");
        let mut first = make_block_with_large_user_tx(B256::ZERO, 1);
        first.header.number = 0;
        let mut second = make_block_with_large_user_tx(first.header.hash_slow(), 2);
        second.header.number = 1;
        encoder.add_block(first).unwrap();
        encoder.add_block(second).unwrap();

        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        let _first_submission = encoder.next_submission().expect("first full blob");
        assert!(encoder.next_submission().is_none());
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        let packed_submission = encoder.next_submission().expect("cross-channel blob");
        let packed_artifact = encoder
            .egress
            .pending_artifacts(packed_submission.id)
            .expect("pending submission")
            .iter()
            .filter_map(|id| {
                encoder.egress.artifacts().iter().find(|artifact| artifact.id() == *id)
            })
            .find(|artifact| artifact.channel_ids().len() == 2)
            .expect("cross-channel artifact");
        assert!(
            packed_artifact.channel_ids().contains(&encoder.channels[0].id())
                && packed_artifact.channel_ids().contains(&encoder.channels[1].id())
        );
        let final_submission = encoder.next_submission().expect("second-channel tail");

        encoder.confirm(final_submission.id, 1);
        encoder.advance_l1_head(1);
        encoder.advance_l1_head(4);

        assert!(encoder.channels.is_empty());
        assert_eq!(encoder.egress.pending_submission_count(), 0);
        assert_eq!(encoder.block_cursor, 0);
        assert_eq!(encoder.blocks.len(), 2);
    }

    #[test]
    fn test_timeout_replay_discards_entire_channel_suffix() {
        let rollup_config = Arc::new(RollupConfig {
            channel_timeout: 2,
            granite_channel_timeout: 2,
            ..RollupConfig::default()
        });
        let config = EncoderConfig {
            compressed_size_target: Some(1),
            max_blobs_per_tx: 6,
            max_channel_duration: 1000,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(rollup_config, config).expect("valid config");

        let mut first = make_block_with_large_user_tx(B256::ZERO, 1);
        first.header.number = 0;
        let mut second = make_block_with_large_user_tx(first.header.hash_slow(), 2);
        second.header.number = 1;
        let mut third = make_block_with_large_user_tx(second.header.hash_slow(), 3);
        third.header.number = 2;

        encoder.add_block(first).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        encoder.flush().unwrap();
        let first_channel_id = encoder.channels[0].id();
        for submission in drain_submissions(&mut encoder) {
            encoder.confirm(submission.id, 1);
        }
        assert!(encoder.egress.channel_fully_confirmed(&encoder.channels[0]));

        encoder.add_block(second).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        let second_channel_id = encoder.channels[1].id();
        let second_submission = encoder.next_submission().expect("second channel blob");
        encoder.confirm(second_submission.id, 1);
        assert!(!encoder.egress.channel_fully_confirmed(&encoder.channels[1]));

        encoder.add_block(third).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::ChannelClosed);
        assert_eq!(encoder.channels.len(), 3);

        encoder.advance_l1_head(4);

        assert_eq!(encoder.channels.len(), 1);
        assert_eq!(encoder.channels[0].id(), first_channel_id);
        assert!(
            encoder
                .egress
                .artifacts()
                .iter()
                .all(|artifact| { !artifact.channel_ids().contains(&second_channel_id) })
        );
        assert_eq!(encoder.block_cursor, 1);
    }

    #[test]
    fn test_max_blobs_per_tx_limits_transaction_not_channel() {
        let config = EncoderConfig { max_blobs_per_tx: 1, ..EncoderConfig::default() };
        let mut encoder =
            BatchEncoder::new(Arc::new(RollupConfig::default()), config).expect("valid config");
        encoder.add_block(make_block_with_large_user_tx(B256::ZERO, 1)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        let submissions = drain_submissions(&mut encoder);
        assert!(submissions.len() >= 2, "one channel should span multiple transactions");
        assert!(submissions.iter().all(|submission| submission.blob_count() == 1));
        assert_eq!(encoder.channels.len(), 1, "transaction cuts must not split channels");
    }

    /// A requeue makes every frame in the submission ready again.
    #[test]
    fn test_requeue_multi_frame_marks_submission_ready() {
        let config =
            EncoderConfig { max_frame_size: 32, max_blobs_per_tx: 3, ..EncoderConfig::default() };
        let mut encoder =
            BatchEncoder::new(Arc::new(RollupConfig::default()), config).expect("valid config");

        encoder.add_block(make_block_with_user_tx(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        let sub = encoder.next_submission().expect("multi-frame submission");
        let id = sub.id;
        let submitted_frame_count = sub.frame_count();
        assert!(submitted_frame_count > 1);

        encoder.requeue(id);

        let resub = encoder.next_submission().expect("requeued submission");
        assert_eq!(
            resub.frame_count(),
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
            encoder.egress.artifacts().iter().map(crate::DaArtifact::state).collect::<Vec<_>>(),
            vec![crate::ArtifactState::Ready, crate::ArtifactState::Confirmed]
        );

        let retry = encoder.next_submission().unwrap();
        assert!(Arc::ptr_eq(
            retry.first_frame().expect("retry frame"),
            first.first_frame().expect("original frame")
        ));
        assert_eq!(
            encoder.egress.artifacts().iter().map(crate::DaArtifact::state).collect::<Vec<_>>(),
            vec![crate::ArtifactState::Pending, crate::ArtifactState::Confirmed]
        );
    }

    #[test]
    fn test_late_confirmation_replays_timed_out_channel() {
        let mut encoder = encoder_with_confirmation_timeout(2);

        encoder.add_block(make_block_with_user_tx(B256::ZERO)).unwrap();
        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        let original_channel_id = encoder.channels[0].id();
        let submissions = drain_submissions(&mut encoder);
        assert!(
            submissions.len() >= 2,
            "test requires a multi-frame channel, got {} submission(s)",
            submissions.len()
        );

        encoder.confirm(submissions[0].id, 1);
        encoder.advance_l1_head(1);
        assert_eq!(encoder.blocks.len(), 1, "partial confirmation must not prune blocks");

        encoder.confirm(submissions[1].id, 4);
        encoder.advance_l1_head(4);
        assert_eq!(encoder.blocks.len(), 1, "timed-out confirmation must preserve blocks");
        assert_eq!(encoder.block_cursor, 0, "encoder must rewind to replay the block");
        assert!(encoder.channels.is_empty(), "old channel must be discarded");
        assert_eq!(
            encoder.egress.pending_submission_count(),
            0,
            "stale in-flight tail submissions must be forgotten"
        );

        for submission in submissions.iter().skip(2) {
            encoder.confirm(submission.id, 3);
        }
        assert_eq!(encoder.blocks.len(), 1, "stale late confirmations must be no-ops");

        assert_eq!(encoder.step().unwrap(), StepResult::BlockEncoded);
        encoder.flush().unwrap();

        let replay_submissions = drain_submissions(&mut encoder);
        assert!(!replay_submissions.is_empty(), "replay must emit a fresh channel");
        assert_ne!(
            replay_submissions[0].first_frame().expect("replay frame").id,
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

        assert_eq!(encoder.channels.len(), 1);
        assert!(encoder.egress.channel_fully_confirmed(&encoder.channels[0]));
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

        assert!(encoder.channels.is_empty());
        assert_eq!(encoder.egress.pending_submission_count(), 0);
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
        assert!(encoder.channels.is_empty(), "expired channel must be discarded");
        assert_eq!(
            encoder.egress.pending_submission_count(),
            0,
            "stale tail submissions must be forgotten"
        );

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
        let open = encoder.channels.back().unwrap();
        assert_eq!(open.block_range(), 0..1);
        assert_eq!(open.blocks_added(), 1);
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
        assert!(encoder.channels.is_empty());
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

    /// `encode_and_drain` steps until idle, flushes, and returns all submissions.
    #[test]
    fn test_encode_and_drain_returns_submissions() {
        let mut encoder = default_encoder();
        encoder.add_block(make_block_with_user_tx(B256::ZERO)).expect("add block");
        let submissions = encoder.encode_and_drain().expect("encode_and_drain");
        assert!(!submissions.is_empty(), "encode_and_drain must return a submission");
        assert!(submissions.iter().all(|submission| submission.frame_count() > 0));
    }

    #[test]
    fn test_encode_and_drain_releases_already_closed_tails() {
        let config = EncoderConfig { compressed_size_target: Some(1), ..EncoderConfig::default() };
        let mut encoder =
            BatchEncoder::new(Arc::new(RollupConfig::default()), config).expect("valid config");
        let first = make_block_with_user_tx(B256::ZERO);
        let second = make_block_with_user_tx(first.header.hash_slow());
        encoder.add_block(first).expect("add first block");
        encoder.add_block(second).expect("add second block");

        let submissions = encoder.encode_and_drain().expect("encode_and_drain");

        assert!(!submissions.is_empty(), "drain must release size-closed channel tails");
        assert!(!encoder.has_ready_submission(), "drain must consume every released tail");
    }

    /// `encode_and_drain` with no blocks added returns empty (Idle immediately).
    #[test]
    fn test_encode_and_drain_no_blocks_returns_empty() {
        let mut encoder = default_encoder();
        let submissions = encoder.encode_and_drain().expect("encode_and_drain");
        assert!(submissions.is_empty(), "no blocks must produce no submissions");
    }

    /// Encoding with a small `max_frame_size` fragments a multi-block channel
    /// into multiple frames, proving the encoder respects the frame-size limit.
    #[test]
    fn frame_fragmentation_with_small_frame_size() {
        let rollup_config = Arc::new(RollupConfig::default());
        let config = EncoderConfig { max_frame_size: 80, ..EncoderConfig::default() };
        let mut encoder = BatchEncoder::new(rollup_config, config).expect("valid config");

        // Add 5 L2 blocks with a user tx in each to produce non-trivial payload.
        let mut parent = B256::ZERO;
        for _ in 0..5 {
            let block = make_block_with_user_tx(parent);
            parent = block.header.hash_slow();
            encoder.add_block(block).expect("add block");
        }

        let submissions = encoder.encode_and_drain().expect("encode_and_drain");
        let frame_count = submissions.iter().map(BatchSubmission::frame_count).sum::<usize>();
        assert!(
            frame_count >= 3,
            "expected at least 3 frames with max_frame_size=80, got {frame_count}"
        );
    }

    /// Calldata submissions carry exactly one derivation frame.
    #[test]
    fn calldata_submits_one_frame_per_transaction() {
        let rollup_config = Arc::new(RollupConfig::default());
        let config = EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: 100,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(rollup_config, config).expect("valid config");

        let block = make_block_with_user_tx(B256::ZERO);
        encoder.add_block(block).expect("add block");

        // Run until idle, flush, and drain submissions.
        loop {
            if encoder.step().expect("step") == StepResult::Idle {
                break;
            }
        }
        encoder.flush().unwrap();

        // The calldata transaction format carries one derivation frame.
        let mut count = 0;
        while let Some(sub) = encoder.next_submission() {
            assert_eq!(sub.frame_count(), 1, "calldata submission must have exactly 1 frame");
            count += 1;
        }
        assert!(count >= 1, "expected at least one submission");
    }

    /// A retry preserves the exact artifact produced while blob override was active.
    #[test]
    fn blob_override_retry_preserves_immutable_blob_artifact() {
        let rollup_config = Arc::new(RollupConfig::default());
        let config = EncoderConfig {
            da_type: DaType::Calldata,
            max_frame_size: 200,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(rollup_config, config).expect("valid config");

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
        assert_eq!(sub.da_type(), DaType::Blob, "retry must preserve the original DA artifact");
    }

    /// `set_blob_override(true)` is a no-op for blob-configured encoders —
    /// submissions are blob-typed regardless of the override.
    #[test]
    fn blob_override_is_noop_for_blob_configured_encoder() {
        let rollup_config = Arc::new(RollupConfig::default());
        let config = EncoderConfig { da_type: DaType::Blob, ..EncoderConfig::default() };
        let mut encoder = BatchEncoder::new(rollup_config, config).expect("valid config");

        encoder.add_block(make_block_with_user_tx(B256::ZERO)).expect("add block");
        while encoder.step().expect("step") != StepResult::Idle {}
        encoder.flush().unwrap();
        encoder.set_blob_override(true);
        let submission = encoder.next_submission().expect("blob submission");
        assert_eq!(submission.da_type(), DaType::Blob);
    }
}
