use std::time::Duration;

use alloy_primitives::B256;
use alloy_provider::RootProvider;
use alloy_transport::TransportError;
use base_common_network::Base;
use thiserror::Error;

use crate::{WithdrawalRpcTransport, WithdrawerConfig};

/// RPC operations that could not complete.
#[derive(Debug, Error)]
pub enum WithdrawalRpcError {
    /// A request failed.
    #[error("RPC request failed: {0}")]
    Request(#[source] TransportError),
    /// The transaction was included but reverted.
    #[error("transaction {0} reverted")]
    Reverted(B256),
}

/// Typed L1 and L2 providers for withdrawal reads and estimates.
#[derive(Debug, Clone)]
pub struct WithdrawalRpc {
    /// L1 state reads and gas estimates.
    pub l1: RootProvider,
    /// Base L2 state reads, including deposit receipts and storage proofs.
    pub l2: RootProvider<Base>,
    /// Deadline for dry-run preparation.
    pub timeout: Duration,
}

impl WithdrawalRpc {
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
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        future::pending,
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use alloy_primitives::{Address, Bytes, U256};
    use alloy_provider::Provider;
    use alloy_rpc_client::BatchRequest;
    use axum::{
        Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post,
    };
    use clap::Parser;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};
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
