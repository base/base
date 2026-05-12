//! Transaction submission pipeline with preparation, signing, and sending stages.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use alloy_consensus::transaction::SignableTransaction;
use alloy_eips::Encodable2718;
use alloy_network::{Ethereum, TransactionBuilder};
use alloy_primitives::{Address, Bytes, TxHash, U256};
use alloy_provider::RootProvider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::NonceManager;
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{ResultsTracker, SentTransaction};
use crate::rpc::{BatchRpcClient, BatchSendResult};

/// Number of signer tasks per submission RPC.
pub const SIGNER_WORKERS_PER_RPC: usize = 10;
/// Number of sender tasks per submission RPC.
pub const SENDER_WORKERS_PER_RPC: usize = 10;
/// Maximum signer task count.
pub const MAX_SIGNER_WORKER_COUNT: usize = 32;
/// Maximum sender task count.
pub const MAX_SENDER_WORKER_COUNT: usize = 64;
/// Number of queued prepared or signed batches allowed before backpressure.
pub const SUBMIT_BATCH_QUEUE_BUFFER: usize = 4096;
/// Maximum send attempts for signed transaction batches.
pub const SUBMIT_MAX_ATTEMPTS: u32 = 5;

/// A transaction request ready for nonce assignment and signing.
#[derive(Debug, Clone)]
pub struct PreparedTransaction {
    /// Sender address.
    pub from: Address,
    /// Optional destination address. `None` represents contract creation.
    pub to: Option<Address>,
    /// ETH value.
    pub value: U256,
    /// Transaction input data.
    pub data: Bytes,
    /// Gas limit.
    pub gas_limit: u64,
}

/// Submission events emitted by signer and sender stages.
#[derive(Debug)]
pub enum SubmitEvent {
    /// Transaction was accepted by a submission RPC.
    Submitted(TxHash),
    /// Transaction failed before acceptance.
    Failed(String),
    /// Sender has one fewer queued or in-flight submission.
    Released(Address),
}

/// Summary of queued submissions abandoned during pipeline shutdown.
#[derive(Debug)]
pub struct QueuedSubmitFailures {
    /// Failure reason applied to every abandoned transaction.
    pub reason: &'static str,
    /// Number of abandoned transactions.
    pub failed_count: u64,
    /// Number of abandoned queued or in-flight submissions by sender.
    pub released_by_sender: HashMap<Address, u64>,
}

impl QueuedSubmitFailures {
    /// Creates an empty abandoned-submission summary.
    pub fn new(reason: &'static str) -> Self {
        Self { reason, failed_count: 0, released_by_sender: HashMap::new() }
    }

    /// Records one abandoned transaction for a sender.
    pub fn record(&mut self, from: Address) {
        self.failed_count = self.failed_count.saturating_add(1);
        *self.released_by_sender.entry(from).or_insert(0) += 1;
    }
}

/// A signed transaction ready for network submission.
#[derive(Debug)]
pub struct SignedTransaction {
    /// EIP-2718 encoded signed transaction bytes.
    pub raw: Bytes,
    /// Locally computed transaction hash.
    pub tx_hash: TxHash,
    /// Sender address.
    pub from: Address,
    /// Signed nonce.
    pub nonce: u64,
}

/// A batch of prepared transactions.
#[derive(Debug)]
pub struct PreparedBatch {
    /// Stable batch id used for logging and endpoint sharding.
    pub id: u64,
    /// Gas price snapshot used while signing the batch.
    pub gas_price: u128,
    /// Prepared transactions.
    pub txs: Vec<PreparedTransaction>,
}

impl PreparedBatch {
    /// Returns the number of transactions in the batch.
    pub const fn len(&self) -> usize {
        self.txs.len()
    }

    /// Returns true when the batch has no transactions.
    pub const fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }
}

/// A batch of signed transactions.
#[derive(Debug)]
pub struct SignedBatch {
    /// Stable batch id used for logging and endpoint sharding.
    pub id: u64,
    /// Current send attempt.
    pub attempt: u32,
    /// Signed transactions.
    pub txs: Vec<SignedTransaction>,
}

impl SignedBatch {
    /// Returns the number of transactions in the batch.
    pub const fn len(&self) -> usize {
        self.txs.len()
    }

    /// Returns true when the batch has no transactions.
    pub const fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }
}

/// Result of classifying an individual batch response error.
#[derive(Debug, PartialEq, Eq)]
pub enum BatchTxError {
    /// The transaction is already present in the node's pool.
    AlreadyKnown,
    /// The transaction was rejected for a condition expected to clear.
    RetryableRejected(String),
    /// The transaction's acceptance status is unknown.
    RetryableUnknown(String),
    /// The sender nonce was already used.
    NonceTooLow,
    /// The transaction was rejected permanently.
    Rejected(String),
}

/// A bounded queue with pending batch accounting.
pub struct PipelineQueue<T> {
    /// Queue receiver shared by workers.
    pub receiver: Mutex<mpsc::Receiver<T>>,
    /// Number of queued or in-progress batches.
    pub pending_batches: AtomicU64,
}

impl<T> fmt::Debug for PipelineQueue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineQueue")
            .field("pending_batches", &self.pending_batches())
            .finish_non_exhaustive()
    }
}

impl<T> PipelineQueue<T> {
    /// Creates a queue wrapper.
    pub fn new(receiver: mpsc::Receiver<T>) -> Self {
        Self { receiver: Mutex::new(receiver), pending_batches: AtomicU64::new(0) }
    }

    /// Returns queued plus in-progress batch count.
    pub fn pending_batches(&self) -> u64 {
        self.pending_batches.load(Ordering::SeqCst)
    }
}

/// Shared signer stage context.
#[derive(Clone)]
pub struct SignerContext {
    /// Cached private key signers by address.
    pub signers: Arc<HashMap<Address, PrivateKeySigner>>,
    /// Nonce managers by sender address.
    pub nonce_managers: Arc<HashMap<Address, NonceManager<RootProvider<Ethereum>>>>,
    /// Events emitted to the runner.
    pub submit_event_tx: mpsc::Sender<SubmitEvent>,
    /// Chain ID used for signing.
    pub chain_id: u64,
    /// Maximum allowed gas price.
    pub max_gas_price: u128,
    /// Sender for signed batches.
    pub signed_batch_tx: mpsc::Sender<SignedBatch>,
    /// Signed queue accounting.
    pub signed_queue: Arc<PipelineQueue<SignedBatch>>,
}

impl fmt::Debug for SignerContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignerContext")
            .field("signers", &self.signers.len())
            .field("nonce_managers", &self.nonce_managers.len())
            .field("chain_id", &self.chain_id)
            .field("max_gas_price", &self.max_gas_price)
            .finish_non_exhaustive()
    }
}

/// Shared sender stage context.
#[derive(Clone)]
pub struct SenderContext {
    /// Transaction submission RPC clients.
    pub submission_batch_rpcs: Arc<Vec<BatchRpcClient>>,
    /// Nonce managers by sender address.
    pub nonce_managers: Arc<HashMap<Address, NonceManager<RootProvider<Ethereum>>>>,
    /// Results tracker updated after RPC acceptance.
    pub results_tracker: ResultsTracker,
    /// Events emitted to the runner.
    pub submit_event_tx: mpsc::Sender<SubmitEvent>,
}

impl fmt::Debug for SenderContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SenderContext")
            .field("submission_batch_rpcs", &self.submission_batch_rpcs.len())
            .field("nonce_managers", &self.nonce_managers.len())
            .finish_non_exhaustive()
    }
}

/// Running submission pipeline.
pub struct SubmissionPipeline {
    prepared_batch_tx: Option<mpsc::Sender<PreparedBatch>>,
    prepared_queue: Arc<PipelineQueue<PreparedBatch>>,
    signed_queue: Arc<PipelineQueue<SignedBatch>>,
    shutdown: CancellationToken,
    signer_workers: Vec<JoinHandle<()>>,
    sender_workers: Vec<JoinHandle<()>>,
}

impl fmt::Debug for SubmissionPipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubmissionPipeline")
            .field("prepared_queue", &self.prepared_queue)
            .field("signed_queue", &self.signed_queue)
            .field("signer_workers", &self.signer_workers.len())
            .field("sender_workers", &self.sender_workers.len())
            .finish_non_exhaustive()
    }
}

impl SubmissionPipeline {
    /// Starts the signer and sender stages.
    pub fn start(
        signers: Arc<HashMap<Address, PrivateKeySigner>>,
        nonce_managers: Arc<HashMap<Address, NonceManager<RootProvider<Ethereum>>>>,
        submission_batch_rpcs: Arc<Vec<BatchRpcClient>>,
        results_tracker: ResultsTracker,
        submit_event_tx: mpsc::Sender<SubmitEvent>,
        chain_id: u64,
        max_gas_price: u128,
    ) -> Self {
        let (prepared_batch_tx, prepared_batch_rx) =
            mpsc::channel::<PreparedBatch>(SUBMIT_BATCH_QUEUE_BUFFER);
        let (signed_batch_tx, signed_batch_rx) =
            mpsc::channel::<SignedBatch>(SUBMIT_BATCH_QUEUE_BUFFER);
        let prepared_queue = Arc::new(PipelineQueue::new(prepared_batch_rx));
        let signed_queue = Arc::new(PipelineQueue::new(signed_batch_rx));
        let shutdown = CancellationToken::new();
        let signer_worker_count = Self::signer_worker_count(submission_batch_rpcs.len());
        let sender_worker_count = Self::sender_worker_count(submission_batch_rpcs.len());

        info!(
            signer_worker_count,
            sender_worker_count,
            submit_rpc_count = submission_batch_rpcs.len(),
            "starting submission pipeline"
        );

        let mut signer_workers = Vec::with_capacity(signer_worker_count);
        for worker_id in 0..signer_worker_count {
            let ctx = SignerContext {
                signers: Arc::clone(&signers),
                nonce_managers: Arc::clone(&nonce_managers),
                submit_event_tx: submit_event_tx.clone(),
                chain_id,
                max_gas_price,
                signed_batch_tx: signed_batch_tx.clone(),
                signed_queue: Arc::clone(&signed_queue),
            };
            let queue = Arc::clone(&prepared_queue);
            let shutdown = shutdown.clone();
            signer_workers.push(tokio::spawn(async move {
                Self::signer_worker(worker_id, ctx, queue, shutdown).await;
            }));
        }

        let mut sender_workers = Vec::with_capacity(sender_worker_count);
        for worker_id in 0..sender_worker_count {
            let ctx = SenderContext {
                submission_batch_rpcs: Arc::clone(&submission_batch_rpcs),
                nonce_managers: Arc::clone(&nonce_managers),
                results_tracker: results_tracker.clone(),
                submit_event_tx: submit_event_tx.clone(),
            };
            let queue = Arc::clone(&signed_queue);
            let shutdown = shutdown.clone();
            sender_workers.push(tokio::spawn(async move {
                Self::sender_worker(worker_id, ctx, queue, shutdown).await;
            }));
        }

        drop(signed_batch_tx);

        Self {
            prepared_batch_tx: Some(prepared_batch_tx),
            prepared_queue,
            signed_queue,
            shutdown,
            signer_workers,
            sender_workers,
        }
    }

    /// Returns signer worker count for a submission RPC count.
    pub fn signer_worker_count(submission_rpc_count: usize) -> usize {
        (submission_rpc_count * SIGNER_WORKERS_PER_RPC).clamp(1, MAX_SIGNER_WORKER_COUNT)
    }

    /// Returns sender worker count for a submission RPC count.
    pub fn sender_worker_count(submission_rpc_count: usize) -> usize {
        (submission_rpc_count * SENDER_WORKERS_PER_RPC).clamp(1, MAX_SENDER_WORKER_COUNT)
    }

    /// Enqueues a prepared batch for signing.
    pub async fn enqueue_prepared(
        &self,
        batch: PreparedBatch,
    ) -> std::result::Result<(), PreparedBatch> {
        let Some(tx) = &self.prepared_batch_tx else {
            return Err(batch);
        };

        self.prepared_queue.pending_batches.fetch_add(1, Ordering::SeqCst);
        match tx.send(batch).await {
            Ok(()) => Ok(()),
            Err(e) => {
                self.prepared_queue.pending_batches.fetch_sub(1, Ordering::SeqCst);
                Err(e.0)
            }
        }
    }

    /// Closes the prepared input queue after generation is complete.
    pub fn close_input(&mut self) {
        self.prepared_batch_tx = None;
    }

    /// Returns queued plus in-progress prepared and signed batch count.
    pub fn pending_batches(&self) -> u64 {
        self.prepared_queue.pending_batches() + self.signed_queue.pending_batches()
    }

    /// Closes both queues and summarizes queued-but-not-started batch failures.
    pub async fn close_and_fail_queued(&self, reason: &'static str) -> QueuedSubmitFailures {
        let mut failures = QueuedSubmitFailures::new(reason);
        let abandoned_prepared = {
            let mut receiver = self.prepared_queue.receiver.lock().await;
            receiver.close();
            let mut batches = Vec::new();
            while let Ok(batch) = receiver.try_recv() {
                batches.push(batch);
            }
            batches
        };
        for batch in abandoned_prepared {
            self.prepared_queue.pending_batches.fetch_sub(1, Ordering::SeqCst);
            for prepared in batch.txs {
                failures.record(prepared.from);
            }
        }

        let abandoned_signed = {
            let mut receiver = self.signed_queue.receiver.lock().await;
            receiver.close();
            let mut batches = Vec::new();
            while let Ok(batch) = receiver.try_recv() {
                batches.push(batch);
            }
            batches
        };
        for batch in abandoned_signed {
            self.signed_queue.pending_batches.fetch_sub(1, Ordering::SeqCst);
            for signed in batch.txs {
                failures.record(signed.from);
            }
        }
        failures
    }

    /// Signals workers to stop and waits for them.
    pub async fn shutdown_and_join(&mut self, timeout: Duration) {
        self.shutdown.cancel();

        for mut worker in self.signer_workers.drain(..).chain(self.sender_workers.drain(..)) {
            match tokio::time::timeout(timeout, &mut worker).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) if e.is_cancelled() => {}
                Ok(Err(e)) => warn!(error = %e, "submission worker panicked"),
                Err(_) => {
                    warn!("submission worker did not shut down in time, aborting");
                    worker.abort();
                }
            }
        }
    }

    /// Fails prepared transactions.
    pub async fn fail_prepared_batch(
        submit_event_tx: &mpsc::Sender<SubmitEvent>,
        prepared_txs: Vec<PreparedTransaction>,
        reason: &'static str,
    ) {
        for prepared in prepared_txs {
            Self::release_prepared(submit_event_tx, &prepared).await;
            let _ = submit_event_tx.send(SubmitEvent::Failed(reason.into())).await;
        }
    }

    /// Classifies an individual batch response error.
    pub fn classify_batch_error(msg: String) -> BatchTxError {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("already known") || lower.contains("already imported") {
            BatchTxError::AlreadyKnown
        } else if lower.contains("nonce too low") {
            BatchTxError::NonceTooLow
        } else if lower.contains("missing response") || lower.contains("invalid tx hash") {
            BatchTxError::RetryableUnknown(msg)
        } else if lower.contains("txpool is full")
            || lower.contains("transaction pool is full")
            || lower.contains("pool is full")
            || lower.contains("temporarily unavailable")
        {
            BatchTxError::RetryableRejected(msg)
        } else {
            BatchTxError::Rejected(msg)
        }
    }

    /// Computes EIP-1559 max fee for submissions.
    pub fn submission_max_fee(gas_price: u128, priority_fee: u128, max_gas_price: u128) -> u128 {
        gas_price.saturating_mul(2).max(priority_fee).min(max_gas_price)
    }

    async fn signer_worker(
        worker_id: usize,
        ctx: SignerContext,
        queue: Arc<PipelineQueue<PreparedBatch>>,
        shutdown: CancellationToken,
    ) {
        loop {
            let batch = {
                let mut receiver = queue.receiver.lock().await;
                tokio::select! {
                    batch = receiver.recv() => batch,
                    () = shutdown.cancelled() => None,
                }
            };

            let Some(batch) = batch else {
                debug!(worker_id, "signer worker exiting");
                break;
            };

            let batch_id = batch.id;
            let batch_len = batch.len();
            let signed_batch = Self::sign_batch(&ctx, batch).await;
            queue.pending_batches.fetch_sub(1, Ordering::SeqCst);

            let Some(signed_batch) = signed_batch else {
                debug!(worker_id, batch_id, batch_len, "prepared batch had no signed txs");
                continue;
            };

            let signed_len = signed_batch.len();
            ctx.signed_queue.pending_batches.fetch_add(1, Ordering::SeqCst);
            if let Err(e) = ctx.signed_batch_tx.send(signed_batch).await {
                ctx.signed_queue.pending_batches.fetch_sub(1, Ordering::SeqCst);
                warn!(worker_id, batch_id, signed_len, "signed queue closed");
                Self::fail_signed_batch(&ctx.submit_event_tx, e.0.txs, "signed queue closed").await;
            }
        }
    }

    async fn sender_worker(
        worker_id: usize,
        ctx: SenderContext,
        queue: Arc<PipelineQueue<SignedBatch>>,
        shutdown: CancellationToken,
    ) {
        loop {
            let batch = {
                let mut receiver = queue.receiver.lock().await;
                tokio::select! {
                    batch = receiver.recv() => batch,
                    () = shutdown.cancelled() => None,
                }
            };

            let Some(batch) = batch else {
                debug!(worker_id, "sender worker exiting");
                break;
            };

            let batch_id = batch.id;
            let batch_len = batch.len();
            let submitted = Self::send_batch(ctx.clone(), batch, &shutdown).await;
            queue.pending_batches.fetch_sub(1, Ordering::SeqCst);
            debug!(worker_id, batch_id, batch_len, submitted, "signed batch complete");
        }
    }

    async fn sign_batch(ctx: &SignerContext, batch: PreparedBatch) -> Option<SignedBatch> {
        let mut signed_txs = Vec::with_capacity(batch.txs.len());
        for prepared in batch.txs {
            if let Some(tx) = Self::sign_prepared(ctx, &prepared, batch.gas_price).await {
                signed_txs.push(tx);
            } else {
                Self::release_prepared(&ctx.submit_event_tx, &prepared).await;
            }
        }

        (!signed_txs.is_empty()).then_some(SignedBatch {
            id: batch.id,
            attempt: 0,
            txs: signed_txs,
        })
    }

    async fn send_batch(
        ctx: SenderContext,
        mut batch: SignedBatch,
        shutdown: &CancellationToken,
    ) -> u64 {
        let batch_id = batch.id;
        let mut submitted = 0u64;

        loop {
            if batch.txs.is_empty() {
                return submitted;
            }

            let attempt = batch.attempt;
            let raw_list: Vec<Bytes> = batch.txs.iter().map(|s| s.raw.clone()).collect();
            let rpc_index = batch_id as usize % ctx.submission_batch_rpcs.len();
            let batch_results =
                match ctx.submission_batch_rpcs[rpc_index].send_raw_transactions(&raw_list).await {
                    Ok(results) => results,
                    Err(e) => {
                        if attempt + 1 >= SUBMIT_MAX_ATTEMPTS {
                            warn!(
                                batch_id,
                                attempt,
                                error = %e,
                                count = batch.txs.len(),
                                "batch RPC failed after max attempts"
                            );
                            Self::fail_signed_batch(
                                &ctx.submit_event_tx,
                                batch.txs,
                                "batch transport failed after retries",
                            )
                            .await;
                            return submitted;
                        }

                        warn!(
                            batch_id,
                            attempt,
                            next_attempt = attempt + 1,
                            error = %e,
                            count = batch.txs.len(),
                            "batch RPC failed, retrying signed batch"
                        );
                        batch.attempt += 1;
                        if !Self::wait_submit_retry(shutdown, batch.attempt).await {
                            Self::fail_signed_batch(
                                &ctx.submit_event_tx,
                                batch.txs,
                                "submit worker shutdown",
                            )
                            .await;
                            return submitted;
                        }
                        continue;
                    }
                };

            let mut retry_unknown_txs = Vec::new();
            let mut retry_rejected_txs = Vec::new();

            for (signed, result) in batch.txs.into_iter().zip(batch_results) {
                match result {
                    BatchSendResult::Success(hash) => {
                        submitted +=
                            Self::record_submitted(&ctx, signed, hash, "tx submitted (batch)")
                                .await;
                    }
                    BatchSendResult::Error(msg) => match Self::classify_batch_error(msg) {
                        BatchTxError::AlreadyKnown => {
                            let tx_hash = signed.tx_hash;
                            submitted +=
                                Self::record_submitted(&ctx, signed, tx_hash, "tx already known")
                                    .await;
                        }
                        BatchTxError::RetryableRejected(msg) => {
                            debug!(
                                from = %signed.from,
                                nonce = signed.nonce,
                                attempt,
                                error = %msg,
                                "tx rejected with retryable error"
                            );
                            retry_rejected_txs.push(signed);
                        }
                        BatchTxError::RetryableUnknown(msg) => {
                            debug!(
                                from = %signed.from,
                                nonce = signed.nonce,
                                attempt,
                                error = %msg,
                                "tx status unknown, retrying signed transaction"
                            );
                            retry_unknown_txs.push(signed);
                        }
                        BatchTxError::NonceTooLow => {
                            debug!(
                                from = %signed.from,
                                nonce = signed.nonce,
                                attempt,
                                "nonce too low during batch submission"
                            );
                            // On retry, a nonce-too-low response usually means a prior attempt
                            // was accepted but its response was lost. Treat it as submitted so we
                            // do not return a consumed nonce; if another transaction consumed the
                            // nonce, the optimistic pending entry expires through the normal
                            // confirmation timeout.
                            if attempt > 0 {
                                let tx_hash = signed.tx_hash;
                                submitted += Self::record_submitted(
                                    &ctx,
                                    signed,
                                    tx_hash,
                                    "tx nonce already used",
                                )
                                .await;
                            } else {
                                Self::release_signed(&ctx.submit_event_tx, &signed).await;
                                let _ = ctx
                                    .submit_event_tx
                                    .send(SubmitEvent::Failed("nonce too low".into()))
                                    .await;
                            }
                        }
                        BatchTxError::Rejected(msg) => {
                            debug!(
                                from = %signed.from,
                                nonce = signed.nonce,
                                error = %msg,
                                "tx rejected in batch"
                            );
                            Self::return_signed_nonce(&ctx, &signed).await;
                            Self::release_signed(&ctx.submit_event_tx, &signed).await;
                            let _ = ctx.submit_event_tx.send(SubmitEvent::Failed(msg)).await;
                        }
                    },
                }
            }

            if retry_unknown_txs.is_empty() && retry_rejected_txs.is_empty() {
                return submitted;
            }

            if attempt + 1 >= SUBMIT_MAX_ATTEMPTS {
                let failed = retry_unknown_txs.len() + retry_rejected_txs.len();
                warn!(batch_id, attempt, failed, "retryable tx errors exceeded max attempts");
                Self::fail_signed_batch(
                    &ctx.submit_event_tx,
                    retry_unknown_txs,
                    "tx status unknown after retries",
                )
                .await;
                Self::fail_rejected_signed_batch(
                    &ctx,
                    &ctx.submit_event_tx,
                    retry_rejected_txs,
                    "txpool retry failed after retries",
                )
                .await;
                return submitted;
            }

            retry_unknown_txs.extend(retry_rejected_txs);
            batch = SignedBatch { id: batch_id, attempt: attempt + 1, txs: retry_unknown_txs };
            if !Self::wait_submit_retry(shutdown, batch.attempt).await {
                Self::fail_signed_batch(&ctx.submit_event_tx, batch.txs, "submit worker shutdown")
                    .await;
                return submitted;
            }
        }
    }

    async fn record_submitted(
        ctx: &SenderContext,
        signed: SignedTransaction,
        tx_hash: TxHash,
        message: &'static str,
    ) -> u64 {
        let tracked_hash = if tx_hash != signed.tx_hash {
            debug!(
                local = %signed.tx_hash,
                server = %tx_hash,
                "tx hash mismatch, using server hash"
            );
            tx_hash
        } else {
            signed.tx_hash
        };
        Self::release_signed(&ctx.submit_event_tx, &signed).await;
        ctx.results_tracker
            .sent_transactions(vec![SentTransaction { tx_hash: tracked_hash, from: signed.from }]);
        let _ = ctx.submit_event_tx.send(SubmitEvent::Submitted(tracked_hash)).await;
        debug!(
            tx_hash = %tracked_hash,
            from = %signed.from,
            nonce = signed.nonce,
            outcome = message,
            "tx submission accepted"
        );
        1
    }

    async fn fail_signed_batch(
        submit_event_tx: &mpsc::Sender<SubmitEvent>,
        signed_txs: Vec<SignedTransaction>,
        reason: &'static str,
    ) {
        for signed in signed_txs {
            Self::release_signed(submit_event_tx, &signed).await;
            let _ = submit_event_tx.send(SubmitEvent::Failed(reason.into())).await;
        }
    }

    async fn fail_rejected_signed_batch(
        ctx: &SenderContext,
        submit_event_tx: &mpsc::Sender<SubmitEvent>,
        signed_txs: Vec<SignedTransaction>,
        reason: &'static str,
    ) {
        for signed in signed_txs {
            Self::return_signed_nonce(ctx, &signed).await;
            Self::release_signed(submit_event_tx, &signed).await;
            let _ = submit_event_tx.send(SubmitEvent::Failed(reason.into())).await;
        }
    }

    async fn return_signed_nonce(ctx: &SenderContext, signed: &SignedTransaction) {
        let Some(nonce_manager) = ctx.nonce_managers.get(&signed.from) else {
            warn!(from = %signed.from, nonce = signed.nonce, "no nonce manager for nonce return");
            return;
        };
        nonce_manager.return_reserved_nonce(signed.nonce).await;
    }

    async fn release_prepared(
        submit_event_tx: &mpsc::Sender<SubmitEvent>,
        prepared: &PreparedTransaction,
    ) {
        let _ = submit_event_tx.send(SubmitEvent::Released(prepared.from)).await;
    }

    async fn release_signed(
        submit_event_tx: &mpsc::Sender<SubmitEvent>,
        signed: &SignedTransaction,
    ) {
        let _ = submit_event_tx.send(SubmitEvent::Released(signed.from)).await;
    }

    async fn sign_prepared(
        ctx: &SignerContext,
        prepared: &PreparedTransaction,
        gas_price: u128,
    ) -> Option<SignedTransaction> {
        let priority_fee = (gas_price / 10).max(1);
        let max_fee = Self::submission_max_fee(gas_price, priority_fee, ctx.max_gas_price);

        let Some(signer) = ctx.signers.get(&prepared.from) else {
            warn!(from = %prepared.from, "no signer for sender");
            let _ = ctx.submit_event_tx.send(SubmitEvent::Failed("no signer".into())).await;
            return None;
        };

        let Some(nonce_manager) = ctx.nonce_managers.get(&prepared.from) else {
            warn!(from = %prepared.from, "no nonce manager for sender");
            let _ = ctx.submit_event_tx.send(SubmitEvent::Failed("no nonce manager".into())).await;
            return None;
        };

        let nonce_guard = match nonce_manager.next_nonce().await {
            Ok(guard) => guard,
            Err(e) => {
                warn!(from = %prepared.from, error = %e, "failed to acquire nonce");
                let _ = ctx
                    .submit_event_tx
                    .send(SubmitEvent::Failed("nonce acquisition failed".into()))
                    .await;
                return None;
            }
        };
        let nonce = nonce_guard.nonce();

        let mut tx = TransactionRequest::default()
            .with_from(prepared.from)
            .with_value(prepared.value)
            .with_input(prepared.data.clone())
            .with_nonce(nonce)
            .with_chain_id(ctx.chain_id)
            .with_max_fee_per_gas(max_fee)
            .with_max_priority_fee_per_gas(priority_fee)
            .with_gas_limit(prepared.gas_limit);
        if let Some(to) = prepared.to {
            tx = tx.with_to(to);
        }

        let typed_tx = match tx.build_typed_tx() {
            Ok(t) => t,
            Err(e) => {
                warn!(from = %prepared.from, nonce, error = ?e, "failed to build typed tx");
                nonce_guard.rollback();
                let _ =
                    ctx.submit_event_tx.send(SubmitEvent::Failed("tx build failed".into())).await;
                return None;
            }
        };

        let sig_hash = typed_tx.signature_hash();
        let signature = match signer.sign_hash_sync(&sig_hash) {
            Ok(sig) => sig,
            Err(e) => {
                warn!(from = %prepared.from, nonce, error = %e, "failed to sign tx");
                nonce_guard.rollback();
                let _ =
                    ctx.submit_event_tx.send(SubmitEvent::Failed("signing failed".into())).await;
                return None;
            }
        };

        let signed = typed_tx.into_signed(signature);
        let tx_hash = *signed.hash();
        let raw = Bytes::from(signed.encoded_2718());

        // Drop the nonce guard immediately after signing. The guard holds
        // the NonceManager mutex; keeping it alive until after RPC send
        // would serialize unrelated network latency through nonce allocation.
        drop(nonce_guard);

        Some(SignedTransaction { raw, tx_hash, from: prepared.from, nonce })
    }

    fn submit_retry_delay(attempt: u32) -> Duration {
        let millis = 50u64.saturating_mul(1u64 << attempt.min(6));
        Duration::from_millis(millis.min(2_000))
    }

    async fn wait_submit_retry(shutdown: &CancellationToken, attempt: u32) -> bool {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => false,
            () = tokio::time::sleep(Self::submit_retry_delay(attempt)) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };

    use alloy_primitives::{Address, Bytes, TxHash, U256};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::{
        BatchTxError, PipelineQueue, PreparedBatch, PreparedTransaction, SignedBatch,
        SignedTransaction, SubmissionPipeline, SubmitEvent,
    };

    #[test]
    fn batch_error_classification_identifies_retryable_transport_gaps() {
        assert_eq!(
            SubmissionPipeline::classify_batch_error("missing response".to_string()),
            BatchTxError::RetryableUnknown("missing response".to_string()),
        );
        assert_eq!(
            SubmissionPipeline::classify_batch_error("invalid tx hash: bad length".to_string()),
            BatchTxError::RetryableUnknown("invalid tx hash: bad length".to_string()),
        );
    }

    #[test]
    fn batch_error_classification_identifies_submission_outcomes() {
        assert_eq!(
            SubmissionPipeline::classify_batch_error("already known".to_string()),
            BatchTxError::AlreadyKnown,
        );
        assert_eq!(
            SubmissionPipeline::classify_batch_error("nonce too low".to_string()),
            BatchTxError::NonceTooLow,
        );
        assert_eq!(
            SubmissionPipeline::classify_batch_error("txpool is full".to_string()),
            BatchTxError::RetryableRejected("txpool is full".to_string()),
        );
        assert_eq!(
            SubmissionPipeline::classify_batch_error(
                "insufficient funds for gas * price + value".to_string(),
            ),
            BatchTxError::Rejected("insufficient funds for gas * price + value".to_string()),
        );
    }

    #[test]
    fn submission_max_fee_is_at_least_priority_fee() {
        assert_eq!(SubmissionPipeline::submission_max_fee(0, 1, 1_000_000_000), 1);
        assert_eq!(SubmissionPipeline::submission_max_fee(100, 10, 1_000_000_000), 200);
        assert_eq!(SubmissionPipeline::submission_max_fee(1_000, 10, 500), 500);
    }

    #[tokio::test]
    async fn close_and_fail_queued_summarizes_without_sending_submit_events() {
        let (submit_event_tx, mut submit_event_rx) = mpsc::channel(1);
        submit_event_tx
            .send(SubmitEvent::Failed("buffer already full".into()))
            .await
            .expect("submit event channel open");

        let sender = Address::repeat_byte(0x11);
        let (prepared_batch_tx, prepared_batch_rx) = mpsc::channel(2);
        let prepared_queue = Arc::new(PipelineQueue::new(prepared_batch_rx));
        prepared_queue.pending_batches.fetch_add(1, Ordering::SeqCst);
        prepared_batch_tx
            .send(PreparedBatch {
                id: 0,
                gas_price: 1,
                txs: vec![PreparedTransaction {
                    from: sender,
                    to: None,
                    value: U256::ZERO,
                    data: Bytes::new(),
                    gas_limit: 21_000,
                }],
            })
            .await
            .expect("prepared queue open");
        drop(prepared_batch_tx);

        let (signed_batch_tx, signed_batch_rx) = mpsc::channel(2);
        let signed_queue = Arc::new(PipelineQueue::new(signed_batch_rx));
        signed_queue.pending_batches.fetch_add(1, Ordering::SeqCst);
        signed_batch_tx
            .send(SignedBatch {
                id: 1,
                attempt: 0,
                txs: vec![SignedTransaction {
                    raw: Bytes::new(),
                    tx_hash: TxHash::ZERO,
                    from: sender,
                    nonce: 0,
                }],
            })
            .await
            .expect("signed queue open");
        drop(signed_batch_tx);

        let pipeline = SubmissionPipeline {
            prepared_batch_tx: None,
            prepared_queue,
            signed_queue,
            shutdown: CancellationToken::new(),
            signer_workers: Vec::new(),
            sender_workers: Vec::new(),
        };

        let failures = tokio::time::timeout(
            Duration::from_millis(100),
            pipeline.close_and_fail_queued("submit queue abandoned"),
        )
        .await
        .expect("abandoned queue summary should not block on submit events");

        assert_eq!(failures.reason, "submit queue abandoned");
        assert_eq!(failures.failed_count, 2);
        assert_eq!(failures.released_by_sender.get(&sender).copied(), Some(2));
        assert_eq!(pipeline.pending_batches(), 0);
        assert!(matches!(submit_event_rx.try_recv(), Ok(SubmitEvent::Failed(_))));
        assert!(submit_event_rx.try_recv().is_err());
    }
}
