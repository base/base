//! Integration tests for [`BatchDriver`] lifecycle: source exhaustion, flush, and drain.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, DaThrottle, NoopThrottleClient, ThrottleController,
    test_utils::{
        DriverFixture, ImmediateConfirmTxManager, NeverConfirmTxManager, PendingL1HeadSource,
        Recorded, SubmissionStub, TrackingPipeline,
    },
};
use base_batcher_encoder::SubmissionId;
use base_batcher_source::{ChannelBlockSource, L2BlockEvent, test_utils::InMemoryBlockSource};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// When the block source returns `SourceError::Exhausted`, the driver must
/// treat it as a graceful shutdown signal: close the current channel,
/// drain in-flight submissions within the timeout, then exit cleanly.
#[test]
fn test_source_exhaustion_shuts_down_driver_gracefully() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let driver = BatchDriver::new(
            ctx.clone(),
            pipeline,
            InMemoryBlockSource::new(), // empty → Exhausted immediately
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        );

        let handle = ctx.spawn(driver.run());
        ctx.sleep(Duration::from_millis(50)).await;

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "driver must exit cleanly when source exhausts");
        assert_eq!(
            recorded.lock().unwrap().force_close_count,
            1,
            "force_close_channel must be called once on source exhaustion shutdown"
        );
    });
}

/// When the source delivers `L2BlockEvent::Flush`, the driver must call
/// `force_close_channel` immediately. On subsequent shutdown it is called once
/// more, giving a total of two calls.
#[test]
fn test_flush_event_calls_force_close_channel() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (source, source_tx) = ChannelBlockSource::new();

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
        );
        let handle = ctx.spawn(driver.run());

        source_tx.send(L2BlockEvent::Flush).unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        // Flush arm: +1; Shutdown arm: +1 → total 2
        assert_eq!(
            recorded.lock().unwrap().force_close_count,
            2,
            "force_close_channel must be called for Flush and again on shutdown"
        );
    });
}

/// When cancellation fires while a submission is in-flight with a
/// `NeverConfirmTxManager`, the drain timeout must fire and the driver must
/// exit cleanly. This verifies the `runtime.sleep(drain_timeout)` fix.
#[test]
fn test_drain_timeout_exits_with_in_flight_submissions() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::stub());

        let driver = DriverFixture::build(ctx.clone(), pipeline, NeverConfirmTxManager);
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(20)).await;
        ctx.cancel();

        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "driver must exit after drain timeout even with in-flight submissions"
        );
        let r = recorded.lock().unwrap();
        assert_eq!(r.dequeued, vec![SubmissionId(0)], "submission must have been dequeued");
        assert_eq!(r.force_close_count, 1, "force_close_channel must be called on shutdown");
    });
}
