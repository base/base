//! Optional admin JSON-RPC handler.
//!
//! Provides `POST /` JSON-RPC admin methods mirroring the Go `op-proposer` API:
//! - `admin_startProposer`   — start the driver loop
//! - `admin_stopProposer`    — stop the driver loop
//! - `admin_proposerRunning` — query whether the driver is running

use std::{fmt, sync::Arc};

use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use serde::{Deserialize, Serialize};

use crate::driver::ProposerDriverControl;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// State shared across admin JSON-RPC handlers.
#[derive(Clone)]
pub struct AdminState {
    /// Handle to the proposer driver for start/stop/query.
    pub driver: Arc<dyn ProposerDriverControl>,
}

impl fmt::Debug for AdminState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminState").field("driver", &"<dyn ProposerDriverControl>").finish()
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC types (minimal, hand-rolled for three methods)
// ---------------------------------------------------------------------------

/// Incoming JSON-RPC 2.0 request.
#[derive(Deserialize)]
struct JsonRpcRequest {
    /// Must be "2.0".
    #[serde(rename = "jsonrpc")]
    _jsonrpc: String,
    /// Method name.
    method: String,
    /// Unused — admin methods take no parameters.
    #[serde(rename = "params")]
    _params: Option<serde_json::Value>,
    /// Caller-chosen request id.
    id: serde_json::Value,
}

/// Outgoing JSON-RPC 2.0 response.
#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
    id: serde_json::Value,
}

/// JSON-RPC error object.
#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

impl JsonRpcResponse {
    const fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self { jsonrpc: "2.0", result: Some(result), error: None, id }
    }

    const fn error(id: serde_json::Value, code: i32, message: String) -> Self {
        Self { jsonrpc: "2.0", result: None, error: Some(JsonRpcError { code, message }), id }
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `POST /` — dispatches JSON-RPC admin methods.
async fn admin_rpc(
    State(state): State<AdminState>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let response = match request.method.as_str() {
        "admin_startProposer" => match state.driver.start_proposer().await {
            Ok(()) => JsonRpcResponse::success(request.id, serde_json::Value::Null),
            Err(e) => JsonRpcResponse::error(request.id, -32000, e),
        },
        "admin_stopProposer" => match state.driver.stop_proposer().await {
            Ok(()) => JsonRpcResponse::success(request.id, serde_json::Value::Null),
            Err(e) => JsonRpcResponse::error(request.id, -32000, e),
        },
        "admin_proposerRunning" => {
            let running = state.driver.is_running();
            JsonRpcResponse::success(request.id, serde_json::json!(running))
        }
        other => JsonRpcResponse::error(request.id, -32601, format!("method not found: {other}")),
    };

    Json(response)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl AdminState {
    /// Returns an [`axum::Router`] with the admin JSON-RPC endpoint at `POST /`.
    ///
    /// The returned router has its own state applied and is served on a
    /// dedicated listener, separate from the health server.
    pub fn router(driver: Arc<dyn ProposerDriverControl>) -> Router {
        let state = Self { driver };
        Router::new().route("/", post(admin_rpc)).with_state(state)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// Mock driver control that tracks start/stop calls.
    struct MockDriverControl {
        running: AtomicBool,
    }

    impl MockDriverControl {
        fn new() -> Self {
            Self { running: AtomicBool::new(false) }
        }
    }

    #[async_trait]
    impl ProposerDriverControl for MockDriverControl {
        async fn start_proposer(&self) -> Result<(), String> {
            if self.running.load(Ordering::SeqCst) {
                return Err("already running".into());
            }
            self.running.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn stop_proposer(&self) -> Result<(), String> {
            if !self.running.load(Ordering::SeqCst) {
                return Err("not running".into());
            }
            self.running.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }
    }

    /// Starts the admin server on an ephemeral port and returns its address.
    async fn start_test_server(
        driver: Arc<dyn ProposerDriverControl>,
    ) -> (std::net::SocketAddr, CancellationToken) {
        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = AdminState::router(driver);

        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel_clone.cancelled().await })
                .await
                .unwrap();
        });

        (addr, cancel)
    }

    #[tokio::test]
    async fn test_admin_start_stop() {
        let driver: Arc<dyn ProposerDriverControl> = Arc::new(MockDriverControl::new());
        let (addr, cancel) = start_test_server(Arc::clone(&driver)).await;

        let client = reqwest::Client::new();

        // Start the proposer
        let resp = client
            .post(format!("http://{addr}/"))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "admin_startProposer",
                "id": 1
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"].is_null());
        assert!(driver.is_running());

        // Check running status
        let resp = client
            .post(format!("http://{addr}/"))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "admin_proposerRunning",
                "id": 2
            }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["result"], true);

        // Stop the proposer
        let resp = client
            .post(format!("http://{addr}/"))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "admin_stopProposer",
                "id": 3
            }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"].is_null());
        assert!(!driver.is_running());

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_admin_unknown_method() {
        let driver: Arc<dyn ProposerDriverControl> = Arc::new(MockDriverControl::new());
        let (addr, cancel) = start_test_server(driver).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/"))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "admin_doesNotExist",
                "id": 1
            }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], -32601);
        assert!(body["error"]["message"].as_str().unwrap().contains("method not found"));

        cancel.cancel();
    }
}
