//! Test utilities for the batcher encoder.

use std::collections::VecDeque;

use base_alloy_consensus::OpBlock;

use crate::{BatchPipeline, BatchSubmission, ReorgError, StepError, StepResult, SubmissionId};

/// A mock implementation of [`BatchPipeline`] for testing downstream consumers
/// such as the [`BatchDriver`](crate::BatchPipeline).
///
/// Records all method calls for assertion in tests.
#[derive(Debug, Default)]
pub struct MockBatchPipeline {
    /// Blocks that were added via [`add_block`](BatchPipeline::add_block).
    pub blocks_added: Vec<OpBlock>,
    /// Number of times [`step`](BatchPipeline::step) was called.
    pub steps_taken: usize,
    /// Submissions to return from [`next_submission`](BatchPipeline::next_submission).
    pub submissions: VecDeque<BatchSubmission>,
    /// Confirmed submissions (id, `l1_block`).
    pub confirmed: Vec<(SubmissionId, u64)>,
    /// Requeued submission ids.
    pub requeued: Vec<SubmissionId>,
    /// Number of times [`reset`](BatchPipeline::reset) was called.
    pub resets: usize,
    /// L1 heads that were advanced to.
    pub l1_heads: Vec<u64>,
}

impl BatchPipeline for MockBatchPipeline {
    fn add_block(&mut self, block: OpBlock) -> Result<(), Box<(ReorgError, OpBlock)>> {
        self.blocks_added.push(block);
        Ok(())
    }

    fn step(&mut self) -> Result<StepResult, StepError> {
        self.steps_taken += 1;
        Ok(StepResult::Idle)
    }

    fn next_submission(&mut self) -> Option<BatchSubmission> {
        self.submissions.pop_front()
    }

    fn confirm(&mut self, id: SubmissionId, l1_block: u64) {
        self.confirmed.push((id, l1_block));
    }

    fn requeue(&mut self, id: SubmissionId) {
        self.requeued.push(id);
    }

    fn advance_l1_head(&mut self, l1_block: u64) {
        self.l1_heads.push(l1_block);
    }

    fn reset(&mut self) {
        self.resets += 1;
    }

    fn da_backlog_bytes(&self) -> u64 {
        0
    }
}
