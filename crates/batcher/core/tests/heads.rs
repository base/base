//! Integration tests for L1 and safe L2 head handling in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, B256};
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, BatchDriverError, BatchDriverHeads, DaThrottle,
    DerivationStatus, NoopThrottleClient, ThrottleController,
    test_utils::{
        DriverFixture, ImmediateConfirmTxManager, PendingL1HeadSource, PendingSource, Recorded,
        SubmissionStub, TrackingPipeline, TrackingSource,
    },
};
use base_batcher_source::{ChannelL1HeadSource, L1HeadEvent};
use base_protocol::BlockInfo;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};
use tokio::sync::mpsc;

fn safe_head(number: u64) -> BlockInfo {
    BlockInfo { hash: B256::with_last_byte(number as u8), number, ..Default::default() }
}

/// When the L1 head source delivers a new head, the driver must call
/// `advance_l1_head` on the pipeline with the new value.
#[test]
fn test_l1_head_source_advances_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (l1_source, l1_tx) = ChannelL1HeadSource::new();

        let driver = BatchDriver::new_without_derivation_status(
            ctx.clone(),
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            l1_source,
        );
        let handle = ctx.spawn(driver.run());

        // Send a new L1 head via the channel.
        l1_tx.send(L1HeadEvent::NewHead(42)).unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let r = recorded.lock().unwrap();
        assert!(
            r.l1_heads.contains(&42),
            "advance_l1_head must be called with the source value, got {:?}",
            r.l1_heads
        );
    });
}

/// When the L1 head source is exhausted, the driver must disable that arm and
/// continue running — it must not shut down. The L1 head delivered before
/// exhaustion must be processed normally.
#[test]
fn test_l1_source_exhausted_disables_arm_driver_continues() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (l1_source, l1_tx) = ChannelL1HeadSource::new();

        let driver = BatchDriver::new_without_derivation_status(
            ctx.clone(),
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            l1_source,
        );
        let handle = ctx.spawn(driver.run());

        l1_tx.send(L1HeadEvent::NewHead(77)).unwrap();
        ctx.sleep(Duration::from_millis(20)).await;
        drop(l1_tx); // triggers Exhausted → L1SourceClosed

        // Driver must still be running after L1 source closes.
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver must continue after L1 source closes");
        let r = recorded.lock().unwrap();
        assert!(
            r.l1_heads.contains(&77),
            "L1 head delivered before close must be processed, got {:?}",
            r.l1_heads
        );
    });
}

#[test]
fn test_safe_head_conflicts_reset_pipeline_and_source() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_safe_head_match(false);
        let (status_tx, status_rx) = mpsc::channel(1);
        let (source, catchup_heads) = TrackingSource::new();

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
            BatchDriverHeads::new(
                PendingL1HeadSource,
                0,
                DerivationStatus::from_safe_l2(safe_head(10)),
                status_rx,
            ),
        );
        let handle = ctx.spawn(driver.run());

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
        assert_eq!(recorded.resets, 3);
        assert_eq!(recorded.safe_numbers, vec![10]);
        assert_eq!(*catchup_heads.lock().unwrap(), vec![regressed, replacement, safe_head(10)]);
    });
}

#[test]
fn test_derivation_cursor_advance_replays_stalled_channel() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_derivation_stalled(true);
        let (status_tx, status_rx) = mpsc::channel(1);
        let (source, catchup_heads) = TrackingSource::new();
        let safe_l2 = safe_head(10);

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
            BatchDriverHeads::new(
                PendingL1HeadSource,
                0,
                DerivationStatus::from_safe_l2(safe_l2),
                status_rx,
            ),
        );
        let handle = ctx.spawn(driver.run());

        status_tx.send(DerivationStatus::new(safe_l2, safe_head(50))).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.safe_numbers, vec![safe_l2.number]);
        assert_eq!(recorded.resets, 1);
        assert_eq!(*catchup_heads.lock().unwrap(), vec![safe_l2]);
    });
}

#[test]
fn test_queued_safe_head_preempts_submission() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::stub());
        let (status_tx, status_rx) = mpsc::channel(1);
        status_tx.send(DerivationStatus::from_safe_l2(safe_head(5))).await.unwrap();

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_derivation_status_rx(
                    DerivationStatus::from_safe_l2(safe_head(10)),
                    status_rx,
                );
        let handle = ctx.spawn(driver.run());
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.resets, 1);
        assert!(recorded.dequeued.is_empty());
    });
}

#[test]
fn test_derivation_status_sender_drop_is_fatal() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (status_tx, status_rx) = mpsc::channel(1);
        let driver = DriverFixture::build(ctx, pipeline, ImmediateConfirmTxManager { l1_block: 1 })
            .with_derivation_status_rx(DerivationStatus::from_safe_l2(safe_head(0)), status_rx);
        drop(status_tx);

        assert!(matches!(driver.run().await, Err(BatchDriverError::DerivationStatusSourceClosed)));
    });
}

#[test]
fn test_derivation_status_sender_drop_during_shutdown_is_clean() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default())));
        let (status_tx, status_rx) = mpsc::channel(1);
        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_derivation_status_rx(DerivationStatus::from_safe_l2(safe_head(0)), status_rx);

        ctx.cancel();
        drop(status_tx);
        assert!(driver.run().await.is_ok());
    });
}
