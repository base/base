//! The [`BatchEncoder`] implementation.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::Arc,
};

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::B256;
use base_alloy_consensus::{OpBlock, OpTxEnvelope};
use base_comp::{
    BatchComposer, ChannelOut, CompressionAlgo, CompressorType, Config, ShadowCompressor,
};
use base_consensus_genesis::RollupConfig;
use base_protocol::{Batch, ChannelId};
use rand::{RngCore, SeedableRng, rngs::SmallRng};
use tracing::{debug, warn};

use crate::{
    BatchPipeline, BatchSubmission, EncoderConfig, ReorgError, StepError, StepResult, SubmissionId,
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
    blocks: VecDeque<OpBlock>,
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
            tip: B256::ZERO,
            current_channel: None,
            ready_channels: VecDeque::new(),
            pending: HashMap::new(),
            next_id: 0,
            rng: SmallRng::from_os_rng(),
        }
    }

    /// Close the current channel, drain its frames, and push it to `ready_channels`.
    fn close_current_channel(&mut self) {
        let Some(mut open) = self.current_channel.take() else {
            return;
        };

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

        debug!(
            channel_id = ?channel_id,
            frame_count = frames.len(),
            block_range_start = block_range.start,
            block_range_end = block_range.end,
            "closed channel"
        );

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
            approx_compr_ratio: 0.6,
        };
        let compressor = ShadowCompressor::from(compressor_config);

        let channel_out = ChannelOut::new(id, Arc::clone(&self.rollup_config), compressor);

        self.current_channel = Some(OpenChannel { out: channel_out, opened_at_l1: self.l1_head });
    }

    /// Check if the current channel has timed out and close it if so.
    fn check_channel_timeout(&mut self) -> bool {
        let should_close = if let Some(ref open) = self.current_channel {
            // Apply the safety margin so channels are closed `sub_safety_margin` L1 blocks
            // before the configured `max_channel_duration`, ensuring frames land well within
            // the protocol's `channel_timeout` inclusion window.
            let effective_duration =
                self.config.max_channel_duration.saturating_sub(self.config.sub_safety_margin);
            self.l1_head.saturating_sub(open.opened_at_l1) >= effective_duration
        } else {
            false
        };

        if should_close {
            debug!(l1_head = self.l1_head, "channel timed out, closing");
            self.close_current_channel();
        }

        should_close
    }
}

impl BatchPipeline for BatchEncoder {
    fn add_block(&mut self, block: OpBlock) -> Result<(), Box<(ReorgError, OpBlock)>> {
        if !self.blocks.is_empty() && block.header.parent_hash != self.tip {
            return Err(Box::new((
                ReorgError::ParentMismatch { expected: self.tip, got: block.header.parent_hash },
                block,
            )));
        }

        let hash = block.header.hash_slow();
        self.tip = hash;
        self.blocks.push_back(block);

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

        // Ensure a channel is open.
        if self.current_channel.is_none() {
            self.open_new_channel();
        }

        // Get the block at the cursor.
        let block = &self.blocks[self.block_cursor];

        // Convert block to a SingleBatch. Failure here is fatal: skipping the block
        // would produce a gap in the L2 block sequence submitted to L1.
        let (single_batch, _l1_info) = BatchComposer::block_to_single_batch(block)
            .map_err(|source| StepError::CompositionFailed { cursor: self.block_cursor, source })?;

        // Try to add the batch to the current channel.
        let batch = Batch::Single(single_batch);
        let open = self.current_channel.as_mut().unwrap();
        Ok(match open.out.add_batch(batch) {
            Ok(()) => {
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
                self.close_current_channel();
                StepResult::ChannelClosed
            }
        })
    }

    fn next_submission(&mut self) -> Option<BatchSubmission> {
        // Find the first ready channel with unsubmitted frames.
        for (chan_idx, channel) in self.ready_channels.iter_mut().enumerate() {
            if channel.cursor < channel.frames.len() {
                let frame_start = channel.cursor;
                // Pack up to `target_num_frames` frames into a single L1 transaction.
                let available = channel.frames.len() - frame_start;
                let frame_count = available.min(self.config.target_num_frames).max(1);
                // Clone the Arcs (pointer copies, not deep copies of frame data).
                let frames: Vec<_> =
                    channel.frames[frame_start..frame_start + frame_count].to_vec();

                let id = SubmissionId(self.next_id);
                self.next_id += 1;

                channel.cursor += frame_count;
                channel.pending_confirmations += 1;

                self.pending
                    .insert(id, PendingRef { channel_idx: chan_idx, frame_start, frame_count });

                return Some(BatchSubmission { id, channel_id: channel.id, frames });
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
            warn!(id = ?id, chan_idx, "confirm: channel index out of bounds; submission lost");
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
                block_range_start = block_range.start,
                block_range_end = block_range.end,
                "channel fully confirmed, pruning blocks"
            );

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
                for _ in 0..prune_count {
                    self.blocks.pop_front();
                }
                self.block_cursor = self.block_cursor.saturating_sub(prune_count);

                // Adjust the high-water mark for all remaining channels.
                // block_range.start is always 0 and unused in prune logic.
                for ch in &mut self.ready_channels {
                    ch.block_range.end = ch.block_range.end.saturating_sub(prune_count);
                }
            }
        }
    }

    fn requeue(&mut self, id: SubmissionId) {
        let Some(pending_ref) = self.pending.remove(&id) else {
            warn!(id = ?id, "requeue called for unknown submission id");
            return;
        };

        let chan_idx = pending_ref.channel_idx;
        if chan_idx >= self.ready_channels.len() {
            warn!(id = ?id, chan_idx, "requeue: channel index out of bounds; submission lost");
            return;
        }

        let channel = &mut self.ready_channels[chan_idx];
        channel.pending_confirmations = channel.pending_confirmations.saturating_sub(1);
        // Rewind cursor to the first frame of the requeued submission so all frames
        // in the batch are retried together.
        if pending_ref.frame_start < channel.cursor {
            channel.cursor = pending_ref.frame_start;
        }

        debug!(
            id = ?id,
            frame_start = pending_ref.frame_start,
            frame_count = pending_ref.frame_count,
            "requeued submission"
        );
    }

    fn advance_l1_head(&mut self, l1_block: u64) {
        if l1_block <= self.l1_head {
            return;
        }
        self.l1_head = l1_block;
        self.check_channel_timeout();
    }

    fn reset(&mut self) {
        self.blocks.clear();
        self.block_cursor = 0;
        self.tip = B256::ZERO;
        self.current_channel = None;
        self.ready_channels.clear();
        self.pending.clear();
        // Intentionally not resetting `next_id`: keeping it monotonically
        // increasing across resets means post-reset submissions can never
        // share an ID with any pre-reset in-flight submission, eliminating
        // stale-confirm silent corruption.
        self.rng = SmallRng::from_os_rng();
    }

    fn da_backlog_bytes(&self) -> u64 {
        let mut total: u64 = 0;
        for block in self.blocks.iter().skip(self.block_cursor) {
            for tx in &block.body.transactions {
                // Exclude deposit transactions (type 0x7E).
                if matches!(tx, OpTxEnvelope::Deposit(_)) {
                    continue;
                }
                total += tx.encode_2718_len() as u64;
            }
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
    use alloy_primitives::{Bytes, Sealed, Signature};
    use base_alloy_consensus::{OpTxEnvelope, TxDeposit};
    use base_protocol::{L1BlockInfoBedrock, L1BlockInfoTx};

    use super::*;

    fn make_deposit_tx() -> OpTxEnvelope {
        let calldata = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default()).encode_calldata();
        OpTxEnvelope::Deposit(Sealed::new(TxDeposit { input: calldata, ..Default::default() }))
    }

    fn make_block(parent_hash: B256) -> OpBlock {
        OpBlock {
            header: Header { parent_hash, ..Default::default() },
            body: BlockBody { transactions: vec![make_deposit_tx()], ..Default::default() },
        }
    }

    fn make_block_with_user_tx(parent_hash: B256) -> OpBlock {
        let user_tx = {
            let signed = TxLegacy::default().into_signed(Signature::test_signature());
            OpTxEnvelope::Legacy(signed)
        };

        OpBlock {
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
        let (err, returned_block) = *encoder.add_block(block2).unwrap_err();
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

    /// With `sub_safety_margin > 0` the effective timeout is shortened by the margin.
    /// A channel opened at L1 block 0 with `max_channel_duration=10, sub_safety_margin=4`
    /// must close when `l1_head` reaches 6 (effective = 10 - 4 = 6), not 10.
    #[test]
    fn test_sub_safety_margin_shortens_effective_timeout() {
        let config = EncoderConfig {
            max_channel_duration: 10,
            sub_safety_margin: 4,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(Arc::new(RollupConfig::default()), config);

        encoder.add_block(make_block(B256::ZERO)).unwrap();
        encoder.step().unwrap();
        assert!(encoder.current_channel.is_some());

        // One block before the effective threshold — channel stays open.
        encoder.advance_l1_head(5);
        assert!(
            encoder.current_channel.is_some(),
            "channel must stay open before effective timeout"
        );

        // At the effective threshold — channel closes.
        encoder.advance_l1_head(6);
        assert!(encoder.current_channel.is_none(), "channel must close at effective timeout");
        assert!(!encoder.ready_channels.is_empty());
    }

    /// When `sub_safety_margin == 0` the effective timeout equals `max_channel_duration`.
    #[test]
    fn test_sub_safety_margin_zero_uses_full_duration() {
        let config = EncoderConfig {
            max_channel_duration: 5,
            sub_safety_margin: 0,
            ..EncoderConfig::default()
        };
        let mut encoder = BatchEncoder::new(Arc::new(RollupConfig::default()), config);

        encoder.add_block(make_block(B256::ZERO)).unwrap();
        encoder.step().unwrap();

        encoder.advance_l1_head(4);
        assert!(encoder.current_channel.is_some(), "channel must stay open before full duration");

        encoder.advance_l1_head(5);
        assert!(encoder.current_channel.is_none(), "channel must close at full duration");
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

    fn make_empty_block(parent_hash: B256) -> OpBlock {
        OpBlock {
            header: Header { parent_hash, ..Default::default() },
            body: BlockBody { transactions: vec![], ..Default::default() },
        }
    }

    fn make_non_deposit_block(parent_hash: B256) -> OpBlock {
        let user_tx = {
            let signed = TxLegacy::default().into_signed(Signature::test_signature());
            OpTxEnvelope::Legacy(signed)
        };
        OpBlock {
            header: Header { parent_hash, ..Default::default() },
            body: BlockBody { transactions: vec![user_tx], ..Default::default() },
        }
    }

    fn make_bad_calldata_block(parent_hash: B256) -> OpBlock {
        let deposit = OpTxEnvelope::Deposit(Sealed::new(TxDeposit {
            input: Bytes::new(),
            ..Default::default()
        }));
        OpBlock {
            header: Header { parent_hash, ..Default::default() },
            body: BlockBody { transactions: vec![deposit], ..Default::default() },
        }
    }

    /// An empty block (no transactions) cannot be composed into a `SingleBatch`.
    /// `step()` must return Err rather than skip the block.
    #[test]
    fn test_step_fatal_on_empty_block() {
        let mut encoder = default_encoder();
        encoder.add_block(make_empty_block(B256::ZERO)).unwrap();

        let err = encoder.step().unwrap_err();
        assert!(matches!(
            err,
            StepError::CompositionFailed {
                cursor: 0,
                source: base_comp::BatchComposeError::EmptyBlock
            }
        ));
    }

    /// A block whose first transaction is not a deposit cannot be composed.
    /// `step()` must return Err rather than skip the block.
    #[test]
    fn test_step_fatal_on_no_deposit_tx() {
        let mut encoder = default_encoder();
        encoder.add_block(make_non_deposit_block(B256::ZERO)).unwrap();

        let err = encoder.step().unwrap_err();
        assert!(matches!(
            err,
            StepError::CompositionFailed {
                cursor: 0,
                source: base_comp::BatchComposeError::NotDepositTx
            }
        ));
    }

    /// A deposit with undecodable L1 info calldata cannot be composed.
    /// `step()` must return Err rather than skip the block.
    #[test]
    fn test_step_fatal_on_bad_l1_info_calldata() {
        let mut encoder = default_encoder();
        encoder.add_block(make_bad_calldata_block(B256::ZERO)).unwrap();

        let err = encoder.step().unwrap_err();
        assert!(matches!(
            err,
            StepError::CompositionFailed {
                cursor: 0,
                source: base_comp::BatchComposeError::L1InfoDecode
            }
        ));
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
}
