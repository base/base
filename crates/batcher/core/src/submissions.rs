//! Submission lifecycle management for the batch driver.

use std::{future::Future, pin::Pin, sync::Arc};

use alloy_primitives::{Address, Bytes, U256};
use base_batcher_encoder::{BatchPipeline, DaType, FrameEncoder, SubmissionId};
use base_blobs::BlobEncoder;
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use futures::stream::{FuturesUnordered, StreamExt};
use metrics::{counter, gauge};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::{BatcherMetrics, TxOutcome};

/// Type alias for the in-flight receipt future collection.
type InFlight = FuturesUnordered<Pin<Box<dyn Future<Output = (SubmissionId, TxOutcome)> + Send>>>;

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
    /// Loops until the semaphore is exhausted, the pipeline has nothing ready,
    /// or the txpool is blocked. Each accepted submission is pushed to the
    /// in-flight future set.
    pub async fn submit_pending<P: BatchPipeline>(&mut self, pipeline: &mut P) {
        loop {
            if self.txpool_blocked {
                break;
            }
            let Ok(permit) = Arc::clone(&self.semaphore).try_acquire_owned() else {
                break;
            };
            let Some(sub) = pipeline.next_submission() else {
                drop(permit);
                break;
            };
            let id = sub.id;
            let frame_bytes: usize = sub.frames.iter().map(|f| f.data.len()).sum();
            let da_type_label = match sub.da_type {
                DaType::Blob => BatcherMetrics::DA_TYPE_BLOB,
                DaType::Calldata => BatcherMetrics::DA_TYPE_CALLDATA,
            };
            let candidate = match sub.da_type {
                DaType::Blob => match BlobEncoder::encode_frames(&sub.frames) {
                    Ok(blobs) => TxCandidate {
                        to: Some(self.inbox),
                        tx_data: Bytes::new(),
                        value: U256::ZERO,
                        gas_limit: 0,
                        blobs: Arc::new(blobs),
                    },
                    Err(e) => {
                        warn!(id = %id.0, error = %e, "failed to encode frames to blobs, requeueing");
                        pipeline.requeue(id);
                        drop(permit);
                        continue;
                    }
                },
                DaType::Calldata => TxCandidate {
                    to: Some(self.inbox),
                    tx_data: FrameEncoder::to_calldata(&sub.frames[0]),
                    value: U256::ZERO,
                    gas_limit: 0,
                    blobs: vec![].into(),
                },
            };
            info!(
                id = %id.0,
                da_type = %da_type_label,
                frame_bytes = %frame_bytes,
                "submitting batch frames to L1"
            );
            counter!(BatcherMetrics::SUBMISSION_TOTAL, "outcome" => BatcherMetrics::OUTCOME_SUBMITTED).increment(1);
            counter!(BatcherMetrics::DA_BYTES_SUBMITTED_TOTAL, "da_type" => da_type_label)
                .increment(frame_bytes as u64);
            gauge!(BatcherMetrics::IN_FLIGHT_SUBMISSIONS).increment(1.0);
            let handle = self.tx_manager.send_async(candidate).await;
            let fut: Pin<Box<dyn Future<Output = (SubmissionId, TxOutcome)> + Send>> = Box::pin(
                async move {
                    let outcome = match handle.await {
                        Ok(receipt) => {
                            let l1_block = receipt.block_number.unwrap_or_else(|| {
                                warn!(id = %id.0, "confirmed receipt missing block number; l1_head will not advance");
                                0
                            });
                            TxOutcome::Confirmed { l1_block }
                        }
                        Err(TxManagerError::AlreadyReserved) => {
                            warn!(id = %id.0, "txpool nonce slot already reserved");
                            TxOutcome::TxpoolBlocked
                        }
                        Err(e) => {
                            warn!(id = %id.0, error = %e, "submission failed");
                            TxOutcome::Failed
                        }
                    };
                    drop(permit);
                    (id, outcome)
                },
            );
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
    /// On confirmation, calls `pipeline.confirm` and `pipeline.advance_l1_head`.
    /// On failure, requeues. On txpool blockage, requeues and sets the blocked flag.
    pub fn handle_outcome<P: BatchPipeline>(
        &mut self,
        pipeline: &mut P,
        id: SubmissionId,
        outcome: TxOutcome,
    ) {
        gauge!(BatcherMetrics::IN_FLIGHT_SUBMISSIONS).decrement(1.0);
        match outcome {
            TxOutcome::Confirmed { l1_block } => {
                pipeline.confirm(id, l1_block);
                pipeline.advance_l1_head(l1_block);
                counter!(BatcherMetrics::SUBMISSION_TOTAL, "outcome" => BatcherMetrics::OUTCOME_CONFIRMED).increment(1);
                info!(id = %id.0, l1_block = %l1_block, "submission confirmed on L1");
            }
            TxOutcome::Failed => {
                pipeline.requeue(id);
                counter!(BatcherMetrics::SUBMISSION_TOTAL, "outcome" => BatcherMetrics::OUTCOME_FAILED).increment(1);
                warn!(id = %id.0, "submission failed, requeued for retry");
            }
            TxOutcome::TxpoolBlocked => {
                pipeline.requeue(id);
                self.txpool_blocked = true;
                counter!(BatcherMetrics::SUBMISSION_TOTAL, "outcome" => BatcherMetrics::OUTCOME_REQUEUED).increment(1);
                warn!(id = %id.0, "submission blocked by txpool nonce slot, requeued");
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
                Some((id, outcome)) = self.in_flight.next() => {
                    gauge!(BatcherMetrics::IN_FLIGHT_SUBMISSIONS).decrement(1.0);
                    match outcome {
                        TxOutcome::Confirmed { l1_block } => {
                            pipeline.confirm(id, l1_block);
                            pipeline.advance_l1_head(l1_block);
                            counter!(BatcherMetrics::SUBMISSION_TOTAL, "outcome" => BatcherMetrics::OUTCOME_CONFIRMED).increment(1);
                            info!(id = %id.0, l1_block = %l1_block, "submission confirmed on L1 during drain");
                        }
                        TxOutcome::Failed => {
                            counter!(BatcherMetrics::SUBMISSION_TOTAL, "outcome" => BatcherMetrics::OUTCOME_FAILED).increment(1);
                            warn!(id = %id.0, "submission failed during drain, abandoning");
                        }
                        TxOutcome::TxpoolBlocked => {
                            counter!(BatcherMetrics::SUBMISSION_TOTAL, "outcome" => BatcherMetrics::OUTCOME_REQUEUED).increment(1);
                            warn!(id = %id.0, "submission txpool-blocked during drain, abandoning");
                        }
                    }
                }
            }
        }
    }

    /// Discard all in-flight futures, returning their semaphore permits.
    ///
    /// Used on reorg to prevent stale completions from modifying the freshly
    /// reset pipeline.
    pub fn discard(&mut self) {
        let discarded = self.in_flight.len();
        if discarded > 0 {
            warn!(discarded = %discarded, "discarding in-flight submissions due to reorg");
            gauge!(BatcherMetrics::IN_FLIGHT_SUBMISSIONS).set(0.0);
        }
        self.in_flight = FuturesUnordered::new();
    }

    /// Returns a future for the next settled `(id, outcome)` pair.
    ///
    /// Resolves immediately to `None` when in-flight is empty; safe to use as
    /// a `select!` arm with a `Some(...)` pattern guard.
    pub fn next_settled(&mut self) -> impl Future<Output = Option<(SubmissionId, TxOutcome)>> + '_ {
        self.in_flight.next()
    }

    /// Returns the number of currently in-flight submissions.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }
}
