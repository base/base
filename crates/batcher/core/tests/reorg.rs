//! Integration tests for reorg handling in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use base_batcher_core::test_utils::{
    DriverFixture, ImmediateConfirmTxManager, ManualConfirmTxManager, OneBlockSource, Recorded,
    ReorgPipeline, SubmissionStub, TrackingPipeline,
};
use base_batcher_source::{L2BlockEvent, test_utils::ChannelBlockSource};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// When `add_block` returns a `ReorgError`, the driver must reset the pipeline
/// instead of propagating a fatal error. This mirrors the `L2BlockEvent::Reorg`
/// handling path.
#[test]
fn test_add_block_reorg_resets_pipeline_instead_of_fatal_error() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = ReorgPipeline::new(Arc::clone(&recorded));

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .source(OneBlockSource::new())
                .build();
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
/// pipeline. This is distinct from the `add_block`-triggered reorg path tested
/// above.
#[test]
fn test_l2_reorg_event_resets_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (source, source_tx) = ChannelBlockSource::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .source(source)
                .build();
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

/// A submission in flight when the pipeline resets must stay tracked and settle normally
/// once its receipt arrives.
#[test]
fn test_reorg_keeps_tracking_in_flight_submissions() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::stub());
        let tx_manager = ManualConfirmTxManager::default();
        let (source, source_tx) = ChannelBlockSource::new();
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, tx_manager.clone()).source(source).build();
        let handle = ctx.spawn(driver.run());

        // Let the driver submit the stub, then reorg while it is in flight.
        ctx.sleep(Duration::from_millis(10)).await;
        source_tx.send(L2BlockEvent::Reorg).unwrap();
        ctx.sleep(Duration::from_millis(10)).await;

        assert_eq!(recorded.lock().unwrap().resets, 1);
        assert_eq!(
            handles.admin.get_status().await.unwrap().in_flight,
            1,
            "the reset must not drop the in-flight submission"
        );

        tx_manager.confirm_next(7);
        ctx.sleep(Duration::from_millis(10)).await;

        assert_eq!(
            handles.admin.get_status().await.unwrap().in_flight,
            0,
            "the receipt must settle the submission after the reset"
        );
        assert_eq!(recorded.lock().unwrap().l1_heads, vec![7]);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}
