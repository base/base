//! Submission lifecycle management for the batch driver.

use std::{future::Future, pin::Pin, sync::Arc};

use alloy_primitives::{Address, Bytes, U256};
use base_batcher_encoder::{BatchPipeline, BatcherMetrics, DaType, FrameEncoder, SubmissionId};
use base_blobs::{BlobEncodeError, BlobEncoder};
use base_protocol::Frame;
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::{sync::Semaphore, task::JoinSet};
use tracing::{info, warn};

use crate::{DynAltDaClient, TxOutcome};

/// Max concurrent alt-DA shadow writes detached from the primary submission semaphore.
const ALT_DA_MAX_IN_FLIGHT: usize = 2;

/// Type alias for the in-flight receipt future collection.
type InFlight =
    FuturesUnordered<Pin<Box<dyn Future<Output = (Vec<SubmissionId>, TxOutcome)> + Send>>>;

/// Builds L1 transaction candidates for batch submissions.
#[derive(Debug)]
pub struct BatchTxCandidateBuilder;

impl BatchTxCandidateBuilder {
    /// Build a blob transaction candidate from batch frames.
    ///
    /// Each frame is encoded as one Base blob payload and the returned byte
    /// count is the total payload bytes submitted across all blobs.
    pub fn blob_tx_candidate(
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
/// batcher inbox address. These were previously loose fields on [`BatchDriver`].
#[derive(Debug)]
pub struct SubmissionQueue<TM: TxManager + 'static> {
    tx_manager: Arc<TM>,
    in_flight: InFlight,
    semaphore: Arc<Semaphore>,
    inbox: Address,
    txpool_blocked: bool,
    alt_da: Option<DynAltDaClient>,
    /// Limits concurrent shadow PUT + commitment work without touching primary capacity.
    alt_da_semaphore: Option<Arc<Semaphore>>,
    /// Detached alt-DA shadow-write tasks, tracked so a reorg can cancel them.
    alt_da_tasks: JoinSet<()>,
}

impl<TM: TxManager + 'static> SubmissionQueue<TM> {
    /// Create a new [`SubmissionQueue`].
    pub fn new(
        tx_manager: TM,
        inbox: Address,
        max_pending: usize,
        alt_da: Option<DynAltDaClient>,
    ) -> Self {
        let alt_da_semaphore =
            alt_da.is_some().then(|| Arc::new(Semaphore::new(ALT_DA_MAX_IN_FLIGHT)));
        Self {
            tx_manager: Arc::new(tx_manager),
            in_flight: FuturesUnordered::new(),
            semaphore: Arc::new(Semaphore::new(max_pending)),
            inbox,
            txpool_blocked: false,
            alt_da,
            alt_da_semaphore,
            alt_da_tasks: JoinSet::new(),
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

            let da_type_label = match sub.da_type {
                DaType::Blob => BatcherMetrics::DA_TYPE_BLOB,
                DaType::Calldata => BatcherMetrics::DA_TYPE_CALLDATA,
            };
            let blob_payload_bytes;
            let candidate = match sub.da_type {
                DaType::Blob => {
                    match BatchTxCandidateBuilder::blob_tx_candidate(self.inbox, &sub.frames) {
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
            let frame_bytes = sub.frames.iter().map(|f| f.data.len()).sum::<usize>();
            let alt_da_body = match sub.da_type {
                DaType::Calldata if self.alt_da.is_some() => Some(candidate.tx_data.clone()),
                _ => None,
            };
            let shadow_ids = alt_da_body.as_ref().map(|_| vec![sub.id]);
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

            if let Some(body) = alt_da_body {
                // alt_da_body is only Some when self.alt_da.is_some(); surface the invariant
                // rather than silently dropping the shadow write if that ever changes.
                let alt_da = self.alt_da.clone().expect("alt_da present when alt_da_body is Some");
                let shadow_ids = shadow_ids.expect("shadow ids when alt-da body present");
                let alt_da_semaphore =
                    self.alt_da_semaphore.as_ref().expect("shadow semaphore when alt-da enabled");
                if let Ok(alt_da_permit) = Arc::clone(alt_da_semaphore).try_acquire_owned() {
                    let tx_manager = Arc::clone(&self.tx_manager);
                    let inbox = self.inbox;
                    // Reap finished shadow tasks so the JoinSet stays bounded.
                    while self.alt_da_tasks.try_join_next().is_some() {}
                    self.alt_da_tasks.spawn(async move {
                        let _alt_da_permit = alt_da_permit;
                        let commitment = match alt_da.put(body).await {
                            Ok(commitment) => commitment,
                            Err(error) => {
                                // A failed PUT leaves this batch absent from S3. Harmless while
                                // calldata is primary, but it is a gap that would block the S3-only
                                // cutover, so surface it as a metric, not just a log line.
                                warn!(submission_ids = ?shadow_ids, %error, "alt-da put failed; calldata submission unchanged");
                                BatcherMetrics::alt_da_commitment_total(
                                    BatcherMetrics::OUTCOME_PUT_FAILED,
                                )
                                .increment(1);
                                return;
                            }
                        };

                        let tx_data = commitment.encode_tx_data();
                        info!(
                            submission_ids = ?shadow_ids,
                            commitment_bytes = %tx_data.len(),
                            "submitting alt-da commitment to L1"
                        );
                        let candidate = TxCandidate {
                            to: Some(inbox),
                            tx_data,
                            value: U256::ZERO,
                            gas_limit: 0,
                            blobs: vec![].into(),
                        };

                        // Shadow commitments share the primary TxManager signer + nonce pool today:
                        // a slow PUT/commitment can stall primary submissions, and an abort_all
                        // between these two awaits can leave a reserved nonce unconfirmed. Accepted
                        // for the calldata-primary shadow phase; a separate shadow signer is the
                        // S3-only cutover follow-up (PRIV-1972).
                        match tx_manager.send_async(candidate).await.await {
                            Ok(receipt) => {
                                let l1_block = receipt.block_number.unwrap_or_else(|| {
                                    warn!(
                                        submission_ids = ?shadow_ids,
                                        "alt-da commitment receipt missing block number"
                                    );
                                    0
                                });
                                info!(
                                    submission_ids = ?shadow_ids,
                                    l1_block = %l1_block,
                                    "alt-da commitment confirmed on L1"
                                );
                                BatcherMetrics::alt_da_commitment_total(
                                    BatcherMetrics::OUTCOME_CONFIRMED,
                                )
                                .increment(1);
                            }
                            Err(error) => {
                                warn!(
                                    submission_ids = ?shadow_ids,
                                    %error,
                                    "alt-da commitment submission failed"
                                );
                                BatcherMetrics::alt_da_commitment_total(
                                    BatcherMetrics::OUTCOME_FAILED,
                                )
                                .increment(1);
                            }
                        }
                    });
                } else {
                    warn!(
                        submission_ids = ?shadow_ids,
                        "alt-da shadow write skipped: shadow capacity exhausted"
                    );
                    BatcherMetrics::alt_da_commitment_total(BatcherMetrics::OUTCOME_SKIPPED)
                        .increment(1);
                }
            }
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
        // Shadow commitments are not tied to pipeline confirmation; cancel on shutdown.
        self.alt_da_tasks.abort_all();

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
        // Cancel pending shadow commitments so a reorged batch can't still land on L1.
        self.alt_da_tasks.abort_all();
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
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, Bloom, Bytes};
    use alloy_rpc_types_eth::TransactionReceipt;
    use async_trait::async_trait;
    use base_batcher_encoder::{
        BatchPipeline, BatchSubmission, DaType, DerivationReconciliation, StepError, StepResult,
        SubmissionId,
    };
    use base_protocol::{
        BlockInfo, ChannelId, DERIVATION_VERSION_0, DERIVATION_VERSION_1, Frame,
    };
    use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager};
    use tokio::sync::oneshot;

    use super::SubmissionQueue;
    use crate::{
        AltDaClient, AltDaError, DynAltDaClient, GENERIC_COMMITMENT_SENTINEL,
        GENERIC_COMMITMENT_TYPE, GenericCommitment, TxOutcome,
        test_utils::{NeverConfirmTxManager, Recorded, SubmissionStub, TrackingPipeline},
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

        let mut queue = SubmissionQueue::new(NeverConfirmTxManager, Address::ZERO, 2, None);

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

        let mut queue = SubmissionQueue::new(NeverConfirmTxManager, Address::ZERO, 1, None);

        let drained = queue.submit_pending(&mut pipeline).await;

        assert!(!drained, "a ready submission is still waiting on semaphore capacity");
        assert_eq!(recorded.lock().unwrap().dequeued.len(), 1, "only the single permit is used");
    }

    fn sample_commitment() -> GenericCommitment {
        let mut commitment = [0u8; 34];
        commitment[0] = GENERIC_COMMITMENT_TYPE;
        commitment[1] = GENERIC_COMMITMENT_SENTINEL;
        commitment[2..].copy_from_slice(&[0xab; 32]);
        GenericCommitment::new(commitment)
    }

    #[derive(Debug)]
    struct RecordingAltDaClient {
        puts: Arc<Mutex<Vec<Bytes>>>,
    }

    impl Default for RecordingAltDaClient {
        fn default() -> Self {
            Self { puts: Arc::new(Mutex::new(Vec::new())) }
        }
    }

    #[async_trait]
    impl AltDaClient for RecordingAltDaClient {
        async fn put(&self, body: Bytes) -> Result<GenericCommitment, AltDaError> {
            self.puts.lock().unwrap().push(body);
            Ok(sample_commitment())
        }
    }

    #[derive(Debug)]
    struct CountingTxManager {
        sends: Arc<AtomicUsize>,
        commitment_sends: Arc<AtomicUsize>,
    }

    impl CountingTxManager {
        fn new() -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let sends = Arc::new(AtomicUsize::new(0));
            let commitment_sends = Arc::new(AtomicUsize::new(0));
            (
                Self { sends: Arc::clone(&sends), commitment_sends: Arc::clone(&commitment_sends) },
                sends,
                commitment_sends,
            )
        }

        fn is_commitment(candidate: &TxCandidate) -> bool {
            candidate.tx_data.first() == Some(&DERIVATION_VERSION_1)
        }
    }

    impl TxManager for CountingTxManager {
        async fn send(&self, _: TxCandidate) -> SendResponse {
            unreachable!()
        }

        fn send_async(
            &self,
            candidate: TxCandidate,
        ) -> impl std::future::Future<Output = SendHandle> + Send {
            self.sends.fetch_add(1, Ordering::SeqCst);
            if Self::is_commitment(&candidate) {
                self.commitment_sends.fetch_add(1, Ordering::SeqCst);
            }
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Ok(stub_receipt()));
            std::future::ready(SendHandle::new(rx))
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    fn stub_receipt() -> TransactionReceipt {
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
            transaction_hash: alloy_primitives::B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(alloy_primitives::B256::ZERO),
            block_number: Some(1),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        }
    }

    #[derive(Debug, Default)]
    struct CalldataPipeline {
        submissions: std::collections::VecDeque<BatchSubmission>,
    }

    impl BatchPipeline for CalldataPipeline {
        fn add_block(
            &mut self,
            _: base_common_consensus::BaseBlock,
        ) -> Result<(), (base_batcher_encoder::ReorgError, Box<base_common_consensus::BaseBlock>)>
        {
            Ok(())
        }

        fn step(&mut self) -> Result<StepResult, StepError> {
            Ok(StepResult::Idle)
        }

        fn next_submission(&mut self) -> Option<BatchSubmission> {
            self.submissions.pop_front()
        }

        fn has_ready_submission(&self) -> bool {
            !self.submissions.is_empty()
        }

        fn confirm(&mut self, _: SubmissionId, _: u64) {}

        fn requeue(&mut self, _: SubmissionId) {}

        fn force_close_channel(&mut self) {}

        fn advance_l1_head(&mut self, _: u64) {}

        fn reconcile_derivation(
            &mut self,
            _: BlockInfo,
            _: Option<u64>,
        ) -> DerivationReconciliation {
            DerivationReconciliation::Consistent
        }

        fn reset(&mut self) {}

        fn da_backlog_bytes(&self) -> u64 {
            0
        }
    }

    fn calldata_submission() -> BatchSubmission {
        BatchSubmission {
            id: SubmissionId(1),
            channel_id: ChannelId::default(),
            da_type: DaType::Calldata,
            frames: vec![Arc::new(Frame { data: vec![1, 2, 3], ..Frame::default() })],
        }
    }

    #[tokio::test]
    async fn alt_da_shadow_write_puts_bytes_and_posts_commitment() {
        let client = Arc::new(RecordingAltDaClient::default());
        let puts = Arc::clone(&client.puts);
        let alt_da: DynAltDaClient = client;
        let (tx_manager, sends, commitment_sends) = CountingTxManager::new();
        let mut queue = SubmissionQueue::new(tx_manager, Address::ZERO, 1, Some(alt_da));
        let mut pipeline = CalldataPipeline {
            submissions: std::collections::VecDeque::from([calldata_submission()]),
        };

        queue.submit_pending(&mut pipeline).await;
        let (ids, outcome) = queue.next_settled().await.expect("primary submission in flight");
        assert_eq!(ids, vec![SubmissionId(1)]);
        assert!(matches!(outcome, TxOutcome::Confirmed { .. }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(sends.load(Ordering::SeqCst), 2);
        assert_eq!(commitment_sends.load(Ordering::SeqCst), 1);
        let puts = puts.lock().unwrap();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].first(), Some(&DERIVATION_VERSION_0));
    }

    #[derive(Debug)]
    struct GatedAltDaClient {
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl AltDaClient for GatedAltDaClient {
        async fn put(&self, _: Bytes) -> Result<GenericCommitment, AltDaError> {
            self.gate.notified().await;
            Ok(sample_commitment())
        }
    }

    #[tokio::test]
    async fn drain_aborts_pending_shadow_tasks() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let alt_da: DynAltDaClient = Arc::new(GatedAltDaClient { gate });
        let (tx_manager, _, commitment_sends) = CountingTxManager::new();
        let mut queue = SubmissionQueue::new(tx_manager, Address::ZERO, 1, Some(alt_da));
        let mut pipeline = CalldataPipeline {
            submissions: std::collections::VecDeque::from([calldata_submission()]),
        };

        queue.submit_pending(&mut pipeline).await;
        let timeout = Box::pin(std::future::ready(()));
        queue.drain(&mut pipeline, timeout).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            commitment_sends.load(Ordering::SeqCst),
            0,
            "drain should abort shadow tasks before commitment txs are sent"
        );
    }
}
