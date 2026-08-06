//! The batcher pipeline trait.

use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;

use crate::{BatchSubmission, ReorgError, StepError, StepResult, SubmissionId};

/// The batcher pipeline -- inverse of the derivation pipeline.
///
/// Where the derivation pipeline accepts L1 data and produces L2 payload attributes,
/// the batcher pipeline accepts L2 blocks and produces L1 submission data (frames -> blobs).
///
/// The pipeline is a synchronous state machine. Callers drive it by:
/// 1. Feeding L2 blocks via [`add_block`](Self::add_block).
/// 2. Advancing state via [`step`](Self::step) until [`StepResult::Idle`].
/// 3. Draining ready submissions via [`next_submission`](Self::next_submission).
/// 4. Reporting outcomes via [`confirm`](Self::confirm) / [`requeue`](Self::requeue).
pub trait BatchPipeline: Send {
    /// Add an L2 block to the pipeline's input queue.
    ///
    /// Returns `Err((ReorgError, block))` if the block's parent hash does not match the
    /// current tip. The caller must reset the pipeline and restart its block source
    /// from a trusted head.
    fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)>;

    /// Advance the pipeline by one step.
    ///
    /// A step encodes one pending block into the current channel, or closes a full channel
    /// and moves it to the submission queue. Call repeatedly until [`StepResult::Idle`].
    ///
    /// Returns [`StepError`] if a block cannot be composed into a batch. This is fatal:
    /// skipping the block would silently break the contiguous L2 block sequence required
    /// by the derivation spec. The caller must not continue and should surface the error.
    fn step(&mut self) -> Result<StepResult, StepError>;

    /// Returns the next [`BatchSubmission`] ready for L1 submission, if any.
    ///
    /// Each submission is one L1 transaction's worth of data (currently one frame -> one blob).
    /// Returns `None` if no submission is ready. Assigns a unique [`SubmissionId`] for
    /// tracking via [`confirm`](Self::confirm) / [`requeue`](Self::requeue).
    fn next_submission(&mut self) -> Option<BatchSubmission>;

    /// Mark a submission as confirmed at the given L1 block number.
    ///
    /// Records frame inclusion without removing the channel or its L2 blocks.
    /// [`prune_safe`](Self::prune_safe) owns both channel and block removal.
    fn confirm(&mut self, id: SubmissionId, l1_block: u64);

    /// Mark a submission as failed -- rewinds the frame cursor so frames are resubmitted.
    fn requeue(&mut self, id: SubmissionId);

    /// Notify the pipeline of the current L1 head block number.
    ///
    /// Used to detect channel timeouts: if `l1_head - channel.opened_at > max_channel_duration`,
    /// the channel is force-closed and its blocks are requeued.
    fn advance_l1_head(&mut self, l1_block: u64);

    /// Force-close the current channel, moving it to the submission queue.
    ///
    /// Unlike [`advance_l1_head`](Self::advance_l1_head) with `u64::MAX`, this does not
    /// mutate the L1 head tracker, so subsequent real [`advance_l1_head`](Self::advance_l1_head)
    /// calls continue to work correctly.
    ///
    /// Intended for test harnesses that need to flush the current channel without
    /// simulating L1 time progression.
    fn force_close_channel(&mut self);

    /// Reset all pipeline state.
    ///
    /// Called after a reorg is detected. The caller is responsible for waiting for all
    /// in-flight submissions to settle (confirm or requeue) before calling reset.
    fn reset(&mut self);

    /// Prune buffered blocks at or below the reported safe L2 head.
    ///
    /// Returns `false` without pruning when the safe head leaves a gap below the
    /// buffered window, lies above it, or has a different hash than the boundary
    /// block. The caller must reset the pipeline in these cases. An empty buffer
    /// is valid.
    ///
    /// Pruning includes blocks not yet fed into a channel and clamps the encoding
    /// cursor to the remaining queue.
    fn prune_safe(&mut self, safe_l2: BlockInfo) -> bool;

    /// Returns the estimated DA backlog in bytes.
    ///
    /// Sum of unsafe DA bytes that have not yet been submitted to L1, including
    /// pending block estimates, open channel estimates, and closed channel frame bytes.
    /// Deposit transactions are excluded from block estimates.
    fn da_backlog_bytes(&self) -> u64;

    /// Force the next [`next_submission`](Self::next_submission) calls to emit
    /// blob-typed submissions even when the configured `da_type` is calldata.
    ///
    /// Wired by the driver from DA-throttle state: when throttle is active and
    /// `force_blobs_when_throttling` is set, the override is enabled. When the
    /// throttle deactivates the override is cleared. Default no-op for
    /// pipelines that do not support DA-type overrides.
    fn set_blob_override(&mut self, _active: bool) {}
}
