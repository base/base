//! The async batch driver that orchestrates encoding, block sourcing, and L1 submission.

use std::time::Duration;

use base_batcher_encoder::{BatchPipeline, DerivationReconciliation, StepError, StepResult};
use base_batcher_source::{
    L1HeadEvent, L1HeadSource, L2BlockEvent, SourceError, UnsafeBlockSource,
};
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use base_runtime::Runtime;
use base_tx_manager::TxManager;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::{
    AdminCommand, BatchDriverConfig, BatchDriverError, BatcherStatus, DaThrottle, DerivationStatus,
    SubmissionQueue, ThrottleClient, ThrottleController, event::DriverEvent,
};

/// Live L1 and derivation inputs consumed when constructing a [`BatchDriver`].
#[derive(Debug)]
pub struct BatchDriverHeads<L> {
    l1_head_source: L,
    initial_l1_head: Option<u64>,
    derivation_feed: Option<(DerivationStatus, mpsc::Receiver<DerivationStatus>)>,
}

impl<L> BatchDriverHeads<L> {
    /// Production inputs: L1 head source, current L1 tip, and derivation status.
    pub const fn new(
        l1_head_source: L,
        initial_l1_head: u64,
        initial_status: DerivationStatus,
        derivation_status_rx: mpsc::Receiver<DerivationStatus>,
    ) -> Self {
        Self {
            l1_head_source,
            initial_l1_head: Some(initial_l1_head),
            derivation_feed: Some((initial_status, derivation_status_rx)),
        }
    }

    /// Head inputs without derivation tracking or an initial L1 tip, for tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub const fn without_derivation(l1_head_source: L) -> Self {
        Self { l1_head_source, initial_l1_head: None, derivation_feed: None }
    }
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
    ///
    /// Set to `None` after the source returns [`SourceError::Exhausted`] or
    /// [`SourceError::Closed`], causing the driver to park that select arm forever.
    l1_head_source: Option<L>,
    /// Last trusted L2 safe head.
    safe_head: Option<BlockInfo>,
    /// Ordered derivation-progress snapshots.
    derivation_status_rx: Option<mpsc::Receiver<DerivationStatus>>,
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
    /// Acknowledgements for in-progress flushes, fired once encoding and submission both
    /// report no further ready work for the current channel (see [`Self::run`]).
    pending_flush_acks: Vec<oneshot::Sender<()>>,
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

    /// Create a [`BatchDriver`] from live L1 and derivation inputs.
    ///
    /// Advances the pipeline to the initial L1 tip before the event loop starts
    /// so channel duration is measured from that tip, not from block 0.
    pub fn new(
        runtime: R,
        mut pipeline: P,
        source: S,
        tx_manager: TM,
        config: BatchDriverConfig,
        throttle: DaThrottle<TC>,
        heads: BatchDriverHeads<L>,
    ) -> Self {
        let (initial_status, derivation_status_rx) = heads.derivation_feed.unzip();
        if let Some(initial_l1_head) = heads.initial_l1_head {
            pipeline.advance_l1_head(initial_l1_head);
        }
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
            l1_head_source: Some(heads.l1_head_source),
            safe_head: initial_status.map(|status| status.safe_l2),
            derivation_status_rx,
            drain_timeout: config.drain_timeout,
            stopped: false,
            admin_rx: None,
            force_blobs_when_throttling: config.force_blobs_when_throttling,
            pending_flush_acks: Vec::new(),
        }
    }

    /// Create a driver without derivation-status tracking for tests that do not exercise it.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn new_without_derivation_status(
        runtime: R,
        pipeline: P,
        source: S,
        tx_manager: TM,
        config: BatchDriverConfig,
        throttle: DaThrottle<TC>,
        l1_head_source: L,
    ) -> Self {
        Self::new(
            runtime,
            pipeline,
            source,
            tx_manager,
            config,
            throttle,
            BatchDriverHeads::without_derivation(l1_head_source),
        )
    }

    /// Attach a derivation-status feed to a test driver created without one.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_derivation_status_rx(
        mut self,
        initial: DerivationStatus,
        rx: mpsc::Receiver<DerivationStatus>,
    ) -> Self {
        self.safe_head = Some(initial.safe_l2);
        self.derivation_status_rx = Some(rx);
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
    /// When shutting down (after cancellation or source exhaustion), the I/O phase is
    /// replaced by a bounded drain of all in-flight receipts.
    ///
    /// If a [`DriverEvent::Flush`] carried an acknowledgement, it fires as soon as a later
    /// CPU phase reports both encoding and submission fully drained (i.e. the flush's frames
    /// have all been handed to the tx manager) — see the `pending_flush_acks` field.
    ///
    /// This "fully drained" check is global, not scoped to the triggering flush: it's the
    /// weakest condition that's still always *sufficient* (the flush's own frames can never be
    /// dequeued before this fires) but not *tight* — if further `Block` events keep arriving
    /// and producing fresh encoding/submission work while the ack is outstanding, it's delayed
    /// until that work drains too, and under sustained continuous ingestion may not fire at
    /// all. Callers that need a precise, always-terminating signal must ensure the source is
    /// otherwise quiesced before flushing (as the action-test harness does).
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

            let encoding_idle = self.drain_encoding()?;
            let is_throttling = self.throttle.apply(self.pipeline.da_backlog_bytes()).await;
            if self.force_blobs_when_throttling {
                self.pipeline.set_blob_override(is_throttling);
            }
            self.submissions.recover_txpool().await;
            let submissions_idle = self.submissions.submit_pending(&mut self.pipeline).await;

            if encoding_idle && submissions_idle && !self.pending_flush_acks.is_empty() {
                debug!(
                    acks = %self.pending_flush_acks.len(),
                    "flush settled: encoding and submission fully drained"
                );
                for ack in self.pending_flush_acks.drain(..) {
                    let _ = ack.send(());
                }
            }

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
                DriverEvent::Flush(ack) => {
                    self.pipeline.flush()?;
                    if let Some(ack) = ack {
                        self.pending_flush_acks.push(ack);
                    }
                    debug!("flush signal received, closed channel");
                }
                DriverEvent::Reorg => {
                    warn!("L2 reorg detected, resetting pipeline and catching up from safe head");
                    self.reset_to_safe_head();
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
                DriverEvent::L1SourceClosed => {
                    debug!("L1 head source closed, disabling arm");
                    self.l1_head_source = None;
                }
            }
        }
    }

    /// Drain encoding steps synchronously up to [`Self::STEP_BUDGET`].
    ///
    /// Returns `Ok(true)` if the pipeline reached [`StepResult::Idle`] (nothing left to
    /// encode), or `Ok(false)` if the step budget ran out first. Returns `Err` on a fatal
    /// [`StepError`](base_batcher_encoder::StepError).
    fn drain_encoding(&mut self) -> Result<bool, BatchDriverError> {
        let mut budget = Self::STEP_BUDGET;
        let mut steps = 0usize;
        let idle = loop {
            match self.pipeline.step() {
                Ok(StepResult::Idle) => break true,
                Ok(StepResult::BlockEncoded | StepResult::ChannelClosed) => {
                    steps += 1;
                    budget -= 1;
                    if budget == 0 {
                        debug!(steps = %steps, "encoding step budget exhausted, yielding");
                        break false;
                    }
                }
                Err(e) => {
                    error!(error = %e, "fatal encoding step error, batcher halting");
                    return Err(e.into());
                }
            }
        };
        if steps > 0 {
            debug!(steps = %steps, "completed encoding drain");
        }
        Ok(idle)
    }

    /// Reset volatile state and restart delivery above the latest safe head.
    fn reset_to_safe_head(&mut self) {
        self.submissions.discard();
        self.pipeline.reset();

        if let Some(safe_head) = self.safe_head {
            self.source.reset_catchup(safe_head);
        }

        self.discard_pending_flush_acks();
    }

    /// Reconcile buffered state with an ordered derivation-progress snapshot.
    fn on_derivation_status(&mut self, status: DerivationStatus) {
        let head = status.safe_l2;
        let previous = self.safe_head.replace(head);

        if let Some(previous) = previous.filter(|previous| {
            head.number < previous.number
                || (head.number == previous.number && head.hash != previous.hash)
        }) {
            warn!(
                previous_safe_l2 = %previous.number,
                previous_safe_hash = %previous.hash,
                safe_l2 = %head.number,
                safe_hash = %head.hash,
                "safe L2 head changed chain, resetting pipeline"
            );
            self.reset_to_safe_head();
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
                self.reset_to_safe_head();
            }
            DerivationReconciliation::StalledChannel => {
                warn!(
                    current_l1 = ?status.current_l1.map(|current_l1| current_l1.number),
                    safe_l2 = %head.number,
                    "rollup node passed a fully confirmed channel without deriving it, resetting pipeline"
                );
                self.reset_to_safe_head();
            }
        }
    }

    /// Apply derivation-status updates that arrived before the next CPU phase.
    fn apply_pending_derivation_status_updates(&mut self) -> Result<(), BatchDriverError> {
        loop {
            let Some(rx) = self.derivation_status_rx.as_mut() else {
                return Ok(());
            };

            match rx.try_recv() {
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
    /// discards in-flight submissions, resets the pipeline, and restarts
    /// sequential catchup from `safe_head + 1`. The triggering block will be
    /// re-delivered by the sequential poller.
    fn on_block(&mut self, block: Box<BaseBlock>) {
        let number = block.header.number;
        if self.safe_head.is_some_and(|safe_head| number <= safe_head.number) {
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
                self.reset_to_safe_head();
            }
        }
    }

    /// Drop any outstanding flush acknowledgements without firing them.
    ///
    /// Called whenever the pipeline is reset: the blocks a pending flush was waiting on no
    /// longer exist, so firing the ack would falsely report settlement. Dropping the sender
    /// surfaces as a closed-channel error to the waiter.
    fn discard_pending_flush_acks(&mut self) {
        self.pending_flush_acks.clear();
    }

    /// Block on the next external event using a biased `tokio::select!`.
    ///
    /// Admin commands are handled inline in the loop — only non-admin events
    /// are returned to the caller. Admin commands are placed before the source
    /// arm so control-plane operations (pause, resume, flush) are never starved
    /// by sustained block throughput.
    /// Derivation-status changes are also handled before unsafe blocks so pruning and
    /// recovery cannot be starved by sequential catchup.
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
                        AdminCommand::Flush { ack } => return Ok(DriverEvent::Flush(ack)),
                        AdminCommand::Pause => {
                            self.submissions.discard();
                            self.pipeline.reset();
                            self.stopped = true;
                            self.discard_pending_flush_acks();
                            info!(stopped = true, "batcher paused via admin");
                        }
                        AdminCommand::Resume => {
                            if let Some(safe_head) = self.safe_head {
                                self.source.reset_catchup(safe_head);
                                info!(
                                    stopped = false,
                                    safe_l2 = %safe_head.number,
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

                derivation_status = async {
                    if let Some(ref mut rx) = self.derivation_status_rx {
                        rx.recv().await
                    } else {
                        std::future::pending::<Option<DerivationStatus>>().await
                    }
                } => {
                    match derivation_status {
                        Some(status) => DriverEvent::DerivationStatus(status),
                        None => return Err(BatchDriverError::DerivationStatusSourceClosed),
                    }
                }

                event = self.source.next() => match event {
                    Ok(L2BlockEvent::Block(_)) if self.stopped => {
                        continue;
                    }
                    Ok(L2BlockEvent::Flush { ack }) if self.stopped => {
                        // Drop (rather than fire) any ack: the batcher is paused, so this
                        // flush produces no frames and firing would falsely report
                        // settlement. The waiter observes a closed-channel error instead of
                        // a silent, indefinite-looking drop.
                        if ack.is_some() {
                            debug!("flush ack dropped: batcher is stopped, flush produces no frames");
                        }
                        continue;
                    }
                    Ok(L2BlockEvent::Block(block)) => DriverEvent::Block(block),
                    Ok(L2BlockEvent::Flush { ack }) => DriverEvent::Flush(ack),
                    Ok(L2BlockEvent::Reorg) => DriverEvent::Reorg,
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
    use base_batcher_encoder::{
        BatchSubmission, BlobPayload, FrameEncoder, SubmissionId, SubmissionPayload,
    };
    use base_batcher_source::{
        L1HeadEvent, L1HeadSource, L2BlockEvent, SourceError, UnsafeBlockSource,
    };
    use base_blobs::{BlobDecoder, BlobEncoder};
    use base_protocol::{BlockInfo, Frame};
    use base_runtime::{
        Cancellation, Clock, Spawner,
        deterministic::{Config, Runner},
    };
    use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError};
    use tokio::sync::{mpsc, oneshot};

    use crate::{
        AdminCommand, BatchDriver, BatchDriverConfig, BatchDriverHeads, DaThrottle,
        DerivationStatus, NoopThrottleClient, ThrottleController,
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

        fn reset_catchup(&mut self, _: BlockInfo) {}
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

    fn safe_head(number: u64) -> BlockInfo {
        BlockInfo { hash: B256::with_last_byte(number as u8), number, ..Default::default() }
    }

    #[test]
    fn new_driver_seeds_pipeline_from_live_l1_head() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            let (_status_tx, status_rx) = mpsc::channel(1);

            let _driver = BatchDriver::new(
                ctx,
                pipeline,
                QueuedSource::new(std::iter::empty()),
                NeverConfirmTxManager,
                BatchDriverConfig {
                    inbox: Address::ZERO,
                    max_pending_transactions: 1,
                    drain_timeout: Duration::from_millis(10),
                    force_blobs_when_throttling: true,
                },
                DaThrottle::new(ThrottleController::noop(), Arc::new(NoopThrottleClient)),
                BatchDriverHeads::new(
                    QueuedL1HeadSource::new(std::iter::empty()),
                    50,
                    DerivationStatus::from_safe_l2(safe_head(10)),
                    status_rx,
                ),
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
        BatchDriver::new_without_derivation_status(
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
            admin_tx
                .send(AdminCommand::Flush { ack: None })
                .await
                .expect("admin receiver should be open");

            let mut driver = driver_for_next_event(
                ctx.clone(),
                [Ok(L2BlockEvent::Flush { ack: None })],
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
            admin_tx
                .send(AdminCommand::Flush { ack: None })
                .await
                .expect("admin receiver should be open");

            let mut driver = driver_for_next_event(
                ctx,
                [Ok(L2BlockEvent::Block(Box::default()))],
                [Ok(L1HeadEvent::NewHead(9))],
                ImmediateConfirmTxManager { l1_block: 1 },
            )
            .with_admin_rx(admin_rx);

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::Flush(_)));
        });
    }

    #[test]
    fn next_event_prioritizes_source_before_receipts_and_heads() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (_status_tx, status_rx) = mpsc::channel(1);
            let mut driver = driver_for_next_event(
                ctx,
                [Ok(L2BlockEvent::Flush { ack: None })],
                [Ok(L1HeadEvent::NewHead(9))],
                ImmediateConfirmTxManager { l1_block: 1 },
            )
            .with_derivation_status_rx(DerivationStatus::from_safe_l2(safe_head(0)), status_rx);
            driver.pipeline.submissions.push_back(SubmissionStub::stub());
            driver.submissions.submit_pending(&mut driver.pipeline).await;

            let event = driver.next_event().await.expect("next_event should succeed");
            assert!(matches!(event, DriverEvent::Flush(_)));
        });
    }

    #[test]
    fn next_event_prioritizes_derivation_status_before_source_and_receipts() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (status_tx, status_rx) = mpsc::channel(1);
            status_tx
                .send(DerivationStatus::from_safe_l2(safe_head(5)))
                .await
                .expect("derivation-status receiver should be open");

            let mut driver = driver_for_next_event(
                ctx,
                [Ok(L2BlockEvent::Flush { ack: None })],
                [Ok(L1HeadEvent::NewHead(9))],
                ImmediateConfirmTxManager { l1_block: 42 },
            )
            .with_derivation_status_rx(DerivationStatus::from_safe_l2(safe_head(0)), status_rx);
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
            let (status_tx, status_rx) = mpsc::channel(1);
            status_tx
                .send(DerivationStatus::from_safe_l2(safe_head(5)))
                .await
                .expect("derivation-status receiver should be open");

            let mut driver = driver_for_next_event(
                ctx,
                [],
                [Ok(L1HeadEvent::NewHead(9))],
                ImmediateConfirmTxManager { l1_block: 1 },
            )
            .with_derivation_status_rx(DerivationStatus::from_safe_l2(safe_head(0)), status_rx);

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
    /// pipeline is responsible for choosing the frames that belong in a transaction.
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
    /// `target_num_frames > 1`. Each frame becomes its own blob in the same L1
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

    /// A flush acknowledgement must not fire until every ready submission has been dequeued
    /// and handed to the tx manager — not just the first. Regression test for a race where a
    /// caller could observe the ack after only the first frame of a multi-frame flush was
    /// queued, then mine an L1 block missing the later frames.
    #[test]
    fn test_flush_ack_waits_for_all_ready_submissions() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(SubmissionStub::with_id(0));
            pipeline.submissions.push_back(SubmissionStub::with_id(1));

            let (admin_tx, admin_rx) = mpsc::channel(1);
            let (ack_tx, ack_rx) = oneshot::channel();
            admin_tx
                .send(AdminCommand::Flush { ack: Some(ack_tx) })
                .await
                .expect("admin receiver should be open");

            let handle = ctx.spawn(
                DriverFixture::build_with_max_pending(
                    ctx.clone(),
                    pipeline,
                    ImmediateConfirmTxManager { l1_block: 1 },
                    2,
                )
                .with_admin_rx(admin_rx)
                .run(),
            );

            ack_rx.await.expect("flush ack must fire");
            assert_eq!(
                recorded.lock().unwrap().dequeued.len(),
                2,
                "ack must not fire until both ready submissions are dequeued"
            );

            ctx.cancel();
            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        });
    }

    /// If a ready submission can't fit within semaphore capacity, the flush ack must not
    /// fire — firing early would let a caller believe the flush fully settled before every
    /// frame was actually handed to the tx manager.
    #[test]
    fn test_flush_ack_does_not_fire_while_backpressured() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(SubmissionStub::with_id(0));
            pipeline.submissions.push_back(SubmissionStub::with_id(1));

            let (admin_tx, admin_rx) = mpsc::channel(1);
            let (ack_tx, mut ack_rx) = oneshot::channel();
            admin_tx
                .send(AdminCommand::Flush { ack: Some(ack_tx) })
                .await
                .expect("admin receiver should be open");

            let handle = ctx.spawn(
                DriverFixture::build_with_max_pending(
                    ctx.clone(),
                    pipeline,
                    NeverConfirmTxManager,
                    1,
                )
                .with_admin_rx(admin_rx)
                .run(),
            );

            ctx.sleep(Duration::from_millis(50)).await;
            assert!(
                ack_rx.try_recv().is_err(),
                "ack must not fire while a ready submission is still waiting on semaphore capacity"
            );
            assert_eq!(
                recorded.lock().unwrap().dequeued.len(),
                1,
                "only the single available permit should have been used"
            );

            ctx.cancel();
            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        });
    }
}
