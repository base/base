//! Submission lifecycle management for the batch driver.

use std::{future::Future, pin::Pin, sync::Arc};

use alloy_primitives::{Address, Bytes, U256};
use base_batcher_encoder::{
    BatchPipeline, BatcherMetrics, BlobPayload, DaType, EncoderConfig, FrameEncoder, SubmissionId,
    SubmissionPayload,
};
use base_blobs::{BlobEncodeError, BlobEncoder};
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::TxOutcome;

/// Type alias for the in-flight receipt future collection.
type InFlight =
    FuturesUnordered<Pin<Box<dyn Future<Output = (Vec<SubmissionId>, TxOutcome)> + Send>>>;

/// Builds L1 transaction candidates for batch submissions.
#[derive(Debug)]
pub struct BatchTxCandidateBuilder;

/// Failure while building a batch transaction candidate.
#[derive(Debug, thiserror::Error)]
pub enum BatchTxCandidateError {
    /// A blob transaction must contain a protocol-valid number of blobs.
    #[error("blob transaction contains {count} blobs; expected 1..={maximum}")]
    InvalidBlobCount {
        /// Supplied blob count.
        count: usize,
        /// Protocol transaction maximum.
        maximum: usize,
    },
    /// One packed payload could not be encoded as a blob.
    #[error(transparent)]
    BlobEncoding(#[from] BlobEncodeError),
}

impl BatchTxCandidateBuilder {
    /// Build a blob transaction candidate from packed frame payloads.
    ///
    /// The returned byte count is the total derivation payload submitted across
    /// all blobs, including each blob's derivation-version prefix and frame metadata.
    pub fn blob_tx_candidate(
        inbox: Address,
        payloads: &[BlobPayload],
    ) -> Result<(TxCandidate, u64), BatchTxCandidateError> {
        if payloads.is_empty() || payloads.len() > EncoderConfig::MAX_BLOBS_PER_TX {
            return Err(BatchTxCandidateError::InvalidBlobCount {
                count: payloads.len(),
                maximum: EncoderConfig::MAX_BLOBS_PER_TX,
            });
        }

        let mut blobs = Vec::with_capacity(payloads.len());
        let mut payload_size = 0usize;

        // Encode each packed payload independently; blob boundaries are already
        // fixed by the encoder and must remain visible to the transaction sidecar.
        for payload in payloads {
            payload_size += 1 + payload
                .frames()
                .iter()
                .map(|frame| BlobEncoder::FRAME_OVERHEAD + frame.data.len())
                .sum::<usize>();
            blobs.push(BlobEncoder::encode_packed(payload.frames())?);
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
/// batcher inbox address. These were previously loose fields on [`BatchDriver`].
#[derive(Debug)]
pub struct SubmissionQueue<TM: TxManager> {
    tx_manager: TM,
    in_flight: InFlight,
    semaphore: Arc<Semaphore>,
    inbox: Address,
    txpool_blocked: bool,
}

impl<TM: TxManager> SubmissionQueue<TM> {
    /// Create a new [`SubmissionQueue`].
    pub fn new(tx_manager: TM, inbox: Address, max_pending: usize) -> Self {
        Self {
            tx_manager,
            in_flight: FuturesUnordered::new(),
            semaphore: Arc::new(Semaphore::new(max_pending)),
            inbox,
            txpool_blocked: false,
        }
    }

    /// Submit all ready frames that fit within semaphore capacity.
    ///
    /// For each available semaphore permit (= one L1 transaction), dequeues one
    /// ready submission and encodes it as a blob or calldata transaction. Each
    /// blob payload may contain frames from multiple channels.
    /// Loops until the semaphore is exhausted, the pipeline has no ready submissions,
    /// or the txpool is blocked.
    ///
    /// Returns `true` if the pipeline reported no further ready submissions (fully
    /// drained), or `false` if it stopped early because the semaphore is exhausted, the
    /// txpool is blocked, or a blob-encoding failure required a requeue.
    pub async fn submit_pending<P: BatchPipeline>(&mut self, pipeline: &mut P) -> bool {
        loop {
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

            // Convert the submission into its final L1 transaction payload before
            // handing ownership to the asynchronous transaction manager.
            let da_type = sub.da_type();
            let da_type_label = match da_type {
                DaType::Blob => BatcherMetrics::DA_TYPE_BLOB,
                DaType::Calldata => BatcherMetrics::DA_TYPE_CALLDATA,
            };
            let blob_payload_bytes;
            let candidate = match sub.payload() {
                SubmissionPayload::Blobs(payloads) => {
                    match BatchTxCandidateBuilder::blob_tx_candidate(self.inbox, payloads) {
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
                    }
                }
                SubmissionPayload::Calldata(frame) => {
                    blob_payload_bytes = None;
                    TxCandidate {
                        to: Some(self.inbox),
                        tx_data: FrameEncoder::to_calldata(frame),
                        value: U256::ZERO,
                        gas_limit: 0,
                        blobs: vec![].into(),
                    }
                }
            };

            let frame_bytes = sub.frame_bytes();
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
            let handle = self.tx_manager.send_async(candidate).await;
            let fut: Pin<Box<dyn Future<Output = (Vec<SubmissionId>, TxOutcome)> + Send>> =
                Box::pin(async move {
                    let outcome = match handle.await {
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
                            warn!(error = %e, "submission failed");
                            TxOutcome::Failed
                        }
                    };
                    drop(permit);
                    (vec![sub.id], outcome)
                });
            self.in_flight.push(fut);
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
                warn!(error = %e, "cancel_tx failed, txpool remains blocked");
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
        loop {
            if self.in_flight.is_empty() {
                break;
            }
            tokio::select! {
                _ = &mut timeout_fut => {
                    warn!(remaining = %self.in_flight.len(), "drain timeout reached, abandoning in-flight submissions");
                    break;
                }
                Some((ids, outcome)) = self.in_flight.next() => {
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
            }
        }
    }

    /// Discard all in-flight futures, returning their semaphore permits.
    ///
    /// Used before resetting the pipeline so stale completions cannot modify
    /// freshly rebuilt state.
    pub fn discard(&mut self) {
        let discarded = self.in_flight.len();
        if discarded > 0 {
            warn!(discarded = %discarded, "discarding in-flight submissions before pipeline reset");
            BatcherMetrics::in_flight_submissions().set(0.0);
        }
        self.in_flight = FuturesUnordered::new();
    }

    /// Returns a future for the next settled `(ids, outcome)` pair.
    ///
    /// Resolves immediately to `None` when in-flight is empty; safe to use as
    /// a `select!` arm with a `Some(...)` pattern guard.
    pub fn next_settled(
        &mut self,
    ) -> impl Future<Output = Option<(Vec<SubmissionId>, TxOutcome)>> + '_ {
        self.in_flight.next()
    }

    /// Returns the number of currently in-flight submissions.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alloy_primitives::Address;

    use super::*;
    use crate::test_utils::{NeverConfirmTxManager, Recorded, SubmissionStub, TrackingPipeline};

    #[test]
    fn blob_candidate_rejects_empty_transaction() {
        assert!(matches!(
            BatchTxCandidateBuilder::blob_tx_candidate(Address::ZERO, &[]),
            Err(BatchTxCandidateError::InvalidBlobCount { count: 0, .. })
        ));
    }

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
}
