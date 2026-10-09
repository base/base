//! The async batch driver that orchestrates encoding, block sourcing, and L1 submission.

use std::time::Duration;

use base_batcher_encoder::{BatchPipeline, BatcherMetrics, DerivationReconciliation, StepResult};
use base_batcher_source::{L1HeadSource, L2BlockEvent, UnsafeBlockSource};
use base_common_consensus::BaseBlock;
use base_runtime::Runtime;
use base_tx_manager::TxManager;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{
    AdminCommand, AdminError, BatchDriverConfig, BatchDriverError, BatcherStatus, DaThrottle,
    DerivationStatus, SubmissionQueue, ThrottleController,
};

/// Encoding steps per CPU phase.
const STEP_BUDGET: usize = 128;

/// The sources a [`BatchDriver`] listens to, and the L1 head and derivation status it starts
/// from.
#[derive(Debug)]
pub struct BatchDriverInputs<S, L> {
    /// Source of unsafe L2 blocks and reorg signals.
    pub source: S,
    /// Source of live L1 head updates.
    pub l1_head_source: L,
    /// Live L1 head at startup.
    pub initial_l1_head: u64,
    /// Derivation status at startup.
    pub initial_derivation_status: DerivationStatus,
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
pub struct BatchDriver<R, P, S, TM, L>
where
    R: Runtime,
    P: BatchPipeline,
    S: UnsafeBlockSource,
    TM: TxManager,
    L: L1HeadSource,
{
    /// Runtime providing cancellation and the shutdown drain timer.
    runtime: R,
    /// The encoding pipeline.
    pipeline: P,
    /// The L2 block source.
    source: S,
    /// Submission lifecycle manager (tx manager, in-flight tracking).
    submissions: SubmissionQueue<TM>,
    /// DA backlog throttle, which publishes the limits the block builders apply.
    throttle: DaThrottle,
    /// L1 head source for chain head advancement.
    l1_head_source: L,
    /// The last derivation status acted on. Blocks at or below its safe head are dropped as
    /// derived and catchup restarts above it; a lower safe head is judged against its L1 block.
    derivation: DerivationStatus,
    /// Ordered derivation statuses.
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

impl<R, P, S, TM, L> BatchDriver<R, P, S, TM, L>
where
    R: Runtime,
    P: BatchPipeline,
    S: UnsafeBlockSource,
    TM: TxManager,
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
        throttle: DaThrottle,
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
            derivation: inputs.initial_derivation_status,
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
    /// 1. **CPU phase** (`work`): drain encoding, apply throttle, submit ready submissions up
    ///    to the in-flight limit.
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
    /// derivation status, receipts, L2 blocks, L1 heads. Admin commands come before the
    /// source so control-plane operations (stop, start, flush) are never starved by sustained
    /// block throughput. Derivation-status changes come before unsafe blocks so pruning and
    /// recovery cannot be starved by sequential catchup. Receipts come before unsafe blocks so
    /// a failed submission is resent before anything a block ready at the same wait releases. A
    /// stopped batcher does not poll its source at all.
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

                Some((id, outcome)) = self.submissions.next_settled() => {
                    self.submissions.handle_outcome(&mut self.pipeline, id, outcome);
                }

                event = self.source.next(), if !self.stopped && !encoding_left => match event {
                    L2BlockEvent::Block(block) => self.on_block(block),
                    L2BlockEvent::Reorg => {
                        warn!("L2 reorg detected, resetting pipeline and catching up from safe head");
                        self.reset_to_safe_head(BatcherMetrics::RESET_SOURCE_REORG);
                    }
                },

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

    /// The CPU phase: encode what is buffered, apply the DA throttle and submit ready
    /// submissions up to the in-flight limit.
    ///
    /// Returns `true` when the encoding step budget ran out, so encoding must continue. Fails
    /// on a fatal encoding error or a blob submission that cannot be built.
    async fn work(&mut self) -> Result<bool, BatchDriverError> {
        let encoding_left = self.drain_encoding()?;

        let is_throttling = self.throttle.publish_limits(self.pipeline.da_backlog_bytes());
        if self.force_blobs_when_throttling {
            self.pipeline.set_blob_override(is_throttling);
        }

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

    /// Reset volatile state and restart delivery above the safe head.
    fn reset_to_safe_head(&mut self, reason: &'static str) {
        self.reset_pipeline(reason);
        self.source.reset_catchup(self.derivation.safe_l2);
    }

    /// Reconcile buffered state with a derivation status.
    ///
    /// A safe head lower than the last one acted on means one of two things. Either the node is
    /// behind on L1, as a new leader or a restarted node is, and derives the same blocks again:
    /// while it has not read L1 past the block the last safe head was reported at, the status
    /// is ignored. Or L1 lost the data that made the last safe head safe:
    /// once the node has read past that block and its safe head is still lower, the driver
    /// resets the pipeline and posts the blocks above the lower safe head again.
    fn on_derivation_status(&mut self, status: DerivationStatus) {
        let last = self.derivation;
        let went_back = status.safe_l2.number < last.safe_l2.number;
        if went_back && status.current_l1.number <= last.current_l1.number {
            debug!(
                safe_l2 = %status.safe_l2.number,
                current_l1 = %status.current_l1.number,
                last_safe_l2 = %last.safe_l2.number,
                last_current_l1 = %last.current_l1.number,
                "rollup node behind on L1, ignoring its lower safe head"
            );
            return;
        }
        self.derivation = status;

        if went_back {
            warn!(
                safe_l2 = %status.safe_l2.number,
                current_l1 = %status.current_l1.number,
                last_safe_l2 = %last.safe_l2.number,
                last_current_l1 = %last.current_l1.number,
                "safe L2 head went back with derivation past the last one's L1 block, resetting pipeline"
            );
            self.reset_to_safe_head(BatcherMetrics::RESET_SAFE_HEAD_REORG);
            return;
        }

        match self.pipeline.reconcile_derivation(status.safe_l2, status.current_l1.number) {
            DerivationReconciliation::Consistent => {}
            DerivationReconciliation::SafeHeadMismatch => {
                warn!(
                    safe_l2 = %status.safe_l2.number,
                    safe_hash = %status.safe_l2.hash,
                    "safe L2 head does not match buffered chain, resetting pipeline"
                );
                self.reset_to_safe_head(BatcherMetrics::RESET_SAFE_HEAD_MISMATCH);
            }
            DerivationReconciliation::StalledChannel => {
                warn!(
                    current_l1 = %status.current_l1.number,
                    safe_l2 = %status.safe_l2.number,
                    "rollup node passed a fully confirmed channel without deriving it, resetting pipeline"
                );
                self.reset_to_safe_head(BatcherMetrics::RESET_STALLED_CHANNEL);
            }
        }
    }

    /// Ingest a new L2 block into the pipeline.
    ///
    /// If the pipeline signals a reorg via `add_block` (parent-hash mismatch),
    /// resets the pipeline and restarts sequential catchup above the safe head.
    /// The triggering block will be re-delivered by the sequential poller.
    fn on_block(&mut self, block: Box<BaseBlock>) {
        let number = block.header.number;
        if number <= self.derivation.safe_l2.number {
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

        self.source.reset_catchup(self.derivation.safe_l2);
        info!(
            stopped = false,
            safe_l2 = %self.derivation.safe_l2.number,
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

    use base_batcher_encoder::{
        BlobPayload, ChannelLimit, FrameEncoder, StepError, SubmissionId, SubmissionPayload,
    };
    use base_batcher_source::{
        L2BlockEvent,
        test_utils::{ChannelBlockSource, ChannelL1HeadSource},
    };
    use base_blobs::{BlobDecoder, BlobEncoder};
    use base_protocol::Frame;
    use base_runtime::{
        Cancellation, Clock, Spawner,
        deterministic::{Config, Runner},
    };

    use super::STEP_BUDGET;
    use crate::{
        BatchDriverError, BatchTxCandidateError, DerivationStatus,
        test_utils::{
            BlockStub, DriverFixture, PipelineCall, ScriptedTxManager, SendOutcome, SubmissionStub,
            TrackingPipeline,
        },
    };

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
                .build();

            assert_eq!(recorded.lock().unwrap().l1_heads(), [50]);
        });
    }

    /// Blocks at or below the safe head are already derived, so the driver drops them instead
    /// of batching them again.
    #[test]
    fn run_drops_blocks_at_or_below_the_safe_head() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            let (source, l1_head_source) = queued_sources([4, 5, 6], []);
            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                    .source(source)
                    .l1_head_source(l1_head_source)
                    .derivation_status(DerivationStatus {
                        safe_l2: BlockStub::info(5),
                        ..Default::default()
                    })
                    .build();

            let handle = ctx.spawn(driver.run());
            ctx.sleep(Duration::from_millis(10)).await;
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());

            assert_eq!(
                recorded.lock().unwrap().calls,
                [PipelineCall::AddBlock(6), PipelineCall::Flush]
            );
        });
    }

    /// A fatal encoding error stops the driver with that error, instead of dropping the block and
    /// leaving a gap in the L2 chain posted to L1.
    #[test]
    fn run_halts_on_a_fatal_step_error() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let pipeline =
                TrackingPipeline::new().with_step_error(StepError::BlockExceedsChannelLimit {
                    cursor: 0,
                    limit: ChannelLimit::RlpBytes { required: 1, maximum: 0 },
                });
            let (driver, _handles) =
                DriverFixture::new(ctx, pipeline, ScriptedTxManager::confirming_at(1)).build();

            assert!(matches!(
                driver.run().await,
                Err(BatchDriverError::Step(StepError::BlockExceedsChannelLimit { .. }))
            ));
        });
    }

    // The loop polls its arms in priority order. Each test in this group makes several arms
    // ready at once and checks which one the driver serves first. Every call log ends with the
    // shutdown flush.

    /// A cancelled driver shuts down without serving the admin flush, block and L1 head already
    /// waiting, and the flush call fails.
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
            // Only the shutdown flush is recorded, so neither the admin flush, the block nor the
            // head was served.
            assert_eq!(recorded.lock().unwrap().calls, [PipelineCall::Flush]);
        });
    }

    /// On cancellation the derivation-status poller exits too and closes its channel, so the
    /// driver can see both at once. It must shut down cleanly, not fail with
    /// `DerivationStatusSourceClosed`.
    #[test]
    fn run_prioritizes_cancellation_over_closed_derivation_status() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (driver, handles) = DriverFixture::new(
                ctx.clone(),
                TrackingPipeline::new(),
                ScriptedTxManager::confirming_at(1),
            )
            .build();

            ctx.cancel();
            drop(handles);
            assert!(driver.run().await.is_ok());
        });
    }

    /// A ready admin command is served before a ready L2 block, so a steady block stream cannot
    /// hold back a stop, start or flush.
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

            assert_eq!(
                recorded.lock().unwrap().calls,
                [PipelineCall::Flush, PipelineCall::AddBlock(1), PipelineCall::Flush]
            );
        });
    }

    /// A new safe head is reconciled before a ready L2 block, so pruning and reorg recovery are
    /// not held back by a stream of blocks to encode.
    #[test]
    fn run_prioritizes_derivation_status_before_source() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            let (source, l1_head_source) = queued_sources([6], []);
            let (driver, handles) =
                DriverFixture::new(ctx.clone(), pipeline, ScriptedTxManager::confirming_at(1))
                    .source(source)
                    .l1_head_source(l1_head_source)
                    .build();
            handles
                .derivation_status_tx
                .send(DerivationStatus {
                    safe_l2: BlockStub::info(5),
                    current_l1: BlockStub::info(1),
                })
                .await
                .unwrap();

            let handle = ctx.spawn(driver.run());
            ctx.sleep(Duration::from_millis(10)).await;
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());

            assert_eq!(
                recorded.lock().unwrap().calls,
                [
                    PipelineCall::ReconcileDerivation { safe_l2: 5, current_l1: 1 },
                    PipelineCall::AddBlock(6),
                    PipelineCall::Flush,
                ]
            );
        });
    }

    /// A failed submission is resent before any submission a new block releases, even when the
    /// failure and the block arrive together.
    #[test]
    fn run_resends_a_failed_submission_before_a_block_releases_newer_ones() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            let recorded = pipeline.recorded();
            // Submitted by the first CPU phase, so its failure is ready at the first wait,
            // along with the block.
            pipeline.submissions.push_back(SubmissionStub::stub());
            let (source, l1_head_source) = queued_sources([1], []);
            let (driver, _handles) = DriverFixture::new(
                ctx.clone(),
                pipeline,
                ScriptedTxManager::new([SendOutcome::Failed]),
            )
            .source(source)
            .l1_head_source(l1_head_source)
            .build();

            let handle = ctx.spawn(driver.run());
            ctx.sleep(Duration::from_millis(10)).await;
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());

            assert_eq!(
                recorded.lock().unwrap().calls,
                [
                    PipelineCall::Dequeue(SubmissionId(0)),
                    PipelineCall::Requeue(SubmissionId(0)),
                    PipelineCall::Dequeue(SubmissionId(1)),
                    PipelineCall::AddBlock(1),
                    PipelineCall::Flush,
                ]
            );
        });
    }

    // Encoding runs in slices of `STEP_BUDGET` steps. The tests below check what happens
    // between two slices.

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

    /// The driver yields to other tasks between encoding slices, so a task can send an admin
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
            // Send the stop once the first slice is done, which this task only sees if the driver
            // yields.
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

    // The tests below check how ready submissions become L1 transactions.

    /// A blob submission that cannot be built into a transaction is fatal, because a retry would
    /// fail the same way and hold back every submission behind it.
    #[test]
    fn test_blob_encoding_failure_is_fatal() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            // A frame as large as a whole blob no longer fits once framed.
            pipeline.submissions.push_back(SubmissionPayload::Blobs(vec![BlobPayload::new(vec![
                Arc::new(Frame {
                    data: vec![0u8; BlobEncoder::BLOB_MAX_DATA_SIZE],
                    ..Frame::default()
                }),
            ])]));

            let (driver, _handles) =
                DriverFixture::new(ctx, pipeline, ScriptedTxManager::confirming_at(1)).build();

            let result = driver.run().await;
            assert!(
                matches!(
                    result,
                    Err(BatchDriverError::Blob(BatchTxCandidateError::BlobEncoding(_)))
                ),
                "got {result:?}"
            );
        });
    }

    /// Each blob payload of a submission becomes its own blob of one L1 transaction, in order,
    /// even when all their frames would fit in one blob.
    #[test]
    fn run_sends_each_blob_payload_as_its_own_blob() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let mut pipeline = TrackingPipeline::new();
            let frames: Vec<_> =
                (0..3).map(|number| Arc::new(Frame { number, ..Frame::default() })).collect();
            pipeline.submissions.push_back(SubmissionPayload::Blobs(
                frames.iter().map(|frame| BlobPayload::new(vec![Arc::clone(frame)])).collect(),
            ));
            let tx_manager = ScriptedTxManager::confirming_at(10);
            let (driver, _handles) =
                DriverFixture::new(ctx.clone(), pipeline, tx_manager.clone()).build();

            let handle = ctx.spawn(driver.run());
            ctx.sleep(Duration::from_millis(10)).await;
            ctx.cancel();
            assert!(handle.await.unwrap().is_ok());

            let candidates = tx_manager.candidates();
            assert_eq!(candidates.len(), 1);
            assert!(candidates[0].tx_data.is_empty(), "a blob transaction carries no calldata");
            let blobs: Vec<_> = candidates[0]
                .blobs
                .iter()
                .map(|blob| BlobDecoder::decode(blob).expect("the blob decodes"))
                .collect();
            let frames: Vec<_> =
                frames.iter().map(|frame| FrameEncoder::to_calldata(frame)).collect();
            assert_eq!(blobs, frames);
        });
    }
}
