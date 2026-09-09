//! Tower middleware for backwards-compatible HTTP GET health checks.
//!
//! Replicates go-ethereum's behavior of returning HTTP 200 for empty GET requests
//! to `"/"`, enabling AWS load balancers and similar infrastructure to probe node
//! liveness without requiring a JSON-RPC POST.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures::{FutureExt, TryFutureExt};
use http::{Method, StatusCode, header};
use http_body::Body as HttpBodyTrait;
use jsonrpsee::server::{HttpBody, HttpRequest, HttpResponse};
use tower::{Layer, Service};

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Tower [`Layer`] that returns HTTP 200 for empty GET requests to `"/"`.
///
/// Closely mirrors go-ethereum's [`rpc/http.go`] behavior:
///
/// ```go
/// if r.Method == http.MethodGet && r.ContentLength == 0 && r.URL.RawQuery == "" {
///     w.WriteHeader(http.StatusOK)
///     return
/// }
/// ```
///
/// **Intentional divergence from go-ethereum**: the original code matches any
/// path (e.g. `GET /some/path`); this implementation restricts the short-circuit
/// to `"/"` only. This is deliberate — we want to health-check only the root
/// path for AWS ALB compatibility while letting other paths (e.g. `/healthz`)
/// fall through to the inner service.
///
/// This enables AWS load balancers and similar infrastructure to check node
/// liveness using a plain `GET /` without requiring a JSON-RPC POST request,
/// maintaining backwards compatibility with existing health check clients.
///
/// [`rpc/http.go`]: https://github.com/ethereum/go-ethereum/blob/master/rpc/http.go
#[derive(Clone, Debug, Default)]
pub struct EthHealthCheckLayer;

impl<S> Layer<S> for EthHealthCheckLayer {
    type Service = EthHealthCheckService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        EthHealthCheckService { inner }
    }
}

/// Tower [`Service`] produced by [`EthHealthCheckLayer`].
#[derive(Clone, Debug)]
pub struct EthHealthCheckService<S> {
    inner: S,
}

impl<S, B> Service<HttpRequest<B>> for EthHealthCheckService<S>
where
    S: Service<HttpRequest, Response = HttpResponse>,
    S::Response: 'static,
    S::Error: Into<BoxError> + 'static,
    S::Future: Send + 'static,
    B: HttpBodyTrait<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    type Response = HttpResponse;
    type Error = BoxError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: HttpRequest<B>) -> Self::Future {
        // Mirror go-ethereum rpc/http.go: return 200 for empty GET / with no query string.
        // Used by AWS health checks and similar load balancer probes for node liveness.
        // Mirror go-ethereum's r.ContentLength semantics:
        //   - absent header        → 0  (no body; Go's net/http reports ContentLength = 0
        //                               when no Content-Length header and no body is read)
        //   - present, numeric     → the parsed i64 value (0 triggers health check; any
        //                               other value, e.g. 5 or 100, does not)
        //   - present, malformed   → -1 (non-UTF8 bytes or non-numeric string; fail closed,
        //                               does not trigger health check)
        // We use i64 rather than u64 so that malformed header values (non-UTF8 or
        // non-numeric strings) fall into the unwrap_or(-1) branch instead of being
        // silently dropped and misread as an empty-body request. -1 is chosen to mirror
        // Go's "unknown length" sentinel; the only thing that matters is that it is != 0.
        let content_length: i64 = req
            .headers()
            .get(header::CONTENT_LENGTH)
            .map_or(0, |v| v.to_str().ok().and_then(|s| s.parse().ok()).unwrap_or(-1));

        if req.method() == Method::GET
            && req.uri().path() == "/"
            // Go: r.URL.RawQuery == "" is true for both no `?` and a bare `?`.
            // uri().query() returns None for no `?` and Some("") for `/?`, so
            // we must accept both cases to match go-ethereum semantics exactly.
            && req.uri().query().is_none_or(|q| q.is_empty())
            && content_length == 0
        {
            return async {
                Ok(HttpResponse::builder()
                    .status(StatusCode::OK)
                    .body(HttpBody::from(String::new()))
                    .expect("valid response"))
            }
            .boxed();
        }

        self.inner.call(req.map(HttpBody::new)).map_err(Into::into).boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use alloy_transport_http::reqwest;
    use jsonrpsee::{
        RpcModule,
        core::client::ClientT,
        http_client::HttpClientBuilder,
        rpc_params,
        server::{Server, ServerHandle, middleware::http::ProxyGetRequestLayer},
    };
    use tower::ServiceBuilder;

    use super::*;
    use crate::{HealthzApiServer, HealthzResponse, HealthzRpc};

    /// Build a test server with the full production middleware stack
    /// (`EthHealthCheckLayer` → `ProxyGetRequestLayer`) and the given RPC module.
    ///
    /// Pass `RpcModule::new(())` for tests that exercise the health-check short-circuit
    /// without needing real RPC methods; pass a module with `healthz` registered for
    /// end-to-end GET /healthz proxy tests and JSON-RPC POST tests.
    async fn build_server(module: RpcModule<()>) -> (SocketAddr, ServerHandle) {
        let server = Server::builder()
            .set_http_middleware(ServiceBuilder::new().layer(EthHealthCheckLayer).layer(
                ProxyGetRequestLayer::new([("/healthz", "healthz")]).expect("valid proxy config"),
            ))
            .build("127.0.0.1:0")
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();
        let handle = server.start(module);
        (addr, handle)
    }

    /// Returns an `RpcModule` with the `healthz` method registered.
    fn healthz_module() -> RpcModule<()> {
        let mut module = RpcModule::new(());
        module
            .merge(HealthzApiServer::into_rpc(HealthzRpc::new(env!("CARGO_PKG_VERSION"))))
            .expect("healthz merge");
        module
    }

    // ── EthHealthCheckLayer short-circuit: GET / with no query and no body ────────

    #[tokio::test]
    async fn test_empty_get_root_returns_200() {
        // The canonical go-ethereum health probe: plain GET / with no query string and
        // no body. EthHealthCheckLayer must short-circuit this and return 200 immediately
        // without touching the inner service.
        let (addr, _handle) = build_server(RpcModule::new(())).await;
        let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    // ── Conditions that must NOT trigger the short-circuit ────────────────────────

    #[tokio::test]
    async fn test_get_root_with_query_passes_through() {
        // GET / with a non-empty query string does not match the go-ethereum health check
        // pattern. EthHealthCheckLayer passes it through; ProxyGetRequestLayer has no
        // mapping for plain `/`, so the request reaches the jsonrpsee transport, which
        // returns 405 because only POST is accepted for JSON-RPC dispatch.
        let (addr, _handle) = build_server(RpcModule::new(())).await;
        let resp = reqwest::get(format!("http://{addr}/?foo=bar")).await.unwrap();
        assert_eq!(resp.status(), 405);
    }

    #[tokio::test]
    async fn test_get_root_with_content_length_passes_through() {
        // GET / with a non-zero Content-Length header must not trigger the short-circuit
        // (mirrors go-ethereum's `r.ContentLength == 0` guard). The request falls through
        // to the transport, which rejects the GET with 405.
        //
        // Use `.body("hello")` rather than `.header(CONTENT_LENGTH, "5")` on a bodyless
        // GET. reqwest/hyper derive Content-Length from the actual body bytes, so a real
        // 5-byte body guarantees the wire request carries `content-length: 5`. A manually
        // set Content-Length on a bodyless GET is an implementation detail of hyper's
        // `set_length` (it skips removal when body is None today, but that is not a
        // stable guarantee). Attaching a real body is the only portable way to ensure the
        // server sees a non-zero Content-Length and the short-circuit is not triggered.
        let (addr, _handle) = build_server(RpcModule::new(())).await;
        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .body("hello") // 5-byte body → Content-Length: 5 on the wire, guaranteed
            .send()
            .await
            .unwrap();
        // EthHealthCheckLayer must NOT short-circuit (content_length != 0);
        // the request reaches the jsonrpsee transport, which rejects GET with 405.
        assert_eq!(resp.status(), 405);
    }

    #[tokio::test]
    async fn test_post_to_root_passes_through() {
        // POST / is a normal JSON-RPC request and must never be intercepted by
        // EthHealthCheckLayer, even though the path and absence of query match the
        // health-check conditions. Only GET triggers the short-circuit.
        let (addr, _handle) = build_server(healthz_module()).await;
        let client = HttpClientBuilder::default().build(format!("http://{addr}")).unwrap();
        // A well-formed JSON-RPC POST reaches the inner service and succeeds.
        let result: HealthzResponse = client.request("healthz", rpc_params![]).await.unwrap();
        assert_eq!(result.version, env!("CARGO_PKG_VERSION"));
    }

    // ── End-to-end: GET /healthz proxied through to the RPC method ────────────────

    #[tokio::test]
    async fn test_get_healthz_returns_200() {
        // Full production middleware stack end-to-end: EthHealthCheckLayer passes
        // GET /healthz through (path is not `/`), then ProxyGetRequestLayer rewrites
        // it into a POST to the `healthz` JSON-RPC method, which returns HTTP 200.
        let (addr, _handle) = build_server(healthz_module()).await;
        let resp = reqwest::get(format!("http://{addr}/healthz")).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    // ── Edge case: GET /? (bare `?` with empty query string) ─────────────────────

    #[tokio::test]
    async fn test_bare_query_marker_returns_200() {
        // GET /? has an empty query string (uri().query() == Some("")), not absent
        // (uri().query() == None). go-ethereum's RawQuery == "" is true for both cases.
        // The implementation uses query().map_or(true, |q| q.is_empty()) to accept both.
        // This test guards against regression to query().is_none(), which would have
        // passed None but rejected Some(""), causing GET /? to fall through to the
        // transport and return 405 instead of 200.
        let (addr, _handle) = build_server(RpcModule::new(())).await;
        let resp = reqwest::get(format!("http://{addr}/?")).await.unwrap();
        assert_eq!(resp.status(), 200);
    }
}
