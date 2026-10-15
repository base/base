use std::{
    task::{Context, Poll},
    time::Duration,
};

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_provider::RootProvider;
use alloy_rpc_client::ClientBuilder;
use alloy_transport::{BoxTransport, TransportError, TransportErrorKind, TransportFut};
use tower::{Service, ServiceExt, layer::layer_fn};
use tracing::{instrument::WithSubscriber, subscriber::NoSubscriber};
use url::Url;

use crate::WithdrawerConfig;

/// Bounded exponential retries and credential-safe errors over alloy transports.
#[derive(Debug, Clone)]
pub struct WithdrawalRpcTransport {
    inner: BoxTransport,
    max_retries: u32,
    retry_delay: Duration,
}

impl WithdrawalRpcTransport {
    /// Builds a typed provider with the configured retry policy and built-in connection handling.
    pub async fn connect(
        url: &Url,
        config: &WithdrawerConfig,
    ) -> Result<RootProvider, TransportError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(TransportErrorKind::non_retryable_str("RPC URL must use HTTP(S)"));
        }
        ClientBuilder::default()
            .layer(layer_fn(|inner| Self {
                inner,
                max_retries: config.max_retries,
                retry_delay: config.retry_delay,
            }))
            .connect(url.as_str())
            .with_subscriber(NoSubscriber::default())
            .await
            .map(RootProvider::new)
            .map_err(Self::redact)
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
                TransportError::DeserError { err, text: "[redacted RPC response]".into() }
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
        let mut this = self.clone();
        Box::pin(async move {
            let mut delay = this.retry_delay;
            for attempt in 0..=this.max_retries {
                // Alloy transport spans include the complete endpoint URL, including API keys.
                let result = async { this.inner.ready().await?.call(request.clone()).await }
                    .with_subscriber(NoSubscriber::default())
                    .await;
                let error = match result {
                    Ok(response) => match response.as_error() {
                        Some(error) => TransportError::ErrorResp(error.clone()),
                        None => return Ok(response),
                    },
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
