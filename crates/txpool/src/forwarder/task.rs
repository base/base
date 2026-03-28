use std::{collections::VecDeque, sync::Arc, time::Instant};

use alloy_eips::Encodable2718;
use alloy_primitives::Bytes;
use jsonrpsee::{
    core::{
        ClientError,
        client::{BatchResponse, ClientT},
        params::BatchRequestBuilder,
    },
    http_client::HttpClient,
};
use reth_transaction_pool::{PoolTransaction, ValidPoolTransaction};
use tokio::{sync::broadcast, time};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use super::{config::ForwarderConfig, metrics::ForwarderMetrics};
use crate::{ValidatedTransaction, transaction::BundleTransaction};

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
        let capacity = if max_rps == 0 { 0 } else { max_rps as usize };
        Self { timestamps: VecDeque::with_capacity(capacity), max_rps }
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
    /// precise duration until the next slot opens. A `max_rps` of 0 disables
    /// rate limiting entirely.
    fn check_rate_limit(&mut self) -> Option<std::time::Duration> {
        if self.max_rps == 0 {
            return None;
        }

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
        if self.max_rps > 0 {
            self.timestamps.push_back(Instant::now());
        }
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
    builder_url: url::Url,
    /// Pre-computed URL label shared cheaply across metric emissions.
    url_label: Arc<str>,
    client: HttpClient,
    receiver: broadcast::Receiver<Arc<ValidPoolTransaction<T>>>,
    config: Arc<ForwarderConfig>,
    cancel: CancellationToken,
    limiter: RateLimiter,
    buffer: Vec<ValidatedTransaction>,
}

impl<T> Forwarder<T>
where
    T: PoolTransaction + BundleTransaction,
    <T as PoolTransaction>::Consensus: Encodable2718,
{
    /// Creates a new forwarder for a single builder endpoint.
    pub fn new(
        builder_url: url::Url,
        client: HttpClient,
        receiver: broadcast::Receiver<Arc<ValidPoolTransaction<T>>>,
        config: Arc<ForwarderConfig>,
        cancel: CancellationToken,
    ) -> Self {
        let limiter = RateLimiter::new(config.max_rps);
        let initial_capacity = if config.max_batch_size == 0 { 256 } else { config.max_batch_size };
        let buffer = Vec::with_capacity(initial_capacity);
        let url_label: Arc<str> = builder_url.to_string().into();
        Self { builder_url, url_label, client, receiver, config, cancel, limiter, buffer }
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
                break;
            }

            match self.limiter.check_rate_limit() {
                None if !self.buffer.is_empty() => {
                    self.flush_buffer().await;
                    continue;
                }
                Some(wait) => {
                    let closed = tokio::select! {
                        _ = self.cancel.cancelled() => break,
                        _ = time::sleep(wait) => { continue; }
                        result = self.receiver.recv() => {
                            self.handle_recv(result)
                        }
                    };
                    if closed {
                        break;
                    }
                    continue;
                }
                _ => {}
            }

            let closed = tokio::select! {
                _ = self.cancel.cancelled() => break,
                result = self.receiver.recv() => {
                    self.handle_recv(result)
                }
            };
            if closed {
                break;
            }
            if !self.buffer.is_empty() && self.limiter.check_rate_limit().is_none() {
                self.flush_buffer().await;
            }
        }

        self.flush_remaining().await;
    }

    /// Returns `true` if the channel is closed and the forwarder should shut down.
    fn handle_recv(
        &mut self,
        result: Result<Arc<ValidPoolTransaction<T>>, broadcast::error::RecvError>,
    ) -> bool {
        match result {
            Ok(tx) => {
                let sender = *tx.sender_ref();
                let consensus = tx.transaction.clone_into_consensus();
                let raw = Bytes::from(consensus.inner().encoded_2718());
                let target_block_number = tx.transaction.target_block_number();
                let min_timestamp = tx.transaction.min_timestamp_millis();
                let max_timestamp = tx.transaction.max_timestamp_millis();
                self.buffer.push(ValidatedTransaction {
                    sender,
                    raw,
                    target_block_number,
                    min_timestamp,
                    max_timestamp,
                });
                ForwarderMetrics::buffer_size(Arc::clone(&self.url_label))
                    .set(self.buffer.len() as f64);
                false
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    builder_url = %self.builder_url,
                    skipped = skipped,
                    "forwarder lagged, dropped transactions",
                );
                ForwarderMetrics::batches_lagged(Arc::clone(&self.url_label)).increment(1);
                ForwarderMetrics::txs_lagged(Arc::clone(&self.url_label)).increment(skipped);
                false
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!(
                    builder_url = %self.builder_url,
                    buffered = self.buffer.len(),
                    "broadcast channel closed",
                );
                true
            }
        }
    }

    async fn flush_remaining(&mut self) {
        while !self.buffer.is_empty() {
            self.flush_buffer().await;
        }
    }

    async fn flush_buffer(&mut self) {
        let batch_size = if self.config.max_batch_size == 0 {
            self.buffer.len()
        } else {
            self.buffer.len().min(self.config.max_batch_size)
        };
        let batch: Vec<ValidatedTransaction> = self.buffer.drain(..batch_size).collect();
        ForwarderMetrics::buffer_size(Arc::clone(&self.url_label)).set(self.buffer.len() as f64);

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

    async fn send_with_retries(&self, batch: Vec<ValidatedTransaction>) {
        let tx_count = batch.len() as u64;
        let overall_start = Instant::now();
        for attempt in 0..=self.config.max_retries {
            let result = self.send_batch(&batch).await;

            match result {
                Ok(response) => {
                    ForwarderMetrics::rpc_latency(Arc::clone(&self.url_label))
                        .record(overall_start.elapsed().as_secs_f64());
                    ForwarderMetrics::batches_sent(Arc::clone(&self.url_label)).increment(1);

                    let mut ok_count = 0u64;
                    let mut err_count = 0u64;
                    for res in response {
                        match res {
                            Ok(()) => ok_count += 1,
                            Err(e) => {
                                debug!(
                                    builder_url = %self.builder_url,
                                    error = %e,
                                    "batch item rejected",
                                );
                                err_count += 1;
                            }
                        }
                    }

                    ForwarderMetrics::txs_forwarded(Arc::clone(&self.url_label))
                        .increment(ok_count);
                    if err_count > 0 {
                        ForwarderMetrics::num_tx_rejected_in_batch(Arc::clone(&self.url_label))
                            .increment(err_count);
                    }
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
                    ForwarderMetrics::rpc_latency(Arc::clone(&self.url_label))
                        .record(overall_start.elapsed().as_secs_f64());
                    error!(
                        builder_url = %self.builder_url,
                        error = %err,
                        txs = tx_count,
                        retryable = Self::is_retryable(&err),
                        "RPC send failed, dropping batch",
                    );
                    ForwarderMetrics::rpc_errors(Arc::clone(&self.url_label)).increment(1);
                    return;
                }
            }
        }
    }

    async fn send_batch(
        &self,
        batch: &[ValidatedTransaction],
    ) -> Result<BatchResponse<'_, ()>, ClientError> {
        let mut request = BatchRequestBuilder::new();
        for tx in batch {
            request.insert("base_insertValidatedTransaction", (tx,)).expect("valid method name");
        }
        self.client.batch_request(request).await
    }

    const fn is_retryable(err: &ClientError) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_unlimited_when_zero() {
        let mut limiter = RateLimiter::new(0);

        for _ in 0..10_000 {
            assert!(limiter.check_rate_limit().is_none());
            limiter.record_send();
        }

        assert!(limiter.timestamps.is_empty());
    }

    #[test]
    fn rate_limiter_enforces_limit() {
        let mut limiter = RateLimiter::new(3);

        for _ in 0..3 {
            assert!(limiter.check_rate_limit().is_none());
            limiter.record_send();
        }

        assert!(limiter.check_rate_limit().is_some());
    }
}
