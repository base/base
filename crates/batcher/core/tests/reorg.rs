//! Integration tests for reorg handling in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, B256};
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, DaThrottle, NoopThrottleClient, ThrottleController,
    test_utils::{
        ImmediateConfirmTxManager, OneBlockSource, OneReorgPipeline, PendingL1HeadSource, Recorded,
        ReorgPipeline, TrackingPipeline,
    },
};
use base_batcher_source::{ChannelBlockSource, L2BlockEvent};
use base_protocol::{BlockInfo, L2BlockInfo};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// When `add_block` returns `ReorgError`, the driver must reset the pipeline and
/// call `reset_catchup` on the source so it re-delivers all post-reorg blocks
/// sequentially. The triggering block must NOT be re-added directly — the source
/// will re-deliver it via sequential catchup.
#[test]
fn test_reorg_triggers_pipeline_reset_and_catchup() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let blocks_accepted = Arc::new(Mutex::new(0usize));
        let resets = Arc::new(Mutex::new(0usize));
        let pipeline = OneReorgPipeline::new(Arc::clone(&blocks_accepted), Arc::clone(&resets));

        let driver = BatchDriver::new(
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

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(*resets.lock().unwrap(), 1, "pipeline must be reset on reorg");
        // The triggering block is NOT re-added directly; the source re-delivers it
        // via reset_catchup. In this test OneBlockSource is a no-op so blocks_accepted stays 0.
        assert_eq!(
            *blocks_accepted.lock().unwrap(),
            0,
            "block must not be re-added directly; source will re-deliver via catchup"
        );
    });
}

/// When `add_block` returns a `ReorgError`, the driver must reset the pipeline
/// and discard in-flight futures instead of propagating a fatal error. This
/// mirrors the `L2BlockEvent::Reorg` handling path.
#[test]
fn test_add_block_reorg_resets_pipeline_instead_of_fatal_error() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = ReorgPipeline::new(Arc::clone(&recorded));

        let driver = BatchDriver::new(
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
/// `add_block`-triggered reorg path tested in
/// `test_reorg_block_is_readded_after_reset`.
#[test]
fn test_l2_reorg_event_resets_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (source, source_tx) = ChannelBlockSource::new();

        let driver = BatchDriver::new(
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

        let reorg_head =
            L2BlockInfo::new(BlockInfo::new(B256::ZERO, 5, B256::ZERO, 0), Default::default(), 0);
        source_tx.send(L2BlockEvent::Reorg { new_safe_head: reorg_head }).unwrap();
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
