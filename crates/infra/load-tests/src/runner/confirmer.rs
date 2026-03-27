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
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::BlockFirstSeen;
use crate::{metrics::TransactionMetrics, rpc::ReceiptProvider};

/// Shared map of transaction hashes to their flashblock inclusion times.
pub type FlashblockTimes = Arc<RwLock<HashMap<TxHash, Instant>>>;

/// Default channel buffer size for pending transactions.
/// Sized for ~2 seconds of throughput at 1000 TPS.
const PENDING_CHANNEL_BUFFER: usize = 2000;

/// Maximum number of concurrent receipt lookups per poll cycle.
const MAX_RECEIPT_LOOKUPS: usize = 50;

/// Tracks pending transactions and collects confirmation metrics.
pub struct Confirmer {
    pending: HashMap<TxHash, PendingTx>,
    metrics_tx: mpsc::Sender<TransactionMetrics>,
    in_flight_per_sender: Arc<HashMap<Address, Arc<AtomicU64>>>,
    total_in_flight: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    poll_interval: Duration,
    max_pending_age: Duration,
    straggler_age: Duration,
    pending_rx: Option<mpsc::Receiver<PendingTx>>,
    pending_tx: mpsc::Sender<PendingTx>,
    flashblock_times: FlashblockTimes,
    block_first_seen: BlockFirstSeen,
}

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
    /// Set when the tx is first seen in a pending/latest block.
    /// Used as the definitive latency endpoint.
    included_at: Option<Instant>,
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

        let pending = PendingTx { tx_hash, from, submit_time: Instant::now(), included_at: None };

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
        Self::with_timing_data(
            sender_addresses,
            metrics_tx,
            stop_flag,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
        )
    }

    /// Creates a new confirmer with shared flashblock and block timing data.
    pub fn with_timing_data(
        sender_addresses: &[Address],
        metrics_tx: mpsc::Sender<TransactionMetrics>,
        stop_flag: Arc<AtomicBool>,
        flashblock_times: FlashblockTimes,
        block_first_seen: BlockFirstSeen,
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
            poll_interval: Duration::from_millis(100),
            max_pending_age: Duration::from_secs(60),
            straggler_age: Duration::from_secs(10),
            pending_rx: Some(pending_rx),
            pending_tx,
            flashblock_times,
            block_first_seen,
        }
    }

    fn get_fb_sequencer_latency(&self, tx_hash: &TxHash, pending: &PendingTx) -> Option<Duration> {
        self.flashblock_times
            .read()
            .get(tx_hash)
            .map(|fb_time| fb_time.saturating_duration_since(pending.submit_time))
    }

    fn get_block_latency(&self, block_number: u64, pending: &PendingTx) -> Option<Duration> {
        self.block_first_seen
            .read()
            .get(&block_number)
            .map(|block_time| block_time.saturating_duration_since(pending.submit_time))
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
        if self.pending.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut confirmed = Vec::new();
        let mut expired = Vec::new();

        for (tx_hash, pending) in &self.pending {
            if now.duration_since(pending.submit_time) > self.max_pending_age {
                expired.push(*tx_hash);
            }
        }

        // Phase 1: Check pending block for tx inclusion.
        // Records the inclusion timestamp but doesn't yet have gas metrics.
        self.check_pending_block(client).await;

        // Phase 2: Fetch receipts for txns that have been included (have included_at set)
        // or are old enough to be stragglers.
        self.fetch_receipts(client, &mut confirmed).await;

        let confirmed_hashes: HashSet<TxHash> = confirmed.iter().map(|(hash, _)| *hash).collect();

        // Prune confirmed transactions from shared timing maps to prevent unbounded growth.
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
            if confirmed_hashes.contains(&tx_hash) {
                continue;
            }
            if let Some(pending) = self.pending.remove(&tx_hash) {
                warn!(tx_hash = %tx_hash, from = %pending.from, "transaction expired without confirmation");
                self.decrement_in_flight(&pending.from);
            }
        }
    }

    /// Polls `eth_getBlock("pending")` and marks matching transactions with an inclusion timestamp.
    async fn check_pending_block(&mut self, client: &impl ReceiptProvider) {
        let tx_hashes = match client.get_pending_block_tx_hashes().await {
            Ok(hashes) => hashes,
            Err(e) => {
                warn!(error = %e, "failed to get pending block");
                return;
            }
        };

        let now = Instant::now();
        for tx_hash in &tx_hashes {
            if let Some(pending) = self.pending.get_mut(tx_hash)
                && pending.included_at.is_none()
            {
                pending.included_at = Some(now);
                debug!(tx_hash = %tx_hash, "tx detected in pending block");
            }
        }
    }

    /// Fetches individual receipts for transactions that have been included in a block
    /// or have been pending long enough to be stragglers.
    async fn fetch_receipts(
        &self,
        client: &impl ReceiptProvider,
        confirmed: &mut Vec<(TxHash, Address)>,
    ) {
        let now = Instant::now();

        let to_lookup: Vec<TxHash> = self
            .pending
            .iter()
            .filter(|(_, pending)| {
                pending.included_at.is_some()
                    || now.duration_since(pending.submit_time) > self.straggler_age
            })
            .take(MAX_RECEIPT_LOOKUPS)
            .map(|(hash, _)| *hash)
            .collect();

        if to_lookup.is_empty() {
            return;
        }

        let futures = to_lookup.iter().map(|tx_hash| client.get_transaction_receipt(*tx_hash));
        let results = join_all(futures).await;

        for (tx_hash, result) in to_lookup.into_iter().zip(results) {
            let Some(pending) = self.pending.get(&tx_hash) else {
                continue;
            };

            match result {
                Ok(Some(receipt)) => {
                    let block_num = receipt.inner.block_number.unwrap_or(0);
                    let block_latency = self.get_block_latency(block_num, pending);
                    let fb_sequencer_latency = self.get_fb_sequencer_latency(&tx_hash, pending);
                    let metrics = TransactionMetrics::new(
                        tx_hash,
                        block_latency,
                        fb_sequencer_latency,
                        receipt.inner.gas_used,
                        receipt.inner.effective_gas_price,
                        block_num,
                    );
                    debug!(
                        tx_hash = %tx_hash,
                        block_latency_ms = block_latency.map(|d| d.as_millis()),
                        "tx confirmed"
                    );
                    let _ = self.metrics_tx.send(metrics).await;
                    confirmed.push((tx_hash, pending.from));
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "receipt lookup failed");
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
