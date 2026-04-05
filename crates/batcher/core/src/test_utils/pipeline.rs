//! Test [`BatchPipeline`] implementations for action-testing the batch driver.

use std::sync::{Arc, Mutex};

use alloy_primitives::B256;
use base_alloy_consensus::BaseBlock;
use base_batcher_encoder::{
    BatchPipeline, BatchSubmission, ReorgError, StepError, StepResult, SubmissionId,
};

/// Shared recording state populated by the test pipeline implementations.
#[derive(Debug, Default)]
pub struct Recorded {
    /// L1 block numbers passed to `advance_l1_head` in order.
    pub l1_heads: Vec<u64>,
    /// Submission IDs passed to `requeue` in order.
    pub requeued: Vec<SubmissionId>,
    /// Submission IDs dequeued via `next_submission` in order.
    pub dequeued: Vec<SubmissionId>,
    /// Number of times `reset()` was called.
    pub resets: usize,
    /// Safe L2 block numbers passed to `prune_safe` in order.
    pub safe_numbers: Vec<u64>,
    /// Number of times `force_close_channel()` was called.
    pub force_close_count: usize,
}

/// [`BatchPipeline`] that records every significant method call into a shared [`Recorded`].
///
/// Submissions are pre-loaded by pushing into [`TrackingPipeline::submissions`] before
/// handing the pipeline to the driver.
#[derive(Debug)]
pub struct TrackingPipeline {
    /// Shared recording state.
    pub recorded: Arc<Mutex<Recorded>>,
    /// Queue of submissions returned by `next_submission` in FIFO order.
    pub submissions: std::collections::VecDeque<BatchSubmission>,
    /// Value returned by `da_backlog_bytes`. Default: 0.
    da_backlog_bytes_value: u64,
}

impl TrackingPipeline {
    /// Create a new pipeline that records into `recorded`.
    pub fn new(recorded: Arc<Mutex<Recorded>>) -> Self {
        Self { recorded, submissions: Default::default(), da_backlog_bytes_value: 0 }
    }

    /// Set the value returned by `da_backlog_bytes`.
    pub const fn with_da_backlog(mut self, value: u64) -> Self {
        self.da_backlog_bytes_value = value;
        self
    }
}

impl BatchPipeline for TrackingPipeline {
    fn add_block(&mut self, _: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
        Ok(())
    }

    fn step(&mut self) -> Result<StepResult, StepError> {
        Ok(StepResult::Idle)
    }

    fn next_submission(&mut self) -> Option<BatchSubmission> {
        let sub = self.submissions.pop_front()?;
        self.recorded.lock().unwrap().dequeued.push(sub.id);
        Some(sub)
    }

    fn confirm(&mut self, _: SubmissionId, _: u64) {}

    fn requeue(&mut self, id: SubmissionId) {
        self.recorded.lock().unwrap().requeued.push(id);
    }

    fn force_close_channel(&mut self) {
        self.recorded.lock().unwrap().force_close_count += 1;
    }

    fn advance_l1_head(&mut self, l1_block: u64) {
        self.recorded.lock().unwrap().l1_heads.push(l1_block);
    }

    fn prune_safe(&mut self, safe_l2_number: u64) {
        self.recorded.lock().unwrap().safe_numbers.push(safe_l2_number);
    }

    fn reset(&mut self) {
        self.recorded.lock().unwrap().resets += 1;
    }

    fn da_backlog_bytes(&self) -> u64 {
        self.da_backlog_bytes_value
    }
}

/// [`BatchPipeline`] that always returns [`ReorgError`] from `add_block`.
///
/// Used to verify the driver's reorg-on-add path: it must reset the pipeline and
/// discard in-flight submissions rather than propagating a fatal error.
#[derive(Debug)]
pub struct ReorgPipeline {
    /// Shared recording state (only `resets` is incremented).
    pub recorded: Arc<Mutex<Recorded>>,
}

impl ReorgPipeline {
    /// Create a new pipeline that records resets into `recorded`.
    pub const fn new(recorded: Arc<Mutex<Recorded>>) -> Self {
        Self { recorded }
    }
}

impl BatchPipeline for ReorgPipeline {
    fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
        Err((
            ReorgError::ParentMismatch { expected: B256::ZERO, got: B256::with_last_byte(1) },
            Box::new(block),
        ))
    }

    fn step(&mut self) -> Result<StepResult, StepError> {
        Ok(StepResult::Idle)
    }

    fn next_submission(&mut self) -> Option<BatchSubmission> {
        None
    }

    fn confirm(&mut self, _: SubmissionId, _: u64) {}
    fn requeue(&mut self, _: SubmissionId) {}
    fn force_close_channel(&mut self) {}
    fn advance_l1_head(&mut self, _: u64) {}
    fn prune_safe(&mut self, _: u64) {}

    fn reset(&mut self) {
        self.recorded.lock().unwrap().resets += 1;
    }

    fn da_backlog_bytes(&self) -> u64 {
        0
    }
}

/// [`BatchPipeline`] that rejects the first `add_block` call and accepts all subsequent ones.
///
/// Used to verify that the driver re-adds the triggering block after `reset()`, so the
/// block is not silently lost when the source will not re-deliver it.
#[derive(Debug)]
pub struct OneReorgPipeline {
    /// Incremented each time `add_block` succeeds (post-reorg re-adds).
    pub blocks_accepted: Arc<Mutex<usize>>,
    /// Whether the next `add_block` call should simulate a reorg.
    fail_next: bool,
    /// Incremented each time `reset()` is called.
    pub resets: Arc<Mutex<usize>>,
}

impl OneReorgPipeline {
    /// Create a new pipeline backed by the given shared counters.
    pub const fn new(blocks_accepted: Arc<Mutex<usize>>, resets: Arc<Mutex<usize>>) -> Self {
        Self { blocks_accepted, fail_next: true, resets }
    }
}

impl BatchPipeline for OneReorgPipeline {
    fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
        if self.fail_next {
            self.fail_next = false;
            return Err((
                ReorgError::ParentMismatch { expected: B256::ZERO, got: B256::with_last_byte(1) },
                Box::new(block),
            ));
        }
        *self.blocks_accepted.lock().unwrap() += 1;
        Ok(())
    }

    fn step(&mut self) -> Result<StepResult, StepError> {
        Ok(StepResult::Idle)
    }

    fn next_submission(&mut self) -> Option<BatchSubmission> {
        None
    }

    fn confirm(&mut self, _: SubmissionId, _: u64) {}
    fn requeue(&mut self, _: SubmissionId) {}
    fn force_close_channel(&mut self) {}
    fn advance_l1_head(&mut self, _: u64) {}
    fn prune_safe(&mut self, _: u64) {}

    fn reset(&mut self) {
        *self.resets.lock().unwrap() += 1;
    }

    fn da_backlog_bytes(&self) -> u64 {
        0
    }
}
