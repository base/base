//! Integration tests for reorg handling in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use base_batcher_core::{
    AdminHandle, BatchDriver, BatchDriverConfig, DaThrottle, NoopThrottleClient,
    ThrottleController,
    test_utils::{
        ImmediateConfirmTxManager, ManualConfirmTxManager, OneBlockSource, PendingL1HeadSource,
        Recorded, ReorgPipeline, SubmissionStub, TrackingPipeline,
    },
};
use base_batcher_source::{ChannelBlockSource, L2BlockEvent};
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
/// pipeline. This is distinct from the `add_block`-triggered reorg path tested
/// above.
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
        let (admin_handle, admin_rx) = AdminHandle::channel();

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
        )
        .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        // Let the driver submit the stub, then reorg while it is in flight.
        ctx.sleep(Duration::from_millis(10)).await;
        source_tx.send(L2BlockEvent::Reorg).unwrap();
        ctx.sleep(Duration::from_millis(10)).await;

        assert_eq!(recorded.lock().unwrap().resets, 1);
        assert_eq!(
            admin_handle.get_status().await.unwrap().in_flight,
            1,
            "the reset must not drop the in-flight submission"
        );

        tx_manager.confirm_next(7);
        ctx.sleep(Duration::from_millis(10)).await;

        assert_eq!(
            admin_handle.get_status().await.unwrap().in_flight,
            0,
            "the receipt must settle the submission after the reset"
        );
        assert_eq!(recorded.lock().unwrap().l1_heads, vec![7]);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}
