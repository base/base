//! JSON-RPC reverse-proxy handlers.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use reqwest::Client;
use serde_json::{Value, json};
use tracing::{error, warn};
use url::Url;

use crate::Backend;

/// Maximum accepted inbound JSON-RPC request body size (`2 MiB`).
pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Maximum accepted backend response body size (`2 MiB`).
pub(crate) const MAX_RESPONSE_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Shared state for JSON-RPC proxy routes.
#[derive(Debug, Clone)]
pub struct ProxyState {
    /// HTTP client used to call backend URLs.
    client: Client,
    /// Configured backend name (for logs).
    backend_name: Arc<str>,
    /// URL used for forwarding (first URL of the backend for now).
    backend_url: Url,
}

impl ProxyState {
    /// Builds proxy state from a validated backend.
    pub fn from_backend(backend: &Backend) -> Self {
        let backend_url = backend.urls[0].clone();
        Self { client: Client::new(), backend_name: Arc::from(backend.name.as_str()), backend_url }
    }

    /// Returns the JSON-RPC proxy router (`POST /`).
    pub fn router(self) -> Router {
        Router::new()
            .route("/", post(Self::handle_rpc))
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
            .with_state(self)
    }

    async fn handle_rpc(State(state): State<Self>, body: Bytes) -> Response {
        let parsed: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => {
                warn!(error = %error, "invalid JSON-RPC request body");
                return jsonrpc_error(Value::Null, -32700, "parse error");
            }
        };

        if parsed.is_array() {
            return jsonrpc_error(Value::Null, -32600, "batch requests are not supported");
        }

        if !parsed.is_object() {
            return jsonrpc_error(Value::Null, -32600, "invalid request");
        }

        let id = parsed.get("id").cloned().unwrap_or(Value::Null);

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
                jsonrpc_error(id, -32000, "backend response too large")
            }
            Err(ForwardError::Transport(error)) => {
                error!(
                    error = %error,
                    backend = %state.backend_name,
                    url = %state.backend_url,
                    "backend request failed"
                );
                jsonrpc_error(id, -32000, "backend request failed")
            }
        }
    }

    async fn forward(&self, body: &Bytes) -> Result<Bytes, ForwardError> {
        let mut response = self
            .client
            .post(self.backend_url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.clone())
            .send()
            .await
            .map_err(ForwardError::Transport)?
            .error_for_status()
            .map_err(ForwardError::Transport)?;

        if let Some(content_length) = response.content_length()
            && content_length > MAX_RESPONSE_BODY_BYTES as u64
        {
            return Err(ForwardError::ResponseTooLarge);
        }

        let mut buf = Vec::new();
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
