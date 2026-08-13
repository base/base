//! JSON-RPC reverse-proxy handlers.

use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use reqwest::Client;
use serde::{Deserialize, de::IgnoredAny};
use serde_json::{Value, json};
use tracing::{error, warn};
use url::Url;

use crate::Backend;

/// Maximum accepted inbound JSON-RPC request body size (`2 MiB`).
pub const MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Maximum accepted backend response body size (`2 MiB`).
pub const MAX_RESPONSE_BODY_BYTES: usize = 2 * 1024 * 1024;

/// TCP connect timeout for backend requests.
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Overall timeout for a backend request (connect + headers + body).
const BACKEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared state for JSON-RPC proxy routes.
#[derive(Debug, Clone)]
pub struct ProxyState {
    /// HTTP client used to call backend URLs.
    client: Client,
    /// Configured backend name (for logs).
    backend_name: Arc<str>,
    /// URL used for forwarding (first URL of the backend for now).
    backend_url: Arc<Url>,
}

impl ProxyState {
    /// Builds proxy state from a validated backend.
    pub fn from_backend(backend: &Backend) -> Self {
        let backend_url = Arc::new(backend.urls[0].clone());
        let client = Client::builder()
            .connect_timeout(BACKEND_CONNECT_TIMEOUT)
            .timeout(BACKEND_REQUEST_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");
        Self { client, backend_name: Arc::from(backend.name.as_str()), backend_url }
    }

    /// Returns the JSON-RPC proxy router (`POST /`).
    pub fn router(self) -> Router {
        Router::new()
            .route("/", post(Self::handle_rpc))
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
            .with_state(self)
    }

    async fn handle_rpc(State(state): State<Self>, body: Bytes) -> Response {
        // Avoid full `Value` materialization (heap amplification on crafted payloads).
        match first_non_whitespace(&body) {
            Some(b'[') => {
                return jsonrpc_error(Value::Null, -32600, "batch requests are not supported");
            }
            Some(b'{') => {}
            Some(_) | None => return jsonrpc_error(Value::Null, -32600, "invalid request"),
        }

        // Validate JSON without building a `Value` tree; unused fields are skipped.
        if let Err(error) = serde_json::from_slice::<IgnoredAny>(&body) {
            warn!(error = %error, "invalid JSON-RPC request body");
            return jsonrpc_error(Value::Null, -32700, "parse error");
        }

        match state.forward(&body).await {
            Ok(response_body) => (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                response_body,
            )
                .into_response(),
            Err(ForwardError::ResponseTooLarge) => {
                warn!(
                    backend = %state.backend_name,
                    url = %state.backend_url,
                    max_bytes = MAX_RESPONSE_BODY_BYTES,
                    "backend response exceeded size limit"
                );
                jsonrpc_error(request_id(&body), -32000, "backend response too large")
            }
            Err(ForwardError::Transport(error)) => {
                error!(
                    error = %error,
                    backend = %state.backend_name,
                    url = %state.backend_url,
                    "backend request failed"
                );
                jsonrpc_error(request_id(&body), -32000, "backend request failed")
            }
        }
    }

    async fn forward(&self, body: &Bytes) -> Result<Bytes, ForwardError> {
        let mut response = self
            .client
            .post(self.backend_url.as_str())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.clone())
            .send()
            .await
            .map_err(ForwardError::Transport)?;

        let status = response.status();
        if !status.is_success() {
            // Keep the body: some backends put JSON-RPC errors on non-2xx HTTP statuses.
            warn!(
                backend = %self.backend_name,
                url = %self.backend_url,
                status = %status.as_u16(),
                "backend returned non-success HTTP status"
            );
        }

        let content_length = response.content_length();
        if let Some(content_length) = content_length
            && content_length > MAX_RESPONSE_BODY_BYTES as u64
        {
            return Err(ForwardError::ResponseTooLarge);
        }

        let mut buf = content_length.map_or_else(Vec::new, |len| Vec::with_capacity(len as usize));
        while let Some(chunk) = response.chunk().await.map_err(ForwardError::Transport)? {
            let next_len = buf.len().saturating_add(chunk.len());
            if next_len > MAX_RESPONSE_BODY_BYTES {
                return Err(ForwardError::ResponseTooLarge);
            }
            buf.extend_from_slice(&chunk);
        }

        Ok(Bytes::from(buf))
    }
}

/// Failure while forwarding a request to a backend.
#[derive(Debug)]
enum ForwardError {
    /// HTTP client / transport failure.
    Transport(reqwest::Error),
    /// Backend response exceeded [`MAX_RESPONSE_BODY_BYTES`].
    ResponseTooLarge,
}

/// Minimal request view used only when building proxy-generated JSON-RPC errors.
#[derive(Debug, Deserialize)]
struct IdOnly {
    /// JSON-RPC request id, if present.
    #[serde(default)]
    id: Option<Value>,
}

/// Returns the first non-ASCII-whitespace byte in `body`, if any.
fn first_non_whitespace(body: &[u8]) -> Option<u8> {
    body.iter().copied().find(|byte| !byte.is_ascii_whitespace())
}

/// Extracts `id` without materializing the rest of the request as a [`Value`] tree.
fn request_id(body: &[u8]) -> Value {
    serde_json::from_slice::<IdOnly>(body)
        .ok()
        .and_then(|request| request.id)
        .unwrap_or(Value::Null)
}

fn jsonrpc_error(id: Value, code: i64, message: &str) -> Response {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    });
    (StatusCode::OK, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_non_whitespace_skips_leading_space() {
        assert_eq!(first_non_whitespace(b"  \n{"), Some(b'{'));
        assert_eq!(first_non_whitespace(b"\t["), Some(b'['));
        assert_eq!(first_non_whitespace(b"   "), None);
    }

    #[test]
    fn request_id_reads_only_id_field() {
        let body = br#"{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":42}"#;
        assert_eq!(request_id(body), json!(42));

        let body = br#"{"jsonrpc":"2.0","method":"eth_chainId"}"#;
        assert_eq!(request_id(body), Value::Null);
    }
}
