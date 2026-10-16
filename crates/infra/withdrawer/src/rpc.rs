use std::time::Duration;

use alloy_consensus::TxEnvelope;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::TransactionReceipt;
use alloy_transport::TransportError;
use base_common_network::Base;
use serde::{Deserialize, de::IgnoredAny};
use thiserror::Error;
use tracing::warn;

use crate::{WithdrawalRpcTransport, WithdrawerConfig};

/// RPC operations that could not complete.
#[derive(Debug, Error)]
pub enum WithdrawalRpcError {
    /// A request failed.
    #[error("RPC request failed: {0}")]
    Request(#[source] TransportError),
    /// The configured deadline elapsed.
    #[error("RPC operation timed out")]
    Timeout,
    /// The transaction was included but reverted.
    #[error("transaction {0} reverted")]
    Reverted(B256),
}

/// Typed chain providers and withdrawal submission policy.
#[derive(Debug, Clone)]
pub struct WithdrawalRpc {
    /// L1 state reads and signed transaction submission.
    pub l1: RootProvider,
    /// Base L2 state reads, including deposit receipts and storage proofs.
    pub l2: RootProvider<Base>,
    /// Deadline for broadcast and receipt waiting.
    pub timeout: Duration,
}

impl WithdrawalRpc {
    /// Fixed delay between receipt requests.
    pub const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(2);

    /// Geth error for receipt lookups while the transaction index is rebuilt; the transaction may still be pending or included.
    pub const TX_INDEXING_IN_PROGRESS: &str = "transaction indexing is in progress";

    /// Builds providers for both configured endpoints without network I/O.
    pub fn new(config: &WithdrawerConfig) -> Result<Self, WithdrawalRpcError> {
        let l1 = WithdrawalRpcTransport::connect(&config.l1_rpc_url, config)
            .map_err(WithdrawalRpcError::Request)?;
        let l2 = WithdrawalRpcTransport::connect(&config.network.l2_rpc_url, config)
            .map_err(WithdrawalRpcError::Request)?;
        Ok(Self { l1, l2, timeout: config.timeout })
    }

    /// Broadcasts a signed envelope; any error is ambiguous because an earlier attempt may have landed, so check `tx.tx_hash()` with `wait_for_receipt` before re-signing.
    pub async fn broadcast(&self, tx: &TxEnvelope) -> Result<B256, WithdrawalRpcError> {
        let hash = *tx.tx_hash();
        let bytes = Bytes::from(tx.encoded_2718());
        tokio::time::timeout(self.timeout, async {
            let result = self
                .l1
                .client()
                .request::<_, BroadcastResult>("eth_sendRawTransaction", (bytes,))
                .await;
            match result {
                Ok(BroadcastResult::Hash(remote)) if remote == hash => {}
                Ok(BroadcastResult::Hash(remote)) => {
                    warn!(
                        local_hash = %hash,
                        remote_hash = %remote,
                        "node returned a different transaction hash"
                    );
                }
                Ok(BroadcastResult::Unexpected(_)) => {
                    warn!(
                        local_hash = %hash,
                        "node accepted transaction with an unexpected result"
                    );
                }
                Err(error)
                    if error.as_error_resp().is_some_and(|payload| {
                        let message = payload.message.trim().to_ascii_lowercase();
                        payload.code != 3
                            && (message == "known transaction"
                                || [
                                    "already known",
                                    "transaction already imported",
                                    "transaction already in pool",
                                ]
                                .iter()
                                .any(|known| message.starts_with(known)))
                    }) => {}
                Err(error) => {
                    if error.as_error_resp().is_some()
                        && matches!(self.l1.get_transaction_by_hash(hash).await, Ok(Some(_)))
                    {
                        warn!(
                            local_hash = %hash,
                            "node knows transaction after broadcast rejection"
                        );
                        return Ok(hash);
                    }
                    return Err(WithdrawalRpcError::Request(WithdrawalRpcTransport::redact(error)));
                }
            }
            Ok(hash)
        })
        .await
        .map_err(|_| WithdrawalRpcError::Timeout)?
    }

    /// Waits for L1 inclusion, distinguishing a revert from a deadline.
    pub async fn wait_for_receipt(
        &self,
        hash: B256,
    ) -> Result<TransactionReceipt, WithdrawalRpcError> {
        tokio::time::timeout(self.timeout, async {
            loop {
                match self.l1.get_transaction_receipt(hash).await {
                    Ok(Some(receipt)) => {
                        if !receipt.status() {
                            return Err(WithdrawalRpcError::Reverted(hash));
                        }
                        return Ok(receipt);
                    }
                    Ok(None) => {}
                    Err(error)
                        if WithdrawalRpcTransport::should_retry(&error)
                            || error.as_error_resp().is_some_and(|payload| {
                                payload
                                    .message
                                    .to_ascii_lowercase()
                                    .contains(Self::TX_INDEXING_IN_PROGRESS)
                            }) =>
                    {
                        warn!(transaction_hash = %hash, error = %error, "receipt request failed transiently");
                    }
                    Err(error) => {
                        return Err(WithdrawalRpcError::Request(WithdrawalRpcTransport::redact(
                            error,
                        )));
                    }
                }
                tokio::time::sleep(Self::RECEIPT_POLL_INTERVAL).await;
            }
        })
        .await
        .map_err(|_| WithdrawalRpcError::Timeout)?
    }
}

/// A successful broadcast result, including nodes that return an invalid hash.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BroadcastResult {
    /// A well-formed transaction hash.
    Hash(B256),
    /// A valid success envelope with an unexpected result value.
    Unexpected(IgnoredAny),
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        future::pending,
        io::{self, Write},
        sync::{Arc, Mutex},
        time::Instant,
    };

    use alloy_consensus::{SignableTransaction, TxEip1559, transaction::Recovered};
    use alloy_primitives::{Address, Signature, U256};
    use alloy_rpc_client::BatchRequest;
    use alloy_rpc_types_eth::Transaction;
    use axum::{
        Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post,
    };
    use clap::Parser;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle, time::advance};
    use tracing::instrument::WithSubscriber;
    use url::Url;

    use super::*;
    use crate::WithdrawerArgs;

    #[derive(Clone)]
    struct Script {
        requests: Arc<Mutex<Vec<(Value, tokio::time::Instant)>>>,
        responses: Vec<(StatusCode, Value)>,
        hung_attempts: usize,
    }

    #[derive(Clone)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Server {
        url: Url,
        requests: Arc<Mutex<Vec<(Value, tokio::time::Instant)>>>,
        task: JoinHandle<()>,
    }

    impl Server {
        async fn new(responses: Vec<(StatusCode, Value)>) -> Self {
            Self::with_hung_attempts(responses, 0).await
        }

        async fn with_hung_attempts(
            responses: Vec<(StatusCode, Value)>,
            hung_attempts: usize,
        ) -> Self {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let state = Script { requests: Arc::clone(&requests), responses, hung_attempts };
            let router = Router::new()
                .fallback(post(
                    |State(script): State<Script>, Json(request): Json<Value>| async move {
                        let index = {
                            let mut log = script.requests.lock().unwrap();
                            let index = log.len();
                            log.push((request.clone(), tokio::time::Instant::now()));
                            index
                        };
                        if script.responses.is_empty() || index < script.hung_attempts {
                            pending::<()>().await;
                        }
                        let (status, mut response) = script.responses
                            [(index - script.hung_attempts).min(script.responses.len() - 1)]
                        .clone();
                        if let Some(object) = response.as_object_mut() {
                            object.insert("id".into(), request["id"].clone());
                        }
                        if let (Some(responses), Some(requests)) =
                            (response.as_array_mut(), request.as_array())
                        {
                            for (response, request) in responses.iter_mut().zip(requests) {
                                response["id"] = request["id"].clone();
                            }
                        }
                        match response {
                            Value::String(body) => (status, body).into_response(),
                            response => (status, Json(response)).into_response(),
                        }
                    },
                ))
                .with_state(state);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            Self { url, requests, task }
        }

        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        async fn wait_for_requests(&self, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.count() < count {
                assert!(Instant::now() < deadline, "server did not receive request {count}");
                tokio::task::yield_now().await;
            }
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
        }

        fn config(&self) -> WithdrawerConfig {
            let mut config = WithdrawerConfig::try_from(
                WithdrawerArgs::try_parse_from([
                    "withdrawer",
                    "--l1-rpc-url",
                    self.url.as_str(),
                    "--withdrawal",
                    &B256::ZERO.to_string(),
                    "--private-key",
                    &B256::repeat_byte(1).to_string(),
                ])
                .unwrap(),
            )
            .unwrap();
            config.network.l2_rpc_url = self.url.clone();
            config.max_retries = 2;
            config.retry_delay = Duration::from_millis(20);
            config.timeout = Duration::from_secs(7);
            config
        }

        fn signed() -> TxEnvelope {
            TxEip1559::default()
                .into_signed(Signature::new(U256::from(1), U256::from(2), false))
                .into()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn transient_failures_stop_at_retry_bound_with_exponential_delays() {
        for response in [
            (StatusCode::TOO_MANY_REQUESTS, json!("slow down")),
            (StatusCode::INTERNAL_SERVER_ERROR, json!("unavailable")),
            (StatusCode::SERVICE_UNAVAILABLE, json!("unavailable")),
            (
                StatusCode::OK,
                json!({"jsonrpc":"2.0", "error":{"code":-32005,"message":"rate limit exceeded"}}),
            ),
        ] {
            let server = Server::new(vec![response]).await;
            let config = server.config();
            let rpc = WithdrawalRpc::new(&config).unwrap();
            assert!(rpc.l1.get_block_number().await.is_err());
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), 3, "max_retries counts attempts after the first");
            assert!(requests[1].1.duration_since(requests[0].1) >= config.retry_delay);
            assert!(requests[2].1.duration_since(requests[1].1) >= config.retry_delay * 2);
        }
    }

    #[tokio::test]
    async fn retryable_http_status_overrides_json_rpc_error_body() {
        for status in [StatusCode::TOO_MANY_REQUESTS, StatusCode::SERVICE_UNAVAILABLE] {
            let server = Server::new(vec![
                (
                    status,
                    json!({"jsonrpc":"2.0", "error":{"code":-32603,"message":"upstream unavailable"}}),
                ),
                (StatusCode::OK, json!({"jsonrpc":"2.0", "result":"0x2a"})),
            ])
            .await;
            let rpc = WithdrawalRpc::new(&server.config()).unwrap();
            assert_eq!(rpc.l1.get_block_number().await.unwrap(), 42);
            assert_eq!(server.count(), 2);
        }
    }

    #[tokio::test]
    async fn execution_reverted_is_not_retried() {
        let server = Server::new(vec![(
            StatusCode::OK,
            json!({"jsonrpc":"2.0", "error":{"code":3,"message":"execution reverted"}}),
        )])
        .await;
        let rpc = WithdrawalRpc::new(&server.config()).unwrap();
        let error = rpc.l1.call(Default::default()).await.unwrap_err();
        assert!(error.to_string().contains("execution reverted"));
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn batch_delivers_success_when_another_call_reverts() {
        let server = Server::new(vec![(
            StatusCode::OK,
            json!([
                {"jsonrpc":"2.0", "error":{"code":3,"message":"execution reverted"}},
                {"jsonrpc":"2.0", "result":"0x2a"}
            ]),
        )])
        .await;
        let rpc = WithdrawalRpc::new(&server.config()).unwrap();
        let mut batch = BatchRequest::new(rpc.l1.client());
        let reverted = batch.add_call::<_, Bytes>("eth_call", &(json!({}), "latest")).unwrap();
        let successful = batch.add_call::<_, U256>("eth_blockNumber", &()).unwrap();
        batch.send().await.unwrap();
        assert_eq!(successful.await.unwrap(), U256::from(42));
        assert_eq!(reverted.await.unwrap_err().as_error_resp().unwrap().code, 3);
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn deterministic_broadcast_rejections_are_not_retried_or_accepted() {
        for message in [
            "execution reverted: rate limit exceeded",
            "execution reverted: already known",
            "unknown transaction",
        ] {
            let server = Server::new(vec![
                (
                    StatusCode::OK,
                    json!({"jsonrpc":"2.0", "error":{"code":-32000,"message":message}}),
                ),
                (StatusCode::OK, json!({"jsonrpc":"2.0", "result":null})),
            ])
            .await;
            let rpc = WithdrawalRpc::new(&server.config()).unwrap();
            assert!(
                rpc.broadcast(&Server::signed()).await.is_err(),
                "a rejection is not acceptance"
            );
            assert_eq!(server.count(), 2, "a rejection gets one lookup, not another submission");
        }
    }

    #[tokio::test]
    async fn already_known_broadcast_is_success() {
        for message in ["already known", "transaction already imported"] {
            let server = Server::new(vec![(
                StatusCode::OK,
                json!({"jsonrpc":"2.0", "error":{"code":-32000,"message":message}}),
            )])
            .await;
            let rpc = WithdrawalRpc::new(&server.config()).unwrap();
            let tx = Server::signed();
            let result = rpc.broadcast(&tx).await;
            assert!(result.is_ok(), "already-known broadcast must succeed: {result:?}");
            assert_eq!(result.unwrap(), *tx.tx_hash());
            assert_eq!(server.count(), 1);
        }
    }

    #[tokio::test]
    async fn broadcast_replays_identical_bytes_and_accepts_already_known() {
        for message in ["already known", "transaction already imported"] {
            let server = Server::new(vec![
                (StatusCode::SERVICE_UNAVAILABLE, json!("unavailable")),
                (
                    StatusCode::OK,
                    json!({"jsonrpc":"2.0", "error":{"code":-32000,"message":message}}),
                ),
            ])
            .await;
            let rpc = WithdrawalRpc::new(&server.config()).unwrap();
            let tx = Server::signed();
            let result = rpc.broadcast(&tx).await;
            assert!(result.is_ok(), "already-known broadcast must succeed: {result:?}");
            assert_eq!(result.unwrap(), *tx.tx_hash());
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].0["params"], requests[1].0["params"]);
        }
    }

    #[tokio::test]
    async fn broadcast_reconciles_rejection_by_local_hash_and_preserves_unknown_failure() {
        let tx = Server::signed();
        let transaction = Transaction {
            inner: Recovered::new_unchecked(tx.clone(), Address::ZERO),
            block_hash: None,
            block_number: None,
            transaction_index: None,
            effective_gas_price: None,
            block_timestamp: None,
        };
        for lookup in [
            json!({"jsonrpc":"2.0", "result":transaction}),
            json!({"jsonrpc":"2.0", "result":null}),
            json!({"jsonrpc":"2.0", "error":{"code":-32603,"message":"lookup failed"}}),
        ] {
            let found = lookup.get("result").is_some_and(Value::is_object);
            let server = Server::new(vec![
                (StatusCode::SERVICE_UNAVAILABLE, json!("unavailable")),
                (
                    StatusCode::OK,
                    json!({"jsonrpc":"2.0", "error":{"code":-32000,"message":"nonce too low"}}),
                ),
                (StatusCode::OK, lookup),
            ])
            .await;
            let rpc = WithdrawalRpc::new(&server.config()).unwrap();
            let result = rpc.broadcast(&tx).await;
            if found {
                assert_eq!(result.unwrap(), *tx.tx_hash());
            } else {
                let error = result.unwrap_err();
                let source = error.source().unwrap().downcast_ref::<TransportError>().unwrap();
                let payload = source.as_error_resp().unwrap();
                assert_eq!(payload.code, -32000);
                assert_eq!(payload.message, "nonce too low");
            }
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0].0["params"], requests[1].0["params"]);
            assert_eq!(requests[2].0["method"], "eth_getTransactionByHash");
            assert_eq!(requests[2].0["params"], json!([tx.tx_hash()]));
        }
    }

    #[tokio::test]
    async fn broadcast_times_out_when_server_never_responds() {
        let server = Server::new(vec![]).await;
        let mut config = server.config();
        config.timeout = Duration::from_millis(100);
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), rpc.broadcast(&Server::signed()))
            .await
            .expect("broadcast must not hang");
        assert!(matches!(result, Err(WithdrawalRpcError::Timeout)), "{result:?}");
        assert!(server.count() >= 1);
    }

    #[tokio::test]
    async fn broadcast_retries_a_hung_attempt_before_the_operation_deadline() {
        let tx = Server::signed();
        let server = Server::with_hung_attempts(
            vec![(StatusCode::OK, json!({"jsonrpc":"2.0", "result":tx.tx_hash()}))],
            1,
        )
        .await;
        let mut config = server.config();
        config.timeout = Duration::from_millis(500);
        let rpc = WithdrawalRpc::new(&config).unwrap();
        assert_eq!(rpc.broadcast(&tx).await.unwrap(), *tx.tx_hash());
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn provider_requests_time_out_and_retry_when_server_never_responds() {
        let server = Server::new(vec![]).await;
        let mut config = server.config();
        config.timeout = Duration::from_millis(100);
        config.max_retries = 1;
        let rpc = WithdrawalRpc::new(&config).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), rpc.l1.get_block_number())
            .await
            .expect("each HTTP attempt must be bounded")
            .unwrap_err();
        let source = error.source().unwrap().downcast_ref::<reqwest::Error>().unwrap();
        assert!(source.is_timeout());
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn broadcast_uses_local_hash_when_node_returns_an_unexpected_result() {
        for result in [
            json!(B256::repeat_byte(9)),
            json!("not a hash"),
            Value::Null,
            json!({"unexpected":true}),
        ] {
            let server =
                Server::new(vec![(StatusCode::OK, json!({"jsonrpc":"2.0", "result":result}))])
                    .await;
            let rpc = WithdrawalRpc::new(&server.config()).unwrap();
            let tx = Server::signed();
            assert_eq!(rpc.broadcast(&tx).await.unwrap(), *tx.tx_hash());
            assert_eq!(server.count(), 1);
        }
    }

    #[tokio::test]
    async fn non_rpc_success_body_does_not_mean_broadcast_acceptance() {
        for body in [
            json!("<html>proxy error</html>"),
            json!(""),
            json!({"message":"rate limited"}),
            json!({}),
        ] {
            let server = Server::new(vec![(StatusCode::OK, body)]).await;
            let rpc = WithdrawalRpc::new(&server.config()).unwrap();
            assert!(rpc.broadcast(&Server::signed()).await.is_err());
            assert_eq!(server.count(), 1);
        }
    }

    #[tokio::test]
    async fn deserialization_errors_do_not_echo_response_credentials() {
        let body = serde_json::to_string(
            "https://rpc-user:rpc-password@example.com/path-secret?key=query-secret",
        )
        .unwrap();
        let server = Server::new(vec![(StatusCode::OK, Value::String(body))]).await;
        let rpc = WithdrawalRpc::new(&server.config()).unwrap();
        let error = rpc.l1.get_block_number().await.unwrap_err();
        let mut rendered = format!("{error} {error:?}");
        let mut source = error.source();
        while let Some(cause) = source {
            rendered.push_str(&format!(" {cause} {cause:?}"));
            source = cause.source();
        }
        assert!(rendered.contains("Data error at line"), "{rendered}");
        for secret in ["rpc-user", "rpc-password", "path-secret", "query-secret"] {
            assert!(!rendered.contains(secret), "RPC credential leaked: {rendered}");
        }
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn non_http_schemes_are_rejected_without_exposing_the_url() {
        let server = Server::new(vec![(StatusCode::OK, json!({"result":"0x0"}))]).await;
        for url in [
            "ws://rpc-user:rpc-password@127.0.0.1:1/path-secret",
            "wss://rpc-user:rpc-password@127.0.0.1:1/path-secret",
            "file:///path-secret",
            "socks5://rpc-user:rpc-password@127.0.0.1:1/path-secret",
        ] {
            let mut config = server.config();
            config.l1_rpc_url = url.parse().unwrap();
            let error = WithdrawalRpc::new(&config).unwrap_err();
            let rendered = format!("{error} {error:?}");
            assert!(
                rendered.contains("HTTP(S)"),
                "unsupported scheme must explain the supported transport: {rendered}"
            );
            assert!(!rendered.contains("secret") && !rendered.contains("password"), "{rendered}");
        }
    }

    #[tokio::test]
    async fn credential_url_is_absent_from_errors_and_sources() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let server = Server::new(vec![(StatusCode::OK, json!({"result":"0x0"}))]).await;
        let mut config = server.config();
        config.l1_rpc_url = format!(
            "http://rpc-user:rpc-password@127.0.0.1:{port}/v3/path-secret?key=query-secret"
        )
        .parse()
        .unwrap();
        config.max_retries = 0;
        let capture = LogCapture(Arc::new(Mutex::new(Vec::new())));
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
            .with_writer(move || writer.clone())
            .finish();
        let error = async {
            let rpc = WithdrawalRpc::new(&config).unwrap();
            rpc.l1.get_block_number().await.unwrap_err()
        }
        .with_subscriber(subscriber)
        .await;
        let logs = capture.0.lock().unwrap();
        let mut rendered = format!("{error} {error:?} {}", String::from_utf8_lossy(&logs));
        let mut source = error.source();
        assert!(source.is_some(), "connection failure must retain a safe source");
        while let Some(cause) = source {
            rendered.push_str(&format!(" {cause} {cause:?}"));
            source = cause.source();
        }
        assert!(rendered.to_ascii_lowercase().contains("connection refused"), "{rendered}");
        for secret in ["rpc-user", "rpc-password", "path-secret", "query-secret"] {
            assert!(!rendered.contains(secret), "RPC credential leaked: {rendered}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn receipt_polling_waits_between_requests_and_times_out() {
        let server =
            Server::new(vec![(StatusCode::OK, json!({"jsonrpc":"2.0","result":null}))]).await;
        let rpc = WithdrawalRpc::new(&server.config()).unwrap();
        let task = tokio::spawn(async move { rpc.wait_for_receipt(B256::ZERO).await });
        server.wait_for_requests(1).await;
        for second in 1..=6 {
            advance(Duration::from_secs(1)).await;
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                server.count(),
                1 + second / 2,
                "receipt polling must wait between requests"
            );
        }
        advance(Duration::from_secs(1)).await;
        assert!(matches!(task.await.unwrap(), Err(WithdrawalRpcError::Timeout)));
        assert_eq!(server.count(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn receipt_status_distinguishes_revert_from_success_after_a_transient_failure() {
        let transient_failures = [
            (StatusCode::SERVICE_UNAVAILABLE, json!("unavailable")),
            (
                StatusCode::OK,
                json!({"jsonrpc":"2.0", "error":{"code":-32000,"message":WithdrawalRpc::TX_INDEXING_IN_PROGRESS}}),
            ),
        ];
        for (status, transient) in
            ["0x0", "0x1"].into_iter().flat_map(|s| transient_failures.clone().map(|t| (s, t)))
        {
            let receipt = json!({
                "transactionHash":B256::ZERO,"transactionIndex":"0x0", "blockHash":B256::ZERO,
                "blockNumber":"0x1","from":Address::ZERO,"to":Address::ZERO,
                "cumulativeGasUsed":"0x5208","gasUsed":"0x5208","contractAddress":null,
                "logs":[],"logsBloom":format!("0x{}", "00".repeat(256)),"status":status,
                "effectiveGasPrice":"0x1","type":"0x0"
            });
            let server = Server::new(vec![
                transient,
                (StatusCode::OK, json!({"jsonrpc":"2.0","result":receipt})),
            ])
            .await;
            let mut config = server.config();
            config.max_retries = 0;
            config.l1_rpc_url.set_username("rpc-user").unwrap();
            config.l1_rpc_url.set_password(Some("rpc-password")).unwrap();
            config.l1_rpc_url.set_path("/path-secret");
            config.l1_rpc_url.set_query(Some("key=query-secret"));
            let rpc = WithdrawalRpc::new(&config).unwrap();
            let capture = LogCapture(Arc::new(Mutex::new(Vec::new())));
            let writer = capture.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            let task = tokio::spawn(
                async move { rpc.wait_for_receipt(B256::ZERO).await }.with_subscriber(subscriber),
            );
            server.wait_for_requests(1).await;
            advance(WithdrawalRpc::RECEIPT_POLL_INTERVAL).await;
            server.wait_for_requests(2).await;
            let result = task.await.unwrap();
            let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
            assert!(logs.contains("receipt request failed transiently"), "{logs}");
            assert!(logs.contains("error="), "{logs}");
            assert!(
                logs.contains("503") || logs.contains(WithdrawalRpc::TX_INDEXING_IN_PROGRESS),
                "{logs}"
            );
            for secret in ["rpc-user", "rpc-password", "path-secret", "query-secret"] {
                assert!(!logs.contains(secret), "RPC credential leaked: {logs}");
            }
            if status == "0x0" {
                assert!(
                    matches!(result, Err(WithdrawalRpcError::Reverted(_))),
                    "status zero must return Reverted: {result:?}"
                );
            } else {
                assert!(result.unwrap().status());
            }
        }
    }

    #[tokio::test]
    async fn receipt_polling_stops_on_deterministic_errors() {
        let server = Server::new(vec![(
            StatusCode::OK,
            json!({"jsonrpc":"2.0", "error":{"code":-32602,"message":"invalid params"}}),
        )])
        .await;
        let rpc = WithdrawalRpc::new(&server.config()).unwrap();
        assert!(matches!(
            rpc.wait_for_receipt(B256::ZERO).await,
            Err(WithdrawalRpcError::Request(_))
        ));
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn l2_provider_deserializes_deposit_receipts() {
        let receipt: Value = serde_json::from_str(include_str!(
            "../fixtures/synthetic-deposit-withdrawal-receipt.json"
        ))
        .unwrap();
        let server =
            Server::new(vec![(StatusCode::OK, json!({"jsonrpc":"2.0", "result":receipt}))]).await;
        let rpc = WithdrawalRpc::new(&server.config()).unwrap();
        let receipt = rpc.l2.get_transaction_receipt(B256::ZERO).await.unwrap().unwrap();
        assert_eq!(crate::Withdrawal::from_receipt(&receipt).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pending_nonce_uses_the_typed_provider() {
        let server =
            Server::new(vec![(StatusCode::OK, json!({"jsonrpc":"2.0","result":"0x5"}))]).await;
        let rpc = WithdrawalRpc::new(&server.config()).unwrap();
        assert_eq!(rpc.l1.get_transaction_count(Address::ZERO).pending().await.unwrap(), 5);
        assert_eq!(server.requests.lock().unwrap()[0].0["params"][1], "pending");
        assert_eq!(rpc.l2.get_block_number().await.unwrap(), 5);
    }
}
