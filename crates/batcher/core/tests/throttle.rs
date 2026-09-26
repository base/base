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
use base_batcher_source::{
    L2BlockEvent,
    test_utils::{ChannelBlockSource, ChannelL1HeadSource},
};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// Without a backlog, the upper limits are pushed once at startup, lifting any throttle left on
/// the block builder, and not again on the later iterations that leave them unchanged.
#[test]
fn test_upper_limits_are_pushed_once_without_backlog() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let (l1_head_source, l1_head_tx) = ChannelL1HeadSource::new();

        let config = ThrottleConfig::default();
        let upper_limits = (config.tx_size_upper_limit, config.block_size_upper_limit);
        let throttle = ThrottleController::new(config, ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .l1_head_source(l1_head_source)
                .throttle(DaThrottle::new(throttle, Arc::new(throttle_client)))
                .build();
        let handle = ctx.spawn(driver.run());

        // Each L1 head wakes the loop for another iteration with the same backlog.
        for l1_head in 1..=3 {
            l1_head_tx.send(l1_head).unwrap();
        }
        ctx.sleep(Duration::from_millis(10)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        assert_eq!(*throttle_recorded.lock().unwrap(), [upper_limits]);
    });
}

/// Verifies that when the DA backlog transitions from above the threshold
/// (throttle active) to zero (throttle inactive), the driver makes exactly
/// two RPC calls: one with the lower limits and one resetting to the upper limits.
#[test]
fn test_throttle_transitions_from_active_to_inactive() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let (source, source_tx) = ChannelBlockSource::new();

        // Start with 2 MB backlog — above the default 1 MB threshold.
        let pipeline = TrackingPipeline::new().with_da_backlog(2_000_000);
        let backlog = Arc::clone(&pipeline.da_backlog_bytes);

        let config = ThrottleConfig::default();
        let lower_limits = (config.tx_size_lower_limit, config.block_size_lower_limit);
        let upper_limits = (config.tx_size_upper_limit, config.block_size_upper_limit);
        let throttle = ThrottleController::new(config, ThrottleStrategy::Linear);
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

        // Twice the threshold is full intensity, so the lower limits first, then the upper
        // limits once the backlog is gone.
        assert_eq!(*throttle_recorded.lock().unwrap(), [lower_limits, upper_limits]);
    });
}
