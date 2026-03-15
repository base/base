//! The async batch driver that orchestrates encoding, block sourcing, and L1 submission.

use std::{future::Future, pin::Pin, sync::Arc};

use alloy_primitives::{Address, Bytes, U256};
use base_batcher_encoder::{BatchPipeline, DaType, StepResult, SubmissionId};
use base_batcher_source::{L2BlockEvent, UnsafeBlockSource};
use base_blobs::BlobEncoder;
use base_protocol::DERIVATION_VERSION_0;
use base_tx_manager::{TxCandidate, TxManager};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{BatchDriverError, ThrottleClient, ThrottleController, ThrottleParams, TxOutcome};

/// Type alias for the in-flight receipt future collection.
type InFlight = FuturesUnordered<Pin<Box<dyn Future<Output = (SubmissionId, TxOutcome)> + Send>>>;

/// Async orchestration loop for the batcher.
///
/// Combines a [`BatchPipeline`] (encoding), an [`UnsafeBlockSource`] (L2 block delivery),
/// and a [`TxManager`] (L1 submission) into a single `tokio::select!` task.
///
/// Uses [`FuturesUnordered`] for concurrent receipt tracking and a [`Semaphore`]
/// for pending transaction backpressure.
#[derive(Debug)]
pub struct BatchDriver<P, S, TM, TC>
where
    P: BatchPipeline,
    S: UnsafeBlockSource,
    TM: TxManager,
    TC: ThrottleClient,
{
    /// The encoding pipeline.
    pipeline: P,
    /// The L2 block source.
    source: S,
    /// The L1 transaction manager.
    tx_manager: TM,
    /// The batcher inbox address on L1.
    inbox: Address,
    /// In-flight receipt futures.
    in_flight: InFlight,
    /// Limits concurrent in-flight transactions.
    semaphore: Arc<Semaphore>,
    /// DA backlog throttle controller.
    throttle: ThrottleController,
    /// Client for applying DA size limits to the block builder.
    throttle_client: TC,
    /// Last applied DA limits (`max_tx_size`, `max_block_size`) to avoid redundant RPC calls.
    last_applied_da_limits: Option<(u64, u64)>,
}

/// Maximum number of encoding steps to run synchronously per outer loop iteration
/// before yielding to the tokio executor. Prevents a large block backlog from
/// starving receipt processing and cancellation checks.
const STEP_BUDGET: usize = 128;

impl<P, S, TM, TC> BatchDriver<P, S, TM, TC>
where
    P: BatchPipeline,
    S: UnsafeBlockSource,
    TM: TxManager,
    TC: ThrottleClient,
{
    /// Create a new [`BatchDriver`].
    pub fn new(
        pipeline: P,
        source: S,
        tx_manager: TM,
        inbox: Address,
        max_pending_transactions: usize,
        throttle: ThrottleController,
        throttle_client: TC,
    ) -> Self {
        Self {
            pipeline,
            source,
            tx_manager,
            inbox,
            in_flight: FuturesUnordered::new(),
            semaphore: Arc::new(Semaphore::new(max_pending_transactions)),
            throttle,
            throttle_client,
            last_applied_da_limits: None,
        }
    }

    /// Run the batch driver loop.
    ///
    /// This method drives the full batcher lifecycle:
    /// 1. Drains encoding steps synchronously.
    /// 2. Submits any ready frames non-blocking (`try_acquire_owned`).
    /// 3. Selects on cancellation, block source events, and receipt completion.
    /// 4. Returns `Ok(())` on cancellation or `Err` on source/encoding failure.
    pub async fn run(mut self, cancellation: CancellationToken) -> Result<(), BatchDriverError> {
        loop {
            // Drain encoding steps synchronously before I/O. A budget prevents
            // a large block backlog from starving the tokio executor: after
            // STEP_BUDGET steps we break and yield at the select! below, then
            // resume encoding on the next outer loop iteration.
            let mut budget = STEP_BUDGET;
            loop {
                match self.pipeline.step() {
                    Ok(StepResult::Idle) => break,
                    Ok(StepResult::BlockEncoded | StepResult::ChannelClosed) => {
                        budget -= 1;
                        if budget == 0 {
                            break;
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "fatal encoding step error, batcher halting");
                        return Err(e.into());
                    }
                }
            }

            // Check DA backlog and apply throttle if needed.
            let backlog_bytes = self.pipeline.da_backlog_bytes();
            let throttle_params = self.throttle.update(backlog_bytes);
            let is_throttling = throttle_params.as_ref().is_some_and(ThrottleParams::is_throttling);

            let (max_tx_size, max_block_size) = throttle_params.as_ref().map_or_else(
                || {
                    (
                        self.throttle.config().tx_size_upper_limit,
                        self.throttle.config().block_size_upper_limit,
                    )
                },
                |p| (p.max_tx_size, p.max_block_size),
            );

            let new_limits = (max_tx_size, max_block_size);
            if self.last_applied_da_limits != Some(new_limits) {
                if let Err(e) =
                    self.throttle_client.set_max_da_size(max_tx_size, max_block_size).await
                {
                    warn!(error = %e, "failed to apply DA size limits to block builder");
                } else {
                    if is_throttling {
                        info!(
                            intensity = throttle_params.as_ref().unwrap().intensity,
                            max_block_size, max_tx_size, "DA throttle activated"
                        );
                    } else {
                        info!(max_block_size, max_tx_size, "DA throttle deactivated, limits reset");
                    }
                    self.last_applied_da_limits = Some(new_limits);
                }
            }

            // Submit all ready frames without blocking. Using try_acquire_owned
            // avoids the hot-loop that would occur if acquire_owned were polled
            // inside select! when there are no submissions ready: the semaphore
            // would fire immediately on the next iteration, loop forever. It also
            // keeps next_submission() and send_async out of the select! body,
            // preventing pipeline state mutation from being interleaved with other
            // async events mid-select.
            loop {
                let Ok(permit) = Arc::clone(&self.semaphore).try_acquire_owned() else {
                    break; // All slots occupied; wait for a receipt to free one.
                };
                let Some(sub) = self.pipeline.next_submission() else {
                    drop(permit);
                    break; // Nothing to submit; wait for more encoding work.
                };
                let id = sub.id;
                let candidate = match sub.da_type {
                    DaType::Blob => match BlobEncoder::encode_frames(&sub.frames) {
                        Ok(blobs) => TxCandidate {
                            to: Some(self.inbox),
                            tx_data: Bytes::new(),
                            value: U256::ZERO,
                            gas_limit: 0,
                            blobs,
                        },
                        Err(e) => {
                            warn!(id = %id.0, error = %e, "failed to encode frames to blobs, requeueing");
                            self.pipeline.requeue(id);
                            drop(permit);
                            continue;
                        }
                    },
                    DaType::Calldata => {
                        // Calldata mode: one frame per submission (enforced by
                        // EncoderConfig::validate). Encode as
                        // [DERIVATION_VERSION_0] ++ frame.encode().
                        let frame = &sub.frames[0];
                        let encoded = frame.encode();
                        let mut data = Vec::with_capacity(1 + encoded.len());
                        data.push(DERIVATION_VERSION_0);
                        data.extend_from_slice(&encoded);
                        TxCandidate {
                            to: Some(self.inbox),
                            tx_data: Bytes::from(data),
                            value: U256::ZERO,
                            gas_limit: 0,
                            blobs: vec![],
                        }
                    }
                };
                {
                    let handle = self.tx_manager.send_async(candidate).await;
                    let fut: Pin<Box<dyn Future<Output = (SubmissionId, TxOutcome)> + Send>> =
                        Box::pin(async move {
                            let outcome = match handle.await {
                                Ok(receipt) => {
                                    let l1_block = receipt.block_number.unwrap_or_else(|| {
                                        warn!(id = %id.0, "confirmed receipt missing block number; l1_head will not advance");
                                        0
                                    });
                                    TxOutcome::Confirmed { l1_block }
                                }
                                Err(e) => {
                                    warn!(id = %id.0, error = %e, "submission failed");
                                    TxOutcome::Failed
                                }
                            };
                            drop(permit);
                            (id, outcome)
                        });
                    self.in_flight.push(fut);
                }
            }

            // Block on I/O: cancellation, new blocks, or receipt completions.
            tokio::select! {
                biased;

                _ = cancellation.cancelled() => {
                    info!("batcher driver cancelled");
                    return Ok(());
                }

                event = self.source.next() => {
                    match event? {
                        L2BlockEvent::Block(block) => {
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
                                    self.in_flight = FuturesUnordered::new();
                                    self.pipeline.reset();
                                    // Re-add the triggering block. After reset the block
                                    // queue is empty, so the parent-hash check is skipped
                                    // and the block is always accepted. This prevents the
                                    // block from being silently lost when the source won't
                                    // re-deliver it (e.g. HybridBlockSource deduplication).
                                    let _ = self.pipeline.add_block(*block);
                                }
                            }
                        }
                        L2BlockEvent::Reorg { new_safe_head } => {
                            warn!(
                                head = %new_safe_head.block_info.number,
                                "L2 reorg detected, resetting pipeline"
                            );
                            // Discard in-flight futures before reset. The underlying L1
                            // transactions are already broadcast and cannot be recalled,
                            // but we must not let their completions call confirm/requeue
                            // on the freshly-reset pipeline. Dropping the futures also
                            // returns their semaphore permits.
                            self.in_flight = FuturesUnordered::new();
                            self.pipeline.reset();
                        }
                    }
                }

                Some((id, outcome)) = self.in_flight.next() => {
                    match outcome {
                        TxOutcome::Confirmed { l1_block } => {
                            self.pipeline.confirm(id, l1_block);
                            self.pipeline.advance_l1_head(l1_block);
                            debug!(id = %id.0, l1_block = %l1_block, "submission confirmed");
                        }
                        TxOutcome::Failed => {
                            self.pipeline.requeue(id);
                            warn!(id = %id.0, "submission failed, requeued");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fmt,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, B256, Bloom};
    use alloy_rpc_types_eth::TransactionReceipt;
    use async_trait::async_trait;
    use base_alloy_consensus::OpBlock;
    use base_batcher_encoder::{
        BatchPipeline, BatchSubmission, ReorgError, StepError, StepResult, SubmissionId,
    };
    use base_batcher_source::{L2BlockEvent, SourceError, UnsafeBlockSource};
    use base_protocol::{ChannelId, Frame};
    use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError};
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use super::BatchDriver;
    use crate::{
        NoopThrottleClient, ThrottleConfig, ThrottleController, ThrottleStrategy,
        test_utils::TrackingThrottleClient,
    };

    // ---- Shared recording state ----

    #[derive(Debug, Default)]
    struct Recorded {
        l1_heads: Vec<u64>,
        requeued: Vec<SubmissionId>,
        /// Submission IDs in the order they were dequeued via `next_submission()`.
        dequeued: Vec<SubmissionId>,
        /// Number of times `reset()` was called.
        resets: usize,
    }

    // ---- Pipeline that records advance_l1_head calls via shared state ----

    #[derive(Debug)]
    struct TrackingPipeline {
        recorded: Arc<Mutex<Recorded>>,
        submissions: std::collections::VecDeque<BatchSubmission>,
        /// Value returned by `da_backlog_bytes()`. Default: 0.
        da_backlog_bytes_value: u64,
    }

    impl TrackingPipeline {
        fn new(recorded: Arc<Mutex<Recorded>>) -> Self {
            Self { recorded, submissions: Default::default(), da_backlog_bytes_value: 0 }
        }

        fn with_da_backlog(mut self, value: u64) -> Self {
            self.da_backlog_bytes_value = value;
            self
        }
    }

    impl BatchPipeline for TrackingPipeline {
        fn add_block(&mut self, _: OpBlock) -> Result<(), (ReorgError, Box<OpBlock>)> {
            Ok(())
        }
        fn step(&mut self) -> Result<StepResult, StepError> {
            Ok(StepResult::Idle)
        }
        fn next_submission(&mut self) -> Option<BatchSubmission> {
            let sub = self.submissions.pop_front()?;
            self.recorded.lock().unwrap().dequeued.push(sub.id);
            Some(sub)
        }
        fn confirm(&mut self, _: SubmissionId, _: u64) {}
        fn requeue(&mut self, id: SubmissionId) {
            self.recorded.lock().unwrap().requeued.push(id);
        }
        fn advance_l1_head(&mut self, l1_block: u64) {
            self.recorded.lock().unwrap().l1_heads.push(l1_block);
        }
        fn reset(&mut self) {
            self.recorded.lock().unwrap().resets += 1;
        }
        fn da_backlog_bytes(&self) -> u64 {
            self.da_backlog_bytes_value
        }
    }

    // ---- Pipeline that always returns ReorgError from add_block ----

    #[derive(Debug)]
    struct ReorgPipeline {
        recorded: Arc<Mutex<Recorded>>,
    }

    impl ReorgPipeline {
        fn new(recorded: Arc<Mutex<Recorded>>) -> Self {
            Self { recorded }
        }
    }

    impl BatchPipeline for ReorgPipeline {
        fn add_block(&mut self, block: OpBlock) -> Result<(), (ReorgError, Box<OpBlock>)> {
            Err((
                ReorgError::ParentMismatch { expected: B256::ZERO, got: B256::with_last_byte(1) },
                Box::new(block),
            ))
        }
        fn step(&mut self) -> Result<StepResult, StepError> {
            Ok(StepResult::Idle)
        }
        fn next_submission(&mut self) -> Option<BatchSubmission> {
            None
        }
        fn confirm(&mut self, _: SubmissionId, _: u64) {}
        fn requeue(&mut self, _: SubmissionId) {}
        fn advance_l1_head(&mut self, _: u64) {}
        fn reset(&mut self) {
            self.recorded.lock().unwrap().resets += 1;
        }
        fn da_backlog_bytes(&self) -> u64 {
            0
        }
    }

    // ---- Source that delivers one block then parks forever ----

    #[derive(Debug)]
    struct OneBlockSource {
        delivered: bool,
    }

    impl OneBlockSource {
        fn new() -> Self {
            Self { delivered: false }
        }
    }

    #[async_trait]
    impl UnsafeBlockSource for OneBlockSource {
        async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
            if !self.delivered {
                self.delivered = true;
                Ok(L2BlockEvent::Block(Box::default()))
            } else {
                std::future::pending().await
            }
        }
    }

    // ---- Source that parks the arm so the submission arm can fire ----

    #[derive(Debug)]
    struct PendingSource;

    #[async_trait]
    impl UnsafeBlockSource for PendingSource {
        async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
            std::future::pending().await
        }
    }

    // ---- TxManager helpers ----

    fn stub_receipt(block_number: u64) -> TransactionReceipt {
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
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(B256::ZERO),
            block_number: Some(block_number),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        }
    }

    /// Immediately confirms every submission at the given L1 block number.
    #[derive(Debug)]
    struct ImmediateConfirmTxManager {
        l1_block: u64,
    }

    impl TxManager for ImmediateConfirmTxManager {
        async fn send(&self, _: TxCandidate) -> SendResponse {
            unreachable!()
        }

        fn send_async(
            &self,
            _: TxCandidate,
        ) -> impl std::future::Future<Output = SendHandle> + Send {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Ok(stub_receipt(self.l1_block)));
            std::future::ready(SendHandle::new(rx))
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    /// Immediately fails every submission.
    #[derive(Debug)]
    struct ImmediateFailTxManager;

    impl TxManager for ImmediateFailTxManager {
        async fn send(&self, _: TxCandidate) -> SendResponse {
            unreachable!()
        }

        fn send_async(
            &self,
            _: TxCandidate,
        ) -> impl std::future::Future<Output = SendHandle> + Send {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Err(TxManagerError::ChannelClosed));
            std::future::ready(SendHandle::new(rx))
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    /// Never confirms any submission — the in-flight future parks forever.
    ///
    /// This is used to test semaphore backpressure: with this manager, permits
    /// are consumed but never released, so `try_acquire_owned` will fail once
    /// the limit is reached and no further submissions will be dequeued.
    struct NeverConfirmTxManager;

    impl fmt::Debug for NeverConfirmTxManager {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("NeverConfirmTxManager")
        }
    }

    impl TxManager for NeverConfirmTxManager {
        async fn send(&self, _: TxCandidate) -> SendResponse {
            unreachable!()
        }

        fn send_async(
            &self,
            _: TxCandidate,
        ) -> impl std::future::Future<Output = SendHandle> + Send {
            let (tx, rx) = oneshot::channel();
            // Spawn a task that parks forever, keeping `tx` alive so `rx`
            // never resolves. The task is cancelled when the test runtime
            // drops at the end of the test.
            tokio::spawn(async move {
                std::future::pending::<()>().await;
                drop(tx);
            });
            std::future::ready(SendHandle::new(rx))
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    /// Minimal [`BatchSubmission`] whose single empty frame blob-encodes cleanly.
    fn make_submission() -> BatchSubmission {
        make_submission_with_id(0)
    }

    fn make_submission_with_id(id: u64) -> BatchSubmission {
        BatchSubmission {
            id: SubmissionId(id),
            channel_id: ChannelId::default(),
            da_type: base_batcher_encoder::DaType::Blob,
            frames: vec![Arc::new(Frame::default())],
        }
    }

    fn noop_throttle() -> ThrottleController {
        ThrottleController::new(
            ThrottleConfig { threshold_bytes: 0, max_intensity: 0.0, ..Default::default() },
            ThrottleStrategy::Off,
        )
    }

    fn make_driver<TM: TxManager>(
        pipeline: TrackingPipeline,
        tx_manager: TM,
    ) -> BatchDriver<TrackingPipeline, PendingSource, TM, Arc<NoopThrottleClient>> {
        BatchDriver::new(
            pipeline,
            PendingSource,
            tx_manager,
            Address::ZERO,
            1,
            noop_throttle(),
            Arc::new(NoopThrottleClient),
        )
    }

    fn make_driver_with_max_pending<TM: TxManager>(
        pipeline: TrackingPipeline,
        tx_manager: TM,
        max_pending: usize,
    ) -> BatchDriver<TrackingPipeline, PendingSource, TM, Arc<NoopThrottleClient>> {
        BatchDriver::new(
            pipeline,
            PendingSource,
            tx_manager,
            Address::ZERO,
            max_pending,
            noop_throttle(),
            Arc::new(NoopThrottleClient),
        )
    }

    /// `advance_l1_head` must be called with the confirmed L1 block on every
    /// confirmation so the encoder can detect channel timeouts.
    #[tokio::test]
    async fn test_advance_l1_head_called_on_confirmation() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission());

        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(
            make_driver(pipeline, ImmediateConfirmTxManager { l1_block: 42 })
                .run(cancellation.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        assert_eq!(
            recorded.lock().unwrap().l1_heads,
            vec![42],
            "advance_l1_head must be called with the confirmed L1 block"
        );
    }

    /// `advance_l1_head` must NOT be called when a submission fails — we have no
    /// confirmed L1 block to report.
    #[tokio::test]
    async fn test_advance_l1_head_not_called_on_failure() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission());

        let cancellation = CancellationToken::new();
        let handle =
            tokio::spawn(make_driver(pipeline, ImmediateFailTxManager).run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        assert!(
            recorded.lock().unwrap().l1_heads.is_empty(),
            "advance_l1_head must NOT be called on submission failure"
        );
    }

    /// When blob encoding fails the submission has already been dequeued from the pipeline
    /// (cursor advanced, `pending_confirmations` incremented). Without a requeue the channel
    /// is permanently stuck — `pending_confirmations` never returns to zero and blocks are
    /// never pruned. The driver must call requeue so the encoder can unwind that state.
    #[tokio::test]
    async fn test_blob_encoding_failure_requeues_submission() {
        // encode_frames feeds: DERIVATION_VERSION_0 (1) + frame.encode() (23 + data.len())
        // = 24 + data.len() bytes into BlobEncoder::encode. It fails when > BLOB_MAX_DATA_SIZE
        // (130_044), so data.len() >= 130_021 guarantees DataTooLarge.
        const OVERSIZED: usize = 130_021;

        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(BatchSubmission {
            id: SubmissionId(0),
            channel_id: ChannelId::default(),
            da_type: base_batcher_encoder::DaType::Blob,
            frames: vec![Arc::new(Frame { data: vec![0u8; OVERSIZED], ..Frame::default() })],
        });

        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(
            make_driver(pipeline, ImmediateConfirmTxManager { l1_block: 1 })
                .run(cancellation.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

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
    }

    /// The submission loop must drain all ready frames in a single pass when
    /// permits allow. With `max_pending_transactions`=2 and two frames ready,
    /// both must be submitted and confirmed without waiting for an I/O event
    /// between them.
    #[tokio::test]
    async fn test_submission_loop_drains_multiple_frames_concurrently() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission_with_id(0));
        pipeline.submissions.push_back(make_submission_with_id(1));

        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(
            make_driver_with_max_pending(pipeline, ImmediateConfirmTxManager { l1_block: 10 }, 2)
                .run(cancellation.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.dequeued.len(), 2, "both submissions must be dequeued");
        assert_eq!(recorded.l1_heads.len(), 2, "both submissions must be confirmed");
    }

    /// The semaphore must prevent more concurrent in-flight submissions than
    /// `max_pending_transactions`. With max=1 and a tx manager that never
    /// confirms, exactly one submission must be dequeued; the second must not
    /// be dequeued because `try_acquire_owned` fails when the slot is occupied.
    #[tokio::test]
    async fn test_semaphore_prevents_excess_concurrent_submissions() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission_with_id(0));
        pipeline.submissions.push_back(make_submission_with_id(1));

        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(
            make_driver_with_max_pending(pipeline, NeverConfirmTxManager, 1)
                .run(cancellation.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        assert_eq!(
            recorded.lock().unwrap().dequeued,
            vec![SubmissionId(0)],
            "only the first submission must be dequeued when the semaphore slot is occupied"
        );
    }

    /// With `max_pending_transactions`=1, the second submission must only be
    /// dequeued and confirmed after the first is confirmed (freeing the permit).
    /// Both must ultimately be confirmed.
    #[tokio::test]
    async fn test_second_submission_sent_after_permit_freed() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut pipeline = TrackingPipeline::new(Arc::clone(&recorded));
        pipeline.submissions.push_back(make_submission_with_id(0));
        pipeline.submissions.push_back(make_submission_with_id(1));

        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(
            make_driver_with_max_pending(pipeline, ImmediateConfirmTxManager { l1_block: 7 }, 1)
                .run(cancellation.clone()),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok(), "driver should exit cleanly on cancellation");
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.dequeued.len(), 2, "both submissions must eventually be dequeued");
        assert_eq!(
            recorded.l1_heads,
            vec![7, 7],
            "both submissions must be confirmed once the permit is freed between them"
        );
    }

    // ---- Pipeline that fails once then succeeds ----

    /// Pipeline that rejects the first `add_block` call and accepts all subsequent ones.
    ///
    /// Used to verify that the driver re-adds the triggering block after `reset()`,
    /// so the block is not silently lost when the source won't re-deliver it.
    #[derive(Debug)]
    struct OneReorgPipeline {
        /// Number of times `add_block` succeeded (post-reorg re-adds).
        blocks_accepted: Arc<Mutex<usize>>,
        /// Whether the next `add_block` call should simulate a reorg.
        fail_next: bool,
        resets: Arc<Mutex<usize>>,
    }

    impl OneReorgPipeline {
        fn new(blocks_accepted: Arc<Mutex<usize>>, resets: Arc<Mutex<usize>>) -> Self {
            Self { blocks_accepted, fail_next: true, resets }
        }
    }

    impl BatchPipeline for OneReorgPipeline {
        fn add_block(&mut self, block: OpBlock) -> Result<(), (ReorgError, Box<OpBlock>)> {
            if self.fail_next {
                self.fail_next = false;
                return Err((
                    ReorgError::ParentMismatch {
                        expected: B256::ZERO,
                        got: B256::with_last_byte(1),
                    },
                    Box::new(block),
                ));
            }
            *self.blocks_accepted.lock().unwrap() += 1;
            Ok(())
        }
        fn step(&mut self) -> Result<StepResult, StepError> {
            Ok(StepResult::Idle)
        }
        fn next_submission(&mut self) -> Option<BatchSubmission> {
            None
        }
        fn confirm(&mut self, _: SubmissionId, _: u64) {}
        fn requeue(&mut self, _: SubmissionId) {}
        fn advance_l1_head(&mut self, _: u64) {}
        fn reset(&mut self) {
            *self.resets.lock().unwrap() += 1;
        }
        fn da_backlog_bytes(&self) -> u64 {
            0
        }
    }

    /// When `add_block` returns `ReorgError`, the driver must reset the pipeline and
    /// then re-add the triggering block so it is not permanently lost. The block
    /// queue in the encoder is empty after reset, so the parent-hash check is
    /// skipped and the re-add always succeeds.
    #[tokio::test]
    async fn test_reorg_block_is_readded_after_reset() {
        let blocks_accepted = Arc::new(Mutex::new(0usize));
        let resets = Arc::new(Mutex::new(0usize));
        let pipeline = OneReorgPipeline::new(Arc::clone(&blocks_accepted), Arc::clone(&resets));

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            OneBlockSource::new(),
            ImmediateConfirmTxManager { l1_block: 1 },
            Address::ZERO,
            1,
            noop_throttle(),
            Arc::new(NoopThrottleClient),
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        assert!(handle.await.unwrap().is_ok());
        assert_eq!(*resets.lock().unwrap(), 1, "pipeline must be reset on reorg");
        assert_eq!(
            *blocks_accepted.lock().unwrap(),
            1,
            "the triggering block must be re-added after reset"
        );
    }

    /// When `add_block` returns a `ReorgError`, the driver must reset the pipeline
    /// and discard in-flight futures instead of propagating a fatal error. This
    /// mirrors the `L2BlockEvent::Reorg` handling path.
    #[tokio::test]
    async fn test_add_block_reorg_resets_pipeline_instead_of_fatal_error() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = ReorgPipeline::new(Arc::clone(&recorded));

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            OneBlockSource::new(),
            ImmediateConfirmTxManager { l1_block: 1 },
            Address::ZERO,
            1,
            noop_throttle(),
            Arc::new(NoopThrottleClient),
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "driver must not return a fatal error on add_block reorg");
        assert_eq!(
            recorded.lock().unwrap().resets,
            1,
            "pipeline.reset() must be called when add_block returns ReorgError"
        );
    }

    // ---- Throttle integration tests ----

    /// When the DA backlog exceeds the threshold, the driver must call
    /// `set_max_da_size` on the throttle client with reduced limits.
    #[tokio::test]
    async fn test_throttle_client_called_on_high_backlog() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        // 2 MB backlog — above the default 1 MB threshold.
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(2_000_000);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            Address::ZERO,
            1,
            throttle,
            Arc::new(throttle_client),
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert!(!calls.is_empty(), "throttle client must be called when backlog is high");
        let (max_tx_size, max_block_size) = calls[0];
        assert!(
            max_block_size < 130_000,
            "max_block_size should be below upper limit when throttled, got {max_block_size}"
        );
        assert!(
            max_tx_size < 20_000,
            "max_tx_size should be below upper limit when throttled, got {max_tx_size}"
        );
    }

    /// When the DA backlog is zero (below threshold), the driver must call
    /// `set_max_da_size` with the upper limits to reset any previous throttle.
    #[tokio::test]
    async fn test_throttle_client_called_with_upper_limits_on_zero_backlog() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(0);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            Address::ZERO,
            1,
            throttle,
            Arc::new(throttle_client),
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert!(!calls.is_empty(), "throttle client must be called even with zero backlog");
        let (max_tx_size, max_block_size) = calls[0];
        assert_eq!(
            max_block_size, 130_000,
            "max_block_size should be the upper limit when not throttling"
        );
        assert_eq!(
            max_tx_size, 20_000,
            "max_tx_size should be the upper limit when not throttling"
        );
    }

    /// When the DA limits do not change between iterations, the driver must not
    /// call `set_max_da_size` redundantly. The deduplication check via
    /// `last_applied_da_limits` ensures the RPC is called at most once for
    /// identical limits.
    #[tokio::test]
    async fn test_throttle_not_called_redundantly() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(0);

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            Address::ZERO,
            1,
            throttle,
            Arc::new(throttle_client),
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        // Run for 100ms to allow multiple loop iterations.
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancellation.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "set_max_da_size must be called exactly once when limits do not change, got {}",
            calls.len()
        );
    }

    /// With the Step strategy and full intensity, when backlog is above the
    /// threshold, the driver must apply the lower DA limits.
    #[tokio::test]
    async fn test_step_strategy_full_intensity_applies_lower_limits() {
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        // Backlog of 100 — above threshold of 1.
        let pipeline = TrackingPipeline::new(Arc::clone(&recorded)).with_da_backlog(100);

        let config =
            ThrottleConfig { threshold_bytes: 1, max_intensity: 1.0, ..Default::default() };
        let throttle = ThrottleController::new(config, ThrottleStrategy::Step);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            PendingSource,
            ImmediateConfirmTxManager { l1_block: 1 },
            Address::ZERO,
            1,
            throttle,
            Arc::new(throttle_client),
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert!(!calls.is_empty(), "throttle client must be called with Step strategy");
        let (max_tx_size, max_block_size) = calls[0];
        assert_eq!(
            max_block_size, 2_000,
            "Step strategy at full intensity must apply block_size_lower_limit"
        );
        assert_eq!(
            max_tx_size, 150,
            "Step strategy at full intensity must apply tx_size_lower_limit"
        );
    }

    /// Verifies that when the DA backlog transitions from above the threshold
    /// (throttle active) to zero (throttle inactive), the driver makes exactly
    /// two RPC calls: one with reduced limits and one resetting to upper limits.
    #[tokio::test]
    async fn test_throttle_transitions_from_active_to_inactive() {
        // Pipeline whose DA backlog is controlled from the test via a shared lock.
        struct DynamicPipeline {
            backlog: Arc<Mutex<u64>>,
        }

        impl BatchPipeline for DynamicPipeline {
            fn add_block(&mut self, _: OpBlock) -> Result<(), (ReorgError, Box<OpBlock>)> {
                Ok(())
            }

            fn step(&mut self) -> Result<StepResult, StepError> {
                Ok(StepResult::Idle)
            }

            fn next_submission(&mut self) -> Option<BatchSubmission> {
                None
            }

            fn confirm(&mut self, _: SubmissionId, _: u64) {}

            fn requeue(&mut self, _: SubmissionId) {}

            fn advance_l1_head(&mut self, _: u64) {}

            fn reset(&mut self) {}

            fn da_backlog_bytes(&self) -> u64 {
                *self.backlog.lock().unwrap()
            }
        }

        // Source driven by an mpsc channel so the test can wake the driver loop
        // by sending a dummy block event after changing the backlog.
        struct ChannelSource {
            rx: mpsc::UnboundedReceiver<L2BlockEvent>,
        }

        #[async_trait]
        impl UnsafeBlockSource for ChannelSource {
            async fn next(&mut self) -> Result<L2BlockEvent, SourceError> {
                match self.rx.recv().await {
                    Some(event) => Ok(event),
                    // Channel closed: park until the driver is cancelled.
                    None => std::future::pending().await,
                }
            }
        }

        let (source_tx, source_rx) = mpsc::unbounded_channel();

        // Start with 2 MB backlog — above the default 1 MB threshold.
        let backlog = Arc::new(Mutex::new(2_000_000u64));
        let pipeline = DynamicPipeline { backlog: Arc::clone(&backlog) };

        let throttle = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Linear);
        let (throttle_client, throttle_recorded) = TrackingThrottleClient::new();

        let cancellation = CancellationToken::new();
        let driver = BatchDriver::new(
            pipeline,
            ChannelSource { rx: source_rx },
            ImmediateConfirmTxManager { l1_block: 1 },
            Address::ZERO,
            1,
            throttle,
            Arc::new(throttle_client),
        );
        let handle = tokio::spawn(driver.run(cancellation.clone()));

        // First iteration fires immediately on startup; give it time to complete.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Drop the backlog to zero, then wake the driver by delivering a dummy
        // block so the select! arm fires and the loop re-runs the throttle check.
        *backlog.lock().unwrap() = 0;
        source_tx.send(L2BlockEvent::Block(Box::default())).unwrap();

        tokio::time::sleep(Duration::from_millis(30)).await;
        cancellation.cancel();
        assert!(handle.await.unwrap().is_ok());

        let calls = throttle_recorded.lock().unwrap();
        assert!(
            calls.len() >= 2,
            "expected at least 2 throttle calls (activate + deactivate), got {}",
            calls.len()
        );

        // First call must have reduced limits (throttle active, backlog was high).
        let (first_tx, first_block) = calls[0];
        assert!(
            first_block < 130_000,
            "first call should apply throttled block limit, got {first_block}"
        );
        assert!(first_tx < 20_000, "first call should apply throttled tx limit, got {first_tx}");

        // Last call must reset to upper limits (throttle deactivated).
        let (last_tx, last_block) = *calls.last().unwrap();
        assert_eq!(last_block, 130_000, "last call should reset block limit to upper bound");
        assert_eq!(last_tx, 20_000, "last call should reset tx limit to upper bound");
    }
}
