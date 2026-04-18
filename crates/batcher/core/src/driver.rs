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
                Ok(StepResult::BlockEncoded | StepResult::ChannelClosed) => {
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
        sync::{Arc, Mutex},
        time::Duration,
    };

    use base_batcher_encoder::{BatchSubmission, DaType, SubmissionId};
    use base_blobs::BlobEncoder;
    use base_protocol::{ChannelId, Frame};
    use base_runtime::{
        Cancellation, Clock, Spawner,
        deterministic::{Config, Runner},
    };

    use crate::test_utils::{
        DriverFixture, ImmediateConfirmTxManager, ImmediateFailTxManager, NeverConfirmTxManager,
        Recorded, SubmissionStub, TrackingPipeline,
    };

    /// Build a [`BatchSubmission`] whose single frame exactly fills one blob payload,
    /// leaving no room for any additional frame alongside it.
    ///
    /// `payload = 1 (DERIVATION_VERSION_0) + FRAME_OVERHEAD + data.len() = BLOB_MAX_DATA_SIZE`
    fn blob_filling_submission(id: u64) -> BatchSubmission {
        let data_len = BlobEncoder::BLOB_MAX_DATA_SIZE - 1 - BlobEncoder::FRAME_OVERHEAD;
        BatchSubmission {
            id: SubmissionId(id),
            channel_id: ChannelId::default(),
            da_type: DaType::Blob,
            frames: vec![Arc::new(Frame { data: vec![0u8; data_len], ..Frame::default() })],
        }
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
        // encode_packed feeds: DERIVATION_VERSION_0 (1) + frame.encode() (23 + data.len())
        // = 24 + data.len() bytes into BlobEncoder::encode. It fails when > BLOB_MAX_DATA_SIZE
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

    /// The submission loop must pack small frames together. With `max_pending_transactions`=2
    /// and two tiny frames ready, both must be packed into a single blob and confirmed in
    /// one L1 transaction — not submitted as two separate transactions.
    #[test]
    fn test_submission_loop_packs_multiple_frames_into_one_blob() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(SubmissionStub::with_id(0));
            pipeline.submissions.push_back(SubmissionStub::with_id(1));

            let handle = ctx.spawn(
                DriverFixture::build_with_max_pending(
                    ctx.clone(),
                    pipeline,
                    ImmediateConfirmTxManager { l1_block: 10 },
                    2,
                )
                .run(),
            );

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            let recorded = recorded.lock().unwrap();
            assert_eq!(recorded.dequeued.len(), 2, "both submissions must be dequeued");
            // Both tiny frames fit in one blob → one L1 tx → one advance_l1_head call.
            assert_eq!(
                recorded.l1_heads,
                vec![10],
                "both frames packed into one blob; one confirmation"
            );
        });
    }

    /// The semaphore must prevent more concurrent in-flight L1 txs than
    /// `max_pending_transactions`. With max=1 and two blob-filling submissions
    /// (each requiring its own tx), the second submission is peeked and requeued
    /// (no room in the full blob), and the semaphore blocks any further tx attempt.
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
            // sub(0) fills the blob; sub(1) is peeked, doesn't fit, and is requeued.
            assert_eq!(
                recorded.requeued,
                vec![SubmissionId(1)],
                "second submission requeued: blob full"
            );
            // The semaphore (max=1) is occupied by blob 1 — no second tx was submitted.
            assert!(recorded.l1_heads.is_empty(), "no confirmation while semaphore is full");
        });
    }

    /// With `max_pending_transactions`=1 and blob-filling submissions, the second
    /// blob tx is only submitted once the first is confirmed (freeing the permit).
    /// Uses 3 submissions so sub(1) (requeued from blob 1) gives way to sub(2) for blob 2.
    #[test]
    fn test_second_blob_tx_submitted_after_permit_freed() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            // sub(0) fills blob 1; sub(1) is peeked and requeued (doesn't fit);
            // sub(2) is available as the first candidate for blob 2.
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
                vec![7, 7],
                "blob 2 must be confirmed once blob 1 frees the permit"
            );
        });
    }
}
