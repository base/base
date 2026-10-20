use std::{
    fmt,
    task::{Context, Poll},
    time::Duration,
};

use alloy_json_rpc::{Request, RequestPacket, Response, ResponsePacket, ResponsePayload};
use alloy_network::Network;
use alloy_primitives::{Bytes, keccak256};
use alloy_provider::RootProvider;
use alloy_rpc_client::ClientBuilder;
use alloy_transport::{TransportError, TransportErrorKind, TransportFut, utils::guess_local_url};
use serde::de::Error as _;
use tower::Service;
use tracing::{instrument::WithSubscriber, subscriber::NoSubscriber};
use url::Url;

use crate::{BroadcastJournal, WithdrawerConfig};

/// Bounded exponential retries and credential-safe errors over reqwest HTTP.
#[derive(Clone)]
pub struct WithdrawalRpcTransport {
    client: reqwest::Client,
    url: Url,
    max_retries: u32,
    retry_delay: Duration,
    redact_errors: bool,
    journal: Option<BroadcastJournal>,
}

impl fmt::Debug for WithdrawalRpcTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WithdrawalRpcTransport")
            .field("max_retries", &self.max_retries)
            .field("retry_delay", &self.retry_delay)
            .finish_non_exhaustive()
    }
}

impl WithdrawalRpcTransport {
    /// Maximum duration of one HTTP attempt, further capped by operation and submission deadlines.
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// Builds a typed HTTP provider with bounded requests and retries, without network I/O.
    pub fn connect<N: Network>(
        url: &Url,
        config: &WithdrawerConfig,
    ) -> Result<RootProvider<N>, TransportError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(TransportErrorKind::non_retryable_str("RPC URL must use HTTP(S)"));
        }
        let client = reqwest::Client::builder()
            .timeout(Self::REQUEST_TIMEOUT.min(config.timeout / 2))
            .build()
            .map_err(|error| Self::redact(TransportErrorKind::custom(error)))?;
        Ok(RootProvider::new(ClientBuilder::default().transport(
            Self {
                client,
                url: url.clone(),
                max_retries: config.max_retries,
                retry_delay: config.retry_delay,
                redact_errors: false,
                journal: None,
            },
            guess_local_url(url),
        )))
    }

    /// Builds a redacting submission provider that records hashes before forwarding requests.
    pub fn connect_for_submission<N: Network>(
        url: &Url,
        config: &WithdrawerConfig,
        journal: BroadcastJournal,
    ) -> Result<RootProvider<N>, TransportError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(TransportErrorKind::non_retryable_str("RPC URL must use HTTP(S)"));
        }
        let attempt_timeout =
            config.submission_request_timeout().map_err(TransportErrorKind::custom)?;
        let client = reqwest::Client::builder()
            .timeout(attempt_timeout)
            .build()
            .map_err(|error| Self::redact(TransportErrorKind::custom(error)))?;
        Ok(RootProvider::new(ClientBuilder::default().transport(
            Self {
                client,
                url: url.clone(),
                max_retries: config.max_retries,
                retry_delay: config.retry_delay,
                redact_errors: true,
                journal: Some(journal),
            },
            guess_local_url(url),
        )))
    }

    /// Retries alloy rate-limit responses, HTTP 429/5xx and connection failures, not EVM errors.
    pub fn should_retry(error: &TransportError) -> bool {
        match error {
            TransportError::ErrorResp(payload) => {
                payload.code != 3
                    && !payload.message.to_ascii_lowercase().contains("execution reverted")
                    && payload.is_retry_err()
            }
            TransportError::Transport(kind) => {
                if let Some(http) = kind.as_http_error() {
                    return http.status == 429 || (500..600).contains(&http.status);
                }
                match kind {
                    TransportErrorKind::Custom(error) => {
                        error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
                            error.is_connect()
                                || error.is_timeout()
                                || error.is_request()
                                || error.is_body()
                        })
                    }
                    TransportErrorKind::BackendGone
                    | TransportErrorKind::MissingBatchResponse(_) => true,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Removes URL-bearing transport details while preserving safe source errors.
    pub fn redact(error: TransportError) -> TransportError {
        match error {
            TransportError::Transport(TransportErrorKind::Custom(error)) => {
                error.downcast::<reqwest::Error>().map_or_else(
                    |_| TransportErrorKind::custom_str("transport request failed"),
                    |error| TransportErrorKind::custom(error.without_url()),
                )
            }
            TransportError::Transport(kind) if kind.is_http_error() => {
                TransportErrorKind::http_error(
                    kind.as_http_error().expect("HTTP error").status,
                    String::new(),
                )
            }
            TransportError::DeserError { err, .. } => {
                let diagnostic = format!(
                    "{:?} error at line {} column {}",
                    err.classify(),
                    err.line(),
                    err.column()
                );
                TransportError::DeserError {
                    err: serde_json::Error::custom(diagnostic),
                    text: "[redacted RPC response]".into(),
                }
            }
            error => error,
        }
    }

    /// Redacts server messages and data after retry classification; must preserve the code and "execution reverted" marker for revert classification.
    pub fn redact_diagnostic(error: TransportError) -> TransportError {
        let mut error = Self::redact(error);
        if let TransportError::ErrorResp(payload) = &mut error {
            let message = payload.message.trim_start().to_ascii_lowercase();
            payload.message = if payload.code == 3 || message.starts_with("execution reverted") {
                "execution reverted"
            } else if message.contains(crate::WithdrawalRpc::TX_INDEXING_IN_PROGRESS) {
                crate::WithdrawalRpc::TX_INDEXING_IN_PROGRESS
            } else {
                "RPC server rejected request"
            }
            .into();
            // Provider diagnostics can echo authenticated endpoints or request secrets.
            payload.data = None;
        }
        error
    }
}

impl Service<RequestPacket> for WithdrawalRpcTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        if let Some(journal) = &self.journal {
            journal.record(&request);
        }
        let this = self.clone();
        Box::pin(async move {
            let mut delay = this.retry_delay;
            for attempt in 0..=this.max_retries {
                // HTTP client spans can include endpoint credentials.
                let result = async {
                    let response = this
                        .client
                        .post(this.url.clone())
                        .json(&request)
                        .headers(request.headers())
                        .send()
                        .await
                        .map_err(TransportErrorKind::custom)?;
                    let status = response.status();
                    if status.as_u16() == 429 || status.is_server_error() {
                        return Err(TransportErrorKind::http_error(status.as_u16(), String::new()));
                    }
                    let body = response.bytes().await.map_err(|error| {
                        if status.is_success() {
                            TransportErrorKind::custom(error)
                        } else {
                            TransportErrorKind::http_error(status.as_u16(), String::new())
                        }
                    })?;
                    let parsed = serde_json::from_slice::<ResponsePacket>(&body);
                    if !status.is_success() && !parsed.as_ref().is_ok_and(ResponsePacket::is_error)
                    {
                        return Err(TransportErrorKind::http_error(status.as_u16(), String::new()));
                    }
                    parsed.map_err(|err| TransportError::DeserError { err, text: String::new() })
                }
                .with_subscriber(NoSubscriber::default())
                .await;
                let error = match result {
                    Ok(ResponsePacket::Single(response)) if response.is_error() => {
                        TransportError::ErrorResp(
                            response.payload.as_error().expect("error response").clone(),
                        )
                    }
                    Ok(response) => return Ok(response),
                    Err(error) => error,
                };
                if attempt > 0
                    && this.journal.is_some()
                    && let RequestPacket::Single(request) = &request
                    && request.method() == "eth_sendRawTransaction"
                    && let Some(params) = request.params()
                    && let Ok([raw]) = serde_json::from_str::<[Bytes; 1]>(params.get())
                {
                    let hash = keccak256(raw);
                    let known =
                        match base_tx_manager::RpcErrorClassifier::classify_rpc_error(&error) {
                            base_tx_manager::TxManagerError::AlreadyKnown => true,
                            base_tx_manager::TxManagerError::NonceTooLow => {
                                let lookup = Request::new(
                                    "eth_getTransactionByHash",
                                    request.id().clone(),
                                    [hash],
                                )
                                .serialize()
                                .expect("transaction hash serializes");
                                // A consumed nonce alone does not identify the accepted transaction.
                                matches!(
                                    this.clone().call(RequestPacket::Single(lookup)).await,
                                    Ok(ResponsePacket::Single(response))
                                        if response.try_success_as::<Option<serde_json::Value>>()
                                            .is_some_and(|result| matches!(result, Ok(Some(_))))
                                )
                            }
                            _ => false,
                        };
                    if known {
                        return Ok(ResponsePacket::Single(Response {
                            id: request.id().clone(),
                            payload: ResponsePayload::Success(
                                serde_json::value::to_raw_value(&hash)
                                    .expect("transaction hash serializes"),
                            ),
                        }));
                    }
                }
                if attempt == this.max_retries || !Self::should_retry(&error) {
                    return Err(if this.redact_errors {
                        // Fee-bump and nonce policy need typed errors, not server diagnostics.
                        let classified =
                            base_tx_manager::RpcErrorClassifier::classify_rpc_error(&error);
                        let mut error = Self::redact_diagnostic(error);
                        if !matches!(
                            classified,
                            base_tx_manager::TxManagerError::Rpc(_)
                                | base_tx_manager::TxManagerError::ExecutionReverted { .. }
                        ) && let TransportError::ErrorResp(payload) = &mut error
                        {
                            payload.message = classified.to_string().into();
                        }
                        error
                    } else {
                        Self::redact(error)
                    });
                }
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
            unreachable!("the initial request attempt always runs")
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use alloy_json_rpc::Id;
    use alloy_network::{Ethereum, EthereumWallet};
    use alloy_primitives::B256;
    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
    use base_tx_manager::{NoopTxMetrics, SendState, SimpleTxManager};
    use clap::Parser;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn lost_broadcast_response_is_reconciled_before_the_manager_deadline() {
        let broadcasts = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::clone(&broadcasts);
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<Value>| {
                let attempts = Arc::clone(&attempts);
                async move {
                    if request["method"] == "eth_chainId" {
                        return Json(json!({
                            "jsonrpc": "2.0", "id": request["id"], "result": "0x1",
                        }));
                    }
                    assert_eq!(request["method"], "eth_sendRawTransaction");
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        std::future::pending::<()>().await;
                    }
                    Json(json!({
                        "jsonrpc": "2.0", "id": request["id"],
                        "error": {"code": -32000, "message": "already known"},
                    }))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = WithdrawerConfig::try_from(
            crate::WithdrawerArgs::try_parse_from([
                "withdrawer",
                "--l1-rpc-url",
                &url,
                "--withdrawal",
                &B256::ZERO.to_string(),
                "--private-key",
                &B256::repeat_byte(1).to_string(),
                "--tx-manager.network-timeout=1s",
                "--retry-delay=100ms",
            ])
            .unwrap(),
        )
        .unwrap();
        let journal = BroadcastJournal::default();
        let provider = WithdrawalRpcTransport::connect_for_submission::<Ethereum>(
            &config.l1_rpc_url,
            &config,
            journal.clone(),
        )
        .unwrap();
        let signer = crate::WithdrawalSigner::new(&config.signer).await.unwrap();
        let manager = SimpleTxManager::from_wallet(
            provider,
            EthereumWallet::from(signer),
            config.tx_manager.clone(),
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap();
        let raw = Bytes::from_static(&[2, 3]);
        let send_state = SendState::new(config.tx_manager.safe_abort_nonce_too_low_count).unwrap();
        let started = tokio::time::Instant::now();
        let result = manager.publish_tx(&send_state, &raw, None).await;
        server.abort();
        assert_eq!(result.unwrap(), keccak256(&raw));
        assert!(started.elapsed() < config.tx_manager.network_timeout);
        assert_eq!(broadcasts.load(Ordering::SeqCst), 2);
        assert_eq!(journal.hashes(), vec![keccak256(&raw)]);
    }

    #[tokio::test]
    async fn replayed_broadcast_reconciles_only_the_accepted_transaction() {
        for (retry, rejection, transaction, accepted) in [
            (true, "already known", Value::Null, true),
            (true, "nonce too low", json!({"hash": keccak256([2, 3])}), true),
            (true, "nonce too low", Value::Null, false),
            (false, "already known", Value::Null, false),
            (false, "nonce too low", json!({"hash": keccak256([2, 3])}), false),
        ] {
            let broadcasts = Arc::new(AtomicUsize::new(0));
            let attempts = Arc::clone(&broadcasts);
            let app = Router::new().route(
                "/",
                post(move |Json(request): Json<Value>| {
                    let attempts = Arc::clone(&attempts);
                    let transaction = transaction.clone();
                    async move {
                        if request["method"] == "eth_sendRawTransaction" {
                            if attempts.fetch_add(1, Ordering::SeqCst) == 0 && retry {
                                return StatusCode::BAD_GATEWAY.into_response();
                            }
                            Json(json!({
                                "jsonrpc": "2.0", "id": request["id"],
                                "error": {"code": -32000, "message": rejection},
                            }))
                            .into_response()
                        } else {
                            assert_eq!(request["method"], "eth_getTransactionByHash");
                            assert_eq!(request["params"][0], json!(keccak256([2, 3])));
                            Json(json!({
                                "jsonrpc": "2.0", "id": request["id"], "result": transaction,
                            }))
                            .into_response()
                        }
                    }
                }),
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let mut transport = WithdrawalRpcTransport {
                client: reqwest::Client::new(),
                url,
                max_retries: 1,
                retry_delay: Duration::from_millis(1),
                redact_errors: true,
                journal: Some(BroadcastJournal::default()),
            };
            let request = Request::new(
                "eth_sendRawTransaction",
                Id::Number(7),
                [Bytes::from_static(&[2, 3])],
            )
            .serialize()
            .unwrap();
            let result = transport.call(RequestPacket::Single(request)).await;
            server.abort();
            if accepted {
                let ResponsePacket::Single(response) = result.unwrap() else {
                    panic!("expected a single send response");
                };
                assert_eq!(response.id, Id::Number(7));
                assert_eq!(response.try_success_as::<B256>().unwrap().unwrap(), keccak256([2, 3]));
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    base_tx_manager::RpcErrorClassifier::classify_rpc_error(&error),
                    if rejection == "already known" {
                        base_tx_manager::TxManagerError::AlreadyKnown
                    } else {
                        base_tx_manager::TxManagerError::NonceTooLow
                    },
                );
            }
            assert_eq!(broadcasts.load(Ordering::SeqCst), if retry { 2 } else { 1 });
        }
    }

    #[tokio::test]
    async fn failed_broadcasts_retain_distinct_hashes_including_batches() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
        drop(listener);
        let journal = BroadcastJournal::default();
        let mut transport = WithdrawalRpcTransport {
            client: reqwest::Client::new(),
            url,
            max_retries: 1,
            retry_delay: Duration::from_millis(1),
            redact_errors: true,
            journal: Some(journal.clone()),
        };
        let request =
            Request::new("eth_sendRawTransaction", Id::Number(1), [Bytes::from_static(&[2, 3])])
                .serialize()
                .unwrap();
        let replacement =
            Request::new("eth_sendRawTransaction", Id::Number(4), [Bytes::from_static(&[2, 4])])
                .serialize()
                .unwrap();
        let malformed = Request::new("eth_sendRawTransaction", Id::Number(2), json!(["not hex"]))
            .serialize()
            .unwrap();
        let read = Request::new("eth_call", Id::Number(3), [Bytes::from_static(&[4])])
            .serialize()
            .unwrap();
        assert!(transport.call(RequestPacket::Single(request.clone())).await.is_err());
        assert!(
            transport
                .call(RequestPacket::Batch(vec![request, replacement, malformed, read]))
                .await
                .is_err()
        );
        assert_eq!(journal.hashes(), vec![keccak256([2, 3]), keccak256([2, 4])]);
    }
}
