//! Batcher pipeline trait and the types that drive it.

use alloy_primitives::B256;
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;

use crate::{BatchComposeError, BatchSubmission, ChannelError, ChannelLimit, SubmissionId};

/// Result of a [`BatchPipeline::step`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResult {
    /// One block was encoded into the current channel.
    BlockEncoded,
    /// The writable channel reached a closure trigger and finalized its compression stream.
    ChannelClosed,
    /// No transition is available: no block is pending and no channel must close.
    Idle,
}

/// Returned by [`BatchPipeline::step`] when a block cannot be encoded and the
/// pipeline cannot continue.
///
/// Encoding failures are fatal. Continuing after a composition, compression, or framing
/// error could omit part of the contiguous L2 block sequence required by the derivation spec.
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
    /// A block cannot fit in an empty derivation channel.
    #[error("block at cursor {cursor} exceeds a derivation channel limit: {limit}")]
    BlockExceedsChannelLimit {
        /// Index of the block in the encoder's input queue.
        cursor: usize,
        /// Limit that the block exceeded in an otherwise empty channel.
        limit: ChannelLimit,
    },
    /// Channel state transition failed.
    #[error(transparent)]
    Channel(#[from] ChannelError),
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

/// The batcher pipeline -- inverse of the derivation pipeline.
///
/// Where the derivation pipeline accepts L1 data and produces L2 payload attributes,
/// the batcher pipeline accepts L2 blocks and produces framed L1 submissions.
///
/// The pipeline is a synchronous state machine. Callers drive it by:
/// 1. Feeding L2 blocks via [`add_block`](Self::add_block).
/// 2. Advancing state via [`step`](Self::step) until [`StepResult::Idle`].
/// 3. Draining ready submissions via [`next_submission`](Self::next_submission).
/// 4. Reporting outcomes via [`confirm`](Self::confirm) / [`requeue`](Self::requeue).
/// 5. Reconciling derivation progress via
///    [`reconcile_derivation`](Self::reconcile_derivation).
pub trait BatchPipeline: Send {
    /// Add an L2 block to the pipeline's input queue.
    ///
    /// Returns `Err((ReorgError, block))` if the block's parent hash does not match the
    /// current tip. The caller must reset the pipeline and restart its block source
    /// from a trusted head.
    fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)>;

    /// Advance the pipeline by one step.
    ///
    /// A step encodes one pending block into the writable FIFO tail, or closes
    /// that channel at a configured boundary. Streaming egress may make full
    /// artifacts available before closure. Call repeatedly until [`StepResult::Idle`].
    ///
    /// Returns [`StepError`] if batch composition or channel finalization fails. This is
    /// fatal: continuing could silently break the contiguous L2 block sequence required
    /// by the derivation spec. The caller must not continue and should surface the error.
    fn step(&mut self) -> Result<StepResult, StepError>;

    /// Returns the next [`BatchSubmission`] ready for L1 submission, if any.
    ///
    /// Each submission is one L1 transaction's worth of frames. Blob submissions may
    /// contain several blobs, each packed with one or more frames; calldata submissions
    /// contain one frame.
    ///
    /// Returns `None` if no submission is ready. Assigns a unique [`SubmissionId`]
    /// for tracking via [`confirm`](Self::confirm) / [`requeue`](Self::requeue).
    fn next_submission(&mut self) -> Option<BatchSubmission>;

    /// Returns `true` if a call to [`next_submission`](Self::next_submission) would
    /// currently return `Some`, without mutating any pipeline state.
    ///
    /// Used to distinguish "no more ready work" from "backpressured" when the caller
    /// has run out of L1 submission capacity: the pipeline can be non-empty even
    /// though the caller cannot currently drain it.
    fn has_ready_submission(&self) -> bool;

    /// Mark a submission as confirmed at the given L1 block number.
    ///
    /// Records frame inclusion without removing the channel or its L2 blocks.
    /// [`reconcile_derivation`](Self::reconcile_derivation) owns safe-prefix removal.
    /// Call [`advance_l1_head`](Self::advance_l1_head) after processing the receipt.
    fn confirm(&mut self, id: SubmissionId, l1_block: u64);

    /// Mark a submission as failed so its frames can be submitted again.
    fn requeue(&mut self, id: SubmissionId);

    /// Notify the pipeline of the current L1 head block number.
    ///
    /// Closes open channels that reach their maximum duration and replays closed channels
    /// whose confirmation window expires.
    fn advance_l1_head(&mut self, l1_block: u64);

    /// Flushes the pipeline without changing its L1 head.
    ///
    /// Closes the writable channel and releases every retained partial artifact.
    /// Returns the compression error directly so callers never observe a deferred
    /// failure from an unrelated later operation.
    fn flush(&mut self) -> Result<(), StepError>;

    /// Reset buffered encoding and submission state.
    ///
    /// Called when buffered state must be discarded, such as after a reorg, derivation
    /// mismatch, or pause. The caller must discard in-flight submission tracking first
    /// so stale outcomes cannot mutate the rebuilt state.
    fn reset(&mut self);

    /// Reconcile buffered state with the reported derivation progress.
    ///
    /// Prunes buffered blocks at or below `safe_l2`, returning
    /// [`DerivationReconciliation::SafeHeadMismatch`] without mutation when the safe head leaves a
    /// gap below the buffered window, lies above it, or has a different hash than the boundary
    /// block. An empty buffer is valid.
    ///
    /// After pruning, returns [`DerivationReconciliation::StalledChannel`] if `current_l1` has
    /// moved strictly beyond the last inclusion block of a fully confirmed channel whose last L2
    /// block is not safe. `current_l1` must be the derivation cursor, not the live L1 chain head;
    /// `None` disables this check for providers that do not expose the cursor.
    fn reconcile_derivation(
        &mut self,
        safe_l2: BlockInfo,
        current_l1: Option<u64>,
    ) -> DerivationReconciliation;

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
