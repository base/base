use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_primitives::TxHash;
use base_execution_txpool::MAX_VALIDATED_INSERT_BATCH;
use base_observability_events::{
    TransactionEventProducer, TransactionEventType, transaction_event,
};
use jsonrpsee::{
    core::{
        ClientError,
        client::{BatchResponse, ClientT},
        params::{ArrayParams, BatchRequestBuilder},
    },
    http_client::HttpClient,
    types::{ErrorObject, ErrorObjectOwned},
};
use serde_json::{Map, json, value::RawValue};
use tokio::{sync::mpsc, time};
use tracing::{debug, error, info, trace};

use super::{config::ForwarderConfig, metrics::ForwarderMetrics, request::ForwardRequest};

/// Builder method that inserts a whole batch of validated transactions in one call.
const INSERT_BATCH_METHOD: &str = "base_insertValidatedTransactions";

/// How a builder's pool error message ends when the pool already holds the transaction.
///
/// The builder relays the pool error as text, so this is matched against the message's end.
const ALREADY_IMPORTED: &str = "already imported";

/// Token-bucket rate limiter: a bucket of `max_rps` sends, refilled at `max_rps` per second.
///
/// Each send waits after the previous one for a share of the interval `1 / max_rps` equal to the
/// share of the bucket already spent: not at all when it is full, a whole interval when it is
/// empty. So no send waits longer than one interval, and under a sustained stream of small
/// requests sends coalesce into larger batches while the bucket drains, rather than spending it
/// on single requests and then pacing every later send a full interval apart. A sliding window of
/// `max_rps` sends per second instead let a run of small sends spend the whole second's budget at
/// once and then stall the destination queue for the rest of the second, repeating every second
/// after it.
///
/// Waits are slept on tokio's timer, which rounds a deadline up to the next millisecond. Once a
/// send has spent any of the bucket, the next one waits at least until then, unless the previous
/// RPC took longer. Against a builder that answers in under a millisecond, a busy destination
/// therefore sends at most about 1000 batches per second, however high `max_rps` is set.
struct RateLimiter {
    /// Spacing between sends at the limit; `None` when rate limiting is disabled.
    interval: Option<Duration>,
    /// When the next send would be due if every send had been paced exactly.
    next_due: Option<Instant>,
    /// When the previous send started.
    last_send: Option<Instant>,
}

impl RateLimiter {
    fn new(max_rps: u32) -> Self {
        // Above 1e9 sends per second the interval would truncate to zero and remove the cap.
        let interval =
            (max_rps > 0).then(|| (Duration::from_secs(1) / max_rps).max(Duration::from_nanos(1)));
        Self { interval, next_due: None, last_send: None }
    }

    /// Returns `None` if a send is allowed at `now`, or `Some(wait)` with the precise duration
    /// until the next one is. A `max_rps` of 0 disables rate limiting entirely.
    fn check_rate_limit(&self, now: Instant) -> Option<Duration> {
        // A full bucket lets `max_rps - 1` sends go ahead of the paced schedule.
        let (interval, next_due) = (self.interval?, self.next_due?);
        let ahead = Duration::from_secs(1).saturating_sub(interval);
        let allowed_at = next_due.checked_sub(ahead).unwrap_or(next_due);
        // The emptier the bucket, the longer the gap after the previous send, up to one interval
        // when it is empty. Sends then coalesce while the bucket drains, instead of running it dry
        // with small sends and pacing every later one a full interval apart.
        let used = next_due.saturating_duration_since(now).as_secs_f64().min(1.0);
        let spaced = self.last_send.map(|last| last + interval.mul_f64(used));
        let allowed_at = spaced.map_or(allowed_at, |spaced| spaced.max(allowed_at));
        let wait = allowed_at.saturating_duration_since(now);
        (!wait.is_zero()).then_some(wait)
    }

    /// Records a send starting at `now`.
    fn record_send(&mut self, now: Instant) {
        if let Some(interval) = self.interval {
            self.last_send = Some(now);
            self.next_due = Some(self.next_due.map_or(now, |due| due.max(now)) + interval);
        }
    }
}

/// Async forwarder task that receives requests from a destination queue and
/// sends them to a single builder via RPC.
///
/// Once one request is available, the forwarder drains all other immediately
/// available requests into the same batch, capped at `max_batch_size`. This
/// batches bursts without delaying an isolated request. While the rate limit
/// (`max_rps`) holds the next send back, requests continue buffering for it.
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
    /// Where to report transactions the destination confirmed it holds.
    delivery_receipts: Option<mpsc::Sender<(TxHash, Instant)>>,
    /// Set when a send fails in a way that may mean the destination restarted, so the reader
    /// resends what it confirmed.
    destination_lost: Arc<AtomicBool>,
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
        let buffer_limit = config.batch_limit();
        let buffer = Vec::with_capacity(buffer_limit);
        let url_label: Arc<str> = builder_url.to_string().into();
        Self {
            builder_url,
            url_label,
            client,
            receiver,
            config,
            limiter,
            buffer,
            buffer_limit,
            delivery_receipts: None,
            destination_lost: Arc::default(),
        }
    }

    /// Reports each transaction the destination accepted, or rejected as already held, on
    /// `receipts`, and sets `lost` when a send to the destination fails with a connection error or
    /// timeout, as it does while restarting.
    ///
    /// Receipts are best effort: one that does not fit in `receipts` is dropped, which only
    /// leaves that transaction on its regular resend schedule.
    pub(crate) fn with_delivery_receipts(
        mut self,
        receipts: mpsc::Sender<(TxHash, Instant)>,
        lost: Arc<AtomicBool>,
    ) -> Self {
        self.delivery_receipts = Some(receipts);
        self.destination_lost = lost;
        self
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

            match self.limiter.check_rate_limit(Instant::now()) {
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

        // Spacing counts from the start of a send, so a slow builder does not add the full
        // interval on top of its own latency.
        self.limiter.record_send(Instant::now());
        self.send_with_retries(batch).await;
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
            // The first attempt is not journaled: every send ends in a failure or drop event here
            // or a builder-side insert event. Only retries add information.
            if attempt > 0 {
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
            }
            let result = self.send(&batch).await;
            // A connection error or timeout may mean the destination restarted and lost the
            // transactions it confirmed earlier, even if a retry then succeeds.
            if result.as_ref().is_err_and(Self::is_retryable) {
                self.destination_lost.store(true, Ordering::Relaxed);
            }

            match result {
                Ok(response) => {
                    ForwarderMetrics::rpc_latency(Arc::clone(&self.url_label))
                        .record(overall_start.elapsed().as_secs_f64());
                    ForwarderMetrics::batches_sent(Arc::clone(&self.url_label)).increment(1);

                    // When the destination confirmed what it holds, which bounds how long a
                    // receipt may defer a resend however late the reader takes it.
                    let acked_at = Instant::now();
                    let mut ok_count = 0u64;
                    let mut err_count = 0u64;
                    for (idx, res) in response.into_iter().enumerate() {
                        let tx_hash = tx_hashes.get(idx).copied().flatten();
                        let method = methods.get(idx).copied().unwrap_or_default();
                        let held = res
                            .as_ref()
                            .map_or_else(|e| e.message().ends_with(ALREADY_IMPORTED), |()| true);
                        if held
                            && let (Some(receipts), Some(tx_hash)) =
                                (&self.delivery_receipts, tx_hash)
                        {
                            let _ = receipts.try_send((tx_hash, acked_at));
                        }
                        match res {
                            // Delivery is journaled by the destination as
                            // `TXPOOL_VALIDATED_INSERT_ACCEPTED`; a node-side success event per
                            // destination would duplicate it.
                            Ok(()) => ok_count += 1,
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

    /// Sends `batch` and returns one result per request, in order.
    ///
    /// With `batched_insert` on, a batch made only of inserts, and no larger than
    /// [`MAX_VALIDATED_INSERT_BATCH`], goes out as one [`INSERT_BATCH_METHOD`] call, which the
    /// builder admits as one pool batch. Everything else goes out as a JSON-RPC batch of single
    /// calls.
    async fn send(&self, batch: &[R]) -> Result<Vec<Result<(), ErrorObjectOwned>>, ClientError> {
        if self.config.batched_insert
            && batch.len() <= MAX_VALIDATED_INSERT_BATCH
            && let Some(transactions) =
                batch.iter().map(ForwardRequest::batched_insert).collect::<Option<Vec<_>>>()
        {
            return self.send_insert_batch(transactions, batch.len()).await;
        }
        let response = self.send_batch(batch).await?;
        Ok(response.into_iter().map(|result| result.map_err(ErrorObject::into_owned)).collect())
    }

    async fn send_insert_batch(
        &self,
        transactions: Vec<Result<Box<RawValue>, serde_json::Error>>,
        expected: usize,
    ) -> Result<Vec<Result<(), ErrorObjectOwned>>, ClientError> {
        let transactions = transactions
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(ClientError::ParseError)?;
        let mut params = ArrayParams::new();
        params.insert(transactions).map_err(ClientError::ParseError)?;
        // Each entry is `null` or the error `base_insertValidatedTransaction` would have returned.
        let entries: Vec<Option<Box<RawValue>>> =
            self.client.request(INSERT_BATCH_METHOD, params).await?;
        if entries.len() != expected {
            return Err(ClientError::Custom(format!(
                "{INSERT_BATCH_METHOD} returned {} results for {expected} transactions",
                entries.len()
            )));
        }
        entries
            .into_iter()
            .map(|entry| {
                entry.map_or(Ok(Ok(())), |raw| {
                    serde_json::from_str::<ErrorObject<'_>>(raw.get())
                        .map(|error| Err(error.into_owned()))
                        .map_err(ClientError::ParseError)
                })
            })
            .collect()
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
    use jsonrpsee::{RpcModule, http_client::HttpClientBuilder, server::Server};
    use reth_transaction_pool::error::{PoolError, PoolErrorKind};
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    use super::{
        super::{config::UNLIMITED_BATCH_BUFFER_LIMIT, request::InsertValidatedTransaction},
        *,
    };

    const INSERT_METHOD: &str = "base_insertValidatedTransaction";

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
                metering: None,
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
            batched_insert: false,
        })
    }

    /// [`config`] with `batched_insert` on.
    fn batched(max_rps: u32, max_batch_size: usize) -> Arc<ForwarderConfig> {
        Arc::new(ForwarderConfig {
            batched_insert: true,
            ..(*config(max_rps, max_batch_size)).clone()
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

    /// A destination serving both insert methods. The batched method records each transaction
    /// under its own name and rejects the one with nonce 1.
    async fn batch_rpc_server() -> (url::Url, Calls, jsonrpsee::server::ServerHandle) {
        let received: Calls = Arc::default();
        let mut module = RpcModule::new(Arc::clone(&received));
        module
            .register_method(INSERT_METHOD, |params, received, _| {
                let (argument,): (Value,) = params.parse()?;
                received.lock().unwrap().push((INSERT_METHOD.to_string(), argument));
                Ok::<_, ErrorObjectOwned>(())
            })
            .unwrap();
        module
            .register_method(INSERT_BATCH_METHOD, |params, received, _| {
                let (transactions,): (Vec<Value>,) = params.parse()?;
                let mut received = received.lock().unwrap();
                Ok::<_, ErrorObjectOwned>(
                    transactions
                        .into_iter()
                        .map(|tx| {
                            let rejected = tx["raw"] == "0x01";
                            received.push((INSERT_BATCH_METHOD.to_string(), tx));
                            rejected
                                .then(|| ErrorObjectOwned::owned(-32000, "rejected", None::<()>))
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap();
        let server = Server::builder().build(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let address = server.local_addr().unwrap();
        let handle = server.start(module);
        (url::Url::parse(&format!("http://{address}")).unwrap(), received, handle)
    }

    fn methods(received: &Calls) -> Vec<String> {
        received.lock().unwrap().iter().map(|(method, _)| method.clone()).collect()
    }

    /// Params of every recorded call, dropping the method name.
    fn payloads(received: &Calls) -> Vec<Value> {
        received.lock().unwrap().iter().map(|(_, params)| params.clone()).collect()
    }

    /// Nonce of every recorded insert, in arrival order, recovered from the single `raw` byte
    /// [`transaction`] encodes. Lets a `run`-driven test assert end-to-end delivery order without
    /// observing internal batch boundaries the server never sees.
    fn nonces(received: &Calls) -> Vec<u64> {
        payloads(received)
            .iter()
            .map(|params| {
                let raw = params["raw"].as_str().expect("raw hex string");
                u64::from_str_radix(raw.trim_start_matches("0x"), 16).expect("single nonce byte")
            })
            .collect()
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
            assert!(limiter.check_rate_limit(Instant::now()).is_none());
            limiter.record_send(Instant::now());
        }

        assert!(limiter.next_due.is_none());
    }

    /// Sends as fast as the limiter allows from `start`, returning each send's wait.
    fn waits_when_saturated(
        limiter: &mut RateLimiter,
        start: Instant,
        sends: usize,
    ) -> Vec<Duration> {
        let mut now = start;
        (0..sends)
            .map(|_| {
                let wait = limiter.check_rate_limit(now).unwrap_or_default();
                now += wait;
                limiter.record_send(now);
                wait
            })
            .collect()
    }

    /// A run of small sends must not spend the whole second's budget and then hold every later
    /// send back for the rest of the second. A sliding one-second window would hold the 201st
    /// send back for about a second.
    #[test]
    fn rate_limiter_never_holds_a_send_back_longer_than_one_interval() {
        let mut limiter = RateLimiter::new(200);

        let waits = waits_when_saturated(&mut limiter, Instant::now(), 2_000);

        assert!(waits.iter().all(|wait| *wait <= Duration::from_millis(5)));
    }

    /// Saturated, the limiter averages `max_rps` sends per second, on top of at most one bucket.
    #[test]
    fn rate_limiter_enforces_limit() {
        let mut limiter = RateLimiter::new(200);

        let waits = waits_when_saturated(&mut limiter, Instant::now(), 2_000);

        let elapsed: Duration = waits.iter().sum();
        assert!(elapsed >= Duration::from_secs(9), "2000 sends took {elapsed:?}");
    }

    /// An isolated send goes out at once, and the gap after each send grows as the bucket drains.
    #[test]
    fn rate_limiter_spaces_sends_further_apart_as_the_bucket_drains() {
        let mut limiter = RateLimiter::new(200);
        let start = Instant::now();
        assert_eq!(limiter.check_rate_limit(start), None);

        let waits = waits_when_saturated(&mut limiter, start, 400);

        assert!(waits.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(waits[1] > Duration::ZERO && waits[1] < Duration::from_micros(100));
        assert!(waits[199] > waits[1], "the first bucketful spreads out instead of going at once");
        assert!(waits[399] > Duration::from_millis(4), "a draining bucket nears a full interval");
        let idle = start + waits.iter().sum::<Duration>() + Duration::from_secs(1);
        assert_eq!(limiter.check_rate_limit(idle), None, "a refilled bucket sends at once");
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
        forwarder.limiter.record_send(Instant::now());

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
        assert!(forwarder.limiter.check_rate_limit(Instant::now()).is_some());
        assert_eq!(received.lock().unwrap().len(), 2);

        forwarder.flush_buffer().await;
        assert_eq!(forwarder.buffer.len(), 1);
        assert_eq!(received.lock().unwrap().len(), 4);

        forwarder.flush_buffer().await;
        assert!(forwarder.buffer.is_empty());
        assert_eq!(received.lock().unwrap().len(), 5);
    }

    /// Focused unit test of `fill_buffer_from_queue`: a queue with more ready requests than
    /// `max_batch_size` forms successive full batches (100, 100, 50), in submission order. The
    /// server never sees batch boundaries, so this asserts them directly on the buffer; the
    /// `run`-driven tests below cover the surrounding loop wiring end-to-end.
    #[tokio::test]
    async fn fill_buffer_from_queue_forms_capped_batches_in_order() {
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

    /// An isolated request is forwarded without waiting to accumulate a batch. The sender stays
    /// open, so the forwarder cannot be draining on shutdown;
    /// if `run` blocked to fill a batch, the request would never arrive and the timeout would fire.
    #[tokio::test]
    async fn run_forwards_an_isolated_request_without_waiting() {
        let (url, received, _server) = rpc_server().await;
        let (sender, receiver) = mpsc::channel(8);
        let forwarder =
            forwarder::<InsertValidatedTransaction<NoExtensions>>(url, receiver, config(0, 100));
        let task = tokio::spawn(forwarder.run());

        sender.send(transaction(0)).await.unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            while received.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("isolated request was not forwarded promptly");

        assert_eq!(nonces(&received), vec![0], "the lone request is sent on its own");

        drop(sender);
        task.await.unwrap();
    }

    /// End-to-end through `run`: a queue holding more than `max_batch_size` ready requests is
    /// drained across successive batches and every request reaches the destination in submission
    /// order. Exercises the real loop wiring — recv guard, queue drain, batch chunking, and the
    /// final `flush_remaining` on channel close — that the focused unit test above bypasses.
    #[tokio::test]
    async fn run_batches_ready_requests_and_preserves_order() {
        let (url, received, _server) = rpc_server().await;
        let (sender, receiver) = mpsc::channel(250);
        for nonce in 0..250 {
            sender.send(transaction::<NoExtensions>(nonce)).await.unwrap();
        }
        drop(sender);

        let forwarder =
            forwarder::<InsertValidatedTransaction<NoExtensions>>(url, receiver, config(0, 100));
        forwarder.run().await;

        assert_eq!(nonces(&received), (0..250).collect::<Vec<_>>());
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
        forwarder.limiter.record_send(Instant::now());
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
        forwarder.limiter.record_send(Instant::now());
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

    #[tokio::test]
    async fn insert_batches_travel_in_one_batched_call_with_per_transaction_results() {
        let (url, received, _server) = batch_rpc_server().await;
        let (_sender, receiver) = mpsc::channel(1);
        let forwarder = forwarder(url, receiver, batched(0, 100));
        let batch: Vec<_> = (0..3).map(transaction::<NoExtensions>).collect();

        let results = forwarder.send(&batch).await.unwrap();

        assert_eq!(methods(&received), [INSERT_BATCH_METHOD; 3]);
        assert_eq!(nonces(&received), [0, 1, 2]);
        assert!(results[0].is_ok() && results[2].is_ok());
        assert_eq!(results[1].as_ref().unwrap_err().message(), "rejected");
    }

    /// With `batched_insert` off, inserts go out as a JSON-RPC batch of single calls even to a
    /// destination that serves the batched method.
    #[tokio::test]
    async fn batched_insert_off_sends_single_calls() {
        let (url, received, _server) = batch_rpc_server().await;
        let (_sender, receiver) = mpsc::channel(1);
        let forwarder = forwarder(url, receiver, config(0, 100));
        let batch: Vec<_> = (0..3).map(transaction::<NoExtensions>).collect();

        assert!(forwarder.send(&batch).await.unwrap().iter().all(Result::is_ok));
        assert_eq!(methods(&received), [INSERT_METHOD; 3]);
        assert_eq!(nonces(&received), [0, 1, 2]);
    }

    /// Batches the batched method cannot carry — over its limit, or holding requests without a
    /// batched form — still go out as JSON-RPC batches of single calls.
    #[tokio::test]
    async fn batches_without_a_batched_form_use_single_calls() {
        let (url, received, _server) = batch_rpc_server().await;
        let (_sender, receiver) = mpsc::channel(1);
        let oversized_forwarder = forwarder(url.clone(), receiver, batched(0, 0));
        let oversized: Vec<_> =
            (0..=MAX_VALIDATED_INSERT_BATCH as u64).map(transaction::<NoExtensions>).collect();
        assert!(oversized_forwarder.send(&oversized).await.unwrap().iter().all(Result::is_ok));
        assert!(methods(&received).iter().all(|method| method == INSERT_METHOD));

        received.lock().unwrap().clear();
        let (_sender, receiver) = mpsc::channel(1);
        let mixed_forwarder = forwarder(url, receiver, batched(0, 0));
        let mixed = [TestRequest::Insert(Box::new(transaction::<NoExtensions>(0).transaction))];
        mixed_forwarder.send(&mixed).await.unwrap();
        assert_eq!(methods(&received), [INSERT_METHOD]);
    }

    /// The forwarder reports a transaction as held when the builder accepted it or already had
    /// it, and not when the builder rejected it for any other reason, and reports the builder lost
    /// once a send to it fails.
    #[rstest::rstest]
    #[tokio::test]
    async fn delivery_receipts_cover_accepted_and_already_imported_transactions(
        #[values(false, true)] batched_insert: bool,
    ) {
        // Builder-shaped rejection messages, as `BuilderApiImpl` formats pool errors.
        fn pool_rejection(nonce: u64, kind: PoolErrorKind) -> ErrorObjectOwned {
            let error = PoolError::new(B256::with_last_byte(nonce as u8), kind);
            ErrorObjectOwned::owned(
                -32603,
                format!("pool rejected transaction: {error}"),
                None::<()>,
            )
        }
        fn verdict(tx: &Value) -> Option<ErrorObjectOwned> {
            match tx["raw"].as_str() {
                Some("0x01") => Some(pool_rejection(1, PoolErrorKind::AlreadyImported)),
                Some("0x02") => Some(pool_rejection(2, PoolErrorKind::DiscardedOnInsert)),
                _ => None,
            }
        }
        let mut module = RpcModule::new(());
        module
            .register_method(INSERT_METHOD, |params, _, _| {
                let (tx,): (Value,) = params.parse()?;
                verdict(&tx).map_or(Ok(()), Err)
            })
            .unwrap();
        module
            .register_method(INSERT_BATCH_METHOD, |params, _, _| {
                let (txs,): (Vec<Value>,) = params.parse()?;
                Ok::<_, ErrorObjectOwned>(txs.iter().map(verdict).collect::<Vec<_>>())
            })
            .unwrap();
        let server = Server::builder().build(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let url = url::Url::parse(&format!("http://{}", server.local_addr().unwrap())).unwrap();
        let server = server.start(module);
        let (_sender, receiver) = mpsc::channel(1);
        let (receipts, mut receipts_rx) = mpsc::channel(8);
        let lost = Arc::new(AtomicBool::new(false));
        let config = if batched_insert { batched(0, 0) } else { config(0, 0) };
        let mut forwarder = forwarder::<InsertValidatedTransaction>(url, receiver, config)
            .with_delivery_receipts(receipts, Arc::clone(&lost));
        for nonce in 0..3 {
            assert!(!forwarder.handle_recv(Some(transaction(nonce))));
        }

        forwarder.flush_buffer().await;

        let held: Vec<_> = std::iter::from_fn(|| receipts_rx.try_recv().ok()).collect();
        let held: Vec<_> = held.into_iter().map(|(hash, _)| hash).collect();
        assert_eq!(held, vec![B256::with_last_byte(0), B256::with_last_byte(1)]);
        assert!(!lost.load(Ordering::Relaxed));

        server.stop().unwrap();
        server.stopped().await;
        assert!(!forwarder.handle_recv(Some(transaction(3))));
        forwarder.flush_buffer().await;

        assert!(lost.load(Ordering::Relaxed));
        assert!(receipts_rx.try_recv().is_err());
    }

    /// A timeout marks the destination lost even when the retry then succeeds: the builder may
    /// have restarted in between and dropped what it confirmed before.
    #[tokio::test]
    async fn a_timeout_marks_the_destination_lost_even_if_the_retry_succeeds() {
        let mut module = RpcModule::new(std::sync::atomic::AtomicUsize::new(0));
        module
            .register_async_method(INSERT_METHOD, |_, calls, _| async move {
                if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    time::sleep(Duration::from_millis(500)).await;
                }
                Ok::<_, ErrorObjectOwned>(())
            })
            .unwrap();
        let server = Server::builder().build(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let url = url::Url::parse(&format!("http://{}", server.local_addr().unwrap())).unwrap();
        let _server = server.start(module);
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_millis(100))
            .build(url.as_str())
            .unwrap();
        let config = Arc::new(ForwarderConfig { max_retries: 1, ..(*config(0, 0)).clone() });
        let (_sender, receiver) = mpsc::channel(1);
        let (receipts, mut receipts_rx) = mpsc::channel(8);
        let lost = Arc::new(AtomicBool::new(false));
        let mut forwarder =
            DestinationForwarder::<InsertValidatedTransaction>::new(url, client, receiver, config)
                .with_delivery_receipts(receipts, Arc::clone(&lost));
        assert!(!forwarder.handle_recv(Some(transaction(0))));

        forwarder.flush_buffer().await;

        assert_eq!(
            receipts_rx.try_recv().ok().map(|(hash, _)| hash),
            Some(B256::with_last_byte(0)),
            "the retry landed"
        );
        assert!(lost.load(Ordering::Relaxed));
    }

    /// Receipts that do not fit are dropped without holding up forwarding.
    #[tokio::test]
    async fn a_full_receipt_channel_drops_receipts_without_blocking() {
        let (url, received, _server) = rpc_server().await;
        let (_sender, receiver) = mpsc::channel(1);
        let (receipts, mut receipts_rx) = mpsc::channel(1);
        let mut forwarder = forwarder::<InsertValidatedTransaction>(url, receiver, config(0, 0))
            .with_delivery_receipts(receipts, Arc::default());
        for nonce in 0..3 {
            assert!(!forwarder.handle_recv(Some(transaction(nonce))));
        }

        time::timeout(Duration::from_secs(5), forwarder.flush_buffer())
            .await
            .expect("forwarding does not wait for receipt capacity");

        assert_eq!(nonces(&received), [0, 1, 2]);
        assert_eq!(
            receipts_rx.try_recv().ok().map(|(hash, _)| hash),
            Some(B256::with_last_byte(0))
        );
        assert!(receipts_rx.try_recv().is_err());
    }
}
