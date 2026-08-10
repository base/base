//! Integration tests for L1 and safe L2 head handling in [`BatchDriver`].
//!
//! [`TrackingChannelSource`] is hand-rolled because this integration test needs to coordinate
//! live block delivery with safe-head updates while retaining a shared log of reset requests.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_consensus::Header;
use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use base_batcher_core::{
    BatchDriver, BatchDriverConfig, BatchDriverError, DaThrottle, NoopThrottleClient,
    ThrottleController,
    test_utils::{
        DriverFixture, ImmediateConfirmTxManager, PendingL1HeadSource, PendingSource, Recorded,
        SubmissionStub, TrackingPipeline, TrackingSource,
    },
};
use base_batcher_source::{
    ChannelL1HeadSource, L1HeadEvent, L2BlockEvent, SourceError, UnsafeBlockSource,
};
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use base_runtime::{
    Cancellation, Clock, Spawner,
    deterministic::{Config, Runner},
};
use tokio::sync::mpsc;

#[derive(Debug)]
struct TrackingChannelSource {
    rx: mpsc::UnboundedReceiver<L2BlockEvent>,
    catchup_heads: Arc<Mutex<Vec<BlockInfo>>>,
}

impl TrackingChannelSource {
    fn new() -> (Self, mpsc::UnboundedSender<L2BlockEvent>, Arc<Mutex<Vec<BlockInfo>>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let catchup_heads = Arc::new(Mutex::new(Vec::new()));
        (Self { rx, catchup_heads: Arc::clone(&catchup_heads) }, tx, catchup_heads)
    }
}

#[async_trait]
impl UnsafeBlockSource for TrackingChannelSource {
    async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
        self.rx.recv().await.ok_or(SourceError::Exhausted)
    }

    fn reset_catchup(&mut self, safe_head: BlockInfo) {
        self.catchup_heads.lock().unwrap().push(safe_head);
    }
}

fn safe_head(number: u64) -> BlockInfo {
    BlockInfo { hash: B256::with_last_byte(number as u8), number, ..Default::default() }
}

fn child_block(parent: BlockInfo, number: u64) -> BaseBlock {
    BaseBlock {
        header: Header {
            number,
            parent_hash: parent.hash,
            extra_data: vec![number as u8].into(),
            ..Default::default()
        },
        body: Default::default(),
    }
}

/// When the L1 head source delivers a new head, the driver must call
/// `advance_l1_head` on the pipeline with the new value.
#[test]
fn test_l1_head_source_advances_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (l1_source, l1_tx) = ChannelL1HeadSource::new();

        let driver = BatchDriver::new_without_safe_head(
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

        let driver = BatchDriver::new_without_safe_head(
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
fn test_safe_head_regression_and_chain_mismatch_reset_pipeline() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (safe_tx, safe_rx) = mpsc::channel(1);
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
            (PendingL1HeadSource, safe_head(10), safe_rx),
        );
        let handle = ctx.spawn(driver.run());

        let regressed = safe_head(5);
        let replacement =
            BlockInfo { hash: B256::repeat_byte(0xff), number: 5, ..Default::default() };
        safe_tx.send(regressed).await.unwrap();
        safe_tx.send(replacement).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.resets, 2);
        assert!(recorded.safe_numbers.is_empty());
        assert_eq!(*catchup_heads.lock().unwrap(), vec![regressed, replacement]);
    });
}

#[test]
fn test_forward_safe_head_replacement_reanchors_to_new_chain() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (safe_tx, safe_rx) = mpsc::channel(1);
        let (source, block_tx, catchup_heads) = TrackingChannelSource::new();
        let initial_safe = safe_head(80);

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
            (PendingL1HeadSource, initial_safe, safe_rx),
        );
        let handle = ctx.spawn(driver.run());

        let mut parent = initial_safe;
        let mut canonical_safe = initial_safe;
        for number in 81..=100 {
            let block = child_block(parent, number);
            parent = BlockInfo::from(&block);
            if number == 90 {
                canonical_safe = parent;
            }
            block_tx.send(L2BlockEvent::Block(Box::new(block))).unwrap();
        }
        ctx.sleep(Duration::from_millis(50)).await;

        safe_tx.send(canonical_safe).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;

        let replacement =
            BlockInfo { hash: B256::repeat_byte(0xff), number: 95, ..Default::default() };
        safe_tx.send(replacement).await.unwrap();
        ctx.sleep(Duration::from_millis(50)).await;
        ctx.cancel();

        assert!(handle.await.unwrap().is_ok());
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.safe_numbers, vec![90]);
        assert_eq!(recorded.resets, 1);
        assert_eq!(*catchup_heads.lock().unwrap(), vec![replacement]);
    });
}

#[test]
fn test_queued_safe_head_preempts_submission() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::stub());
        let (safe_tx, safe_rx) = mpsc::channel(1);
        safe_tx.send(safe_head(5)).await.unwrap();

        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_safe_head_rx(safe_head(10), safe_rx);
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
fn test_safe_head_sender_drop_is_fatal() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        let (safe_tx, safe_rx) = mpsc::channel(1);
        let driver = DriverFixture::build(ctx, pipeline, ImmediateConfirmTxManager { l1_block: 1 })
            .with_safe_head_rx(safe_head(0), safe_rx);
        drop(safe_tx);

        assert!(matches!(driver.run().await, Err(BatchDriverError::SafeHeadSourceClosed)));
    });
}

#[test]
fn test_safe_head_sender_drop_during_shutdown_is_clean() {
    Runner::start(Config::seeded(0), |ctx| async move {
        let pipeline = TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default())));
        let (safe_tx, safe_rx) = mpsc::channel(1);
        let driver =
            DriverFixture::build(ctx.clone(), pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .with_safe_head_rx(safe_head(0), safe_rx);

        ctx.cancel();
        drop(safe_tx);
        assert!(driver.run().await.is_ok());
    });
}
