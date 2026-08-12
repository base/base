//! JSON-RPC reverse-proxy handlers.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use reqwest::Client;
use serde_json::{Value, json};
use tracing::{error, warn};
use url::Url;

use crate::Backend;

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
        Router::new().route("/", post(Self::handle_rpc)).with_state(self)
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
            Err(error) => {
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

    async fn forward(&self, body: &Bytes) -> Result<Bytes, reqwest::Error> {
        let response = self
            .client
            .post(self.backend_url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.clone())
            .send()
            .await?
            .error_for_status()?;
        let bytes = response.bytes().await?;
        Ok(bytes)
    }
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
