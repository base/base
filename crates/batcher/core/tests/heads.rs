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
use base_batcher_source::L2BlockEvent;
use base_protocol::BlockInfo;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// A status reporting `safe_l2` with derivation at L1 block `current_l1`.
fn status(safe_l2: BlockInfo, current_l1: u64) -> DerivationStatus {
    DerivationStatus { safe_l2, current_l1: BlockStub::info(current_l1) }
}

/// A lower safe head from a node that has not read L1 past the block the last safe head was
/// reported at is the status of a node behind on L1. The driver neither reconciles it nor
/// resets, and the reset an L2 reorg triggers meanwhile restarts the source from the last safe
/// head, not the lower one.
#[test]
fn test_lower_safe_head_from_a_node_behind_on_l1_is_ignored() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        let (source, catchup_heads) = TrackingSource::new();

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source.with_events([L2BlockEvent::Reorg]))
                .derivation_status(status(BlockStub::info(10), 5))
                .build();
        let status_tx = handles.derivation_status_tx;

        // The first status, at the L1 block of the last one, is served before the source reorg
        // and the second, below it, after. The catchup head then shows neither moved the safe
        // head.
        status_tx.send(status(BlockStub::info(5), 5)).await.unwrap();
        let handle = ctx.spawn(driver.run());
        status_tx.send(status(BlockStub::info(5), 3)).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(recorded.lock().unwrap().calls, [PipelineCall::Reset, PipelineCall::Flush]);
        assert_eq!(*catchup_heads.lock().unwrap(), [BlockStub::info(10)]);
    });
}

/// A lower safe head is judged against the last status acted on, so the L1 block a node must
/// have read past moves with every reconciled status.
#[test]
fn test_lower_safe_head_is_judged_against_the_last_status_acted_on() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        let (source, catchup_heads) = TrackingSource::new();

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .derivation_status(status(BlockStub::info(10), 5))
                .build();
        let handle = ctx.spawn(driver.run());
        let status_tx = handles.derivation_status_tx;

        // Past the initial L1 block, but not past the one of the reconciled status.
        status_tx.send(status(BlockStub::info(12), 8)).await.unwrap();
        status_tx.send(status(BlockStub::info(11), 7)).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            recorded.lock().unwrap().calls,
            [PipelineCall::ReconcileDerivation { safe_l2: 12, current_l1: 8 }, PipelineCall::Flush]
        );
        assert!(catchup_heads.lock().unwrap().is_empty());
    });
}

/// A lower safe head from a node that has read L1 past the block the last safe head was
/// reported at means L1 lost the data that made it safe. The driver resets the pipeline and
/// restarts the source from the lower safe head, without reconciling.
#[test]
fn test_lower_safe_head_from_a_node_ahead_on_l1_resets_pipeline_and_source() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        let (source, catchup_heads) = TrackingSource::new();

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .derivation_status(status(BlockStub::info(10), 5))
                .build();
        let handle = ctx.spawn(driver.run());

        handles.derivation_status_tx.send(status(BlockStub::info(5), 6)).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(recorded.lock().unwrap().calls, [PipelineCall::Reset, PipelineCall::Flush]);
        assert_eq!(*catchup_heads.lock().unwrap(), [BlockStub::info(5)]);
    });
}

/// A safe head at the same height as the last one with another hash goes through
/// reconciliation: the pipeline finds it off the buffered chain, and the driver resets the
/// pipeline and restarts the source from it.
#[test]
fn test_safe_head_off_the_buffered_chain_resets_pipeline_and_source() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline =
            TrackingPipeline::new().with_reconciliation(DerivationReconciliation::SafeHeadMismatch);
        let recorded = pipeline.recorded();
        let (source, catchup_heads) = TrackingSource::new();

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .derivation_status(status(BlockStub::info(10), 5))
                .build();
        let handle = ctx.spawn(driver.run());

        let replacement =
            BlockInfo { hash: B256::repeat_byte(0xff), number: 10, ..Default::default() };
        handles.derivation_status_tx.send(status(replacement, 5)).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            recorded.lock().unwrap().calls,
            [
                PipelineCall::ReconcileDerivation { safe_l2: 10, current_l1: 5 },
                PipelineCall::Reset,
                PipelineCall::Flush,
            ]
        );
        assert_eq!(*catchup_heads.lock().unwrap(), [replacement]);
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
                .derivation_status(status(safe_l2, 1))
                .build();
        let handle = ctx.spawn(driver.run());

        handles.derivation_status_tx.send(status(safe_l2, 50)).await.unwrap();
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
