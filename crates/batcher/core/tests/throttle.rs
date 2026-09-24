//! Integration tests for DA throttle behaviour in [`BatchDriver`].

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use base_batcher_core::{
    DaThrottle, ThrottleConfig, ThrottleController, ThrottleStrategy,
    test_utils::{
        BlockStub, DriverFixture, ScriptedTxManager, TrackingPipeline, TrackingThrottleClient,
    },
};
use base_batcher_source::{L2BlockEvent, test_utils::ChannelBlockSource};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// When the DA backlog exceeds the threshold, the driver must call
/// `set_max_da_size` on the throttle client with reduced limits.
#[test]
fn test_throttle_client_called_on_high_backlog() {
    Runner::start(Config::seeded(0), |ctx| async move {
        // 2 MB backlog — above the default 1 MB threshold.
        let pipeline = TrackingPipeline::new().with_da_backlog(2_000_000);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .throttle(DaThrottle::new(throttle, Arc::new(throttle_client)))
                .build();
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
        let pipeline = TrackingPipeline::new().with_da_backlog(0);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .throttle(DaThrottle::new(throttle, Arc::new(throttle_client)))
                .build();
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
        let pipeline = TrackingPipeline::new().with_da_backlog(0);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .throttle(DaThrottle::new(throttle, Arc::new(throttle_client)))
                .build();
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
        // Backlog of 100 — above threshold of 1.
        let pipeline = TrackingPipeline::new().with_da_backlog(100);

        let config =
            ThrottleConfig { threshold_bytes: 1, max_intensity: 1.0, ..Default::default() };
        let throttle = ThrottleController::new(config, ThrottleStrategy::Step);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .throttle(DaThrottle::new(throttle, Arc::new(throttle_client)))
                .build();
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
    Runner::start(Config::seeded(0), |ctx| async move {
        let (source, source_tx) = ChannelBlockSource::new();

        // Start with 2 MB backlog — above the default 1 MB threshold.
        let pipeline = TrackingPipeline::new().with_da_backlog(2_000_000);
        let backlog = Arc::clone(&pipeline.da_backlog_bytes);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .throttle(DaThrottle::new(throttle, Arc::new(throttle_client)))
                .build();
        let handle = ctx.spawn(driver.run());

        // First iteration fires immediately on startup; give it time to complete.
        ctx.sleep(Duration::from_millis(30)).await;

        // Drop the backlog to zero, then wake the driver by delivering a dummy
        // block so the select! arm fires and the loop re-runs the throttle check.
        backlog.store(0, Ordering::SeqCst);
        source_tx.send(L2BlockEvent::Block(Box::new(BlockStub::with_number(1)))).unwrap();

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
