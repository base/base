//! Integration tests for L1 and safe L2 head handling in [`BatchDriver`].

use std::time::Duration;

use alloy_primitives::B256;
use base_batcher_core::{
    BatchDriverError, DerivationStatus,
    test_utils::{
        DriverFixture, PipelineCall, ScriptedTxManager, TrackingPipeline, TrackingSource,
    },
};
use base_batcher_encoder::DerivationReconciliation;
use base_batcher_source::test_utils::ChannelL1HeadSource;
use base_protocol::BlockInfo;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

fn safe_head(number: u64) -> BlockInfo {
    BlockInfo { hash: B256::with_last_byte(number as u8), number, ..Default::default() }
}

/// When the L1 head source delivers a new head, the driver must call
/// `advance_l1_head` on the pipeline with the new value.
#[test]
fn test_l1_head_source_advances_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();

        let (l1_source, l1_tx) = ChannelL1HeadSource::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .l1_head_source(l1_source)
                .build();
        let handle = ctx.spawn(driver.run());

        // Send a new L1 head via the channel.
        l1_tx.send(42).unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(recorded.lock().unwrap().l1_heads(), [42]);
    });
}

#[test]
fn test_safe_head_conflicts_reset_pipeline_and_source() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline =
            TrackingPipeline::new().with_reconciliation(DerivationReconciliation::SafeHeadMismatch);
        let recorded = pipeline.recorded();
        let (source, _source_tx, catchup_heads) = TrackingSource::new();

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .safe_head(safe_head(10))
                .build();
        let handle = ctx.spawn(driver.run());
        let status_tx = handles.derivation_status_tx;

        let regressed = safe_head(5);
        let replacement =
            BlockInfo { hash: B256::repeat_byte(0xff), number: 5, ..Default::default() };
        status_tx.send(DerivationStatus::from_safe_l2(regressed)).await.unwrap();
        status_tx.send(DerivationStatus::from_safe_l2(replacement)).await.unwrap();
        status_tx.send(DerivationStatus::from_safe_l2(safe_head(10))).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.resets(), 3);
        assert_eq!(recorded.reconciled(), [10]);
        assert_eq!(*catchup_heads.lock().unwrap(), [regressed, replacement, safe_head(10)]);
    });
}

#[test]
fn test_derivation_cursor_advance_replays_stalled_channel() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline =
            TrackingPipeline::new().with_reconciliation(DerivationReconciliation::StalledChannel);
        let recorded = pipeline.recorded();
        let (source, _source_tx, catchup_heads) = TrackingSource::new();
        let safe_l2 = safe_head(10);

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .safe_head(safe_l2)
                .build();
        let handle = ctx.spawn(driver.run());
        let status_tx = handles.derivation_status_tx;

        status_tx.send(DerivationStatus::new(safe_l2, safe_head(50))).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        // Reconciliation runs once, the reset follows, and the shutdown flush ends the log.
        assert_eq!(
            recorded.lock().unwrap().calls,
            [
                PipelineCall::ReconcileDerivation { safe_l2: safe_l2.number, current_l1: Some(50) },
                PipelineCall::Reset,
                PipelineCall::Flush,
            ]
        );
        assert_eq!(*catchup_heads.lock().unwrap(), [safe_l2]);
    });
}

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

#[test]
fn test_derivation_status_sender_drop_during_shutdown_is_clean() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let (driver, handles) = DriverFixture::new(
            ctx.clone(),
            TrackingPipeline::new(),
            ScriptedTxManager::confirming_at(1),
        )
        .build();

        ctx.cancel();
        drop(handles);
        assert!(driver.run().await.is_ok());
    });
}
