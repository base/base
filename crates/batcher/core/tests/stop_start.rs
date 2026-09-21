//! Integration tests for stop/start admin commands in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use base_batcher_core::{
    AdminError, AdminHandle, BatchDriver, BatchDriverConfig, BatchDriverError, DaThrottle,
    DerivationStatus, NoopThrottleClient, ThrottleController,
    test_utils::{
        DriverFixture, ImmediateConfirmTxManager, ManualConfirmTxManager, NeverConfirmTxManager,
        PendingL1HeadSource, Recorded, SubmissionStub, TrackingPipeline, TrackingSource,
    },
};
use base_batcher_encoder::{
    BatchPipeline, BatchSubmission, ChannelLimit, DerivationReconciliation, ReorgError, StepError,
    StepResult, SubmissionId,
};
use base_batcher_source::{ChannelBlockSource, L2BlockEvent};
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};
use tokio::sync::mpsc;

/// `AdminCommand::Stop` must immediately reset the pipeline. Stopping a batcher
/// that is already stopped succeeds without resetting it again.
#[test]
fn test_stop_resets_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (admin_handle, admin_rx) = AdminHandle::channel();

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        admin_handle.stop().await.unwrap();
        admin_handle.stop().await.unwrap();
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            recorded.lock().unwrap().resets,
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
        let (source, catchup_args) = TrackingSource::new();
        let (admin_handle, admin_rx) = AdminHandle::channel();
        let (derivation_status_tx, derivation_status_rx) = mpsc::channel(1);
        let safe_head = BlockInfo { number: 42, ..Default::default() };

        let driver = BatchDriver::new_without_derivation_status(
            ctx.clone(),
            TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default()))),
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
        )
        .with_admin_rx(admin_rx)
        .with_derivation_status_rx(DerivationStatus::from_safe_l2(safe_head), derivation_status_rx);

        let handle = ctx.spawn(driver.run());

        // Stop then start with safe_head = 42; the source will poll 43 next.
        admin_handle.stop().await.unwrap();
        admin_handle.start().await.unwrap();
        admin_handle.start().await.unwrap();
        ctx.cancel();

        // Keep the derivation-status channel alive until the driver stops.
        drop(derivation_status_tx);
        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            *catchup_args.lock().unwrap(),
            vec![safe_head],
            "source must be reanchored at the safe head on start"
        );
    });
}

/// While stopped, `Block` and `Flush` source events must be dropped; the
/// pipeline must not receive any blocks.
#[test]
fn test_stopped_drops_block_and_flush_events() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let (admin_handle, admin_rx) = AdminHandle::channel();
        let (source, source_tx) = ChannelBlockSource::new();

        // Use a pipeline variant that counts add_block calls.
        let add_block_calls = Arc::new(Mutex::new(0usize));
        struct CountingPipeline {
            calls: Arc<Mutex<usize>>,
            inner: TrackingPipeline,
        }
        impl BatchPipeline for CountingPipeline {
            fn add_block(&mut self, block: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
                *self.calls.lock().unwrap() += 1;
                self.inner.add_block(block)
            }
            fn step(&mut self) -> Result<StepResult, StepError> {
                self.inner.step()
            }
            fn next_submission(&mut self) -> Option<BatchSubmission> {
                self.inner.next_submission()
            }
            fn has_ready_submission(&self) -> bool {
                self.inner.has_ready_submission()
            }
            fn confirm(&mut self, id: SubmissionId, n: u64) {
                self.inner.confirm(id, n);
            }
            fn requeue(&mut self, id: SubmissionId) {
                self.inner.requeue(id);
            }
            fn flush(&mut self) -> Result<(), StepError> {
                self.inner.flush()
            }
            fn advance_l1_head(&mut self, n: u64) {
                self.inner.advance_l1_head(n);
            }
            fn reconcile_derivation(
                &mut self,
                safe_l2: BlockInfo,
                current_l1: Option<u64>,
            ) -> DerivationReconciliation {
                self.inner.reconcile_derivation(safe_l2, current_l1)
            }
            fn reset(&mut self) {
                self.inner.reset();
            }
            fn da_backlog_bytes(&self) -> u64 {
                self.inner.da_backlog_bytes()
            }
        }

        let pipeline = CountingPipeline {
            calls: Arc::clone(&add_block_calls),
            inner: TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default()))),
        };

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
        )
        .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        // Stop, then send a block — it must be dropped.
        admin_handle.stop().await.unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        source_tx.send(L2BlockEvent::Block(Box::default())).unwrap();
        ctx.sleep(Duration::from_millis(10)).await;

        // A flush's ack must also be dropped (not silently leaked/hung) while stopped, so a
        // waiter sees an immediate closed-channel error rather than an indefinite wait.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        source_tx.send(L2BlockEvent::Flush { ack: Some(ack_tx) }).unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        assert!(
            ack_rx.await.is_err(),
            "flush ack must be dropped (not fired) while the batcher is stopped"
        );

        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            *add_block_calls.lock().unwrap(),
            0,
            "add_block must not be called while stopped"
        );
    });
}

/// A stop is answered only once the submissions in flight have settled, and the
/// driver keeps serving other admin requests while it waits.
#[test]
fn test_stop_waits_for_in_flight_submissions() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline = TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default())));
        pipeline.submissions.push_back(SubmissionStub::stub());
        let tx_manager = ManualConfirmTxManager::default();
        let (admin_handle, admin_rx) = AdminHandle::channel();

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, tx_manager.clone()).with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        // Request a stop while the stub is in flight. It must not answer yet.
        let mut stop = ctx.spawn({
            let admin_handle = admin_handle.clone();
            async move { admin_handle.stop().await }
        });
        ctx.sleep(Duration::from_millis(1)).await;
        assert!(futures::poll!(&mut stop).is_pending());

        // Check that the driver still answers other admin requests while the stop waits.
        let status = admin_handle.get_status().await.unwrap();
        assert!(status.stopped);
        assert_eq!(status.in_flight, 1);

        // Confirm the submission so the stop can answer.
        tx_manager.confirm_next(1);
        assert!(stop.await.unwrap().is_ok());
        assert_eq!(admin_handle.get_status().await.unwrap().in_flight, 0);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// If submissions are still in flight after the drain timeout, the stop reports
/// it and the batcher stays stopped.
#[test]
fn test_stop_times_out_and_stays_stopped() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline = TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default())));
        pipeline.submissions.push_back(SubmissionStub::stub());
        let (admin_handle, admin_rx) = AdminHandle::channel();

        let driver = DriverFixture::build(ctx.clone(), pipeline, NeverConfirmTxManager)
            .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        let result = admin_handle.stop().await;

        assert!(matches!(result, Err(AdminError::StopTimeout { in_flight: 1 })));
        assert!(admin_handle.get_status().await.unwrap().stopped);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// A start received while a stop is still waiting wins: the batcher runs again
/// and the stop reports that it was superseded.
#[test]
fn test_start_supersedes_pending_stop() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline = TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default())));
        pipeline.submissions.push_back(SubmissionStub::stub());
        let (admin_handle, admin_rx) = AdminHandle::channel();

        let driver = DriverFixture::build(ctx.clone(), pipeline, NeverConfirmTxManager)
            .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        // Request a stop that cannot complete because the stub never confirms.
        let stop = ctx.spawn({
            let admin_handle = admin_handle.clone();
            async move { admin_handle.stop().await }
        });
        ctx.sleep(Duration::from_millis(1)).await;

        admin_handle.start().await.unwrap();

        assert!(matches!(stop.await.unwrap(), Err(AdminError::StopSuperseded)));
        assert!(!admin_handle.get_status().await.unwrap().stopped);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// Stop requests that arrive while an earlier one is waiting share its deadline, so repeating
/// the call cannot extend the wait.
#[test]
fn test_repeated_stops_share_the_deadline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline = TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default())));
        pipeline.submissions.push_back(SubmissionStub::stub());
        let (admin_handle, admin_rx) = AdminHandle::channel();

        let driver = DriverFixture::build(ctx.clone(), pipeline, NeverConfirmTxManager)
            .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        let first = ctx.spawn({
            let admin_handle = admin_handle.clone();
            async move { admin_handle.stop().await }
        });
        ctx.sleep(Duration::from_millis(5)).await;
        let second = admin_handle.stop().await;

        assert!(matches!(first.await.unwrap(), Err(AdminError::StopTimeout { in_flight: 1 })));
        assert!(matches!(second, Err(AdminError::StopTimeout { in_flight: 1 })));
        assert_eq!(ctx.now(), Duration::from_millis(10));

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// A flush on a running batcher closes the current channel and reports success.
#[test]
fn test_flush_closes_the_channel_on_a_running_batcher() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (admin_handle, admin_rx) = AdminHandle::channel();

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        admin_handle.flush().await.unwrap();

        assert_eq!(recorded.lock().unwrap().flush_count, 1);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// A stopped batcher refuses to flush instead of reporting a flush that produces nothing.
#[test]
fn test_flush_is_rejected_while_stopped() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (admin_handle, admin_rx) = AdminHandle::channel();

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        admin_handle.stop().await.unwrap();
        let result = admin_handle.flush().await;

        assert!(matches!(result, Err(AdminError::Stopped)));
        assert_eq!(recorded.lock().unwrap().flush_count, 0);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// A flush failure is fatal for the driver; the admin caller must still get the error.
#[test]
fn test_flush_failure_is_reported_before_the_driver_exits() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default())))
            .with_flush_error(StepError::BlockExceedsChannelLimit {
                cursor: 0,
                limit: ChannelLimit::RlpBytes { required: 1, maximum: 0 },
            });
        let (admin_handle, admin_rx) = AdminHandle::channel();

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_admin_rx(admin_rx);
        let handle = ctx.spawn(driver.run());

        let result = admin_handle.flush().await;

        assert!(matches!(result, Err(AdminError::FlushFailed(_))));
        assert!(matches!(handle.await.unwrap(), Err(BatchDriverError::Step(_))));
    });
}
