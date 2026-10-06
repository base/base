//! Transaction submission pipeline with preparation, signing, and sending stages.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::transaction::SignableTransaction;
use alloy_eips::Encodable2718;
use alloy_network::{Ethereum, TransactionBuilder};
use alloy_primitives::{Address, Bytes, TxHash, U256};
use alloy_provider::RootProvider;
use alloy_rpc_types::TransactionRequest;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_execution_txpool::ValidityPredicate;
use base_tx_manager::NonceManager;
use tokio::{
    sync::{Mutex, Semaphore, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::{ResultsTracker, SentTransaction};
use crate::{
    BaselineError, Result,
    rpc::{BatchRpcClient, BatchSendResult, SubmitItem},
};

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
/// Multiplier applied to the base fee when computing `maxFeePerGas`.
///
/// Base fee can rise at most 12.5% per block (EIP-1559); a 4x buffer tolerates
/// ~11 full blocks (~24s on a 2s chain) of growth before a tx goes underwater.
/// The `max_gas_price` cap bounds worst-case cost, so the headroom is cheap.
pub const MAX_FEE_BASE_FEE_MULTIPLIER: u128 = 4;
/// Minimum priority fee used by measured load so very low base fees do not produce zero-value tips.
pub const MIN_PRIORITY_FEE: u128 = 1;

/// Ensures the rate-limit warning is only logged once per process, since a
/// saturated RPC can otherwise report it on every batch under sustained load.
static RATE_LIMIT_WARNED: AtomicBool = AtomicBool::new(false);

/// Submission cohort a transaction is routed to.
///
/// A sender's cohort is assigned deterministically so its entire nonce stream
/// flows through a single submission origin, keeping nonces contiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubmitCohort {
    /// Plain `eth_sendRawTransaction` submission carrying no predicates.
    #[default]
    Plain,
    /// Validity submission carrying resolved predicates.
    ValidityPass,
    /// Validity submission priced ahead of the plain cohort.
    ValidityPassPriorityLead,
}

impl SubmitCohort {
    /// Returns true when the cohort submits via the validity path.
    pub const fn is_validity(&self) -> bool {
        !matches!(self, Self::Plain)
    }

    /// Returns a stable label for the cohort.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::ValidityPass => "validity_pass",
            Self::ValidityPassPriorityLead => "validity_pass_priority_lead",
        }
    }

    /// Maps the cohort to its serializable metrics label.
    pub const fn to_metric_label(self) -> crate::metrics::SubmitCohortLabel {
        match self {
            Self::Plain => crate::metrics::SubmitCohortLabel::Plain,
            Self::ValidityPass | Self::ValidityPassPriorityLead => {
                crate::metrics::SubmitCohortLabel::ValidityPass
            }
        }
    }
}

/// EIP-1559 fee fields for a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fees {
    /// `maxFeePerGas`.
    pub max_fee: u128,
    /// `maxPriorityFeePerGas`.
    pub priority_fee: u128,
}

/// Multiplier for funder/setup `maxFeePerGas` relative to the current base fee.
///
/// Funding, draining, and token-mint transfers are short-lived and do not need the
/// measured-load headroom (`MAX_FEE_BASE_FEE_MULTIPLIER`). `2x` covers a single base-fee
/// jump while keeping the funder's declared max-cost affordable.
pub const FUNDING_MAX_FEE_BASE_FEE_MULTIPLIER: u128 = 2;

/// Computes EIP-1559 fee fields from a base fee, capped at a `max_gas_price` ceiling.
///
/// Centralizes fee formulas so callers never need to reimplement the pricing math.
/// Use [`Self::fees_for`] for measured load submissions and [`Self::funding_fees_for`]
/// for funder/setup transfers.
#[derive(Debug, Clone, Copy)]
pub struct GasPricer {
    max_gas_price: u128,
}

impl GasPricer {
    /// Creates a pricer that never quotes fees above `max_gas_price`.
    pub const fn new(max_gas_price: u128) -> Self {
        Self { max_gas_price }
    }

    /// Returns the configured fee ceiling.
    pub const fn max_gas_price(&self) -> u128 {
        self.max_gas_price
    }

    /// Fees for a measured-load submission at the given base fee.
    pub fn fees_for(&self, base_fee: u128) -> Fees {
        self.fees_for_cohort(base_fee, SubmitCohort::Plain, 1, 1)
    }

    /// Fees for measured load, pricing the bulk and priority-lead validity cohorts separately.
    pub fn fees_for_cohort(
        &self,
        base_fee: u128,
        cohort: SubmitCohort,
        validity_priority_fee_divisor: u128,
        validity_priority_lead_multiplier: u128,
    ) -> Fees {
        let priority_fee =
            (base_fee / 10).max(MIN_PRIORITY_FEE).min(self.max_gas_price.saturating_sub(base_fee));
        let priority_fee = match cohort {
            SubmitCohort::Plain => priority_fee,
            SubmitCohort::ValidityPass => priority_fee / validity_priority_fee_divisor,
            SubmitCohort::ValidityPassPriorityLead => priority_fee
                .saturating_mul(validity_priority_lead_multiplier)
                .min(self.max_gas_price.saturating_sub(base_fee)),
        };
        let max_fee =
            SubmissionPipeline::submission_max_fee(base_fee, priority_fee, self.max_gas_price);
        Fees { max_fee, priority_fee }
    }

    /// Fees for funder/setup transfers: `2 * base_fee` max fee and a 1 wei tip.
    ///
    /// Setup transfers are not competing for inclusion against measured load, so a larger tip
    /// is unnecessary. Budgeting and sending use the same quote so affordability checks match
    /// the transactions that will actually be broadcast.
    pub fn funding_fees_for(&self, base_fee: u128) -> Fees {
        let priority_fee = 1u128.min(self.max_gas_price);
        let max_fee = base_fee
            .saturating_mul(FUNDING_MAX_FEE_BASE_FEE_MULTIPLIER)
            .min(self.max_gas_price)
            .max(priority_fee);
        Fees { max_fee, priority_fee }
    }

    /// Fees for a replacement attempt, scaling both fields by `multiplier` and
    /// re-capping at `max_gas_price`. Used to bump a stuck pending transaction
    /// (e.g. a leftover from a prior run at the same nonce) above the incumbent.
    pub fn bumped(&self, fees: Fees, multiplier: u128) -> Fees {
        Fees {
            max_fee: fees.max_fee.saturating_mul(multiplier).min(self.max_gas_price),
            priority_fee: fees.priority_fee.saturating_mul(multiplier).min(self.max_gas_price),
        }
    }
}

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
    /// Calibrated execution gas used for pacing.
    pub estimated_gas: u64,
    /// Resolved validity predicates transported with the transaction. Empty for
    /// the plain cohort.
    pub validity: Vec<ValidityPredicate>,
    /// Submission cohort this transaction is routed to.
    pub cohort: SubmitCohort,
}

/// Submission events emitted by signer and sender stages.
#[derive(Debug)]
pub enum SubmitEvent {
    /// Transaction was accepted by a submission RPC.
    Submitted(TxHash),
    /// Transaction failed before acceptance.
    Failed(String),
    /// Sender has one fewer queued or in-flight submission.
    Released {
        /// Sender whose queued count is released.
        from: Address,
        /// Estimated gas removed from local queued-depth accounting.
        estimated_gas: u64,
        /// Whether the RPC accepted the transaction.
        accepted: bool,
    },
    /// Every transaction in a signed batch has received a terminal RPC result.
    BatchCompleted {
        /// Stable batch identifier.
        id: u64,
        /// Local completion time.
        completed_at: Instant,
    },
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
    /// Gas reserved by the transaction.
    pub gas_limit: u64,
    /// Calibrated execution gas used for pacing.
    pub estimated_gas: u64,
    /// Resolved validity predicates transported with the transaction. Empty for
    /// the plain cohort.
    pub validity: Vec<ValidityPredicate>,
    /// Submission cohort this transaction is routed to.
    pub cohort: SubmitCohort,
}

/// A batch of prepared transactions.
#[derive(Debug)]
pub struct PreparedBatch {
    /// Stable batch id used for logging and endpoint sharding.
    pub id: u64,
    /// Latest base fee snapshot used to derive `maxFeePerGas` while signing.
    pub base_fee: u128,
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
    /// Whether accepted transactions belong to the measured cohort.
    pub measured: bool,
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
    /// The RPC is rate-limiting submissions (HTTP 429 or a JSON-RPC-level
    /// rate-limit rejection). Retried with a longer backoff than other
    /// retryable errors since rate limits typically take longer to clear.
    RateLimited(String),
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
    /// Priority-tip multiplier for the validity priority-lead cohort.
    pub validity_priority_lead_multiplier: u128,
    /// Priority-tip divisor for validity-cohort measured transactions.
    pub validity_priority_fee_divisor: u128,
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
            .field("validity_priority_lead_multiplier", &self.validity_priority_lead_multiplier)
            .field("validity_priority_fee_divisor", &self.validity_priority_fee_divisor)
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
    /// Whether nonce return is enabled for rejected signed transactions.
    pub return_reserved_nonces: bool,
    /// Optional cap on concurrent outbound submission RPC requests, shared across
    /// all sender workers. Bounds request *rate* to the endpoint independently of
    /// how many transactions are in flight (unconfirmed) or how many sender
    /// workers exist. `None` leaves concurrency bounded only by the sender worker
    /// count, as before.
    pub submit_request_limiter: Option<Arc<Semaphore>>,
}

impl fmt::Debug for SenderContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SenderContext")
            .field("submission_batch_rpcs", &self.submission_batch_rpcs.len())
            .field("nonce_managers", &self.nonce_managers.len())
            .field("return_reserved_nonces", &self.return_reserved_nonces)
            .finish_non_exhaustive()
    }
}

/// Running submission pipeline.
pub struct SubmissionPipeline {
    prepared_batch_tx: Option<mpsc::Sender<PreparedBatch>>,
    signed_batch_tx: Option<mpsc::Sender<SignedBatch>>,
    prepared_queue: Arc<PipelineQueue<PreparedBatch>>,
    signed_queue: Arc<PipelineQueue<SignedBatch>>,
    shutdown: CancellationToken,
    signer_workers: Vec<JoinHandle<()>>,
    sender_workers: Vec<JoinHandle<()>>,
}

/// Runtime configuration for submission pipeline workers and signing.
#[derive(Debug, Clone, Copy)]
pub struct PipelineStartConfig {
    /// Chain ID used for transaction signing.
    pub chain_id: u64,
    /// Maximum allowed gas price.
    pub max_gas_price: u128,
    /// Priority-tip multiplier for the validity priority-lead cohort.
    pub validity_priority_lead_multiplier: u128,
    /// Priority-tip divisor for validity-cohort measured transactions.
    pub validity_priority_fee_divisor: u128,
    /// Optional cap on concurrent outbound submission RPC requests across all
    /// sender workers and per-batch RPC chunks. `None` leaves those requests
    /// unconstrained by a shared semaphore.
    pub max_concurrent_submit_requests: Option<usize>,
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
        config: PipelineStartConfig,
    ) -> Self {
        let (prepared_batch_tx, prepared_batch_rx) =
            mpsc::channel::<PreparedBatch>(SUBMIT_BATCH_QUEUE_BUFFER);
        let (signed_batch_tx, signed_batch_rx) =
            mpsc::channel::<SignedBatch>(SUBMIT_BATCH_QUEUE_BUFFER);
        let prepared_queue = Arc::new(PipelineQueue::new(prepared_batch_rx));
        let signed_queue = Arc::new(PipelineQueue::new(signed_batch_rx));
        let shutdown = CancellationToken::new();
        let signer_worker_count = Self::signer_worker_count(
            submission_batch_rpcs.len(),
            config.max_concurrent_submit_requests,
        );
        let sender_worker_count = Self::sender_worker_count(
            submission_batch_rpcs.len(),
            config.max_concurrent_submit_requests,
        );
        let submit_request_limiter =
            config.max_concurrent_submit_requests.map(|max| Arc::new(Semaphore::new(max.max(1))));

        let mut signer_workers = Vec::with_capacity(signer_worker_count);
        for worker_id in 0..signer_worker_count {
            let ctx = SignerContext {
                signers: Arc::clone(&signers),
                nonce_managers: Arc::clone(&nonce_managers),
                submit_event_tx: submit_event_tx.clone(),
                chain_id: config.chain_id,
                max_gas_price: config.max_gas_price,
                validity_priority_lead_multiplier: config.validity_priority_lead_multiplier,
                validity_priority_fee_divisor: config.validity_priority_fee_divisor,
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
        for _ in 0..sender_worker_count {
            let ctx = SenderContext {
                submission_batch_rpcs: Arc::clone(&submission_batch_rpcs),
                nonce_managers: Arc::clone(&nonce_managers),
                results_tracker: results_tracker.clone(),
                submit_event_tx: submit_event_tx.clone(),
                return_reserved_nonces: false,
                submit_request_limiter: submit_request_limiter.clone(),
            };
            let queue = Arc::clone(&signed_queue);
            let shutdown = shutdown.clone();
            sender_workers.push(tokio::spawn(async move {
                Self::sender_worker(ctx, queue, shutdown).await;
            }));
        }

        Self {
            prepared_batch_tx: Some(prepared_batch_tx),
            signed_batch_tx: Some(signed_batch_tx),
            prepared_queue,
            signed_queue,
            shutdown,
            signer_workers,
            sender_workers,
        }
    }

    /// Returns signer worker count for a submission RPC count and request limit.
    pub fn signer_worker_count(
        submission_rpc_count: usize,
        max_concurrent_submit_requests: Option<usize>,
    ) -> usize {
        let endpoint_workers =
            (submission_rpc_count * SIGNER_WORKERS_PER_RPC).clamp(1, MAX_SIGNER_WORKER_COUNT);
        max_concurrent_submit_requests
            .map_or(endpoint_workers, |request_limit| endpoint_workers.max(request_limit))
            .clamp(1, MAX_SIGNER_WORKER_COUNT)
    }

    /// Returns sender worker count for a submission RPC count and request limit.
    pub fn sender_worker_count(
        submission_rpc_count: usize,
        max_concurrent_submit_requests: Option<usize>,
    ) -> usize {
        let endpoint_workers =
            (submission_rpc_count * SENDER_WORKERS_PER_RPC).clamp(1, MAX_SENDER_WORKER_COUNT);
        max_concurrent_submit_requests
            .map_or(endpoint_workers, |request_limit| endpoint_workers.max(request_limit))
            .clamp(1, MAX_SENDER_WORKER_COUNT)
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

    /// Enqueues a signed batch for sending.
    pub async fn enqueue_signed(&self, batch: SignedBatch) -> std::result::Result<(), SignedBatch> {
        let Some(tx) = &self.signed_batch_tx else {
            return Err(batch);
        };

        self.signed_queue.pending_batches.fetch_add(1, Ordering::SeqCst);
        match tx.send(batch).await {
            Ok(()) => Ok(()),
            Err(e) => {
                self.signed_queue.pending_batches.fetch_sub(1, Ordering::SeqCst);
                Err(e.0)
            }
        }
    }

    /// Closes direct input to both stages after generation is complete.
    ///
    /// Signer workers retain their signed-stage senders until the prepared queue drains, then
    /// sender workers terminate naturally after the signed queue drains.
    pub fn close_input(&mut self) {
        self.prepared_batch_tx = None;
        self.signed_batch_tx = None;
    }

    /// Returns queued plus in-progress prepared and signed batch count.
    pub fn pending_batches(&self) -> u64 {
        self.prepared_queue.pending_batches() + self.signed_queue.pending_batches()
    }

    /// Closes both queues and summarizes queued-but-not-started batch failures.
    pub async fn close_and_fail_queued(&self, reason: &'static str) -> QueuedSubmitFailures {
        self.shutdown.cancel();
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

        let deadline = Instant::now() + timeout;
        for mut worker in self.signer_workers.drain(..).chain(self.sender_workers.drain(..)) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!("submission worker shutdown deadline elapsed, aborting");
                worker.abort();
                continue;
            }
            match tokio::time::timeout(remaining, &mut worker).await {
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
        } else if lower.contains("replacement transaction underpriced") {
            BatchTxError::NonceTooLow
        } else if Self::is_rate_limited_message(&msg) {
            BatchTxError::RateLimited(msg)
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

    /// True when an error message indicates the RPC is rate-limiting requests,
    /// e.g. an HTTP 429 status or a JSON-RPC-level "rate limit" rejection.
    fn is_rate_limited_message(msg: &str) -> bool {
        let lower = msg.to_ascii_lowercase();
        // Prefer phrase matches; only treat bare "429" as HTTP/RPC status tokens
        // (e.g. "status 429", "error code 429", "-320429") rather than any digit run.
        lower.contains("rate limit")
            || lower.contains("too many requests")
            || lower.contains("status 429")
            || lower.contains("code 429")
            || lower.contains(" http 429")
            || lower.split(|c: char| !c.is_ascii_digit()).any(|token| token == "429")
    }

    /// Logs a warning the first time a rate-limited response is observed, so the
    /// user knows the RPC is throttling submissions without flooding the logs on
    /// every occurrence (rate limiting can recur on every batch under load).
    fn warn_rate_limited_once(message: &str) {
        if RATE_LIMIT_WARNED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            warn!(
                error = %message,
                "submission RPC is rate-limiting requests; backing off exponentially and \
                 retrying (this warning is logged once per process; check `top failures` for \
                 ongoing rate-limit counts, consider lowering max_concurrent_submit_requests or \
                 adding more transaction_submission_rpcs endpoints)"
            );
        }
    }

    /// Computes EIP-1559 `maxFeePerGas` for submissions.
    ///
    /// The result is `min(target, max_gas_price)`, where
    /// `target = max(base_fee * MULTIPLIER, base_fee + priority_fee)`.
    /// When `max_gas_price` is lower than the target, the tx may be unincludable.
    pub fn submission_max_fee(base_fee: u128, priority_fee: u128, max_gas_price: u128) -> u128 {
        let target = base_fee
            .saturating_mul(MAX_FEE_BASE_FEE_MULTIPLIER)
            .max(base_fee.saturating_add(priority_fee));
        target.min(max_gas_price)
    }

    /// Signs `prepared` at the given nonce and EIP-1559 [`Fees`], without allocating a
    /// nonce or touching a [`SignerContext`]. Shared by the batch signer stage
    /// ([`Self::sign_prepared`]) and any caller building a transaction outside the
    /// pipeline (e.g. setup/funding/draining flows) so there is exactly one
    /// request-building and signing implementation.
    pub fn sign_at_nonce(
        signer: &PrivateKeySigner,
        prepared: &PreparedTransaction,
        chain_id: u64,
        nonce: u64,
        fees: Fees,
    ) -> Result<SignedTransaction> {
        let mut tx = TransactionRequest::default()
            .with_from(prepared.from)
            .with_value(prepared.value)
            .with_input(prepared.data.clone())
            .with_nonce(nonce)
            .with_chain_id(chain_id)
            .with_max_fee_per_gas(fees.max_fee)
            .with_max_priority_fee_per_gas(fees.priority_fee)
            .with_gas_limit(prepared.gas_limit);
        if let Some(to) = prepared.to {
            tx = tx.with_to(to);
        }

        let typed_tx = tx
            .build_typed_tx()
            .map_err(|e| BaselineError::Transaction(format!("failed to build typed tx: {e:?}")))?;
        let sig_hash = typed_tx.signature_hash();
        let signature = signer
            .sign_hash_sync(&sig_hash)
            .map_err(|e| BaselineError::Transaction(format!("failed to sign tx: {e}")))?;
        let signed = typed_tx.into_signed(signature);
        let tx_hash = *signed.hash();
        let raw = Bytes::from(signed.encoded_2718());
        Ok(SignedTransaction {
            raw,
            tx_hash,
            from: prepared.from,
            nonce,
            gas_limit: prepared.gas_limit,
            estimated_gas: prepared.estimated_gas,
            validity: prepared.validity.clone(),
            cohort: prepared.cohort,
        })
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
                break;
            };

            let batch_id = batch.id;
            let signed_batch = Self::sign_batch(&ctx, batch).await;
            queue.pending_batches.fetch_sub(1, Ordering::SeqCst);

            let Some(signed_batch) = signed_batch else {
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
                break;
            };

            let batch_id = batch.id;
            Self::send_batch(ctx.clone(), batch, &shutdown).await;
            queue.pending_batches.fetch_sub(1, Ordering::SeqCst);
            let _ = ctx
                .submit_event_tx
                .send(SubmitEvent::BatchCompleted { id: batch_id, completed_at: Instant::now() })
                .await;
        }
    }

    async fn sign_batch(ctx: &SignerContext, batch: PreparedBatch) -> Option<SignedBatch> {
        let mut signed_txs = Vec::with_capacity(batch.txs.len());
        for prepared in batch.txs {
            if let Some(tx) = Self::sign_prepared(ctx, &prepared, batch.base_fee).await {
                signed_txs.push(tx);
            } else {
                Self::release_prepared(&ctx.submit_event_tx, &prepared).await;
            }
        }

        (!signed_txs.is_empty()).then_some(SignedBatch {
            id: batch.id,
            attempt: 0,
            measured: true,
            txs: signed_txs,
        })
    }

    async fn send_batch(
        ctx: SenderContext,
        mut batch: SignedBatch,
        shutdown: &CancellationToken,
    ) -> u64 {
        let batch_id = batch.id;
        let measured = batch.measured;
        let mut submitted = 0u64;

        // Register deterministic signed hashes before awaiting the RPC response so a very fast
        // local chain cannot mine a transaction before the block watcher knows about it.
        ctx.results_tracker.sent_transactions(
            batch
                .txs
                .iter()
                .map(|signed| SentTransaction {
                    tx_hash: signed.tx_hash,
                    from: signed.from,
                    estimated_gas: signed.estimated_gas,
                    measured,
                    cohort: signed.cohort,
                })
                .collect(),
        );

        loop {
            if batch.txs.is_empty() {
                return submitted;
            }

            let attempt = batch.attempt;
            let submit_items: Vec<SubmitItem> = batch
                .txs
                .iter()
                .map(|s| SubmitItem::with_validity(s.raw.clone(), s.validity.clone()))
                .collect();
            let rpc_index = batch_id as usize % ctx.submission_batch_rpcs.len();
            let batch_results = match ctx.submission_batch_rpcs[rpc_index]
                .send_raw_transactions(&submit_items, ctx.submit_request_limiter.as_deref())
                .await
            {
                Ok(results) => results,
                Err(e) => {
                    let rate_limited = Self::is_rate_limited_message(&e.to_string());
                    if rate_limited {
                        Self::warn_rate_limited_once(&e.to_string());
                    }

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

                    debug!(
                        batch_id,
                        attempt,
                        next_attempt = attempt + 1,
                        error = %e,
                        count = batch.txs.len(),
                        "batch RPC failed, retrying signed batch"
                    );
                    batch.attempt += 1;
                    let delay = if rate_limited {
                        Self::rate_limit_retry_delay(batch.attempt)
                    } else {
                        Self::submit_retry_delay(batch.attempt)
                    };
                    if !Self::wait_submit_retry(shutdown, delay).await {
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
            let mut retry_rate_limited_txs = Vec::new();
            let mut terminal_rejections = 0usize;
            let mut retry_unknown_error = None;
            let mut retry_rejected_error = None;
            let mut retry_rate_limited_error = None;
            let mut terminal_rejection_error = None;

            for (signed, result) in batch.txs.into_iter().zip(batch_results) {
                match result {
                    BatchSendResult::Success(hash) => {
                        submitted += Self::record_submitted(&ctx, signed, hash, measured).await;
                    }
                    BatchSendResult::Error(err) => match Self::classify_batch_error(err.message) {
                        BatchTxError::AlreadyKnown => {
                            let tx_hash = signed.tx_hash;
                            submitted +=
                                Self::record_submitted(&ctx, signed, tx_hash, measured).await;
                        }
                        BatchTxError::RetryableRejected(message) => {
                            retry_rejected_error.get_or_insert(message);
                            retry_rejected_txs.push(signed);
                        }
                        BatchTxError::RetryableUnknown(message) => {
                            retry_unknown_error.get_or_insert(message);
                            retry_unknown_txs.push(signed);
                        }
                        BatchTxError::RateLimited(message) => {
                            Self::warn_rate_limited_once(&message);
                            retry_rate_limited_error.get_or_insert(message);
                            retry_rate_limited_txs.push(signed);
                        }
                        BatchTxError::NonceTooLow => {
                            // Nonce-too-low means the nonce was already consumed on-chain,
                            // either by a prior attempt or by the same tx landing before
                            // the response arrived. Treat as submitted to avoid returning
                            // the nonce (which would cause replacement-tx-underpriced cycles).
                            let tx_hash = signed.tx_hash;
                            submitted +=
                                Self::record_submitted(&ctx, signed, tx_hash, measured).await;
                        }
                        BatchTxError::Rejected(message) => {
                            terminal_rejections = terminal_rejections.saturating_add(1);
                            if terminal_rejection_error.is_none() {
                                terminal_rejection_error = Some(message.clone());
                            }
                            ctx.results_tracker.discard_transaction(signed.tx_hash);
                            Self::return_signed_nonce(&ctx, &signed).await;
                            Self::release_signed(&ctx.submit_event_tx, &signed, false).await;
                            let _ = ctx.submit_event_tx.send(SubmitEvent::Failed(message)).await;
                        }
                    },
                }
            }

            let retry_unknown_count = retry_unknown_txs.len();
            let retry_rejected_count = retry_rejected_txs.len();
            let retry_rate_limited_count = retry_rate_limited_txs.len();
            if retry_unknown_count > 0
                || retry_rejected_count > 0
                || retry_rate_limited_count > 0
                || terminal_rejections > 0
            {
                debug!(
                    batch_id,
                    attempt,
                    retry_unknown_count,
                    retry_rejected_count,
                    retry_rate_limited_count,
                    terminal_rejections,
                    retry_unknown_error = ?retry_unknown_error.as_deref(),
                    retry_rejected_error = ?retry_rejected_error.as_deref(),
                    retry_rate_limited_error = ?retry_rate_limited_error.as_deref(),
                    terminal_rejection_error = ?terminal_rejection_error.as_deref(),
                    "batch submission contained transaction errors"
                );
            }

            if retry_unknown_txs.is_empty()
                && retry_rejected_txs.is_empty()
                && retry_rate_limited_txs.is_empty()
            {
                return submitted;
            }

            let rate_limited = !retry_rate_limited_txs.is_empty();
            retry_rejected_txs.extend(retry_rate_limited_txs);

            if attempt + 1 >= SUBMIT_MAX_ATTEMPTS {
                let failed = retry_unknown_txs.len() + retry_rejected_txs.len();
                warn!(
                    batch_id,
                    attempt,
                    failed,
                    retry_unknown_error = ?retry_unknown_error.as_deref(),
                    retry_rejected_error = ?retry_rejected_error.as_deref(),
                    retry_rate_limited_error = ?retry_rate_limited_error.as_deref(),
                    "retryable tx errors exceeded max attempts"
                );
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
            batch = SignedBatch {
                id: batch_id,
                attempt: attempt + 1,
                measured,
                txs: retry_unknown_txs,
            };
            // A batch can mix rate-limited and other retryable errors; use the
            // longer rate-limit backoff for the whole retry when any tx hit one,
            // since resending immediately would likely just be rate-limited again.
            let delay = if rate_limited {
                Self::rate_limit_retry_delay(batch.attempt)
            } else {
                Self::submit_retry_delay(batch.attempt)
            };
            if !Self::wait_submit_retry(shutdown, delay).await {
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
        measured: bool,
    ) -> u64 {
        let tracked_hash = if tx_hash != signed.tx_hash {
            debug!(
                local = %signed.tx_hash,
                server = %tx_hash,
                "tx hash mismatch, using server hash"
            );
            ctx.results_tracker.discard_transaction(signed.tx_hash);
            ctx.results_tracker.sent_transactions(vec![SentTransaction {
                tx_hash,
                from: signed.from,
                estimated_gas: signed.estimated_gas,
                measured,
                cohort: signed.cohort,
            }]);
            tx_hash
        } else {
            signed.tx_hash
        };
        Self::release_signed(&ctx.submit_event_tx, &signed, true).await;
        let _ = ctx.submit_event_tx.send(SubmitEvent::Submitted(tracked_hash)).await;
        1
    }

    async fn fail_signed_batch(
        submit_event_tx: &mpsc::Sender<SubmitEvent>,
        signed_txs: Vec<SignedTransaction>,
        reason: &'static str,
    ) {
        for signed in signed_txs {
            Self::release_signed(submit_event_tx, &signed, false).await;
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
            ctx.results_tracker.discard_transaction(signed.tx_hash);
            Self::return_signed_nonce(ctx, &signed).await;
            Self::release_signed(submit_event_tx, &signed, false).await;
            let _ = submit_event_tx.send(SubmitEvent::Failed(reason.into())).await;
        }
    }

    async fn return_signed_nonce(ctx: &SenderContext, signed: &SignedTransaction) {
        if !ctx.return_reserved_nonces {
            return;
        }

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
        let _ = submit_event_tx
            .send(SubmitEvent::Released {
                from: prepared.from,
                estimated_gas: prepared.estimated_gas,
                accepted: false,
            })
            .await;
    }

    async fn release_signed(
        submit_event_tx: &mpsc::Sender<SubmitEvent>,
        signed: &SignedTransaction,
        accepted: bool,
    ) {
        let _ = submit_event_tx
            .send(SubmitEvent::Released {
                from: signed.from,
                estimated_gas: signed.estimated_gas,
                accepted,
            })
            .await;
    }

    async fn sign_prepared(
        ctx: &SignerContext,
        prepared: &PreparedTransaction,
        base_fee: u128,
    ) -> Option<SignedTransaction> {
        let fees = GasPricer::new(ctx.max_gas_price).fees_for_cohort(
            base_fee,
            prepared.cohort,
            ctx.validity_priority_fee_divisor,
            ctx.validity_priority_lead_multiplier,
        );

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

        let signed = match Self::sign_at_nonce(signer, prepared, ctx.chain_id, nonce, fees) {
            Ok(signed) => signed,
            Err(e) => {
                warn!(from = %prepared.from, nonce, error = %e, "failed to build or sign tx");
                nonce_guard.rollback();
                let _ = ctx
                    .submit_event_tx
                    .send(SubmitEvent::Failed("tx build or sign failed".into()))
                    .await;
                return None;
            }
        };

        // Drop the nonce guard immediately after signing. The guard holds
        // the NonceManager mutex; keeping it alive until after RPC send
        // would serialize unrelated network latency through nonce allocation.
        drop(nonce_guard);

        Some(signed)
    }

    fn submit_retry_delay(attempt: u32) -> Duration {
        let millis = 50u64.saturating_mul(1u64 << attempt.min(6));
        Duration::from_millis(millis.min(2_000))
    }

    /// Backoff for rate-limited (HTTP 429) retries. Starts higher and caps far
    /// higher than [`Self::submit_retry_delay`] since rate limits typically take
    /// longer to clear than transient transport blips.
    fn rate_limit_retry_delay(attempt: u32) -> Duration {
        let millis = 500u64.saturating_mul(1u64 << attempt.min(6));
        Duration::from_millis(millis.min(30_000))
    }

    async fn wait_submit_retry(shutdown: &CancellationToken, delay: Duration) -> bool {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => false,
            () = tokio::time::sleep(delay) => true,
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
    use alloy_signer_local::PrivateKeySigner;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use super::{
        BatchTxError, Fees, GasPricer, MAX_FEE_BASE_FEE_MULTIPLIER, MIN_PRIORITY_FEE,
        PipelineQueue, PreparedBatch, PreparedTransaction, SignedBatch, SignedTransaction,
        SubmissionPipeline, SubmitCohort, SubmitEvent,
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
    fn batch_error_classification_identifies_rate_limiting() {
        assert_eq!(
            SubmissionPipeline::classify_batch_error("over rate limit".to_string()),
            BatchTxError::RateLimited("over rate limit".to_string()),
        );
        assert_eq!(
            SubmissionPipeline::classify_batch_error(
                "batch send request returned HTTP 429 Too Many Requests: ...".to_string()
            ),
            BatchTxError::RateLimited(
                "batch send request returned HTTP 429 Too Many Requests: ...".to_string()
            ),
        );
    }

    #[test]
    fn rate_limit_retry_delay_starts_higher_and_caps_higher_than_default_backoff() {
        assert!(
            SubmissionPipeline::rate_limit_retry_delay(0)
                > SubmissionPipeline::submit_retry_delay(0)
        );
        assert_eq!(SubmissionPipeline::rate_limit_retry_delay(10), Duration::from_secs(30));
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
    fn submission_max_fee_honors_absolute_cap() {
        assert_eq!(SubmissionPipeline::submission_max_fee(0, 1, 1_000_000_000), 1);
        assert_eq!(SubmissionPipeline::submission_max_fee(1_000, 10, 500), 500);
        assert_eq!(SubmissionPipeline::submission_max_fee(0, 10, 1), 1);
    }

    #[test]
    fn submission_max_fee_applies_base_fee_multiplier() {
        // 4x base fee dominates when it exceeds base_fee + priority_fee.
        assert_eq!(SubmissionPipeline::submission_max_fee(100, 10, 1_000_000_000), 400);
    }

    #[test]
    fn submission_max_fee_covers_base_fee_plus_tip() {
        // Verifies a transaction is never minted underwater: max_fee always
        // covers base_fee + priority_fee when the cap is not binding.
        let base_fee = 1_000_000u128;
        let priority_fee = 100u128;
        let cap = 10_000_000_000u128;
        let max_fee = SubmissionPipeline::submission_max_fee(base_fee, priority_fee, cap);
        assert!(max_fee >= base_fee + priority_fee, "max_fee must cover base fee + tip");
        assert_eq!(max_fee, base_fee * MAX_FEE_BASE_FEE_MULTIPLIER);
    }

    #[test]
    fn gas_pricer_fees_for_matches_submission_max_fee() {
        let pricer = GasPricer::new(2_000_000_000);
        let fees = pricer.fees_for(100);
        let expected_priority_fee = (100 / 10).max(MIN_PRIORITY_FEE);
        assert_eq!(fees.priority_fee, expected_priority_fee);
        assert_eq!(
            fees.max_fee,
            SubmissionPipeline::submission_max_fee(100, expected_priority_fee, 2_000_000_000)
        );
    }

    #[test]
    fn gas_pricer_funding_fees_uses_two_x_base_fee_and_one_wei_priority() {
        let pricer = GasPricer::new(1_000_000_000);
        let fees = pricer.funding_fees_for(100);
        assert_eq!(fees.priority_fee, 1);
        assert_eq!(fees.max_fee, 200);

        let capped = GasPricer::new(150).funding_fees_for(100);
        assert_eq!(capped, Fees { max_fee: 150, priority_fee: 1 });
    }

    #[test]
    fn gas_pricer_fees_for_honors_max_gas_price_floor_and_cap() {
        // Priority fee stays at the configured minimum when the cap permits it.
        let pricer = GasPricer::new(2_000_000_000);
        assert_eq!(pricer.fees_for(0).priority_fee, MIN_PRIORITY_FEE);

        // Both fee fields are capped at max_gas_price.
        let capped_pricer = GasPricer::new(500);
        let fees = capped_pricer.fees_for(1_000_000);
        assert_eq!(fees.max_fee, 500);
        assert_eq!(fees.priority_fee, 0);
    }

    #[test]
    fn gas_pricer_divides_only_validity_tip_and_covers_it() {
        let pricer = GasPricer::new(10_000);
        let plain = pricer.fees_for_cohort(100, SubmitCohort::Plain, 5, 3);
        let validity = pricer.fees_for_cohort(100, SubmitCohort::ValidityPass, 5, 3);
        let priority_lead =
            pricer.fees_for_cohort(100, SubmitCohort::ValidityPassPriorityLead, 5, 3);

        assert_eq!(plain.priority_fee, 10);
        assert_eq!(validity.priority_fee, 2);
        assert_eq!(priority_lead.priority_fee, 30);
        assert!(validity.max_fee >= 102);

        let capped = GasPricer::new(130).fees_for_cohort(100, SubmitCohort::ValidityPass, 5, 3);
        assert_eq!(capped, Fees { max_fee: 130, priority_fee: 2 });
        let capped_lead =
            GasPricer::new(120).fees_for_cohort(100, SubmitCohort::ValidityPassPriorityLead, 5, 3);
        assert_eq!(capped_lead, Fees { max_fee: 120, priority_fee: 20 });
    }

    #[test]
    fn gas_pricer_bumped_scales_and_caps_at_max_gas_price() {
        let pricer = GasPricer::new(1_000);
        let fees = Fees { max_fee: 100, priority_fee: 10 };

        let bumped = pricer.bumped(fees, 3);
        assert_eq!(bumped, Fees { max_fee: 300, priority_fee: 30 });

        // A bump that would exceed max_gas_price is capped, not rejected.
        let over_cap = pricer.bumped(Fees { max_fee: 500, priority_fee: 500 }, 3);
        assert_eq!(over_cap, Fees { max_fee: 1_000, priority_fee: 1_000 });
    }

    #[test]
    fn sign_at_nonce_produces_deterministic_hash_for_same_inputs() {
        let signer = PrivateKeySigner::random();
        let prepared = PreparedTransaction {
            from: signer.address(),
            to: Some(Address::repeat_byte(0xAB)),
            value: U256::from(1),
            data: Bytes::new(),
            gas_limit: 21_000,
            estimated_gas: 12_345,
            validity: Vec::new(),
            cohort: SubmitCohort::Plain,
        };
        let fees = Fees { max_fee: 100, priority_fee: 10 };

        let signed_a = SubmissionPipeline::sign_at_nonce(&signer, &prepared, 8453, 5, fees)
            .expect("signing should succeed");
        let signed_b = SubmissionPipeline::sign_at_nonce(&signer, &prepared, 8453, 5, fees)
            .expect("signing should succeed");

        assert_eq!(signed_a.tx_hash, signed_b.tx_hash);
        assert_eq!(signed_a.nonce, 5);
        assert_eq!(signed_a.from, signer.address());
        assert_eq!(signed_a.estimated_gas, 12_345);
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
                base_fee: 1,
                txs: vec![PreparedTransaction {
                    from: sender,
                    to: None,
                    value: U256::ZERO,
                    data: Bytes::new(),
                    gas_limit: 21_000,
                    estimated_gas: 21_000,
                    validity: Vec::new(),
                    cohort: SubmitCohort::Plain,
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
                measured: true,
                txs: vec![SignedTransaction {
                    raw: Bytes::new(),
                    tx_hash: TxHash::ZERO,
                    from: sender,
                    nonce: 0,
                    gas_limit: 21_000,
                    estimated_gas: 21_000,
                    validity: Vec::new(),
                    cohort: SubmitCohort::Plain,
                }],
            })
            .await
            .expect("signed queue open");
        drop(signed_batch_tx);

        let pipeline = SubmissionPipeline {
            prepared_batch_tx: None,
            signed_batch_tx: None,
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

    #[tokio::test]
    async fn close_and_fail_queued_cancels_workers_before_locking_receivers() {
        let (_prepared_tx, prepared_rx) = mpsc::channel(1);
        let (_signed_tx, signed_rx) = mpsc::channel(1);
        let prepared_queue = Arc::new(PipelineQueue::new(prepared_rx));
        let signed_queue = Arc::new(PipelineQueue::new(signed_rx));
        let shutdown = CancellationToken::new();
        let queue = Arc::clone(&signed_queue);
        let worker_shutdown = shutdown.clone();
        let (locked_tx, locked_rx) = oneshot::channel();
        let holder = tokio::spawn(async move {
            let _receiver = queue.receiver.lock().await;
            let _ = locked_tx.send(());
            worker_shutdown.cancelled().await;
        });
        locked_rx.await.expect("receiver holder must start");

        let pipeline = SubmissionPipeline {
            prepared_batch_tx: None,
            signed_batch_tx: None,
            prepared_queue,
            signed_queue,
            shutdown,
            signer_workers: Vec::new(),
            sender_workers: Vec::new(),
        };
        tokio::time::timeout(
            Duration::from_millis(100),
            pipeline.close_and_fail_queued("submit queue abandoned"),
        )
        .await
        .expect("queue close must release receiver holders before acquiring their locks");
        holder.await.expect("receiver holder must stop cleanly");
    }

    #[tokio::test]
    async fn shutdown_timeout_is_shared_across_all_workers() {
        let (_prepared_tx, prepared_rx) = mpsc::channel(1);
        let (_signed_tx, signed_rx) = mpsc::channel(1);
        let blocked_worker = || tokio::spawn(std::future::pending::<()>());
        let mut pipeline = SubmissionPipeline {
            prepared_batch_tx: None,
            signed_batch_tx: None,
            prepared_queue: Arc::new(PipelineQueue::new(prepared_rx)),
            signed_queue: Arc::new(PipelineQueue::new(signed_rx)),
            shutdown: CancellationToken::new(),
            signer_workers: vec![blocked_worker(), blocked_worker()],
            sender_workers: vec![blocked_worker(), blocked_worker()],
        };

        tokio::time::timeout(
            Duration::from_millis(250),
            pipeline.shutdown_and_join(Duration::from_millis(100)),
        )
        .await
        .expect("worker shutdown must use one shared timeout");
    }

    #[test]
    fn rate_limit_detection_matches_status_tokens_not_embedded_digits() {
        assert!(SubmissionPipeline::is_rate_limited_message("HTTP 429 Too Many Requests"));
        assert!(SubmissionPipeline::is_rate_limited_message("over rate limit"));
        assert!(SubmissionPipeline::is_rate_limited_message("error code 429"));
        assert!(!SubmissionPipeline::is_rate_limited_message("gas estimate 42900 exceeds limit"));
        assert!(!SubmissionPipeline::is_rate_limited_message("nonce 429123"));
    }

    #[test]
    fn request_limit_expands_submission_worker_pools() {
        assert_eq!(SubmissionPipeline::signer_worker_count(1, None), 10);
        assert_eq!(SubmissionPipeline::signer_worker_count(1, Some(32)), 32);
        assert_eq!(SubmissionPipeline::signer_worker_count(2, Some(8)), 20);
        assert_eq!(SubmissionPipeline::signer_worker_count(1, Some(128)), 32);
        assert_eq!(SubmissionPipeline::sender_worker_count(1, None), 10);
        assert_eq!(SubmissionPipeline::sender_worker_count(1, Some(32)), 32);
        assert_eq!(SubmissionPipeline::sender_worker_count(2, Some(8)), 20);
        assert_eq!(SubmissionPipeline::sender_worker_count(1, Some(128)), 64);
    }
}
