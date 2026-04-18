//! Submission lifecycle management for the batch driver.

use std::{future::Future, pin::Pin, sync::Arc};

use alloy_primitives::{Address, Bytes, U256};
use base_batcher_encoder::{BatchPipeline, BatcherMetrics, DaType, FrameEncoder, SubmissionId};
use base_blobs::BlobEncoder;
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::TxOutcome;

/// Type alias for the in-flight receipt future collection.
type InFlight =
    FuturesUnordered<Pin<Box<dyn Future<Output = (Vec<SubmissionId>, TxOutcome)> + Send>>>;

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
    /// For each available semaphore permit (= one L1 transaction), packs as many
    /// pending frames as fit into a single blob payload (up to
    /// [`BlobEncoder::BLOB_MAX_DATA_SIZE`] bytes), then submits one L1 tx carrying
    /// that blob. Loops until the semaphore is exhausted, the pipeline has no
    /// ready submissions, or the txpool is blocked.
    pub async fn submit_pending<P: BatchPipeline>(&mut self, pipeline: &mut P) {
        loop {
            if self.txpool_blocked {
                break;
            }
            let Ok(permit) = Arc::clone(&self.semaphore).try_acquire_owned() else {
                break;
            };

            // Collect as many submissions as fit into one blob payload.
            // payload_size tracks: 1 (DERIVATION_VERSION_0) + sum of frame.encode() sizes.
            let mut ids: Vec<SubmissionId> = Vec::new();
            let mut frames = Vec::new();
            let mut payload_size: usize = 1; // DERIVATION_VERSION_0 prefix
            let mut frame_bytes: usize = 0;
            let mut da_type = DaType::Blob;

            while let Some(sub) = pipeline.next_submission() {
                // Calculate the encoded byte cost of this submission's frames.
                let sub_frame_size: usize =
                    sub.frames.iter().map(|f| BlobEncoder::FRAME_OVERHEAD + f.data.len()).sum();

                // If the blob already has at least one submission and this one doesn't fit,
                // put it back and stop packing.
                if !ids.is_empty()
                    && payload_size + sub_frame_size > BlobEncoder::BLOB_MAX_DATA_SIZE
                {
                    pipeline.requeue(sub.id);
                    break;
                }

                if ids.is_empty() {
                    da_type = sub.da_type;
                } else if sub.da_type != da_type {
                    pipeline.requeue(sub.id);
                    break;
                }
                frame_bytes += sub.frames.iter().map(|f| f.data.len()).sum::<usize>();
                payload_size += sub_frame_size;
                ids.push(sub.id);
                frames.extend(sub.frames);

                // Calldata mode: exactly one frame per L1 transaction (protocol requirement).
                if matches!(da_type, DaType::Calldata) {
                    break;
                }
            }

            if ids.is_empty() {
                drop(permit);
                break;
            }

            let da_type_label = match da_type {
                DaType::Blob => BatcherMetrics::DA_TYPE_BLOB,
                DaType::Calldata => BatcherMetrics::DA_TYPE_CALLDATA,
            };
            let candidate = match da_type {
                DaType::Blob => match BlobEncoder::encode_packed(&frames) {
                    Ok(blob) => TxCandidate {
                        to: Some(self.inbox),
                        tx_data: Bytes::new(),
                        value: U256::ZERO,
                        gas_limit: 0,
                        blobs: Arc::from(vec![blob]),
                    },
                    Err(e) => {
                        warn!(error = %e, "failed to encode frames to blob, requeueing");
                        for id in ids {
                            pipeline.requeue(id);
                        }
                        drop(permit);
                        continue;
                    }
                },
                DaType::Calldata => TxCandidate {
                    to: Some(self.inbox),
                    tx_data: FrameEncoder::to_calldata(&frames[0]),
                    value: U256::ZERO,
                    gas_limit: 0,
                    blobs: vec![].into(),
                },
            };
            info!(
                submissions = %ids.len(),
                da_type = %da_type_label,
                frame_bytes = %frame_bytes,
                "submitting packed batch frames to L1"
            );
            BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_SUBMITTED)
                .increment(ids.len() as u64);
            BatcherMetrics::da_bytes_submitted_total(da_type_label).increment(frame_bytes as u64);
            BatcherMetrics::in_flight_submissions().increment(1.0);
            // Capture for the post-confirm metric: blob_used_bytes_total counts
            // payload bytes that actually landed on L1, not bytes attempted, so
            // we only increment after the tx confirms.
            let blob_payload_bytes = matches!(da_type, DaType::Blob).then_some(payload_size as u64);
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
                    (ids, outcome)
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
    /// On confirmation, calls `pipeline.confirm` for each packed submission and
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
                warn!(submissions = %count, "submission failed, requeued for retry");
            }
            TxOutcome::TxpoolBlocked => {
                let count = ids.len();
                for id in ids {
                    pipeline.requeue(id);
                }
                self.txpool_blocked = true;
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_REQUEUED)
                    .increment(count as u64);
                warn!(submissions = %count, "submission blocked by txpool nonce slot, requeued");
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
    /// Used on reorg to prevent stale completions from modifying the freshly
    /// reset pipeline.
    pub fn discard(&mut self) {
        let discarded = self.in_flight.len();
        if discarded > 0 {
            warn!(discarded = %discarded, "discarding in-flight submissions due to reorg");
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
