use std::{collections::VecDeque, sync::Arc, time::Instant};

use alloy_primitives::TxHash;
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
use serde_json::{Map, json};
use tokio::{sync::mpsc, time};
use tracing::{debug, error, info, trace};

use super::{config::ForwarderConfig, metrics::ForwarderMetrics, request::ForwardRequest};

/// Internal buffer cap used when RPC batch size is configured as unlimited.
const UNLIMITED_BATCH_BUFFER_LIMIT: usize = 1024;

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

/// Async forwarder task that receives requests from a destination queue and
/// sends them to a single builder via RPC.
///
/// Once one request is available, the forwarder drains all other immediately
/// available requests into the same batch, capped at `max_batch_size`. This
/// batches bursts without delaying an isolated request. If the sliding window
/// rate limit (`max_rps`) is hit, requests continue buffering until the window
/// opens.
///
/// Requests are relayed in the order the queue yields them, and a batch
/// preserves that order, so a producer may rely on submission order between
/// requests to the same destination.
pub(crate) struct DestinationForwarder<R> {
    builder_url: url::Url,
    /// Pre-computed URL label shared cheaply across metric emissions.
    url_label: Arc<str>,
    client: HttpClient,
    receiver: mpsc::Receiver<R>,
    config: Arc<ForwarderConfig>,
    limiter: RateLimiter,
    buffer: Vec<R>,
    buffer_limit: usize,
}

impl<R: ForwardRequest> DestinationForwarder<R> {
    /// Creates a new forwarder for a single builder endpoint.
    pub(crate) fn new(
        builder_url: url::Url,
        client: HttpClient,
        receiver: mpsc::Receiver<R>,
        config: Arc<ForwarderConfig>,
    ) -> Self {
        let limiter = RateLimiter::new(config.max_rps);
        let buffer_limit = if config.max_batch_size == 0 {
            UNLIMITED_BATCH_BUFFER_LIMIT
        } else {
            config.max_batch_size
        };
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
            if self.buffer.is_empty() {
                let request = self.receiver.recv().await;
                if self.handle_recv(request) {
                    break;
                }
            }

            if self.fill_buffer_from_queue() {
                break;
            }

            match self.limiter.check_rate_limit() {
                None => {
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
            }
        }

        self.flush_remaining().await;
    }

    /// Fills the current batch from requests already waiting in the destination queue.
    ///
    /// Returns `true` when the queue is closed and all of its remaining requests are buffered.
    fn fill_buffer_from_queue(&mut self) -> bool {
        let mut closed = false;
        while self.buffer.len() < self.buffer_limit {
            match self.receiver.try_recv() {
                Ok(request) => self.buffer.push(request),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        self.update_pending_metrics();
        closed
    }

    /// Returns `true` if the channel is closed and the forwarder should shut down.
    fn handle_recv(&mut self, request: Option<R>) -> bool {
        match request {
            Some(request) => {
                self.buffer.push(request);
                self.update_pending_metrics();
                false
            }
            None => {
                self.update_pending_metrics();
                info!(
                    builder_url = %self.builder_url,
                    buffered = self.buffer.len(),
                    "destination queue closed",
                );
                true
            }
        }
    }

    fn update_pending_metrics(&self) {
        ForwarderMetrics::buffer_size(Arc::clone(&self.url_label)).set(self.buffer.len() as f64);
        ForwarderMetrics::queue_size(Arc::clone(&self.url_label)).set(self.receiver.len() as f64);
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
        let batch: Vec<R> = self.buffer.drain(..batch_size).collect();
        ForwarderMetrics::buffer_size(Arc::clone(&self.url_label)).set(self.buffer.len() as f64);

        if batch.is_empty() {
            return;
        }

        ForwarderMetrics::batch_size(Arc::clone(&self.url_label)).record(batch.len() as f64);

        trace!(
            builder_url = %self.builder_url,
            txs = batch.len(),
            remaining = self.buffer.len(),
            "flushing batch",
        );

        self.send_with_retries(batch).await;
        self.limiter.record_send();
    }

    async fn send_with_retries(&self, batch: Vec<R>) {
        // Shutdown waits for this in-flight retry loop to finish. With the default backoff and
        // retry count, a down endpoint can add up to 700ms before the forwarder observes channel
        // closure; the service-level shutdown timeout is much larger than that.
        // Parallel to `batch` by index, so a batch response entry maps back to the request that
        // produced it. Collected once up front because both are read on every retry attempt.
        let tx_hashes: Vec<Option<TxHash>> = batch.iter().map(ForwardRequest::tx_hash).collect();
        let methods: Vec<&'static str> = batch.iter().map(ForwardRequest::method).collect();
        let tx_count = batch.len() as u64;
        let overall_start = Instant::now();
        for attempt in 0..=self.config.max_retries {
            for (tx_hash, method) in tx_hashes.iter().zip(&methods) {
                self.emit_forward_event(
                    TransactionEventType::TxpoolBuilderForwardAttempt,
                    *tx_hash,
                    method,
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
                        let tx_hash = tx_hashes.get(idx).copied().flatten();
                        let method = methods.get(idx).copied().unwrap_or_default();
                        match res {
                            Ok(()) => {
                                ok_count += 1;
                                self.emit_forward_event(
                                    TransactionEventType::TxpoolBuilderForwardSuccess,
                                    tx_hash,
                                    method,
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
                                    method,
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
                    for (tx_hash, method) in tx_hashes.iter().zip(&methods) {
                        self.emit_forward_event(
                            TransactionEventType::TxpoolBuilderForwardDropped,
                            *tx_hash,
                            method,
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

    async fn send_batch(&self, batch: &[R]) -> Result<BatchResponse<'_, ()>, ClientError> {
        let mut request = BatchRequestBuilder::new();
        for item in batch {
            // A payload that will not serialize fails the whole batch rather than panicking the
            // task. `ParseError` is classified non-retryable below, which is correct: re-encoding
            // the same value fails identically, so retrying would only delay the drop.
            let params = item.params().map_err(ClientError::ParseError)?;
            request.insert(item.method(), params).expect("valid method name");
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
        rpc_method: &'static str,
        attempt: Option<u32>,
        mut data: Map<String, serde_json::Value>,
    ) {
        data.entry("target".to_string()).or_insert_with(|| json!("builder_forwarder"));
        data.entry("rpc_method".to_string()).or_insert_with(|| json!(rpc_method));
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

impl<R> std::fmt::Debug for DestinationForwarder<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DestinationForwarder")
            .field("builder_url", &self.builder_url)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Mutex, time::Duration};

    use alloy_primitives::{Address, B256, Bytes};
    use base_execution_txpool::{NoExtensions, ValidatedTransaction};
    use jsonrpsee::{
        RpcModule, core::params::ArrayParams, http_client::HttpClientBuilder, server::Server,
    };
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    use super::{super::request::InsertValidatedTransaction, *};

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    struct TestExtensions {
        test_tag: String,
    }

    /// One insert request, tagged by `nonce` so ordering assertions can name it.
    fn transaction<E: Default>(nonce: u64) -> InsertValidatedTransaction<E> {
        InsertValidatedTransaction {
            transaction: ValidatedTransaction {
                sender: Address::repeat_byte((nonce + 1) as u8),
                raw: Bytes::from(vec![nonce as u8]),
                min_block_number: None,
                max_block_number: None,
                min_timestamp: None,
                max_timestamp: None,
                extensions: E::default(),
            },
            tx_hash: B256::with_last_byte(nonce as u8),
        }
    }

    fn tagged(nonce: u64) -> InsertValidatedTransaction<TestExtensions> {
        let mut request = transaction::<TestExtensions>(nonce);
        request.transaction.extensions.test_tag = "forwarded".to_string();
        request
    }

    /// A two-method request, standing in for a downstream producer that relays more than inserts
    /// over one destination queue.
    #[derive(Debug)]
    enum TestRequest {
        Insert(Box<ValidatedTransaction>),
        Remove(TxHash),
    }

    impl ForwardRequest for TestRequest {
        fn method(&self) -> &'static str {
            match self {
                Self::Insert(_) => "base_insertValidatedTransaction",
                Self::Remove(_) => "test_removeTransaction",
            }
        }

        fn params(&self) -> Result<ArrayParams, serde_json::Error> {
            let mut params = ArrayParams::new();
            match self {
                Self::Insert(transaction) => params.insert(transaction.as_ref())?,
                Self::Remove(hash) => params.insert(hash)?,
            }
            Ok(params)
        }

        fn tx_hash(&self) -> Option<TxHash> {
            match self {
                Self::Insert(_) => None,
                Self::Remove(hash) => Some(*hash),
            }
        }
    }

    fn config(max_rps: u32, max_batch_size: usize) -> Arc<ForwarderConfig> {
        Arc::new(ForwarderConfig {
            max_rps,
            max_batch_size,
            max_retries: 0,
            retry_backoff: Duration::ZERO,
            request_timeout: Duration::from_secs(1),
        })
    }

    /// Records `(method, params)` for every call, in arrival order.
    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    async fn rpc_server() -> (url::Url, Calls, jsonrpsee::server::ServerHandle) {
        let received: Calls = Arc::new(Mutex::new(Vec::new()));
        let mut module = RpcModule::new(Arc::clone(&received));
        for method in ["base_insertValidatedTransaction", "test_removeTransaction"] {
            module
                .register_method(method, move |params, received, _| {
                    let (argument,): (Value,) = params.parse()?;
                    received.lock().unwrap().push((method.to_string(), argument));
                    Ok::<_, jsonrpsee::types::ErrorObjectOwned>(())
                })
                .unwrap();
        }
        let server = Server::builder().build(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let address = server.local_addr().unwrap();
        let handle = server.start(module);
        (url::Url::parse(&format!("http://{address}")).unwrap(), received, handle)
    }

    /// Params of every recorded call, dropping the method name.
    fn payloads(received: &Calls) -> Vec<Value> {
        received.lock().unwrap().iter().map(|(_, params)| params.clone()).collect()
    }

    fn forwarder<R: ForwardRequest>(
        url: url::Url,
        receiver: mpsc::Receiver<R>,
        config: Arc<ForwarderConfig>,
    ) -> DestinationForwarder<R> {
        let client = HttpClientBuilder::default().build(url.as_str()).unwrap();
        DestinationForwarder::new(url, client, receiver, config)
    }

    #[test]
    fn unlimited_batch_size_uses_a_bounded_internal_buffer() {
        let (_sender, receiver) = mpsc::channel::<InsertValidatedTransaction>(16);
        let url = url::Url::parse("http://builder.test").unwrap();
        let forwarder = forwarder(url, receiver, config(1, 0));

        assert_eq!(forwarder.buffer_limit, UNLIMITED_BATCH_BUFFER_LIMIT);
        assert_eq!(forwarder.buffer.capacity(), UNLIMITED_BATCH_BUFFER_LIMIT);
    }

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

    #[tokio::test]
    async fn queue_closure_drains_buffered_transactions_and_writes_extensions() {
        let (url, received, _server) = rpc_server().await;
        let (sender, receiver) = mpsc::channel(4);
        drop(sender);
        let mut forwarder = forwarder(url, receiver, config(1, 0));
        for nonce in 0..3 {
            assert!(!forwarder.handle_recv(Some(tagged(nonce))));
        }
        forwarder.limiter.record_send();

        forwarder.run().await;

        let received = payloads(&received);
        assert_eq!(received.len(), 3);
        assert!(received.iter().all(|tx| tx["test_tag"] == "forwarded"));
        assert!(received.iter().all(|tx| tx.get("extensions").is_none()));
    }

    #[tokio::test]
    async fn flush_buffer_chunks_transactions_at_max_batch_size() {
        let (url, received, _server) = rpc_server().await;
        let (_sender, receiver) = mpsc::channel(1);
        let mut forwarder =
            forwarder::<InsertValidatedTransaction<NoExtensions>>(url, receiver, config(1, 2));
        for nonce in 0..5 {
            assert!(!forwarder.handle_recv(Some(transaction(nonce))));
        }

        forwarder.flush_buffer().await;
        assert_eq!(forwarder.buffer.len(), 3);
        assert!(forwarder.limiter.check_rate_limit().is_some());
        assert_eq!(received.lock().unwrap().len(), 2);

        forwarder.flush_buffer().await;
        assert_eq!(forwarder.buffer.len(), 1);
        assert_eq!(received.lock().unwrap().len(), 4);

        forwarder.flush_buffer().await;
        assert!(forwarder.buffer.is_empty());
        assert_eq!(received.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn ready_queue_is_drained_into_full_batches() {
        let (sender, receiver) = mpsc::channel(250);
        for nonce in 0..250 {
            sender.send(transaction::<NoExtensions>(nonce)).await.unwrap();
        }
        drop(sender);

        let url = url::Url::parse("http://builder.test").unwrap();
        let mut forwarder = forwarder(url, receiver, config(0, 100));
        let mut batch_sizes = Vec::new();
        let mut hashes = Vec::new();

        loop {
            let request = forwarder.receiver.recv().await;
            if forwarder.handle_recv(request) {
                break;
            }
            let closed = forwarder.fill_buffer_from_queue();
            batch_sizes.push(forwarder.buffer.len());
            hashes.extend(forwarder.buffer.drain(..).map(|request| request.tx_hash));
            if closed {
                break;
            }
        }

        assert_eq!(batch_sizes, [100, 100, 50]);
        assert_eq!(
            hashes,
            (0..250).map(|nonce| B256::with_last_byte(nonce as u8)).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn run_buffers_transactions_until_the_rate_limit_reopens() {
        let (url, received, _server) = rpc_server().await;
        let (sender, receiver) = mpsc::channel(5);
        for nonce in 0..5 {
            sender.send(transaction(nonce)).await.unwrap();
        }
        drop(sender);

        let mut forwarder =
            forwarder::<InsertValidatedTransaction<NoExtensions>>(url, receiver, config(1, 2));
        forwarder.limiter.record_send();
        let task = tokio::spawn(forwarder.run());

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(received.lock().unwrap().is_empty());
        assert!(!task.is_finished());

        tokio::time::timeout(Duration::from_secs(4), task).await.unwrap().unwrap();
        assert_eq!(received.lock().unwrap().len(), 5);
    }

    /// A producer may mix request kinds on one destination queue and rely on submission order.
    ///
    /// This is the property a downstream caller needs when a removal supersedes an earlier insert:
    /// if the removal overtook it, the destination would delete nothing and then keep a request
    /// that was meant to be withdrawn. Batching must not reorder, so `max_rps` is set to force all
    /// four into a single batch rather than four separate sends.
    #[tokio::test]
    async fn a_batch_preserves_submission_order_across_request_kinds() {
        let (url, received, _server) = rpc_server().await;
        let (sender, receiver) = mpsc::channel(8);
        let removed = B256::repeat_byte(0x7e);
        let insert =
            |nonce| TestRequest::Insert(Box::new(transaction::<NoExtensions>(nonce).transaction));
        sender.send(insert(0)).await.unwrap();
        sender.send(TestRequest::Remove(removed)).await.unwrap();
        sender.send(insert(1)).await.unwrap();
        sender.send(TestRequest::Remove(B256::repeat_byte(0x7f))).await.unwrap();
        drop(sender);

        let mut forwarder = forwarder(url, receiver, config(1, 0));
        // Consume the one available slot so everything buffers into a single batch.
        forwarder.limiter.record_send();
        forwarder.run().await;

        let calls = received.lock().unwrap();
        let methods: Vec<&str> = calls.iter().map(|(method, _)| method.as_str()).collect();
        assert_eq!(
            methods,
            [
                "base_insertValidatedTransaction",
                "test_removeTransaction",
                "base_insertValidatedTransaction",
                "test_removeTransaction",
            ],
            "a batch must reach the destination in submission order",
        );
        assert_eq!(calls[1].1, json!(removed), "each entry keeps its own params");
    }
}
