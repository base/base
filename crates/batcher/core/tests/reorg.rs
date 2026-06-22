//! Integration tests for reorg handling in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, B256};
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, DaThrottle, NoopThrottleClient, ThrottleController,
    test_utils::{
        ImmediateConfirmTxManager, OneBlockSource, PendingL1HeadSource, Recorded, ReorgPipeline,
        TrackingPipeline,
    },
};
use base_batcher_encoder::{
    BatchPipeline, BatchSubmission, DerivationReconciliation, ReorgError, StepError, StepResult,
    SubmissionId,
};
use base_batcher_source::{ChannelBlockSource, L2BlockEvent};
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// [`BatchPipeline`] that rejects the first `add_block` call and accepts all subsequent ones.
///
/// Used to verify that on a reorg the driver resets the pipeline and does not re-add the
/// triggering block directly — the source re-delivers it via sequential catchup.
#[derive(Debug)]
struct OneReorgPipeline {
    /// Incremented each time `add_block` succeeds (post-reorg re-adds).
    blocks_accepted: Arc<Mutex<usize>>,
    /// Whether the next `add_block` call should simulate a reorg.
    fail_next: bool,
    /// Incremented each time `reset()` is called.
    resets: Arc<Mutex<usize>>,
}

impl OneReorgPipeline {
    /// Create a new pipeline backed by the given shared counters.
    const fn new(blocks_accepted: Arc<Mutex<usize>>, resets: Arc<Mutex<usize>>) -> Self {
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

    fn has_ready_submission(&self) -> bool {
        false
    }

    fn confirm(&mut self, _: SubmissionId, _: u64) {}
    fn requeue(&mut self, _: SubmissionId) {}
    fn force_close_channel(&mut self) {}
    fn advance_l1_head(&mut self, _: u64) {}
    fn reconcile_derivation(&mut self, _: BlockInfo, _: Option<u64>) -> DerivationReconciliation {
        DerivationReconciliation::Consistent
    }

    fn reset(&mut self) {
        *self.resets.lock().unwrap() += 1;
    }

    fn da_backlog_bytes(&self) -> u64 {
        0
    }
}

/// When `add_block` returns `ReorgError`, the driver must reset the pipeline and
/// call `reset_catchup` on the source so it re-delivers all post-reorg blocks
/// sequentially. The triggering block must NOT be re-added directly — the source
/// will re-deliver it via sequential catchup.
#[test]
fn test_reorg_triggers_pipeline_reset_and_catchup() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let blocks_accepted = Arc::new(Mutex::new(0usize));
        let resets = Arc::new(Mutex::new(0usize));
        let pipeline = OneReorgPipeline::new(Arc::clone(&blocks_accepted), Arc::clone(&resets));

        let driver = BatchDriver::new_without_derivation_status(
            ctx.clone(),
            pipeline,
            OneBlockSource::new(),
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
                alt_da: None,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(*resets.lock().unwrap(), 1, "pipeline must be reset on reorg");
        // The triggering block is NOT re-added directly; the source re-delivers it
        // via reset_catchup. In this test OneBlockSource is a no-op so blocks_accepted stays 0.
        assert_eq!(
            *blocks_accepted.lock().unwrap(),
            0,
            "block must not be re-added directly; source will re-deliver via catchup"
        );
    });
}

/// When `add_block` returns a `ReorgError`, the driver must reset the pipeline
/// and discard in-flight futures instead of propagating a fatal error. This
/// mirrors the `L2BlockEvent::Reorg` handling path.
#[test]
fn test_add_block_reorg_resets_pipeline_instead_of_fatal_error() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = ReorgPipeline::new(Arc::clone(&recorded));

        let driver = BatchDriver::new_without_derivation_status(
            ctx.clone(),
            pipeline,
            OneBlockSource::new(),
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
                alt_da: None,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "driver must not return a fatal error on add_block reorg");
        assert_eq!(
            recorded.lock().unwrap().resets,
            1,
            "pipeline.reset() must be called when add_block returns ReorgError"
        );
    });
}

/// When the source delivers `L2BlockEvent::Reorg`, the driver must reset the
/// pipeline and discard in-flight submissions. This is distinct from the
/// `add_block`-triggered reorg path tested above.
#[test]
fn test_l2_reorg_event_resets_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (source, source_tx) = ChannelBlockSource::new();

        let driver = BatchDriver::new_without_derivation_status(
            ctx.clone(),
            pipeline,
            source,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
                alt_da: None,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        source_tx.send(L2BlockEvent::Reorg).unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            recorded.lock().unwrap().resets,
            1,
            "pipeline must be reset when source delivers a Reorg event"
        );
    });
}
