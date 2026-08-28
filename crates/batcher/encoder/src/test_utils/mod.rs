//! Test utilities for the batcher encoder.

use std::collections::VecDeque;

use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;

use crate::{
    BatchPipeline, BatchSubmission, DerivationReconciliation, ReorgError, StepError, StepResult,
    SubmissionId,
};

/// A mock implementation of [`BatchPipeline`] for testing downstream consumers
/// such as the [`BatchDriver`](crate::BatchPipeline).
///
/// Records all method calls for assertion in tests.
#[derive(Debug, Default)]
pub struct MockBatchPipeline {
    /// Blocks that were added via [`add_block`](BatchPipeline::add_block).
    pub blocks_added: Vec<BaseBlock>,
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
    /// Safe L2 block numbers passed to derivation reconciliation.
    pub safe_l2_numbers_reconciled: Vec<u64>,
}

impl BatchPipeline for MockBatchPipeline {
    fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
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

    fn has_ready_submission(&self) -> bool {
        !self.submissions.is_empty()
    }

    fn confirm(&mut self, id: SubmissionId, l1_block: u64) {
        self.confirmed.push((id, l1_block));
    }

    fn requeue(&mut self, id: SubmissionId) {
        self.requeued.push(id);
    }

    fn flush(&mut self) -> Result<(), StepError> {
        Ok(())
    }

    fn advance_l1_head(&mut self, l1_block: u64) {
        self.l1_heads.push(l1_block);
    }

    fn reconcile_derivation(
        &mut self,
        safe_l2: BlockInfo,
        _: Option<u64>,
    ) -> DerivationReconciliation {
        self.safe_l2_numbers_reconciled.push(safe_l2.number);
        DerivationReconciliation::Consistent
    }

    fn reset(&mut self) {
        self.resets += 1;
    }

    fn da_backlog_bytes(&self) -> u64 {
        0
    }
}
