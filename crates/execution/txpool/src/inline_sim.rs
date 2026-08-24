//! Pre-sim queue shared by `eth_sendRawTransaction` and mempool meter_bundle workers.
//!
//! RPC validates cheaply, then [`InlineSimQueue::try_enqueue`], then waits for the
//! worker to `meter_bundle` and `add_transaction`. Queue-full returns
//! [`reth_transaction_pool::error::PoolErrorKind::DiscardedOnInsert`] (`TxPoolOverflow`).
//! The queue is installed in the node-started hook together with the workers.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::transaction::Recovered;
use alloy_primitives::TxHash;
use base_bundles::MeterBundleResponse;
use base_common_consensus::BaseTxEnvelope;
use futures::future::{self, Either};
use parking_lot::RwLock;
use reth_transaction_pool::{
    AddedTransactionOutcome, PoolTransaction, TransactionOrigin, TransactionPool, error::PoolError,
};
use tokio::sync::{
    mpsc::{self, error::TrySendError},
    oneshot,
};
use tracing::warn;

use crate::BasePooledTransaction;

base_metrics::define_metrics! {
    inline_simulation,
    struct = InlineSimMetrics,
    #[describe("Transactions waiting in the pre-sim queue")]
    sim_queue_size: gauge,
    #[describe("Sim workers currently running meter_bundle")]
    sim_workers_busy: gauge,
    #[describe("Sim worker tasks still in their recv loop")]
    sim_workers_alive: gauge,
    #[describe("Wall time in seconds for one sim worker job (meter_bundle plus pool insert)")]
    sim_seconds: histogram,
    #[describe("Queue-full rejections mapped to TxPoolOverflow")]
    sim_queue_full: counter,
    #[describe("meter_bundle failures that inserted MeterBundleResponse::default")]
    #[label(name = "reason", default = ["timeout", "meter", "join"])]
    sim_failures: counter,
    #[describe("Pool inserts that carried MeterBundleResponse::default after a failed or timed-out sim")]
    sim_default_inserts: counter,
    #[describe("Seconds waiting for meter_bundle to finish after the configured timeout fired")]
    sim_timeout_wait_seconds: histogram,
    #[describe("Enqueued jobs whose pool insert failed (RPC receives the same error)")]
    sim_insert_failures: counter,
}

/// Job waiting for an in-process meter_bundle worker.
pub struct InlineSimJob {
    /// Origin used when the worker later calls `add_transaction`.
    pub origin: TransactionOrigin,
    /// Transaction that already passed cheap pool validation.
    pub transaction: BasePooledTransaction,
    inserted: oneshot::Sender<Result<AddedTransactionOutcome, PoolError>>,
}

impl std::fmt::Debug for InlineSimJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineSimJob")
            .field("origin", &self.origin)
            .field("hash", self.transaction.hash())
            .finish_non_exhaustive()
    }
}

impl InlineSimJob {
    /// Builds a job and the receiver RPC waits on until pool insert finishes.
    pub fn new(
        origin: TransactionOrigin,
        transaction: BasePooledTransaction,
    ) -> (Self, oneshot::Receiver<Result<AddedTransactionOutcome, PoolError>>) {
        let (inserted, rx) = oneshot::channel();
        (Self { origin, transaction, inserted }, rx)
    }
}

/// Why [`InlineSimQueue::try_enqueue`] rejected a job.
#[derive(Debug)]
pub enum InlineSimEnqueueError {
    /// `--enable-inline-simulation` is off, or workers have not installed a queue.
    Disabled(InlineSimJob),
    /// Bounded pre-sim queue is full. RPC maps this to `TxPoolOverflow`.
    Full(TxHash),
}

static QUEUE: RwLock<Option<mpsc::Sender<InlineSimJob>>> = RwLock::new(None);

/// Handle for the mempool pre-sim queue and worker pool.
#[derive(Debug)]
pub struct InlineSimQueue;

impl InlineSimQueue {
    /// Installs the enqueue sender used by `eth_sendRawTransaction`.
    pub fn install(sender: mpsc::Sender<InlineSimJob>) {
        InlineSimMetrics::sim_queue_size().set(0.0);
        *QUEUE.write() = Some(sender);
    }

    /// Returns true when RPC should validate-then-enqueue instead of `add_transaction`.
    pub fn is_enabled() -> bool {
        QUEUE.read().is_some()
    }

    /// Pushes a validated transaction for a sim worker.
    pub fn try_enqueue(job: InlineSimJob) -> Result<(), InlineSimEnqueueError> {
        let Some(sender) = QUEUE.read().clone() else {
            return Err(InlineSimEnqueueError::Disabled(job));
        };
        match sender.try_send(job) {
            Ok(()) => {
                InlineSimMetrics::sim_queue_size().increment(1.0);
                Ok(())
            }
            Err(TrySendError::Full(job)) => {
                InlineSimMetrics::sim_queue_full().increment(1);
                Err(InlineSimEnqueueError::Full(*job.transaction.hash()))
            }
            Err(TrySendError::Closed(job)) => {
                Self::uninstall();
                Err(InlineSimEnqueueError::Disabled(job))
            }
        }
    }

    fn uninstall() {
        *QUEUE.write() = None;
    }

    /// Spawns `workers` tasks that meter and insert queued transactions.
    ///
    /// ponytail: one mutex on recv; split into per-worker channels if lock contends
    pub fn spawn_workers<P, F>(
        pool: P,
        meter: F,
        rx: mpsc::Receiver<InlineSimJob>,
        workers: usize,
        timeout: Duration,
    ) where
        P: TransactionPool<Transaction = BasePooledTransaction> + Clone + Send + 'static,
        F: Fn(Recovered<BaseTxEnvelope>) -> Result<MeterBundleResponse, String>
            + Send
            + Sync
            + 'static,
    {
        if workers == 0 {
            return;
        }
        let meter = Arc::new(meter);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        for _ in 0..workers {
            let meter = Arc::clone(&meter);
            let rx = Arc::clone(&rx);
            let pool = pool.clone();
            tokio::spawn(async move {
                InlineSimMetrics::sim_workers_alive().increment(1.0);
                let _alive = WorkerAlive;
                loop {
                    let job = rx.lock().await.recv().await;
                    let Some(job) = job else {
                        break;
                    };
                    InlineSimMetrics::sim_queue_size().decrement(1.0);

                    let started = Instant::now();
                    let job = meter_job(job, Arc::clone(&meter), timeout).await;
                    let InlineSimJob { origin, transaction, inserted } = job;
                    let result = pool.add_transaction(origin, transaction).await;
                    notify_insert(inserted, result);
                    InlineSimMetrics::sim_seconds().record(started.elapsed().as_secs_f64());
                }
            });
        }
    }

    /// Drops the enqueue sender so workers exit after draining.
    #[cfg(test)]
    pub fn clear() {
        Self::uninstall();
        InlineSimMetrics::sim_queue_size().set(0.0);
        InlineSimMetrics::sim_workers_busy().set(0.0);
        InlineSimMetrics::sim_workers_alive().set(0.0);
    }

    /// Attaches [`MeterBundleResponse::default`] after a failed or timed-out
    /// sim so the Consumer can still forward.
    pub fn with_default_metering(job: InlineSimJob, reason: &'static str) -> InlineSimJob {
        InlineSimMetrics::sim_failures(reason).increment(1);
        InlineSimMetrics::sim_default_inserts().increment(1);
        InlineSimJob {
            origin: job.origin,
            transaction: job.transaction.with_metering(MeterBundleResponse::default()),
            inserted: job.inserted,
        }
    }
}

/// Decrements [`InlineSimMetrics::sim_workers_alive`] if a worker panics or exits.
struct WorkerAlive;

impl Drop for WorkerAlive {
    fn drop(&mut self) {
        InlineSimMetrics::sim_workers_alive().decrement(1.0);
    }
}

/// Sends the insert result to RPC. Insert already ran; a dropped receiver
/// (client gone) does not undo it.
fn notify_insert(
    inserted: oneshot::Sender<Result<AddedTransactionOutcome, PoolError>>,
    result: Result<AddedTransactionOutcome, PoolError>,
) {
    if let Err(ref error) = result {
        InlineSimMetrics::sim_insert_failures().increment(1);
        warn!(error = %error, hash = %error.hash, "inline sim pool insert failed");
    }
    let _ = inserted.send(result);
}

async fn meter_job<F>(job: InlineSimJob, meter: Arc<F>, timeout: Duration) -> InlineSimJob
where
    F: Fn(Recovered<BaseTxEnvelope>) -> Result<MeterBundleResponse, String> + Send + Sync + 'static,
{
    let recovered = job.transaction.clone_into_consensus();
    InlineSimMetrics::sim_workers_busy().increment(1.0);

    // `--inline-simulation-timeout-ms` only chooses real vs Default metering.
    // spawn_blocking cannot be cancelled. The worker joins so blocking tasks
    // stay bounded by worker count and do not pile up.
    let handle = tokio::task::spawn_blocking(move || meter(recovered));
    let job = match future::select(handle, Box::pin(tokio::time::sleep(timeout))).await {
        Either::Left((Ok(Ok(metering)), _)) => InlineSimJob {
            origin: job.origin,
            transaction: job.transaction.with_metering(metering),
            inserted: job.inserted,
        },
        Either::Left((Ok(Err(error)), _)) => {
            warn!(error = %error, hash = %job.transaction.hash(), "inline sim meter_bundle failed");
            InlineSimQueue::with_default_metering(job, "meter")
        }
        Either::Left((Err(join_error), _)) => {
            warn!(error = %join_error, hash = %job.transaction.hash(), "inline sim worker task failed");
            InlineSimQueue::with_default_metering(job, "join")
        }
        Either::Right((_, handle)) => {
            warn!(hash = %job.transaction.hash(), "inline sim meter_bundle timed out");
            let waited = Instant::now();
            let _ = handle.await;
            InlineSimMetrics::sim_timeout_wait_seconds().record(waited.elapsed().as_secs_f64());
            InlineSimQueue::with_default_metering(job, "timeout")
        }
    };

    InlineSimMetrics::sim_workers_busy().decrement(1.0);
    job
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use std::sync::Arc;

    use alloy_consensus::{SignableTransaction, TxEip1559, transaction::SignerRecoverable};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_bundles::{MeterBundleResponse, TransactionResult};
    use base_common_consensus::BaseTxEnvelope;
    use parking_lot::Mutex;
    use reth_transaction_pool::TransactionOrigin;
    use tokio::sync::mpsc;

    use super::*;

    static TEST_GUARD: Mutex<()> = Mutex::new(());

    fn sim_job() -> InlineSimJob {
        InlineSimJob::new(TransactionOrigin::Local, pooled_tx()).0
    }

    fn pooled_tx() -> BasePooledTransaction {
        let signer = PrivateKeySigner::random();
        let tx = TxEip1559 {
            chain_id: 8453,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(Address::repeat_byte(0xEE)),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered = envelope.clone().try_into_recovered().unwrap();
        BasePooledTransaction::new(recovered, envelope.encode_2718_len())
    }

    fn metering() -> MeterBundleResponse {
        MeterBundleResponse {
            results: vec![TransactionResult {
                coinbase_diff: U256::ZERO,
                eth_sent_to_coinbase: U256::ZERO,
                from_address: Address::ZERO,
                gas_fees: U256::ZERO,
                gas_price: U256::ZERO,
                gas_used: 21_000,
                to_address: None,
                tx_hash: Default::default(),
                value: U256::ZERO,
                execution_time_us: 1,
                opcode_gas: Vec::new(),
            }],
            total_gas_used: 21_000,
            ..MeterBundleResponse::default()
        }
    }

    #[test]
    fn try_enqueue_reports_disabled_without_install() {
        let _guard = TEST_GUARD.lock();
        InlineSimQueue::clear();

        let err = InlineSimQueue::try_enqueue(sim_job());
        assert!(matches!(err, Err(InlineSimEnqueueError::Disabled(_))));
    }

    #[test]
    fn try_enqueue_rejects_when_queue_is_full() {
        let _guard = TEST_GUARD.lock();
        InlineSimQueue::clear();
        let (tx, _rx) = mpsc::channel(1);
        InlineSimQueue::install(tx);

        let first = InlineSimQueue::try_enqueue(sim_job());
        assert!(first.is_ok(), "first enqueue must succeed");

        let second = InlineSimQueue::try_enqueue(sim_job());
        assert!(
            matches!(second, Err(InlineSimEnqueueError::Full(_))),
            "full queue must return TxPoolOverflow, not skip-sim insert"
        );
        InlineSimQueue::clear();
    }

    #[test]
    fn try_enqueue_disables_when_workers_drop_the_queue() {
        let _guard = TEST_GUARD.lock();
        InlineSimQueue::clear();
        let (tx, rx) = mpsc::channel(1);
        InlineSimQueue::install(tx);
        drop(rx);

        let err = InlineSimQueue::try_enqueue(sim_job());
        assert!(matches!(err, Err(InlineSimEnqueueError::Disabled(_))));
        assert!(
            !InlineSimQueue::is_enabled(),
            "a closed worker channel must uninstall the queue"
        );
        InlineSimQueue::clear();
    }

    #[tokio::test]
    async fn meter_job_attaches_metering_on_success() {
        let job = sim_job();
        let expected = metering();
        let expected_clone = expected.clone();

        let out = meter_job(
            job,
            Arc::new(move |_| Ok(expected_clone.clone())),
            Duration::from_secs(1),
        )
        .await;

        assert_eq!(out.transaction.metering(), Some(&expected));
    }

    #[tokio::test]
    async fn meter_job_uses_default_metering_on_meter_error() {
        let job = sim_job();

        let out =
            meter_job(job, Arc::new(|_| Err("boom".to_string())), Duration::from_secs(1)).await;

        assert_eq!(
            out.transaction.metering(),
            Some(&MeterBundleResponse::default()),
            "failed meter_bundle must still insert with default metering"
        );
    }

    #[tokio::test]
    async fn meter_job_uses_default_metering_on_timeout() {
        let job = sim_job();

        let out = meter_job(
            job,
            Arc::new(|_| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(metering())
            }),
            Duration::from_millis(10),
        )
        .await;

        assert_eq!(
            out.transaction.metering(),
            Some(&MeterBundleResponse::default()),
            "timed-out meter_bundle must still insert with default metering"
        );
    }

    #[tokio::test]
    async fn meter_job_uses_default_metering_when_blocking_task_panics() {
        let job = sim_job();

        let out = meter_job(job, Arc::new(|_| panic!("meter panic")), Duration::from_secs(1)).await;

        assert_eq!(
            out.transaction.metering(),
            Some(&MeterBundleResponse::default()),
            "a panicked spawn_blocking task must still insert with default metering"
        );
    }

    #[test]
    fn notify_insert_delivers_pool_error_to_rpc() {
        let (job, rx) = InlineSimJob::new(TransactionOrigin::Local, pooled_tx());
        let hash = *job.transaction.hash();
        let err = PoolError::new(hash, reth_transaction_pool::error::PoolErrorKind::AlreadyImported);

        notify_insert(job.inserted, Err(err));

        let got = rx.blocking_recv().expect("RPC waits for insert");
        assert!(got.is_err(), "insert failure must surface on the oneshot");
        assert_eq!(got.unwrap_err().hash, hash);
    }

    #[test]
    fn notify_insert_still_ok_if_rpc_dropped() {
        let (job, rx) = InlineSimJob::new(TransactionOrigin::Local, pooled_tx());
        drop(rx);
        let err = PoolError::new(
            *job.transaction.hash(),
            reth_transaction_pool::error::PoolErrorKind::AlreadyImported,
        );

        notify_insert(job.inserted, Err(err));
    }
}
