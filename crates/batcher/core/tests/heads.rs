//! Integration tests for safe L2 head handling in [`BatchDriver`].

use std::time::Duration;

use alloy_primitives::B256;
use base_batcher_core::{
    BatchDriverError, DerivationStatus,
    test_utils::{
        BlockStub, DriverFixture, PipelineCall, ScriptedTxManager, TrackingPipeline, TrackingSource,
    },
};
use base_batcher_encoder::DerivationReconciliation;
use base_protocol::BlockInfo;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// A lower safe head, a different block at the same height and a safe head missing from the
/// buffered chain each reset the pipeline and restart the source from the new safe head. The
/// driver catches the first two itself and sends only the third through reconciliation.
#[test]
fn test_safe_head_conflicts_reset_pipeline_and_source() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline =
            TrackingPipeline::new().with_reconciliation(DerivationReconciliation::SafeHeadMismatch);
        let recorded = pipeline.recorded();
        let (source, catchup_heads) = TrackingSource::new();

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .safe_head(BlockStub::info(10))
                .build();
        let handle = ctx.spawn(driver.run());
        let status_tx = handles.derivation_status_tx;

        let regressed = BlockStub::info(5);
        let replacement =
            BlockInfo { hash: B256::repeat_byte(0xff), number: 5, ..Default::default() };
        for safe_l2 in [regressed, replacement, BlockStub::info(10)] {
            status_tx
                .send(DerivationStatus { safe_l2, current_l1: BlockStub::info(1) })
                .await
                .unwrap();
        }
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            recorded.lock().unwrap().calls,
            [
                PipelineCall::Reset,
                PipelineCall::Reset,
                PipelineCall::ReconcileDerivation { safe_l2: 10, current_l1: 1 },
                PipelineCall::Reset,
                PipelineCall::Flush,
            ]
        );
        assert_eq!(*catchup_heads.lock().unwrap(), [regressed, replacement, BlockStub::info(10)]);
    });
}

/// When derivation moves past a confirmed channel without making its blocks safe, the driver
/// resets the pipeline and restarts the source from the safe head, so those blocks are batched
/// again.
#[test]
fn test_stalled_channel_resets_pipeline_and_source() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline =
            TrackingPipeline::new().with_reconciliation(DerivationReconciliation::StalledChannel);
        let recorded = pipeline.recorded();
        let (source, catchup_heads) = TrackingSource::new();
        let safe_l2 = BlockStub::info(10);

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .safe_head(safe_l2)
                .build();
        let handle = ctx.spawn(driver.run());
        let status_tx = handles.derivation_status_tx;

        status_tx
            .send(DerivationStatus { safe_l2, current_l1: BlockStub::info(50) })
            .await
            .unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        // Reconciliation runs once, the reset follows, and the shutdown flush ends the log.
        assert_eq!(
            recorded.lock().unwrap().calls,
            [
                PipelineCall::ReconcileDerivation { safe_l2: safe_l2.number, current_l1: 50 },
                PipelineCall::Reset,
                PipelineCall::Flush,
            ]
        );
        assert_eq!(*catchup_heads.lock().unwrap(), [safe_l2]);
    });
}

/// The driver fails when the derivation-status source stops, instead of batching on without
/// ever learning the safe head again.
#[test]
fn test_derivation_status_sender_drop_is_fatal() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let (driver, handles) =
            DriverFixture::new(ctx, TrackingPipeline::new(), ScriptedTxManager::confirming_at(1))
                .build();
        drop(handles);

        assert!(matches!(driver.run().await, Err(BatchDriverError::DerivationStatusSourceClosed)));
    });
}
