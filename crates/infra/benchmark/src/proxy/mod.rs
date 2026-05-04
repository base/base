//! Axum-based JSON-RPC proxy that intercepts `eth_sendRawTransaction` calls
//! into a [`FakeMempool`] and forwards all other methods to the upstream RPC.

use std::sync::Arc;

use alloy_primitives::Bytes;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use axum::routing::post;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::consensus::FakeMempool;
use crate::error::BenchmarkError;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    params: Option<Value>,
    id: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse<T: Serialize> {
    jsonrpc: String,
    id: Value,
    result: T,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    jsonrpc: String,
    id: Value,
    error: JsonRpcErrorBody,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorBody {
    code: i64,
    message: String,
}

#[derive(Clone)]
struct ProxyState {
    upstream: Url,
    mempool: FakeMempool,
    client: reqwest::Client,
}

async fn handle_rpc(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return proxy_raw(&state, &headers, body).await.into_response();
        }
    };

    if req.method == "eth_sendRawTransaction" {
        return handle_send_raw_transaction(&state, req).await.into_response();
    }

    proxy_raw(&state, &headers, body).await.into_response()
}

async fn handle_send_raw_transaction(
    state: &ProxyState,
    req: JsonRpcRequest,
) -> impl IntoResponse {
    let raw_hex = req
        .params
        .as_ref()
        .and_then(|p| p.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let raw_bytes = match hex::decode(raw_hex.trim_start_matches("0x")) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            warn!(error = %e, "failed to decode raw transaction hex");
            let resp = JsonRpcError {
                jsonrpc: req.jsonrpc,
                id: req.id,
                error: JsonRpcErrorBody {
                    code: -32602,
                    message: format!("invalid hex: {e}"),
                },
            };
            return (StatusCode::OK, axum::Json(resp)).into_response();
        }
    };

    let tx_hash = alloy_primitives::keccak256(&raw_bytes);
    state.mempool.add_transactions(vec![raw_bytes]);
    info!(tx_hash = %tx_hash, "intercepted eth_sendRawTransaction");

    let resp = JsonRpcResponse {
        jsonrpc: req.jsonrpc,
        id: req.id,
        result: format!("{tx_hash:#x}"),
    };
    (StatusCode::OK, axum::Json(resp)).into_response()
}

async fn proxy_raw(
    state: &ProxyState,
    headers: &HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let mut upstream_req = state.client.post(state.upstream.as_str()).body(body.to_vec());

    for (key, value) in headers {
        let name = key.as_str();
        if name == "host" || name == "content-length" {
            continue;
        }
        upstream_req = upstream_req.header(key, value);
    }

    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "upstream RPC request failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let resp_bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "failed to read upstream response body");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let mut builder = Response::builder().status(status.as_u16());
    for (key, value) in &resp_headers {
        if key.as_str() == "content-length" {
            continue;
        }
        builder = builder.header(key, value);
    }

    builder
        .body(Body::from(resp_bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub async fn run_proxy(
    listen_port: u16,
    upstream_url: Url,
    mempool: FakeMempool,
    cancel: CancellationToken,
) -> Result<(), BenchmarkError> {
    let state = Arc::new(ProxyState {
        upstream: upstream_url,
        mempool,
        client: reqwest::Client::new(),
    });

    let router = Router::new()
        .route("/", post(handle_rpc))
        .with_state(state);

    let listener = TcpListener::bind(("0.0.0.0", listen_port))
        .await
        .map_err(|e| BenchmarkError::Io(e))?;

    info!(port = listen_port, "proxy listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .map_err(|e| BenchmarkError::Io(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_error_serializes() {
        let err = JsonRpcError {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(1),
            error: JsonRpcErrorBody {
                code: -32602,
                message: "bad param".into(),
            },
        };
        let s = serde_json::to_string(&err).unwrap();
        assert!(s.contains("-32602"));
        assert!(s.contains("bad param"));
    }

    #[test]
    fn json_rpc_response_serializes() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(42),
            result: "0xdeadbeef",
        };
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("0xdeadbeef"));
        assert!(s.contains("42"));
    }
}
