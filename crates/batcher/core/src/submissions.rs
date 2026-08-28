//! Submission lifecycle management for the batch driver.

use std::{future::Future, pin::Pin, sync::Arc};

use alloy_primitives::{Address, Bytes, U256};
use base_batcher_encoder::{BatchPipeline, BatcherMetrics, DaType, FrameEncoder, SubmissionId};
use base_blobs::{BlobEncodeError, BlobEncoder};
use base_protocol::Frame;
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::TxOutcome;

/// Type alias for the in-flight receipt future collection.
pub type InFlight =
    FuturesUnordered<Pin<Box<dyn Future<Output = (Vec<SubmissionId>, TxOutcome)> + Send>>>;

/// Identifies whether a settled submission still belongs to pipeline bookkeeping.
#[derive(Debug)]
pub enum SettledSubmission {
    /// Completion that may still update the current pipeline.
    Attached {
        /// Pipeline submission identifiers represented by the transaction.
        ids: Vec<SubmissionId>,
        /// Terminal transaction outcome.
        outcome: TxOutcome,
    },
    /// Completion retained only for nonce lifecycle and operational accounting.
    Detached {
        /// Obsolete pipeline submission identifiers retained for metrics.
        ids: Vec<SubmissionId>,
        /// Terminal transaction outcome.
        outcome: TxOutcome,
    },
}

/// Builds L1 transaction candidates for batch submissions.
#[derive(Debug)]
pub struct BatchTxCandidateBuilder;

impl BatchTxCandidateBuilder {
    /// Builds one blob candidate while preserving one blob per batch frame.
    pub fn blob(
        inbox: Address,
        frames: &[Arc<Frame>],
    ) -> Result<(TxCandidate, u64), BlobEncodeError> {
        let mut blobs = Vec::with_capacity(frames.len());
        let mut payload_size = 0usize;

        for frame in frames {
            let data = FrameEncoder::to_calldata(frame);
            payload_size += data.len();
            blobs.push(BlobEncoder::encode(data.as_ref())?);
        }

        Ok((
            TxCandidate {
                to: Some(inbox),
                tx_data: Bytes::new(),
                value: U256::ZERO,
                gas_limit: 0,
                blobs: Arc::from(blobs),
            },
            payload_size as u64,
        ))
    }
}

/// Manages the full submission lifecycle for the batch driver.
///
/// Owns capacity management (semaphore), in-flight receipt tracking
/// ([`FuturesUnordered`]), txpool blockage state, the [`TxManager`], and the
/// batcher inbox address. These were previously loose fields on [`crate::BatchDriver`].
#[derive(Debug)]
pub struct SubmissionQueue<TM: TxManager> {
    /// Transaction lifecycle service owning submitted L1 nonces.
    tx_manager: TM,
    /// Completion futures for submissions still attached to pipeline bookkeeping.
    in_flight: InFlight,
    /// Completion futures detached by a pipeline reset but still manager-owned.
    detached: InFlight,
    /// Capacity permits retained until each admitted nonce reaches resolution.
    semaphore: Arc<Semaphore>,
    /// L1 batch-inbox destination for blob and calldata transactions.
    inbox: Address,
    /// Whether an incompatible txpool reservation has paused new admission.
    txpool_blocked: bool,
}

impl<TM: TxManager> SubmissionQueue<TM> {
    /// Create a new [`SubmissionQueue`].
    pub fn new(tx_manager: TM, inbox: Address, max_pending: usize) -> Self {
        Self {
            tx_manager,
            in_flight: FuturesUnordered::new(),
            detached: FuturesUnordered::new(),
            semaphore: Arc::new(Semaphore::new(max_pending)),
            inbox,
            txpool_blocked: false,
        }
    }

    /// Submit all ready frames that fit within semaphore capacity.
    ///
    /// For each available semaphore permit (= one L1 transaction), dequeues one
    /// ready submission and encodes it as a blob or calldata transaction. Blob
    /// submissions map each frame to one blob.
    /// Loops until the semaphore is exhausted, the pipeline has no ready submissions,
    /// or the txpool is blocked.
    ///
    /// Returns `true` if the pipeline reported no further ready submissions (fully
    /// drained), or `false` if it stopped early because the semaphore is exhausted, the
    /// txpool is blocked, or a blob-encoding failure required a requeue.
    pub async fn submit_pending<P: BatchPipeline>(&mut self, pipeline: &mut P) -> bool {
        loop {
            // Phase 1: enforce caller-owned, non-blocking capacity before
            // removing work from the batch pipeline.
            if self.txpool_blocked {
                return false;
            }
            let Ok(permit) = Arc::clone(&self.semaphore).try_acquire_owned() else {
                // Semaphore is exhausted. This is only real backpressure if the pipeline
                // still has work waiting -- if it's actually empty, the caller should see
                // this as fully drained rather than blocked (see `has_ready_submission`).
                return !pipeline.has_ready_submission();
            };

            let Some(sub) = pipeline.next_submission() else {
                drop(permit);
                return true;
            };
            debug_assert!(!sub.frames.is_empty(), "batch submissions must contain frames");
            if sub.frames.is_empty() {
                warn!(submission = ?sub.id, "skipping empty batch submission");
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_FAILED).increment(1);
                drop(permit);
                continue;
            }

            // Phase 2: convert exactly one pipeline submission into one L1
            // candidate. Encoding failures return ownership to the pipeline.
            let da_type_label = match sub.da_type {
                DaType::Blob => BatcherMetrics::DA_TYPE_BLOB,
                DaType::Calldata => BatcherMetrics::DA_TYPE_CALLDATA,
            };
            let blob_payload_bytes;
            let candidate = match sub.da_type {
                DaType::Blob => match BatchTxCandidateBuilder::blob(self.inbox, &sub.frames) {
                    Ok((candidate, payload_size)) => {
                        blob_payload_bytes = Some(payload_size);
                        candidate
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to encode frames to blob, requeueing");
                        pipeline.requeue(sub.id);
                        drop(permit);
                        return false;
                    }
                },
                DaType::Calldata => {
                    debug_assert!(
                        sub.frames.len() == 1,
                        "calldata submissions must contain exactly one frame"
                    );
                    if sub.frames.len() > 1 {
                        warn!(
                            submission = ?sub.id,
                            frame_count = %sub.frames.len(),
                            "calldata submission has multiple frames; only first will be submitted"
                        );
                    }
                    blob_payload_bytes = None;
                    TxCandidate {
                        to: Some(self.inbox),
                        tx_data: FrameEncoder::to_calldata(&sub.frames[0]),
                        value: U256::ZERO,
                        gas_limit: 0,
                        blobs: vec![].into(),
                    }
                }
            };
            // Phase 3: hand the candidate to the transaction manager and retain
            // the semaphore permit for the complete nonce lifecycle.
            let frame_bytes = sub.frames.iter().map(|f| f.data.len()).sum::<usize>();
            info!(
                submissions = 1,
                da_type = %da_type_label,
                frame_bytes = %frame_bytes,
                "submitting batch frames to L1"
            );
            BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_SUBMITTED).increment(1);
            BatcherMetrics::da_bytes_submitted_total(da_type_label).increment(frame_bytes as u64);
            BatcherMetrics::in_flight_submissions().increment(1.0);
            // Capture for the post-confirm metric: blob_used_bytes_total counts
            // payload bytes that actually landed on L1, not bytes attempted, so
            // we only increment after the tx confirms.
            let handle = self.tx_manager.submit(candidate);
            let fut: Pin<Box<dyn Future<Output = (Vec<SubmissionId>, TxOutcome)> + Send>> =
                Box::pin(async move {
                    let result = handle.wait().await;
                    // Phase 4: translate terminal manager state into pipeline
                    // bookkeeping without exposing provider error payloads.
                    let outcome = match result {
                        Ok(receipt) => {
                            let l1_block = receipt.block_number.unwrap_or_else(|| {
                                warn!("confirmed receipt missing block number; l1_head will not advance");
                                0
                            });
                            if let Some(bytes) = blob_payload_bytes {
                                BatcherMetrics::blob_used_bytes_total().increment(bytes);
                            }
                            TxOutcome::Confirmed { l1_block }
                        }
                        Err(TxManagerError::AlreadyReserved) => {
                            warn!("txpool nonce slot already reserved");
                            TxOutcome::TxpoolBlocked
                        }
                        Err(e) => {
                            warn!(error_kind = e.kind(), "submission failed");
                            TxOutcome::Failed
                        }
                    };
                    drop(permit);
                    (vec![sub.id], outcome)
                });
            self.in_flight.extend([fut]);
        }
    }

    /// Attempt to clear a txpool blockage by cancelling the stuck transaction.
    ///
    /// No-op if the txpool is not currently blocked. On success, clears the
    /// blocked flag so submission can resume.
    pub async fn recover_txpool(&mut self) {
        if !self.txpool_blocked {
            return;
        }
        match self.tx_manager.cancel_tx().await {
            Ok(()) => {
                self.txpool_blocked = false;
                info!("txpool unblocked after cancellation tx");
            }
            Err(e) => {
                warn!(error_kind = e.kind(), "cancel_tx failed, txpool remains blocked");
            }
        }
    }

    /// Handle a settled in-flight receipt.
    ///
    /// On confirmation, calls `pipeline.confirm` for each submitted id and
    /// `pipeline.advance_l1_head` once. On failure, requeues all. On txpool
    /// blockage, requeues all and sets the blocked flag.
    pub fn handle_outcome<P: BatchPipeline>(
        &mut self,
        pipeline: &mut P,
        ids: Vec<SubmissionId>,
        outcome: TxOutcome,
    ) {
        BatcherMetrics::in_flight_submissions().decrement(1.0);
        match outcome {
            TxOutcome::Confirmed { l1_block } => {
                for id in &ids {
                    pipeline.confirm(*id, l1_block);
                }
                pipeline.advance_l1_head(l1_block);
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_CONFIRMED)
                    .increment(ids.len() as u64);
                info!(submissions = %ids.len(), l1_block = %l1_block, "submission confirmed on L1");
            }
            TxOutcome::Failed => {
                let count = ids.len();
                for id in ids {
                    pipeline.requeue(id);
                }
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_FAILED)
                    .increment(count as u64);
                warn!(submissions = %count, "submission failed");
            }
            TxOutcome::TxpoolBlocked => {
                let count = ids.len();
                for id in ids {
                    pipeline.requeue(id);
                }
                self.txpool_blocked = true;
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_REQUEUED)
                    .increment(count as u64);
                warn!(submissions = %count, "submission blocked by txpool nonce slot");
            }
        }
    }

    /// Drain all in-flight futures up to the given deadline.
    ///
    /// Confirmed receipts call `pipeline.confirm` + `pipeline.advance_l1_head`.
    /// Failed or txpool-blocked submissions are logged and abandoned — no requeue
    /// because the process is shutting down.
    pub async fn drain<P: BatchPipeline>(
        &mut self,
        pipeline: &mut P,
        mut timeout_fut: Pin<Box<dyn Future<Output = ()> + Send>>,
    ) {
        // Shutdown consumes only already-admitted work. It never requeues a
        // failed submission into a pipeline that is about to be dropped.
        loop {
            if self.in_flight_count() == 0 {
                break;
            }
            tokio::select! {
                _ = &mut timeout_fut => {
                    warn!(remaining = %self.in_flight_count(), "drain timeout reached, abandoning in-flight submissions");
                    break;
                }
                Some(settled) = self.next_settled() => {
                    match settled {
                        SettledSubmission::Attached { ids, outcome } => {
                            BatcherMetrics::in_flight_submissions().decrement(1.0);
                            match outcome {
                                TxOutcome::Confirmed { l1_block } => {
                                    for id in &ids {
                                        pipeline.confirm(*id, l1_block);
                                    }
                                    pipeline.advance_l1_head(l1_block);
                                    BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_CONFIRMED).increment(ids.len() as u64);
                                    info!(submissions = %ids.len(), l1_block = %l1_block, "submission confirmed on L1 during drain");
                                }
                                TxOutcome::Failed => {
                                    BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_FAILED).increment(ids.len() as u64);
                                    warn!(submissions = %ids.len(), "submission failed during drain, abandoning");
                                }
                                TxOutcome::TxpoolBlocked => {
                                    BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_REQUEUED).increment(ids.len() as u64);
                                    warn!(submissions = %ids.len(), "submission txpool-blocked during drain, abandoning");
                                }
                            }
                        }
                        SettledSubmission::Detached { ids, outcome } => {
                            self.handle_detached_outcome(ids, outcome);
                        }
                    }
                }
            }
        }
    }

    /// Detach in-flight submissions from pipeline bookkeeping.
    ///
    /// Used before resetting the pipeline so stale completions cannot modify
    /// freshly rebuilt state. The driver continues polling these futures and
    /// retains their semaphore permits until each nonce is resolved; resetting
    /// the pipeline therefore cannot bypass transaction backpressure.
    pub fn discard(&mut self) {
        let discarded = self.in_flight.len();
        if discarded == 0 {
            return;
        }
        warn!(discarded = %discarded, "detaching in-flight submissions before pipeline reset");

        // Move the futures out of pipeline bookkeeping but keep them under
        // explicit driver ownership until their nonce lifecycles resolve.
        self.detached.extend(std::mem::take(&mut self.in_flight));
    }

    /// Records an obsolete submission outcome without mutating the reset pipeline.
    pub fn handle_detached_outcome(&mut self, ids: Vec<SubmissionId>, outcome: TxOutcome) {
        BatcherMetrics::in_flight_submissions().decrement(1.0);
        match outcome {
            TxOutcome::Confirmed { l1_block } => {
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_CONFIRMED)
                    .increment(ids.len() as u64);
                info!(
                    submissions = %ids.len(),
                    l1_block,
                    "detached submission confirmed on L1",
                );
            }
            TxOutcome::Failed => {
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_FAILED)
                    .increment(ids.len() as u64);
                warn!(submissions = %ids.len(), "detached submission failed");
            }
            TxOutcome::TxpoolBlocked => {
                self.txpool_blocked = true;
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_REQUEUED)
                    .increment(ids.len() as u64);
                warn!(submissions = %ids.len(), "detached submission blocked by txpool nonce slot");
            }
        }
    }

    /// Waits for the next attached or detached submission to settle.
    ///
    /// Resolves immediately to `None` when no submission is pending.
    pub async fn next_settled(&mut self) -> Option<SettledSubmission> {
        tokio::select! {
            settled = self.in_flight.next(), if !self.in_flight.is_empty() => {
                settled.map(|(ids, outcome)| SettledSubmission::Attached { ids, outcome })
            }
            settled = self.detached.next(), if !self.detached.is_empty() => {
                settled.map(|(ids, outcome)| SettledSubmission::Detached { ids, outcome })
            }
            else => None,
        }
    }

    /// Returns the number of currently in-flight submissions.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len().saturating_add(self.detached.len())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alloy_primitives::Address;

    use super::*;
    use crate::test_utils::{
        ImmediateConfirmTxManager, NeverConfirmTxManager, Recorded, SubmissionStub,
        TrackingPipeline,
    };

    /// Regression test: if exactly `max_pending` submissions are ready, all permits are
    /// handed out and held by in-flight (unconfirmed) transactions. The pipeline itself
    /// is now empty, so `submit_pending` must report "fully drained" -- not backpressured
    /// -- otherwise a caller waiting for drain-and-flush-ack (e.g. `BatchDriver::run`)
    /// would wait forever for capacity that was never coming back this cycle.
    #[tokio::test]
    async fn submit_pending_reports_drained_when_ready_work_exactly_fills_permits() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::with_id(0));
        pipeline.submissions.push_back(SubmissionStub::with_id(1));

        let mut queue = SubmissionQueue::new(NeverConfirmTxManager, Address::ZERO, 2);

        let drained = queue.submit_pending(&mut pipeline).await;

        assert!(
            drained,
            "pipeline has no more ready work even though all permits are held by \
             unconfirmed in-flight submissions"
        );
        assert_eq!(recorded.lock().unwrap().dequeued.len(), 2, "both submissions must be sent");
    }

    /// Companion case: with a ready submission still queued behind exhausted permits,
    /// `submit_pending` must report backpressure rather than falsely claiming drained.
    #[tokio::test]
    async fn submit_pending_reports_backpressure_when_ready_work_remains() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::with_id(0));
        pipeline.submissions.push_back(SubmissionStub::with_id(1));

        let mut queue = SubmissionQueue::new(NeverConfirmTxManager, Address::ZERO, 1);

        let drained = queue.submit_pending(&mut pipeline).await;

        assert!(!drained, "a ready submission is still waiting on semaphore capacity");
        assert_eq!(recorded.lock().unwrap().dequeued.len(), 1, "only the single permit is used");
    }

    #[tokio::test]
    async fn discard_retains_capacity_until_detached_nonce_resolves() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(SubmissionStub::with_id(0));
        let mut queue = SubmissionQueue::new(NeverConfirmTxManager, Address::ZERO, 1);

        assert!(queue.submit_pending(&mut pipeline).await);
        queue.discard();
        assert_eq!(queue.in_flight_count(), 1, "detached work remains operationally visible");
        pipeline.submissions.push_back(SubmissionStub::with_id(1));

        assert!(
            !queue.submit_pending(&mut pipeline).await,
            "a detached unresolved nonce must continue applying backpressure"
        );
        assert_eq!(
            recorded.lock().unwrap().dequeued.len(),
            1,
            "the reset pipeline must not bypass the detached nonce"
        );
    }

    #[tokio::test]
    async fn detached_completion_remains_tracked_until_observed() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(recorded);
        pipeline.submissions.push_back(SubmissionStub::with_id(0));
        let mut queue =
            SubmissionQueue::new(ImmediateConfirmTxManager { l1_block: 7 }, Address::ZERO, 1);

        assert!(queue.submit_pending(&mut pipeline).await);
        queue.discard();
        assert_eq!(queue.in_flight_count(), 1);

        let Some(SettledSubmission::Detached { ids, outcome }) = queue.next_settled().await else {
            panic!("detached submission should settle")
        };
        assert_eq!(ids.len(), 1);
        assert!(matches!(outcome, TxOutcome::Confirmed { l1_block: 7 }));
        assert_eq!(queue.in_flight_count(), 0);
    }
}
