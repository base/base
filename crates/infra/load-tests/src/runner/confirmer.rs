use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use alloy_primitives::{Address, TxHash};
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::BlockFirstSeen;
use crate::{metrics::TransactionMetrics, rpc::{BatchRpcClient, ReceiptProvider}};

/// Shared map of transaction hashes to their flashblock inclusion times.
pub type FlashblockTimes = Arc<RwLock<HashMap<TxHash, Instant>>>;

/// Default channel buffer size for pending transactions.
/// Sized for ~2 seconds of throughput at 1000 TPS.
const PENDING_CHANNEL_BUFFER: usize = 2000;

/// Maximum number of receipt lookups per poll cycle.
const MAX_RECEIPT_LOOKUPS: usize = 1000;

/// Tracks pending transactions and collects confirmation metrics.
pub struct Confirmer {
    pending: HashMap<TxHash, PendingTx>,
    metrics_tx: mpsc::Sender<TransactionMetrics>,
    in_flight_per_sender: Arc<HashMap<Address, Arc<AtomicU64>>>,
    total_in_flight: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    poll_interval: Duration,
    max_pending_age: Duration,

    pending_rx: Option<mpsc::Receiver<PendingTx>>,
    pending_tx: mpsc::Sender<PendingTx>,
    flashblock_times: FlashblockTimes,
    block_first_seen: BlockFirstSeen,
    deferred_block_latencies: Vec<DeferredBlockLatency>,
    block_watcher_enabled: bool,
    batch_rpc: BatchRpcClient,
    expired_count: Arc<AtomicU64>,
}

/// A confirmed tx whose block latency could not be computed yet because
/// the block-first-seen timestamp had not arrived from the [`BlockWatcher`].
struct DeferredBlockLatency {
    metrics: TransactionMetrics,
    block_number: u64,
    submit_time: Instant,
    submit_timestamp: u64,
    deferred_at: Instant,
}

/// Max wait for a block to appear before sending metrics without block latency.
const BLOCK_LATENCY_DEFER_TIMEOUT: Duration = Duration::from_secs(1);

impl std::fmt::Debug for Confirmer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Confirmer")
            .field("pending_count", &self.pending.len())
            .field("poll_interval", &self.poll_interval)
            .field("max_pending_age", &self.max_pending_age)
            .finish_non_exhaustive()
    }
}

/// A pending transaction awaiting confirmation.
#[derive(Debug)]
struct PendingTx {
    tx_hash: TxHash,
    from: Address,
    submit_time: Instant,
    submit_timestamp: u64,
}

/// Handle for submitting transactions to the confirmer.
#[derive(Debug, Clone)]
pub struct ConfirmerHandle {
    pending_tx: mpsc::Sender<PendingTx>,
    in_flight_per_sender: Arc<HashMap<Address, Arc<AtomicU64>>>,
    total_in_flight: Arc<AtomicU64>,
    expired_count: Arc<AtomicU64>,
}

impl ConfirmerHandle {
    /// Records a submitted transaction for confirmation tracking.
    /// Returns false if the confirmer has shut down.
    pub async fn record_submitted(&self, tx_hash: TxHash, from: Address) -> bool {
        if let Some(counter) = self.in_flight_per_sender.get(&from) {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        self.total_in_flight.fetch_add(1, Ordering::SeqCst);

        let submit_timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let pending = PendingTx {
            tx_hash,
            from,
            submit_time: Instant::now(),
            submit_timestamp,
        };

        if self.pending_tx.send(pending).await.is_err() {
            if let Some(counter) = self.in_flight_per_sender.get(&from) {
                counter.fetch_sub(1, Ordering::SeqCst);
            }
            self.total_in_flight.fetch_sub(1, Ordering::SeqCst);
            return false;
        }

        true
    }

    /// Returns the in-flight count for a specific sender.
    pub fn in_flight_for(&self, address: &Address) -> u64 {
        self.in_flight_per_sender.get(address).map(|c| c.load(Ordering::SeqCst)).unwrap_or(0)
    }

    /// Returns the total in-flight count.
    pub fn total_in_flight(&self) -> u64 {
        self.total_in_flight.load(Ordering::SeqCst)
    }

    /// Returns the number of senders at or above the given in-flight limit.
    pub fn senders_at_limit(&self, limit: u64) -> usize {
        self.in_flight_per_sender.values().filter(|c| c.load(Ordering::SeqCst) >= limit).count()
    }

    /// Returns the number of transactions that expired without confirmation.
    pub fn expired_count(&self) -> u64 {
        self.expired_count.load(Ordering::SeqCst)
    }
}

impl Confirmer {
    /// Creates a confirmer with shared timing data.
    ///
    /// Set `block_watcher_enabled` to `true` when a `BlockWatcher` is running.
    /// When `false`, block latency is calculated from block timestamps fetched
    /// via RPC.
    pub fn new(
        sender_addresses: &[Address],
        metrics_tx: mpsc::Sender<TransactionMetrics>,
        stop_flag: Arc<AtomicBool>,
        flashblock_times: FlashblockTimes,
        block_first_seen: BlockFirstSeen,
        block_watcher_enabled: bool,
        batch_rpc: BatchRpcClient,
    ) -> Self {
        let mut in_flight_map = HashMap::new();
        for addr in sender_addresses {
            in_flight_map.insert(*addr, Arc::new(AtomicU64::new(0)));
        }

        let (pending_tx, pending_rx) = mpsc::channel(PENDING_CHANNEL_BUFFER);

        Self {
            pending: HashMap::new(),
            metrics_tx,
            in_flight_per_sender: Arc::new(in_flight_map),
            total_in_flight: Arc::new(AtomicU64::new(0)),
            stop_flag,
            poll_interval: Duration::from_millis(200),
            max_pending_age: Duration::from_secs(60),
            pending_rx: Some(pending_rx),
            pending_tx,
            flashblock_times,
            block_first_seen,
            deferred_block_latencies: Vec::new(),
            block_watcher_enabled,
            batch_rpc,
            expired_count: Arc::new(AtomicU64::new(0)),
        }
    }

    fn get_flashblocks_latency(&self, tx_hash: &TxHash, pending: &PendingTx) -> Option<Duration> {
        self.flashblock_times
            .read()
            .get(tx_hash)
            .and_then(|&fb_time| fb_time.checked_duration_since(pending.submit_time))
    }

    fn get_block_latency(
        &self,
        block_number: u64,
        pending_submit_time: Instant,
    ) -> Option<Duration> {
        self.block_first_seen
            .read()
            .get(&block_number)
            .and_then(|&block_time| block_time.checked_duration_since(pending_submit_time))
    }

    /// Creates a handle for submitting transactions.
    ///
    /// This method can be called multiple times; each handle shares the same
    /// underlying channel and in-flight counters. At least one handle must
    /// be created and passed to `run()` to prove the submission channel is
    /// in use.
    pub fn handle(&mut self) -> ConfirmerHandle {
        ConfirmerHandle {
            pending_tx: self.pending_tx.clone(),
            in_flight_per_sender: Arc::clone(&self.in_flight_per_sender),
            total_in_flight: Arc::clone(&self.total_in_flight),
            expired_count: Arc::clone(&self.expired_count),
        }
    }

    /// Runs the confirmation loop until stopped.
    ///
    /// Requires a `ConfirmerHandle` as proof that the submission channel is
    /// in use. The handle itself is not consumed, allowing continued
    /// submission during the run.
    pub async fn run(mut self, client: impl ReceiptProvider, _handle: &ConfirmerHandle) {
        let mut pending_rx =
            self.pending_rx.take().expect("run() called twice on the same Confirmer");

        loop {
            while let Ok(pending) = pending_rx.try_recv() {
                self.pending.insert(pending.tx_hash, pending);
            }

            let stopped = self.stop_flag.load(Ordering::SeqCst);
            if stopped && self.pending.is_empty() && self.deferred_block_latencies.is_empty() {
                while let Ok(pending) = pending_rx.try_recv() {
                    self.pending.insert(pending.tx_hash, pending);
                }
                if self.pending.is_empty() && self.deferred_block_latencies.is_empty() {
                    break;
                }
            }

            self.poll_confirmations(&client, stopped).await;

            if stopped {
                tokio::time::sleep(Duration::from_millis(100)).await;
            } else {
                tokio::time::sleep(self.poll_interval).await;
            }
        }

        debug!(confirmed = self.metrics_tx.is_closed(), "confirmer shutting down");
    }

    async fn poll_confirmations(&mut self, client: &impl ReceiptProvider, draining: bool) {
        self.resolve_deferred_block_latencies(client, draining).await;

        if self.pending.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut expired = Vec::new();

        for (tx_hash, pending) in &self.pending {
            if now.duration_since(pending.submit_time) > self.max_pending_age {
                expired.push(*tx_hash);
            }
        }

        let confirmed = self.poll_receipts().await;
        let confirmed_set: HashSet<TxHash> =
            confirmed.iter().map(|(hash, _)| *hash).collect();

        if !confirmed.is_empty() {
            let mut fb_times = self.flashblock_times.write();
            for (tx_hash, _) in &confirmed {
                fb_times.remove(tx_hash);
            }
        }

        for (tx_hash, from) in confirmed {
            self.pending.remove(&tx_hash);
            self.decrement_in_flight(&from);
        }

        for tx_hash in expired {
            if confirmed_set.contains(&tx_hash) {
                continue;
            }
            if let Some(pending) = self.pending.remove(&tx_hash) {
                debug!(tx_hash = %tx_hash, from = %pending.from, "transaction expired without confirmation");
                self.expired_count.fetch_add(1, Ordering::SeqCst);
                self.decrement_in_flight(&pending.from);
            }
        }
    }

    async fn poll_receipts(&mut self) -> Vec<(TxHash, Address)> {
        let to_lookup: Vec<TxHash> =
            self.pending.keys().copied().take(MAX_RECEIPT_LOOKUPS).collect();

        if to_lookup.is_empty() {
            return Vec::new();
        }

        let results = match tokio::time::timeout(
            Duration::from_secs(15),
            self.batch_rpc.batch_get_transaction_receipts(&to_lookup),
        )
        .await
        {
            Ok(Ok(results)) => results,
            Ok(Err(e)) => {
                warn!(error = %e, count = to_lookup.len(), "batch receipt fetch failed");
                return Vec::new();
            }
            Err(_) => {
                warn!(count = to_lookup.len(), "batch receipt fetch timed out");
                return Vec::new();
            }
        };

        let mut confirmed = Vec::new();

        for (tx_hash, receipt_opt) in to_lookup.into_iter().zip(results) {
            let Some(pending) = self.pending.get(&tx_hash) else {
                continue;
            };

            let Some(receipt) = receipt_opt else {
                continue;
            };

            let block_num = receipt.inner.block_number;
            let block_latency =
                block_num.and_then(|n| self.get_block_latency(n, pending.submit_time));
            let flashblocks_latency = self.get_flashblocks_latency(&tx_hash, pending);
            let mut metrics = TransactionMetrics::new(
                tx_hash,
                block_latency,
                flashblocks_latency,
                receipt.inner.gas_used,
                receipt.inner.effective_gas_price,
                block_num,
            );
            metrics.confirmed_at = Some(Instant::now());

            if let (None, Some(bn)) = (block_latency, block_num) {
                self.deferred_block_latencies.push(DeferredBlockLatency {
                    metrics,
                    block_number: bn,
                    submit_time: pending.submit_time,
                    submit_timestamp: pending.submit_timestamp,
                    deferred_at: Instant::now(),
                });
            } else {
                debug!(
                    tx_hash = %tx_hash,
                    block = ?block_num,
                    block_latency_ms = ?block_latency.map(|d| d.as_millis()),
                    "tx confirmed"
                );
                if self.metrics_tx.send(metrics).await.is_err() {
                    debug!(tx_hash = %tx_hash, "metrics channel closed");
                }
            }
            confirmed.push((tx_hash, pending.from));
        }

        confirmed
    }

    async fn resolve_deferred_block_latencies(
        &mut self,
        client: &impl ReceiptProvider,
        flush: bool,
    ) {
        if self.deferred_block_latencies.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut still_pending = Vec::new();
        let mut to_send = Vec::new();

        for mut deferred in self.deferred_block_latencies.drain(..) {
            if self.block_watcher_enabled {
                let block_latency = self
                    .block_first_seen
                    .read()
                    .get(&deferred.block_number)
                    .and_then(|&t| t.checked_duration_since(deferred.submit_time));

                if let Some(latency) = block_latency {
                    deferred.metrics.block_latency = Some(latency);
                    debug!(
                        tx_hash = %deferred.metrics.tx_hash,
                        block = deferred.block_number,
                        block_latency_ms = latency.as_millis(),
                        "deferred block latency resolved via websocket"
                    );
                    to_send.push(deferred.metrics);
                } else if flush
                    || now.duration_since(deferred.deferred_at) > BLOCK_LATENCY_DEFER_TIMEOUT
                {
                    debug!(
                        tx_hash = %deferred.metrics.tx_hash,
                        block = deferred.block_number,
                        "deferred block latency timed out"
                    );
                    to_send.push(deferred.metrics);
                } else {
                    still_pending.push(deferred);
                }
            } else {
                match client.get_block_timestamp(deferred.block_number).await {
                    Ok(Some(block_ts)) if block_ts >= deferred.submit_timestamp => {
                        let latency = Duration::from_secs(block_ts - deferred.submit_timestamp);
                        deferred.metrics.block_latency = Some(latency);
                        debug!(
                            tx_hash = %deferred.metrics.tx_hash,
                            block = deferred.block_number,
                            block_latency_ms = latency.as_millis(),
                            "block latency resolved via block timestamp"
                        );
                        to_send.push(deferred.metrics);
                    }
                    Ok(Some(block_ts)) => {
                        debug!(
                            tx_hash = %deferred.metrics.tx_hash,
                            block = deferred.block_number,
                            block_ts,
                            submit_ts = deferred.submit_timestamp,
                            "block timestamp before submit time (clock skew)"
                        );
                        to_send.push(deferred.metrics);
                    }
                    Ok(None)
                        if flush
                            || now.duration_since(deferred.deferred_at)
                                > BLOCK_LATENCY_DEFER_TIMEOUT =>
                    {
                        to_send.push(deferred.metrics);
                    }
                    Ok(None) => {
                        still_pending.push(deferred);
                    }
                    Err(e) => {
                        warn!(
                            tx_hash = %deferred.metrics.tx_hash,
                            block = deferred.block_number,
                            error = %e,
                            "failed to fetch block timestamp"
                        );
                        if flush
                            || now.duration_since(deferred.deferred_at)
                                > BLOCK_LATENCY_DEFER_TIMEOUT
                        {
                            to_send.push(deferred.metrics);
                        } else {
                            still_pending.push(deferred);
                        }
                    }
                }
            }
        }

        self.deferred_block_latencies = still_pending;

        for metrics in to_send {
            if self.metrics_tx.send(metrics).await.is_err() {
                debug!("metrics channel closed during deferred resolution");
                break;
            }
        }
    }

    fn decrement_in_flight(&self, from: &Address) {
        if let Some(counter) = self.in_flight_per_sender.get(from) {
            let _ = counter.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1));
        }
        let _ = self
            .total_in_flight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1));
    }
}
