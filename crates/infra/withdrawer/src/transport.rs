use std::{
    fmt,
    task::{Context, Poll},
    time::Duration,
};

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_network::Network;
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

    /// Redacts server diagnostics after retry classification, retaining only valid hex-string data for execution reverts and their code and marker.
    pub fn redact_diagnostic(error: TransportError) -> TransportError {
        let mut error = Self::redact(error);
        if let TransportError::ErrorResp(payload) = &mut error {
            let message = payload.message.trim_start().to_ascii_lowercase();
            let execution_revert = payload.code == 3 || message.starts_with("execution reverted");
            payload.message = if execution_revert {
                "execution reverted"
            } else if message.contains(crate::WithdrawalRpc::TX_INDEXING_IN_PROGRESS) {
                crate::WithdrawalRpc::TX_INDEXING_IN_PROGRESS
            } else {
                "RPC server rejected request"
            }
            .into();
            // Only ABI bytes are safe to retain from provider-supplied diagnostics.
            if !execution_revert
                || !payload.data.as_ref().is_some_and(|data| {
                    serde_json::from_str::<String>(data.get()).ok().is_some_and(|data| {
                        data.strip_prefix("0x")
                            .is_some_and(|hex| alloy_primitives::hex::decode(hex).is_ok())
                    })
                })
            {
                payload.data = None;
            }
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

#[cfg(test)]
mod tests {
    use alloy_json_rpc::ErrorPayload;
    use serde_json::json;

    use super::*;

    #[test]
    fn redaction_retains_only_hex_execution_revert_data() {
        for (code, message, data, retained) in [
            (3, "secret endpoint", json!("0xd9bc01be"), true),
            (-32000, "execution reverted: secret endpoint", json!("0x730a1074"), true),
            (3, "execution reverted", json!("0x"), true),
            (3, "execution reverted", json!("https://secret.endpoint"), false),
            (3, "execution reverted", json!("0xnothex"), false),
            (3, "execution reverted", json!("0x123"), false),
            (3, "execution reverted", json!({"data":"0xd9bc01be"}), false),
            (-32000, "secret endpoint", json!("0xd9bc01be"), false),
        ] {
            let payload: ErrorPayload = serde_json::from_value(json!({
                "code":code,"message":message,"data":data,
            }))
            .unwrap();
            let error =
                WithdrawalRpcTransport::redact_diagnostic(TransportError::ErrorResp(payload));
            let payload = error.as_error_resp().unwrap();
            assert_eq!(payload.data.is_some(), retained, "code={code}, data={data}");
            assert!(!payload.message.contains("secret"));
            if retained {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(payload.data.as_ref().unwrap().get())
                        .unwrap(),
                    data
                );
            }
        }
    }
}
