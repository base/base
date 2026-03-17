//! Integration tests for [`BatchDriver`] end-to-end behaviour.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use base_alloy_consensus::OpBlock;
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, DaThrottle, NoopThrottleClient, ThrottleConfig,
    ThrottleController, ThrottleStrategy,
    test_utils::{
        DriverFixture, ImmediateConfirmTxManager, ImmediateFailTxManager, NeverConfirmTxManager,
        OneBlockSource, OneReorgPipeline, PendingL1HeadSource, PendingSource, Recorded,
        ReorgPipeline, SubmissionStub, TrackingPipeline, TrackingThrottleClient,
    },
};
use base_batcher_encoder::{
    BatchPipeline, BatchSubmission, ReorgError, StepError, StepResult, SubmissionId,
};
use base_batcher_source::{
    ChannelBlockSource, ChannelL1HeadSource, L1HeadEvent, L2BlockEvent, SourceError,
    UnsafeBlockSource, test_utils::InMemoryBlockSource,
};
use base_protocol::{BlockInfo, L2BlockInfo};
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};
use tokio::sync::mpsc;

// ---- Reorg tests ----

/// When `add_block` returns `ReorgError`, the driver must reset the pipeline and
/// then re-add the triggering block so it is not permanently lost. The block
/// queue in the encoder is empty after reset, so the parent-hash check is
/// skipped and the re-add always succeeds.
#[test]
fn test_reorg_block_is_readded_after_reset() {
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
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(*resets.lock().unwrap(), 1, "pipeline must be reset on reorg");
        assert_eq!(
            *blocks_accepted.lock().unwrap(),
            1,
            "the triggering block must be re-added after reset"
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

// ---- Throttle integration tests ----

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
        fn add_block(&mut self, _: OpBlock) -> Result<(), (ReorgError, Box<OpBlock>)> {
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

// ---- L1 head source + safe head watch receiver tests ----

/// When the L1 head source delivers a new head, the driver must call
/// `advance_l1_head` on the pipeline with the new value.
#[test]
fn test_l1_head_source_advances_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (l1_source, l1_tx) = ChannelL1HeadSource::new();

        let driver = BatchDriver::new(
            ctx.clone(),
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
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

/// When a safe head watch receiver fires, the driver must call
/// `prune_safe` on the pipeline with the new value.
#[test]
fn test_safe_head_watch_prunes_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (safe_tx, safe_rx) = tokio::sync::watch::channel(0u64);

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_safe_head_rx(safe_rx);
        let handle = ctx.spawn(driver.run());

        // Send a new safe head.
        safe_tx.send(100).unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let r = recorded.lock().unwrap();
        assert!(
            r.safe_numbers.contains(&100),
            "prune_safe must be called with the watch value, got {:?}",
            r.safe_numbers
        );
    });
}

/// When the safe head sender is dropped while the driver is running, the watch
/// arm must disable itself rather than spinning. The driver continues running
/// and remains cancellable after the sender disappears.
#[test]
fn test_safe_head_sender_drop_does_not_busyloop() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (safe_tx, safe_rx) = tokio::sync::watch::channel(0u64);

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_safe_head_rx(safe_rx);
        let handle = ctx.spawn(driver.run());

        // Send one value, then drop the sender while the driver is still running.
        safe_tx.send(50).unwrap();
        ctx.sleep(Duration::from_millis(20)).await;
        drop(safe_tx);

        // Give the driver time to process the drop. If the arm busy-loops,
        // prune_safe would be called many additional times here.
        ctx.sleep(Duration::from_millis(50)).await;
        let prune_count_after_drop = recorded.lock().unwrap().safe_numbers.len();

        // Cancel and wait — driver must exit cleanly, not hang.
        ctx.cancel();
        assert!(handle.await.unwrap().is_ok(), "driver must exit cleanly after sender drop");

        let r = recorded.lock().unwrap();
        assert!(
            r.safe_numbers.contains(&50),
            "prune_safe must have been called with the sent value"
        );
        // After the sender drops, prune_safe must not be called again.
        assert_eq!(
            r.safe_numbers.len(),
            prune_count_after_drop,
            "prune_safe must not be called after sender drop (arm must be disabled)"
        );
    });
}

/// Without a safe head receiver, confirmation-based L1 head advancement must
/// still work normally. The driver uses `PendingL1HeadSource` (parks forever)
/// so only submission confirmations drive `advance_l1_head`.
#[test]
fn test_no_safe_head_receiver_driver_runs_normally() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::stub());

        // No .with_safe_head_rx() — safe_head remains None.
        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 7 });
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let r = recorded.lock().unwrap();
        assert_eq!(r.l1_heads, vec![7], "confirmation-based advance_l1_head must still work");
        assert!(r.safe_numbers.is_empty(), "prune_safe must not be called without a receiver");
    });
}

// ---- Driver lifecycle tests ----

/// When the block source returns `SourceError::Exhausted`, the driver must
/// treat it as a graceful shutdown signal: close the current channel,
/// drain in-flight submissions within the timeout, then exit cleanly.
#[test]
fn test_source_exhaustion_shuts_down_driver_gracefully() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let driver = BatchDriver::new(
            ctx.clone(),
            pipeline,
            InMemoryBlockSource::new(), // empty → Exhausted immediately
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        );

        let handle = ctx.spawn(driver.run());
        ctx.sleep(Duration::from_millis(50)).await;

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "driver must exit cleanly when source exhausts");
        assert_eq!(
            recorded.lock().unwrap().force_close_count,
            1,
            "force_close_channel must be called once on source exhaustion shutdown"
        );
    });
}

/// When the source delivers `L2BlockEvent::Flush`, the driver must call
/// `force_close_channel` immediately. On subsequent shutdown it is called once
/// more, giving a total of two calls.
#[test]
fn test_flush_event_calls_force_close_channel() {
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
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            PendingL1HeadSource,
        );
        let handle = ctx.spawn(driver.run());

        source_tx.send(L2BlockEvent::Flush).unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        // Flush arm: +1; Shutdown arm: +1 → total 2
        assert_eq!(
            recorded.lock().unwrap().force_close_count,
            2,
            "force_close_channel must be called for Flush and again on shutdown"
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

/// When the L1 head source is exhausted, the driver must disable that arm and
/// continue running — it must not shut down. The L1 head delivered before
/// exhaustion must be processed normally.
#[test]
fn test_l1_source_exhausted_disables_arm_driver_continues() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (l1_source, l1_tx) = ChannelL1HeadSource::new();

        let driver = BatchDriver::new(
            ctx.clone(),
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
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

/// When cancellation fires while a submission is in-flight with a
/// `NeverConfirmTxManager`, the drain timeout must fire and the driver must
/// exit cleanly. This verifies the `runtime.sleep(drain_timeout)` fix.
#[test]
fn test_drain_timeout_exits_with_in_flight_submissions() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::stub());

        let driver = DriverFixture::build(ctx.clone(), pipeline, NeverConfirmTxManager);
        let handle = ctx.spawn(driver.run());

        ctx.sleep(Duration::from_millis(20)).await;
        ctx.cancel();

        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "driver must exit after drain timeout even with in-flight submissions"
        );
        let r = recorded.lock().unwrap();
        assert_eq!(r.dequeued, vec![SubmissionId(0)], "submission must have been dequeued");
        assert_eq!(r.force_close_count, 1, "force_close_channel must be called on shutdown");
    });
}
