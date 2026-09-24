//! Integration tests for [`BatchDriver`] lifecycle: drain and the order of work and waiting
//! in the loop.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use base_batcher_core::{
    BatchDriverError,
    test_utils::{DriverFixture, Recorded, ScriptedTxManager, SubmissionStub, TrackingPipeline},
};
use base_batcher_encoder::{ChannelLimit, StepError, SubmissionId};
use base_batcher_source::{L2BlockEvent, UnsafeBlockSource};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// When cancellation fires while a submission is in flight and never settles, the drain
/// timeout must fire and the driver must exit cleanly.
#[test]
fn test_drain_timeout_exits_with_in_flight_submissions() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        pipeline.submissions.push_back(SubmissionStub::stub());

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::new([])).build();
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(20)).await;
        ctx.cancel();

        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "driver must exit after drain timeout even with in-flight submissions"
        );
        let r = recorded.lock().unwrap();
        assert_eq!(r.dequeued(), [SubmissionId(0)], "submission must have been dequeued");
        assert_eq!(r.flushes(), 1, "flush must be called on shutdown");
    });
}

/// A flush error on shutdown must not skip the in-flight receipt drain.
#[test]
fn test_shutdown_drains_in_flight_before_returning_flush_error() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline =
            TrackingPipeline::new().with_flush_error(StepError::BlockExceedsChannelLimit {
                cursor: 0,
                limit: ChannelLimit::RlpBytes { required: 1, maximum: 0 },
            });
        let recorded = pipeline.recorded();
        pipeline.submissions.push_back(SubmissionStub::stub());

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::new([])).build();
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
        assert_eq!(r.dequeued(), [SubmissionId(0)]);
        assert_eq!(r.flushes(), 1);
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
    async fn next(&mut self) -> L2BlockEvent {
        let dequeued = self.recorded.lock().unwrap().dequeued().len();
        self.dequeued_at_poll.lock().unwrap().push(dequeued);
        std::future::pending().await
    }
}

/// The driver does all the work it can before waiting for the next event: a receipt that
/// frees an in-flight slot gets the next ready submission sent before any source is waited on
/// again.
#[test]
fn test_driver_finishes_pending_work_before_waiting_for_events() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        pipeline.submissions.push_back(SubmissionStub::with_id(0));
        pipeline.submissions.push_back(SubmissionStub::with_id(1));
        let tx_manager = ScriptedTxManager::new([]);
        let dequeued_at_poll = Arc::new(Mutex::new(Vec::new()));
        let source = PollRecorder { recorded, dequeued_at_poll: Arc::clone(&dequeued_at_poll) };

        // One tx in flight at most: the second submission can only leave the pipeline once the
        // receipt of the first one has been processed.
        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, tx_manager.clone()).source(source).build();
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
            [1, 2],
            "the submission released by the receipt must be sent before the driver waits again"
        );
    });
}
