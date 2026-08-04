//! Integration tests for L1 and safe L2 head handling in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, B256};
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, BatchDriverError, DaThrottle, NoopThrottleClient,
    ThrottleController,
    test_utils::{
        DriverFixture, ImmediateConfirmTxManager, PendingSource, Recorded, SubmissionStub,
        TrackingPipeline,
    },
};
use base_batcher_source::{ChannelL1HeadSource, L1HeadEvent};
use base_protocol::BlockInfo;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};
use tokio::sync::mpsc;

fn safe_head(number: u64) -> BlockInfo {
    BlockInfo { hash: B256::with_last_byte(number as u8), number, ..Default::default() }
}

/// When the L1 head source delivers a new head, the driver must call
/// `advance_l1_head` on the pipeline with the new value.
#[test]
fn test_l1_head_source_advances_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_safe_head_match(false);

        let (l1_source, l1_tx) = ChannelL1HeadSource::new();

        let driver = BatchDriver::new(
            ctx.clone(),
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            l1_source,
        );
        let handle = ctx.spawn(driver.run());

        // Send a new L1 head via the channel.
        l1_tx.send(L1HeadEvent::NewHead(42)).unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let r = recorded.lock().unwrap();
        assert!(
            r.l1_heads.contains(&42),
            "advance_l1_head must be called with the source value, got {:?}",
            r.l1_heads
        );
    });
}

/// When the L1 head source is exhausted, the driver must disable that arm and
/// continue running — it must not shut down. The L1 head delivered before
/// exhaustion must be processed normally.
#[test]
fn test_l1_source_exhausted_disables_arm_driver_continues() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (l1_source, l1_tx) = ChannelL1HeadSource::new();

        let driver = BatchDriver::new(
            ctx.clone(),
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            l1_source,
        );
        let handle = ctx.spawn(driver.run());

        l1_tx.send(L1HeadEvent::NewHead(77)).unwrap();
        ctx.sleep(Duration::from_millis(20)).await;
        drop(l1_tx); // triggers Exhausted → L1SourceClosed

        // Driver must still be running after L1 source closes.
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver must continue after L1 source closes");
        let r = recorded.lock().unwrap();
        assert!(
            r.l1_heads.contains(&77),
            "L1 head delivered before close must be processed, got {:?}",
            r.l1_heads
        );
    });
}

/// When a safe-head update arrives, the driver must call
/// `prune_safe` on the pipeline with the new value.
#[test]
fn test_safe_head_update_prunes_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (safe_tx, safe_rx) = mpsc::channel(1);

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_safe_head_rx(safe_head(0), safe_rx);
        let handle = ctx.spawn(driver.run());

        // Send a new safe head.
        safe_tx.send(safe_head(100)).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let r = recorded.lock().unwrap();
        assert!(
            r.safe_numbers.contains(&100),
            "prune_safe must be called with the safe-head value, got {:?}",
            r.safe_numbers
        );
    });
}

#[test]
fn test_safe_head_regression_resets_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_safe_head_match(false);
        let (safe_tx, safe_rx) = mpsc::channel(1);

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_safe_head_rx(safe_head(10), safe_rx);
        let handle = ctx.spawn(driver.run());

        safe_tx.send(safe_head(5)).await.unwrap();
        safe_tx.send(safe_head(10)).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.resets, 2);
        assert!(recorded.safe_numbers.contains(&10));
    });
}

#[test]
fn test_safe_head_sender_drop_is_fatal() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (safe_tx, safe_rx) = mpsc::channel(1);
        let driver = DriverFixture::build(ctx, pipeline, ImmediateConfirmTxManager { l1_block: 1 })
            .with_safe_head_rx(safe_head(0), safe_rx);
        drop(safe_tx);

        assert!(matches!(driver.run().await, Err(BatchDriverError::SafeHeadSourceClosed)));
    });
}

/// Without a safe head receiver, confirmation-based L1 head advancement must
/// still work normally. The driver uses `PendingL1HeadSource` (parks forever)
/// so only submission confirmations drive `advance_l1_head`.
#[test]
fn test_no_safe_head_receiver_driver_runs_normally() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::stub());

        // No .with_safe_head_rx() — safe_head remains None.
        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 7 });
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let r = recorded.lock().unwrap();
        assert_eq!(r.l1_heads, vec![7], "confirmation-based advance_l1_head must still work");
        assert!(r.safe_numbers.is_empty(), "prune_safe must not be called without a receiver");
    });
}
