//! Integration tests for reorg handling in [`BatchDriver`].

use std::time::Duration;

use base_batcher_core::test_utils::{
    BlockStub, DriverFixture, PipelineCall, ScriptedTxManager, SubmissionStub, TrackingPipeline,
    TrackingSource,
};
use base_batcher_source::{L2BlockEvent, test_utils::ChannelBlockSource};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// A block that does not build on the buffered chain resets the pipeline, and the source
/// starts again from the safe head.
#[test]
fn test_add_block_reorg_resets_pipeline_and_source() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new().with_add_block_reorg();
        let recorded = pipeline.recorded();
        let (source, catchup_heads) = TrackingSource::new();
        let source =
            source.with_events([L2BlockEvent::Block(Box::new(BlockStub::with_number(11)))]);

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .safe_head(BlockStub::info(10))
                .build();
        let handle = ctx.spawn(driver.run());
        ctx.sleep(Duration::from_millis(10)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            recorded.lock().unwrap().calls,
            [PipelineCall::AddBlock(11), PipelineCall::Reset, PipelineCall::Flush]
        );
        assert_eq!(*catchup_heads.lock().unwrap(), [BlockStub::info(10)]);
    });
}

/// A reorg the source reports resets the pipeline, and the source starts again from the safe
/// head.
#[test]
fn test_l2_reorg_event_resets_pipeline_and_source() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        let (source, catchup_heads) = TrackingSource::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source.with_events([L2BlockEvent::Reorg]))
                .safe_head(BlockStub::info(10))
                .build();
        let handle = ctx.spawn(driver.run());
        ctx.sleep(Duration::from_millis(10)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(recorded.lock().unwrap().calls, [PipelineCall::Reset, PipelineCall::Flush]);
        assert_eq!(*catchup_heads.lock().unwrap(), [BlockStub::info(10)]);
    });
}

/// A submission in flight when the pipeline resets must stay tracked and settle normally
/// once its receipt arrives.
#[test]
fn test_reorg_keeps_tracking_in_flight_submissions() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        pipeline.submissions.push_back(SubmissionStub::stub());
        let tx_manager = ScriptedTxManager::new([]);
        let (source, source_tx) = ChannelBlockSource::new();
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, tx_manager.clone()).source(source).build();
        let handle = ctx.spawn(driver.run());

        // Let the driver submit the stub, then reorg while it is in flight.
        ctx.sleep(Duration::from_millis(10)).await;
        source_tx.send(L2BlockEvent::Reorg).unwrap();
        ctx.sleep(Duration::from_millis(10)).await;

        assert_eq!(recorded.lock().unwrap().resets(), 1);
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
        assert_eq!(recorded.lock().unwrap().l1_heads(), [7]);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}
