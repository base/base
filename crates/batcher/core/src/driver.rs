//! The async batch driver that orchestrates encoding, block sourcing, and L1 submission.

use std::time::Duration;

use base_batcher_encoder::{BatchPipeline, BatcherMetrics, DerivationReconciliation, StepResult};
use base_batcher_source::{L1HeadSource, L2BlockEvent, UnsafeBlockSource};
use base_common_consensus::BaseBlock;
use base_protocol::BlockInfo;
use base_runtime::Runtime;
use base_tx_manager::TxManager;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{
    AdminCommand, AdminError, BatchDriverConfig, BatchDriverError, BatcherStatus, DaThrottle,
    DerivationStatus, SubmissionQueue, ThrottleClient, ThrottleController,
};

/// Encoding steps per CPU phase.
const STEP_BUDGET: usize = 128;

/// The sources a [`BatchDriver`] listens to, and the L1 head and safe L2 head it starts from.
#[derive(Debug)]
pub struct BatchDriverInputs<S, L> {
    /// Source of unsafe L2 blocks and reorg signals.
    pub source: S,
    /// Source of live L1 head updates.
    pub l1_head_source: L,
    /// Live L1 head at startup.
    pub initial_l1_head: u64,
    /// Safe L2 head at startup.
    pub initial_safe_head: BlockInfo,
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
/// Uses [`SubmissionQueue`] to send submissions and track their receipts, and
/// [`DaThrottle`] for DA backlog throttle management.
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
    /// Runtime providing cancellation and the shutdown drain timer.
    runtime: R,
    /// The encoding pipeline.
    pipeline: P,
    /// The L2 block source.
    source: S,
    /// Submission lifecycle manager (tx manager, in-flight tracking, txpool state).
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
            safe_head: inputs.initial_safe_head,
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
    /// 1. **CPU phase** (`work`): drain encoding, apply throttle, recover txpool, submit
    ///    ready submissions up to the in-flight limit.
    /// 2. **I/O phase**: block on a biased `tokio::select!` until an event fires, and apply it.
    ///
    /// Every event is therefore followed by a CPU phase before the driver waits again, so the
    /// work an event releases is done before the next one. Encoding is done in slices of
    /// `STEP_BUDGET` steps: when a slice is not enough, the I/O phase yields once instead of
    /// waiting, serves whatever became ready, then the next CPU phase continues encoding. A
    /// large backlog therefore delays no other task by more than a slice, and no event by
    /// more than a CPU phase. The two sources are not polled until the backlog is encoded:
    /// their next block or head would only add to it, and a poll the next slice abandons
    /// would waste an RPC.
    ///
    /// The I/O phase polls its arms in priority order: cancellation, admin commands,
    /// derivation status, L2 blocks, receipts, L1 heads. Admin commands come before the
    /// source so control-plane operations (stop, start, flush) are never starved by sustained
    /// block throughput; derivation-status changes come before unsafe blocks so pruning and
    /// recovery cannot be starved by sequential catchup. A stopped batcher does not poll its
    /// source at all.
    ///
    /// Cancellation ends the loop with a bounded drain of the in-flight submissions; see
    /// `shutdown`.
    pub async fn run(mut self) -> Result<(), BatchDriverError> {
        if self.stopped {
            info!(
                stopped = true,
                "batcher starting in stopped state; call admin_startBatcher to begin submission"
            );
        }

        loop {
            let encoding_left = self.work().await?;

            tokio::select! {
                biased;

                _ = self.runtime.cancelled() => break,

                Some(cmd) = self.admin_rx.recv() => self.on_admin(cmd)?,

                status = self.derivation_status_rx.recv() => match status {
                    Some(status) => self.on_derivation_status(status),
                    None => return Err(BatchDriverError::DerivationStatusSourceClosed),
                },

                event = self.source.next(), if !self.stopped && !encoding_left => match event {
                    L2BlockEvent::Block(block) => self.on_block(block),
                    L2BlockEvent::Reorg => {
                        warn!("L2 reorg detected, resetting pipeline and catching up from safe head");
                        self.reset_to_safe_head(BatcherMetrics::RESET_SOURCE_REORG);
                    }
                },

                Some((id, outcome)) = self.submissions.next_settled() => {
                    self.submissions.handle_outcome(&mut self.pipeline, id, outcome);
                }

                head = self.l1_head_source.next(), if !encoding_left => {
                    self.pipeline.advance_l1_head(head);
                    debug!(l1_head = %head, "L1 head advanced via source");
                }

                // Yield once, so other tasks run and the arms above get another look, then
                // continue encoding.
                () = tokio::task::yield_now(), if encoding_left => {}
            }
        }

        self.shutdown().await
    }

    /// The CPU phase: encode what is buffered, apply the DA throttle, recover the txpool and
    /// submit ready submissions up to the in-flight limit.
    ///
    /// Returns `true` when the encoding step budget ran out, so encoding must continue. Fails
    /// on a fatal encoding error or a blob submission that cannot be built.
    async fn work(&mut self) -> Result<bool, BatchDriverError> {
        let encoding_left = self.drain_encoding()?;

        let is_throttling = self.throttle.apply(self.pipeline.da_backlog_bytes()).await;
        if self.force_blobs_when_throttling {
            self.pipeline.set_blob_override(is_throttling);
        }

        self.submissions.recover_txpool().await;
        self.submissions.submit_pending(&mut self.pipeline).await?;
        Ok(encoding_left)
    }

    /// Flush the current channel, submit what it released, then wait for the in-flight
    /// submissions to settle, up to the drain timeout.
    ///
    /// The drain always runs; an error from the flush or the final CPU phase is reported
    /// afterwards.
    async fn shutdown(mut self) -> Result<(), BatchDriverError> {
        info!(
            in_flight = %self.submissions.in_flight_count(),
            "batcher shutting down, draining in-flight submissions"
        );

        let flushed = self.pipeline.flush().inspect_err(|error| {
            warn!(error = %error, "flush failed during shutdown");
        });
        let worked = self.work().await;

        self.submissions.drain(&mut self.pipeline, self.runtime.sleep(self.drain_timeout)).await;

        flushed?;
        worked?;
        Ok(())
    }

    /// Run up to `STEP_BUDGET` encoding steps.
    ///
    /// Returns `Ok(true)` when the budget ran out before [`StepResult::Idle`], `Err` on a
    /// fatal [`StepError`](base_batcher_encoder::StepError).
    fn drain_encoding(&mut self) -> Result<bool, BatchDriverError> {
        for encoded in 0..STEP_BUDGET {
            match self.pipeline.step() {
                Ok(StepResult::Idle) => {
                    if encoded > 0 {
                        debug!(steps = %encoded, "completed encoding drain");
                    }
                    return Ok(false);
                }
                Ok(StepResult::BlockEncoded | StepResult::ChannelClosed) => {}
                Err(e) => {
                    error!(error = %e, "fatal encoding step error, batcher halting");
                    return Err(e.into());
                }
            }
        }
        debug!(steps = %STEP_BUDGET, "encoding step budget exhausted");
        Ok(true)
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

    /// Apply an admin command and answer it.
    fn on_admin(&mut self, cmd: AdminCommand) -> Result<(), BatchDriverError> {
        match cmd {
            AdminCommand::Flush { reply } if self.stopped => {
                let _ = reply.send(Err(AdminError::Stopped));
            }
            AdminCommand::Flush { reply } => {
                self.pipeline.flush()?;
                let _ = reply.send(Ok(()));
                debug!("admin flush applied, released channel artifacts");
            }
            AdminCommand::Stop { reply } => {
                self.on_admin_stop();
                let _ = reply.send(());
            }
            AdminCommand::Start { reply } => {
                self.on_admin_start();
                let _ = reply.send(());
            }
            AdminCommand::SetThrottle { strategy, config, reply } => {
                self.throttle.set_controller(ThrottleController::new(config, strategy));
                let _ = reply.send(());
                info!("throttle controller replaced via admin");
            }
            AdminCommand::ResetThrottle { reply } => {
                self.throttle.reset();
                let _ = reply.send(());
                info!("throttle controller reset via admin");
            }
            AdminCommand::GetThrottleInfo { reply } => {
                let _ = reply.send(self.throttle.snapshot(self.pipeline.da_backlog_bytes()));
            }
            AdminCommand::GetStatus { reply } => {
                let _ = reply.send(BatcherStatus {
                    stopped: self.stopped,
                    in_flight: self.submissions.in_flight_count(),
                    da_backlog_bytes: self.pipeline.da_backlog_bytes(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_primitives::B256;
    use base_batcher_encoder::{
        BatchSubmission, BlobPayload, FrameEncoder, SubmissionId, SubmissionPayload,
    };
    use base_batcher_source::{
        L2BlockEvent,
        test_utils::{ChannelBlockSource, ChannelL1HeadSource},
    };
    use base_blobs::{BlobDecoder, BlobEncoder};
    use base_protocol::{BlockInfo, Frame};
    use base_runtime::{
        Cancellation, Clock, Spawner,
        deterministic::{Config, Runner},
    };

    use super::STEP_BUDGET;
    use crate::{
        BatchDriverError, DerivationStatus,
        test_utils::{
            BlockStub, DriverFixture, PipelineCall, ScriptedTxManager, SendOutcome, SubmissionStub,
            TrackingPipeline,
        },
    };

    fn safe_head(number: u64) -> BlockInfo {
        BlockInfo { hash: B256::with_last_byte(number as u8), number, ..Default::default() }
    }

    /// Sources that deliver the given blocks and L1 heads, then park.
    fn queued_sources(
        blocks: impl IntoIterator<Item = u64>,
        l1_heads: impl IntoIterator<Item = u64>,
    ) -> (ChannelBlockSource, ChannelL1HeadSource) {
        let (source, source_tx) = ChannelBlockSource::new();
        for number in blocks {
            source_tx.send(L2BlockEvent::Block(Box::new(BlockStub::with_number(number)))).unwrap();
        }
        let (l1_head_source, l1_head_tx) = ChannelL1HeadSource::new();
        for head in l1_heads {
            l1_head_tx.send(head).unwrap();
        }
        (source, l1_head_source)
    }

    /// The pipeline starts from the live L1 head, so channel duration is not measured from
    /// block 0.
    #[test]
    fn new_driver_seeds_pipeline_from_live_l1_head() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();

            let (_driver, _handles) = DriverFixture::new(ctx, pipeline, ScriptedTxManager::new([]))
                .initial_l1_head(50)
                .safe_head(safe_head(10))
                .build();

            assert_eq!(recorded.lock().unwrap().l1_heads(), [50]);
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

    // The loop polls its arms in priority order; each test below makes several arms ready at
    // once and checks which one the driver serves first. The shutdown flush ends every log.

    #[test]
    fn run_prioritizes_cancellation_over_ready_admin() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            let (source, l1_head_source) = queued_sources([1], [9]);
            let (driver, handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                    .source(source)
                    .l1_head_source(l1_head_source)
                    .build();

            // Queue a flush, then cancel before the driver runs.
            let admin = handles.admin.clone();
            let flush = ctx.spawn(async move { admin.flush().await });
            ctx.sleep(Duration::from_millis(1)).await;
            ctx.cancel();

            assert!(driver.run().await.is_ok());
            assert!(flush.await.unwrap().is_err(), "a cancelled driver must not serve the flush");
            assert!(!recorded.lock().unwrap().calls.contains(&PipelineCall::AddBlock(1)));
        });
    }

    #[test]
    fn run_prioritizes_admin_before_source() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            let (source, l1_head_source) = queued_sources([1], []);
            let (driver, handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                    .source(source)
                    .l1_head_source(l1_head_source)
                    .build();

            // Queue a flush before the driver runs.
            let admin = handles.admin.clone();
            let flush = ctx.spawn(async move { admin.flush().await });
            ctx.sleep(Duration::from_millis(1)).await;

            let handle = ctx.spawn(driver.run());
            ctx.sleep(Duration::from_millis(10)).await;
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());
            assert!(flush.await.unwrap().is_ok());

            let recorded = recorded.lock().unwrap();
            assert!(
                recorded.calls.starts_with(&[PipelineCall::Flush, PipelineCall::AddBlock(1)]),
                "{:?}",
                recorded.calls
            );
        });
    }

    #[test]
    fn run_prioritizes_source_before_receipts_and_heads() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            // Submitted by the first CPU phase, so its receipt is ready at the first wait.
            pipeline.submissions.push_back(SubmissionStub::stub());
            let (source, l1_head_source) = queued_sources([1], [9]);
            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                    .source(source)
                    .l1_head_source(l1_head_source)
                    .build();

            let handle = ctx.spawn(driver.run());
            ctx.sleep(Duration::from_millis(10)).await;
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());

            // The receipt confirms at L1 block 1 before the source's head 9 arrives; the other
            // way round, head 1 would not advance past 9.
            let recorded = recorded.lock().unwrap();
            assert!(
                recorded.calls.starts_with(&[
                    PipelineCall::Dequeue(SubmissionId(0)),
                    PipelineCall::AddBlock(1),
                    PipelineCall::Confirm(SubmissionId(0), 1),
                    PipelineCall::AdvanceL1Head(1),
                    PipelineCall::AdvanceL1Head(9),
                ]),
                "{:?}",
                recorded.calls
            );
        });
    }

    #[test]
    fn run_prioritizes_derivation_status_before_source_and_receipts() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            pipeline.submissions.push_back(SubmissionStub::stub());
            let (source, l1_head_source) = queued_sources([6], []);
            let (driver, handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(42))
                    .source(source)
                    .l1_head_source(l1_head_source)
                    .build();
            handles
                .derivation_status_tx
                .send(DerivationStatus::from_safe_l2(safe_head(5)))
                .await
                .unwrap();

            let handle = ctx.spawn(driver.run());
            ctx.sleep(Duration::from_millis(10)).await;
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());

            let recorded = recorded.lock().unwrap();
            assert!(
                recorded.calls.starts_with(&[
                    PipelineCall::Dequeue(SubmissionId(0)),
                    PipelineCall::ReconcileDerivation { safe_l2: 5, current_l1: None },
                    PipelineCall::AddBlock(6),
                    PipelineCall::Confirm(SubmissionId(0), 42),
                    PipelineCall::AdvanceL1Head(42),
                ]),
                "{:?}",
                recorded.calls
            );
        });
    }

    #[test]
    fn run_prioritizes_derivation_status_before_l1_head() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            let (source, l1_head_source) = queued_sources([], [9]);
            let (driver, handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                    .source(source)
                    .l1_head_source(l1_head_source)
                    .build();
            handles
                .derivation_status_tx
                .send(DerivationStatus::from_safe_l2(safe_head(5)))
                .await
                .unwrap();

            let handle = ctx.spawn(driver.run());
            ctx.sleep(Duration::from_millis(10)).await;
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());

            let recorded = recorded.lock().unwrap();
            assert!(
                recorded.calls.starts_with(&[
                    PipelineCall::ReconcileDerivation { safe_l2: 5, current_l1: None },
                    PipelineCall::AdvanceL1Head(9),
                ]),
                "{:?}",
                recorded.calls
            );
        });
    }

    /// A backlog larger than one encoding slice is finished without any external event.
    #[test]
    fn run_finishes_a_backlog_larger_than_one_slice_without_events() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let blocks = 2 * STEP_BUDGET + 5;
            let pipeline = TrackingPipeline::new().with_encoding_steps(blocks);
            let recorded = pipeline.recorded();
            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::new([])).build();

            let handle = ctx.spawn(driver.run());
            ctx.sleep(Duration::from_millis(10)).await;
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());

            assert_eq!(recorded.lock().unwrap().encoded_steps(), blocks);
        });
    }

    /// A ready admin command is served between two encoding slices, not after the whole
    /// backlog.
    #[test]
    fn run_serves_admin_between_encoding_slices() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let pipeline = TrackingPipeline::new().with_encoding_steps(2 * STEP_BUDGET + 5);
            let recorded = pipeline.recorded();
            let (driver, handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::new([])).build();

            let handle = ctx.spawn(driver.run());
            // Stop resets the pipeline, which drops whatever was still to encode.
            handles.admin.stop().await.unwrap();
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());

            assert_eq!(recorded.lock().unwrap().encoded_steps(), STEP_BUDGET);
        });
    }

    /// The driver yields to other tasks between encoding slices: a task can send an admin
    /// command in the middle of a backlog and have it served before the backlog is done.
    #[test]
    fn run_yields_to_other_tasks_between_encoding_slices() {
        let config = Config { cycle_limit: Some(1_000_000), ..Config::seeded(0) };
        Runner::start(config, |ctx| async move {
            let pipeline = TrackingPipeline::new().with_encoding_steps(3 * STEP_BUDGET);
            let recorded = pipeline.recorded();
            let (driver, handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::new([])).build();

            let handle = ctx.spawn(driver.run());
            // Send the stop once the first slice is done: the driver must yield for this task
            // to observe that.
            while recorded.lock().unwrap().encoded_steps() < STEP_BUDGET {
                tokio::task::yield_now().await;
            }
            handles.admin.stop().await.unwrap();
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());

            let encoded = recorded.lock().unwrap().encoded_steps();
            assert!(encoded < 3 * STEP_BUDGET, "the stop must not wait for the whole backlog");
        });
    }

    /// `advance_l1_head` must be called with the confirmed L1 block on every
    /// confirmation so the encoder can detect channel timeouts.
    #[test]
    fn test_advance_l1_head_called_on_confirmation() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            pipeline.submissions.push_back(SubmissionStub::stub());

            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(42))
                    .build();
            let handle = ctx.spawn(driver.run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            assert_eq!(
                recorded.lock().unwrap().l1_heads(),
                [42],
                "advance_l1_head must be called with the confirmed L1 block"
            );
        });
    }

    /// `advance_l1_head` must NOT be called when a submission fails — we have no
    /// confirmed L1 block to report.
    #[test]
    fn test_advance_l1_head_not_called_on_failure() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            pipeline.submissions.push_back(SubmissionStub::stub());

            let (driver, _handles) = DriverFixture::new(
                ctx.clone(),
                pipeline,
                ScriptedTxManager::new([SendOutcome::Failed]),
            )
            .build();
            let handle = ctx.spawn(driver.run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            assert!(
                recorded.lock().unwrap().l1_heads().is_empty(),
                "advance_l1_head must NOT be called on submission failure"
            );
        });
    }

    /// A blob submission that cannot be built into a transaction is fatal: a retry would fail
    /// the same way and hold back every submission behind it.
    #[test]
    fn test_blob_encoding_failure_is_fatal() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            // A frame as large as a whole blob no longer fits once framed.
            pipeline.submissions.push_back(BatchSubmission::blobs(
                SubmissionId(0),
                vec![BlobPayload::new(vec![Arc::new(Frame {
                    data: vec![0u8; BlobEncoder::BLOB_MAX_DATA_SIZE],
                    ..Frame::default()
                })])],
            ));

            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                    .build();

            let result = ctx.spawn(driver.run()).await.unwrap();
            assert!(matches!(result, Err(BatchDriverError::Blob(_))), "got {result:?}");
        });
    }

    /// The submission loop must submit each pipeline submission as one L1 tx. The
    /// pipeline is responsible for choosing the frames that belong in a transaction.
    #[test]
    fn test_submission_loop_submits_each_pipeline_submission_as_one_tx() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            pipeline.submissions.push_back(SubmissionStub::with_id(0));
            pipeline.submissions.push_back(SubmissionStub::with_id(1));
            let tx_manager = ScriptedTxManager::confirming_at(10);

            let (driver, _handles) = DriverFixture::new(ctx.clone(), pipeline, tx_manager.clone())
                .max_pending(2)
                .build();
            let handle = ctx.spawn(driver.run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            let recorded = recorded.lock().unwrap();
            assert_eq!(recorded.dequeued().len(), 2, "both submissions must be dequeued");
            assert_eq!(
                recorded.confirmed(),
                [SubmissionId(0), SubmissionId(1)],
                "each pipeline submission should produce its own confirmation"
            );
            assert_eq!(
                tx_manager.candidates().len(),
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
            let mut pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            let submission = blob_filling_submission_with_frames(0, 3);
            let SubmissionPayload::Blobs(payloads) = submission.payload() else {
                panic!("helper must create blob payloads");
            };
            let expected_blob_payloads: Vec<_> = payloads
                .iter()
                .map(|payload| FrameEncoder::to_calldata(&payload.frames()[0]))
                .collect();
            pipeline.submissions.push_back(submission);
            let tx_manager = ScriptedTxManager::confirming_at(10);

            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, tx_manager.clone()).build();
            let handle = ctx.spawn(driver.run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            let recorded = recorded.lock().unwrap();
            assert_eq!(recorded.dequeued(), [SubmissionId(0)], "submission must be dequeued");
            assert!(
                recorded.requeued().is_empty(),
                "multi-frame blob submission must not be requeued by blob encoding"
            );
            assert_eq!(
                recorded.confirmed(),
                [SubmissionId(0)],
                "multi-frame blob submission should confirm once"
            );
            let candidates = tx_manager.candidates();
            assert_eq!(candidates.len(), 1, "multi-frame submission should use one L1 tx");
            assert!(
                candidates[0].tx_data.is_empty(),
                "blob transactions must not also carry calldata"
            );
            let decoded_blob_payloads: Vec<_> = candidates[0]
                .blobs
                .iter()
                .map(|blob| BlobDecoder::decode(blob).expect("blob payload should decode"))
                .collect();
            assert_eq!(
                decoded_blob_payloads, expected_blob_payloads,
                "each frame in the submission must become its own blob payload"
            );
        });
    }

    /// No more than `max_pending_transactions` L1 txs are in flight. With max=1 and two
    /// submissions, the second submission must not be dequeued while the first tx is pending.
    #[test]
    fn test_in_flight_limit_holds_back_further_submissions() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            pipeline.submissions.push_back(blob_filling_submission(0));
            pipeline.submissions.push_back(blob_filling_submission(1));

            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::new([]))
                    .max_pending(1)
                    .build();
            let handle = ctx.spawn(driver.run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            let recorded = recorded.lock().unwrap();
            assert_eq!(recorded.dequeued(), [SubmissionId(0)], "only one tx may be in flight");
            assert!(recorded.requeued().is_empty(), "a held-back submission is not requeued");
            assert!(recorded.l1_heads().is_empty(), "the tx in flight never confirms");
        });
    }

    /// With `max_pending_transactions`=1 and blob-filling submissions, the second
    /// blob tx is only submitted once the first is confirmed.
    #[test]
    fn test_next_blob_tx_submitted_once_the_previous_settles() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            pipeline.submissions.push_back(blob_filling_submission(0));
            pipeline.submissions.push_back(blob_filling_submission(1));
            pipeline.submissions.push_back(blob_filling_submission(2));

            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(7))
                    .max_pending(1)
                    .build();
            let handle = ctx.spawn(driver.run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            assert_eq!(
                recorded.lock().unwrap().confirmed(),
                [SubmissionId(0), SubmissionId(1), SubmissionId(2)],
                "each queued submission must confirm as the tx before it settles"
            );
        });
    }

    /// `AlreadyReserved` means another transaction owns the sender nonce slot.
    /// The driver must requeue the submission, mark the txpool blocked, and
    /// call `cancel_tx` before accepting more submissions. Once the cancel
    /// succeeds, the requeued submission is sent again.
    #[test]
    fn test_txpool_blocked_requeues_and_attempts_recovery() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            pipeline.submissions.push_back(SubmissionStub::stub());
            let tx_manager = ScriptedTxManager::new([SendOutcome::TxpoolBlocked]);

            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, tx_manager.clone()).build();
            let handle = ctx.spawn(driver.run());

            ctx.sleep(Duration::from_millis(50)).await;
            ctx.cancel();

            assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
            assert_eq!(
                recorded.lock().unwrap().requeued(),
                [SubmissionId(0)],
                "txpool-blocked submissions must be requeued"
            );
            assert_eq!(
                tx_manager.cancellations(),
                1,
                "driver must attempt txpool recovery with cancel_tx"
            );
            assert_eq!(
                tx_manager.candidates().len(),
                2,
                "the requeued submission must be sent again once the txpool is unblocked"
            );
        });
    }
}
