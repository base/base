//! The async batch driver that orchestrates encoding, block sourcing, and L1 submission.

use std::time::Duration;

use base_batcher_encoder::{
    BatchPipeline, BatcherMetrics, DerivationReconciliation, StepError, StepResult,
};
use base_batcher_source::{L1HeadSource, L2BlockEvent, UnsafeBlockSource};
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use base_runtime::Runtime;
use base_tx_manager::TxManager;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{
    AdminCommand, AdminError, BatchDriverConfig, BatchDriverError, BatcherStatus, DaThrottle,
    DerivationStatus, SubmissionQueue, ThrottleClient, ThrottleController, event::DriverEvent,
};

/// The sources a [`BatchDriver`] listens to, and the L1 head and derivation status it
/// starts from.
#[derive(Debug)]
pub struct BatchDriverInputs<S, L> {
    /// Source of unsafe L2 blocks and reorg signals.
    pub source: S,
    /// Source of live L1 head updates.
    pub l1_head_source: L,
    /// Live L1 head at startup.
    pub initial_l1_head: u64,
    /// Derivation status at startup.
    pub initial_status: DerivationStatus,
    /// Ordered derivation-status updates.
    pub derivation_status_rx: mpsc::Receiver<DerivationStatus>,
    /// Admin commands; see [`AdminHandle::channel`](crate::AdminHandle::channel).
    pub admin_rx: mpsc::Receiver<AdminCommand>,
}

/// Async orchestration loop for the batcher.
///
/// Combines a [`BatchPipeline`] (encoding), an [`UnsafeBlockSource`] (L2 block delivery),
/// an [`L1HeadSource`] (L1 chain head tracking), ordered [`DerivationStatus`] updates,
/// and a [`TxManager`] (L1 submission) into a single `tokio::select!` task.
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
    l1_head_source: L,
    /// Last trusted L2 safe head.
    safe_head: BlockInfo,
    /// Ordered derivation-progress snapshots.
    derivation_status_rx: mpsc::Receiver<DerivationStatus>,
    /// Maximum wall-clock time to wait for in-flight submissions to settle
    /// when draining on cancellation.
    drain_timeout: Duration,
    /// Whether block ingestion is currently stopped (via admin or the `--stopped` flag).
    stopped: bool,
    /// Admin command channel. Once every [`AdminHandle`](crate::AdminHandle) is dropped, the
    /// arm goes quiet.
    admin_rx: mpsc::Receiver<AdminCommand>,
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

    /// Create a [`BatchDriver`].
    ///
    /// Advances the pipeline to the initial L1 head, so channel duration is measured from
    /// the live L1 tip rather than from block 0.
    pub fn new(
        runtime: R,
        mut pipeline: P,
        tx_manager: TM,
        config: BatchDriverConfig,
        throttle: DaThrottle<TC>,
        inputs: BatchDriverInputs<S, L>,
    ) -> Self {
        pipeline.advance_l1_head(inputs.initial_l1_head);
        Self {
            runtime,
            pipeline,
            source: inputs.source,
            submissions: SubmissionQueue::new(
                tx_manager,
                config.inbox,
                config.max_pending_transactions,
            ),
            throttle,
            l1_head_source: inputs.l1_head_source,
            safe_head: inputs.initial_status.safe_l2,
            derivation_status_rx: inputs.derivation_status_rx,
            drain_timeout: config.drain_timeout,
            stopped: config.stopped,
            admin_rx: inputs.admin_rx,
            force_blobs_when_throttling: config.force_blobs_when_throttling,
        }
    }

    /// Run the batch driver loop.
    ///
    /// Each iteration has two phases:
    /// 1. **CPU phase**: drain encoding, apply throttle, recover txpool, submit pending frames.
    /// 2. **I/O phase**: block on `tokio::select!` until one external event fires.
    ///
    /// Every event the I/O phase returns is therefore followed by a CPU phase before the
    /// driver waits again, so the work an event releases is done before the next one, up to
    /// the encoding step budget.
    ///
    /// When shutting down after cancellation, the I/O phase is replaced by a bounded drain
    /// of all in-flight receipts.
    pub async fn run(mut self) -> Result<(), BatchDriverError> {
        if self.stopped {
            info!(
                stopped = true,
                "batcher starting in stopped state; call admin_startBatcher to begin submission"
            );
        }

        let mut shutting_down = false;
        let mut shutdown_flush_error: Option<StepError> = None;
        loop {
            if !shutting_down {
                self.apply_pending_derivation_status_updates()?;
            }

            self.drain_encoding()?;
            let is_throttling = self.throttle.apply(self.pipeline.da_backlog_bytes()).await;
            if self.force_blobs_when_throttling {
                self.pipeline.set_blob_override(is_throttling);
            }
            self.submissions.recover_txpool().await;
            self.submissions.submit_pending(&mut self.pipeline).await;

            if shutting_down {
                self.submissions
                    .drain(&mut self.pipeline, self.runtime.sleep(self.drain_timeout))
                    .await;
                if let Some(error) = shutdown_flush_error {
                    return Err(error.into());
                }
                return Ok(());
            }

            match self.next_event().await? {
                DriverEvent::Shutdown => {
                    info!(
                        in_flight = %self.submissions.in_flight_count(),
                        "batcher shutting down, draining in-flight submissions"
                    );
                    if let Err(error) = self.pipeline.flush() {
                        warn!(error = %error, "flush failed during shutdown");
                        shutdown_flush_error = Some(error);
                    }
                    shutting_down = true;
                }
                DriverEvent::Block(b) => {
                    self.on_block(b);
                }
                DriverEvent::Flush(reply) => {
                    self.pipeline.flush()?;
                    let _ = reply.send(Ok(()));
                    debug!("admin flush applied, released channel artifacts");
                }
                DriverEvent::Reorg => {
                    warn!("L2 reorg detected, resetting pipeline and catching up from safe head");
                    self.reset_to_safe_head(BatcherMetrics::RESET_SOURCE_REORG);
                }
                DriverEvent::Receipt(ids, o) => {
                    self.submissions.handle_outcome(&mut self.pipeline, ids, o);
                }
                DriverEvent::L1Head(n) => {
                    self.pipeline.advance_l1_head(n);
                    debug!(l1_head = %n, "L1 head advanced via source");
                }
                DriverEvent::DerivationStatus(status) => {
                    self.on_derivation_status(status);
                }
            }
        }
    }

    /// Drain encoding steps synchronously up to [`Self::STEP_BUDGET`].
    ///
    /// Stops at [`StepResult::Idle`] (nothing left to encode) or when the step budget runs
    /// out. Returns `Err` on a fatal [`StepError`].
    fn drain_encoding(&mut self) -> Result<(), BatchDriverError> {
        let mut steps = 0usize;
        loop {
            match self.pipeline.step() {
                Ok(StepResult::Idle) => break,
                Ok(StepResult::BlockEncoded | StepResult::ChannelClosed) => {
                    steps += 1;
                    if steps == Self::STEP_BUDGET {
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

    /// Drop buffered pipeline state, recording why it was dropped.
    fn reset_pipeline(&mut self, reason: &'static str) {
        BatcherMetrics::pipeline_reset_total(reason).increment(1);
        self.pipeline.reset();
    }

    /// Reset volatile state and restart delivery above the latest safe head.
    fn reset_to_safe_head(&mut self, reason: &'static str) {
        self.reset_pipeline(reason);
        self.source.reset_catchup(self.safe_head);
    }

    /// Reconcile buffered state with an ordered derivation-progress snapshot.
    fn on_derivation_status(&mut self, status: DerivationStatus) {
        let head = status.safe_l2;
        let previous = self.safe_head;
        self.safe_head = head;

        if head.number < previous.number
            || (head.number == previous.number && head.hash != previous.hash)
        {
            warn!(
                previous_safe_l2 = %previous.number,
                previous_safe_hash = %previous.hash,
                safe_l2 = %head.number,
                safe_hash = %head.hash,
                "safe L2 head changed chain, resetting pipeline"
            );
            self.reset_to_safe_head(BatcherMetrics::RESET_SAFE_HEAD_REORG);
            return;
        }

        match self
            .pipeline
            .reconcile_derivation(head, status.current_l1.map(|current_l1| current_l1.number))
        {
            DerivationReconciliation::Consistent => {}
            DerivationReconciliation::SafeHeadMismatch => {
                warn!(
                    safe_l2 = %head.number,
                    safe_hash = %head.hash,
                    "safe L2 head does not match buffered chain, resetting pipeline"
                );
                self.reset_to_safe_head(BatcherMetrics::RESET_SAFE_HEAD_MISMATCH);
            }
            DerivationReconciliation::StalledChannel => {
                warn!(
                    current_l1 = ?status.current_l1.map(|current_l1| current_l1.number),
                    safe_l2 = %head.number,
                    "rollup node passed a fully confirmed channel without deriving it, resetting pipeline"
                );
                self.reset_to_safe_head(BatcherMetrics::RESET_STALLED_CHANNEL);
            }
        }
    }

    /// Apply derivation-status updates that arrived before the next CPU phase.
    fn apply_pending_derivation_status_updates(&mut self) -> Result<(), BatchDriverError> {
        loop {
            match self.derivation_status_rx.try_recv() {
                Ok(status) => self.on_derivation_status(status),
                Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::error::TryRecvError::Disconnected) if self.runtime.is_cancelled() => {
                    return Ok(());
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(BatchDriverError::DerivationStatusSourceClosed);
                }
            }
        }
    }

    /// Ingest a new L2 block into the pipeline.
    ///
    /// If the pipeline signals a reorg via `add_block` (parent-hash mismatch),
    /// resets the pipeline and restarts sequential catchup from `safe_head + 1`.
    /// The triggering block will be re-delivered by the sequential poller.
    fn on_block(&mut self, block: Box<BaseBlock>) {
        let number = block.header.number;
        if number <= self.safe_head.number {
            return;
        }

        match self.pipeline.add_block(*block) {
            Ok(()) => {
                debug!(block = %number, "added unsafe block to pipeline");
            }
            Err((e, _block)) => {
                warn!(
                    block = %number,
                    error = %e,
                    "reorg detected during block ingestion, resetting pipeline and catching up from safe head"
                );
                self.reset_to_safe_head(BatcherMetrics::RESET_INGEST_REORG);
            }
        }
    }

    /// Stop block ingestion and drop the buffered pipeline state.
    ///
    /// Submissions already in flight keep settling.
    fn on_admin_stop(&mut self) {
        // Leave a stopped batcher alone. It holds nothing to reset.
        if self.stopped {
            return;
        }

        self.reset_pipeline(BatcherMetrics::RESET_ADMIN_STOP);
        self.stopped = true;
        info!(stopped = true, "batcher stopped via admin");
    }

    /// Start block ingestion again from the safe head.
    fn on_admin_start(&mut self) {
        // Leave a running batcher alone. Re-anchoring its source would replay blocks the
        // pipeline already holds.
        if !self.stopped {
            return;
        }

        self.source.reset_catchup(self.safe_head);
        info!(
            stopped = false,
            safe_l2 = %self.safe_head.number,
            "batcher started via admin, catching up from safe head"
        );
        self.stopped = false;
    }

    /// Block on the next external event using a biased `tokio::select!`.
    ///
    /// Admin commands are handled inline in the loop. Only a flush on a running
    /// batcher is returned to the caller, as [`DriverEvent::Flush`]. Admin
    /// commands are placed before the source arm so control-plane operations
    /// (stop, start, flush) are never starved by sustained block throughput.
    /// Derivation-status changes are also handled before unsafe blocks so pruning and
    /// recovery cannot be starved by sequential catchup.
    ///
    /// [`AdminCommand::Stop`] immediately resets the pipeline, then drops `Block`
    /// source events until [`AdminCommand::Start`] is received. Reorg events
    /// propagate regardless of the stopped state. On start the source is reset to
    /// catch up sequentially from the last known safe L2 head. Stopping a stopped
    /// batcher or starting a running one does nothing. Each command is answered
    /// once it has been applied.
    async fn next_event(&mut self) -> Result<DriverEvent, BatchDriverError> {
        loop {
            let event = tokio::select! {
                biased;

                _ = self.runtime.cancelled() => DriverEvent::Shutdown,

                Some(cmd) = self.admin_rx.recv() => {
                    match cmd {
                        AdminCommand::Flush { reply } if self.stopped => {
                            let _ = reply.send(Err(AdminError::Stopped));
                        }
                        AdminCommand::Flush { reply } => {
                            return Ok(DriverEvent::Flush(reply));
                        }
                        AdminCommand::Stop { reply } => {
                            self.on_admin_stop();
                            let _ = reply.send(Ok(()));
                        }
                        AdminCommand::Start { reply } => {
                            self.on_admin_start();
                            let _ = reply.send(Ok(()));
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
                    // Await the next real event. Only a flush on a running batcher returns above.
                    continue;
                }

                status = self.derivation_status_rx.recv() => match status {
                    Some(status) => DriverEvent::DerivationStatus(status),
                    None => return Err(BatchDriverError::DerivationStatusSourceClosed),
                },

                event = self.source.next() => match event {
                    L2BlockEvent::Block(_) if self.stopped => continue,
                    L2BlockEvent::Block(block) => DriverEvent::Block(block),
                    L2BlockEvent::Reorg => DriverEvent::Reorg,
                },

                Some((ids, outcome)) = self.submissions.next_settled() => {
                    DriverEvent::Receipt(ids, outcome)
                }

                head = self.l1_head_source.next() => DriverEvent::L1Head(head),
            };
            return Ok(event);
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
    use base_batcher_encoder::{
        BatchSubmission, BlobPayload, FrameEncoder, SubmissionId, SubmissionPayload,
    };
    use base_batcher_source::{L1HeadSource, L2BlockEvent, UnsafeBlockSource};
    use base_blobs::{BlobDecoder, BlobEncoder};
    use base_protocol::{BlockInfo, Frame};
    use base_runtime::{
        Cancellation, Clock, Spawner,
        deterministic::{Config, Runner},
    };
    use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError};
    use tokio::sync::{mpsc, oneshot};

    use crate::{
        AdminCommand, BatchDriver, BatchDriverConfig, BatchDriverInputs, DaThrottle,
        DerivationStatus, NoopThrottleClient, ThrottleController,
        event::DriverEvent,
        test_utils::{
            DriverFixture, ImmediateConfirmTxManager, ImmediateFailTxManager,
            NeverConfirmTxManager, Recorded, SubmissionStub, TrackingPipeline,
        },
    };

    #[derive(Debug)]
    struct QueuedSource {
        events: VecDeque<L2BlockEvent>,
    }

    impl QueuedSource {
        fn new(events: impl IntoIterator<Item = L2BlockEvent>) -> Self {
            Self { events: events.into_iter().collect() }
        }
    }

    #[async_trait::async_trait]
    impl UnsafeBlockSource for QueuedSource {
        async fn next(&mut self) -> L2BlockEvent {
            match self.events.pop_front() {
                Some(event) => event,
                None => std::future::pending().await,
            }
        }

        fn reset_catchup(&mut self, _: BlockInfo) {}
    }

    #[derive(Debug)]
    struct QueuedL1HeadSource {
        heads: VecDeque<u64>,
    }

    impl QueuedL1HeadSource {
        fn new(heads: impl IntoIterator<Item = u64>) -> Self {
            Self { heads: heads.into_iter().collect() }
        }
    }

    #[async_trait::async_trait]
    impl L1HeadSource for QueuedL1HeadSource {
        async fn next(&mut self) -> u64 {
            match self.heads.pop_front() {
                Some(head) => head,
                None => std::future::pending().await,
            }
        }
    }

    fn safe_head(number: u64) -> BlockInfo {
        BlockInfo { hash: B256::with_last_byte(number as u8), number, ..Default::default() }
    }

    /// The pipeline starts from the live L1 head, so channel duration is not measured from
    /// block 0.
    #[test]
    fn new_driver_seeds_pipeline_from_live_l1_head() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let (_admin_tx, admin_rx) = mpsc::channel(1);
            let (_status_tx, status_rx) = mpsc::channel(1);

            let _driver = BatchDriver::new(
                ctx,
                TrackingPipeline::new(Arc::clone(&recorded)),
                NeverConfirmTxManager,
                BatchDriverConfig {
                    inbox: Address::ZERO,
                    max_pending_transactions: 1,
                    drain_timeout: Duration::from_millis(10),
                    force_blobs_when_throttling: true,
                    stopped: false,
                },
                DaThrottle::new(ThrottleController::disabled(), Arc::new(NoopThrottleClient)),
                BatchDriverInputs {
                    source: QueuedSource::new([]),
                    l1_head_source: QueuedL1HeadSource::new([]),
                    initial_l1_head: 50,
                    initial_status: DerivationStatus::from_safe_l2(safe_head(10)),
                    derivation_status_rx: status_rx,
                    admin_rx,
                },
            );

            assert_eq!(recorded.lock().unwrap().l1_heads, vec![50]);
        });
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
        BatchSubmission::blobs(
            SubmissionId(id),
            (0..frame_count)
                .map(|number| {
                    BlobPayload::new(vec![Arc::new(Frame {
                        number: number.try_into().expect("frame number fits in u16"),
                        data: vec![0u8; data_len],
                        ..Frame::default()
                    })])
                })
                .collect(),
        )
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

    /// The channels a [`driver_for_next_event`] driver listens to, kept by the test.
    struct EventChannels {
        admin_tx: mpsc::Sender<AdminCommand>,
        status_tx: mpsc::Sender<DerivationStatus>,
    }

    type EventDriver<R, TM> = BatchDriver<
        R,
        TrackingPipeline,
        QueuedSource,
        TM,
        Arc<NoopThrottleClient>,
        QueuedL1HeadSource,
    >;

    /// A driver whose source and L1 head source deliver the given events, then park.
    fn driver_for_next_event<R: base_runtime::Runtime, TM: TxManager>(
        runtime: R,
        source_events: impl IntoIterator<Item = L2BlockEvent>,
        l1_heads: impl IntoIterator<Item = u64>,
        tx_manager: TM,
    ) -> (EventDriver<R, TM>, EventChannels) {
        let (admin_tx, admin_rx) = mpsc::channel(1);
        let (status_tx, status_rx) = mpsc::channel(1);
        let driver = BatchDriver::new(
            runtime,
            TrackingPipeline::new(Arc::new(Mutex::new(Recorded::default()))),
            tx_manager,
            BatchDriverConfig {
                inbox: Address::ZERO,
                max_pending_transactions: 1,
                drain_timeout: Duration::from_millis(10),
                force_blobs_when_throttling: true,
                stopped: false,
            },
            DaThrottle::new(ThrottleController::disabled(), Arc::new(NoopThrottleClient)),
            BatchDriverInputs {
                source: QueuedSource::new(source_events),
                l1_head_source: QueuedL1HeadSource::new(l1_heads),
                initial_l1_head: 0,
                initial_status: DerivationStatus::from_safe_l2(safe_head(0)),
                derivation_status_rx: status_rx,
                admin_rx,
            },
        );
        (driver, EventChannels { admin_tx, status_tx })
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
            let (mut driver, channels) = driver_for_next_event(
                ctx.clone(),
                [L2BlockEvent::Block(Box::default())],
                [9],
                ImmediateConfirmTxManager { l1_block: 1 },
            );
            let (reply, _reply_rx) = oneshot::channel();
            channels
                .admin_tx
                .send(AdminCommand::Flush { reply })
                .await
                .expect("admin receiver should be open");

            ctx.cancel();

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::Shutdown));
        });
    }

    #[test]
    fn next_event_prioritizes_admin_before_source() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (mut driver, channels) = driver_for_next_event(
                ctx,
                [L2BlockEvent::Block(Box::default())],
                [9],
                ImmediateConfirmTxManager { l1_block: 1 },
            );
            let (reply, _reply_rx) = oneshot::channel();
            channels
                .admin_tx
                .send(AdminCommand::Flush { reply })
                .await
                .expect("admin receiver should be open");

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::Flush(_)));
        });
    }

    #[test]
    fn next_event_prioritizes_source_before_receipts_and_heads() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (mut driver, _channels) = driver_for_next_event(
                ctx,
                [L2BlockEvent::Block(Box::default())],
                [9],
                ImmediateConfirmTxManager { l1_block: 1 },
            );
            driver.pipeline.submissions.push_back(SubmissionStub::stub());
            driver.submissions.submit_pending(&mut driver.pipeline).await;

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::Block(_)));
        });
    }

    #[test]
    fn next_event_prioritizes_derivation_status_before_source_and_receipts() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (mut driver, channels) = driver_for_next_event(
                ctx,
                [L2BlockEvent::Block(Box::default())],
                [9],
                ImmediateConfirmTxManager { l1_block: 42 },
            );
            channels
                .status_tx
                .send(DerivationStatus::from_safe_l2(safe_head(5)))
                .await
                .expect("derivation-status receiver should be open");
            driver.pipeline.submissions.push_back(SubmissionStub::stub());
            driver.submissions.submit_pending(&mut driver.pipeline).await;

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(
                event,
                DriverEvent::DerivationStatus(status) if status.safe_l2.number == 5
            ));
        });
    }

    #[test]
    fn next_event_prioritizes_derivation_status_before_l1_head() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (mut driver, channels) =
                driver_for_next_event(ctx, [], [9], ImmediateConfirmTxManager { l1_block: 1 });
            channels
                .status_tx
                .send(DerivationStatus::from_safe_l2(safe_head(5)))
                .await
                .expect("derivation-status receiver should be open");

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(
                event,
                DriverEvent::DerivationStatus(status) if status.safe_l2.number == 5
            ));
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

            let (driver, _handles) = DriverFixture::new(
                ctx.clone(),
                pipeline,
                ImmediateConfirmTxManager { l1_block: 42 },
            )
            .build();
            let handle = ctx.spawn(driver.run());

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

            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, ImmediateFailTxManager).build();
            let handle = ctx.spawn(driver.run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            assert!(
                recorded.lock().unwrap().l1_heads.is_empty(),
                "advance_l1_head must NOT be called on submission failure"
            );
        });
    }

    /// When blob encoding fails, the submission has already been dequeued and its frames marked
    /// pending. Without a requeue those frames never become ready again, so the driver must
    /// requeue the submission before retrying.
    #[test]
    fn test_blob_encoding_failure_requeues_submission() {
        // Blob submission encoding feeds DERIVATION_VERSION_0 (1) + frame.encode()
        // (23 + data.len()) into BlobEncoder::encode. It fails when > BLOB_MAX_DATA_SIZE
        // (130_044), so data.len() >= 130_021 guarantees DataTooLarge.
        const OVERSIZED: usize = 130_021;

        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(BatchSubmission::blobs(
                SubmissionId(0),
                vec![BlobPayload::new(vec![Arc::new(Frame {
                    data: vec![0u8; OVERSIZED],
                    ..Frame::default()
                })])],
            ));

            let (driver, _handles) = DriverFixture::new(
                ctx.clone(),
                pipeline,
                ImmediateConfirmTxManager { l1_block: 1 },
            )
            .build();
            let handle = ctx.spawn(driver.run());

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
    /// pipeline is responsible for choosing the frames that belong in a transaction.
    #[test]
    fn test_submission_loop_submits_each_pipeline_submission_as_one_tx() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let candidates = Arc::new(Mutex::new(Vec::new()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(SubmissionStub::with_id(0));
            pipeline.submissions.push_back(SubmissionStub::with_id(1));

            let (driver, _handles) = DriverFixture::new(
                ctx.clone(),
                pipeline,
                RecordingConfirmTxManager { l1_block: 10, candidates: Arc::clone(&candidates) },
            )
            .max_pending(2)
            .build();
            let handle = ctx.spawn(driver.run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            let recorded = recorded.lock().unwrap();
            assert_eq!(recorded.dequeued.len(), 2, "both submissions must be dequeued");
            assert_eq!(
                recorded.confirmed,
                vec![SubmissionId(0), SubmissionId(1)],
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
    /// `max_blobs_per_tx > 1`. Each frame becomes its own blob in the same L1
    /// transaction.
    #[test]
    fn test_multi_frame_blob_submission_maps_frames_to_blobs() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let candidates = Arc::new(Mutex::new(Vec::new()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            let submission = blob_filling_submission_with_frames(0, 3);
            let SubmissionPayload::Blobs(payloads) = submission.payload() else {
                panic!("helper must create blob payloads");
            };
            let expected_blob_payloads: Vec<_> = payloads
                .iter()
                .map(|payload| FrameEncoder::to_calldata(&payload.frames()[0]))
                .collect();
            pipeline.submissions.push_back(submission);

            let (driver, _handles) = DriverFixture::new(
                ctx.clone(),
                pipeline,
                RecordingConfirmTxManager { l1_block: 10, candidates: Arc::clone(&candidates) },
            )
            .build();
            let handle = ctx.spawn(driver.run());

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

            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, NeverConfirmTxManager)
                    .max_pending(1)
                    .build();
            let handle = ctx.spawn(driver.run());

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

            let (driver, _handles) = DriverFixture::new(
                ctx.clone(),
                pipeline,
                ImmediateConfirmTxManager { l1_block: 7 },
            )
            .max_pending(1)
            .build();
            let handle = ctx.spawn(driver.run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            assert_eq!(
                recorded.lock().unwrap().confirmed,
                vec![SubmissionId(0), SubmissionId(1), SubmissionId(2)],
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

            let (driver, _handles) = DriverFixture::new(ctx.clone(), pipeline, tx_manager).build();
            let handle = ctx.spawn(driver.run());

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
