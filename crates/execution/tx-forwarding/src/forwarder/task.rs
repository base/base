use std::{collections::VecDeque, sync::Arc, time::Instant};

use alloy_eips::Encodable2718;
use alloy_primitives::{Bytes, TxHash};
use base_execution_txpool::{
    BundleTransaction, NoExtensions, ValidatedTransaction, ValidatedTransactionExtensions,
};
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use jsonrpsee::{
    core::{
        ClientError,
        client::{BatchResponse, ClientT},
        params::BatchRequestBuilder,
    },
    http_client::HttpClient,
};
use reth_transaction_pool::{PoolTransaction, ValidPoolTransaction};
use serde_json::{Map, json};
use tokio::{sync::mpsc, time};
use tracing::{debug, error, info, trace};

use super::{config::ForwarderConfig, metrics::ForwarderMetrics};
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
        Self { timestamps: VecDeque::with_capacity(max_rps as usize), max_rps }
    }

    fn prune(&mut self, now: Instant) {
        let window = std::time::Duration::from_secs(1);
        while self.timestamps.front().is_some_and(|front| now.duration_since(*front) >= window) {
            self.timestamps.pop_front();
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

        Some(std::time::Duration::from_secs(1).saturating_sub(
            now.duration_since(*self.timestamps.front().expect("non-empty after prune")),
        ))
    }

    fn record_send(&mut self) {
        if self.max_rps > 0 {
            self.timestamps.push_back(Instant::now());
        }
    }
}

/// Async forwarder task that receives transactions from a destination queue
/// and sends them to a single builder via RPC.
///
/// Under normal load, each transaction is sent immediately as a batch of 1.
/// When the sliding window rate limit (`max_rps`) is hit, incoming
/// transactions buffer and flush as a single batch (capped at
/// `max_batch_size`) once the window opens.
pub(crate) struct DestinationForwarder<T: PoolTransaction, E = NoExtensions> {
    builder_url: url::Url,
    /// Pre-computed URL label shared cheaply across metric emissions.
    url_label: Arc<str>,
    client: HttpClient,
    receiver: mpsc::Receiver<Arc<ValidPoolTransaction<T>>>,
    config: Arc<ForwarderConfig>,
    limiter: RateLimiter,
    buffer: Vec<BufferedTransaction<E>>,
    buffer_limit: usize,
}

struct BufferedTransaction<E> {
    transaction: ValidatedTransaction<E>,
    tx_hash: TxHash,
}

impl<T, E> DestinationForwarder<T, E>
where
    T: PoolTransaction + BundleTransaction,
    <T as PoolTransaction>::Consensus: Encodable2718,
    E: ValidatedTransactionExtensions<T>,
{
    /// Creates a new forwarder for a single builder endpoint.
    pub(crate) fn new(
        builder_url: url::Url,
        client: HttpClient,
        receiver: mpsc::Receiver<Arc<ValidPoolTransaction<T>>>,
        config: Arc<ForwarderConfig>,
        queue_capacity: usize,
    ) -> Self {
        let limiter = RateLimiter::new(config.max_rps);
        let buffer_limit =
            if config.max_batch_size == 0 { queue_capacity } else { config.max_batch_size };
        let buffer = Vec::with_capacity(buffer_limit);
        let url_label: Arc<str> = builder_url.to_string().into();
        Self { builder_url, url_label, client, receiver, config, limiter, buffer, buffer_limit }
    }

    /// Runs the forwarder loop until the destination queue closes.
    pub(crate) async fn run(mut self) {
        info!(
            builder_url = %self.builder_url,
            max_rps = self.config.max_rps,
            max_batch_size = self.config.max_batch_size,
            "starting transaction forwarder",
        );

        loop {
            match self.limiter.check_rate_limit() {
                None if !self.buffer.is_empty() => {
                    self.flush_buffer().await;
                    continue;
                }
                Some(wait) => {
                    if self.buffer.len() >= self.buffer_limit {
                        time::sleep(wait).await;
                        continue;
                    }
                    let closed = tokio::select! {
                        _ = time::sleep(wait) => { continue; }
                        transaction = self.receiver.recv() => {
                            self.handle_recv(transaction)
                        }
                    };
                    if closed {
                        break;
                    }
                    continue;
                }
                _ => {}
            }

            let transaction = self.receiver.recv().await;
            let closed = self.handle_recv(transaction);
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
    fn handle_recv(&mut self, transaction: Option<Arc<ValidPoolTransaction<T>>>) -> bool {
        match transaction {
            Some(tx) => {
                let sender = *tx.sender_ref();
                let tx_hash = *tx.transaction.hash();
                let consensus = tx.transaction.clone_into_consensus();
                let raw = Bytes::from(consensus.inner().encoded_2718());
                let min_block_number = tx.transaction.min_block_number();
                let max_block_number = tx.transaction.max_block_number();
                let min_timestamp = tx.transaction.min_timestamp_millis();
                let max_timestamp = tx.transaction.max_timestamp_millis();
                self.buffer.push(BufferedTransaction {
                    transaction: ValidatedTransaction {
                        sender,
                        raw,
                        min_block_number,
                        max_block_number,
                        min_timestamp,
                        max_timestamp,
                        extensions: E::extract(&tx),
                    },
                    tx_hash,
                });
                ForwarderMetrics::buffer_size(Arc::clone(&self.url_label))
                    .set(self.buffer.len() as f64);
                false
            }
            None => {
                info!(
                    builder_url = %self.builder_url,
                    buffered = self.buffer.len(),
                    "destination queue closed",
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
        let buffered: Vec<BufferedTransaction<E>> = self.buffer.drain(..batch_size).collect();
        ForwarderMetrics::buffer_size(Arc::clone(&self.url_label)).set(self.buffer.len() as f64);

        if buffered.is_empty() {
            return;
        }
        let (tx_hashes, batch): (Vec<TxHash>, Vec<ValidatedTransaction<E>>) =
            buffered.into_iter().map(|tx| (tx.tx_hash, tx.transaction)).unzip();

        trace!(
            builder_url = %self.builder_url,
            txs = batch.len(),
            remaining = self.buffer.len(),
            "flushing batch",
        );

        self.send_with_retries(batch, tx_hashes).await;
        self.limiter.record_send();
    }

    async fn send_with_retries(&self, batch: Vec<ValidatedTransaction<E>>, tx_hashes: Vec<TxHash>) {
        let tx_count = batch.len() as u64;
        let overall_start = Instant::now();
        for attempt in 0..=self.config.max_retries {
            for tx_hash in &tx_hashes {
                self.emit_forward_event(
                    TransactionEventType::TxpoolBuilderForwardAttempt,
                    Some(*tx_hash),
                    Some(attempt),
                    Map::from_iter([
                        ("attempt".to_string(), json!(attempt)),
                        ("batch_size".to_string(), json!(tx_count)),
                    ]),
                );
            }
            let result = self.send_batch(&batch).await;

            match result {
                Ok(response) => {
                    ForwarderMetrics::rpc_latency(Arc::clone(&self.url_label))
                        .record(overall_start.elapsed().as_secs_f64());
                    ForwarderMetrics::batches_sent(Arc::clone(&self.url_label)).increment(1);

                    let mut ok_count = 0u64;
                    let mut err_count = 0u64;
                    for (idx, res) in response.into_iter().enumerate() {
                        let tx_hash = tx_hashes.get(idx).copied();
                        match res {
                            Ok(()) => {
                                ok_count += 1;
                                self.emit_forward_event(
                                    TransactionEventType::TxpoolBuilderForwardSuccess,
                                    tx_hash,
                                    Some(attempt),
                                    Map::from_iter([
                                        ("attempt".to_string(), json!(attempt)),
                                        ("batch_size".to_string(), json!(tx_count)),
                                    ]),
                                );
                            }
                            Err(e) => {
                                debug!(
                                    builder_url = %self.builder_url,
                                    error = %e,
                                    "batch item rejected",
                                );
                                err_count += 1;
                                self.emit_forward_event(
                                    TransactionEventType::TxpoolBuilderForwardFailure,
                                    tx_hash,
                                    Some(attempt),
                                    Map::from_iter([
                                        ("attempt".to_string(), json!(attempt)),
                                        ("batch_size".to_string(), json!(tx_count)),
                                        ("error".to_string(), json!(e.to_string())),
                                    ]),
                                );
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
                    time::sleep(backoff).await;
                }
                Err(err) => {
                    ForwarderMetrics::rpc_latency(Arc::clone(&self.url_label))
                        .record(overall_start.elapsed().as_secs_f64());
                    for tx_hash in &tx_hashes {
                        self.emit_forward_event(
                            TransactionEventType::TxpoolBuilderForwardDropped,
                            Some(*tx_hash),
                            Some(attempt),
                            Map::from_iter([
                                ("drop_reason".to_string(), json!("rpc_failure")),
                                ("attempt".to_string(), json!(attempt)),
                                ("batch_size".to_string(), json!(tx_count)),
                                ("retryable".to_string(), json!(Self::is_retryable(&err))),
                                ("error".to_string(), json!(err.to_string())),
                            ]),
                        );
                    }
                    error!(
                        builder_url = %self.builder_url,
                        error = %err,
                        txs = tx_count,
                        retryable = Self::is_retryable(&err),
                        "RPC send failed, dropping batch",
                    );
                    ForwarderMetrics::rpc_errors(
                        Arc::clone(&self.url_label),
                        ForwarderMetrics::rpc_error_label(&err),
                    )
                    .increment(1);
                    return;
                }
            }
        }
    }

    async fn send_batch(
        &self,
        batch: &[ValidatedTransaction<E>],
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

    fn emit_forward_event(
        &self,
        event_type: TransactionEventType,
        tx_hash: Option<TxHash>,
        attempt: Option<u32>,
        mut data: Map<String, serde_json::Value>,
    ) {
        data.entry("target".to_string()).or_insert_with(|| json!("builder_forwarder"));
        data.entry("rpc_method".to_string())
            .or_insert_with(|| json!("base_insertValidatedTransaction"));
        let attempt_id = attempt.map(|attempt| attempt.to_string()).unwrap_or_default();

        let _ = transaction_event!(
            producer: TransactionEventProducer::BaseRethNode,
            event_type: event_type,
            maybe_tx_hash: tx_hash,
            id: {
                "builder_url" => self.url_label.as_ref(),
                "attempt" => attempt_id,
                "tx_hash" => tx_hash.map(|hash| format!("{hash:#x}")).unwrap_or_default(),
            },
            data: data,
        );
    }
}

impl<T: PoolTransaction, E> std::fmt::Debug for DestinationForwarder<T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DestinationForwarder")
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
