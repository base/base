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

/// A backlog at twice the threshold pushes the lower limits and forces blob submissions. Once the
/// backlog is gone, the upper limits are pushed back and blobs are no longer forced.
#[test]
fn test_throttle_transitions_from_active_to_inactive() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let (source, source_tx) = ChannelBlockSource::new();

        let config = ThrottleConfig::default();
        let pipeline = TrackingPipeline::new().with_da_backlog(2 * config.threshold_bytes);
        let backlog = Arc::clone(&pipeline.da_backlog_bytes);
        let blob_override = Arc::clone(&pipeline.blob_override);

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

        // The first iteration runs at startup, so give it time to complete.
        ctx.sleep(Duration::from_millis(30)).await;
        assert!(blob_override.load(Ordering::SeqCst), "throttling forces blobs");

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
        assert!(!blob_override.load(Ordering::SeqCst), "blobs are no longer forced");
    });
}

/// Setting or resetting the throttle controller over the admin API pushes its limits again,
/// even when they did not change.
#[test]
fn test_admin_set_and_reset_push_the_limits_again() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let config = ThrottleConfig::default();
        let upper_limits = (config.tx_size_upper_limit, config.block_size_upper_limit);
        let throttle = ThrottleController::new(config.clone(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();
        let (driver, handles) = DriverFixture::new(
            ctx.clone(),
            TrackingPipeline::new(),
            ScriptedTxManager::confirming_at(1),
        )
        .throttle(DaThrottle::new(throttle, Arc::new(throttle_client)))
        .build();
        let handle = ctx.spawn(driver.run());

        handles.admin.set_throttle(ThrottleStrategy::Linear, config).await.unwrap();
        handles.admin.reset_throttle().await.unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        assert_eq!(*throttle_recorded.lock().unwrap(), [upper_limits, upper_limits, upper_limits]);
    });
}

/// With blob forcing off, throttling pushes the lower limits but leaves the DA type alone.
#[test]
fn test_throttling_without_blob_forcing_keeps_the_da_type() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let config = ThrottleConfig::default();
        let pipeline = TrackingPipeline::new().with_da_backlog(2 * config.threshold_bytes);
        let blob_override = Arc::clone(&pipeline.blob_override);
        let lower_limits = (config.tx_size_lower_limit, config.block_size_lower_limit);
        let throttle = ThrottleController::new(config, ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .throttle(DaThrottle::new(throttle, Arc::new(throttle_client)))
                .force_blobs_when_throttling(false)
                .build();
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(10)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        assert_eq!(*throttle_recorded.lock().unwrap(), [lower_limits]);
        assert!(!blob_override.load(Ordering::SeqCst), "the DA type is left alone");
    });
}

/// Without a backlog, the upper limits are pushed at startup, lifting any throttle left on the
/// block builder. A push the block builder refuses is made again on the next iteration, and
/// limits it accepted are not pushed again while they stay the same.
#[test]
fn test_upper_limits_are_pushed_at_startup_until_accepted() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        let (l1_head_source, l1_head_tx) = ChannelL1HeadSource::new();

        let config = ThrottleConfig::default();
        let upper_limits = (config.tx_size_upper_limit, config.block_size_upper_limit);
        let throttle = ThrottleController::new(config, ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .l1_head_source(l1_head_source)
                .throttle(DaThrottle::new(throttle, Arc::new(throttle_client.with_failures(1))))
                .build();
        let handle = ctx.spawn(driver.run());

        // Each L1 head wakes the loop for another iteration with the same backlog.
        for l1_head in 1..=3 {
            l1_head_tx.send(l1_head).unwrap();
        }
        ctx.sleep(Duration::from_millis(10)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        assert_eq!(recorded.lock().unwrap().l1_heads(), [1, 2, 3]);
        assert_eq!(*throttle_recorded.lock().unwrap(), [upper_limits, upper_limits]);
    });
}
