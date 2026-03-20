use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_primitives::{Address, TxHash};
use futures::future::join_all;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::{metrics::TransactionMetrics, rpc::ReceiptProvider};

/// Default channel buffer size for pending transactions.
/// Sized for ~2 seconds of throughput at 1000 TPS.
const PENDING_CHANNEL_BUFFER: usize = 2000;

/// Initial block lookback window on first poll.
/// Catches transactions confirmed between submission and confirmer startup.
const INITIAL_BLOCK_LOOKBACK: u64 = 5;

/// Tracks pending transactions and collects confirmation metrics.
#[derive(Debug)]
pub struct Confirmer {
    pending: HashMap<TxHash, PendingTx>,
    metrics_tx: mpsc::Sender<TransactionMetrics>,
    in_flight_per_sender: Arc<HashMap<Address, Arc<AtomicU64>>>,
    total_in_flight: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    poll_interval: Duration,
    max_pending_age: Duration,
    last_checked_block: u64,
    max_straggler_lookups: usize,
    pending_rx: Option<mpsc::Receiver<PendingTx>>,
    pending_tx: mpsc::Sender<PendingTx>,
}

/// A pending transaction awaiting confirmation.
#[derive(Debug)]
struct PendingTx {
    tx_hash: TxHash,
    from: Address,
    submit_time: Instant,
}

/// Handle for submitting transactions to the confirmer.
#[derive(Debug, Clone)]
pub struct ConfirmerHandle {
    pending_tx: mpsc::Sender<PendingTx>,
    in_flight_per_sender: Arc<HashMap<Address, Arc<AtomicU64>>>,
    total_in_flight: Arc<AtomicU64>,
}

impl ConfirmerHandle {
    /// Records a submitted transaction for confirmation tracking.
    /// Returns false if the confirmer has shut down.
    pub async fn record_submitted(&self, tx_hash: TxHash, from: Address) -> bool {
        if let Some(counter) = self.in_flight_per_sender.get(&from) {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        self.total_in_flight.fetch_add(1, Ordering::SeqCst);

        let pending = PendingTx { tx_hash, from, submit_time: Instant::now() };

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
}

impl Confirmer {
    /// Creates a new confirmer.
    pub fn new(
        sender_addresses: &[Address],
        metrics_tx: mpsc::Sender<TransactionMetrics>,
        stop_flag: Arc<AtomicBool>,
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
            poll_interval: Duration::from_millis(500),
            max_pending_age: Duration::from_secs(60),
            last_checked_block: 0,
            max_straggler_lookups: 10,
            pending_rx: Some(pending_rx),
            pending_tx,
        }
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
            if stopped && self.pending.is_empty() {
                while let Ok(pending) = pending_rx.try_recv() {
                    self.pending.insert(pending.tx_hash, pending);
                }
                if self.pending.is_empty() {
                    break;
                }
            }

            self.poll_confirmations(&client).await;

            if stopped {
                tokio::time::sleep(Duration::from_millis(100)).await;
            } else {
                tokio::time::sleep(self.poll_interval).await;
            }
        }

        debug!(confirmed = self.metrics_tx.is_closed(), "confirmer shutting down");
    }

    async fn poll_confirmations(&mut self, client: &impl ReceiptProvider) {
        let now = Instant::now();
        let mut confirmed = Vec::new();
        let mut expired = Vec::new();

        for (tx_hash, pending) in &self.pending {
            if now.duration_since(pending.submit_time) > self.max_pending_age {
                expired.push(*tx_hash);
            }
        }

        let current_block = match client.get_block_number().await {
            Ok(block) => block,
            Err(e) => {
                warn!(error = %e, "failed to get current block number");
                return;
            }
        };

        let start_block = if self.last_checked_block == 0 {
            current_block.saturating_sub(INITIAL_BLOCK_LOOKBACK)
        } else {
            self.last_checked_block + 1
        };

        let mut found_in_blocks = HashSet::new();

        for block_num in start_block..=current_block {
            match client.get_block_receipts(block_num).await {
                Ok(Some(receipts)) => {
                    trace!(
                        block = block_num,
                        receipt_count = receipts.len(),
                        "fetched block receipts"
                    );
                    for receipt in receipts {
                        let tx_hash = receipt.inner.transaction_hash;
                        if let Some(pending) = self.pending.get(&tx_hash) {
                            found_in_blocks.insert(tx_hash);
                            let latency = pending.submit_time.elapsed();
                            let metrics = TransactionMetrics::new(
                                tx_hash,
                                latency,
                                receipt.inner.gas_used,
                                receipt.inner.effective_gas_price,
                                receipt.inner.block_number.unwrap_or(block_num),
                            );
                            debug!(
                                tx_hash = %tx_hash,
                                latency_ms = latency.as_millis(),
                                block = block_num,
                                "confirmed via block receipts"
                            );
                            let _ = self.metrics_tx.send(metrics).await;
                            confirmed.push((tx_hash, pending.from));
                        }
                    }
                }
                Ok(None) => {
                    trace!(block = block_num, "no receipts for block");
                }
                Err(e) => {
                    warn!(block = block_num, error = %e, "failed to get block receipts");
                }
            }
        }

        self.last_checked_block = current_block;

        self.lookup_stragglers(client, &found_in_blocks, &mut confirmed).await;

        let confirmed_hashes: HashSet<TxHash> = confirmed.iter().map(|(hash, _)| *hash).collect();

        for (tx_hash, from) in confirmed {
            self.pending.remove(&tx_hash);
            self.decrement_in_flight(&from);
        }

        for tx_hash in expired {
            // Skip if already processed as confirmed to avoid double-decrement
            if confirmed_hashes.contains(&tx_hash) {
                continue;
            }
            if let Some(pending) = self.pending.remove(&tx_hash) {
                warn!(tx_hash = %tx_hash, from = %pending.from, "transaction expired without confirmation");
                self.decrement_in_flight(&pending.from);
            }
        }
    }

    /// Looks up individual receipts for old pending transactions not found in recent blocks.
    ///
    /// Block receipt fetching may miss transactions confirmed in blocks before the lookback
    /// window. This fallback queries receipts directly for transactions pending longer than
    /// 10 seconds, limited to `max_straggler_lookups` per poll to bound RPC load.
    async fn lookup_stragglers(
        &self,
        client: &impl ReceiptProvider,
        already_found: &HashSet<TxHash>,
        confirmed: &mut Vec<(TxHash, Address)>,
    ) {
        let now = Instant::now();
        let old_pending: Vec<_> = self
            .pending
            .iter()
            .filter(|(hash, pending)| {
                !already_found.contains(*hash)
                    && now.duration_since(pending.submit_time) > Duration::from_secs(10)
            })
            .take(self.max_straggler_lookups)
            .map(|(hash, pending)| (*hash, pending.from, pending.submit_time))
            .collect();

        let futures =
            old_pending.iter().map(|(tx_hash, _, _)| client.get_transaction_receipt(*tx_hash));
        let results = join_all(futures).await;

        for ((tx_hash, from, submit_time), result) in old_pending.into_iter().zip(results) {
            match result {
                Ok(Some(receipt)) => {
                    let latency = submit_time.elapsed();
                    let metrics = TransactionMetrics::new(
                        tx_hash,
                        latency,
                        receipt.inner.gas_used,
                        receipt.inner.effective_gas_price,
                        receipt.inner.block_number.unwrap_or(0),
                    );
                    debug!(
                        tx_hash = %tx_hash,
                        latency_ms = latency.as_millis(),
                        "confirmed via straggler lookup"
                    );
                    let _ = self.metrics_tx.send(metrics).await;
                    confirmed.push((tx_hash, from));
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "straggler lookup failed");
                }
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
