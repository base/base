//! Pre-sim queue shared by `eth_sendRawTransaction` and mempool meter_bundle workers.
//!
//! RPC validates cheaply, then [`InlineSimQueue::try_enqueue`]. Workers pop, run
//! `meter_bundle`, and insert with [`crate::BasePooledTransaction::with_metering`].
//! The queue is installed in the node-started hook together with the workers.
//! Queue-full inserts [`MeterBundleResponse::default`] instead of rejecting.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::transaction::Recovered;
use base_bundles::MeterBundleResponse;
use base_common_consensus::BaseTxEnvelope;
use futures::future::{self, Either};
use parking_lot::RwLock;
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::{debug, warn};

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
    #[describe("Queue-full events that inserted MeterBundleResponse::default instead of simulating")]
    sim_queue_full: counter,
    #[describe("meter_bundle failures that inserted MeterBundleResponse::default")]
    #[label(name = "reason", default = ["timeout", "meter", "join", "queue_full"])]
    sim_failures: counter,
    #[describe("Pool inserts that carried MeterBundleResponse::default after a failed or timed-out sim")]
    sim_default_inserts: counter,
    #[describe("Seconds waiting for meter_bundle to finish after the configured timeout fired")]
    sim_timeout_wait_seconds: histogram,
    #[describe("Enqueued jobs whose pool insert failed after RPC already returned the hash")]
    sim_insert_failures: counter,
}

/// Job waiting for an in-process meter_bundle worker.
#[derive(Debug)]
pub struct InlineSimJob {
    /// Origin used when the worker later calls `add_transaction`.
    pub origin: TransactionOrigin,
    /// Transaction that already passed cheap pool validation.
    pub transaction: BasePooledTransaction,
}

/// Why [`InlineSimQueue::try_enqueue`] rejected a job.
#[derive(Debug)]
pub enum InlineSimEnqueueError {
    /// `--enable-inline-simulation` is off, or workers have not installed a queue.
    Disabled(InlineSimJob),
    /// Bounded pre-sim queue is full. Caller inserts with default metering.
    Full(InlineSimJob),
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
                Err(InlineSimEnqueueError::Full(job))
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
                    if let Err(error) = pool.add_transaction(job.origin, job.transaction).await
                    {
                        InlineSimMetrics::sim_insert_failures().increment(1);
                        debug!(error = %error, "inline sim pool insert failed");
                    }
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

    /// Attaches [`MeterBundleResponse::default`] after a failed, timed-out, or
    /// queue-full sim so the Consumer can still forward.
    pub fn with_default_metering(job: InlineSimJob, reason: &'static str) -> InlineSimJob {
        InlineSimMetrics::sim_failures(reason).increment(1);
        InlineSimMetrics::sim_default_inserts().increment(1);
        InlineSimJob {
            origin: job.origin,
            transaction: job.transaction.with_metering(MeterBundleResponse::default()),
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

        let err = InlineSimQueue::try_enqueue(InlineSimJob {
            origin: TransactionOrigin::Local,
            transaction: pooled_tx(),
        });
        assert!(matches!(err, Err(InlineSimEnqueueError::Disabled(_))));
    }

    #[test]
    fn try_enqueue_rejects_when_queue_is_full() {
        let _guard = TEST_GUARD.lock();
        InlineSimQueue::clear();
        let (tx, _rx) = mpsc::channel(1);
        InlineSimQueue::install(tx);

        let first = InlineSimQueue::try_enqueue(InlineSimJob {
            origin: TransactionOrigin::Local,
            transaction: pooled_tx(),
        });
        assert!(first.is_ok(), "first enqueue must succeed");

        let second = InlineSimQueue::try_enqueue(InlineSimJob {
            origin: TransactionOrigin::Local,
            transaction: pooled_tx(),
        });
        let InlineSimEnqueueError::Full(job) = second.expect_err("queue is full") else {
            panic!("full queue must return the job so RPC can insert default metering");
        };
        let job = InlineSimQueue::with_default_metering(job, "queue_full");
        assert_eq!(
            job.transaction.metering(),
            Some(&MeterBundleResponse::default()),
            "queue-full fallback attaches default metering"
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

        let err = InlineSimQueue::try_enqueue(InlineSimJob {
            origin: TransactionOrigin::Local,
            transaction: pooled_tx(),
        });
        assert!(matches!(err, Err(InlineSimEnqueueError::Disabled(_))));
        assert!(
            !InlineSimQueue::is_enabled(),
            "a closed worker channel must uninstall the queue"
        );
        InlineSimQueue::clear();
    }

    #[tokio::test]
    async fn meter_job_attaches_metering_on_success() {
        let job = InlineSimJob { origin: TransactionOrigin::Local, transaction: pooled_tx() };
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
        let job = InlineSimJob { origin: TransactionOrigin::Local, transaction: pooled_tx() };

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
        let job = InlineSimJob { origin: TransactionOrigin::Local, transaction: pooled_tx() };

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
        let job = InlineSimJob { origin: TransactionOrigin::Local, transaction: pooled_tx() };

        let out = meter_job(job, Arc::new(|_| panic!("meter panic")), Duration::from_secs(1)).await;

        assert_eq!(
            out.transaction.metering(),
            Some(&MeterBundleResponse::default()),
            "a panicked spawn_blocking task must still insert with default metering"
        );
    }
}
