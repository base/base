//! Integration tests for L1 head source and safe head watch behaviour in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, DaThrottle, NoopThrottleClient, ThrottleController,
    test_utils::{
        DriverFixture, ImmediateConfirmTxManager, PendingSource, Recorded, SubmissionStub,
        TrackingPipeline,
    },
};
use base_batcher_source::{ChannelL1HeadSource, L1HeadEvent};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};
use tokio::sync::watch;

/// When the L1 head source delivers a new head, the driver must call
/// `advance_l1_head` on the pipeline with the new value.
#[test]
fn test_l1_head_source_advances_pipeline() {
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

/// When a safe head watch receiver fires, the driver must call
/// `prune_safe` on the pipeline with the new value.
#[test]
fn test_safe_head_watch_prunes_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (safe_tx, safe_rx) = watch::channel(0u64);

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_safe_head_rx(safe_rx);
        let handle = ctx.spawn(driver.run());

        // Send a new safe head.
        safe_tx.send(100).unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let r = recorded.lock().unwrap();
        assert!(
            r.safe_numbers.contains(&100),
            "prune_safe must be called with the watch value, got {:?}",
            r.safe_numbers
        );
    });
}

/// When the safe head sender is dropped while the driver is running, the watch
/// arm must disable itself rather than spinning. The driver continues running
/// and remains cancellable after the sender disappears.
#[test]
fn test_safe_head_sender_drop_does_not_busyloop() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (safe_tx, safe_rx) = watch::channel(0u64);

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_safe_head_rx(safe_rx);
        let handle = ctx.spawn(driver.run());

        // Send one value, then drop the sender while the driver is still running.
        safe_tx.send(50).unwrap();
        ctx.sleep(Duration::from_millis(20)).await;
        drop(safe_tx);

        // Give the driver time to process the drop. If the arm busy-loops,
        // prune_safe would be called many additional times here.
        ctx.sleep(Duration::from_millis(50)).await;
        let prune_count_after_drop = recorded.lock().unwrap().safe_numbers.len();

        // Cancel and wait — driver must exit cleanly, not hang.
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok(), "driver must exit cleanly after sender drop");

        let r = recorded.lock().unwrap();
        assert!(
            r.safe_numbers.contains(&50),
            "prune_safe must have been called with the sent value"
        );
        // After the sender drops, prune_safe must not be called again.
        assert_eq!(
            r.safe_numbers.len(),
            prune_count_after_drop,
            "prune_safe must not be called after sender drop (arm must be disabled)"
        );
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
