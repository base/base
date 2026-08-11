//! Integration tests for reorg handling in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, DaThrottle, NoopThrottleClient, ThrottleController,
    test_utils::{
        ImmediateConfirmTxManager, OneBlockSource, PendingL1HeadSource, Recorded, ReorgPipeline,
        TrackingPipeline,
    },
};
use base_batcher_source::{ChannelBlockSource, L2BlockEvent};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// When `add_block` returns a `ReorgError`, the driver must reset the pipeline
/// and discard in-flight futures instead of propagating a fatal error. This
/// mirrors the `L2BlockEvent::Reorg` handling path.
#[test]
fn test_add_block_reorg_resets_pipeline_instead_of_fatal_error() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = ReorgPipeline::new(Arc::clone(&recorded));

        let driver = BatchDriver::new_without_derivation_status(
            ctx.clone(),
            pipeline,
            OneBlockSource::new(),
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
/// pipeline and discard in-flight submissions. This is distinct from the
/// `add_block`-triggered reorg path tested above.
#[test]
fn test_l2_reorg_event_resets_pipeline() {
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
