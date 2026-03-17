//! The async batch driver that orchestrates encoding, block sourcing, and L1 submission.

use std::time::Duration;

use base_alloy_consensus::OpBlock;
use base_batcher_encoder::{BatchPipeline, StepResult};
use base_batcher_source::{
    L1HeadEvent, L1HeadSource, L2BlockEvent, SourceError, UnsafeBlockSource,
};
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
    /// Whether block ingestion is currently paused via the admin API.
    paused: bool,
    /// Admin command channel, wired in via [`Self::with_admin_rx`].
    admin_rx: Option<mpsc::Receiver<AdminCommand>>,
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
            paused: false,
            admin_rx: None,
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

    /// Run the batch driver loop.
    ///
    /// Each iteration has two phases:
    /// 1. **CPU phase**: drain encoding, apply throttle, recover txpool, submit pending frames.
    /// 2. **I/O phase**: block on `tokio::select!` until one external event fires.
    ///
    /// When draining (after cancellation or source exhaustion), the I/O phase is
    /// replaced by a bounded drain of all in-flight receipts.
    pub async fn run(mut self) -> Result<(), BatchDriverError> {
        let mut draining = false;
        loop {
            self.drain_encoding()?;
            self.throttle.apply(self.pipeline.da_backlog_bytes()).await;
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
    /// Admin commands are handled inline in the loop — only non-admin events
    /// are returned to the caller. When paused via [`AdminCommand::Pause`],
    /// the source arm still fires but `Block`, `Flush`, and `Reorg` events are
    /// silently discarded; source errors and exhaustion are still propagated.
    ///
    /// Non-fatal L1 head source errors loop internally to avoid polluting the
    /// return type with a no-op variant.
    async fn next_event(&mut self) -> Result<DriverEvent, BatchDriverError> {
        loop {
            let event = tokio::select! {
                biased;

                _ = self.runtime.cancelled() => DriverEvent::Shutdown,

                event = self.source.next() => match event {
                    Ok(L2BlockEvent::Block(_) | L2BlockEvent::Flush | L2BlockEvent::Reorg { .. })
                        if self.paused =>
                    {
                        continue;
                    }
                    Ok(L2BlockEvent::Block(block)) => DriverEvent::Block(block),
                    Ok(L2BlockEvent::Flush) => DriverEvent::Flush,
                    Ok(L2BlockEvent::Reorg { new_safe_head }) => DriverEvent::Reorg(new_safe_head),
                    Err(SourceError::Exhausted) => DriverEvent::Shutdown,
                    Err(e) => return Err(e.into()),
                },

                cmd = Self::next_admin_cmd(&mut self.admin_rx) => {
                    match cmd {
                        AdminCommand::Flush => return Ok(DriverEvent::Flush),
                        AdminCommand::Pause => {
                            self.paused = true;
                            info!(paused = true, "batcher paused via admin");
                        }
                        AdminCommand::Resume => {
                            self.paused = false;
                            info!(paused = false, "batcher resumed via admin");
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
                                paused: self.paused,
                                in_flight: self.submissions.in_flight_count(),
                                da_backlog_bytes: self.pipeline.da_backlog_bytes(),
                            });
                        }
                    }
                    // All commands except Flush loop to await the next real event.
                    continue;
                }

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
    use base_protocol::{ChannelId, Frame};
    use base_runtime::{
        Cancellation, Clock, Spawner,
        deterministic::{Config, Runner},
    };

    use crate::test_utils::{
        DriverFixture, ImmediateConfirmTxManager, ImmediateFailTxManager, NeverConfirmTxManager,
        Recorded, SubmissionStub, TrackingPipeline,
    };

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
        // encode_frames feeds: DERIVATION_VERSION_0 (1) + frame.encode() (23 + data.len())
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

    /// The submission loop must drain all ready frames in a single pass when
    /// permits allow. With `max_pending_transactions`=2 and two frames ready,
    /// both must be submitted and confirmed without waiting for an I/O event
    /// between them.
    #[test]
    fn test_submission_loop_drains_multiple_frames_concurrently() {
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
            assert_eq!(recorded.l1_heads.len(), 2, "both submissions must be confirmed");
        });
    }

    /// The semaphore must prevent more concurrent in-flight submissions than
    /// `max_pending_transactions`. With max=1 and a tx manager that never
    /// confirms, exactly one submission must be dequeued; the second must not
    /// be dequeued because `try_acquire_owned` fails when the slot is occupied.
    #[test]
    fn test_semaphore_prevents_excess_concurrent_submissions() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(SubmissionStub::with_id(0));
            pipeline.submissions.push_back(SubmissionStub::with_id(1));

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
            assert_eq!(
                recorded.lock().unwrap().dequeued,
                vec![SubmissionId(0)],
                "only the first submission must be dequeued when the semaphore slot is occupied"
            );
        });
    }

    /// With `max_pending_transactions`=1, the second submission must only be
    /// dequeued and confirmed after the first is confirmed (freeing the permit).
    /// Both must ultimately be confirmed.
    #[test]
    fn test_second_submission_sent_after_permit_freed() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let recorded = Arc::new(Mutex::new(Recorded::default()));
            let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
            pipeline.submissions.push_back(SubmissionStub::with_id(0));
            pipeline.submissions.push_back(SubmissionStub::with_id(1));

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
            let recorded = recorded.lock().unwrap();
            assert_eq!(recorded.dequeued.len(), 2, "both submissions must eventually be dequeued");
            assert_eq!(
                recorded.l1_heads,
                vec![7, 7],
                "both submissions must be confirmed once the permit is freed between them"
            );
        });
    }
}
