//! Transaction submission: a pool of sender workers draining one lock-free batch queue.

use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use alloy_consensus::{TxEip1559, transaction::SignableTransaction};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature, TxHash, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{Call, Eip8130Signed, TxEip8130};
use base_execution_txpool::{ValidityAuthorization, ValidityPredicate};
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::{RunEvent, SentTransaction, TxOutcome, TxResult};
use crate::{
    BaselineError, Result,
    config::ValiditySigningMode,
    rpc::{BatchRpcClient, BatchSendResult, SubmitItem},
};

/// Number of sender tasks per submission RPC.
pub const SENDER_WORKERS_PER_RPC: usize = 10;
/// Maximum sender task count.
pub const MAX_SENDER_WORKER_COUNT: usize = 64;
/// Number of queued signed batches allowed before backpressure.
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
            Self::ValidityPass => crate::metrics::SubmitCohortLabel::ValidityPass,
            Self::ValidityPassPriorityLead => {
                crate::metrics::SubmitCohortLabel::ValidityPassPriorityLead
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
    /// Sign a native EIP-8430 EOA self-pay envelope rather than EIP-1559.
    pub native: bool,
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
    /// Priority fee per gas the transaction was signed with.
    pub priority_fee: u128,
    /// Calibrated execution gas used for pacing.
    pub estimated_gas: u64,
    /// Resolved validity predicates transported with the transaction. Empty for
    /// the plain cohort.
    pub validity: Vec<ValidityPredicate>,
    /// Submission cohort this transaction is routed to.
    pub cohort: SubmitCohort,
    /// Sender authorization for resolved predicates, produced at signing time and attached only
    /// when the validity signing mode or the endpoint requires it. Absent for plain submissions.
    pub validity_signature: Option<Signature>,
}

impl SignedTransaction {
    /// Borrows this transaction as a submission item, attaching its validity authorization only
    /// when `sign_validity` is set.
    pub fn submit_item(&self, sign_validity: bool) -> SubmitItem<'_> {
        SubmitItem {
            raw: &self.raw,
            validity: &self.validity,
            validity_signature: self.validity_signature.filter(|_| sign_validity),
        }
    }

    /// Describes this transaction as it enters submission.
    pub const fn sent(&self, measured: bool) -> SentTransaction {
        SentTransaction {
            tx_hash: self.tx_hash,
            from: self.from,
            gas_limit: self.gas_limit,
            priority_fee: self.priority_fee,
            estimated_gas: self.estimated_gas,
            measured,
            cohort: self.cohort,
        }
    }

    /// Pairs this transaction with its final submission outcome.
    pub const fn result(&self, measured: bool, outcome: TxOutcome) -> TxResult {
        TxResult {
            tx_hash: self.tx_hash,
            from: self.from,
            estimated_gas: self.estimated_gas,
            measured,
            outcome,
        }
    }

    /// Signs this transaction's immutable predicate batch without changing its raw bytes or nonce.
    pub fn authorize_validity(&mut self, signer: &PrivateKeySigner, chain_id: u64) -> Result<()> {
        if self.validity.is_empty() || self.validity_signature.is_some() {
            return Ok(());
        }
        if signer.address() != self.from {
            return Err(BaselineError::Transaction(
                "validity signer does not match transaction sender".into(),
            ));
        }
        self.validity_signature = Some(
            signer
                .sign_hash_sync(&ValidityAuthorization::signing_hash(
                    chain_id,
                    self.tx_hash,
                    &self.validity,
                ))
                .map_err(|e| {
                    BaselineError::Transaction(format!("failed to sign validity sidecar: {e}"))
                })?,
        );
        Ok(())
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

/// Shared sender worker context.
#[derive(Debug, Clone)]
pub struct SenderContext {
    /// Explicit signing or one-way adaptive signing during migration.
    pub validity_signing: ValiditySigningMode,
    /// Transaction submission RPC clients.
    pub submission_batch_rpcs: Arc<[BatchRpcClient]>,
    /// Run coordinator inbox.
    pub events: mpsc::Sender<RunEvent>,
    /// Optional cap on concurrent outbound submission RPC requests, shared across
    /// all sender workers. Bounds request *rate* to the endpoint independently of
    /// how many transactions are in flight (unconfirmed) or how many sender
    /// workers exist. `None` leaves concurrency bounded only by the sender worker
    /// count.
    pub submit_request_limiter: Option<Arc<Semaphore>>,
}

/// Pool of sender workers draining one bounded multi-consumer batch queue.
///
/// Workers pull batches without a shared lock. Queue capacity is the backpressure on the
/// coordinator, which owns all queued and outstanding accounting.
#[derive(Debug)]
pub struct SubmissionPipeline {
    batch_tx: Option<async_channel::Sender<SignedBatch>>,
    batch_rx: async_channel::Receiver<SignedBatch>,
    shutdown: CancellationToken,
    sender_workers: Vec<JoinHandle<()>>,
}

/// Runtime configuration for submission pipeline workers.
#[derive(Debug, Clone, Copy)]
pub struct PipelineStartConfig {
    /// Sidecar signing strategy during staged rollout.
    pub validity_signing: ValiditySigningMode,
    /// Optional cap on concurrent outbound submission RPC requests across all
    /// sender workers and per-batch RPC chunks. `None` leaves those requests
    /// unconstrained by a shared semaphore.
    pub max_concurrent_submit_requests: Option<usize>,
}

impl SubmissionPipeline {
    /// Starts the sender workers.
    pub fn start(
        submission_batch_rpcs: Arc<[BatchRpcClient]>,
        events: mpsc::Sender<RunEvent>,
        config: PipelineStartConfig,
    ) -> Self {
        let (batch_tx, batch_rx) = async_channel::bounded(SUBMIT_BATCH_QUEUE_BUFFER);
        let shutdown = CancellationToken::new();
        let ctx = SenderContext {
            validity_signing: config.validity_signing,
            submit_request_limiter: config
                .max_concurrent_submit_requests
                .map(|max| Arc::new(Semaphore::new(max.max(1)))),
            submission_batch_rpcs,
            events,
        };
        let sender_workers = (0..Self::sender_worker_count(
            ctx.submission_batch_rpcs.len(),
            config.max_concurrent_submit_requests,
        ))
            .map(|_| {
                tokio::spawn(Self::sender_worker(ctx.clone(), batch_rx.clone(), shutdown.clone()))
            })
            .collect();

        Self { batch_tx: Some(batch_tx), batch_rx, shutdown, sender_workers }
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

    /// Enqueues a signed batch, waiting while the queue is full. Returns the batch when the
    /// queue is closed.
    pub async fn enqueue(&self, batch: SignedBatch) -> std::result::Result<(), SignedBatch> {
        match &self.batch_tx {
            Some(tx) => tx.send(batch).await.map_err(|error| error.0),
            None => Err(batch),
        }
    }

    /// Stops accepting batches; workers exit once the queue drains.
    pub fn close_input(&mut self) {
        self.batch_tx = None;
    }

    /// Closes the queue, interrupts retry backoffs, and returns batches no worker picked up.
    pub fn abandon_queued(&self) -> Vec<SignedBatch> {
        self.shutdown.cancel();
        self.batch_rx.close();
        std::iter::from_fn(|| self.batch_rx.try_recv().ok()).collect()
    }

    /// Signals workers to stop and waits for them.
    pub async fn shutdown_and_join(&mut self, timeout: Duration) {
        self.shutdown.cancel();

        let deadline = Instant::now() + timeout;
        for mut worker in self.sender_workers.drain(..) {
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
        } else if Self::is_rate_limited_lower(&lower) {
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
        Self::is_rate_limited_lower(&msg.to_ascii_lowercase())
    }

    fn is_rate_limited_lower(lower: &str) -> bool {
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

    /// Signs `prepared` at the given nonce and EIP-1559 [`Fees`]. Shared by the presign
    /// producer and setup, funding, and draining flows so there is exactly one
    /// request-building and signing implementation.
    pub fn sign_at_nonce(
        signer: &PrivateKeySigner,
        prepared: &PreparedTransaction,
        chain_id: u64,
        nonce: u64,
        fees: Fees,
    ) -> Result<SignedTransaction> {
        if prepared.native {
            let to = prepared.to.ok_or_else(|| {
                BaselineError::Transaction("native transfer requires a recipient".into())
            })?;
            if !prepared.validity.is_empty() || prepared.cohort.is_validity() {
                return Err(BaselineError::Config(
                    "native EIP-8430 workloads do not support validity submission".into(),
                ));
            }
            let tx = TxEip8130 {
                chain_id,
                nonce_sequence: nonce,
                max_fee_per_gas: fees.max_fee,
                max_priority_fee_per_gas: fees.priority_fee,
                gas_limit: prepared.gas_limit,
                calls: vec![if prepared.data.is_empty() {
                    vec![Call { to, value: prepared.value, data: prepared.data.clone() }]
                } else {
                    if prepared.value != U256::ZERO {
                        return Err(BaselineError::Config(
                            "native token swaps require zero ETH value".into(),
                        ));
                    }
                    crate::workload::UniswapV3Payload::native_calls(to, &prepared.data)?
                }],
                ..Default::default()
            };
            let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).map_err(|e| {
                BaselineError::Transaction(format!("failed to sign native tx: {e}"))
            })?;
            let signed =
                Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
            return Ok(SignedTransaction {
                raw: Bytes::from(signed.encoded_2718()),
                tx_hash: *signed.hash(),
                from: prepared.from,
                nonce,
                gas_limit: prepared.gas_limit,
                priority_fee: fees.priority_fee,
                estimated_gas: prepared.estimated_gas,
                validity: Vec::new(),
                validity_signature: None,
                cohort: prepared.cohort,
            });
        }
        let typed_tx = TxEip1559 {
            chain_id,
            nonce,
            gas_limit: prepared.gas_limit,
            max_fee_per_gas: fees.max_fee,
            max_priority_fee_per_gas: fees.priority_fee,
            to: prepared.to.into(),
            value: prepared.value,
            access_list: Default::default(),
            input: prepared.data.clone(),
        };
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
            priority_fee: fees.priority_fee,
            estimated_gas: prepared.estimated_gas,
            validity: prepared.validity.clone(),
            validity_signature: None,
            cohort: prepared.cohort,
        })
    }

    async fn sender_worker(
        ctx: SenderContext,
        batches: async_channel::Receiver<SignedBatch>,
        shutdown: CancellationToken,
    ) {
        loop {
            let batch = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                batch = batches.recv() => match batch {
                    Ok(batch) => batch,
                    Err(_) => break,
                },
            };
            let id = batch.id;
            Self::send_batch(&ctx, batch, &shutdown).await;
            let completed = RunEvent::BatchCompleted { id, completed_at: Instant::now() };
            if ctx.events.send(completed).await.is_err() {
                break;
            }
        }
    }

    /// Reports outcomes to the coordinator. A closed inbox means the run is over, so the
    /// outcome has no reader.
    async fn settle(ctx: &SenderContext, results: Vec<TxResult>) {
        if !results.is_empty() {
            let _ = ctx.events.send(RunEvent::Settled(results)).await;
        }
    }

    fn fail_all(
        txs: &[SignedTransaction],
        measured: bool,
        reason: &'static str,
        discard: bool,
    ) -> Vec<TxResult> {
        txs.iter()
            .map(|tx| {
                tx.result(measured, TxOutcome::Failed { reason: Cow::Borrowed(reason), discard })
            })
            .collect()
    }

    async fn send_batch(ctx: &SenderContext, mut batch: SignedBatch, shutdown: &CancellationToken) {
        let batch_id = batch.id;
        let measured = batch.measured;

        // Register deterministic signed hashes before awaiting the RPC response so a very fast
        // local chain cannot mine a transaction before the coordinator knows about it. The
        // inbox is FIFO, so this lands before any block that includes these transactions.
        let sending = RunEvent::Sending {
            txs: batch.txs.iter().map(|tx| tx.sent(measured)).collect(),
            at: Instant::now(),
            at_wall: SystemTime::now(),
        };
        let _ = ctx.events.send(sending).await;

        let rpc = &ctx.submission_batch_rpcs[batch_id as usize % ctx.submission_batch_rpcs.len()];
        while !batch.txs.is_empty() {
            let attempt = batch.attempt;
            let sign_validity = ctx.validity_signing == ValiditySigningMode::Signed
                || rpc.validity_signatures_required();
            let submit_items: Vec<SubmitItem<'_>> =
                batch.txs.iter().map(|tx| tx.submit_item(sign_validity)).collect();
            let attempt_started_at = SystemTime::now();
            let response = rpc
                .send_raw_transactions(&submit_items, ctx.submit_request_limiter.as_deref())
                .await;
            drop(submit_items);
            let batch_results = match response {
                Ok(results) => results,
                Err(e) => {
                    let message = e.to_string();
                    let rate_limited = Self::is_rate_limited_message(&message);
                    if rate_limited {
                        Self::warn_rate_limited_once(&message);
                    }

                    if attempt + 1 >= SUBMIT_MAX_ATTEMPTS {
                        warn!(
                            batch_id,
                            attempt,
                            error = %e,
                            count = batch.txs.len(),
                            "batch RPC failed after max attempts"
                        );
                        let failed = Self::fail_all(
                            &batch.txs,
                            measured,
                            "batch transport failed after retries",
                            false,
                        );
                        Self::settle(ctx, failed).await;
                        return;
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
                        Self::settle(
                            ctx,
                            Self::fail_all(&batch.txs, measured, "submit worker shutdown", false),
                        )
                        .await;
                        return;
                    }
                    continue;
                }
            };

            let mut settled = Vec::with_capacity(batch.txs.len());
            let mut retry_unknown_txs = Vec::new();
            let mut retry_rejected_txs = Vec::new();
            let mut retry_rate_limited_txs = Vec::new();
            let mut terminal_rejections = 0usize;
            let mut retry_unknown_error = None;
            let mut retry_rejected_error = None;
            let mut retry_rate_limited_error = None;
            let mut terminal_rejection_error = None;

            for (signed, result) in batch.txs.into_iter().zip(batch_results) {
                let outcome = match result {
                    BatchSendResult::Success(hash) if hash == signed.tx_hash => {
                        TxOutcome::Accepted { sent_at: attempt_started_at }
                    }
                    BatchSendResult::Success(hash) => {
                        debug!(
                            local = %signed.tx_hash,
                            server = %hash,
                            "tx hash mismatch, using server hash"
                        );
                        TxOutcome::Rehashed { server: hash, sent_at: attempt_started_at }
                    }
                    BatchSendResult::Error(err)
                        if err.is_signature_required()
                            && !signed.validity.is_empty()
                            && !sign_validity =>
                    {
                        rpc.require_validity_signatures();
                        retry_rejected_error.get_or_insert_with(|| {
                            "validity signature required; retrying signed sidecar".to_string()
                        });
                        retry_rejected_txs.push(signed);
                        continue;
                    }
                    BatchSendResult::Error(err) => match Self::classify_batch_error(err.message) {
                        // Nonce-too-low means the nonce was already consumed on-chain, either by
                        // a prior attempt or by the same tx landing before the response arrived.
                        // Treat it as submitted so the sender is not resynced into
                        // replacement-underpriced cycles.
                        BatchTxError::AlreadyKnown | BatchTxError::NonceTooLow => TxOutcome::Known,
                        BatchTxError::RetryableRejected(message) => {
                            retry_rejected_error.get_or_insert(message);
                            retry_rejected_txs.push(signed);
                            continue;
                        }
                        BatchTxError::RetryableUnknown(message) => {
                            retry_unknown_error.get_or_insert(message);
                            retry_unknown_txs.push(signed);
                            continue;
                        }
                        BatchTxError::RateLimited(message) => {
                            Self::warn_rate_limited_once(&message);
                            retry_rate_limited_error.get_or_insert(message);
                            retry_rate_limited_txs.push(signed);
                            continue;
                        }
                        BatchTxError::Rejected(message) => {
                            terminal_rejections = terminal_rejections.saturating_add(1);
                            if terminal_rejection_error.is_none() {
                                terminal_rejection_error = Some(message.clone());
                            }
                            TxOutcome::Failed { reason: Cow::Owned(message), discard: true }
                        }
                    },
                };
                settled.push(signed.result(measured, outcome));
            }
            Self::settle(ctx, settled).await;

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
                return;
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
                let mut failed = Self::fail_all(
                    &retry_unknown_txs,
                    measured,
                    "tx status unknown after retries",
                    false,
                );
                failed.extend(Self::fail_all(
                    &retry_rejected_txs,
                    measured,
                    "txpool retry failed after retries",
                    true,
                ));
                Self::settle(ctx, failed).await;
                return;
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
                Self::settle(
                    ctx,
                    Self::fail_all(&batch.txs, measured, "submit worker shutdown", false),
                )
                .await;
                return;
            }
        }
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
    use std::{sync::Arc, time::Duration};

    use alloy_eips::Decodable2718;
    use alloy_primitives::{Address, Bytes, TxHash, U256};
    use alloy_signer_local::PrivateKeySigner;
    use base_common_consensus::{BaseTxEnvelope, Call};
    use base_execution_txpool::{
        TransactionValidity, ValidityAuthorizationError, ValidityOperator, ValidityPredicate,
    };
    use httpmock::prelude::*;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::{
        BatchTxError, Fees, GasPricer, MAX_FEE_BASE_FEE_MULTIPLIER, MIN_PRIORITY_FEE,
        PipelineStartConfig, PreparedTransaction, SignedBatch, SignedTransaction,
        SubmissionPipeline, SubmitCohort,
    };
    use crate::{BatchRpcClient, RunEvent, TxOutcome, config::ValiditySigningMode};

    /// Pool pressure remains retryable with bounded exponential RPC backoff.
    #[test]
    fn pool_pressure_retains_bounded_retry_backoff() {
        let message = "RPC error -32011: transaction pool is full".to_string();
        assert_eq!(
            SubmissionPipeline::classify_batch_error(message.clone()),
            BatchTxError::RetryableRejected(message),
            "pool pressure must retain work for retry"
        );
        let delays: Vec<_> =
            (1..super::SUBMIT_MAX_ATTEMPTS).map(SubmissionPipeline::submit_retry_delay).collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
                Duration::from_millis(800)
            ],
            "all permitted retry delays must remain bounded and exponential"
        );
    }

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

    /// Native and Ethereum envelopes share the allocated protocol nonce stream.
    #[test]
    fn eip8430_signing_preserves_native_shape_and_legacy_nonces() {
        let signer = PrivateKeySigner::random();
        let mut prepared = PreparedTransaction {
            native: true,
            from: signer.address(),
            to: Some(Address::repeat_byte(0xab)),
            value: U256::from(1234),
            data: Bytes::new(),
            gas_limit: 200_000,
            estimated_gas: 50_000,
            validity: Vec::new(),
            cohort: SubmitCohort::Plain,
        };
        let fees = Fees { max_fee: 100, priority_fee: 10 };
        for nonce in 7..10 {
            prepared.native = nonce != 8;
            let signed = SubmissionPipeline::sign_at_nonce(&signer, &prepared, 8453, nonce, fees)
                .expect("both envelope types must sign successfully");
            let decoded = BaseTxEnvelope::decode_2718(&mut signed.raw.as_ref())
                .expect("raw bytes must decode as a Base envelope");
            assert_eq!(signed.nonce, nonce, "allocated protocol nonce must be preserved");
            assert_eq!(signed.tx_hash, decoded.tx_hash(), "tracked hash must match wire hash");
            if let BaseTxEnvelope::Eip8130(native) = decoded {
                assert!(prepared.native, "only native selections may produce native envelopes");
                assert_eq!(
                    native.recover_eoa_sender().expect("canonical EOA auth"),
                    Some(signer.address()),
                    "sender authentication must recover the signer"
                );
                let tx = native.tx();
                assert_eq!(tx.chain_id, 8453, "native chain ID must be bound");
                assert_eq!(
                    tx.nonce_key,
                    U256::ZERO,
                    "native transfers must use protocol nonce lane"
                );
                assert_eq!(tx.nonce_sequence, nonce, "native sequence must use allocated nonce");
                assert_eq!(tx.sender, None, "native transfer must use implicit EOA auth");
                assert_eq!(tx.payer, None, "native transfer must self-pay");
                assert_eq!(
                    tx.calls,
                    vec![vec![Call {
                        to: prepared.to.unwrap(),
                        value: prepared.value,
                        data: Bytes::new()
                    }]],
                    "native envelope must contain exactly one value-transfer call"
                );
                assert_eq!(
                    native.sender_auth().len(),
                    65,
                    "EOA signature must have no authenticator prefix"
                );
                assert!(native.payer_auth().is_empty(), "self-payment needs no payer auth");
            } else {
                assert!(
                    matches!(decoded, BaseTxEnvelope::Eip1559(_)),
                    "legacy transfer must remain EIP-1559"
                );
                assert!(
                    !prepared.native,
                    "native requests must never fall back to Ethereum signing"
                );
            }
        }
        prepared.native = true;
        prepared.to = None;
        assert!(
            SubmissionPipeline::sign_at_nonce(&signer, &prepared, 8453, 10, fees).is_err(),
            "native transfer must reject contract creation"
        );
    }

    #[test]
    fn sign_at_nonce_produces_deterministic_hash_for_same_inputs() {
        let signer = PrivateKeySigner::random();
        let prepared = PreparedTransaction {
            from: signer.address(),
            native: false,
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

    fn idle_pipeline(workers: Vec<tokio::task::JoinHandle<()>>) -> SubmissionPipeline {
        let (batch_tx, batch_rx) = async_channel::bounded(2);
        SubmissionPipeline {
            batch_tx: Some(batch_tx),
            batch_rx,
            shutdown: CancellationToken::new(),
            sender_workers: workers,
        }
    }

    fn plain_batch(id: u64) -> SignedBatch {
        SignedBatch {
            id,
            attempt: 0,
            measured: true,
            txs: vec![SignedTransaction {
                raw: Bytes::new(),
                tx_hash: TxHash::repeat_byte(id as u8),
                from: Address::repeat_byte(0x11),
                nonce: id,
                gas_limit: 21_000,
                priority_fee: 0,
                estimated_gas: 21_000,
                validity: Vec::new(),
                cohort: SubmitCohort::Plain,
                validity_signature: None,
            }],
        }
    }

    #[tokio::test]
    async fn abandon_queued_returns_unstarted_batches_and_rejects_new_ones() {
        let pipeline = idle_pipeline(Vec::new());
        pipeline.enqueue(plain_batch(1)).await.expect("queue open");
        pipeline.enqueue(plain_batch(2)).await.expect("queue open");

        let abandoned: Vec<_> = pipeline.abandon_queued().into_iter().map(|b| b.id).collect();

        assert_eq!(abandoned, vec![1, 2]);
        assert!(
            pipeline.enqueue(plain_batch(3)).await.is_err(),
            "abandoned queue must stay closed"
        );
    }

    #[tokio::test]
    async fn closed_input_rejects_new_batches() {
        let mut pipeline = idle_pipeline(Vec::new());
        pipeline.close_input();
        assert_eq!(pipeline.enqueue(plain_batch(1)).await.map_err(|b| b.id), Err(1));
    }

    #[tokio::test]
    async fn shutdown_timeout_is_shared_across_all_workers() {
        let blocked_worker = || tokio::spawn(std::future::pending::<()>());
        let mut pipeline = idle_pipeline((0..4).map(|_| blocked_worker()).collect());

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
    fn request_limit_expands_submission_worker_pool() {
        assert_eq!(SubmissionPipeline::sender_worker_count(1, None), 10);
        assert_eq!(SubmissionPipeline::sender_worker_count(1, Some(32)), 32);
        assert_eq!(SubmissionPipeline::sender_worker_count(2, Some(8)), 20);
        assert_eq!(SubmissionPipeline::sender_worker_count(1, Some(128)), 64);
    }

    #[rstest::rstest]
    #[case::adaptive(ValiditySigningMode::Adaptive)]
    #[case::proactive(ValiditySigningMode::Signed)]
    #[tokio::test]
    async fn validity_signing_migrates_without_resigning_transactions(
        #[case] mode: ValiditySigningMode,
    ) {
        let server = MockServer::start();
        let signer = PrivateKeySigner::random();
        let prepared = PreparedTransaction {
            from: signer.address(),
            native: false,
            to: Some(Address::repeat_byte(0xbb)),
            value: U256::ZERO,
            data: Bytes::new(),
            gas_limit: 21_000,
            estimated_gas: 21_000,
            validity: vec![ValidityPredicate::BlockNumber {
                op: ValidityOperator::LessThanOrEqual,
                value: U256::from(31),
            }],
            cohort: SubmitCohort::ValidityPass,
        };
        let client =
            BatchRpcClient::new(server.base_url().parse().unwrap()).expect("build batch client");
        let (events, mut receiver) = mpsc::channel(32);
        let mut pipeline = SubmissionPipeline::start(
            Arc::from([client.clone()]),
            events,
            PipelineStartConfig { validity_signing: mode, max_concurrent_submit_requests: Some(1) },
        );
        for nonce in 0..2 {
            let mut tx = SubmissionPipeline::sign_at_nonce(
                &signer,
                &prepared,
                8453,
                nonce,
                Fees { max_fee: 1_000_000_000, priority_fee: 1 },
            )
            .unwrap();
            tx.authorize_validity(&signer, 8453).unwrap();
            let hash = tx.tx_hash;
            let unsigned = server.mock(|when, then| {
                when.method(POST).json_body(serde_json::json!([{
                    "jsonrpc":"2.0","id":0,"method":"base_sendRawTransactionValidity",
                    "params":[tx.raw, TransactionValidity { validity: tx.validity.clone(), validity_signature: None }],
                }]));
                then.status(200).json_body(serde_json::json!([{"jsonrpc":"2.0","id":0,"error":{
                    "code":-32602,"message":ValidityAuthorizationError::MissingSignature.to_string(),"data":"missing",
                }}]));
            });
            let signed = server.mock(|when, then| {
                when.method(POST).json_body(serde_json::json!([{
                    "jsonrpc":"2.0","id":0,"method":"base_sendRawTransactionValidity",
                    "params":[tx.raw, TransactionValidity { validity: tx.validity.clone(), validity_signature: tx.validity_signature }],
                }]));
                then.status(200).json_body(serde_json::json!([{"jsonrpc":"2.0","id":0,"result":hash}]));
            });
            pipeline
                .enqueue(SignedBatch { id: nonce, attempt: 0, measured: true, txs: vec![tx] })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let RunEvent::Settled(results) =
                        receiver.recv().await.expect("submission events remain open")
                    else {
                        continue;
                    };
                    let result = &results[0];
                    assert!(
                        matches!(result.outcome, TxOutcome::Accepted { .. }),
                        "validity migration failed: {:?}",
                        result.outcome
                    );
                    assert_eq!(result.tx_hash, hash);
                    return;
                }
            })
            .await
            .expect("migration must complete within its retry budget");
            signed.assert_calls(1);
            unsigned.assert_calls(usize::from(mode == ValiditySigningMode::Adaptive && nonce == 0));
        }
        assert_eq!(client.validity_signatures_required(), mode == ValiditySigningMode::Adaptive);
        pipeline.shutdown_and_join(Duration::from_secs(2)).await;
    }
}
