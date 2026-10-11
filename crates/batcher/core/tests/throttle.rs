//! Integration tests for DA throttle behaviour in [`BatchDriver`].

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use base_batcher_core::{
    DaLimits, DaThrottle, ThrottleConfig, ThrottleController, ThrottleStrategy,
    test_utils::{BlockStub, DriverFixture, ScriptedTxManager, TrackingPipeline},
};
use base_batcher_source::{
    L2BlockEvent,
    test_utils::{ChannelBlockSource, ChannelL1HeadSource},
};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// The limits applied when not throttling.
const fn upper_limits(config: &ThrottleConfig) -> DaLimits {
    DaLimits {
        max_tx_size: config.tx_size_upper_limit,
        max_block_size: config.block_size_upper_limit,
    }
}

/// The limits applied at full intensity.
const fn lower_limits(config: &ThrottleConfig) -> DaLimits {
    DaLimits {
        max_tx_size: config.tx_size_lower_limit,
        max_block_size: config.block_size_lower_limit,
    }
}

/// A backlog at the full threshold publishes the lower limits and forces blob submissions. Once
/// the backlog is gone, the upper limits are published again and blobs are no longer forced.
#[test]
fn test_throttle_transitions_from_active_to_inactive() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let (source, source_tx) = ChannelBlockSource::new();

        let config = ThrottleConfig::default();
        let pipeline = TrackingPipeline::new().with_da_backlog(config.full_threshold_bytes);
        let backlog = Arc::clone(&pipeline.da_backlog_bytes);
        let blob_override = Arc::clone(&pipeline.blob_override);

        let throttle =
            DaThrottle::new(ThrottleController::new(config.clone(), ThrottleStrategy::Linear));
        let limits = throttle.subscribe();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .source(source)
                .throttle(throttle)
                .build();
        let handle = ctx.spawn(driver.run());

        // The first iteration runs at startup, so give it time to complete.
        ctx.sleep(Duration::from_millis(30)).await;
        assert_eq!(*limits.borrow(), lower_limits(&config), "the full threshold is full intensity");
        assert!(blob_override.load(Ordering::SeqCst), "throttling forces blobs");

        // Drop the backlog to zero, then wake the driver by delivering a dummy
        // block so the select! arm fires and the loop re-runs the throttle check.
        backlog.store(0, Ordering::SeqCst);
        source_tx.send(L2BlockEvent::Block(Box::new(BlockStub::with_number(1)))).unwrap();

        ctx.sleep(Duration::from_millis(30)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        assert_eq!(*limits.borrow(), upper_limits(&config));
        assert!(!blob_override.load(Ordering::SeqCst), "blobs are no longer forced");
    });
}

/// A throttle controller set over the admin API publishes its limits for the current backlog.
#[test]
fn test_admin_set_throttle_publishes_the_new_limits() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let config = ThrottleConfig::default();
        let pipeline = TrackingPipeline::new().with_da_backlog(config.full_threshold_bytes);
        let throttle = DaThrottle::new(ThrottleController::disabled());
        let mut limits = throttle.subscribe();

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .throttle(throttle)
                .build();
        let handle = ctx.spawn(driver.run());

        handles.admin.set_throttle(ThrottleStrategy::Linear, config.clone()).await.unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        assert!(limits.has_changed().unwrap(), "the new limits are published");
        assert_eq!(*limits.borrow_and_update(), lower_limits(&config));

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// With blob forcing off, throttling publishes the lower limits but leaves the DA type alone.
#[test]
fn test_throttling_without_blob_forcing_keeps_the_da_type() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let config = ThrottleConfig::default();
        let pipeline = TrackingPipeline::new().with_da_backlog(config.full_threshold_bytes);
        let blob_override = Arc::clone(&pipeline.blob_override);
        let throttle =
            DaThrottle::new(ThrottleController::new(config.clone(), ThrottleStrategy::Linear));
        let limits = throttle.subscribe();

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .throttle(throttle)
                .force_blobs_when_throttling(false)
                .build();
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(10)).await;
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());

        assert_eq!(*limits.borrow(), lower_limits(&config));
        assert!(!blob_override.load(Ordering::SeqCst), "the DA type is left alone");
    });
}

/// Without a backlog the throttle starts on the upper limits and publishes nothing while the
/// backlog stays the same.
#[test]
fn test_upper_limits_are_published_once_while_the_backlog_is_unchanged() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new();
        let recorded = pipeline.recorded();
        let (l1_head_source, l1_head_tx) = ChannelL1HeadSource::new();

        let config = ThrottleConfig::default();
        let throttle =
            DaThrottle::new(ThrottleController::new(config.clone(), ThrottleStrategy::Linear));
        let limits = throttle.subscribe();
        assert_eq!(*limits.borrow(), upper_limits(&config));

        let (driver, _handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .l1_head_source(l1_head_source)
                .throttle(throttle)
                .build();
        let handle = ctx.spawn(driver.run());

        // Each L1 head wakes the loop for another iteration with the same backlog.
        for l1_head in 1..=3 {
            l1_head_tx.send(l1_head).unwrap();
        }
        ctx.sleep(Duration::from_millis(10)).await;
        assert_eq!(recorded.lock().unwrap().l1_heads(), [1, 2, 3]);

        // Check the publication while the driver runs, since a stopped driver drops the sender.
        assert!(!limits.has_changed().unwrap(), "unchanged limits are not published again");

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// A stopped batcher has dropped its backlog but posts nothing, so it keeps the last published
/// limits instead of lifting them. Starting again re-evaluates the backlog.
#[test]
fn test_stopped_batcher_keeps_the_last_limits() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let config = ThrottleConfig::default();
        let pipeline = TrackingPipeline::new().with_da_backlog(2 * config.threshold_bytes);
        let backlog = Arc::clone(&pipeline.da_backlog_bytes);
        let blob_override = Arc::clone(&pipeline.blob_override);
        let throttle =
            DaThrottle::new(ThrottleController::new(config.clone(), ThrottleStrategy::Linear));
        let limits = throttle.subscribe();

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                .throttle(throttle)
                .build();
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(10)).await;
        assert_eq!(*limits.borrow(), lower_limits(&config));

        // Stopping drops the buffered state, so the backlog reads as zero.
        backlog.store(0, Ordering::SeqCst);
        handles.admin.stop().await.unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        assert_eq!(*limits.borrow(), lower_limits(&config), "a stopped batcher keeps its limits");
        assert!(blob_override.load(Ordering::SeqCst), "a stopped batcher keeps forcing blobs");

        handles.admin.start().await.unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        assert_eq!(*limits.borrow(), upper_limits(&config), "starting re-evaluates the backlog");

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}
