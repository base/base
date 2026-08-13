//! HTTP server for Roxy.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Context;
use axum::Router;
use base_health::HealthServer;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{Config, ProxyState};

/// Roxy HTTP server.
#[derive(Debug, Clone, Copy, Default)]
pub struct Server;

impl Server {
    /// Returns the application router (health probes + JSON-RPC proxy).
    pub fn router(ready: Arc<AtomicBool>, proxy: ProxyState) -> Router {
        HealthServer::router(ready).merge(proxy.router())
    }

    /// Starts the Roxy HTTP server with the provided configuration.
    pub async fn serve(config: Config, cancel: CancellationToken) -> anyhow::Result<()> {
        let backends = config.backends()?;
        if backends.len() > 1 {
            warn!(
                backend_count = backends.len(),
                active_backend = %backends[0].name,
                "multiple backends configured; only the first is used until routing lands"
            );
        }
        if backends[0].urls.len() > 1 {
            warn!(
                backend = %backends[0].name,
                url_count = backends[0].urls.len(),
                active_url = %backends[0].urls[0],
                "multiple URLs configured for backend; only the first is used until pooling lands"
            );
        }
        let proxy = ProxyState::from_backend(&backends[0]);

        let listen_addr = config.listen_addr;
        let ready = Arc::new(AtomicBool::new(false));
        let app = Self::router(Arc::clone(&ready), proxy);

        let listener = TcpListener::bind(listen_addr)
            .await
            .with_context(|| format!("failed to bind roxy server to {listen_addr}"))?;
        let listen_addr = listener.local_addr().context("failed to read roxy listen address")?;

        info!(
            %listen_addr,
            backend = %backends[0].name,
            url = %backends[0].urls[0],
            url_count = backends[0].urls.len(),
            "roxy server started"
        );

        // Marked ready before `serve` is first polled. The listener is already bound, so the
        // kernel accept backlog queues any connections that arrive in the gap; they are served
        // as soon as the accept loop runs. Callers see added latency, never a refused connection.
        ready.store(true, Ordering::SeqCst);

        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
            .context("roxy server exited with error")?;

        info!("roxy server stopped");
        Ok(())
    }
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

    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path},
    };

    use super::*;
    use crate::Backend;

    /// Starts the server on an ephemeral port for tests.
    async fn start_test_server(
        ready: Arc<AtomicBool>,
        proxy: ProxyState,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>, CancellationToken) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let app = Server::router(ready, proxy);
        let cancel = CancellationToken::new();
        let cancel_for_shutdown = cancel.clone();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancel_for_shutdown.cancelled().await })
                .await
                .expect("server serve");
        });

        (addr, handle, cancel)
    }

    fn dummy_proxy() -> ProxyState {
        let backend = Backend::parse("dummy=http://127.0.0.1:1").expect("parse backend");
        ProxyState::from_backend(&backend)
    }

    #[tokio::test]
    async fn healthz_returns_ok_when_not_ready() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, _handle, cancel) = start_test_server(ready, dummy_proxy()).await;

        let response =
            reqwest::get(format!("http://{addr}/healthz")).await.expect("healthz request");
        assert_eq!(response.status().as_u16(), 200, "liveness must return 200 even when not ready");

        cancel.cancel();
    }

    #[tokio::test]
    async fn readyz_reflects_ready_flag() {
        let ready = Arc::new(AtomicBool::new(false));
        let (addr, _handle, cancel) = start_test_server(Arc::clone(&ready), dummy_proxy()).await;

        let response = reqwest::get(format!("http://{addr}/readyz"))
            .await
            .expect("readyz request while not ready");
        assert_eq!(response.status().as_u16(), 503, "readiness must return 503 before ready");

        ready.store(true, Ordering::SeqCst);

        let response = reqwest::get(format!("http://{addr}/readyz"))
            .await
            .expect("readyz request while ready");
        assert_eq!(response.status().as_u16(), 200, "readiness must return 200 after ready");

        cancel.cancel();
    }

    #[tokio::test]
    async fn proxies_single_jsonrpc_request_to_backend() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(json!({
                "jsonrpc": "2.0",
                "method": "eth_chainId",
                "id": 1
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": "0x2105"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let backend = Backend::parse(&format!("rpcs={}", mock.uri())).expect("parse backend");
        let ready = Arc::new(AtomicBool::new(true));
        let (addr, _handle, cancel) =
            start_test_server(ready, ProxyState::from_backend(&backend)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "eth_chainId",
                "params": [],
                "id": 1
            }))
            .send()
            .await
            .expect("proxy request");

        assert_eq!(response.status().as_u16(), 200, "proxy must return HTTP 200");
        let body: serde_json::Value = response.json().await.expect("response json");
        assert_eq!(body["result"], json!("0x2105"), "result must match mocked backend");
        assert_eq!(body["id"], json!(1), "id must be preserved");

        cancel.cancel();
    }

    #[tokio::test]
    async fn rejects_batch_requests() {
        let mock = MockServer::start().await;
        let backend = Backend::parse(&format!("rpcs={}", mock.uri())).expect("parse backend");
        let ready = Arc::new(AtomicBool::new(true));
        let (addr, _handle, cancel) =
            start_test_server(ready, ProxyState::from_backend(&backend)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .json(&json!([
                {"jsonrpc": "2.0", "method": "eth_chainId", "id": 1},
                {"jsonrpc": "2.0", "method": "eth_blockNumber", "id": 2}
            ]))
            .send()
            .await
            .expect("batch request");

        assert_eq!(response.status().as_u16(), 200, "JSON-RPC errors use HTTP 200");
        let body: serde_json::Value = response.json().await.expect("response json");
        assert_eq!(body["error"]["code"], json!(-32600), "invalid request code");
        assert_eq!(
            body["error"]["message"],
            json!("batch requests are not supported"),
            "batch must be rejected"
        );
        assert_eq!(
            mock.received_requests().await.expect("request log").len(),
            0,
            "backend must not be called"
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn returns_jsonrpc_error_when_backend_is_down() {
        let backend = Backend::parse("rpcs=http://127.0.0.1:1").expect("parse backend");
        let ready = Arc::new(AtomicBool::new(true));
        let (addr, _handle, cancel) =
            start_test_server(ready, ProxyState::from_backend(&backend)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "eth_chainId",
                "params": [],
                "id": 7
            }))
            .send()
            .await
            .expect("proxy request");

        assert_eq!(response.status().as_u16(), 200, "JSON-RPC errors use HTTP 200");
        let body: serde_json::Value = response.json().await.expect("response json");
        assert_eq!(body["id"], json!(7), "id must be preserved on backend failure");
        assert_eq!(body["error"]["code"], json!(-32000), "server error code");

        cancel.cancel();
    }

    #[tokio::test]
    async fn returns_jsonrpc_error_when_backend_response_too_large() {
        let mock = MockServer::start().await;
        let oversized = vec![b'x'; crate::MAX_RESPONSE_BODY_BYTES + 1];
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(oversized))
            .expect(1)
            .mount(&mock)
            .await;

        let backend = Backend::parse(&format!("rpcs={}", mock.uri())).expect("parse backend");
        let ready = Arc::new(AtomicBool::new(true));
        let (addr, _handle, cancel) =
            start_test_server(ready, ProxyState::from_backend(&backend)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "eth_chainId",
                "params": [],
                "id": 3
            }))
            .send()
            .await
            .expect("proxy request");

        assert_eq!(response.status().as_u16(), 200, "JSON-RPC errors use HTTP 200");
        let body: serde_json::Value = response.json().await.expect("response json");
        assert_eq!(body["id"], json!(3), "id must be preserved");
        assert_eq!(body["error"]["code"], json!(-32000), "server error code");
        assert_eq!(
            body["error"]["message"],
            json!("backend response too large"),
            "oversized backend responses must be rejected"
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn forwards_backend_error_body_on_non_success_status() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": 9,
                "error": { "code": -32005, "message": "rate limited" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let backend = Backend::parse(&format!("rpcs={}", mock.uri())).expect("parse backend");
        let ready = Arc::new(AtomicBool::new(true));
        let (addr, _handle, cancel) =
            start_test_server(ready, ProxyState::from_backend(&backend)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "eth_sendRawTransaction",
                "params": ["0xdead"],
                "id": 9
            }))
            .send()
            .await
            .expect("proxy request");

        assert_eq!(response.status().as_u16(), 200, "client still sees HTTP 200");
        let body: serde_json::Value = response.json().await.expect("response json");
        assert_eq!(body["id"], json!(9), "id must match backend payload");
        assert_eq!(body["error"]["code"], json!(-32005), "backend error code must be preserved");
        assert_eq!(
            body["error"]["message"],
            json!("rate limited"),
            "backend error message must be preserved"
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn preserves_backend_content_type() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(502).set_body_raw("<html>bad gateway</html>", "text/html"),
            )
            .expect(1)
            .mount(&mock)
            .await;

        let backend = Backend::parse(&format!("rpcs={}", mock.uri())).expect("parse backend");
        let ready = Arc::new(AtomicBool::new(true));
        let (addr, _handle, cancel) =
            start_test_server(ready, ProxyState::from_backend(&backend)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .body(r#"{"jsonrpc":"2.0","method":"eth_chainId","id":1}"#)
            .send()
            .await
            .expect("proxy request");

        assert_eq!(response.status().as_u16(), 200, "client still sees HTTP 200");
        assert_eq!(
            response.headers().get(reqwest::header::CONTENT_TYPE),
            Some(&reqwest::header::HeaderValue::from_static("text/html")),
            "backend content type must be preserved"
        );
        assert_eq!(
            response.text().await.expect("response body"),
            "<html>bad gateway</html>",
            "backend body must be preserved"
        );

        cancel.cancel();
    }
}
