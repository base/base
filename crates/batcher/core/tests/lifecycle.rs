//! Integration tests for [`BatchDriver`] lifecycle: source exhaustion, flush, drain, and
//! the order of work and waiting in the loop.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use async_trait::async_trait;
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, BatchDriverError, DaThrottle, NoopThrottleClient,
    ThrottleController,
    test_utils::{
        DriverFixture, ImmediateConfirmTxManager, ManualConfirmTxManager, NeverConfirmTxManager,
        PendingL1HeadSource, Recorded, SubmissionStub, TrackingPipeline,
    },
};
use base_batcher_encoder::{ChannelLimit, StepError, SubmissionId};
use base_batcher_source::{
    L2BlockEvent, SourceError, UnsafeBlockSource,
    test_utils::{ChannelBlockSource, InMemoryBlockSource},
};
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

        let driver = BatchDriver::new_without_derivation_status(
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
            recorded.lock().unwrap().flush_count,
            1,
            "flush must be called once on source exhaustion shutdown"
        );
    });
}

/// When the source delivers `L2BlockEvent::Flush`, the driver must call
/// `flush` immediately. On subsequent shutdown it is called once
/// more, giving a total of two calls.
#[test]
fn test_flush_event_calls_pipeline_flush() {
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
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        source_tx.send(L2BlockEvent::Flush { ack: None }).unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        // Flush arm: +1; Shutdown arm: +1 → total 2
        assert_eq!(
            recorded.lock().unwrap().flush_count,
            2,
            "flush must be called for the event and again on shutdown"
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
        assert_eq!(r.flush_count, 1, "flush must be called on shutdown");
    });
}

/// A flush error on shutdown must not skip the in-flight receipt drain.
#[test]
fn test_shutdown_drains_in_flight_before_returning_flush_error() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_flush_error(
            StepError::BlockExceedsChannelLimit {
                cursor: 0,
                limit: ChannelLimit::RlpBytes { required: 1, maximum: 0 },
            },
        );
        pipeline.submissions.push_back(SubmissionStub::stub());

        let driver = DriverFixture::build(ctx.clone(), pipeline, NeverConfirmTxManager);
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(20)).await;
        let cancelled_at = ctx.now();
        ctx.cancel();

        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(BatchDriverError::Step(_))),
            "flush error must be returned after drain"
        );
        assert!(
            ctx.now().saturating_sub(cancelled_at) >= Duration::from_millis(10),
            "in-flight receipts must be drained before the flush error is returned"
        );
        let r = recorded.lock().unwrap();
        assert_eq!(r.dequeued, vec![SubmissionId(0)]);
        assert_eq!(r.flush_count, 1);
    });
}

/// Source that never delivers an event and records, each time the driver waits on it, how
/// many submissions have been dequeued so far.
///
/// Hand-rolled rather than mocked: the count must be read when the driver polls, not when
/// the mock's scripted response is built.
struct PollRecorder {
    recorded: Arc<Mutex<Recorded>>,
    dequeued_at_poll: Arc<Mutex<Vec<usize>>>,
}

#[async_trait]
impl UnsafeBlockSource for PollRecorder {
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        let dequeued = self.recorded.lock().unwrap().dequeued.len();
        self.dequeued_at_poll.lock().unwrap().push(dequeued);
        std::future::pending().await
    }
}

/// The driver does all the work it can before waiting for the next event: a receipt that
/// frees a permit gets the next ready submission sent before any source is waited on again.
#[test]
fn test_driver_finishes_pending_work_before_waiting_for_events() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::with_id(0));
        pipeline.submissions.push_back(SubmissionStub::with_id(1));
        let tx_manager = ManualConfirmTxManager::default();
        let dequeued_at_poll = Arc::new(Mutex::new(Vec::new()));
        let source = PollRecorder {
            recorded: Arc::clone(&recorded),
            dequeued_at_poll: Arc::clone(&dequeued_at_poll),
        };

        // A single permit: the second submission can only leave the pipeline once the
        // receipt of the first one has been processed.
        let driver = BatchDriver::new_without_derivation_status(
            ctx.clone(),
            pipeline,
            source,
            tx_manager.clone(),
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

        // Let the driver submit the first stub and wait on the source, then confirm that stub
        // so the receipt releases the second one.
        ctx.sleep(Duration::from_millis(1)).await;
        tx_manager.confirm_next(1);
        ctx.sleep(Duration::from_millis(1)).await;

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            *dequeued_at_poll.lock().unwrap(),
            vec![1, 2],
            "the submission released by the receipt must be sent before the driver waits again"
        );
    });
}
