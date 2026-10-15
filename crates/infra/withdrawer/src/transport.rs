use std::{
    fmt,
    task::{Context, Poll},
    time::Duration,
};

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_provider::RootProvider;
use alloy_rpc_client::ClientBuilder;
use alloy_transport::{TransportError, TransportErrorKind, TransportFut, utils::guess_local_url};
use serde::de::Error as _;
use tower::Service;
use tracing::{instrument::WithSubscriber, subscriber::NoSubscriber};
use url::Url;

use crate::WithdrawerConfig;

/// Bounded exponential retries and credential-safe errors over reqwest HTTP.
#[derive(Clone)]
pub struct WithdrawalRpcTransport {
    client: reqwest::Client,
    url: Url,
    max_retries: u32,
    retry_delay: Duration,
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
    /// Maximum duration of one HTTP attempt, further capped at half the operation timeout.
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// Builds a typed HTTP provider with bounded requests and retries, without network I/O.
    pub fn connect(url: &Url, config: &WithdrawerConfig) -> Result<RootProvider, TransportError> {
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
}

impl Service<RequestPacket> for WithdrawalRpcTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
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
                if attempt == this.max_retries || !Self::should_retry(&error) {
                    return Err(Self::redact(error));
                }
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
            unreachable!("the initial request attempt always runs")
        })
    }
}
