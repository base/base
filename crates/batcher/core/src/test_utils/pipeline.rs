//! Test [`BatchPipeline`] implementation for the driver tests.
//!
//! Hand-rolled rather than mocked: the driver tests need one call log ordered across several
//! trait methods, and the pipeline holds state (queued submissions, blocks left to encode)
//! that the driver consumes while it runs.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_primitives::B256;
use base_batcher_encoder::{
    BatchPipeline, BatchSubmission, DerivationReconciliation, ReorgError, StepError, StepResult,
    SubmissionId, SubmissionPayload,
};
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;

/// A [`BatchPipeline`] call recorded by [`TrackingPipeline`], with the arguments the tests
/// look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineCall {
    /// `add_block`, with the block number.
    AddBlock(u64),
    /// `step`, recorded only when it encodes a block.
    Step,
    /// `next_submission` handed out this submission.
    Dequeue(SubmissionId),
    /// `confirm`, with the submission id and its L1 inclusion block.
    Confirm(SubmissionId, u64),
    /// `requeue`.
    Requeue(SubmissionId),
    /// `flush`.
    Flush,
    /// `advance_l1_head`, recorded only when the head advances.
    AdvanceL1Head(u64),
    /// `reconcile_derivation`, with the safe L2 block number and the derivation cursor.
    ReconcileDerivation {
        /// The safe L2 block number.
        safe_l2: u64,
        /// The L1 block derivation is at, when known.
        current_l1: Option<u64>,
    },
    /// `reset`.
    Reset,
}

/// The calls a [`TrackingPipeline`] received, in order.
#[derive(Debug, Default)]
pub struct Recorded {
    /// Every recorded call, in call order.
    pub calls: Vec<PipelineCall>,
}

impl Recorded {
    /// The submission ids handed out by `next_submission`, in order.
    pub fn dequeued(&self) -> Vec<SubmissionId> {
        self.pick(|call| match call {
            PipelineCall::Dequeue(id) => Some(*id),
            _ => None,
        })
    }

    /// The submission ids passed to `confirm`, in order.
    pub fn confirmed(&self) -> Vec<SubmissionId> {
        self.pick(|call| match call {
            PipelineCall::Confirm(id, _) => Some(*id),
            _ => None,
        })
    }

    /// The submission ids passed to `requeue`, in order.
    pub fn requeued(&self) -> Vec<SubmissionId> {
        self.pick(|call| match call {
            PipelineCall::Requeue(id) => Some(*id),
            _ => None,
        })
    }

    /// The L1 heads the pipeline advanced to, in order.
    pub fn l1_heads(&self) -> Vec<u64> {
        self.pick(|call| match call {
            PipelineCall::AdvanceL1Head(l1_block) => Some(*l1_block),
            _ => None,
        })
    }

    /// The safe L2 block numbers passed to `reconcile_derivation`, in order.
    pub fn reconciled(&self) -> Vec<u64> {
        self.pick(|call| match call {
            PipelineCall::ReconcileDerivation { safe_l2, .. } => Some(*safe_l2),
            _ => None,
        })
    }

    /// The number of `reset` calls.
    pub fn resets(&self) -> usize {
        self.count(PipelineCall::Reset)
    }

    /// The number of `flush` calls.
    pub fn flushes(&self) -> usize {
        self.count(PipelineCall::Flush)
    }

    /// The number of `step` calls that encoded a block.
    pub fn encoded_steps(&self) -> usize {
        self.count(PipelineCall::Step)
    }

    fn pick<T>(&self, pick: impl Fn(&PipelineCall) -> Option<T>) -> Vec<T> {
        self.calls.iter().filter_map(pick).collect()
    }

    fn count(&self, call: PipelineCall) -> usize {
        self.calls.iter().filter(|&&recorded| recorded == call).count()
    }
}

/// [`BatchPipeline`] that records its calls into a shared [`Recorded`] and hands out the
/// submissions queued in [`submissions`](Self::submissions).
///
/// Like the real pipeline, it returns a requeued submission before the queued ones (here
/// under its original id), and ignores the confirmation or requeue of a submission dequeued
/// before a reset.
#[derive(Debug)]
pub struct TrackingPipeline {
    /// The call log, shared with the test through [`recorded`](Self::recorded).
    recorded: Arc<Mutex<Recorded>>,
    /// Submissions returned by `next_submission`, in FIFO order.
    pub submissions: VecDeque<BatchSubmission>,
    /// Value returned by `da_backlog_bytes`, shared so a test can change it while the driver
    /// runs.
    pub da_backlog_bytes: Arc<AtomicU64>,
    /// Submissions handed out and neither confirmed nor requeued since.
    in_flight: Vec<BatchSubmission>,
    /// The L1 head the pipeline is at. Like the real encoder, a head that does not advance is
    /// ignored.
    l1_head: u64,
    /// What `reconcile_derivation` answers.
    reconciliation: DerivationReconciliation,
    /// Whether `add_block` reports a parent mismatch.
    add_block_reorgs: bool,
    /// When set, `flush` records the call then returns this error.
    flush_error: Option<StepError>,
    /// Blocks left to encode: `step` reports one encoded block per call while above zero.
    encoding_steps: usize,
}

impl Default for TrackingPipeline {
    fn default() -> Self {
        Self {
            recorded: Arc::default(),
            submissions: VecDeque::new(),
            da_backlog_bytes: Arc::default(),
            in_flight: Vec::new(),
            l1_head: 0,
            reconciliation: DerivationReconciliation::Consistent,
            add_block_reorgs: false,
            flush_error: None,
            encoding_steps: 0,
        }
    }
}

impl TrackingPipeline {
    /// Create a pipeline that answers every call as if it were consistent and idle.
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared call log. Take it before handing the pipeline to the driver.
    pub fn recorded(&self) -> Arc<Mutex<Recorded>> {
        Arc::clone(&self.recorded)
    }

    /// Make `step` report `steps` encoded blocks before reporting idle.
    pub const fn with_encoding_steps(mut self, steps: usize) -> Self {
        self.encoding_steps = steps;
        self
    }

    /// Set the value returned by `da_backlog_bytes`.
    pub fn with_da_backlog(self, bytes: u64) -> Self {
        self.da_backlog_bytes.store(bytes, Ordering::SeqCst);
        self
    }

    /// Set what `reconcile_derivation` answers.
    pub const fn with_reconciliation(mut self, reconciliation: DerivationReconciliation) -> Self {
        self.reconciliation = reconciliation;
        self
    }

    /// Make `add_block` report a parent mismatch.
    pub const fn with_add_block_reorg(mut self) -> Self {
        self.add_block_reorgs = true;
        self
    }

    /// Make `flush` fail after recording the call.
    pub fn with_flush_error(mut self, error: StepError) -> Self {
        self.flush_error = Some(error);
        self
    }

    fn record(&self, call: PipelineCall) {
        self.recorded.lock().unwrap().calls.push(call);
    }

    /// A copy of `submission`, which is not `Clone`. The copy shares its frames, held behind
    /// `Arc`s.
    fn duplicate(submission: &BatchSubmission) -> BatchSubmission {
        match submission.payload() {
            SubmissionPayload::Blobs(payloads) => {
                BatchSubmission::blobs(submission.id, payloads.clone())
            }
            SubmissionPayload::Calldata(frame) => {
                BatchSubmission::calldata(submission.id, Arc::clone(frame))
            }
        }
    }
}

impl BatchPipeline for TrackingPipeline {
    fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
        self.record(PipelineCall::AddBlock(block.header.number));
        if self.add_block_reorgs {
            let error =
                ReorgError::ParentMismatch { expected: B256::ZERO, got: B256::with_last_byte(1) };
            return Err((error, Box::new(block)));
        }
        Ok(())
    }

    fn step(&mut self) -> Result<StepResult, StepError> {
        if self.encoding_steps == 0 {
            return Ok(StepResult::Idle);
        }
        self.encoding_steps -= 1;
        self.record(PipelineCall::Step);
        Ok(StepResult::BlockEncoded)
    }

    fn next_submission(&mut self) -> Option<BatchSubmission> {
        let submission = self.submissions.pop_front()?;
        self.record(PipelineCall::Dequeue(submission.id));
        self.in_flight.push(Self::duplicate(&submission));
        Some(submission)
    }

    fn confirm(&mut self, id: SubmissionId, l1_block: u64) {
        self.record(PipelineCall::Confirm(id, l1_block));
        self.in_flight.retain(|submission| submission.id != id);
    }

    fn requeue(&mut self, id: SubmissionId) {
        self.record(PipelineCall::Requeue(id));
        if let Some(index) = self.in_flight.iter().position(|submission| submission.id == id) {
            self.submissions.push_front(self.in_flight.remove(index));
        }
    }

    fn flush(&mut self) -> Result<(), StepError> {
        self.record(PipelineCall::Flush);
        self.flush_error.take().map_or(Ok(()), Err)
    }

    fn advance_l1_head(&mut self, l1_block: u64) {
        if l1_block > self.l1_head {
            self.l1_head = l1_block;
            self.record(PipelineCall::AdvanceL1Head(l1_block));
        }
    }

    fn reconcile_derivation(
        &mut self,
        safe_l2: BlockInfo,
        current_l1: Option<u64>,
    ) -> DerivationReconciliation {
        self.record(PipelineCall::ReconcileDerivation { safe_l2: safe_l2.number, current_l1 });
        self.reconciliation
    }

    fn reset(&mut self) {
        self.record(PipelineCall::Reset);
        self.submissions.clear();
        self.in_flight.clear();
        self.encoding_steps = 0;
    }

    fn da_backlog_bytes(&self) -> u64 {
        self.da_backlog_bytes.load(Ordering::SeqCst)
    }
}
