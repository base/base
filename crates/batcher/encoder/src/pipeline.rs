//! Batcher pipeline trait and the types that drive it.

use alloy_primitives::B256;
use base_common_consensus::BaseBlock;
use base_comp::BatchComposeError;
use base_protocol::BlockInfo;

use crate::{BatchSubmission, ChannelFullReason, OpenChannelError, SubmissionId};

/// Result of a [`BatchPipeline::step`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResult {
    /// One block was encoded into the current channel.
    BlockEncoded,
    /// The current channel reached a closure trigger and was moved to the submission queue.
    ChannelClosed,
    /// No transition is available: no block is pending and no channel must close.
    Idle,
}

/// Encoding failed. Fatal: do not continue.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// The block could not be converted to a [`SingleBatch`].
    #[error("batch composition failed for block at cursor {cursor}: {source}")]
    CompositionFailed {
        /// Index of the block in the encoder's input queue.
        cursor: usize,
        /// Underlying composition error.
        #[source]
        source: BatchComposeError,
    },
    /// A block cannot fit in an empty channel and therefore cannot be published.
    #[error("block at cursor {cursor} was rejected by an empty channel: {reason}")]
    BlockRejectedByEmptyChannel {
        /// Index of the block in the encoder's input queue.
        cursor: usize,
        /// Size constraint that rejected the block.
        reason: ChannelFullReason,
    },
    /// Channel construction, compression, or framing failed.
    #[error(transparent)]
    ChannelFailed(#[from] OpenChannelError),
}

/// Returned by [`BatchPipeline::add_block`] when a reorg is detected.
#[derive(Debug, thiserror::Error)]
pub enum ReorgError {
    /// The block's parent hash does not match the current tip.
    #[error("parent hash mismatch: expected {expected}, got {got}")]
    ParentMismatch {
        /// The expected parent hash (current tip).
        expected: B256,
        /// The actual parent hash from the incoming block.
        got: B256,
    },
}

/// Result of reconciling buffered batcher state with derivation progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DerivationReconciliation {
    /// Buffered state is consistent with derivation and safe blocks were pruned.
    Consistent,
    /// The reported safe L2 head does not match the buffered L2 chain.
    SafeHeadMismatch,
    /// Derivation passed a fully confirmed channel without making its last L2 block safe.
    StalledChannel,
}

/// The batcher pipeline: L2 blocks in, L1 submissions out.
pub trait BatchPipeline: Send {
    /// Queue an L2 block. Parent mismatch returns the block so the caller can reset.
    fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)>;

    /// Encode one pending block or close the current channel.
    ///
    /// Call until [`StepResult::Idle`]. [`StepError`] is fatal.
    fn step(&mut self) -> Result<StepResult, StepError>;

    /// Next L1 transaction, if any. Each call assigns a new [`SubmissionId`].
    fn next_submission(&mut self) -> Option<BatchSubmission>;

    /// Whether [`next_submission`](Self::next_submission) would return `Some`.
    fn has_ready_submission(&self) -> bool;

    /// Record L1 inclusion. Does not prune; [`reconcile_derivation`](Self::reconcile_derivation) does.
    fn confirm(&mut self, id: SubmissionId, l1_block: u64);

    /// Return the submission's frames to ready.
    fn requeue(&mut self, id: SubmissionId);

    /// Close duration-expired channels and replay confirmation timeouts.
    fn advance_l1_head(&mut self, l1_block: u64);

    /// Close the current channel and move it to the submission queue.
    ///
    /// Unlike [`advance_l1_head`](Self::advance_l1_head) with `u64::MAX`, this does not
    /// mutate the L1 head tracker.
    fn flush(&mut self) -> Result<(), StepError>;

    /// Drop buffered encoding state. Discard in-flight tracking first.
    fn reset(&mut self);

    /// Prune blocks at or below `safe_l2`.
    ///
    /// [`DerivationReconciliation::SafeHeadMismatch`] if the head is not on the buffered chain.
    /// [`DerivationReconciliation::StalledChannel`] if `current_l1` (derivation cursor) passed a fully
    /// confirmed channel whose tail is not yet safe. `None` skips that check.
    fn reconcile_derivation(
        &mut self,
        safe_l2: BlockInfo,
        current_l1: Option<u64>,
    ) -> DerivationReconciliation;

    /// Estimated DA bytes still awaiting confirmation.
    ///
    /// Unencoded queued blocks plus channels that are not fully confirmed.
    /// Deposits are excluded.
    fn da_backlog_bytes(&self) -> u64;

    /// Force blob DA on later submissions. No-op unless `da_type` is calldata.
    fn set_blob_override(&mut self, _active: bool) {}
}
