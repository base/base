//! Integration tests for the [`BatchDriver`] shutdown drain and the order of work and waiting
//! in its loop.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use base_batcher_core::{
    BatchDriverError,
    test_utils::{
        DRAIN_TIMEOUT, DriverFixture, Recorded, ScriptedTxManager, SubmissionStub, TrackingPipeline,
    },
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
        let cancelled_at = ctx.now();
        ctx.cancel();

        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "driver must exit after drain timeout even with in-flight submissions"
        );
        assert_eq!(
            ctx.now() - cancelled_at,
            DRAIN_TIMEOUT,
            "the driver must wait the drain timeout for the in-flight submission"
        );
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.dequeued(), [SubmissionId(0)], "submission must have been dequeued");
        assert_eq!(recorded.flushes(), 1, "flush must be called on shutdown");
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
        assert_eq!(
            ctx.now() - cancelled_at,
            DRAIN_TIMEOUT,
            "in-flight receipts must be drained before the flush error is returned"
        );
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.dequeued(), [SubmissionId(0)]);
        assert_eq!(recorded.flushes(), 1);
    });
}

/// Source that never delivers an event and records, each time the driver waits on it, how
/// many submissions have been dequeued so far.
///
/// Hand-rolled rather than mocked because the count must be read when the driver polls, not
/// when the mock's scripted response is built.
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

/// The driver does all the work it can before waiting for the next event. A receipt that
/// frees an in-flight slot gets the next ready submission sent before any source is waited on
/// again.
#[test]
fn test_driver_finishes_pending_work_before_waiting_for_events() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        pipeline.submissions.push_back(SubmissionStub::stub());
        pipeline.submissions.push_back(SubmissionStub::stub());
        let tx_manager = ScriptedTxManager::new([]);
        let dequeued_at_poll = Arc::new(Mutex::new(Vec::new()));
        let source = PollRecorder { recorded, dequeued_at_poll: Arc::clone(&dequeued_at_poll) };

        // With one tx in flight at most, the second submission can only leave the pipeline once
        // the receipt of the first one has been processed.
        let (driver, _handles) = DriverFixture::new(ctx.clone(), pipeline, tx_manager.clone())
            .source(source)
            .max_pending(1)
            .build();
        let handle = ctx.spawn(driver.run());

        // Let the driver submit the first stub and wait on the source, then confirm that stub
        // so the receipt releases the second one.
        ctx.sleep(Duration::from_millis(1)).await;
        let polls_before_receipt = dequeued_at_poll.lock().unwrap().len();
        tx_manager.confirm_next(1);
        ctx.sleep(Duration::from_millis(1)).await;

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
        let polls = dequeued_at_poll.lock().unwrap();
        let (before_receipt, after_receipt) = polls.split_at(polls_before_receipt);
        assert!(
            !before_receipt.is_empty() && before_receipt.iter().all(|&dequeued| dequeued == 1),
            "the driver must wait on the source with the first submission in flight: {polls:?}"
        );
        assert!(
            !after_receipt.is_empty() && after_receipt.iter().all(|&dequeued| dequeued == 2),
            "the submission released by the receipt must be sent before the driver waits again: {polls:?}"
        );
    });
}
