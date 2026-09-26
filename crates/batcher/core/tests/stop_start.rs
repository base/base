//! Integration tests for the stop, start and flush admin commands in [`BatchDriver`].

use std::time::Duration;

use base_batcher_core::{
    AdminError,
    test_utils::{
        BlockStub, DriverFixture, ScriptedTxManager, SubmissionStub, TrackingPipeline,
        TrackingSource,
    },
};
use base_batcher_source::{L2BlockEvent, test_utils::ChannelBlockSource};
use base_protocol::BlockInfo;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// `AdminCommand::Stop` must immediately reset the pipeline. Stopping a batcher
/// that is already stopped succeeds without resetting it again.
#[test]
fn test_stop_resets_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1)).build();
        let handle = ctx.spawn(driver.run());

        handles.admin.stop().await.unwrap();
        handles.admin.stop().await.unwrap();
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            recorded.lock().unwrap().resets(),
            1,
            "pipeline must be reset exactly once when stopped"
        );
    });
}

/// `AdminCommand::Start` must reanchor the source at the safe head so it
/// delivers missed blocks sequentially after that head. Starting a batcher that
/// is already running must not reanchor it again: that would replay blocks the
/// pipeline already holds.
#[test]
fn test_start_triggers_catchup_from_safe_head() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let (source, _source_tx, catchup_heads) = TrackingSource::new();
        let safe_head = BlockInfo { number: 42, ..Default::default() };

        let (driver, handles) = DriverFixture::new(
            ctx.clone(),
            TrackingPipeline::new(),
            ScriptedTxManager::confirming_at(1),
        )
        .source(source)
        .safe_head(safe_head)
        .build();
        let handle = ctx.spawn(driver.run());

        // Stop then start with safe_head = 42; the source will poll 43 next.
        handles.admin.stop().await.unwrap();
        handles.admin.start().await.unwrap();
        handles.admin.start().await.unwrap();
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            *catchup_heads.lock().unwrap(),
            [safe_head],
            "source must be reanchored at the safe head on start"
        );
    });
}

/// While stopped, the batcher does not read its source: the pipeline receives no blocks.
/// Once started again, the blocks queued meanwhile reach the pipeline.
#[test]
fn test_stopped_leaves_the_source_unread() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        let (source, source_tx) = ChannelBlockSource::new();
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .build();
        let handle = ctx.spawn(driver.run());

        // Stop, then send a block: it stays in the source.
        handles.admin.stop().await.unwrap();
        source_tx.send(L2BlockEvent::Block(Box::new(BlockStub::with_number(1)))).unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        assert!(
            recorded.lock().unwrap().added_blocks().is_empty(),
            "a stopped batcher must not ingest blocks"
        );

        // Start: the queued block and the next one reach the pipeline.
        handles.admin.start().await.unwrap();
        source_tx.send(L2BlockEvent::Block(Box::new(BlockStub::with_number(2)))).unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        assert_eq!(
            recorded.lock().unwrap().added_blocks(),
            [1, 2],
            "a started batcher must ingest the queued blocks"
        );

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// A stop does not wait for the submissions in flight. They keep settling while the
/// batcher is stopped, and the status reports how many remain.
#[test]
fn test_stop_leaves_in_flight_submissions_to_settle() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline = TrackingPipeline::new();
        pipeline.submissions.push_back(SubmissionStub::stub());
        let tx_manager = ScriptedTxManager::new([]);
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, tx_manager.clone()).build();
        let handle = ctx.spawn(driver.run());

        // Stop while the stub is in flight.
        handles.admin.stop().await.unwrap();
        let status = handles.admin.get_status().await.unwrap();
        assert!(status.stopped);
        assert_eq!(status.in_flight, 1);

        // Confirm the submission and let the driver process the receipt.
        tx_manager.confirm_next(1);
        ctx.sleep(Duration::from_millis(1)).await;
        assert_eq!(handles.admin.get_status().await.unwrap().in_flight, 0);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// A flush on a running batcher closes the current channel and reports success.
#[test]
fn test_flush_closes_the_channel_on_a_running_batcher() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1)).build();
        let handle = ctx.spawn(driver.run());

        handles.admin.flush().await.unwrap();

        assert_eq!(recorded.lock().unwrap().flushes(), 1);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// A stopped batcher refuses to flush instead of reporting a flush that produces nothing.
#[test]
fn test_flush_is_rejected_while_stopped() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1)).build();
        let handle = ctx.spawn(driver.run());

        handles.admin.stop().await.unwrap();
        let result = handles.admin.flush().await;

        assert!(matches!(result, Err(AdminError::Stopped)));
        assert_eq!(recorded.lock().unwrap().flushes(), 0);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}
