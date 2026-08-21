//! Pre-sim queue shared by `eth_sendRawTransaction` and mempool meter_bundle workers.
//!
//! RPC validates cheaply, then [`InlineSimQueue::try_enqueue`]. Workers pop, run
//! `meter_bundle`, and insert with [`crate::BasePooledTransaction::with_metering`].
//! The queue is installed at node start when `--enable-inline-simulation` is set.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::transaction::Recovered;
use base_bundles::MeterBundleResponse;
use base_common_consensus::BaseTxEnvelope;
use parking_lot::RwLock;
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::BasePooledTransaction;

base_metrics::define_metrics! {
    inline_simulation,
    struct = InlineSimMetrics,
    #[describe("Transactions waiting in the pre-sim queue")]
    sim_queue_size: gauge,
    #[describe("Sim workers currently running meter_bundle")]
    sim_workers_busy: gauge,
    #[describe("Wall time in seconds for one sim worker job (meter_bundle plus pool insert)")]
    sim_seconds: histogram,
    #[describe("Transactions dropped because the pre-sim queue was full")]
    sim_queue_full: counter,
    #[describe("meter_bundle failures that skipped pool insert")]
    #[label(name = "reason", default = ["timeout", "meter", "join"])]
    sim_failures: counter,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineSimEnqueueError {
    /// `--enable-inline-simulation` is off, or workers have not installed a queue.
    Disabled,
    /// Bounded pre-sim queue is full.
    Full,
}

static QUEUE: RwLock<Option<mpsc::Sender<InlineSimJob>>> = RwLock::new(None);
static QUEUE_LEN: AtomicUsize = AtomicUsize::new(0);
static WORKERS_BUSY: AtomicUsize = AtomicUsize::new(0);

/// Handle for the mempool pre-sim queue and worker pool.
#[derive(Debug)]
pub struct InlineSimQueue;

impl InlineSimQueue {
    /// Installs the enqueue sender used by `eth_sendRawTransaction`.
    pub fn install(sender: mpsc::Sender<InlineSimJob>) {
        QUEUE_LEN.store(0, Ordering::Relaxed);
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
            return Err(InlineSimEnqueueError::Disabled);
        };
        sender.try_send(job).map_err(|_| {
            InlineSimMetrics::sim_queue_full().increment(1);
            InlineSimEnqueueError::Full
        })?;
        let len = QUEUE_LEN.fetch_add(1, Ordering::Relaxed) + 1;
        InlineSimMetrics::sim_queue_size().set(len as f64);
        Ok(())
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
        let meter = Arc::new(meter);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        for _ in 0..workers {
            let meter = Arc::clone(&meter);
            let rx = Arc::clone(&rx);
            let pool = pool.clone();
            tokio::spawn(async move {
                loop {
                    let job = rx.lock().await.recv().await;
                    let Some(job) = job else {
                        break;
                    };
                    let len = QUEUE_LEN.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
                    InlineSimMetrics::sim_queue_size().set(len as f64);

                    let started = Instant::now();
                    if let Some(job) = meter_job(job, Arc::clone(&meter), timeout).await
                        && let Err(error) =
                            pool.add_transaction(job.origin, job.transaction).await
                    {
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
        *QUEUE.write() = None;
        QUEUE_LEN.store(0, Ordering::Relaxed);
        WORKERS_BUSY.store(0, Ordering::Relaxed);
        InlineSimMetrics::sim_queue_size().set(0.0);
        InlineSimMetrics::sim_workers_busy().set(0.0);
    }
}

async fn meter_job<F>(job: InlineSimJob, meter: Arc<F>, timeout: Duration) -> Option<InlineSimJob>
where
    F: Fn(Recovered<BaseTxEnvelope>) -> Result<MeterBundleResponse, String> + Send + Sync + 'static,
{
    let recovered = job.transaction.clone_into_consensus();
    let busy = WORKERS_BUSY.fetch_add(1, Ordering::Relaxed) + 1;
    InlineSimMetrics::sim_workers_busy().set(busy as f64);

    // ponytail: spawn_blocking cannot be cancelled; timeout only drops the wait
    let result = tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || meter(recovered)),
    )
    .await;

    let busy = WORKERS_BUSY.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
    InlineSimMetrics::sim_workers_busy().set(busy as f64);

    match result {
        Ok(Ok(Ok(metering))) => Some(InlineSimJob {
            origin: job.origin,
            transaction: job.transaction.with_metering(metering),
        }),
        Ok(Ok(Err(error))) => {
            InlineSimMetrics::sim_failures("meter").increment(1);
            warn!(error = %error, hash = %job.transaction.hash(), "inline sim meter_bundle failed");
            None
        }
        Ok(Err(_)) => {
            InlineSimMetrics::sim_failures("join").increment(1);
            warn!(hash = %job.transaction.hash(), "inline sim worker task failed");
            None
        }
        Err(_) => {
            InlineSimMetrics::sim_failures("timeout").increment(1);
            warn!(hash = %job.transaction.hash(), "inline sim meter_bundle timed out");
            None
        }
    }
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
        assert_eq!(err, Err(InlineSimEnqueueError::Disabled));
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
        assert_eq!(first, Ok(()));

        let second = InlineSimQueue::try_enqueue(InlineSimJob {
            origin: TransactionOrigin::Local,
            transaction: pooled_tx(),
        });
        assert_eq!(second, Err(InlineSimEnqueueError::Full));
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
        .await
        .expect("successful meter must insert");

        assert_eq!(out.transaction.metering(), Some(&expected));
    }

    #[tokio::test]
    async fn meter_job_skips_insert_on_meter_error() {
        let job = InlineSimJob { origin: TransactionOrigin::Local, transaction: pooled_tx() };

        let out =
            meter_job(job, Arc::new(|_| Err("boom".to_string())), Duration::from_secs(1)).await;

        assert!(out.is_none(), "failed meter_bundle must not produce an insert");
    }

    #[tokio::test]
    async fn meter_job_skips_insert_on_timeout() {
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

        assert!(out.is_none(), "timed-out meter_bundle must not produce an insert");
    }
}
