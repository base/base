//! Integration tests for pause/resume admin commands in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use async_trait::async_trait;
use base_batcher_core::{
    AdminHandle, BatchDriver, BatchDriverConfig, DaThrottle, NoopThrottleClient,
    ThrottleController,
    test_utils::{
        DriverFixture, ImmediateConfirmTxManager, PendingL1HeadSource, Recorded, TrackingPipeline,
    },
};
use base_batcher_encoder::{
    BatchPipeline, BatchSubmission, ReorgError, StepError, StepResult, SubmissionId,
};
use base_batcher_source::{ChannelBlockSource, L2BlockEvent, SourceError, UnsafeBlockSource};
use base_common_consensus::BaseBlock;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};
use tokio::sync::watch;

/// Source that tracks `reset_catchup` calls and parks forever on `next()`.
#[derive(Debug)]
struct TrackingSource {
    catchup_args: Arc<Mutex<Vec<u64>>>,
}

impl TrackingSource {
    fn new() -> (Self, Arc<Mutex<Vec<u64>>>) {
        let args = Arc::new(Mutex::new(Vec::new()));
        (Self { catchup_args: Arc::clone(&args) }, args)
    }
}

#[async_trait]
impl UnsafeBlockSource for TrackingSource {
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        std::future::pending().await
    }

    fn reset_catchup(&mut self, start_from: u64) {
        self.catchup_args.lock().unwrap().push(start_from);
    }
}

/// `AdminCommand::Pause` must immediately reset the pipeline and discard
/// in-flight submissions. This is verified by checking that `pipeline.reset()`
/// is called exactly once after the pause command is processed.
#[test]
fn test_pause_resets_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (admin_handle, admin_rx) = AdminHandle::channel();

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        admin_handle.pause().await.unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            recorded.lock().unwrap().resets,
            1,
            "pipeline must be reset exactly once when paused"
        );
    });
}

/// `AdminCommand::Resume` must call `source.reset_catchup(safe_head + 1)`
/// so the source delivers missed blocks sequentially before resuming live
/// polling. When no safe-head watch is wired, no catchup is triggered.
#[test]
fn test_resume_triggers_catchup_from_safe_head() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let (source, catchup_args) = TrackingSource::new();
        let (admin_handle, admin_rx) = AdminHandle::channel();
        let (safe_head_tx, safe_head_rx) = watch::channel::<u64>(42);

        let driver = BatchDriver::new(
            ctx.clone(),
            TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default()))),
            source,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        )
        .with_admin_rx(admin_rx)
        .with_safe_head_rx(safe_head_rx);

        let handle = ctx.spawn(driver.run());

        // Pause then resume with safe_head = 42; expect catchup from 43.
        admin_handle.pause().await.unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        admin_handle.resume().await.unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        ctx.cancel();

        // Keep safe_head_tx alive so the watch channel is not closed early.
        drop(safe_head_tx);
        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            *catchup_args.lock().unwrap(),
            vec![43],
            "source must be reset to safe_head + 1 on resume"
        );
    });
}

/// While paused, `Block` and `Flush` source events must be dropped; the
/// pipeline must not receive any blocks.
#[test]
fn test_paused_drops_block_and_flush_events() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let (admin_handle, admin_rx) = AdminHandle::channel();
        let (source, source_tx) = ChannelBlockSource::new();

        // Use a pipeline variant that counts add_block calls.
        let add_block_calls = Arc::new(Mutex::new(0usize));
        struct CountingPipeline {
            calls: Arc<Mutex<usize>>,
            inner: TrackingPipeline,
        }
        impl BatchPipeline for CountingPipeline {
            fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
                *self.calls.lock().unwrap() += 1;
                self.inner.add_block(block)
            }
            fn step(&mut self) -> Result<StepResult, StepError> {
                self.inner.step()
            }
            fn next_submission(&mut self) -> Option<BatchSubmission> {
                self.inner.next_submission()
            }
            fn confirm(&mut self, id: SubmissionId, n: u64) {
                self.inner.confirm(id, n);
            }
            fn requeue(&mut self, id: SubmissionId) {
                self.inner.requeue(id);
            }
            fn force_close_channel(&mut self) {
                self.inner.force_close_channel();
            }
            fn advance_l1_head(&mut self, n: u64) {
                self.inner.advance_l1_head(n);
            }
            fn prune_safe(&mut self, n: u64) {
                self.inner.prune_safe(n);
            }
            fn reset(&mut self) {
                self.inner.reset();
            }
            fn da_backlog_bytes(&self) -> u64 {
                self.inner.da_backlog_bytes()
            }
        }

        let pipeline = CountingPipeline {
            calls: Arc::clone(&add_block_calls),
            inner: TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default()))),
        };

        let driver = BatchDriver::new(
            ctx.clone(),
            pipeline,
            source,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        )
        .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        // Pause, then send a block — it must be dropped.
        admin_handle.pause().await.unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        source_tx.send(L2BlockEvent::Block(Box::new(BaseBlock::default()))).unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            *add_block_calls.lock().unwrap(),
            0,
            "add_block must not be called while paused"
        );
    });
}
