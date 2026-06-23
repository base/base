//! The async batch driver that orchestrates encoding, block sourcing, and L1 submission.

use std::time::Duration;

use base_batcher_encoder::{BatchPipeline, StepResult};
use base_batcher_source::{
    L1HeadEvent, L1HeadSource, L2BlockEvent, SourceError, UnsafeBlockSource,
};
use base_common_consensus::BaseBlock;
use base_runtime::Runtime;
use base_tx_manager::TxManager;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{
    AdminCommand, BatchDriverConfig, BatchDriverError, BatcherStatus, DaThrottle, SubmissionQueue,
    ThrottleClient, ThrottleController, event::DriverEvent,
};

/// Async orchestration loop for the batcher.
///
/// Combines a [`BatchPipeline`] (encoding), an [`UnsafeBlockSource`] (L2 block delivery),
/// an [`L1HeadSource`] (L1 chain head tracking), and a [`TxManager`] (L1 submission)
/// into a single `tokio::select!` task.
///
/// Uses [`SubmissionQueue`] for concurrent receipt tracking and semaphore backpressure,
/// and [`DaThrottle`] for DA backlog throttle management.
#[derive(Debug)]
pub struct BatchDriver<R, P, S, TM, TC, L>
where
    R: Runtime,
    P: BatchPipeline,
    S: UnsafeBlockSource,
    TM: TxManager,
    TC: ThrottleClient,
    L: L1HeadSource,
{
    /// Runtime providing cancellation (and future clock/spawn use).
    runtime: R,
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
    /// Whether block ingestion is currently stopped (paused via admin or `--stopped` flag).
    stopped: bool,
    /// Admin command channel, wired in via [`Self::with_admin_rx`].
    admin_rx: Option<mpsc::Receiver<AdminCommand>>,
    /// When `true`, the driver toggles a blob-DA override on the pipeline
    /// whenever DA-backlog throttling activates. Lifted from
    /// [`BatchDriverConfig::force_blobs_when_throttling`].
    force_blobs_when_throttling: bool,
}

impl<R, P, S, TM, TC, L> BatchDriver<R, P, S, TM, TC, L>
where
    R: Runtime,
    P: BatchPipeline,
    S: UnsafeBlockSource,
    TM: TxManager,
    TC: ThrottleClient,
    L: L1HeadSource,
{
    /// Maximum number of encoding steps to run synchronously per outer loop iteration
    /// before yielding to the tokio executor. Prevents a large block backlog from
    /// starving receipt processing and cancellation checks.
    pub const STEP_BUDGET: usize = 128;

    /// Create a new [`BatchDriver`].
    pub fn new(
        runtime: R,
        pipeline: P,
        source: S,
        tx_manager: TM,
        config: BatchDriverConfig,
        throttle: DaThrottle<TC>,
        l1_head_source: L,
    ) -> Self {
        Self {
            runtime,
            pipeline,
            source,
            submissions: SubmissionQueue::new(
                tx_manager,
                config.inbox,
                config.max_pending_transactions,
            ),
            throttle,
            l1_head_source: Some(l1_head_source),
            safe_head_rx: None,
            drain_timeout: config.drain_timeout,
            stopped: false,
            admin_rx: None,
            force_blobs_when_throttling: config.force_blobs_when_throttling,
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

    /// Wire an admin command channel into the driver.
    ///
    /// When set, the driver processes admin commands as part of its main
    /// `select!` loop. When absent, the admin arm is permanently pending and
    /// the driver behaves as if no admin server is configured.
    pub fn with_admin_rx(mut self, rx: mpsc::Receiver<AdminCommand>) -> Self {
        self.admin_rx = Some(rx);
        self
    }

    /// Start the driver in a stopped state, deferring block ingestion until
    /// [`AdminCommand::Resume`] is received via the admin API.
    ///
    /// Equivalent to the batcher starting normally and immediately receiving
    /// a pause command, but without discarding any in-flight submissions.
    /// Use this when the `--stopped` flag is set at startup.
    pub const fn with_stopped(mut self, stopped: bool) -> Self {
        self.stopped = stopped;
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
    pub async fn run(mut self) -> Result<(), BatchDriverError> {
        if self.stopped {
            info!(
                stopped = true,
                "batcher starting in stopped state; call admin_startBatcher to begin submission"
            );
        }
        let mut draining = false;
        loop {
            self.drain_encoding()?;
            let is_throttling = self.throttle.apply(self.pipeline.da_backlog_bytes()).await;
            if self.force_blobs_when_throttling {
                self.pipeline.set_blob_override(is_throttling);
            }
            self.submissions.recover_txpool().await;
            self.submissions.submit_pending(&mut self.pipeline).await;

            if draining {
                self.submissions
                    .drain(&mut self.pipeline, self.runtime.sleep(self.drain_timeout))
                    .await;
                return Ok(());
            }

            match self.next_event().await? {
                DriverEvent::Shutdown => {
                    info!(
                        in_flight = %self.submissions.in_flight_count(),
                        "batcher shutting down, draining in-flight submissions"
                    );
                    self.pipeline.force_close_channel();
                    draining = true;
                }
                DriverEvent::Block(b) => {
                    self.on_block(b);
                }
                DriverEvent::Flush => {
                    self.pipeline.force_close_channel();
                    debug!("flush signal received, force-closed channel");
                }
                DriverEvent::Reorg(head) => {
                    let safe_head = self.safe_head_rx.as_ref().map(|rx| *rx.borrow()).unwrap_or(0);
                    let catchup_from = safe_head + 1;
                    warn!(
                        reorg_head = %head.block_info.number,
                        safe_head = %safe_head,
                        catchup_from = %catchup_from,
                        "L2 reorg detected, resetting pipeline and catching up from safe head"
                    );
                    self.submissions.discard();
                    self.pipeline.reset();
                    self.source.reset_catchup(catchup_from);
                }
                DriverEvent::Receipt(ids, o) => {
                    self.submissions.handle_outcome(&mut self.pipeline, ids, o);
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

    /// Drain encoding steps synchronously up to [`Self::STEP_BUDGET`].
    ///
    /// Returns `Err` on a fatal [`StepError`](base_batcher_encoder::StepError).
    fn drain_encoding(&mut self) -> Result<(), BatchDriverError> {
        let mut budget = Self::STEP_BUDGET;
        let mut steps = 0usize;
        loop {
            match self.pipeline.step() {
                Ok(StepResult::Idle) => break,
                Ok(
                    StepResult::BlockEncoded | StepResult::SpanFlushed | StepResult::ChannelClosed,
                ) => {
                    steps += 1;
                    budget -= 1;
                    if budget == 0 {
                        debug!(steps = %steps, "encoding step budget exhausted, yielding");
                        break;
                    }
                }
                Err(e) => {
                    error!(error = %e, "fatal encoding step error, batcher halting");
                    return Err(e.into());
                }
            }
        }
        if steps > 0 {
            debug!(steps = %steps, "completed encoding drain");
        }
        Ok(())
    }

    /// Ingest a new L2 block into the pipeline.
    ///
    /// If the pipeline signals a reorg via `add_block` (parent-hash mismatch),
    /// discards in-flight submissions, resets the pipeline, and restarts
    /// sequential catchup from `safe_head + 1`. The triggering block will be
    /// re-delivered by the sequential poller.
    fn on_block(&mut self, block: Box<BaseBlock>) {
        let number = block.header.number;
        match self.pipeline.add_block(*block) {
            Ok(()) => {
                debug!(block = %number, "added unsafe block to pipeline");
            }
            Err((e, _block)) => {
                let safe_head = self.safe_head_rx.as_ref().map(|rx| *rx.borrow()).unwrap_or(0);
                let catchup_from = safe_head + 1;
                warn!(
                    block = %number,
                    safe_head = %safe_head,
                    catchup_from = %catchup_from,
                    error = %e,
                    "reorg detected during block ingestion, resetting pipeline and catching up from safe head"
                );
                self.submissions.discard();
                self.pipeline.reset();
                self.source.reset_catchup(catchup_from);
            }
        }
    }

    /// Block on the next external event using a biased `tokio::select!`.
    ///
    /// Admin commands are handled inline in the loop — only non-admin events
    /// are returned to the caller. Admin commands are placed before the source
    /// arm so control-plane operations (pause, resume, flush) are never starved
    /// by sustained block throughput.
    ///
    /// [`AdminCommand::Pause`] immediately discards in-flight submissions and
    /// resets the pipeline, then drops `Block` and `Flush` source events until
    /// [`AdminCommand::Resume`] is received. Reorg events propagate regardless
    /// of pause state. On resume the source is reset to catch up sequentially
    /// from the last known safe L2 head.
    ///
    /// Non-fatal L1 head source errors loop internally to avoid polluting the
    /// return type with a no-op variant.
    async fn next_event(&mut self) -> Result<DriverEvent, BatchDriverError> {
        loop {
            let event = tokio::select! {
                biased;

                _ = self.runtime.cancelled() => DriverEvent::Shutdown,

                cmd = Self::next_admin_cmd(&mut self.admin_rx) => {
                    match cmd {
                        AdminCommand::Flush => return Ok(DriverEvent::Flush),
                        AdminCommand::Pause => {
                            self.submissions.discard();
                            self.pipeline.reset();
                            self.stopped = true;
                            info!(stopped = true, "batcher paused via admin");
                        }
                        AdminCommand::Resume => {
                            let safe_head =
                                self.safe_head_rx.as_ref().map(|rx| *rx.borrow());
                            if let Some(n) = safe_head {
                                self.source.reset_catchup(n + 1);
                                info!(
                                    stopped = false,
                                    catchup_from = %(n + 1),
                                    "batcher resumed via admin, catching up from safe head"
                                );
                            } else {
                                info!(stopped = false, "batcher resumed via admin");
                            }
                            self.stopped = false;
                        }
                        AdminCommand::SetThrottle { strategy, config } => {
                            self.throttle.set_controller(
                                ThrottleController::new(config, strategy)
                            );
                            info!("throttle controller replaced via admin");
                        }
                        AdminCommand::ResetThrottle => {
                            self.throttle.reset();
                            info!("throttle controller reset via admin");
                        }
                        AdminCommand::GetThrottleInfo { reply } => {
                            let _ = reply.send(
                                self.throttle.snapshot(self.pipeline.da_backlog_bytes())
                            );
                        }
                        AdminCommand::GetStatus { reply } => {
                            let _ = reply.send(BatcherStatus {
                                stopped: self.stopped,
                                in_flight: self.submissions.in_flight_count(),
                                da_backlog_bytes: self.pipeline.da_backlog_bytes(),
                            });
                        }
                    }
                    // All commands except Flush loop to await the next real event.
                    continue;
                }

                event = self.source.next() => match event {
                    Ok(L2BlockEvent::Block(_) | L2BlockEvent::Flush) if self.stopped => {
                        continue;
                    }
                    Ok(L2BlockEvent::Block(block)) => DriverEvent::Block(block),
                    Ok(L2BlockEvent::Flush) => DriverEvent::Flush,
                    Ok(L2BlockEvent::Reorg { new_safe_head }) => DriverEvent::Reorg(new_safe_head),
                    Err(SourceError::Exhausted) => DriverEvent::Shutdown,
                    Err(e) => return Err(e.into()),
                },

                Some((ids, outcome)) = self.submissions.next_settled() => {
                    DriverEvent::Receipt(ids, outcome)
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
                            // Sender dropped; safe-head poller has exited. Disable this
                            // arm permanently and warn so operators know pruning stopped.
                            warn!("safe-head watch sender dropped; safe-head pruning disabled");
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

    /// Returns the next admin command, or parks forever if no channel is wired.
    ///
    /// Takes only the `Option<Receiver>` to avoid a full `&mut self` borrow
    /// conflicting with the other `select!` arms.
    async fn next_admin_cmd(rx: &mut Option<mpsc::Receiver<AdminCommand>>) -> AdminCommand {
        match rx {
            Some(rx) => match rx.recv().await {
                Some(cmd) => cmd,
                None => std::future::pending().await,
            },
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, B256, Bloom, Bytes};
    use alloy_rpc_types_eth::TransactionReceipt;
    use base_batcher_encoder::{BatchSubmission, DaType, FrameEncoder, SubmissionId};
    use base_batcher_source::{
        L1HeadEvent, L1HeadSource, L2BlockEvent, SourceError, UnsafeBlockSource,
    };
    use base_blobs::{BlobDecoder, BlobEncoder};
    use base_protocol::{ChannelId, Frame};
    use base_runtime::{
        Cancellation, Clock, Spawner,
        deterministic::{Config, Runner},
    };
    use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError};
    use tokio::sync::{mpsc, oneshot, watch};

    use crate::{
        AdminCommand, BatchDriver, BatchDriverConfig, DaThrottle, NoopThrottleClient,
        ThrottleController,
        event::DriverEvent,
        test_utils::{
            DriverFixture, ImmediateConfirmTxManager, ImmediateFailTxManager,
            NeverConfirmTxManager, Recorded, SubmissionStub, TrackingPipeline,
        },
    };

    #[derive(Debug)]
    struct QueuedSource {
        events: VecDeque<Result<L2BlockEvent, SourceError>>,
    }

    impl QueuedSource {
        fn new(events: impl IntoIterator<Item = Result<L2BlockEvent, SourceError>>) -> Self {
            Self { events: events.into_iter().collect() }
        }
    }

    #[async_trait::async_trait]
    impl UnsafeBlockSource for QueuedSource {
        async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
            match self.events.pop_front() {
                Some(event) => event,
                None => std::future::pending().await,
            }
        }

        fn reset_catchup(&mut self, _: u64) {}
    }

    #[derive(Debug)]
    struct QueuedL1HeadSource {
        events: VecDeque<Result<L1HeadEvent, SourceError>>,
    }

    impl QueuedL1HeadSource {
        fn new(events: impl IntoIterator<Item = Result<L1HeadEvent, SourceError>>) -> Self {
            Self { events: events.into_iter().collect() }
        }
    }

    #[async_trait::async_trait]
    impl L1HeadSource for QueuedL1HeadSource {
        async fn next(&mut self) -> Result<L1HeadEvent, SourceError> {
            match self.events.pop_front() {
                Some(event) => event,
                None => std::future::pending().await,
            }
        }
    }

    /// Build a [`BatchSubmission`] whose single frame exactly fills one blob payload,
    /// leaving no room for any additional frame alongside it.
    ///
    /// `payload = 1 (DERIVATION_VERSION_0) + FRAME_OVERHEAD + data.len() = BLOB_MAX_DATA_SIZE`
    fn blob_filling_submission(id: u64) -> BatchSubmission {
        blob_filling_submission_with_frames(id, 1)
    }

    fn blob_filling_submission_with_frames(id: u64, frame_count: usize) -> BatchSubmission {
        let data_len = BlobEncoder::BLOB_MAX_DATA_SIZE - 1 - BlobEncoder::FRAME_OVERHEAD;
        BatchSubmission {
            id: SubmissionId(id),
            channel_id: ChannelId::default(),
            da_type: DaType::Blob,
            frames: (0..frame_count)
                .map(|number| {
                    Arc::new(Frame {
                        number: number.try_into().unwrap(),
                        data: vec![0u8; data_len],
                        ..Frame::default()
                    })
                })
                .collect(),
        }
    }

    const fn stub_receipt(block_number: u64) -> TransactionReceipt {
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

    fn driver_for_next_event<R: base_runtime::Runtime, TM: TxManager>(
        runtime: R,
        source_events: impl IntoIterator<Item = Result<L2BlockEvent, SourceError>>,
        l1_events: impl IntoIterator<Item = Result<L1HeadEvent, SourceError>>,
        tx_manager: TM,
    ) -> BatchDriver<
        R,
        TrackingPipeline,
        QueuedSource,
        TM,
        Arc<NoopThrottleClient>,
        QueuedL1HeadSource,
    > {
        BatchDriver::new(
            runtime,
            TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default()))),
            QueuedSource::new(source_events),
            tx_manager,
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
            },
            DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
            QueuedL1HeadSource::new(l1_events),
        )
    }

    #[derive(Debug, Default)]
    struct TxpoolBlockedState {
        sends: AtomicU64,
        cancellations: AtomicU64,
    }

    #[derive(Debug, Clone)]
    struct TxpoolBlockedOnceTxManager {
        state: Arc<TxpoolBlockedState>,
    }

    impl TxManager for TxpoolBlockedOnceTxManager {
        async fn send(&self, _: TxCandidate) -> SendResponse {
            Err(TxManagerError::AlreadyReserved)
        }

        fn send_async(
            &self,
            _: TxCandidate,
        ) -> impl std::future::Future<Output = SendHandle> + Send {
            self.state.sends.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Err(TxManagerError::AlreadyReserved));
            std::future::ready(SendHandle::new(rx))
        }

        fn cancel_tx(
            &self,
        ) -> impl std::future::Future<Output = base_tx_manager::TxManagerResult<()>> + Send
        {
            let state = Arc::clone(&self.state);
            async move {
                state.cancellations.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[derive(Debug)]
    struct RecordedCandidate {
        tx_data: Bytes,
        decoded_blob_payloads: Vec<Bytes>,
    }

    #[derive(Debug, Clone)]
    struct RecordingConfirmTxManager {
        l1_block: u64,
        candidates: Arc<Mutex<Vec<RecordedCandidate>>>,
    }

    impl TxManager for RecordingConfirmTxManager {
        async fn send(&self, _: TxCandidate) -> SendResponse {
            unreachable!()
        }

        fn send_async(
            &self,
            candidate: TxCandidate,
        ) -> impl std::future::Future<Output = SendHandle> + Send {
            let decoded_blob_payloads = candidate
                .blobs
                .iter()
                .map(|blob| BlobDecoder::decode(blob).expect("blob payload should decode"))
                .collect();
            self.candidates
                .lock()
                .unwrap()
                .push(RecordedCandidate { tx_data: candidate.tx_data, decoded_blob_payloads });
            let l1_block = self.l1_block;
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Ok(stub_receipt(l1_block)));
            std::future::ready(SendHandle::new(rx))
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[test]
    fn next_event_prioritizes_cancellation_over_ready_admin() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (admin_tx, admin_rx) = mpsc::channel(1);
            admin_tx.send(AdminCommand::Flush).await.expect("admin receiver should be open");

            let mut driver = driver_for_next_event(
                ctx.clone(),
                [Ok(L2BlockEvent::Flush)],
                [Ok(L1HeadEvent::NewHead(9))],
                ImmediateConfirmTxManager { l1_block: 1 },
            )
            .with_admin_rx(admin_rx);

            ctx.cancel();

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::Shutdown));
        });
    }

    #[test]
    fn next_event_prioritizes_admin_before_source() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (admin_tx, admin_rx) = mpsc::channel(1);
            admin_tx.send(AdminCommand::Flush).await.expect("admin receiver should be open");

            let mut driver = driver_for_next_event(
                ctx,
                [Ok(L2BlockEvent::Block(Box::default()))],
                [Ok(L1HeadEvent::NewHead(9))],
                ImmediateConfirmTxManager { l1_block: 1 },
            )
            .with_admin_rx(admin_rx);

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::Flush));
        });
    }

    #[test]
    fn next_event_prioritizes_source_before_receipts_and_heads() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (_safe_tx, safe_rx) = watch::channel(0);
            let mut driver = driver_for_next_event(
                ctx,
                [Ok(L2BlockEvent::Flush)],
                [Ok(L1HeadEvent::NewHead(9))],
                ImmediateConfirmTxManager { l1_block: 1 },
            )
            .with_safe_head_rx(safe_rx);
            driver.pipeline.submissions.push_back(SubmissionStub::stub());
            driver.submissions.submit_pending(&mut driver.pipeline).await;

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::Flush));
        });
    }

    #[test]
    fn next_event_prioritizes_receipts_before_l1_head_and_safe_head() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (safe_tx, safe_rx) = watch::channel(0);
            safe_tx.send(5).expect("safe-head receiver should be open");

            let mut driver = driver_for_next_event(
                ctx,
                [],
                [Ok(L1HeadEvent::NewHead(9))],
                ImmediateConfirmTxManager { l1_block: 42 },
            )
            .with_safe_head_rx(safe_rx);
            driver.pipeline.submissions.push_back(SubmissionStub::stub());
            driver.submissions.submit_pending(&mut driver.pipeline).await;

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::Receipt(_, _)));
        });
    }

    #[test]
    fn next_event_prioritizes_l1_head_before_safe_head() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (safe_tx, safe_rx) = watch::channel(0);
            safe_tx.send(5).expect("safe-head receiver should be open");

            let mut driver = driver_for_next_event(
                ctx,
                [],
                [Ok(L1HeadEvent::NewHead(9))],
                ImmediateConfirmTxManager { l1_block: 1 },
            )
            .with_safe_head_rx(safe_rx);

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::L1Head(9)));
        });
    }

    #[test]
    fn next_event_returns_safe_head_when_only_safe_head_is_ready() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (safe_tx, safe_rx) = watch::channel(0);
            safe_tx.send(7).expect("safe-head receiver should be open");

            let mut driver =
                driver_for_next_event(ctx, [], [], ImmediateConfirmTxManager { l1_block: 1 })
                    .with_safe_head_rx(safe_rx);

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::SafeHead(7)));
        });
    }

    /// `advance_l1_head` must be called with the confirmed L1 block on every
    /// confirmation so the encoder can detect channel timeouts.
    #[test]
    fn test_advance_l1_head_called_on_confirmation() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(SubmissionStub::stub());

            let handle = ctx.spawn(
                DriverFixture::build(
                    ctx.clone(),
                    pipeline,
                    ImmediateConfirmTxManager { l1_block: 42 },
                )
                .run(),
            );

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            assert_eq!(
                recorded.lock().unwrap().l1_heads,
                vec![42],
                "advance_l1_head must be called with the confirmed L1 block"
            );
        });
    }

    /// `advance_l1_head` must NOT be called when a submission fails — we have no
    /// confirmed L1 block to report.
    #[test]
    fn test_advance_l1_head_not_called_on_failure() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(SubmissionStub::stub());

            let handle = ctx
                .spawn(DriverFixture::build(ctx.clone(), pipeline, ImmediateFailTxManager).run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            assert!(
                recorded.lock().unwrap().l1_heads.is_empty(),
                "advance_l1_head must NOT be called on submission failure"
            );
        });
    }

    /// When blob encoding fails the submission has already been dequeued from the pipeline
    /// (cursor advanced, `pending_confirmations` incremented). Without a requeue the channel
    /// is permanently stuck — `pending_confirmations` never returns to zero and blocks are
    /// never pruned. The driver must call requeue so the encoder can unwind that state.
    #[test]
    fn test_blob_encoding_failure_requeues_submission() {
        // Blob submission encoding feeds DERIVATION_VERSION_0 (1) + frame.encode()
        // (23 + data.len()) into BlobEncoder::encode. It fails when > BLOB_MAX_DATA_SIZE
        // (130_044), so data.len() >= 130_021 guarantees DataTooLarge.
        const OVERSIZED: usize = 130_021;

        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(BatchSubmission {
                id: SubmissionId(0),
                channel_id: ChannelId::default(),
                da_type: DaType::Blob,
                frames: vec![Arc::new(Frame { data: vec![0u8; OVERSIZED], ..Frame::default() })],
            });

            let handle = ctx.spawn(
                DriverFixture::build(
                    ctx.clone(),
                    pipeline,
                    ImmediateConfirmTxManager { l1_block: 1 },
                )
                .run(),
            );

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

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
        });
    }

    /// The submission loop must submit each pipeline submission as one L1 tx. The
    /// pipeline is responsible for choosing the frames that belong in a tx, matching
    /// op-batcher's `NextTxData` boundary.
    #[test]
    fn test_submission_loop_submits_each_pipeline_submission_as_one_tx() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let candidates = Arc::new(Mutex::new(Vec::new()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(SubmissionStub::with_id(0));
            pipeline.submissions.push_back(SubmissionStub::with_id(1));

            let handle = ctx.spawn(
                DriverFixture::build_with_max_pending(
                    ctx.clone(),
                    pipeline,
                    RecordingConfirmTxManager { l1_block: 10, candidates: Arc::clone(&candidates) },
                    2,
                )
                .run(),
            );

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            let recorded = recorded.lock().unwrap();
            assert_eq!(recorded.dequeued.len(), 2, "both submissions must be dequeued");
            assert_eq!(
                recorded.l1_heads,
                vec![10, 10],
                "each pipeline submission should produce its own confirmation"
            );
            assert_eq!(
                candidates.lock().unwrap().len(),
                2,
                "separate pipeline submissions must not be coalesced into one L1 tx"
            );
        });
    }

    /// A single submission may contain multiple blob-filling frames when
    /// `target_num_frames > 1`. Matching op-batcher, each frame becomes its own
    /// blob in the same L1 transaction.
    #[test]
    fn test_multi_frame_blob_submission_maps_frames_to_blobs() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let candidates = Arc::new(Mutex::new(Vec::new()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            let submission = blob_filling_submission_with_frames(0, 3);
            let expected_blob_payloads: Vec<_> =
                submission.frames.iter().map(|frame| FrameEncoder::to_calldata(frame)).collect();
            pipeline.submissions.push_back(submission);

            let handle = ctx.spawn(
                DriverFixture::build(
                    ctx.clone(),
                    pipeline,
                    RecordingConfirmTxManager { l1_block: 10, candidates: Arc::clone(&candidates) },
                )
                .run(),
            );

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            let recorded = recorded.lock().unwrap();
            assert_eq!(recorded.dequeued, vec![SubmissionId(0)], "submission must be dequeued");
            assert!(
                recorded.requeued.is_empty(),
                "multi-frame blob submission must not be requeued by blob encoding"
            );
            assert_eq!(
                recorded.l1_heads,
                vec![10],
                "multi-frame blob submission should confirm in one L1 tx"
            );
            let candidates = candidates.lock().unwrap();
            assert_eq!(candidates.len(), 1, "multi-frame submission should use one L1 tx");
            assert!(
                candidates[0].tx_data.is_empty(),
                "blob transactions must not also carry calldata"
            );
            assert_eq!(
                candidates[0].decoded_blob_payloads, expected_blob_payloads,
                "each frame in the submission must become its own blob payload"
            );
        });
    }

    /// The semaphore must prevent more concurrent in-flight L1 txs than
    /// `max_pending_transactions`. With max=1 and two submissions, the second
    /// submission must not be dequeued while the first tx still holds the permit.
    #[test]
    fn test_semaphore_prevents_excess_concurrent_submissions() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(blob_filling_submission(0));
            pipeline.submissions.push_back(blob_filling_submission(1));

            let handle = ctx.spawn(
                DriverFixture::build_with_max_pending(
                    ctx.clone(),
                    pipeline,
                    NeverConfirmTxManager,
                    1,
                )
                .run(),
            );

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            let recorded = recorded.lock().unwrap();
            assert_eq!(recorded.dequeued, vec![SubmissionId(0)], "only one permit is available");
            assert!(recorded.requeued.is_empty(), "blocked submissions must not be dequeued");
            // The semaphore (max=1) is occupied by blob 1 — no second tx was submitted.
            assert!(recorded.l1_heads.is_empty(), "no confirmation while semaphore is full");
        });
    }

    /// With `max_pending_transactions`=1 and blob-filling submissions, the second
    /// blob tx is only submitted once the first is confirmed (freeing the permit).
    #[test]
    fn test_second_blob_tx_submitted_after_permit_freed() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(blob_filling_submission(0));
            pipeline.submissions.push_back(blob_filling_submission(1));
            pipeline.submissions.push_back(blob_filling_submission(2));

            let handle = ctx.spawn(
                DriverFixture::build_with_max_pending(
                    ctx.clone(),
                    pipeline,
                    ImmediateConfirmTxManager { l1_block: 7 },
                    1,
                )
                .run(),
            );

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            assert_eq!(
                recorded.lock().unwrap().l1_heads,
                vec![7, 7, 7],
                "each queued submission must confirm as permits are freed"
            );
        });
    }

    /// `AlreadyReserved` means another transaction owns the sender nonce slot.
    /// The driver must requeue the submission, mark the txpool blocked, and
    /// call `cancel_tx` before accepting more submissions.
    #[test]
    fn test_txpool_blocked_requeues_and_attempts_recovery() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(SubmissionStub::stub());

            let state = Arc::new(TxpoolBlockedState::default());
            let tx_manager = TxpoolBlockedOnceTxManager { state: Arc::clone(&state) };

            let handle = ctx.spawn(DriverFixture::build(ctx.clone(), pipeline, tx_manager).run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            assert_eq!(
                recorded.lock().unwrap().requeued,
                vec![SubmissionId(0)],
                "txpool-blocked submissions must be requeued"
            );
            assert_eq!(
                state.sends.load(Ordering::SeqCst),
                1,
                "driver must stop submitting while txpool is blocked"
            );
            assert_eq!(
                state.cancellations.load(Ordering::SeqCst),
                1,
                "driver must attempt txpool recovery with cancel_tx"
            );
        });
    }
}
