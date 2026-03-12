use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_primitives::{Address, TxHash};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::{metrics::TransactionMetrics, rpc::RpcClient};

/// Tracks pending transactions and collects confirmation metrics.
#[derive(Debug)]
pub struct Confirmer {
    rpc_url: url::Url,
    pending: HashMap<TxHash, PendingTx>,
    metrics_tx: mpsc::UnboundedSender<TransactionMetrics>,
    in_flight_per_sender: HashMap<Address, Arc<AtomicU64>>,
    total_in_flight: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    poll_interval: Duration,
    max_pending_age: Duration,
}

/// A pending transaction awaiting confirmation.
#[derive(Debug)]
pub struct PendingTx {
    tx_hash: TxHash,
    from: Address,
    submit_time: Instant,
}

/// Handle for submitting transactions to the confirmer.
#[derive(Debug, Clone)]
pub struct ConfirmerHandle {
    pending_tx: mpsc::UnboundedSender<PendingTx>,
    in_flight_per_sender: Arc<HashMap<Address, Arc<AtomicU64>>>,
    total_in_flight: Arc<AtomicU64>,
}

impl ConfirmerHandle {
    /// Records a submitted transaction for confirmation tracking.
    /// Returns false if the confirmer has shut down.
    pub fn record_submitted(&self, tx_hash: TxHash, from: Address) -> bool {
        let pending = PendingTx { tx_hash, from, submit_time: Instant::now() };

        if self.pending_tx.send(pending).is_err() {
            return false;
        }

        if let Some(counter) = self.in_flight_per_sender.get(&from) {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        self.total_in_flight.fetch_add(1, Ordering::SeqCst);

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
        rpc_url: url::Url,
        sender_addresses: &[Address],
        metrics_tx: mpsc::UnboundedSender<TransactionMetrics>,
        stop_flag: Arc<AtomicBool>,
    ) -> Self {
        let mut in_flight_per_sender = HashMap::new();
        for addr in sender_addresses {
            in_flight_per_sender.insert(*addr, Arc::new(AtomicU64::new(0)));
        }

        Self {
            rpc_url,
            pending: HashMap::new(),
            metrics_tx,
            in_flight_per_sender,
            total_in_flight: Arc::new(AtomicU64::new(0)),
            stop_flag,
            poll_interval: Duration::from_millis(500),
            max_pending_age: Duration::from_secs(60),
        }
    }

    /// Creates a handle for submitting transactions.
    pub fn handle(&self) -> (ConfirmerHandle, mpsc::UnboundedReceiver<PendingTx>) {
        let (pending_tx, pending_rx) = mpsc::unbounded_channel();
        let handle = ConfirmerHandle {
            pending_tx,
            in_flight_per_sender: Arc::new(self.in_flight_per_sender.clone()),
            total_in_flight: Arc::clone(&self.total_in_flight),
        };
        (handle, pending_rx)
    }

    /// Runs the confirmation loop until stopped.
    pub async fn run(mut self, mut pending_rx: mpsc::UnboundedReceiver<PendingTx>) {
        let client = RpcClient::new(self.rpc_url.clone());

        loop {
            if self.stop_flag.load(Ordering::SeqCst) && self.pending.is_empty() {
                break;
            }

            while let Ok(pending) = pending_rx.try_recv() {
                self.pending.insert(pending.tx_hash, pending);
            }

            self.poll_confirmations(&client).await;

            if !self.stop_flag.load(Ordering::SeqCst) {
                tokio::time::sleep(self.poll_interval).await;
            }
        }

        debug!(confirmed = self.metrics_tx.is_closed(), "confirmer shutting down");
    }

    async fn poll_confirmations(&mut self, client: &RpcClient) {
        let now = Instant::now();
        let mut confirmed = Vec::new();
        let mut expired = Vec::new();

        for (tx_hash, pending) in &self.pending {
            if now.duration_since(pending.submit_time) > self.max_pending_age {
                expired.push(*tx_hash);
                continue;
            }

            match client.get_transaction_receipt(*tx_hash).await {
                Ok(Some(receipt)) => {
                    let latency = pending.submit_time.elapsed();
                    let metrics = TransactionMetrics::new(
                        *tx_hash,
                        latency,
                        receipt.gas_used,
                        receipt.effective_gas_price,
                        receipt.block_number.unwrap_or(0),
                    );
                    debug!(tx_hash = %tx_hash, latency_ms = latency.as_millis(), "confirmed");
                    let _ = self.metrics_tx.send(metrics);
                    confirmed.push((*tx_hash, pending.from));
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "failed to get receipt");
                }
            }
        }

        for (tx_hash, from) in confirmed {
            self.pending.remove(&tx_hash);
            self.decrement_in_flight(&from);
        }

        for tx_hash in expired {
            if let Some(pending) = self.pending.remove(&tx_hash) {
                warn!(tx_hash = %tx_hash, from = %pending.from, "transaction expired without confirmation");
                self.decrement_in_flight(&pending.from);
            }
        }
    }

    fn decrement_in_flight(&self, from: &Address) {
        if let Some(counter) = self.in_flight_per_sender.get(from) {
            counter.fetch_sub(1, Ordering::SeqCst);
        }
        self.total_in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}
