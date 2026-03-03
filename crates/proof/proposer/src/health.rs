//! Combined health-check and optional admin JSON-RPC HTTP server.
//!
//! Provides:
//! - `GET /healthz` — liveness probe (always 200 while the process is alive)
//! - `GET /readyz`  — readiness probe (200 when the service is fully initialised)
//! - `POST /`       — JSON-RPC admin methods (only when admin is enabled)
//!
//! The admin JSON-RPC methods mirror the Go `op-proposer` admin API:
//! - `admin_startProposer`  — start the driver loop
//! - `admin_stopProposer`   — stop the driver loop
//! - `admin_proposerRunning` — query whether the driver is running

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::driver::ProposerDriverControl;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// State shared across all HTTP handlers.
#[derive(Clone)]
struct ServerState {
    /// Set to `true` once the service has completed initialisation.
    ready: Arc<AtomicBool>,
    /// If present, admin JSON-RPC methods are enabled.
    driver: Option<Arc<dyn ProposerDriverControl>>,
}

// ---------------------------------------------------------------------------
// Health endpoints
// ---------------------------------------------------------------------------

/// `GET /healthz` — liveness probe.
async fn liveness() -> StatusCode {
    StatusCode::OK
}

/// `GET /readyz` — readiness probe.
async fn readiness(State(state): State<ServerState>) -> StatusCode {
    if state.ready.load(Ordering::Relaxed) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
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
// Admin JSON-RPC handler
// ---------------------------------------------------------------------------

/// `POST /` — dispatches JSON-RPC admin methods.
async fn admin_rpc(
    State(state): State<ServerState>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let driver = match &state.driver {
        Some(d) => d,
        None => {
            return Json(JsonRpcResponse::error(
                request.id,
                -32601,
                "admin methods are not enabled".into(),
            ));
        }
    };

    let response = match request.method.as_str() {
        "admin_startProposer" => match driver.start_proposer().await {
            Ok(()) => JsonRpcResponse::success(request.id, serde_json::Value::Null),
            Err(e) => JsonRpcResponse::error(request.id, -32000, e),
        },
        "admin_stopProposer" => match driver.stop_proposer().await {
            Ok(()) => JsonRpcResponse::success(request.id, serde_json::Value::Null),
            Err(e) => JsonRpcResponse::error(request.id, -32000, e),
        },
        "admin_proposerRunning" => {
            let running = driver.is_running();
            JsonRpcResponse::success(request.id, serde_json::json!(running))
        }
        other => JsonRpcResponse::error(request.id, -32601, format!("method not found: {other}")),
    };

    Json(response)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Starts the combined health and admin HTTP server.
///
/// The server binds to `addr` and runs until `cancel` is triggered.
///
/// # Arguments
///
/// * `addr`   — socket address to listen on (e.g. `127.0.0.1:8545`)
/// * `ready`  — shared flag; `/readyz` returns 200 when this is `true`
/// * `driver` — if `Some`, admin JSON-RPC methods are mounted on `POST /`
/// * `cancel` — cancellation token for graceful shutdown
///
/// # Errors
///
/// Returns an error if the TCP listener cannot bind to `addr`.
pub async fn serve(
    addr: SocketAddr,
    ready: Arc<AtomicBool>,
    driver: Option<Arc<dyn ProposerDriverControl>>,
    cancel: CancellationToken,
) -> eyre::Result<()> {
    let has_admin = driver.is_some();
    let state = ServerState { ready, driver };

    let mut app = Router::new().route("/healthz", get(liveness)).route("/readyz", get(readiness));

    if has_admin {
        app = app.route("/", post(admin_rpc));
    }

    let app = app.with_state(state);

    let listener = TcpListener::bind(addr).await?;
    info!(%addr, admin = has_admin, "Health server started");

    let cancel_for_shutdown = cancel.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel_for_shutdown.cancelled().await })
        .await?;

    info!("Health server stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::driver::ProposerDriverControl;

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

    /// Starts the health server on an ephemeral port and returns its address.
    async fn start_test_server(
        ready: Arc<AtomicBool>,
        driver: Option<Arc<dyn ProposerDriverControl>>,
    ) -> (SocketAddr, CancellationToken) {
        let cancel = CancellationToken::new();
        // Bind to port 0 to get an ephemeral port.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let has_admin = driver.is_some();
        let state = ServerState { ready, driver };

        let mut app =
            Router::new().route("/healthz", get(liveness)).route("/readyz", get(readiness));
        if has_admin {
            app = app.route("/", post(admin_rpc));
        }
        let app = app.with_state(state);

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
    async fn test_liveness_always_ok() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, cancel) = start_test_server(ready, None).await;

        let resp = reqwest::get(format!("http://{addr}/healthz")).await.unwrap();
        assert_eq!(resp.status(), 200);

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_readiness_not_ready() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, cancel) = start_test_server(ready, None).await;

        let resp = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
        assert_eq!(resp.status(), 503);

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_readiness_ready() {
        let ready = Arc::new(AtomicBool::new(true));
        let (addr, cancel) = start_test_server(ready, None).await;

        let resp = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
        assert_eq!(resp.status(), 200);

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_readiness_transitions() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, cancel) = start_test_server(Arc::clone(&ready), None).await;

        // Initially not ready
        let resp = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
        assert_eq!(resp.status(), 503);

        // Mark as ready
        ready.store(true, Ordering::SeqCst);

        let resp = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
        assert_eq!(resp.status(), 200);

        // Mark as not ready (shutdown)
        ready.store(false, Ordering::SeqCst);

        let resp = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();
        assert_eq!(resp.status(), 503);

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_admin_start_stop() {
        let ready = Arc::new(AtomicBool::new(true));
        let driver: Arc<dyn ProposerDriverControl> = Arc::new(MockDriverControl::new());
        let (addr, cancel) = start_test_server(ready, Some(Arc::clone(&driver))).await;

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
        let ready = Arc::new(AtomicBool::new(true));
        let driver: Arc<dyn ProposerDriverControl> = Arc::new(MockDriverControl::new());
        let (addr, cancel) = start_test_server(ready, Some(driver)).await;

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
