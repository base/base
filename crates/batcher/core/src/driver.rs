//! The async batch driver that orchestrates encoding, block sourcing, and L1 submission.

use std::time::Duration;

use base_alloy_consensus::OpBlock;
use base_batcher_encoder::{BatchPipeline, StepResult, SubmissionId};
use base_batcher_source::{
    L1HeadEvent, L1HeadSource, L2BlockEvent, SourceError, UnsafeBlockSource,
};
use base_protocol::L2BlockInfo;
use base_tx_manager::TxManager;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    BatchDriverError, DaThrottle, SubmissionQueue, ThrottleClient, ThrottleController, TxOutcome,
};

/// Configuration for a [`BatchDriver`] instance.
#[derive(Debug, Clone)]
pub struct BatchDriverConfig {
    /// The batcher inbox address on L1.
    pub inbox: alloy_primitives::Address,
    /// Maximum number of in-flight transactions before back-pressure kicks in.
    pub max_pending_transactions: usize,
    /// Maximum time to wait for in-flight transactions to settle when draining
    /// on cancellation or source exhaustion. Submissions that have not
    /// confirmed within this window are abandoned.
    pub drain_timeout: Duration,
}

/// Async orchestration loop for the batcher.
///
/// Combines a [`BatchPipeline`] (encoding), an [`UnsafeBlockSource`] (L2 block delivery),
/// an [`L1HeadSource`] (L1 chain head tracking), and a [`TxManager`] (L1 submission)
/// into a single `tokio::select!` task.
///
/// Uses [`SubmissionQueue`] for concurrent receipt tracking and semaphore backpressure,
/// and [`DaThrottle`] for DA backlog throttle management.
#[derive(Debug)]
pub struct BatchDriver<P, S, TM, TC, L>
where
    P: BatchPipeline,
    S: UnsafeBlockSource,
    TM: TxManager,
    TC: ThrottleClient,
    L: L1HeadSource,
{
    /// The encoding pipeline.
    pipeline: P,
    /// The L2 block source.
    source: S,
    /// Submission lifecycle manager (tx manager, in-flight tracking, semaphore, txpool state).
    submissions: SubmissionQueue<TM>,
    /// DA backlog throttle (controller, client, dedup cache).
    throttle: DaThrottle<TC>,
    /// L1 head source for chain head advancement.
    ///
    /// Set to `None` after the source returns [`SourceError::Exhausted`] or
    /// [`SourceError::Closed`], causing the driver to park that select arm forever.
    l1_head_source: Option<L>,
    /// Optional external L2 safe head feed for pruning confirmed blocks.
    safe_head_rx: Option<tokio::sync::watch::Receiver<u64>>,
    /// Maximum wall-clock time to wait for in-flight submissions to settle
    /// when draining on cancellation or source exhaustion.
    drain_timeout: Duration,
}

/// Maximum number of encoding steps to run synchronously per outer loop iteration
/// before yielding to the tokio executor. Prevents a large block backlog from
/// starving receipt processing and cancellation checks.
const STEP_BUDGET: usize = 128;

/// Events the driver can receive from external sources during the I/O phase.
enum DriverEvent {
    /// Cancellation token fired, or L2 source signalled exhausted.
    Shutdown,
    /// New L2 unsafe block from the source.
    Block(Box<OpBlock>),
    /// Source requested a force-flush of the current channel.
    Flush,
    /// L2 reorganisation; new safe head provided.
    Reorg(L2BlockInfo),
    /// An in-flight L1 transaction settled.
    Receipt(SubmissionId, TxOutcome),
    /// L1 chain head advanced.
    L1Head(u64),
    /// Safe L2 head advanced (from watch channel).
    SafeHead(u64),
    /// L1 head source permanently closed (Exhausted or Closed error).
    L1SourceClosed,
}

impl<P, S, TM, TC, L> BatchDriver<P, S, TM, TC, L>
where
    P: BatchPipeline,
    S: UnsafeBlockSource,
    TM: TxManager,
    TC: ThrottleClient,
    L: L1HeadSource,
{
    /// Create a new [`BatchDriver`].
    pub fn new(
        pipeline: P,
        source: S,
        tx_manager: TM,
        config: BatchDriverConfig,
        throttle: ThrottleController,
        throttle_client: TC,
        l1_head_source: L,
    ) -> Self {
        Self {
            pipeline,
            source,
            submissions: SubmissionQueue::new(
                tx_manager,
                config.inbox,
                config.max_pending_transactions,
            ),
            throttle: DaThrottle::new(throttle, throttle_client),
            l1_head_source: Some(l1_head_source),
            safe_head_rx: None,
            drain_timeout: config.drain_timeout,
        }
    }

    /// Attach an external L2 safe head watch channel.
    ///
    /// When the receiver fires, the pipeline's [`prune_safe`](BatchPipeline::prune_safe)
    /// is called with the new safe L2 block number, allowing the encoder to
    /// free blocks that are confirmed safe on L2.
    pub fn with_safe_head_rx(mut self, rx: tokio::sync::watch::Receiver<u64>) -> Self {
        self.safe_head_rx = Some(rx);
        self
    }

    /// Run the batch driver loop.
    ///
    /// Each iteration has two phases:
    /// 1. **CPU phase**: drain encoding, apply throttle, recover txpool, submit pending frames.
    /// 2. **I/O phase**: block on `tokio::select!` until one external event fires.
    ///
    /// When draining (after cancellation or source exhaustion), the I/O phase is
    /// replaced by a bounded drain of all in-flight receipts.
    pub async fn run(mut self, token: CancellationToken) -> Result<(), BatchDriverError> {
        let mut draining = false;
        loop {
            self.drain_encoding()?;
            self.throttle.apply(self.pipeline.da_backlog_bytes()).await;
            self.submissions.recover_txpool().await;
            self.submissions.submit_pending(&mut self.pipeline).await;

            if draining {
                self.submissions.drain(&mut self.pipeline, self.drain_timeout).await;
                return Ok(());
            }

            match self.next_event(&token).await? {
                DriverEvent::Shutdown => {
                    info!(
                        in_flight = %self.submissions.in_flight_count(),
                        "batcher shutting down, draining in-flight submissions"
                    );
                    self.pipeline.force_close_channel();
                    draining = true;
                }
                DriverEvent::Block(b) => self.on_block(b),
                DriverEvent::Flush => {
                    self.pipeline.force_close_channel();
                    debug!("flush signal received, force-closed channel");
                }
                DriverEvent::Reorg(head) => {
                    warn!(head = %head.block_info.number, "L2 reorg detected, resetting pipeline");
                    self.submissions.discard();
                    self.pipeline.reset();
                }
                DriverEvent::Receipt(id, o) => {
                    self.submissions.handle_outcome(&mut self.pipeline, id, o);
                }
                DriverEvent::L1Head(n) => {
                    self.pipeline.advance_l1_head(n);
                    debug!(l1_head = %n, "L1 head advanced via source");
                }
                DriverEvent::SafeHead(n) => {
                    self.pipeline.prune_safe(n);
                    debug!(safe_l2_number = %n, "pruned safe blocks via watch");
                }
                DriverEvent::L1SourceClosed => {
                    debug!("L1 head source closed, disabling arm");
                    self.l1_head_source = None;
                }
            }
        }
    }

    /// Drain encoding steps synchronously up to `STEP_BUDGET`.
    ///
    /// Returns `Err` on a fatal [`StepError`](base_batcher_encoder::StepError).
    fn drain_encoding(&mut self) -> Result<(), BatchDriverError> {
        let mut budget = STEP_BUDGET;
        loop {
            match self.pipeline.step() {
                Ok(StepResult::Idle) => break,
                Ok(StepResult::BlockEncoded | StepResult::ChannelClosed) => {
                    budget -= 1;
                    if budget == 0 {
                        break;
                    }
                }
                Err(e) => {
                    error!(error = %e, "fatal encoding step error, batcher halting");
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    /// Ingest a new L2 block into the pipeline.
    ///
    /// If the pipeline signals a reorg via `add_block`, discards in-flight
    /// submissions, resets the pipeline, and re-adds the triggering block so
    /// it is not permanently lost.
    fn on_block(&mut self, block: Box<OpBlock>) {
        let number = block.header.number;
        match self.pipeline.add_block(*block) {
            Ok(()) => {
                debug!(block = %number, "added unsafe block to pipeline");
            }
            Err((e, block)) => {
                warn!(
                    block = %number,
                    error = %e,
                    "reorg detected during block ingestion, resetting pipeline"
                );
                self.submissions.discard();
                self.pipeline.reset();
                // Re-add the triggering block. After reset the block queue is
                // empty, so the parent-hash check is skipped and the block is
                // always accepted. This prevents the block from being silently
                // lost when the source won't re-deliver it (e.g. HybridBlockSource
                // deduplication).
                let _ = self.pipeline.add_block(*block);
            }
        }
    }

    /// Block on the next external event using a biased `tokio::select!`.
    ///
    /// Non-fatal L1 head source errors loop internally to avoid polluting the
    /// return type with a no-op variant.
    async fn next_event(
        &mut self,
        token: &CancellationToken,
    ) -> Result<DriverEvent, BatchDriverError> {
        loop {
            let event = tokio::select! {
                biased;

                _ = token.cancelled() => DriverEvent::Shutdown,

                event = self.source.next() => match event {
                    Ok(L2BlockEvent::Block(block)) => DriverEvent::Block(block),
                    Ok(L2BlockEvent::Flush) => DriverEvent::Flush,
                    Ok(L2BlockEvent::Reorg { new_safe_head }) => DriverEvent::Reorg(new_safe_head),
                    Err(SourceError::Exhausted) => DriverEvent::Shutdown,
                    Err(e) => return Err(e.into()),
                },

                Some((id, outcome)) = self.submissions.next_settled() => {
                    DriverEvent::Receipt(id, outcome)
                }

                l1_event = async {
                    if let Some(ref mut src) = self.l1_head_source {
                        src.next().await
                    } else {
                        std::future::pending::<Result<L1HeadEvent, SourceError>>().await
                    }
                } => match l1_event {
                    Ok(L1HeadEvent::NewHead(n)) => DriverEvent::L1Head(n),
                    Err(SourceError::Exhausted | SourceError::Closed) => DriverEvent::L1SourceClosed,
                    Err(e) => {
                        warn!(error = %e, "L1 head source error");
                        continue;
                    }
                },

                _ = async {
                    if let Some(ref mut rx) = self.safe_head_rx {
                        rx.changed().await.ok();
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    if let Some(rx) = &mut self.safe_head_rx {
                        if rx.has_changed().is_err() {
                            // Sender dropped; disable this arm permanently.
                            self.safe_head_rx = None;
                            continue;
                        }
                        let n = *rx.borrow();
                        DriverEvent::SafeHead(n)
                    } else {
                        continue;
                    }
                }
            };
            return Ok(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fmt,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, B256, Bloom};
    use alloy_rpc_types_eth::TransactionReceipt;
    use async_trait::async_trait;
    use base_alloy_consensus::OpBlock;
    use base_batcher_encoder::{
        BatchPipeline, BatchSubmission, ReorgError, StepError, StepResult, SubmissionId,
    };
    use base_batcher_source::{
        ChannelL1HeadSource, L1HeadEvent, L1HeadSource, L2BlockEvent, SourceError,
        UnsafeBlockSource,
    };
    use base_protocol::{ChannelId, Frame};
    use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError};
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use super::{BatchDriver, BatchDriverConfig};
    use crate::{
        NoopThrottleClient, ThrottleConfig, ThrottleController, ThrottleStrategy,
        test_utils::TrackingThrottleClient,
    };

    // ---- Shared recording state ----

    #[derive(Debug, Default)]
    struct Recorded {
        l1_heads: Vec<u64>,
        requeued: Vec<SubmissionId>,
        /// Submission IDs in the order they were dequeued via `next_submission()`.
        dequeued: Vec<SubmissionId>,
        /// Number of times `reset()` was called.
        resets: usize,
        /// Safe L2 block numbers passed to `prune_safe`.
        safe_numbers: Vec<u64>,
    }

    // ---- Pipeline that records advance_l1_head calls via shared state ----

    #[derive(Debug)]
    struct TrackingPipeline {
        recorded: Arc<Mutex<Recorded>>,
        submissions: std::collections::VecDeque<BatchSubmission>,
        /// Value returned by `da_backlog_bytes()`. Default: 0.
        da_backlog_bytes_value: u64,
    }

    impl TrackingPipeline {
        fn new(recorded: Arc<Mutex<Recorded>>) -> Self {
            Self { recorded, submissions: Default::default(), da_backlog_bytes_value: 0 }
        }

        fn with_da_backlog(mut self, value: u64) -> Self {
            self.da_backlog_bytes_value = value;
            self
        }
    }

    impl BatchPipeline for TrackingPipeline {
        fn add_block(&mut self, _: OpBlock) -> Result<(), (ReorgError, Box<OpBlock>)> {
            Ok(())
        }
        fn step(&mut self) -> Result<StepResult, StepError> {
            Ok(StepResult::Idle)
        }
        fn next_submission(&mut self) -> Option<BatchSubmission> {
            let sub = self.submissions.pop_front()?;
            self.recorded.lock().unwrap().dequeued.push(sub.id);
            Some(sub)
        }
        fn confirm(&mut self, _: SubmissionId, _: u64) {}
        fn requeue(&mut self, id: SubmissionId) {
            self.recorded.lock().unwrap().requeued.push(id);
        }
        fn force_close_channel(&mut self) {}
        fn advance_l1_head(&mut self, l1_block: u64) {
            self.recorded.lock().unwrap().l1_heads.push(l1_block);
        }
        fn prune_safe(&mut self, safe_l2_number: u64) {
            self.recorded.lock().unwrap().safe_numbers.push(safe_l2_number);
        }
        fn reset(&mut self) {
            self.recorded.lock().unwrap().resets += 1;
        }
        fn da_backlog_bytes(&self) -> u64 {
            self.da_backlog_bytes_value
        }
    }

    // ---- Pipeline that always returns ReorgError from add_block ----

    #[derive(Debug)]
    struct ReorgPipeline {
        recorded: Arc<Mutex<Recorded>>,
    }

    impl ReorgPipeline {
        fn new(recorded: Arc<Mutex<Recorded>>) -> Self {
            Self { recorded }
        }
    }

    impl BatchPipeline for ReorgPipeline {
        fn add_block(&mut self, block: OpBlock) -> Result<(), (ReorgError, Box<OpBlock>)> {
            Err((
                ReorgError::ParentMismatch { expected: B256::ZERO, got: B256::with_last_byte(1) },
                Box::new(block),
            ))
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
        fn reset(&mut self) {
            self.recorded.lock().unwrap().resets += 1;
        }
        fn da_backlog_bytes(&self) -> u64 {
            0
        }
    }

    // ---- Source that delivers one block then parks forever ----

    #[derive(Debug)]
    struct OneBlockSource {
        delivered: bool,
    }

    impl OneBlockSource {
        fn new() -> Self {
            Self { delivered: false }
        }
    }

    #[async_trait]
    impl UnsafeBlockSource for OneBlockSource {
        async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
            if !self.delivered {
                self.delivered = true;
                Ok(L2BlockEvent::Block(Box::default()))
            } else {
                std::future::pending().await
            }
        }
    }

    // ---- Source that parks the arm so the submission arm can fire ----

    #[derive(Debug)]
    struct PendingSource;

    #[async_trait]
    impl UnsafeBlockSource for PendingSource {
        async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
            std::future::pending().await
        }
    }

    // ---- L1 head source that parks forever (default for tests not testing L1 head) ----

    #[derive(Debug)]
    struct PendingL1HeadSource;

    #[async_trait]
    impl L1HeadSource for PendingL1HeadSource {
        async fn next(&mut self) -> Result<L1HeadEvent, SourceError> {
            std::future::pending().await
        }
    }

    // ---- TxManager helpers ----

    fn stub_receipt(block_number: u64) -> TransactionReceipt {
        let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::ZERO,
        });
        TransactionReceipt {
            inner,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(B256::ZERO),
            block_number: Some(block_number),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        }
    }

    /// Immediately confirms every submission at the given L1 block number.
    #[derive(Debug)]
    struct ImmediateConfirmTxManager {
        l1_block: u64,
    }

    impl TxManager for ImmediateConfirmTxManager {
        async fn send(&self, _: TxCandidate) -> SendResponse {
            unreachable!()
        }

        fn send_async(
            &self,
            _: TxCandidate,
        ) -> impl std::future::Future<Output = SendHandle> + Send {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Ok(stub_receipt(self.l1_block)));
            std::future::ready(SendHandle::new(rx))
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    /// Immediately fails every submission.
    #[derive(Debug)]
    struct ImmediateFailTxManager;

    impl TxManager for ImmediateFailTxManager {
        async fn send(&self, _: TxCandidate) -> SendResponse {
            unreachable!()
        }

        fn send_async(
            &self,
            _: TxCandidate,
        ) -> impl std::future::Future<Output = SendHandle> + Send {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Err(TxManagerError::ChannelClosed));
            std::future::ready(SendHandle::new(rx))
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    /// Never confirms any submission — the in-flight future parks forever.
    ///
    /// This is used to test semaphore backpressure: with this manager, permits
    /// are consumed but never released, so `try_acquire_owned` will fail once
    /// the limit is reached and no further submissions will be dequeued.
    struct NeverConfirmTxManager;

    impl fmt::Debug for NeverConfirmTxManager {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("NeverConfirmTxManager")
        }
    }

    impl TxManager for NeverConfirmTxManager {
        async fn send(&self, _: TxCandidate) -> SendResponse {
            unreachable!()
        }

        fn send_async(
            &self,
            _: TxCandidate,
        ) -> impl std::future::Future<Output = SendHandle> + Send {
            let (tx, rx) = oneshot::channel();
            // Spawn a task that parks forever, keeping `tx` alive so `rx`
            // never resolves. The task is cancelled when the test runtime
            // drops at the end of the test.
            tokio::spawn(async move {
                std::future::pending::<()>().await;
                drop(tx);
            });
            std::future::ready(SendHandle::new(rx))
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    /// Minimal [`BatchSubmission`] whose single empty frame blob-encodes cleanly.
    fn make_submission() -> BatchSubmission {
        make_submission_with_id(0)
    }

    fn make_submission_with_id(id: u64) -> BatchSubmission {
        BatchSubmission {
            id: SubmissionId(id),
            channel_id: ChannelId::default(),
            da_type: base_batcher_encoder::DaType::Blob,
            frames: vec![Arc::new(Frame::default())],
        }
    }

    fn noop_throttle() -> ThrottleController {
        ThrottleController::new(
            ThrottleConfig { threshold_bytes: 0, max_intensity: 0.0, ..Default::default() },
            ThrottleStrategy::Off,
        )
    }

    fn make_driver<TM: TxManager>(
        pipeline: TrackingPipeline,
        tx_manager: TM,
    ) -> BatchDriver<
        TrackingPipeline,
        PendingSource,
        TM,
        Arc<NoopThrottleClient>,
        PendingL1HeadSource,
    > {
        BatchDriver::new(
            pipeline,
            PendingSource,
            tx_manager,
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
            },
            noop_throttle(),
            Arc::new(NoopThrottleClient),
            PendingL1HeadSource,
        )
    }

    fn make_driver_with_max_pending<TM: TxManager>(
        pipeline: TrackingPipeline,
        tx_manager: TM,
        max_pending: usize,
    ) -> BatchDriver<
        TrackingPipeline,
        PendingSource,
        TM,
        Arc<NoopThrottleClient>,
        PendingL1HeadSource,
    > {
        BatchDriver::new(
            pipeline,
            PendingSource,
            tx_manager,
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: max_pending,
                drain_timeout: Duration::from_millis(10),
            },
            noop_throttle(),
            Arc::new(NoopThrottleClient),
            PendingL1HeadSource,
        )
    }

    /// `advance_l1_head` must be called with the confirmed L1 block on every
    /// confirmation so the encoder can detect channel timeouts.
    #[tokio::test]
    async fn test_advance_l1_head_called_on_confirmation() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission());

        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(
            make_driver(pipeline, ImmediateConfirmTxManager { l1_block: 42 })
                .run(cancellation.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        assert_eq!(
            recorded.lock().unwrap().l1_heads,
            vec![42],
            "advance_l1_head must be called with the confirmed L1 block"
        );
    }

    /// `advance_l1_head` must NOT be called when a submission fails — we have no
    /// confirmed L1 block to report.
    #[tokio::test]
    async fn test_advance_l1_head_not_called_on_failure() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission());

        let cancellation = CancellationToken::new();
        let handle =
            tokio::spawn(make_driver(pipeline, ImmediateFailTxManager).run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        assert!(
            recorded.lock().unwrap().l1_heads.is_empty(),
            "advance_l1_head must NOT be called on submission failure"
        );
    }

    /// When blob encoding fails the submission has already been dequeued from the pipeline
    /// (cursor advanced, `pending_confirmations` incremented). Without a requeue the channel
    /// is permanently stuck — `pending_confirmations` never returns to zero and blocks are
    /// never pruned. The driver must call requeue so the encoder can unwind that state.
    #[tokio::test]
    async fn test_blob_encoding_failure_requeues_submission() {
        // encode_frames feeds: DERIVATION_VERSION_0 (1) + frame.encode() (23 + data.len())
        // = 24 + data.len() bytes into BlobEncoder::encode. It fails when > BLOB_MAX_DATA_SIZE
        // (130_044), so data.len() >= 130_021 guarantees DataTooLarge.
        const OVERSIZED: usize = 130_021;

        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(BatchSubmission {
            id: SubmissionId(0),
            channel_id: ChannelId::default(),
            da_type: base_batcher_encoder::DaType::Blob,
            frames: vec![Arc::new(Frame { data: vec![0u8; OVERSIZED], ..Frame::default() })],
        });

        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(
            make_driver(pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .run(cancellation.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");

        let recorded = recorded.lock().unwrap();
        assert_eq!(
            recorded.requeued,
            vec![SubmissionId(0)],
            "requeue must be called when blob encoding fails so the channel is not stuck"
        );
        assert!(
            recorded.l1_heads.is_empty(),
            "advance_l1_head must not be called when blob encoding fails"
        );
    }

    /// The submission loop must drain all ready frames in a single pass when
    /// permits allow. With `max_pending_transactions`=2 and two frames ready,
    /// both must be submitted and confirmed without waiting for an I/O event
    /// between them.
    #[tokio::test]
    async fn test_submission_loop_drains_multiple_frames_concurrently() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission_with_id(0));
        pipeline.submissions.push_back(make_submission_with_id(1));

        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(
            make_driver_with_max_pending(pipeline, ImmediateConfirmTxManager { l1_block: 10 }, 2)
                .run(cancellation.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.dequeued.len(), 2, "both submissions must be dequeued");
        assert_eq!(recorded.l1_heads.len(), 2, "both submissions must be confirmed");
    }

    /// The semaphore must prevent more concurrent in-flight submissions than
    /// `max_pending_transactions`. With max=1 and a tx manager that never
    /// confirms, exactly one submission must be dequeued; the second must not
    /// be dequeued because `try_acquire_owned` fails when the slot is occupied.
    #[tokio::test]
    async fn test_semaphore_prevents_excess_concurrent_submissions() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission_with_id(0));
        pipeline.submissions.push_back(make_submission_with_id(1));

        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(
            make_driver_with_max_pending(pipeline, NeverConfirmTxManager, 1)
                .run(cancellation.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        assert_eq!(
            recorded.lock().unwrap().dequeued,
            vec![SubmissionId(0)],
            "only the first submission must be dequeued when the semaphore slot is occupied"
        );
    }

    /// With `max_pending_transactions`=1, the second submission must only be
    /// dequeued and confirmed after the first is confirmed (freeing the permit).
    /// Both must ultimately be confirmed.
    #[tokio::test]
    async fn test_second_submission_sent_after_permit_freed() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission_with_id(0));
        pipeline.submissions.push_back(make_submission_with_id(1));

        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(
            make_driver_with_max_pending(pipeline, ImmediateConfirmTxManager { l1_block: 7 }, 1)
                .run(cancellation.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.dequeued.len(), 2, "both submissions must eventually be dequeued");
        assert_eq!(
            recorded.l1_heads,
            vec![7, 7],
            "both submissions must be confirmed once the permit is freed between them"
        );
    }

    // ---- Pipeline that fails once then succeeds ----

    /// Pipeline that rejects the first `add_block` call and accepts all subsequent ones.
    ///
    /// Used to verify that the driver re-adds the triggering block after `reset()`,
    /// so the block is not silently lost when the source won't re-deliver it.
    #[derive(Debug)]
    struct OneReorgPipeline {
        /// Number of times `add_block` succeeded (post-reorg re-adds).
        blocks_accepted: Arc<Mutex<usize>>,
        /// Whether the next `add_block` call should simulate a reorg.
        fail_next: bool,
        resets: Arc<Mutex<usize>>,
    }

    impl OneReorgPipeline {
        fn new(blocks_accepted: Arc<Mutex<usize>>, resets: Arc<Mutex<usize>>) -> Self {
            Self { blocks_accepted, fail_next: true, resets }
        }
    }

    impl BatchPipeline for OneReorgPipeline {
        fn add_block(&mut self, block: OpBlock) -> Result<(), (ReorgError, Box<OpBlock>)> {
            if self.fail_next {
                self.fail_next = false;
                return Err((
                    ReorgError::ParentMismatch {
                        expected: B256::ZERO,
                        got: B256::with_last_byte(1),
                    },
                    Box::new(block),
                ));
            }
            *self.blocks_accepted.lock().unwrap() += 1;
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
        fn reset(&mut self) {
            *self.resets.lock().unwrap() += 1;
        }
        fn da_backlog_bytes(&self) -> u64 {
            0
        }
    }

    /// When `add_block` returns `ReorgError`, the driver must reset the pipeline and
    /// then re-add the triggering block so it is not permanently lost. The block
    /// queue in the encoder is empty after reset, so the parent-hash check is
    /// skipped and the re-add always succeeds.
    #[tokio::test]
    async fn test_reorg_block_is_readded_after_reset() {
        let blocks_accepted = Arc::new(Mutex::new(0usize));
        let resets = Arc::new(Mutex::new(0usize));
        let pipeline = OneReorgPipeline::new(Arc::clone(&blocks_accepted), Arc::clone(&resets));

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            OneBlockSource::new(),
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
            },
            noop_throttle(),
            Arc::new(NoopThrottleClient),
            PendingL1HeadSource,
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(*resets.lock().unwrap(), 1, "pipeline must be reset on reorg");
        assert_eq!(
            *blocks_accepted.lock().unwrap(),
            1,
            "the triggering block must be re-added after reset"
        );
    }

    /// When `add_block` returns a `ReorgError`, the driver must reset the pipeline
    /// and discard in-flight futures instead of propagating a fatal error. This
    /// mirrors the `L2BlockEvent::Reorg` handling path.
    #[tokio::test]
    async fn test_add_block_reorg_resets_pipeline_instead_of_fatal_error() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = ReorgPipeline::new(Arc::clone(&recorded));

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            OneBlockSource::new(),
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
            },
            noop_throttle(),
            Arc::new(NoopThrottleClient),
            PendingL1HeadSource,
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "driver must not return a fatal error on add_block reorg");
        assert_eq!(
            recorded.lock().unwrap().resets,
            1,
            "pipeline.reset() must be called when add_block returns ReorgError"
        );
    }

    // ---- Throttle integration tests ----

    /// When the DA backlog exceeds the threshold, the driver must call
    /// `set_max_da_size` on the throttle client with reduced limits.
    #[tokio::test]
    async fn test_throttle_client_called_on_high_backlog() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        // 2 MB backlog — above the default 1 MB threshold.
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(2_000_000);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
            },
            throttle,
            Arc::new(throttle_client),
            PendingL1HeadSource,
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
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
    }

    /// When the DA backlog is zero (below threshold), the driver must call
    /// `set_max_da_size` with the upper limits to reset any previous throttle.
    #[tokio::test]
    async fn test_throttle_client_called_with_upper_limits_on_zero_backlog() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(0);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
            },
            throttle,
            Arc::new(throttle_client),
            PendingL1HeadSource,
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
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
    }

    /// When the DA limits do not change between iterations, the driver must not
    /// call `set_max_da_size` redundantly. The deduplication check via
    /// `last_applied_da_limits` ensures the RPC is called at most once for
    /// identical limits.
    #[tokio::test]
    async fn test_throttle_not_called_redundantly() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(0);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
            },
            throttle,
            Arc::new(throttle_client),
            PendingL1HeadSource,
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        // Run for 100ms to allow multiple loop iterations.
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancellation.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "set_max_da_size must be called exactly once when limits do not change, got {}",
            calls.len()
        );
    }

    /// With the Step strategy and full intensity, when backlog is above the
    /// threshold, the driver must apply the lower DA limits.
    #[tokio::test]
    async fn test_step_strategy_full_intensity_applies_lower_limits() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        // Backlog of 100 — above threshold of 1.
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(100);

        let config =
            ThrottleConfig { threshold_bytes: 1, max_intensity: 1.0, ..Default::default() };
        let throttle = ThrottleController::new(config, ThrottleStrategy::Step);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
            },
            throttle,
            Arc::new(throttle_client),
            PendingL1HeadSource,
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
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
    }

    /// Verifies that when the DA backlog transitions from above the threshold
    /// (throttle active) to zero (throttle inactive), the driver makes exactly
    /// two RPC calls: one with reduced limits and one resetting to upper limits.
    #[tokio::test]
    async fn test_throttle_transitions_from_active_to_inactive() {
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

        let (source_tx, source_rx) = mpsc::unbounded_channel();

        // Start with 2 MB backlog — above the default 1 MB threshold.
        let backlog = Arc::new(Mutex::new(2_000_000u64));
        let pipeline = DynamicPipeline { backlog: Arc::clone(&backlog) };

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            ChannelSource { rx: source_rx },
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
            },
            throttle,
            Arc::new(throttle_client),
            PendingL1HeadSource,
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        // First iteration fires immediately on startup; give it time to complete.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Drop the backlog to zero, then wake the driver by delivering a dummy
        // block so the select! arm fires and the loop re-runs the throttle check.
        *backlog.lock().unwrap() = 0;
        source_tx.send(L2BlockEvent::Block(Box::default())).unwrap();

        tokio::time::sleep(Duration::from_millis(30)).await;
        cancellation.cancel();
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
    }

    // ---- L1 head source + safe head watch receiver tests ----

    /// When the L1 head source delivers a new head, the driver must call
    /// `advance_l1_head` on the pipeline with the new value.
    #[tokio::test]
    async fn test_l1_head_source_advances_pipeline() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (l1_source, l1_tx) = ChannelL1HeadSource::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
            },
            noop_throttle(),
            Arc::new(NoopThrottleClient),
            l1_source,
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        // Send a new L1 head via the channel.
        l1_tx.send(L1HeadEvent::NewHead(42)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok());
        let r = recorded.lock().unwrap();
        assert!(
            r.l1_heads.contains(&42),
            "advance_l1_head must be called with the source value, got {:?}",
            r.l1_heads
        );
    }

    /// When a safe head watch receiver fires, the driver must call
    /// `prune_safe` on the pipeline with the new value.
    #[tokio::test]
    async fn test_safe_head_watch_prunes_pipeline() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (safe_tx, safe_rx) = tokio::sync::watch::channel(0u64);

        let cancellation = CancellationToken::new();
        let driver = make_driver(pipeline, ImmediateConfirmTxManager { l1_block: 1 })
            .with_safe_head_rx(safe_rx);
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        // Send a new safe head.
        safe_tx.send(100).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok());
        let r = recorded.lock().unwrap();
        assert!(
            r.safe_numbers.contains(&100),
            "prune_safe must be called with the watch value, got {:?}",
            r.safe_numbers
        );
    }

    /// When the safe head sender is dropped while the driver is running, the watch
    /// arm must disable itself rather than spinning. The driver continues running
    /// and remains cancellable after the sender disappears.
    #[tokio::test]
    async fn test_safe_head_sender_drop_does_not_busyloop() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded));

        let (safe_tx, safe_rx) = tokio::sync::watch::channel(0u64);

        let cancellation = CancellationToken::new();
        let driver = make_driver(pipeline, ImmediateConfirmTxManager { l1_block: 1 })
            .with_safe_head_rx(safe_rx);
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        // Send one value, then drop the sender while the driver is still running.
        safe_tx.send(50).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(safe_tx);

        // Give the driver time to process the drop. If the arm busy-loops,
        // prune_safe would be called many additional times here.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let prune_count_after_drop = recorded.lock().unwrap().safe_numbers.len();

        // Cancel and wait — driver must exit cleanly, not hang.
        cancellation.cancel();
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
    }

    /// Without a safe head receiver, confirmation-based L1 head advancement must
    /// still work normally. The driver uses `PendingL1HeadSource` (parks forever)
    /// so only submission confirmations drive `advance_l1_head`.
    #[tokio::test]
    async fn test_no_safe_head_receiver_driver_runs_normally() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission());

        let cancellation = CancellationToken::new();
        // No .with_safe_head_rx() — safe_head remains None.
        let driver = make_driver(pipeline, ImmediateConfirmTxManager { l1_block: 7 });
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok());
        let r = recorded.lock().unwrap();
        assert_eq!(r.l1_heads, vec![7], "confirmation-based advance_l1_head must still work");
        assert!(r.safe_numbers.is_empty(), "prune_safe must not be called without a receiver");
    }
}
