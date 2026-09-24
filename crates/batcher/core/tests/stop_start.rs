//! Integration tests for the stop, start and flush admin commands in [`BatchDriver`].

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use base_batcher_core::{
    AdminError,
    test_utils::{
        BlockStub, DriverFixture, ImmediateConfirmTxManager, ManualConfirmTxManager, Recorded,
        SubmissionStub, TrackingPipeline, TrackingSource,
    },
};
use base_batcher_encoder::{
    BatchPipeline, BatchSubmission, DerivationReconciliation, ReorgError, StepError, StepResult,
    SubmissionId,
};
use base_batcher_source::{L2BlockEvent, test_utils::ChannelBlockSource};
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};

/// `AdminCommand::Stop` must immediately reset the pipeline. Stopping a batcher
/// that is already stopped succeeds without resetting it again.
#[test]
fn test_stop_resets_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .build();
        let handle = ctx.spawn(driver.run());

        handles.admin.stop().await.unwrap();
        handles.admin.stop().await.unwrap();
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
        let safe_head = BlockInfo { number: 42, ..Default::default() };

        let (driver, handles) = DriverFixture::new(
            ctx.clone(),
            TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default()))),
            ImmediateConfirmTxManager { l1_block: 1 },
        )
        .source(source)
        .safe_head(safe_head)
        .build();
        let handle = ctx.spawn(driver.run());

        // Stop then start with safe_head = 42; the source will poll 43 next.
        handles.admin.stop().await.unwrap();
        handles.admin.start().await.unwrap();
        handles.admin.start().await.unwrap();
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(
            *catchup_args.lock().unwrap(),
            vec![safe_head],
            "source must be reanchored at the safe head on start"
        );
    });
}

/// While stopped, `Block` source events must be dropped; the pipeline must not
/// receive any blocks. Once started again, new blocks reach the pipeline.
#[test]
fn test_stopped_drops_block_events() {
    Runner::start(Config::seeded(0), |ctx| async move {
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

        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .source(source)
                .build();
        let handle = ctx.spawn(driver.run());

        // Stop, then send a block: it must be dropped.
        handles.admin.stop().await.unwrap();
        source_tx.send(L2BlockEvent::Block(Box::new(BlockStub::with_number(1)))).unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        assert_eq!(
            *add_block_calls.lock().unwrap(),
            0,
            "add_block must not be called while stopped"
        );

        // Start, then send the next block: it must reach the pipeline.
        handles.admin.start().await.unwrap();
        source_tx.send(L2BlockEvent::Block(Box::new(BlockStub::with_number(2)))).unwrap();
        ctx.sleep(Duration::from_millis(10)).await;
        assert_eq!(*add_block_calls.lock().unwrap(), 1, "add_block must be called once started");

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}

/// A stop does not wait for the submissions in flight. They keep settling while the
/// batcher is stopped, and the status reports how many remain.
#[test]
fn test_stop_leaves_in_flight_submissions_to_settle() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let mut pipeline = TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default())));
        pipeline.submissions.push_back(SubmissionStub::stub());
        let tx_manager = ManualConfirmTxManager::default();
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, tx_manager.clone()).build();
        let handle = ctx.spawn(driver.run());

        // Stop while the stub is in flight.
        handles.admin.stop().await.unwrap();
        let status = handles.admin.get_status().await.unwrap();
        assert!(status.stopped);
        assert_eq!(status.in_flight, 1);

        // Confirm the submission and let the driver process the receipt.
        tx_manager.confirm_next(1);
        ctx.sleep(Duration::from_millis(1)).await;
        assert_eq!(handles.admin.get_status().await.unwrap().in_flight, 0);

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
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .build();
        let handle = ctx.spawn(driver.run());

        handles.admin.flush().await.unwrap();

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
        let (driver, handles) =
            DriverFixture::new(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .build();
        let handle = ctx.spawn(driver.run());

        handles.admin.stop().await.unwrap();
        let result = handles.admin.flush().await;

        assert!(matches!(result, Err(AdminError::Stopped)));
        assert_eq!(recorded.lock().unwrap().flush_count, 0);

        ctx.cancel();
        assert!(handle.await.unwrap().is_ok());
    });
}
