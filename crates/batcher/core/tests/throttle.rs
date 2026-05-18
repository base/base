//! Integration tests for DA throttle behaviour in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use async_trait::async_trait;
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, DaThrottle, ThrottleConfig, ThrottleController,
    ThrottleStrategy,
    test_utils::{
        ImmediateConfirmTxManager, PendingL1HeadSource, PendingSource, Recorded, TrackingPipeline,
        TrackingThrottleClient,
    },
};
use base_batcher_encoder::{
    BatchPipeline, BatchSubmission, ReorgError, StepError, StepResult, SubmissionId,
};
use base_batcher_source::{L2BlockEvent, SourceError, UnsafeBlockSource};
use base_common_consensus::BaseBlock;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};
use tokio::sync::mpsc;

/// When the DA backlog exceeds the threshold, the driver must call
/// `set_max_da_size` on the throttle client with reduced limits.
#[test]
fn test_throttle_client_called_on_high_backlog() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        // 2 MB backlog — above the default 1 MB threshold.
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(2_000_000);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let driver = BatchDriver::new(
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
            DaThrottle::new(throttle, Arc::new(throttle_client)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert!(!calls.is_empty(), "throttle client must be called when backlog is high");
        let (max_tx_size, max_block_size) = calls[0];
        assert!(
            max_block_size < 130_000,
            "max_block_size should be below upper limit when throttled, got {max_block_size}"
        );
        assert!(
            max_tx_size < 20_000,
            "max_tx_size should be below upper limit when throttled, got {max_tx_size}"
        );
    });
}

/// When the DA backlog is zero (below threshold), the driver must call
/// `set_max_da_size` with the upper limits to reset any previous throttle.
#[test]
fn test_throttle_client_called_with_upper_limits_on_zero_backlog() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(0);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let driver = BatchDriver::new(
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
            DaThrottle::new(throttle, Arc::new(throttle_client)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert!(!calls.is_empty(), "throttle client must be called even with zero backlog");
        let (max_tx_size, max_block_size) = calls[0];
        assert_eq!(
            max_block_size, 130_000,
            "max_block_size should be the upper limit when not throttling"
        );
        assert_eq!(
            max_tx_size, 20_000,
            "max_tx_size should be the upper limit when not throttling"
        );
    });
}

/// `set_max_da_size` must be called exactly once when limits do not change
/// between driver loop iterations.
#[test]
fn test_throttle_not_called_redundantly() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(0);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let driver = BatchDriver::new(
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
            DaThrottle::new(throttle, Arc::new(throttle_client)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        // Run for 100ms to allow multiple loop iterations.
        ctx.sleep(Duration::from_millis(100)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "set_max_da_size must be called exactly once when limits do not change, got {}",
            calls.len()
        );
    });
}

/// With the Step strategy and full intensity, when backlog is above the
/// threshold, the driver must apply the lower DA limits.
#[test]
fn test_step_strategy_full_intensity_applies_lower_limits() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        // Backlog of 100 — above threshold of 1.
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(100);

        let config =
            ThrottleConfig { threshold_bytes: 1, max_intensity: 1.0, ..Default::default() };
        let throttle = ThrottleController::new(config, ThrottleStrategy::Step);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let driver = BatchDriver::new(
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
            DaThrottle::new(throttle, Arc::new(throttle_client)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert!(!calls.is_empty(), "throttle client must be called with Step strategy");
        let (max_tx_size, max_block_size) = calls[0];
        assert_eq!(
            max_block_size, 2_000,
            "Step strategy at full intensity must apply block_size_lower_limit"
        );
        assert_eq!(
            max_tx_size, 150,
            "Step strategy at full intensity must apply tx_size_lower_limit"
        );
    });
}

/// Verifies that when the DA backlog transitions from above the threshold
/// (throttle active) to zero (throttle inactive), the driver makes exactly
/// two RPC calls: one with reduced limits and one resetting to upper limits.
#[test]
fn test_throttle_transitions_from_active_to_inactive() {
    // Pipeline whose DA backlog is controlled from the test via a shared lock.
    struct DynamicPipeline {
        backlog: Arc<Mutex<u64>>,
    }

    impl BatchPipeline for DynamicPipeline {
        fn add_block(&mut self, _: BaseBlock) -> Result<(), (ReorgError, Box<BaseBlock>)> {
            Ok(())
        }

        fn step(&mut self) -> Result<StepResult, StepError> {
            Ok(StepResult::Idle)
        }

        fn next_submission(&mut self) -> Option<BatchSubmission> {
            None
        }

        fn confirm(&mut self, _: SubmissionId, _: u64) {}
        fn requeue(&mut self, _: SubmissionId) {}
        fn force_close_channel(&mut self) {}
        fn advance_l1_head(&mut self, _: u64) {}
        fn prune_safe(&mut self, _: u64) {}
        fn reset(&mut self) {}

        fn da_backlog_bytes(&self) -> u64 {
            *self.backlog.lock().unwrap()
        }
    }

    // Source driven by an mpsc channel so the test can wake the driver loop
    // by sending a dummy block event after changing the backlog.
    struct ChannelSource {
        rx: mpsc::UnboundedReceiver<L2BlockEvent>,
    }

    #[async_trait]
    impl UnsafeBlockSource for ChannelSource {
        async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
            match self.rx.recv().await {
                Some(event) => Ok(event),
                // Channel closed: park until the driver is cancelled.
                None => std::future::pending().await,
            }
        }
    }

    Runner::start(Config::seeded(0), |ctx| async move {
        let (source_tx, source_rx) = mpsc::unbounded_channel();

        // Start with 2 MB backlog — above the default 1 MB threshold.
        let backlog = Arc::new(Mutex::new(2_000_000u64));
        let pipeline = DynamicPipeline { backlog: Arc::clone(&backlog) };

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let driver = BatchDriver::new(
            ctx.clone(),
            pipeline,
            ChannelSource { rx: source_rx },
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(throttle, Arc::new(throttle_client)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        // First iteration fires immediately on startup; give it time to complete.
        ctx.sleep(Duration::from_millis(30)).await;

        // Drop the backlog to zero, then wake the driver by delivering a dummy
        // block so the select! arm fires and the loop re-runs the throttle check.
        *backlog.lock().unwrap() = 0;
        source_tx.send(L2BlockEvent::Block(Box::default())).unwrap();

        ctx.sleep(Duration::from_millis(30)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert!(
            calls.len() >= 2,
            "expected at least 2 throttle calls (activate + deactivate), got {}",
            calls.len()
        );

        // First call must have reduced limits (throttle active, backlog was high).
        let (first_tx, first_block) = calls[0];
        assert!(
            first_block < 130_000,
            "first call should apply throttled block limit, got {first_block}"
        );
        assert!(first_tx < 20_000, "first call should apply throttled tx limit, got {first_tx}");

        // Last call must reset to upper limits (throttle deactivated).
        let (last_tx, last_block) = *calls.last().unwrap();
        assert_eq!(last_block, 130_000, "last call should reset block limit to upper bound");
        assert_eq!(last_tx, 20_000, "last call should reset tx limit to upper bound");
    });
}
