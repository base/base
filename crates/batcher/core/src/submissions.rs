//! Submission lifecycle management for the batch driver.

use std::{future::Future, pin::Pin, sync::Arc};

use alloy_primitives::{Address, Bytes, U256};
use base_batcher_encoder::{
    BatchPipeline, BatcherMetrics, BlobPayload, DaEgress, DaType, EncoderConfig, FrameEncoder,
    SubmissionId, SubmissionPayload,
};
use base_blobs::{BlobEncodeError, BlobEncoder};
use base_protocol::Frame;
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use futures::stream::{FuturesUnordered, StreamExt};
use tracing::{info, warn};

use crate::TxOutcome;

/// Receipt futures of the transactions in flight, each resolving to the submission it
/// carries and how its transaction settled.
type InFlight = FuturesUnordered<Pin<Box<dyn Future<Output = (SubmissionId, TxOutcome)> + Send>>>;

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
            payload_size += 1 + payload.frame_bytes();
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

    /// Build a calldata transaction candidate carrying one version-prefixed frame.
    pub fn calldata_tx_candidate(inbox: Address, frame: &Frame) -> TxCandidate {
        TxCandidate {
            to: Some(inbox),
            tx_data: FrameEncoder::to_calldata(frame),
            value: U256::ZERO,
            gas_limit: 0,
            blobs: Arc::from([]),
        }
    }
}

/// Sends ready submissions to L1, one transaction each, and tracks those transactions until
/// they settle.
///
/// At most `max_pending` transactions are in flight at once. A
/// [`TxOutcome::TxpoolBlocked`] outcome suspends sending until
/// [`recover_txpool`](Self::recover_txpool) cancels the transaction holding the nonce slot.
#[derive(Debug)]
pub struct SubmissionQueue<TM: TxManager> {
    tx_manager: TM,
    in_flight: InFlight,
    max_pending: usize,
    inbox: Address,
    txpool_blocked: bool,
}

impl<TM: TxManager> SubmissionQueue<TM> {
    /// Create a new [`SubmissionQueue`].
    pub fn new(tx_manager: TM, inbox: Address, max_pending: usize) -> Self {
        Self {
            tx_manager,
            in_flight: FuturesUnordered::new(),
            max_pending,
            inbox,
            txpool_blocked: false,
        }
    }

    /// Send ready submissions, one L1 transaction each, until `max_pending` transactions are
    /// in flight, the pipeline has nothing ready, or the txpool is blocked.
    ///
    /// Fails when a blob submission cannot be built into a transaction. The encoder packs
    /// blobs within protocol limits, so such a failure is a bug that a retry would repeat.
    pub async fn submit_pending<P: BatchPipeline>(
        &mut self,
        pipeline: &mut P,
    ) -> Result<(), BatchTxCandidateError> {
        while !self.txpool_blocked && self.in_flight.len() < self.max_pending {
            let Some(sub) = pipeline.next_submission() else {
                return Ok(());
            };

            // Build the transaction.
            let da_type_label = match sub.da_type() {
                DaType::Blob => BatcherMetrics::DA_TYPE_BLOB,
                DaType::Calldata => BatcherMetrics::DA_TYPE_CALLDATA,
            };
            let (candidate, blob_payload_bytes) = match sub.payload() {
                SubmissionPayload::Blobs(payloads) => {
                    let (candidate, payload_size) =
                        BatchTxCandidateBuilder::blob_tx_candidate(self.inbox, payloads)?;
                    BatcherMetrics::blobs_per_tx().record(payloads.len() as f64);
                    for payload in payloads {
                        BatcherMetrics::blob_fill_ratio()
                            .record(payload.frame_bytes() as f64 / DaEgress::BLOB_CAPACITY as f64);
                    }
                    (candidate, Some(payload_size))
                }
                SubmissionPayload::Calldata(frame) => {
                    (BatchTxCandidateBuilder::calldata_tx_candidate(self.inbox, frame), None)
                }
            };

            let id = sub.id;
            let frame_bytes = sub.frame_bytes();
            info!(
                id = ?id,
                da_type = %da_type_label,
                frame_bytes = %frame_bytes,
                "submitting batch frames to L1"
            );
            BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_SUBMITTED).increment(1);
            BatcherMetrics::da_bytes_submitted_total(da_type_label).increment(frame_bytes as u64);
            BatcherMetrics::in_flight_submissions().increment(1.0);

            // Send it, and track its receipt until the transaction settles.
            let handle = self.tx_manager.send_async(candidate).await;
            self.in_flight.push(Box::pin(async move {
                let outcome = match handle.await {
                    Ok(receipt) => {
                        let l1_block = receipt.block_number.unwrap_or_else(|| {
                            warn!(id = ?id, "confirmed receipt missing block number; l1_head will not advance");
                            0
                        });
                        // Count blob bytes only once they land on L1, not when attempted.
                        if let Some(bytes) = blob_payload_bytes {
                            BatcherMetrics::blob_used_bytes_total().increment(bytes);
                        }
                        TxOutcome::Confirmed { l1_block }
                    }
                    Err(TxManagerError::AlreadyReserved) => {
                        warn!(id = ?id, "txpool nonce slot already reserved");
                        TxOutcome::TxpoolBlocked
                    }
                    Err(e) => {
                        warn!(id = ?id, error = %e, "submission failed");
                        TxOutcome::Failed
                    }
                };
                (id, outcome)
            }));
        }
        Ok(())
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

    /// Report a settled transaction to the pipeline.
    ///
    /// A confirmation confirms the submission and advances the pipeline's L1 head to the
    /// inclusion block. A failure requeues the submission. A txpool blockage requeues it too,
    /// and suspends sending until [`recover_txpool`](Self::recover_txpool) succeeds.
    pub fn handle_outcome<P: BatchPipeline>(
        &mut self,
        pipeline: &mut P,
        id: SubmissionId,
        outcome: TxOutcome,
    ) {
        BatcherMetrics::in_flight_submissions().decrement(1.0);
        match outcome {
            TxOutcome::Confirmed { l1_block } => {
                pipeline.confirm(id, l1_block);
                pipeline.advance_l1_head(l1_block);
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_CONFIRMED).increment(1);
                info!(id = ?id, l1_block = %l1_block, "submission confirmed on L1");
            }
            TxOutcome::Failed => {
                pipeline.requeue(id);
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_FAILED).increment(1);
            }
            TxOutcome::TxpoolBlocked => {
                pipeline.requeue(id);
                self.txpool_blocked = true;
                BatcherMetrics::submission_total(BatcherMetrics::OUTCOME_REQUEUED).increment(1);
            }
        }
    }

    /// Report the in-flight transactions to the pipeline as they settle, until none is left
    /// or `timeout` fires.
    ///
    /// Used on shutdown: the transactions still in flight at the timeout are abandoned.
    pub async fn drain<P: BatchPipeline>(
        &mut self,
        pipeline: &mut P,
        mut timeout: impl Future<Output = ()> + Unpin,
    ) {
        while !self.in_flight.is_empty() {
            tokio::select! {
                () = &mut timeout => {
                    warn!(
                        remaining = %self.in_flight.len(),
                        "drain timeout reached, abandoning in-flight submissions"
                    );
                    return;
                }
                Some((id, outcome)) = self.in_flight.next() => {
                    self.handle_outcome(pipeline, id, outcome);
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::*;

    #[test]
    fn blob_candidate_rejects_empty_transaction() {
        assert!(matches!(
            BatchTxCandidateBuilder::blob_tx_candidate(Address::ZERO, &[]),
            Err(BatchTxCandidateError::InvalidBlobCount { count: 0, .. })
        ));
    }
}
