use std::{collections::VecDeque, sync::Arc, time::Instant};

use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes};
use jsonrpsee::{
    core::{ClientError, client::ClientT},
    http_client::HttpClient,
};
use reth_transaction_pool::{PoolTransaction, ValidPoolTransaction};
use serde::{Deserialize, Serialize};
use tokio::{sync::broadcast, time};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use super::{config::ForwarderConfig, metrics::ForwarderMetrics};

/// Pre-validated transaction for the builder RPC wire format.
///
/// Carries the recovered sender address so the builder can skip signer
/// recovery, and the EIP-2718 encoded transaction envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidTransaction {
    /// Recovered signer address.
    pub sender: Address,
    /// EIP-2718 encoded transaction bytes.
    pub raw: Bytes,
}

/// Sliding window rate limiter that tracks request timestamps.
///
/// Maintains a bounded deque of send timestamps within a 1-second window.
/// When the window is full (at `max_rps`), reports how long until the
/// oldest entry expires so the caller can sleep precisely.
struct RateLimiter {
    timestamps: VecDeque<Instant>,
    max_rps: u32,
}

impl RateLimiter {
    fn new(max_rps: u32) -> Self {
        assert!(max_rps > 0, "max_rps must be at least 1");
        Self { timestamps: VecDeque::with_capacity(max_rps as usize), max_rps }
    }

    fn prune(&mut self, now: Instant) {
        let window = std::time::Duration::from_secs(1);
        while let Some(&front) = self.timestamps.front() {
            if now.duration_since(front) >= window {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
    }

    /// Returns `None` if a send is allowed now, or `Some(wait)` with the
    /// precise duration until the next slot opens.
    fn check_rate_limit(&mut self) -> Option<std::time::Duration> {
        let now = Instant::now();
        self.prune(now);

        if (self.timestamps.len() as u32) < self.max_rps {
            return None;
        }

        let oldest = self.timestamps.front().expect("non-empty after prune");
        let window = std::time::Duration::from_secs(1);
        let elapsed = now.duration_since(*oldest);
        Some(window.saturating_sub(elapsed))
    }

    fn record_send(&mut self) {
        self.timestamps.push_back(Instant::now());
    }
}

/// Async forwarder task that receives transactions from a broadcast channel
/// and sends them to a single builder via RPC.
///
/// Under normal load, each transaction is sent immediately as a batch of 1.
/// When the sliding window rate limit (`max_rps`) is hit, incoming
/// transactions buffer and flush as a single batch (capped at
/// `max_batch_size`) once the window opens.
pub struct Forwarder<T: PoolTransaction> {
    builder_url: String,
    client: HttpClient,
    receiver: broadcast::Receiver<Arc<ValidPoolTransaction<T>>>,
    config: ForwarderConfig,
    metrics: ForwarderMetrics,
    cancel: CancellationToken,
    limiter: RateLimiter,
    buffer: Vec<ValidTransaction>,
}

impl<T> Forwarder<T>
where
    T: PoolTransaction,
    <T as PoolTransaction>::Consensus: Encodable2718,
{
    /// Creates a new forwarder for a single builder endpoint.
    pub fn new(
        builder_url: String,
        client: HttpClient,
        receiver: broadcast::Receiver<Arc<ValidPoolTransaction<T>>>,
        config: ForwarderConfig,
        metrics: ForwarderMetrics,
        cancel: CancellationToken,
    ) -> Self {
        let limiter = RateLimiter::new(config.max_rps);
        let buffer = Vec::with_capacity(config.max_batch_size);
        Self { builder_url, client, receiver, config, metrics, cancel, limiter, buffer }
    }

    /// Runs the forwarder loop until cancelled.
    pub async fn run(mut self) {
        info!(
            builder_url = %self.builder_url,
            max_rps = self.config.max_rps,
            max_batch_size = self.config.max_batch_size,
            "starting transaction forwarder",
        );

        loop {
            if self.cancel.is_cancelled() {
                return;
            }

            match self.limiter.check_rate_limit() {
                None if !self.buffer.is_empty() => {
                    self.flush_buffer().await;
                    continue;
                }
                Some(wait) => {
                    tokio::select! {
                        _ = self.cancel.cancelled() => return,
                        _ = time::sleep(wait) => continue,
                        result = self.receiver.recv() => {
                            self.handle_recv(result);
                        }
                    }
                    continue;
                }
                _ => {}
            }

            tokio::select! {
                _ = self.cancel.cancelled() => return,
                result = self.receiver.recv() => {
                    self.handle_recv(result);
                    if !self.buffer.is_empty() && self.limiter.check_rate_limit().is_none() {
                        self.flush_buffer().await;
                    }
                }
            }
        }
    }

    fn handle_recv(
        &mut self,
        result: Result<Arc<ValidPoolTransaction<T>>, broadcast::error::RecvError>,
    ) {
        match result {
            Ok(tx) => {
                let sender = *tx.sender_ref();
                let consensus = tx.transaction.clone_into_consensus();
                let raw = Bytes::from(consensus.inner().encoded_2718());
                self.buffer.push(ValidTransaction { sender, raw });
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    builder_url = %self.builder_url,
                    skipped = skipped,
                    "forwarder lagged, dropped transactions",
                );
                self.metrics.batches_lagged.increment(1);
                self.metrics.txs_lagged.increment(skipped);
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!(
                    builder_url = %self.builder_url,
                    "broadcast channel closed, shutting down forwarder",
                );
                self.cancel.cancel();
            }
        }
    }

    async fn flush_buffer(&mut self) {
        let batch_size = self.buffer.len().min(self.config.max_batch_size);
        let batch: Vec<ValidTransaction> = self.buffer.drain(..batch_size).collect();

        if batch.is_empty() {
            return;
        }

        trace!(
            builder_url = %self.builder_url,
            txs = batch.len(),
            remaining = self.buffer.len(),
            "flushing batch",
        );

        self.send_with_retries(batch).await;
        self.limiter.record_send();
    }

    async fn send_with_retries(&self, batch: Vec<ValidTransaction>) {
        let tx_count = batch.len() as u64;
        let overall_start = Instant::now();

        for attempt in 0..=self.config.max_retries {
            let result: Result<serde_json::Value, ClientError> =
                self.client.request("base_insertValidatedTransactions", vec![&batch]).await;

            match result {
                Ok(_) => {
                    self.metrics.rpc_latency.record(overall_start.elapsed().as_secs_f64());
                    self.metrics.batches_sent.increment(1);
                    self.metrics.txs_forwarded.increment(tx_count);
                    return;
                }
                Err(err) if Self::is_retryable(&err) && attempt < self.config.max_retries => {
                    let backoff = self.config.retry_backoff * 2u32.saturating_pow(attempt);
                    debug!(
                        builder_url = %self.builder_url,
                        attempt = attempt + 1,
                        max_retries = self.config.max_retries,
                        backoff_ms = backoff.as_millis() as u64,
                        error = %err,
                        "RPC send failed, retrying",
                    );
                    tokio::select! {
                        _ = self.cancel.cancelled() => return,
                        _ = time::sleep(backoff) => {}
                    }
                }
                Err(err) => {
                    self.metrics.rpc_latency.record(overall_start.elapsed().as_secs_f64());
                    error!(
                        builder_url = %self.builder_url,
                        error = %err,
                        txs = tx_count,
                        retryable = Self::is_retryable(&err),
                        "RPC send failed, dropping batch",
                    );
                    self.metrics.rpc_errors.increment(1);
                    return;
                }
            }
        }
    }

    fn is_retryable(err: &ClientError) -> bool {
        matches!(
            err,
            ClientError::Transport(_) | ClientError::RequestTimeout | ClientError::RestartNeeded(_)
        )
    }
}

impl<T: PoolTransaction> std::fmt::Debug for Forwarder<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forwarder")
            .field("builder_url", &self.builder_url)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}
